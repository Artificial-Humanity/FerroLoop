//! `fl install`: the installer's planning surface (#205 phase A2).
//!
//! Only `--dry-run` exists. It reads a manifest (the built-in one, or
//! `--manifest`), the install receipt if there is one, and `lstat`/`readlink`/
//! read of the paths the plan names. It writes nothing, anywhere.

use clap::Args;
use fl_core::install::{
    HostInfo, HostProbe, InstallReceipt, InstallRequest, Layout, MAX_FINGERPRINT_BYTES,
    MAX_MANIFEST_BYTES, MAX_RECEIPT_BYTES, PathState, ReleaseManifest, default_manifest,
    fingerprint, plan,
};
use std::fs;
use std::io::Read;
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
    let probe = FsProbe {
        home: std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from),
    };
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

/// `--prefix`, then `$FL_INSTALL_ROOT`, then the per-user default.
fn resolve_layout(
    prefix: Option<&Path>,
    root: Option<std::ffi::OsString>,
) -> Result<Layout, fl_core::install::PlanError> {
    use fl_core::install::PlanError;
    let utf8 = |p: &std::ffi::OsStr, what: &str| {
        p.to_str()
            .map(str::to_owned)
            .ok_or_else(|| PlanError::PrefixInvalid(format!("{what} is not valid UTF-8")))
    };
    if let Some(p) = prefix {
        return Layout::explicit(&utf8(p.as_os_str(), "--prefix")?);
    }
    match root.filter(|r| !r.is_empty()) {
        Some(r) => Layout::under_root(&utf8(&r, "$FL_INSTALL_ROOT")?),
        None => Ok(Layout::user_default()),
    }
}

/// Read at most `max` bytes of a regular file. Refuses a symlink, a directory
/// and anything larger.
fn read_bounded(path: &Path, max: usize) -> Result<Option<String>, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.kind().to_string()),
    };
    if !meta.file_type().is_file() {
        return Err("is not a regular file".into());
    }
    if meta.len() > max as u64 {
        return Err(format!("is larger than {max} bytes"));
    }
    let mut buf = String::new();
    fs::File::open(path)
        .and_then(|f| f.take(max as u64 + 1).read_to_string(&mut buf))
        .map_err(|e| e.kind().to_string())?;
    Ok(Some(buf))
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

/// A failure while reading a file the probe opened, in words that do not depend
/// on the platform's error text.
fn read_failure(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::InvalidData => e.to_string(),
        k => k.to_string(),
    }
}

/// Open `path` to read it, without blocking and without following a symlink,
/// then check THAT descriptor is a regular file of bounded size. The `lstat`
/// that chose to open it may be stale; a FIFO swapped in since would otherwise
/// block the open, and a symlink swapped in would be followed. `Ok(None)`: not
/// a regular file any more.
fn open_regular(path: &Path) -> Result<Option<fs::File>, String> {
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let f = opts.open(path).map_err(|e| e.kind().to_string())?;
    let meta = f.metadata().map_err(|e| e.kind().to_string())?;
    if !meta.is_file() {
        return Ok(None);
    }
    if meta.len() > MAX_FINGERPRINT_BYTES {
        return Err(format!("file is larger than {MAX_FINGERPRINT_BYTES} bytes"));
    }
    Ok(Some(f))
}

/// The real probe: `lstat`, `readlink`, and a read to hash a file. No write.
struct FsProbe {
    home: Option<PathBuf>,
}

impl FsProbe {
    fn resolve(&self, shown: &str) -> Result<PathBuf, String> {
        match shown.strip_prefix("~/") {
            Some(rest) => self
                .home
                .as_ref()
                .map(|h| h.join(rest))
                .ok_or_else(|| "HOME is not set".to_string()),
            None => Ok(PathBuf::from(shown)),
        }
    }
}

impl HostProbe for FsProbe {
    fn probe(&self, shown: &str) -> PathState {
        let path = match self.resolve(shown) {
            Ok(p) => p,
            Err(reason) => return PathState::Unreadable { reason },
        };
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PathState::Missing,
            Err(e) => {
                return PathState::Unreadable {
                    reason: e.kind().to_string(),
                };
            }
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            match fs::read_link(&path) {
                Ok(t) => PathState::Symlink {
                    target: t.to_string_lossy().into_owned(),
                },
                Err(e) => PathState::Unreadable {
                    reason: e.kind().to_string(),
                },
            }
        } else if ft.is_dir() {
            PathState::Dir
        } else if ft.is_file() {
            match open_regular(&path) {
                Ok(Some(f)) => match fingerprint(f) {
                    Ok(fp) => PathState::File(fp),
                    Err(e) => PathState::Unreadable {
                        reason: read_failure(&e),
                    },
                },
                // Replaced by a non-file between the lstat and the open.
                Ok(None) => PathState::Other,
                Err(reason) => PathState::Unreadable { reason },
            }
        } else {
            PathState::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fifo_is_neither_opened_blocking_nor_read() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        // The open returns at once (O_NONBLOCK) and the descriptor is not a file.
        assert!(matches!(open_regular(&fifo), Ok(None)));
        let probe = FsProbe { home: None };
        assert_eq!(probe.probe(fifo.to_str().unwrap()), PathState::Other);
    }

    #[test]
    fn a_symlink_swapped_in_after_the_lstat_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        fs::write(&target, "x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_regular(&link).is_err(), "O_NOFOLLOW must refuse it");
    }

    #[test]
    fn an_oversized_file_is_reported_unreadable_without_being_read() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        let f = fs::File::create(&big).unwrap();
        f.set_len(MAX_FINGERPRINT_BYTES + 1).unwrap(); // sparse: no data is written
        let probe = FsProbe { home: None };
        match probe.probe(big.to_str().unwrap()) {
            PathState::Unreadable { reason } => assert!(reason.contains("larger than"), "{reason}"),
            other => panic!("{other:?}"),
        }
        let ok = dir.path().join("ok");
        fs::write(&ok, "hello").unwrap();
        assert!(matches!(
            probe.probe(ok.to_str().unwrap()),
            PathState::File(_)
        ));
    }

    #[test]
    fn prefix_beats_the_isolated_root_beats_the_default() {
        let l = resolve_layout(Some(Path::new("/work/p")), Some("/root1".into())).unwrap();
        assert_eq!(l.prefix, "/work/p");
        assert_eq!(l.bin_dir, "/work/p/bin");
        let l = resolve_layout(None, Some("/work/r".into())).unwrap();
        assert_eq!(l.prefix, "/work/r/.local/share/fl");
        assert_eq!(l.bin_dir, "/work/r/.local/bin");
        let l = resolve_layout(None, None).unwrap();
        assert_eq!(l.prefix, "~/.local/share/fl");
        assert_eq!(
            resolve_layout(None, Some("".into())).unwrap().prefix,
            "~/.local/share/fl"
        );
        assert!(resolve_layout(Some(Path::new("/usr")), None).is_err());
        assert!(resolve_layout(None, Some("/".into())).is_err());
    }
}
