//! Gate failover engine (Record #98): one governor reservation up front,
//! journalled attempts, settlement exactly once at the end.
//!
//! Witness honesty: ureq cannot report connection reuse or bytes written.
//! Failover proxy attempts disable pooling (`max_idle_connections_per_host(0)`
//! in ureq 3.x terms) and use a fresh agent per attempt, so every connection
//! is fresh by construction. Only DNS / connect-refused errors, known to
//! occur before any request byte is written, yield
//! `NoExecutionProven(FreshConnectionZeroBytes)`; any error after request
//! send starts is `UnknownAfterSend`. See `proxy::ProxyAttemptWitness`.

use std::sync::Arc;

use fl_core::failover::{
    FailoverAction, FailoverDecision, FailoverError, PolicyFingerprint, StopReason,
    drift_invalidates,
};
use fl_core::governor::{
    AdmissionOutcome, AdmissionRequest, GovernorStore, Permit, SettledOutcome,
};
use fl_core::{
    AttemptJournal, AttemptJournalEntry, AttemptOutcome, AttemptState, EndpointSet, JournalStore,
    ParentEnvelope, ZeroBytesWitness,
};

use crate::GateError;
use crate::connector::ToolOutcome;
use crate::governor::GateGovernor;

pub const UNKNOWN_AFTER_SEND_TEXT: &str = "unknown_after_send: may have executed";
pub const FAILOVER_JOURNAL_CAP: usize = 200;

/// Persistent [`JournalStore`] over the governor store backend (Runtime
/// entry slug `failover-journal` in the persistent impl, capped at 200).
/// Strict on corrupt: a load failure is `JournalUnavailable`, never a reset.
/// Mem-backed governor stores keep the same entries in memory for tests.
pub struct PersistentFailoverJournal {
    backend: Arc<dyn GovernorStore>,
    entries: Vec<AttemptJournalEntry>,
    spent: u64,
}

impl PersistentFailoverJournal {
    pub fn new(backend: Arc<dyn GovernorStore>) -> Result<Self, FailoverError> {
        let entries = backend
            .load_failover_journal()
            .map_err(|_| FailoverError::JournalUnavailable)?;
        Ok(Self {
            backend,
            entries,
            spent: 0,
        })
    }
    pub fn has_uncertain_for(&self, logical_operation_id: &str) -> bool {
        JournalStore::has_uncertain(self, logical_operation_id)
    }
    fn persist(&self) -> Result<(), FailoverError> {
        self.backend
            .save_failover_journal(&self.entries)
            .map_err(|_| FailoverError::JournalUnavailable)
    }
    /// Evict finished entries only. Unfinished (`Dispatching`) entries are
    /// never evicted: if the journal is full of them, the caller gets
    /// `JournalUnavailable` rather than losing an uncertain attempt.
    fn capped(&mut self) -> Result<(), FailoverError> {
        while self.entries.len() > FAILOVER_JOURNAL_CAP {
            match self
                .entries
                .iter()
                .position(|e| e.state != AttemptState::Dispatching)
            {
                Some(pos) => {
                    self.entries.remove(pos);
                }
                None => return Err(FailoverError::JournalUnavailable),
            }
        }
        Ok(())
    }
}

impl JournalStore for PersistentFailoverJournal {
    fn begin(&mut self, entry: AttemptJournalEntry) -> Result<(), FailoverError> {
        if self.entries.iter().any(|e| {
            e.logical_operation_id == entry.logical_operation_id
                && e.attempt_index == entry.attempt_index
        }) {
            return Err(FailoverError::DuplicateBegin {
                logical_operation_id: entry.logical_operation_id,
                attempt_index: entry.attempt_index,
            });
        }
        self.entries.push(entry);
        if let Err(e) = self.capped() {
            self.entries.pop();
            return Err(e);
        }
        self.persist()
    }
    fn finish(
        &mut self,
        logical_operation_id: &str,
        attempt_index: u32,
        outcome: AttemptOutcome,
    ) -> Result<(), FailoverError> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| {
                e.logical_operation_id == logical_operation_id && e.attempt_index == attempt_index
            })
            .ok_or_else(|| FailoverError::FinishBeforeBegin {
                logical_operation_id: logical_operation_id.to_string(),
                attempt_index,
            })?;
        if entry.state != AttemptState::Dispatching {
            return Err(FailoverError::AttemptAlreadyFinished {
                logical_operation_id: logical_operation_id.to_string(),
                attempt_index,
            });
        }
        entry.state = outcome.into();
        self.persist()
    }
    fn entries(&self) -> &[AttemptJournalEntry] {
        &self.entries
    }
    fn record_spend(&mut self, units: u64) {
        self.spent = self.spent.saturating_add(units);
    }
    fn spent_units(&self) -> u64 {
        self.spent
    }
}

fn snapshot(journal: &dyn JournalStore, logical_operation_id: &str) -> AttemptJournal {
    AttemptJournal::from_entries(
        journal
            .entries()
            .iter()
            .filter(|e| e.logical_operation_id == logical_operation_id)
            .cloned()
            .collect(),
        journal.spent_units(),
    )
}

/// Outcome of a dispatch-only failover run (no admission/settlement; the
/// caller — `governed_dispatch` — owns those).
pub enum DispatchOnlyOutcome<T> {
    Executed {
        response: Option<T>,
        bytes: u64,
        journal: AttemptJournal,
    },
    Unknown {
        journal: AttemptJournal,
    },
    AllNoExecutionProven {
        journal: AttemptJournal,
        error: Option<GateError>,
        reason: StopReason,
    },
}

/// Classify transport facts. Only a fresh connection with zero written
/// bytes and no after-connect write failure proves no execution; a reused
/// connection, a reset, or any written byte is `UnknownAfterSend`.
pub fn classify_transport(
    fresh_connection: bool,
    bytes_written: u64,
    failed_after_connect: bool,
) -> AttemptOutcome {
    if fresh_connection && bytes_written == 0 && !failed_after_connect {
        AttemptOutcome::NoExecutionProven {
            witness: ZeroBytesWitness::FreshConnectionZeroBytes,
        }
    } else {
        AttemptOutcome::UnknownAfterSend
    }
}

/// One classified dispatch attempt.
#[derive(Debug)]
pub struct ClassifiedAttempt<T> {
    pub outcome: AttemptOutcome,
    pub response: Option<T>,
    pub bytes: u64,
    pub error: Option<GateError>,
}

/// Convenience alias for connector tool attempts.
pub type ToolClassifiedAttempt = ClassifiedAttempt<(crate::proxy::ProxyResponse, ToolOutcome)>;

#[derive(Debug)]
pub enum FailoverRun<T> {
    Completed {
        response: Option<T>,
        permit: Permit,
        journal: AttemptJournal,
    },
    Refused(AdmissionOutcome),
    Stopped {
        reason: StopReason,
        permit: Option<Permit>,
        journal: AttemptJournal,
        response: Option<T>,
        error: Option<GateError>,
    },
}

pub struct FailoverEngine<'a> {
    pub governor: &'a GateGovernor,
}

impl<'a> FailoverEngine<'a> {
    pub fn new(governor: &'a GateGovernor) -> Self {
        Self { governor }
    }

    /// Run failover over `dispatch`. One reservation sized
    /// `envelope.reserved_units` is taken up front; settlement happens
    /// exactly once: Executed->Executed/complete, Unknown->Unknown/
    /// incomplete, all-NoExecutionProven exhausted->NeverDispatched.
    /// Drift: `fingerprint_source` is re-read before EVERY dispatch.
    #[allow(clippy::too_many_arguments)]
    pub fn run<T, F>(
        &self,
        set: &EndpointSet,
        envelope: &ParentEnvelope,
        req: &AdmissionRequest,
        logical_operation_id: &str,
        idempotency_key: Option<&str>,
        fingerprint_at_admission: Option<&PolicyFingerprint>,
        fingerprint_source: &dyn Fn() -> PolicyFingerprint,
        now_fn: &dyn Fn() -> u64,
        dispatch: F,
    ) -> FailoverRun<T>
    where
        F: FnMut(&str, u64, Option<&str>) -> ClassifiedAttempt<T>,
    {
        let mut journal = AttemptJournal::new();
        self.run_with_journal(
            set,
            envelope,
            req,
            logical_operation_id,
            idempotency_key,
            fingerprint_at_admission,
            fingerprint_source,
            now_fn,
            &mut journal,
            dispatch,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_with_journal<T, F>(
        &self,
        set: &EndpointSet,
        envelope: &ParentEnvelope,
        req: &AdmissionRequest,
        logical_operation_id: &str,
        idempotency_key: Option<&str>,
        fingerprint_at_admission: Option<&PolicyFingerprint>,
        fingerprint_source: &dyn Fn() -> PolicyFingerprint,
        now_fn: &dyn Fn() -> u64,
        journal: &mut dyn JournalStore,
        dispatch: F,
    ) -> FailoverRun<T>
    where
        F: FnMut(&str, u64, Option<&str>) -> ClassifiedAttempt<T>,
    {
        if !self.governor.is_policy_approved(set) {
            return FailoverRun::Refused(AdmissionOutcome::Denied {
                reason: format!("unapproved_policy_digest:{}", set.computed_policy_digest()),
            });
        }
        if journal.has_uncertain(logical_operation_id) {
            return FailoverRun::Refused(AdmissionOutcome::Denied {
                reason: format!("uncertain_prior_attempt:{logical_operation_id}"),
            });
        }
        let now0 = now_fn();
        let permit = match self
            .governor
            .admit_with_reserved(req, now0, envelope.reserved_units)
        {
            AdmissionOutcome::Admitted(p) => p,
            other => return FailoverRun::Refused(other),
        };
        if now0 >= permit.deadline_mono_ms {
            self.governor
                .settle_permit(&permit, 0, SettledOutcome::NeverDispatched, true);
            return FailoverRun::Stopped {
                reason: StopReason::DeadlineExpired,
                permit: Some(permit),
                journal: snapshot(journal, logical_operation_id),
                response: None,
                error: None,
            };
        }
        match self.run_dispatch_only(
            set,
            envelope,
            logical_operation_id,
            idempotency_key,
            fingerprint_at_admission,
            fingerprint_source,
            now_fn,
            journal,
            dispatch,
        ) {
            DispatchOnlyOutcome::Executed {
                response,
                bytes,
                journal: j,
            } => {
                self.governor
                    .settle_permit(&permit, bytes, SettledOutcome::Executed, true);
                FailoverRun::Completed {
                    response,
                    permit,
                    journal: j,
                }
            }
            DispatchOnlyOutcome::Unknown { journal: j } => {
                self.governor.settle_permit(
                    &permit,
                    envelope.reserved_units,
                    SettledOutcome::UnknownAfterSend,
                    false,
                );
                FailoverRun::Stopped {
                    reason: StopReason::UnknownAfterSendTerminal,
                    permit: Some(permit),
                    journal: j,
                    response: None,
                    error: Some(GateError::UpstreamHttp {
                        cause: UNKNOWN_AFTER_SEND_TEXT.into(),
                    }),
                }
            }
            DispatchOnlyOutcome::AllNoExecutionProven {
                journal: j,
                error,
                reason,
            } => {
                self.governor
                    .settle_permit(&permit, 0, SettledOutcome::NeverDispatched, true);
                FailoverRun::Stopped {
                    reason,
                    permit: Some(permit),
                    journal: j,
                    response: None,
                    error,
                }
            }
        }
    }

    /// Dispatch-only loop: no admission/settlement. Used inside
    /// `governed_dispatch` so the replay guard wraps the whole failover.
    #[allow(clippy::too_many_arguments)]
    pub fn run_dispatch_only<T, F>(
        &self,
        set: &EndpointSet,
        envelope: &ParentEnvelope,
        logical_operation_id: &str,
        idempotency_key: Option<&str>,
        fingerprint_at_admission: Option<&PolicyFingerprint>,
        fingerprint_source: &dyn Fn() -> PolicyFingerprint,
        now_fn: &dyn Fn() -> u64,
        journal: &mut dyn JournalStore,
        mut dispatch: F,
    ) -> DispatchOnlyOutcome<T>
    where
        F: FnMut(&str, u64, Option<&str>) -> ClassifiedAttempt<T>,
    {
        if !self.governor.is_policy_approved(set) {
            return DispatchOnlyOutcome::AllNoExecutionProven {
                journal: snapshot(journal, logical_operation_id),
                error: Some(GateError::PolicyDenied {
                    secret: "failover".into(),
                    reason: format!("unapproved_policy_digest:{}", set.computed_policy_digest()),
                }),
                reason: StopReason::UnapprovedPolicyDigest,
            };
        }
        if journal.has_uncertain(logical_operation_id) {
            return DispatchOnlyOutcome::AllNoExecutionProven {
                journal: snapshot(journal, logical_operation_id),
                error: Some(GateError::PolicyDenied {
                    secret: "failover".into(),
                    reason: format!("uncertain_prior_attempt:{logical_operation_id}"),
                }),
                reason: StopReason::UncertainPriorAttempt,
            };
        }
        let per_attempt_worst = if envelope.max_attempts == 0 {
            envelope.reserved_units
        } else {
            envelope.reserved_units / u64::from(envelope.max_attempts)
        };
        let mut last_error: Option<GateError> = None;
        loop {
            let now = now_fn();
            // Live drift: re-read the source before EVERY attempt dispatch.
            // Drift vs the admitted fingerprint stops further attempts;
            // prior journal entries are untouched and this is never
            // settled as executed (caller settles NeverDispatched).
            let current = fingerprint_source();
            if let Some(a) = fingerprint_at_admission
                && drift_invalidates(a, &current)
            {
                return DispatchOnlyOutcome::AllNoExecutionProven {
                    journal: snapshot(journal, logical_operation_id),
                    error: last_error,
                    reason: StopReason::DriftInvalidated,
                };
            }
            let snap = snapshot(journal, logical_operation_id);
            let action = FailoverDecision::next_action(
                set,
                &snap,
                envelope,
                now,
                idempotency_key,
                None,
                None,
            );
            let (endpoint_index, key) = match action {
                FailoverAction::Attempt {
                    endpoint_index,
                    idempotency_key: k,
                } => (endpoint_index, k),
                FailoverAction::Stop { reason } => {
                    return DispatchOnlyOutcome::AllNoExecutionProven {
                        journal: snapshot(journal, logical_operation_id),
                        error: last_error,
                        reason,
                    };
                }
            };
            let endpoint = set.endpoints[endpoint_index].clone();
            let attempt_index = snap.attempts_used();
            if journal
                .begin(AttemptJournalEntry {
                    logical_operation_id: logical_operation_id.to_string(),
                    attempt_index,
                    endpoint: endpoint.clone(),
                    state: AttemptState::Dispatching,
                    at_ms: now,
                })
                .is_err()
            {
                return DispatchOnlyOutcome::AllNoExecutionProven {
                    journal: snapshot(journal, logical_operation_id),
                    error: last_error,
                    reason: StopReason::AttemptInFlight,
                };
            }
            let Some(per_try) = envelope.per_try_timeout_ms(now_fn()) else {
                let _ = journal.finish(
                    logical_operation_id,
                    attempt_index,
                    AttemptOutcome::NoExecutionProven {
                        witness: ZeroBytesWitness::PreDispatchRefusal,
                    },
                );
                journal.record_spend(per_attempt_worst);
                return DispatchOnlyOutcome::AllNoExecutionProven {
                    journal: snapshot(journal, logical_operation_id),
                    error: last_error,
                    reason: StopReason::DeadlineExpired,
                };
            };
            let key_ref = key.as_deref().or(idempotency_key);
            let attempt = dispatch(&endpoint, per_try, key_ref);
            let outcome = attempt.outcome;
            // Unknown outcomes never surface the raw upstream error/body.
            if !matches!(outcome, AttemptOutcome::UnknownAfterSend) && attempt.error.is_some() {
                last_error = attempt.error;
            }
            let _ = journal.finish(logical_operation_id, attempt_index, outcome);
            journal.record_spend(per_attempt_worst);
            match outcome {
                AttemptOutcome::Executed => {
                    return DispatchOnlyOutcome::Executed {
                        response: attempt.response,
                        bytes: attempt.bytes,
                        journal: snapshot(journal, logical_operation_id),
                    };
                }
                AttemptOutcome::UnknownAfterSend => {
                    return DispatchOnlyOutcome::Unknown {
                        journal: snapshot(journal, logical_operation_id),
                    };
                }
                AttemptOutcome::NoExecutionProven { .. } => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::governor::{
        GovernorPolicy, GovernorStore, MemGovernorStore, Principal, SideEffectClass,
    };
    use std::sync::Arc;

    fn set() -> EndpointSet {
        EndpointSet {
            connector_id: "c1".into(),
            tool: "t1".into(),
            endpoints: vec!["https://a.example/x".into(), "https://b.example/x".into()],
            side_effect_class: SideEffectClass::SideEffectful,
            idempotency_conformance_test: None,
            owner_approved: true,
            policy_digest: "pd1".into(),
        }
    }
    fn gov_with_budget(budget: u64) -> (GateGovernor, Arc<MemGovernorStore>) {
        let policy = GovernorPolicy {
            budget_units_per_scope: budget,
            ..Default::default()
        };
        let store = Arc::new(MemGovernorStore::new(policy));
        let mut approved = std::collections::HashSet::new();
        approved.insert(set().computed_policy_digest());
        let mut set3 = set();
        set3.endpoints.push("https://c.example/x".into());
        approved.insert(set3.computed_policy_digest());
        let mut idem = set();
        idem.side_effect_class = SideEffectClass::IdempotentWithKey;
        idem.idempotency_conformance_test = Some("idem_conformance_v1".into());
        approved.insert(idem.computed_policy_digest());
        (
            GateGovernor {
                store: Some(store.clone()),
                policy: GovernorPolicy::default(),
                aliases: vec![],
                approved_policy_digests: approved,
            },
            store,
        )
    }
    fn req() -> AdmissionRequest {
        AdmissionRequest::new(Principal::Owner, "proj", "a.example/x", 1)
    }
    fn dummy_fp() -> PolicyFingerprint {
        PolicyFingerprint {
            contract_digest: "cd".into(),
            policy_digest: "pd".into(),
            credential_scope: "s".into(),
        }
    }
    fn attempt(outcome: AttemptOutcome) -> ClassifiedAttempt<ToolOutcome> {
        ClassifiedAttempt {
            outcome,
            response: None,
            bytes: 0,
            error: None,
        }
    }

    #[test]
    fn failover_on_fresh_connection_no_execution_witness() {
        let (gov, _store) = gov_with_budget(10_000);
        let s = set();
        let envelope = ParentEnvelope::build(2, s.endpoints.len(), 10_000, 10).unwrap();
        let order = std::cell::RefCell::new(Vec::new());
        let mut calls = 0;
        let engine = FailoverEngine::new(&gov);
        let run = engine.run(
            &s,
            &envelope,
            &req(),
            "op1",
            None,
            None,
            &dummy_fp,
            &|| 1_000,
            |endpoint, _timeout, _key| {
                calls += 1;
                order.borrow_mut().push(format!("dispatch:{endpoint}"));
                if calls == 1 {
                    ClassifiedAttempt {
                        outcome: classify_transport(true, 0, false),
                        response: None,
                        bytes: 0,
                        error: None,
                    }
                } else {
                    ClassifiedAttempt {
                        outcome: AttemptOutcome::Executed,
                        response: Some(ToolOutcome::Uncontracted(serde_json::json!({"ok":true}))),
                        bytes: 5,
                        error: None,
                    }
                }
            },
        );
        match run {
            FailoverRun::Completed {
                response, journal, ..
            } => {
                assert!(matches!(response, Some(ToolOutcome::Uncontracted(_))));
                // journal order: begin/finish ep1, begin/finish ep2
                let states: Vec<_> = journal
                    .entries()
                    .iter()
                    .map(|e| (e.attempt_index, e.state, e.endpoint.clone()))
                    .collect();
                assert_eq!(states.len(), 2);
                assert_eq!(states[0].1, AttemptState::NoExecutionProven);
                assert_eq!(states[1].1, AttemptState::Executed);
                assert!(states[0].2.contains("a.example"));
                assert!(states[1].2.contains("b.example"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert_eq!(calls, 2);
    }

    #[test]
    fn broker_refusal_releases_governor_reservation() {
        // Pre-dispatch (broker-style) refusal on every endpoint:
        // NoExecutionProven(PreDispatchRefusal) -> exhausted ->
        // NeverDispatched settlement releases the parent reservation fully.
        let (gov, store) = gov_with_budget(20);
        let s = set();
        let envelope = ParentEnvelope::build(2, s.endpoints.len(), 10_000, 10).unwrap();
        assert_eq!(envelope.reserved_units, 20);
        let engine = FailoverEngine::new(&gov);
        let run = engine.run(
            &s,
            &envelope,
            &req(),
            "op1",
            None,
            None,
            &dummy_fp,
            &|| 1_000,
            |_e, _t, _k| ClassifiedAttempt {
                outcome: AttemptOutcome::NoExecutionProven {
                    witness: ZeroBytesWitness::PreDispatchRefusal,
                },
                response: None::<ToolOutcome>,
                bytes: 0,
                error: Some(GateError::InvalidToolArgs {
                    tool: "t1".into(),
                    reason: "bad".into(),
                }),
            },
        );
        assert!(
            matches!(
                run,
                FailoverRun::Stopped {
                    reason: StopReason::AttemptsExhausted,
                    ..
                }
            ),
            "{run:?}"
        );
        assert!(store.unconfirmed_permits().is_empty());
        // Budget fully restored: a fresh full-size reservation is admitted.
        assert!(matches!(
            gov.admit_with_reserved(&req(), 2_000, 20),
            AdmissionOutcome::Admitted(_)
        ));
        // Admission refusal before dispatch also leaves no reservation.
        let (gov2, store2) = gov_with_budget(5);
        let engine2 = FailoverEngine::new(&gov2);
        let refused = engine2.run(
            &s,
            &envelope,
            &req(),
            "op2",
            None,
            None,
            &dummy_fp,
            &|| 1_000,
            |_e, _t, _k| attempt(AttemptOutcome::Executed),
        );
        assert!(matches!(
            refused,
            FailoverRun::Refused(AdmissionOutcome::Denied { .. })
        ));
        assert!(store2.unconfirmed_permits().is_empty());
    }

    #[test]
    fn parent_reservation_exhaustion_stops_failover() {
        // Envelope reserved 20 (2 attempts x 10) but governor budget admits it;
        // after 2 NoExecutionProven attempts spend is exhausted -> stop.
        // Directly exercise decision via engine with 3 endpoints would exceed;
        // here exhaustion is via attempts, and spent accounting is asserted.
        let (gov, _store) = gov_with_budget(10_000);
        let mut s = set();
        s.endpoints.push("https://c.example/x".into());
        let envelope = ParentEnvelope::build(2, s.endpoints.len(), 10_000, 10).unwrap();
        let engine = FailoverEngine::new(&gov);
        let run = engine.run(
            &s,
            &envelope,
            &req(),
            "op1",
            None,
            None,
            &dummy_fp,
            &|| 1_000,
            |_e, _t, _k| {
                attempt(AttemptOutcome::NoExecutionProven {
                    witness: ZeroBytesWitness::FreshConnectionZeroBytes,
                })
            },
        );
        match run {
            FailoverRun::Stopped {
                journal, reason, ..
            } => {
                assert_eq!(journal.spent_units(), 20);
                assert!(
                    matches!(
                        reason,
                        StopReason::AttemptsExhausted | StopReason::ReservationExhausted
                    ),
                    "{reason:?}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_after_send_is_terminal_and_unconfirmed() {
        let (gov, store) = gov_with_budget(10_000);
        let s = set();
        let envelope = ParentEnvelope::build(2, s.endpoints.len(), 10_000, 10).unwrap();
        let engine = FailoverEngine::new(&gov);
        let mut calls = 0;
        let run = engine.run(
            &s,
            &envelope,
            &req(),
            "op1",
            None,
            None,
            &dummy_fp,
            &|| 1_000,
            |_e, _t, _k| {
                calls += 1;
                attempt(classify_transport(false, 0, true))
            },
        );
        assert_eq!(calls, 1, "unknown must never replay to next endpoint");
        assert!(matches!(
            run,
            FailoverRun::Stopped {
                reason: StopReason::UnknownAfterSendTerminal,
                ..
            }
        ));
        assert_eq!(
            store.unconfirmed_permits().len(),
            1,
            "unknown stays counted"
        );
    }

    #[test]
    fn drift_refuses_new_dispatch_before_first_attempt() {
        let (gov, store) = gov_with_budget(10_000);
        let s = set();
        let envelope = ParentEnvelope::build(2, s.endpoints.len(), 10_000, 10).unwrap();
        let a = PolicyFingerprint {
            contract_digest: "cd".into(),
            policy_digest: "pd1".into(),
            credential_scope: "s".into(),
        };
        let b = PolicyFingerprint {
            policy_digest: "pd2".into(),
            ..a.clone()
        };
        let engine = FailoverEngine::new(&gov);
        let called = std::cell::Cell::new(false);
        let run = engine.run(
            &s,
            &envelope,
            &req(),
            "op1",
            None,
            Some(&a),
            &|| b.clone(),
            &|| 1_000,
            |_e, _t, _k| {
                called.set(true);
                attempt(AttemptOutcome::Executed)
            },
        );
        assert!(!called.get());
        assert!(matches!(
            run,
            FailoverRun::Stopped {
                reason: StopReason::DriftInvalidated,
                ..
            }
        ));
        assert!(store.unconfirmed_permits().is_empty());
    }

    #[test]
    fn unapproved_computed_digest_typed_refusal_and_forged_digest_ignored() {
        let (mut gov, _store) = gov_with_budget(10_000);
        gov.approved_policy_digests.clear();
        // Even with owner_approved=true and a stored digest claiming approval.
        let mut s = set();
        s.policy_digest = "owner-approved".into();
        let envelope = ParentEnvelope::build(2, s.endpoints.len(), 10_000, 10).unwrap();
        let engine = FailoverEngine::new(&gov);
        let called = std::cell::Cell::new(false);
        let run = engine.run(
            &s,
            &envelope,
            &req(),
            "op-unapproved",
            None,
            None,
            &dummy_fp,
            &|| 1_000,
            |_e, _t, _k| {
                called.set(true);
                attempt(AttemptOutcome::Executed)
            },
        );
        assert!(!called.get());
        match run {
            FailoverRun::Refused(AdmissionOutcome::Denied { reason }) => {
                assert!(reason.starts_with("unapproved_policy_digest:"), "{reason}");
                assert!(reason.contains(&s.computed_policy_digest()), "{reason}");
            }
            other => panic!("expected typed refusal, got {other:?}"),
        }
    }

    #[test]
    fn idempotent_key_identical_on_first_and_failover_attempts() {
        let (gov, _store) = gov_with_budget(10_000);
        let mut s = set();
        s.side_effect_class = SideEffectClass::IdempotentWithKey;
        s.idempotency_conformance_test = Some("idem_conformance_v1".into());
        let envelope = ParentEnvelope::build(2, s.endpoints.len(), 10_000, 10).unwrap();
        let keys = std::cell::RefCell::new(Vec::new());
        let mut calls = 0;
        let engine = FailoverEngine::new(&gov);
        let run = engine.run(
            &s,
            &envelope,
            &req(),
            "op-idem",
            Some("op-key-123"),
            None,
            &dummy_fp,
            &|| 1_000,
            |_e, _t, key| {
                calls += 1;
                keys.borrow_mut().push(key.map(str::to_string));
                if calls == 1 {
                    attempt(AttemptOutcome::NoExecutionProven {
                        witness: ZeroBytesWitness::FreshConnectionZeroBytes,
                    })
                } else {
                    attempt(AttemptOutcome::Executed)
                }
            },
        );
        assert!(matches!(run, FailoverRun::Completed { .. }), "{run:?}");
        assert_eq!(
            *keys.borrow(),
            vec![
                Some("op-key-123".to_string()),
                Some("op-key-123".to_string())
            ]
        );
    }

    #[test]
    fn unknown_after_send_typed_surface_never_raw_upstream() {
        let (gov, _store) = gov_with_budget(10_000);
        let s = set();
        let envelope = ParentEnvelope::build(2, s.endpoints.len(), 10_000, 10).unwrap();
        let engine = FailoverEngine::new(&gov);
        let run = engine.run(
            &s,
            &envelope,
            &req(),
            "op-unknown",
            None,
            None,
            &dummy_fp,
            &|| 1_000,
            |_e, _t, _k| ClassifiedAttempt {
                outcome: AttemptOutcome::UnknownAfterSend,
                response: None::<ToolOutcome>,
                bytes: 0,
                error: Some(GateError::UpstreamHttp {
                    cause: "RAW_SECRET_BODY_xyz".into(),
                }),
            },
        );
        match run {
            FailoverRun::Stopped { error: Some(e), .. } => {
                let text = format!("{e}");
                assert!(text.contains(UNKNOWN_AFTER_SEND_TEXT), "{text}");
                assert!(!text.contains("RAW_SECRET_BODY_xyz"), "{text}");
            }
            other => panic!("expected typed unknown stop, got {other:?}"),
        }
    }

    #[test]
    fn persistent_journal_crash_dispatching_refuses_same_logical_id() {
        let (_gov, store) = gov_with_budget(10_000);
        let backend: Arc<dyn GovernorStore> = store.clone();
        {
            let mut j = PersistentFailoverJournal::new(backend.clone()).unwrap();
            j.begin(AttemptJournalEntry {
                logical_operation_id: "op-crash".into(),
                attempt_index: 0,
                endpoint: "https://a.example/x".into(),
                state: AttemptState::Dispatching,
                at_ms: 100,
            })
            .unwrap();
            // No finish: simulated crash.
        }
        // New journal instance over the same backend: uncertain refusal.
        let mut j2 = PersistentFailoverJournal::new(backend.clone()).unwrap();
        assert!(j2.has_uncertain_for("op-crash"));
        let (gov, _s2) = gov_with_budget(10_000);
        let s = set();
        let envelope = ParentEnvelope::build(2, s.endpoints.len(), 10_000, 10).unwrap();
        let engine = FailoverEngine::new(&gov);
        let out = engine.run_dispatch_only(
            &s,
            &envelope,
            "op-crash",
            None,
            None,
            &dummy_fp,
            &|| 1_000,
            &mut j2,
            |_e, _t, _k| attempt(AttemptOutcome::Executed),
        );
        match out {
            DispatchOnlyOutcome::AllNoExecutionProven { reason, error, .. } => {
                assert_eq!(reason, StopReason::UncertainPriorAttempt);
                assert!(
                    format!("{}", error.unwrap()).contains("uncertain_prior_attempt"),
                    "typed uncertain refusal"
                );
            }
            _ => panic!("expected uncertain refusal"),
        }
        // Journal records the endpoint URL.
        assert_eq!(j2.entries()[0].endpoint, "https://a.example/x");
    }

    #[test]
    fn fingerprint_digests_cover_contract_policy_and_credential_scope() {
        use crate::contract::{ResponseContract, ResponseSchema};
        let c1 = ResponseContract {
            schema: ResponseSchema::from_value(serde_json::json!({"type":"object","additionalProperties":false,"required":["id"],"properties":{"id":{"type":"string"}}})).unwrap(),
            max_response_bytes: 1024,
            max_response_tokens: None,
            max_depth: 4,
            max_array_len: 10,
            max_string_len: 100,
        };
        let c2 = ResponseContract {
            schema: ResponseSchema::from_value(serde_json::json!({"type":"object","additionalProperties":false,"required":["name"],"properties":{"name":{"type":"string"}}})).unwrap(),
            ..c1.clone()
        };
        assert_ne!(
            crate::governor::contract_digest(Some(&c1)),
            crate::governor::contract_digest(Some(&c2))
        );
        assert_ne!(
            crate::governor::credential_scope_digest(&["a.example".into()]),
            crate::governor::credential_scope_digest(&["b.example".into()])
        );
        let s = set();
        assert_eq!(
            crate::governor::credential_scope_digest(&["a.example".into(), "b.example".into()]),
            crate::governor::credential_scope_digest(&["a.example".into(), "b.example".into()])
        );
        let _ = s.computed_policy_digest();
    }

    #[test]
    fn fingerprint_flips_after_attempt1_attempt2_never_dispatched() {
        let (gov, store) = gov_with_budget(10_000);
        let s = set();
        let envelope = ParentEnvelope::build(2, s.endpoints.len(), 10_000, 10).unwrap();
        let a = dummy_fp();
        let b = PolicyFingerprint {
            policy_digest: "pd-flipped".into(),
            ..a.clone()
        };
        let calls = std::cell::Cell::new(0u32);
        let engine = FailoverEngine::new(&gov);
        let run = engine.run(
            &s,
            &envelope,
            &req(),
            "op-drift-live",
            None,
            Some(&a),
            &|| {
                if calls.get() >= 1 {
                    b.clone()
                } else {
                    a.clone()
                }
            },
            &|| 1_000,
            |_e, _t, _k| {
                calls.set(calls.get() + 1);
                attempt(AttemptOutcome::NoExecutionProven {
                    witness: ZeroBytesWitness::FreshConnectionZeroBytes,
                })
            },
        );
        assert_eq!(calls.get(), 1, "attempt 2 must never be dispatched");
        match run {
            FailoverRun::Stopped {
                reason, journal, ..
            } => {
                assert_eq!(reason, StopReason::DriftInvalidated);
                assert_eq!(journal.entries().len(), 1);
                assert_eq!(journal.entries()[0].state, AttemptState::NoExecutionProven);
            }
            other => panic!("expected drift stop, got {other:?}"),
        }
        assert!(
            store.unconfirmed_permits().is_empty(),
            "drift must not settle as executed"
        );
    }

    #[test]
    fn approval_revocation_after_attempt1_attempt2_never_dispatched() {
        // Env-independent closure simulating the production mechanism:
        // same fingerprint except the approval marker in `policy_digest`
        // (`approval_marker_digest(computed, approved)`), flipped off
        // after attempt 1 by the live source.
        let (gov, store) = gov_with_budget(10_000);
        let s = set();
        let envelope = ParentEnvelope::build(2, s.endpoints.len(), 10_000, 10).unwrap();
        let computed = s.computed_policy_digest();
        let a = PolicyFingerprint {
            contract_digest: "cd".into(),
            policy_digest: crate::governor::approval_marker_digest(&computed, true),
            credential_scope: "s".into(),
        };
        let revoked = PolicyFingerprint {
            policy_digest: crate::governor::approval_marker_digest(&computed, false),
            ..a.clone()
        };
        assert!(fl_core::drift_invalidates(&a, &revoked));
        let calls = std::cell::Cell::new(0u32);
        let engine = FailoverEngine::new(&gov);
        let run = engine.run(
            &s,
            &envelope,
            &req(),
            "op-approval-revoked",
            None,
            Some(&a),
            &|| {
                if calls.get() >= 1 {
                    revoked.clone()
                } else {
                    a.clone()
                }
            },
            &|| 1_000,
            |_e, _t, _k| {
                calls.set(calls.get() + 1);
                attempt(AttemptOutcome::NoExecutionProven {
                    witness: ZeroBytesWitness::FreshConnectionZeroBytes,
                })
            },
        );
        assert_eq!(calls.get(), 1, "attempt 2 must never be dispatched");
        match run {
            FailoverRun::Stopped {
                reason, journal, ..
            } => {
                assert_eq!(reason, StopReason::DriftInvalidated);
                assert_eq!(journal.entries().len(), 1);
            }
            other => panic!("expected drift stop, got {other:?}"),
        }
        assert!(store.unconfirmed_permits().is_empty());
    }

    #[test]
    fn dispatching_survives_journal_cap_pressure() {
        let (_gov, store) = gov_with_budget(10_000);
        let backend: Arc<dyn GovernorStore> = store;
        let mut j = PersistentFailoverJournal::new(backend).unwrap();
        // Fill to cap with finished entries.
        for i in 0..FAILOVER_JOURNAL_CAP {
            let id = format!("op-{i}");
            j.begin(AttemptJournalEntry {
                logical_operation_id: id.clone(),
                attempt_index: 0,
                endpoint: "https://a.example/x".into(),
                state: AttemptState::Dispatching,
                at_ms: 1,
            })
            .unwrap();
            j.finish(&id, 0, AttemptOutcome::Executed).unwrap();
        }
        j.begin(AttemptJournalEntry {
            logical_operation_id: "op-live".into(),
            attempt_index: 0,
            endpoint: "https://a.example/x".into(),
            state: AttemptState::Dispatching,
            at_ms: 2,
        })
        .unwrap();
        for i in 0..10 {
            let id = format!("op-more-{i}");
            j.begin(AttemptJournalEntry {
                logical_operation_id: id.clone(),
                attempt_index: 0,
                endpoint: "https://a.example/x".into(),
                state: AttemptState::Dispatching,
                at_ms: 3,
            })
            .unwrap();
            j.finish(&id, 0, AttemptOutcome::Executed).unwrap();
        }
        assert!(
            j.has_uncertain_for("op-live"),
            "Dispatching must survive cap"
        );
        assert_eq!(j.entries().len(), FAILOVER_JOURNAL_CAP);
        // Full of unfinished -> JournalUnavailable, nothing evicted.
        let mut j2 = PersistentFailoverJournal::new(Arc::new(MemGovernorStore::new(
            GovernorPolicy::default(),
        )))
        .unwrap();
        for i in 0..FAILOVER_JOURNAL_CAP {
            j2.begin(AttemptJournalEntry {
                logical_operation_id: format!("op-u-{i}"),
                attempt_index: 0,
                endpoint: "https://a.example/x".into(),
                state: AttemptState::Dispatching,
                at_ms: 1,
            })
            .unwrap();
        }
        assert_eq!(
            j2.begin(AttemptJournalEntry {
                logical_operation_id: "op-overflow".into(),
                attempt_index: 0,
                endpoint: "https://a.example/x".into(),
                state: AttemptState::Dispatching,
                at_ms: 1,
            }),
            Err(FailoverError::JournalUnavailable)
        );
        assert_eq!(j2.entries().len(), FAILOVER_JOURNAL_CAP);
    }

    #[test]
    fn classify_transport_rules() {
        assert_eq!(
            classify_transport(true, 0, false),
            AttemptOutcome::NoExecutionProven {
                witness: ZeroBytesWitness::FreshConnectionZeroBytes
            }
        );
        assert_eq!(
            classify_transport(false, 0, false),
            AttemptOutcome::UnknownAfterSend
        );
        assert_eq!(
            classify_transport(true, 1, false),
            AttemptOutcome::UnknownAfterSend
        );
        assert_eq!(
            classify_transport(true, 0, true),
            AttemptOutcome::UnknownAfterSend
        );
    }
}
