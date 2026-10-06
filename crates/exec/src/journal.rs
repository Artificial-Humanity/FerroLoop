//! A tracker and ledger over a `MemStore` that writes down the order of
//! every flush and every state change, and can refuse the flush — for the
//! evidence-before-state tests (GitHub ledger spec §2.2, §8.3).

use fl_core::MemStore;
use fl_core::decision::{Decision, Flushed};
use fl_core::finding::Finding;
use fl_core::ids::{FindingId, GateId, ProjectId, RecordId};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use fl_core::model::{Record, State};
use fl_core::store::{Ledger, Roles, StoreError, Tracker};
use std::cell::RefCell;

pub struct Journal<'a> {
    pub store: &'a MemStore,
    events: RefCell<Vec<&'static str>>,
    decisions: RefCell<Vec<Decision>>,
    /// What every flush fails with, when flushes fail.
    refuse_flush: Option<fn() -> StoreError>,
}

fn unreachable() -> StoreError {
    StoreError::Unreachable {
        store: "github:acme/widgets".into(),
        cause: "connection refused".into(),
    }
}

impl<'a> Journal<'a> {
    pub fn new(store: &'a MemStore) -> Self {
        Self {
            store,
            events: RefCell::new(vec![]),
            decisions: RefCell::new(vec![]),
            refuse_flush: None,
        }
    }

    /// Every flush fails as GitHub unreachable.
    pub fn refusing(store: &'a MemStore) -> Self {
        Self::refusing_with(store, unreachable)
    }

    /// Every flush fails with `cause()`.
    pub fn refusing_with(store: &'a MemStore, cause: fn() -> StoreError) -> Self {
        Self {
            refuse_flush: Some(cause),
            ..Self::new(store)
        }
    }

    /// The store as the catalog; this journal as the tracker and the ledger.
    pub fn roles(&self) -> Roles<'_> {
        Roles {
            catalog: self.store,
            tracker: self,
            ledger: self,
        }
    }

    pub fn events(&self) -> Vec<&'static str> {
        self.events.borrow().clone()
    }

    pub fn decisions(&self) -> Vec<Decision> {
        self.decisions.borrow().clone()
    }
}

impl Ledger for Journal<'_> {
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError> {
        self.store.append_gate_run(run)
    }
    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError> {
        self.store.append_attempt(attempt)
    }
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        self.store.gate_runs(gate)
    }
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        self.store.attempts(project)
    }
    fn flush(&self, decision: Decision) -> Result<Flushed, StoreError> {
        self.events.borrow_mut().push("flush");
        if let Some(cause) = self.refuse_flush {
            return Err(cause());
        }
        self.decisions.borrow_mut().push(decision);
        Ok(Flushed {
            commit: Some("c1".into()),
            left_local: vec![],
        })
    }
}

impl Tracker for Journal<'_> {
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError> {
        self.store.add_record_with_area(project, title, area)
    }
    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.store.get_record(id)
    }
    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        self.store.list_records(project)
    }
    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        self.events.borrow_mut().push("set_record_state");
        self.store.set_record_state(id, state)
    }
    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        self.store.add_finding(finding)
    }
    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        self.store.get_finding(id)
    }
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        self.events.borrow_mut().push("update_finding");
        self.store.update_finding(finding)
    }
    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        self.store.list_findings(project)
    }
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        self.store.withdrawals_by(actor)
    }
    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        self.store.add_alias(primary, alias)
    }
}
