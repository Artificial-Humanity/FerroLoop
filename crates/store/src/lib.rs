//! redb persistence for the `fl-core` store roles.

use fl_core::finding::{Finding, FindingState};
use fl_core::ids::{FindingId, GateId, Kind, ProjectId, RecordId};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use fl_core::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};
use fl_core::store::{Catalog, Handles, Ledger, StoreError, Tracker};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;

pub mod manifest;

use crate::manifest::{Manifest, ManifestError};

/// Format 2: ids are IRIs, with an ownership index and per-kind handles.
/// Format 1 keyed every table by a `u64` id from one shared counter.
pub const FORMAT_VERSION: u64 = 2;
const FORMAT_KEY: &str = "format_version";

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// Every id this store ever minted → its kind's wire name, OR the literal
/// `"alias"` for an id minted by `add_alias`. `"alias"` is not a `Kind`: it
/// is an index marker, so `check` must handle it before `Kind::from_wire`.
const IDS: TableDefinition<&str, &str> = TableDefinition::new("ids");
/// alias → primary, for every id `add_alias` minted. Never chains: `primary`
/// is always resolved to a true primary before it is written here.
const ALIASES: TableDefinition<&str, &str> = TableDefinition::new("aliases");
const HANDLES: TableDefinition<(&str, u64), &str> = TableDefinition::new("handles");
const HANDLE_OF: TableDefinition<&str, u64> = TableDefinition::new("handle_of");
const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");
const GATES: TableDefinition<&str, &str> = TableDefinition::new("gates");
const TRANSITIONS: TableDefinition<(&str, &str), &str> = TableDefinition::new("transitions");
const RECORDS: TableDefinition<&str, &str> = TableDefinition::new("records");
const FINDINGS: TableDefinition<&str, &str> = TableDefinition::new("findings");
/// Ledger rows keep internal sequence keys. They are not ids and never leave
/// the store.
const GATE_RUNS: TableDefinition<u64, &str> = TableDefinition::new("gate_runs");
const ATTEMPTS: TableDefinition<u64, &str> = TableDefinition::new("attempts");
/// project → the `content_sha256` of the manifest it was imported from.
/// Created by the first import; a store without it has imported nothing
/// (GitHub tracker spec §4.2).
const IMPORTS: TableDefinition<&str, &str> = TableDefinition::new("imports");
/// configured `owner/repo` (lowercase) → the repository's `node_id`.
/// Additive like `imports`: created by the first bind, and a store without
/// it has bound nothing. An older fl ignores it, and cannot use GitHub mode.
const GITHUB_BINDINGS: TableDefinition<&str, &str> = TableDefinition::new("github_bindings");

/// ⚠ The format of a store that holds an import. The first import raises
/// the store from 2 to 3 in the same transaction, so an older fl — which
/// knows nothing of imports and would let a person edit an imported gate —
/// REFUSES the store with `FormatVersion` instead of ignoring the mark.
/// This build opens both. A store that never imports stays 2 and still
/// opens in older builds.
pub const FORMAT_WITH_IMPORTS: u64 = 3;

const NEXT_RUN: &str = "next_run";
const NEXT_ATTEMPT: &str = "next_attempt";

pub struct RedbStore {
    db: Database,
    label: String,
}

/// What an import did, for the person who ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub project: ProjectId,
    pub gates_added: usize,
    pub gates_changed: usize,
    pub gates_unchanged: usize,
    pub transitions: usize,
    /// Names of transitions this store held that the manifest no longer
    /// lists, and that the import therefore removed.
    pub transitions_removed: Vec<String>,
    /// `(old, new)` when a re-import came from another checkout. The
    /// project's gates now run over the new root, and the CLI says so.
    pub root_moved: Option<(String, String)>,
}

fn backend(e: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// Reading a stored value back into its type. Separate from [`backend`]
/// because the remedy is different: a decode failure is a version mismatch,
/// not a broken database.
fn decode(e: impl std::fmt::Display) -> StoreError {
    StoreError::Decode(e.to_string())
}

/// One alias hop, given already-open `IDS` and `ALIASES` tables. Shared by
/// [`RedbStore::locate`] (inside a read transaction) and `add_alias` (inside
/// a write transaction) — both `Table` and `ReadOnlyTable` implement
/// `ReadableTable`, so the same function serves either. `id` is already
/// known to name an alias (its own `IDS` entry reads `"alias"`); this
/// returns the primary it names and that primary's own kind, as a wire
/// string.
///
/// A damaged store — an alias with no `ALIASES` row, or one naming a primary
/// `IDS` no longer holds — is `Decode`, never a panic: on-disk corruption is
/// not a bug this build can rule out by construction, so it must be reported
/// the same way any other unreadable stored value is.
fn alias_primary(
    id: &Iri,
    ids: &impl ReadableTable<&'static str, &'static str>,
    aliases: &impl ReadableTable<&'static str, &'static str>,
) -> Result<(Iri, String), StoreError> {
    let Some(p) = aliases.get(id.as_str()).map_err(backend)? else {
        return Err(decode(format!("alias `{id}` has no primary recorded")));
    };
    let primary = Iri::parse(p.value()).map_err(decode)?;
    let Some(pk) = ids.get(primary.as_str()).map_err(backend)? else {
        return Err(decode(format!(
            "alias `{id}` names {primary}, which this store does not hold"
        )));
    };
    Ok((primary, pk.value().to_string()))
}

/// Index `id` as a new item of `kind` and give it the next handle, inside a
/// write transaction the caller already holds. Refuses an id already
/// indexed: an insert never overwrites. Shared by `insert_new_with_id` and
/// `import_manifest`, so a minted id and an imported id are indexed the same
/// way.
fn index_new(tx: &redb::WriteTransaction, id: &Iri, kind: Kind) -> Result<(), StoreError> {
    let mut ids = tx.open_table(IDS).map_err(backend)?;
    if ids.get(id.as_str()).map_err(backend)?.is_some() {
        return Err(StoreError::AlreadyExists(id.clone()));
    }
    ids.insert(id.as_str(), kind.as_wire()).map_err(backend)?;

    let key = format!("next_handle:{}", kind.as_wire());
    let mut meta = tx.open_table(META).map_err(backend)?;
    let n = meta
        .get(key.as_str())
        .map_err(backend)?
        .map(|v| v.value())
        .unwrap_or(0)
        + 1;
    meta.insert(key.as_str(), n).map_err(backend)?;

    tx.open_table(HANDLES)
        .map_err(backend)?
        .insert((kind.as_wire(), n), id.as_str())
        .map_err(backend)?;
    tx.open_table(HANDLE_OF)
        .map_err(backend)?
        .insert(id.as_str(), n)
        .map_err(backend)?;
    Ok(())
}

impl RedbStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let label = path.display().to_string();
        let db = Database::create(path).map_err(|e| StoreError::Unreachable {
            store: label.clone(),
            cause: e.to_string(),
        })?;

        // ⚠ Read before writing. A missing version key must not read as
        // "fresh": that would make an old store look empty — a vacuous pass.
        let found = {
            let tx = db.begin_read().map_err(backend)?;
            match tx.open_table(META) {
                Ok(meta) => Some(meta.get(FORMAT_KEY).map_err(backend)?.map(|v| v.value())),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(backend(e)),
            }
        };
        match found {
            // No META table at all: a brand-new file.
            None => Self::create_tables(&db)?,
            Some(Some(v)) if v == FORMAT_VERSION || v == FORMAT_WITH_IMPORTS => {}
            Some(v) => {
                return Err(StoreError::FormatVersion {
                    found: v,
                    expected: FORMAT_VERSION,
                });
            }
        }
        Ok(Self { db, label })
    }

    fn create_tables(db: &Database) -> Result<(), StoreError> {
        let tx = db.begin_write().map_err(backend)?;
        {
            let mut meta = tx.open_table(META).map_err(backend)?;
            meta.insert(FORMAT_KEY, FORMAT_VERSION).map_err(backend)?;
        }
        {
            tx.open_table(IDS).map_err(backend)?;
            tx.open_table(ALIASES).map_err(backend)?;
            tx.open_table(HANDLES).map_err(backend)?;
            tx.open_table(HANDLE_OF).map_err(backend)?;
            tx.open_table(PROJECTS).map_err(backend)?;
            tx.open_table(GATES).map_err(backend)?;
            tx.open_table(TRANSITIONS).map_err(backend)?;
            tx.open_table(RECORDS).map_err(backend)?;
            tx.open_table(FINDINGS).map_err(backend)?;
            tx.open_table(GATE_RUNS).map_err(backend)?;
            tx.open_table(ATTEMPTS).map_err(backend)?;
        }
        tx.commit().map_err(backend)
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    fn insert_new<T: serde::Serialize>(
        &self,
        kind: Kind,
        table: TableDefinition<&str, &str>,
        build: impl FnOnce(Iri) -> T,
    ) -> Result<Iri, StoreError> {
        let id = Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7())).map_err(backend)?;
        self.insert_new_with_id(id, kind, table, build)
    }

    /// Mint, index, hand out a handle, and write the row — in ONE write
    /// transaction, so they land together or not at all (the lesson of the old
    /// `bump_and_put`). `pub(crate)` so a test can force a collision.
    ///
    /// That lesson: an earlier version of this store bumped its id counter in
    /// its own transaction (committing immediately) and then wrote the row in
    /// a second one. If the row write failed in between, the counter's commit
    /// had already landed — an id burned with no row behind it, a silent side
    /// effect riding along on a call that reported failure. Here an error
    /// anywhere before `commit()` drops the transaction (`redb`'s
    /// `WriteTransaction` aborts on `Drop` when it was not completed), which
    /// discards the `IDS` entry and the handle bump along with everything
    /// else.
    pub(crate) fn insert_new_with_id<T: serde::Serialize>(
        &self,
        id: Iri,
        kind: Kind,
        table: TableDefinition<&str, &str>,
        build: impl FnOnce(Iri) -> T,
    ) -> Result<Iri, StoreError> {
        let json = serde_json::to_string(&build(id.clone())).map_err(backend)?;
        let tx = self.db.begin_write().map_err(backend)?;
        index_new(&tx, &id, kind)?;
        tx.open_table(table)
            .map_err(backend)?
            .insert(id.as_str(), json.as_str())
            .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(id)
    }

    /// The primary id and kind behind `id`, following one alias hop if `id`
    /// names an alias rather than a primary. `"alias"` is an index marker in
    /// `IDS`, not a `Kind`, so it is handled here before `Kind::from_wire`
    /// ever sees it.
    ///
    /// ⚠ Every method that takes an id asks this (via [`Self::check`] or
    /// directly) first — list methods too. A list over a project this store
    /// never held is "didn't look", and an empty list would say "looked,
    /// found nothing".
    fn locate(&self, id: &Iri) -> Result<(Iri, Kind), StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let ids = tx.open_table(IDS).map_err(backend)?;
        let Some(v) = ids.get(id.as_str()).map_err(backend)? else {
            return Err(StoreError::NotOwned {
                id: id.clone(),
                searched: vec![self.label.clone()],
            });
        };
        if v.value() == "alias" {
            let aliases = tx.open_table(ALIASES).map_err(backend)?;
            let (primary, wire) = alias_primary(id, &ids, &aliases)?;
            let kind = Kind::from_wire(&wire)
                .ok_or_else(|| decode(format!("unknown kind `{wire}` for {primary}")))?;
            return Ok((primary, kind));
        }
        let kind = Kind::from_wire(v.value())
            .ok_or_else(|| decode(format!("unknown kind `{}` for {id}", v.value())))?;
        Ok((id.clone(), kind))
    }

    /// The kind this store holds `id` under, or `NotOwned` naming this store.
    /// An alias is owned too: this follows it to its primary's kind.
    fn check(&self, id: &Iri) -> Result<Kind, StoreError> {
        self.locate(id).map(|(_, kind)| kind)
    }

    /// `check`, and then refuse an id held under any kind but `expected`
    /// with `WrongKind`. Every method that takes a project (and
    /// `add_finding`'s record) asks this rather than `check`: answering an
    /// empty list for a gate's IRI would say "looked, found nothing" about
    /// an item that was never a project.
    fn check_kind(&self, id: &Iri, expected: Kind) -> Result<(), StoreError> {
        let found = self.check(id)?;
        if found != expected {
            return Err(StoreError::WrongKind {
                id: id.clone(),
                expected,
                found,
            });
        }
        Ok(())
    }

    /// Whether this store holds `id`. An I/O failure is an error, never
    /// `false`: "could not look" is not "not mine".
    pub fn owns(&self, id: &Iri) -> Result<bool, StoreError> {
        match self.check(id) {
            Ok(_) => Ok(true),
            Err(StoreError::NotOwned { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Log rows are keyed by an internal sequence, not by an id: they are
    /// never addressed from outside the store.
    fn append_json<T: serde::Serialize>(
        &self,
        counter: &str,
        table: TableDefinition<u64, &str>,
        value: &T,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(value).map_err(backend)?;
        let tx = self.db.begin_write().map_err(backend)?;
        {
            let mut meta = tx.open_table(META).map_err(backend)?;
            let seq = meta
                .get(counter)
                .map_err(backend)?
                .map(|v| v.value())
                .unwrap_or(0)
                + 1;
            meta.insert(counter, seq).map_err(backend)?;
            tx.open_table(table)
                .map_err(backend)?
                .insert(seq, json.as_str())
                .map_err(backend)?;
        }
        tx.commit().map_err(backend)
    }

    fn put_json<T: serde::Serialize>(
        &self,
        table: TableDefinition<&str, &str>,
        key: &Iri,
        value: &T,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(value).map_err(backend)?;
        let tx = self.db.begin_write().map_err(backend)?;
        {
            let mut t = tx.open_table(table).map_err(backend)?;
            t.insert(key.as_str(), json.as_str()).map_err(backend)?;
        }
        tx.commit().map_err(backend)?;
        Ok(())
    }

    /// Checks ownership first, so an id this store never held is `NotOwned`,
    /// and an id it holds under another kind is `Ok(None)`. `key` may be an
    /// alias: `locate` resolves it to the primary the row is stored under,
    /// which is what this reads — a lookup by alias answers as the primary
    /// would (spec §2.5).
    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        table: TableDefinition<&str, &str>,
        key: &Iri,
    ) -> Result<Option<T>, StoreError> {
        let (target, _kind) = self.locate(key)?;
        let tx = self.db.begin_read().map_err(backend)?;
        let t = tx.open_table(table).map_err(backend)?;
        // `ReadOnlyTable::get_owned` (unlike `Table::get`) keeps the read
        // transaction alive via a reference-counted guard, so the returned
        // value can outlive the local borrow of `t`.
        let Some(v) = t.get_owned(target.as_str()).map_err(backend)? else {
            return Ok(None);
        };
        let parsed = serde_json::from_str(v.value()).map_err(decode)?;
        Ok(Some(parsed))
    }

    fn all_json<T: serde::de::DeserializeOwned>(
        &self,
        table: TableDefinition<&str, &str>,
    ) -> Result<Vec<T>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let t = tx.open_table(table).map_err(backend)?;
        let mut out = Vec::new();
        for entry in t.iter().map_err(backend)? {
            let (_k, v) = entry.map_err(backend)?;
            out.push(serde_json::from_str(v.value()).map_err(decode)?);
        }
        Ok(out)
    }

    fn all_log<T: serde::de::DeserializeOwned>(
        &self,
        table: TableDefinition<u64, &str>,
    ) -> Result<Vec<T>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let t = tx.open_table(table).map_err(backend)?;
        let mut out = Vec::new();
        for entry in t.iter().map_err(backend)? {
            let (_k, v) = entry.map_err(backend)?;
            out.push(serde_json::from_str(v.value()).map_err(decode)?);
        }
        Ok(out)
    }

    /// The hash of the manifest `project` was imported from, or `None` if
    /// this store authors it. A project this store never held is `NotOwned`:
    /// `None` would read as "authored here".
    pub fn imported_hash(&self, project: &ProjectId) -> Result<Option<String>, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(IMPORTS) {
            Ok(t) => t,
            // The first import creates the table. Its absence is "nothing
            // imported", read from a table that does not exist yet — not a
            // failure to look.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        let hash = table
            .get(project.iri().as_str())
            .map_err(backend)?
            .map(|v| v.value().to_string());
        Ok(hash)
    }

    fn refuse_if_imported(
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

    /// Export `project`, which this store must author.
    pub fn export_manifest(
        &self,
        project: &ProjectId,
        commit: &str,
        exported_at_unix: u64,
    ) -> Result<Manifest, ManifestError> {
        self.check_kind(project.iri(), Kind::Project)?;
        if self.imported_hash(project)?.is_some() {
            return Err(ManifestError::NotAuthoring(project.clone()));
        }
        manifest::export(self, project, commit, exported_at_unix)
    }

    /// Write the manifest's project, gates and transitions under their own
    /// IRIs, and mark the project imported — in ONE write transaction.
    /// Everything that can refuse is decided before anything is written.
    pub fn import_manifest(&self, m: &Manifest, root: &str) -> Result<ImportReport, ManifestError> {
        // A `Manifest` can be built by hand; the store checks it itself.
        m.verify()?;
        let body = &m.body;
        let project = &body.project;

        let held_project = match self.locate(project.iri()) {
            Ok((_, Kind::Project)) => true,
            Ok((_, found)) => {
                return Err(StoreError::WrongKind {
                    id: project.iri().clone(),
                    expected: Kind::Project,
                    found,
                }
                .into());
            }
            Err(StoreError::NotOwned { .. }) => false,
            Err(e) => return Err(e.into()),
        };
        if held_project && self.imported_hash(project)?.is_none() {
            return Err(ManifestError::AuthoringStore(project.clone()));
        }
        // Another project on the same root would give one checkout two
        // sets of gates and two handles, with nothing said.
        for other in self.list_projects()? {
            if other.id != *project && other.root == root {
                return Err(ManifestError::RootTaken {
                    root: root.to_string(),
                    other: other.id,
                });
            }
        }
        let root_moved = match held_project {
            true => {
                let held = self
                    .get_project(project)?
                    .ok_or_else(|| ManifestError::Inconsistent(format!("{project} vanished")))?;
                (held.root != root).then(|| (held.root, root.to_string()))
            }
            false => None,
        };

        let held_gates = if held_project {
            self.list_gates(project)?
        } else {
            vec![]
        };
        for g in &held_gates {
            if !body.gates.iter().any(|m| m.id == g.id) {
                return Err(ManifestError::WouldRemoveGate {
                    id: g.id.clone(),
                    name: g.name.clone(),
                });
            }
        }
        // Transitions mirror the manifest: one it no longer lists is
        // removed. (Neither side has a command that removes one today.)
        let stale_transitions: Vec<String> = if held_project {
            self.list_transitions(project)?
                .into_iter()
                .filter(|t| !body.transitions.iter().any(|m| m.name == t.name))
                .map(|t| t.name)
                .collect()
        } else {
            vec![]
        };

        let mut report = ImportReport {
            project: project.clone(),
            gates_added: 0,
            gates_changed: 0,
            gates_unchanged: 0,
            transitions: body.transitions.len(),
            transitions_removed: stale_transitions.clone(),
            root_moved,
        };
        // (definition to write, whether it is new to this store)
        let mut writes: Vec<(&GateDef, bool)> = Vec::new();
        for g in &body.gates {
            match self.locate(g.id.iri()) {
                Err(StoreError::NotOwned { .. }) => {
                    writes.push((g, true));
                    report.gates_added += 1;
                }
                Ok((_, Kind::Gate)) => {
                    let Some(held) = held_gates.iter().find(|h| h.id == g.id) else {
                        return Err(ManifestError::Inconsistent(format!(
                            "gate {} is held by this store under another project",
                            g.id
                        )));
                    };
                    let mut bare = held.clone();
                    bare.last_pass_commit = None;
                    if bare == *g {
                        report.gates_unchanged += 1;
                    } else {
                        writes.push((g, false));
                        report.gates_changed += 1;
                    }
                }
                Ok((_, found)) => {
                    return Err(StoreError::WrongKind {
                        id: g.id.iri().clone(),
                        expected: Kind::Gate,
                        found,
                    }
                    .into());
                }
                Err(e) => return Err(e.into()),
            }
        }

        let tx = self.db.begin_write().map_err(backend)?;
        if !held_project || report.root_moved.is_some() {
            if !held_project {
                index_new(&tx, project.iri(), Kind::Project)?;
            }
            let json = serde_json::to_string(&Project {
                id: project.clone(),
                root: root.to_string(),
            })
            .map_err(backend)?;
            tx.open_table(PROJECTS)
                .map_err(backend)?
                .insert(project.iri().as_str(), json.as_str())
                .map_err(backend)?;
        }
        for (g, is_new) in &writes {
            if *is_new {
                index_new(&tx, g.id.iri(), Kind::Gate)?;
            }
            let json = serde_json::to_string(g).map_err(backend)?;
            tx.open_table(GATES)
                .map_err(backend)?
                .insert(g.id.iri().as_str(), json.as_str())
                .map_err(backend)?;
        }
        {
            let mut table = tx.open_table(TRANSITIONS).map_err(backend)?;
            for name in &stale_transitions {
                table
                    .remove((project.iri().as_str(), name.as_str()))
                    .map_err(backend)?;
            }
            for t in &body.transitions {
                let json = serde_json::to_string(t).map_err(backend)?;
                table
                    .insert((t.project.iri().as_str(), t.name.as_str()), json.as_str())
                    .map_err(backend)?;
            }
        }
        tx.open_table(IMPORTS)
            .map_err(backend)?
            .insert(project.iri().as_str(), m.content_sha256.as_str())
            .map_err(backend)?;
        tx.open_table(META)
            .map_err(backend)?
            .insert(FORMAT_KEY, FORMAT_WITH_IMPORTS)
            .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(report)
    }
}

impl Catalog for RedbStore {
    fn add_project(&self, root: &str) -> Result<ProjectId, StoreError> {
        let id = self.insert_new(Kind::Project, PROJECTS, |id| Project {
            id: ProjectId(id),
            root: root.to_string(),
        })?;
        Ok(ProjectId(id))
    }

    fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError> {
        self.get_json(PROJECTS, id.iri())
    }

    fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        self.all_json(PROJECTS)
    }

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
    ) -> Result<GateId, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        self.refuse_if_imported(project, "add a gate to")?;
        let id = self.insert_new(Kind::Gate, GATES, |id| GateDef {
            id: GateId(id),
            project: project.clone(),
            name: name.to_string(),
            kind,
            selector,
            min_population,
            authored_at_commit: authored_at_commit.to_string(),
            authored_by: authored_by.to_string(),
            last_pass_commit: None,
        })?;
        Ok(GateId(id))
    }

    fn get_gate(&self, id: &GateId) -> Result<Option<GateDef>, StoreError> {
        self.get_json(GATES, id.iri())
    }

    fn list_gates(&self, project: &ProjectId) -> Result<Vec<GateDef>, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let all: Vec<GateDef> = self.all_json(GATES)?;
        Ok(all.into_iter().filter(|g| g.project == *project).collect())
    }

    fn update_gate(&self, def: &GateDef) -> Result<(), StoreError> {
        let Some(held) = self.get_gate(&def.id)? else {
            return Err(StoreError::NoSuchGate(def.id.clone()));
        };
        // An imported gate may earn a local pass mark and nothing else
        // (GitHub tracker spec §4.2).
        if self.imported_hash(&held.project)?.is_some() {
            let (mut before, mut after) = (held, def.clone());
            before.last_pass_commit = None;
            after.last_pass_commit = None;
            if before != after {
                return Err(StoreError::Imported {
                    id: def.id.iri().clone(),
                    action: "change the definition of",
                });
            }
        }
        self.put_json(GATES, def.id.iri(), def)
    }

    fn add_transition(&self, t: Transition) -> Result<(), StoreError> {
        self.check_kind(t.project.iri(), Kind::Project)?;
        self.refuse_if_imported(&t.project, "add a transition to")?;
        let json = serde_json::to_string(&t).map_err(backend)?;
        let tx = self.db.begin_write().map_err(backend)?;
        {
            let mut table = tx.open_table(TRANSITIONS).map_err(backend)?;
            table
                .insert((t.project.iri().as_str(), t.name.as_str()), json.as_str())
                .map_err(backend)?;
        }
        tx.commit().map_err(backend)?;
        Ok(())
    }

    fn get_transition(
        &self,
        project: &ProjectId,
        name: &str,
    ) -> Result<Option<Transition>, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let tx = self.db.begin_read().map_err(backend)?;
        let table = tx.open_table(TRANSITIONS).map_err(backend)?;
        let Some(v) = table
            .get_owned((project.iri().as_str(), name))
            .map_err(backend)?
        else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_str(v.value()).map_err(decode)?))
    }

    fn list_transitions(&self, project: &ProjectId) -> Result<Vec<Transition>, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let tx = self.db.begin_read().map_err(backend)?;
        let table = tx.open_table(TRANSITIONS).map_err(backend)?;
        let mut out = Vec::new();
        for row in table.iter().map_err(backend)? {
            let (_, v) = row.map_err(backend)?;
            let t: Transition = serde_json::from_str(v.value()).map_err(decode)?;
            // Filtered on the stored field rather than on the key's project
            // half: the key is a storage convention, the field is the fact,
            // and only one of the two is checked by the type system.
            if t.project == *project {
                out.push(t);
            }
        }
        Ok(out)
    }

    fn kind_of(&self, id: &Iri) -> Result<Kind, StoreError> {
        self.check(id)
    }
}

impl Tracker for RedbStore {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let id = self.insert_new(Kind::Record, RECORDS, |id| Record {
            id: RecordId(id),
            project: project.clone(),
            title: title.to_string(),
            state: State::Todo,
            also_known_as: vec![],
        })?;
        Ok(RecordId(id))
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.get_json(RECORDS, id.iri())
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let all: Vec<Record> = self.all_json(RECORDS)?;
        Ok(all.into_iter().filter(|r| r.project == *project).collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        let mut rec: Record = self
            .get_record(id)?
            .ok_or_else(|| StoreError::NoSuchRecord(id.clone()))?;
        rec.state = state;
        // Write under `rec.id`, not `id`: `id` may be an alias, and `rec.id`
        // is always the primary (an alias never changes what a fetched item
        // reports as its own id). Writing under an alias key would leave a
        // stray row behind instead of updating the one that exists.
        self.put_json(RECORDS, rec.id.iri(), &rec)
    }

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        self.check_kind(finding.project.iri(), Kind::Project)?;
        // `record` may be given as an alias (e.g. the CLI stores whatever
        // the caller typed): resolve to the primary, so two findings raised
        // against the same record always agree on which IRI names it.
        let (record_primary, record_kind) = self.locate(finding.record.iri())?;
        if record_kind != Kind::Record {
            return Err(StoreError::WrongKind {
                id: finding.record.iri().clone(),
                expected: Kind::Record,
                found: record_kind,
            });
        }
        let id = self.insert_new(Kind::Finding, FINDINGS, |id| {
            let mut finding = finding;
            finding.id = FindingId(id);
            finding.record = RecordId(record_primary);
            finding
        })?;
        Ok(FindingId(id))
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        self.get_json(FINDINGS, id.iri())
    }

    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        let Some(mut stored) = self.get_finding(&finding.id)? else {
            return Err(StoreError::NoSuchFinding(finding.id.clone()));
        };
        // `stored.id` is always the primary: `get_finding` already resolved
        // any alias before returning it. Take every other field from the
        // caller's version, but keep the id pinned to the primary — even if
        // `finding.id` (what the caller passed) is an alias — so an update
        // through an alias still lands on, and stays keyed by, the primary,
        // rather than writing a second row under the alias.
        //
        // The stored `also_known_as` is kept and the caller's ignored (see
        // the trait): only `add_alias` adds a name.
        let primary = stored.id.clone();
        let also_known_as = std::mem::take(&mut stored.also_known_as);
        stored = finding.clone();
        stored.id = primary.clone();
        stored.also_known_as = also_known_as;
        self.put_json(FINDINGS, primary.iri(), &stored)
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let all: Vec<Finding> = self.all_json(FINDINGS)?;
        Ok(all.into_iter().filter(|f| f.project == *project).collect())
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        let all: Vec<Finding> = self.all_json(FINDINGS)?;
        Ok(all
            .into_iter()
            .filter(|f| f.raised_by == actor && f.state == FindingState::Withdrawn)
            .count() as u64)
    }

    /// Mint an alias, index it, and append it to the item's `also_known_as`
    /// row — in ONE write transaction, for the same reason as
    /// `insert_new_with_id`: a failure partway must leave neither the index
    /// entry nor the row change behind.
    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;

        let (resolved, kind) = {
            let ids = tx.open_table(IDS).map_err(backend)?;
            // `alias` must be unused, whether as a primary id or as another
            // alias — both live in this one table.
            if ids.get(alias.as_str()).map_err(backend)?.is_some() {
                return Err(StoreError::AlreadyExists(alias));
            }
            let Some(pv) = ids.get(primary.as_str()).map_err(backend)? else {
                return Err(StoreError::NotOwned {
                    id: primary.clone(),
                    searched: vec![self.label.clone()],
                });
            };
            if pv.value() == "alias" {
                // `primary` is itself an alias: resolve one more hop so
                // `ALIASES` never chains. Shares `locate`'s own alias-hop
                // logic rather than re-implementing it.
                let aliases = tx.open_table(ALIASES).map_err(backend)?;
                let (resolved, wire) = alias_primary(primary, &ids, &aliases)?;
                let kind = Kind::from_wire(&wire)
                    .ok_or_else(|| decode(format!("unknown kind `{wire}` for {resolved}")))?;
                (resolved, kind)
            } else {
                let wire = pv.value().to_string();
                let kind = Kind::from_wire(&wire)
                    .ok_or_else(|| decode(format!("unknown kind `{wire}` for {primary}")))?;
                (primary.clone(), kind)
            }
        };

        let table = match kind {
            Kind::Record => RECORDS,
            Kind::Finding => FINDINGS,
            other => {
                return Err(StoreError::Backend(format!(
                    "{resolved} is a {}, and only a record or finding can carry an alias",
                    other.as_wire()
                )));
            }
        };
        {
            let mut t = tx.open_table(table).map_err(backend)?;
            // `IDS` says this id is a record or finding, but that does not
            // put its row here: a damaged store can hold the index entry
            // without the row. That is `Decode`, like `alias_primary`'s
            // damaged cases — never a panic.
            let json = match t.get(resolved.as_str()).map_err(backend)? {
                Some(v) => v.value().to_string(),
                None => {
                    return Err(decode(format!(
                        "{resolved} is indexed as a {}, but this store holds no row for it",
                        kind.as_wire()
                    )));
                }
            };
            let updated = match kind {
                Kind::Record => {
                    let mut r: Record = serde_json::from_str(&json).map_err(decode)?;
                    r.also_known_as.push(alias.clone());
                    serde_json::to_string(&r).map_err(backend)?
                }
                Kind::Finding => {
                    let mut f: Finding = serde_json::from_str(&json).map_err(decode)?;
                    f.also_known_as.push(alias.clone());
                    serde_json::to_string(&f).map_err(backend)?
                }
                _ => unreachable!("checked above"),
            };
            t.insert(resolved.as_str(), updated.as_str())
                .map_err(backend)?;
        }
        {
            let mut ids = tx.open_table(IDS).map_err(backend)?;
            ids.insert(alias.as_str(), "alias").map_err(backend)?;
        }
        {
            let mut aliases = tx.open_table(ALIASES).map_err(backend)?;
            aliases
                .insert(alias.as_str(), resolved.as_str())
                .map_err(backend)?;
        }

        tx.commit().map_err(backend)?;
        Ok(())
    }
}

impl Ledger for RedbStore {
    // `append_*` checks nothing (spec §3.4): the engine already resolved the
    // references it is recording.
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError> {
        self.append_json(NEXT_RUN, GATE_RUNS, &run)
    }

    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError> {
        self.append_json(NEXT_ATTEMPT, ATTEMPTS, &attempt)
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        self.check(gate.iri())?;
        let all: Vec<GateRun> = self.all_log(GATE_RUNS)?;
        Ok(all.into_iter().filter(|r| r.gate == *gate).collect())
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let all: Vec<Attempt> = self.all_log(ATTEMPTS)?;
        Ok(all.into_iter().filter(|a| a.project == *project).collect())
    }
}

impl Handles for RedbStore {
    /// `None` for an id this store does not hold under `kind`: handles are
    /// per kind, so a gate's IRI has no project handle.
    fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let ids = tx.open_table(IDS).map_err(backend)?;
        match ids.get(id.as_str()).map_err(backend)? {
            Some(k) if k.value() == kind.as_wire() => {}
            _ => return Ok(None),
        }
        let t = tx.open_table(HANDLE_OF).map_err(backend)?;
        Ok(t.get(id.as_str()).map_err(backend)?.map(|v| v.value()))
    }

    fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let t = tx.open_table(HANDLES).map_err(backend)?;
        let Some(v) = t.get((kind.as_wire(), handle)).map_err(backend)? else {
            return Ok(None);
        };
        Iri::parse(v.value()).map(Some).map_err(decode)
    }
}

impl fl_core::store::Bindings for RedbStore {
    fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(GITHUB_BINDINGS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        let found = table
            .get(repo.to_ascii_lowercase().as_str())
            .map_err(backend)?
            .map(|v| v.value().to_string());
        Ok(found)
    }
    fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        tx.open_table(GITHUB_BINDINGS)
            .map_err(backend)?
            .insert(repo.to_ascii_lowercase().as_str(), node_id)
            .map_err(backend)?;
        tx.commit().map_err(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ids::seq_iri;
    use fl_core::log::GateRun;
    use fl_core::model::State;
    use fl_core::verdict::Verdict;
    use redb::ReadableTableMetadata;

    /// A store written before format versioning: tables, and no version key.
    /// The table definitions are local and frozen: this fixture must keep
    /// writing the OLD shape after Task 4 changes the live ones.
    fn legacy_store(path: &std::path::Path) {
        const OLD_META: TableDefinition<&str, u64> = TableDefinition::new("meta");
        const OLD_PROJECTS: TableDefinition<u64, &str> = TableDefinition::new("projects");
        let db = redb::Database::create(path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(OLD_META)
                .unwrap()
                .insert("next_id", 3u64)
                .unwrap();
            tx.open_table(OLD_PROJECTS)
                .unwrap()
                .insert(1u64, "{}")
                .unwrap();
        }
        tx.commit().unwrap();
    }

    #[test]
    fn a_project_survives_a_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");

        let id = {
            let s = RedbStore::open(&path).unwrap();
            s.add_project("/tmp/p").unwrap()
        };

        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.get_project(&id).unwrap().unwrap().root, "/tmp/p");
    }

    #[test]
    fn two_independent_stores_mint_different_ids() {
        let (a, _da) = fresh();
        let (b, _db) = fresh();
        let pa = a.add_project("/p").unwrap();
        let pb = b.add_project("/p").unwrap();
        let ga = a
            .add_gate(&pa, "g", kind(), selector(), 1, "abc", "o")
            .unwrap();
        let gb = b
            .add_gate(&pb, "g", kind(), selector(), 1, "abc", "o")
            .unwrap();
        assert_ne!(
            ga, gb,
            "two installs must not both have `gate 1` as their id"
        );
        assert!(ga.iri().as_str().starts_with("urn:uuid:"));
    }

    #[test]
    fn a_handle_is_never_reused_after_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        {
            let s = RedbStore::open(&path).unwrap();
            s.add_project("/a").unwrap();
        }
        let s = RedbStore::open(&path).unwrap();
        let b = s.add_project("/b").unwrap();
        assert_eq!(s.handle_of(Kind::Project, b.iri()).unwrap(), Some(2));
    }

    #[test]
    fn a_minted_id_that_already_exists_is_refused_and_nothing_is_overwritten() {
        let (s, _d) = fresh();
        let id = Iri::parse("urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b").unwrap();
        s.insert_new_with_id(id.clone(), Kind::Project, PROJECTS, |i| Project {
            id: ProjectId(i),
            root: "/first".into(),
        })
        .unwrap();
        let err = s
            .insert_new_with_id(id.clone(), Kind::Project, PROJECTS, |i| Project {
                id: ProjectId(i),
                root: "/second".into(),
            })
            .unwrap_err();
        assert!(
            matches!(err, StoreError::AlreadyExists(ref i) if *i == id),
            "{err:?}"
        );
        assert_eq!(
            s.get_project(&ProjectId(id)).unwrap().unwrap().root,
            "/first"
        );
    }

    #[test]
    fn a_format_1_store_is_refused_by_format_2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.redb");
        {
            let db = redb::Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            tx.open_table(META)
                .unwrap()
                .insert("format_version", 1u64)
                .unwrap();
            tx.commit().unwrap();
        }
        let err = RedbStore::open(&path).err().unwrap();
        assert!(
            matches!(
                err,
                StoreError::FormatVersion {
                    found: Some(1),
                    expected: 2
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_record_state_change_is_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let (p, r) = {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let r = s.add_record(&p, "t").unwrap();
            s.set_record_state(&r, State::Review).unwrap();
            (p, r)
        };
        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.get_record(&r).unwrap().unwrap().state, State::Review);
        assert_eq!(s.get_project(&p).unwrap().unwrap().id, p);
    }

    /// Beyond the brief: `Verdict`'s `Deserialize` is hand-written and rejects
    /// a zero-population `Pass`. A `GateRun` carries a `Verdict` through this
    /// store as JSON, so round-tripping it is the one place this task can
    /// silently corrupt a passing gate into something else (or fail to
    /// deserialize at all). Pin that the verdict and its population survive
    /// a close and reopen unchanged.
    #[test]
    fn a_gate_run_verdict_survives_a_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");

        let verdict = Verdict::from_predicate(true, 3);
        let g = {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let g = s
                .add_gate(
                    &p,
                    "fmt",
                    fl_core::model::GateKind::Command(fl_core::model::CommandSpec {
                        program: "true".into(),
                        args: vec![],
                        delivery: fl_core::model::PopulationDelivery::Args,
                        timeout_secs: 5,
                        pass_codes: vec![0],
                    }),
                    fl_core::model::Selector::Glob {
                        pattern: "**/*.rs".into(),
                    },
                    1,
                    "abc",
                    "owner",
                )
                .unwrap();
            s.append_gate_run(GateRun {
                id: None,
                at: None,
                gate: g.clone(),
                record: None,
                commit: "abc".into(),
                verdict: verdict.clone(),
                population: 3,
                output_excerpt: Some(String::new()),
                duration_ms: 1,
                cost_usd_micros: 0,
            })
            .unwrap();
            g
        };

        let s = RedbStore::open(&path).unwrap();
        let runs = s.gate_runs(&g).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].verdict, verdict);
        assert_eq!(runs[0].verdict.population(), Some(3));
    }

    /// Fix round 1, rewritten for handles: pins that minting, indexing,
    /// the handle bump and the row write inside `insert_new_with_id` commit
    /// as one unit. If they didn't, a failed insert would still leave the
    /// handle counter advanced and the id in `IDS` — a burned handle, and an
    /// id this store "owns" with no row behind it.
    ///
    /// There is no seam in the public role API to make the *last* step of a
    /// normal insert fail deterministically and cheaply: every concrete type
    /// this store serializes always serializes via `serde_json` without
    /// error, and redb's own size ceiling (`MAX_VALUE_LENGTH`, 3 GiB) is too
    /// large to hit in a fast test. So this test reaches for the same
    /// private field `RedbStore::open` itself would build (`db`, visible to
    /// this module) and pre-corrupts the on-disk `gates` table with a
    /// mismatched value type *before* constructing the store — bypassing
    /// `RedbStore::open`, which eagerly creates every table itself. This
    /// makes `add_gate`'s `IDS` insert, its `next_handle:gate` bump and its
    /// handle rows succeed, and its final `open_table(GATES)` fail with a
    /// genuine `redb::TableTypeMismatch`, inside the same still-uncommitted
    /// write transaction — the same commit boundary a real row-write failure
    /// would cross.
    #[test]
    fn a_failed_insert_does_not_advance_the_shared_id_counter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");

        let db = redb::Database::create(&path).unwrap();
        {
            // Same table name as `GATES`, wrong key and value types. Opening
            // it later under the real `GATES` definition will fail with
            // `TableTypeMismatch`, not silently coerce.
            const WRONG_GATES: TableDefinition<u64, u64> = TableDefinition::new("gates");
            let tx = db.begin_write().unwrap();
            {
                tx.open_table(WRONG_GATES).unwrap();
            }
            tx.commit().unwrap();
        }

        // Bypasses `RedbStore::open` deliberately: it would try to create
        // the real `GATES` definition itself and fail right there, before we
        // ever get to call `add_gate`.
        let store = RedbStore {
            db,
            label: path.display().to_string(),
        };

        // The gate needs a project this store owns, or `add_gate` would be
        // refused by the ownership check before it reached the write.
        let p = store.add_project("/p").unwrap();

        let failed = store.add_gate(
            &p,
            "fmt",
            GateKind::Command(fl_core::model::CommandSpec {
                program: "true".into(),
                args: vec![],
                delivery: fl_core::model::PopulationDelivery::Args,
                timeout_secs: 5,
                pass_codes: vec![0],
            }),
            Selector::Glob {
                pattern: "**/*.rs".into(),
            },
            1,
            "abc",
            "owner",
        );
        assert!(
            failed.is_err(),
            "expected the mismatched `gates` table to reject the write"
        );

        let tx = store.db.begin_read().unwrap();
        let meta = tx.open_table(META).unwrap();
        assert_eq!(
            meta.get("next_handle:gate").unwrap().map(|v| v.value()),
            None,
            "a failed insert must not burn a handle"
        );
        let ids = tx.open_table(IDS).unwrap();
        assert_eq!(
            ids.len().unwrap(),
            1,
            "a failed insert must leave no `IDS` entry: only the project is owned"
        );
        assert!(ids.get(p.iri().as_str()).unwrap().is_some());
    }

    #[test]
    fn a_finding_survives_a_close_and_reopen_with_a_real_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");

        let id = {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let r = s.add_record(&p, "t").unwrap();
            s.add_finding(Finding::raise(p, r, "reviewer", "wrong on empty"))
                .unwrap()
        };
        assert_ne!(
            id,
            FindingId(seq_iri(0)),
            "the store must replace the placeholder id"
        );

        let s = RedbStore::open(&path).unwrap();
        let back = s.get_finding(&id).unwrap().unwrap();
        assert_eq!(back.id, id);
        assert_eq!(back.state, FindingState::Raised);
    }

    #[test]
    fn withdrawals_are_counted_against_whoever_raised_the_finding_and_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");

        {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let r = s.add_record(&p, "t").unwrap();

            for claim in ["a", "b"] {
                let id = s
                    .add_finding(Finding::raise(p.clone(), r.clone(), "hasty", claim))
                    .unwrap();
                let mut f = s.get_finding(&id).unwrap().unwrap();
                f.withdraw("not concrete").unwrap();
                s.update_finding(&f).unwrap();
            }
            let id = s
                .add_finding(Finding::raise(p.clone(), r.clone(), "careful", "c"))
                .unwrap();
            let mut f = s.get_finding(&id).unwrap().unwrap();
            f.attach_reproduction(GateId(seq_iri(1))).unwrap();
            s.update_finding(&f).unwrap();
        }

        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.withdrawals_by("hasty").unwrap(), 2);
        assert_eq!(s.withdrawals_by("careful").unwrap(), 0);
        assert_eq!(s.withdrawals_by("nobody").unwrap(), 0);
    }

    #[test]
    fn a_record_written_in_an_older_wire_format_is_refused_with_a_remedy() {
        // What a wire-format change looks like from the other side. The
        // refusal must name the cause AND what to do, not just repeat what
        // serde said. Written as raw JSON because the point is a value this
        // build can no longer produce.
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("old.redb");
        let s = RedbStore::open(&path).expect("open");
        // A record this store owns, so the read reaches the decode step; its
        // row is then overwritten with what an older build wrote (a
        // PascalCase state).
        let p = s.add_project("/p").expect("project");
        let r = s.add_record(&p, "t").expect("record");
        {
            let tx = s.db.begin_write().expect("write tx");
            {
                let mut t = tx.open_table(RECORDS).expect("table");
                let old = format!(
                    r#"{{"id":"{}","project":"{}","title":"t","state":"Todo"}}"#,
                    r.iri(),
                    p.iri()
                );
                t.insert(r.iri().as_str(), old.as_str()).expect("insert");
            }
            tx.commit().expect("commit");
        }
        let err = s.get_record(&r).expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("unknown variant"), "got {msg}");
        assert!(msg.contains("there is no migration"), "got {msg}");
        assert!(msg.contains("restore the file from a backup"), "got {msg}");
        assert!(
            matches!(err, StoreError::Decode(_)),
            "a decode failure must be Decode, not merely not-Backend: {err:?}"
        );
    }

    // Final review, item 7: `IDS` naming an id as a record whose row is
    // missing is on-disk damage, and `add_alias` must report it as `Decode`
    // — as `alias_primary` reports its own damaged cases — never panic.
    #[test]
    fn add_alias_on_an_indexed_record_whose_row_is_missing_is_decode_not_a_panic() {
        let (s, _d) = fresh();
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        {
            let tx = s.db.begin_write().unwrap();
            tx.open_table(RECORDS)
                .unwrap()
                .remove(r.iri().as_str())
                .unwrap();
            tx.commit().unwrap();
        }
        let alias = Iri::parse("https://github.com/o/r/issues/15").unwrap();
        let err = s.add_alias(r.iri(), alias.clone()).unwrap_err();
        assert!(matches!(err, StoreError::Decode(_)), "{err:?}");
        assert!(err.to_string().contains(r.iri().as_str()), "{err}");
        assert!(
            !s.owns(&alias).unwrap(),
            "a refused add_alias must not index the alias"
        );
    }

    fn fresh() -> (RedbStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = RedbStore::open(&dir.path().join("t.redb")).unwrap();
        (s, dir)
    }

    /// A store holding one run and one attempt in the shape every store held
    /// before entry ids: raw rows, as the current fl writes them. The row
    /// counters move with them, so a later append cannot overwrite them.
    fn with_legacy_entries() -> (RedbStore, tempfile::TempDir, ProjectId, GateId) {
        let (s, d) = fresh();
        let p = s.add_project("/p").unwrap();
        let g = s
            .add_gate(&p, "fmt", kind(), selector(), 1, "abc", "o")
            .unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let run = format!(
            r#"{{"gate":"{}","record":"{}","commit":"abc","verdict":{{"pass":{{"population":1}}}},"population":1,"output_excerpt":"ok","duration_ms":1,"cost_usd_micros":0}}"#,
            g.iri(),
            r.iri()
        );
        let attempt = format!(
            r#"{{"project":"{}","record":"{}","adapter":"claude","status":"completed","duration_ms":1,"tokens_in":0,"tokens_out":0,"cost_usd_micros":0,"paths_touched":["a.rs"],"output_excerpt":""}}"#,
            p.iri(),
            r.iri()
        );
        {
            let tx = s.db.begin_write().unwrap();
            tx.open_table(GATE_RUNS)
                .unwrap()
                .insert(1u64, run.as_str())
                .unwrap();
            tx.open_table(ATTEMPTS)
                .unwrap()
                .insert(1u64, attempt.as_str())
                .unwrap();
            {
                let mut meta = tx.open_table(META).unwrap();
                meta.insert(NEXT_RUN, 1u64).unwrap();
                meta.insert(NEXT_ATTEMPT, 1u64).unwrap();
            }
            tx.commit().unwrap();
        }
        (s, d, p, g)
    }

    #[test]
    fn a_run_and_an_attempt_stored_before_entry_ids_still_read() {
        let (s, _d, p, g) = with_legacy_entries();
        let runs = s.gate_runs(&g).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].id.as_ref(), runs[0].at.as_ref()), (None, None));
        assert_eq!(runs[0].output_excerpt.as_deref(), Some("ok"));
        let attempts = s.attempts(&p).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].id, None);
        assert_eq!(
            attempts[0].paths_touched,
            fl_core::log::PathsTouched::Listed(vec!["a.rs".into()])
        );
    }

    fn kind() -> GateKind {
        GateKind::Command(fl_core::model::CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: fl_core::model::PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        })
    }

    fn selector() -> Selector {
        Selector::Glob {
            pattern: "**/*.rs".into(),
        }
    }

    #[test]
    fn a_store_from_before_format_versioning_is_refused_with_a_remedy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        legacy_store(&path);
        let err = RedbStore::open(&path)
            .err()
            .expect("an unversioned store must be refused");
        assert!(
            matches!(
                err,
                StoreError::FormatVersion {
                    found: None,
                    expected: FORMAT_VERSION
                }
            ),
            "{err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("start a new store"), "no remedy: {msg}");
    }

    // Separate from the refusal above, so that "refuses everything" cannot pass
    // as "refuses the old store".
    #[test]
    fn a_new_store_is_accepted_and_accepted_again_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.redb");
        RedbStore::open(&path).unwrap();
        RedbStore::open(&path).unwrap();
    }

    #[test]
    fn a_store_already_open_is_unreachable_and_names_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let _held = RedbStore::open(&path).unwrap();
        let err = RedbStore::open(&path)
            .err()
            .expect("a second open of a held store must fail");
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn redb_store_meets_every_role_contract() {
        use fl_core::conformance::Single;
        let single = || {
            let (s, g) = fresh();
            Single(s, g)
        };
        fl_core::conformance::catalog(fresh);
        fl_core::conformance::tracker(single);
        fl_core::conformance::ledger(fresh);
        fl_core::conformance::all_roles(single);
        fl_core::conformance::local_handles(fresh);
    }

    use crate::manifest::{Manifest, ManifestError, content_sha256};
    use fl_core::model::{Regret, Transition};

    /// An authoring store with one project: gates `fmt` and `lint`, and a
    /// transition over both. Returns the store's guard too.
    fn authoring() -> (RedbStore, tempfile::TempDir, ProjectId, GateId, GateId) {
        let (s, dir) = fresh();
        let p = s.add_project("/author").unwrap();
        let g1 = s
            .add_gate(&p, "fmt", kind(), selector(), 1, "c1", "o")
            .unwrap();
        let g2 = s
            .add_gate(&p, "lint", kind(), selector(), 1, "c1", "o")
            .unwrap();
        s.add_transition(Transition {
            project: p.clone(),
            name: "ship".into(),
            from: State::Review,
            to: State::Done,
            regret: Regret::High,
            gates: vec![g1.clone(), g2.clone()],
        })
        .unwrap();
        (s, dir, p, g1, g2)
    }

    fn stamp(s: &RedbStore, g: &GateId, commit: &str) {
        let mut def = s.get_gate(g).unwrap().unwrap();
        def.last_pass_commit = Some(commit.into());
        s.update_gate(&def).unwrap();
    }

    #[test]
    fn an_import_writes_every_item_under_its_own_iri_and_marks_the_project() {
        let (a, _ga, p, g1, g2) = authoring();
        let m = a.export_manifest(&p, "c1", 7).unwrap();
        let (b, _gb) = fresh();
        let report = b.import_manifest(&m, "/elsewhere").unwrap();
        assert_eq!((report.gates_added, report.transitions), (2, 1));
        assert_eq!(b.get_project(&p).unwrap().unwrap().root, "/elsewhere");
        assert_eq!(b.get_gate(&g1).unwrap(), a.get_gate(&g1).unwrap());
        assert!(b.owns(g2.iri()).unwrap());
        assert!(b.get_transition(&p, "ship").unwrap().is_some());
        assert_eq!(b.imported_hash(&p).unwrap(), Some(m.content_sha256.clone()));
        assert!(b.handle_of(Kind::Gate, g1.iri()).unwrap().is_some());
    }

    #[test]
    fn a_store_that_never_imported_reads_as_nothing_imported() {
        let (s, _g) = fresh();
        let p = s.add_project("/p").unwrap();
        assert_eq!(s.imported_hash(&p).unwrap(), None);
    }

    #[test]
    fn the_authoring_store_refuses_to_import_its_own_project() {
        let (a, _g, p, _, _) = authoring();
        let m = a.export_manifest(&p, "c1", 7).unwrap();
        let err = a.import_manifest(&m, "/author").unwrap_err();
        assert!(matches!(err, ManifestError::AuthoringStore(_)), "{err}");
    }

    #[test]
    fn an_importing_store_refuses_to_export() {
        let (a, _ga, p, _, _) = authoring();
        let (b, _gb) = fresh();
        b.import_manifest(&a.export_manifest(&p, "c1", 7).unwrap(), "/x")
            .unwrap();
        let err = b.export_manifest(&p, "c1", 8).unwrap_err();
        assert!(matches!(err, ManifestError::NotAuthoring(_)), "{err}");
    }

    #[test]
    fn an_imported_gate_can_earn_a_pass_mark_but_cannot_be_edited() {
        let (a, _ga, p, g1, _) = authoring();
        let (b, _gb) = fresh();
        b.import_manifest(&a.export_manifest(&p, "c1", 7).unwrap(), "/x")
            .unwrap();
        stamp(&b, &g1, "c1");
        assert_eq!(
            b.get_gate(&g1)
                .unwrap()
                .unwrap()
                .last_pass_commit
                .as_deref(),
            Some("c1")
        );

        let mut edited = b.get_gate(&g1).unwrap().unwrap();
        edited.name = "renamed".into();
        let err = b.update_gate(&edited).unwrap_err();
        assert!(matches!(err, StoreError::Imported { .. }), "{err}");

        let err = b
            .add_gate(&p, "new", kind(), selector(), 1, "c1", "o")
            .unwrap_err();
        assert!(matches!(err, StoreError::Imported { .. }), "{err}");

        let err = b
            .add_transition(Transition {
                project: p.clone(),
                name: "other".into(),
                from: State::Todo,
                to: State::Doing,
                regret: Regret::Low,
                gates: vec![],
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::Imported { .. }), "{err}");
    }

    #[test]
    fn a_reimport_keeps_the_mark_of_an_unchanged_gate_and_drops_a_changed_ones() {
        let (a, _ga, p, g1, g2) = authoring();
        let (b, _gb) = fresh();
        b.import_manifest(&a.export_manifest(&p, "c1", 7).unwrap(), "/x")
            .unwrap();
        stamp(&b, &g1, "c1");
        stamp(&b, &g2, "c1");

        let mut changed = a.get_gate(&g2).unwrap().unwrap();
        changed.authored_at_commit = "c2".into();
        a.update_gate(&changed).unwrap();
        let g3 = a
            .add_gate(&p, "new", kind(), selector(), 1, "c2", "o")
            .unwrap();

        let report = b
            .import_manifest(&a.export_manifest(&p, "c2", 8).unwrap(), "/x")
            .unwrap();
        assert_eq!(
            (
                report.gates_added,
                report.gates_changed,
                report.gates_unchanged
            ),
            (1, 1, 1)
        );
        assert_eq!(
            b.get_gate(&g1)
                .unwrap()
                .unwrap()
                .last_pass_commit
                .as_deref(),
            Some("c1")
        );
        assert_eq!(b.get_gate(&g2).unwrap().unwrap().last_pass_commit, None);
        assert_eq!(b.get_gate(&g2).unwrap().unwrap().authored_at_commit, "c2");
        assert!(b.handle_of(Kind::Gate, g3.iri()).unwrap().is_some());
    }

    #[test]
    fn an_import_refuses_a_manifest_it_did_not_verify() {
        let (a, _ga, p, _, _) = authoring();
        let mut m = a.export_manifest(&p, "c1", 7).unwrap();
        m.body.gates[0].name = "edited".into();
        let (b, _gb) = fresh();
        let err = b.import_manifest(&m, "/x").unwrap_err();
        assert!(matches!(err, ManifestError::HandEdited { .. }), "{err}");
        assert!(!b.owns(p.iri()).unwrap(), "nothing may be written");
    }

    #[test]
    fn a_fresh_import_onto_a_root_another_project_uses_is_refused() {
        let (a, _ga, p, _, _) = authoring();
        let (b, _gb) = fresh();
        let local = b.add_project("/x").unwrap();
        let err = b
            .import_manifest(&a.export_manifest(&p, "c1", 7).unwrap(), "/x")
            .unwrap_err();
        assert!(
            matches!(err, ManifestError::RootTaken { ref other, .. } if *other == local),
            "{err}"
        );
    }

    #[test]
    fn a_reimport_from_another_checkout_moves_the_root_and_says_so() {
        let (a, _ga, p, _, _) = authoring();
        let (b, _gb) = fresh();
        let m = a.export_manifest(&p, "c1", 7).unwrap();
        assert_eq!(b.import_manifest(&m, "/one").unwrap().root_moved, None);
        let report = b.import_manifest(&m, "/two").unwrap();
        assert_eq!(report.root_moved, Some(("/one".into(), "/two".into())));
        assert_eq!(b.get_project(&p).unwrap().unwrap().root, "/two");
    }

    #[test]
    fn a_reimport_mirrors_the_manifests_transitions() {
        let (a, _ga, p, _, _) = authoring();
        let (b, _gb) = fresh();
        let first = a.export_manifest(&p, "c1", 7).unwrap();
        b.import_manifest(&first, "/x").unwrap();
        let mut body = first.body.clone();
        body.transitions.clear();
        let without = Manifest {
            content_sha256: content_sha256(&body).unwrap(),
            body,
        };
        let report = b.import_manifest(&without, "/x").unwrap();
        assert!(b.list_transitions(&p).unwrap().is_empty());
        assert_eq!(report.transitions_removed, vec!["ship".to_string()]);
    }

    #[test]
    fn imported_hash_of_a_project_this_store_never_held_is_not_owned() {
        let (s, _g) = fresh();
        let err = s.imported_hash(&ProjectId(seq_iri(42))).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
    }

    #[test]
    fn a_store_that_imported_is_format_3_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.redb");
        let (a, _ga, p, _, _) = authoring();
        {
            let b = RedbStore::open(&path).unwrap();
            b.import_manifest(&a.export_manifest(&p, "c1", 7).unwrap(), "/x")
                .unwrap();
        }
        let db = redb::Database::open(&path).unwrap();
        let tx = db.begin_read().unwrap();
        let meta = tx.open_table(META).unwrap();
        assert_eq!(
            meta.get(FORMAT_KEY).unwrap().map(|v| v.value()),
            Some(FORMAT_WITH_IMPORTS),
            "an older fl must refuse this store rather than ignoring its imports"
        );
        drop(meta);
        drop(tx);
        drop(db);
        let b = RedbStore::open(&path).unwrap();
        assert!(b.imported_hash(&p).unwrap().is_some());
    }

    #[test]
    fn a_reimport_that_removes_a_gate_is_refused_and_changes_nothing() {
        let (a, _ga, p, g1, g2) = authoring();
        let (b, _gb) = fresh();
        let first = a.export_manifest(&p, "c1", 7).unwrap();
        b.import_manifest(&first, "/x").unwrap();

        let mut body = first.body.clone();
        body.gates.retain(|g| g.id != g2);
        for t in &mut body.transitions {
            t.gates.retain(|g| *g != g2);
        }
        let shrunk = Manifest {
            content_sha256: content_sha256(&body).unwrap(),
            body,
        };
        let err = b.import_manifest(&shrunk, "/x").unwrap_err();
        assert!(
            matches!(err, ManifestError::WouldRemoveGate { ref id, ref name } if *id == g2 && name == "lint"),
            "{err}"
        );
        assert_eq!(b.imported_hash(&p).unwrap(), Some(first.content_sha256));
        assert!(b.get_gate(&g1).unwrap().is_some());
    }

    #[test]
    fn a_gate_held_locally_under_another_project_is_refused_and_writes_nothing() {
        let (a, _ga, p, _g1, _g2) = authoring();
        let m = a.export_manifest(&p, "c1", 7).unwrap();

        // `b` authors its own project locally and holds a gate under it —
        // the manifest below is rewritten to claim that same gate id for a
        // project `b` has never held.
        let (b, _gb) = fresh();
        let q = b.add_project("/local").unwrap();
        let local_gate = b
            .add_gate(&q, "local", kind(), selector(), 1, "c0", "o")
            .unwrap();

        let old_id = m.body.gates[0].id.clone();
        let mut body = m.body.clone();
        body.gates[0].id = local_gate.clone();
        for t in &mut body.transitions {
            for gid in &mut t.gates {
                if *gid == old_id {
                    *gid = local_gate.clone();
                }
            }
        }
        let swapped = Manifest {
            content_sha256: content_sha256(&body).unwrap(),
            body,
        };

        let err = b.import_manifest(&swapped, "/x").unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
        assert!(!b.owns(p.iri()).unwrap(), "nothing may be written");
        assert_eq!(b.get_gate(&local_gate).unwrap().unwrap().project, q);
    }

    #[test]
    fn a_binding_survives_a_reopen_and_a_fresh_store_has_none() {
        use fl_core::store::Bindings;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.redb");
        {
            let s = RedbStore::open(&path).unwrap();
            assert_eq!(s.bound_node_id("acme/widgets").unwrap(), None);
            s.bind_node_id("Acme/Widgets", "R_1").unwrap();
        }
        let s = RedbStore::open(&path).unwrap();
        assert_eq!(
            s.bound_node_id("ACME/widgets").unwrap().as_deref(),
            Some("R_1")
        );
    }
}
