//! Gather the doctor's inputs from the file system and run the engine. Reads only:
//! `lstat`, `readlink`, a bounded read of the receipt, of `build-info.json` and of installed
//! `ferro*.service` user units, and a bounded hash of the files the receipt owns. Nothing is
//! created, written, locked or executed, and the vault files are only `lstat`ed.

use crate::fsprobe::{FsProbe, read_bounded};
use crate::ui::read_ui_build_info;
use fl_core::build_info::BuildInfo;
use fl_core::doctor::{
    DoctorInputs, DoctorReport, NodeMeta, ReceiptSource, StoreFacts, UiFacts, UnitFacts,
    UnitFileFact, VaultFacts, VaultNode, run, user_unit_directives,
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

/// The most bytes read from one user unit. A larger file is reported unread, not parsed.
const MAX_UNIT_BYTES: usize = 64 * 1024;
/// The most `ferro*.service` names one report lists. The rest are a note, so the check
/// cannot pass on a partial directory.
const MAX_UNITS: usize = 32;

/// `ferrostore.service`, `ferrostore@.service`, `ferrowire.service`, `ferroloop-hub.service`,
/// and any other `ferro*.service` in the user unit directory.
fn is_ferro_unit(name: &str) -> bool {
    name.starts_with("ferro")
        && name.ends_with(".service")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '_' | '.' | '+' | '-'))
}

fn expand_unit_dir(shown: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    match shown.strip_prefix("~/") {
        Some(rest) => home
            .map(|h| h.join(rest))
            .ok_or_else(|| format!("unit directory {shown} needs HOME")),
        None if shown.starts_with('/') => Ok(PathBuf::from(shown)),
        None => Err(format!("unit directory {shown} is not absolute")),
    }
}

fn read_unit(name: &str, path: &Path) -> UnitFileFact {
    match read_bounded(path, MAX_UNIT_BYTES) {
        Ok(Some(text)) => {
            let d = user_unit_directives(&text);
            UnitFileFact {
                name: name.to_string(),
                readable: true,
                after_default_target: d.after_default_target,
                wanted_by_default_target: d.wanted_by_default_target,
            }
        }
        Ok(None) | Err(_) => UnitFileFact {
            name: name.to_string(),
            readable: false,
            after_default_target: false,
            wanted_by_default_target: false,
        },
    }
}

/// List installed `ferro*.service` units. A missing directory is "none installed", not an error.
fn unit_facts(shown: &str, home: Option<&Path>) -> UnitFacts {
    let dir = match expand_unit_dir(shown, home) {
        Ok(dir) => dir,
        Err(reason) => {
            return UnitFacts {
                note: Some(reason),
                units: Vec::new(),
            };
        }
    };
    // Follow a symlink for the directory itself (`read_dir` does too). Unit files are opened
    // separately and a symlink there is not followed.
    match std::fs::metadata(&dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return UnitFacts {
                note: Some(format!(
                    "unit directory {} is not a directory",
                    dir.display()
                )),
                units: Vec::new(),
            };
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return UnitFacts::default(),
        Err(e) => {
            return UnitFacts {
                note: Some(format!(
                    "unit directory {} could not be listed ({})",
                    dir.display(),
                    e.kind()
                )),
                units: Vec::new(),
            };
        }
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => {
            return UnitFacts {
                note: Some(format!(
                    "unit directory {} could not be listed ({})",
                    dir.display(),
                    e.kind()
                )),
                units: Vec::new(),
            };
        }
    };
    let mut names = Vec::new();
    let mut note = None;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                note = Some(format!(
                    "unit directory {} could not be listed in full ({})",
                    dir.display(),
                    e.kind()
                ));
                break;
            }
        };
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if is_ferro_unit(name) {
            names.push(name.to_string());
        }
    }
    names.sort();
    let omitted = names.len().saturating_sub(MAX_UNITS);
    if omitted > 0 {
        names.truncate(MAX_UNITS);
        let more = format!("+{omitted} more ferro*.service units were not read");
        note = Some(match note {
            Some(first) => format!("{first}; {more}"),
            None => more,
        });
    }
    let units = names
        .iter()
        .map(|name| read_unit(name, &dir.join(name)))
        .collect();
    UnitFacts { note, units }
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
        units: unit_facts(&o.layout.unit_dir, o.home.as_deref()),
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
