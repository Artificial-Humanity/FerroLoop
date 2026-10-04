//! Test doubles shared by the command modules' unit tests.

use fl_core::decision::{Decision, Flushed};
use fl_core::ids::{GateId, ProjectId};
use fl_core::log::{Attempt, GateRun};
use fl_core::store::{Ledger, StoreError};
use std::cell::RefCell;

/// A ledger that keeps every decision it is asked to flush — or, built with
/// [`Flushes::refusing`] or [`Flushes::refusing_with`], refuses every one;
/// built with [`Flushes::failing_append`], fails every attempt's save;
/// built with [`Flushes::publishing_nothing`], lands every flush and names
/// no commit.
#[derive(Default)]
pub struct Flushes {
    refuse: Option<fn() -> StoreError>,
    fail_append: bool,
    no_commit: bool,
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

    /// Every attempt's local save fails, as a full disk would.
    pub fn failing_append() -> Self {
        Self {
            fail_append: true,
            ..Self::default()
        }
    }

    /// Every flush lands and names no commit: it published nothing.
    pub fn publishing_nothing() -> Self {
        Self {
            no_commit: true,
            ..Self::default()
        }
    }
}

impl Ledger for Flushes {
    fn append_gate_run(&self, _: GateRun) -> Result<(), StoreError> {
        Ok(())
    }
    fn append_attempt(&self, _: Attempt) -> Result<(), StoreError> {
        if self.fail_append {
            return Err(StoreError::Backend("the disk is full.".into()));
        }
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
            commit: (!self.no_commit).then(|| "c1".into()),
            left_local: vec![],
        })
    }
}
