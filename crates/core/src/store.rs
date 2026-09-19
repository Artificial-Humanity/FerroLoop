use crate::ids::{GateId, ProjectId, RecordId};
use crate::log::{Attempt, GateRun};
use crate::model::{
    GateDef, GateKind, Project, Record, Selector, State, Transition,
};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no such gate: {0}")]
    NoSuchGate(GateId),
    #[error("no such record: {0}")]
    NoSuchRecord(RecordId),
    #[error("backend failure: {0}")]
    Backend(String),
}

/// Everything the engine needs from persistence. `fl-core` is generic over
/// this and names no engine.
///
/// ⚠ There is no method that stores a population, and adding one would break
/// the design. A population is enumerated fresh from the working tree at run
/// time so it cannot go stale in the store.
pub trait Store {
    fn add_project(&mut self, root: &str) -> Result<ProjectId, StoreError>;
    fn get_project(&self, id: ProjectId) -> Result<Option<Project>, StoreError>;
    fn list_projects(&self) -> Result<Vec<Project>, StoreError>;

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
    ) -> Result<GateId, StoreError>;
    fn get_gate(&self, id: GateId) -> Result<Option<GateDef>, StoreError>;
    fn list_gates(&self, project: ProjectId) -> Result<Vec<GateDef>, StoreError>;
    fn update_gate(&mut self, def: &GateDef) -> Result<(), StoreError>;

    fn add_transition(&mut self, t: Transition) -> Result<(), StoreError>;
    fn get_transition(
        &self,
        project: ProjectId,
        name: &str,
    ) -> Result<Option<Transition>, StoreError>;

    fn add_record(&mut self, project: ProjectId, title: &str) -> Result<RecordId, StoreError>;
    fn get_record(&self, id: RecordId) -> Result<Option<Record>, StoreError>;
    fn set_record_state(&mut self, id: RecordId, state: State) -> Result<(), StoreError>;

    fn append_gate_run(&mut self, run: GateRun) -> Result<(), StoreError>;
    fn append_attempt(&mut self, attempt: Attempt) -> Result<(), StoreError>;
    fn gate_runs(&self, gate: GateId) -> Result<Vec<GateRun>, StoreError>;
    fn attempts(&self, project: ProjectId) -> Result<Vec<Attempt>, StoreError>;
}

/// An in-memory `Store` for tests. Deliberately lives in `fl-core` so the
/// engine's own tests need no backend at all.
#[derive(Default)]
pub struct MemStore {
    next_id: u64,
    projects: BTreeMap<u64, Project>,
    gates: BTreeMap<u64, GateDef>,
    transitions: BTreeMap<(u64, String), Transition>,
    records: BTreeMap<u64, Record>,
    runs: Vec<GateRun>,
    attempts: Vec<Attempt>,
}

impl MemStore {
    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

impl Store for MemStore {
    fn add_project(&mut self, root: &str) -> Result<ProjectId, StoreError> {
        let id = ProjectId(self.next());
        self.projects.insert(id.0, Project { id, root: root.to_string() });
        Ok(id)
    }

    fn get_project(&self, id: ProjectId) -> Result<Option<Project>, StoreError> {
        Ok(self.projects.get(&id.0).cloned())
    }

    fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        Ok(self.projects.values().cloned().collect())
    }

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
        let id = GateId(self.next());
        self.gates.insert(
            id.0,
            GateDef {
                id,
                project,
                name: name.to_string(),
                kind,
                selector,
                min_population,
                authored_at_commit: authored_at_commit.to_string(),
                authored_by: authored_by.to_string(),
                last_pass_commit: None,
            },
        );
        Ok(id)
    }

    fn get_gate(&self, id: GateId) -> Result<Option<GateDef>, StoreError> {
        Ok(self.gates.get(&id.0).cloned())
    }

    fn list_gates(&self, project: ProjectId) -> Result<Vec<GateDef>, StoreError> {
        Ok(self.gates.values().filter(|g| g.project == project).cloned().collect())
    }

    fn update_gate(&mut self, def: &GateDef) -> Result<(), StoreError> {
        if !self.gates.contains_key(&def.id.0) {
            return Err(StoreError::NoSuchGate(def.id));
        }
        self.gates.insert(def.id.0, def.clone());
        Ok(())
    }

    fn add_transition(&mut self, t: Transition) -> Result<(), StoreError> {
        self.transitions.insert((t.project.0, t.name.clone()), t);
        Ok(())
    }

    fn get_transition(
        &self,
        project: ProjectId,
        name: &str,
    ) -> Result<Option<Transition>, StoreError> {
        Ok(self.transitions.get(&(project.0, name.to_string())).cloned())
    }

    fn add_record(&mut self, project: ProjectId, title: &str) -> Result<RecordId, StoreError> {
        let id = RecordId(self.next());
        self.records.insert(
            id.0,
            Record { id, project, title: title.to_string(), state: State::Todo },
        );
        Ok(id)
    }

    fn get_record(&self, id: RecordId) -> Result<Option<Record>, StoreError> {
        Ok(self.records.get(&id.0).cloned())
    }

    fn set_record_state(&mut self, id: RecordId, state: State) -> Result<(), StoreError> {
        let rec = self.records.get_mut(&id.0).ok_or(StoreError::NoSuchRecord(id))?;
        rec.state = state;
        Ok(())
    }

    fn append_gate_run(&mut self, run: GateRun) -> Result<(), StoreError> {
        self.runs.push(run);
        Ok(())
    }

    fn append_attempt(&mut self, attempt: Attempt) -> Result<(), StoreError> {
        self.attempts.push(attempt);
        Ok(())
    }

    fn gate_runs(&self, gate: GateId) -> Result<Vec<GateRun>, StoreError> {
        Ok(self.runs.iter().filter(|r| r.gate == gate).cloned().collect())
    }

    fn attempts(&self, project: ProjectId) -> Result<Vec<Attempt>, StoreError> {
        Ok(self.attempts.iter().filter(|a| a.project == project).cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommandSpec, PopulationDelivery};
    use crate::verdict::Verdict;

    fn project(store: &mut impl Store) -> ProjectId {
        store.add_project("/tmp/p").unwrap()
    }

    #[test]
    fn a_project_round_trips() {
        let mut s = MemStore::default();
        let id = project(&mut s);
        assert_eq!(s.get_project(id).unwrap().unwrap().root, "/tmp/p");
        assert_eq!(s.list_projects().unwrap().len(), 1);
    }

    #[test]
    fn ids_are_handed_out_in_sequence_and_never_reused() {
        let mut s = MemStore::default();
        let a = s.add_project("/a").unwrap();
        let b = s.add_project("/b").unwrap();
        assert_ne!(a, b);
        assert_eq!(b.get(), a.get() + 1);
    }

    #[test]
    fn a_missing_project_is_none_and_not_an_error() {
        let s = MemStore::default();
        assert!(s.get_project(ProjectId(42)).unwrap().is_none());
    }

    #[test]
    fn a_record_state_change_is_visible_on_the_next_read() {
        let mut s = MemStore::default();
        let p = project(&mut s);
        let r = s.add_record(p, "fix the thing").unwrap();
        s.set_record_state(r, State::Doing).unwrap();
        assert_eq!(s.get_record(r).unwrap().unwrap().state, State::Doing);
    }

    #[test]
    fn the_logs_are_append_only_and_read_back_in_order() {
        let mut s = MemStore::default();
        let p = project(&mut s);
        let g = s
            .add_gate(p, "fmt", sample_kind(), sample_selector(), 1, "abc", "owner")
            .unwrap();
        s.append_gate_run(sample_run(g, "abc", 3)).unwrap();
        s.append_gate_run(sample_run(g, "def", 5)).unwrap();
        let runs = s.gate_runs(g).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].commit, "abc");
        assert_eq!(runs[1].population, 5);
    }

    #[test]
    fn affirming_a_gate_moves_only_its_stamp() {
        let mut s = MemStore::default();
        let p = project(&mut s);
        let g = s
            .add_gate(p, "fmt", sample_kind(), sample_selector(), 1, "abc", "owner")
            .unwrap();
        let mut def = s.get_gate(g).unwrap().unwrap();
        def.authored_at_commit = "def".into();
        s.update_gate(&def).unwrap();
        let back = s.get_gate(g).unwrap().unwrap();
        assert_eq!(back.authored_at_commit, "def");
        assert_eq!(back.name, "fmt");
    }

    fn sample_kind() -> GateKind {
        GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        })
    }

    fn sample_selector() -> Selector {
        Selector::Glob { pattern: "**/*.rs".into() }
    }

    fn sample_run(gate: GateId, commit: &str, population: u64) -> GateRun {
        GateRun {
            gate,
            record: None,
            commit: commit.into(),
            verdict: Verdict::from_predicate(true, population),
            population,
            output_excerpt: String::new(),
            duration_ms: 1,
            cost_usd_micros: 0,
        }
    }
}
