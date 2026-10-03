//! The GitHub ledger (GitHub ledger spec §1.1, §3): the `fl/ledger` branch
//! of the repository that backs the tracker, where mode B publishes each
//! decision and the evidence it rests on.

mod append;
pub mod disclose;
mod git;
mod init;
pub mod layout;
mod read;
pub mod render;
mod verify;

#[cfg(test)]
mod fixture;

use crate::client::{Client, Method};
use crate::tracker::Repo;
use fl_core::StoreError;
use fl_core::ids::{GateId, ProjectId, RecordId};
use fl_core::log::{Attempt, GateRun};
use fl_core::split::{Batch, LedgerMemory, RemoteLedger};
use serde_json::Value;
use std::cell::{OnceCell, RefCell};
use std::collections::BTreeSet;
use std::time::Duration;

pub use append::TRIES;
pub use disclose::Visibility;
pub use init::{InitOutcome, Mode, guidance, ruleset_command};
pub use read::Note;
pub use verify::{BadCommit, SameId, VERIFY_LIMIT, Verified, VerifyPhase};

/// The GitHub side of mode B, for one repository and one command.
///
/// ⚠ It borrows the tracker's `Client` — one credential, one origin guard,
/// one rate-limit handling (spec §1.1) — and the local store, which keeps
/// the anchor, the last head this machine checked, and every file it read.
pub struct GithubLedger<'a> {
    pub(crate) client: &'a Client,
    pub(crate) repo: Repo,
    pub(crate) local: &'a dyn LedgerMemory,
    /// How many times a lagging answer is read again before it counts
    /// (spec §3.5 check 2), and the pause between reads.
    pub(crate) lag_reads: u32,
    pub(crate) lag_pause: Duration,
    /// What reads noted without refusing, once each.
    pub(crate) notes: RefCell<BTreeSet<Note>>,
    /// Read once per `GithubLedger` (spec §5; ruling 21).
    pub(crate) visibility: OnceCell<Visibility>,
    /// `by` on every line it writes (spec §3.1; ruling 18).
    pub(crate) identity: OnceCell<String>,
}

impl<'a> GithubLedger<'a> {
    pub fn new(client: &'a Client, repo: Repo, local: &'a dyn LedgerMemory) -> Self {
        Self {
            client,
            repo,
            local,
            lag_reads: 3,
            lag_pause: Duration::from_millis(500),
            notes: RefCell::new(BTreeSet::new()),
            visibility: OnceCell::new(),
            identity: OnceCell::new(),
        }
    }

    /// Tests only: how often a lagging answer is read again, and the pause
    /// between reads.
    #[doc(hidden)]
    pub fn with_lag(mut self, reads: u32, pause: Duration) -> Self {
        self.lag_reads = reads;
        self.lag_pause = pause;
        self
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// What reads noted since the last call (a quarantined line skipped),
    /// once each, for the command to print.
    pub fn take_notes(&self) -> Vec<Note> {
        std::mem::take(&mut *self.notes.borrow_mut())
            .into_iter()
            .collect()
    }

    /// The repository's visibility, read live, once per `GithubLedger` —
    /// one command, one decision (spec §5).
    ///
    /// ⚠ A failed read is an error: an unknown visibility is not private.
    /// An answer that names none is taken as not private, which withholds
    /// (ruling 21): a published excerpt cannot be taken back.
    pub fn visibility(&self) -> Result<Visibility, StoreError> {
        if let Some(v) = self.visibility.get() {
            return Ok(*v);
        }
        let r = self.client.send(Method::Get, &self.path(""), None)?;
        let repo = &self.repo.full_name;
        match r.status {
            200 => {}
            // ⚠ A server error says nothing lasting: transient, retry.
            500..=599 => {
                return Err(self.unreachable(format!(
                    "GitHub answered {} when fl read the repository's visibility, so fl \
                     publishes nothing to its ledger: an unknown visibility is not private",
                    r.status
                )));
            }
            s => {
                return Err(StoreError::Backend(format!(
                    "fl could not read the visibility of {repo} (GitHub answered {s}), so it \
                     publishes nothing to its ledger: an unknown visibility is not private"
                )));
            }
        }
        let v = Visibility::from_github(
            r.body
                .get("visibility")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        let _ = self.visibility.set(v);
        Ok(v)
    }

    /// Who the lines this ledger writes say wrote them, read once.
    pub(crate) fn identity(&self) -> Result<String, StoreError> {
        if let Some(by) = self.identity.get() {
            return Ok(by.clone());
        }
        let by = self.client.identity()?;
        let _ = self.identity.set(by.clone());
        Ok(by)
    }
}

impl RemoteLedger for GithubLedger<'_> {
    fn repo_node_id(&self) -> &str {
        &self.repo.node_id
    }

    fn owns_record(&self, record: &RecordId) -> Result<bool, StoreError> {
        self.owns(record)
    }

    fn publish(&self, batch: &Batch) -> Result<Option<String>, StoreError> {
        self.publish_batch(batch)
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        self.runs(gate)
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        self.attempts_of(project)
    }
}
