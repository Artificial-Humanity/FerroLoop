//! redb persistence for the `fl-core` Store trait.

use fl_core::finding::{Finding, FindingState};
use fl_core::ids::{FindingId, GateId, ProjectId, RecordId};
use fl_core::log::{Attempt, GateRun};
use fl_core::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};
use fl_core::store::{Store, StoreError};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const PROJECTS: TableDefinition<u64, &str> = TableDefinition::new("projects");
const GATES: TableDefinition<u64, &str> = TableDefinition::new("gates");
const TRANSITIONS: TableDefinition<&str, &str> = TableDefinition::new("transitions");
const RECORDS: TableDefinition<u64, &str> = TableDefinition::new("records");
const GATE_RUNS: TableDefinition<u64, &str> = TableDefinition::new("gate_runs");
const ATTEMPTS: TableDefinition<u64, &str> = TableDefinition::new("attempts");
const FINDINGS: TableDefinition<u64, &str> = TableDefinition::new("findings");

const NEXT_ID: &str = "next_id";
const NEXT_RUN: &str = "next_run";
const NEXT_ATTEMPT: &str = "next_attempt";

pub struct RedbStore {
    db: Database,
}

fn backend(e: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

impl RedbStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = Database::create(path).map_err(backend)?;
        // Create every table once so a read on a fresh database does not error.
        let tx = db.begin_write().map_err(backend)?;
        {
            tx.open_table(META).map_err(backend)?;
            tx.open_table(PROJECTS).map_err(backend)?;
            tx.open_table(GATES).map_err(backend)?;
            tx.open_table(TRANSITIONS).map_err(backend)?;
            tx.open_table(RECORDS).map_err(backend)?;
            tx.open_table(GATE_RUNS).map_err(backend)?;
            tx.open_table(ATTEMPTS).map_err(backend)?;
            tx.open_table(FINDINGS).map_err(backend)?;
        }
        tx.commit().map_err(backend)?;
        Ok(Self { db })
    }

    /// Bumps `counter` and writes `build(id)` as JSON into `table` under
    /// that id, both inside **one** write transaction that commits once.
    ///
    /// This is the fix for a real gap: an earlier version of this store
    /// bumped the counter in its own transaction (committing immediately)
    /// and then wrote the row in a second, separate transaction. If the
    /// process died or the row write failed in between, the counter's
    /// commit had already landed — the id was burned permanently with no
    /// row ever written for it. That never violated the "ids are never
    /// reused" rule the trait actually requires, but it is a silent,
    /// unrecoverable side effect riding along on a call that reported
    /// failure. Doing both steps against the same `WriteTransaction` and
    /// committing once means they can only ever land together or not at
    /// all: an error anywhere before `commit()` drops the transaction
    /// (`redb::WriteTransaction`'s `Drop` aborts automatically when it
    /// wasn't completed), which discards the counter bump along with
    /// everything else.
    ///
    /// `Table` (the write-side handle) only offers `get` via the
    /// `ReadableTable` trait — `get_owned` is an inherent method on
    /// `ReadOnlyTable` and does not exist here. `u64`'s `SelfType` is an
    /// owned `u64` regardless, so the borrowed guard is fine to read and
    /// drop within this expression.
    fn bump_and_put<T: serde::Serialize>(
        &self,
        counter: &str,
        table: TableDefinition<u64, &str>,
        build: impl FnOnce(u64) -> T,
    ) -> Result<u64, StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        let id;
        {
            let mut meta = tx.open_table(META).map_err(backend)?;
            let current = meta
                .get(counter)
                .map_err(backend)?
                .map(|v| v.value())
                .unwrap_or(0);
            id = current + 1;
            meta.insert(counter, id).map_err(backend)?;
        }
        let value = build(id);
        let json = serde_json::to_string(&value).map_err(backend)?;
        {
            let mut t = tx.open_table(table).map_err(backend)?;
            t.insert(id, json.as_str()).map_err(backend)?;
        }
        tx.commit().map_err(backend)?;
        Ok(id)
    }

    fn put_json<T: serde::Serialize>(
        &self,
        table: TableDefinition<u64, &str>,
        key: u64,
        value: &T,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(value).map_err(backend)?;
        let tx = self.db.begin_write().map_err(backend)?;
        {
            let mut t = tx.open_table(table).map_err(backend)?;
            t.insert(key, json.as_str()).map_err(backend)?;
        }
        tx.commit().map_err(backend)?;
        Ok(())
    }

    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        table: TableDefinition<u64, &str>,
        key: u64,
    ) -> Result<Option<T>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let t = tx.open_table(table).map_err(backend)?;
        // `ReadOnlyTable::get_owned` (unlike `Table::get`) keeps the read
        // transaction alive via a reference-counted guard, so the returned
        // value can outlive the local borrow of `t`.
        let Some(v) = t.get_owned(key).map_err(backend)? else {
            return Ok(None);
        };
        let parsed = serde_json::from_str(v.value()).map_err(backend)?;
        Ok(Some(parsed))
    }

    fn all_json<T: serde::de::DeserializeOwned>(
        &self,
        table: TableDefinition<u64, &str>,
    ) -> Result<Vec<T>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let t = tx.open_table(table).map_err(backend)?;
        let mut out = Vec::new();
        for entry in t.iter().map_err(backend)? {
            let (_k, v) = entry.map_err(backend)?;
            out.push(serde_json::from_str(v.value()).map_err(backend)?);
        }
        Ok(out)
    }
}

impl Store for RedbStore {
    fn add_project(&mut self, root: &str) -> Result<ProjectId, StoreError> {
        let id = self.bump_and_put(NEXT_ID, PROJECTS, |id| Project {
            id: ProjectId(id),
            root: root.to_string(),
        })?;
        Ok(ProjectId(id))
    }

    fn get_project(&self, id: ProjectId) -> Result<Option<Project>, StoreError> {
        self.get_json(PROJECTS, id.0)
    }

    fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        self.all_json(PROJECTS)
    }

    #[allow(clippy::too_many_arguments)]
    fn add_gate(
        &mut self,
        project: ProjectId,
        name: &str,
        kind: GateKind,
        selector: Selector,
        min_population: u64,
        authored_at_commit: &str,
        authored_by: &str,
    ) -> Result<GateId, StoreError> {
        let id = self.bump_and_put(NEXT_ID, GATES, |id| GateDef {
            id: GateId(id),
            project,
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

    fn get_gate(&self, id: GateId) -> Result<Option<GateDef>, StoreError> {
        self.get_json(GATES, id.0)
    }

    fn list_gates(&self, project: ProjectId) -> Result<Vec<GateDef>, StoreError> {
        let all: Vec<GateDef> = self.all_json(GATES)?;
        Ok(all.into_iter().filter(|g| g.project == project).collect())
    }

    fn update_gate(&mut self, def: &GateDef) -> Result<(), StoreError> {
        if self.get_gate(def.id)?.is_none() {
            return Err(StoreError::NoSuchGate(def.id));
        }
        self.put_json(GATES, def.id.0, def)
    }

    fn add_transition(&mut self, t: Transition) -> Result<(), StoreError> {
        let key = format!("{}:{}", t.project.0, t.name);
        let json = serde_json::to_string(&t).map_err(backend)?;
        let tx = self.db.begin_write().map_err(backend)?;
        {
            let mut table = tx.open_table(TRANSITIONS).map_err(backend)?;
            table.insert(key.as_str(), json.as_str()).map_err(backend)?;
        }
        tx.commit().map_err(backend)?;
        Ok(())
    }

    fn get_transition(
        &self,
        project: ProjectId,
        name: &str,
    ) -> Result<Option<Transition>, StoreError> {
        let key = format!("{}:{}", project.0, name);
        let tx = self.db.begin_read().map_err(backend)?;
        let table = tx.open_table(TRANSITIONS).map_err(backend)?;
        let Some(v) = table.get_owned(key.as_str()).map_err(backend)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_str(v.value()).map_err(backend)?))
    }

    fn add_record(&mut self, project: ProjectId, title: &str) -> Result<RecordId, StoreError> {
        let id = self.bump_and_put(NEXT_ID, RECORDS, |id| Record {
            id: RecordId(id),
            project,
            title: title.to_string(),
            state: State::Todo,
        })?;
        Ok(RecordId(id))
    }

    fn get_record(&self, id: RecordId) -> Result<Option<Record>, StoreError> {
        self.get_json(RECORDS, id.0)
    }

    fn list_records(&self, project: ProjectId) -> Result<Vec<Record>, StoreError> {
        let all: Vec<Record> = self.all_json(RECORDS)?;
        Ok(all.into_iter().filter(|r| r.project == project).collect())
    }

    fn set_record_state(&mut self, id: RecordId, state: State) -> Result<(), StoreError> {
        let mut rec: Record = self.get_record(id)?.ok_or(StoreError::NoSuchRecord(id))?;
        rec.state = state;
        self.put_json(RECORDS, id.0, &rec)
    }

    fn append_gate_run(&mut self, run: GateRun) -> Result<(), StoreError> {
        self.bump_and_put(NEXT_RUN, GATE_RUNS, |_seq| run)?;
        Ok(())
    }

    fn append_attempt(&mut self, attempt: Attempt) -> Result<(), StoreError> {
        self.bump_and_put(NEXT_ATTEMPT, ATTEMPTS, |_seq| attempt)?;
        Ok(())
    }

    fn gate_runs(&self, gate: GateId) -> Result<Vec<GateRun>, StoreError> {
        let all: Vec<GateRun> = self.all_json(GATE_RUNS)?;
        Ok(all.into_iter().filter(|r| r.gate == gate).collect())
    }

    fn attempts(&self, project: ProjectId) -> Result<Vec<Attempt>, StoreError> {
        let all: Vec<Attempt> = self.all_json(ATTEMPTS)?;
        Ok(all.into_iter().filter(|a| a.project == project).collect())
    }

    fn add_finding(&mut self, finding: Finding) -> Result<FindingId, StoreError> {
        let id = self.bump_and_put(NEXT_ID, FINDINGS, |id| {
            let mut finding = finding;
            finding.id = FindingId(id);
            finding
        })?;
        Ok(FindingId(id))
    }

    fn get_finding(&self, id: FindingId) -> Result<Option<Finding>, StoreError> {
        self.get_json(FINDINGS, id.0)
    }

    fn update_finding(&mut self, finding: &Finding) -> Result<(), StoreError> {
        if self.get_finding(finding.id)?.is_none() {
            return Err(StoreError::NoSuchFinding(finding.id));
        }
        self.put_json(FINDINGS, finding.id.0, finding)
    }

    fn list_findings(&self, project: ProjectId) -> Result<Vec<Finding>, StoreError> {
        let all: Vec<Finding> = self.all_json(FINDINGS)?;
        Ok(all.into_iter().filter(|f| f.project == project).collect())
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        let all: Vec<Finding> = self.all_json(FINDINGS)?;
        Ok(all
            .into_iter()
            .filter(|f| f.raised_by == actor && f.state == FindingState::Withdrawn)
            .count() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::log::GateRun;
    use fl_core::model::State;
    use fl_core::verdict::Verdict;

    #[test]
    fn a_project_survives_a_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");

        let id = {
            let mut s = RedbStore::open(&path).unwrap();
            s.add_project("/tmp/p").unwrap()
        };

        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.get_project(id).unwrap().unwrap().root, "/tmp/p");
    }

    #[test]
    fn the_id_counter_survives_a_reopen_so_ids_are_never_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");

        let first = {
            let mut s = RedbStore::open(&path).unwrap();
            s.add_project("/a").unwrap()
        };
        let second = {
            let mut s = RedbStore::open(&path).unwrap();
            s.add_project("/b").unwrap()
        };
        assert_eq!(second.get(), first.get() + 1);
    }

    #[test]
    fn a_record_state_change_is_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let (p, r) = {
            let mut s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let r = s.add_record(p, "t").unwrap();
            s.set_record_state(r, State::Review).unwrap();
            (p, r)
        };
        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.get_record(r).unwrap().unwrap().state, State::Review);
        assert_eq!(s.get_project(p).unwrap().unwrap().id, p);
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
            let mut s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let g = s
                .add_gate(
                    p,
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
                gate: g,
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
        let runs = s.gate_runs(g).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].verdict, verdict);
        assert_eq!(runs[0].verdict.population(), Some(3));
    }

    /// Fix round 1: pins that the counter bump and the row write inside
    /// `bump_and_put` commit as one unit. If they didn't, a failed insert
    /// would still leave the counter advanced — a burned id with no row.
    ///
    /// There is no seam in the public `Store` API to make the *second*
    /// half of a normal insert fail deterministically and cheaply: every
    /// concrete type this store serializes (`Project`, `GateDef`, `Record`,
    /// `GateRun`, `Attempt`) always serializes via `serde_json` without
    /// error, and redb's own size ceiling (`MAX_VALUE_LENGTH`, 3 GiB) is
    /// too large to hit in a fast test. So this test reaches for the same
    /// private field `RedbStore::open` itself would build (`db`, visible to
    /// this module) and pre-corrupts the on-disk `gates` table with a
    /// mismatched value type *before* constructing the store — bypassing
    /// `RedbStore::open`, which eagerly opens every table itself and would
    /// fail immediately if it went through the normal path. This makes
    /// `add_gate`'s meta-counter bump succeed and its second `open_table`
    /// call fail with a genuine `redb::TableTypeMismatch`, inside the same
    /// still-uncommitted write transaction — the same commit boundary a
    /// real row-write failure would cross.
    #[test]
    fn a_failed_insert_does_not_advance_the_shared_id_counter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");

        let db = redb::Database::create(&path).unwrap();
        {
            // Same table name as `GATES`, wrong value type. Opening it later
            // under the real `GATES` definition (`TableDefinition<u64,
            // &str>`) will fail with `TableTypeMismatch`, not silently
            // coerce.
            const WRONG_GATES: TableDefinition<u64, u64> = TableDefinition::new("gates");
            let tx = db.begin_write().unwrap();
            {
                tx.open_table(WRONG_GATES).unwrap();
            }
            tx.commit().unwrap();
        }

        // Bypasses `RedbStore::open` deliberately: it would try to open the
        // real `GATES` definition itself and fail right there, before we
        // ever get to call `add_gate`.
        let mut store = RedbStore { db };

        let failed = store.add_gate(
            ProjectId(1),
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

        // `add_project` shares the same `next_id` counter and writes to an
        // unrelated, correctly-typed table. If the failed `add_gate` call
        // above had left its counter bump committed, this would come back
        // as 2, not 1.
        let id = store.add_project("/p").unwrap();
        assert_eq!(id.get(), 1, "a failed insert must not burn an id");
    }

    #[test]
    fn a_finding_survives_a_close_and_reopen_with_a_real_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");

        let id = {
            let mut s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let r = s.add_record(p, "t").unwrap();
            s.add_finding(Finding::raise(p, r, "reviewer", "wrong on empty"))
                .unwrap()
        };
        assert_ne!(id.get(), 0, "the store must replace the placeholder id");

        let s = RedbStore::open(&path).unwrap();
        let back = s.get_finding(id).unwrap().unwrap();
        assert_eq!(back.id, id);
        assert_eq!(back.state, FindingState::Raised);
    }

    #[test]
    fn withdrawals_are_counted_against_whoever_raised_the_finding_and_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");

        {
            let mut s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let r = s.add_record(p, "t").unwrap();

            for claim in ["a", "b"] {
                let id = s.add_finding(Finding::raise(p, r, "hasty", claim)).unwrap();
                let mut f = s.get_finding(id).unwrap().unwrap();
                f.withdraw("not concrete").unwrap();
                s.update_finding(&f).unwrap();
            }
            let id = s.add_finding(Finding::raise(p, r, "careful", "c")).unwrap();
            let mut f = s.get_finding(id).unwrap().unwrap();
            f.attach_reproduction(GateId(1)).unwrap();
            s.update_finding(&f).unwrap();
        }

        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.withdrawals_by("hasty").unwrap(), 2);
        assert_eq!(s.withdrawals_by("careful").unwrap(), 0);
        assert_eq!(s.withdrawals_by("nobody").unwrap(), 0);
    }
}
