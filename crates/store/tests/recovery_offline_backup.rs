//! Explicit offline backup uses fixtures only and never probes a data socket.
#![cfg(target_os = "linux")]
mod recovery_task6_support;
use fl_store::recovery::{admission::*, backup::*, manifest::BundleManifestV1};
use recovery_task6_support::*;
use std::{fs, os::unix::net::UnixListener};
#[test]
fn offline_backup_unserved_store_requires_lease() {
    let f = Fixture::new();
    let reference = f.bundle();
    let manifest: BundleManifestV1 =
        serde_json::from_slice(&fs::read(reference.path().join("manifest.json")).unwrap()).unwrap();
    let request = f.backup_request();
    let binding = f.lease_request(
        OfflineAction::Backup,
        request.operation_id,
        manifest.validation_profile_sha256,
        &f.source,
    );
    let lease = create_offline_lease(&binding, &f.filesystem(), &request.limits).unwrap();
    let socket = fl_store::default_socket_path_for_store(&f.source);
    let trap = UnixListener::bind(&socket).unwrap();
    trap.set_nonblocking(true).unwrap();
    let before = fs::read(&f.source).unwrap();
    let outcome = backup_offline(
        &request,
        &lease,
        &f.filesystem(),
        &fl_store::recovery::CancelToken::new(),
    )
    .unwrap();
    assert!(matches!(outcome, PublicationOutcome::Durable(_)));
    assert_eq!(fs::read(&f.source).unwrap(), before);
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let mut missing = binding.clone();
    missing.operation_id = uuid::Uuid::now_v7();
    missing.exclusion.legacy_and_cached_handles_excluded = false;
    assert!(create_offline_lease(&missing, &f.filesystem(), &limits()).is_err());
    missing.exclusion.legacy_and_cached_handles_excluded = true;
    missing.expires_at_unix_ms = missing.issued_at_unix_ms;
    assert!(create_offline_lease(&missing, &f.filesystem(), &limits()).is_err());
}
#[test]
fn recovery_cli_exposes_explicit_offline_commands() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ferrostore"))
        .args(["recovery", "--help"])
        .env_remove("AGENT_PERSONA")
        .env_remove("AGENT_NAME")
        .env_remove("FL_HOSTED_ID")
        .env_remove("AGENT_HYDRATION_GENERATION")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let help = String::from_utf8(out.stdout).unwrap();
    for name in [
        "backup-offline",
        "restore",
        "resume",
        "rollback",
        "finalize",
        "purge",
        "status",
    ] {
        assert!(help.contains(name));
    }
}
#[test]
fn offline_backup_cold_mixed_snapshot_and_wal_uses_existing_publication() {
    let f = Fixture::new();
    let store = fl_store::FerroStore::open(&f.source).unwrap();
    store
        .create_vector_collection(
            "vectors",
            4,
            fl_store::ferrostore::VectorStorage::F32,
            fl_store::ferrostore::VectorMetric::L2,
            4,
            8,
            8,
        )
        .unwrap();
    store
        .upsert_vector("vectors", "a", &[1., 2., 3., 4.], None)
        .unwrap();
    store.compact().unwrap();
    store
        .upsert_vector("vectors", "b", &[2., 3., 4., 5.], None)
        .unwrap();
    drop(store);
    let mut request = f.backup_request();
    request.profile = fl_store::recovery::ValidationProfile::new(
        fl_store::recovery::ProfileKind::MixedTablesVectors,
        request.store.catalog_key.clone(),
    );
    let owner = fl_store::FerroStore::open(&f.source).unwrap();
    let pin = owner
        .capture_recovery(&request.capture, &fl_store::recovery::CancelToken::new())
        .unwrap();
    let expected = pin.expectations(&request.profile, &request.limits).unwrap();
    let profile_hash = serde_json::to_value(&expected).unwrap()["profile_sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(expected);
    drop(pin);
    drop(owner);
    let binding = f.lease_request(
        OfflineAction::Backup,
        request.operation_id,
        profile_hash,
        &f.source,
    );
    let lease = create_offline_lease(&binding, &f.filesystem(), &request.limits).unwrap();
    assert!(matches!(
        backup_offline(
            &request,
            &lease,
            &f.filesystem(),
            &fl_store::recovery::CancelToken::new()
        )
        .unwrap(),
        PublicationOutcome::Durable(_)
    ));
}
#[test]
fn recovery_cli_without_root_refuses_before_request_or_store_io() {
    use recovery_task6_support::test_support as support;
    let root = support::fixture_root();
    let source = root.path().join("absent.fls");
    let lease = root.path().join("missing-lease.json");
    let request = root.path().join("missing-request.json");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ferrostore"))
        .arg("recovery")
        .arg("--store")
        .arg(&source)
        .arg("--lease")
        .arg(&lease)
        .arg("--request")
        .arg(&request)
        .arg("backup-offline")
        .env_remove("AGENT_PERSONA")
        .env_remove("AGENT_NAME")
        .env_remove("FL_HOSTED_ID")
        .env_remove("AGENT_HYDRATION_GENERATION")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("explicit --root"));
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}
#[test]
fn recovery_cli_delegates_private_lease_and_explicit_offline_backup() {
    use fl_store::cli::recovery::OfflineBackupRequestV1;
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let bundle = f.bundle();
    let manifest: BundleManifestV1 =
        serde_json::from_slice(&fs::read(bundle.path().join("manifest.json")).unwrap()).unwrap();
    let request = f.backup_request();
    let binding = f.lease_request(
        OfflineAction::Backup,
        request.operation_id,
        manifest.validation_profile_sha256,
        &f.source,
    );
    let lease_request = f.root.path().join("lease-request.json");
    fs::write(&lease_request, serde_json::to_vec(&binding).unwrap()).unwrap();
    fs::set_permissions(&lease_request, fs::Permissions::from_mode(0o600)).unwrap();
    let command = || {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_ferrostore"));
        c.arg("recovery")
            .arg("--root")
            .arg(&f.backups)
            .arg("--store")
            .arg(&f.source);
        for name in [
            "AGENT_PERSONA",
            "AGENT_NAME",
            "FL_HOSTED_ID",
            "AGENT_HYDRATION_GENERATION",
        ] {
            c.env_remove(name);
        }
        c
    };
    let out = command()
        .arg("--request")
        .arg(&lease_request)
        .arg("lease")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let lease = std::path::PathBuf::from(report["lease"].as_str().unwrap());
    assert_eq!(
        fs::metadata(&lease).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(report["administrative_attestation"], true);
    let dto = OfflineBackupRequestV1 {
        version: 1,
        operation_id: request.operation_id,
        source: f.source.clone(),
        store: request.store,
        source_map_sha256: request.source_map_sha256,
        profile: request.profile,
        pin_byte_limit: request.capture.pin_byte_limit,
        preparation_byte_limit: request.capture.preparation_byte_limit,
        writer_delay_limit_ms: 1000,
        image_byte_limit: request.image_byte_limit,
        metadata_byte_limit: request.metadata_byte_limit,
        space_margin_bytes: request.space_margin_bytes,
        configs: request.configs,
        excluded_vault_key: request.excluded_vault_key,
        code_index_configuration: request.code_index_configuration,
    };
    let request_path = f.root.path().join("backup-request.json");
    fs::write(&request_path, serde_json::to_vec(&dto).unwrap()).unwrap();
    fs::set_permissions(&request_path, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&f.source).unwrap();
    let trap = UnixListener::bind(fl_store::default_socket_path_for_store(&f.source)).unwrap();
    trap.set_nonblocking(true).unwrap();
    let out = command()
        .arg("--lease")
        .arg(&lease)
        .arg("--request")
        .arg(&request_path)
        .arg("backup-offline")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["outcome"], "durable");
    assert_eq!(fs::read(&f.source).unwrap(), before);
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn completed_bundle_selection_refuses_invalid_receipt_without_repair_or_quarantine() {
    let f = Fixture::new();
    let bundle = f.bundle();
    let operation =
        uuid::Uuid::parse_str(bundle.path().file_name().unwrap().to_str().unwrap()).unwrap();
    let receipt = f.backups.join(".catalog").join(format!("{operation}.json"));
    let saved = fs::read(&receipt).unwrap();
    let image = fs::read(bundle.path().join("image.fls")).unwrap();
    assert!(
        fl_store::recovery::catalog::select_completed_bundle(operation, &f.filesystem(), &limits())
            .is_ok()
    );
    let mut malformed: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    malformed["manifest_sha256"] = serde_json::Value::String("0".repeat(64));
    let malformed = serde_json::to_vec(&malformed).unwrap();
    fs::write(&receipt, &malformed).unwrap();
    assert!(
        fl_store::recovery::catalog::select_completed_bundle(operation, &f.filesystem(), &limits())
            .is_err()
    );
    assert_eq!(fs::read(&receipt).unwrap(), malformed);
    assert_eq!(fs::read(bundle.path().join("image.fls")).unwrap(), image);
    assert!(
        !f.backups
            .join(".quarantine")
            .join(operation.to_string())
            .exists()
    );
    fs::remove_file(&receipt).unwrap();
    assert!(
        fl_store::recovery::catalog::select_completed_bundle(operation, &f.filesystem(), &limits())
            .is_err()
    );
    assert!(!receipt.exists());
    assert!(
        !f.backups
            .join(".quarantine")
            .join(operation.to_string())
            .exists()
    );
    fs::write(&receipt, &saved).unwrap();
    let validation = bundle.path().join("validation.json");
    let old = fs::read(&validation).unwrap();
    fs::write(&validation, b"{}").unwrap();
    assert!(
        fl_store::recovery::catalog::select_completed_bundle(operation, &f.filesystem(), &limits())
            .is_err()
    );
    assert_eq!(fs::read(validation).unwrap(), b"{}");
    assert_eq!(fs::read(receipt).unwrap(), saved);
    assert!(!old.is_empty());
    assert!(
        !f.backups
            .join(".quarantine")
            .join(operation.to_string())
            .exists()
    );
}
