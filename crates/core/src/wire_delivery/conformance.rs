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
