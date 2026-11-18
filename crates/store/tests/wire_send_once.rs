//! The send-once notice operation is shared across both on-disk backends.
use fl_core::at::At;
use fl_core::ferrowire::{DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress};
use fl_core::ids::{MessageId, seq_iri};
use fl_core::store::MessageStore;
use fl_store::{RedbStore, ferrostore::FerroStore};
use std::sync::{Arc, Barrier};

fn args() -> SendWireArgs {
    SendWireArgs {
        id: Some(MessageId(seq_iri(92177))),
        sender_id: None,
        sender_name: "jarvis".into(),
        sender_host: None,
        to: WireAddress::AgentName("ferris".into()),
        kind: MessageKind::Mail,
        urgency: MessageUrgency::Normal,
        durable: true,
        subject: "Prompt".into(),
        body: "Pending".into(),
        reply_to: None,
        thread_id: None,
        deadline_at: None,
        at: At::from_unix_millis(10),
    }
}

fn concurrent<S: MessageStore + Send + Sync>(store: Arc<S>) {
    let barrier = Arc::new(Barrier::new(8));
    let inserted = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    store
                        .send_wire_once_awaiting_daemon(args())
                        .unwrap()
                        .is_some()
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|inserted| *inserted)
            .count()
    });
    assert_eq!(inserted, 1);
    let id = args().id.unwrap();
    assert_eq!(store.wire_messages_after(0).unwrap().len(), 1);
    assert_eq!(store.wire_receipts_after(0).unwrap().len(), 1);
    store
        .update_wire_receipt(
            &id,
            "ferris",
            DeliveryStatus::Acked,
            false,
            At::from_unix_millis(11),
        )
        .unwrap()
        .unwrap();
    store.clear_wire_awaiting_daemon(&id).unwrap();
}

fn after_reopen<S: MessageStore>(store: S) {
    let id = args().id.unwrap();
    assert!(
        store
            .send_wire_once_awaiting_daemon(args())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.list_wire_receipts(&id).unwrap()[0].status,
        DeliveryStatus::Acked
    );
    assert!(store.list_wire_awaiting_daemon().unwrap().is_empty());
}

#[test]
fn redb_concurrent_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wire.redb");
    concurrent(Arc::new(RedbStore::open(&path).unwrap()));
    after_reopen(RedbStore::open(&path).unwrap());
}

#[test]
fn ferro_concurrent_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wire.fls");
    concurrent(Arc::new(FerroStore::open(&path).unwrap()));
    after_reopen(FerroStore::open(&path).unwrap());
}
