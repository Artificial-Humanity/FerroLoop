use crate::finding::Finding;
use crate::ids::{FindingId, GateId, ProjectId, RecordId};
use crate::log::{Attempt, GateRun};
use crate::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no such gate: {0}")]
    NoSuchGate(GateId),
    #[error("no such record: {0}")]
    NoSuchRecord(RecordId),
    #[error("no such finding: {0}")]
    NoSuchFinding(FindingId),
    #[error("backend failure: {0}")]
    Backend(String),
    /// ⚠ The store could not be reached at all. This is "didn't look", and it
    /// must never read as an empty store.
    #[error("the store at {store} could not be opened: {cause}")]
    Unreachable { store: String, cause: String },
    #[error(
        "the store holds format {}, and this version of fl reads format {expected}. \
         There is no migration: start a new store, or keep using the version of fl \
         that wrote this one.",
        match found { Some(v) => v.to_string(), None => "none (written before format versioning)".to_string() }
    )]
    FormatVersion { found: Option<u64>, expected: u64 },
    /// ⚠ A stored record could not be read back into its type. This is what a
    /// wire-format change looks like from the other side, so the message names
    /// that cause and the remedy — a refusal that only reports serde's
    /// complaint tells the reader what broke but not what to do.
    #[error(
        "a stored record could not be read: {0}. \
         Two things produce this and the message cannot tell them apart. \
         Either the store was written by a version of this tool whose wire \
         format has since changed — there is no migration, so start a new \
         store or keep using the version that wrote this one — or the stored \
         bytes are damaged, in which case restore the file from a backup."
    )]
    Decode(String),
}

/// Definitions: projects, gates, transitions. Rarely changed; each belongs
/// to a repository.
///
/// ⚠ There is no method that stores a population, and adding one would break
/// the design. A population is enumerated fresh from the working tree at run
/// time so it cannot go stale in the store.
pub trait Catalog {
    fn add_project(&self, root: &str) -> Result<ProjectId, StoreError>;
    fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError>;
    fn list_projects(&self) -> Result<Vec<Project>, StoreError>;

    #[allow(clippy::too_many_arguments)]
    fn add_gate(
        &self,
        project: &ProjectId,
        name: &str,
        kind: GateKind,
        selector: Selector,
        min_population: u64,
        authored_at_commit: &str,
        authored_by: &str,
    ) -> Result<GateId, StoreError>;
    fn get_gate(&self, id: &GateId) -> Result<Option<GateDef>, StoreError>;
    fn list_gates(&self, project: &ProjectId) -> Result<Vec<GateDef>, StoreError>;
    fn update_gate(&self, def: &GateDef) -> Result<(), StoreError>;

    /// Stores a transition, keyed by `(project, name)`. Overwrites any existing transition with the same key (upsert semantics).
    fn add_transition(&self, t: Transition) -> Result<(), StoreError>;
    fn get_transition(
        &self,
        project: &ProjectId,
        name: &str,
    ) -> Result<Option<Transition>, StoreError>;
    /// Every transition a project declares.
    ///
    /// Needed because a transition is addressed by NAME, but a record move is
    /// addressed by the (from, to) pair it performs — so the move has to ask
    /// which declarations cover it.
    fn list_transitions(&self, project: &ProjectId) -> Result<Vec<Transition>, StoreError>;
}

/// Mutable state that people discuss: records and findings.
pub trait Tracker {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError>;
    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError>;
    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError>;
    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError>;

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError>;
    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError>;
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError>;
    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError>;

    /// How many findings this actor raised and then withdrew.
    ///
    /// ⚠ Decision 27 puts a cost on a claim the reviewer cannot support. A
    /// cost nobody can read is not a cost, so this is part of the trait and
    /// not a report bolted on later.
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError>;
}

/// Append-only evidence: gate runs and attempts.
pub trait Ledger {
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError>;
    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError>;
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError>;
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError>;
}

/// Binds each role to the store that backs it (spec §3.3). A store is the
/// backing entity; a role is what it backs. One store can back all three.
#[derive(Clone, Copy)]
pub struct Roles<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
    pub ledger: &'a dyn Ledger,
}

impl<'a> Roles<'a> {
    /// One store backing all three roles.
    pub fn single<S: Catalog + Tracker + Ledger>(store: &'a S) -> Self {
        Self {
            catalog: store,
            tracker: store,
            ledger: store,
        }
    }
}
