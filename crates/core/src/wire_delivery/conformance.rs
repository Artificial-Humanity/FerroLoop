//! Reusable deterministic assertions; descriptors here confer no admission authority.
use super::*;
use crate::ferrowire::{MessageKind, MessageUrgency, WireAddress};
use crate::ids::seq_iri;

pub fn fixture_message() -> WireMessage {
    WireMessage {
        id: MessageId(seq_iri(9001)),
        thread_id: MessageId(seq_iri(9001)),
        reply_to: None,
        correlation_id: None,
        sender_id: None,
        sender_name: "alice".into(),
        sender_host: None,
        to: WireAddress::AgentName("bob".into()),
        kind: MessageKind::Mail,
        urgency: MessageUrgency::Normal,
        durable: false,
        subject: "Hello".into(),
        body: "Body".into(),
        created_at: At::from_unix_millis(1000),
        deadline_at: None,
    }
}

pub fn fixture_commands() -> Vec<DeliveryCommand> {
    let key = DeliveryKey {
        original_id: fixture_message().id,
        recipient_id: AgentId(seq_iri(9002)),
    };
    let binding = Binding {
        recipient_id: key.recipient_id.clone(),
        binding_revision: Revision(1),
        authority_id: "ai-lab-0".into(),
        local_epoch: 1,
        policy_hash: Digest([1; 32]),
    };
    let lifetime = LifetimeRegistration {
        id: Digest([2; 32]),
        revision: Revision(1),
        home: Digest([3; 32]),
        boot: Digest([4; 32]),
        instance: InstanceId([5; 32]),
        worker: WorkerId([6; 32]),
        connection: ConnectionId([7; 32]),
        emitter_scope: Digest([8; 32]),
    };
    let attempt = AttemptBinding {
        token: PhaseToken([9; 32]),
        daemon: lifetime.instance,
        worker: lifetime.worker,
        connection: lifetime.connection,
        lane: Lane::Push,
        lifetime,
    };
    let route = RoutePin {
        thread_id: "thread".into(),
        generation: 1,
        fingerprint: Digest([10; 32]),
        executable: Some("/fixture/codex".into()),
        executable_digest: Some(Digest([11; 32])),
    };
    let generation = canonical_generation(&fixture_message()).unwrap();
    let bytes = b"fixture peer envelope".to_vec();
    let frozen = FrozenPayload {
        renderer: PEER_RENDERER_VERSION.into(),
        transport: Transport::Queue,
        expected_turn: None,
        digest: canonical_payload(generation, PEER_RENDERER_VERSION, &bytes).unwrap(),
        bytes,
    };
    vec![
        DeliveryCommand::Prepare(PrepareInput {
            key: key.clone(),
            generation,
            binding,
            attempt: attempt.clone(),
            route: route.clone(),
            now: At::from_unix_millis(2000),
        }),
        DeliveryCommand::Arm(ArmInput {
            key: key.clone(),
            revision: Revision(1),
            attempt: attempt.clone(),
            route,
            frozen,
            now: At::from_unix_millis(2001),
        }),
        DeliveryCommand::Settle(SettleInput {
            key: key.clone(),
            revision: Revision(2),
            attempt,
            evidence: DeliveryEvidence::VendorQueued,
            detail: String::new(),
            now: At::from_unix_millis(2002),
        }),
        DeliveryCommand::ManualAck(ManualAckInput {
            key,
            at: At::from_unix_millis(2003),
        }),
    ]
}

/// The owning backend installs its private admission fixture before calling this.
/// `seed` persists immutable messages/registry through that backend's typed APIs.
pub fn delivery_conformance(store: &impl WireDeliveryStore, seed: impl Fn(&WireMessage)) {
    seed(&fixture_message());
    let commands = fixture_commands();
    assert!(matches!(
        store.apply_delivery(commands[0].clone()).unwrap(),
        DeliveryResult::Prepared { .. }
    ));
    assert!(matches!(
        store.apply_delivery(commands[0].clone()).unwrap(),
        DeliveryResult::Refused { .. }
    ));
    assert!(matches!(
        store.apply_delivery(commands[1].clone()).unwrap(),
        DeliveryResult::Armed { .. }
    ));
    assert!(matches!(
        store.apply_delivery(commands[1].clone()).unwrap(),
        DeliveryResult::Refused { .. }
    ));
    store.apply_delivery(commands[2].clone()).unwrap();
    let before = store.inspect_delivery(commands[2].key()).unwrap().unwrap();
    store.apply_delivery(commands[2].clone()).unwrap();
    assert_eq!(
        store.inspect_delivery(commands[2].key()).unwrap().unwrap(),
        before
    );
    store.apply_delivery(commands[3].clone()).unwrap();
    let acked = store.inspect_delivery(commands[3].key()).unwrap().unwrap();
    assert_eq!(acked.attempts, before.attempts);
    assert_eq!(acked.prepares, 1);
    assert!(acked.acknowledgment.manual_at.is_some());
    assert!(store.active_delivery_owners().unwrap().is_empty());
}

/// Ordinary test scenarios, not a lifetime admission or ended-scope verifier.
#[derive(Debug, Clone, Copy)]
pub enum ContradictionCase {
    Unarmed,
    ArmedQueued,
    ArmedAccepted,
    ArmedNoMutation,
    RetiredNoMutation,
    CrossKey,
}

fn inspect(store: &impl WireDeliveryStore, key: &DeliveryKey) -> DeliveryView {
    store.inspect_delivery(key).unwrap().unwrap()
}

fn prepare(store: &impl WireDeliveryStore, input: &PrepareInput) -> DeliveryRecord {
    let DeliveryResult::Prepared { record } = store
        .apply_delivery(DeliveryCommand::Prepare(input.clone()))
        .unwrap()
    else {
        panic!("fixture prepare must succeed")
    };
    record
}

fn arm(store: &impl WireDeliveryStore, input: &PrepareInput, revision: Revision, steer: bool) {
    let DeliveryCommand::Arm(mut a) = fixture_commands().remove(1) else {
        unreachable!()
    };
    a.key = input.key.clone();
    a.attempt = input.attempt.clone();
    a.route = input.route.clone();
    a.revision = revision;
    if steer {
        a.frozen.transport = Transport::Steer;
        a.frozen.expected_turn = Some("turn".into());
    }
    a.frozen.digest =
        canonical_payload(input.generation, &a.frozen.renderer, &a.frozen.bytes).unwrap();
    assert!(matches!(
        store.apply_delivery(DeliveryCommand::Arm(a)).unwrap(),
        DeliveryResult::Armed { .. }
    ));
}

fn settle(store: &impl WireDeliveryStore, input: &PrepareInput, evidence: DeliveryEvidence) {
    let revision = inspect(store, &input.key).revision;
    assert!(matches!(
        store
            .apply_delivery(DeliveryCommand::Settle(SettleInput {
                key: input.key.clone(),
                revision,
                attempt: input.attempt.clone(),
                evidence,
                detail: String::new(),
                now: At::from_unix_millis(3000),
            }))
            .unwrap(),
        DeliveryResult::Applied { .. }
    ));
}

fn assert_uncertain(store: &impl WireDeliveryStore, key: &DeliveryKey) -> DeliveryView {
    let view = inspect(store, key);
    assert_eq!(view.eligibility, Eligibility::Held);
    assert_eq!(view.held, Some(HoldReason::Unconfirmed));
    assert_eq!(view.barrier.as_ref(), Some(key));
    view
}

fn refuses_prepare(store: &impl WireDeliveryStore, input: &PrepareInput) {
    let before = inspect(store, &input.key);
    assert_eq!(
        store
            .apply_delivery(DeliveryCommand::Prepare(input.clone()))
            .unwrap(),
        DeliveryResult::Refused {
            reason: HoldReason::Unconfirmed
        }
    );
    assert_eq!(inspect(store, &input.key), before);
}

/// Shared reference/native assertions. Only owning tests implement `seed` and
/// `retire`: neither this data nor these assertions confer production authority.
/// The retirement callback must supply its own private synthetic ended premise.
pub fn contradiction_conformance(
    store: &impl WireDeliveryStore,
    seed: impl Fn(&PrepareInput, &WireMessage),
    retire: impl Fn(&CapacityRetirementTarget) -> CapacityRetirementResult,
    case: ContradictionCase,
) -> Vec<DeliveryView> {
    let DeliveryCommand::Prepare(first) = fixture_commands().remove(0) else {
        unreachable!()
    };
    seed(&first, &fixture_message());
    let p = prepare(store, &first);
    arm(store, &first, p.revision, false);
    let cross_key = matches!(case, ContradictionCase::CrossKey);
    settle(
        store,
        &first,
        if cross_key {
            DeliveryEvidence::VendorQueued
        } else {
            DeliveryEvidence::DefinitiveNoMutation
        },
    );
    let mut second = first.clone();
    second.attempt.token = PhaseToken([42; 32]);
    if cross_key {
        let mut message = fixture_message();
        message.id = MessageId(seq_iri(9003));
        message.thread_id = message.id.clone();
        second.key.original_id = message.id.clone();
        second.generation = canonical_generation(&message).unwrap();
        seed(&second, &message);
    }
    let p = prepare(store, &second);
    if !matches!(case, ContradictionCase::Unarmed) {
        arm(
            store,
            &second,
            p.revision,
            matches!(case, ContradictionCase::ArmedAccepted),
        );
    }
    let before = inspect(store, &second.key);
    let owner_before = store.active_delivery_owners().unwrap().remove(0);
    // A contradictory report for the old attempt must hold, never replace its
    // first evidence or grant a second emission. Its CAS is the CURRENT revision.
    settle(store, &first, DeliveryEvidence::MutationUnconfirmed);
    let contradiction = assert_uncertain(store, &first.key);
    assert_eq!(
        contradiction.attempts[0].evidence,
        Some(if cross_key {
            DeliveryEvidence::VendorQueued
        } else {
            DeliveryEvidence::DefinitiveNoMutation
        })
    );
    let owner_after = store.active_delivery_owners().unwrap();
    if matches!(case, ContradictionCase::Unarmed) {
        assert!(
            owner_after.is_empty(),
            "unarmed owner must be released atomically"
        );
        assert_eq!(contradiction.attempts[1].capacity, CapacityState::Released);
        assert_eq!(contradiction.attempts[1].armed_at, None);
        assert_eq!(contradiction.attempts[1].evidence, None);
    } else {
        assert_eq!(owner_after.len(), 1, "armed owner must retain capacity");
        assert_eq!(owner_after[0].owner, owner_before.owner);
        assert_eq!(owner_after[0].global_revision, owner_before.global_revision);
        assert_eq!(
            inspect(store, &second.key)
                .attempts
                .last()
                .unwrap()
                .capacity,
            CapacityState::Held
        );
    }
    if cross_key || matches!(case, ContradictionCase::RetiredNoMutation) {
        let target = owner_after[0].clone();
        // Exact revisions/owner still matter even if another key owns the barrier.
        for field in 0..4 {
            let mut stale = target.clone();
            match field {
                0 => stale.record_revision.0 += 1,
                1 => stale.recipient_revision.0 += 1,
                2 => stale.global_revision.0 += 1,
                _ => stale.owner.attempt.token = PhaseToken([99; 32]),
            }
            assert!(matches!(retire(&stale), CapacityRetirementResult::Stale));
            assert_eq!(store.active_delivery_owners().unwrap(), owner_after);
        }
        assert!(
            matches!(retire(&target), CapacityRetirementResult::Retired { .. }),
            "exact armed owner must retire capacity without owning the uncertainty barrier"
        );
        assert!(store.active_delivery_owners().unwrap().is_empty());
        let retired = inspect(store, &second.key);
        let mut expected = if cross_key {
            before.clone()
        } else {
            contradiction.clone()
        };
        expected.revision.0 += 1;
        expected.barrier = Some(first.key.clone());
        expected.attempts.last_mut().unwrap().capacity = CapacityState::Retired;
        assert_eq!(
            retired, expected,
            "retirement changes capacity/revision only"
        );
        assert!(matches!(
            retire(&target),
            CapacityRetirementResult::AlreadyReleased { .. }
        ));
        assert_eq!(inspect(store, &second.key), retired);
    }
    if !matches!(
        case,
        ContradictionCase::Unarmed | ContradictionCase::CrossKey
    ) {
        settle(
            store,
            &second,
            match case {
                ContradictionCase::ArmedQueued => DeliveryEvidence::VendorQueued,
                ContradictionCase::ArmedAccepted => DeliveryEvidence::AppServerAccepted {
                    thread: second.route.thread_id.clone(),
                    turn: "turn".into(),
                },
                _ => DeliveryEvidence::DefinitiveNoMutation,
            },
        );
        assert_uncertain(store, &first.key);
        assert!(store.active_delivery_owners().unwrap().is_empty());
    }
    if cross_key {
        assert_uncertain(store, &first.key);
        let mut fresh = second.clone();
        let mut message = fixture_message();
        message.id = MessageId(seq_iri(9004));
        message.thread_id = message.id.clone();
        fresh.key.original_id = message.id.clone();
        fresh.generation = canonical_generation(&message).unwrap();
        seed(&fresh, &message);
        assert_eq!(
            store
                .apply_delivery(DeliveryCommand::Prepare(fresh.clone()))
                .unwrap(),
            DeliveryResult::Refused {
                reason: HoldReason::Unconfirmed
            }
        );
        assert!(store.inspect_delivery(&fresh.key).unwrap().is_none());
    } else {
        let mut third = second.clone();
        third.attempt.token = PhaseToken([43; 32]);
        refuses_prepare(store, &third);
        let before_admin = assert_uncertain(store, &first.key);
        for command in [
            DeliveryCommand::ManualAck(ManualAckInput {
                key: first.key.clone(),
                at: At::from_unix_millis(4000),
            }),
            DeliveryCommand::Annotate {
                key: first.key.clone(),
                text: "operator note".into(),
            },
            DeliveryCommand::Annotate {
                key: first.key.clone(),
                text: "x".repeat(4097),
            },
            DeliveryCommand::Close {
                key: first.key.clone(),
            },
        ] {
            assert!(matches!(
                store.apply_delivery(command).unwrap(),
                DeliveryResult::Applied { .. }
            ));
            let after = assert_uncertain(store, &first.key);
            assert_eq!(after.attempts, before_admin.attempts);
            assert_eq!(after.prepares, 2);
            assert!(!after.acknowledgment.legacy_unknown);
            refuses_prepare(store, &third);
        }
        for command in [
            DeliveryCommand::InvalidatePrepared {
                key: first.key.clone(),
                revision: inspect(store, &first.key).revision,
                now: At::from_unix_millis(4001),
            },
            DeliveryCommand::Release(ReleaseInput {
                key: first.key.clone(),
                revision: inspect(store, &first.key).revision,
                attempt: second.attempt.clone(),
                proof: DeliveryEvidence::DefinitiveNoMutation,
                now: At::from_unix_millis(4001),
            }),
        ] {
            assert!(matches!(
                store.apply_delivery(command).unwrap(),
                DeliveryResult::Refused { .. }
            ));
            assert_uncertain(store, &first.key);
        }
        // Duplicate contradictory evidence may increment the record CAS but must
        // never decrement capacity twice or replace original evidence.
        settle(store, &first, DeliveryEvidence::MutationUnconfirmed);
        assert_uncertain(store, &first.key);
        refuses_prepare(store, &third);
    }
    assert!(store.active_delivery_owners().unwrap().is_empty());
    let mut views = vec![inspect(store, &first.key)];
    if cross_key {
        views.push(inspect(store, &second.key));
    }
    views
}
