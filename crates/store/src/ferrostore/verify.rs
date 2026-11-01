//! Parity between a redb store and the FerroStore it was migrated into, and the
//! verified cutover that uses it.
//!
//! **Tables.** One [`TableParity`] for each redb table `migrate.rs` copies,
//! named after it. The redb side is read here, row by row, and turned into the
//! document id and JSON value the migration is meant to store; it does not call
//! the migration's code, so a wrong mapping shows as a mismatch. A raw `&str`
//! is hashed as the JSON string the migration stores, a JSON-text `&str` as
//! the value it parses to, a `u64` as a number, a `bool` as a boolean and `()`
//! as `null`. redb's `meta.format_version` is checked as the `__meta` entry,
//! against the FerroStore schema document, and is left out of `meta`. Two more
//! entries catch what the copy list misses: `redb:<name>` for any redb table
//! the list does not name (FerroStore holds none of it), and
//! `ferrostore:unlisted` for any FerroStore document, column table or vector
//! collection outside the list (redb holds none of them).
//!
//! **Hash.** SHA-256 over the rows sorted by key, each as
//! `len(key) ‖ key ‖ len(value) ‖ canonical-JSON(value)`, lengths as `u64`
//! little-endian. Canonical JSON has sorted object keys and no whitespace.
//!
//! **Trait views.** Every list-style trait method, run on both stores with the
//! same arguments (taken from the redb side), its items hashed the same way
//! with each item's canonical JSON as its key: the comparison is of what the
//! method returns, not of its order. A failure on the FerroStore side is a
//! mismatch whose one row is the error; a failure on the redb side is `Err`.
//! Methods that take one message, thread or state id run for at most
//! [`PER_ITEM_SAMPLE`] ids, the lowest; the `*_after(0)` and unfiltered views
//! cover every item. FerroStore views run on one in-memory copy of the store's
//! decoded state, so they cost one decode, not one per call.

use super::doc::{self, SCHEMA_COLLECTION, SCHEMA_KEY};
use super::state::DocState;
use super::{FerroStore, Inner};
use crate::{RedbStore, backend, decode};
use fl_core::ferrowire::{ThreadId, WireFilter};
use fl_core::ids::{GateId, MessageId, ProjectId, StateId};
use fl_core::iri::Iri;
use fl_core::split::{LedgerCache, Outbox};
use fl_core::store::{
    AgentRegistry, Catalog, Ledger, MessageStore, StateStore, StoreError, Tracker, WorkflowStore,
};
use redb::{
    MultimapTableHandle, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
    TableHandle,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

/// The most ids a per-item view (one thread, one message, one state) runs for.
pub const PER_ITEM_SAMPLE: usize = 32;

/// One table or one trait view, on both sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableParity {
    pub table: String,
    pub redb_rows: u64,
    pub ferro_rows: u64,
    pub redb_hash: [u8; 32],
    pub ferro_hash: [u8; 32],
}

impl TableParity {
    /// Same row count and same hash.
    pub fn is_clean(&self) -> bool {
        self.redb_rows == self.ferro_rows && self.redb_hash == self.ferro_hash
    }
}

/// Every table and every trait view the verifier compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityReport {
    pub tables: Vec<TableParity>,
    pub trait_views: Vec<TableParity>,
}

impl ParityReport {
    /// No table and no view differs.
    pub fn is_clean(&self) -> bool {
        self.mismatches().is_empty()
    }

    /// The tables, then the views, that differ.
    pub fn mismatches(&self) -> Vec<&TableParity> {
        self.tables
            .iter()
            .chain(&self.trait_views)
            .filter(|parity| !parity.is_clean())
            .collect()
    }
}

/// Compare `src` with `dst` table by table and view by view. `Ok` even when
/// they differ; `Err` only when a store cannot be read or decoded.
pub fn verify_redb_vs_ferrostore(
    src: &RedbStore,
    dst: &FerroStore,
) -> Result<ParityReport, StoreError> {
    let state = dst.read_state()?;
    let tables = table_parity(src, &state)?;
    let copy = FerroStore::assemble(
        Inner::Memory(Mutex::new(state)),
        format!("{} (verify copy)", dst.label),
    );
    let trait_views = view_parity(src, &copy)?;
    Ok(ParityReport {
        tables,
        trait_views,
    })
}

/// Migrate the redb store at `redb_path` into a new FerroStore at `fls_path`,
/// verified, as one switch:
///
/// 1. migrate into the sibling `<name>.migrating-<pid>` (for `fl.fls`,
///    `fl.fls.migrating-<pid>`), created 0600;
/// 2. verify; a report that is not clean deletes that file, creates no
///    `fls_path`, and is returned as it is, so the caller keeps running on redb;
/// 3. `fsync` the file; 4. move it to `fls_path`; 5. `fsync` the directory;
/// 6. `chmod 0400` the `.redb`, which is kept.
///
/// Refuses when `fls_path` already exists or `redb_path` does not. The move is
/// `link` then `unlink`, not `rename`: a `rename` would replace an `fls_path`
/// another process created after the check, and `link` refuses to.
pub fn migrate_verified(redb_path: &Path, fls_path: &Path) -> Result<ParityReport, StoreError> {
    migrate_verified_with(redb_path, fls_path, |_| Ok(()))
}

/// [`migrate_verified`] with `after_copy` run on the new store between the
/// copy and the verify. Production passes a no-op; tests use it to make the
/// two stores differ.
pub(super) fn migrate_verified_with(
    redb_path: &Path,
    fls_path: &Path,
    after_copy: impl FnOnce(&FerroStore) -> Result<(), StoreError>,
) -> Result<ParityReport, StoreError> {
    if std::fs::symlink_metadata(fls_path).is_ok() {
        return Err(backend(format!(
            "{} already exists: a verified migration only creates a new store",
            fls_path.display()
        )));
    }
    if !redb_path.is_file() {
        return Err(backend(format!(
            "{} is not a redb store file: nothing to migrate",
            redb_path.display()
        )));
    }
    let name = fls_path
        .file_name()
        .ok_or_else(|| backend(format!("{} has no file name", fls_path.display())))?;
    let mut temp_name = name.to_os_string();
    temp_name.push(format!(".migrating-{}", std::process::id()));
    let temp = fls_path.with_file_name(temp_name);
    // A file under this process's id is left by a dead process of the same id.
    match std::fs::remove_file(&temp) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(backend(format!("remove {}: {err}", temp.display()))),
    }

    let outcome = (|| {
        let src = RedbStore::open(redb_path)?;
        let report = {
            let dst = FerroStore::open(&temp)?;
            super::migrate_redb_to_ferrostore(&src, &dst)?;
            after_copy(&dst)?;
            verify_redb_vs_ferrostore(&src, &dst)?
        };
        drop(src);
        Ok(report)
    })();
    let report = match outcome {
        Ok(report) if report.is_clean() => report,
        Ok(report) => {
            remove_temp(&temp)?;
            return Ok(report);
        }
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            return Err(err);
        }
    };

    let publish = (|| {
        std::fs::File::open(&temp)
            .and_then(|file| file.sync_all())
            .map_err(|err| backend(format!("sync {}: {err}", temp.display())))?;
        std::fs::hard_link(&temp, fls_path).map_err(|err| {
            backend(format!(
                "move {} to {}: {err}",
                temp.display(),
                fls_path.display()
            ))
        })?;
        Ok::<(), StoreError>(())
    })();
    if let Err(err) = publish {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    remove_temp(&temp)?;
    super::sync_dir(&super::parent_dir(fls_path))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(redb_path, std::fs::Permissions::from_mode(0o400))
            .map_err(|err| backend(format!("chmod 0400 {}: {err}", redb_path.display())))?;
    }
    Ok(report)
}

fn remove_temp(temp: &Path) -> Result<(), StoreError> {
    std::fs::remove_file(temp).map_err(|err| backend(format!("remove {}: {err}", temp.display())))
}

// ---- hashing ----------------------------------------------------------------

/// Rows of one side: `(key, value)`.
type Rows = Vec<(String, Value)>;

fn canonical(value: &Value, out: &mut Vec<u8>) -> Result<(), StoreError> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key).map_err(backend)?;
                out.push(b':');
                canonical(&map[key], out)?;
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonical(item, out)?;
            }
            out.push(b']');
        }
        scalar => serde_json::to_writer(&mut *out, scalar).map_err(backend)?,
    }
    Ok(())
}

fn canonical_bytes(value: &Value) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::new();
    canonical(value, &mut out)?;
    Ok(out)
}

/// Row count and hash of `rows`, sorted by key (then value, for equal keys).
fn digest(rows: Rows) -> Result<(u64, [u8; 32]), StoreError> {
    let mut encoded = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        encoded.push((key.into_bytes(), canonical_bytes(&value)?));
    }
    encoded.sort();
    let mut hasher = Sha256::new();
    for (key, value) in &encoded {
        hasher.update((key.len() as u64).to_le_bytes());
        hasher.update(key);
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value);
    }
    let count = u64::try_from(encoded.len()).map_err(|_| backend("row count exceeds u64"))?;
    Ok((count, hasher.finalize().into()))
}

fn parity(table: impl Into<String>, redb: Rows, ferro: Rows) -> Result<TableParity, StoreError> {
    let (redb_rows, redb_hash) = digest(redb)?;
    let (ferro_rows, ferro_hash) = digest(ferro)?;
    Ok(TableParity {
        table: table.into(),
        redb_rows,
        ferro_rows,
        redb_hash,
        ferro_hash,
    })
}

// ---- tables -----------------------------------------------------------------

const SEP: char = '\u{1f}';

fn pair(a: &str, b: &str) -> String {
    format!("{a}{SEP}{b}")
}

fn raw(text: &str) -> Value {
    Value::String(text.to_string())
}

fn json_text(table: &str, key: &str, text: &str) -> Result<Value, StoreError> {
    serde_json::from_str(text).map_err(|err| {
        decode(format!(
            "redb table `{table}` key `{key}` is not JSON: {err}"
        ))
    })
}

/// Every row of one redb table, mapped by `row`. A table redb never created
/// has no rows.
macro_rules! redb_rows {
    ($tx:expr, $name:expr, $k:ty, $v:ty, |$key:ident, $value:ident| $row:expr) => {{
        let mut rows: Rows = Vec::new();
        match $tx.open_table(TableDefinition::<$k, $v>::new($name)) {
            Ok(table) => {
                for entry in table.iter().map_err(backend)? {
                    let (key_guard, value_guard) = entry.map_err(backend)?;
                    let $key = key_guard.value();
                    let $value = value_guard.value();
                    let row: Result<(String, Value), StoreError> = $row;
                    rows.push(row?);
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(backend(err)),
        }
        rows
    }};
}

/// `name`'s rows as the migration should have stored them. `None` for a name
/// the migration does not copy.
fn redb_table(tx: &redb::ReadTransaction, name: &str) -> Result<Option<Rows>, StoreError> {
    let rows = match name {
        doc::META => {
            let mut rows = redb_rows!(tx, name, &str, u64, |k, v| Ok((
                k.to_string(),
                Value::from(v)
            )));
            rows.retain(|(key, _)| key != doc::FORMAT_KEY);
            rows
        }
        doc::IDS
        | doc::ALIASES
        | doc::IMPORTS
        | doc::GITHUB_BINDINGS
        | doc::LEDGER_CUTOVERS
        | doc::LEDGER_ROOTS
        | doc::LEDGER_HEADS
        | doc::AGENT_BY_NAME => {
            redb_rows!(tx, name, &str, &str, |k, v| Ok((k.to_string(), raw(v))))
        }
        doc::PROJECTS
        | doc::GATES
        | doc::RECORDS
        | doc::FINDINGS
        | doc::ROUTING
        | doc::ESCALATING
        | doc::TOMBSTONES
        | doc::STATE_ENTRIES
        | doc::STATE_CONFLICTS
        | doc::STATE_SYNC_CURSORS
        | doc::AGENTS
        | doc::AREAS
        | doc::WIRE_MESSAGES
        | doc::WORKFLOW_DEFS
        | doc::WORKFLOW_RUNS => {
            redb_rows!(tx, name, &str, &str, |k, v| Ok((
                k.to_string(),
                json_text(name, k, v)?
            )))
        }
        doc::HANDLE_OF
        | doc::CANDIDATE_RUNS
        | doc::CANDIDATE_ATTEMPTS
        | doc::STATE_REVISION_IDS
        | doc::AGENT_EVENT_IDS
        | doc::WIRE_MSG_SEQ => {
            redb_rows!(tx, name, &str, u64, |k, v| Ok((
                k.to_string(),
                Value::from(v)
            )))
        }
        doc::HANDLES => redb_rows!(tx, name, (&str, u64), &str, |k, v| Ok((
            format!("{}{SEP}{}", k.0, k.1),
            raw(v)
        ))),
        doc::TRANSITIONS | doc::LEDGER_SEGMENTS | doc::WIRE_INBOX => {
            redb_rows!(tx, name, (&str, &str), &str, |k, v| {
                let key = pair(k.0, k.1);
                let value = json_text(name, &key, v)?;
                Ok((key, value))
            })
        }
        doc::WORKFLOW_SLUGS => redb_rows!(tx, name, (&str, &str), &str, |k, v| Ok((
            pair(k.0, k.1),
            raw(v)
        ))),
        doc::LEDGER_PUBLISHED => redb_rows!(tx, name, (&str, &str), bool, |k, v| Ok((
            pair(k.0, k.1),
            Value::Bool(v)
        ))),
        doc::WIRE_THREADS | doc::WIRE_REPLIES => {
            redb_rows!(tx, name, (&str, &str), (), |k, _v| Ok((
                pair(k.0, k.1),
                Value::Null
            )))
        }
        doc::STATE_KEYS => redb_rows!(tx, name, (&str, &str, &str), &str, |k, v| Ok((
            format!("{}{SEP}{}{SEP}{}", k.0, k.1, k.2),
            raw(v)
        ))),
        doc::GATE_RUNS
        | doc::ATTEMPTS
        | doc::STATE_REVISIONS
        | doc::AGENT_EVENTS
        | doc::WIRE_RCPT_LOG => redb_rows!(tx, name, u64, &str, |k, v| {
            let key = k.to_string();
            let value = json_text(name, &key, v)?;
            Ok((key, value))
        }),
        doc::WIRE_MSG_LOG => {
            redb_rows!(tx, name, u64, &str, |k, v| Ok((k.to_string(), raw(v))))
        }
        _ => return Ok(None),
    };
    Ok(Some(rows))
}

/// Every redb table `migrate.rs` copies, in its order.
const COPIED: [&str; 45] = [
    doc::META,
    doc::IDS,
    doc::ALIASES,
    doc::HANDLES,
    doc::HANDLE_OF,
    doc::PROJECTS,
    doc::GATES,
    doc::TRANSITIONS,
    doc::RECORDS,
    doc::FINDINGS,
    doc::GATE_RUNS,
    doc::ATTEMPTS,
    doc::IMPORTS,
    doc::GITHUB_BINDINGS,
    doc::LEDGER_PUBLISHED,
    doc::LEDGER_CUTOVERS,
    doc::CANDIDATE_RUNS,
    doc::CANDIDATE_ATTEMPTS,
    doc::LEDGER_ROOTS,
    doc::LEDGER_HEADS,
    doc::LEDGER_SEGMENTS,
    doc::ROUTING,
    doc::ESCALATING,
    doc::TOMBSTONES,
    doc::STATE_ENTRIES,
    doc::STATE_KEYS,
    doc::STATE_REVISIONS,
    doc::STATE_REVISION_IDS,
    doc::STATE_CONFLICTS,
    doc::STATE_SYNC_CURSORS,
    doc::AGENTS,
    doc::AGENT_BY_NAME,
    doc::AREAS,
    doc::AGENT_EVENTS,
    doc::AGENT_EVENT_IDS,
    doc::WIRE_MESSAGES,
    doc::WIRE_MSG_LOG,
    doc::WIRE_MSG_SEQ,
    doc::WIRE_INBOX,
    doc::WIRE_RCPT_LOG,
    doc::WIRE_THREADS,
    doc::WIRE_REPLIES,
    doc::WORKFLOW_DEFS,
    doc::WORKFLOW_SLUGS,
    doc::WORKFLOW_RUNS,
];

fn ferro_collection(state: &DocState, collection: &str) -> Rows {
    let start = (collection.to_string(), String::new());
    state
        .docs
        .range(start..)
        .take_while(|((held, _), _)| held == collection)
        .map(|((_, id), doc)| (id.clone(), doc.clone()))
        .collect()
}

fn table_parity(src: &RedbStore, state: &DocState) -> Result<Vec<TableParity>, StoreError> {
    let tx = src.db.begin_read().map_err(backend)?;
    let mut out = Vec::with_capacity(COPIED.len() + 2);
    for name in COPIED {
        let redb = redb_table(&tx, name)?
            .ok_or_else(|| backend(format!("the verifier has no mapping for `{name}`")))?;
        out.push(parity(name, redb, ferro_collection(state, name))?);
    }

    // The schema version: redb's `meta.format_version`, FerroStore's `__meta`.
    let mut schema = Vec::new();
    if let Ok(meta) = tx.open_table(TableDefinition::<&str, u64>::new(doc::META))
        && let Some(version) = meta.get(doc::FORMAT_KEY).map_err(backend)?
    {
        schema.push((SCHEMA_KEY.to_string(), Value::from(version.value())));
    }
    out.push(parity(
        SCHEMA_COLLECTION,
        schema,
        ferro_collection(state, SCHEMA_COLLECTION),
    )?);

    // A redb table the copy list does not name: none of it reached FerroStore.
    let listed: BTreeSet<&str> = COPIED.iter().copied().collect();
    for handle in tx.list_tables().map_err(backend)? {
        let name = handle.name().to_string();
        if listed.contains(name.as_str()) {
            continue;
        }
        let table = tx.open_untyped_table(handle).map_err(backend)?;
        let len = table.len().map_err(backend)?;
        let (_, empty) = digest(Vec::new())?;
        out.push(TableParity {
            table: format!("redb:{name}"),
            redb_rows: len,
            ferro_rows: 0,
            redb_hash: empty,
            ferro_hash: empty,
        });
    }
    for handle in tx.list_multimap_tables().map_err(backend)? {
        let name = handle.name().to_string();
        let table = tx.open_untyped_multimap_table(handle).map_err(backend)?;
        let len = table.len().map_err(backend)?;
        let (_, empty) = digest(Vec::new())?;
        out.push(TableParity {
            table: format!("redb:{name}"),
            redb_rows: len,
            ferro_rows: 0,
            redb_hash: empty,
            ferro_hash: empty,
        });
    }

    // FerroStore data outside the copy list: nothing in redb produced it.
    let mut unlisted: Rows = state
        .docs
        .iter()
        .filter(|((collection, _), _)| {
            collection != SCHEMA_COLLECTION && !listed.contains(collection.as_str())
        })
        .map(|((collection, id), doc)| (pair(collection, id), doc.clone()))
        .collect();
    unlisted.extend(
        state
            .tables
            .keys()
            .map(|name| (pair("column table", name), Value::Null)),
    );
    unlisted.extend(
        state
            .vectors
            .keys()
            .map(|name| (pair("vector collection", name), Value::Null)),
    );
    out.push(parity("ferrostore:unlisted", Vec::new(), unlisted)?);
    Ok(out)
}

// ---- trait views ------------------------------------------------------------

/// Every store trait a view calls.
trait Viewed:
    Catalog
    + Tracker
    + Ledger
    + Outbox
    + LedgerCache
    + AgentRegistry
    + StateStore
    + MessageStore
    + WorkflowStore
{
}

impl<T> Viewed for T where
    T: Catalog
        + Tracker
        + Ledger
        + Outbox
        + LedgerCache
        + AgentRegistry
        + StateStore
        + MessageStore
        + WorkflowStore
{
}

/// The arguments the views run with, all read from the redb side.
struct Plan {
    projects: Vec<ProjectId>,
    gates: Vec<GateId>,
    /// `(repository node id, cut-over)` for every repository with one.
    cutovers: Vec<(String, Iri)>,
    /// `(repository node id, top directory)` of every cached ledger segment.
    segment_dirs: Vec<(String, String)>,
    threads: Vec<ThreadId>,
    messages: Vec<MessageId>,
    states: Vec<StateId>,
}

fn items<T: Serialize>(list: Result<Vec<T>, StoreError>) -> Result<Rows, StoreError> {
    let mut rows = Vec::new();
    for item in list? {
        let value = serde_json::to_value(item).map_err(backend)?;
        let key = String::from_utf8(canonical_bytes(&value)?).map_err(backend)?;
        rows.push((key, value));
    }
    Ok(rows)
}

/// `(view name, rows)` for every view `plan` names, on `store`.
fn run_views<S: Viewed>(store: &S, plan: &Plan) -> Vec<(String, Result<Rows, StoreError>)> {
    let mut out: Vec<(String, Result<Rows, StoreError>)> = Vec::new();
    out.push((
        "Catalog::list_projects".into(),
        items(store.list_projects()),
    ));
    for p in &plan.projects {
        out.push((
            format!("Catalog::list_gates({p})"),
            items(store.list_gates(p)),
        ));
        out.push((
            format!("Catalog::list_transitions({p})"),
            items(store.list_transitions(p)),
        ));
        out.push((
            format!("Tracker::list_records({p})"),
            items(store.list_records(p)),
        ));
        out.push((
            format!("Tracker::list_findings({p})"),
            items(store.list_findings(p)),
        ));
        out.push((format!("Ledger::attempts({p})"), items(store.attempts(p))));
        out.push((
            format!("WorkflowStore::list_workflow_defs({p})"),
            items(store.list_workflow_defs(Some(p))),
        ));
        out.push((
            format!("WorkflowStore::list_workflow_runs({p})"),
            items(store.list_workflow_runs(Some(p), None)),
        ));
    }
    for g in &plan.gates {
        out.push((format!("Ledger::gate_runs({g})"), items(store.gate_runs(g))));
    }
    for (repo, after) in &plan.cutovers {
        let pending = store.unpublished(repo, after);
        let (runs, attempts) = match pending {
            Ok(pending) => (Ok(pending.runs), Ok(pending.attempts)),
            Err(err) => {
                let text = err.to_string();
                (Err(err), Err(backend(text)))
            }
        };
        out.push((
            format!("Outbox::unpublished({repo}, {after}).runs"),
            items(runs),
        ));
        out.push((
            format!("Outbox::unpublished({repo}, {after}).attempts"),
            items(attempts),
        ));
    }
    for (repo, dir) in &plan.segment_dirs {
        out.push((
            format!("LedgerCache::cached_under({repo}, {dir})"),
            items(store.cached_under(repo, dir)),
        ));
    }
    out.push((
        "AgentRegistry::list_areas".into(),
        items(store.list_areas()),
    ));
    out.push((
        "AgentRegistry::list_agents(all)".into(),
        items(store.list_agents(None, true)),
    ));
    out.push((
        "AgentRegistry::registry_events_after(0)".into(),
        items(store.registry_events_after(0)),
    ));
    out.push((
        "StateStore::list_state(all)".into(),
        items(store.list_state(None, None)),
    ));
    out.push((
        "StateStore::list_conflicts".into(),
        items(store.list_conflicts()),
    ));
    out.push((
        "StateStore::state_revisions_after(0)".into(),
        items(store.state_revisions_after(0)),
    ));
    for id in &plan.states {
        out.push((
            format!("StateStore::state_revisions({id})"),
            items(store.state_revisions(id)),
        ));
    }
    out.push((
        "MessageStore::list_wire_inbox(all)".into(),
        items(store.list_wire_inbox(&WireFilter::default())),
    ));
    out.push((
        "MessageStore::wire_messages_after(0)".into(),
        items(store.wire_messages_after(0)),
    ));
    out.push((
        "MessageStore::wire_receipts_after(0)".into(),
        items(store.wire_receipts_after(0)),
    ));
    for thread in &plan.threads {
        out.push((
            format!("MessageStore::list_wire_thread({thread})"),
            items(store.list_wire_thread(thread)),
        ));
    }
    for id in &plan.messages {
        out.push((
            format!("MessageStore::list_wire_replies({id})"),
            items(store.list_wire_replies(id)),
        ));
        out.push((
            format!("MessageStore::list_wire_receipts({id})"),
            items(store.list_wire_receipts(id)),
        ));
    }
    out.push((
        "WorkflowStore::list_workflow_defs(all)".into(),
        items(store.list_workflow_defs(None)),
    ));
    out.push((
        "WorkflowStore::list_workflow_runs(all)".into(),
        items(store.list_workflow_runs(None, None)),
    ));
    out
}

fn plan_views(src: &RedbStore) -> Result<Plan, StoreError> {
    let projects: Vec<ProjectId> = src.list_projects()?.into_iter().map(|p| p.id).collect();
    let mut gates = Vec::new();
    for p in &projects {
        gates.extend(src.list_gates(p)?.into_iter().map(|g| g.id));
    }
    let tx = src.db.begin_read().map_err(backend)?;
    let mut cutovers = Vec::new();
    for (repo, _) in redb_table(&tx, doc::LEDGER_CUTOVERS)?.unwrap_or_default() {
        if let Some(after) = src.cutover(&repo)? {
            cutovers.push((repo, after));
        }
    }
    let mut dirs = BTreeSet::new();
    for (key, _) in redb_table(&tx, doc::LEDGER_SEGMENTS)?.unwrap_or_default() {
        if let Some((repo, path)) = key.split_once(SEP)
            && let Some((top, _)) = path.split_once('/')
        {
            dirs.insert((repo.to_string(), top.to_string()));
        }
    }
    drop(tx);
    let messages = src.wire_messages_after(0)?;
    let threads: BTreeSet<ThreadId> = messages
        .iter()
        .map(|(_, message)| message.thread_id.clone())
        .collect();
    let mut message_ids: Vec<MessageId> = messages.into_iter().map(|(_, m)| m.id).collect();
    message_ids.sort();
    message_ids.truncate(PER_ITEM_SAMPLE);
    let mut states: Vec<StateId> = src
        .list_state(None, None)?
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    states.sort();
    states.truncate(PER_ITEM_SAMPLE);
    Ok(Plan {
        projects,
        gates,
        cutovers,
        segment_dirs: dirs.into_iter().collect(),
        threads: threads.into_iter().take(PER_ITEM_SAMPLE).collect(),
        messages: message_ids,
        states,
    })
}

fn view_parity(src: &RedbStore, dst: &FerroStore) -> Result<Vec<TableParity>, StoreError> {
    let plan = plan_views(src)?;
    let redb = run_views(src, &plan);
    let ferro: BTreeMap<String, Result<Rows, StoreError>> =
        run_views(dst, &plan).into_iter().collect();
    let mut ferro = ferro;
    let mut out = Vec::with_capacity(redb.len());
    for (name, redb_rows) in redb {
        let redb_rows = redb_rows?;
        let ferro_rows = match ferro.remove(&name) {
            Some(Ok(rows)) => rows,
            Some(Err(err)) => vec![(format!("error: {err}"), Value::Null)],
            None => vec![("error: the view did not run".to_string(), Value::Null)],
        };
        out.push(parity(name, redb_rows, ferro_rows)?);
    }
    Ok(out)
}
