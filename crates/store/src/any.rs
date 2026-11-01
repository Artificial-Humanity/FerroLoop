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
    /// Whether a ferro selection beside a lone `.redb` migrates it (verified,
    /// see [`crate::migrate::migrate_verified`]) instead of refusing. Default
    /// `true`. When the migration cannot run or does not verify, the store
    /// opens on the `.redb` with a warning, so every process that cannot
    /// migrate keeps using one engine.
    pub auto_migrate: bool,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            backend: None,
            retries: 200,
            auto_migrate: true,
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
    let (backend_store, label) = match kind {
        BackendKind::Redb => (
            Backend::Redb(RedbStore::open_with_retries(&physical, options.retries)?),
            physical.display().to_string(),
        ),
        BackendKind::Ferro => open_ferro(path, &physical, options)?,
    };
    Ok(AnyStore {
        backend: backend_store,
        label,
    })
}

/// Whether `err` is redb saying another holder has the file: the case an open
/// retries, and the case a migration cannot run in. (FerroStore is
/// multi-process safe and never reports it.)
pub fn is_redb_busy(err: &StoreError) -> bool {
    let StoreError::Unreachable { cause, .. } = err else {
        return false;
    };
    cause.contains("Database already open") || cause.contains("Cannot acquire lock")
}

/// One line on stderr; a closed stderr is not a panic.
fn warn(text: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{text}");
}

/// Open the ferro store at `physical`, the file of the logical `path`.
///
/// A lone `.redb` beside it (`physical` absent, `path` a different file that
/// exists) is migrated first when `options.auto_migrate` is set. A migration
/// that cannot run or does not verify leaves no `.fls`: the store opens on the
/// `.redb` instead, so that every process which cannot migrate agrees on one
/// engine. A `.fls` that appears meanwhile (another process migrated first)
/// is opened.
fn open_ferro(
    path: &Path,
    physical: &Path,
    options: &StoreOptions,
) -> Result<(Backend, String), StoreError> {
    let ferro = |physical: &Path| -> Result<(Backend, String), StoreError> {
        Ok((
            Backend::Ferro(Box::new(FerroStore::open(physical)?)),
            physical.display().to_string(),
        ))
    };
    let lone_redb = !physical.exists() && path != physical && path.exists();
    if !lone_redb {
        return ferro(physical);
    }
    if !options.auto_migrate || !path.is_file() {
        return Err(backend(format!(
            "{} holds a redb store and {} does not exist yet: migrate it first \
             (`fl store migrate`), or set {BACKEND_ENV}=redb to keep using the .redb file",
            path.display(),
            physical.display()
        )));
    }
    let stay_on_redb = |why: String| -> Result<(Backend, String), StoreError> {
        warn(&format!(
            "warning: {} stays on redb: {why}; no {} was created",
            path.display(),
            physical.display()
        ));
        Ok((
            Backend::Redb(RedbStore::open_with_retries(path, options.retries)?),
            path.display().to_string(),
        ))
    };
    // A process still holding the redb must not wait out a migration's own
    // 3 s of retries: ask once. (Dropped at once; the migration opens it again.)
    let attempt = RedbStore::open_with_retries(path, 0)
        .map(drop)
        .and_then(|()| crate::migrate::migrate_verified(path, physical));
    match attempt {
        Ok(report) if report.is_clean() => ferro(physical),
        Ok(report) => {
            let first = report.mismatches().into_iter().next();
            let why = match first {
                Some(t) => format!(
                    "the migration did not verify (first mismatch: `{}`, redb {} rows, ferro {} \
                     rows); run `fl store migrate` for the full report",
                    t.table, t.redb_rows, t.ferro_rows
                ),
                None => "the migration did not verify".to_string(),
            };
            stay_on_redb(why)
        }
        Err(err) if is_redb_busy(&err) => stay_on_redb(
            "another process still holds the redb store, so it cannot be migrated yet".to_string(),
        ),
        // Another process migrated first.
        Err(_) if std::fs::symlink_metadata(physical).is_ok() => ferro(physical),
        Err(err) => Err(err),
    }
}

/// Open the redb store at `path` whatever the environment says. For tests and
/// for tools that must read a `.redb` file regardless of `FL_STORE_BACKEND`.
pub fn open_redb(path: &Path) -> Result<AnyStore, StoreError> {
    open_store_with(
        path,
        &StoreOptions {
            backend: Some(BackendKind::Redb),
            ..StoreOptions::default()
        },
    )
}

/// Whether the store named by the logical `path` exists under the engine that
/// [`choose_backend`] selects for it (the process environment's
/// [`BACKEND_ENV`] included): the physical file of that engine, or, under a
/// ferro selection, the lone `.redb` that [`open_store`] migrates (or, when it
/// cannot, keeps opening as redb). Opening either never creates an empty
/// store. An unusable environment value counts as "check the path itself".
pub fn store_exists(path: &Path) -> bool {
    store_exists_with(path, std::env::var(BACKEND_ENV).ok().as_deref())
}

fn store_exists_with(path: &Path, env: Option<&str>) -> bool {
    match choose_backend(path, env) {
        Ok(kind) => {
            let physical = physical_path(path, kind);
            physical.exists() || (kind == BackendKind::Ferro && path != physical && path.is_file())
        }
        Err(_) => path.exists(),
    }
}

/// The identity of the store named `path` under the process environment: the
/// physical file of the selected engine, with symlinks resolved. Two paths
/// name the same store exactly when their identities are equal, so a
/// configured `x.redb` and an explicit `x.fls` agree under FerroStore, and a
/// symlinked alias agrees with its target. A file that does not exist yet has
/// its directory resolved instead. An unusable engine selection falls back to
/// the path itself, as [`store_exists`] does.
pub fn store_identity(path: &Path) -> PathBuf {
    store_identity_with(path, std::env::var(BACKEND_ENV).ok().as_deref())
}

fn store_identity_with(path: &Path, env: Option<&str>) -> PathBuf {
    let physical = match choose_backend(path, env) {
        Ok(kind) => physical_path(path, kind),
        Err(_) => path.to_path_buf(),
    };
    if let Ok(resolved) = physical.canonicalize() {
        return resolved;
    }
    // Absent file: resolve the directory it would be created in.
    match (physical.parent(), physical.file_name()) {
        (Some(dir), Some(name)) => {
            let dir = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            };
            match dir.canonicalize() {
                Ok(resolved) => resolved.join(name),
                Err(_) => physical,
            }
        }
        _ => physical,
    }
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
        let label = store.label().to_string();
        Self {
            backend: Backend::Ferro(Box::new(store)),
            label,
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
            Backend::Ferro(s) => s.format_version(),
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
            Backend::Ferro(s) => {
                s.export_manifest(project, commit, exported_at_unix, repository_node_id)
            }
            Backend::Redb(s) => {
                s.export_manifest(project, commit, exported_at_unix, repository_node_id)
            }
        }
    }

    /// Write the manifest's project, gates and transitions, atomically.
    pub fn import_manifest(&self, m: &Manifest, root: &str) -> Result<ImportReport, ManifestError> {
        match &self.backend {
            Backend::Ferro(s) => s.import_manifest(m, root),
            Backend::Redb(s) => s.import_manifest(m, root),
        }
    }
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
    use fl_core::{
        CommandSpec, HandoffEntry, MessageKind, MessageUrgency, PopulationDelivery,
        SensitivityPatch, StatePayload, WireAddress,
    };
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
    fn store_identity_is_the_logical_path_on_redb() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let redb = root.join("a.redb");
        // Absent: the directory is resolved, the name kept.
        assert_eq!(store_identity_with(&redb, None), redb);
        assert_eq!(store_identity_with(&redb, Some("redb")), redb);
        // A sibling .fls is a different store under redb.
        assert_ne!(
            store_identity_with(&redb, None),
            store_identity_with(&root.join("a.fls"), None)
        );
        std::fs::write(&redb, b"x").unwrap();
        assert_eq!(store_identity_with(&redb, None), redb);
    }

    #[test]
    fn store_identity_is_the_physical_file_on_ferro() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let redb = root.join("a.redb");
        let fls = root.join("a.fls");
        // Both absent: the raw physical path.
        assert_eq!(store_identity_with(&redb, Some("ferro")), fls);
        assert_eq!(store_identity_with(&fls, None), fls);
        assert_eq!(
            store_identity_with(&redb, Some("ferro")),
            store_identity_with(&fls, None)
        );
        // A stale .redb beside it does not change the answer.
        std::fs::write(&redb, b"stale").unwrap();
        assert_eq!(store_identity_with(&redb, Some("ferro")), fls);
        std::fs::write(&fls, b"x").unwrap();
        assert_eq!(store_identity_with(&redb, Some("ferro")), fls);
    }

    #[cfg(unix)]
    #[test]
    fn store_identity_sees_through_symlinks() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        let alias_dir = root.join("alias");
        std::os::unix::fs::symlink(&real, &alias_dir).unwrap();
        // An absent file in an aliased directory.
        assert_eq!(
            store_identity_with(&alias_dir.join("a.redb"), Some("ferro")),
            store_identity_with(&real.join("a.fls"), None)
        );
        // An existing file reached through a file symlink.
        std::fs::write(real.join("a.fls"), b"x").unwrap();
        std::os::unix::fs::symlink(real.join("a.fls"), root.join("link.fls")).unwrap();
        assert_eq!(
            store_identity_with(&root.join("link.fls"), None),
            real.join("a.fls")
        );
        assert_eq!(
            store_identity_with(&alias_dir.join("a.redb"), Some("ferro")),
            store_identity_with(&root.join("link.fls"), None)
        );
        // An unusable engine selection falls back to the path itself.
        assert_eq!(
            store_identity_with(&real.join("b.redb"), Some("sqlite")),
            real.join("b.redb")
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
                auto_migrate: false,
                ..StoreOptions::default()
            },
        )
        .err()
        .expect("must refuse");
        assert!(err.to_string().contains("fl store migrate"), "{err}");
        assert!(
            err.to_string().contains("FL_STORE_BACKEND=redb"),
            "names the remedy: {err}"
        );
        assert!(!dir.path().join("s.fls").exists(), "nothing was created");
    }

    fn handoff(summary: &str) -> StatePayload {
        StatePayload::Handoff(HandoffEntry {
            summary: summary.into(),
            shipped: vec![],
            in_flight: vec![],
            blockers: vec![],
            next_steps: vec![],
            notes: None,
        })
    }

    /// A project, a gate, a record, a state entry and a wire message, written
    /// through the traits.
    fn populate(store: &RedbStore) -> (ProjectId, GateId, RecordId) {
        let project = store.add_project("/work/app").unwrap();
        let gate = store
            .add_gate(
                &project,
                "fmt",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 5,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "**/*.rs".into(),
                },
                1,
                "c1",
                "o",
            )
            .unwrap();
        let record = store.add_record(&project, "keep this").unwrap();
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "rule".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: handoff("v1"),
                at: At::from_unix_millis(10),
                by: "jarvis".into(),
            })
            .unwrap();
        store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to: WireAddress::AgentName("ferris".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: false,
                subject: "first".into(),
                body: "hello".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(10),
            })
            .unwrap();
        (project, gate, record)
    }

    fn assert_populated(store: &AnyStore, project: &ProjectId, gate: &GateId, record: &RecordId) {
        assert_eq!(
            store.get_project(project).unwrap().unwrap().root,
            "/work/app"
        );
        assert_eq!(store.get_gate(gate).unwrap().unwrap().name, "fmt");
        assert!(store.get_record(record).unwrap().is_some());
        let entry = store
            .get_state_by_key(&StateScope::Workspace, StateConcern::Handoff, "rule")
            .unwrap()
            .expect("the state entry");
        assert_eq!(entry.slug, "rule");
        let inbox = store
            .list_wire_inbox(&WireFilter {
                recipient: Some("ferris".into()),
                ..WireFilter::default()
            })
            .unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].message.subject, "first");
    }

    fn ferro_opts(auto_migrate: bool) -> StoreOptions {
        StoreOptions {
            backend: Some(BackendKind::Ferro),
            auto_migrate,
            retries: 0,
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn a_lone_redb_is_migrated_once_and_kept_read_only() {
        use std::os::unix::fs::MetadataExt;
        let dir = TempDir::new().unwrap();
        let redb = dir.path().join("s.redb");
        let fls = dir.path().join("s.fls");
        let (project, gate, record) = {
            let src = RedbStore::open(&redb).unwrap();
            populate(&src)
        };

        let store = open_store_with(&redb, &ferro_opts(true)).unwrap();
        assert_eq!(store.kind(), BackendKind::Ferro);
        assert_populated(&store, &project, &gate, &record);
        drop(store);
        assert_eq!(mode(&fls), 0o600);
        assert_eq!(mode(&redb), 0o400);
        // Opening a redb file rewrites its internal pages, so the bytes before
        // the migration are not comparable; what the migration leaves is.
        let kept = std::fs::read(&redb).unwrap();
        let kept_mtime = std::fs::metadata(&redb).unwrap().modified().unwrap();
        // Its content is still all there: read a writable copy of it.
        let copy = dir.path().join("copy.redb");
        std::fs::copy(&redb, &copy).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_populated(&open_redb(&copy).unwrap(), &project, &gate, &record);
        std::fs::remove_file(&copy).unwrap();
        assert!(
            std::fs::read_dir(dir.path()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("migrating")),
            "no temp file is left"
        );

        // A second open finds the .fls: it does not migrate again.
        let meta = std::fs::metadata(&fls).unwrap();
        let again = open_store_with(&redb, &ferro_opts(true)).unwrap();
        assert_eq!(again.kind(), BackendKind::Ferro);
        assert_populated(&again, &project, &gate, &record);
        drop(again);
        let after = std::fs::metadata(&fls).unwrap();
        assert_eq!(after.ino(), meta.ino());
        assert_eq!(after.mtime(), meta.mtime());
        assert_eq!(after.mtime_nsec(), meta.mtime_nsec());
        assert_eq!(std::fs::read(&redb).unwrap(), kept);
        assert_eq!(
            std::fs::metadata(&redb).unwrap().modified().unwrap(),
            kept_mtime
        );
        assert_eq!(mode(&redb), 0o400);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_redb_beside_its_fls_is_never_opened_for_write() {
        let dir = TempDir::new().unwrap();
        let redb = dir.path().join("s.redb");
        drop(RedbStore::open(&redb).unwrap());
        drop(open_store_with(&redb, &ferro_opts(true)).unwrap());
        assert_eq!(mode(&redb), 0o400);
        let before = std::fs::metadata(&redb).unwrap().modified().unwrap();
        drop(open_store_with(&redb, &ferro_opts(true)).unwrap());
        drop(open_store_with(&redb, &ferro_opts(false)).unwrap());
        assert_eq!(
            std::fs::metadata(&redb).unwrap().modified().unwrap(),
            before
        );
        assert_eq!(mode(&redb), 0o400);
    }

    #[test]
    fn a_redb_another_store_holds_stays_on_redb_and_creates_no_fls() {
        let dir = TempDir::new().unwrap();
        let redb = dir.path().join("s.redb");
        let fls = dir.path().join("s.fls");
        drop(RedbStore::open(&redb).unwrap());
        // redb takes an exclusive file lock per open, so a second open in this
        // process is refused exactly as another process's would be.
        let holder = RedbStore::open(&redb).unwrap();
        let refused = RedbStore::open_with_retries(&redb, 0).err().expect("held");
        assert!(is_redb_busy(&refused), "{refused}");
        let err = open_store_with(&redb, &ferro_opts(true))
            .err()
            .expect("the holder still has the file");
        assert!(is_redb_busy(&err), "{err}");
        assert!(!fls.exists(), "no .fls was created");
        drop(holder);

        // Held for a moment, then released: the migration finds it busy and
        // does not run; the fallback waits its retries and opens the redb.
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let path = redb.clone();
        let holder = std::thread::spawn(move || {
            let store = RedbStore::open(&path).unwrap();
            held_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(store);
        });
        held_rx.recv().unwrap();
        let store = open_store_with(
            &redb,
            &StoreOptions {
                retries: 200,
                ..ferro_opts(true)
            },
        )
        .unwrap();
        assert_eq!(store.kind(), BackendKind::Redb);
        assert!(store.label().ends_with("s.redb"), "{}", store.label());
        assert!(!fls.exists(), "no .fls was created");
        holder.join().unwrap();
        drop(store);

        // Free again: the same call migrates.
        let store = open_store_with(&redb, &ferro_opts(true)).unwrap();
        assert_eq!(store.kind(), BackendKind::Ferro);
        assert!(fls.exists());
    }

    #[test]
    fn a_migration_that_does_not_verify_stays_on_redb_and_creates_no_fls() {
        use redb::TableDefinition;
        let dir = TempDir::new().unwrap();
        let redb = dir.path().join("s.redb");
        let fls = dir.path().join("s.fls");
        let (project, _gate, _record) = {
            let src = RedbStore::open(&redb).unwrap();
            populate(&src)
        };
        // A table the copy does not know: the verifier reports it, so the
        // migration is not clean.
        {
            let db = redb::Database::create(&redb).unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut t = tx
                    .open_table(TableDefinition::<&str, &str>::new("zz_unknown"))
                    .unwrap();
                t.insert("k", "v").unwrap();
            }
            tx.commit().unwrap();
        }
        let store = open_store_with(&redb, &ferro_opts(true)).unwrap();
        assert_eq!(store.kind(), BackendKind::Redb);
        assert_eq!(
            store.get_project(&project).unwrap().unwrap().root,
            "/work/app"
        );
        assert!(!fls.exists(), "no .fls was created");
        drop(store);
        assert!(
            std::fs::read_dir(dir.path()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("migrating")),
            "no temp file is left"
        );
        #[cfg(unix)]
        assert_eq!(mode(&redb), 0o600, "the redb stays writable");
    }

    #[test]
    fn the_busy_error_is_classified_by_cause() {
        let busy = |cause: &str| StoreError::Unreachable {
            store: "s".into(),
            cause: cause.into(),
        };
        assert!(is_redb_busy(&busy(
            "Database already open. Cannot acquire lock."
        )));
        assert!(is_redb_busy(&busy("Cannot acquire lock")));
        assert!(!is_redb_busy(&busy("Permission denied")));
        assert!(!is_redb_busy(&backend("Database already open")));
    }

    #[test]
    fn a_fls_that_appears_first_is_opened_not_migrated_over() {
        let dir = TempDir::new().unwrap();
        let redb = dir.path().join("s.redb");
        let fls = dir.path().join("s.fls");
        let (project, _gate, _record) = {
            let src = RedbStore::open(&redb).unwrap();
            populate(&src)
        };
        // Another process migrated first: its .fls (here with nothing from
        // the redb in it) is the store. (The branch for an `Err` from a
        // migration that lost the race to a `.fls` needs two processes to
        // reach; this is the state it leaves.)
        drop(FerroStore::open(&fls).unwrap());
        let store = open_store_with(&redb, &ferro_opts(true)).unwrap();
        assert_eq!(store.kind(), BackendKind::Ferro);
        assert!(store.get_project(&project).is_err());
    }

    #[test]
    fn turning_auto_migration_off_refuses_and_leaves_the_redb_alone() {
        let dir = TempDir::new().unwrap();
        let redb = dir.path().join("s.redb");
        drop(RedbStore::open(&redb).unwrap());
        let before = std::fs::read(&redb).unwrap();
        let err = open_store_with(&redb, &ferro_opts(false))
            .err()
            .expect("must refuse");
        assert!(err.to_string().contains("fl store migrate"), "{err}");
        assert!(!dir.path().join("s.fls").exists());
        assert_eq!(std::fs::read(&redb).unwrap(), before);
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
    fn an_in_memory_any_store_takes_its_label_from_the_ferrostore() {
        assert_eq!(
            AnyStore::in_memory().label(),
            FerroStore::in_memory().label()
        );
    }

    #[test]
    fn set_routes_makes_the_store_hold_routing_on_both_engines() {
        for kind in both() {
            let (store, _g) = on(kind);
            let project = store.add_project("/repo").unwrap();
            assert!(!store.holds_routing().unwrap(), "{kind:?}");
            store.set_routes(&project, &RoutingMap::starting()).unwrap();
            assert!(store.holds_routing().unwrap(), "{kind:?}");
            assert_eq!(
                store.routes(&project).unwrap(),
                Some(RoutingMap::starting()),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn an_imported_project_is_refused_by_name() {
        let (author, _g) = on(BackendKind::Redb);
        let project = author.add_project("/repo").unwrap();
        let manifest = author.export_manifest(&project, "c0ffee", 1, None).unwrap();
        let (other, _g2) = on(BackendKind::Redb);
        other.import_manifest(&manifest, "/elsewhere").unwrap();
        let err = other.refuse_if_imported(&project, "x").unwrap_err();
        assert!(
            matches!(err, StoreError::Imported { action: "x", .. }),
            "{err}"
        );
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
    fn manifests_and_versions_are_served_on_both_engines() {
        for kind in both() {
            let (store, _g) = on(kind);
            assert!(store.format_version().unwrap().is_some(), "{kind:?}");
            let project = store.add_project("/repo").unwrap();
            let manifest = store.export_manifest(&project, "c0ffee", 1, None).unwrap();
            for target in both() {
                let (other, _g2) = on(target);
                let report = other.import_manifest(&manifest, "/elsewhere").unwrap();
                assert_eq!(report.project, project, "{kind:?} -> {target:?}");
                assert!(
                    other.imported_hash(&project).unwrap().is_some(),
                    "{kind:?} -> {target:?}"
                );
                assert!(
                    other.format_version().unwrap() >= Some(crate::FORMAT_WITH_IMPORTS),
                    "{kind:?} -> {target:?}"
                );
            }
        }
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

    #[test]
    fn open_redb_ignores_the_environment_and_the_path_extension_rules() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.redb");
        let store = open_redb(&path).unwrap();
        assert_eq!(store.kind(), BackendKind::Redb);
        assert!(path.exists());
        assert!(!dir.path().join("s.fls").exists());
    }

    #[test]
    fn store_exists_checks_the_chosen_engines_physical_file() {
        let dir = TempDir::new().unwrap();
        let logical = dir.path().join("s.redb");
        // Nothing yet, under either engine.
        assert!(!store_exists_with(&logical, None));
        assert!(!store_exists_with(&logical, Some("ferro")));
        // A redb file exists for redb; under a ferro selection it is the store
        // that open_store migrates, so it exists there too.
        drop(open_redb(&logical).unwrap());
        assert!(store_exists_with(&logical, None));
        assert!(store_exists_with(&logical, Some("redb")));
        assert!(store_exists_with(&logical, Some("ferro")));
        // A directory named like the redb is not a store.
        let dir_named = dir.path().join("d.redb");
        std::fs::create_dir(&dir_named).unwrap();
        assert!(store_exists_with(&dir_named, None));
        assert!(!store_exists_with(&dir_named, Some("ferro")));
        let ferro = StoreOptions {
            backend: Some(BackendKind::Ferro),
            auto_migrate: false,
            ..StoreOptions::default()
        };
        // Without auto-migration the lone .redb is refused, not created over.
        assert!(open_store_with(&logical, &ferro).is_err());
        assert!(!dir.path().join("s.fls").exists());
        let ferro = StoreOptions {
            auto_migrate: true,
            ..ferro
        };
        // A ferro store of its own: the logical `.redb` name maps to the
        // sibling .fls, and an explicit .fls path needs no environment.
        let fresh = dir.path().join("f.redb");
        assert!(!store_exists_with(&fresh, Some("ferro")));
        drop(open_store_with(&fresh, &ferro).unwrap());
        let physical = dir.path().join("f.fls");
        assert!(physical.exists());
        assert!(store_exists_with(&fresh, Some("ferro")));
        assert!(!store_exists_with(&fresh, None));
        assert!(store_exists_with(&physical, None));
        // A bad environment value falls back to the path itself.
        assert!(store_exists_with(&logical, Some("bogus")));
        assert!(!store_exists_with(
            &dir.path().join("none.redb"),
            Some("bogus")
        ));
    }
}
