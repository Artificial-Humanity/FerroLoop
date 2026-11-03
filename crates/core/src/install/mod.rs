//! The installer's data model and its planner (#205 phase A2).
//!
//! Three dedicated, versioned schemas live here, each with an explicit
//! `schema_version`:
//!
//! * [`ReleaseManifest`] — `release-manifest.json`: what a release contains and
//!   how its products depend on each other.
//! * [`InstallReceipt`] — `install-receipt.json`: what an install OWNS.
//! * [`InstallPlan`] — what `fl install --dry-run` prints.
//!
//! Everything is pure: the planner reads a manifest, an optional receipt and a
//! read-only [`HostProbe`], and returns a plan. Nothing here writes, and
//! `.fl/architecture.toml` stays the architecture model — products only NAME
//! the architecture components that implement them.

mod arch_check;
mod defaults;
mod manifest;
mod plan;
#[cfg(test)]
mod plan_tests;
mod receipt;
mod txn;

pub use arch_check::{ArchitectureCheck, RUNTIME_ONLY, SHARED_COMPONENTS};
pub use defaults::default_manifest;
pub use manifest::{
    ActionKind, ActionSpec, Artifact, Compat, FileKind, MAX_ARTIFACT_BYTES, MAX_MANIFEST_BYTES,
    ManifestError, ProductEntry, ProductFile, ProfileEntry, ReleaseInfo, ReleaseKind,
    ReleaseManifest,
};
pub use plan::{
    Approval, BUDGET_EXCEEDED, ConflictKind, Disposition, FileFingerprint, HostProbe, InstallPlan,
    InstallRequest, Layout, MAX_FINGERPRINT_BYTES, Management, MapProbe, ModuleRoute, NotInstalled,
    Origin, PathRole, PathState, PlanConflict, PlanError, PlanRelease, PlanStatus, PlannedAction,
    PlannedArtifact, PlannedPath, PlannedPathKind, SelectedProduct, UNIT_MARKER, fingerprint,
    is_forbidden_install_root, plan, release_dir_name, resolve_layout,
};
pub use receipt::{
    InstallReceipt, JournalEntry, JournalPhase, MAX_RECEIPT_BYTES, OwnedAlias, OwnedArtifact,
    OwnedCatalogEntry, OwnedConfigBlock, OwnedPath, OwnedPathKind, OwnedUnit, ReceiptError,
    ReceiptLayout, ReceiptRelease,
};
pub use txn::{
    BackupSpec, IntentFile, JOURNAL_SCHEMA_VERSION, JournalLine, JournalOp, LinkSpec,
    MAX_COMMITTED_ENTRIES, RELEASE_JSON, RETAINED_PREVIOUS_RELEASES, Retention, Step, TxnIntent,
    build_receipt, has_committed, hex, next_txn, release_json, retention, sha256_hex, tree_digest,
};

use serde::{Deserialize, Serialize};
use std::fmt;

/// `schema_version` of the release manifest.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
/// `schema_version` of the install receipt.
pub const RECEIPT_SCHEMA_VERSION: u32 = 1;
/// `schema_version` of the printed plan.
pub const PLAN_SCHEMA_VERSION: u32 = 1;

/// Longest relative path (archive path, in-release path) the installer accepts.
pub const MAX_REL_PATH_LEN: usize = 200;
const MAX_REL_SEGMENT_LEN: usize = 80;
const MAX_REL_DEPTH: usize = 12;

/// A sub-product the installer knows. The set is closed: a manifest naming any
/// other id is rejected, and the ids are the spec's (§1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    Ferroloop,
    Ferrostore,
    Ferrowire,
    Ferrostate,
    Ferrogate,
    Ferrotrack,
    Ferrowatch,
    Ferrodeck,
    Ferrostep,
}

crate::wire::wire_names!(Product as product_wire {
    Ferroloop => "ferroloop",
    Ferrostore => "ferrostore",
    Ferrowire => "ferrowire",
    Ferrostate => "ferrostate",
    Ferrogate => "ferrogate",
    Ferrotrack => "ferrotrack",
    Ferrowatch => "ferrowatch",
    Ferrodeck => "ferrodeck",
    Ferrostep => "ferrostep",
});
crate::wire::wire_parse!(Product as product_parse);

impl Product {
    /// The human title (spec §1 table).
    pub fn title(self) -> &'static str {
        match self {
            Product::Ferroloop => "FerroLoop",
            Product::Ferrostore => "FerroStore",
            Product::Ferrowire => "FerroWire",
            Product::Ferrostate => "FerroState",
            Product::Ferrogate => "FerroGate",
            Product::Ferrotrack => "FerroTrack",
            Product::Ferrowatch => "FerroWatch",
            Product::Ferrodeck => "FerroDeck",
            Product::Ferrostep => "FerroStep",
        }
    }

    /// FerroLoop is required in every install; it cannot be deselected.
    pub fn is_required(self) -> bool {
        self == Product::Ferroloop
    }

    /// FerroStore is a hidden default dependency: never offered, always there.
    pub fn is_hidden(self) -> bool {
        self == Product::Ferrostore
    }

    /// The ids of the `.fl/architecture.toml` components that implement this
    /// product. Empty where the spec names no component of its own (FerroState
    /// spans `fl-core`, `fl-cli` and `fl-store`; FerroStep has no crate).
    pub fn arch_components(self) -> &'static [&'static str] {
        match self {
            Product::Ferroloop => &["fl_core", "fl_exec"],
            Product::Ferrostore => &["fl_store"],
            Product::Ferrowire => &["fl_wire"],
            Product::Ferrostate => &[],
            Product::Ferrogate => &["fl_gate"],
            Product::Ferrotrack => &["fl_github"],
            Product::Ferrowatch => &["fl_watch"],
            Product::Ferrodeck => &["fl_hub", "ferrodeck_ui"],
            Product::Ferrostep => &[],
        }
    }
}

impl fmt::Display for Product {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// A named bundle of products (spec §1). `lab` is a superset of `full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Minimal,
    Headless,
    Full,
    Lab,
}

crate::wire::wire_names!(Profile as profile_wire {
    Minimal => "minimal",
    Headless => "headless",
    Full => "full",
    Lab => "lab",
});
crate::wire::wire_parse!(Profile as profile_parse);

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// Operating systems a release can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

crate::wire::wire_names!(Os as os_wire {
    Linux => "linux",
    Macos => "macos",
    Windows => "windows",
});
crate::wire::wire_parse!(Os as os_parse);

/// CPU architectures a release can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    X86_64,
    Aarch64,
}

crate::wire::wire_names!(Arch as arch_wire {
    X86_64 => "x86_64",
    Aarch64 => "aarch64",
});
crate::wire::wire_parse!(Arch as arch_parse);

/// A platform/architecture pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub os: Os,
    pub arch: Arch,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os.as_wire(), self.arch.as_wire())
    }
}

/// The machine a plan is made for. The CLI builds it from the running host;
/// tests build it by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HostInfo {
    pub os: Os,
    pub arch: Arch,
}

impl HostInfo {
    pub fn target(self) -> Target {
        Target {
            os: self.os,
            arch: self.arch,
        }
    }

    /// The running host, in the manifest's vocabulary, if it has one.
    pub fn from_consts(os: &str, arch: &str) -> Option<Self> {
        Some(Self {
            os: Os::from_wire(os)?,
            arch: Arch::from_wire(arch)?,
        })
    }
}

/// Why an in-release or archive path was refused.
pub fn rel_path_problem(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        return Some("is empty");
    }
    if path.len() > MAX_REL_PATH_LEN {
        return Some("is too long");
    }
    if path.contains('\0') {
        return Some("contains a NUL byte");
    }
    if path.starts_with('/') {
        return Some("is absolute");
    }
    if path.contains('\\') {
        return Some("contains a backslash");
    }
    if !path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@' | '/'))
    {
        return Some("contains a character outside [A-Za-z0-9._+@-/]");
    }
    let mut depth = 0;
    for seg in path.split('/') {
        depth += 1;
        if seg.is_empty() {
            return Some("has an empty segment (`//` or a trailing `/`)");
        }
        if seg == "." || seg == ".." {
            return Some("has a `.` or `..` segment");
        }
        if seg.len() > MAX_REL_SEGMENT_LEN {
            return Some("has a segment that is too long");
        }
    }
    if depth > MAX_REL_DEPTH {
        return Some("is nested too deeply");
    }
    None
}

/// Longest accepted name (alias, unit, catalog entry, artifact id).
pub(crate) const MAX_NAME_LEN: usize = 80;

/// A single file name or identifier: `[A-Za-z0-9._+@-]`, at most 80 bytes, never
/// `.` or `..`, never starting with `-`. No separator and no NUL can pass, so a
/// name joined onto a directory stays inside it.
pub fn file_name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME_LEN
        && s != "."
        && s != ".."
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@'))
}

/// A lowercase 64-digit hex SHA-256.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_paths_are_bounded() {
        for ok in [
            "bin/fl",
            "ui/dist",
            "contrib/units/ferrostore@.service",
            "a-b_c.d+e",
        ] {
            assert_eq!(rel_path_problem(ok), None, "{ok}");
        }
        let long = "a".repeat(MAX_REL_PATH_LEN + 1);
        let deep = vec!["a"; MAX_REL_DEPTH + 1].join("/");
        let seg = "a".repeat(MAX_REL_SEGMENT_LEN + 1);
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a/./b",
            "a//b",
            "a/",
            "a\\b",
            "a\0b",
            "a b",
            "C:\\x",
            long.as_str(),
            deep.as_str(),
            seg.as_str(),
            "..",
        ] {
            assert!(rel_path_problem(bad).is_some(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn host_info_speaks_the_manifest_vocabulary() {
        assert_eq!(
            HostInfo::from_consts("linux", "x86_64"),
            Some(HostInfo {
                os: Os::Linux,
                arch: Arch::X86_64
            })
        );
        assert_eq!(HostInfo::from_consts("freebsd", "x86_64"), None);
        assert_eq!(HostInfo::from_consts("linux", "riscv64"), None);
    }

    #[test]
    fn product_ids_are_the_specs_and_only_ferroloop_is_required() {
        let ids: Vec<&str> = Product::ALL.iter().map(|p| p.as_wire()).collect();
        assert_eq!(
            ids,
            [
                "ferroloop",
                "ferrostore",
                "ferrowire",
                "ferrostate",
                "ferrogate",
                "ferrotrack",
                "ferrowatch",
                "ferrodeck",
                "ferrostep"
            ]
        );
        assert_eq!(Product::ALL.iter().filter(|p| p.is_required()).count(), 1);
        assert_eq!(Product::ALL.iter().filter(|p| p.is_hidden()).count(), 1);
    }
}
