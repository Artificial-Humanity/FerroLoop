//! `fl install`: the installer's planning surface (#205 phase A2).
//!
//! Only `--dry-run` exists. It reads a manifest (the built-in one, or
//! `--manifest`), the install receipt if there is one, and `lstat`/`readlink`/
//! read of the paths the plan names. It writes nothing, anywhere.

use clap::Args;
use fl_core::install::{
    FsProbe, HostInfo, InstallReceipt, InstallRequest, MAX_MANIFEST_BYTES, MAX_RECEIPT_BYTES,
    ReleaseManifest, default_manifest, plan, read_bounded, resolve_layout,
};
use std::path::{Path, PathBuf};

/// Exit status of a refused plan, and of `fl install` without `--dry-run`.
pub const EXIT_REFUSED: i32 = 3;

#[derive(Args, Debug)]
pub struct Cmd {
    /// Print the plan and change nothing. Required: installing is not implemented in this release.
    #[arg(long)]
    pub dry_run: bool,
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
    /// Install under this absolute directory (launcher in <PATH>/bin) instead of ~/.local/share/fl.
    /// `$FL_INSTALL_ROOT` isolates the whole default layout under one root.
    #[arg(long, value_name = "PATH")]
    pub prefix: Option<PathBuf>,
    /// Adopt existing unmanaged state. Not implemented in this release.
    #[arg(long)]
    pub adopt: bool,
    /// Print the plan as JSON (schema_version 1).
    #[arg(long)]
    pub json: bool,
}

/// Run the command. A refusal prints to stderr and returns its exit status;
/// the plan goes to stdout.
pub fn run(cmd: Cmd) -> anyhow::Result<i32> {
    match execute(&cmd) {
        Ok(out) => {
            print!("{out}");
            Ok(0)
        }
        Err((code, msg)) => {
            eprintln!("error: {msg}");
            Ok(code)
        }
    }
}

fn execute(cmd: &Cmd) -> Result<String, (i32, String)> {
    if !cmd.dry_run {
        return Err((
            EXIT_REFUSED,
            "installing is not implemented in this release; use --dry-run".into(),
        ));
    }
    let refuse = |e: fl_core::install::PlanError| (e.exit_code(), e.to_string());

    let profile = cmd
        .profile
        .as_deref()
        .map(InstallRequest::parse_profile)
        .transpose()
        .map_err(refuse)?;
    let add = InstallRequest::parse_products(&cmd.add).map_err(refuse)?;
    let remove = InstallRequest::parse_products(&cmd.remove).map_err(refuse)?;
    let layout = resolve_layout(cmd.prefix.as_deref(), std::env::var_os("FL_INSTALL_ROOT"))
        .map_err(refuse)?;
    let host = HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        (
            EXIT_REFUSED,
            format!(
                "unsupported platform {}/{}: supported are linux and macos on x86_64 and aarch64",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        )
    })?;
    let manifest = match &cmd.manifest {
        Some(p) => read_manifest(p)?,
        None => default_manifest(),
    };
    let probe = FsProbe::from_env();
    if layout.prefix.starts_with("~/") && probe.home.is_none() {
        return Err((
            EXIT_REFUSED,
            "cannot locate the install prefix: HOME is not set; pass --prefix".into(),
        ));
    }
    let receipt_display = format!("{}/receipt.json", layout.prefix.trim_end_matches('/'));
    let receipt = read_receipt(&probe, &receipt_display)?;
    let request = InstallRequest {
        profile,
        add,
        remove,
        layout,
        adopt: cmd.adopt,
    };
    let plan = plan(&request, &manifest, receipt.as_ref(), host, &probe).map_err(refuse)?;
    Ok(if cmd.json {
        plan.to_json()
    } else {
        plan.render_text()
    })
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
