//! One handle for either store engine.
//!
//! Every command, the hub and the wire daemon open their store through
//! [`open_store`] and hold an [`AnyStore`], so the engine behind a store is
//! chosen in one place. `AnyStore` implements all thirteen store traits by
//! delegation and carries the inherent methods callers use beyond them.
//!
//! * The engine is chosen by [`choose_backend`]: a `.fls` path is always a
//!   [`FerroStore`]; any other path takes `FL_STORE_BACKEND` (`redb` or
//!   `ferro`), then [`DEFAULT_BACKEND`].
//! * A logical path such as `fl.redb` names the same store under either
//!   engine: [`physical_path`] maps it to the sibling `fl.fls` when the engine
//!   is FerroStore, so a config entry never changes at cutover.

use crate::manifest::{Manifest, ManifestError};
use crate::{FerroStore, ImportReport, RedbStore, backend};
use fl_core::{
    AgentId, AgentRecord, AgentRegistry, AreaRecord, At, Attempt, Bindings, CachedSegment, Catalog,
    Decision, DeliveryReceipt, DeliveryStatus, Escalations, Finding, FindingId, Flushed,
    ForeignRecord, GateDef, GateId, GateKind, GateRun, Handles, Iri, Kind, Ledger, LedgerCache,
    Mark, MemoryLifecycle, MessageId, MessageStore, Outbox, Pending, Project, ProjectId, Record,
    RecordId, RegisterAgentArgs, RegistryEvent, RevisionId, Routes, RoutingMap, Selector,
    SendWireArgs, State, StateConcern, StateConflict, StateEntry, StateId, StateRevision,
    StateScope, StateStore, StateSyncCursor, StoreError, ThreadId, Tombstone, Tracker, Transition,
    UpsertStateArgs, WireFilter, WireImportReport, WireInboxItem, WireMessage, WorkflowDef,
    WorkflowId, WorkflowRun, WorkflowRunId, WorkflowStore,
};
use std::path::{Path, PathBuf};

/// The engine behind a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// redb: single-process, one `.redb` file.
    Redb,
    /// FerroStore: multi-process embedded engine, one `.fls` file.
    Ferro,
}

impl BackendKind {
    /// Parse a `FL_STORE_BACKEND` value.
    pub fn parse(raw: &str) -> Result<Self, StoreError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "redb" => Ok(Self::Redb),
            "ferro" | "ferrostore" | "fls" => Ok(Self::Ferro),
            other => Err(backend(format!(
                "unknown store backend `{other}` in {BACKEND_ENV}: expected `redb` or `ferro`"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Redb => "redb",
            Self::Ferro => "ferro",
        }
    }
}

/// The environment variable that selects the engine for a path that does not
/// name one by its extension.
pub const BACKEND_ENV: &str = "FL_STORE_BACKEND";

/// The engine a path takes when neither the path nor the environment names one.
pub const DEFAULT_BACKEND: BackendKind = BackendKind::Redb;

/// The extension of a FerroStore file.
pub const FERRO_EXTENSION: &str = "fls";

/// Which engine opens `path`: a `.fls` path is always FerroStore, the explicit
/// path winning over `env`; any other path takes `env` (the value of
/// [`BACKEND_ENV`]), then [`DEFAULT_BACKEND`].
pub fn choose_backend(path: &Path, env: Option<&str>) -> Result<BackendKind, StoreError> {
    if path.extension().and_then(|e| e.to_str()) == Some(FERRO_EXTENSION) {
        return Ok(BackendKind::Ferro);
    }
    match env.map(str::trim).filter(|v| !v.is_empty()) {
        Some(raw) => BackendKind::parse(raw),
        None => Ok(DEFAULT_BACKEND),
    }
}

/// The file `kind` keeps the store named `path` in. Redb keeps it at `path`;
/// FerroStore keeps it at the sibling `.fls`, so a store's configured path
/// is the same before and after a cutover.
pub fn physical_path(path: &Path, kind: BackendKind) -> PathBuf {
    match kind {
        BackendKind::Redb => path.to_path_buf(),
        BackendKind::Ferro => path.with_extension(FERRO_EXTENSION),
    }
}

/// How to open a store.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// Force an engine; `None` runs [`choose_backend`] on the path and
    /// [`BACKEND_ENV`].
    pub backend: Option<BackendKind>,
    /// Times to retry a redb file another process holds, 15 ms apart.
    /// FerroStore is multi-process safe and ignores it.
    pub retries: u32,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            backend: None,
            retries: 200,
        }
    }
}

/// Open the store named `path` with the default options.
pub fn open_store(path: &Path) -> Result<AnyStore, StoreError> {
    open_store_with(path, &StoreOptions::default())
}

/// Open the store named `path`.
pub fn open_store_with(path: &Path, options: &StoreOptions) -> Result<AnyStore, StoreError> {
    let kind = match options.backend {
        Some(kind) => kind,
        None => choose_backend(path, std::env::var(BACKEND_ENV).ok().as_deref())?,
    };
    let physical = physical_path(path, kind);
    let label = physical.display().to_string();
    let backend_store = match kind {
        BackendKind::Redb => {
            Backend::Redb(RedbStore::open_with_retries(&physical, options.retries)?)
        }
        BackendKind::Ferro => {
            if !physical.exists() && path != physical && path.exists() {
                return Err(backend(format!(
                    "{} holds a redb store and {} does not exist yet: migrate it first",
                    path.display(),
                    physical.display()
                )));
            }
            Backend::Ferro(Box::new(FerroStore::open(&physical)?))
        }
    };
    Ok(AnyStore {
        backend: backend_store,
        label,
    })
}

enum Backend {
    Ferro(Box<FerroStore>),
    Redb(RedbStore),
}

/// A store on either engine. See the [module documentation](self).
pub struct AnyStore {
    backend: Backend,
    label: String,
}

impl From<RedbStore> for AnyStore {
    fn from(store: RedbStore) -> Self {
        let label = store.label().to_string();
        Self {
            backend: Backend::Redb(store),
            label,
        }
    }
}

impl From<FerroStore> for AnyStore {
    fn from(store: FerroStore) -> Self {
        Self {
            backend: Backend::Ferro(Box::new(store)),
            label: "memory".to_string(),
        }
    }
}

impl AnyStore {
    /// An in-memory FerroStore, for tests.
    pub fn in_memory() -> Self {
        FerroStore::in_memory().into()
    }

    pub fn kind(&self) -> BackendKind {
        match self.backend {
            Backend::Redb(_) => BackendKind::Redb,
            Backend::Ferro(_) => BackendKind::Ferro,
        }
    }

    /// The store's name in messages: the file it was opened from.
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn as_redb(&self) -> Option<&RedbStore> {
        match &self.backend {
            Backend::Redb(s) => Some(s),
            Backend::Ferro(_) => None,
        }
    }

    pub fn as_ferro(&self) -> Option<&FerroStore> {
        match &self.backend {
            Backend::Ferro(s) => Some(s),
            Backend::Redb(_) => None,
        }
    }

    /// Whether this store holds `id`. An I/O failure is an error, never `false`.
    pub fn owns(&self, id: &Iri) -> Result<bool, StoreError> {
        match &self.backend {
            Backend::Ferro(s) => s.owns(id),
            Backend::Redb(s) => s.owns(id),
        }
    }

    /// The hash of the manifest `project` was imported from, or `None` if this
    /// store authors it.
    pub fn imported_hash(&self, project: &ProjectId) -> Result<Option<String>, StoreError> {
        match &self.backend {
            Backend::Ferro(s) => s.imported_hash(project),
            Backend::Redb(s) => s.imported_hash(project),
        }
    }

    /// `Imported`, naming `action`, when this store imported `project`.
    pub fn refuse_if_imported(
        &self,
        project: &ProjectId,
        action: &'static str,
    ) -> Result<(), StoreError> {
        if self.imported_hash(project)?.is_some() {
            return Err(StoreError::Imported {
                id: project.iri().clone(),
                action,
            });
        }
        Ok(())
    }

    /// Whether any project in this store has a routing map.
    pub fn holds_routing(&self) -> Result<bool, StoreError> {
        match &self.backend {
            Backend::Ferro(s) => s.holds_routing(),
            Backend::Redb(s) => s.holds_routing(),
        }
    }

    /// Whether this store records any ledger root.
    pub fn holds_a_ledger_root(&self) -> Result<bool, StoreError> {
        match &self.backend {
            Backend::Ferro(s) => s.holds_a_ledger_root(),
            Backend::Redb(s) => s.holds_a_ledger_root(),
        }
    }

    /// Write `project`'s routing map.
    pub fn set_routes(&self, project: &ProjectId, map: &RoutingMap) -> Result<(), StoreError> {
        match &self.backend {
            Backend::Ferro(s) => s.set_routes(project, map),
            Backend::Redb(s) => s.set_routes(project, map),
        }
    }

    /// The format version the store file records.
    pub fn format_version(&self) -> Result<Option<u64>, StoreError> {
        match &self.backend {
            Backend::Ferro(_) => Err(not_on_ferro("format_version")),
            Backend::Redb(s) => s.format_version(),
        }
    }

    /// Export `project`, which this store must author.
    pub fn export_manifest(
        &self,
        project: &ProjectId,
        commit: &str,
        exported_at_unix: u64,
        repository_node_id: Option<&str>,
    ) -> Result<Manifest, ManifestError> {
        match &self.backend {
            Backend::Ferro(_) => Err(not_on_ferro("export_manifest").into()),
            Backend::Redb(s) => {
                s.export_manifest(project, commit, exported_at_unix, repository_node_id)
            }
        }
    }

    /// Write the manifest's project, gates and transitions, atomically.
    pub fn import_manifest(&self, m: &Manifest, root: &str) -> Result<ImportReport, ManifestError> {
        match &self.backend {
            Backend::Ferro(_) => Err(not_on_ferro("import_manifest").into()),
            Backend::Redb(s) => s.import_manifest(m, root),
        }
    }
}

fn not_on_ferro(what: &str) -> StoreError {
    backend(format!(
        "{what} is not available on a FerroStore yet; use {BACKEND_ENV}=redb for this command"
    ))
}

/// Implement each store trait on [`AnyStore`] by calling the engine's own
/// implementation (always by trait path, so an inherent method of the same
/// name can never be picked instead). Listing every method, defaulted ones
/// included, keeps an engine's override of a default in force.
macro_rules! delegate {
    ($($trait:ident {
        $(fn $name:ident(&self $(, $arg:ident : $ty:ty)* $(,)?) -> $ret:ty;)*
    })*) => {$(
        impl $trait for AnyStore {
            $(fn $name(&self $(, $arg: $ty)*) -> $ret {
                match &self.backend {
                    Backend::Ferro(s) => $trait::$name(&**s $(, $arg)*),
                    Backend::Redb(s) => $trait::$name(s $(, $arg)*),
                }
            })*
        }
    )*};
}

delegate! {
    Catalog {
        fn add_project(&self, root: &str) -> Result<ProjectId, StoreError>;
        fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError>;
        fn list_projects(&self) -> Result<Vec<Project>, StoreError>;
        fn add_gate(&self, project: &ProjectId, name: &str, kind: GateKind, selector: Selector, min_population: u64, authored_at_commit: &str, authored_by: &str) -> Result<GateId, StoreError>;
        fn get_gate(&self, id: &GateId) -> Result<Option<GateDef>, StoreError>;
        fn list_gates(&self, project: &ProjectId) -> Result<Vec<GateDef>, StoreError>;
        fn update_gate(&self, def: &GateDef) -> Result<(), StoreError>;
        fn add_transition(&self, t: Transition) -> Result<(), StoreError>;
        fn get_transition(&self, project: &ProjectId, name: &str) -> Result<Option<Transition>, StoreError>;
        fn list_transitions(&self, project: &ProjectId) -> Result<Vec<Transition>, StoreError>;
        fn kind_of(&self, id: &Iri) -> Result<Kind, StoreError>;
    }
    Routes {
        fn routes(&self, project: &ProjectId) -> Result<Option<RoutingMap>, StoreError>;
    }
    Tracker {
        fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError>;
        fn add_record_with_area(&self, project: &ProjectId, title: &str, area: Option<&str>) -> Result<RecordId, StoreError>;
        fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError>;
        fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError>;
        fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError>;
        fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError>;
        fn add_finding_checked(&self, finding: Finding, record: ForeignRecord) -> Result<FindingId, StoreError>;
        fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError>;
        fn update_finding(&self, finding: &Finding) -> Result<(), StoreError>;
        fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError>;
        fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError>;
        fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError>;
    }
    Escalations {
        fn mark(&self, id: &Iri, mark: &Mark) -> Result<(), StoreError>;
        fn mark_of(&self, id: &Iri) -> Result<Option<Mark>, StoreError>;
        fn unmark(&self, id: &Iri) -> Result<(), StoreError>;
        fn tombstone(&self, id: &Iri, to: &Iri) -> Result<Tombstone, StoreError>;
        fn tombstone_of(&self, id: &Iri) -> Result<Option<Tombstone>, StoreError>;
    }
    Ledger {
        fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError>;
        fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError>;
        fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError>;
        fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError>;
        fn flush(&self, _decision: Decision) -> Result<Flushed, StoreError>;
    }
    Outbox {
        fn unpublished(&self, repo: &str, after: &Iri) -> Result<Pending, StoreError>;
        fn is_published(&self, repo: &str, id: &Iri) -> Result<bool, StoreError>;
        fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError>;
        fn settle(&self, repo: &str, published: &[Iri], set_aside: &[Iri]) -> Result<(), StoreError>;
        fn cutover(&self, repo: &str) -> Result<Option<Iri>, StoreError>;
        fn set_cutover(&self, repo: &str, id: &Iri) -> Result<(), StoreError>;
    }
    Handles {
        fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError>;
        fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError>;
    }
    Bindings {
        fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError>;
        fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError>;
        fn ledger_root(&self, node_id: &str) -> Result<Option<String>, StoreError>;
        fn set_ledger_root(&self, node_id: &str, commit: &str) -> Result<(), StoreError>;
    }
    LedgerCache {
        fn last_head(&self, repo: &str) -> Result<Option<String>, StoreError>;
        fn cached(&self, repo: &str, path: &str) -> Result<Option<CachedSegment>, StoreError>;
        fn cached_under(&self, repo: &str, dir: &str) -> Result<Vec<(String, CachedSegment)>, StoreError>;
        fn remember(&self, repo: &str, head: &str, segments: &[(String, CachedSegment)]) -> Result<(), StoreError>;
    }
    AgentRegistry {
        fn upsert_area(&self, area: AreaRecord) -> Result<AreaRecord, StoreError>;
        fn get_area(&self, area_key: &str) -> Result<Option<AreaRecord>, StoreError>;
        fn list_areas(&self) -> Result<Vec<AreaRecord>, StoreError>;
        fn register_agent(&self, args: RegisterAgentArgs) -> Result<AgentRecord, StoreError>;
        fn retire_agent(&self, id: &AgentId, at: At, by: &str) -> Result<AgentRecord, StoreError>;
        fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, StoreError>;
        fn agent_by_name(&self, name_key: &str) -> Result<Option<AgentRecord>, StoreError>;
        fn list_agents(&self, project: Option<&str>, include_retired: bool) -> Result<Vec<AgentRecord>, StoreError>;
        fn registry_events_after(&self, after_seq: u64) -> Result<Vec<(u64, RegistryEvent)>, StoreError>;
        fn import_registry_events(&self, events: &[RegistryEvent]) -> Result<(), StoreError>;
    }
    StateStore {
        fn upsert_state(&self, args: UpsertStateArgs) -> Result<StateEntry, StoreError>;
        fn retire_memory(&self, scope: &StateScope, slug: &str, lifecycle: MemoryLifecycle, superseded_by: Option<StateId>, at: At, by: &str) -> Result<StateEntry, StoreError>;
        fn get_state(&self, id: &StateId) -> Result<Option<StateEntry>, StoreError>;
        fn get_state_by_key(&self, scope: &StateScope, concern: StateConcern, slug: &str) -> Result<Option<StateEntry>, StoreError>;
        fn list_state(&self, scope: Option<&StateScope>, concern: Option<StateConcern>) -> Result<Vec<StateEntry>, StoreError>;
        fn state_revisions(&self, id: &StateId) -> Result<Vec<StateRevision>, StoreError>;
        fn list_conflicts(&self) -> Result<Vec<StateConflict>, StoreError>;
        fn resolve_conflict(&self, id: &StateId, keep: &RevisionId, at: At, by: &str) -> Result<StateEntry, StoreError>;
        fn state_revisions_after(&self, after_seq: u64) -> Result<Vec<(u64, StateRevision)>, StoreError>;
        fn import_state_revisions(&self, revisions: &[StateRevision], merge_at: At) -> Result<(), StoreError>;
        fn sync_cursor(&self, remote: &str) -> Result<Option<StateSyncCursor>, StoreError>;
        fn set_sync_cursor(&self, cursor: StateSyncCursor) -> Result<(), StoreError>;
    }
    MessageStore {
        fn send_wire(&self, args: SendWireArgs) -> Result<(WireMessage, Vec<DeliveryReceipt>), StoreError>;
        fn get_wire_message(&self, id: &MessageId) -> Result<Option<WireMessage>, StoreError>;
        fn list_wire_inbox(&self, filter: &WireFilter) -> Result<Vec<WireInboxItem>, StoreError>;
        fn list_wire_thread(&self, thread_id: &ThreadId) -> Result<Vec<WireMessage>, StoreError>;
        fn list_wire_replies(&self, correlation_id: &MessageId) -> Result<Vec<WireMessage>, StoreError>;
        fn list_wire_receipts(&self, message_id: &MessageId) -> Result<Vec<DeliveryReceipt>, StoreError>;
        fn update_wire_receipt(&self, message_id: &MessageId, recipient: &str, status: DeliveryStatus, increment_attempts: bool, at: At) -> Result<Option<DeliveryReceipt>, StoreError>;
        fn wire_messages_after(&self, after_seq: u64) -> Result<Vec<(u64, WireMessage)>, StoreError>;
        fn wire_receipts_after(&self, after_seq: u64) -> Result<Vec<(u64, DeliveryReceipt)>, StoreError>;
        fn import_wire_batch(&self, messages: &[WireMessage], receipts: &[DeliveryReceipt]) -> Result<WireImportReport, StoreError>;
        fn prune_wire(&self, before: At) -> Result<usize, StoreError>;
    }
    WorkflowStore {
        fn upsert_workflow_def(&self, def: WorkflowDef) -> Result<WorkflowDef, StoreError>;
        fn get_workflow_def(&self, id: &WorkflowId) -> Result<Option<WorkflowDef>, StoreError>;
        fn get_workflow_def_by_slug(&self, project: &ProjectId, slug: &str) -> Result<Option<WorkflowDef>, StoreError>;
        fn list_workflow_defs(&self, project: Option<&ProjectId>) -> Result<Vec<WorkflowDef>, StoreError>;
        fn create_workflow_run(&self, run: WorkflowRun) -> Result<WorkflowRun, StoreError>;
        fn update_workflow_run(&self, run: WorkflowRun) -> Result<WorkflowRun, StoreError>;
        fn get_workflow_run(&self, id: &WorkflowRunId) -> Result<Option<WorkflowRun>, StoreError>;
        fn list_workflow_runs(&self, project: Option<&ProjectId>, workflow_id: Option<&WorkflowId>) -> Result<Vec<WorkflowRun>, StoreError>;
        fn import_workflow_def(&self, def: WorkflowDef) -> Result<(), StoreError>;
        fn import_workflow_run(&self, run: WorkflowRun) -> Result<(), StoreError>;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::conformance::{Single, SplitOver};
    use tempfile::TempDir;

    /// A store of each engine, with the directory it lives in kept alive.
    fn on(kind: BackendKind) -> (AnyStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = open_store_with(
            &dir.path().join("s.redb"),
            &StoreOptions {
                backend: Some(kind),
                ..StoreOptions::default()
            },
        )
        .unwrap();
        (store, dir)
    }

    fn both() -> [BackendKind; 2] {
        [BackendKind::Redb, BackendKind::Ferro]
    }

    #[test]
    fn any_store_meets_every_store_contract_on_both_engines() {
        for kind in both() {
            let fresh = || on(kind);
            let single = || {
                let (s, g) = fresh();
                Single(s, g)
            };
            let split = || {
                let (s, g) = fresh();
                SplitOver(s, g)
            };
            fl_core::conformance::catalog(fresh);
            fl_core::conformance::tracker(single);
            fl_core::conformance::ledger(single);
            fl_core::conformance::ledger(split);
            fl_core::conformance::split_ledger(split);
            fl_core::conformance::all_roles(single);
            fl_core::conformance::local_handles(fresh);
            fl_core::conformance::ledger_cache(fresh);
            fl_core::conformance::agent_registry(fresh);
            fl_core::conformance::state_store(fresh);
            fl_core::conformance::message_store(fresh);
            fl_core::conformance::workflow_store(fresh);
            fl_core::conformance::escalations(single);
        }
    }

    #[test]
    fn a_fls_path_is_ferrostore_and_other_paths_follow_the_environment() {
        let fls = Path::new("/x/fl.fls");
        let redb = Path::new("/x/fl.redb");
        // An explicit .fls path wins over the environment.
        assert_eq!(choose_backend(fls, None).unwrap(), BackendKind::Ferro);
        assert_eq!(
            choose_backend(fls, Some("redb")).unwrap(),
            BackendKind::Ferro
        );
        assert_eq!(choose_backend(redb, None).unwrap(), DEFAULT_BACKEND);
        assert_eq!(
            choose_backend(redb, Some("ferro")).unwrap(),
            BackendKind::Ferro
        );
        assert_eq!(
            choose_backend(redb, Some(" Redb ")).unwrap(),
            BackendKind::Redb
        );
        assert_eq!(choose_backend(redb, Some("")).unwrap(), DEFAULT_BACKEND);
        let err = choose_backend(redb, Some("sqlite")).unwrap_err();
        assert!(err.to_string().contains("sqlite"), "{err}");
    }

    #[test]
    fn a_store_keeps_one_logical_path_across_engines() {
        let logical = Path::new("/x/fl.redb");
        assert_eq!(physical_path(logical, BackendKind::Redb), logical);
        assert_eq!(
            physical_path(logical, BackendKind::Ferro),
            Path::new("/x/fl.fls")
        );
        assert_eq!(
            physical_path(Path::new("/x/fl.fls"), BackendKind::Ferro),
            Path::new("/x/fl.fls")
        );
    }

    #[test]
    fn each_engine_persists_across_reopen_through_open_store() {
        for kind in both() {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("s.redb");
            let opts = StoreOptions {
                backend: Some(kind),
                ..StoreOptions::default()
            };
            let project = {
                let store = open_store_with(&path, &opts).unwrap();
                assert_eq!(store.kind(), kind);
                store.add_project("/repo").unwrap()
            };
            let store = open_store_with(&path, &opts).unwrap();
            assert_eq!(store.get_project(&project).unwrap().unwrap().root, "/repo");
            assert!(
                physical_path(&path, kind).exists(),
                "{kind:?} wrote its file"
            );
        }
    }

    #[test]
    fn opening_a_fls_store_beside_an_unmigrated_redb_asks_for_a_migration() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.redb");
        drop(RedbStore::open(&path).unwrap());
        let err = open_store_with(
            &path,
            &StoreOptions {
                backend: Some(BackendKind::Ferro),
                ..StoreOptions::default()
            },
        )
        .err()
        .expect("must refuse");
        assert!(err.to_string().contains("migrate"), "{err}");
        assert!(!dir.path().join("s.fls").exists(), "nothing was created");
    }

    #[test]
    fn the_label_names_the_file_the_store_was_opened_from() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.redb");
        let store = open_store_with(
            &path,
            &StoreOptions {
                backend: Some(BackendKind::Ferro),
                ..StoreOptions::default()
            },
        )
        .unwrap();
        assert!(store.label().ends_with("s.fls"), "{}", store.label());
        let (redb, _g) = on(BackendKind::Redb);
        assert!(redb.label().ends_with("s.redb"), "{}", redb.label());
    }

    #[test]
    fn the_inherent_questions_agree_on_both_engines() {
        for kind in both() {
            let (store, _g) = on(kind);
            let project = store.add_project("/repo").unwrap();
            assert!(store.owns(project.iri()).unwrap(), "{kind:?}");
            let stranger = Iri::parse("urn:uuid:00000000-0000-7000-8000-000000000001").unwrap();
            assert!(!store.owns(&stranger).unwrap(), "{kind:?}");
            assert_eq!(store.imported_hash(&project).unwrap(), None, "{kind:?}");
            store.refuse_if_imported(&project, "do a thing").unwrap();
            assert!(!store.holds_routing().unwrap(), "{kind:?}");
            assert!(!store.holds_a_ledger_root().unwrap(), "{kind:?}");
            store.set_ledger_root("R_1", "abc123").unwrap();
            assert!(store.holds_a_ledger_root().unwrap(), "{kind:?}");
            let missing = ProjectId(stranger.clone());
            assert!(
                matches!(
                    store.imported_hash(&missing),
                    Err(StoreError::NotOwned { .. })
                ),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn what_ferrostore_cannot_do_yet_is_refused_by_name() {
        let (store, _g) = on(BackendKind::Ferro);
        let err = store.format_version().unwrap_err();
        assert!(err.to_string().contains("format_version"), "{err}");
        let project = store.add_project("/repo").unwrap();
        let err = store.export_manifest(&project, "c", 0, None).unwrap_err();
        assert!(err.to_string().contains("export_manifest"), "{err}");
    }

    #[test]
    fn the_redb_arm_still_serves_manifests_and_versions() {
        let (store, _g) = on(BackendKind::Redb);
        assert!(store.format_version().unwrap().is_some());
        let project = store.add_project("/repo").unwrap();
        let manifest = store.export_manifest(&project, "c0ffee", 1, None).unwrap();
        let (other, _g2) = on(BackendKind::Redb);
        let report = other.import_manifest(&manifest, "/elsewhere").unwrap();
        assert_eq!(report.project, project);
        assert!(other.imported_hash(&project).unwrap().is_some());
    }
}
