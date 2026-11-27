use std::sync::Barrier;

use fl_core::StateStore;
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_store::{FerroStore, RedbStore};
use serde_json::json;

fn args(number: u64) -> UpsertStateArgs {
    UpsertStateArgs {
        scope: StateScope::Workspace,
        slug: "concurrent-runtime".into(),
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Runtime(RuntimeEntry {
            value: json!(number),
            merge_mode: MergeMode::LastWriter,
        }),
        at: fl_core::At::from_unix_millis(1),
        by: "fixture".into(),
    }
}

fn concurrent(first: &(impl StateStore + Sync), second: &(impl StateStore + Sync)) {
    let created = first
        .upsert_state_if_current(args(0), None)
        .unwrap()
        .unwrap();
    assert!(
        second
            .upsert_state_if_current(args(9), None)
            .unwrap()
            .is_none()
    );

    let barrier = Barrier::new(2);
    let revision = &created.head_revision;
    let (left, right) = std::thread::scope(|threads| {
        let left = threads.spawn(|| {
            barrier.wait();
            first
                .upsert_state_if_current(args(1), Some(revision))
                .unwrap()
        });
        let right = threads.spawn(|| {
            barrier.wait();
            second
                .upsert_state_if_current(args(2), Some(revision))
                .unwrap()
        });
        (left.join().unwrap(), right.join().unwrap())
    });
    assert_ne!(left.is_some(), right.is_some());
    let winner = left.or(right).unwrap();
    assert_eq!(
        first
            .get_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                "concurrent-runtime"
            )
            .unwrap(),
        Some(winner),
    );
    assert_eq!(first.state_revisions(&created.id).unwrap().len(), 2);
}

#[test]
fn redb_conditional_upsert_prevents_concurrent_lost_update() {
    let dir = tempfile::tempdir().unwrap();
    let store = RedbStore::open(&dir.path().join("state.redb")).unwrap();
    concurrent(&store, &store);
}

#[test]
fn ferro_conditional_upsert_prevents_concurrent_lost_update_across_handles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.fls");
    let store = FerroStore::open(&path).unwrap();
    let other = FerroStore::open(&path).unwrap();
    concurrent(&store, &other);
}

/// A conditional delete racing a conditional rewrite of the same revision: exactly one
/// wins, and a rewrite that wins is never deleted.
fn delete_races_rewrite(first: &(impl StateStore + Sync), second: &(impl StateStore + Sync)) {
    let created = first.upsert_state(args(0)).unwrap();
    let barrier = Barrier::new(2);
    let revision = &created.head_revision;
    let (rewritten, deleted) = std::thread::scope(|threads| {
        let rewrite = threads.spawn(|| {
            barrier.wait();
            first
                .upsert_state_if_current(args(1), Some(revision))
                .unwrap()
        });
        let delete = threads.spawn(|| {
            barrier.wait();
            second
                .delete_state_if_current(
                    &StateScope::Workspace,
                    StateConcern::Runtime,
                    "concurrent-runtime",
                    revision,
                )
                .unwrap()
        });
        (rewrite.join().unwrap(), delete.join().unwrap())
    });
    assert_ne!(rewritten.is_some(), deleted);
    assert_eq!(
        first
            .get_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                "concurrent-runtime"
            )
            .unwrap(),
        rewritten,
    );
}

#[test]
fn redb_conditional_delete_never_removes_a_concurrent_rewrite() {
    for _ in 0..8 {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("state.redb")).unwrap();
        delete_races_rewrite(&store, &store);
    }
}

#[test]
fn ferro_conditional_delete_never_removes_a_concurrent_rewrite_across_handles() {
    for _ in 0..8 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.fls");
        let store = FerroStore::open(&path).unwrap();
        let other = FerroStore::open(&path).unwrap();
        delete_races_rewrite(&store, &other);
    }
}
