//! Test doubles shared by the command modules' unit tests.

use fl_core::decision::{Decision, Flushed};
use fl_core::ids::{GateId, ProjectId};
use fl_core::log::{Attempt, GateRun};
use fl_core::store::{Ledger, StoreError};
use std::cell::RefCell;

/// A ledger that keeps every decision it is asked to flush — or, built with
/// [`Flushes::refusing`], refuses every one as unreachable.
#[derive(Default)]
pub struct Flushes {
    refuse: bool,
    pub decisions: RefCell<Vec<Decision>>,
}

impl Flushes {
    pub fn refusing() -> Self {
        Self {
            refuse: true,
            ..Self::default()
        }
    }
}

impl Ledger for Flushes {
    fn append_gate_run(&self, _: GateRun) -> Result<(), StoreError> {
        Ok(())
    }
    fn append_attempt(&self, _: Attempt) -> Result<(), StoreError> {
        Ok(())
    }
    fn gate_runs(&self, _: &GateId) -> Result<Vec<GateRun>, StoreError> {
        Ok(vec![])
    }
    fn attempts(&self, _: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        Ok(vec![])
    }
    fn flush(&self, decision: Decision) -> Result<Flushed, StoreError> {
        if self.refuse {
            return Err(StoreError::Unreachable {
                store: "github:acme/widgets".into(),
                cause: "connection refused".into(),
            });
        }
        self.decisions.borrow_mut().push(decision);
        Ok(Flushed {
            commit: Some("c1".into()),
            left_local: vec![],
        })
    }
}
