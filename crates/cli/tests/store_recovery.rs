//! Both frontends delegate recovery to the same strict arguments and executor.
use fl_store::recovery::{
    CancelToken, OperationLimits,
    admission::*,
    fs::{NativeRecoveryFs, RecoveryFs},
    manifest::{Availability, BuildIdentity},
};
use std::time::{Duration, Instant};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    process::Command,
};
#[path = "../../store/src/recovery/test_support.rs"]
mod support;
fn command(root: &std::path::Path, source: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_fl"));
    c.current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env_remove("FL_DB")
        .env_remove("FL_STORE_BACKEND")
        .arg("--db")
        .arg(source)
        .args(["store", "recovery"]);
    for name in [
        "AGENT_PERSONA",
        "AGENT_NAME",
        "FL_HOSTED_ID",
        "AGENT_HYDRATION_GENERATION",
    ] {
        c.env_remove(name);
    }
    c
}
#[test]
fn store_recovery_shared_executor_refuses_absent_root_without_creating_store() {
    let root = support::fixture_root();
    let source = root.path().join("absent.fls");
    let out = command(root.path(), &source)
        .args([
            "--request",
            "missing-request",
            "--lease",
            "missing-lease",
            "backup-offline",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("explicit --root"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}
#[test]
fn store_recovery_shared_executor_creates_only_requested_private_lease() {
    let root = support::fixture_root();
    let source = root.path().join("source.fls");
    drop(fl_store::FerroStore::open(&source).unwrap());
    let before = fs::read(&source).unwrap();
    let backup_root = root.path().join("backups");
    fs::create_dir(&backup_root).unwrap();
    fs::set_permissions(&backup_root, fs::Permissions::from_mode(0o700)).unwrap();
    let l = OperationLimits::new(
        Instant::now() + Duration::from_secs(30),
        64 << 20,
        128 << 20,
        CancelToken::new(),
    );
    let filesystem = NativeRecoveryFs::new(backup_root.clone(), l.clone());
    let id = |p: &std::path::Path| {
        let m = fs::metadata(p).unwrap();
        (m.dev(), m.ino())
    };
    let now = filesystem.io().unix_ms().unwrap();
    let request = OfflineLeaseRequest {
        version: 1,
        operation_id: l.operation_id,
        action: OfflineAction::Backup,
        catalog_identity: "isolated CLI fixture".into(),
        approved_root_identity: id(&backup_root),
        physical_path: source.clone(),
        parent_identity: id(root.path()),
        expected_file: inspect_file_binding(&source, &l).unwrap(),
        profile_sha256: "a".repeat(64),
        build: BuildIdentity {
            package_version: env!("CARGO_PKG_VERSION").into(),
            source_revision: Availability::Unavailable,
            executable_sha256: Availability::Unavailable,
        },
        issued_at_unix_ms: now,
        expires_at_unix_ms: now + 60000,
        operator: "CLI fixture operator".into(),
        exclusion: ExclusionAttestation {
            legacy_and_cached_handles_excluded: true,
            external_dispatch_excluded: true,
            active_owner_excluded: true,
            evidence: "fixture handles closed; no dispatch".into(),
        },
    };
    let request_path = root.path().join("lease-request.json");
    fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();
    fs::set_permissions(&request_path, fs::Permissions::from_mode(0o600)).unwrap();
    let out = command(root.path(), &source)
        .arg("--root")
        .arg(&backup_root)
        .arg("--request")
        .arg(&request_path)
        .arg("lease")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let lease = PathBuf::from(report["lease"].as_str().unwrap());
    assert_eq!(lease.parent(), Some(backup_root.as_path()));
    assert_eq!(
        fs::metadata(lease).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read(source).unwrap(), before);
    assert_eq!(fs::read_dir(&backup_root).unwrap().count(), 1);
}
