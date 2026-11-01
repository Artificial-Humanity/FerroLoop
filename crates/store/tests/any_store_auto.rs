//! `AnyStore` on the ferro engine goes through a `ferrostore serve` socket
//! when one listens at `<store>.fls.sock`, and opens the file itself when
//! none does (`FerroStore::auto`).
//!
//! Socket paths must fit `sun_path` (108 bytes with the NUL), so every store
//! here sits directly in a short temp directory.

use fl_core::Catalog;
use fl_store::cli::{CompactReport, Wording};
use fl_store::{
    AnyStore, BackendKind, ColumnDef, ColumnFilter, ColumnPredicate, ColumnType, ColumnValue,
    FerroStore, FerroStoreServer, StoreOptions, VectorMetric, VectorStorage, choose_backend,
    default_socket_path_for_store, ferro_stats_at, open_store_with, physical_path,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A temp directory short enough that `<dir>/<name>.fls.sock` fits a Unix
/// socket address.
fn short_dir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new().prefix("fa").tempdir().unwrap();
    let sock = dir.path().join("fl.fls.sock");
    assert!(
        sock.as_os_str().len() < 100,
        "socket path too long for sun_path: {}",
        sock.display()
    );
    dir
}

/// The ferro engine, named, so the process's `FL_STORE_BACKEND` plays no part.
fn ferro() -> StoreOptions {
    StoreOptions {
        backend: Some(BackendKind::Ferro),
        ..StoreOptions::default()
    }
}

fn open(path: &Path) -> AnyStore {
    open_store_with(path, &ferro()).unwrap()
}

/// A server on `<fls>.sock` over the file `fls` itself, as `ferrostore serve` runs.
fn serve_file(fls: &Path) -> FerroStoreServer {
    FerroStoreServer::bind_unix(
        default_socket_path_for_store(fls),
        FerroStore::open(fls).unwrap(),
    )
    .unwrap()
}

fn wording() -> Wording<'static> {
    Wording {
        command: "fl store",
        redb_path_hint: "the bound store, or --db <file>.redb",
    }
}

fn int_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        ty: ColumnType::Int64,
    }
}

/// Every kind of work a caller does through `AnyStore`: the store traits,
/// documents, vectors, a columnar range scan, and a message queue.
fn write_everything(store: &AnyStore) -> fl_core::ProjectId {
    let project = store.add_project("/srv/remote-project").unwrap();
    let ferro = store.as_ferro().unwrap();
    ferro
        .put_doc("docs", "one", &json!({"n": 1, "via": "any"}))
        .unwrap();

    ferro
        .create_vector_collection("v", 3, VectorStorage::F32, VectorMetric::Cosine, 16, 64, 64)
        .unwrap();
    ferro
        .upsert_vector("v", "x", &[1.0, 0.0, 0.0], None)
        .unwrap();
    ferro
        .upsert_vector("v", "y", &[0.0, 1.0, 0.0], Some(&json!({"tag": "y"})))
        .unwrap();

    ferro
        .create_column_table("t", &[int_col("ts"), int_col("v")])
        .unwrap();
    let rows: Vec<Vec<ColumnValue>> = (1..=10)
        .map(|i| vec![ColumnValue::Int64(i), ColumnValue::Int64(i * 10)])
        .collect();
    ferro.append_column_rows("t", &rows).unwrap();

    assert_eq!(
        ferro.publish_mq("topic", "k1", &json!({"m": 1})).unwrap(),
        1
    );
    project
}

/// What [`write_everything`] wrote, read back through `store`.
fn assert_everything(store: &AnyStore, project: &fl_core::ProjectId) {
    let got = store.get_project(project).unwrap().expect("project");
    assert_eq!(got.root, "/srv/remote-project");
    let ferro = store.as_ferro().unwrap();
    assert_eq!(
        ferro.get_doc("docs", "one").unwrap(),
        Some(json!({"n": 1, "via": "any"}))
    );

    let hits = ferro
        .search_vectors("v", &[0.0, 0.9, 0.1], 2, None)
        .unwrap();
    assert_eq!(hits.first().map(|h| h.0.as_str()), Some("y"), "{hits:?}");
    let (values, meta) = ferro.get_vector("v", "y").unwrap().expect("vector y");
    assert_eq!(values, vec![0.0, 1.0, 0.0]);
    assert_eq!(meta, Some(json!({"tag": "y"})));

    let scan = ferro
        .scan_column_table(
            "t",
            &[ColumnFilter {
                column: "ts".to_string(),
                predicate: ColumnPredicate::Between {
                    low: ColumnValue::Int64(3),
                    high: ColumnValue::Int64(5),
                },
            }],
        )
        .unwrap();
    let ts: Vec<ColumnValue> = scan.rows.iter().map(|row| row[0].clone()).collect();
    assert_eq!(
        ts,
        vec![
            ColumnValue::Int64(3),
            ColumnValue::Int64(4),
            ColumnValue::Int64(5)
        ]
    );

    // A consumer group that has not acked sees the message.
    let unread = ferro.poll_mq_group("topic", "fresh-group", 10).unwrap();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].key, "k1");
    assert_eq!(unread[0].payload, json!({"m": 1}));
}

#[test]
fn any_store_works_through_a_listening_server_and_opens_the_file_once_it_stops() {
    let dir = short_dir();
    let fls = dir.path().join("fl.fls");
    let sock = default_socket_path_for_store(&fls);
    let server = serve_file(&fls);
    assert!(sock.exists());

    let store = open(&fls);
    assert!(
        store.is_remote(),
        "a listening socket makes AnyStore remote"
    );
    assert_eq!(store.kind(), BackendKind::Ferro);
    // The label and the file stay the store file, not the socket.
    assert_eq!(store.label(), fls.display().to_string());
    assert_eq!(store.ferro_file(), Some(fls.as_path()));
    // The engine handle under it is the socket's client.
    assert_eq!(
        store.as_ferro().unwrap().label(),
        sock.display().to_string()
    );

    // A second client subscribes before the first publishes. An embedded
    // handle only tells its own subscribers of its own publishes, so this
    // record can only come through the server.
    let listener = open(&fls);
    assert!(listener.is_remote());
    let pushed = listener.as_ferro().unwrap().subscribe_mq("topic");

    let seq_before = FerroStore::stats_at(&fls).unwrap().last_seq;
    let project = write_everything(&store);
    let record = pushed
        .recv_timeout(Duration::from_secs(10))
        .expect("the server pushes the other client's publish");
    assert_eq!(record.seq, 1);
    assert_eq!(record.key, "k1");
    // The server committed the writes to the file it serves.
    assert!(FerroStore::stats_at(&fls).unwrap().last_seq > seq_before);

    assert_everything(&store, &project);
    assert_everything(&listener, &project);

    // Consume: poll, ack, and the group has nothing left.
    let ferro = store.as_ferro().unwrap();
    let polled = ferro.poll_mq_group("topic", "g", 10).unwrap();
    assert_eq!(polled.len(), 1);
    ferro.ack_mq("topic", "g", polled[0].seq).unwrap();
    assert!(ferro.poll_mq_group("topic", "g", 10).unwrap().is_empty());

    // A remote engine handle serves no stats; AnyStore reads the file beside
    // the server instead, as `fl store status` does.
    assert!(store.as_ferro().unwrap().stats().is_err());
    let stats = store.stats().unwrap();
    assert_eq!(stats, ferro_stats_at(&fls).unwrap());
    assert_eq!(stats.engine, BackendKind::Ferro);
    assert!(stats.live_bytes.is_some());
    ferro.checkpoint().unwrap();
    store.compact().unwrap();
    assert_eq!(store.stats().unwrap().dead_bytes, Some(0));

    // `fl store compact` itself, run while the server holds the store.
    match fl_store::cli::compact(&fls, BackendKind::Ferro, &wording()).unwrap() {
        CompactReport::Compacted {
            physical_path,
            after,
            ..
        } => {
            assert_eq!(physical_path, fls);
            assert!(after.is_some(), "the size after compaction is read");
        }
        other => panic!("not compacted: {}", other.text()),
    }

    drop(listener);
    drop(store);
    drop(server);
    assert!(!sock.exists(), "a stopped server removes its socket");

    let store = open(&fls);
    assert!(!store.is_remote(), "no socket: the file is opened embedded");
    assert_eq!(store.label(), fls.display().to_string());
    assert!(store.as_ferro().unwrap().stats().is_ok());
    assert_everything(&store, &project);
    // The ack made through the server is in the file too.
    assert!(
        store
            .as_ferro()
            .unwrap()
            .poll_mq_group("topic", "g", 10)
            .unwrap()
            .is_empty()
    );
}

/// A server over an in-memory store, bound at the socket of a `.fls` that
/// does not exist: any embedded writer would have to create the file.
#[test]
fn every_write_goes_to_the_server_and_none_to_a_second_embedded_writer() {
    let dir = short_dir();
    let fls = dir.path().join("fl.fls");
    let server =
        FerroStoreServer::bind_unix(default_socket_path_for_store(&fls), FerroStore::in_memory())
            .unwrap();

    let store = open(&fls);
    assert!(store.is_remote());
    let project = write_everything(&store);
    assert_everything(&store, &project);
    assert!(
        !fls.exists(),
        "an embedded open would have created {}",
        fls.display()
    );

    // Another AnyStore reads the same in-memory state, so it is served.
    let other = open(&fls);
    assert!(other.is_remote());
    assert_everything(&other, &project);

    // With no file of its own beside the server, stats are an error, not zeros.
    assert!(store.stats().is_err());
    drop(other);
    drop(store);
    drop(server);
    assert!(!fls.exists());
}

/// `~/.config/fl/config.toml` names `<stem>.redb`. On the ferro engine that
/// logical path resolves to `<stem>.fls`, and its socket is `<stem>.fls.sock`.
#[test]
fn a_redb_config_path_on_the_ferro_engine_connects_to_the_fls_socket() {
    let dir = short_dir();
    let configured = dir.path().join("fl.redb");
    let fls = dir.path().join("fl.fls");
    assert_eq!(
        choose_backend(&configured, None).unwrap(),
        BackendKind::Ferro
    );
    assert_eq!(physical_path(&configured, BackendKind::Ferro), fls);
    let sock = default_socket_path_for_store(physical_path(&configured, BackendKind::Ferro));
    assert_eq!(sock, dir.path().join("fl.fls.sock"));

    let server = FerroStoreServer::bind_unix(&sock, FerroStore::in_memory()).unwrap();
    let store = open(&configured);
    assert!(store.is_remote());
    assert_eq!(store.label(), fls.display().to_string());
    assert_eq!(store.ferro_file(), Some(fls.as_path()));
    assert_eq!(
        store.as_ferro().unwrap().label(),
        sock.display().to_string()
    );
    store
        .as_ferro()
        .unwrap()
        .put_doc("docs", "cfg", &json!(1))
        .unwrap();
    assert_eq!(
        open(&configured)
            .as_ferro()
            .unwrap()
            .get_doc("docs", "cfg")
            .unwrap(),
        Some(json!(1))
    );
    assert!(!fls.exists() && !configured.exists());
    drop(store);
    drop(server);

    // With the server gone, the same configured path opens `<stem>.fls`.
    let store = open(&configured);
    assert!(!store.is_remote());
    assert_eq!(store.ferro_file(), Some(fls.as_path()));
    store
        .as_ferro()
        .unwrap()
        .put_doc("docs", "local", &json!(2))
        .unwrap();
    drop(store);
    assert!(fls.exists() && !configured.exists());
}

/// A socket file nobody listens on (a server that crashed) is a refused
/// connect: the store opens embedded, and the stale file is left alone.
#[test]
fn a_stale_socket_with_no_listener_falls_back_to_the_file() {
    let dir = short_dir();
    let fls = dir.path().join("fl.fls");
    let sock: PathBuf = default_socket_path_for_store(&fls);
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    assert!(sock.exists(), "the socket file outlives its listener");
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&sock)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::ConnectionRefused
    );

    let store = open(&fls);
    assert!(!store.is_remote());
    let project = write_everything(&store);
    drop(store);
    assert!(fls.exists());

    let store = open(&fls);
    assert!(!store.is_remote());
    assert_everything(&store, &project);
    assert!(sock.exists());
}

/// What a long-lived remote handle (the wire daemon's, an MCP server's) does
/// when its server stops: it fails every call, and does not redial when a
/// server comes back on the same socket. A new open finds the new server.
#[test]
fn a_remote_handle_fails_after_its_server_stops_and_does_not_redial() {
    let dir = short_dir();
    let fls = dir.path().join("fl.fls");
    let server = serve_file(&fls);
    let store = open(&fls);
    assert!(store.is_remote());
    let ferro = store.as_ferro().unwrap();
    ferro.put_doc("docs", "before", &json!(1)).unwrap();

    drop(server);
    assert!(ferro.put_doc("docs", "after", &json!(2)).is_err());
    assert!(ferro.get_doc("docs", "before").is_err());

    let server = serve_file(&fls);
    assert!(
        ferro.get_doc("docs", "before").is_err(),
        "the old handle does not redial the new server"
    );
    let fresh = open(&fls);
    assert!(fresh.is_remote());
    assert_eq!(
        fresh.as_ferro().unwrap().get_doc("docs", "before").unwrap(),
        Some(json!(1))
    );
    assert_eq!(
        fresh.as_ferro().unwrap().get_doc("docs", "after").unwrap(),
        None
    );
    drop(fresh);
    drop(store);
    drop(server);
}
