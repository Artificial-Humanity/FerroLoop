use crate::finding::{Finding, FindingState};
use crate::ids::{FindingId, GateId, ProjectId, RecordId};
use crate::log::{Attempt, GateRun};
use crate::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};
use crate::store::{Catalog, Ledger, StoreError, Tracker};
use std::cell::RefCell;
use std::collections::BTreeMap;

/// An in-memory store for tests. Deliberately lives in `fl-core` so the
/// engine's own tests need no backend at all. Backs all three roles.
#[derive(Default)]
pub struct MemStore {
    inner: RefCell<Inner>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    projects: BTreeMap<u64, Project>,
    gates: BTreeMap<u64, GateDef>,
    transitions: BTreeMap<(u64, String), Transition>,
    records: BTreeMap<u64, Record>,
    runs: Vec<GateRun>,
    attempts: Vec<Attempt>,
    findings: BTreeMap<u64, Finding>,
}

impl Inner {
    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

impl Catalog for MemStore {
    fn add_project(&self, root: &str) -> Result<ProjectId, StoreError> {
        let mut s = self.inner.borrow_mut();
        let id = ProjectId(s.next());
        s.projects.insert(
            id.0,
            Project {
                id,
                root: root.to_string(),
            },
        );
        Ok(id)
    }

    fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError> {
        Ok(self.inner.borrow().projects.get(&id.0).cloned())
    }

    fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        Ok(self.inner.borrow().projects.values().cloned().collect())
    }

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
        let mut s = self.inner.borrow_mut();
        let id = GateId(s.next());
        s.gates.insert(
            id.0,
            GateDef {
                id,
                project: *project,
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

    fn get_gate(&self, id: &GateId) -> Result<Option<GateDef>, StoreError> {
        Ok(self.inner.borrow().gates.get(&id.0).cloned())
    }

    fn list_gates(&self, project: &ProjectId) -> Result<Vec<GateDef>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .gates
            .values()
            .filter(|g| g.project == *project)
            .cloned()
            .collect())
    }

    fn update_gate(&self, def: &GateDef) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        if !s.gates.contains_key(&def.id.0) {
            return Err(StoreError::NoSuchGate(def.id));
        }
        s.gates.insert(def.id.0, def.clone());
        Ok(())
    }

    fn add_transition(&self, t: Transition) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.transitions.insert((t.project.0, t.name.clone()), t);
        Ok(())
    }

    fn get_transition(
        &self,
        project: &ProjectId,
        name: &str,
    ) -> Result<Option<Transition>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .transitions
            .get(&(project.0, name.to_string()))
            .cloned())
    }

    fn list_transitions(&self, project: &ProjectId) -> Result<Vec<Transition>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .transitions
            .values()
            .filter(|t| t.project == *project)
            .cloned()
            .collect())
    }
}

impl Tracker for MemStore {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        let mut s = self.inner.borrow_mut();
        let id = RecordId(s.next());
        s.records.insert(
            id.0,
            Record {
                id,
                project: *project,
                title: title.to_string(),
                state: State::Todo,
            },
        );
        Ok(id)
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        Ok(self.inner.borrow().records.get(&id.0).cloned())
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .records
            .values()
            .filter(|r| r.project == *project)
            .cloned()
            .collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        let rec = s
            .records
            .get_mut(&id.0)
            .ok_or(StoreError::NoSuchRecord(*id))?;
        rec.state = state;
        Ok(())
    }

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        let mut s = self.inner.borrow_mut();
        let id = FindingId(s.next());
        let mut finding = finding;
        finding.id = id;
        s.findings.insert(id.0, finding);
        Ok(id)
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        Ok(self.inner.borrow().findings.get(&id.0).cloned())
    }

    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        if !s.findings.contains_key(&finding.id.0) {
            return Err(StoreError::NoSuchFinding(finding.id));
        }
        s.findings.insert(finding.id.0, finding.clone());
        Ok(())
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .findings
            .values()
            .filter(|f| f.project == *project)
            .cloned()
            .collect())
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        Ok(self
            .inner
            .borrow()
            .findings
            .values()
            .filter(|f| f.raised_by == actor && f.state == FindingState::Withdrawn)
            .count() as u64)
    }
}

impl Ledger for MemStore {
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError> {
        self.inner.borrow_mut().runs.push(run);
        Ok(())
    }

    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError> {
        self.inner.borrow_mut().attempts.push(attempt);
        Ok(())
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .runs
            .iter()
            .filter(|r| r.gate == *gate)
            .cloned()
            .collect())
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .attempts
            .iter()
            .filter(|a| a.project == *project)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_store_meets_every_role_contract() {
        crate::conformance::catalog(|| (MemStore::default(), ()));
        crate::conformance::tracker(|| (MemStore::default(), ()));
        crate::conformance::ledger(|| (MemStore::default(), ()));
        crate::conformance::all_roles(|| (MemStore::default(), ()));
    }

    #[test]
    fn ids_are_handed_out_in_sequence_and_never_reused() {
        let s = MemStore::default();
        let a = s.add_project("/a").unwrap();
        let b = s.add_project("/b").unwrap();
        assert_ne!(a, b);
        assert_eq!(b.get(), a.get() + 1);
    }

    #[test]
    fn a_missing_project_is_none_and_not_an_error() {
        let s = MemStore::default();
        assert!(s.get_project(&ProjectId(42)).unwrap().is_none());
    }
}
