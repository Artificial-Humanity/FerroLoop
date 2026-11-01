use crate::agent::{AgentRecord, AgentStatus, AreaRecord, RegisterAgentArgs, RegistryEvent};
use crate::at::At;
use crate::decision::{Decision, Flushed};
use crate::escalation::EscalationFault;
use crate::fault::LedgerFault;
use crate::ferrowire::{
    DeliveryReceipt, DeliveryStatus, SendWireArgs, ThreadId, WireFilter, WireImportReport,
    WireInboxItem, WireMessage,
};
use crate::finding::Finding;
use crate::ids::{
    AgentId, FindingId, GateId, Kind, MessageId, ProjectId, RecordId, RevisionId, StateId,
    WorkflowId, WorkflowRunId,
};
use crate::iri::Iri;
use crate::log::{Attempt, GateRun};
use crate::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};
use crate::routing::{ForeignRecord, RoutingFault};
use crate::state::{
    MemoryLifecycle, StateConcern, StateConflict, StateEntry, StateRevision, StateScope,
    StateSyncCursor, UpsertStateArgs,
};
use crate::workflow::{WorkflowDef, WorkflowRun};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no such gate: {0}")]
    NoSuchGate(GateId),
    #[error("no such record: {0}")]
    NoSuchRecord(RecordId),
    #[error("no such finding: {0}")]
    NoSuchFinding(FindingId),
    /// ⚠ This store never held the id. It did not look anywhere else, so this
    /// is never "not found" — `searched` says exactly where it looked. An
    /// EMPTY `searched` means there was no store to look in at all, and the
    /// message says so rather than printing an empty list that reads like a
    /// search that ran.
    #[error(
        "no store holds {id} ({})",
        if searched.is_empty() {
            "no store exists yet, so there was nothing to search".to_string()
        } else {
            format!("searched: {}", searched.join(", "))
        }
    )]
    NotOwned { id: Iri, searched: Vec<String> },
    /// ⚠ The store holds `id`, but as a different kind of item than the
    /// method needs — a gate's IRI passed where a project is expected. This
    /// is neither "nothing there" (an empty list would claim the store
    /// looked at the right item and found nothing) nor `NotOwned` (the store
    /// does hold it).
    #[error(
        "{id} is a {} in this store, not a {}, so it cannot be used where a {} is needed",
        found.as_wire(),
        expected.as_wire(),
        expected.as_wire()
    )]
    WrongKind {
        id: Iri,
        expected: Kind,
        found: Kind,
    },
    #[error("{0} already exists; an insert never overwrites")]
    AlreadyExists(Iri),
    /// ⚠ The item belongs to a project this store imported from a manifest
    /// (GitHub tracker spec §4.2). Its definition is authored elsewhere, and
    /// a local edit would make this copy disagree with the manifest every
    /// other reader resolves.
    #[error(
        "{id}'s project was imported from a manifest, so this store cannot {action} it. \
         Change it in the store that authors the project, run `fl manifest export` there, \
         commit, then run `fl manifest import` here."
    )]
    Imported { id: Iri, action: &'static str },
    #[error("backend failure: {0}")]
    Backend(String),
    /// ⚠ The write committed and is durable at `seq`, but the sync adapter
    /// (`WriteThrough`) has not received it: `cause` is the adapter's error,
    /// for this commit's batch or for an older one still queued ahead of it.
    /// The batch stays pending until a flush sends it. This is never "not
    /// committed": retrying the write would apply it a second time.
    #[error("committed at seq {seq} (durable), not yet delivered to the sync adapter: {cause}")]
    Undelivered {
        seq: u64,
        #[source]
        cause: Box<StoreError>,
    },
    /// ⚠ The store could not be reached at all. This is "didn't look", and it
    /// must never read as an empty store.
    #[error("the store at {store} could not be opened: {cause}")]
    Unreachable { store: String, cause: String },
    #[error("{}", format_version_message(*found, *oldest, *newest))]
    FormatVersion {
        found: Option<u64>,
        oldest: u64,
        newest: u64,
    },
    /// ⚠ A FerroStore file whose container a newer fl wrote: a superblock names
    /// a container version, an engine or an incompatible feature bit this build
    /// does not know. The file is not damaged, and this build neither reads nor
    /// writes it. `store` is the file's path, or empty when the caller had none.
    #[error(
        "{}",
        container_format_message(store, *found, *engine, *unknown_incompat, *newest)
    )]
    ContainerFormat {
        store: String,
        found: u32,
        engine: u32,
        unknown_incompat: u64,
        newest: u32,
    },
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
    /// ⚠ The store holds `to`, but not as the kind of item `from` expects. A
    /// reference that never resolves is one failure; a reference that once
    /// resolved and now points at the wrong kind is a different one, and the
    /// difference matters to whoever is debugging it.
    #[error(
        "{from} refers to {to}, which is dangling: the store holds that id, but not as this kind of item"
    )]
    Dangling { from: String, to: Iri },
    /// ⚠ The owner deleted the item (a GitHub issue answered 410). "It was
    /// here and is gone" is not "no such item".
    #[error(
        "{0} was deleted where it was held, so nothing can be read or written through it. If \
         it should still exist, raise it again: fl keeps no copy of it."
    )]
    Deleted(Iri),
    /// ⚠ fl's own record of an item disagrees with the item's visible state
    /// (GitHub tracker spec §3.4). fl adopts neither side silently.
    #[error(
        "{id} is diverged: {detail}. Run `fl github repair {id} --by <name>` to rewrite its \
         labels and status from fl's record — or, if the issue was never fl's, remove its fl \
         labels."
    )]
    Diverged { id: Iri, detail: String },
    /// ⚠ Another actor wrote the item while fl wrote it (spec §3.3). That
    /// write has landed, and fl's may have overwritten part of it.
    #[error(
        "{id} was changed by another actor while fl wrote it: {detail}. Read it again, check \
         it, and retry."
    )]
    Conflict { id: Iri, detail: String },
    /// ⚠ One entry id, two contents: this machine's copy and the shared
    /// ledger's (GitHub ledger spec §2.5). An entry never changes once it is
    /// written, so one of the two was altered afterwards.
    #[error(
        "{id} is recorded differently on this machine and in the shared ledger: {detail}. An \
         entry never changes once it is written, so one copy was altered afterwards, and fl \
         uses neither. Find out who changed it before trusting either copy."
    )]
    Tampered { id: Iri, detail: String },
    /// ⚠ A ledger's anchor — its branch's first commit — never changes
    /// (GitHub ledger spec §3.5).
    #[error(
        "the GitHub ledger of repository node {node_id} is anchored at commit {held}, and this \
         names {found}. A ledger's first commit never changes: a different one means the ledger \
         was deleted and created again, or the manifest was edited. Find out which before \
         trusting either."
    )]
    LedgerRootChanged {
        node_id: String,
        held: String,
        found: String,
    },
    /// ⚠ The cut-over is recorded once, when a repository's GitHub ledger
    /// is switched on (spec §2.1): moving it would strand every entry
    /// recorded between the two.
    #[error(
        "the GitHub ledger of repository node {node_id} was switched on at {held}, and this \
         names {found}. The cut-over never moves: entries between the two would never be \
         published. Keep the recorded one."
    )]
    CutoverChanged {
        node_id: String,
        held: Iri,
        found: Iri,
    },
    /// The id names something that exists but that fl did not create.
    #[error(
        "{id} is {what}, not an item fl created, so fl neither reads nor changes it. Name an \
         fl record or finding instead."
    )]
    NotAnFlItem { id: Iri, what: String },
    /// ⚠ The item moved out of the store that owned it (a transferred issue).
    #[error(
        "{id} was moved to {to}, outside the repository this tracker binds. Bind that \
         repository, or raise the item again here."
    )]
    Moved { id: Iri, to: String },
    /// ⚠ Reported, never waited out in silence (spec §7).
    #[error("GitHub's rate limit is spent until {reset}. Retry after that time.")]
    RateLimited { reset: String },
    /// ⚠ The configured name now reaches another repository (spec §2.4).
    #[error(
        "`{name}` now reaches a different repository (node {found}) from the one this \
         tracker is bound to (node {bound}). Correct the `github` binding in config.toml; \
         fl refuses every read and write until then."
    )]
    RepositoryReplaced {
        name: String,
        bound: String,
        found: String,
    },
    /// A credential is missing or refused. There is no fallback source.
    #[error("no usable GitHub credential: {0}")]
    Credential(String),
    /// ⚠ Spec §6: a security finding is written only where the public
    /// cannot read it.
    #[error(
        "refused: a security finding cannot be written to {repo}, whose visibility is \
         `{visibility}`. Use a local tracker for it, or bind a private repository."
    )]
    SecurityNotPrivate { repo: String, visibility: String },
    /// ⚠ The shared ledger is not there, or not as fl wrote it (GitHub
    /// ledger spec §3.5, §7).
    #[error(transparent)]
    Ledger(#[from] LedgerFault),
    /// ⚠ The routing tracker refused (routing spec §4).
    #[error(transparent)]
    Routing(#[from] RoutingFault),
    /// ⚠ The item is marked escalating (routing spec §3.3 step 1, §3.6): the
    /// escalation is moving its one live copy to GitHub, so a local write
    /// could be lost. `to_finish` is `fl <kind> escalate <id>`.
    #[error(
        "{id} is marked escalating, so this store refuses to change it. Run `{to_finish}` to \
         finish the escalation, or `{to_finish} --abandon` to stop it"
    )]
    Escalating { id: Iri, to_finish: String },
    /// ⚠ The item was escalated (routing spec §3.6): the store holds only its
    /// tombstone, which the router follows to `to`.
    #[error(
        "{from} was escalated to GitHub and is now {to}. Read and change it there: this store \
         keeps only its tombstone"
    )]
    Escalated { from: Iri, to: Iri },
    /// ⚠ The escalation refused (routing spec §3.2, §4).
    #[error(transparent)]
    Escalation(#[from] EscalationFault),
    /// ⚠ A decision may rest only on entries it publishes or that are
    /// already published (ruling 8).
    #[error(
        "decision {decision} rests on {entry}, which is neither being published nor published. \
         A run tied to no record, one recorded before the GitHub ledger was switched on, or one \
         tied to another repository's record stays local, so a decision cannot rest on it. \
         Nothing was published."
    )]
    RestsOnLocalEntry { decision: Iri, entry: Iri },
    /// ⚠ Others appended to the ledger before each of fl's tries landed
    /// (GitHub ledger spec §3.2 step 5). Nothing was lost.
    #[error(
        "{store} was appended to by someone else before each of fl's {tries} tries could land, \
         so fl's own append did not. Nothing was lost; retry"
    )]
    Contended { store: String, tries: u32 },
    #[error(
        "agent `{name_key}` is already registered as {existing_id} (status: {}); \
         confirm with the owner whether you are resuming/replacing that agent before passing `--replace`",
        status.as_wire()
    )]
    AgentCollision {
        name_key: String,
        existing_id: AgentId,
        status: AgentStatus,
    },
    #[error(
        "project `{project}` already has an active lead (`{lead_name}`, {lead_id}); \
         confirm with the owner before passing `--replace`"
    )]
    LeadTaken {
        project: String,
        lead_name: String,
        lead_id: AgentId,
    },
    #[error(
        "no such area `{0}` in registry (pass `--create-area` or run `fl agent area set` first)"
    )]
    NoSuchArea(String),
    #[error("no such agent: {0}")]
    NoSuchAgent(String),
    #[error("no such state entry: {0}")]
    NoSuchState(String),
    #[error(
        "state entry {entry_id} has an unresolved conflict between local revision {local_head} and remote revision {remote_head}; \
         run `fl state resolve {entry_id} --keep <revision-id> --by <author>` first"
    )]
    StateConflict {
        entry_id: StateId,
        local_head: RevisionId,
        remote_head: RevisionId,
    },
    #[error(
        "refused: state entry {0} carries sensitive history and cannot be synced to a non-private state target (`private = true` is required in config.toml)"
    )]
    SensitiveDisclosure(StateId),
    #[error("invalid state operation: {0}")]
    InvalidState(String),
    #[error("no such wire message: {0}")]
    MessageNotFound(MessageId),
    #[error(
        "wire message {id} is recorded differently on this machine and in the incoming batch: {detail}. \
         A wire message never changes once sent."
    )]
    WireMessageTampered { id: MessageId, detail: String },
    #[error("no such workflow: {0}")]
    NoSuchWorkflow(String),
    #[error("no such workflow run: {0}")]
    NoSuchWorkflowRun(String),
    #[error("invalid workflow operation: {0}")]
    InvalidWorkflow(String),
}

/// Follow a stored reference. `Ok(None)` from the owning store means the
/// reference points at nothing of this kind: dangling. `NotOwned` passes
/// through unchanged — "no store holds it" is not the same as "gone".
pub fn follow<T>(
    from: &str,
    to: &Iri,
    got: Result<Option<T>, StoreError>,
) -> Result<T, StoreError> {
    match got {
        Ok(Some(t)) => Ok(t),
        Ok(None) => Err(StoreError::Dangling {
            from: from.to_string(),
            to: to.clone(),
        }),
        Err(e) => Err(e),
    }
}

/// `FormatVersion`'s message. A store from a NEWER fl is not damaged and
/// needs no new store: importing a manifest that carries a ledger root, for
/// one, raises a store to format 4, and the remedy on an older build is to
/// upgrade.
fn format_version_message(found: Option<u64>, oldest: u64, newest: u64) -> String {
    match found {
        Some(v) if v > newest => format!(
            "the store holds format {v}, which a newer fl wrote, and this version of fl reads \
             formats {oldest} to {newest}. Upgrade fl to open it: nothing is wrong with the store."
        ),
        _ => format!(
            "the store holds format {}, and this version of fl reads formats {oldest} to \
             {newest}. There is no migration: start a new store, or keep using the version of fl \
             that wrote this one.",
            match found {
                Some(v) => v.to_string(),
                None => "none (written before format versioning)".to_string(),
            }
        ),
    }
}

/// `ContainerFormat`'s message. Like a newer schema, a newer container is not
/// damage: the remedy is a newer fl.
fn container_format_message(
    store: &str,
    found: u32,
    engine: u32,
    unknown_incompat: u64,
    newest: u32,
) -> String {
    let file = if store.is_empty() {
        "this FerroStore file".to_string()
    } else {
        format!("the FerroStore file {store}")
    };
    let mut what = format!("container format {found}");
    if engine != 0 {
        what.push_str(&format!(", engine {engine}"));
    }
    if unknown_incompat != 0 {
        what.push_str(&format!(", feature bits {unknown_incompat:#x}"));
    }
    format!(
        "{file} uses {what}, which a newer fl wrote; this version of fl reads container formats \
         1 to {newest} with engine 0 and no feature bits. Upgrade fl to open it: nothing is \
         wrong with the store."
    )
}

impl StoreError {
    /// Whether retrying later can succeed with nothing fixed first: GitHub
    /// could not be reached, its rate limit is spent, or others kept
    /// appending to the ledger. Only these may promise that the next
    /// decision publishes what this one could not (GitHub ledger spec §7).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            StoreError::Unreachable { .. }
                | StoreError::RateLimited { .. }
                | StoreError::Contended { .. }
        )
    }
}

/// `e`'s message as a clause inside a longer sentence: without the one
/// final period some messages end with, so the sentence around it ends
/// with exactly one.
pub fn as_clause(e: &dyn std::fmt::Display) -> String {
    let text = e.to_string();
    match text.strip_suffix('.') {
        Some(clause) => clause.to_string(),
        None => text,
    }
}

/// Definitions: projects, gates, transitions. Rarely changed; each belongs
/// to a repository.
///
/// ⚠ There is no method that stores a population, and adding one would break
/// the design. A population is enumerated fresh from the working tree at run
/// time so it cannot go stale in the store.
///
/// ⚠ Every method here, in [`Tracker`] and in [`Ledger`] that takes a
/// `ProjectId` refuses an id this store holds under another kind with
/// [`StoreError::WrongKind`] — never an empty answer. `add_finding` does the
/// same for its record.
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

    /// The kind this store holds `id` under, following an alias; `NotOwned`
    /// if it never held it. A split binding asks this to check a reference
    /// that crosses into another store (GitHub tracker spec §1.3).
    fn kind_of(&self, id: &Iri) -> Result<Kind, StoreError>;
}

/// Mutable state that people discuss: records and findings.
pub trait Tracker {
    /// A record with no area: [`Tracker::add_record_with_area`] with
    /// `None`. A routed project refuses it (routing spec §2.1).
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        self.add_record_with_area(project, title, None)
    }
    /// A record in `area` (routing spec §1.1), stored with it for good.
    ///
    /// ⚠ The one method an implementation writes: `add_record` reaches it,
    /// so no wrapper can drop the area.
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError>;
    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError>;
    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError>;
    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError>;

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError>;
    /// Raise `finding` about a record in the OTHER tier (routing spec
    /// §2.5). The tracker does not look for the record — it does not hold
    /// it — and takes `record`, the router's proof of its check, instead.
    /// ⚠ `add_finding` still refuses a record the tracker does not hold.
    fn add_finding_checked(
        &self,
        finding: Finding,
        record: ForeignRecord,
    ) -> Result<FindingId, StoreError>;
    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError>;
    /// Replace a stored finding with `finding`, keyed by its primary id even
    /// when `finding.id` is an alias.
    ///
    /// ⚠ `also_known_as` is NOT taken from `finding`: the stored list is
    /// kept and the caller's is ignored. Only `add_alias` adds a name, and it
    /// updates the alias index and this list together. A caller holding a
    /// copy read before an `add_alias` would otherwise erase that alias from
    /// the list while the index still resolves it; a caller that edited the
    /// list would add a name the index cannot resolve.
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError>;
    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError>;

    /// How many findings this actor raised and then withdrew.
    ///
    /// ⚠ Decision 27 puts a cost on a claim the reviewer cannot support. A
    /// cost nobody can read is not a cost, so this is part of the trait and
    /// not a report bolted on later.
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError>;

    /// Name `primary` by `alias` as well (spec §2.5). A lookup by `alias`
    /// then answers as `primary` would. `alias` must not already be used by
    /// this store, whether as a primary id or as another alias — an id
    /// space where identity is exact-string comparison has exactly one
    /// namespace, and an insert into it never overwrites.
    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError>;
}

/// Append-only evidence: gate runs and attempts.
pub trait Ledger {
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError>;
    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError>;
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError>;
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError>;

    /// Publish a decision and the evidence it rests on (GitHub ledger spec
    /// §1.4, §2.2).
    ///
    /// ⚠⚠ Call it BEFORE the state change the decision supports, and treat
    /// an error as a refusal of the decision: no state change. A refused
    /// decision is flushed too (decision 11).
    ///
    /// A ledger with nowhere to publish — every local store — does nothing,
    /// which is this default.
    fn flush(&self, _decision: Decision) -> Result<Flushed, StoreError> {
        Ok(Flushed::NOTHING)
    }
}

/// Short names a person types and reads (spec §4). Display only: a handle
/// never enters a stored item or the wire. Per store and per kind.
pub trait Handles {
    fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError>;
    fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError>;
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

/// A tracker whose project and record references are checked against a
/// catalog held by ANOTHER store (GitHub tracker spec §1.3, §8.1 item 2).
///
/// ⚠ For a split binding only. It reads "the catalog holds this id" as "it
/// is not a record", which is true only when the catalog's store backs no
/// tracker. A store that backs both roles checks its own references.
pub struct CatalogChecked<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
}

impl CatalogChecked<'_> {
    fn project(&self, p: &ProjectId) -> Result<(), StoreError> {
        match self.catalog.kind_of(p.iri())? {
            Kind::Project => Ok(()),
            found => Err(StoreError::WrongKind {
                id: p.iri().clone(),
                expected: Kind::Project,
                found,
            }),
        }
    }

    fn record(&self, r: &RecordId) -> Result<(), StoreError> {
        match self.catalog.kind_of(r.iri()) {
            Ok(found) => Err(StoreError::WrongKind {
                id: r.iri().clone(),
                expected: Kind::Record,
                found,
            }),
            // Not the catalog's: the tracker decides whether it is a record.
            Err(StoreError::NotOwned { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Tracker for CatalogChecked<'_> {
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError> {
        self.project(project)?;
        self.tracker.add_record_with_area(project, title, area)
    }
    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.tracker.get_record(id)
    }
    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        self.project(project)?;
        self.tracker.list_records(project)
    }
    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        self.tracker.set_record_state(id, state)
    }
    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        self.project(&finding.project)?;
        self.record(&finding.record)?;
        self.tracker.add_finding(finding)
    }
    /// The project only: the record is in the other tier, which the router
    /// checked (routing spec §2.5).
    fn add_finding_checked(
        &self,
        finding: Finding,
        record: ForeignRecord,
    ) -> Result<FindingId, StoreError> {
        self.project(&finding.project)?;
        self.tracker.add_finding_checked(finding, record)
    }
    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        self.tracker.get_finding(id)
    }
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        self.tracker.update_finding(finding)
    }
    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        self.project(project)?;
        self.tracker.list_findings(project)
    }
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        self.tracker.withdrawals_by(actor)
    }
    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        self.tracker.add_alias(primary, alias)
    }
}

/// Handles for a split binding: projects and gates are the catalog's to
/// number, records and findings the tracker's.
pub struct KindRouted<'a> {
    pub catalog: &'a dyn Handles,
    pub tracker: &'a dyn Handles,
}

impl KindRouted<'_> {
    fn owner(&self, kind: Kind) -> &dyn Handles {
        // Every kind by name: a new kind must be routed on purpose.
        match kind {
            Kind::Project
            | Kind::Gate
            | Kind::State
            | Kind::Agent
            | Kind::Message
            | Kind::Workflow
            | Kind::WorkflowRun => self.catalog,
            Kind::Record | Kind::Finding => self.tracker,
        }
    }
}

impl Handles for KindRouted<'_> {
    fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError> {
        self.owner(kind).handle_of(kind, id)
    }
    fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError> {
        self.owner(kind).resolve_handle(kind, handle)
    }
}

/// Centralized agent and coverage-area registry (spec §2, identity spec §6a).
pub trait AgentRegistry {
    fn upsert_area(&self, area: AreaRecord) -> Result<AreaRecord, StoreError>;
    fn get_area(&self, area_key: &str) -> Result<Option<AreaRecord>, StoreError>;
    fn list_areas(&self) -> Result<Vec<AreaRecord>, StoreError>;

    fn register_agent(&self, args: RegisterAgentArgs) -> Result<AgentRecord, StoreError>;
    fn retire_agent(&self, id: &AgentId, at: At, by: &str) -> Result<AgentRecord, StoreError>;
    fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, StoreError>;
    fn agent_by_name(&self, name_key: &str) -> Result<Option<AgentRecord>, StoreError>;
    fn list_agents(
        &self,
        project: Option<&str>,
        include_retired: bool,
    ) -> Result<Vec<AgentRecord>, StoreError>;

    /// Append-only `(sequence, RegistryEvent)` log after `after_seq` for `fl/state` sync.
    fn registry_events_after(
        &self,
        after_seq: u64,
    ) -> Result<Vec<(u64, RegistryEvent)>, StoreError>;
    /// Import remote `RegistryEvent`s during `fl state sync`, deduplicating by `event_id`
    /// with byte-for-byte tamper verification and resolving offline `name_key` collisions.
    fn import_registry_events(&self, events: &[RegistryEvent]) -> Result<(), StoreError>;
}

/// Durable project/agent memory, handoff snapshots, and workflow runtime state (spec §1, §3).
pub trait StateStore {
    fn upsert_state(&self, args: UpsertStateArgs) -> Result<StateEntry, StoreError>;
    fn retire_memory(
        &self,
        scope: &StateScope,
        slug: &str,
        lifecycle: MemoryLifecycle,
        superseded_by: Option<StateId>,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError>;
    fn get_state(&self, id: &StateId) -> Result<Option<StateEntry>, StoreError>;
    fn get_state_by_key(
        &self,
        scope: &StateScope,
        concern: StateConcern,
        slug: &str,
    ) -> Result<Option<StateEntry>, StoreError>;
    fn list_state(
        &self,
        scope: Option<&StateScope>,
        concern: Option<StateConcern>,
    ) -> Result<Vec<StateEntry>, StoreError>;
    fn state_revisions(&self, id: &StateId) -> Result<Vec<StateRevision>, StoreError>;
    fn list_conflicts(&self) -> Result<Vec<StateConflict>, StoreError>;
    fn resolve_conflict(
        &self,
        id: &StateId,
        keep: &RevisionId,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError>;

    /// Append-only `(sequence, StateRevision)` log after `after_seq` for `fl/state` sync.
    fn state_revisions_after(
        &self,
        after_seq: u64,
    ) -> Result<Vec<(u64, StateRevision)>, StoreError>;
    /// Import remote `StateRevision`s during `fl state sync`, deduplicating by `RevisionId`
    /// with byte-for-byte tamper verification, aliasing unpublished local same-key collisions
    /// (`S_local -> S_remote`), and computing fast-forward, conflict, `append`, or `last_writer`
    /// merges using `merge_at`.
    fn import_state_revisions(
        &self,
        revisions: &[StateRevision],
        merge_at: At,
    ) -> Result<(), StoreError>;

    fn sync_cursor(&self, remote: &str) -> Result<Option<StateSyncCursor>, StoreError>;
    fn set_sync_cursor(&self, cursor: StateSyncCursor) -> Result<(), StoreError>;
}

/// Durable agent-to-agent and human-to-agent messaging store (FerroWire spec §1.4).
pub trait MessageStore {
    /// Record a new message and create initial `Pending` receipts for all resolved recipients.
    fn send_wire(
        &self,
        args: SendWireArgs,
    ) -> Result<(WireMessage, Vec<DeliveryReceipt>), StoreError>;

    /// Look up a single message by `MessageId`.
    fn get_wire_message(&self, id: &MessageId) -> Result<Option<WireMessage>, StoreError>;

    /// Query inbox items for a recipient (`name_key` or `AgentId` URN), ordered by `MessageId`.
    fn list_wire_inbox(&self, filter: &WireFilter) -> Result<Vec<WireInboxItem>, StoreError>;

    /// List all messages in a thread (`thread_id`) in chronological `MessageId` order.
    fn list_wire_thread(&self, thread_id: &ThreadId) -> Result<Vec<WireMessage>, StoreError>;

    /// List replies correlated to an `Ask` message (`correlation_id`).
    fn list_wire_replies(&self, correlation_id: &MessageId)
    -> Result<Vec<WireMessage>, StoreError>;

    /// List all receipts for a given `MessageId`.
    fn list_wire_receipts(
        &self,
        message_id: &MessageId,
    ) -> Result<Vec<DeliveryReceipt>, StoreError>;

    /// Transition a recipient's receipt (`Delivered`, `Acked`, or failed attempt increment).
    fn update_wire_receipt(
        &self,
        message_id: &MessageId,
        recipient: &str,
        status: DeliveryStatus,
        increment_attempts: bool,
        at: At,
    ) -> Result<Option<DeliveryReceipt>, StoreError>;

    /// Export messages after `after_seq` for peer catch-up and `fl/state` sync.
    fn wire_messages_after(&self, after_seq: u64) -> Result<Vec<(u64, WireMessage)>, StoreError>;

    /// Export receipt events after `after_seq` for peer catch-up and `fl/state` sync.
    fn wire_receipts_after(
        &self,
        after_seq: u64,
    ) -> Result<Vec<(u64, DeliveryReceipt)>, StoreError>;

    /// Import messages and receipts from a remote peer or `fl/state` branch, enforcing
    /// byte-for-byte immutability on `WireMessage` and monotonic convergence on `DeliveryReceipt`.
    ///
    /// Every item is judged alone. A message that fails validation, differs from the copy already
    /// stored under its id, or collides with another kind of id is skipped and listed in the
    /// returned [`WireImportReport`]; so is a receipt for a message this store does not hold. The
    /// other items are still imported. A skipped item is never stored. `Err` is for a store
    /// failure only. What an `Err` leaves behind depends on the store: the redb and FerroStore
    /// stores import the whole batch in one transaction, so nothing from the batch is kept;
    /// `MemStore` writes item by item, so an `Err` partway through can leave the earlier
    /// items imported. Callers must not assume an `Err` imported nothing.
    fn import_wire_batch(
        &self,
        messages: &[WireMessage],
        receipts: &[DeliveryReceipt],
    ) -> Result<WireImportReport, StoreError>;

    /// Delete transient (`durable == false`) messages whose receipts are all `Acked` or
    /// `Expired` and whose `created_at < before`. Returns the count of pruned messages.
    fn prune_wire(&self, before: At) -> Result<usize, StoreError>;
}

/// Directed workflow definitions and stateful workflow runs (workflow spec §2.1).
pub trait WorkflowStore {
    fn upsert_workflow_def(&self, def: WorkflowDef) -> Result<WorkflowDef, StoreError>;
    fn get_workflow_def(&self, id: &WorkflowId) -> Result<Option<WorkflowDef>, StoreError>;
    fn get_workflow_def_by_slug(
        &self,
        project: &ProjectId,
        slug: &str,
    ) -> Result<Option<WorkflowDef>, StoreError>;
    fn list_workflow_defs(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<WorkflowDef>, StoreError>;

    fn create_workflow_run(&self, run: WorkflowRun) -> Result<WorkflowRun, StoreError>;
    fn update_workflow_run(&self, run: WorkflowRun) -> Result<WorkflowRun, StoreError>;
    fn get_workflow_run(&self, id: &WorkflowRunId) -> Result<Option<WorkflowRun>, StoreError>;
    fn list_workflow_runs(
        &self,
        project: Option<&ProjectId>,
        workflow_id: Option<&WorkflowId>,
    ) -> Result<Vec<WorkflowRun>, StoreError>;

    /// Import a replicated `WorkflowDef` during `fl state sync`, preserving its canonical
    /// `WorkflowId` and registering it in `IDS` and `HANDLES`.
    fn import_workflow_def(&self, def: WorkflowDef) -> Result<(), StoreError>;

    /// Import a replicated `WorkflowRun` during `fl state sync`, preserving its canonical
    /// `WorkflowRunId` and registering it in `IDS` and `HANDLES`.
    fn import_workflow_run(&self, run: WorkflowRun) -> Result<(), StoreError>;
}

/// What a local store remembers about the GitHub repositories a tracker is
/// bound to (GitHub tracker spec §2.4): the repository's `node_id`, keyed by
/// the configured `owner/repo`, compared without regard to case; and the
/// ledger root of each repository whose GitHub ledger it knows (GitHub
/// ledger spec §6.1 step 4), keyed by `node_id`.
pub trait Bindings {
    fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError>;
    fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError>;
    fn ledger_root(&self, node_id: &str) -> Result<Option<String>, StoreError>;
    /// ⚠ An anchor never changes: a different root for a `node_id` that has
    /// one is `LedgerRootChanged`; the same root again is a no-op.
    fn set_ledger_root(&self, node_id: &str, commit: &str) -> Result<(), StoreError>;
}

/// Whether `node_id` and `commit` can be a ledger root (GitHub ledger spec
/// §6.1 step 4). `commit` is a full commit id: 40 lowercase hexadecimal
/// digits (SHA-1), or 64 (SHA-256). `node_id` is a GitHub node id: 1 to 128
/// of `A-Za-z0-9_=+/-`, starting with a letter.
///
/// ⚠ A root is the anchor every tamper check rests on (§3.5), so a value
/// that cannot be one is refused where it enters: the manifest's
/// consistency check and `init`. The `Err` says which part is wrong.
pub fn ledger_root_shape(node_id: &str, commit: &str) -> Result<(), String> {
    let hex = commit
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !(commit.len() == 40 || commit.len() == 64) || !hex {
        return Err(format!(
            "`{commit}` is not a full commit id: 40 or 64 lowercase hexadecimal digits"
        ));
    }
    node_id_shape(node_id)
}

/// Whether `node_id` can be a GitHub node id: 1 to 128 of `A-Za-z0-9_=+/-`,
/// starting with a letter. `init` asks this before it creates anything, so
/// a malformed binding leaves no branch behind.
pub fn node_id_shape(node_id: &str) -> Result<(), String> {
    let first = node_id
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic());
    let chars = node_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '=' | '+' | '/'));
    if !first || !chars || node_id.len() > 128 {
        return Err(format!(
            "`{node_id}` is not a GitHub node id: 1 to 128 letters, digits and `_-=+/`, \
             starting with a letter"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::ids::seq_iri;
    use crate::model::{CommandSpec, GateKind, PopulationDelivery};

    /// Every message along `e`'s source chain, as `{:#}` prints them.
    fn chain(e: &dyn std::error::Error) -> String {
        let mut said = e.to_string();
        let mut at = e.source();
        while let Some(s) = at {
            said.push_str(": ");
            said.push_str(&s.to_string());
            at = s.source();
        }
        said
    }

    // A wrapped ledger or routing fault is said once along the chain a
    // person reads, not once by the wrapper and again as its source.
    #[test]
    fn a_wrapped_fault_is_said_once() {
        let ledger = StoreError::from(LedgerFault::NotSetUp {
            repo: "acme/widgets".into(),
        });
        let routing = StoreError::from(RoutingFault::NoArea { declared: vec![] });
        for (e, phrase) in [
            (ledger, "has no GitHub ledger yet"),
            (routing, "needs an area for every new item"),
        ] {
            let said = chain(&e);
            assert_eq!(said.matches(phrase).count(), 1, "{said}");
        }
    }

    /// A tracker that must never be asked: every refusal below has to come
    /// from the binding's own check, before the tracker is reached.
    struct NeverAsked;
    impl Tracker for NeverAsked {
        fn add_record_with_area(
            &self,
            _: &ProjectId,
            _: &str,
            _: Option<&str>,
        ) -> Result<RecordId, StoreError> {
            unreachable!("the binding must refuse before the tracker is asked")
        }
        fn get_record(&self, _: &RecordId) -> Result<Option<Record>, StoreError> {
            unreachable!("not used")
        }
        fn list_records(&self, _: &ProjectId) -> Result<Vec<Record>, StoreError> {
            Ok(vec![])
        }
        fn set_record_state(&self, _: &RecordId, _: State) -> Result<(), StoreError> {
            unreachable!("not used")
        }
        fn add_finding(&self, _: Finding) -> Result<FindingId, StoreError> {
            unreachable!("the binding must refuse before the tracker is asked")
        }
        fn add_finding_checked(
            &self,
            _: Finding,
            _: ForeignRecord,
        ) -> Result<FindingId, StoreError> {
            unreachable!("the binding must refuse before the tracker is asked")
        }
        fn get_finding(&self, _: &FindingId) -> Result<Option<Finding>, StoreError> {
            unreachable!("not used")
        }
        fn update_finding(&self, _: &Finding) -> Result<(), StoreError> {
            unreachable!("not used")
        }
        fn list_findings(&self, _: &ProjectId) -> Result<Vec<Finding>, StoreError> {
            unreachable!("the binding must refuse before the tracker is asked")
        }
        fn withdrawals_by(&self, _: &str) -> Result<u64, StoreError> {
            unreachable!("not used")
        }
        fn add_alias(&self, _: &Iri, _: Iri) -> Result<(), StoreError> {
            unreachable!("not used")
        }
    }

    fn gate(catalog: &MemStore, p: &ProjectId) -> GateId {
        let kind = GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        });
        let sel = Selector::Glob {
            pattern: "**/*".into(),
        };
        catalog.add_gate(p, "g", kind, sel, 1, "c", "o").unwrap()
    }

    #[test]
    fn a_split_binding_refuses_a_project_the_catalog_never_held() {
        let catalog = MemStore::default();
        let t = CatalogChecked {
            catalog: &catalog,
            tracker: &NeverAsked,
        };
        let stranger = ProjectId(seq_iri(99));
        let err = t.add_record(&stranger, "t").unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        let err = t.list_findings(&stranger).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
    }

    #[test]
    fn a_split_binding_refuses_another_kind_where_a_project_or_record_is_needed() {
        let catalog = MemStore::default();
        let p = catalog.add_project("/p").unwrap();
        let g = gate(&catalog, &p);
        let t = CatalogChecked {
            catalog: &catalog,
            tracker: &NeverAsked,
        };
        let err = t.add_record(&ProjectId(g.0.clone()), "t").unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::WrongKind {
                    expected: Kind::Project,
                    found: Kind::Gate,
                    ..
                }
            ),
            "{err:?}"
        );
        let err = t
            .add_finding(Finding::raise(p, RecordId(g.0.clone()), "a", "c"))
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::WrongKind {
                    expected: Kind::Record,
                    found: Kind::Gate,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    // A split binding checks the project of a checked reference — and no
    // record: the reference is to the other tier.
    #[test]
    fn a_split_binding_checks_only_the_project_of_a_checked_reference() {
        use crate::routing::{ForeignRecord, Tier};
        let catalog = MemStore::default();
        let url = RecordId(Iri::parse("https://github.com/acme/widgets/issues/7").unwrap());
        let refuses = CatalogChecked {
            catalog: &catalog,
            tracker: &NeverAsked,
        };
        let stranger = ProjectId(seq_iri(99));
        let err = refuses
            .add_finding_checked(
                Finding::raise(stranger, url.clone(), "a", "c"),
                ForeignRecord::for_tests(url.clone(), "t", Tier::Github),
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        let p = catalog.add_project("/p").unwrap();
        let passes = CatalogChecked {
            catalog: &catalog,
            tracker: &catalog,
        };
        let id = passes
            .add_finding_checked(
                Finding::raise(p, url.clone(), "a", "c"),
                ForeignRecord::for_tests(url.clone(), "t", Tier::Github),
            )
            .unwrap();
        assert_eq!(catalog.get_finding(&id).unwrap().unwrap().record, url);
    }

    #[test]
    fn a_split_binding_passes_a_held_project_through_to_the_tracker() {
        let catalog = MemStore::default();
        let p = catalog.add_project("/p").unwrap();
        let t = CatalogChecked {
            catalog: &catalog,
            tracker: &NeverAsked,
        };
        assert_eq!(t.list_records(&p).unwrap(), vec![]);
    }

    #[test]
    fn kind_routed_asks_the_store_that_holds_each_kind() {
        let catalog = MemStore::default();
        let tracker = MemStore::default();
        let p = catalog.add_project("/p").unwrap();
        // MemStore ids are sequential, so both stores would mint the same
        // first id. Burn one in the tracker's store, so `tp` is an id the
        // catalog does not hold.
        tracker.add_project("/burned").unwrap();
        let tp = tracker.add_project("/q").unwrap();
        let r = tracker.add_record(&tp, "t").unwrap();
        let h = KindRouted {
            catalog: &catalog,
            tracker: &tracker,
        };
        assert_eq!(h.handle_of(Kind::Project, p.iri()).unwrap(), Some(1));
        assert_eq!(h.handle_of(Kind::Record, r.iri()).unwrap(), Some(1));
        assert_ne!(p, tp, "the routing check below needs two different ids");
        // A project the TRACKER's store holds is not asked of the tracker.
        assert_eq!(h.handle_of(Kind::Project, tp.iri()).unwrap(), None);
        assert_eq!(
            h.resolve_handle(Kind::Record, 1).unwrap().as_ref(),
            Some(r.iri())
        );
    }

    #[test]
    fn a_memory_store_remembers_a_binding_by_case_insensitive_name() {
        let s = MemStore::default();
        assert_eq!(s.bound_node_id("Acme/Widgets").unwrap(), None);
        s.bind_node_id("Acme/Widgets", "R_1").unwrap();
        assert_eq!(
            s.bound_node_id("ACME/widgets").unwrap().as_deref(),
            Some("R_1")
        );
    }

    // Spec §1.4: a ledger with nowhere to publish — every local store —
    // answers a flush with `Nothing`, and changes nothing.
    #[test]
    fn a_local_store_has_nowhere_to_publish_so_its_flush_does_nothing() {
        use crate::at::At;
        use crate::decision::{Decision, Flushed, Outcome};
        use crate::log::AttemptStatus;
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let d = Decision {
            id: seq_iri(90),
            at: At::from_unix_millis(1),
            record: r,
            finding: None,
            outcome: Outcome::Attempt {
                status: AttemptStatus::Completed,
            },
            rests_on: vec![],
        };
        assert_eq!(s.flush(d).unwrap(), Flushed::NOTHING);
    }

    // Spec §3.5: the anchor never changes. A different root for a
    // repository that has one is refused, the same one again is not.
    #[test]
    fn a_memory_store_records_a_ledger_root_once() {
        let s = MemStore::default();
        assert_eq!(s.ledger_root("R_1").unwrap(), None);
        s.set_ledger_root("R_1", "abc").unwrap();
        s.set_ledger_root("R_1", "abc").unwrap();
        let err = s.set_ledger_root("R_1", "def").unwrap_err();
        assert!(
            matches!(err, StoreError::LedgerRootChanged { ref held, ref found, .. } if held == "abc" && found == "def"),
            "{err:?}"
        );
        assert_eq!(s.ledger_root("R_1").unwrap().as_deref(), Some("abc"));
        assert_eq!(s.ledger_root("R_2").unwrap(), None);
    }

    // Spec §7: only a GitHub that could not be reached, a spent rate limit,
    // or a ledger others kept appending to clears up by waiting. Every
    // other refusal names something to fix first.
    #[test]
    fn only_an_unreachable_a_rate_limited_or_a_contended_ledger_is_transient() {
        use crate::fault::LedgerFault;
        let transient = [
            StoreError::Unreachable {
                store: "s".into(),
                cause: "c".into(),
            },
            StoreError::RateLimited { reset: "r".into() },
            StoreError::Contended {
                store: "s".into(),
                tries: 5,
            },
        ];
        for e in &transient {
            assert!(e.is_transient(), "{e}");
        }
        let lasting = [
            StoreError::Backend("b".into()),
            StoreError::Credential("c".into()),
            StoreError::Tampered {
                id: seq_iri(1),
                detail: "d".into(),
            },
            StoreError::NotOwned {
                id: seq_iri(1),
                searched: vec![],
            },
            StoreError::RestsOnLocalEntry {
                decision: seq_iri(1),
                entry: seq_iri(2),
            },
            StoreError::Ledger(LedgerFault::NotSetUp {
                repo: "acme/widgets".into(),
            }),
            // Routing spec §3.6: an escalation is finished or abandoned, and a
            // tombstone is followed — waiting changes neither.
            StoreError::Escalating {
                id: seq_iri(1),
                to_finish: "fl record escalate x".into(),
            },
            StoreError::Escalated {
                from: seq_iri(1),
                to: seq_iri(2),
            },
            StoreError::Escalation(crate::escalation::EscalationFault::NotMarked {
                id: seq_iri(1),
            }),
        ];
        for e in &lasting {
            assert!(!e.is_transient(), "{e}");
        }
    }

    #[test]
    fn a_message_as_a_clause_drops_its_one_final_period() {
        let e = StoreError::RestsOnLocalEntry {
            decision: seq_iri(1),
            entry: seq_iri(2),
        };
        assert!(
            as_clause(&e).ends_with("Nothing was published"),
            "{}",
            as_clause(&e)
        );
        assert_eq!(as_clause(&"no period"), "no period");
    }

    #[test]
    fn a_ledger_fault_reads_as_itself() {
        use crate::fault::LedgerFault;
        let fault = LedgerFault::NotSetUp {
            repo: "acme/widgets".into(),
        };
        let e: StoreError = fault.clone().into();
        assert_eq!(e.to_string(), fault.to_string());
    }

    // Ruling 20: a root is the anchor every tamper check rests on, so a
    // value that cannot be one is refused where it enters.
    #[test]
    fn a_ledger_root_is_a_full_commit_id_and_a_github_node_id() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        for (node, commit) in [
            ("R_1", sha1),
            ("R_kgDOAbCdEf", sha256),
            ("MDEwOlJlcG9zaXRvcnkxMjk2MjY5", sha1),
            ("MDEwOlJlcG9zaXRvcnkx+/==", sha1),
        ] {
            assert_eq!(ledger_root_shape(node, commit), Ok(()), "{node} {commit}");
        }
        for (node, commit, names) in [
            ("R_1", "abc", "abc"),
            (
                "R_1",
                "0123456789ABCDEF0123456789ABCDEF01234567",
                "0123456789ABCDEF",
            ),
            (
                "R_1",
                "0123456789abcdef0123456789abcdef0123456",
                "0123456789abcdef0123456",
            ),
            ("R_1", "g123456789abcdef0123456789abcdef01234567", "g123"),
            ("", sha1, "``"),
            ("1R", sha1, "1R"),
            ("R 1", sha1, "R 1"),
            ("R_1;", sha1, "R_1;"),
        ] {
            let why = ledger_root_shape(node, commit).expect_err(node);
            assert!(why.contains(names), "{why}");
        }
        let longest = format!("R{}", "a".repeat(127));
        assert_eq!(longest.len(), 128);
        assert_eq!(ledger_root_shape(&longest, sha1), Ok(()), "128 characters");
        let long = format!("R{}", "a".repeat(128));
        assert!(ledger_root_shape(&long, sha1).is_err(), "129 characters");
    }
}
