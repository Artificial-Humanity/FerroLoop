//! Test doubles shared by the command modules' unit tests.

use fl_core::decision::{Decision, Flushed};
use fl_core::ids::{GateId, ProjectId};
use fl_core::log::{Attempt, GateRun};
use fl_core::store::{Ledger, StoreError};
use std::cell::RefCell;

/// A ledger that keeps every decision it is asked to flush — or, built with
/// [`Flushes::refusing`] or [`Flushes::refusing_with`], refuses every one.
#[derive(Default)]
pub struct Flushes {
    refuse: Option<fn() -> StoreError>,
    pub decisions: RefCell<Vec<Decision>>,
}

fn unreachable() -> StoreError {
    StoreError::Unreachable {
        store: "github:acme/widgets".into(),
        cause: "connection refused".into(),
    }
}

impl Flushes {
    /// Every flush fails as GitHub unreachable.
    pub fn refusing() -> Self {
        Self::refusing_with(unreachable)
    }

    /// Every flush fails with `cause()`.
    pub fn refusing_with(cause: fn() -> StoreError) -> Self {
        Self {
            refuse: Some(cause),
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
        if let Some(cause) = self.refuse {
            return Err(cause());
        }
        self.decisions.borrow_mut().push(decision);
        Ok(Flushed {
            commit: Some("c1".into()),
            left_local: vec![],
        })
    }
}
