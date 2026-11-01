//! `list_wire_inbox` and `update_wire_receipt` match a receipt to an agent
//! directly when they can, and look up a redirect only when they cannot.

use super::{FerroStore, FerroStoreServer, doc};
use fl_core::agent::{AgentRecord, AgentRole, RegisterAgentArgs};
use fl_core::at::At;
use fl_core::ferrowire::{
    DeliveryReceipt, DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress,
    WireFilter, WireMessage,
};
use fl_core::ids::Kind;
use fl_core::iri::Iri;
use fl_core::store::{AgentRegistry, MessageStore};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

fn register(store: &FerroStore, name: &str) -> AgentRecord {
    store
        .register_agent(RegisterAgentArgs {
            display_name: name.into(),
            email: None,
            project: format!("{}-project", name.to_ascii_lowercase()),
            role: AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(1),
            by: "owner".into(),
        })
        .unwrap()
}

fn send(store: &FerroStore, to: &str, subject: &str, at: u64) -> WireMessage {
    store
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::AgentName(to.into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: subject.into(),
            body: "body".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(at),
        })
        .unwrap()
        .0
}

fn receipt(msg: &WireMessage, id: &str, name: &str, status: DeliveryStatus) -> DeliveryReceipt {
    DeliveryReceipt {
        message_id: msg.id.clone(),
        recipient_id: Iri::parse(id).unwrap(),
        recipient_name: name.into(),
        status,
        attempts: 0,
        updated_at: At::from_unix_millis(50),
        acked_at: (status == DeliveryStatus::Acked).then(|| At::from_unix_millis(50)),
    }
}

fn import(store: &FerroStore, receipts: &[DeliveryReceipt]) {
    let report = store.import_wire_batch(&[], receipts).unwrap();
    assert_eq!(report.imported_receipts, receipts.len(), "{report:?}");
}

/// Point `old` at `new`, as a re-registration under a new id does.
fn redirect(store: &FerroStore, old: &Iri, new: &Iri) {
    store
        .with_write(|txn| doc::alias_unpublished(txn, Kind::Agent, old, new))
        .unwrap();
}

fn unread_for(store: &FerroStore, query: &str) -> BTreeSet<(String, String)> {
    store
        .list_wire_inbox(&WireFilter {
            recipient: Some(query.into()),
            unread_only: true,
            ..WireFilter::default()
        })
        .unwrap()
        .into_iter()
        .map(|item| {
            (
                item.message.subject,
                item.receipt.recipient_id.as_str().to_string(),
            )
        })
        .collect()
}

/// Alice's inbox: one receipt of each way to match her, one acked and one
/// expired receipt that would match, and Bob's receipt.
struct Inbox {
    alice: AgentRecord,
    old_alice: Iri,
    expected: BTreeSet<(String, String)>,
}

fn alice_inbox(store: &FerroStore) -> Inbox {
    let alice = register(store, "Alice");
    register(store, "Bob");
    let old_alice = crate::mint().unwrap();
    let alice_id = alice.id.as_str().to_string();
    send(store, "alice", "direct", 10);
    let by_name = send(store, "bob", "by-name", 11);
    let by_case = send(store, "bob", "by-case", 12);
    let by_redirect = send(store, "bob", "by-redirect", 13);
    let acked = send(store, "alice", "acked", 14);
    let expired = send(store, "bob", "expired", 15);
    import(
        store,
        &[
            receipt(
                &by_name,
                "urn:x:alice-elsewhere",
                "alice",
                DeliveryStatus::Pending,
            ),
            receipt(
                &by_case,
                "urn:x:alice-loud",
                "ALICE",
                DeliveryStatus::Pending,
            ),
            // Matches only through the redirect: neither its id nor its name is Alice's.
            receipt(
                &by_redirect,
                old_alice.as_str(),
                "alice-before",
                DeliveryStatus::Delivered,
            ),
            receipt(
                &expired,
                "urn:x:alice-gone",
                "alice",
                DeliveryStatus::Expired,
            ),
        ],
    );
    redirect(store, &old_alice, alice.id.iri());
    store
        .update_wire_receipt(
            &acked.id,
            &alice_id,
            DeliveryStatus::Acked,
            false,
            At::from_unix_millis(60),
        )
        .unwrap()
        .expect("the acked receipt");
    let expected = [
        ("direct", alice_id.as_str()),
        ("by-name", "urn:x:alice-elsewhere"),
        ("by-case", "urn:x:alice-loud"),
        ("by-redirect", old_alice.as_str()),
    ]
    .into_iter()
    .map(|(subject, id)| (subject.to_string(), id.to_string()))
    .collect();
    Inbox {
        alice,
        old_alice,
        expected,
    }
}

#[test]
fn an_unread_inbox_matches_by_id_name_case_and_redirect_and_skips_acked_and_expired() {
    let store = FerroStore::in_memory();
    let inbox = alice_inbox(&store);
    for query in ["alice", "Alice", "ALICE", " alice "] {
        assert_eq!(unread_for(&store, query), inbox.expected, "query {query:?}");
    }
    // A query by id compares a receipt's name with the agent's name key
    // exactly; only a query by name ignores case.
    let mut by_id = inbox.expected.clone();
    by_id.retain(|(subject, _)| subject != "by-case");
    for query in [
        inbox.alice.id.as_str(),
        inbox.old_alice.as_str(),
        "urn:fl:agent:alice",
    ] {
        assert_eq!(unread_for(&store, query), by_id, "query {query:?}");
    }
    // Bob's receipts are his alone.
    let bob: BTreeSet<String> = unread_for(&store, "bob")
        .into_iter()
        .map(|(subject, _)| subject)
        .collect();
    let want: BTreeSet<String> = ["by-name", "by-case", "by-redirect", "expired"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(bob, want);
    // Without `unread_only` the acked and expired receipts are listed.
    let all = store
        .list_wire_inbox(&WireFilter {
            recipient: Some("alice".into()),
            ..WireFilter::default()
        })
        .unwrap();
    let subjects: BTreeSet<String> = all.into_iter().map(|item| item.message.subject).collect();
    for subject in ["acked", "expired", "direct", "by-redirect"] {
        assert!(
            subjects.contains(subject),
            "{subject} missing: {subjects:?}"
        );
    }
}

#[test]
fn an_ack_by_the_new_id_still_finds_a_receipt_addressed_to_the_old_one() {
    let store = FerroStore::in_memory();
    let inbox = alice_inbox(&store);
    let msg = store
        .list_wire_inbox(&WireFilter {
            recipient: Some("alice".into()),
            unread_only: true,
            ..WireFilter::default()
        })
        .unwrap()
        .into_iter()
        .find(|item| item.message.subject == "by-redirect")
        .unwrap()
        .message;
    let acked = store
        .update_wire_receipt(
            &msg.id,
            inbox.alice.id.as_str(),
            DeliveryStatus::Acked,
            false,
            At::from_unix_millis(70),
        )
        .unwrap()
        .expect("the redirected receipt");
    assert_eq!(acked.recipient_id, inbox.old_alice);
    assert_eq!(acked.status, DeliveryStatus::Acked);
}

// ---- requests over a remote store ------------------------------------------------

struct Served {
    server: FerroStoreServer,
    client: FerroStore,
    _dir: tempfile::TempDir,
}

fn serve(store: FerroStore) -> Served {
    let dir = tempfile::tempdir().unwrap();
    let sock: PathBuf = dir.path().join("s.sock");
    let server = FerroStoreServer::bind_unix(&sock, store).unwrap();
    let client = FerroStore::connect_uri(&format!("unix://{}", sock.display())).unwrap();
    Served {
        server,
        client,
        _dir: dir,
    }
}

impl Served {
    /// Requests the server ran for `f`, its session's close included.
    fn requests<T>(&self, f: impl FnOnce(&FerroStore) -> T) -> (u64, T) {
        self.settle();
        let before = self.server.dispatched();
        let out = f(&self.client);
        self.settle();
        (self.server.dispatched() - before, out)
    }

    /// Wait until no read session is open: a close is sent without waiting
    /// for its reply, and `dispatch` counts a request before it runs it.
    fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.server.open_sessions() != 0 {
            assert!(Instant::now() < deadline, "a session stayed open");
            thread::sleep(Duration::from_millis(2));
        }
    }
}

#[test]
fn acked_receipts_and_direct_name_matches_ask_for_no_redirect() {
    let store = FerroStore::in_memory();
    register(&store, "Alice");
    send(&store, "alice", "first", 10);
    let served = serve(store);
    let unread = |client: &FerroStore| {
        client
            .list_wire_inbox(&WireFilter {
                recipient: Some("alice".into()),
                unread_only: true,
                ..WireFilter::default()
            })
            .unwrap()
            .len()
    };
    let (base, listed) = served.requests(unread);
    assert_eq!(listed, 1);

    // One more message, with receipts under five ids this store does not
    // hold: one pending that matches Alice by name, four acked or expired.
    let second = send(&served.client, "alice", "second", 20);
    let mut extra = vec![receipt(
        &second,
        "urn:x:alice-elsewhere",
        "alice",
        DeliveryStatus::Pending,
    )];
    for (i, status) in [
        DeliveryStatus::Acked,
        DeliveryStatus::Acked,
        DeliveryStatus::Expired,
        DeliveryStatus::Acked,
    ]
    .into_iter()
    .enumerate()
    {
        extra.push(receipt(
            &second,
            &format!("urn:x:gone-{i}"),
            "alice",
            status,
        ));
    }
    import(&served.client, &extra);

    let (now, listed) = served.requests(unread);
    assert_eq!(listed, 3);
    // The only new read is the second message itself: no id lookup for the
    // name match, none for an acked or expired receipt.
    assert_eq!(now, base + 1, "base {base}, now {now}");
}

#[test]
fn an_ack_by_name_asks_for_no_redirect_for_a_direct_name_match() {
    let store = FerroStore::in_memory();
    register(&store, "Alice");
    register(&store, "Bob");
    // Alice's own receipt: its id is the one the query resolved to.
    let own = send(&store, "alice", "own", 10);
    // Bob's message, and a receipt that names Alice under an id this store
    // does not hold. `urn:a:` sorts before the store's `urn:uuid:` ids, so
    // it is the first receipt of the message the ack looks at.
    let named = send(&store, "bob", "named", 11);
    let served = serve(store);
    import(
        &served.client,
        &[receipt(
            &named,
            "urn:a:alice-elsewhere",
            "alice",
            DeliveryStatus::Pending,
        )],
    );
    let ack = |msg: &WireMessage| {
        let id = msg.id.clone();
        move |client: &FerroStore| {
            client
                .update_wire_receipt(
                    &id,
                    "alice",
                    DeliveryStatus::Acked,
                    false,
                    At::from_unix_millis(30),
                )
                .unwrap()
                .expect("a receipt")
        }
    };
    let (own_cost, acked) = served.requests(ack(&own));
    assert_eq!(acked.status, DeliveryStatus::Acked);
    let (named_cost, acked) = served.requests(ack(&named));
    assert_eq!(acked.recipient_id.as_str(), "urn:a:alice-elsewhere");
    assert_eq!(acked.status, DeliveryStatus::Acked);
    // The name match costs no more than the agent's own id, whose lookup the
    // query itself already made.
    assert_eq!(named_cost, own_cost);
}
