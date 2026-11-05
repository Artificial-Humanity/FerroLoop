//! `fl doctor`'s check engine (#205 phase A3): observational only.
//!
//! [`run`] is pure. It takes [`DoctorInputs`] (facts a caller read without writing anything:
//! the build stamp, the install receipt's text, a read-only [`HostProbe`], the outcome of a
//! read-only store verification, the vault files' modes, the UI bundle's `build-info.json`,
//! and the `After=` / `WantedBy=` lines of installed `ferro*.service` user units) and returns
//! a [`DoctorReport`] of exactly seven checks, in a fixed order, each with a stable id:
//!
//! | id | observes |
//! |---|---|
//! | `G7.release-identity` | the running binary's build stamp, against the receipt's release |
//! | `G7.receipt-integrity` | the install receipt parses and validates |
//! | `G7.path-ownership` | every owned path, alias and unit matches the receipt |
//! | `G7.store-health` | each store opens read-only at a format this build reads |
//! | `G7.vault-perms` | the vault and master key files are private to the owner |
//! | `G7.code-ui-agree` | the UI bundle was built from the binary's commit |
//! | `G7.unit-order` | no installed `ferro*.service` is both wanted by and ordered after `default.target` |
//!
//! A status is `pass` only on positive evidence. An absent receipt, store, vault, UI bundle
//! or user unit is `not-installed`; a proof the inputs cannot make is `unsupported`; neither
//! is ever `pass`. The report carries no timestamp, so equal inputs give equal bytes.
//!
//! This module does no I/O, reads no clock and uses no `unsafe`. The caller gathers the facts:
//! `fl_install::observe` reads the file system (reads only) and calls [`run`]; it is what the
//! CLI and the hub use.

mod facts;
mod identity;
mod paths;
mod receipt;
mod render;
mod store;
mod ui;
mod units;
mod vault;

#[cfg(test)]
mod tests;

use crate::build_info::BuildInfo;
use crate::install::{HostProbe, InstallReceipt};
use serde::{Deserialize, Serialize};

pub use facts::{
    NodeMeta, ReceiptSource, StoreFact, StoreFacts, StoreOutcome, UiFacts, UnitFacts, UnitFileFact,
    VaultFacts, VaultNode,
};
pub use units::user_unit_directives;

/// `schema_version` of the printed report.
pub const DOCTOR_SCHEMA_VERSION: u32 = 1;

/// The check ids, in the order the report lists them.
pub mod ids {
    pub const RELEASE_IDENTITY: &str = "G7.release-identity";
    pub const RECEIPT_INTEGRITY: &str = "G7.receipt-integrity";
    pub const PATH_OWNERSHIP: &str = "G7.path-ownership";
    pub const STORE_HEALTH: &str = "G7.store-health";
    pub const VAULT_PERMS: &str = "G7.vault-perms";
    pub const CODE_UI_AGREE: &str = "G7.code-ui-agree";
    pub const UNIT_ORDER: &str = "G7.unit-order";
}

/// Every check id, in report order.
pub const CHECK_IDS: [&str; 7] = [
    ids::RELEASE_IDENTITY,
    ids::RECEIPT_INTEGRITY,
    ids::PATH_OWNERSHIP,
    ids::STORE_HEALTH,
    ids::VAULT_PERMS,
    ids::CODE_UI_AGREE,
    ids::UNIT_ORDER,
];

/// The outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    NotInstalled,
    Unsupported,
}

impl CheckStatus {
    /// The wire string (`pass`, `warn`, `fail`, `not-installed`, `unsupported`).
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Pass => "pass",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
            CheckStatus::NotInstalled => "not-installed",
            CheckStatus::Unsupported => "unsupported",
        }
    }

    /// How bad: `fail` > `warn` > `unsupported` > `not-installed` > `pass`.
    fn severity(self) -> u8 {
        match self {
            CheckStatus::Pass => 0,
            CheckStatus::NotInstalled => 1,
            CheckStatus::Unsupported => 2,
            CheckStatus::Warn => 3,
            CheckStatus::Fail => 4,
        }
    }
}

/// One check's result. These four fields are the whole contract FerroDeck reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub id: String,
    pub title: String,
    pub status: CheckStatus,
    pub detail: String,
}

/// How many checks ended in each status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub pass: u32,
    pub warn: u32,
    pub fail: u32,
    pub not_installed: u32,
    pub unsupported: u32,
}

/// The doctor's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    /// `pass` only when every check passed; otherwise the worst check's status.
    pub overall: CheckStatus,
    pub summary: Summary,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    fn new(checks: Vec<DoctorCheck>) -> Self {
        let mut summary = Summary::default();
        for c in &checks {
            match c.status {
                CheckStatus::Pass => summary.pass += 1,
                CheckStatus::Warn => summary.warn += 1,
                CheckStatus::Fail => summary.fail += 1,
                CheckStatus::NotInstalled => summary.not_installed += 1,
                CheckStatus::Unsupported => summary.unsupported += 1,
            }
        }
        let overall = checks
            .iter()
            .map(|c| c.status)
            .max_by_key(|s| s.severity())
            .unwrap_or(CheckStatus::Unsupported);
        Self {
            schema_version: DOCTOR_SCHEMA_VERSION,
            overall,
            summary,
            checks,
        }
    }

    /// `0` when no check failed, `1` when at least one did. `warn`, `not-installed` and
    /// `unsupported` do not fail the run.
    pub fn exit_code(&self) -> i32 {
        i32::from(self.summary.fail > 0)
    }

    /// Pretty JSON with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut out = serde_json::to_string_pretty(self).expect("report serializes");
        out.push('\n');
        out
    }

    /// The check with `id`.
    pub fn check(&self, id: &str) -> Option<&DoctorCheck> {
        self.checks.iter().find(|c| c.id == id)
    }
}

/// What the doctor reads. Every field is a fact a caller gathered without writing.
pub struct DoctorInputs<'a> {
    /// The running binary's build stamp.
    pub binary: BuildInfo,
    /// The binary's name in messages (`fl`, `fl-hub`).
    pub binary_name: &'a str,
    /// The receipt's displayed path.
    pub receipt_shown: String,
    pub receipt: ReceiptSource,
    /// A read-only look at the paths the receipt owns.
    pub probe: &'a dyn HostProbe,
    pub stores: StoreFacts,
    pub vault: VaultFacts,
    pub ui: UiFacts,
    /// Installed `ferro*.service` user units, already parsed. Empty when none are installed.
    pub units: UnitFacts,
}

impl<'a> DoctorInputs<'a> {
    /// Inputs that know nothing: an unknown build, no receipt, no store, no vault, no UI.
    pub fn empty(probe: &'a dyn HostProbe) -> Self {
        Self {
            binary: BuildInfo::from_stamp("0.0.0", "", "", "", None),
            binary_name: "fl",
            receipt_shown: "receipt.json".into(),
            receipt: ReceiptSource::Absent,
            probe,
            stores: StoreFacts::default(),
            vault: VaultFacts::default(),
            ui: UiFacts::default(),
            units: UnitFacts::default(),
        }
    }
}

/// The receipt as the checks see it, parsed once.
pub(crate) enum ReceiptView {
    Absent,
    Unlocatable(String),
    Unreadable(String),
    Invalid(String),
    Valid(Box<InstallReceipt>),
}

impl ReceiptView {
    fn of(source: &ReceiptSource) -> Self {
        match source {
            ReceiptSource::Absent => ReceiptView::Absent,
            ReceiptSource::Unlocatable(r) => ReceiptView::Unlocatable(r.clone()),
            ReceiptSource::Unreadable(r) => ReceiptView::Unreadable(r.clone()),
            ReceiptSource::Text(t) => match InstallReceipt::parse(t) {
                Ok(r) => ReceiptView::Valid(Box::new(r)),
                Err(e) => ReceiptView::Invalid(e.to_string()),
            },
        }
    }
}

/// Run the seven checks.
pub fn run(inputs: &DoctorInputs<'_>) -> DoctorReport {
    let receipt = ReceiptView::of(&inputs.receipt);
    let checks = vec![
        identity::check(inputs, &receipt),
        receipt::check(inputs, &receipt),
        paths::check(inputs, &receipt),
        store::check(inputs),
        vault::check(inputs),
        ui::check(inputs),
        units::check(inputs),
    ];
    DoctorReport::new(checks)
}

fn make(id: &str, title: &str, status: CheckStatus, detail: String) -> DoctorCheck {
    DoctorCheck {
        id: id.into(),
        title: title.into(),
        status,
        detail,
    }
}

/// The first `n` bytes-worth of a commit, for messages.
fn short(commit: &str) -> &str {
    &commit[..commit.len().min(crate::build_info::SHORT_COMMIT_LEN)]
}

/// `items` joined with `; `, at most `max` of them, then `(+N more)`.
fn joined(items: &[String], max: usize) -> String {
    let mut out = items
        .iter()
        .take(max)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    if items.len() > max {
        out.push_str(&format!("; (+{} more)", items.len() - max));
    }
    out
}
