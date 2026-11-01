//! Spec §5: compaction by sibling inode.
//!
//! The simulated disk (`crash_sim`) drives the interleavings deterministically:
//! a writer blocked on the old inode's `flock` during the rename, and a
//! reader that is part way through a tail read of the old inode when the
//! file is swapped. A child process (this test binary, re-run with a filter)
//! keeps writing while the parent compacts, on a real file.

use super::compact::{AUTO_COMPACT_BYTES, auto_compact_warnings};
use super::crash_sim::{IoOp, SIM_DEV, SimFs};
use super::layout::{HEADER_BYTES, SLOT_LEN};
use super::state::DocState;
use super::{FerroStore, FerroStoreServer, Inner, lock_mutex};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const STORE: &str = "/sim/store.fls";

fn open(fs: &SimFs) -> FerroStore {
    FerroStore::open_with(fs, Path::new(STORE)).unwrap()
}

/// The inode `STORE` names now.
fn store_ino(fs: &SimFs) -> usize {
    fs.live_image().names[Path::new(STORE)]
}

/// Every name in the simulated directory that a compaction's temp file has.
fn temp_names(fs: &SimFs) -> Vec<PathBuf> {
    fs.live_image()
        .names
        .keys()
        .filter(|name| name.to_string_lossy().contains(".compact-"))
        .cloned()
        .collect()
}

/// `(dev, ino)` of the state this handle's cache holds.
fn cached_identity(store: &FerroStore) -> (u64, u64) {
    let Inner::Disk(disk) = &store.inner else {
        panic!("a disk store");
    };
    lock_mutex(&disk.cache)
        .as_ref()
        .expect("a cached state")
        .file_id
}

fn is_slot_read(op: &IoOp) -> bool {
    matches!(op, IoOp::Read { offset, len, .. }
        if *len == SLOT_LEN as u64 && (*offset == 0 || *offset == SLOT_LEN as u64))
}

/// What a reader can observe of a state: its sequence, documents, tables
/// and vector collections.
type View = (
    u64,
    Vec<(String, String, Value)>,
    Vec<(String, Vec<Vec<u8>>)>,
    Vec<String>,
);

fn view(state: &DocState) -> View {
    (
        state.last_seq,
        state
            .docs
            .iter()
            .map(|(collection, id, doc)| (collection.clone(), id.clone(), doc.clone()))
            .collect(),
        state
            .tables
            .iter()
            .map(|(name, table)| (name.clone(), table.groups.clone()))
            .collect(),
        state.vectors.keys().cloned().collect(),
    )
}

fn text(bytes: usize) -> Value {
    json!("x".repeat(bytes))
}

/// A store with several checkpoints behind it: every checkpoint leaves the
/// snapshot and the WAL before it dead.
fn with_dead_extents(store: &FerroStore, checkpoints: usize) {
    for round in 0..checkpoints {
        for i in 0..20 {
            store
                .put_doc(
                    "docs",
                    &format!("r{round}-{i:02}"),
                    &json!({"round": round, "i": i}),
                )
                .unwrap();
        }
        store.checkpoint().unwrap();
    }
    store.put_doc("docs", "in-the-wal", &json!("tail")).unwrap();
}

// ---- what a compaction leaves --------------------------------------------------

#[test]
fn compaction_leaves_one_snapshot_a_newer_epoch_and_the_same_state() {
    let fs = SimFs::default();
    let store = open(&fs);
    store.set_auto_compact_floor(u64::MAX);
    with_dead_extents(&store, 5);
    let before = store.stats().unwrap();
    let state = store.read_state().unwrap();
    assert!(before.dead_bytes > 0, "{before:?}");
    assert_eq!(
        before.file_bytes,
        before.live_bytes + before.dead_bytes,
        "dead bytes are the file less its live extents: {before:?}"
    );
    let old = store_ino(&fs);

    store.compact().unwrap();

    let after = store.stats().unwrap();
    assert_ne!(store_ino(&fs), old, "the path names a new inode");
    assert_eq!(after.dead_bytes, 0, "{after:?}");
    assert_eq!(after.file_bytes, after.live_bytes);
    assert_eq!(
        after.epoch,
        before.epoch + 1,
        "the new file's epoch is the old file's current epoch + 1"
    );
    assert_eq!(after.last_seq, before.last_seq, "last_seq is kept");
    assert_eq!(view(&store.read_state().unwrap()), view(&state));
    assert!(temp_names(&fs).is_empty(), "{:?}", temp_names(&fs));
    // The new file: a v2 header, one snapshot and an empty WAL.
    let image = fs.live_image();
    let bytes = image.file(Path::new(STORE)).unwrap();
    assert_eq!(&bytes[HEADER_BYTES as usize..][..8], b"FERRSNAP");
    assert_eq!(bytes.len() as u64, after.file_bytes);
    // It was created 0600, with `O_EXCL`.
    assert!(
        fs.trace()
            .iter()
            .any(|op| matches!(op, IoOp::CreateNew { path, mode: 0o600 }
            if path.to_string_lossy().contains("store.fls.compact-"))),
        "no 0600 create_new of a `.compact-` sibling"
    );
    // A reopen reads the same state.
    let again = open(&fs);
    assert_eq!(view(&again.read_state().unwrap()), view(&state));
    assert_eq!(again.stats().unwrap().epoch, after.epoch);
}

#[test]
fn a_compaction_installs_its_result_so_the_next_read_reads_only_the_new_slots() {
    let fs = SimFs::default();
    let store = open(&fs);
    with_dead_extents(&store, 2);
    store.compact().unwrap();
    let new = store_ino(&fs);
    assert_eq!(cached_identity(&store), (SIM_DEV, new as u64));
    let mark = fs.trace().len();
    assert_eq!(
        store.get_doc("docs", "in-the-wal").unwrap(),
        Some(json!("tail"))
    );
    for op in &fs.trace()[mark..] {
        match op {
            IoOp::Read { ino, .. } => {
                assert!(is_slot_read(op), "an extent read after the install: {op}");
                assert_eq!(*ino, new, "a read of the old inode: {op}");
            }
            IoOp::Len { .. } | IoOp::StatId { .. } => {}
            other => panic!("a read after the compaction did {other}"),
        }
    }
}

#[test]
fn the_old_inode_lock_is_released_only_after_the_rename_and_the_directory_sync() {
    let fs = SimFs::default();
    let store = open(&fs);
    with_dead_extents(&store, 2);
    let old = store_ino(&fs);
    let mark = fs.trace().len();
    let locks_before = fs.lock_events().len();
    store.compact().unwrap();
    let trace = fs.trace();
    let rename = (mark..trace.len())
        .find(|at| matches!(&trace[*at], IoOp::Rename { to, .. } if to == Path::new(STORE)))
        .expect("the compaction renamed onto the store path");
    let dir_sync = (rename..trace.len())
        .find(|at| matches!(trace[*at], IoOp::SyncDir { .. }))
        .expect("the directory is synced after the rename");
    let events = fs.lock_events();
    let unlock_old = events[locks_before..]
        .iter()
        .find(|(_, event)| *event == IoOp::Unlock { ino: old })
        .map(|(at, _)| *at)
        .expect("the old inode was unlocked");
    let lock_old = events[locks_before..]
        .iter()
        .find(|(_, event)| *event == IoOp::Lock { ino: old })
        .map(|(at, _)| *at)
        .expect("the old inode was locked");
    assert!(
        lock_old <= mark + 1,
        "the compactor locks the old inode first"
    );
    assert!(
        unlock_old > dir_sync,
        "the old inode's lock was released at op #{unlock_old}, before the rename (#{rename}) \
         and the directory sync (#{dir_sync}) were done"
    );
    // The new inode's own lock is held from before its first write until
    // after the directory sync, so no writer commits into a file whose name
    // is not yet durable.
    let new = store_ino(&fs);
    let first_new_write = (mark..trace.len())
        .find(|at| matches!(trace[*at], IoOp::Write { ino, .. } if ino == new))
        .unwrap();
    let lock_new = events[locks_before..]
        .iter()
        .find(|(_, event)| *event == IoOp::Lock { ino: new })
        .map(|(at, _)| *at)
        .unwrap();
    let unlock_new = events[locks_before..]
        .iter()
        .find(|(_, event)| *event == IoOp::Unlock { ino: new })
        .map(|(at, _)| *at)
        .unwrap();
    assert!(lock_new <= first_new_write && unlock_new > dir_sync);
}

#[test]
fn a_memory_store_compacts_as_a_no_op_and_a_remote_one_asks_its_server() {
    let memory = FerroStore::in_memory();
    memory.put_doc("c", "a", &json!(1)).unwrap();
    memory.compact().unwrap();
    assert_eq!(memory.get_doc("c", "a").unwrap(), Some(json!(1)));
    let stats = memory.stats().unwrap();
    assert_eq!((stats.file_bytes, stats.dead_bytes), (0, 0));

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("store.fls");
    let local = FerroStore::open(&path).unwrap();
    with_dead_extents(&local, 3);
    let before = std::fs::metadata(&path).unwrap().len();
    let sock = tmp.path().join("store.fls.sock");
    let server = FerroStoreServer::bind_unix(&sock, local).unwrap();
    let client = FerroStore::connect_uri(&format!("unix://{}", sock.display())).unwrap();
    client.compact().unwrap();
    let after = std::fs::metadata(&path).unwrap().len();
    assert!(
        after < before,
        "the server compacted: {before} -> {after} bytes"
    );
    assert_eq!(
        client.get_doc("docs", "in-the-wal").unwrap(),
        Some(json!("tail"))
    );
    assert_eq!(server.store().stats().unwrap().dead_bytes, 0);
}

#[test]
fn compaction_shrinks_a_real_store_with_several_checkpoints_to_one_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("store.fls");
    let store = FerroStore::open(&path).unwrap();
    store.set_auto_compact_floor(u64::MAX);
    for round in 0..6 {
        for i in 0..200 {
            store
                .put_doc(
                    "docs",
                    &format!("d{i:03}"),
                    &json!({"round": round, "body": "y".repeat(200)}),
                )
                .unwrap();
        }
        store.checkpoint().unwrap();
    }
    let before = store.stats().unwrap();
    let on_disk_before = std::fs::metadata(&path).unwrap().len();
    assert_eq!(before.file_bytes, on_disk_before);
    store.compact().unwrap();
    let after = store.stats().unwrap();
    let on_disk_after = std::fs::metadata(&path).unwrap().len();
    println!(
        "§5 compaction, 6 checkpoints of 200 docs: {on_disk_before} B ({} live, {} dead) -> \
         {on_disk_after} B (header {HEADER_BYTES} + snapshot {})",
        before.live_bytes,
        before.dead_bytes,
        after.live_bytes - HEADER_BYTES
    );
    assert_eq!(on_disk_after, after.file_bytes);
    assert_eq!(after.file_bytes, after.live_bytes);
    assert!(on_disk_after * 4 < on_disk_before);
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777
    };
    assert_eq!(mode, 0o600, "the compacted file is 0600");
    drop(store);
    let again = FerroStore::open(&path).unwrap();
    assert_eq!(again.get_doc("docs", "d199").unwrap().unwrap()["round"], 5);
}

// ---- swap detection ------------------------------------------------------------

/// Wait (at most 30 s) until `n` descriptions wait for the lock of `ino`.
fn wait_for_lock_waiters(fs: &SimFs, ino: usize, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while fs.lock_waiters(ino) < n {
        assert!(Instant::now() < deadline, "no writer waited on ino{ino}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_writer_blocked_on_the_old_inode_during_the_rename_lands_its_write_in_the_new_file() {
    let fs = SimFs::default();
    let compactor = open(&fs);
    with_dead_extents(&compactor, 2);
    // Another handle: another process, as far as the store can tell.
    let writer = Arc::new(open(&fs));
    assert!(writer.get_doc("docs", "in-the-wal").unwrap().is_some());
    let old = store_ino(&fs);
    let blocked = Arc::new(Mutex::new(None));
    {
        let (fs2, writer, blocked) = (fs.clone(), Arc::clone(&writer), Arc::clone(&blocked));
        // Just before the sibling's sync: the compactor holds the old
        // inode's lock, and the rename is still to come.
        fs.before_next_sync(
            move |op| matches!(op, IoOp::SyncAll { ino } if *ino != old),
            move || {
                let handle = std::thread::spawn(move || {
                    writer.put_doc("docs", "blocked", &json!("lands in the new file"))
                });
                wait_for_lock_waiters(&fs2, old, 1);
                *lock_mutex(&blocked) = Some(handle);
            },
        );
    }
    compactor.compact().unwrap();
    let handle = lock_mutex(&blocked).take().expect("the writer was started");
    handle.join().unwrap().unwrap();
    let new = store_ino(&fs);
    assert_ne!(new, old);
    // The acknowledged write is in the file the path names.
    let fresh = open(&fs);
    assert_eq!(
        fresh.get_doc("docs", "blocked").unwrap(),
        Some(json!("lands in the new file")),
        "the blocked writer's acknowledged commit is lost"
    );
    assert_eq!(
        compactor.get_doc("docs", "blocked").unwrap(),
        Some(json!("lands in the new file"))
    );
    // Nothing was written to the old inode after the rename.
    let trace = fs.trace();
    let rename = trace
        .iter()
        .position(|op| matches!(op, IoOp::Rename { .. }))
        .unwrap();
    assert!(
        !trace[rename..]
            .iter()
            .any(|op| matches!(op, IoOp::Write { ino, .. } if *ino == old)),
        "a write went to the unlinked inode"
    );
}

#[test]
fn a_reader_mid_tail_read_on_the_old_inode_finishes_then_moves_to_the_new_inode() {
    let fs = SimFs::default();
    let writer = Arc::new(open(&fs));
    with_dead_extents(&writer, 2);
    let reader = open(&fs);
    assert!(reader.get_doc("docs", "in-the-wal").unwrap().is_some());
    let first = reader.stats().unwrap();
    writer.put_doc("docs", "tail", &json!(1)).unwrap();
    let old = store_ino(&fs);
    {
        let writer = Arc::clone(&writer);
        // Once the reader has read the new frame from the old inode, and
        // before it decodes it: compact, then commit on the new file.
        fs.after_next_read(
            move |op| matches!(op, IoOp::Read { ino, .. } if *ino == old) && !is_slot_read(op),
            move || {
                writer.compact().unwrap();
                writer.put_doc("docs", "after", &json!(2)).unwrap();
            },
        );
    }
    let state = reader.read_state().unwrap();
    // The read finished on the old inode, consistently: the tail is there.
    assert_eq!(state.docs.get("docs", "tail"), Some(&json!(1)));
    assert_eq!(state.docs.get("docs", "after"), None);
    let mid = state.last_seq;
    assert!(store_ino(&fs) != old, "the compaction ran during the read");
    // The next refresh moves to the new inode and sees the later commit.
    let mark = fs.trace().len();
    let next = reader.read_state().unwrap();
    assert_eq!(next.docs.get("docs", "after"), Some(&json!(2)));
    assert!(next.last_seq > mid);
    let new = store_ino(&fs);
    assert!(
        fs.trace()[mark..]
            .iter()
            .all(|op| !matches!(op, IoOp::Read { ino, .. } if *ino == old)),
        "the refresh after the swap read the old inode"
    );
    assert_eq!(cached_identity(&reader), (SIM_DEV, new as u64));
    let last = reader.stats().unwrap();
    assert!(last.epoch > first.epoch && last.last_seq >= first.last_seq);
    // And never goes back.
    for _ in 0..5 {
        let again = reader.read_state().unwrap();
        assert!(again.last_seq >= next.last_seq);
        assert_eq!(again.docs.get("docs", "after"), Some(&json!(2)));
    }
}

#[test]
fn a_slow_read_of_the_old_inode_never_replaces_the_compacted_cache_of_its_own_handle() {
    let fs = SimFs::default();
    let store = Arc::new(open(&fs));
    with_dead_extents(&store, 2);
    // A frame this handle has not read yet.
    let other = open(&fs);
    other.put_doc("docs", "tail", &json!(1)).unwrap();
    let old = store_ino(&fs);
    {
        let store = Arc::clone(&store);
        fs.after_next_read(
            move |op| matches!(op, IoOp::Read { ino, .. } if *ino == old) && !is_slot_read(op),
            move || store.compact().unwrap(),
        );
    }
    // The read's tail read of the old inode triggers the compaction on this
    // same handle, which installs the new inode's state first.
    assert_eq!(store.get_doc("docs", "tail").unwrap(), Some(json!(1)));
    let new = store_ino(&fs);
    assert_ne!(new, old);
    assert_eq!(
        cached_identity(&store),
        (SIM_DEV, new as u64),
        "the slow read of the old inode replaced the compacted state in the cache"
    );
    // The next read is a cache hit on the new inode.
    let mark = fs.trace().len();
    assert_eq!(store.get_doc("docs", "tail").unwrap(), Some(json!(1)));
    assert!(
        fs.trace()[mark..]
            .iter()
            .all(|op| !matches!(op, IoOp::Read { .. }) || is_slot_read(op)),
        "the read after the compaction reloaded"
    );
}

#[test]
fn every_generation_of_the_path_has_a_strictly_newer_epoch() {
    let fs = SimFs::default();
    let store = open(&fs);
    let reader = open(&fs);
    let mut seen = vec![reader.stats().unwrap()];
    let mut identities = vec![store_ino(&fs)];
    for round in 0..4 {
        store
            .put_doc("c", &format!("k{round}"), &json!(round))
            .unwrap();
        store.checkpoint().unwrap();
        seen.push(reader.stats().unwrap());
        store.compact().unwrap();
        identities.push(store_ino(&fs));
        seen.push(reader.stats().unwrap());
        store
            .put_doc("c", &format!("after{round}"), &json!(round))
            .unwrap();
        seen.push(reader.stats().unwrap());
    }
    for pair in seen.windows(2) {
        assert!(
            pair[1].epoch > pair[0].epoch,
            "the epoch did not move forward across a publish: {:?} -> {:?}",
            pair[0],
            pair[1]
        );
        assert!(pair[1].last_seq >= pair[0].last_seq);
    }
    identities.dedup();
    assert_eq!(identities.len(), 5, "each compaction is a new inode");
}

// ---- auto-compaction -----------------------------------------------------------

fn renames_since(fs: &SimFs, mark: usize) -> usize {
    fs.trace()[mark..]
        .iter()
        .filter(|op| matches!(op, IoOp::Rename { .. }))
        .count()
}

#[test]
fn auto_compaction_runs_exactly_once_per_crossing_of_the_threshold() {
    const FLOOR: u64 = 24 << 10;
    let fs = SimFs::default();
    let store = open(&fs);
    store.set_auto_checkpoint_floor(u64::MAX);
    store.set_auto_compact_floor(FLOOR);
    let (mut crossings, mut quiet, mut quiet_after_a_crossing) = (0, 0, 0);
    let mut last_crossed = false;
    for i in 0..40 {
        let mark = fs.trace().len();
        store
            .put_doc("bulk", &format!("{i:02}"), &text(400 + 29 * i))
            .unwrap();
        assert_eq!(renames_since(&fs, mark), 0, "a commit compacted");
        let before = store.stats().unwrap();
        let mark = fs.trace().len();
        store.checkpoint().unwrap();
        let after = store.stats().unwrap();
        // The checkpoint appends its snapshot at the end of the file: what was
        // in the file past the header is dead once it is published.
        let snapshot = after.live_bytes - HEADER_BYTES;
        let dead = before.file_bytes - HEADER_BYTES;
        let due = dead > (HEADER_BYTES + snapshot).max(FLOOR);
        assert_eq!(
            renames_since(&fs, mark),
            usize::from(due),
            "checkpoint {i}: dead {dead} B, live {} B, floor {FLOOR} B",
            after.live_bytes
        );
        if due {
            crossings += 1;
            assert_eq!(after.dead_bytes, 0);
        } else {
            quiet += 1;
            quiet_after_a_crossing += usize::from(last_crossed);
            assert_eq!(after.dead_bytes, dead);
        }
        last_crossed = due;
    }
    assert!(crossings >= 3, "{crossings} crossings");
    assert!(quiet >= crossings && quiet_after_a_crossing >= 1);
    assert_eq!(AUTO_COMPACT_BYTES, 64 << 20);
}

#[test]
fn a_commit_past_both_thresholds_checkpoints_then_compacts_each_in_its_own_lock_window() {
    let fs = SimFs::default();
    let store = open(&fs);
    store.set_auto_compact_floor(0);
    store.set_auto_checkpoint_floor(0);
    // Each commit past the snapshot's length checkpoints; the dead bytes
    // pass the live ones after a few.
    let mut compacted = None;
    for i in 0..10 {
        let old = store_ino(&fs);
        let snapshot = store.stats().unwrap().live_bytes;
        let mark = fs.trace().len();
        let locks = fs.lock_events().len();
        store
            .put_doc("bulk", &format!("{i}"), &text(snapshot as usize + 600))
            .unwrap();
        if renames_since(&fs, mark) == 1 {
            let windows = fs.lock_events()[locks..]
                .iter()
                .filter(|(_, event)| *event == IoOp::Lock { ino: old })
                .count();
            compacted = Some(windows);
            break;
        }
    }
    assert_eq!(
        compacted,
        Some(3),
        "the commit, its checkpoint and the compaction each take the old inode's lock"
    );
    assert!(store.get_doc("bulk", "0").unwrap().is_some());
}

#[test]
fn a_failing_auto_compaction_keeps_the_checkpoint_backs_off_and_warns_once() {
    const FLOOR: u64 = 8 << 10;
    let fs = SimFs::default();
    let store = open(&fs);
    store.set_auto_checkpoint_floor(u64::MAX);
    store.set_auto_compact_floor(FLOOR);
    let warnings = auto_compact_warnings();
    // A failed operation is not in the trace: an attempt is counted by its
    // lock window, which only a due checkpoint's compaction takes.
    let attempts = |mark: usize, ino: usize| {
        fs.lock_events()[mark..]
            .iter()
            .filter(|(_, event)| *event == IoOp::Lock { ino })
            .count()
            - 1
    };
    // `(checkpoint, dead bytes)` of each failed attempt. The first two fail.
    let mut failures: Vec<(usize, u64)> = Vec::new();
    for i in 0..80 {
        if failures.len() < 2 {
            fs.fail_next(|op| matches!(op, IoOp::CreateNew { .. }));
        }
        store
            .put_doc("bulk", &format!("{i:02}"), &text(500))
            .unwrap();
        let (mark, ino) = (fs.lock_events().len(), store_ino(&fs));
        // A failed compaction does not fail the checkpoint before it.
        store.checkpoint().unwrap();
        let stats = store.stats().unwrap();
        let threshold = stats.live_bytes.max(FLOOR);
        assert!(store.get_doc("bulk", &format!("{i:02}")).unwrap().is_some());
        let tried = attempts(mark, ino);
        if tried == 0 {
            if let Some(&(_, at)) = failures.last() {
                assert!(
                    stats.dead_bytes <= threshold || stats.dead_bytes - at <= threshold,
                    "checkpoint {i} did not retry past the back-off: {stats:?}, failed at {at}"
                );
            }
            continue;
        }
        assert_eq!(tried, 1, "one attempt per due checkpoint");
        if failures.len() < 2 {
            if let Some(&(previous, at)) = failures.last() {
                assert!(i > previous + 1, "retried on the very next checkpoint");
                assert!(stats.dead_bytes - at > threshold);
            }
            assert!(
                stats.dead_bytes > threshold,
                "the file is as it was: {stats:?}"
            );
            failures.push((i, stats.dead_bytes));
            // One warning for the run, however many attempts fail.
            assert_eq!(auto_compact_warnings(), warnings + 1);
            assert!(temp_names(&fs).is_empty());
        } else {
            assert_eq!(stats.dead_bytes, 0, "the retry compacted: {stats:?}");
            return;
        }
    }
    panic!("no compaction after the failures {failures:?}");
}

// ---- failures ------------------------------------------------------------------

#[test]
fn a_failed_sync_during_compaction_leaves_the_old_file_as_the_store() {
    let fs = SimFs::default();
    let store = open(&fs);
    with_dead_extents(&store, 2);
    let state = store.read_state().unwrap();
    let old = store_ino(&fs);
    fs.fail_next(move |op| matches!(op, IoOp::SyncAll { ino } if *ino != old));
    let err = store.compact().unwrap_err();
    assert!(err.to_string().contains("simulated I/O error"), "{err}");
    assert_eq!(store_ino(&fs), old, "the old file is still the store");
    assert!(
        temp_names(&fs).is_empty(),
        "the temp is removed: {:?}",
        temp_names(&fs)
    );
    // A failed fsync poisons the handle, as any failed fsync does.
    let refused = store.put_doc("c", "x", &json!(1)).unwrap_err();
    assert!(refused.to_string().contains("poisoned"), "{refused}");
    assert_eq!(
        view(&store.read_state().unwrap()),
        view(&state),
        "reads go on from the cache"
    );
    // The old file is untouched and is the store a reopen reads.
    let again = open(&fs);
    assert_eq!(view(&again.read_state().unwrap()), view(&state));
    again.compact().unwrap();
    assert_ne!(store_ino(&fs), old);
}

#[test]
fn a_failed_write_of_the_sibling_removes_it_and_leaves_the_handle_usable() {
    let fs = SimFs::default();
    let store = open(&fs);
    with_dead_extents(&store, 2);
    let old = store_ino(&fs);
    fs.fail_next(move |op| matches!(op, IoOp::Write { ino, .. } if *ino != old));
    assert!(store.compact().is_err());
    assert_eq!(store_ino(&fs), old);
    assert!(temp_names(&fs).is_empty());
    // Not an fsync: the handle is not poisoned.
    store.put_doc("c", "after", &json!(1)).unwrap();
    store.compact().unwrap();
    assert_eq!(store.get_doc("c", "after").unwrap(), Some(json!(1)));
}

#[test]
fn a_failed_directory_sync_after_the_rename_poisons_the_handle() {
    let fs = SimFs::default();
    let store = open(&fs);
    with_dead_extents(&store, 2);
    let state = store.read_state().unwrap();
    let old = store_ino(&fs);
    let renamed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let renamed = Arc::clone(&renamed);
        fs.fail_next(move |op| {
            if matches!(op, IoOp::Rename { .. }) {
                renamed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            renamed.load(std::sync::atomic::Ordering::SeqCst) && matches!(op, IoOp::SyncDir { .. })
        });
    }
    assert!(store.compact().is_err());
    assert_ne!(store_ino(&fs), old, "the rename stands");
    let refused = store.put_doc("c", "x", &json!(1)).unwrap_err();
    assert!(refused.to_string().contains("poisoned"), "{refused}");
    assert_eq!(view(&open(&fs).read_state().unwrap()), view(&state));
}

#[test]
fn a_store_whose_path_is_removed_still_reads_and_refuses_a_write() {
    let fs = SimFs::default();
    let store = open(&fs);
    store.put_doc("c", "a", &json!(1)).unwrap();
    super::container_io::ContainerFs::unlink(&fs, Path::new(STORE)).unwrap();
    // No compaction removes the name: the read goes on through its file.
    assert_eq!(store.get_doc("c", "a").unwrap(), Some(json!(1)));
    // A commit into the unlinked file would be lost: refused.
    let err = store.put_doc("c", "b", &json!(2)).unwrap_err();
    assert!(err.to_string().contains("stat"), "{err}");
}

// ---- leftover temps ------------------------------------------------------------

fn age_by(path: &Path, age: Duration) {
    let when = std::time::SystemTime::now() - age;
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .unwrap();
}

#[test]
fn a_compaction_sweeps_only_this_stores_stale_compaction_temps() {
    let old = Duration::from_secs(25 * 60 * 60);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let path = dir.join("fl.fls");
    let store = FerroStore::open(&path).unwrap();
    with_dead_extents(&store, 2);
    let write_aged = |name: &str, age: Option<Duration>| {
        let at = dir.join(name);
        std::fs::write(&at, b"left by a crashed compaction").unwrap();
        if let Some(age) = age {
            age_by(&at, age);
        }
        at
    };
    let stale = write_aged("fl.fls.compact-4242-0-0badf00d", Some(old));
    let fresh = write_aged("fl.fls.compact-4242-1-0badf00d", None);
    let unlike = [
        "fl.fls.compact-4242-0-0badf00d.bak",
        "fl.fls.compact-taken",
        "fl.fls.compact-4242-0-badf00d",
        "other.fls.compact-4242-0-0badf00d",
        // A migration's temp is the migration's to sweep.
        "fl.fls.migrating-4242-0-0badf00d",
    ]
    .map(|name| write_aged(name, Some(old)));
    store.compact().unwrap();
    assert!(!stale.exists(), "the stale compaction temp is removed");
    assert!(fresh.exists(), "a temp written within 24 h is kept");
    for at in &unlike {
        assert!(at.exists(), "{} is kept", at.display());
    }
    // None of them is ever read as the store.
    drop(store);
    let again = FerroStore::open(&path).unwrap();
    assert_eq!(
        again.get_doc("docs", "in-the-wal").unwrap(),
        Some(json!("tail"))
    );
    let mut leftovers: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("fl.fls.compact-"))
        .collect();
    leftovers.sort();
    assert_eq!(
        leftovers,
        [
            "fl.fls.compact-4242-0-0badf00d.bak",
            "fl.fls.compact-4242-0-badf00d",
            "fl.fls.compact-4242-1-0badf00d",
            "fl.fls.compact-taken",
        ],
        "only the stale temp went, and the compaction left none of its own"
    );
}

#[test]
fn a_store_opened_through_a_symlink_is_not_compacted() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real.fls");
    let link = tmp.path().join("link.fls");
    drop(FerroStore::open(&real).unwrap());
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let store = FerroStore::open(&link).unwrap();
    with_dead_extents(&store, 2);
    let err = store.compact().unwrap_err();
    assert!(err.to_string().contains("symlink"), "{err}");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    store.put_doc("c", "still", &json!(1)).unwrap();
}

// ---- another process -----------------------------------------------------------

const CHILD: &str = "FL_FERROSTORE_COMPACT_CHILD";
const CHILD_STOP: &str = "FL_FERROSTORE_COMPACT_CHILD_STOP";

/// The child: write one document after another, printing `ack <i>` once
/// each `put_doc` has returned, until the parent creates the stop file.
fn child_writer(path: &Path, stop: &Path) {
    use std::io::Write;
    let store = FerroStore::open(path).unwrap();
    let mut i = 0u64;
    let mut out = std::io::stdout();
    while i < 5 || !stop.exists() {
        store
            .put_doc("child", &i.to_string(), &json!({"i": i}))
            .unwrap();
        writeln!(out, "ack {i}").unwrap();
        out.flush().unwrap();
        i += 1;
    }
    for j in 0..i {
        assert_eq!(
            store.get_doc("child", &j.to_string()).unwrap(),
            Some(json!({"i": j}))
        );
    }
}

#[test]
fn compaction_while_another_process_writes_keeps_every_acknowledged_write() {
    if let (Some(path), Some(stop)) = (std::env::var_os(CHILD), std::env::var_os(CHILD_STOP)) {
        child_writer(Path::new(&path), Path::new(&stop));
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("store.fls");
    let stop = tmp.path().join("stop");
    let store = FerroStore::open(&path).unwrap();
    store.set_auto_compact_floor(u64::MAX);
    with_dead_extents(&store, 2);
    let exe = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(exe)
        .args([
            "--exact",
            "ferrostore::tests_compact::compaction_while_another_process_writes_keeps_every_acknowledged_write",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD, &path)
        .env(CHILD_STOP, &stop)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if line.starts_with("ack ") {
                let _ = tx.send(line.clone());
            }
            lines.push(line);
        }
        lines
    });
    let mut acks = 0usize;
    let wait_for_more_acks = |acks: &mut usize, more: usize| {
        let deadline = Instant::now() + Duration::from_secs(60);
        let goal = *acks + more;
        while *acks < goal {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(_) => *acks += 1,
                Err(err) => panic!("the child stopped acknowledging ({acks} acks): {err}"),
            }
        }
    };
    wait_for_more_acks(&mut acks, 1);
    let mut epochs = Vec::new();
    let rounds = 8;
    for round in 0..rounds {
        store
            .put_doc("parent", &format!("p{round}"), &json!(round))
            .unwrap();
        store.checkpoint().unwrap();
        store.compact().unwrap();
        epochs.push(store.stats().unwrap().epoch);
        // The child writes between this compaction and the next.
        wait_for_more_acks(&mut acks, 3);
    }
    std::fs::write(&stop, b"stop").unwrap();
    let status = child.wait().unwrap();
    let lines = reader.join().unwrap();
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(
        status.success(),
        "child failed:\n{}\n{stderr}",
        lines.join("\n")
    );
    assert!(
        lines.iter().any(|line| line.contains("1 passed")),
        "the child did not run the test:\n{}",
        lines.join("\n")
    );
    let acked: Vec<u64> = lines
        .iter()
        .filter_map(|line| line.strip_prefix("ack "))
        .map(|n| n.parse().unwrap())
        .collect();
    assert!(acked.len() >= 3 * rounds, "{} acks", acked.len());
    assert!(epochs.windows(2).all(|pair| pair[1] > pair[0]));
    drop(store);
    let again = FerroStore::open(&path).unwrap();
    let lost: Vec<u64> = acked
        .iter()
        .copied()
        .filter(|i| again.get_doc("child", &i.to_string()).unwrap() != Some(json!({"i": i})))
        .collect();
    assert!(
        lost.is_empty(),
        "{} of {} acknowledged child writes are lost after {rounds} compactions: {:?}",
        lost.len(),
        acked.len(),
        &lost[..lost.len().min(20)]
    );
    for round in 0..rounds {
        assert_eq!(
            again.get_doc("parent", &format!("p{round}")).unwrap(),
            Some(json!(round))
        );
    }
    println!(
        "§5 child-process compaction: {} acknowledged child writes across {rounds} compactions, \
         none lost",
        acked.len()
    );
}
