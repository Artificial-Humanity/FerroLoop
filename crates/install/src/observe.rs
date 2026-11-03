//! Gather the doctor's inputs from the file system and run the engine. Reads only:
//! `lstat`, `readlink`, a bounded read of the receipt and of `build-info.json`, and a bounded
//! hash of the files the receipt owns. Nothing is created, written, locked or executed, and
//! the vault files are only `lstat`ed.

use crate::fsprobe::{FsProbe, read_bounded};
use crate::ui::read_ui_build_info;
use fl_core::build_info::BuildInfo;
use fl_core::doctor::{
    DoctorInputs, DoctorReport, NodeMeta, ReceiptSource, StoreFacts, UiFacts, VaultFacts,
    VaultNode, run,
};
use fl_core::install::{Layout, MAX_RECEIPT_BYTES};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where the UI bundle is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiDir {
    /// Nothing serves a bundle.
    NotServed,
    /// `<prefix>/current/ui/dist` of the install.
    ReleaseCurrent,
    /// A configured directory.
    Explicit(PathBuf),
}

/// What to observe. The caller resolves paths (it knows the config and the environment) and
/// verifies the stores (`fl_store::health`); `observe` does the rest.
pub struct Observe<'a> {
    pub binary: BuildInfo,
    pub binary_name: &'a str,
    /// What a leading `~/` in the layout expands to.
    pub home: Option<PathBuf>,
    pub layout: Layout,
    pub ui_dir: UiDir,
    /// The vault file and the master key file, as the vault resolves them.
    pub vault_path: PathBuf,
    pub key_path: PathBuf,
    /// Whether `FL_VAULT_MASTER_KEY` is set (never its value).
    pub key_in_env: bool,
    pub stores: StoreFacts,
    /// The most time the path hashing may take; past it the remaining paths are reported
    /// unchecked.
    pub budget: Duration,
}

/// `lstat` one path into [`NodeMeta`].
pub fn node_meta(path: &Path) -> NodeMeta {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return NodeMeta::Absent,
        Err(e) => return NodeMeta::Unreadable(e.kind().to_string()),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        return NodeMeta::Symlink;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if ft.is_file() {
            return NodeMeta::Regular {
                mode: meta.mode(),
                uid: meta.uid(),
            };
        }
        if ft.is_dir() {
            return NodeMeta::Dir {
                mode: meta.mode(),
                uid: meta.uid(),
            };
        }
    }
    NodeMeta::Other
}

fn vault_facts(o: &Observe<'_>) -> VaultFacts {
    if !cfg!(unix) {
        return VaultFacts {
            supported: false,
            ..VaultFacts::default()
        };
    }
    let node = |role: &str, path: &Path| VaultNode {
        role: role.into(),
        path: path.display().to_string(),
        node: node_meta(path),
        parent: path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or(NodeMeta::Absent, node_meta),
    };
    #[cfg(unix)]
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = Some(unsafe { libc::geteuid() });
    #[cfg(not(unix))]
    let euid = None;
    VaultFacts {
        supported: true,
        euid,
        key_in_env: o.key_in_env,
        files: vec![
            node("vault", &o.vault_path),
            node("master key", &o.key_path),
        ],
    }
}

/// Gather and run.
pub fn observe(o: Observe<'_>) -> DoctorReport {
    let probe = FsProbe::new(o.home.clone()).with_deadline(Instant::now() + o.budget);
    let prefix = o.layout.prefix.trim_end_matches('/').to_string();
    let receipt_shown = format!("{prefix}/receipt.json");
    let receipt = match probe.resolve(&receipt_shown) {
        Err(why) => ReceiptSource::Unlocatable(format!("{why}; pass --prefix")),
        Ok(real) => match read_bounded(&real, MAX_RECEIPT_BYTES) {
            Ok(None) => ReceiptSource::Absent,
            Ok(Some(text)) => ReceiptSource::Text(text),
            Err(why) => ReceiptSource::Unreadable(why),
        },
    };
    let ui = match &o.ui_dir {
        UiDir::NotServed => UiFacts::default(),
        UiDir::Explicit(dir) => ui_facts(dir.display().to_string(), dir),
        UiDir::ReleaseCurrent => {
            let shown = format!("{prefix}/current/ui/dist");
            match probe.resolve(&shown) {
                Ok(real) => ui_facts(shown, &real),
                Err(_) => UiFacts::default(),
            }
        }
    };
    let inputs = DoctorInputs {
        vault: vault_facts(&o),
        binary: o.binary,
        binary_name: o.binary_name,
        receipt_shown,
        receipt,
        probe: &probe,
        stores: o.stores,
        ui,
    };
    run(&inputs)
}

fn ui_facts(shown: String, real: &Path) -> UiFacts {
    let dir_exists = std::fs::metadata(real).is_ok_and(|m| m.is_dir());
    let info = dir_exists.then(|| read_ui_build_info(real)).flatten();
    UiFacts {
        dir: Some(shown),
        dir_exists,
        info,
    }
}
