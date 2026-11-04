//! Transactional routine and intent control ledger.
//!
//! A backend must load and commit `ControlLedger` in a single native write transaction.
//! The caller authenticates authority, verifies proofs and resolves external resources;
//! this layer rechecks its persisted inputs and does not itself dispatch any work.
//! Typed intent/member result records still require additional store operations.
use crate::routine::{FiringRecord, FiringState, RoutineSpec, RoutineState, firing_key};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// Maximum encoded control state, leaving headroom for backend framing.
pub const MAX_CONTROL_LEDGER_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    #[error("invalid input: {0}")]
    Invalid(&'static str),
    #[error("record is missing: {0}")]
    Missing(&'static str),
    #[error("transaction conflict: {0}")]
    Conflict(&'static str),
    #[error("budget exhausted")]
    Budget,
    #[error("effect outcome is unresolved")]
    Ambiguous,
    #[error("backend unavailable: {0}")]
    Backend(String),
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Funds {
    pub tokens: u64,
    pub cost_micros: u64,
}
impl Funds {
    fn nonzero(self) -> bool {
        self.tokens > 0 && self.cost_micros > 0
    }
    fn add(self, b: Self) -> Result<Self, ControlError> {
        Ok(Self {
            tokens: self
                .tokens
                .checked_add(b.tokens)
                .ok_or(ControlError::Budget)?,
            cost_micros: self
                .cost_micros
                .checked_add(b.cost_micros)
                .ok_or(ControlError::Budget)?,
        })
    }
    fn sub(self, b: Self) -> Result<Self, ControlError> {
        Ok(Self {
            tokens: self
                .tokens
                .checked_sub(b.tokens)
                .ok_or(ControlError::Budget)?,
            cost_micros: self
                .cost_micros
                .checked_sub(b.cost_micros)
                .ok_or(ControlError::Budget)?,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetWindow {
    pub id: String,
    pub routine_id: String,
    pub capacity: Funds,
    pub remaining: Funds,
    pub period: String,
    pub expires_at: String,
    pub expired_credit: Funds,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationRecord {
    pub id: String,
    pub parent_id: Option<String>,
    pub root_id: String,
    pub window_id: String,
    pub settlement_id: String,
    pub total: Funds,
    pub remaining: Funds,
    pub deadline_utc: String,
    pub settled: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionReservation {
    pub id: String,
    pub allocation_id: String,
    pub settlement_id: String,
    pub amount: Funds,
    pub resolution: Option<Resolution>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    NeverDispatched,
    Measured(Funds),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TicketState {
    Issued,
    Accepted,
    Revoked,
    TombstonedUnstarted,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchAuth {
    pub routine_id: String,
    pub revision: u64,
    pub firing_key: String,
    pub scheduler_epoch: u64,
    pub controller: Option<(String, u64)>,
    pub host_spawn_token: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxRecord {
    pub id: String,
    pub reservation_id: String,
    pub proof_record_id: String,
    pub auth: DispatchAuth,
    pub deadline_utc: String,
    pub ticket: TicketState,
    /// Opaque authenticated host/controller acknowledgement, supplied by the trusted caller.
    pub tombstone_receipt: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionRequest {
    pub routine_id: String,
    pub expected_revision: u64,
    pub approval_digest: String,
    pub scheduler_epoch: u64,
    pub firing: FiringRecord,
    pub window_id: String,
    pub now_utc: String,
    pub next_instant_utc: String,
    pub deadline_utc: String,
    pub envelope_id: String,
    pub envelope_settlement_id: String,
    pub reservation_id: String,
    pub reservation_settlement_id: String,
    pub outbox_id: String,
    pub proof_record_id: String,
    pub execution_bound: Funds,
    pub controller: Option<(String, u64)>,
    pub host_spawn_token: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchUpProgress {
    pub routine_id: String,
    pub revision: u64,
    pub sequence: u64,
    pub scan: crate::routine::CatchUpScan,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentEffect {
    pub outbox_id: String,
    pub proof_record_id: String,
    pub scheduler_epoch: u64,
    pub now_utc: String,
    pub controller: Option<(String, u64)>,
    pub host_spawn_token: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildRequest {
    pub id: String,
    pub settlement_id: String,
    pub amount: Funds,
    pub deadline_utc: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutineTransaction {
    InsertRoutine(RoutineSpec),
    ReplaceRoutine {
        expected_revision: u64,
        spec: RoutineSpec,
    },
    ApproveRoutine {
        expected_revision: u64,
        routine_id: String,
        approval: crate::routine::Approval,
    },
    SetSchedulerEpoch {
        expected: u64,
        next: u64,
    },
    SetControllerEpoch {
        controller_id: String,
        expected: Option<u64>,
        next: u64,
    },
    OpenWindow {
        id: String,
        routine_id: String,
        at_utc: String,
        expires_at: String,
    },
    CommitAdmission(AdmissionRequest),
    StartCatchUp {
        scan_id: String,
        routine_id: String,
        expected_revision: u64,
        scheduler_epoch: u64,
        from_utc: String,
        to_utc: String,
    },
    AdvanceCatchUp {
        scan_id: String,
        expected_sequence: u64,
        scheduler_epoch: u64,
        admission: Option<AdmissionRequest>,
        refusal_reason: Option<String>,
    },
    AcceptEffect {
        outbox_id: String,
        scheduler_epoch: u64,
        now_utc: String,
    },
    RevokeEffect {
        outbox_id: String,
        scheduler_epoch: u64,
    },
    TombstoneUnstarted {
        outbox_id: String,
        scheduler_epoch: u64,
        receipt: String,
    },
    MarkAmbiguous {
        outbox_id: String,
        scheduler_epoch: u64,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentTransaction {
    Carve {
        parent_id: String,
        parent_outbox_id: String,
        scheduler_epoch: u64,
        now_utc: String,
        children: Vec<ChildRequest>,
        committee: bool,
    },
    Reserve {
        allocation_id: String,
        reservation_id: String,
        settlement_id: String,
        amount: Funds,
        deadline_utc: String,
        effect: IntentEffect,
    },
    SettleReservation {
        reservation_id: String,
        settlement_id: String,
        resolution: Resolution,
    },
    SettleAllocation {
        allocation_id: String,
        settlement_id: String,
        now_utc: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlTransaction {
    Routine(Box<RoutineTransaction>),
    Intent(Box<IntentTransaction>),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlResult {
    Done,
    Admission { firing_key: String },
    Allocations(Vec<AllocationRecord>),
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlLedger {
    pub scheduler_epoch: u64,
    pub controllers: BTreeMap<String, u64>,
    pub routines: BTreeMap<String, RoutineSpec>,
    pub windows: BTreeMap<String, BudgetWindow>,
    pub firings: BTreeMap<String, FiringRecord>,
    pub watermark: BTreeMap<String, String>,
    pub catch_up_scans: BTreeMap<String, CatchUpProgress>,
    pub daily_counts: BTreeMap<String, BTreeMap<String, u32>>,
    pub overlap: BTreeMap<String, String>,
    pub allocations: BTreeMap<String, AllocationRecord>,
    pub reservations: BTreeMap<String, ExecutionReservation>,
    pub outbox: BTreeMap<String, OutboxRecord>,
    pub settlement_ids: BTreeSet<String>,
}
/// Digest the complete authority-bearing routine definition. State and approval are
/// excluded because they change when a reviewed revision is approved or paused.
/// Domain separation and a fixed JSON representation prevent confusion with other digests.
pub fn routine_approval_digest(spec: &RoutineSpec) -> Result<String, ControlError> {
    let mut canonical = spec.clone();
    canonical.state = RoutineState::Proposed;
    canonical.approval = None;
    let bytes =
        serde_json::to_vec(&canonical).map_err(|_| ControlError::Invalid("routine encoding"))?;
    let mut hash = Sha256::new();
    hash.update(b"ferroloop:routine-approval:v1\0");
    hash.update(bytes);
    Ok(format!("sha256:{:x}", hash.finalize()))
}
fn id(s: &str) -> Result<(), ControlError> {
    if s.trim().is_empty() || s.len() > 1024 || s.chars().any(char::is_control) {
        Err(ControlError::Invalid("identifier"))
    } else {
        Ok(())
    }
}
fn time(s: &str) -> Result<chrono::DateTime<chrono::Utc>, ControlError> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&chrono::Utc))
        .map_err(|_| ControlError::Invalid("timestamp"))
}
fn before(a: &str, b: &str) -> Result<bool, ControlError> {
    Ok(time(a)? < time(b)?)
}

pub trait RoutineStore {
    fn routine_transaction(
        &self,
        transaction: RoutineTransaction,
    ) -> Result<ControlResult, ControlError>;
    fn control_snapshot(&self) -> Result<ControlLedger, ControlError>;
}
pub trait IntentStore {
    fn intent_transaction(
        &self,
        transaction: IntentTransaction,
    ) -> Result<ControlResult, ControlError>;
}
/// A thread-safe memory backend for concurrent coordinators.
#[derive(Default)]
pub struct MemoryRoutineIntentStore {
    state: Mutex<ControlLedger>,
}
impl RoutineStore for MemoryRoutineIntentStore {
    fn routine_transaction(
        &self,
        transaction: RoutineTransaction,
    ) -> Result<ControlResult, ControlError> {
        self.state
            .lock()
            .map_err(|e| ControlError::Backend(e.to_string()))?
            .apply(ControlTransaction::Routine(Box::new(transaction)))
    }
    fn control_snapshot(&self) -> Result<ControlLedger, ControlError> {
        Ok(self
            .state
            .lock()
            .map_err(|e| ControlError::Backend(e.to_string()))?
            .clone())
    }
}
impl IntentStore for MemoryRoutineIntentStore {
    fn intent_transaction(
        &self,
        transaction: IntentTransaction,
    ) -> Result<ControlResult, ControlError> {
        self.state
            .lock()
            .map_err(|e| ControlError::Backend(e.to_string()))?
            .apply(ControlTransaction::Intent(Box::new(transaction)))
    }
}
impl ControlLedger {
    /// Stage all modifications on a clone; every error leaves `self` unchanged.
    pub fn apply(
        &mut self,
        transaction: ControlTransaction,
    ) -> Result<ControlResult, ControlError> {
        let mut next = self.clone();
        let result = next.apply_inner(transaction)?;
        // Every supported backend stores one bounded, consistent snapshot in a native transaction.
        next.validate_snapshot()?;
        next.encode_snapshot()?;
        *self = next;
        Ok(result)
    }
    /// Bound the shared snapshot below every supported backend's transport limits.
    pub fn encode_snapshot(&self) -> Result<Vec<u8>, ControlError> {
        let data = serde_json::to_vec(self).map_err(|e| ControlError::Backend(e.to_string()))?;
        if data.len() > MAX_CONTROL_LEDGER_BYTES {
            return Err(ControlError::Invalid("control ledger exceeds size limit"));
        }
        Ok(data)
    }
    /// Decode existing bytes. Invalid, oversized or inconsistent data is never a fresh ledger.
    pub fn decode_snapshot(data: &[u8]) -> Result<Self, ControlError> {
        if data.is_empty() || data.len() > MAX_CONTROL_LEDGER_BYTES {
            return Err(ControlError::Invalid("corrupt control ledger"));
        }
        let ledger: Self = serde_json::from_slice(data)
            .map_err(|_| ControlError::Invalid("corrupt control ledger"))?;
        ledger.validate_snapshot()?;
        Ok(ledger)
    }
    fn validate_snapshot(&self) -> Result<(), ControlError> {
        for (key, spec) in &self.routines {
            if key != &spec.routine_id || id(key).is_err() || spec.revision == 0 {
                return Err(ControlError::Invalid("corrupt routine"));
            }
            if matches!(spec.state, RoutineState::Approved)
                && spec.approval.as_ref().is_none_or(|approval| {
                    approval.revision != spec.revision
                        || approval.approved_by.trim().is_empty()
                        || routine_approval_digest(spec).ok().as_ref() != Some(&approval.digest)
                })
            {
                return Err(ControlError::Invalid("corrupt routine approval"));
            }
        }
        for (key, window) in &self.windows {
            if key != &window.id
                || !self.routines.contains_key(&window.routine_id)
                || !window.capacity.nonzero()
                || window.capacity.sub(window.remaining).is_err()
                || time(&window.expires_at).is_err()
            {
                return Err(ControlError::Invalid("corrupt window"));
            }
        }
        let mut settlements = BTreeSet::new();
        for (key, node) in &self.allocations {
            if key != &node.id
                || !node.total.nonzero()
                || node.total.sub(node.remaining).is_err()
                || !self.windows.contains_key(&node.window_id)
                || time(&node.deadline_utc).is_err()
                || !settlements.insert(node.settlement_id.clone())
            {
                return Err(ControlError::Invalid("corrupt allocation"));
            }
            if let Some(parent_id) = &node.parent_id {
                let parent = self
                    .allocations
                    .get(parent_id)
                    .ok_or(ControlError::Invalid("corrupt parent"))?;
                if parent.id == node.id
                    || node.root_id != parent.root_id
                    || node.window_id != parent.window_id
                    || time(&node.deadline_utc)? > time(&parent.deadline_utc)?
                {
                    return Err(ControlError::Invalid("corrupt lineage"));
                }
            } else if node.id != node.root_id {
                return Err(ControlError::Invalid("corrupt root"));
            }
        }
        for (key, reservation) in &self.reservations {
            if key != &reservation.id
                || !self.allocations.contains_key(&reservation.allocation_id)
                || !reservation.amount.nonzero()
                || !settlements.insert(reservation.settlement_id.clone())
                || matches!(reservation.resolution,Some(Resolution::Measured(spent)) if reservation.amount.sub(spent).is_err())
            {
                return Err(ControlError::Invalid("corrupt reservation"));
            }
        }
        // Check lineage reaches its root and no persisted credits or reservations exceed caps.
        for node in self.allocations.values() {
            let mut cursor = node;
            let mut seen = BTreeSet::new();
            while let Some(parent) = &cursor.parent_id {
                if !seen.insert(cursor.id.clone()) {
                    return Err(ControlError::Invalid("cyclic allocation"));
                }
                cursor = self
                    .allocations
                    .get(parent)
                    .ok_or(ControlError::Invalid("corrupt parent"))?;
            }
            if cursor.id != node.root_id {
                return Err(ControlError::Invalid("corrupt root"));
            }
            let mut accounted = node.remaining;
            for child in self
                .allocations
                .values()
                .filter(|c| c.parent_id.as_deref() == Some(node.id.as_str()))
            {
                accounted = accounted.add(if child.settled {
                    child.total.sub(child.remaining)?
                } else {
                    child.total
                })?;
            }
            for reservation in self
                .reservations
                .values()
                .filter(|r| r.allocation_id == node.id)
            {
                accounted = accounted.add(match reservation.resolution {
                    None => reservation.amount,
                    Some(Resolution::NeverDispatched) => Funds::default(),
                    Some(Resolution::Measured(spent)) => spent,
                })?;
            }
            if node.total.sub(accounted).is_err() {
                return Err(ControlError::Invalid("allocation overcommitted"));
            }
        }
        for window in self.windows.values() {
            let mut accounted = window.remaining.add(window.expired_credit)?;
            for root in self
                .allocations
                .values()
                .filter(|a| a.window_id == window.id && a.parent_id.is_none())
            {
                accounted = accounted.add(if root.settled {
                    root.total.sub(root.remaining)?
                } else {
                    root.total
                })?;
            }
            if window.capacity.sub(accounted).is_err() {
                return Err(ControlError::Invalid("window overcommitted"));
            }
        }
        if settlements != self.settlement_ids {
            return Err(ControlError::Invalid("corrupt settlement index"));
        }
        let mut associated = BTreeSet::new();
        for (key, out) in &self.outbox {
            if key != &out.id
                || !self.reservations.contains_key(&out.reservation_id)
                || !associated.insert(&out.reservation_id)
                || !self.firings.contains_key(&out.auth.firing_key)
                || id(&out.proof_record_id).is_err()
                || time(&out.deadline_utc).is_err()
                || (out.ticket == TicketState::TombstonedUnstarted
                    && out
                        .tombstone_receipt
                        .as_ref()
                        .is_none_or(|s| id(s).is_err()))
            {
                return Err(ControlError::Invalid("corrupt outbox"));
            }
        }
        for (key, firing) in &self.firings {
            if key != &firing.firing_key
                || !self.routines.contains_key(&firing.routine_id)
                || firing_key(&firing.routine_id, &firing.scheduled_instant_utc)
                    .ok()
                    .as_ref()
                    != Some(key)
            {
                return Err(ControlError::Invalid("corrupt firing"));
            }
        }
        for (routine, key) in &self.overlap {
            if self
                .firings
                .get(key)
                .is_none_or(|f| &f.routine_id != routine || f.state == FiringState::Settled)
            {
                return Err(ControlError::Invalid("corrupt overlap"));
            }
        }
        for (routine, watermark) in &self.watermark {
            if !self.routines.contains_key(routine) || time(watermark).is_err() {
                return Err(ControlError::Invalid("corrupt watermark"));
            }
        }
        Ok(())
    }
    fn apply_inner(&mut self, tx: ControlTransaction) -> Result<ControlResult, ControlError> {
        match tx {
            ControlTransaction::Routine(tx) => self.apply_routine(*tx),
            ControlTransaction::Intent(tx) => self.apply_intent(*tx),
        }
    }
    fn fresh_settlement(&mut self, s: &str) -> Result<(), ControlError> {
        id(s)?;
        if !self.settlement_ids.insert(s.into()) {
            return Err(ControlError::Conflict("settlement id"));
        }
        Ok(())
    }
    fn check_epoch(&self, epoch: u64) -> Result<(), ControlError> {
        if epoch == 0 || epoch != self.scheduler_epoch {
            Err(ControlError::Conflict("scheduler lease"))
        } else {
            Ok(())
        }
    }
    fn apply_routine(&mut self, tx: RoutineTransaction) -> Result<ControlResult, ControlError> {
        match tx {
            RoutineTransaction::InsertRoutine(spec) => {
                id(&spec.routine_id)?;
                if self.routines.contains_key(&spec.routine_id) {
                    return Err(ControlError::Conflict("routine exists"));
                }
                if matches!(spec.state, RoutineState::Approved) {
                    let approval = spec
                        .approval
                        .as_ref()
                        .ok_or(ControlError::Invalid("approval"))?;
                    if approval.revision != spec.revision
                        || approval.digest != routine_approval_digest(&spec)?
                        || approval.approved_by.trim().is_empty()
                        || spec.validate_for_approval(&approval.approved_at).is_err()
                    {
                        return Err(ControlError::Invalid("approval"));
                    }
                }
                if spec.revision == 0 {
                    return Err(ControlError::Invalid("revision"));
                }
                self.routines.insert(spec.routine_id.clone(), spec);
            }
            RoutineTransaction::ReplaceRoutine {
                expected_revision,
                spec,
            } => {
                let old = self
                    .routines
                    .get(&spec.routine_id)
                    .ok_or(ControlError::Missing("routine"))?;
                if old.revision != expected_revision
                    || spec.revision
                        != expected_revision
                            .checked_add(1)
                            .ok_or(ControlError::Invalid("revision overflow"))?
                {
                    return Err(ControlError::Conflict("routine revision"));
                }
                if matches!(spec.state, RoutineState::Approved) {
                    return Err(ControlError::Invalid("edits require fresh approval"));
                }
                for out in self.outbox.values_mut().filter(|o| {
                    o.auth.routine_id == spec.routine_id && o.ticket == TicketState::Issued
                }) {
                    out.ticket = TicketState::Revoked;
                }
                self.routines.insert(spec.routine_id.clone(), spec);
            }
            RoutineTransaction::ApproveRoutine {
                expected_revision,
                routine_id,
                approval,
            } => {
                let spec = self
                    .routines
                    .get_mut(&routine_id)
                    .ok_or(ControlError::Missing("routine"))?;
                if !matches!(spec.state, RoutineState::Proposed)
                    || spec.revision != expected_revision
                    || approval.revision != expected_revision
                    || approval.approved_by.trim().is_empty()
                    || approval.digest != routine_approval_digest(spec)?
                    || spec.validate_for_approval(&approval.approved_at).is_err()
                {
                    return Err(ControlError::Conflict("approval"));
                }
                spec.approval = Some(approval);
                spec.state = RoutineState::Approved;
            }
            RoutineTransaction::SetSchedulerEpoch { expected, next } => {
                if self.scheduler_epoch != expected || next <= expected {
                    return Err(ControlError::Conflict("scheduler lease"));
                }
                self.scheduler_epoch = next;
            }
            RoutineTransaction::SetControllerEpoch {
                controller_id,
                expected,
                next,
            } => {
                id(&controller_id)?;
                if self.controllers.get(&controller_id).copied() != expected
                    || next == 0
                    || expected.is_some_and(|old| next <= old)
                {
                    return Err(ControlError::Conflict("controller lease"));
                }
                self.controllers.insert(controller_id, next);
            }
            RoutineTransaction::OpenWindow {
                id: window_id,
                routine_id,
                at_utc,
                expires_at,
            } => {
                id(&window_id)?;
                let spec = self
                    .routines
                    .get(&routine_id)
                    .ok_or(ControlError::Missing("routine"))?;
                let period = period_for(spec, &at_utc)?;
                if !before(&at_utc, &expires_at)?
                    || time(&expires_at)? != window_end(spec, &at_utc)?
                {
                    return Err(ControlError::Invalid("window expiry"));
                }
                if self.windows.contains_key(&window_id)
                    || self.windows.values().any(|w| {
                        w.routine_id == routine_id
                            && (w.period == period
                                || time(&at_utc)
                                    .ok()
                                    .zip(time(&w.expires_at).ok())
                                    .is_none_or(|(at, end)| at < end))
                    })
                {
                    return Err(ControlError::Conflict("window exists"));
                }
                let capacity = Funds {
                    tokens: spec.allocation.window_tokens,
                    cost_micros: spec.allocation.window_cost_micros,
                };
                if !capacity.nonzero() {
                    return Err(ControlError::Invalid("window capacity"));
                }
                self.windows.insert(
                    window_id.clone(),
                    BudgetWindow {
                        id: window_id,
                        routine_id,
                        capacity,
                        remaining: capacity,
                        period,
                        expires_at,
                        expired_credit: Funds::default(),
                    },
                );
            }
            RoutineTransaction::CommitAdmission(req) => return self.admit(req),
            RoutineTransaction::StartCatchUp {
                scan_id,
                routine_id,
                expected_revision,
                scheduler_epoch,
                from_utc,
                to_utc,
            } => {
                self.check_epoch(scheduler_epoch)?;
                id(&scan_id)?;
                if self.catch_up_scans.contains_key(&scan_id)
                    || self
                        .catch_up_scans
                        .values()
                        .any(|scan| scan.routine_id == routine_id && !scan.scan.finished())
                {
                    return Err(ControlError::Conflict("catch-up scan exists"));
                }
                let spec = self
                    .routines
                    .get(&routine_id)
                    .ok_or(ControlError::Missing("routine"))?;
                if spec.revision != expected_revision
                    || !matches!(spec.state, RoutineState::Approved)
                {
                    return Err(ControlError::Conflict("routine revision"));
                }
                let scan = spec
                    .start_catch_up_scan(
                        &from_utc,
                        &to_utc,
                        self.watermark.get(&routine_id).map(String::as_str),
                    )
                    .map_err(|_| ControlError::Invalid("catch-up scan"))?;
                self.catch_up_scans.insert(
                    scan_id,
                    CatchUpProgress {
                        routine_id,
                        revision: expected_revision,
                        sequence: 0,
                        scan,
                    },
                );
            }
            RoutineTransaction::AdvanceCatchUp {
                scan_id,
                expected_sequence,
                scheduler_epoch,
                admission,
                refusal_reason,
            } => {
                self.check_epoch(scheduler_epoch)?;
                let progress = self
                    .catch_up_scans
                    .get(&scan_id)
                    .ok_or(ControlError::Missing("catch-up scan"))?
                    .clone();
                if progress.sequence != expected_sequence || progress.scan.finished() {
                    return Err(ControlError::Conflict("catch-up cursor"));
                }
                let spec = self
                    .routines
                    .get(&progress.routine_id)
                    .ok_or(ControlError::Missing("routine"))?;
                if spec.revision != progress.revision
                    || !matches!(spec.state, RoutineState::Approved)
                {
                    return Err(ControlError::Conflict("routine revision"));
                }
                let page = spec
                    .catch_up_page(&progress.scan)
                    .map_err(|_| ControlError::Invalid("catch-up page"))?;
                let planned = page
                    .records
                    .iter()
                    .find(|f| f.state == FiringState::Planned);
                if planned.is_none() && (admission.is_some() || refusal_reason.is_some()) {
                    return Err(ControlError::Invalid("unexpected catch-up admission"));
                }
                if planned.is_some()
                    && admission.is_none()
                    && refusal_reason.as_ref().is_none_or(|r| id(r).is_err())
                {
                    return Err(ControlError::Invalid("missing catch-up disposition"));
                }
                if planned.is_some() && admission.is_some() && refusal_reason.is_some() {
                    return Err(ControlError::Invalid("ambiguous catch-up disposition"));
                }
                for mut record in page.records {
                    if self.firings.contains_key(&record.firing_key)
                        || self.watermark.get(&progress.routine_id).is_some_and(|w| {
                            time(w)
                                .ok()
                                .zip(time(&record.scheduled_instant_utc).ok())
                                .is_none_or(|(a, b)| a >= b)
                        })
                    {
                        return Err(ControlError::Conflict("catch-up watermark"));
                    }
                    if record.state == FiringState::Planned {
                        if let Some(req) = &admission {
                            if req.firing != record || req.scheduler_epoch != scheduler_epoch {
                                return Err(ControlError::Conflict("catch-up admission mismatch"));
                            }
                            self.admit(req.clone())?;
                        } else {
                            record.state = FiringState::Refused {
                                reason: refusal_reason.clone().unwrap(),
                            };
                            self.watermark.insert(
                                progress.routine_id.clone(),
                                record.scheduled_instant_utc.clone(),
                            );
                            self.firings.insert(record.firing_key.clone(), record);
                        }
                    } else {
                        self.watermark.insert(
                            progress.routine_id.clone(),
                            record.scheduled_instant_utc.clone(),
                        );
                        self.firings.insert(record.firing_key.clone(), record);
                    }
                }
                let entry = self.catch_up_scans.get_mut(&scan_id).unwrap();
                entry.sequence = entry
                    .sequence
                    .checked_add(1)
                    .ok_or(ControlError::Invalid("scan revision overflow"))?;
                entry.scan = page.next;
            }
            RoutineTransaction::AcceptEffect {
                outbox_id,
                scheduler_epoch,
                now_utc,
            } => {
                self.check_epoch(scheduler_epoch)?;
                let out = self
                    .outbox
                    .get(&outbox_id)
                    .cloned()
                    .ok_or(ControlError::Missing("outbox"))?;
                let spec = self
                    .routines
                    .get(&out.auth.routine_id)
                    .ok_or(ControlError::Missing("routine"))?;
                if out.ticket != TicketState::Issued
                    || out.auth.scheduler_epoch != scheduler_epoch
                    || spec.revision != out.auth.revision
                    || !matches!(spec.state, RoutineState::Approved)
                    || spec
                        .approval
                        .as_ref()
                        .is_none_or(|a| a.revision != spec.revision || a.digest.trim().is_empty())
                    || self.overlap.get(&out.auth.routine_id) != Some(&out.auth.firing_key)
                    || self.firings.get(&out.auth.firing_key).is_none_or(|f| {
                        matches!(
                            f.state,
                            FiringState::Ambiguous
                                | FiringState::Cancelled
                                | FiringState::Settled
                                | FiringState::Failed
                        )
                    })
                    || time(&out.deadline_utc)? - time(&now_utc)? < chrono::Duration::seconds(60)
                {
                    return Err(ControlError::Conflict("effect fence"));
                }
                if let Some((controller, epoch)) = &out.auth.controller
                    && self.controllers.get(controller) != Some(epoch)
                {
                    return Err(ControlError::Conflict("controller lease"));
                }
                self.outbox.get_mut(&outbox_id).unwrap().ticket = TicketState::Accepted;
                self.firings
                    .get_mut(&out.auth.firing_key)
                    .ok_or(ControlError::Missing("firing"))?
                    .state = FiringState::EffectAccepted;
            }
            RoutineTransaction::RevokeEffect {
                outbox_id,
                scheduler_epoch,
            } => {
                self.check_epoch(scheduler_epoch)?;
                let out = self
                    .outbox
                    .get_mut(&outbox_id)
                    .ok_or(ControlError::Missing("outbox"))?;
                if out.ticket != TicketState::Issued {
                    return Err(ControlError::Conflict("effect already crossed"));
                }
                out.ticket = TicketState::Revoked;
            }
            RoutineTransaction::TombstoneUnstarted {
                outbox_id,
                scheduler_epoch,
                receipt,
            } => {
                self.check_epoch(scheduler_epoch)?;
                id(&receipt)?;
                let out = self
                    .outbox
                    .get_mut(&outbox_id)
                    .ok_or(ControlError::Missing("outbox"))?;
                if out.ticket != TicketState::Accepted {
                    return Err(ControlError::Conflict("effect not accepted"));
                }
                // The authenticated host/controller must supply its durable tombstone evidence.
                out.ticket = TicketState::TombstonedUnstarted;
                out.tombstone_receipt = Some(receipt);
            }
            RoutineTransaction::MarkAmbiguous {
                outbox_id,
                scheduler_epoch,
            } => {
                self.check_epoch(scheduler_epoch)?;
                let out = self
                    .outbox
                    .get(&outbox_id)
                    .ok_or(ControlError::Missing("outbox"))?;
                if out.ticket != TicketState::Accepted {
                    return Err(ControlError::Conflict("effect not accepted"));
                }
                self.firings
                    .get_mut(&out.auth.firing_key)
                    .ok_or(ControlError::Missing("firing"))?
                    .state = FiringState::Ambiguous;
            }
        }
        Ok(ControlResult::Done)
    }
    fn admit(&mut self, req: AdmissionRequest) -> Result<ControlResult, ControlError> {
        self.check_epoch(req.scheduler_epoch)?;
        let spec = self
            .routines
            .get(&req.routine_id)
            .ok_or(ControlError::Missing("routine"))?;
        let approval = spec
            .approval
            .as_ref()
            .ok_or(ControlError::Conflict("approval"))?;
        if !matches!(spec.state, RoutineState::Approved)
            || spec.revision != req.expected_revision
            || approval.revision != spec.revision
            || approval.digest != req.approval_digest
            || approval.digest != routine_approval_digest(spec)?
            || approval.approved_by.trim().is_empty()
        {
            return Err(ControlError::Conflict("routine authority"));
        }
        id(&req.proof_record_id)?;
        if req.firing.routine_id != req.routine_id
            || req.firing.revision != spec.revision
            || req.firing.state != FiringState::Planned
            || req.firing.firing_key
                != firing_key(&req.routine_id, &req.firing.scheduled_instant_utc)
                    .map_err(|_| ControlError::Invalid("firing key"))?
        {
            return Err(ControlError::Invalid("firing"));
        }
        let instant = time(&req.firing.scheduled_instant_utc)?;
        let before_instant = instant
            .checked_sub_signed(chrono::Duration::nanoseconds(1))
            .ok_or(ControlError::Invalid("timestamp"))?;
        let actual = spec
            .schedule
            .instants(
                &before_instant.to_rfc3339(),
                &req.firing.scheduled_instant_utc,
                None,
            )
            .map_err(|_| ControlError::Invalid("schedule"))?;
        if actual.len() != 1
            || actual[0].nominal_slots != req.firing.nominal_slots
            || actual[0].coalesced != req.firing.coalesced
        {
            return Err(ControlError::Invalid("firing evidence"));
        }
        // The deadline is capped at `max_firing_secs` (at most 24 hours).
        // Searching beyond that ceiling cannot affect either the on-time or
        // catch-up deadline, and valid annual schedules may have a next
        // occurrence beyond the schedule iterator's 90-day safety bound.
        let proposed_next = time(&req.next_instant_utc)?;
        if proposed_next <= instant || !(60..=86_400).contains(&spec.max_firing_secs) {
            return Err(ControlError::Invalid("next instant"));
        }
        let cap = instant
            .checked_add_signed(chrono::Duration::seconds(spec.max_firing_secs as i64))
            .ok_or(ControlError::Invalid("next instant"))?;
        let search_end = proposed_next.min(cap);
        let next = spec
            .schedule
            .instants(
                &req.firing.scheduled_instant_utc,
                &search_end.to_rfc3339(),
                None,
            )
            .map_err(|_| ControlError::Invalid("next instant"))?;
        if proposed_next <= cap {
            if next.len() != 1 || time(&next[0].scheduled_instant_utc)? != proposed_next {
                return Err(ControlError::Invalid("next instant"));
            }
        } else if !next.is_empty() {
            return Err(ControlError::Invalid("next instant"));
        }
        let computed_deadline = spec
            .firing_deadline(
                &req.firing.scheduled_instant_utc,
                &req.next_instant_utc,
                &req.now_utc,
            )
            .map_err(|_| ControlError::Invalid("expired candidate"))?;
        if time(&computed_deadline)? != time(&req.deadline_utc)? {
            return Err(ControlError::Invalid("deadline mismatch"));
        }
        if self.firings.contains_key(&req.firing.firing_key)
            || self.overlap.contains_key(&req.routine_id)
        {
            return Err(ControlError::Conflict("firing or overlap"));
        }
        if self.watermark.get(&req.routine_id).is_some_and(|w| {
            time(w)
                .ok()
                .zip(time(&req.firing.scheduled_instant_utc).ok())
                .is_none_or(|(a, b)| a >= b)
        }) {
            return Err(ControlError::Conflict("watermark"));
        }
        if !before(&req.now_utc, &spec.expires_at)?
            || !before(&req.firing.scheduled_instant_utc, &spec.expires_at)?
            || !before(&req.now_utc, &req.deadline_utc)?
            || time(&req.deadline_utc)? - time(&req.now_utc)? < chrono::Duration::seconds(60)
            || time(&req.deadline_utc)? > time(&spec.expires_at)?
        {
            return Err(ControlError::Invalid("expired candidate"));
        }
        let window = self
            .windows
            .get(&req.window_id)
            .ok_or(ControlError::Missing("window"))?;
        if window.routine_id != req.routine_id
            || window.period != period_for(spec, &req.firing.scheduled_instant_utc)?
            || !before(&req.now_utc, &window.expires_at)?
        {
            return Err(ControlError::Conflict("source window"));
        }
        let day = local_day(spec, &req.firing.scheduled_instant_utc)?;
        let count = self
            .daily_counts
            .get(&req.routine_id)
            .and_then(|days| days.get(&day))
            .copied()
            .unwrap_or(0);
        if count >= spec.max_firings_per_day {
            return Err(ControlError::Conflict("daily cap"));
        }
        let envelope = Funds {
            tokens: spec.allocation.per_firing_tokens,
            cost_micros: spec.allocation.per_firing_cost_micros,
        };
        if !envelope.nonzero() || !req.execution_bound.nonzero() {
            return Err(ControlError::Budget);
        }
        let remaining = envelope.sub(req.execution_bound)?;
        let window_remaining = window.remaining.sub(envelope)?;
        for s in [&req.envelope_id, &req.reservation_id, &req.outbox_id] {
            id(s)?;
        }
        if self.allocations.contains_key(&req.envelope_id)
            || self.reservations.contains_key(&req.reservation_id)
            || self.outbox.contains_key(&req.outbox_id)
        {
            return Err(ControlError::Conflict("record exists"));
        }
        match (&req.controller, &req.host_spawn_token) {
            (Some((controller, epoch)), None)
                if self.controllers.get(controller) == Some(epoch) => {}
            (None, Some(token))
                if id(token).is_ok()
                    && !self
                        .outbox
                        .values()
                        .any(|o| o.auth.host_spawn_token.as_ref() == Some(token)) => {}
            _ => return Err(ControlError::Conflict("target fence")),
        }
        self.fresh_settlement(&req.envelope_settlement_id)?;
        self.fresh_settlement(&req.reservation_settlement_id)?;
        self.windows.get_mut(&req.window_id).unwrap().remaining = window_remaining;
        self.allocations.insert(
            req.envelope_id.clone(),
            AllocationRecord {
                id: req.envelope_id.clone(),
                parent_id: None,
                root_id: req.envelope_id.clone(),
                window_id: req.window_id.clone(),
                settlement_id: req.envelope_settlement_id,
                total: envelope,
                remaining,
                deadline_utc: req.deadline_utc.clone(),
                settled: false,
            },
        );
        self.reservations.insert(
            req.reservation_id.clone(),
            ExecutionReservation {
                id: req.reservation_id.clone(),
                allocation_id: req.envelope_id,
                settlement_id: req.reservation_settlement_id,
                amount: req.execution_bound,
                resolution: None,
            },
        );
        let firing_key = req.firing.firing_key.clone();
        self.outbox.insert(
            req.outbox_id.clone(),
            OutboxRecord {
                id: req.outbox_id,
                reservation_id: req.reservation_id,
                proof_record_id: req.proof_record_id,
                auth: DispatchAuth {
                    routine_id: req.routine_id.clone(),
                    revision: req.expected_revision,
                    firing_key: firing_key.clone(),
                    scheduler_epoch: req.scheduler_epoch,
                    controller: req.controller,
                    host_spawn_token: req.host_spawn_token,
                },
                deadline_utc: req.deadline_utc,
                ticket: TicketState::Issued,
                tombstone_receipt: None,
            },
        );
        self.watermark.insert(
            req.routine_id.clone(),
            req.firing.scheduled_instant_utc.clone(),
        );
        self.daily_counts
            .entry(req.routine_id.clone())
            .or_default()
            .insert(
                day,
                count
                    .checked_add(1)
                    .ok_or(ControlError::Invalid("daily count overflow"))?,
            );
        self.overlap.insert(req.routine_id, firing_key.clone());
        let mut firing = req.firing;
        firing.state = FiringState::Queued;
        self.firings.insert(firing_key.clone(), firing);
        Ok(ControlResult::Admission { firing_key })
    }
    fn apply_intent(&mut self, tx: IntentTransaction) -> Result<ControlResult, ControlError> {
        match tx {
            IntentTransaction::Carve {
                parent_id,
                parent_outbox_id,
                scheduler_epoch,
                now_utc,
                children,
                committee,
            } => {
                self.check_epoch(scheduler_epoch)?;
                if children.is_empty()
                    || (committee && !(2..=5).contains(&children.len()))
                    || (!committee && children.len() != 1)
                {
                    return Err(ControlError::Invalid("child count"));
                }
                if committee && children.iter().any(|c| c.amount != children[0].amount) {
                    return Err(ControlError::Invalid("committee budgets differ"));
                }
                let parent = self
                    .allocations
                    .get(&parent_id)
                    .ok_or(ControlError::Missing("parent allocation"))?
                    .clone();
                if parent.settled {
                    return Err(ControlError::Conflict("parent settled"));
                }
                // New descendants require the exact still-active parent effect and
                // the current authority, not merely a once-accepted old ticket.
                let out = self
                    .outbox
                    .get(&parent_outbox_id)
                    .ok_or(ControlError::Missing("parent outbox"))?;
                let spec = self
                    .routines
                    .get(&out.auth.routine_id)
                    .ok_or(ControlError::Missing("routine"))?;
                let parent_effect_active = out.ticket == TicketState::Accepted
                    && out.auth.scheduler_epoch == scheduler_epoch
                    && spec.revision == out.auth.revision
                    && matches!(spec.state, RoutineState::Approved)
                    && self.overlap.get(&out.auth.routine_id) == Some(&out.auth.firing_key)
                    && self.firings.get(&out.auth.firing_key).is_some_and(|f| {
                        !matches!(
                            f.state,
                            FiringState::Ambiguous
                                | FiringState::Cancelled
                                | FiringState::Settled
                                | FiringState::Failed
                        )
                    })
                    && self
                        .reservations
                        .get(&out.reservation_id)
                        .is_some_and(|r| r.allocation_id == parent_id && r.resolution.is_none())
                    && out
                        .auth
                        .controller
                        .as_ref()
                        .is_none_or(|(controller, epoch)| {
                            self.controllers.get(controller) == Some(epoch)
                        })
                    && before(&now_utc, &parent.deadline_utc)?
                    && before(&now_utc, &out.deadline_utc)?;
                if !parent_effect_active {
                    return Err(ControlError::Conflict("parent turn is not active"));
                }
                let mut total = Funds::default();
                let mut ids = BTreeSet::new();
                let mut settlements = BTreeSet::new();
                for c in &children {
                    id(&c.id)?;
                    id(&c.settlement_id)?;
                    if self.allocations.contains_key(&c.id)
                        || !ids.insert(c.id.clone())
                        || self.settlement_ids.contains(&c.settlement_id)
                        || !settlements.insert(c.settlement_id.clone())
                        || !c.amount.nonzero()
                        || c.amount.tokens >= parent.total.tokens
                        || c.amount.cost_micros >= parent.total.cost_micros
                        || time(&c.deadline_utc)? > time(&parent.deadline_utc)?
                    {
                        return Err(ControlError::Invalid("child allocation"));
                    }
                    total = total.add(c.amount)?;
                }
                let remaining = parent.remaining.sub(total)?;
                let mut out = Vec::new();
                for c in children {
                    self.fresh_settlement(&c.settlement_id)?;
                    let child = AllocationRecord {
                        id: c.id.clone(),
                        parent_id: Some(parent_id.clone()),
                        root_id: parent.root_id.clone(),
                        window_id: parent.window_id.clone(),
                        settlement_id: c.settlement_id,
                        total: c.amount,
                        remaining: c.amount,
                        deadline_utc: c.deadline_utc,
                        settled: false,
                    };
                    self.allocations.insert(c.id, child.clone());
                    out.push(child);
                }
                self.allocations.get_mut(&parent_id).unwrap().remaining = remaining;
                Ok(ControlResult::Allocations(out))
            }
            IntentTransaction::Reserve {
                allocation_id,
                reservation_id,
                settlement_id,
                amount,
                deadline_utc,
                effect,
            } => {
                self.check_epoch(effect.scheduler_epoch)?;
                id(&effect.outbox_id)?;
                id(&effect.proof_record_id)?;
                if self.outbox.contains_key(&effect.outbox_id) {
                    return Err(ControlError::Conflict("outbox"));
                }
                id(&reservation_id)?;
                if self.reservations.contains_key(&reservation_id) || !amount.nonzero() {
                    return Err(ControlError::Conflict("reservation"));
                }
                let node = self
                    .allocations
                    .get(&allocation_id)
                    .ok_or(ControlError::Missing("allocation"))?;
                if node.settled || time(&deadline_utc)? > time(&node.deadline_utc)? {
                    return Err(ControlError::Conflict("allocation fence"));
                }
                let remaining = node.remaining.sub(amount)?;
                let root_id = node.root_id.clone();
                if time(&deadline_utc)? - time(&effect.now_utc)? < chrono::Duration::seconds(60) {
                    return Err(ControlError::Invalid("expired candidate"));
                }
                let root_out = self
                    .outbox
                    .values()
                    .find(|o| {
                        self.reservations
                            .get(&o.reservation_id)
                            .is_some_and(|r| r.allocation_id == root_id)
                    })
                    .ok_or(ControlError::Missing("root outbox"))?
                    .clone();
                let spec = self
                    .routines
                    .get(&root_out.auth.routine_id)
                    .ok_or(ControlError::Missing("routine"))?;
                if root_out.ticket != TicketState::Accepted
                    || self.firings.get(&root_out.auth.firing_key).is_none_or(|f| {
                        matches!(
                            f.state,
                            FiringState::Ambiguous
                                | FiringState::Cancelled
                                | FiringState::Settled
                                | FiringState::Failed
                        )
                    })
                {
                    return Err(ControlError::Conflict("root effect not active"));
                }
                if spec.revision != root_out.auth.revision
                    || !matches!(spec.state, RoutineState::Approved)
                    || self.overlap.get(&root_out.auth.routine_id)
                        != Some(&root_out.auth.firing_key)
                {
                    return Err(ControlError::Conflict("routine fence"));
                }
                match (&effect.controller, &effect.host_spawn_token) {
                    (Some((controller, epoch)), None)
                        if self.controllers.get(controller) == Some(epoch) => {}
                    (None, Some(token))
                        if id(token).is_ok()
                            && !self
                                .outbox
                                .values()
                                .any(|o| o.auth.host_spawn_token.as_ref() == Some(token)) => {}
                    _ => return Err(ControlError::Conflict("target fence")),
                }
                self.fresh_settlement(&settlement_id)?;
                self.allocations.get_mut(&allocation_id).unwrap().remaining = remaining;
                self.reservations.insert(
                    reservation_id.clone(),
                    ExecutionReservation {
                        id: reservation_id.clone(),
                        allocation_id,
                        settlement_id,
                        amount,
                        resolution: None,
                    },
                );
                self.outbox.insert(
                    effect.outbox_id.clone(),
                    OutboxRecord {
                        id: effect.outbox_id,
                        reservation_id,
                        proof_record_id: effect.proof_record_id,
                        auth: DispatchAuth {
                            routine_id: root_out.auth.routine_id,
                            revision: root_out.auth.revision,
                            firing_key: root_out.auth.firing_key,
                            scheduler_epoch: effect.scheduler_epoch,
                            controller: effect.controller,
                            host_spawn_token: effect.host_spawn_token,
                        },
                        deadline_utc,
                        ticket: TicketState::Issued,
                        tombstone_receipt: None,
                    },
                );
                Ok(ControlResult::Done)
            }
            IntentTransaction::SettleReservation {
                reservation_id,
                settlement_id,
                resolution,
            } => {
                let reservation = self
                    .reservations
                    .get(&reservation_id)
                    .ok_or(ControlError::Missing("reservation"))?
                    .clone();
                if reservation.settlement_id != settlement_id {
                    return Err(ControlError::Conflict("settlement id"));
                }
                if let Some(previous) = reservation.resolution {
                    return if previous == resolution {
                        Ok(ControlResult::Done)
                    } else {
                        Err(ControlError::Conflict("settlement already applied"))
                    };
                }
                let effect = self
                    .outbox
                    .values()
                    .find(|o| o.reservation_id == reservation_id);
                let spent = match resolution {
                    Resolution::NeverDispatched => {
                        if effect.is_none_or(|o| {
                            !(o.ticket == TicketState::Revoked
                                || (o.ticket == TicketState::TombstonedUnstarted
                                    && o.tombstone_receipt
                                        .as_ref()
                                        .is_some_and(|receipt| id(receipt).is_ok())))
                        }) {
                            return Err(ControlError::Ambiguous);
                        }
                        Funds::default()
                    }
                    Resolution::Measured(amount) => {
                        if effect.is_none_or(|o| o.ticket != TicketState::Accepted) {
                            return Err(ControlError::Conflict("effect never accepted"));
                        }
                        reservation.amount.sub(amount)?;
                        amount
                    }
                };
                let refund = reservation.amount.sub(spent)?;
                let allocation = self
                    .allocations
                    .get_mut(&reservation.allocation_id)
                    .ok_or(ControlError::Missing("allocation"))?;
                if allocation.settled {
                    return Err(ControlError::Conflict("allocation settled"));
                }
                allocation.remaining = allocation.remaining.add(refund)?;
                if allocation.remaining.tokens > allocation.total.tokens
                    || allocation.remaining.cost_micros > allocation.total.cost_micros
                {
                    return Err(ControlError::Budget);
                }
                self.reservations
                    .get_mut(&reservation_id)
                    .unwrap()
                    .resolution = Some(resolution);
                Ok(ControlResult::Done)
            }
            IntentTransaction::SettleAllocation {
                allocation_id,
                settlement_id,
                now_utc,
            } => {
                let node = self
                    .allocations
                    .get(&allocation_id)
                    .ok_or(ControlError::Missing("allocation"))?
                    .clone();
                if node.settlement_id != settlement_id {
                    return Err(ControlError::Conflict("settlement id"));
                }
                if node.settled {
                    return Ok(ControlResult::Done);
                }
                if self
                    .allocations
                    .values()
                    .any(|x| x.parent_id.as_deref() == Some(&allocation_id) && !x.settled)
                    || self
                        .reservations
                        .values()
                        .any(|x| x.allocation_id == allocation_id && x.resolution.is_none())
                {
                    return Err(ControlError::Conflict("unsettled descendants"));
                }
                if let Some(parent_id) = &node.parent_id {
                    let parent = self
                        .allocations
                        .get_mut(parent_id)
                        .ok_or(ControlError::Missing("parent"))?;
                    if parent.settled {
                        return Err(ControlError::Conflict("parent settled"));
                    }
                    parent.remaining = parent.remaining.add(node.remaining)?;
                    if parent.remaining.tokens > parent.total.tokens
                        || parent.remaining.cost_micros > parent.total.cost_micros
                    {
                        return Err(ControlError::Budget);
                    }
                } else {
                    let window = self
                        .windows
                        .get_mut(&node.window_id)
                        .ok_or(ControlError::Missing("window"))?;
                    if before(&now_utc, &window.expires_at)? {
                        window.remaining = window.remaining.add(node.remaining)?;
                        if window.remaining.tokens > window.capacity.tokens
                            || window.remaining.cost_micros > window.capacity.cost_micros
                        {
                            return Err(ControlError::Budget);
                        }
                    } else {
                        window.expired_credit = window.expired_credit.add(node.remaining)?;
                    }
                    let firing = self
                        .outbox
                        .values()
                        .find(|o| {
                            self.reservations
                                .get(&o.reservation_id)
                                .is_some_and(|r| r.allocation_id == allocation_id)
                        })
                        .map(|o| (o.auth.routine_id.clone(), o.auth.firing_key.clone()));
                    if let Some((routine, key)) = firing {
                        if self.overlap.get(&routine) == Some(&key) {
                            self.overlap.remove(&routine);
                        }
                        self.firings
                            .get_mut(&key)
                            .ok_or(ControlError::Missing("firing"))?
                            .state = FiringState::Settled;
                    }
                }
                self.allocations.get_mut(&allocation_id).unwrap().settled = true;
                Ok(ControlResult::Done)
            }
        }
    }
}
fn local_day(spec: &RoutineSpec, timestamp: &str) -> Result<String, ControlError> {
    use chrono::Datelike;
    let tz = spec
        .schedule
        .timezone()
        .map_err(|_| ControlError::Invalid("timezone"))?;
    let local = time(timestamp)?.with_timezone(&tz);
    Ok(format!(
        "{:04}-{:02}-{:02}",
        local.year(),
        local.month(),
        local.day()
    ))
}
fn period_for(spec: &RoutineSpec, timestamp: &str) -> Result<String, ControlError> {
    use chrono::Datelike;
    let tz = spec
        .schedule
        .timezone()
        .map_err(|_| ControlError::Invalid("timezone"))?;
    let local = time(timestamp)?.with_timezone(&tz);
    Ok(match spec.allocation.window {
        crate::routine::Window::Day => format!(
            "{:04}-{:02}-{:02}",
            local.year(),
            local.month(),
            local.day()
        ),
        crate::routine::Window::Week => format!(
            "{:04}-W{:02}",
            local.iso_week().year(),
            local.iso_week().week()
        ),
    })
}

impl RoutineStore for crate::mem::MemStore {
    fn routine_transaction(&self, tx: RoutineTransaction) -> Result<ControlResult, ControlError> {
        self.control_ledger
            .lock()
            .map_err(|e| ControlError::Backend(e.to_string()))?
            .apply(ControlTransaction::Routine(Box::new(tx)))
    }
    fn control_snapshot(&self) -> Result<ControlLedger, ControlError> {
        Ok(self
            .control_ledger
            .lock()
            .map_err(|e| ControlError::Backend(e.to_string()))?
            .clone())
    }
}
impl IntentStore for crate::mem::MemStore {
    fn intent_transaction(&self, tx: IntentTransaction) -> Result<ControlResult, ControlError> {
        self.control_ledger
            .lock()
            .map_err(|e| ControlError::Backend(e.to_string()))?
            .apply(ControlTransaction::Intent(Box::new(tx)))
    }
}

fn window_end(spec: &RoutineSpec, at: &str) -> Result<chrono::DateTime<chrono::Utc>, ControlError> {
    use chrono::{Datelike, TimeZone};
    let tz = spec
        .schedule
        .timezone()
        .map_err(|_| ControlError::Invalid("timezone"))?;
    let local = time(at)?.with_timezone(&tz);
    let days = match spec.allocation.window {
        crate::routine::Window::Day => 1,
        crate::routine::Window::Week => 7 - u64::from(local.weekday().num_days_from_monday()),
    };
    let date = local
        .date_naive()
        .checked_add_days(chrono::Days::new(days))
        .ok_or(ControlError::Invalid("window boundary"))?;
    let start = date
        .and_hms_opt(0, 0, 0)
        .ok_or(ControlError::Invalid("window boundary"))?;
    for minute in 0..=2880 {
        let wall = start
            .checked_add_signed(chrono::Duration::minutes(minute))
            .ok_or(ControlError::Invalid("window boundary"))?;
        if let Some(d) = tz.from_local_datetime(&wall).earliest() {
            return Ok(d.with_timezone(&chrono::Utc));
        }
    }
    Err(ControlError::Invalid("window boundary"))
}
