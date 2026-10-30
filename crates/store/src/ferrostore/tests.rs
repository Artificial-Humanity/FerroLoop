use super::layout::{self, Op, Superblock};
use super::{FerroStore, file_uri_path, latest_superblock, write_slot};
use fl_core::StoreError;
use serde_json::json;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn temp_fls() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.fls");
    (dir, path)
}

fn slots(path: &Path) -> [Option<Superblock>; 2] {
    let file = File::open(path).unwrap();
    let mut out = [None, None];
    for (index, slot) in out.iter_mut().enumerate() {
        let mut buf = [0u8; layout::SLOT_LEN];
        file.read_exact_at(&mut buf, index as u64 * layout::SLOT_LEN as u64)
            .unwrap();
        *slot = layout::parse_superblock(&buf);
    }
    out
}

fn live(path: &Path) -> Superblock {
    let file = File::open(path).unwrap();
    latest_superblock(&file).unwrap().1
}

fn extend_wal(path: &Path, extra: &[u8]) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let (index, sb) = latest_superblock(&file).unwrap();
    let at = sb.wal_offset + sb.wal_length;
    file.write_all_at(extra, at).unwrap();
    let mut next = sb;
    next.epoch += 1;
    next.wal_length += u64::try_from(extra.len()).unwrap();
    write_slot(&file, index ^ 1, &next).unwrap();
    file.sync_all().unwrap();
}

fn corrupt_live_crc(path: &Path) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let (index, _) = latest_superblock(&file).unwrap();
    let offset = index as u64 * layout::SLOT_LEN as u64;
    let mut buf = [0u8; layout::SLOT_LEN];
    file.read_exact_at(&mut buf, offset).unwrap();
    buf[20] ^= 0xff;
    file.write_all_at(&buf, offset).unwrap();
    file.sync_all().unwrap();
}

fn seed(path: &Path) {
    let store = FerroStore::open(path).unwrap();
    store
        .put_doc("docs", "kept", &json!({ "ok": true }))
        .unwrap();
}

#[test]
fn crc32_iso_hdlc_and_superblock_round_trip() {
    assert_eq!(layout::crc32(b"123456789"), 0xCBF4_3926);
    let sb = Superblock {
        epoch: 7,
        snapshot_offset: 8192,
        snapshot_length: 3,
        wal_offset: 9000,
        wal_length: 4,
        snapshot_seq: 5,
    };
    let bytes = layout::encode_superblock(&sb);
    assert_eq!(layout::parse_superblock(&bytes), Some(sb));
    let mut torn = bytes;
    torn[20] ^= 0xff;
    assert!(layout::parse_superblock(&torn).is_none());
    torn = bytes;
    torn[64] = 1;
    assert!(layout::parse_superblock(&torn).is_none());
}

#[test]
fn superblock_double_buffer_falls_back_when_the_live_slot_is_torn() {
    let (_dir, path) = temp_fls();
    {
        let store = FerroStore::open(&path).unwrap();
        drop(store);
    }
    let fresh = slots(&path);
    assert_eq!(fresh[0].unwrap().epoch, 1);
    assert!(fresh[1].is_none());
    assert_eq!(fresh[0].unwrap().wal_length, 0);
    assert_eq!(fresh[0].unwrap().snapshot_offset, 8192);
    assert_eq!(fresh[0].unwrap().snapshot_seq, 0);

    {
        let store = FerroStore::open(&path).unwrap();
        store.put_doc("docs", "a", &json!({ "n": 1 })).unwrap();
        store.put_doc("docs", "b", &json!({ "n": 2 })).unwrap();
        store.put_doc("docs", "c", &json!({ "n": 3 })).unwrap();
    }
    let both = slots(&path);
    assert_eq!(both[0].map(|sb| sb.epoch), Some(3));
    assert_eq!(both[1].map(|sb| sb.epoch), Some(4));
    assert!(both[1].unwrap().wal_length > both[0].unwrap().wal_length);

    {
        let store = FerroStore::open(&path).unwrap();
        assert!(store.get_doc("docs", "c").unwrap().is_some());
    }
    corrupt_live_crc(&path);
    let after = slots(&path);
    assert_eq!(after[0].map(|sb| sb.epoch), Some(3));
    assert!(after[1].is_none());

    let store = FerroStore::open(&path).unwrap();
    assert_eq!(store.get_doc("docs", "a").unwrap().unwrap()["n"], 1);
    assert_eq!(store.get_doc("docs", "b").unwrap().unwrap()["n"], 2);
    assert!(store.get_doc("docs", "c").unwrap().is_none());
}

#[test]
fn wal_truncated_tail_is_ignored_and_a_bad_crc_in_the_middle_is_corrupt() {
    let (_dir, path) = temp_fls();
    seed(&path);
    let mut header_only = Vec::new();
    header_only.extend_from_slice(&64u32.to_le_bytes());
    header_only.extend_from_slice(&0u32.to_le_bytes());
    extend_wal(&path, &header_only);
    {
        let store = FerroStore::open(&path).unwrap();
        assert_eq!(store.get_doc("docs", "kept").unwrap().unwrap()["ok"], true);
    }

    let (_dir, path) = temp_fls();
    seed(&path);
    let mut short = Vec::new();
    short.extend_from_slice(&32u32.to_le_bytes());
    short.extend_from_slice(&0u32.to_le_bytes());
    short.extend_from_slice(&[1, 2, 3]);
    extend_wal(&path, &short);
    {
        let store = FerroStore::open(&path).unwrap();
        assert!(store.get_doc("docs", "kept").unwrap().is_some());
    }

    let (_dir, path) = temp_fls();
    seed(&path);
    extend_wal(&path, &[0x01, 0x02, 0x03]);
    {
        let store = FerroStore::open(&path).unwrap();
        assert!(store.get_doc("docs", "kept").unwrap().is_some());
    }

    let (_dir, path) = temp_fls();
    seed(&path);
    let mut ghost = layout::encode_frame(
        2,
        &[Op::Put {
            collection: "docs".into(),
            id: "ghost".into(),
            doc: json!({ "no": true }),
        }],
    )
    .unwrap();
    ghost[4] ^= 0xff;
    extend_wal(&path, &ghost);
    {
        let store = FerroStore::open(&path).unwrap();
        assert!(store.get_doc("docs", "kept").unwrap().is_some());
        assert!(store.get_doc("docs", "ghost").unwrap().is_none());
    }

    let (_dir, path) = temp_fls();
    seed(&path);
    let mut middle = layout::encode_frame(
        2,
        &[Op::Put {
            collection: "docs".into(),
            id: "middle".into(),
            doc: json!({ "n": 2 }),
        }],
    )
    .unwrap();
    middle[4] ^= 0xff;
    let tail = layout::encode_frame(
        3,
        &[Op::Put {
            collection: "docs".into(),
            id: "tail".into(),
            doc: json!({ "n": 3 }),
        }],
    )
    .unwrap();
    middle.extend(tail);
    extend_wal(&path, &middle);
    let err = match FerroStore::open(&path) {
        Ok(_) => panic!("open of a corrupt wal succeeded"),
        Err(err) => err,
    };
    let msg = err.to_string();
    assert!(
        matches!(err, StoreError::Backend(_)) && msg.contains("corrupt"),
        "{msg}"
    );
}

#[test]
fn checkpoint_compacts_the_wal_and_later_puts_survive_reopen() {
    let (_dir, path) = temp_fls();
    {
        let store = FerroStore::open(&path).unwrap();
        store.put_doc("docs", "a", &json!({ "n": 1 })).unwrap();
        store.put_doc("docs", "b", &json!({ "n": 2 })).unwrap();
        store.checkpoint().unwrap();
    }
    let sb = live(&path);
    assert_eq!(sb.wal_length, 0);
    assert_eq!(sb.snapshot_seq, 2);
    assert!(sb.snapshot_length > 0);

    {
        let store = FerroStore::open(&path).unwrap();
        assert_eq!(store.get_doc("docs", "a").unwrap().unwrap()["n"], 1);
        assert_eq!(store.get_doc("docs", "b").unwrap().unwrap()["n"], 2);
        store.put_doc("docs", "c", &json!({ "n": 3 })).unwrap();
    }
    let store = FerroStore::open(&path).unwrap();
    assert_eq!(store.get_doc("docs", "a").unwrap().unwrap()["n"], 1);
    assert_eq!(store.get_doc("docs", "c").unwrap().unwrap()["n"], 3);
    assert!(live(&path).wal_length > 0);
}

#[test]
fn two_handles_commit_and_a_reader_sees_the_put_without_unreachable() {
    let (_dir, path) = temp_fls();
    let writer = FerroStore::open(&path).unwrap();
    let reader = FerroStore::open(&path).unwrap();
    match FerroStore::open(&path) {
        Ok(_) => {}
        Err(StoreError::Unreachable { .. }) => panic!("another open returned Unreachable"),
        Err(err) => panic!("another open failed: {err}"),
    }

    let worker = std::thread::spawn(move || {
        writer
            .put_doc("docs", "k", &json!({ "v": "seen" }))
            .unwrap();
    });
    let start = std::time::Instant::now();
    let seen = loop {
        match reader.get_doc("docs", "k") {
            Ok(Some(value)) => {
                assert_eq!(value, json!({ "v": "seen" }));
                break true;
            }
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(5) {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!("reader: {err}"),
        }
    };
    worker.join().expect("writer panicked");
    assert!(seen, "reader did not observe the committed document");

    let left = FerroStore::open(&path).unwrap();
    let right = FerroStore::open(&path).unwrap();
    let left_task = std::thread::spawn(move || {
        for i in 0..10 {
            left.put_doc("c", &format!("l{i}"), &json!({ "n": i }))
                .unwrap();
        }
    });
    let right_task = std::thread::spawn(move || {
        for i in 0..10 {
            right
                .put_doc("c", &format!("r{i}"), &json!({ "n": i }))
                .unwrap();
        }
    });
    left_task.join().unwrap();
    right_task.join().unwrap();
    let store = FerroStore::open(&path).unwrap();
    for i in 0..10 {
        assert_eq!(
            store.get_doc("c", &format!("l{i}")).unwrap().unwrap()["n"],
            i
        );
        assert_eq!(
            store.get_doc("c", &format!("r{i}")).unwrap().unwrap()["n"],
            i
        );
    }
    store.checkpoint().unwrap();
    // The earlier single put plus 20 concurrent puts.
    assert_eq!(live(&path).snapshot_seq, 21);
    assert_eq!(live(&path).wal_length, 0);
}

#[test]
fn documents_round_trip_field_queries_and_bm25() {
    let (_dir, path) = temp_fls();
    {
        let store = FerroStore::open(&path).unwrap();
        store
            .put_doc(
                "docs",
                "c",
                &json!({ "name": "ada", "n": 1, "ok": true, "skip": null, "tags": ["red"], "nest": { "name": "nope" } }),
            )
            .unwrap();
        store
            .put_doc("docs", "a", &json!({ "name": "ada", "n": 1, "ok": false }))
            .unwrap();
        store
            .put_doc("docs", "b", &json!({ "name": "bea", "n": 2 }))
            .unwrap();
        store
            .put_doc("docs", "f", &json!({ "x": 1.5, "neg": -2 }))
            .unwrap();
        assert_eq!(store.get_doc("docs", "b").unwrap().unwrap()["name"], "bea");
        store
            .put_doc("docs", "b", &json!({ "name": "bea2", "n": 2 }))
            .unwrap();
        assert_eq!(store.get_doc("docs", "b").unwrap().unwrap()["name"], "bea2");

        assert_eq!(
            store.query_by_field("docs", "name", "ada").unwrap(),
            vec!["a".to_string(), "c".to_string()]
        );
        assert_eq!(
            store.query_by_field("docs", "n", "1").unwrap(),
            vec!["a".to_string(), "c".to_string()]
        );
        assert_eq!(
            store.query_by_field("docs", "n", "2").unwrap(),
            vec!["b".to_string()]
        );
        assert_eq!(
            store.query_by_field("docs", "x", "1.5").unwrap(),
            vec!["f".to_string()]
        );
        assert_eq!(
            store.query_by_field("docs", "neg", "-2").unwrap(),
            vec!["f".to_string()]
        );
        assert_eq!(
            store.query_by_field("docs", "ok", "true").unwrap(),
            vec!["c".to_string()]
        );
        assert_eq!(
            store.query_by_field("docs", "ok", "false").unwrap(),
            vec!["a".to_string()]
        );
        assert!(
            store
                .query_by_field("docs", "tags", "red")
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .query_by_field("docs", "skip", "null")
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .query_by_field("docs", "name", "nope")
                .unwrap()
                .is_empty()
        );

        store.delete_doc("docs", "a").unwrap();
        assert!(store.get_doc("docs", "a").unwrap().is_none());
        assert_eq!(
            store.query_by_field("docs", "name", "ada").unwrap(),
            vec!["c".to_string()]
        );
        store.delete_doc("docs", "missing").unwrap();

        store
            .put_doc("bm25", "r", &json!({ "text": "cat cat" }))
            .unwrap();
        store
            .put_doc("bm25", "o", &json!({ "text": "cat dog" }))
            .unwrap();
        store
            .put_doc("bm25", "n", &json!({ "text": "bird bird" }))
            .unwrap();
        store
            .put_doc(
                "bm25",
                "nest",
                &json!({ "outer": { "inner": "zebra zebra" } }),
            )
            .unwrap();
        store
            .put_doc("bm25", "keys", &json!({ "cat": "hound" }))
            .unwrap();
        let hits = store.search_bm25("bm25", "cat", 10).unwrap();
        assert_eq!(hits[0].0, "r");
        assert_eq!(hits[1].0, "o");
        assert!(hits[0].1 > hits[1].1);
        assert!(hits.iter().all(|(id, _)| id != "n" && id != "keys"));
        let zebra = store.search_bm25("bm25", "ZeBra!", 10).unwrap();
        assert_eq!(zebra.len(), 1);
        assert_eq!(zebra[0].0, "nest");
        assert!(
            store
                .search_bm25("bm25", "hound", 10)
                .unwrap()
                .iter()
                .any(|(id, _)| id == "keys")
        );
        assert!(store.search_bm25("bm25", "", 10).unwrap().is_empty());
        assert!(store.search_bm25("bm25", "cat", 0).unwrap().is_empty());
        assert!(store.search_bm25("bm25", "!!!", 5).unwrap().is_empty());
    }

    let store = FerroStore::open(&path).unwrap();
    assert!(store.get_doc("docs", "a").unwrap().is_none());
    assert_eq!(store.get_doc("docs", "b").unwrap().unwrap()["name"], "bea2");
    assert_eq!(
        store.query_by_field("docs", "name", "ada").unwrap(),
        vec!["c".to_string()]
    );
    let hits = store.search_bm25("bm25", "cat", 10).unwrap();
    assert_eq!(hits[0].0, "r");
    assert!(hits[0].1 > hits[1].1);
    assert!(hits.iter().all(|(id, _)| id != "n"));
}

#[test]
fn ferromq_redelivers_until_ack_and_subscribers_see_the_next_publish() {
    let (_dir, path) = temp_fls();
    let store = FerroStore::open(&path).unwrap();
    assert!(store.poll_mq_group("orders", "g", 0).unwrap().is_empty());
    let seq1 = store.publish_mq("orders", "k", &json!({ "a": 1 })).unwrap();
    let seq2 = store.publish_mq("orders", "k2", &json!(2)).unwrap();
    assert_eq!((seq1, seq2), (1, 2));
    assert_eq!(
        store.publish_mq("other", "k", &json!("x")).unwrap(),
        1,
        "sequences are per topic"
    );
    store.ack_mq("orders", "g", 99).unwrap();

    let first = store.poll_mq_group("orders", "g", 1).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].seq, 1);
    let both = store.poll_mq_group("orders", "g", 10).unwrap();
    assert_eq!(both.len(), 2);
    assert_eq!(both[0].seq, 1);
    assert_eq!(both[0].key, "k");
    assert_eq!(both[0].payload, json!({ "a": 1 }));
    assert_eq!(both[1].seq, 2);
    assert_eq!(both[1].payload, json!(2));

    let stored = store.get_doc("mq:orders", "1").unwrap().unwrap();
    assert_eq!(stored["seq"], 1);
    assert_eq!(stored["key"], "k");
    assert_eq!(stored["payload"]["a"], 1);

    store.ack_mq("orders", "g", 1).unwrap();
    store.ack_mq("orders", "g", 1).unwrap();
    let ack = store.get_doc("mq_ack:orders:g", "1").unwrap().unwrap();
    assert_eq!(ack["seq"], 1);
    let after = store.poll_mq_group("orders", "g", 10).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].seq, 2);
    let again = store.poll_mq_group("orders", "g", 10).unwrap();
    assert_eq!(again[0].seq, 2);
    assert_eq!(
        store.poll_mq_group("orders", "other", 10).unwrap().len(),
        2,
        "another group has its own acks"
    );

    store.ack_mq("orders", "g", 2).unwrap();
    assert!(store.poll_mq_group("orders", "g", 10).unwrap().is_empty());

    let rx = store.subscribe_mq("orders");
    assert!(rx.try_recv().is_err(), "subscribe does not replay history");
    let seq3 = store
        .publish_mq("orders", "k3", &json!({ "n": 3 }))
        .unwrap();
    let rec = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(rec.seq, seq3);
    assert_eq!(rec.key, "k3");
    assert_eq!(rec.payload, json!({ "n": 3 }));
    drop(store);

    let store = FerroStore::open(&path).unwrap();
    let pending = store.poll_mq_group("orders", "g", 10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].seq, 3);
    assert_eq!(store.publish_mq("orders", "k4", &json!(4)).unwrap(), 4);
}

#[test]
fn connect_uri_mem_and_file_and_refuses_other_schemes() {
    let mem = FerroStore::connect_uri("mem://").unwrap();
    mem.checkpoint().unwrap();
    mem.put_doc("c", "a", &json!({ "n": 1 })).unwrap();
    assert_eq!(mem.get_doc("c", "a").unwrap().unwrap()["n"], 1);
    let other = FerroStore::connect_uri("mem:///").unwrap();
    other.checkpoint().unwrap();
    assert!(other.get_doc("c", "a").unwrap().is_none());

    assert_eq!(file_uri_path("/tmp/x.fls"), "/tmp/x.fls");
    assert_eq!(file_uri_path("localhost/tmp/x.fls"), "/tmp/x.fls");
    assert_eq!(file_uri_path("rel/x.fls"), "rel/x.fls");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.fls");
    let absolute = format!("file://{}", path.display());
    {
        let store = FerroStore::connect_uri(&absolute).unwrap();
        store.put_doc("c", "a", &json!({ "n": 7 })).unwrap();
    }
    let store = FerroStore::connect_uri(&absolute).unwrap();
    assert_eq!(store.get_doc("c", "a").unwrap().unwrap()["n"], 7);
    let via_host = format!("file://localhost{}", path.display());
    let store = FerroStore::connect_uri(&via_host).unwrap();
    assert_eq!(store.get_doc("c", "a").unwrap().unwrap()["n"], 7);
    drop(store);
    let store = FerroStore::open(&path).unwrap();
    assert_eq!(store.get_doc("c", "a").unwrap().unwrap()["n"], 7);

    let name = format!("ferrostore-rel-{}", std::process::id());
    let _cleanup = RemoveDir(PathBuf::from(&name));
    std::fs::create_dir_all(&name).unwrap();
    let uri = format!("file://{name}/t.fls");
    {
        let store = FerroStore::connect_uri(&uri).unwrap();
        store.put_doc("c", "rel", &json!({ "ok": true })).unwrap();
    }
    let reopened = FerroStore::connect_uri(&uri).unwrap();
    assert_eq!(reopened.get_doc("c", "rel").unwrap().unwrap()["ok"], true);

    for uri in [
        "unix:///tmp/ferro.sock",
        "ws://127.0.0.1:9",
        "wss://example.test/store",
    ] {
        let err = match FerroStore::connect_uri(uri) {
            Ok(_) => panic!("served uri {uri} was accepted"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(msg.contains("Phase 2"), "{uri}: {msg}");
        assert!(!msg.contains("unknown"), "{uri}: {msg}");
        assert!(!msg.contains("deferred"), "{uri}: {msg}");
    }
    for uri in ["", "http://example.test", "bogus", "mem://other"] {
        let err = match FerroStore::connect_uri(uri) {
            Ok(_) => panic!("uri {uri} was accepted"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(msg.contains("unknown"), "{uri}: {msg}");
        assert!(!msg.contains("Phase 2"), "{uri}: {msg}");
        assert!(!msg.contains("deferred"), "{uri}: {msg}");
    }
}

struct RemoveDir(PathBuf);

impl Drop for RemoveDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
