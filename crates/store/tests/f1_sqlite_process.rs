//! Black-box contention across independent processes opening the same local preparation file.
use fl_core::f1_records::{
    AllocationEvidenceRecord, F1PreparationStore, F1RecordError, SafetyState,
};
use fl_store::SqliteF1PreparationStore;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn child_worker() {
    let Some(root) = std::env::var_os("FL_F1_SQLITE_TEST_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let name = std::env::var("FL_F1_SQLITE_TEST_CHILD").unwrap();
    let store = SqliteF1PreparationStore::open(&root.join("f1.sqlite")).unwrap();
    std::fs::write(root.join(format!("ready-{name}")), b"ready").unwrap();
    wait_for(&root.join("go"));
    let result = store.request_allocation_stop("allocation", 1);
    let text = match result {
        Ok(()) => "won",
        Err(F1RecordError::Conflict) => "conflict",
        other => panic!("unexpected result: {other:?}"),
    };
    std::fs::write(root.join(format!("result-{name}")), text).unwrap();
}

#[test]
fn independent_processes_cannot_both_win_the_same_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let store = SqliteF1PreparationStore::open(&root.join("f1.sqlite")).unwrap();
    store
        .insert_allocation(AllocationEvidenceRecord {
            version: 1,
            allocation_id: "allocation".into(),
            parent_id: None,
            root_id: "allocation".into(),
            source_window_id: "window".into(),
            deadline_utc: "2030-01-01T00:00:00Z".into(),
            tokens: 10,
            cost_micros: 10,
            safety: SafetyState::Inert,
        })
        .unwrap();
    drop(store);
    let mut children: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|name| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "child_worker", "--nocapture"])
                .env("FL_F1_SQLITE_TEST_ROOT", root)
                .env("FL_F1_SQLITE_TEST_CHILD", name)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    wait_for(&root.join("ready-a"));
    wait_for(&root.join("ready-b"));
    std::fs::write(root.join("go"), b"go").unwrap();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    let mut results = [
        std::fs::read_to_string(root.join("result-a")).unwrap(),
        std::fs::read_to_string(root.join("result-b")).unwrap(),
    ];
    results.sort();
    assert_eq!(results, ["conflict", "won"]);
    let reopened = SqliteF1PreparationStore::open(&root.join("f1.sqlite")).unwrap();
    assert_eq!(
        reopened.allocation("allocation").unwrap().unwrap().safety,
        SafetyState::StopRequested
    );
}

#[cfg(unix)]
#[test]
fn private_permissions_child() {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let Some(root) = std::env::var_os("FL_F1_SQLITE_PERMISSION_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let path = root.join("private.sqlite");
    let store = SqliteF1PreparationStore::open(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch(
        "PRAGMA journal_mode=WAL; BEGIN IMMEDIATE; INSERT INTO f1_records VALUES ('probe', x'00');",
    )
    .unwrap();
    for sidecar in ["private.sqlite-wal", "private.sqlite-shm"] {
        let mode = std::fs::metadata(root.join(sidecar))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{sidecar}");
    }
    raw.execute_batch("ROLLBACK;").unwrap();
    drop(raw);
    drop(store);
    let existing = root.join("existing.sqlite");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&existing)
        .unwrap();
    let store = SqliteF1PreparationStore::open(&existing).unwrap();
    assert_eq!(
        std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
        0o644
    );
    drop(store);
}

#[cfg(unix)]
#[test]
fn created_sqlite_and_sidecars_are_private_under_permissive_umask() {
    use std::os::unix::process::CommandExt;
    let tmp = tempfile::tempdir().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "private_permissions_child", "--nocapture"])
        .env("FL_F1_SQLITE_PERMISSION_ROOT", tmp.path());
    // SAFETY: umask is changed only in the just-forked child before exec.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0);
            Ok(())
        });
    }
    assert!(command.status().unwrap().success());
}
