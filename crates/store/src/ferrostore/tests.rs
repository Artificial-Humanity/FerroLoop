use super::layout::{self, Op, Superblock};
use super::{
    AttemptRollup, ColumnDef, ColumnFilter, ColumnPredicate, ColumnType, ColumnValue, FerroStore,
    FerroStoreServer, GateRunRollup, default_socket_path_for_store, file_uri_path,
    latest_superblock, write_slot,
};
use fl_core::StoreError;
use fl_core::ids::{GateId, ProjectId, RecordId, seq_iri};
use fl_core::log::{Attempt, AttemptStatus, GateRun, PathsTouched};
use fl_core::store::Ledger;
use fl_core::verdict::Verdict;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
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
fn zeroed_header_recovers_and_a_longer_headerless_file_does_not() {
    assert_eq!(layout::HEADER_BYTES, 8192);
    let (_dir, path) = temp_fls();
    {
        let file = File::create(&path).unwrap();
        file.set_len(8192).unwrap();
        file.sync_all().unwrap();
    }
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 8192);
    assert!(slots(&path).iter().all(Option::is_none));
    {
        let store = FerroStore::open(&path).unwrap();
        store.put_doc("docs", "a", &json!({ "n": 1 })).unwrap();
    }
    let store = FerroStore::open(&path).unwrap();
    assert_eq!(store.get_doc("docs", "a").unwrap().unwrap()["n"], 1);

    let (_dir, path) = temp_fls();
    let marker = vec![0xA5u8; 8193];
    {
        let file = File::create(&path).unwrap();
        file.write_all_at(&marker, 0).unwrap();
        file.sync_all().unwrap();
    }
    let err = match FerroStore::open(&path) {
        Ok(_) => panic!("open rewrote a long headerless file into a store"),
        Err(err) => err,
    };
    let msg = err.to_string();
    assert!(
        matches!(err, StoreError::Backend(_)) && msg.contains("no valid FerroStore superblock"),
        "{msg}"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 8193);
    let file = File::open(&path).unwrap();
    let mut buf = vec![0u8; marker.len()];
    file.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(buf, marker);
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
fn unpublished_snapshot_past_the_live_extent_keeps_committed_documents() {
    let (_dir, path) = temp_fls();
    {
        let store = FerroStore::open(&path).unwrap();
        store.put_doc("docs", "kept", &json!({ "n": 1 })).unwrap();
        store.put_doc("docs", "also", &json!({ "n": 2 })).unwrap();
    }
    let before = live(&path);
    let mut docs = BTreeMap::new();
    docs.insert(("docs".into(), "kept".into()), json!({ "n": 99 }));
    docs.insert(("docs".into(), "ghost".into()), json!({ "n": 7 }));
    let blob = layout::encode_snapshot(99, &docs).unwrap();
    let snap = layout::decode_snapshot(&blob).unwrap();
    assert_eq!(snap.seq, 99);
    assert_eq!(
        snap.docs.get(&("docs".into(), "ghost".into())).unwrap()["n"],
        7
    );

    {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let (_, sb) = latest_superblock(&file).unwrap();
        let live_end = sb
            .snapshot_offset
            .saturating_add(sb.snapshot_length)
            .max(sb.wal_offset.saturating_add(sb.wal_length))
            .max(layout::HEADER_BYTES);
        let offset = file.metadata().unwrap().len().max(live_end);
        assert!(offset >= live_end);
        file.write_all_at(&blob, offset).unwrap();
        file.sync_all().unwrap();
        assert!(file.metadata().unwrap().len() > live_end);
    }
    assert_eq!(live(&path), before);

    let store = FerroStore::open(&path).unwrap();
    assert_eq!(store.get_doc("docs", "kept").unwrap().unwrap()["n"], 1);
    assert_eq!(store.get_doc("docs", "also").unwrap().unwrap()["n"], 2);
    assert!(store.get_doc("docs", "ghost").unwrap().is_none());
    assert_eq!(live(&path), before);
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

fn must_not_unreachable<T>(result: Result<T, StoreError>) -> Result<T, String> {
    match result {
        Ok(value) => Ok(value),
        Err(err) => {
            assert!(!matches!(err, StoreError::Unreachable { .. }), "{err}");
            Err(err.to_string())
        }
    }
}

#[test]
fn unix_socket_serves_concurrent_clients_without_opening_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("store.fls");
    let sock = dir.path().join("store.sock");
    let server = FerroStoreServer::bind_unix(&sock, FerroStore::open(&file).unwrap()).unwrap();
    let uri = format!("unix://{}", sock.display());

    let mut handles = Vec::new();
    for i in 0..4 {
        let uri = uri.clone();
        handles.push(thread::spawn(move || {
            let client = must_not_unreachable(FerroStore::connect_uri(&uri))?;
            let id = format!("id-{i}");
            must_not_unreachable(client.put_doc(
                "docs",
                &id,
                &json!({"n": i, "name": format!("name{i}")}),
            ))?;
            let doc = must_not_unreachable(client.get_doc("docs", &id))?
                .ok_or_else(|| format!("missing {id}"))?;
            if doc["n"] != i {
                return Err(format!("doc {id} was {doc}"));
            }
            Ok(())
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    let host_uri = format!("unix://localhost{}", sock.display());
    let client = FerroStore::connect_uri(&host_uri).unwrap();
    assert_eq!(
        client.query_by_field("docs", "name", "name1").unwrap(),
        vec!["id-1".to_string()]
    );
    let hits = client.search_bm25("docs", "name2", 10).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].0, "id-2");
    client.delete_doc("docs", "id-3").unwrap();
    assert!(client.get_doc("docs", "id-3").unwrap().is_none());
    client.checkpoint().unwrap();
    client.with_read(|_| Ok(())).unwrap();
    client.with_write(|_| Ok(())).unwrap();

    assert_eq!(
        server.store().get_doc("docs", "id-0").unwrap().unwrap()["n"],
        0
    );
    assert_eq!(
        server.store().get_doc("docs", "id-1").unwrap().unwrap()["name"],
        "name1"
    );
    assert!(server.store().get_doc("docs", "id-3").unwrap().is_none());
    drop(client);
    drop(server);

    let opened = FerroStore::open(&file).unwrap();
    assert_eq!(opened.get_doc("docs", "id-0").unwrap().unwrap()["n"], 0);
    assert_eq!(
        opened.get_doc("docs", "id-1").unwrap().unwrap()["name"],
        "name1"
    );
    assert_eq!(opened.get_doc("docs", "id-2").unwrap().unwrap()["n"], 2);
    assert!(opened.get_doc("docs", "id-3").unwrap().is_none());
}

#[test]
fn unix_socket_mq_subscribe_does_not_replay_and_ack_matches_local() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("store.fls");
    let sock = dir.path().join("mq.sock");
    let server = FerroStoreServer::bind_unix(&sock, FerroStore::open(&file).unwrap()).unwrap();
    let uri = format!("unix://{}", sock.display());

    let publisher = FerroStore::connect_uri(&uri).unwrap();
    let early = publisher
        .publish_mq("orders", "before", &json!({"n": 0}))
        .unwrap();
    assert_eq!(early, 1);

    let client_a = FerroStore::connect_uri(&uri).unwrap();
    let rx = client_a.subscribe_mq("orders");
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "subscribe does not replay history"
    );

    let client_b = FerroStore::connect_uri(&uri).unwrap();
    let seq = client_b
        .publish_mq("orders", "k", &json!({"n": 1}))
        .unwrap();
    let rec = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(rec.seq, seq);
    assert_eq!(rec.key, "k");
    assert_eq!(rec.payload, json!({"n": 1}));
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());

    let polled = client_a.poll_mq_group("orders", "g", 10).unwrap();
    assert_eq!(polled.len(), 2);
    assert_eq!(polled[0].seq, 1);
    assert_eq!(polled[0].key, "before");
    assert_eq!(polled[0].payload, json!({"n": 0}));
    assert_eq!(polled[1].seq, seq);
    assert_eq!(polled[1].key, "k");
    assert_eq!(polled[1].payload, json!({"n": 1}));
    assert_eq!(client_b.poll_mq_group("orders", "g", 10).unwrap().len(), 2);
    client_a.ack_mq("orders", "g", 1).unwrap();
    let after = client_a.poll_mq_group("orders", "g", 10).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].seq, seq);
    assert_eq!(
        client_a.poll_mq_group("orders", "other", 10).unwrap().len(),
        2
    );
    assert!(client_a.poll_mq_group("orders", "g", 0).unwrap().is_empty());
    client_a.ack_mq("orders", "g", seq).unwrap();
    assert!(
        client_a
            .poll_mq_group("orders", "g", 10)
            .unwrap()
            .is_empty()
    );
    client_a.ack_mq("orders", "g", 99).unwrap();
    assert!(
        client_a
            .poll_mq_group("orders", "g", 10)
            .unwrap()
            .is_empty()
    );

    drop(client_a);
    drop(client_b);
    drop(publisher);
    drop(server);
    let opened = FerroStore::open(&file).unwrap();
    assert!(opened.poll_mq_group("orders", "g", 10).unwrap().is_empty());
    assert_eq!(
        opened.poll_mq_group("orders", "other", 10).unwrap().len(),
        2
    );
}

#[test]
fn bind_unix_refuses_a_non_socket_and_drop_unlinks_its_socket() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-socket");
    std::fs::write(&file, b"file").unwrap();
    let err = match FerroStoreServer::bind_unix(&file, FerroStore::in_memory()) {
        Ok(_) => panic!("non-socket path was bound"),
        Err(err) => err,
    };
    assert!(matches!(err, StoreError::Backend(_)), "{err}");
    assert!(file.is_file(), "a non-socket path is left in place");

    let sock = dir.path().join("s.sock");
    let stale = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    drop(stale);
    assert!(sock.exists());
    let server = FerroStoreServer::bind_unix(&sock, FerroStore::in_memory()).unwrap();
    let client = FerroStore::connect_uri(&format!("unix://{}", sock.display())).unwrap();
    client.put_doc("c", "a", &json!(1)).unwrap();
    assert_eq!(server.store().get_doc("c", "a").unwrap().unwrap(), json!(1));
    drop(client);
    drop(server);
    assert!(!sock.exists());
}

#[test]
fn unix_socket_get_doc_returns_stored_null_and_missing_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("null.sock");
    let server = FerroStoreServer::bind_unix(&sock, FerroStore::in_memory()).unwrap();
    let client = FerroStore::connect_uri(&format!("unix://{}", sock.display())).unwrap();
    client.put_doc("docs", "empty", &json!(null)).unwrap();
    let stored = client.get_doc("docs", "empty");
    assert!(
        matches!(stored, Ok(Some(ref value)) if value.is_null()),
        "{stored:?}"
    );
    let missing = client.get_doc("docs", "missing");
    assert!(matches!(missing, Ok(None)), "{missing:?}");
    drop(client);
    drop(server);
}

#[test]
fn unix_socket_subscribe_after_server_drop_is_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gone.sock");
    let server = FerroStoreServer::bind_unix(&sock, FerroStore::in_memory()).unwrap();
    let client = FerroStore::connect_uri(&format!("unix://{}", sock.display())).unwrap();
    drop(server);
    let rx = client.subscribe_mq("orders");
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(400)).unwrap_err(),
        RecvTimeoutError::Disconnected
    );
}

#[test]
fn websocket_serves_concurrent_clients_without_opening_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("store.fls");
    let server =
        FerroStoreServer::bind_tcp("127.0.0.1:0", FerroStore::open(&file).unwrap()).unwrap();
    assert_ne!(server.local_addr().port(), 0);
    let uri = format!("ws://{}", server.local_addr());

    let mut handles = Vec::new();
    for i in 0..4 {
        let uri = uri.clone();
        handles.push(thread::spawn(move || {
            let client = must_not_unreachable(FerroStore::connect_uri(&uri))?;
            let id = format!("id-{i}");
            must_not_unreachable(client.put_doc(
                "docs",
                &id,
                &json!({"n": i, "name": format!("name{i}")}),
            ))?;
            let doc = must_not_unreachable(client.get_doc("docs", &id))?
                .ok_or_else(|| format!("missing {id}"))?;
            if doc["n"] != i {
                return Err(format!("doc {id} was {doc}"));
            }
            Ok(())
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    for i in 0..4 {
        assert_eq!(
            server
                .store()
                .get_doc("docs", &format!("id-{i}"))
                .unwrap()
                .unwrap()["n"],
            i
        );
    }
    drop(server);

    let opened = FerroStore::open(&file).unwrap();
    for i in 0..4 {
        assert_eq!(
            opened.get_doc("docs", &format!("id-{i}")).unwrap().unwrap()["n"],
            i
        );
    }
}

#[test]
fn websocket_mq_subscribe_does_not_replay_and_ack_matches_local() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("store.fls");
    let server =
        FerroStoreServer::bind_tcp("127.0.0.1:0", FerroStore::open(&file).unwrap()).unwrap();
    let uri = format!("ws://{}", server.local_addr());

    let publisher = FerroStore::connect_uri(&uri).unwrap();
    let early = publisher
        .publish_mq("orders", "before", &json!({"n": 0}))
        .unwrap();
    assert_eq!(early, 1);

    let client_a = FerroStore::connect_uri(&uri).unwrap();
    let rx = client_a.subscribe_mq("orders");
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "subscribe does not replay history"
    );

    let client_b = FerroStore::connect_uri(&format!("{uri}/store")).unwrap();
    let seq = client_b
        .publish_mq("orders", "k", &json!({"n": 1}))
        .unwrap();
    let rec = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(rec.seq, seq);
    assert_eq!(rec.key, "k");
    assert_eq!(rec.payload, json!({"n": 1}));
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());

    let polled = client_a.poll_mq_group("orders", "g", 10).unwrap();
    assert_eq!(polled.len(), 2);
    assert_eq!(polled[0].seq, 1);
    assert_eq!(polled[0].key, "before");
    assert_eq!(polled[0].payload, json!({"n": 0}));
    assert_eq!(polled[1].seq, seq);
    assert_eq!(polled[1].key, "k");
    assert_eq!(polled[1].payload, json!({"n": 1}));
    assert_eq!(client_b.poll_mq_group("orders", "g", 10).unwrap().len(), 2);
    client_a.ack_mq("orders", "g", 1).unwrap();
    let after = client_a.poll_mq_group("orders", "g", 10).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].seq, seq);
    assert_eq!(
        client_a.poll_mq_group("orders", "other", 10).unwrap().len(),
        2
    );
    assert!(client_a.poll_mq_group("orders", "g", 0).unwrap().is_empty());
    client_a.ack_mq("orders", "g", seq).unwrap();
    assert!(
        client_a
            .poll_mq_group("orders", "g", 10)
            .unwrap()
            .is_empty()
    );
    client_a.ack_mq("orders", "g", 99).unwrap();
    assert!(
        client_a
            .poll_mq_group("orders", "g", 10)
            .unwrap()
            .is_empty()
    );

    drop(client_a);
    drop(client_b);
    drop(publisher);
    drop(server);
    let opened = FerroStore::open(&file).unwrap();
    assert!(opened.poll_mq_group("orders", "g", 10).unwrap().is_empty());
    assert_eq!(
        opened.poll_mq_group("orders", "other", 10).unwrap().len(),
        2
    );
}

#[test]
fn websocket_get_doc_returns_stored_null() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("null.fls");
    let server =
        FerroStoreServer::bind_tcp("127.0.0.1:0", FerroStore::open(&file).unwrap()).unwrap();
    let client = FerroStore::connect_uri(&format!("ws://{}/store", server.local_addr())).unwrap();
    client.put_doc("docs", "empty", &json!(null)).unwrap();
    let stored = client.get_doc("docs", "empty");
    assert!(
        matches!(stored, Ok(Some(ref value)) if value.is_null()),
        "{stored:?}"
    );
    let missing = client.get_doc("docs", "missing");
    assert!(matches!(missing, Ok(None)), "{missing:?}");
    assert!(
        server
            .store()
            .get_doc("docs", "empty")
            .unwrap()
            .unwrap()
            .is_null()
    );
    drop(client);
    drop(server);
}

#[test]
fn websocket_subscribe_disconnects_when_the_server_drops() {
    let server = FerroStoreServer::bind_tcp("127.0.0.1:0", FerroStore::in_memory()).unwrap();
    let client = FerroStore::connect_uri(&format!("ws://{}", server.local_addr())).unwrap();
    let rx = client.subscribe_mq("orders");
    drop(server);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap_err(),
        RecvTimeoutError::Disconnected
    );
}

#[test]
fn unix_socket_subscribe_disconnects_when_the_server_drops() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("live.sock");
    let server = FerroStoreServer::bind_unix(&sock, FerroStore::in_memory()).unwrap();
    let client = FerroStore::connect_uri(&format!("unix://{}", sock.display())).unwrap();
    let rx = client.subscribe_mq("orders");
    drop(server);
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(500)).unwrap_err(),
        RecvTimeoutError::Disconnected
    );
}

fn committed_frames(path: &Path) -> Vec<(u64, Vec<Op>)> {
    let file = File::open(path).unwrap();
    let sb = live(path);
    let mut buf = vec![0u8; usize::try_from(sb.wal_length).unwrap()];
    if !buf.is_empty() {
        file.read_exact_at(&mut buf, sb.wal_offset).unwrap();
    }
    layout::decode_wal(&buf).unwrap()
}

#[test]
fn ferromq_sequence_survives_acked_delete_checkpoint_and_reopen() {
    let (_dir, path) = temp_fls();
    {
        let store = FerroStore::open(&path).unwrap();
        assert_eq!(
            store.publish_mq("orders", "k", &json!({ "a": 1 })).unwrap(),
            1
        );
        let frames = committed_frames(&path);
        assert_eq!(frames.len(), 1, "message and high-water share one frame");
        let (seq, ops) = &frames[0];
        assert_eq!(*seq, 1);
        assert_eq!(ops.len(), 2);
        match &ops[..] {
            [
                Op::Put {
                    collection: message_collection,
                    id: message_id,
                    doc: message,
                },
                Op::Put {
                    collection: hw_collection,
                    id: hw_id,
                    doc: hw,
                },
            ] => {
                assert_eq!(message_collection, "mq:orders");
                assert_eq!(message_id, "1");
                assert_eq!(message["seq"], 1);
                assert_eq!(hw_collection, "mq_hw:orders");
                assert_eq!(hw_id, "high_water");
                assert_eq!(hw["seq"], 1);
            }
            other => panic!("expected message and high-water puts, got {other:?}"),
        }

        store.ack_mq("orders", "g", 1).unwrap();
        store.delete_doc("mq:orders", "1").unwrap();
        assert!(store.get_doc("mq:orders", "1").unwrap().is_none());
        assert_eq!(
            store
                .get_doc("mq_hw:orders", "high_water")
                .unwrap()
                .unwrap()["seq"],
            1
        );
        store.checkpoint().unwrap();
    }

    let store = FerroStore::open(&path).unwrap();
    assert!(store.get_doc("mq:orders", "1").unwrap().is_none());
    assert_eq!(
        store
            .get_doc("mq_hw:orders", "high_water")
            .unwrap()
            .unwrap()["seq"],
        1
    );
    let seq = store.publish_mq("orders", "k2", &json!(2)).unwrap();
    assert_eq!(seq, 2);
    let pending = store.poll_mq_group("orders", "g", 10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].seq, 2);
    assert_eq!(pending[0].key, "k2");
    assert_eq!(pending[0].payload, json!(2));

    // A file that already has messages and no high-water document.
    store
        .put_doc(
            "mq:legacy",
            "4",
            &json!({ "seq": 4, "key": "old", "payload": true }),
        )
        .unwrap();
    assert_eq!(store.publish_mq("legacy", "n", &json!(1)).unwrap(), 5);
    store.delete_doc("mq:legacy", "4").unwrap();
    store.delete_doc("mq:legacy", "5").unwrap();
    assert_eq!(store.publish_mq("legacy", "n2", &json!(2)).unwrap(), 6);
    let legacy = store.poll_mq_group("legacy", "g", 10).unwrap();
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0].seq, 6);
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

    for uri in ["ws://127.0.0.1:9", "ws://127.0.0.1:9/store"] {
        let err = match FerroStore::connect_uri(uri) {
            Ok(_) => panic!("closed ws port {uri} was accepted"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(matches!(err, StoreError::Backend(_)), "{uri}: {msg}");
        assert!(!msg.contains("unknown"), "{uri}: {msg}");
        assert!(!msg.contains("Phase 2"), "{uri}: {msg}");
        assert!(!msg.contains("deferred"), "{uri}: {msg}");
    }
    let wss = match FerroStore::connect_uri("wss://example.test/store") {
        Ok(_) => panic!("wss uri was accepted"),
        Err(err) => err,
    };
    let msg = wss.to_string();
    assert!(matches!(wss, StoreError::Backend(_)), "{msg}");
    assert!(msg.contains("wss"), "{msg}");
    assert!(!msg.contains("unknown"), "{msg}");
    assert!(!msg.contains("Phase 2"), "{msg}");
    assert!(!msg.contains("deferred"), "{msg}");
    let missing = match FerroStore::connect_uri("unix:///tmp/ferro.sock") {
        Ok(_) => panic!("missing unix socket was accepted"),
        Err(err) => err,
    };
    let msg = missing.to_string();
    assert!(matches!(missing, StoreError::Backend(_)), "{msg}");
    assert!(!msg.contains("unknown"), "{msg}");
    assert!(!msg.contains("Phase 2"), "{msg}");
    assert!(!msg.contains("deferred"), "{msg}");
    let via_host = match FerroStore::connect_uri("unix://localhost/tmp/ferro.sock") {
        Ok(_) => panic!("missing unix socket was accepted"),
        Err(err) => err,
    };
    let msg = via_host.to_string();
    assert!(msg.contains("/tmp/ferro.sock"), "{msg}");
    assert!(!msg.contains("unknown"), "{msg}");
    assert!(!msg.contains("Phase 2"), "{msg}");
    let rel = format!("unix://ferrostore-missing-{}/x.sock", std::process::id());
    let missing_rel = match FerroStore::connect_uri(&rel) {
        Ok(_) => panic!("missing unix socket was accepted"),
        Err(err) => err,
    };
    let msg = missing_rel.to_string();
    assert!(
        msg.contains(&format!("ferrostore-missing-{}/x.sock", std::process::id())),
        "{msg}"
    );
    assert!(!msg.contains("unknown"), "{msg}");
    assert!(!msg.contains("Phase 2"), "{msg}");
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

#[test]
fn with_write_is_one_frame_and_a_failure_or_empty_body_writes_nothing() {
    let (_dir, path) = temp_fls();
    let store = FerroStore::open(&path).unwrap();
    store
        .with_write(|txn| {
            txn.put("c", "a", json!(1));
            assert_eq!(txn.get("c", "a").unwrap(), json!(1));
            txn.put("c", "a", json!(2));
            assert_eq!(txn.get("c", "a").unwrap(), json!(2));
            txn.put("c", "b", json!(3));
            txn.delete("c", "b");
            assert!(txn.get("c", "b").is_none());
            txn.put("d", "z", json!(4));
            let ids: Vec<_> = txn.scan("c").into_iter().map(|(id, _)| id).collect();
            assert_eq!(ids, vec!["a".to_string()]);
            assert!(txn.scan("missing").is_empty());
            Ok(())
        })
        .unwrap();
    assert_eq!(store.get_doc("c", "a").unwrap().unwrap(), json!(2));
    assert!(store.get_doc("c", "b").unwrap().is_none());
    assert_eq!(store.get_doc("d", "z").unwrap().unwrap(), json!(4));

    let err = store
        .with_write(|txn| {
            txn.put("c", "nope", json!(9));
            Err::<(), _>(layout::backend("rollback"))
        })
        .unwrap_err();
    assert!(err.to_string().contains("rollback"), "{err}");
    assert!(store.get_doc("c", "nope").unwrap().is_none());

    store
        .with_write(|txn| {
            assert_eq!(txn.get("c", "a").unwrap(), json!(2));
            Ok(())
        })
        .unwrap();

    store.checkpoint().unwrap();
    assert_eq!(live(&path).snapshot_seq, 1);
    assert_eq!(live(&path).wal_length, 0);

    let mem = FerroStore::in_memory();
    mem.with_write(|txn| {
        txn.put("c", "a", json!("mem"));
        assert_eq!(txn.scan("c").len(), 1);
        txn.delete("c", "a");
        assert!(txn.scan("c").is_empty());
        txn.put("c", "b", json!(1));
        Ok(())
    })
    .unwrap();
    assert!(mem.get_doc("c", "a").unwrap().is_none());
    assert_eq!(mem.get_doc("c", "b").unwrap().unwrap(), json!(1));
    assert!(
        mem.with_write(|txn| {
            txn.put("c", "gone", json!(1));
            Err::<(), _>(layout::backend("rollback"))
        })
        .is_err()
    );
    assert!(mem.get_doc("c", "gone").unwrap().is_none());
}

#[test]
fn ferrostore_meets_the_store_contracts() {
    use fl_core::conformance::{Single, SplitOver};
    let fresh = || (FerroStore::in_memory(), ());
    let single = || {
        let (s, g) = fresh();
        Single(s, g)
    };
    let split = || {
        let (s, g) = fresh();
        SplitOver(s, g)
    };
    fl_core::conformance::catalog(fresh);
    fl_core::conformance::tracker(single);
    fl_core::conformance::ledger(single);
    fl_core::conformance::ledger(split);
    fl_core::conformance::split_ledger(split);
    fl_core::conformance::all_roles(single);
    fl_core::conformance::local_handles(fresh);
    fl_core::conformance::ledger_cache(fresh);
    fl_core::conformance::agent_registry(fresh);
    fl_core::conformance::state_store(fresh);
    fl_core::conformance::message_store(fresh);
    fl_core::conformance::workflow_store(fresh);
    fl_core::conformance::escalations(|| {
        let (s, g) = fresh();
        Single(s, g)
    });
}

#[test]
fn project_record_and_state_survive_reopen() {
    use fl_core::model::State;
    use fl_core::store::{Catalog, Tracker};
    let (_dir, path) = temp_fls();
    let (project, record) = {
        let store = FerroStore::open(&path).unwrap();
        let project = store.add_project("/work/app").unwrap();
        let record = store.add_record(&project, "keep this").unwrap();
        store.set_record_state(&record, State::Doing).unwrap();
        (project, record)
    };
    let store = FerroStore::open(&path).unwrap();
    let loaded = store.get_project(&project).unwrap().expect("project");
    assert_eq!(loaded.id, project);
    assert_eq!(loaded.root, "/work/app");
    let records = store.list_records(&project).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, record);
    assert_eq!(records[0].title, "keep this");
    assert_eq!(records[0].state, State::Doing);
}

struct RemoveDir(PathBuf);

impl Drop for RemoveDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn gate_kind() -> fl_core::model::GateKind {
    fl_core::model::GateKind::Command(fl_core::model::CommandSpec {
        program: "true".into(),
        args: vec![],
        delivery: fl_core::model::PopulationDelivery::Args,
        timeout_secs: 5,
        pass_codes: vec![0],
    })
}

fn gate_selector() -> fl_core::model::Selector {
    fl_core::model::Selector::Glob {
        pattern: "**/*.rs".into(),
    }
}

fn redb_format(store: &crate::RedbStore) -> u64 {
    use redb::ReadableDatabase;
    let tx = store.db.begin_read().unwrap();
    let meta = tx.open_table(crate::META).unwrap();
    meta.get(crate::FORMAT_KEY)
        .unwrap()
        .expect("format version")
        .value()
}

#[test]
fn a_redb_store_migrates_into_ferrostore_without_reminting_ids() {
    use fl_core::agent::{AgentRole, AreaRecord, RegisterAgentArgs};
    use fl_core::at::At;
    use fl_core::conformance::{entry_iri, sample_attempt, sample_record_run};
    use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress, WireFilter};
    use fl_core::finding::Finding;
    use fl_core::ids::{Kind, RecordId, WorkflowId, WorkflowRunId, seq_iri};
    use fl_core::model::{Regret, State, Transition};
    use fl_core::split::{CachedSegment, LedgerCache};
    use fl_core::state::{
        HandoffEntry, SensitivityPatch, StatePayload, StateScope, UpsertStateArgs,
    };
    use fl_core::store::{
        AgentRegistry, Bindings, Catalog, Handles, Ledger, MessageStore, StateStore, Tracker,
        WorkflowStore,
    };
    use fl_core::workflow::{
        AssigneeRule, DEFAULT_MAX_TOTAL_STEPS, DEFAULT_STEP_MAX_ATTEMPTS, DEFAULT_STEP_MAX_VISITS,
        END_STEP, EdgeCondition, EdgeDef, JoinPolicy, StepCompletion, StepDef, WorkflowDef,
        WorkflowRun,
    };
    use std::collections::BTreeMap;

    use crate::RedbStore;
    use crate::migrate_redb_to_ferrostore;

    let dir = tempfile::tempdir().unwrap();
    let redb = RedbStore::open(&dir.path().join("source.redb")).unwrap();
    let project = redb.add_project("/work/app").unwrap();
    let gate = redb
        .add_gate(&project, "fmt", gate_kind(), gate_selector(), 1, "c1", "o")
        .unwrap();
    redb.add_transition(Transition {
        project: project.clone(),
        name: "ship".into(),
        from: State::Review,
        to: State::Done,
        regret: Regret::High,
        gates: vec![gate.clone()],
    })
    .unwrap();
    let record = redb.add_record(&project, "keep this").unwrap();
    redb.set_record_state(&record, State::Doing).unwrap();
    let finding = redb
        .add_finding(Finding::raise(
            project.clone(),
            record.clone(),
            "reviewer",
            "wrong on empty",
        ))
        .unwrap();
    let alias = entry_iri(90);
    redb.add_alias(record.iri(), alias.clone()).unwrap();
    redb.append_gate_run(sample_record_run(1, &gate, Some(&record)))
        .unwrap();
    redb.append_attempt(sample_attempt(2, &project, &record))
        .unwrap();

    let area = AreaRecord::new(
        "ferroloop",
        Some("FerroLoop/".into()),
        "Agent orchestration",
        At::from_unix_millis(1),
        "owner",
    )
    .unwrap();
    redb.upsert_area(area).unwrap();
    let agent = redb
        .register_agent(RegisterAgentArgs {
            display_name: "ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: AgentRole::Lead,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(2),
            by: "owner".into(),
        })
        .unwrap();
    let state = redb
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: "rule".into(),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Handoff(HandoffEntry {
                summary: "v6".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();

    let workflow = redb
        .upsert_workflow_def(WorkflowDef {
            id: WorkflowId(seq_iri(801)),
            project: project.clone(),
            slug: "ci-flow".into(),
            title: "CI Flow".into(),
            start_step: "build".into(),
            steps: vec![StepDef {
                step_id: "build".into(),
                title: "Build".into(),
                prompt: "Run build.".into(),
                assignee: AssigneeRule::Agent {
                    name: "felicia".into(),
                },
                reviews_step: None,
                require_different_vendor: false,
                completion: StepCompletion::Ungated,
                join_policy: JoinPolicy::All,
                max_attempts: DEFAULT_STEP_MAX_ATTEMPTS,
                max_visits: DEFAULT_STEP_MAX_VISITS,
                budget_usd_micros: None,
                max_tokens: None,
                isolate_worktree: false,
            }],
            edges: vec![EdgeDef {
                from_step: "build".into(),
                to_step: END_STEP.into(),
                on: EdgeCondition::Pass,
            }],
            max_total_steps: DEFAULT_MAX_TOTAL_STEPS,
            budget_usd_micros: None,
            max_tokens: None,
            declared_keys: BTreeMap::new(),
            authored_at_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            authored_by: "ferris".into(),
        })
        .unwrap();
    let workflow_run = redb
        .create_workflow_run(
            WorkflowRun::new(
                WorkflowRunId(seq_iri(802)),
                &workflow,
                "CI Run 1",
                At::from_unix_millis(100),
                "ferris",
            )
            .unwrap(),
        )
        .unwrap();

    let (message, _) = redb
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: Some("ai-lab-0".into()),
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "Format 7 test".into(),
            body: "Raise to 7".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(10),
        })
        .unwrap();

    let root = "0123456789abcdef0123456789abcdef01234567";
    redb.set_ledger_root("R_1", root).unwrap();
    let segment = CachedSegment {
        oid: "o1".into(),
        bytes: vec![0xff, 0xfe, 0xfd],
        closed: false,
    };
    redb.remember(
        "R_1",
        "c1",
        &[("runs/aa/1.jsonl".to_string(), segment.clone())],
    )
    .unwrap();

    let version = redb_format(&redb);
    let fls = dir.path().join("migrated.fls");
    {
        let ferro = FerroStore::open(&fls).unwrap();
        migrate_redb_to_ferrostore(&redb, &ferro).unwrap();
    }
    let ferro = FerroStore::open(&fls).unwrap();

    assert_eq!(
        ferro.get_project(&project).unwrap(),
        redb.get_project(&project).unwrap()
    );
    assert_eq!(
        ferro.get_gate(&gate).unwrap(),
        redb.get_gate(&gate).unwrap()
    );
    assert_eq!(
        ferro.get_transition(&project, "ship").unwrap(),
        redb.get_transition(&project, "ship").unwrap()
    );
    assert_eq!(
        ferro.get_record(&record).unwrap(),
        redb.get_record(&record).unwrap()
    );
    assert_eq!(
        ferro.get_record(&record).unwrap().unwrap().state,
        State::Doing
    );
    assert_eq!(
        ferro
            .get_record(&RecordId(alias.clone()))
            .unwrap()
            .unwrap()
            .id,
        record
    );
    assert_eq!(
        ferro.get_finding(&finding).unwrap(),
        redb.get_finding(&finding).unwrap()
    );
    assert_eq!(
        ferro.gate_runs(&gate).unwrap(),
        redb.gate_runs(&gate).unwrap()
    );
    assert_eq!(
        ferro.attempts(&project).unwrap(),
        redb.attempts(&project).unwrap()
    );
    assert_eq!(
        ferro.get_area("ferroloop").unwrap(),
        redb.get_area("ferroloop").unwrap()
    );
    assert_eq!(
        ferro.get_agent(&agent.id).unwrap(),
        redb.get_agent(&agent.id).unwrap()
    );
    assert_eq!(
        ferro.get_state(&state.id).unwrap(),
        redb.get_state(&state.id).unwrap()
    );
    assert_eq!(
        ferro.get_workflow_def(&workflow.id).unwrap(),
        redb.get_workflow_def(&workflow.id).unwrap()
    );
    assert_eq!(
        ferro.get_workflow_run(&workflow_run.id).unwrap(),
        redb.get_workflow_run(&workflow_run.id).unwrap()
    );
    assert_eq!(
        ferro.get_wire_message(&message.id).unwrap(),
        redb.get_wire_message(&message.id).unwrap()
    );
    let filter = WireFilter {
        recipient: Some("ferris".into()),
        ..WireFilter::default()
    };
    assert_eq!(
        ferro.list_wire_inbox(&filter).unwrap(),
        redb.list_wire_inbox(&filter).unwrap()
    );
    assert!(!ferro.list_wire_inbox(&filter).unwrap().is_empty());
    assert_eq!(
        ferro.cached("R_1", "runs/aa/1.jsonl").unwrap(),
        Some(segment)
    );
    assert_eq!(ferro.ledger_root("R_1").unwrap().as_deref(), Some(root));
    for (kind, id) in [
        (Kind::Project, project.iri()),
        (Kind::Record, record.iri()),
        (Kind::Agent, agent.id.iri()),
    ] {
        let handle = redb.handle_of(kind, id).unwrap();
        assert!(handle.is_some(), "{kind:?} has no handle");
        assert_eq!(ferro.handle_of(kind, id).unwrap(), handle);
    }
    assert_eq!(
        ferro.get_doc("meta", "format_version").unwrap(),
        Some(json!(version))
    );
}

#[test]
fn migration_refuses_a_non_empty_store_and_an_empty_redb_stays_empty() {
    use fl_core::store::{Catalog, StoreError};

    use crate::RedbStore;
    use crate::migrate_redb_to_ferrostore;

    let dir = tempfile::tempdir().unwrap();
    let redb = RedbStore::open(&dir.path().join("empty.redb")).unwrap();
    let mem = FerroStore::in_memory();
    migrate_redb_to_ferrostore(&redb, &mem).unwrap();
    assert!(mem.list_projects().unwrap().is_empty());
    let project = mem.add_project("/p").unwrap();
    assert_eq!(mem.get_project(&project).unwrap().unwrap().root, "/p");
    assert_eq!(mem.get_project(&project).unwrap().unwrap().id, project);

    let occupied = FerroStore::in_memory();
    occupied
        .put_doc("docs", "kept", &json!({ "ok": true }))
        .unwrap();
    let err = migrate_redb_to_ferrostore(&redb, &occupied).unwrap_err();
    assert!(matches!(err, StoreError::Backend(_)), "{err}");
    assert_eq!(
        occupied.get_doc("docs", "kept").unwrap(),
        Some(json!({ "ok": true }))
    );
    assert!(occupied.list_projects().unwrap().is_empty());
    assert!(
        occupied
            .get_doc("meta", "format_version")
            .unwrap()
            .is_none()
    );
}

#[test]
fn nonzero_short_file_is_not_initialized() {
    let (_dir, path) = temp_fls();
    {
        let file = File::create(&path).unwrap();
        file.write_all_at(&[0x01], 0).unwrap();
        file.sync_all().unwrap();
    }
    let err = match FerroStore::open(&path) {
        Ok(_) => panic!("open initialized a one-byte non-zero file"),
        Err(err) => err,
    };
    let msg = err.to_string();
    assert!(
        matches!(err, StoreError::Backend(_)) && msg.contains("no valid FerroStore superblock"),
        "{msg}"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 1);
    let file = File::open(&path).unwrap();
    let mut buf = [0u8; 1];
    file.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(buf, [0x01]);

    let (_dir, path) = temp_fls();
    {
        FerroStore::open(&path).unwrap();
    }
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 8192);
    let created = slots(&path);
    assert!(created[0].is_some());
    assert!(created[1].is_none());
    corrupt_live_crc(&path);
    assert!(slots(&path).iter().all(Option::is_none));
    let file = File::open(&path).unwrap();
    let mut before = [0u8; 8192];
    file.read_exact_at(&mut before, 0).unwrap();
    drop(file);
    let err = match FerroStore::open(&path) {
        Ok(_) => panic!("open rewrote a header whose live crc was torn"),
        Err(err) => err,
    };
    let msg = err.to_string();
    assert!(
        matches!(err, StoreError::Backend(_)) && msg.contains("no valid FerroStore superblock"),
        "{msg}"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 8192);
    let file = File::open(&path).unwrap();
    let mut after = [0u8; 8192];
    file.read_exact_at(&mut after, 0).unwrap();
    assert_eq!(after, before);

    let (_dir, path) = temp_fls();
    {
        let file = File::create(&path).unwrap();
        file.write_all_at(&[0x00], 0).unwrap();
        file.sync_all().unwrap();
    }
    FerroStore::open(&path).unwrap();
    assert_eq!(slots(&path)[0].unwrap().epoch, 1);
    assert!(slots(&path)[1].is_none());
}

#[test]
fn parse_superblock_rejects_epoch_zero_and_illegal_extents() {
    let legal = Superblock {
        epoch: 1,
        snapshot_offset: 8192,
        snapshot_length: 0,
        wal_offset: 8192,
        wal_length: 0,
        snapshot_seq: 0,
    };
    assert_eq!(
        layout::parse_superblock(&layout::encode_superblock(&legal)),
        Some(legal)
    );

    let mut epoch_zero = legal;
    epoch_zero.epoch = 0;
    assert!(layout::parse_superblock(&layout::encode_superblock(&epoch_zero)).is_none());

    let mut snapshot_short = legal;
    snapshot_short.snapshot_offset = 8191;
    assert!(layout::parse_superblock(&layout::encode_superblock(&snapshot_short)).is_none());

    let mut wal_inside_header = legal;
    wal_inside_header.wal_offset = 0;
    wal_inside_header.wal_length = 0;
    assert!(layout::parse_superblock(&layout::encode_superblock(&wal_inside_header)).is_none());

    let mut snapshot_wraps = legal;
    snapshot_wraps.snapshot_offset = 8192;
    snapshot_wraps.snapshot_length = u64::MAX;
    assert!(layout::parse_superblock(&layout::encode_superblock(&snapshot_wraps)).is_none());

    let mut wal_wraps = legal;
    wal_wraps.wal_offset = 8192;
    wal_wraps.wal_length = u64::MAX;
    assert!(layout::parse_superblock(&layout::encode_superblock(&wal_wraps)).is_none());
}

#[test]
fn apply_frames_skips_snapshot_prefix_and_rejects_bad_seqs() {
    let put = |id: &str| Op::Put {
        collection: "docs".into(),
        id: id.into(),
        doc: json!(id),
    };
    let assert_corrupt = |err: StoreError| {
        let msg = err.to_string();
        assert!(
            matches!(err, StoreError::Backend(_)) && msg.contains("corrupt"),
            "{msg}"
        );
    };

    let mut state = super::state::DocState::at_seq(0);
    let err = super::state::apply_frames(&mut state, 0, &[(0, vec![put("zero")])]).unwrap_err();
    assert_corrupt(err);
    assert!(state.docs.is_empty());
    assert_eq!(state.last_seq, 0);

    let mut state = super::state::DocState::at_seq(10);
    state
        .docs
        .insert(("docs".into(), "kept".into()), json!("kept"));
    let err = super::state::apply_frames(
        &mut state,
        10,
        &[(9, vec![put("down")]), (8, vec![put("lower")])],
    )
    .unwrap_err();
    assert_corrupt(err);
    assert_eq!(
        state.docs.get(&("docs".into(), "kept".into())),
        Some(&json!("kept"))
    );
    assert_eq!(state.docs.len(), 1);
    assert_eq!(state.last_seq, 10);

    let mut state = super::state::DocState::at_seq(10);
    state
        .docs
        .insert(("docs".into(), "kept".into()), json!("kept"));
    let err = super::state::apply_frames(
        &mut state,
        10,
        &[(8, vec![put("dup")]), (8, vec![put("again")])],
    )
    .unwrap_err();
    assert_corrupt(err);
    assert_eq!(
        state.docs.get(&("docs".into(), "kept".into())),
        Some(&json!("kept"))
    );
    assert_eq!(state.docs.len(), 1);
    assert_eq!(state.last_seq, 10);

    let mut state = super::state::DocState::at_seq(2);
    state
        .docs
        .insert(("docs".into(), "kept".into()), json!("kept"));
    super::state::apply_frames(
        &mut state,
        2,
        &[
            (1, vec![put("skip-a")]),
            (2, vec![put("skip-b")]),
            (3, vec![put("new")]),
        ],
    )
    .unwrap();
    assert_eq!(
        state.docs.get(&("docs".into(), "kept".into())),
        Some(&json!("kept"))
    );
    assert_eq!(
        state.docs.get(&("docs".into(), "new".into())),
        Some(&json!("new"))
    );
    assert!(!state.docs.contains_key(&("docs".into(), "skip-a".into())));
    assert!(!state.docs.contains_key(&("docs".into(), "skip-b".into())));
    assert_eq!(state.last_seq, 3);

    let mut state = super::state::DocState::at_seq(5);
    super::state::apply_frames(
        &mut state,
        5,
        &[(3, vec![put("old")]), (4, vec![put("older")])],
    )
    .unwrap();
    assert!(state.docs.is_empty());
    assert_eq!(state.last_seq, 5);

    let mut state = super::state::DocState::at_seq(5);
    let err = super::state::apply_frames(
        &mut state,
        5,
        &[(4, vec![put("skip")]), (7, vec![put("gap")])],
    )
    .unwrap_err();
    assert_corrupt(err);
    assert!(state.docs.is_empty());
    assert_eq!(state.last_seq, 5);

    let mut state = super::state::DocState::at_seq(0);
    super::state::apply_frames(
        &mut state,
        0,
        &[(1, vec![put("one")]), (2, vec![put("two")])],
    )
    .unwrap();
    assert_eq!(
        state.docs.get(&("docs".into(), "one".into())),
        Some(&json!("one"))
    );
    assert_eq!(
        state.docs.get(&("docs".into(), "two".into())),
        Some(&json!("two"))
    );
    assert_eq!(state.last_seq, 2);
}

#[test]
fn default_socket_path_for_store_names_the_socket_beside_the_file() {
    let path = default_socket_path_for_store("dir/store.fls");
    assert_eq!(path, PathBuf::from("dir/store.fls.sock"));
    assert!(path.ends_with("store.fls.sock"));
    assert_eq!(path.parent(), Some(Path::new("dir")));
    assert_eq!(
        default_socket_path_for_store("/"),
        PathBuf::from("/ferrostore.sock")
    );
    assert_eq!(
        default_socket_path_for_store(".."),
        PathBuf::from("../ferrostore.sock")
    );
}

#[test]
fn auto_uses_the_listening_socket_and_otherwise_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.fls");

    {
        let store = FerroStore::auto(&path).unwrap();
        store.put_doc("docs", "local", &json!({"n": 1})).unwrap();
    }
    let opened = FerroStore::open(&path).unwrap();
    assert_eq!(opened.get_doc("docs", "local").unwrap().unwrap()["n"], 1);
    drop(opened);

    // A socket file with nobody listening is a refused connect, not an error.
    let sock = default_socket_path_for_store(&path);
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    {
        let store = FerroStore::auto(&path).unwrap();
        store.put_doc("docs", "stale", &json!({"n": 4})).unwrap();
    }
    let opened = FerroStore::open(&path).unwrap();
    assert_eq!(opened.get_doc("docs", "stale").unwrap().unwrap()["n"], 4);
    drop(opened);

    let server = FerroStoreServer::bind_unix(&sock, FerroStore::open(&path).unwrap()).unwrap();
    let client = FerroStore::auto(&path).unwrap();
    client.with_read(|_| Ok(())).unwrap();
    client.put_doc("docs", "remote", &json!({"n": 2})).unwrap();
    assert_eq!(
        server.store().get_doc("docs", "remote").unwrap().unwrap()["n"],
        2
    );
    drop(client);
    drop(server);

    let client = FerroStore::auto(&path).unwrap();
    assert_eq!(client.get_doc("docs", "remote").unwrap().unwrap()["n"], 2);
    client.put_doc("docs", "again", &json!({"n": 3})).unwrap();
    drop(client);
    let opened = FerroStore::open(&path).unwrap();
    assert_eq!(opened.get_doc("docs", "again").unwrap().unwrap()["n"], 3);
    assert_eq!(opened.get_doc("docs", "remote").unwrap().unwrap()["n"], 2);
}

fn exercise_store_traits(store: &FerroStore, tag: &str) {
    use fl_core::agent::{AgentRole, AreaRecord, RegisterAgentArgs};
    use fl_core::at::At;
    use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
    use fl_core::ids::{WorkflowId, WorkflowRunId, seq_iri};
    use fl_core::model::State;
    use fl_core::state::{
        HandoffEntry, SensitivityPatch, StatePayload, StateScope, UpsertStateArgs,
    };
    use fl_core::store::{
        AgentRegistry, Catalog, MessageStore, StateStore, Tracker, WorkflowStore,
    };
    use fl_core::workflow::{
        AssigneeRule, DEFAULT_MAX_TOTAL_STEPS, DEFAULT_STEP_MAX_ATTEMPTS, DEFAULT_STEP_MAX_VISITS,
        END_STEP, EdgeCondition, EdgeDef, JoinPolicy, StepCompletion, StepDef, WorkflowDef,
        WorkflowRun,
    };
    use std::collections::BTreeMap;

    let root = format!("/work/{tag}");
    let project = store.add_project(&root).unwrap();
    let loaded = store.get_project(&project).unwrap().expect("project");
    assert_eq!(loaded.id, project);
    assert_eq!(loaded.root, root);

    let title = format!("title {tag}");
    let record = store.add_record(&project, &title).unwrap();
    store.set_record_state(&record, State::Doing).unwrap();
    let got = store.get_record(&record).unwrap().expect("record");
    assert_eq!(got.id, record);
    assert_eq!(got.state, State::Doing);
    assert_eq!(got.title, title);

    let area_name = format!("area-{tag}");
    store
        .upsert_area(
            AreaRecord::new(
                &area_name,
                Some(format!("{tag}/")),
                "Agent orchestration",
                At::from_unix_millis(1),
                "owner",
            )
            .unwrap(),
        )
        .unwrap();
    let agent_name = format!("agent-{tag}");
    let agent = store
        .register_agent(RegisterAgentArgs {
            display_name: agent_name.clone(),
            email: None,
            project: area_name.clone(),
            role: AgentRole::Lead,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(2),
            by: "owner".into(),
        })
        .unwrap();
    let fetched = store.get_agent(&agent.id).unwrap().expect("agent");
    assert_eq!(fetched.id, agent.id);
    assert_eq!(fetched.display_name, agent_name);
    assert_eq!(fetched.project, area_name);

    let slug = format!("rule-{tag}");
    let state = store
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: slug.clone(),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Handoff(HandoffEntry {
                summary: "v6".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let got_state = store.get_state(&state.id).unwrap().expect("state");
    assert_eq!(got_state.id, state.id);
    assert_eq!(got_state.slug, slug);

    let flow = format!("flow-{tag}");
    let workflow = store
        .upsert_workflow_def(WorkflowDef {
            id: WorkflowId(seq_iri(900)),
            project: project.clone(),
            slug: flow.clone(),
            title: format!("CI {tag}"),
            start_step: "build".into(),
            steps: vec![StepDef {
                step_id: "build".into(),
                title: "Build".into(),
                prompt: "Run build.".into(),
                assignee: AssigneeRule::Agent {
                    name: "felicia".into(),
                },
                reviews_step: None,
                require_different_vendor: false,
                completion: StepCompletion::Ungated,
                join_policy: JoinPolicy::All,
                max_attempts: DEFAULT_STEP_MAX_ATTEMPTS,
                max_visits: DEFAULT_STEP_MAX_VISITS,
                budget_usd_micros: None,
                max_tokens: None,
                isolate_worktree: false,
            }],
            edges: vec![EdgeDef {
                from_step: "build".into(),
                to_step: END_STEP.into(),
                on: EdgeCondition::Pass,
            }],
            max_total_steps: DEFAULT_MAX_TOTAL_STEPS,
            budget_usd_micros: None,
            max_tokens: None,
            declared_keys: BTreeMap::new(),
            authored_at_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            authored_by: "ferris".into(),
        })
        .unwrap();
    let got_workflow = store
        .get_workflow_def(&workflow.id)
        .unwrap()
        .expect("workflow");
    assert_eq!(got_workflow.slug, flow);
    assert_eq!(got_workflow.project, project);

    let run = store
        .create_workflow_run(
            WorkflowRun::new(
                WorkflowRunId(seq_iri(901)),
                &got_workflow,
                format!("CI run {tag}"),
                At::from_unix_millis(20),
                "ferris",
            )
            .unwrap(),
        )
        .unwrap();
    let got_run = store.get_workflow_run(&run.id).unwrap().expect("run");
    assert_eq!(got_run.id, run.id);
    assert_eq!(got_run.workflow_id, got_workflow.id);

    let import_def = got_workflow.clone();
    let import_run = got_run.clone();
    store.import_workflow_def(import_def).unwrap();
    store.import_workflow_run(import_run).unwrap();
    assert_eq!(
        store.get_workflow_def(&got_workflow.id).unwrap().as_ref(),
        Some(&got_workflow)
    );
    assert_eq!(
        store.get_workflow_run(&got_run.id).unwrap().as_ref(),
        Some(&got_run)
    );

    let body = format!("Body {tag}");
    let (message, _) = store
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: Some("ai-lab-0".into()),
            to: WireAddress::AgentName(agent_name),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: format!("Hello {tag}"),
            body: body.clone(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(10),
        })
        .unwrap();
    let got_message = store
        .get_wire_message(&message.id)
        .unwrap()
        .expect("wire message");
    assert_eq!(got_message.id, message.id);
    assert_eq!(got_message.body, body);
}

fn assert_concurrent_increments(connect: impl Fn() -> FerroStore + Sync, path: &Path) {
    const THREADS: u64 = 8;
    thread::scope(|scope| {
        for i in 0..THREADS {
            let connect = &connect;
            scope.spawn(move || {
                let client = connect();
                client
                    .with_write(|txn| {
                        let n = txn
                            .get("counter", "n")
                            .and_then(|doc| doc.as_u64())
                            .unwrap_or(0);
                        txn.put("counter", "n", json!(n + 1));
                        txn.put("private", format!("t{i}"), json!(i));
                        Ok(())
                    })
                    .unwrap();
            });
        }
    });

    let client = connect();
    assert_eq!(
        client.get_doc("counter", "n").unwrap().unwrap(),
        json!(THREADS)
    );
    for i in 0..THREADS {
        assert_eq!(
            client
                .get_doc("private", &format!("t{i}"))
                .unwrap()
                .unwrap(),
            json!(i)
        );
    }

    let frames = committed_frames(path);
    assert_eq!(
        frames.len(),
        usize::try_from(THREADS).unwrap(),
        "{frames:?}"
    );
    let mut privates = BTreeMap::new();
    for (i, (seq, ops)) in frames.iter().enumerate() {
        let expect = i as u64 + 1;
        assert_eq!(*seq, expect);
        assert_eq!(ops.len(), 2, "seq {seq} was not one frame: {ops:?}");
        match &ops[..] {
            [
                Op::Put {
                    collection: counter_collection,
                    id: counter_id,
                    doc: counter,
                },
                Op::Put {
                    collection: private_collection,
                    id: private_id,
                    doc: private,
                },
            ] => {
                assert_eq!(counter_collection, "counter");
                assert_eq!(counter_id, "n");
                assert_eq!(counter, &json!(expect));
                assert_eq!(private_collection, "private");
                assert_eq!(private, &json!(private.as_u64().unwrap()));
                assert!(
                    privates
                        .insert(private_id.clone(), private.clone())
                        .is_none(),
                    "duplicate {private_id}"
                );
            }
            other => panic!("expected counter and private puts, got {other:?}"),
        }
    }
    assert_eq!(privates.len(), usize::try_from(THREADS).unwrap());
}

#[test]
fn store_traits_run_over_unix_ws_and_auto() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("traits.fls");
    let sock = dir.path().join("traits.sock");
    let server = FerroStoreServer::bind_unix(&sock, FerroStore::open(&file).unwrap()).unwrap();
    let client = FerroStore::connect_uri(&format!("unix://{}", sock.display())).unwrap();
    exercise_store_traits(&client, "unix");
    drop(client);
    drop(server);

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("traits.fls");
    let server =
        FerroStoreServer::bind_tcp("127.0.0.1:0", FerroStore::open(&file).unwrap()).unwrap();
    let port = server.local_addr().port();
    let client = FerroStore::connect_uri(&format!("ws://127.0.0.1:{port}")).unwrap();
    exercise_store_traits(&client, "ws");
    drop(client);
    drop(server);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("traits.fls");
    let server = FerroStoreServer::bind_unix(
        default_socket_path_for_store(&path),
        FerroStore::open(&path).unwrap(),
    )
    .unwrap();
    let client = FerroStore::auto(&path).unwrap();
    exercise_store_traits(&client, "auto");
    drop(client);
    drop(server);
}

#[test]
fn remote_with_write_commits_both_ops_in_one_frame() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cas.fls");
    let sock = dir.path().join("cas.sock");
    let server = FerroStoreServer::bind_unix(&sock, FerroStore::open(&file).unwrap()).unwrap();
    let uri = format!("unix://{}", sock.display());
    assert_concurrent_increments(|| FerroStore::connect_uri(&uri).unwrap(), &file);
    drop(server);

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cas.fls");
    let server =
        FerroStoreServer::bind_tcp("127.0.0.1:0", FerroStore::open(&file).unwrap()).unwrap();
    let port = server.local_addr().port();
    let uri = format!("ws://127.0.0.1:{port}");
    assert_concurrent_increments(|| FerroStore::connect_uri(&uri).unwrap(), &file);
    drop(server);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cas.fls");
    let server = FerroStoreServer::bind_unix(
        default_socket_path_for_store(&path),
        FerroStore::open(&path).unwrap(),
    )
    .unwrap();
    assert_concurrent_increments(|| FerroStore::auto(&path).unwrap(), &path);
    drop(server);
}

fn int_column() -> Vec<ColumnDef> {
    vec![ColumnDef {
        name: "n".into(),
        ty: ColumnType::Int64,
    }]
}

fn eq_int(value: i64) -> ColumnFilter {
    ColumnFilter {
        column: "n".into(),
        predicate: ColumnPredicate::Eq(ColumnValue::Int64(value)),
    }
}

fn ints(values: &[i64]) -> Vec<Vec<ColumnValue>> {
    values
        .iter()
        .copied()
        .map(|value| vec![ColumnValue::Int64(value)])
        .collect()
}

fn snapshot_version(path: &Path) -> u32 {
    let sb = live(path);
    assert!(sb.snapshot_length > 0);
    let file = File::open(path).unwrap();
    let mut buf = [0u8; 4];
    file.read_exact_at(&mut buf, sb.snapshot_offset + 8)
        .unwrap();
    u32::from_le_bytes(buf)
}

#[test]
fn scan_skips_a_row_group_only_when_the_zone_map_excludes_it() {
    let store = FerroStore::in_memory();
    store.create_column_table("t", &int_column()).unwrap();
    store.append_column_rows("t", &ints(&[1, 2, 3])).unwrap();
    store.append_column_rows("t", &ints(&[10, 11, 12])).unwrap();

    let hit = store.scan_column_table("t", &[eq_int(10)]).unwrap();
    assert_eq!(hit.row_groups_total, 2);
    assert_eq!(hit.row_groups_skipped, 1);
    assert_eq!(hit.rows, ints(&[10]));

    let miss = store.scan_column_table("t", &[eq_int(100)]).unwrap();
    assert_eq!(miss.row_groups_total, 2);
    assert_eq!(miss.row_groups_skipped, 2);
    assert!(miss.rows.is_empty());

    let overlap = FerroStore::in_memory();
    overlap.create_column_table("t", &int_column()).unwrap();
    overlap.append_column_rows("t", &ints(&[1, 100])).unwrap();
    let inside = overlap.scan_column_table("t", &[eq_int(50)]).unwrap();
    assert_eq!(inside.row_groups_total, 1);
    assert_eq!(inside.row_groups_skipped, 0);
    assert!(inside.rows.is_empty());

    let err = store
        .scan_column_table(
            "t",
            &[ColumnFilter {
                column: "missing".into(),
                predicate: ColumnPredicate::Eq(ColumnValue::Int64(1)),
            }],
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Backend(_)), "{err}");
    let err = store
        .scan_column_table(
            "t",
            &[ColumnFilter {
                column: "n".into(),
                predicate: ColumnPredicate::Eq(ColumnValue::TimestampMs(1)),
            }],
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Backend(_)), "{err}");
}

#[test]
fn bad_width_and_a_second_create_write_nothing() {
    let (_dir, path) = temp_fls();
    let store = FerroStore::open(&path).unwrap();
    store.create_column_table("t", &int_column()).unwrap();
    store.append_column_rows("t", &ints(&[7])).unwrap();
    let frames = committed_frames(&path).len();
    assert_eq!(frames, 2);

    let err = store
        .append_column_rows("t", &[vec![ColumnValue::Int64(1), ColumnValue::Int64(2)]])
        .unwrap_err();
    assert!(matches!(err, StoreError::Backend(_)), "{err}");
    let err = store
        .append_column_rows("t", &[vec![ColumnValue::Utf8("no".into())]])
        .unwrap_err();
    assert!(matches!(err, StoreError::Backend(_)), "{err}");
    store.append_column_rows("t", &[]).unwrap();
    let err = store.create_column_table("t", &int_column()).unwrap_err();
    assert!(matches!(err, StoreError::Backend(_)), "{err}");
    let err = store
        .append_column_rows("missing", &ints(&[1]))
        .unwrap_err();
    assert!(matches!(err, StoreError::Backend(_)), "{err}");

    assert_eq!(committed_frames(&path).len(), frames);
    assert_eq!(store.scan_column_table("t", &[]).unwrap().rows, ints(&[7]));
}

#[test]
fn column_rows_survive_reopen_and_a_version_2_checkpoint() {
    let (_dir, path) = temp_fls();
    {
        let store = FerroStore::open(&path).unwrap();
        store.put_doc("docs", "a", &json!(1)).unwrap();
        store.checkpoint().unwrap();
        assert_eq!(snapshot_version(&path), 1);
        store.create_column_table("t", &int_column()).unwrap();
        store.append_column_rows("t", &ints(&[1, 2, 3])).unwrap();
        store.append_column_rows("t", &ints(&[10])).unwrap();
    }
    {
        let store = FerroStore::open(&path).unwrap();
        assert_eq!(store.get_doc("docs", "a").unwrap(), Some(json!(1)));
        let scan = store.scan_column_table("t", &[eq_int(10)]).unwrap();
        assert_eq!(scan.rows, ints(&[10]));
        assert_eq!(scan.row_groups_total, 2);
        assert_eq!(scan.row_groups_skipped, 1);
        store.checkpoint().unwrap();
        assert_eq!(snapshot_version(&path), 2);
        assert_eq!(live(&path).wal_length, 0);
    }
    let store = FerroStore::open(&path).unwrap();
    assert_eq!(store.get_doc("docs", "a").unwrap(), Some(json!(1)));
    assert_eq!(
        store.scan_column_table("t", &[]).unwrap().rows,
        ints(&[1, 2, 3, 10])
    );
}

fn exercise_served_columns(store: &FerroStore) {
    store.create_column_table("t", &int_column()).unwrap();
    store.append_column_rows("t", &ints(&[1, 2])).unwrap();
    store.append_column_rows("t", &ints(&[9, 10])).unwrap();
    let scan = store.scan_column_table("t", &[eq_int(9)]).unwrap();
    assert_eq!(scan.row_groups_total, 2);
    assert_eq!(scan.row_groups_skipped, 1);
    assert_eq!(scan.rows, ints(&[9]));
    let miss = store.scan_column_table("t", &[eq_int(100)]).unwrap();
    assert_eq!(miss.row_groups_skipped, 2);
    assert!(miss.rows.is_empty());
    store.put_doc("docs", "kept", &json!(null)).unwrap();
    assert_eq!(store.get_doc("docs", "kept").unwrap(), Some(json!(null)));
    assert!(store.get_doc("docs", "missing").unwrap().is_none());
}

#[test]
fn served_column_scan_works_over_unix_and_ws_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cols.fls");
    let sock = dir.path().join("cols.sock");
    let server = FerroStoreServer::bind_unix(&sock, FerroStore::open(&file).unwrap()).unwrap();
    let client = FerroStore::connect_uri(&format!("unix://{}", sock.display())).unwrap();
    exercise_served_columns(&client);
    drop(client);
    drop(server);
    let reopened = FerroStore::open(&file).unwrap();
    assert_eq!(
        reopened.scan_column_table("t", &[eq_int(9)]).unwrap().rows,
        ints(&[9])
    );
    assert_eq!(reopened.get_doc("docs", "kept").unwrap(), Some(json!(null)));

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cols.fls");
    let server =
        FerroStoreServer::bind_tcp("127.0.0.1:0", FerroStore::open(&file).unwrap()).unwrap();
    let port = server.local_addr().port();
    let client = FerroStore::connect_uri(&format!("ws://127.0.0.1:{port}")).unwrap();
    exercise_served_columns(&client);
    drop(client);
    drop(server);
    let reopened = FerroStore::open(&file).unwrap();
    let scan = reopened.scan_column_table("t", &[eq_int(9)]).unwrap();
    assert_eq!(scan.row_groups_total, 2);
    assert_eq!(scan.row_groups_skipped, 1);
    assert_eq!(scan.rows, ints(&[9]));
}

fn sample_attempt(
    project: ProjectId,
    cost: u64,
    tokens_in: u64,
    tokens_out: u64,
    duration_ms: u64,
) -> Attempt {
    Attempt {
        id: None,
        at: None,
        project,
        record: RecordId(seq_iri(9)),
        adapter: "claude".into(),
        status: AttemptStatus::Completed,
        duration_ms,
        tokens_in,
        tokens_out,
        cost_usd_micros: cost,
        paths_touched: PathsTouched::Listed(vec![]),
        output_excerpt: None,
    }
}

fn sample_run(gate: GateId, pass: bool, duration_ms: u64, cost_usd_micros: u64) -> GateRun {
    GateRun {
        id: None,
        at: None,
        gate,
        record: None,
        commit: "abc".into(),
        verdict: Verdict::from_predicate(pass, 1),
        population: 1,
        output_excerpt: None,
        duration_ms,
        cost_usd_micros,
    }
}

fn assert_rollups(store: &FerroStore) {
    let project_a = ProjectId(seq_iri(1));
    let project_b = ProjectId(seq_iri(2));
    let gate_a = GateId(seq_iri(3));
    let gate_b = GateId(seq_iri(4));
    store
        .append_attempt(sample_attempt(project_a.clone(), 10, 1, 2, 3))
        .unwrap();
    store
        .append_attempt(sample_attempt(project_b.clone(), 100, 4, 5, 6))
        .unwrap();
    store
        .append_gate_run(sample_run(gate_a.clone(), true, 7, 8))
        .unwrap();
    store
        .append_gate_run(sample_run(gate_b.clone(), false, 9, 10))
        .unwrap();

    assert_eq!(
        store.attempt_rollup(None).unwrap(),
        AttemptRollup {
            count: 2,
            cost_usd_micros: 110,
            tokens_in: 5,
            tokens_out: 7,
            duration_ms: 9,
        }
    );
    assert_eq!(
        store.attempt_rollup(Some(&project_a)).unwrap(),
        AttemptRollup {
            count: 1,
            cost_usd_micros: 10,
            tokens_in: 1,
            tokens_out: 2,
            duration_ms: 3,
        }
    );
    assert_eq!(store.attempt_rollup(Some(&project_b)).unwrap().count, 1);
    assert_eq!(
        store.gate_run_rollup(None).unwrap(),
        GateRunRollup {
            pass: 1,
            fail: 1,
            error: 0,
            duration_ms: 16,
            cost_usd_micros: 18,
        }
    );
    assert_eq!(
        store.gate_run_rollup(Some(&gate_a)).unwrap(),
        GateRunRollup {
            pass: 1,
            fail: 0,
            error: 0,
            duration_ms: 7,
            cost_usd_micros: 8,
        }
    );
    assert_eq!(store.gate_run_rollup(Some(&gate_b)).unwrap().fail, 1);
    assert_eq!(store.gate_run_rollup(Some(&gate_b)).unwrap().pass, 0);
}

#[test]
fn attempt_and_gate_rollups_match_hand_sums_locally_and_over_unix() {
    assert_rollups(&FerroStore::in_memory());

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("roll.fls");
    let sock = dir.path().join("roll.sock");
    let server = FerroStoreServer::bind_unix(&sock, FerroStore::open(&file).unwrap()).unwrap();
    let client = FerroStore::connect_uri(&format!("unix://{}", sock.display())).unwrap();
    assert_rollups(&client);
    drop(client);
    drop(server);
}
