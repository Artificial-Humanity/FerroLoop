//! `health::verify_read_only` (`fl doctor`): it classifies a store and changes NOTHING.
//!
//! Every case snapshots the store's directory (each entry's name, length, mtime and SHA-256,
//! and the directory's own mtime) before and after the verification and demands equality, so a
//! created `-wal` or lock file, a rewritten page, a migration or a touched mtime fails here.

use super::layout::{self};
use super::{FerroStore, latest_superblock};
use crate::health::verify_read_only;
use crate::{
    BackendKind, FORMAT_KEY, FORMAT_VERSION, FORMAT_WITH_AREA_DELETE, FORMAT_WITH_STATE,
    FORMAT_WITH_WORKFLOW, META, RedbStore,
};
use fl_core::doctor::StoreOutcome;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

type Snapshot = BTreeMap<String, (u64, SystemTime, String)>;

fn snapshot(dir: &Path) -> (Snapshot, SystemTime) {
    let mut out = Snapshot::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let meta = e.metadata().unwrap();
        let hash = if meta.is_file() {
            format!("{:x}", Sha256::digest(std::fs::read(e.path()).unwrap()))
        } else {
            String::new()
        };
        out.insert(
            e.file_name().to_string_lossy().into_owned(),
            (meta.len(), meta.modified().unwrap(), hash),
        );
    }
    (out, std::fs::metadata(dir).unwrap().modified().unwrap())
}

/// Run `verify_read_only(path, forced)` and prove the directory did not change.
fn verify(path: &Path, forced: Option<BackendKind>) -> StoreOutcome {
    let dir = path.parent().unwrap();
    let before = snapshot(dir);
    let outcome = verify_read_only(path, forced);
    let after = snapshot(dir);
    assert_eq!(
        before, after,
        "the verification changed the store directory"
    );
    outcome
}

fn fls_with_schema(path: &Path, schema: Option<u64>) {
    let store = FerroStore::open(path).unwrap();
    store.put_doc("c", "a", &json!(1)).unwrap();
    if let Some(v) = schema {
        store
            .put_doc_unchecked("__meta", "schema_version", &json!(v), false)
            .unwrap();
    }
}

fn set_redb_format(path: &Path, to: u64) {
    let db = redb::Database::open(path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut meta = tx.open_table(META).unwrap();
        meta.insert(FORMAT_KEY, to).unwrap();
    }
    tx.commit().unwrap();
}

#[test]
fn an_absent_store_is_absent_and_nothing_is_created() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        verify(&tmp.path().join("fl.redb"), None),
        StoreOutcome::Absent
    );
    assert_eq!(
        verify(&tmp.path().join("fl.redb"), Some(BackendKind::Redb)),
        StoreOutcome::Absent
    );
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    // A directory that does not exist is absent too, and is not made.
    let missing = tmp.path().join("nope/fl.redb");
    assert_eq!(verify_read_only(&missing, None), StoreOutcome::Absent);
    assert!(!tmp.path().join("nope").exists());
}

#[test]
fn a_fls_store_is_verified_at_its_recorded_format() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fl.fls");
    // Opening a store records the newest schema version.
    drop(FerroStore::open(&path).unwrap());
    assert_eq!(
        verify(&path, None),
        StoreOutcome::Verified {
            engine: "fls".into(),
            format: Some(FORMAT_WITH_WORKFLOW),
            newest: FORMAT_WITH_AREA_DELETE,
            backup_beside: false
        }
    );
    fls_with_schema(&path, None);
    assert!(matches!(
        verify(&path, None),
        StoreOutcome::Verified { format: Some(f), .. } if f == FORMAT_WITH_WORKFLOW
    ));
    // The logical `.redb` path of the same store finds the `.fls`.
    let logical = tmp.path().join("fl.redb");
    assert!(matches!(
        verify(&logical, None),
        StoreOutcome::Verified { ref engine, backup_beside: false, .. } if engine == "fls"
    ));

    let at_state = tmp.path().join("state.fls");
    fls_with_schema(&at_state, Some(FORMAT_WITH_STATE));
    assert!(matches!(
        verify(&at_state, None),
        StoreOutcome::Verified { format: Some(f), .. } if f == FORMAT_WITH_STATE
    ));
}

#[test]
fn a_fls_newer_or_off_the_ladder_is_unsupported() {
    let tmp = tempfile::tempdir().unwrap();
    for (name, schema) in [
        ("newer.fls", FORMAT_WITH_AREA_DELETE + 1),
        ("old.fls", FORMAT_VERSION - 1),
    ] {
        let path = tmp.path().join(name);
        fls_with_schema(&path, Some(schema));
        match verify(&path, None) {
            StoreOutcome::Unsupported { reason } => {
                assert!(reason.contains(&schema.to_string()), "{reason}");
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[test]
fn a_fls_from_a_newer_container_is_unsupported_not_corrupt() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fl.fls");
    fls_with_schema(&path, None);
    let (index, mut sb) = latest_superblock(&File::open(&path).unwrap()).unwrap();
    sb.epoch += 1;
    let slot = layout::encode_superblock_as(&sb, 3, 0, 0);
    let f = OpenOptions::new().write(true).open(&path).unwrap();
    f.write_all_at(&slot, ((index ^ 1) * layout::SLOT_LEN) as u64)
        .unwrap();
    f.sync_all().unwrap();
    drop(f);
    match verify(&path, None) {
        StoreOutcome::Unsupported { reason } => {
            assert!(reason.contains("newer fl"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_damaged_or_empty_file_is_invalid_and_is_not_initialised() {
    let tmp = tempfile::tempdir().unwrap();
    let garbage = tmp.path().join("garbage.fls");
    std::fs::write(&garbage, vec![0xAB; 9000]).unwrap();
    assert!(matches!(
        verify(&garbage, None),
        StoreOutcome::Invalid { .. }
    ));
    // An empty file is what an interrupted first open leaves: a normal open would
    // initialise it; doctor must not.
    let empty = tmp.path().join("empty.fls");
    std::fs::write(&empty, b"").unwrap();
    assert!(matches!(verify(&empty, None), StoreOutcome::Invalid { .. }));
    assert_eq!(std::fs::metadata(&empty).unwrap().len(), 0);
    // A directory where the store should be is not a store.
    let dir = tmp.path().join("dir.fls");
    std::fs::create_dir(&dir).unwrap();
    assert!(matches!(verify(&dir, None), StoreOutcome::Invalid { .. }));
    // A garbage .redb.
    let redb = tmp.path().join("bad.redb");
    std::fs::write(&redb, vec![0x11; 9000]).unwrap();
    assert!(matches!(
        verify(&redb, Some(BackendKind::Redb)),
        StoreOutcome::Invalid { .. }
    ));
}

#[test]
fn a_lone_redb_would_migrate_and_is_not_migrated() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fl.redb");
    drop(RedbStore::open(&path).unwrap());
    match verify(&path, None) {
        StoreOutcome::WouldMigrate { format } => assert_eq!(format, Some(FORMAT_VERSION)),
        other => panic!("{other:?}"),
    }
    assert!(!tmp.path().join("fl.fls").exists(), "doctor migrated");
    // Under an explicit redb selection nothing migrates, so it is simply verified.
    assert!(matches!(
        verify(&path, Some(BackendKind::Redb)),
        StoreOutcome::Verified { ref engine, format: Some(f), .. }
            if engine == "redb" && f == FORMAT_VERSION
    ));
}

#[test]
fn a_redb_beside_a_fls_is_the_backup_and_the_fls_is_what_is_verified() {
    let tmp = tempfile::tempdir().unwrap();
    let redb = tmp.path().join("fl.redb");
    drop(RedbStore::open(&redb).unwrap());
    fls_with_schema(&tmp.path().join("fl.fls"), None);
    assert!(matches!(
        verify(&redb, None),
        StoreOutcome::Verified { ref engine, backup_beside: true, .. } if engine == "fls"
    ));
}

#[test]
fn a_redb_newer_than_supported_is_unsupported_even_when_it_would_migrate() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fl.redb");
    drop(RedbStore::open(&path).unwrap());
    set_redb_format(&path, FORMAT_WITH_AREA_DELETE + 1);
    for forced in [None, Some(BackendKind::Redb)] {
        match verify(&path, forced) {
            StoreOutcome::Unsupported { reason } => {
                assert!(reason.contains("newer fl"), "{reason}");
            }
            other => panic!("{forced:?}: {other:?}"),
        }
    }
}

#[test]
fn a_redb_held_by_a_writer_is_locked_and_the_wait_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fl.redb");
    let _writer = RedbStore::open(&path).unwrap();
    let t = Instant::now();
    let outcome = verify_read_only(&path, Some(BackendKind::Redb));
    let took = t.elapsed();
    assert_eq!(outcome, StoreOutcome::Locked);
    assert!(took < Duration::from_secs(3), "waited {took:?}");
    assert!(
        !tmp.path().join("fl.redb-wal").exists()
            && std::fs::read_dir(tmp.path()).unwrap().count() == 1,
        "doctor left a file behind"
    );
}

#[test]
fn a_read_only_store_file_is_verified_without_a_permission_error() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fl.redb");
    drop(RedbStore::open(&path).unwrap());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    // The state a migration leaves a .redb in.
    assert!(matches!(
        verify(&path, Some(BackendKind::Redb)),
        StoreOutcome::Verified { .. }
    ));
    let fls = tmp.path().join("ro.fls");
    fls_with_schema(&fls, None);
    std::fs::set_permissions(&fls, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert!(matches!(verify(&fls, None), StoreOutcome::Verified { .. }));
}

#[test]
fn a_symlink_that_is_not_to_a_regular_file_is_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    let link = tmp.path().join("fl.fls");
    std::os::unix::fs::symlink(tmp.path(), &link).unwrap();
    assert!(matches!(verify(&link, None), StoreOutcome::Invalid { .. }));
}

#[test]
fn a_redb_not_closed_cleanly_needs_recovery_and_is_neither_repaired_nor_called_corrupt() {
    let live = tempfile::tempdir().unwrap();
    let live_path = live.path().join("fl.redb");
    let writer = RedbStore::open(&live_path).unwrap();
    // A file copied under a live writer is what a crash leaves: the clean-shutdown flag unset.
    let dir = tempfile::tempdir().unwrap();
    let dirty = dir.path().join("fl.redb");
    std::fs::copy(&live_path, &dirty).unwrap();
    drop(writer);

    let bytes = std::fs::read(&dirty).unwrap();
    let first = verify(&dirty, Some(BackendKind::Redb));
    assert_eq!(first, StoreOutcome::NeedsRecovery);
    // Deterministic, and nothing was repaired or left beside it.
    for _ in 0..3 {
        assert_eq!(verify(&dirty, Some(BackendKind::Redb)), first);
    }
    assert_eq!(
        std::fs::read(&dirty).unwrap(),
        bytes,
        "doctor changed the file"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "a sidecar appeared"
    );
    // The logical path under the ferro selection (a lone .redb) reports the same.
    assert_eq!(verify(&dirty, None), StoreOutcome::NeedsRecovery);
    // It is valid, not corrupt: a normal open recovers it (and only then does it change).
    assert!(RedbStore::open(&dirty).is_ok());
    // Garbage stays a failure.
    let garbage = dir.path().join("garbage.redb");
    std::fs::write(&garbage, vec![0x5A; 9000]).unwrap();
    assert!(matches!(
        verify(&garbage, Some(BackendKind::Redb)),
        StoreOutcome::Invalid { .. }
    ));
}

#[test]
fn a_fls_held_open_by_a_live_writer_is_verified_at_once_without_a_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fl.fls");
    let writer = FerroStore::open(&path).unwrap();
    writer.put_doc("c", "a", &json!(1)).unwrap();
    let t = Instant::now();
    let outcome = verify(&path, None);
    let took = t.elapsed();
    assert!(
        matches!(outcome, StoreOutcome::Verified { ref engine, .. } if engine == "fls"),
        "{outcome:?}"
    );
    assert!(took < Duration::from_millis(500), "waited {took:?}");
    // The writer is unharmed and still writes after the check.
    writer.put_doc("c", "b", &json!(2)).unwrap();
    assert_eq!(writer.get_doc("c", "b").unwrap(), Some(json!(2)));
    assert!(matches!(verify(&path, None), StoreOutcome::Verified { .. }));
}

#[test]
fn a_fifo_in_place_of_a_store_is_refused_without_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    let fifo = tmp.path().join("fl.fls");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let t = Instant::now();
    assert!(matches!(verify(&fifo, None), StoreOutcome::Invalid { .. }));
    assert!(t.elapsed() < Duration::from_secs(2));
}
