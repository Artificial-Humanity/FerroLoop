//! Shared control-ledger contract cases for memory and persistent backends.
use crate::routine::*;
use crate::routine_store::*;
use std::sync::{Arc, Barrier};

fn spec() -> RoutineSpec {
    let mut spec = RoutineSpec {
        routine_id: "r".into(),
        revision: 1,
        project: "p".into(),
        schedule: Schedule::Interval {
            every_secs: 3600,
            anchor_utc: "2026-01-01T00:00:00Z".into(),
        },
        work: RoutineWork::ModelTurn {
            prompt_template_digest: "digest".into(),
            workflow_step: None,
            target: RoutineTarget::FreshSession {
                template_id: "t".into(),
            },
        },
        allocation: Allocation {
            per_firing_tokens: 100,
            per_firing_cost_micros: 100,
            window: Window::Day,
            window_tokens: 100,
            window_cost_micros: 100,
        },
        concurrency: Concurrency::Forbid,
        catch_up: CatchUp::Skip,
        misfire_grace_secs: 300,
        max_firing_secs: 3600,
        max_firings_per_day: 24,
        expires_at: "2026-01-04T00:00:00Z".into(),
        state: RoutineState::Approved,
        approval: Some(Approval {
            approved_by: "owner".into(),
            approved_at: "2026-01-01T00:00:00Z".into(),
            revision: 1,
            digest: String::new(),
        }),
    };
    spec.approval.as_mut().unwrap().digest = routine_approval_digest(&spec).unwrap();
    spec
}
fn setup<S: RoutineStore>(s: &S) {
    s.routine_transaction(RoutineTransaction::InsertRoutine(spec()))
        .unwrap();
    s.routine_transaction(RoutineTransaction::SetSchedulerEpoch {
        expected: 0,
        next: 1,
    })
    .unwrap();
    s.routine_transaction(RoutineTransaction::OpenWindow {
        id: "window".into(),
        routine_id: "r".into(),
        at_utc: "2026-01-01T00:00:00Z".into(),
        expires_at: "2026-01-02T00:00:00Z".into(),
    })
    .unwrap();
}
fn request() -> AdmissionRequest {
    let instant = "2026-01-01T01:00:00Z";
    AdmissionRequest {
        routine_id: "r".into(),
        expected_revision: 1,
        approval_digest: routine_approval_digest(&spec()).unwrap(),
        scheduler_epoch: 1,
        firing: FiringRecord {
            firing_key: firing_key("r", instant).unwrap(),
            routine_id: "r".into(),
            revision: 1,
            scheduled_instant_utc: instant.into(),
            nominal_slots: vec!["2026-01-01T01:00:00Z".into()],
            coalesced: false,
            state: FiringState::Planned,
        },
        window_id: "window".into(),
        now_utc: instant.into(),
        next_instant_utc: "2026-01-01T02:00:00Z".into(),
        deadline_utc: "2026-01-01T02:00:00Z".into(),
        envelope_id: "root".into(),
        envelope_settlement_id: "settle-root".into(),
        reservation_id: "execution".into(),
        reservation_settlement_id: "settle-execution".into(),
        outbox_id: "outbox".into(),
        proof_record_id: "proof".into(),
        execution_bound: Funds {
            tokens: 20,
            cost_micros: 20,
        },
        controller: None,
        host_spawn_token: Some("spawn".into()),
    }
}
fn admit<S: RoutineStore>(s: &S) {
    s.routine_transaction(RoutineTransaction::CommitAdmission(request()))
        .unwrap();
}
fn child(id: &str, tokens: u64) -> ChildRequest {
    ChildRequest {
        id: id.into(),
        settlement_id: format!("settle-{id}"),
        amount: Funds {
            tokens,
            cost_micros: tokens,
        },
        deadline_utc: "2026-01-01T01:59:00Z".into(),
    }
}
fn activate<S: RoutineStore>(s: &S) {
    s.routine_transaction(RoutineTransaction::AcceptEffect {
        outbox_id: "outbox".into(),
        scheduler_epoch: 1,
        now_utc: "2026-01-01T01:01:00Z".into(),
    })
    .unwrap();
}
fn carve<S: RoutineStore + IntentStore>(s: &S, id: &str, n: u64) {
    if s.control_snapshot().unwrap().outbox["outbox"].ticket == TicketState::Issued {
        activate(s);
    }
    s.intent_transaction(IntentTransaction::Carve {
        parent_outbox_id: "outbox".into(),
        scheduler_epoch: 1,
        now_utc: "2026-01-01T01:01:00Z".into(),
        parent_id: "root".into(),
        children: vec![child(id, n)],
        committee: false,
    })
    .unwrap();
}
fn settle_child<S: IntentStore>(s: &S, id: &str) {
    s.intent_transaction(IntentTransaction::SettleAllocation {
        allocation_id: id.into(),
        settlement_id: format!("settle-{id}"),
        now_utc: "2026-01-01T01:10:00Z".into(),
    })
    .unwrap();
}
fn revoke<S: RoutineStore>(s: &S) {
    s.routine_transaction(RoutineTransaction::RevokeEffect {
        outbox_id: "outbox".into(),
        scheduler_epoch: 1,
    })
    .unwrap();
}
fn settle_exec<S: IntentStore>(s: &S, resolution: Resolution) {
    s.intent_transaction(IntentTransaction::SettleReservation {
        reservation_id: "execution".into(),
        settlement_id: "settle-execution".into(),
        resolution,
    })
    .unwrap();
}
fn settle_root<S: IntentStore>(s: &S, now: &str) {
    s.intent_transaction(IntentTransaction::SettleAllocation {
        allocation_id: "root".into(),
        settlement_id: "settle-root".into(),
        now_utc: now.into(),
    })
    .unwrap();
}

pub fn concurrent_claim_same_firing_one_wins<S: RoutineStore + Send + Sync + 'static>(s: S) {
    setup(&s);
    let s = Arc::new(s);
    let barrier = Arc::new(Barrier::new(3));
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let (s, b) = (s.clone(), barrier.clone());
        jobs.push(std::thread::spawn(move || {
            b.wait();
            s.routine_transaction(RoutineTransaction::CommitAdmission(request()))
                .is_ok()
        }));
    }
    barrier.wait();
    assert_eq!(
        jobs.into_iter()
            .map(|j| j.join().unwrap())
            .filter(|x| *x)
            .count(),
        1
    );
    assert_eq!(s.control_snapshot().unwrap().firings.len(), 1);
}
pub fn stale_scheduler_lease_cannot_transition<S: RoutineStore>(s: S) {
    setup(&s);
    admit(&s);
    s.routine_transaction(RoutineTransaction::SetSchedulerEpoch {
        expected: 1,
        next: 2,
    })
    .unwrap();
    assert!(
        s.routine_transaction(RoutineTransaction::AcceptEffect {
            outbox_id: "outbox".into(),
            scheduler_epoch: 1,
            now_utc: "2026-01-01T01:01:00Z".into()
        })
        .is_err()
    );
    assert_eq!(
        s.control_snapshot().unwrap().outbox["outbox"].ticket,
        TicketState::Issued
    );
}
pub fn concurrent_reservations_cannot_overspend_window<
    S: RoutineStore + IntentStore + Send + Sync + 'static,
>(
    s: S,
) {
    setup(&s);
    admit(&s);
    activate(&s);
    let s = Arc::new(s);
    let b = Arc::new(Barrier::new(3));
    let mut jobs = Vec::new();
    for name in ["a", "b"] {
        let (s, b) = (s.clone(), b.clone());
        jobs.push(std::thread::spawn(move || {
            b.wait();
            s.intent_transaction(IntentTransaction::Carve {
                parent_outbox_id: "outbox".into(),
                scheduler_epoch: 1,
                now_utc: "2026-01-01T01:01:00Z".into(),
                parent_id: "root".into(),
                children: vec![child(name, 50)],
                committee: false,
            })
            .is_ok()
        }));
    }
    b.wait();
    assert_eq!(
        jobs.into_iter()
            .map(|j| j.join().unwrap())
            .filter(|x| *x)
            .count(),
        1
    );
    assert_eq!(
        s.control_snapshot().unwrap().allocations["root"]
            .remaining
            .tokens,
        30
    );
}
pub fn ambiguous_child_keeps_carve_out<S: RoutineStore + IntentStore>(s: S) {
    setup(&s);
    admit(&s);
    carve(&s, "a", 30);
    s.intent_transaction(IntentTransaction::Reserve {
        allocation_id: "a".into(),
        reservation_id: "c".into(),
        settlement_id: "c-settle".into(),
        amount: Funds {
            tokens: 10,
            cost_micros: 10,
        },
        deadline_utc: "2026-01-01T01:30:00Z".into(),
        effect: IntentEffect {
            outbox_id: "child-outbox".into(),
            proof_record_id: "proof".into(),
            scheduler_epoch: 1,
            now_utc: "2026-01-01T01:01:00Z".into(),
            controller: None,
            host_spawn_token: Some("child-token".into()),
        },
    })
    .unwrap();
    s.routine_transaction(RoutineTransaction::AcceptEffect {
        outbox_id: "child-outbox".into(),
        scheduler_epoch: 1,
        now_utc: "2026-01-01T01:02:00Z".into(),
    })
    .unwrap();
    s.routine_transaction(RoutineTransaction::MarkAmbiguous {
        outbox_id: "child-outbox".into(),
        scheduler_epoch: 1,
    })
    .unwrap();
    assert!(
        s.intent_transaction(IntentTransaction::SettleReservation {
            reservation_id: "c".into(),
            settlement_id: "c-settle".into(),
            resolution: Resolution::NeverDispatched
        })
        .is_err()
    );
    assert!(
        s.intent_transaction(IntentTransaction::SettleAllocation {
            allocation_id: "a".into(),
            settlement_id: "settle-a".into(),
            now_utc: "2026-01-01T01:10:00Z".into()
        })
        .is_err()
    );
    assert_eq!(
        s.control_snapshot().unwrap().allocations["root"]
            .remaining
            .tokens,
        50
    );
}
pub fn confirmed_undispatched_child_returns_carve_out<S: RoutineStore + IntentStore>(s: S) {
    setup(&s);
    admit(&s);
    carve(&s, "a", 30);
    settle_child(&s, "a");
    settle_child(&s, "a");
    assert_eq!(
        s.control_snapshot().unwrap().allocations["root"]
            .remaining
            .tokens,
        80
    );
}
pub fn carve_never_draws_on_in_flight_reservation<S: RoutineStore + IntentStore>(s: S) {
    setup(&s);
    admit(&s);
    activate(&s);
    let before = s.control_snapshot().unwrap();
    assert!(
        s.intent_transaction(IntentTransaction::Carve {
            parent_outbox_id: "outbox".into(),
            scheduler_epoch: 1,
            now_utc: "2026-01-01T01:01:00Z".into(),
            parent_id: "root".into(),
            children: vec![child("a", 90)],
            committee: false
        })
        .is_err()
    );
    assert_eq!(s.control_snapshot().unwrap(), before);
}
pub fn refund_credits_exact_source_window_once<S: RoutineStore + IntentStore>(s: S) {
    setup(&s);
    admit(&s);
    revoke(&s);
    settle_exec(&s, Resolution::NeverDispatched);
    settle_root(&s, "2026-01-01T01:10:00Z");
    settle_root(&s, "2026-01-01T01:10:00Z");
    assert_eq!(
        s.control_snapshot().unwrap().windows["window"]
            .remaining
            .tokens,
        100
    );
}
pub fn late_refund_after_window_close_is_expired_credit<S: RoutineStore + IntentStore>(s: S) {
    setup(&s);
    admit(&s);
    revoke(&s);
    settle_exec(&s, Resolution::NeverDispatched);
    s.routine_transaction(RoutineTransaction::OpenWindow {
        id: "next-window".into(),
        routine_id: "r".into(),
        at_utc: "2026-01-02T00:00:00Z".into(),
        expires_at: "2026-01-03T00:00:00Z".into(),
    })
    .unwrap();
    settle_root(&s, "2026-01-02T00:00:00Z");
    let v = s.control_snapshot().unwrap();
    assert_eq!(v.windows["next-window"].remaining.tokens, 100);
    assert_eq!(v.windows["window"].remaining.tokens, 0);
    assert_eq!(v.windows["window"].expired_credit.tokens, 100);
}
pub fn parent_cannot_settle_before_children<S: RoutineStore + IntentStore>(s: S) {
    setup(&s);
    admit(&s);
    carve(&s, "a", 30);
    settle_exec(&s, Resolution::Measured(Funds::default()));
    assert!(
        s.intent_transaction(IntentTransaction::SettleAllocation {
            allocation_id: "root".into(),
            settlement_id: "settle-root".into(),
            now_utc: "2026-01-01T01:10:00Z".into()
        })
        .is_err()
    );
    assert!(!s.control_snapshot().unwrap().allocations["root"].settled);
}
pub fn commit_admission_atomic_edit_or_cancel_race_wins_cleanly<
    S: RoutineStore + Send + Sync + 'static,
>(
    s: S,
) {
    setup(&s);
    let s = Arc::new(s);
    let b = Arc::new(Barrier::new(3));
    let (s1, b1) = (s.clone(), b.clone());
    let a = std::thread::spawn(move || {
        b1.wait();
        s1.routine_transaction(RoutineTransaction::CommitAdmission(request()))
    });
    let (s2, b2) = (s.clone(), b.clone());
    let e = std::thread::spawn(move || {
        let mut changed = spec();
        changed.revision = 2;
        changed.state = RoutineState::Cancelled;
        changed.approval = None;
        b2.wait();
        s2.routine_transaction(RoutineTransaction::ReplaceRoutine {
            expected_revision: 1,
            spec: changed,
        })
    });
    b.wait();
    let claimed = a.join().unwrap().is_ok();
    assert!(e.join().unwrap().is_ok());
    let v = s.control_snapshot().unwrap();
    assert_eq!(v.firings.len(), usize::from(claimed));
    if claimed {
        assert_eq!(v.outbox["outbox"].ticket, TicketState::Revoked);
    } else {
        assert_eq!(v.windows["window"].remaining.tokens, 100);
    }
}
pub fn two_firing_keys_race_last_budget_cap_and_overlap_slot<
    S: RoutineStore + Send + Sync + 'static,
>(
    s: S,
) {
    setup(&s);
    let s = Arc::new(s);
    let b = Arc::new(Barrier::new(3));
    let mut jobs = Vec::new();
    for hour in [1, 2] {
        let (s, b) = (s.clone(), b.clone());
        jobs.push(std::thread::spawn(move || {
            let mut r = request();
            if hour == 2 {
                r.firing.scheduled_instant_utc = "2026-01-01T02:00:00Z".into();
                r.firing.nominal_slots = vec!["2026-01-01T02:00:00Z".into()];
                r.firing.firing_key = firing_key("r", &r.firing.scheduled_instant_utc).unwrap();
                r.now_utc = r.firing.scheduled_instant_utc.clone();
                r.next_instant_utc = "2026-01-01T03:00:00Z".into();
                r.deadline_utc = r.next_instant_utc.clone();
                r.envelope_id = "root-2".into();
                r.envelope_settlement_id = "settle-root-2".into();
                r.reservation_id = "execution-2".into();
                r.reservation_settlement_id = "settle-execution-2".into();
                r.outbox_id = "outbox-2".into();
                r.host_spawn_token = Some("spawn-2".into());
            }
            b.wait();
            s.routine_transaction(RoutineTransaction::CommitAdmission(r))
                .is_ok()
        }));
    }
    b.wait();
    assert_eq!(
        jobs.into_iter()
            .map(|j| j.join().unwrap())
            .filter(|x| *x)
            .count(),
        1
    );
    assert_eq!(
        s.control_snapshot().unwrap().windows["window"]
            .remaining
            .tokens,
        0
    );
}
pub fn refusal_before_commit_reserves_nothing<S: RoutineStore>(s: S) {
    setup(&s);
    let before = s.control_snapshot().unwrap();
    let mut r = request();
    r.execution_bound.tokens = 101;
    assert!(
        s.routine_transaction(RoutineTransaction::CommitAdmission(r))
            .is_err()
    );
    assert_eq!(s.control_snapshot().unwrap(), before);
}
pub fn failure_after_commit_reconciles_reservation_exactly_once<S: RoutineStore + IntentStore>(
    s: S,
) {
    setup(&s);
    admit(&s);
    assert!(
        s.intent_transaction(IntentTransaction::SettleReservation {
            reservation_id: "execution".into(),
            settlement_id: "settle-execution".into(),
            resolution: Resolution::NeverDispatched
        })
        .is_err()
    );
    revoke(&s);
    settle_exec(&s, Resolution::NeverDispatched);
    settle_exec(&s, Resolution::NeverDispatched);
    assert_eq!(
        s.control_snapshot().unwrap().allocations["root"]
            .remaining
            .tokens,
        100
    );
}
pub fn envelope_caps_parent_plus_children_plus_resumes<S: RoutineStore + IntentStore>(s: S) {
    setup(&s);
    admit(&s);
    carve(&s, "a", 60);
    let before = s.control_snapshot().unwrap();
    assert!(
        s.intent_transaction(IntentTransaction::Carve {
            parent_outbox_id: "outbox".into(),
            scheduler_epoch: 1,
            now_utc: "2026-01-01T01:01:00Z".into(),
            parent_id: "root".into(),
            children: vec![child("b", 30)],
            committee: false
        })
        .is_err()
    );
    assert_eq!(s.control_snapshot().unwrap(), before);
}
pub fn descendants_inherit_envelope_deadline_not_window<S: RoutineStore + IntentStore>(s: S) {
    setup(&s);
    admit(&s);
    activate(&s);
    let mut c = child("a", 30);
    c.deadline_utc = "2026-01-01T02:01:00Z".into();
    assert!(
        s.intent_transaction(IntentTransaction::Carve {
            parent_outbox_id: "outbox".into(),
            scheduler_epoch: 1,
            now_utc: "2026-01-01T01:01:00Z".into(),
            parent_id: "root".into(),
            children: vec![c],
            committee: false
        })
        .is_err()
    );
}

pub fn committee_carving_is_atomic<S: RoutineStore + IntentStore>(s: S) {
    setup(&s);
    admit(&s);
    activate(&s);
    let before = s.control_snapshot().unwrap();
    assert!(
        s.intent_transaction(IntentTransaction::Carve {
            parent_outbox_id: "outbox".into(),
            scheduler_epoch: 1,
            now_utc: "2026-01-01T01:01:00Z".into(),
            parent_id: "root".into(),
            children: vec![child("a", 50), child("b", 50)],
            committee: true
        })
        .is_err()
    );
    assert_eq!(s.control_snapshot().unwrap(), before);
    s.intent_transaction(IntentTransaction::Carve {
        parent_outbox_id: "outbox".into(),
        scheduler_epoch: 1,
        now_utc: "2026-01-01T01:01:00Z".into(),
        parent_id: "root".into(),
        children: vec![child("a", 30), child("b", 30)],
        committee: true,
    })
    .unwrap();
    assert_eq!(
        s.control_snapshot().unwrap().allocations["root"]
            .remaining
            .tokens,
        20
    );
}
pub fn accepted_effect_requires_tombstone_before_refund<S: RoutineStore + IntentStore>(s: S) {
    setup(&s);
    admit(&s);
    s.routine_transaction(RoutineTransaction::AcceptEffect {
        outbox_id: "outbox".into(),
        scheduler_epoch: 1,
        now_utc: "2026-01-01T01:01:00Z".into(),
    })
    .unwrap();
    assert!(
        s.routine_transaction(RoutineTransaction::RevokeEffect {
            outbox_id: "outbox".into(),
            scheduler_epoch: 1
        })
        .is_err()
    );
    assert!(
        s.intent_transaction(IntentTransaction::SettleReservation {
            reservation_id: "execution".into(),
            settlement_id: "settle-execution".into(),
            resolution: Resolution::NeverDispatched
        })
        .is_err()
    );
    assert!(
        s.routine_transaction(RoutineTransaction::TombstoneUnstarted {
            outbox_id: "outbox".into(),
            scheduler_epoch: 1,
            receipt: "".into()
        })
        .is_err()
    );
    s.routine_transaction(RoutineTransaction::TombstoneUnstarted {
        outbox_id: "outbox".into(),
        scheduler_epoch: 1,
        receipt: "host-receipt".into(),
    })
    .unwrap();
    settle_exec(&s, Resolution::NeverDispatched);
}
pub fn memory_adapter_and_json_round_trip() {
    let s = crate::mem::MemStore::default();
    setup(&s);
    admit(&s);
    let snap = s.control_snapshot().unwrap();
    let encoded = serde_json::to_vec(&snap).unwrap();
    assert_eq!(
        serde_json::from_slice::<ControlLedger>(&encoded).unwrap(),
        snap
    );
}

pub fn catch_up_page_is_atomic_and_fenced<S: RoutineStore>(s: S) {
    setup(&s);
    s.routine_transaction(RoutineTransaction::StartCatchUp {
        scan_id: "scan".into(),
        routine_id: "r".into(),
        expected_revision: 1,
        scheduler_epoch: 1,
        from_utc: "2026-01-01T00:00:00Z".into(),
        to_utc: "2026-01-01T03:00:00Z".into(),
    })
    .unwrap();
    let before = s.control_snapshot().unwrap();
    assert!(
        s.routine_transaction(RoutineTransaction::AdvanceCatchUp {
            scan_id: "scan".into(),
            expected_sequence: 1,
            scheduler_epoch: 1,
            admission: None,
            refusal_reason: None
        })
        .is_err()
    );
    assert_eq!(s.control_snapshot().unwrap(), before);
    s.routine_transaction(RoutineTransaction::AdvanceCatchUp {
        scan_id: "scan".into(),
        expected_sequence: 0,
        scheduler_epoch: 1,
        admission: None,
        refusal_reason: None,
    })
    .unwrap();
    let result = s.control_snapshot().unwrap();
    assert_eq!(result.firings.len(), 3);
    assert!(
        result
            .firings
            .values()
            .all(|f| f.state == FiringState::SkippedMissed)
    );
    assert_eq!(result.watermark["r"], "2026-01-01T03:00:00Z");
    assert_eq!(result.catch_up_scans["scan"].sequence, 1);
    assert!(
        s.routine_transaction(RoutineTransaction::AdvanceCatchUp {
            scan_id: "scan".into(),
            expected_sequence: 0,
            scheduler_epoch: 1,
            admission: None,
            refusal_reason: None
        })
        .is_err()
    );
    assert_eq!(s.control_snapshot().unwrap(), result);
}
pub fn snapshot_corruption_fails_closed() {
    let s = MemoryRoutineIntentStore::default();
    setup(&s);
    admit(&s);
    let snap = s.control_snapshot().unwrap();
    let bytes = snap.encode_snapshot().unwrap();
    assert_eq!(ControlLedger::decode_snapshot(&bytes).unwrap(), snap);
    assert!(ControlLedger::decode_snapshot(b"not json").is_err());
    assert!(ControlLedger::decode_snapshot(&vec![b'x'; MAX_CONTROL_LEDGER_BYTES + 1]).is_err());
    let mut corrupt = snap.clone();
    corrupt.windows.get_mut("window").unwrap().remaining.tokens = 101;
    assert!(ControlLedger::decode_snapshot(&corrupt.encode_snapshot().unwrap()).is_err());
}

#[cfg(test)]
mod tests {
    use super::*;
    macro_rules! cases {($($name:ident),+ $(,)?) => {$ (#[test] fn $name(){super::$name(MemoryRoutineIntentStore::default());})+};}
    #[test]
    fn a_fenced_parent_cannot_create_a_child_allocation() {
        let s = MemoryRoutineIntentStore::default();
        setup(&s);
        admit(&s);
        activate(&s);
        s.routine_transaction(RoutineTransaction::SetSchedulerEpoch {
            expected: 1,
            next: 2,
        })
        .unwrap();
        let before = s.control_snapshot().unwrap();
        assert!(
            s.intent_transaction(IntentTransaction::Carve {
                parent_outbox_id: "outbox".into(),
                scheduler_epoch: 1,
                now_utc: "2026-01-01T01:01:00Z".into(),
                parent_id: "root".into(),
                children: vec![child("after-fence", 10)],
                committee: false,
            })
            .is_err()
        );
        assert_eq!(s.control_snapshot().unwrap(), before);
    }
    #[test]
    fn an_edited_or_expired_parent_cannot_create_children() {
        for change in ["edit", "cancel", "expiry"] {
            let s = MemoryRoutineIntentStore::default();
            setup(&s);
            admit(&s);
            activate(&s);
            if change != "expiry" {
                let mut changed = spec();
                changed.revision = 2;
                changed.state = if change == "edit" {
                    RoutineState::Proposed
                } else {
                    RoutineState::Cancelled
                };
                changed.approval = None;
                s.routine_transaction(RoutineTransaction::ReplaceRoutine {
                    expected_revision: 1,
                    spec: changed,
                })
                .unwrap();
            }
            let before = s.control_snapshot().unwrap();
            assert!(
                s.intent_transaction(IntentTransaction::Carve {
                    parent_id: "root".into(),
                    parent_outbox_id: "outbox".into(),
                    scheduler_epoch: 1,
                    now_utc: if change == "expiry" {
                        "2026-01-01T02:00:00Z"
                    } else {
                        "2026-01-01T01:01:00Z"
                    }
                    .into(),
                    children: vec![child("after-fence", 10)],
                    committee: false,
                })
                .is_err(),
                "{change}"
            );
            assert_eq!(s.control_snapshot().unwrap(), before, "{change}");
        }
    }
    #[test]
    fn long_period_firing_does_not_require_an_unbounded_scan() {
        let s = MemoryRoutineIntentStore::default();
        let mut long = spec();
        long.schedule = Schedule::Interval {
            every_secs: 100 * 86_400,
            anchor_utc: "2026-01-01T01:00:00Z".into(),
        };
        long.approval.as_mut().unwrap().digest = routine_approval_digest(&long).unwrap();
        s.routine_transaction(RoutineTransaction::InsertRoutine(long.clone()))
            .unwrap();
        s.routine_transaction(RoutineTransaction::SetSchedulerEpoch {
            expected: 0,
            next: 1,
        })
        .unwrap();
        s.routine_transaction(RoutineTransaction::OpenWindow {
            id: "window".into(),
            routine_id: "r".into(),
            at_utc: "2026-01-01T00:00:00Z".into(),
            expires_at: "2026-01-02T00:00:00Z".into(),
        })
        .unwrap();
        let mut req = request();
        req.approval_digest = routine_approval_digest(&long).unwrap();
        req.next_instant_utc = "2026-04-11T01:00:00Z".into();
        assert!(
            s.routine_transaction(RoutineTransaction::CommitAdmission(req))
                .is_ok()
        );
    }
    #[test]
    fn annual_cron_firing_does_not_require_an_unbounded_scan() {
        let s = MemoryRoutineIntentStore::default();
        let mut annual = spec();
        annual.schedule = Schedule::Calendar {
            cron: "0 1 1 1 *".into(),
            tz: "UTC".into(),
        };
        annual.approval.as_mut().unwrap().digest = routine_approval_digest(&annual).unwrap();
        s.routine_transaction(RoutineTransaction::InsertRoutine(annual.clone()))
            .unwrap();
        s.routine_transaction(RoutineTransaction::SetSchedulerEpoch {
            expected: 0,
            next: 1,
        })
        .unwrap();
        s.routine_transaction(RoutineTransaction::OpenWindow {
            id: "window".into(),
            routine_id: "r".into(),
            at_utc: "2026-01-01T00:00:00Z".into(),
            expires_at: "2026-01-02T00:00:00Z".into(),
        })
        .unwrap();
        let mut req = request();
        req.approval_digest = routine_approval_digest(&annual).unwrap();
        req.firing.nominal_slots = vec!["2026-01-01T01:00:00".into()];
        req.next_instant_utc = "2027-01-01T01:00:00Z".into();
        assert!(
            s.routine_transaction(RoutineTransaction::CommitAdmission(req))
                .is_ok()
        );
    }
    #[test]
    fn false_late_next_instant_cannot_hide_an_earlier_deadline() {
        let s = MemoryRoutineIntentStore::default();
        setup(&s);
        let before = s.control_snapshot().unwrap();
        let mut req = request();
        req.next_instant_utc = "2027-01-01T01:00:00Z".into();
        assert!(
            s.routine_transaction(RoutineTransaction::CommitAdmission(req))
                .is_err()
        );
        assert_eq!(s.control_snapshot().unwrap(), before);
    }
    #[test]
    fn approval_digest_binds_authority_fields_but_not_state_or_receipt() {
        let original = spec();
        let digest = routine_approval_digest(&original).unwrap();
        let mut changed = original.clone();
        changed.state = RoutineState::Paused {
            reason: "pending".into(),
        };
        changed.approval = None;
        assert_eq!(routine_approval_digest(&changed).unwrap(), digest);
        changed.schedule = Schedule::Interval {
            every_secs: 7200,
            anchor_utc: "2026-01-01T00:00:00Z".into(),
        };
        assert_ne!(routine_approval_digest(&changed).unwrap(), digest);
        let mut changed = original;
        changed.revision += 1;
        assert_ne!(routine_approval_digest(&changed).unwrap(), digest);
    }
    #[test]
    fn tampered_approved_snapshot_fails_closed() {
        let s = MemoryRoutineIntentStore::default();
        setup(&s);
        let mut snapshot = s.control_snapshot().unwrap();
        snapshot
            .routines
            .get_mut("r")
            .unwrap()
            .allocation
            .per_firing_tokens -= 1;
        let encoded = snapshot.encode_snapshot().unwrap();
        assert!(ControlLedger::decode_snapshot(&encoded).is_err());
    }
    #[test]
    fn approved_routine_rejects_a_digest_that_does_not_match_the_spec() {
        let s = MemoryRoutineIntentStore::default();
        let mut changed = spec();
        changed.allocation.per_firing_tokens -= 1;
        assert!(
            s.routine_transaction(RoutineTransaction::InsertRoutine(changed))
                .is_err()
        );
        assert!(s.control_snapshot().unwrap().routines.is_empty());
    }
    #[test]
    fn snapshot_corruption_fails_closed() {
        super::snapshot_corruption_fails_closed();
    }
    #[test]
    fn memory_adapter_and_json_round_trip() {
        super::memory_adapter_and_json_round_trip();
    }
    cases!(
        concurrent_claim_same_firing_one_wins,
        stale_scheduler_lease_cannot_transition,
        concurrent_reservations_cannot_overspend_window,
        ambiguous_child_keeps_carve_out,
        confirmed_undispatched_child_returns_carve_out,
        carve_never_draws_on_in_flight_reservation,
        refund_credits_exact_source_window_once,
        late_refund_after_window_close_is_expired_credit,
        parent_cannot_settle_before_children,
        commit_admission_atomic_edit_or_cancel_race_wins_cleanly,
        two_firing_keys_race_last_budget_cap_and_overlap_slot,
        refusal_before_commit_reserves_nothing,
        failure_after_commit_reconciles_reservation_exactly_once,
        envelope_caps_parent_plus_children_plus_resumes,
        descendants_inherit_envelope_deadline_not_window,
        committee_carving_is_atomic,
        accepted_effect_requires_tombstone_before_refund,
        catch_up_page_is_atomic_and_fenced
    );
}
