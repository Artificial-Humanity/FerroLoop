//! `fl install`: plan, and with `--yes` apply (#205 phases A2, A4a and A4b).
//!
//! `--dry-run` reads a manifest (the built-in one, or `--manifest`), the install receipt if
//! there is one, and `lstat`/`readlink`/read of the paths the plan names, and writes nothing,
//! anywhere. `--yes` applies THE SAME plan as a journaled transaction (`fl_install::apply`),
//! from a local `--artifact-dir`. `--resume` / `--rollback` finish or undo an interrupted one.

use clap::Args;
use fl_core::install::{
    HostInfo, InstallReceipt, InstallRequest, MAX_MANIFEST_BYTES, MAX_RECEIPT_BYTES,
    ReleaseManifest, default_manifest, plan, resolve_layout,
};
use fl_install::{
    ApplyArgs, Decisions, Env, FromGitArgs, FromGitResult, FsProbe, ProcessBuilder, RecoverMode,
    apply, install_from_git, manager_for, plan_from_git, read_bounded, recover,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Exit status of a refused plan, and of `fl install` without `--dry-run` or `--yes`.
pub const EXIT_REFUSED: i32 = 3;

#[derive(Args, Debug)]
pub struct Cmd {
    /// Print the plan and change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Apply the plan (without it, `fl install` prints the plan and exits 3).
    #[arg(long)]
    pub yes: bool,
    /// Start from a profile: minimal, headless, full or lab (lab is a superset of full).
    #[arg(long, value_name = "PROFILE")]
    pub profile: Option<String>,
    /// Also install this product, with its real dependencies (repeatable).
    #[arg(long = "add", value_name = "PRODUCT")]
    pub add: Vec<String>,
    /// Do not install this product (repeatable). Refused while another selected product needs it.
    #[arg(long = "remove", value_name = "PRODUCT")]
    pub remove: Vec<String>,
    /// Read the release manifest from this file instead of the built-in one.
    #[arg(long, value_name = "PATH")]
    pub manifest: Option<PathBuf>,
    /// A local directory of EXTRACTED artifacts the manifest names; each artifact's `file` is a
    /// directory under it, pinned by the manifest's tree digest.
    #[arg(long, value_name = "DIR")]
    pub artifact_dir: Option<PathBuf>,
    /// Install under this absolute directory (launcher in <PATH>/bin) instead of ~/.local/share/fl.
    /// `$FL_INSTALL_ROOT` isolates the whole default layout under one root.
    #[arg(long, value_name = "PATH")]
    pub prefix: Option<PathBuf>,
    /// For a conflict: move this path to <prefix>/backups/<n>/ and install over the empty slot
    /// (repeatable).
    #[arg(long = "replace", value_name = "PATH")]
    pub replace: Vec<String>,
    /// For a conflict: leave this path alone and skip that slot (repeatable; launcher and
    /// aliases only).
    #[arg(long = "keep", value_name = "PATH")]
    pub keep: Vec<String>,
    /// Finish an interrupted install when its staged data verifies, otherwise undo it.
    #[arg(long)]
    pub resume: bool,
    /// Undo an interrupted install.
    #[arg(long)]
    pub rollback: bool,
    /// Seconds to wait for another installer's lock (0: do not wait).
    #[arg(long, value_name = "SECONDS", default_value_t = fl_install::DEFAULT_LOCK_TIMEOUT_SECS)]
    pub lock_timeout: u64,
    /// Build the commit REV (a revision of --repo) in a clean directory under the install
    /// prefix and install the result. The commit must be an ancestor of the local origin/main
    /// unless --allow-unreviewed is given. `--dry-run` plans without building.
    #[arg(long, value_name = "REV")]
    pub from_git: Option<String>,
    /// With --from-git: the git checkout holding REV (default: the current directory's).
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// With --from-git: build a commit that is not an ancestor of origin/main. Loud, and the
    /// receipt records `reviewed: false`.
    #[arg(long)]
    pub allow_unreviewed: bool,
    /// With --from-git: also build the UI (`npm ci && npm run build` in ui/).
    #[arg(long)]
    pub with_ui: bool,
    /// With --from-git: kill the build after this many seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = fl_install::DEFAULT_BUILD_TIMEOUT_SECS)]
    pub build_timeout: u64,
    /// Clear old releases. Not implemented in this release.
    #[arg(long)]
    pub gc: bool,
    /// Adopt existing unmanaged state. Not implemented in this release.
    #[arg(long)]
    pub adopt: bool,
    /// Print the plan (or the result) as JSON.
    #[arg(long)]
    pub json: bool,
}

/// What a run prints and how it exits.
struct Done {
    code: i32,
    stdout: String,
    stderr: Option<String>,
}

impl Done {
    fn ok(stdout: String) -> Self {
        Self {
            code: 0,
            stdout,
            stderr: None,
        }
    }

    fn fail(code: i32, msg: String) -> Self {
        Self {
            code,
            stdout: String::new(),
            stderr: Some(msg),
        }
    }
}

/// Run the command. A refusal prints to stderr and returns its exit status;
/// the plan goes to stdout.
pub fn run(cmd: Cmd) -> anyhow::Result<i32> {
    let done = execute(&cmd);
    print!("{}", done.stdout);
    if let Some(m) = done.stderr {
        eprintln!("error: {m}");
    }
    Ok(done.code)
}

fn execute(cmd: &Cmd) -> Done {
    let usage = |m: &str| Done::fail(2, m.to_string());
    let not_implemented = |what: &str| {
        Done::fail(
            EXIT_REFUSED,
            format!("{what} is not implemented in this release"),
        )
    };
    if cmd.from_git.is_none() && (cmd.repo.is_some() || cmd.allow_unreviewed || cmd.with_ui) {
        return usage("--repo, --allow-unreviewed and --with-ui go with --from-git");
    }
    if cmd.from_git.is_some() {
        if cmd.artifact_dir.is_some() || cmd.manifest.is_some() {
            return usage(
                "--from-git builds its own artifact: it cannot be combined with --artifact-dir \
                 or --manifest",
            );
        }
        if cmd.resume || cmd.rollback {
            return usage("--from-git cannot be combined with --resume or --rollback");
        }
    }
    if cmd.gc {
        return not_implemented("--gc");
    }
    if cmd.dry_run && (cmd.yes || cmd.resume || cmd.rollback) {
        return usage(
            "--dry-run changes nothing; it cannot be combined with --yes, --resume or --rollback",
        );
    }
    if cmd.resume && cmd.rollback {
        return usage("choose one of --resume and --rollback");
    }
    if (cmd.resume || cmd.rollback) && cmd.yes {
        return usage("--resume and --rollback take no --yes: they finish or undo what was begun");
    }
    let refuse = |e: fl_core::install::PlanError| Done::fail(e.exit_code(), e.to_string());

    let layout = match resolve_layout(cmd.prefix.as_deref(), std::env::var_os("FL_INSTALL_ROOT")) {
        Ok(l) => l,
        Err(e) => return refuse(e),
    };
    let probe = FsProbe::from_env();
    if layout.prefix.starts_with("~/") && probe.home.is_none() {
        return Done::fail(
            EXIT_REFUSED,
            "cannot locate the install prefix: HOME is not set; pass --prefix".into(),
        );
    }
    let declared_root = declared_root(cmd, &probe);

    if cmd.resume || cmd.rollback {
        let Some(root) = declared_root else {
            return Done::fail(EXIT_REFUSED, "HOME is not set; pass --prefix".into());
        };
        let mut env = Env::new(root, probe.home.clone());
        env.lock_timeout = Duration::from_secs(cmd.lock_timeout);
        env.faults = fl_install::faults_from_env();
        let mode = if cmd.rollback {
            RecoverMode::Rollback
        } else {
            RecoverMode::Auto
        };
        return match recover(&env, &layout, mode) {
            Ok(r) if cmd.json => Done::ok(serde_json::to_string_pretty(&r).unwrap() + "\n"),
            Ok(r) => Done::ok(r.render_text()),
            Err(e) => Done::fail(e.exit_code(), e.to_string()),
        };
    }

    let profile = match cmd
        .profile
        .as_deref()
        .map(InstallRequest::parse_profile)
        .transpose()
    {
        Ok(p) => p,
        Err(e) => return refuse(e),
    };
    let add = match InstallRequest::parse_products(&cmd.add) {
        Ok(v) => v,
        Err(e) => return refuse(e),
    };
    let remove = match InstallRequest::parse_products(&cmd.remove) {
        Ok(v) => v,
        Err(e) => return refuse(e),
    };
    let host = match HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH) {
        Some(h) => h,
        None => {
            return Done::fail(
                EXIT_REFUSED,
                format!(
                    "unsupported platform {}/{}: supported are linux and macos on x86_64 and aarch64",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            );
        }
    };
    if let Some(rev) = &cmd.from_git {
        let request = InstallRequest {
            profile,
            add,
            remove,
            layout,
            adopt: cmd.adopt,
        };
        return from_git(cmd, rev, &request, host, declared_root, &probe);
    }
    let manifest = match &cmd.manifest {
        Some(p) => match read_manifest(p) {
            Ok(m) => m,
            Err((c, m)) => return Done::fail(c, m),
        },
        None => default_manifest(),
    };
    let request = InstallRequest {
        profile,
        add,
        remove,
        layout,
        adopt: cmd.adopt,
    };

    if cmd.yes {
        let Some(root) = declared_root else {
            return Done::fail(EXIT_REFUSED, "HOME is not set; pass --prefix".into());
        };
        let mut env = Env::new(root, probe.home.clone());
        env.lock_timeout = Duration::from_secs(cmd.lock_timeout);
        env.faults = fl_install::faults_from_env();
        env.services = manager_for(host.os);
        let decisions = Decisions {
            replace: cmd.replace.clone(),
            keep: cmd.keep.clone(),
        };
        let args = ApplyArgs {
            request: &request,
            manifest: &manifest,
            host,
            artifact_dir: cmd.artifact_dir.as_deref(),
            decisions: &decisions,
        };
        return match apply(&env, &args) {
            Ok(r) if cmd.json => Done::ok(r.to_json()),
            Ok(r) => Done::ok(r.render_text()),
            Err(e) => Done::fail(e.exit_code(), e.to_string()),
        };
    }

    // A plan: `--dry-run` prints it and succeeds; a bare `fl install` prints it and refuses.
    let receipt_display = format!(
        "{}/receipt.json",
        request.layout.prefix.trim_end_matches('/')
    );
    let receipt = match read_receipt(&probe, &receipt_display) {
        Ok(r) => r,
        Err((c, m)) => return Done::fail(c, m),
    };
    let plan = match plan(&request, &manifest, receipt.as_ref(), host, &probe) {
        Ok(p) => p,
        Err(e) => return refuse(e),
    };
    let out = if cmd.json {
        plan.to_json()
    } else {
        plan.render_text()
    };
    if cmd.dry_run {
        Done::ok(out)
    } else {
        Done {
            code: EXIT_REFUSED,
            stdout: out,
            stderr: Some("pass --yes to apply (or --dry-run)".into()),
        }
    }
}

/// `fl install --from-git <rev>`: plan (`--dry-run`, or no `--yes`) or build and apply.
fn from_git(
    cmd: &Cmd,
    rev: &str,
    request: &InstallRequest,
    host: HostInfo,
    root: Option<PathBuf>,
    probe: &FsProbe,
) -> Done {
    let Some(root) = root else {
        return Done::fail(EXIT_REFUSED, "HOME is not set; pass --prefix".into());
    };
    let Ok(cwd) = std::env::current_dir() else {
        return Done::fail(EXIT_REFUSED, "cannot read the current directory".into());
    };
    let mut env = Env::new(root, probe.home.clone());
    env.lock_timeout = Duration::from_secs(cmd.lock_timeout);
    env.faults = fl_install::faults_from_env();
    env.services = manager_for(host.os);
    let decisions = Decisions {
        replace: cmd.replace.clone(),
        keep: cmd.keep.clone(),
    };
    let parent_env: Vec<_> = std::env::vars_os().collect();
    let args = FromGitArgs {
        rev,
        repo: cmd.repo.as_deref(),
        cwd: &cwd,
        allow_unreviewed: cmd.allow_unreviewed,
        with_ui: cmd.with_ui,
        build_timeout: Duration::from_secs(cmd.build_timeout),
        parent_env: &parent_env,
        request,
        host,
        decisions: &decisions,
    };
    let result = if cmd.yes {
        install_from_git(&env, &args, &ProcessBuilder)
    } else {
        plan_from_git(&env, &args)
    };
    let (info, res) = match result {
        Ok(r) => r,
        Err(e) => return Done::fail(e.exit_code(), e.to_string()),
    };
    match res {
        FromGitResult::Planned { plan } => {
            let out = if cmd.json {
                let v = serde_json::json!({
                    "from_git": info,
                    "plan": serde_json::from_str::<serde_json::Value>(&plan.to_json())
                        .unwrap_or_default(),
                });
                serde_json::to_string_pretty(&v).unwrap() + "\n"
            } else {
                format!("{}\n{}", info.render_text(), plan.render_text())
            };
            if cmd.dry_run {
                Done::ok(out)
            } else {
                Done {
                    code: EXIT_REFUSED,
                    stdout: out,
                    stderr: Some("pass --yes to build and apply (or --dry-run)".into()),
                }
            }
        }
        FromGitResult::Applied { report, built } => {
            if cmd.json {
                let v = serde_json::json!({
                    "from_git": info,
                    "built": built,
                    "result": serde_json::from_str::<serde_json::Value>(&report.to_json())
                        .unwrap_or_default(),
                });
                Done::ok(serde_json::to_string_pretty(&v).unwrap() + "\n")
            } else {
                Done::ok(format!("{}\n{}", info.render_text(), report.render_text()))
            }
        }
    }
}

/// Where this install may write: `--prefix`, else `$FL_INSTALL_ROOT`, else `$HOME`.
fn declared_root(cmd: &Cmd, probe: &FsProbe) -> Option<PathBuf> {
    if let Some(p) = &cmd.prefix {
        return Some(p.clone());
    }
    match std::env::var_os("FL_INSTALL_ROOT").filter(|r| !r.is_empty()) {
        Some(r) => Some(PathBuf::from(r)),
        None => probe.home.clone(),
    }
}

fn read_manifest(path: &Path) -> Result<ReleaseManifest, (i32, String)> {
    let shown = path.display();
    let raw = read_bounded(path, MAX_MANIFEST_BYTES)
        .map_err(|e| (EXIT_REFUSED, format!("manifest {shown}: {e}")))?
        .ok_or_else(|| (EXIT_REFUSED, format!("manifest {shown} does not exist")))?;
    ReleaseManifest::parse(&raw).map_err(|errs| {
        (
            EXIT_REFUSED,
            format!(
                "manifest {shown} is refused: {}",
                errs.iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        )
    })
}

/// The receipt at `display` (the unexpanded path), if there is one. A receipt
/// that exists but cannot be read is a refusal, never "unmanaged".
fn read_receipt(probe: &FsProbe, display: &str) -> Result<Option<InstallReceipt>, (i32, String)> {
    let real = probe.resolve(display).map_err(|e| {
        (
            EXIT_REFUSED,
            format!("cannot locate the install receipt: {e}"),
        )
    })?;
    let raw = read_bounded(&real, MAX_RECEIPT_BYTES).map_err(|e| {
        (
            EXIT_REFUSED,
            format!(
                "install receipt {display} cannot be read ({e}); it is not treated as unmanaged"
            ),
        )
    })?;
    match raw {
        None => Ok(None),
        Some(raw) => InstallReceipt::parse(&raw).map(Some).map_err(|e| {
            (
                EXIT_REFUSED,
                format!(
                    "install receipt {display} is invalid ({e}); it is not treated as unmanaged"
                ),
            )
        }),
    }
}
