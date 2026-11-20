//! Isolated requested-heap measurement: no production allocator or live inputs.
use fl_store::ferrostore::{
    ColumnDef, ColumnType, ColumnValue, FerroStore, VectorMetric, VectorStorage,
};
use fl_store::recovery::{
    CancelToken, CaptureBudget, OperationLimits, ProfileKind, ValidationProfile, copy_and_verify,
    verify_existing_copy,
};
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[path = "../src/recovery/test_support.rs"]
mod test_support;
struct Meter;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn added(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::SeqCst) + bytes;
    PEAK.fetch_max(live, Ordering::SeqCst);
}
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            added(layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            added(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, size) };
        if !ptr.is_null() {
            // Charge both sides briefly: realloc may allocate before freeing.
            added(size);
            LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        }
        ptr
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;

#[test]
fn mixed_eighty_mib_capture_fits_one_gib() {
    let Ok(shape) = std::env::var("FL_CAPTURE_MEMORY_CHILD") else {
        // Both fixed-size fixtures are ~80MiB: one stresses vector payloads,
        // the other graph node count (the old N² reservation exceeds 16GiB).
        for shape in ["wide", "many"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "mixed_eighty_mib_capture_fits_one_gib",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("FL_CAPTURE_MEMORY_CHILD", shape)
                .env_remove("AGENT_PERSONA")
                .env_remove("AGENT_NAME")
                .env_remove("FL_HOSTED_ID")
                .env_remove("AGENT_HYDRATION_GENERATION")
                .spawn()
                .unwrap();
            // Aggregate watchdog covers fixture setup plus five separately
            // bounded phases, each with a 1GiB memory ceiling.
            let deadline = Instant::now() + Duration::from_secs(1200);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "isolated {shape} measurement failed");
                    break;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("isolated five-phase measurement exceeded 600 seconds");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        return;
    };
    let (vectors, dimensions) = match shape.as_str() {
        "wide" => (128, 16384),
        "many" => (1024, 2048),
        _ => panic!("unknown fixture shape"),
    };
    let tmp = test_support::fixture_root();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = tmp.path().join("mixed.fls");
    let store = FerroStore::open(&path).unwrap();
    store
        .transaction(|txn| {
            for n in 0..1024 {
                txn.put_doc(
                    "documents",
                    &n.to_string(),
                    &json!({
                        "body": "d".repeat(40 * 1024), "ordinal": n,
                        "labels": ["fixture", "capture", "mixed"],
                        "nested": {"enabled": true, "score": n as f64 / 7.0}
                    }),
                )?;
            }
            Ok(())
        })
        .unwrap();
    store
        .create_column_table(
            "rows",
            &[
                ColumnDef {
                    name: "index".into(),
                    ty: ColumnType::Int64,
                },
                ColumnDef {
                    name: "payload".into(),
                    ty: ColumnType::Bytes,
                },
            ],
        )
        .unwrap();
    for group in 0..32 {
        let rows: Vec<_> = (0..128)
            .map(|row| {
                vec![
                    ColumnValue::Int64(group * 128 + row),
                    ColumnValue::Bytes(
                        (0..8192)
                            .map(|byte| ((byte * 17 + row * 31 + group) % 251) as u8)
                            .collect(),
                    ),
                ]
            })
            .collect();
        store.append_column_rows("rows", &rows).unwrap();
    }
    store
        .create_vector_collection(
            "vectors",
            dimensions,
            VectorStorage::F32,
            VectorMetric::L2,
            4,
            8,
            8,
        )
        .unwrap();
    for n in 0..vectors {
        let values: Vec<_> = (0..dimensions)
            .map(|x| ((x + n) % 257) as f32 / 257.0)
            .collect();
        store
            .upsert_vector(
                "vectors",
                &format!("v{n:04}"),
                &values,
                Some(&json!({"ordinal": n, "labels": ["vector", "fixture"]})),
            )
            .unwrap();
    }
    store.compact().unwrap();
    for n in 0..8 {
        store
            .upsert_vector(
                "vectors",
                &format!("v{n:04}"),
                &vec![n as f32; dimensions as usize],
                Some(&json!({"wal": true, "ordinal": n})),
            )
            .unwrap();
    }
    let file_bytes = std::fs::metadata(&path).unwrap().len();
    assert!(
        (80 << 20..84 << 20).contains(&file_bytes),
        "fixture bytes: {file_bytes}"
    );
    // One doc transaction, two collection creates, 32 appends, vector upserts.
    let sequence = 1 + 2 + 32 + u64::from(vectors) + 8;
    let budget = CaptureBudget::new(
        Instant::now() + Duration::from_secs(120),
        1 << 30,
        1 << 30,
        Duration::from_secs(1),
    );
    let cancel = CancelToken::new();
    // Baseline includes fixture construction and the owner's existing cache.
    // One child, one test thread, no concurrent writers. Only capture allocations
    // contribute to peak minus baseline; keep the immutable pin alive at reading.
    let baseline = LIVE.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);
    let capture = store.capture_recovery(&budget, &cancel).unwrap();
    let peak = PEAK.load(Ordering::SeqCst) - baseline;
    assert_eq!(capture.sequence(), sequence);
    let charged = capture.preparation_bytes();
    assert!(
        charged >= peak as u64,
        "charged {charged} < measured peak {peak}"
    );
    assert!(charged <= 1 << 30);
    println!(
        "capture_memory shape={shape} vectors={vectors} dimensions={dimensions} M=4 wal_upserts=8 file_bytes={file_bytes} baseline={baseline} peak_extra={peak} charged={charged} model_safety_factor=2"
    );
    println!(
        "retained_memory shape={shape} bytes={}",
        capture.retained_bytes()
    );
    // Each phase receives its own explicit 1 GiB limit. The source cache and
    // immutable pin remain alive throughout, and are included in each baseline.
    let limits = || {
        OperationLimits::new(
            Instant::now() + Duration::from_secs(120),
            1 << 30,
            1 << 30,
            CancelToken::new(),
        )
    };
    let image = tmp.path().join("image.fls");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&image)
        .unwrap();
    let pipeline_baseline = baseline;
    let mut pipeline_peak = baseline + peak;
    let mut measured = |phase: &str, f: &mut dyn FnMut(&OperationLimits)| {
        let l = limits();
        let baseline = LIVE.load(Ordering::SeqCst);
        PEAK.store(baseline, Ordering::SeqCst);
        f(&l);
        let absolute_peak = PEAK.load(Ordering::SeqCst);
        pipeline_peak = pipeline_peak.max(absolute_peak);
        let peak = absolute_peak.saturating_sub(baseline);
        let charged = l.admitted_memory_bytes();
        assert!(
            charged >= 2 * peak as u64,
            "{phase}: charge {charged} < 2x measured {peak}"
        );
        assert!(charged <= 1 << 30);
        println!(
            "recovery_memory shape={shape} phase={phase} baseline={baseline} peak_extra={peak} charged={charged} model_safety_factor=2 budget={}",
            1u64 << 30
        );
        assert!(peak <= 1 << 30);
    };
    measured("write_image", &mut |l| {
        capture.write_image(&mut file, l).unwrap();
    });
    let inventory = fl_store::recovery::collect_inventory(&fl_store::recovery::InventoryInputs {
        roots: vec![tmp.path().to_path_buf()],
        bindings: vec![fl_store::recovery::BindingInput::config(
            "memory",
            path.clone(),
            Some("ferro".into()),
            "memory fixture",
        )],
        ..Default::default()
    })
    .unwrap();
    let identity = inventory
        .stores
        .iter()
        .find(|s| s.identity.canonical_path.as_deref() == Some(&path))
        .unwrap()
        .identity
        .clone();
    let profile = ValidationProfile::new(
        ProfileKind::MixedTablesVectors,
        identity.catalog_key.clone(),
    );
    let mut expected = None;
    measured("expectations", &mut |l| {
        expected = Some(capture.expectations(&profile, l).unwrap());
    });
    let expected = expected.unwrap();
    let mut profile_hash = None;
    if cfg!(target_os = "linux") {
        let copy = tmp.path().join("copy.fls");
        measured("copy_and_verify", &mut |l| {
            copy_and_verify(&image, &copy, &expected, &profile, l).unwrap();
        });
        measured("verify_existing", &mut |l| {
            profile_hash = Some(
                verify_existing_copy(&copy, &expected, &profile, l)
                    .unwrap()
                    .profile_sha256,
            );
        });
    }
    println!(
        "recovery_pipeline shape={shape} fixture_baseline={pipeline_baseline} peak_extra={} absolute_peak={pipeline_peak} source_cache_and_pin_retained=true",
        pipeline_peak - pipeline_baseline
    );
    assert!(pipeline_peak - pipeline_baseline <= 1 << 30);
    // The offline attestation excludes cached owners. Drop the fixture writer,
    // pin and expectations before measuring the actual cold offline operations;
    // their operation-created live objects all remain inside each measured peak.
    drop(expected);
    drop(capture);
    drop(store);
    drop(file);
    #[cfg(target_os = "linux")]
    offline_source_and_original_memory(
        &tmp,
        &path,
        identity,
        profile,
        profile_hash.unwrap(),
        shape.as_str(),
    );
}
#[cfg(target_os = "linux")]
fn offline_source_and_original_memory(
    tmp: &tempfile::TempDir,
    path: &std::path::Path,
    identity: fl_store::recovery::StoreIdentity,
    profile: ValidationProfile,
    profile_sha256: String,
    shape: &str,
) {
    use fl_store::recovery::{
        admission::*,
        backup::*,
        fs::{NativeRecoveryFs, RecoveryFs},
        manifest::{Availability, BuildIdentity},
        restore::*,
    };
    use std::os::unix::fs::MetadataExt;
    let root = tmp.path().join("offline-backups");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Linux CI's wide cold backup exhausted its 120s deadline after earlier
    // phases took ~185s; charge was 846676376 bytes, below the 1GiB ceiling.
    // Give this fixture 3x that observed deadline, keeping a finite bound.
    let new_limits = || {
        OperationLimits::new(
            Instant::now() + Duration::from_secs(360),
            1 << 30,
            1 << 30,
            CancelToken::new(),
        )
    };
    let filesystem = NativeRecoveryFs::new(root.clone(), new_limits());
    let lease = |action, op, hash: String| {
        let id = |p: &std::path::Path| {
            let m = std::fs::metadata(p).unwrap();
            (m.dev(), m.ino())
        };
        let now = filesystem.io().unix_ms().unwrap();
        create_offline_lease(
            &OfflineLeaseRequest {
                version: 1,
                operation_id: op,
                action,
                catalog_identity: identity.catalog_key.clone(),
                approved_root_identity: id(&root),
                physical_path: path.to_path_buf(),
                parent_identity: id(path.parent().unwrap()),
                expected_file: inspect_file_binding(path, &new_limits()).unwrap(),
                profile_sha256: hash,
                build: BuildIdentity {
                    package_version: env!("CARGO_PKG_VERSION").into(),
                    source_revision: Availability::Unavailable,
                    executable_sha256: Availability::Unavailable,
                },
                issued_at_unix_ms: now,
                expires_at_unix_ms: now + 600000,
                operator: "memory fixture operator".into(),
                exclusion: ExclusionAttestation {
                    legacy_and_cached_handles_excluded: true,
                    external_dispatch_excluded: true,
                    active_owner_excluded: true,
                    evidence: "fixture cached writer and captured pin dropped; no dispatch".into(),
                },
            },
            &filesystem,
            &new_limits(),
        )
        .unwrap()
    };
    let measure = |phase: &str, l: &OperationLimits, action: &mut dyn FnMut()| {
        let baseline = LIVE.load(Ordering::SeqCst);
        PEAK.store(baseline, Ordering::SeqCst);
        action();
        let absolute = PEAK.load(Ordering::SeqCst);
        let extra = absolute.saturating_sub(baseline);
        let charge = l.admitted_memory_bytes();
        println!(
            "offline_memory shape={shape} phase={phase} baseline={baseline} peak_extra={extra} charged={charge} budget={} cached_fixture_owner_retained=false",
            1u64 << 30
        );
        assert!(
            charge >= 2 * extra as u64,
            "{phase}: charge {charge} < 2x measured {extra}"
        );
        assert!(charge <= 1 << 30);
        assert!(extra <= 1 << 30);
    };
    let l = new_limits();
    let request = BackupRequest {
        operation_id: l.operation_id,
        store: identity.clone(),
        source_map_sha256: "a".repeat(64),
        profile,
        batch_id: None,
        destination: root.clone(),
        capture: CaptureBudget::new(l.deadline, 1 << 30, 1 << 30, Duration::from_secs(1)),
        limits: l.clone(),
        image_byte_limit: None,
        metadata_byte_limit: 1 << 20,
        space_margin_bytes: 4096,
        configs: vec![],
        excluded_vault_key: None,
        code_index_configuration: Availability::Unavailable,
    };
    let backup_lease = lease(
        OfflineAction::Backup,
        request.operation_id,
        profile_sha256.clone(),
    );
    let mut bundle = None;
    measure("bounded_cold_offline_backup", &l, &mut || {
        let outcome =
            backup_offline(&request, &backup_lease, &filesystem, &CancelToken::new()).unwrap();
        let PublicationOutcome::Durable(selected) = outcome else {
            panic!("offline publication unknown")
        };
        bundle = Some(selected);
    });
    let bundle = bundle.unwrap();
    use sha2::{Digest, Sha256};
    let digest = format!(
        "{:x}",
        Sha256::digest(std::fs::read(bundle.path().join("manifest.json")).unwrap())
    );
    let bundle_op =
        uuid::Uuid::parse_str(bundle.path().file_name().unwrap().to_str().unwrap()).unwrap();
    let l = new_limits();
    let restore_lease = lease(
        OfflineAction::Restore {
            bundle_operation: bundle_op,
            bundle_sha256: digest,
        },
        l.operation_id,
        profile_sha256,
    );
    let request = RestoreRequest {
        operation_id: l.operation_id,
        store: identity,
        destination: path.to_path_buf(),
        staging_parent: tmp.path().to_path_buf(),
        selected_bundle: bundle,
        limits: l.clone(),
        metadata_byte_limit: 1 << 20,
        space_margin_bytes: 4096,
        original_profile: request.profile.clone(),
        available_dependencies: vec![],
    };
    measure("bounded_physical_original_and_candidate", &l, &mut || {
        let outcome = restore_offline(&request, &restore_lease, &filesystem).unwrap();
        let RestoreOutcome::Installed { journal } = outcome else {
            panic!("restore outcome unknown")
        };
        assert!(journal.original_verification.is_some());
        assert_eq!(journal.budget.rollback_new_bytes, 0);
        assert!(
            journal.budget.candidate_bytes + journal.budget.metadata_bytes
                <= 2 * journal.budget.baseline_bytes
        );
    });
}
