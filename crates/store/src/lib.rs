//! redb persistence for the `fl-core` store roles.

use fl_core::finding::{Finding, FindingState};
use fl_core::ids::{FindingId, GateId, Kind, ProjectId, RecordId};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use fl_core::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};
use fl_core::store::{Catalog, Handles, Ledger, StoreError, Tracker};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;

/// Format 2: ids are IRIs, with an ownership index and per-kind handles.
/// Format 1 keyed every table by a `u64` id from one shared counter.
pub const FORMAT_VERSION: u64 = 2;
const FORMAT_KEY: &str = "format_version";

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// Every id this store ever minted → its kind's wire name. Append-only.
const IDS: TableDefinition<&str, &str> = TableDefinition::new("ids");
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

const NEXT_RUN: &str = "next_run";
const NEXT_ATTEMPT: &str = "next_attempt";

pub struct RedbStore {
    db: Database,
    label: String,
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
            Some(Some(v)) if v == FORMAT_VERSION => {}
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
        {
            let mut ids = tx.open_table(IDS).map_err(backend)?;
            if ids.get(id.as_str()).map_err(backend)?.is_some() {
                return Err(StoreError::AlreadyExists(id));
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
            tx.open_table(table)
                .map_err(backend)?
                .insert(id.as_str(), json.as_str())
                .map_err(backend)?;
        }
        tx.commit().map_err(backend)?;
        Ok(id)
    }

    /// The kind this store holds `id` under, or `NotOwned` naming this store.
    ///
    /// ⚠ Every method that takes an id asks this first — list methods too.
    /// A list over a project this store never held is "didn't look", and an
    /// empty list would say "looked, found nothing".
    fn check(&self, id: &Iri) -> Result<Kind, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let ids = tx.open_table(IDS).map_err(backend)?;
        let Some(v) = ids.get(id.as_str()).map_err(backend)? else {
            return Err(StoreError::NotOwned {
                id: id.clone(),
                searched: vec![self.label.clone()],
            });
        };
        Kind::from_wire(v.value())
            .ok_or_else(|| decode(format!("unknown kind `{}` for {id}", v.value())))
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
    /// and an id it holds under another kind is `Ok(None)`.
    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        table: TableDefinition<&str, &str>,
        key: &Iri,
    ) -> Result<Option<T>, StoreError> {
        self.check(key)?;
        let tx = self.db.begin_read().map_err(backend)?;
        let t = tx.open_table(table).map_err(backend)?;
        // `ReadOnlyTable::get_owned` (unlike `Table::get`) keeps the read
        // transaction alive via a reference-counted guard, so the returned
        // value can outlive the local borrow of `t`.
        let Some(v) = t.get_owned(key.as_str()).map_err(backend)? else {
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
        self.check(project.iri())?;
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
        self.check(project.iri())?;
        let all: Vec<GateDef> = self.all_json(GATES)?;
        Ok(all.into_iter().filter(|g| g.project == *project).collect())
    }

    fn update_gate(&self, def: &GateDef) -> Result<(), StoreError> {
        if self.get_gate(&def.id)?.is_none() {
            return Err(StoreError::NoSuchGate(def.id.clone()));
        }
        self.put_json(GATES, def.id.iri(), def)
    }

    fn add_transition(&self, t: Transition) -> Result<(), StoreError> {
        self.check(t.project.iri())?;
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
        self.check(project.iri())?;
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
        self.check(project.iri())?;
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
}

impl Tracker for RedbStore {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        self.check(project.iri())?;
        let id = self.insert_new(Kind::Record, RECORDS, |id| Record {
            id: RecordId(id),
            project: project.clone(),
            title: title.to_string(),
            state: State::Todo,
        })?;
        Ok(RecordId(id))
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.get_json(RECORDS, id.iri())
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        self.check(project.iri())?;
        let all: Vec<Record> = self.all_json(RECORDS)?;
        Ok(all.into_iter().filter(|r| r.project == *project).collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        let mut rec: Record = self
            .get_record(id)?
            .ok_or_else(|| StoreError::NoSuchRecord(id.clone()))?;
        rec.state = state;
        self.put_json(RECORDS, id.iri(), &rec)
    }

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        self.check(finding.project.iri())?;
        self.check(finding.record.iri())?;
        let id = self.insert_new(Kind::Finding, FINDINGS, |id| {
            let mut finding = finding;
            finding.id = FindingId(id);
            finding
        })?;
        Ok(FindingId(id))
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        self.get_json(FINDINGS, id.iri())
    }

    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        if self.get_finding(&finding.id)?.is_none() {
            return Err(StoreError::NoSuchFinding(finding.id.clone()));
        }
        self.put_json(FINDINGS, finding.id.iri(), finding)
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        self.check(project.iri())?;
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
        self.check(project.iri())?;
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
                gate: g.clone(),
                record: None,
                commit: "abc".into(),
                verdict: verdict.clone(),
                population: 3,
                output_excerpt: String::new(),
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

    fn fresh() -> (RedbStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = RedbStore::open(&dir.path().join("t.redb")).unwrap();
        (s, dir)
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
        fl_core::conformance::catalog(fresh);
        fl_core::conformance::tracker(fresh);
        fl_core::conformance::ledger(fresh);
        fl_core::conformance::all_roles(fresh);
    }
}
