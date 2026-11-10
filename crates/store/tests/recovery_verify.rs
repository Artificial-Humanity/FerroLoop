//! Independent image verification uses only isolated fixture files and inert sockets.
use fl_store::ferrostore::{
    ColumnDef, ColumnType, ColumnValue, FerroStore, VectorMetric, VectorStorage,
};
use fl_store::recovery::{
    CancelToken, CaptureBudget, OperationLimits, ProfileKind, ValidationProfile, copy_and_verify,
    open_recovery_image_existing, verify_existing_copy,
};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::{Duration, Instant},
};
fn private_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}
fn limits() -> OperationLimits {
    OperationLimits::new(
        Instant::now() + Duration::from_secs(30),
        32 << 20,
        256 << 20,
        CancelToken::new(),
    )
}
fn pin(store: &FerroStore) -> fl_store::ferrostore::CapturedStore {
    store
        .capture_recovery(
            &CaptureBudget::new(
                Instant::now() + Duration::from_secs(30),
                64 << 20,
                128 << 20,
                Duration::from_secs(1),
            ),
            &CancelToken::new(),
        )
        .unwrap()
}
fn image(store: &FerroStore, path: &Path) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    pin(store).write_image(&mut file, &limits()).unwrap();
}
fn profile() -> ValidationProfile {
    ValidationProfile::new(ProfileKind::Workspace, "fixture-catalog")
}

#[cfg(target_os = "linux")]
#[test]
fn wrong_payload_same_count_absent_null_and_array_order_are_rejected() {
    for (original, changed) in [
        (
            json!({"body":"private-original"}),
            json!({"body":"private-mutated"}),
        ),
        (json!({}), json!({"missing":null})),
        (json!([1, 2]), json!([2, 1])),
    ] {
        let tmp = private_dir();
        let a = FerroStore::open(tmp.path().join("a.fls")).unwrap();
        let b = FerroStore::open(tmp.path().join("b.fls")).unwrap();
        a.put_doc("unknown", "secret-id", &original).unwrap();
        b.put_doc("unknown", "secret-id", &changed).unwrap();
        let expected = pin(&a).expectations(&profile(), &limits()).unwrap();
        image(&b, &tmp.path().join("wrong.fls"));
        let error = verify_existing_copy(
            &tmp.path().join("wrong.fls"),
            &expected,
            &profile(),
            &limits(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            fl_store::recovery::RecoveryError::Verification { .. }
        ));
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains("private-") && !diagnostic.contains("secret-id"));
    }
}
#[cfg(target_os = "linux")]
#[test]
fn dropped_unknown_collection_is_rejected() {
    let tmp = private_dir();
    let a = FerroStore::open(tmp.path().join("a.fls")).unwrap();
    let b = FerroStore::open(tmp.path().join("b.fls")).unwrap();
    a.put_doc("vendor_unknown", "secret-id", &json!({"v":42}))
        .unwrap();
    b.put_doc("different_unknown", "secret-id", &json!({"v":42}))
        .unwrap();
    let p = ValidationProfile::new(ProfileKind::SmallEmpty, "fixture");
    let expected = pin(&a).expectations(&p, &limits()).unwrap();
    image(&b, &tmp.path().join("wrong.fls"));
    assert!(matches!(
        verify_existing_copy(&tmp.path().join("wrong.fls"), &expected, &p, &limits()),
        Err(fl_store::recovery::RecoveryError::Verification { .. })
    ));
}
#[cfg(target_os = "linux")]
#[test]
fn existing_only_and_one_copy_never_contact_socket_or_live_source() {
    assert!(
        std::env::var_os("FL_DB").is_none(),
        "fixture runner must clear inherited FL_DB"
    );
    let tmp = private_dir();
    let source = tmp.path().join("source.fls");
    let store = FerroStore::open(&source).unwrap();
    store
        .put_doc("unknown", "x", &json!({"body":"fixture lexical"}))
        .unwrap();
    let captured = pin(&store);
    let expected = captured.expectations(&profile(), &limits()).unwrap();
    let image_path = tmp.path().join("image.fls");
    image(&store, &image_path);
    // Keep the unlinked inode alive so the filesystem cannot reuse its number
    // for the new copy. The missing source pathname still traps live fallback.
    let _unlinked_source_guard = store;
    fs::remove_file(&source).unwrap();
    let copy = tmp.path().join("copy.fls");
    let socket = std::os::unix::net::UnixListener::bind(copy.with_extension("fls.sock")).unwrap();
    socket.set_nonblocking(true).unwrap();
    let before = fs::read_dir(tmp.path()).unwrap().count();
    let report = copy_and_verify(&image_path, &copy, &expected, &profile(), &limits()).unwrap();
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), before + 1);
    assert_ne!(
        fs::metadata(&copy).unwrap().ino(),
        fs::metadata(&image_path).unwrap().ino()
    );
    assert_eq!(fs::metadata(&copy).unwrap().mode() & 0o777, 0o600);
    let bytes = fs::read(&copy).unwrap();
    let again = verify_existing_copy(&copy, &expected, &profile(), &limits()).unwrap();
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), before + 1);
    assert_eq!(bytes, fs::read(&copy).unwrap());
    assert_eq!(report.image_sha256, again.image_sha256);
    assert_eq!(again.unknown_collection_count, 1);
    assert!(socket.accept().is_err());
    let absent = tmp.path().join("absent.fls");
    assert!(open_recovery_image_existing(&absent).is_err());
    assert!(!absent.exists());
}
#[cfg(target_os = "linux")]
#[test]
fn rejects_alias_existing_destination_wrong_identity_sequence_and_limits() {
    let tmp = private_dir();
    let store = FerroStore::open(tmp.path().join("source.fls")).unwrap();
    let expected = pin(&store).expectations(&profile(), &limits()).unwrap();
    let path = tmp.path().join("image.fls");
    image(&store, &path);
    assert!(copy_and_verify(&path, &path, &expected, &profile(), &limits()).is_err());
    let alias = tmp.path().join("hardlink.fls");
    fs::hard_link(&path, &alias).unwrap();
    assert!(verify_existing_copy(&alias, &expected, &profile(), &limits()).is_err());
    fs::remove_file(alias).unwrap();
    let wrong = ValidationProfile::new(ProfileKind::Workspace, "other-catalog");
    assert!(verify_existing_copy(&path, &expected, &wrong, &limits()).is_err());
    store.put_doc("x", "x", &Value::Null).unwrap();
    let newer = tmp.path().join("newer.fls");
    image(&store, &newer);
    assert!(verify_existing_copy(&newer, &expected, &profile(), &limits()).is_err());
    let mut tiny = limits();
    tiny.memory_byte_limit = 1;
    assert!(verify_existing_copy(&path, &expected, &profile(), &tiny).is_err());
    let cancelled = limits();
    cancelled.cancel.cancel();
    assert!(verify_existing_copy(&path, &expected, &profile(), &cancelled).is_err());
}
#[cfg(target_os = "linux")]
#[test]
fn mixed_columns_and_authoritative_vectors_are_preserved_and_checked() {
    let tmp = private_dir();
    let store = FerroStore::open(tmp.path().join("source.fls")).unwrap();
    store
        .create_column_table(
            "measurements",
            &[ColumnDef {
                name: "n".into(),
                ty: ColumnType::Int64,
            }],
        )
        .unwrap();
    store
        .append_column_rows("measurements", &[vec![ColumnValue::Int64(42)]])
        .unwrap();
    store
        .create_vector_collection(
            "authoritative",
            2,
            VectorStorage::F32,
            VectorMetric::L2,
            4,
            8,
            8,
        )
        .unwrap();
    store
        .upsert_vector(
            "authoritative",
            "v1",
            &[1.0, 0.0],
            Some(&json!({"private":"metadata"})),
        )
        .unwrap();
    let p = ValidationProfile::new(ProfileKind::MixedTablesVectors, "fixture");
    let expected = pin(&store).expectations(&p, &limits()).unwrap();
    let path = tmp.path().join("image.fls");
    image(&store, &path);
    let report = verify_existing_copy(&path, &expected, &p, &limits()).unwrap();
    assert!(!serde_json::to_string(&report).unwrap().contains("metadata"));
    store
        .upsert_vector("authoritative", "v1", &[0.0, 1.0], None)
        .unwrap();
    let wrong = tmp.path().join("wrong.fls");
    image(&store, &wrong);
    assert!(verify_existing_copy(&wrong, &expected, &p, &limits()).is_err());
}
#[cfg(target_os = "linux")]
#[test]
fn captured_original_cannot_be_its_own_independent_copy() {
    let tmp = private_dir();
    let path = tmp.path().join("original.fls");
    let store = FerroStore::open(&path).unwrap();
    store.put_doc("unknown", "x", &json!(1)).unwrap();
    store.compact().unwrap();
    let expected = pin(&store).expectations(&profile(), &limits()).unwrap();
    let before = fs::read(&path).unwrap();
    assert!(verify_existing_copy(&path, &expected, &profile(), &limits()).is_err());
    assert_eq!(before, fs::read(&path).unwrap());
}
#[cfg(target_os = "linux")]
#[test]
fn manifest_binds_report_members_and_models_unpublished_status() {
    use fl_store::recovery::manifest::*;
    use sha2::{Digest, Sha256};
    let tmp = private_dir();
    let store = FerroStore::open(tmp.path().join("source.fls")).unwrap();
    let p = ValidationProfile::new(ProfileKind::SmallEmpty, "fixture-empty");
    let expected = pin(&store).expectations(&p, &limits()).unwrap();
    let path = tmp.path().join("image.fls");
    image(&store, &path);
    let report = verify_existing_copy(&path, &expected, &p, &limits()).unwrap();
    let members = [
        (
            MemberKind::ValidationExpectations,
            "validation.json",
            serde_json::to_vec(&expected).unwrap(),
        ),
        (
            MemberKind::VerificationReport,
            "report.json",
            serde_json::to_vec(&report).unwrap(),
        ),
    ]
    .into_iter()
    .map(|(kind, name, bytes)| MemberChecksum {
        kind,
        relative_name: name.into(),
        bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    })
    .collect();
    let context = ManifestContext {
        batch_id: None,
        source_map_sha256: "a".repeat(64),
        creator_build_identity: BuildIdentity {
            package_version: "fixture-build".into(),
            source_revision: Availability::Available("fixture-source".into()),
            executable_sha256: Availability::Unavailable,
        },
        retained_members: members,
        code_index_configuration: Availability::Unavailable,
    };
    let manifest =
        BundleManifestV1::verified_unpublished(&expected, &p, &report, context.clone(), &limits())
            .unwrap();
    assert_eq!(manifest.local_status, LocalPublicationStatus::NotPublished);
    assert_eq!(
        manifest.independent_copy_status,
        IndependentCopyStatus::Verified
    );
    assert!(manifest.published_at_unix_ms.is_none());
    assert_eq!(manifest.image_bytes, fs::metadata(&path).unwrap().len());
    assert_eq!(manifest.intrinsic_lineage, Availability::Unavailable);
    assert_eq!(
        manifest.verifier_build_identity.source_revision,
        Availability::Unavailable
    );
    assert_eq!(
        manifest,
        serde_json::from_slice(&serde_json::to_vec(&manifest).unwrap()).unwrap()
    );
    let mut corrupt = report;
    corrupt.image_sha256 = "bad".into();
    assert!(
        BundleManifestV1::verified_unpublished(&expected, &p, &corrupt, context, &limits())
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn filesystem_events_prove_wrapper_one_image_and_existing_zero() {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let tmp = private_dir();
    let store = FerroStore::open(tmp.path().join("source.fls")).unwrap();
    let expected = pin(&store).expectations(&profile(), &limits()).unwrap();
    let source = tmp.path().join("image.fls");
    image(&store, &source);
    let original = fs::read(&source).unwrap();
    // Observe actual creates, including temporary files deleted before return.
    // SAFETY: inotify returns an owned descriptor, and paths are NUL terminated.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    assert!(fd >= 0);
    let monitor = unsafe { std::fs::File::from_raw_fd(fd) };
    let name = std::ffi::CString::new(tmp.path().as_os_str().as_bytes()).unwrap();
    assert!(
        unsafe { libc::inotify_add_watch(monitor.as_raw_fd(), name.as_ptr(), libc::IN_CREATE) }
            >= 0
    );
    let drain = || {
        let mut buf = [0u8; 8192];
        let n = unsafe { libc::read(monitor.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock
            );
            return 0;
        }
        let mut count = 0;
        let mut offset = 0;
        while offset < n as usize {
            let event = unsafe {
                std::ptr::read_unaligned(buf.as_ptr().add(offset).cast::<libc::inotify_event>())
            };
            assert_eq!(event.mask & libc::IN_Q_OVERFLOW, 0);
            if event.mask & libc::IN_CREATE != 0 {
                count += 1;
            }
            offset += std::mem::size_of::<libc::inotify_event>() + event.len as usize;
        }
        count
    };
    let copy = tmp.path().join("copy.fls");
    copy_and_verify(&source, &copy, &expected, &profile(), &limits()).unwrap();
    assert_eq!(drain(), 1);
    verify_existing_copy(&copy, &expected, &profile(), &limits()).unwrap();
    assert_eq!(drain(), 0);
    assert_eq!(original, fs::read(&source).unwrap());
    assert_eq!(original, fs::read(&copy).unwrap());
}
#[cfg(target_os = "linux")]
#[test]
fn pin_expectations_and_reader_do_not_inherit_source_adapters() {
    use fl_store::ferrostore::{SyncAdapter, SyncBatch, SyncMode};
    struct Trap;
    impl SyncAdapter for Trap {
        fn name(&self) -> &str {
            "inert-fixture"
        }
        fn on_commit(&self, _: &SyncBatch) -> Result<(), fl_core::StoreError> {
            panic!("outbound effect")
        }
        fn fetch_document(&self, _: &str, _: &str) -> Result<Option<Value>, fl_core::StoreError> {
            panic!("live fallback")
        }
    }
    let tmp = private_dir();
    let store = FerroStore::open(tmp.path().join("source.fls")).unwrap();
    store.put_doc("unknown", "x", &json!(1)).unwrap();
    store
        .attach_sync_adapter(SyncMode::LocalCache, std::sync::Arc::new(Trap))
        .unwrap();
    let captured = pin(&store);
    let expected = captured.expectations(&profile(), &limits()).unwrap();
    let path = tmp.path().join("image.fls");
    image(&store, &path);
    let report = verify_existing_copy(&path, &expected, &profile(), &limits()).unwrap();
    assert!(!report.durable_rpc_receipts_claimed);
    assert!(!report.transient_rpc_dedupe_persisted);
}

#[cfg(target_os = "linux")]
#[test]
fn source_identity_collision_conservatively_requests_a_new_operation() {
    let tmp = private_dir();
    let store = FerroStore::open(tmp.path().join("source.fls")).unwrap();
    let expected = pin(&store).expectations(&profile(), &limits()).unwrap();
    let path = tmp.path().join("image.fls");
    image(&store, &path);
    let meta = fs::metadata(&path).unwrap();
    let mut value = serde_json::to_value(&expected).unwrap();
    // Simulate a recycled source inode; it is exclusion evidence only.
    value["source_file_identity"] = json!([meta.dev(), meta.ino()]);
    let expected = serde_json::from_value(value).unwrap();
    let error = verify_existing_copy(&path, &expected, &profile(), &limits()).unwrap_err();
    assert!(matches!(
        error,
        fl_store::recovery::RecoveryError::RefusedInput { .. }
    ));
    assert!(error.to_string().contains("new capture"));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn private_verifier_refuses_platforms_without_native_evidence() {
    let tmp = private_dir();
    let path = tmp.path().join("missing.fls");
    assert!(matches!(
        open_recovery_image_existing(&path),
        Err(fl_store::recovery::RecoveryError::Unsupported { .. })
    ));
    assert!(!path.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn council_report_redacts_private_entry_payload_and_identifiers() {
    use fl_core::{
        At,
        state::{
            MergeMode, RuntimeEntry, SensitivityPatch, StatePayload, StateScope, UpsertStateArgs,
        },
        store::StateStore,
    };
    let tmp = private_dir();
    let store = FerroStore::open(tmp.path().join("source.fls")).unwrap();
    let entry = store
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Project("private-council".into()),
            slug: "private-slug".into(),
            sensitivity: SensitivityPatch::SetSensitive,
            payload: StatePayload::Runtime(RuntimeEntry {
                value: json!({"private-payload":"secret-value"}),
                merge_mode: MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(1),
            by: "private-author".into(),
        })
        .unwrap();
    let p = ValidationProfile::new(ProfileKind::CouncilPrivacyRevisions, "private-catalog");
    let expected = pin(&store).expectations(&p, &limits()).unwrap();
    let path = tmp.path().join("image.fls");
    image(&store, &path);
    let report = verify_existing_copy(&path, &expected, &p, &limits()).unwrap();
    let diagnostic = format!("{report:?} {}", serde_json::to_string(&report).unwrap());
    for secret in [
        "private-council",
        "private-slug",
        "private-payload",
        "secret-value",
        "private-author",
        "private-catalog",
        entry.id.as_str(),
        entry.head_revision.as_str(),
    ] {
        assert!(!diagnostic.contains(secret));
    }
    assert!(report.opaque_runtime_references);
    assert_eq!(
        report.readiness,
        fl_store::recovery::manifest::Readiness::UnknownDomainCoverage
    );
}

#[cfg(target_os = "linux")]
#[test]
fn r1_sync_cursor_requires_exact_external_binding_and_reconciliation() {
    use fl_core::{state::StateSyncCursor, store::StateStore};
    use fl_store::recovery::{
        manifest::Readiness,
        profiles::{DependencyPolicy, ReferenceDependency},
        reference_digest,
    };
    for commit in [Some("fixture-commit".to_string()), None] {
        let tmp = private_dir();
        let store = FerroStore::open(tmp.path().join("cursor.fls")).unwrap();
        let cursor = StateSyncCursor {
            remote: "fixture-remote".into(),
            branch: "fixture-branch".into(),
            last_pushed_commit: commit.clone(),
            last_revision_seq: 17,
            last_registry_seq: 19,
            last_wire_msg_seq: 23,
            last_wire_rcpt_seq: 29,
        };
        store.set_sync_cursor(cursor.clone()).unwrap();
        let captured = pin(&store);
        assert!(
            captured.expectations(&profile(), &limits()).is_err(),
            "cursor-only store must not claim fully verified local contents"
        );
        let bound = |remote: &str, branch: &str, commit: &Option<String>| {
            let mut p = profile();
            p.dependencies.push(ReferenceDependency {
                reference_sha256: reference_digest(
                    "state_sync_cursors",
                    "fixture-remote",
                    "",
                    "external_state_sync_cursor",
                    &serde_json::to_string(&json!([remote, branch, commit])).unwrap(),
                    &limits(),
                )
                .unwrap(),
                catalog_identity: "fixture-repository".into(),
                required_sequence: None,
                policy: DependencyPolicy::ReconcileBeforeAdmission,
            });
            p
        };
        for wrong in [
            bound("other-remote", "fixture-branch", &commit),
            bound("fixture-remote", "other-branch", &commit),
            bound(
                "fixture-remote",
                "fixture-branch",
                &Some("other-commit".into()),
            ),
        ] {
            assert!(captured.expectations(&wrong, &limits()).is_err());
        }
        let p = bound("fixture-remote", "fixture-branch", &commit);
        let expected = captured.expectations(&p, &limits()).unwrap();
        image(&store, &tmp.path().join("copy.fls"));
        let report =
            verify_existing_copy(&tmp.path().join("copy.fls"), &expected, &p, &limits()).unwrap();
        assert_eq!(report.dependency_count, 1);
        assert_eq!(
            report.readiness,
            Readiness::DependenciesRequireReconciliation
        );
        let mut changed = cursor;
        changed.last_wire_rcpt_seq += 1;
        let other = FerroStore::open(tmp.path().join("other.fls")).unwrap();
        other.set_sync_cursor(changed).unwrap();
        assert_eq!(captured.sequence(), pin(&other).sequence());
        image(&other, &tmp.path().join("changed.fls"));
        assert!(
            verify_existing_copy(&tmp.path().join("changed.fls"), &expected, &p, &limits())
                .is_err()
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn r1_query_work_budget_is_cumulative_across_both_verification_opens() {
    use fl_store::recovery::RecoveryError;
    let tmp = private_dir();
    let store = FerroStore::open(tmp.path().join("work.fls")).unwrap();
    store
        .put_doc("fixture", "id", &json!({"body":"fixture"}))
        .unwrap();
    let captured = pin(&store);
    let p = profile();
    let expected = captured.expectations(&p, &limits()).unwrap();
    image(&store, &tmp.path().join("copy.fls"));
    // Find the minimum admitted input for one real pass. This black-box boundary
    // avoids coupling the test to the number or serialization size of probes.
    let (mut low, mut high) = (0, 1_000_000);
    while low < high {
        let middle = low + (high - low) / 2;
        let mut l = limits();
        l.query_work_limit = middle;
        match captured.expectations(&p, &l) {
            Ok(_) => high = middle,
            Err(RecoveryError::Quota {
                resource: "query work",
                ..
            }) => low = middle + 1,
            Err(e) => panic!("unexpected admission error: {e}"),
        }
    }
    assert!(low > 0 && low < 1_000_000);
    let mut one_pass = limits();
    one_pass.query_work_limit = low;
    assert!(matches!(
        verify_existing_copy(&tmp.path().join("copy.fls"), &expected, &p, &one_pass),
        Err(RecoveryError::Quota {
            resource: "query work",
            ..
        })
    ));
    let mut two_passes = limits();
    two_passes.query_work_limit = low * 2;
    assert!(verify_existing_copy(&tmp.path().join("copy.fls"), &expected, &p, &two_passes).is_ok());
}
