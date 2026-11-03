//! Building the binaries (and, on request, the UI) of a source tree for `fl install
//! --from-git` (#205 A4b).
//!
//! [`Builder`] is the seam: [`ProcessBuilder`] runs `cargo` (and `npm`) as argument vectors in
//! a scrubbed environment ([`sandbox_env`]); tests inject a fake. The contract on `Ok`:
//! `<src>/target/release/<bin>` exists for every requested binary and, with the UI,
//! `<src>/ui/dist` does.
//!
//! The scrubbed environment is an allow-list. This is hygiene (no token, vault or cloud
//! credential in the environment of code that was merely reviewed), not isolation: build
//! scripts and procedural macros of the commit run as the user, with the user's file system
//! and network.

use crate::process::{self, Spec, absolute_path_entries, which};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The environment variables copied from the installer's own environment into a build. A name
/// not on this list never reaches the build, whatever it holds.
pub const ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "CARGO_NET_OFFLINE",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// The variables the installer sets itself (never taken from the caller's environment).
pub const ENV_FIXED: &[&str] = &[
    "HOME",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "SOURCE_DATE_EPOCH",
    "CARGO_TERM_COLOR",
    "CARGO_INCREMENTAL",
    "LC_ALL",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_GLOBAL",
    "GIT_TERMINAL_PROMPT",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_CEILING_DIRECTORIES",
];

/// How `bin/<name>` is built: which package holds the binary.
pub const BIN_PACKAGES: &[(&str, &str)] = &[("fl", "fl-cli"), ("ferrostore", "fl-store")];

/// The package that builds `bin/<name>`, if the installer knows a recipe.
pub fn package_for(bin: &str) -> Option<&'static str> {
    BIN_PACKAGES
        .iter()
        .find(|(b, _)| *b == bin)
        .map(|(_, p)| *p)
}

/// The environment of every process of a build. `parent` is the installer's own environment;
/// `build_dir` is the private directory the build lives in (its `home/` and `tmp/` are the
/// build's HOME and TMPDIR); `epoch` is the commit time.
pub fn sandbox_env(
    parent: &[(OsString, OsString)],
    build_dir: &Path,
    epoch: u64,
) -> Vec<(OsString, OsString)> {
    let get = |k: &str| {
        parent
            .iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
    };
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    let path = get("PATH").unwrap_or_default();
    env.push(("PATH".into(), absolute_path_entries(&path)));
    for name in ENV_PASSTHROUGH.iter().filter(|n| **n != "PATH") {
        let Some(v) = get(name) else { continue };
        if v.is_empty() {
            continue;
        }
        // Directories and files must be absolute: a relative one would mean something else in
        // the build directory.
        let needs_abs = !matches!(*name, "RUSTUP_TOOLCHAIN" | "CARGO_NET_OFFLINE");
        if needs_abs && !Path::new(&v).is_absolute() {
            continue;
        }
        env.push(((*name).into(), v));
    }
    // The toolchain lives under the REAL home; the build's HOME is private. Keep rustup and
    // cargo working by naming their homes explicitly when they exist.
    let real_home = get("HOME").map(PathBuf::from).filter(|h| h.is_absolute());
    for (name, dir) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
        if env.iter().any(|(n, _)| n == name) {
            continue;
        }
        if let Some(h) = &real_home
            && h.join(dir).is_dir()
        {
            env.push((name.into(), h.join(dir).into_os_string()));
        }
    }
    let kv = |k: &str, v: OsString| (OsString::from(k), v);
    let home = build_dir.join("home");
    env.push(kv("HOME", home.clone().into_os_string()));
    env.push(kv("XDG_CONFIG_HOME", home.join(".config").into_os_string()));
    env.push(kv("TMPDIR", build_dir.join("tmp").into_os_string()));
    env.push(kv("SOURCE_DATE_EPOCH", epoch.to_string().into()));
    env.push(kv("CARGO_TERM_COLOR", "never".into()));
    env.push(kv("CARGO_INCREMENTAL", "0".into()));
    env.push(kv("LC_ALL", "C".into()));
    env.push(kv("GIT_CONFIG_NOSYSTEM", "1".into()));
    env.push(kv("GIT_CONFIG_GLOBAL", "/dev/null".into()));
    env.push(kv("GIT_TERMINAL_PROMPT", "0".into()));
    env.push(kv("GIT_NO_REPLACE_OBJECTS", "1".into()));
    // Never look for a repository above the build directory.
    env.push(kv(
        "GIT_CEILING_DIRECTORIES",
        build_dir.to_path_buf().into_os_string(),
    ));
    env
}

/// What a build runs in.
pub struct BuildCtx<'a> {
    pub env: &'a [(OsString, OsString)],
    pub src: &'a Path,
    pub log: &'a Path,
    /// The whole build's budget.
    pub timeout: Duration,
}

impl BuildCtx<'_> {
    fn path_var(&self) -> OsString {
        self.env
            .iter()
            .find(|(n, _)| n == "PATH")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

/// What to build.
pub struct BuildJob {
    /// The `bin/<name>` files wanted, e.g. `fl`, `ferrostore`.
    pub bins: Vec<String>,
    /// Also build `ui/dist` (`npm ci && npm run build`).
    pub with_ui: bool,
    /// With the UI: let the npm dependencies' lifecycle scripts run.
    pub npm_scripts: bool,
}

/// The `npm ci` argument vector. Dependency lifecycle scripts (arbitrary code from the
/// registry) are off unless asked for.
pub fn npm_ci_args(scripts: bool) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["ci", "--no-audit", "--no-fund", "--no-update-notifier"]
        .iter()
        .map(OsString::from)
        .collect();
    if !scripts {
        a.push("--ignore-scripts".into());
    }
    a
}

/// `rustc -V` and `cargo -V`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    pub rustc: String,
    pub cargo: String,
}

/// Why a build did not produce its outputs.
#[derive(Debug)]
pub enum BuildError {
    /// A tool the build needs is missing (a refusal: nothing was tried).
    Unavailable(String),
    /// The build ran and failed, timed out, or could not be started.
    Failed(String),
}

pub trait Builder {
    /// The toolchain a build would use.
    fn toolchain(&self, ctx: &BuildCtx<'_>) -> Result<Toolchain, BuildError>;
    /// Build `job` in `ctx.src`, logging to `ctx.log`.
    fn build(&self, ctx: &BuildCtx<'_>, job: &BuildJob) -> Result<(), BuildError>;
}

/// The real thing: `cargo build --release --locked`, and with the UI `npm ci` and
/// `npm run build`, all as argument vectors.
#[derive(Debug, Default)]
pub struct ProcessBuilder;

/// The `cargo build` argument vector for `bins`.
pub fn cargo_args(bins: &[String]) -> Result<Vec<OsString>, BuildError> {
    let mut packages: Vec<&str> = Vec::new();
    for b in bins {
        let p = package_for(b).ok_or_else(|| {
            BuildError::Unavailable(format!("the installer has no build recipe for bin/{b}"))
        })?;
        if !packages.contains(&p) {
            packages.push(p);
        }
    }
    let mut a: Vec<OsString> = ["build", "--release", "--locked"]
        .iter()
        .map(OsString::from)
        .collect();
    for p in packages {
        a.push("-p".into());
        a.push(p.into());
    }
    for b in bins {
        a.push("--bin".into());
        a.push(b.into());
    }
    Ok(a)
}

fn tool_line(
    exe: &Path,
    ctx: &BuildCtx<'_>,
    args: &[&str],
    what: &str,
) -> Result<String, BuildError> {
    let o = process::run(&Spec {
        program: exe,
        args: args.iter().map(OsString::from).collect(),
        cwd: ctx.src,
        env: ctx.env,
        timeout: Duration::from_secs(60),
        log: None,
        capture_cap: 4096,
        stdout_only: false,
    })
    .map_err(|e| BuildError::Unavailable(format!("cannot run {what}: {e}")))?;
    let line = o.text().lines().next().unwrap_or("").trim().to_string();
    if !o.success() || line.is_empty() {
        return Err(BuildError::Unavailable(format!(
            "`{what}` did not answer ({})",
            o.describe()
        )));
    }
    Ok(line.chars().filter(|c| !c.is_control()).take(200).collect())
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

impl ProcessBuilder {
    fn step(
        &self,
        ctx: &BuildCtx<'_>,
        exe: &Path,
        args: Vec<OsString>,
        cwd: &Path,
        deadline: Instant,
        label: &str,
    ) -> Result<(), BuildError> {
        if remaining(deadline).is_zero() {
            return Err(BuildError::Failed(format!(
                "{label}: the build timeout was used up before it started"
            )));
        }
        let o = process::run(&Spec {
            program: exe,
            args,
            cwd,
            env: ctx.env,
            timeout: remaining(deadline),
            log: Some(ctx.log),
            capture_cap: 0,
            stdout_only: false,
        })
        .map_err(|e| BuildError::Failed(format!("{label}: cannot start: {e}")))?;
        if o.log_truncated
            && let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(ctx.log)
        {
            let _ = std::io::Write::write_all(
                &mut f,
                b"\n[fl install: the log was cut at its size limit; the rest of the output was dropped]\n",
            );
        }
        if o.success() {
            Ok(())
        } else {
            Err(BuildError::Failed(format!("{label}: {}", o.describe())))
        }
    }
}

impl Builder for ProcessBuilder {
    fn toolchain(&self, ctx: &BuildCtx<'_>) -> Result<Toolchain, BuildError> {
        let path = ctx.path_var();
        let cargo = which("cargo", &path)
            .ok_or_else(|| BuildError::Unavailable("cargo is not on PATH".into()))?;
        let rustc = which("rustc", &path)
            .ok_or_else(|| BuildError::Unavailable("rustc is not on PATH".into()))?;
        Ok(Toolchain {
            rustc: tool_line(&rustc, ctx, &["-V"], "rustc -V")?,
            cargo: tool_line(&cargo, ctx, &["-V"], "cargo -V")?,
        })
    }

    fn build(&self, ctx: &BuildCtx<'_>, job: &BuildJob) -> Result<(), BuildError> {
        let deadline = Instant::now() + ctx.timeout;
        let path = ctx.path_var();
        let cargo = which("cargo", &path)
            .ok_or_else(|| BuildError::Unavailable("cargo is not on PATH".into()))?;
        let args = cargo_args(&job.bins)?;
        self.step(ctx, &cargo, args, ctx.src, deadline, "cargo build")?;
        if job.with_ui {
            let npm = which("npm", &path)
                .ok_or_else(|| BuildError::Unavailable("npm is not on PATH (--with-ui)".into()))?;
            let ui = ctx.src.join("ui");
            let words = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
            self.step(
                ctx,
                &npm,
                npm_ci_args(job.npm_scripts),
                &ui,
                deadline,
                "npm ci",
            )?;
            self.step(
                ctx,
                &npm,
                words(&["run", "build"]),
                &ui,
                deadline,
                "npm run build",
            )?;
        }
        Ok(())
    }
}

/// Whether `name` is a variable a build may see (used by tests that dump a build's environment).
pub fn env_name_allowed(name: &str) -> bool {
    ENV_PASSTHROUGH.contains(&name) || ENV_FIXED.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn only_allow_listed_names_reach_a_build() {
        let parent = p(&[
            ("PATH", "/usr/bin:/bin:."),
            ("HOME", "/home/u"),
            ("FL_TOKEN", "t"),
            ("GITHUB_TOKEN", "t"),
            ("AWS_SECRET_ACCESS_KEY", "t"),
            ("FL_VAULT_KEY", "t"),
            ("CARGO_REGISTRY_TOKEN", "t"),
            ("RUSTFLAGS", "-Cevil"),
            ("CARGO_HOME", "/opt/cargo"),
            ("RUSTUP_HOME", "relative/rustup"),
            ("RUSTUP_TOOLCHAIN", "stable"),
            ("FL_RELEASE_ID", "forged"),
            ("SOURCE_DATE_EPOCH", "1"),
        ]);
        let env = sandbox_env(&parent, Path::new("/b/abc"), 1700000000);
        let names: Vec<String> = env
            .iter()
            .map(|(n, _)| n.to_string_lossy().into_owned())
            .collect();
        for n in &names {
            assert!(env_name_allowed(n), "{n} is not allow-listed");
        }
        for banned in [
            "FL_TOKEN",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "FL_VAULT_KEY",
            "CARGO_REGISTRY_TOKEN",
            "RUSTFLAGS",
            "FL_RELEASE_ID",
        ] {
            assert!(!names.iter().any(|n| n == banned), "{banned} leaked");
        }
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        assert_eq!(get("PATH").as_deref(), Some("/usr/bin:/bin"));
        assert_eq!(get("HOME").as_deref(), Some("/b/abc/home"));
        assert_eq!(get("CARGO_HOME").as_deref(), Some("/opt/cargo"));
        assert_eq!(
            get("RUSTUP_HOME"),
            None,
            "a relative toolchain home is dropped"
        );
        assert_eq!(get("RUSTUP_TOOLCHAIN").as_deref(), Some("stable"));
        assert_eq!(get("SOURCE_DATE_EPOCH").as_deref(), Some("1700000000"));
        assert_eq!(get("GIT_CONFIG_GLOBAL").as_deref(), Some("/dev/null"));
    }

    #[test]
    fn npm_ci_ignores_dependency_scripts_unless_asked() {
        let s = |v: Vec<OsString>| -> Vec<String> {
            v.iter().map(|x| x.to_string_lossy().into_owned()).collect()
        };
        assert!(s(npm_ci_args(false)).contains(&"--ignore-scripts".to_string()));
        assert!(!s(npm_ci_args(true)).contains(&"--ignore-scripts".to_string()));
        assert_eq!(s(npm_ci_args(true))[0], "ci");
    }

    #[test]
    fn the_cargo_command_is_locked_release_and_names_each_binary() {
        let a = cargo_args(&["fl".into(), "ferrostore".into()]).unwrap();
        let s: Vec<String> = a.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert_eq!(
            s,
            [
                "build",
                "--release",
                "--locked",
                "-p",
                "fl-cli",
                "-p",
                "fl-store",
                "--bin",
                "fl",
                "--bin",
                "ferrostore"
            ]
        );
        assert!(matches!(
            cargo_args(&["fl-host".into()]),
            Err(BuildError::Unavailable(_))
        ));
    }
}
