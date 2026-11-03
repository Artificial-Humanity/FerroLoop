//! `fl install --from-git <rev>` (#205 A4b): build a reviewed commit and install it through
//! the same transaction as `--artifact-dir`.
//!
//! The order of things, and why:
//!
//! 1. **Read-only.** Open the checkout, resolve `<rev>` to one commit, decide whether it is
//!    reviewed (an ancestor of the local `origin/main`; nothing is fetched), plan the install
//!    and check every conflict decision. A refusal here costs nothing.
//! 2. **Already installed?** If the receipt already records this commit's source build, the
//!    engine's own idempotence test says so and nothing is rebuilt.
//! 3. **Build** in `<prefix>/build/<sha12>/`, a directory this installer made (a marker file
//!    says so): the commit's files are written straight from its objects, a fresh `.git`
//!    gives the build scripts a `HEAD` to stamp, and `cargo build --release --locked` runs
//!    with an allow-listed environment, in its own process group, under a deadline, logging
//!    to `build.log`.
//! 4. **Assemble and verify.** The built binaries are copied into an artifact tree and run
//!    with `--version --json`: they must report this commit, a clean tree and this commit's
//!    time. The tree's digest pins the artifact in an in-memory manifest the A2 validator
//!    accepts.
//! 5. **Install** with [`apply`], unchanged: `--artifact-dir` and `--from-git` differ only in
//!    where the artifact tree came from.
//! 6. The build directory is removed after success and kept after a failure.

use crate::builder::{
    BuildCtx, BuildError, BuildJob, Builder, Toolchain, package_for, sandbox_env,
};
use crate::engine::{
    ApplyArgs, ApplyReport, Decisions, Env, apply_from_source_build, installed_receipt,
    is_installed, plan_only, preflight,
};
use crate::error::{Ctx, InstallError, Result};
use crate::fsprobe::read_bounded;
use crate::git::{Git, Repo, Review, TreeEntry, extract_tree, init_stamp_repo};
use crate::lock::pid_alive;
use crate::process::{self, Spec};
use crate::safe::{Safe, make_dir};
use crate::tree::{MAX_TREE_FILES, remove_tree, scan_tree};
use fl_core::install::{
    Artifact, BuildProvenance, FileKind, HostInfo, InstallPlan, InstallRequest, Product,
    ReleaseKind, ReleaseManifest, default_manifest, file_name_ok,
};
use serde::Serialize;
use serde_json::Value;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The default for `--build-timeout`.
pub const DEFAULT_BUILD_TIMEOUT_SECS: u64 = 45 * 60;
/// The directory under the prefix that builds live in.
pub const BUILD_ROOT: &str = "build";
/// The file that marks a build directory as one this installer made.
pub const MARKER: &str = ".fl-install-build";
const MARKER_MAGIC: &str = "fl-install build directory v1";
/// Shown in a plan made before any build: the toolchain is not known yet.
const NOT_BUILT: &str = "(determined when built)";
const MAX_BINARY_BYTES: u64 = 1024 * 1024 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

/// What `fl install --from-git` was asked.
pub struct FromGitArgs<'a> {
    pub rev: &'a str,
    /// `--repo`; the current directory's checkout when absent.
    pub repo: Option<&'a Path>,
    pub cwd: &'a Path,
    pub allow_unreviewed: bool,
    pub with_ui: bool,
    /// With the UI: run the npm dependencies' lifecycle scripts (`npm ci` without
    /// `--ignore-scripts`).
    pub npm_scripts: bool,
    /// Where warnings go. They are given BEFORE the build starts.
    pub on_warning: &'a dyn Fn(&str),
    pub build_timeout: Duration,
    /// The installer's own environment (only the allow-listed names are ever used).
    pub parent_env: &'a [(OsString, OsString)],
    pub request: &'a InstallRequest,
    pub host: HostInfo,
    pub decisions: &'a Decisions,
}

/// What the commit is and how it would be (or was) built.
#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub rev: String,
    pub commit: String,
    pub commit_time_unix: u64,
    pub reviewed: bool,
    pub origin_main: Option<String>,
    pub version: String,
    pub build_dir: String,
    pub cargo_command: Vec<String>,
    pub with_ui: bool,
    pub notes: Vec<String>,
}

impl SourceInfo {
    pub fn render_text(&self) -> String {
        let mut o = String::new();
        o.push_str(&format!(
            "source    {} -> {} ({})\n",
            self.rev,
            self.commit,
            if self.reviewed {
                "reviewed: an ancestor of origin/main"
            } else {
                "UNREVIEWED: not an ancestor of origin/main"
            }
        ));
        if !self.reviewed {
            o.push_str(
                "WARNING   --allow-unreviewed: this commit is built and installed without \
                 review; the receipt records reviewed: false and `fl doctor` warns\n",
            );
        }
        o.push_str(&format!("build in  {}\n", self.build_dir));
        o.push_str(&format!("build     {}\n", self.cargo_command.join(" ")));
        if self.with_ui {
            o.push_str("ui        npm ci && npm run build (in ui/)\n");
        } else {
            o.push_str("ui        not built (pass --with-ui)\n");
        }
        for n in &self.notes {
            o.push_str(&format!("note: {n}\n"));
        }
        o
    }
}

/// What a run did.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FromGitResult {
    /// `--dry-run` (or no `--yes`): the plan; nothing was built or written.
    Planned { plan: Box<InstallPlan> },
    /// The transaction ran (or found the release already installed).
    Applied {
        report: Box<ApplyReport>,
        /// A build was made (false: the receipt already records this commit).
        built: bool,
    },
}

// ------------------------------------------------------------- prepare

struct Prepared {
    repo: Repo,
    entries: Vec<TreeEntry>,
    sha: String,
    review: Review,
    commit_time: u64,
    version: String,
    units: Vec<String>,
    safe: Safe,
    prefix: PathBuf,
    manifest: ReleaseManifest,
    notes: Vec<String>,
}

fn path_var(env: &[(OsString, OsString)]) -> OsString {
    env.iter()
        .rev()
        .find(|(n, _)| n == "PATH")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `[workspace.package] version = "x"` of a root `Cargo.toml`.
fn workspace_version(toml: &str) -> Option<String> {
    let mut in_pkg = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_pkg = t == "[workspace.package]";
            continue;
        }
        if in_pkg
            && let Some(rest) = t.strip_prefix("version")
            && let Some(v) = rest.trim_start().strip_prefix('=')
        {
            let v = v.trim();
            let v = v.split('#').next().unwrap_or(v).trim();
            if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
                return Some(v[1..v.len() - 1].to_string());
            }
        }
    }
    None
}

/// The built-in manifest narrowed to what this commit and this run can deliver: a unit the
/// commit does not ship is not offered (and no action that names it), and `ui/dist` is only
/// offered when it is built.
fn narrowed_manifest(
    version: &str,
    commit: &str,
    units: &[String],
    with_ui: bool,
    build: BuildProvenance,
    notes: &mut Vec<String>,
) -> ReleaseManifest {
    let mut m = default_manifest();
    m.release.version = version.to_string();
    m.release.kind = ReleaseKind::SourceBuild;
    m.release.commit = Some(commit.to_string());
    m.release.build = Some(build);
    let shipped = |path: &str| {
        path.strip_prefix("contrib/units/")
            .is_some_and(|n| units.iter().any(|u| u == n))
    };
    for p in &mut m.products {
        p.files.retain(|f| match f.kind {
            FileKind::Binary => true,
            FileKind::UnitTemplate => shipped(&f.path),
            FileKind::AssetDir => with_ui,
        });
        let kept: Vec<String> = p.files.iter().map(|f| f.path.clone()).collect();
        let id = p.id.clone();
        let mut dropped: std::collections::BTreeMap<String, Vec<&'static str>> =
            std::collections::BTreeMap::new();
        p.actions.retain(|a| {
            let ok = !a.kind.names_a_unit()
                || kept
                    .iter()
                    .any(|f| f == &format!("contrib/units/{}", a.subject));
            if !ok {
                dropped
                    .entry(a.subject.clone())
                    .or_default()
                    .push(a.kind.as_wire());
            }
            ok
        });
        for (unit, kinds) in dropped {
            notes.push(format!(
                "the commit ships no contrib/units/{unit}: {id} does not offer {}",
                kinds.join(", ")
            ));
        }
    }
    m
}

fn prepare(env: &Env, args: &FromGitArgs<'_>) -> Result<Prepared> {
    let git = Git::find(&path_var(args.parent_env))?;
    let start = args.repo.unwrap_or(args.cwd);
    let repo = git.open(start)?;
    let safe = Safe::new(&env.declared_root, env.home.as_deref())?;
    let prefix = safe.resolve(&args.request.layout.prefix)?;
    if prefix.starts_with(&repo.top) || repo.top.starts_with(&prefix) {
        return Err(InstallError::refused(format!(
            "the checkout {} and the install root {} overlap; the build directory must be \
             outside the source repository and the repository outside the install root",
            repo.top.display(),
            prefix.display()
        )));
    }
    let sha = repo.resolve(args.rev)?;
    let review = repo.review(&sha, now_unix())?;
    if !review.reviewed && !args.allow_unreviewed {
        let why = if review.origin_main.is_some() {
            "is not an ancestor of origin/main"
        } else {
            "cannot be shown to be reviewed: origin/main is absent"
        };
        return Err(InstallError::refused(format!(
            "commit {sha} {why}; only reviewed commits are built (--allow-unreviewed builds it \
             anyway and records reviewed: false). {}",
            review.notes.join(" ")
        )));
    }
    let commit_time = repo.commit_time(&sha)?;
    let entries = repo.list_tree(&sha)?;

    let blob_of = |path: &str| entries.iter().find(|e| e.path == path && e.is_blob());
    let root_toml = blob_of("Cargo.toml")
        .ok_or_else(|| InstallError::refused("the commit has no root Cargo.toml"))?;
    let toml = String::from_utf8(repo.blob(&root_toml.oid, 1024 * 1024)?)
        .map_err(|_| InstallError::refused("the root Cargo.toml is not UTF-8"))?;
    let version = workspace_version(&toml).ok_or_else(|| {
        InstallError::refused("cannot read [workspace.package] version from the root Cargo.toml")
    })?;
    if blob_of("Cargo.lock").is_none() {
        return Err(InstallError::refused(
            "the commit has no Cargo.lock: a --locked build needs one",
        ));
    }
    let mut units: Vec<String> = entries
        .iter()
        .filter(|e| e.is_blob())
        .filter_map(|e| e.path.strip_prefix("contrib/units/"))
        .filter(|n| !n.contains('/') && file_name_ok(n))
        .map(str::to_string)
        .collect();
    units.sort();
    let has_ui = blob_of("ui/package.json").is_some() && blob_of("ui/package-lock.json").is_some();
    if args.with_ui && !has_ui {
        return Err(InstallError::refused(
            "--with-ui: the commit has no ui/package.json with a ui/package-lock.json",
        ));
    }

    let mut notes = review.notes.clone();
    let prov = BuildProvenance {
        reviewed: review.reviewed,
        rustc: NOT_BUILT.into(),
        cargo: NOT_BUILT.into(),
        build_id: sha[..12].to_string(),
        build_time_unix: commit_time,
    };
    let manifest = narrowed_manifest(&version, &sha, &units, args.with_ui, prov, &mut notes);
    manifest.validate().map_err(|e| {
        InstallError::refused(format!("the source-build manifest is invalid: {e:?}"))
    })?;
    Ok(Prepared {
        repo,
        entries,
        sha,
        review,
        commit_time,
        version,
        units,
        safe,
        prefix,
        manifest,
        notes,
    })
}

/// The binaries the selected products need, as `bin/<name>` file names.
fn binaries(plan: &InstallPlan, manifest: &ReleaseManifest) -> Result<Vec<String>> {
    let mut bins: Vec<String> = Vec::new();
    for sp in &plan.products {
        let Some(entry) = manifest.product(sp.id) else {
            continue;
        };
        for f in &entry.files {
            if f.kind == FileKind::Binary
                && let Some(n) = f.path.strip_prefix("bin/")
                && !bins.iter().any(|b| b == n)
            {
                bins.push(n.to_string());
            }
        }
    }
    // The recipe table's order: `fl` first, so the command is the same on every run.
    bins.sort_by_key(|b| {
        crate::builder::BIN_PACKAGES
            .iter()
            .position(|(n, _)| n == b)
            .unwrap_or(usize::MAX)
    });
    for b in &bins {
        if package_for(b).is_none() {
            return Err(InstallError::refused(format!(
                "the selection needs bin/{b}, and the installer has no build recipe for it"
            )));
        }
    }
    if !bins.iter().any(|b| b == "fl") {
        return Err(InstallError::refused(
            "the selection does not include bin/fl",
        ));
    }
    Ok(bins)
}

fn source_info(p: &Prepared, args: &FromGitArgs<'_>, bins: &[String]) -> SourceInfo {
    let cargo_command = crate::builder::cargo_args(bins)
        .map(|a| {
            std::iter::once("cargo".to_string())
                .chain(a.iter().map(|x| x.to_string_lossy().into_owned()))
                .collect()
        })
        .unwrap_or_default();
    SourceInfo {
        rev: args.rev.to_string(),
        commit: p.sha.clone(),
        commit_time_unix: p.commit_time,
        reviewed: p.review.reviewed,
        origin_main: p.review.origin_main.clone(),
        version: p.version.clone(),
        build_dir: p
            .prefix
            .join(BUILD_ROOT)
            .join(&p.sha[..12])
            .display()
            .to_string(),
        cargo_command,
        with_ui: args.with_ui,
        notes: p.notes.clone(),
    }
}

/// The plan of `--from-git <rev>`: resolves the revision, checks the review rule and plans.
/// Builds nothing and writes nothing, not even the build directory.
pub fn plan(env: &Env, args: &FromGitArgs<'_>) -> Result<(SourceInfo, FromGitResult)> {
    let p = prepare(env, args)?;
    let aargs = ApplyArgs {
        request: args.request,
        manifest: &p.manifest,
        host: args.host,
        artifact_dir: None,
        decisions: args.decisions,
    };
    let plan = plan_only(env, &aargs)?;
    let bins = binaries(&plan, &p.manifest)?;
    Ok((
        source_info(&p, args, &bins),
        FromGitResult::Planned {
            plan: Box::new(plan),
        },
    ))
}

// ----------------------------------------------------------- build dir

struct Claimed {
    dir: PathBuf,
}

fn marker_text(sha: &str) -> String {
    format!("{MARKER_MAGIC}\nsha={sha}\npid={}\n", std::process::id())
}

/// What a marker file says: the commit and the pid of the installer that made the directory.
fn parse_marker(text: &str) -> Option<(String, u32)> {
    let mut l = text.lines();
    if l.next()? != MARKER_MAGIC {
        return None;
    }
    let sha = l.next()?.strip_prefix("sha=")?.to_string();
    let pid = l.next()?.strip_prefix("pid=")?.parse().ok()?;
    Some((sha, pid))
}

/// Make `<build_root>/<sha12>/` ours. A directory already there is cleaned only when its
/// marker file says this installer made it for this commit and the installer that did is gone.
fn claim_build_dir(build_root: &Path, sha: &str) -> Result<Claimed> {
    let dir = build_root.join(&sha[..12]);
    match make_dir(&dir, 0o700) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta =
                fs::symlink_metadata(&dir).ctx(|| format!("cannot inspect {}", dir.display()))?;
            let foreign = || {
                InstallError::refused(format!(
                    "{} exists and is not a build directory this installer made; it is not \
                     touched (remove it by hand after checking it)",
                    dir.display()
                ))
            };
            if !meta.is_dir() {
                return Err(foreign());
            }
            let text = read_bounded(&dir.join(MARKER), 512).map_err(|_| foreign())?;
            let Some((msha, pid)) = text.as_deref().and_then(parse_marker) else {
                return Err(foreign());
            };
            if msha != sha {
                return Err(foreign());
            }
            if pid_alive(pid) {
                return Err(InstallError::Busy { pid: Some(pid) });
            }
            remove_tree(&dir)?;
            make_dir(&dir, 0o700).ctx(|| format!("cannot create {}", dir.display()))?;
        }
        Err(e) => {
            return Err(InstallError::io(
                format!("cannot create {}", dir.display()),
                e,
            ));
        }
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(MARKER))
        .ctx(|| "cannot write the build marker".to_string())?;
    std::io::Write::write_all(&mut f, marker_text(sha).as_bytes())
        .ctx(|| "cannot write the build marker".to_string())?;
    for sub in ["src", "home", "tmp", "out"] {
        make_dir(&dir.join(sub), 0o700)
            .ctx(|| format!("cannot create {}", dir.join(sub).display()))?;
    }
    Ok(Claimed { dir })
}

// ---------------------------------------------------------- assemble

fn copy_regular(src: &Path, dest: &Path, mode: u32, max: u64) -> Result<()> {
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(src)
        .ctx(|| format!("cannot open {}", src.display()))?;
    let meta = input
        .metadata()
        .ctx(|| format!("cannot stat {}", src.display()))?;
    if !meta.is_file() {
        return Err(InstallError::BuildFailed {
            what: format!("{} is not a regular file", src.display()),
            log: None,
        });
    }
    if meta.len() > max {
        return Err(InstallError::refused(format!(
            "{} is larger than {max} bytes",
            src.display()
        )));
    }
    if let Some(p) = dest.parent() {
        fs::create_dir_all(p).ctx(|| format!("cannot create {}", p.display()))?;
    }
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dest)
        .ctx(|| format!("cannot create {}", dest.display()))?;
    let n = std::io::copy(&mut (&mut input).take(max + 1), &mut out)
        .ctx(|| format!("cannot copy {}", src.display()))?;
    if n != meta.len() {
        return Err(InstallError::refused(format!(
            "{} changed while it was copied",
            src.display()
        )));
    }
    Ok(())
}

fn copy_tree(src: &Path, dest: &Path, count: &mut usize) -> Result<()> {
    fs::create_dir_all(dest).ctx(|| format!("cannot create {}", dest.display()))?;
    let mut entries: Vec<_> = fs::read_dir(src)
        .ctx(|| format!("cannot read {}", src.display()))?
        .collect::<std::io::Result<_>>()
        .ctx(|| format!("cannot read {}", src.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let m = fs::symlink_metadata(&p).ctx(|| format!("cannot inspect {}", p.display()))?;
        let to = dest.join(e.file_name());
        if m.file_type().is_symlink() {
            return Err(InstallError::refused(format!(
                "the UI build produced a symlink ({}); an artifact holds none",
                p.display()
            )));
        } else if m.is_dir() {
            copy_tree(&p, &to, count)?;
        } else if m.is_file() {
            *count += 1;
            if *count > MAX_TREE_FILES {
                return Err(InstallError::refused(format!(
                    "ui/dist holds more than {MAX_TREE_FILES} files"
                )));
            }
            copy_regular(&p, &to, 0o644, MAX_BINARY_BYTES)?;
        } else {
            return Err(InstallError::refused(format!(
                "{} is not a file or directory",
                p.display()
            )));
        }
    }
    Ok(())
}

/// Run `<bin> --version --json` in the build's environment and parse the answer.
fn version_json(bin: &Path, cwd: &Path, env: &[(OsString, OsString)]) -> Result<Value> {
    let o = process::run(&Spec {
        program: bin,
        args: vec!["--version".into(), "--json".into()],
        cwd,
        env,
        timeout: VERSION_TIMEOUT,
        log: None,
        capture_cap: 64 * 1024,
        stdout_only: true,
    })
    .ctx(|| format!("cannot run {}", bin.display()))?;
    if !o.success() {
        return Err(InstallError::refused(format!(
            "{} --version --json: {}; the build is not installed",
            bin.display(),
            o.describe()
        )));
    }
    serde_json::from_slice(&o.output).map_err(|e| {
        InstallError::refused(format!(
            "{} --version --json is not one JSON object ({e}); the build is not installed",
            bin.display()
        ))
    })
}

/// The built binary must be THIS commit, built from a clean tree, stamped with its time.
fn check_stamp(name: &str, v: &Value, sha: &str, time: u64, version: &str) -> Result<()> {
    let bad =
        |why: String| InstallError::refused(format!("{name} {why}; the build is not installed"));
    if v["commit"].as_str() != Some(sha) {
        return Err(bad(format!(
            "reports commit {}, but the commit built is {sha}",
            v["commit"]
        )));
    }
    if v["dirty"].as_bool() != Some(false) {
        return Err(bad(format!(
            "reports dirty = {} (the build tree must match the commit exactly)",
            v["dirty"]
        )));
    }
    if v["source"].as_str() != Some("source-build") {
        return Err(bad(format!("reports source = {}", v["source"])));
    }
    if v["build_time_unix"].as_u64() != Some(time) {
        return Err(bad(format!(
            "reports build time {}, expected the commit time {time}",
            v["build_time_unix"]
        )));
    }
    if v["version"].as_str() != Some(version) {
        return Err(bad(format!(
            "reports version {}, the commit's workspace version is {version}",
            v["version"]
        )));
    }
    Ok(())
}

fn u32_at(v: &Value, path: &[&str]) -> Option<u32> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_u64().and_then(|n| u32::try_from(n).ok())
}

struct Assembled {
    out: PathBuf,
    manifest: ReleaseManifest,
}

#[allow(clippy::too_many_arguments)]
fn assemble(
    p: &Prepared,
    args: &FromGitArgs<'_>,
    dir: &Path,
    env: &[(OsString, OsString)],
    bins: &[String],
    tool: &Toolchain,
    log: &Path,
) -> Result<Assembled> {
    let src = dir.join("src");
    let artifact_id = format!(
        "src-{}-{}-{}",
        &p.sha[..12],
        args.host.os.as_wire(),
        args.host.arch.as_wire()
    );
    let out = dir.join("out");
    let root = out.join(&artifact_id);
    fs::create_dir_all(&root).ctx(|| format!("cannot create {}", root.display()))?;

    let mut fl_json: Option<Value> = None;
    for b in bins {
        let built = src.join("target/release").join(b);
        let dest = root.join("bin").join(b);
        match fs::symlink_metadata(&built) {
            Ok(m) if m.is_file() => {}
            _ => {
                return Err(InstallError::BuildFailed {
                    what: format!(
                        "the build finished but produced no regular file target/release/{b}"
                    ),
                    log: Some(log.to_path_buf()),
                });
            }
        }
        copy_regular(&built, &dest, 0o755, MAX_BINARY_BYTES)?;
        // The bytes that will be hashed and installed are the bytes that are checked.
        let v = version_json(&dest, &root, env)?;
        check_stamp(&format!("bin/{b}"), &v, &p.sha, p.commit_time, &p.version)?;
        if b == "fl" {
            fl_json = Some(v);
        }
    }
    let fl_json = fl_json.ok_or_else(|| InstallError::refused("bin/fl was not built"))?;
    let protocol = u32_at(&fl_json, &["protocol", "ferrowire", "current"]);
    let smin = u32_at(&fl_json, &["store_formats", "redb_schema", "min"]);
    let smax = u32_at(&fl_json, &["store_formats", "redb_schema", "max"]);
    let (Some(protocol), Some(smin), Some(smax)) = (protocol, smin, smax) else {
        return Err(InstallError::refused(
            "bin/fl --version --json does not report its protocol and store formats; the build \
             is not installed",
        ));
    };

    for u in &p.units {
        copy_regular(
            &src.join("contrib/units").join(u),
            &root.join("contrib/units").join(u),
            0o644,
            1024 * 1024,
        )?;
    }
    if args.with_ui {
        let dist = src.join("ui/dist");
        if !fs::symlink_metadata(&dist).is_ok_and(|m| m.is_dir()) {
            return Err(InstallError::BuildFailed {
                what: "the UI build finished but produced no ui/dist directory".into(),
                log: Some(log.to_path_buf()),
            });
        }
        copy_tree(&dist, &root.join("ui/dist"), &mut 0)?;
    }

    // The build id names the commit and the digest of what was built; release.json (which
    // carries it) joins the tree after the digest is taken.
    let pre = scan_tree(&root, u64::MAX / 2)?;
    let build_id = format!("{}-{}", &p.sha[..12], &pre.digest[..12]);
    let prov = BuildProvenance {
        reviewed: p.review.reviewed,
        rustc: tool.rustc.clone(),
        cargo: tool.cargo.clone(),
        build_id,
        build_time_unix: p.commit_time,
    };
    let info = serde_json::json!({
        "schema_version": 1,
        "name": "fl",
        "version": p.version,
        "commit": p.sha,
        "commit_short": &p.sha[..12],
        "source": "source-build",
        "reviewed": prov.reviewed,
        "rustc": prov.rustc,
        "cargo": prov.cargo,
        "build_id": prov.build_id,
        "build_time_unix": prov.build_time_unix,
    });
    let mut text = serde_json::to_string_pretty(&info).expect("release info serializes");
    text.push('\n');
    crate::tree::write_new(&root.join("release.json"), text.as_bytes())?;
    let fin = scan_tree(&root, u64::MAX / 2)?;
    if fin.total_bytes == 0 {
        return Err(InstallError::refused("the assembled artifact is empty"));
    }

    let mut manifest = p.manifest.clone();
    manifest.compat.protocol_major = protocol;
    manifest.compat.store_format_min = smin;
    manifest.compat.store_format_max = smax;
    manifest.release.build = Some(prov);
    manifest.artifacts = vec![Artifact {
        id: artifact_id.clone(),
        target: args.host.target(),
        file: artifact_id.clone(),
        sha256: fin.digest,
        size: fin.total_bytes,
        provides: Product::ALL.iter().map(|x| x.to_string()).collect(),
    }];
    manifest.validate().map_err(|e| {
        InstallError::refused(format!(
            "the manifest for the built artifact is invalid: {e:?}"
        ))
    })?;
    Ok(Assembled { out, manifest })
}

// --------------------------------------------------------------- install

/// Whether the receipt already records this exact source build: returns the manifest naming
/// the artifact the receipt recorded, for the engine's idempotence test.
fn recorded_build(
    env: &Env,
    p: &Prepared,
    args: &FromGitArgs<'_>,
) -> Result<Option<ReleaseManifest>> {
    let Some(r) = installed_receipt(env, &args.request.layout)? else {
        return Ok(None);
    };
    let same = r.release.kind == ReleaseKind::SourceBuild
        && r.release.commit.as_deref() == Some(p.sha.as_str())
        && r.artifacts.len() == 1
        && r.release
            .build
            .as_ref()
            .is_some_and(|b| b.reviewed == p.review.reviewed);
    if !same {
        return Ok(None);
    }
    let mut m = p.manifest.clone();
    m.release.build = r.release.build.clone();
    let a = &r.artifacts[0];
    m.artifacts = vec![Artifact {
        id: a.id.clone(),
        target: args.host.target(),
        file: a.id.clone(),
        sha256: a.sha256.clone(),
        size: 1,
        provides: Product::ALL.iter().map(|x| x.to_string()).collect(),
    }];
    Ok(Some(m))
}

fn build_err(e: BuildError, log: &Path) -> InstallError {
    match e {
        BuildError::Unavailable(m) => InstallError::refused(m),
        BuildError::Failed(m) => InstallError::BuildFailed {
            what: format!("the build failed: {m}"),
            log: Some(log.to_path_buf()),
        },
    }
}

/// `fl install --from-git <rev> --yes`: build the commit and install it.
pub fn install(
    env: &Env,
    args: &FromGitArgs<'_>,
    builder: &dyn Builder,
) -> Result<(SourceInfo, FromGitResult)> {
    let p = prepare(env, args)?;
    let aargs = ApplyArgs {
        request: args.request,
        manifest: &p.manifest,
        host: args.host,
        artifact_dir: None,
        decisions: args.decisions,
    };
    let planned = plan_only(env, &aargs)?;
    preflight(env, &aargs)?;
    let bins = binaries(&planned, &p.manifest)?;
    let info = source_info(&p, args, &bins);

    // Already installed from this commit: the engine says so, nothing is rebuilt.
    if let Some(m) = recorded_build(env, &p, args)? {
        let again = ApplyArgs {
            manifest: &m,
            ..aargs
        };
        if is_installed(env, &again)? {
            let mut report = apply_from_source_build(env, &again)?;
            report
                .notes
                .push("not rebuilt: the receipt already records this commit's build".into());
            return Ok((
                info,
                FromGitResult::Applied {
                    report: Box::new(report),
                    built: false,
                },
            ));
        }
    }

    // A release directory is immutable and its `release.json` says how it was built. The same
    // commit with another review status is not "the same install", and it cannot be written
    // over the directory: it is refused.
    if let Some(r) = installed_receipt(env, &args.request.layout)?
        && r.release.name == planned.release.name
        && r.release
            .build
            .as_ref()
            .is_some_and(|b| b.reviewed != p.review.reviewed)
    {
        return Err(InstallError::refused(format!(
            "release {} is installed with reviewed: {}, and this commit is now reviewed: {}; a \
             release directory is not changed in place, because its review status is part of \
             what was built (removing a release is not implemented in this release: use \
             another --prefix)",
            planned.release.name, !p.review.reviewed, p.review.reviewed
        )));
    }

    if !p.review.reviewed {
        (args.on_warning)(&format!(
            "WARNING: --allow-unreviewed: commit {} is NOT an ancestor of origin/main. It will \
             be built (its build scripts run as you) and installed unreviewed; the receipt \
             records reviewed: false.",
            p.sha
        ));
    }

    // ---- from here on, files are written
    let mut made: Vec<PathBuf> = p.safe.ensure_dir(&p.prefix)?;
    let build_root = p.prefix.join(BUILD_ROOT);
    made.extend(p.safe.ensure_dir_mode(&build_root, 0o700)?);
    let claimed = match claim_build_dir(&build_root, &p.sha) {
        Ok(c) => c,
        Err(e) => {
            for d in made.iter().rev() {
                let _ = fs::remove_dir(d);
            }
            return Err(e);
        }
    };
    let dir = claimed.dir;
    let mut started = false;
    let mut links_skipped = 0usize;
    let log = dir.join("build.log");

    let result = (|| -> Result<ApplyReport> {
        let src = dir.join("src");
        let written = extract_tree(&p.repo, &p.entries, &src)?;
        links_skipped = written.symlinks_skipped.len();
        init_stamp_repo(&p.repo, &src, &p.sha, &written.symlinks_skipped)?;
        let benv = sandbox_env(args.parent_env, &dir, p.commit_time);
        let ctx = BuildCtx {
            env: &benv,
            src: &src,
            log: &log,
            timeout: args.build_timeout,
        };
        let tool = builder.toolchain(&ctx).map_err(|e| build_err(e, &log))?;
        started = true;
        builder
            .build(
                &ctx,
                &BuildJob {
                    bins: bins.clone(),
                    with_ui: args.with_ui,
                    npm_scripts: args.npm_scripts,
                },
            )
            .map_err(|e| build_err(e, &log))?;
        let a = assemble(&p, args, &dir, &benv, &bins, &tool, &log)?;
        let final_args = ApplyArgs {
            request: args.request,
            manifest: &a.manifest,
            host: args.host,
            artifact_dir: Some(&a.out),
            decisions: args.decisions,
        };
        apply_from_source_build(env, &final_args)
    })();

    match result {
        Ok(mut report) => {
            if links_skipped > 0 {
                report.notes.push(format!(
                    "{links_skipped} symlink(s) in the commit were not written to the build tree"
                ));
            }
            match remove_tree(&dir) {
                Ok(()) => {
                    let _ = fs::remove_dir(&build_root);
                }
                Err(e) => report.notes.push(format!(
                    "the build directory {} could not be removed: {e}",
                    dir.display()
                )),
            }
            Ok((
                info,
                FromGitResult::Applied {
                    report: Box::new(report),
                    built: true,
                },
            ))
        }
        Err(e) if started => Err(InstallError::Kept {
            cause: Box::new(e),
            dir,
        }),
        Err(e) => {
            // Nothing was built: the directory this run made and the ones it made for it go.
            let _ = remove_tree(&dir);
            for d in made.iter().rev() {
                let _ = fs::remove_dir(d);
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workspace_version_is_read_from_its_table_only() {
        let t = "[workspace]\nmembers = []\n\n[workspace.package]\nedition = \"2024\"\nversion = \"1.2.3\" # x\n\n[workspace.dependencies]\nversion = \"9\"\n";
        assert_eq!(workspace_version(t).as_deref(), Some("1.2.3"));
        assert_eq!(workspace_version("[package]\nversion = \"1\"\n"), None);
        assert_eq!(
            workspace_version("[workspace.package]\nversion = 1\n"),
            None
        );
    }

    #[test]
    fn a_marker_round_trips_and_a_foreign_file_is_not_one() {
        let t = marker_text(&"a".repeat(40));
        let (sha, pid) = parse_marker(&t).unwrap();
        assert_eq!(sha, "a".repeat(40));
        assert_eq!(pid, std::process::id());
        assert!(parse_marker("hello\n").is_none());
        assert!(parse_marker(&format!("{MARKER_MAGIC}\nsha=x\npid=zz\n")).is_none());
    }

    #[test]
    fn a_narrowed_manifest_offers_only_what_exists() {
        let mut notes = Vec::new();
        let prov = BuildProvenance {
            reviewed: true,
            rustc: "r".into(),
            cargo: "c".into(),
            build_id: "x".into(),
            build_time_unix: 1,
        };
        let m = narrowed_manifest("0.1.0", &"a".repeat(40), &[], false, prov, &mut notes);
        assert!(m.validate().is_ok());
        for p in &m.products {
            for f in &p.files {
                assert_eq!(f.kind, FileKind::Binary, "{}", f.path);
            }
        }
        assert!(!notes.is_empty());
    }
}
