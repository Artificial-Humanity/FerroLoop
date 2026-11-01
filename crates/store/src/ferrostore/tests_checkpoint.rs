//! Spec §4.4: the auto-checkpoint after a commit, inside the commit's lock.

use super::crash_sim::{IoOp, SimFs};
use super::layout::{SLOT_LEN, SlotRead, Superblock, decode_wal_prefix, parse_superblock};
use super::{FerroStore, lock_mutex};
use fl_core::StoreError;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

const STORE: &str = "/sim/store.fls";
const SNAPSHOT_MAGIC: &[u8] = b"FERRSNAP";

fn open(fs: &SimFs) -> FerroStore {
    FerroStore::open_with(fs, Path::new(STORE)).unwrap()
}

/// The live superblock of the simulated store, as the page cache holds it.
fn live_sb(fs: &SimFs) -> Superblock {
    let image = fs.live_image();
    let bytes = image.file(Path::new(STORE)).unwrap();
    (0..2)
        .filter_map(|index| {
            match parse_superblock(&bytes[index * SLOT_LEN..(index + 1) * SLOT_LEN]) {
                SlotRead::Valid(sb) => Some(sb),
                _ => None,
            }
        })
        .max_by_key(|sb| sb.epoch)
        .unwrap()
}

/// The frames of the live WAL.
fn live_frames(fs: &SimFs) -> usize {
    let sb = live_sb(fs);
    let image = fs.live_image();
    let bytes = image.file(Path::new(STORE)).unwrap();
    let wal = &bytes[sb.wal_offset as usize..(sb.wal_offset + sb.wal_length) as usize];
    decode_wal_prefix(wal).unwrap().0.len()
}

fn is_slot_write(op: &IoOp) -> bool {
    matches!(op, IoOp::Write { offset, len, .. }
        if *len == SLOT_LEN as u64 && (*offset == 0 || *offset == SLOT_LEN as u64))
}

fn text(bytes: usize) -> Value {
    json!("x".repeat(bytes))
}

#[test]
fn each_commit_that_crosses_the_threshold_checkpoints_exactly_once() {
    const FLOOR: u64 = 16 << 10;
    let fs = SimFs::default();
    let store = open(&fs);
    store.set_auto_checkpoint_floor(FLOOR);
    let (mut crossings, mut quiet) = (0, 0);
    for i in 0..60 {
        let before = live_sb(&fs);
        let mark = fs.trace().len();
        store
            .put_doc("bulk", &format!("{i:02}"), &text(1500 + 37 * i))
            .unwrap();
        let trace = fs.trace();
        let frame = trace[mark..]
            .iter()
            .find_map(|op| match op {
                IoOp::Write { len, .. } if !is_slot_write(op) => Some(*len),
                _ => None,
            })
            .unwrap();
        let crossed = before.wal_length + frame > before.snapshot_length.max(FLOOR);
        let snapshots = fs
            .writes_starting_with(SNAPSHOT_MAGIC)
            .into_iter()
            .filter(|at| *at >= mark)
            .count();
        assert_eq!(
            snapshots,
            usize::from(crossed),
            "commit {i}: WAL {} + frame {frame} against max(snapshot {}, {FLOOR})",
            before.wal_length,
            before.snapshot_length
        );
        let after = live_sb(&fs);
        if crossed {
            crossings += 1;
            assert_eq!(after.wal_length, 0, "commit {i}: the WAL restarts");
            assert_eq!(after.snapshot_seq, before.snapshot_seq.max(i as u64 + 1));
        } else {
            quiet += 1;
            assert_eq!(after.wal_length, before.wal_length + frame, "commit {i}");
        }
    }
    // The snapshot grows past the floor, so later crossings follow it.
    assert!(
        crossings >= 3 && quiet >= 20,
        "{crossings} crossings, {quiet} not"
    );
    assert!(live_sb(&fs).snapshot_length > FLOOR);
    let reopened = open(&fs);
    for i in 0..60 {
        assert_eq!(
            reopened.get_doc("bulk", &format!("{i:02}")).unwrap(),
            Some(text(1500 + 37 * i)),
            "doc {i}"
        );
    }
}

#[test]
fn a_reopen_after_an_auto_checkpoint_replays_only_the_wal_tail() {
    // The same writes with auto-checkpoint off, then on.
    let run = |floor: u64| {
        let fs = SimFs::default();
        let store = open(&fs);
        store.set_auto_checkpoint_floor(floor);
        for i in 0..20 {
            store.put_doc("bulk", &format!("{i}"), &text(4000)).unwrap();
        }
        for i in 0..3 {
            store.put_doc("tail", &format!("{i}"), &json!(i)).unwrap();
        }
        drop(store);
        let reopened = open(&fs);
        for i in 0..20 {
            assert_eq!(
                reopened.get_doc("bulk", &format!("{i}")).unwrap(),
                Some(text(4000))
            );
        }
        (live_frames(&fs), live_sb(&fs))
    };
    let (all, _) = run(u64::MAX);
    assert_eq!(
        all, 23,
        "with no checkpoint the reopen replays every commit"
    );
    let (tail, sb) = run(64 << 10);
    assert!(tail < 23, "a checkpoint ran: {tail} frames");
    assert!(
        sb.wal_length <= sb.snapshot_length.max(64 << 10),
        "the WAL a reopen replays is bounded: {sb:?}"
    );
    // 20 x 4 KB crosses 64 KiB once, at commit 16 or 17: what is left is
    // the commits after it.
    assert!(
        (3..=7).contains(&tail),
        "{tail} frames after the checkpoint"
    );
}

/// The writers of one group, queued in the order given (see `tests_group`).
fn one_group<'s, R: Send + 's>(
    store: &'s FerroStore,
    writers: Vec<Box<dyn FnOnce() -> R + Send + 's>>,
) -> Vec<R> {
    std::thread::scope(|scope| {
        let gate = lock_mutex(&store.commit_gate);
        let mut handles = Vec::new();
        for (queued, writer) in writers.into_iter().enumerate() {
            handles.push(scope.spawn(writer));
            store.wait_until_queued(queued + 1);
        }
        drop(gate);
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    })
}

/// A group whose publish crosses the threshold: the second member returns
/// before the leader starts the checkpoint. The leader is held at the
/// checkpoint's first op (its `len`, after the group's two syncs) until the
/// member has returned, or for 10 s.
#[test]
fn a_crossing_group_acknowledges_its_members_before_the_checkpoint() {
    let fs = SimFs::default();
    let store = open(&fs);
    store.set_auto_checkpoint_floor(1);
    let (returned_tx, returned_rx) = mpsc::channel::<()>();
    let returned_rx = Mutex::new(returned_rx);
    let member_first = Arc::new(AtomicBool::new(false));
    let held = Arc::new(AtomicBool::new(false));
    let (first, was_held) = (Arc::clone(&member_first), Arc::clone(&held));
    let mut syncs = 0;
    fs.fail_next(move |op| {
        match op {
            IoOp::SyncData { .. } => syncs += 1,
            IoOp::Len { .. } if syncs == 2 && !was_held.swap(true, Ordering::SeqCst) => {
                let waited = returned_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
                first.store(waited.is_ok(), Ordering::SeqCst);
            }
            _ => {}
        }
        false
    });
    let mark = fs.trace().len();
    let s = &store;
    let results = one_group(
        s,
        vec![
            Box::new(move || s.put_doc("g", "leader", &text(1000))),
            Box::new(move || {
                let result = s.put_doc("g", "member", &text(1000));
                returned_tx.send(()).unwrap();
                result
            }),
        ],
    );
    for result in results {
        result.unwrap();
    }
    assert!(held.load(Ordering::SeqCst), "the checkpoint never started");
    assert!(
        member_first.load(Ordering::SeqCst),
        "the member was not acknowledged before the checkpoint"
    );
    let snapshots = fs
        .writes_starting_with(SNAPSHOT_MAGIC)
        .into_iter()
        .filter(|at| *at >= mark)
        .count();
    assert_eq!(snapshots, 1, "one checkpoint after the group");
    assert_eq!(live_sb(&fs).wal_length, 0);
    let reopened = open(&fs);
    assert_eq!(reopened.get_doc("g", "member").unwrap(), Some(text(1000)));
    assert_eq!(reopened.get_doc("g", "leader").unwrap(), Some(text(1000)));
}

const POISONED: &str = "FerroStore handle poisoned after a failed fsync";

/// What fails in the auto-checkpoint.
#[derive(Clone, Copy, Debug)]
enum Fault {
    /// The `n`th `sync_data` of the checkpoint (1: the snapshot, 2: the slot).
    Sync(usize),
    /// The snapshot write.
    SnapshotWrite,
}

/// A commit whose auto-checkpoint meets `fault`. Returns the commit's
/// result and the next write's.
fn failed_auto_checkpoint(fault: Fault) -> (Result<(), StoreError>, Result<(), StoreError>, SimFs) {
    let fs = SimFs::default();
    let store = open(&fs);
    store.put_doc("f", "before", &json!(0)).unwrap();
    store.set_auto_checkpoint_floor(1);
    // The commit's own frame sync and slot sync come first.
    let mut syncs = 0;
    fs.fail_next(move |op| {
        let sync = matches!(op, IoOp::SyncData { .. });
        if syncs < 2 {
            syncs += usize::from(sync);
            return false;
        }
        match fault {
            Fault::Sync(n) => {
                syncs += usize::from(sync);
                sync && syncs == 2 + n
            }
            Fault::SnapshotWrite => matches!(op, IoOp::Write { .. }) && !is_slot_write(op),
        }
    });
    let commit = store.put_doc("f", "big", &text(2000));
    let next = store.put_doc("f", "next", &json!(1));
    (commit, next, fs)
}

#[test]
fn a_failed_auto_checkpoint_sync_keeps_the_commit_and_poisons_the_handle() {
    for fault in [Fault::Sync(1), Fault::Sync(2)] {
        let (commit, next, fs) = failed_auto_checkpoint(fault);
        assert!(
            commit.is_ok(),
            "{fault:?}: the durable commit failed: {commit:?}"
        );
        let err = next.unwrap_err().to_string();
        assert!(err.contains(POISONED), "{fault:?}: {err}");
        let reopened = open(&fs);
        assert_eq!(
            reopened.get_doc("f", "big").unwrap(),
            Some(text(2000)),
            "{fault:?}"
        );
        assert_eq!(reopened.get_doc("f", "next").unwrap(), None, "{fault:?}");
    }
}

#[test]
fn a_failed_auto_checkpoint_write_keeps_the_commit_and_the_next_commit_checkpoints() {
    let (commit, next, fs) = failed_auto_checkpoint(Fault::SnapshotWrite);
    commit.unwrap();
    // A failed write is not a failed fsync: the handle is not poisoned, and
    // the next commit, still past the threshold, checkpoints.
    next.unwrap();
    assert_eq!(live_sb(&fs).wal_length, 0);
    let reopened = open(&fs);
    assert_eq!(reopened.get_doc("f", "big").unwrap(), Some(text(2000)));
    assert_eq!(reopened.get_doc("f", "next").unwrap(), Some(json!(1)));
}
