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
use fl_core::install::{BUDGET_EXCEEDED, InstallReceipt, Layout, MAX_RECEIPT_BYTES, OwnedUnit};
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
/// Every `read_dir` entry counts, not only `ferro*` names. One past this stops the scan.
const MAX_DIR_ENTRIES: usize = 4096;

/// `ferrostore.service`, `ferrostore@.service`, `ferrowire.service`, `ferroloop-hub.service`,
/// and any other `ferro*.service` in the user unit directory.
///
/// `:` and `\` belong in a unit name (`ferrostore@foo:bar.service`, a `\x` escape). `/` does
/// not: `name` is one `read_dir` component and must not leave that directory.
fn is_ferro_unit(name: &str) -> bool {
    name.starts_with("ferro")
        && name.ends_with(".service")
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '@' | '_' | '.' | '+' | '-' | ':' | '\\')
        })
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

struct LocatedUnit {
    name: String,
    path: PathBuf,
}

enum Scan {
    /// The time budget ran out. Do not read unit files.
    Budget,
    Names(Vec<String>),
}

fn budget_exceeded(probe: &FsProbe) -> bool {
    probe
        .deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
}

fn push_note(note: &mut Option<String>, more: &str) {
    match note {
        Some(first) => {
            first.push_str("; ");
            first.push_str(more);
        }
        None => *note = Some(more.to_string()),
    }
}

/// List installed `ferro*.service` units. A missing directory is "none from the directory",
/// not an error. A parsed receipt selects `layout.unit_dir` and any owned unit file whose
/// name was not in that listing.
fn unit_facts(
    fallback: &str,
    home: Option<&Path>,
    probe: &FsProbe,
    receipt: &ReceiptSource,
) -> UnitFacts {
    let parsed = match receipt {
        ReceiptSource::Text(text) => InstallReceipt::parse(text).ok(),
        _ => None,
    };
    let shown = parsed
        .as_ref()
        .map(|r| r.layout.unit_dir.as_str())
        .unwrap_or(fallback);

    let mut note = None;
    let mut located = Vec::new();
    let mut stop_reads = false;
    match expand_unit_dir(shown, home) {
        Err(reason) => push_note(&mut note, &reason),
        Ok(dir) => match scan_unit_dir(&dir, probe, &mut note) {
            Scan::Budget => stop_reads = true,
            Scan::Names(names) => {
                for name in names {
                    located.push(LocatedUnit {
                        path: dir.join(&name),
                        name,
                    });
                }
            }
        },
    }
    if stop_reads {
        return UnitFacts {
            note,
            units: Vec::new(),
        };
    }
    if let Some(parsed) = &parsed {
        push_receipt_units(&parsed.units, home, &mut located);
    }
    located.sort_by(|a, b| a.name.cmp(&b.name));
    let omitted = located.len().saturating_sub(MAX_UNITS);
    if omitted > 0 {
        located.truncate(MAX_UNITS);
        push_note(
            &mut note,
            &format!("+{omitted} more ferro*.service units were not read"),
        );
    }
    let mut units = Vec::with_capacity(located.len());
    for unit in &located {
        if budget_exceeded(probe) {
            push_note(&mut note, BUDGET_EXCEEDED);
            break;
        }
        units.push(read_unit(&unit.name, &unit.path));
    }
    UnitFacts { note, units }
}

fn push_receipt_units(units: &[OwnedUnit], home: Option<&Path>, located: &mut Vec<LocatedUnit>) {
    for unit in units {
        if !is_ferro_unit(&unit.name) || located.iter().any(|have| have.name == unit.name) {
            continue;
        }
        let Ok(path) = expand_unit_dir(&unit.path, home) else {
            continue;
        };
        // The final component must be the unit name. Do not canonicalize or follow links;
        // `read_unit` opens with `O_NOFOLLOW`.
        if path.file_name().and_then(|name| name.to_str()) != Some(unit.name.as_str()) {
            continue;
        }
        located.push(LocatedUnit {
            name: unit.name.clone(),
            path,
        });
    }
}

fn scan_unit_dir(dir: &Path, probe: &FsProbe, note: &mut Option<String>) -> Scan {
    if budget_exceeded(probe) {
        push_note(note, BUDGET_EXCEEDED);
        return Scan::Budget;
    }
    // Follow a symlink for the directory itself (`read_dir` does too). Unit files are opened
    // separately and a symlink there is not followed.
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            push_note(
                note,
                &format!("unit directory {} is not a directory", dir.display()),
            );
            return Scan::Names(Vec::new());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Scan::Names(Vec::new()),
        Err(e) => {
            push_note(
                note,
                &format!(
                    "unit directory {} could not be listed ({})",
                    dir.display(),
                    e.kind()
                ),
            );
            return Scan::Names(Vec::new());
        }
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            push_note(
                note,
                &format!(
                    "unit directory {} could not be listed ({})",
                    dir.display(),
                    e.kind()
                ),
            );
            return Scan::Names(Vec::new());
        }
    };
    let mut names = Vec::new();
    for (scanned, entry) in entries.enumerate() {
        if budget_exceeded(probe) {
            push_note(note, BUDGET_EXCEEDED);
            return Scan::Budget;
        }
        if scanned == MAX_DIR_ENTRIES {
            push_note(
                note,
                &format!("unit directory scan stopped at {MAX_DIR_ENTRIES} entries"),
            );
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                push_note(
                    note,
                    &format!(
                        "unit directory {} could not be listed in full ({})",
                        dir.display(),
                        e.kind()
                    ),
                );
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
    names.dedup();
    Scan::Names(names)
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
    let units = unit_facts(&o.layout.unit_dir, o.home.as_deref(), &probe, &receipt);
    let inputs = DoctorInputs {
        vault: vault_facts(&o),
        binary: o.binary,
        binary_name: o.binary_name,
        receipt_shown,
        receipt,
        probe: &probe,
        stores: o.stores,
        ui,
        units,
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
