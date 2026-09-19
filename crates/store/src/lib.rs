//! redb persistence for the `fl-core` Store trait.

use fl_core::ids::{GateId, ProjectId, RecordId};
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
        }
        tx.commit().map_err(backend)?;
        Ok(Self { db })
    }

    /// Atomically increments and returns a named counter. Used both for the
    /// entity id sequence and for the append-only log sequences, so that a
    /// reopened database never reuses a number it already handed out.
    fn bump(&self, counter: &str) -> Result<u64, StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        let next;
        {
            let mut t = tx.open_table(META).map_err(backend)?;
            // `Table` (the write-side handle) only offers `get` via the
            // `ReadableTable` trait — `get_owned` is an inherent method on
            // `ReadOnlyTable` and does not exist here. `u64`'s `SelfType` is
            // an owned `u64` regardless, so the borrowed guard is fine to
            // read and drop within this expression.
            let current = t.get(counter).map_err(backend)?.map(|v| v.value()).unwrap_or(0);
            next = current + 1;
            t.insert(counter, next).map_err(backend)?;
        }
        tx.commit().map_err(backend)?;
        Ok(next)
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
        let id = ProjectId(self.bump(NEXT_ID)?);
        self.put_json(PROJECTS, id.0, &Project { id, root: root.to_string() })?;
        Ok(id)
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
        let id = GateId(self.bump(NEXT_ID)?);
        let def = GateDef {
            id,
            project,
            name: name.to_string(),
            kind,
            selector,
            min_population,
            authored_at_commit: authored_at_commit.to_string(),
            authored_by: authored_by.to_string(),
            last_pass_commit: None,
        };
        self.put_json(GATES, id.0, &def)?;
        Ok(id)
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
        let id = RecordId(self.bump(NEXT_ID)?);
        let rec = Record { id, project, title: title.to_string(), state: State::Todo };
        self.put_json(RECORDS, id.0, &rec)?;
        Ok(id)
    }

    fn get_record(&self, id: RecordId) -> Result<Option<Record>, StoreError> {
        self.get_json(RECORDS, id.0)
    }

    fn set_record_state(&mut self, id: RecordId, state: State) -> Result<(), StoreError> {
        let mut rec: Record = self.get_record(id)?.ok_or(StoreError::NoSuchRecord(id))?;
        rec.state = state;
        self.put_json(RECORDS, id.0, &rec)
    }

    fn append_gate_run(&mut self, run: GateRun) -> Result<(), StoreError> {
        let seq = self.bump(NEXT_RUN)?;
        self.put_json(GATE_RUNS, seq, &run)
    }

    fn append_attempt(&mut self, attempt: Attempt) -> Result<(), StoreError> {
        let seq = self.bump(NEXT_ATTEMPT)?;
        self.put_json(ATTEMPTS, seq, &attempt)
    }

    fn gate_runs(&self, gate: GateId) -> Result<Vec<GateRun>, StoreError> {
        let all: Vec<GateRun> = self.all_json(GATE_RUNS)?;
        Ok(all.into_iter().filter(|r| r.gate == gate).collect())
    }

    fn attempts(&self, project: ProjectId) -> Result<Vec<Attempt>, StoreError> {
        let all: Vec<Attempt> = self.all_json(ATTEMPTS)?;
        Ok(all.into_iter().filter(|a| a.project == project).collect())
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
                    fl_core::model::Selector::Glob { pattern: "**/*.rs".into() },
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
}
