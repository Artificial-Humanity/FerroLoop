//! Publication exercises only fixture roots; never reads user config or live stores.
//! Set FL_RECOVERY_TEST_DIR to a writable native ext4/XFS directory. The default
//! uses the platform temp directory, but unsupported filesystems fail clearly;
//! they never count as passing a refusal assertion. No test silently skips.
#![cfg(target_os = "linux")]
use fl_store::recovery::RecoveryError;
use fl_store::{
    FerroStore,
    recovery::{
        CancelToken, CaptureBudget, OperationLimits, ProfileKind, ValidationProfile,
        backup::{BackupRequest, PublicationOutcome, backup, reconcile_publication},
        fs::{Directory, Member, NativeRecoveryFs, RecoveryEvent, RecoveryFs, RecoveryIo},
        inventory::{BindingInput, InventoryInputs, collect_inventory},
    },
};
use serde_json::json;
use std::{
    fs, io,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn reconciliation_limits() -> OperationLimits {
    OperationLimits::new(
        Instant::now() + Duration::from_secs(30),
        64 << 20,
        256 << 20,
        CancelToken::new(),
    )
}

struct Fixture {
    _root: tempfile::TempDir,
    source: PathBuf,
    destination: PathBuf,
    store: FerroStore,
}
impl Fixture {
    fn new() -> Self {
        let test_root = std::env::var_os("FL_RECOVERY_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let root = tempfile::tempdir_in(&test_root)
            .expect("FL_RECOVERY_TEST_DIR must be a writable native ext4/XFS directory");
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        let path = std::ffi::CString::new(root.path().as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) }, 0);
        assert!(
            matches!(
                unsafe { stat.assume_init() }.f_type as u64,
                0xef53 | 0x58465342
            ),
            "recovery_backup requires native ext4/XFS: set FL_RECOVERY_TEST_DIR; unsupported filesystem is not a refusal-test pass"
        );
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let source = root.path().join("source.fls");
        let destination = root.path().join("backups");
        fs::create_dir(&destination).unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();
        let store = FerroStore::open(&source).unwrap();
        // A real append history leaves enough physical budget for two compact
        // recovery images plus metadata. No source padding or backup exemption.
        for n in 0..24 {
            store
                .put_doc(
                    "fixture_unknown",
                    "one",
                    &json!({"n":n,"body":"x".repeat(4096)}),
                )
                .unwrap();
        }
        Self {
            _root: root,
            source,
            destination,
            store,
        }
    }
    fn request(&self) -> BackupRequest {
        fixture_request(self._root.path(), &self.source, &self.destination)
    }
    fn assert_source_usable(&self) {
        self.store
            .put_doc("fixture_unknown", "after", &json!({"ok":true}))
            .unwrap();
        assert_eq!(
            self.store.get_doc("fixture_unknown", "after").unwrap(),
            Some(json!({"ok":true}))
        );
    }
}

fn fixture_request(root: &Path, source: &Path, destination: &Path) -> BackupRequest {
    let inventory = collect_inventory(&InventoryInputs {
        roots: vec![root.to_path_buf()],
        bindings: vec![BindingInput::config(
            "fixture",
            source.to_path_buf(),
            Some("ferro".into()),
            "fixture",
        )],
        ..Default::default()
    })
    .unwrap();
    let identity = inventory
        .stores
        .iter()
        .find(|r| r.identity.canonical_path.as_deref() == Some(source))
        .unwrap()
        .identity
        .clone();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut limits = OperationLimits::new(deadline, 64 << 20, 256 << 20, CancelToken::new());
    limits.operation_id = uuid::Uuid::now_v7();
    let profile = ValidationProfile::new(ProfileKind::SmallEmpty, identity.catalog_key.clone());
    BackupRequest {
        store: identity,
        source_map_sha256: "a".repeat(64),
        operation_id: limits.operation_id,
        batch_id: Some(uuid::Uuid::now_v7()),
        destination: destination.to_path_buf(),
        capture: CaptureBudget::new(deadline, 64 << 20, 128 << 20, Duration::from_secs(1)),
        limits,
        profile,
        image_byte_limit: Some(16 << 10),
        space_margin_bytes: 4096,
        metadata_byte_limit: 128 << 10,
        configs: vec![],
        excluded_vault_key: None,
        code_index_configuration: fl_store::recovery::manifest::Availability::Unavailable,
    }
}

#[derive(Default)]
struct Fault {
    failure: Mutex<Option<RecoveryEvent>>,
    seen: Mutex<Vec<RecoveryEvent>>,
}
impl RecoveryIo for Fault {
    fn before(&self, event: RecoveryEvent) -> io::Result<()> {
        self.seen.lock().unwrap().push(event);
        if self.failure.lock().unwrap().as_ref() == Some(&event) {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(())
    }
}
struct FaultFs {
    root: PathBuf,
    io: Arc<dyn RecoveryIo>,
    limits: OperationLimits,
}
impl RecoveryFs for FaultFs {
    fn root(&self) -> &Path {
        &self.root
    }
    fn io(&self) -> Arc<dyn RecoveryIo> {
        self.io.clone()
    }
    fn reconciliation_limits(&self) -> &OperationLimits {
        &self.limits
    }
}

// A missing parent sync must prevent both a Durable value and catalog selection.
#[test]
fn parent_sync_failure_is_unknown_and_reconciliation_repairs_catalog() {
    let f = Fixture::new();
    let mut request = f.request();
    request.profile.catalog_identity = request.store.catalog_key.clone();
    let faults = Arc::new(Fault::default());
    *faults.failure.lock().unwrap() = Some(RecoveryEvent::Sync(Directory::PublicationParent));
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: faults.clone(),
        limits: reconciliation_limits(),
    };
    let outcome = backup(&f.store, &request, &filesystem, &CancelToken::new()).unwrap();
    assert!(matches!(outcome, PublicationOutcome::OutcomeUnknown { .. }));
    f.assert_source_usable();
    assert!(
        !f.destination
            .join(".catalog")
            .join(format!("{}.json", request.operation_id))
            .exists()
    );
    *faults.failure.lock().unwrap() = None;
    let recovered = reconcile_publication(&request.operation_id.to_string(), &filesystem).unwrap();
    assert!(matches!(recovered, PublicationOutcome::Durable(_)));
    assert!(
        f.destination
            .join(".catalog")
            .join(format!("{}.json", request.operation_id))
            .is_file()
    );
}

// Skipping any flush or treating its error as success would publish a partial or
// unacknowledged bundle; the independent verifier's own copy flush is included.
#[test]
fn prepublication_flush_failures_never_create_final_names_or_poison_source() {
    for event in [
        RecoveryEvent::Sync(Directory::Initialization),
        RecoveryEvent::Flush(Member::Image),
        RecoveryEvent::Flush(Member::Validation),
        RecoveryEvent::Flush(Member::Profile),
        RecoveryEvent::Flush(Member::VerificationCopy),
        RecoveryEvent::Flush(Member::Report),
        RecoveryEvent::Flush(Member::Manifest),
        RecoveryEvent::Sync(Directory::StagingBeforeVerify),
        RecoveryEvent::Sync(Directory::StagingBeforePublish),
    ] {
        let f = Fixture::new();
        let native = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
        let PublicationOutcome::Durable(old) =
            backup(&f.store, &f.request(), &native, &CancelToken::new()).unwrap()
        else {
            panic!("old backup")
        };
        let old_bytes = fs::read(old.path().join("image.fls")).unwrap();
        let mut request = f.request();
        request.profile.catalog_identity = request.store.catalog_key.clone();
        let faults = Arc::new(Fault::default());
        *faults.failure.lock().unwrap() = Some(event);
        let filesystem = FaultFs {
            root: f.destination.clone(),
            io: faults.clone(),
            limits: reconciliation_limits(),
        };
        assert!(
            matches!(backup(&f.store, &request, &filesystem, &CancelToken::new()),
                Err(fl_store::recovery::RecoveryError::Destination { operation_id })
                    if operation_id == request.operation_id),
            "{event:?}"
        );
        assert!(
            !f.destination
                .join(request.operation_id.to_string())
                .exists(),
            "{event:?}"
        );
        assert!(
            faults.seen.lock().unwrap().contains(&event),
            "fault not reached: {event:?}"
        );
        assert_eq!(old_bytes, fs::read(old.path().join("image.fls")).unwrap());
        f.assert_source_usable();
    }
}

#[test]
fn nofollow_destination_escape_refuses_without_capture() {
    let f = Fixture::new();
    let alias = f._root.path().join("alias");
    symlink(&f.destination, &alias).unwrap();
    let mut request = f.request();
    request.destination = alias.clone();
    let faults = Arc::new(Fault::default());
    let filesystem = FaultFs {
        root: alias,
        io: faults.clone(),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &CancelToken::new()),
        Err(RecoveryError::Verification { .. })
    ));
    assert!(
        !faults
            .seen
            .lock()
            .unwrap()
            .contains(&RecoveryEvent::Capture)
    );
    f.assert_source_usable();
}

#[test]
fn durable_bundle_uses_independent_copy_and_collision_preserves_old_bytes() {
    let f = Fixture::new();
    let mut request = f.request();
    request.profile.catalog_identity = request.store.catalog_key.clone();
    let filesystem = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
    let result = backup(&f.store, &request, &filesystem, &CancelToken::new()).unwrap();
    let PublicationOutcome::Durable(bundle) = result else {
        panic!("publication not durable")
    };
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.path().join("manifest.json")).unwrap()).unwrap();
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.path().join("report.json")).unwrap()).unwrap();
    assert_eq!(manifest["independent_copy_status"], "verified");
    assert_eq!(report["copy_evidence"]["creation"], "wrapper_created");
    assert_ne!(
        report["copy_evidence"]["source_image_identity"],
        report["copy_evidence"]["verified_file_identity"]
    );
    assert!(!bundle.path().join("verification.fls").exists());
    let old = fs::read(bundle.path().join("image.fls")).unwrap();
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &CancelToken::new()),
        Err(RecoveryError::RefusedInput { .. })
    ));
    assert_eq!(fs::read(bundle.path().join("image.fls")).unwrap(), old);
    f.assert_source_usable();
}

// An empty store cannot fit two full images and any metadata within twice its
// physical size. Capture must never run merely to discover that impossibility.
#[test]
fn oversized_image_ceiling_refuses_before_capture() {
    let ceiling = Some(u64::MAX);
    let f = Fixture::new();
    let mut request = f.request();
    request.image_byte_limit = ceiling;
    request.profile.catalog_identity = request.store.catalog_key.clone();
    let faults = Arc::new(Fault::default());
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: faults.clone(),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &CancelToken::new()),
        Err(RecoveryError::Quota { .. })
    ));
    assert!(
        !faults
            .seen
            .lock()
            .unwrap()
            .contains(&RecoveryEvent::Capture)
    );
    f.assert_source_usable();
}

#[test]
fn unknown_or_review_required_alias_and_wrong_configured_root_refuse_before_capture() {
    use fl_store::recovery::inventory::AliasStatus;
    for alias in [AliasStatus::Unknown, AliasStatus::ReviewRequired] {
        let f = Fixture::new();
        let mut request = f.request();
        request.store.alias_status = alias;
        let faults = Arc::new(Fault::default());
        let filesystem = FaultFs {
            root: f.destination.clone(),
            io: faults.clone(),
            limits: reconciliation_limits(),
        };
        assert!(matches!(
            backup(&f.store, &request, &filesystem, &CancelToken::new()),
            Err(RecoveryError::RefusedInput { .. })
        ));
        assert!(
            !faults
                .seen
                .lock()
                .unwrap()
                .contains(&RecoveryEvent::Capture)
        );
    }
    let f = Fixture::new();
    let request = f.request();
    let faults = Arc::new(Fault::default());
    let filesystem = FaultFs {
        root: f._root.path().to_path_buf(),
        io: faults.clone(),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &CancelToken::new()),
        Err(RecoveryError::RefusedInput { .. })
    ));
    assert!(
        !faults
            .seen
            .lock()
            .unwrap()
            .contains(&RecoveryEvent::Capture)
    );
}

#[test]
fn small_incompressible_source_refuses_before_capture() {
    let f = Fixture::new();
    let compact_path = f._root.path().join("compact.fls");
    let small = FerroStore::open(&compact_path).unwrap();
    // JSON contains no compressible repeated string; image/header overhead and
    // metadata still need admission even for a tiny valid source.
    small
        .put_doc("opaque", "one", &json!({"value":"0ca87615bf4239de"}))
        .unwrap();
    small.compact().unwrap();
    let mut request = f.request();
    let inventory = collect_inventory(&InventoryInputs {
        roots: vec![f._root.path().to_path_buf()],
        bindings: vec![BindingInput::config(
            "fixture",
            compact_path.clone(),
            Some("ferro".into()),
            "fixture",
        )],
        ..Default::default()
    })
    .unwrap();
    request.store = inventory
        .stores
        .iter()
        .find(|r| r.identity.canonical_path.as_deref() == Some(compact_path.as_path()))
        .unwrap()
        .identity
        .clone();
    request.profile.catalog_identity = request.store.catalog_key.clone();
    request.image_byte_limit = Some(fs::metadata(&compact_path).unwrap().len());
    request.limits.byte_limit = fs::metadata(&compact_path).unwrap().len() * 2;
    let faults = Arc::new(Fault::default());
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: faults.clone(),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        backup(&small, &request, &filesystem, &CancelToken::new()),
        Err(RecoveryError::Quota { .. })
    ));
    assert!(
        !faults
            .seen
            .lock()
            .unwrap()
            .contains(&RecoveryEvent::Capture)
    );
    small.put_doc("opaque", "after", &json!(true)).unwrap();
}

#[test]
fn streaming_ceiling_overrun_has_distinct_error_and_no_final_bundle() {
    let f = Fixture::new();
    let mut request = f.request();
    request.profile.catalog_identity = request.store.catalog_key.clone();
    request.image_byte_limit = Some(1);
    let filesystem = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
    let error = backup(&f.store, &request, &filesystem, &CancelToken::new()).unwrap_err();
    assert!(matches!(
        error,
        fl_store::recovery::RecoveryError::ImageExceedsCeiling { .. }
    ));
    assert!(
        !f.destination
            .join(request.operation_id.to_string())
            .exists()
    );
    assert!(
        !f.destination
            .join(".staging")
            .join(request.operation_id.to_string())
            .exists()
    );
    f.assert_source_usable();
}

#[test]
fn corrupted_final_member_is_quarantined_without_deletion() {
    let f = Fixture::new();
    let mut request = f.request();
    request.profile.catalog_identity = request.store.catalog_key.clone();
    let faults = Arc::new(Fault::default());
    *faults.failure.lock().unwrap() = Some(RecoveryEvent::Sync(Directory::PublicationParent));
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: faults.clone(),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &CancelToken::new()).unwrap(),
        PublicationOutcome::OutcomeUnknown { .. }
    ));
    let final_path = f.destination.join(request.operation_id.to_string());
    fs::write(final_path.join("report.json"), b"corrupt fixture report").unwrap();
    *faults.failure.lock().unwrap() = None;
    assert!(matches!(
        reconcile_publication(&request.operation_id.to_string(), &filesystem),
        Err(RecoveryError::Quarantined { .. })
    ));
    assert!(final_path.join("image.fls").is_file());
    assert!(
        f.destination
            .join(".quarantine")
            .join(request.operation_id.to_string())
            .exists()
    );
    assert!(
        !f.destination
            .join(".catalog")
            .join(format!("{}.json", request.operation_id))
            .exists()
    );
}

#[test]
fn source_hardlink_or_request_for_a_different_owner_refuses_before_capture() {
    let f = Fixture::new();
    let mut request = f.request();
    request.profile.catalog_identity = request.store.catalog_key.clone();
    let other = FerroStore::open(f._root.path().join("other.fls")).unwrap();
    let faults = Arc::new(Fault::default());
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: faults.clone(),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        backup(&other, &request, &filesystem, &CancelToken::new()),
        Err(RecoveryError::RefusedInput { .. })
    ));
    assert!(
        !faults
            .seen
            .lock()
            .unwrap()
            .contains(&RecoveryEvent::Capture)
    );
    fs::hard_link(&f.source, f.destination.join("alias.fls")).unwrap();
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &CancelToken::new()),
        Err(RecoveryError::RefusedInput { .. })
    ));
    assert!(
        !faults
            .seen
            .lock()
            .unwrap()
            .contains(&RecoveryEvent::Capture)
    );
    assert_eq!(
        fs::read(&f.source).unwrap(),
        fs::read(f.destination.join("alias.fls")).unwrap()
    );
}

#[test]
fn cancellation_and_expired_deadline_refuse_before_capture() {
    for cancel_argument in [true, false] {
        let f = Fixture::new();
        let request = f.request();
        let cancel = CancelToken::new();
        if cancel_argument {
            cancel.cancel()
        } else {
            request.limits.cancel.cancel()
        }
        let faults = Arc::new(Fault::default());
        let filesystem = FaultFs {
            root: f.destination.clone(),
            io: faults.clone(),
            limits: reconciliation_limits(),
        };
        assert!(matches!(
            backup(&f.store, &request, &filesystem, &cancel),
            Err(fl_store::recovery::RecoveryError::Cancelled { .. })
        ));
        assert!(
            !faults
                .seen
                .lock()
                .unwrap()
                .contains(&RecoveryEvent::Capture)
        );
        f.assert_source_usable();
    }
    let f = Fixture::new();
    let mut request = f.request();
    request.limits.deadline = Instant::now();
    let faults = Arc::new(Fault::default());
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: faults.clone(),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &CancelToken::new()),
        Err(fl_store::recovery::RecoveryError::Deadline { .. })
    ));
    assert!(
        !faults
            .seen
            .lock()
            .unwrap()
            .contains(&RecoveryEvent::Capture)
    );
}

// The child opens only the explicit fixture authorized by its parent. It never
// invokes a vendor/CLI or inherits a production configuration or FL_DB.
#[test]
fn backup_lock_contender_child() {
    let Some(root) = std::env::var_os("FERRO_RECOVERY_FIXTURE_ROOT") else {
        return;
    };
    assert!(std::env::var_os("FL_DB").is_none());
    let root = PathBuf::from(root);
    let marker = fs::read_to_string(root.join("fixture-authority")).unwrap();
    assert_eq!(
        marker,
        std::env::var("FERRO_RECOVERY_FIXTURE_TOKEN").unwrap()
    );
    let source = root.join("source.fls");
    let destination = root.join("backups");
    let owner = FerroStore::open(&source).unwrap();
    let mut request = fixture_request(&root, &source, &destination);
    request.profile.catalog_identity = request.store.catalog_key.clone();
    let filesystem = NativeRecoveryFs::new(destination, reconciliation_limits());
    assert!(matches!(
        backup(&owner, &request, &filesystem, &CancelToken::new()),
        Err(fl_store::recovery::RecoveryError::ConcurrentBackup { .. })
    ));
}

#[test]
fn physical_store_lock_excludes_a_second_process() {
    use std::os::fd::AsRawFd;
    let f = Fixture::new();
    let request = f.request();
    let locks = f.destination.join(".locks");
    fs::create_dir(&locks).unwrap();
    fs::set_permissions(&locks, fs::Permissions::from_mode(0o700)).unwrap();
    // Hold the public protocol's physical-identity lock, independently of the
    // implementation's process-local state. Child success catches missing flock.
    let identity = request.store.filesystem_identity.unwrap();
    let lock = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .read(true)
        .open(locks.join(format!("{identity}.lock")))
        .unwrap();
    fs::set_permissions(
        locks.join(format!("{identity}.lock")),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let token = uuid::Uuid::now_v7().to_string();
    fs::write(f._root.path().join("fixture-authority"), &token).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "backup_lock_contender_child", "--nocapture"])
        .env_clear()
        .env("FERRO_RECOVERY_FIXTURE_ROOT", f._root.path())
        .env("FERRO_RECOVERY_FIXTURE_TOKEN", token)
        .env_remove("AGENT_PERSONA")
        .env_remove("AGENT_NAME")
        .env_remove("FL_HOSTED_ID")
        .env_remove("AGENT_HYDRATION_GENERATION")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failure: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    f.assert_source_usable();
}

struct Substitute {
    event: RecoveryEvent,
    staging: PathBuf,
    operation: String,
    source: PathBuf,
    directory: bool,
    done: std::sync::atomic::AtomicBool,
}
impl RecoveryIo for Substitute {
    fn before(&self, event: RecoveryEvent) -> io::Result<()> {
        if event == self.event && !self.done.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let stage = self.staging.join(&self.operation);
            if self.directory {
                fs::rename(&stage, self.staging.join("held-original"))?;
                fs::create_dir(&stage)?;
                fs::set_permissions(&stage, fs::Permissions::from_mode(0o700))?;
            } else {
                fs::remove_file(stage.join("image.fls"))?;
                fs::hard_link(&self.source, stage.join("image.fls"))?;
            }
        }
        Ok(())
    }
}
#[test]
fn substitution_at_publish_boundary_never_occupies_a_final_name() {
    for directory in [false, true] {
        let f = Fixture::new();
        let request = f.request();
        let hook = Arc::new(Substitute {
            event: RecoveryEvent::Publish,
            staging: f.destination.join(".staging"),
            operation: request.operation_id.to_string(),
            source: f.source.clone(),
            directory,
            done: Default::default(),
        });
        let filesystem = FaultFs {
            root: f.destination.clone(),
            io: hook.clone(),
            limits: reconciliation_limits(),
        };
        let result = backup(&f.store, &request, &filesystem, &CancelToken::new());
        if directory {
            assert!(
                matches!(result, Err(RecoveryError::Verification { .. })),
                "{result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(RecoveryError::RefusedInput { .. })),
                "{result:?}"
            );
        }
        assert!(hook.done.load(std::sync::atomic::Ordering::Acquire));
        assert!(
            !f.destination
                .join(request.operation_id.to_string())
                .exists()
        );
        assert!(f.source.is_file());
    }
}
#[test]
fn substitution_at_rename_entry_never_occupies_a_final_name() {
    let f = Fixture::new();
    let request = f.request();
    let hook = Arc::new(Substitute {
        event: RecoveryEvent::RenameEntry,
        staging: f.destination.join(".staging"),
        operation: request.operation_id.to_string(),
        source: f.source.clone(),
        directory: true,
        done: Default::default(),
    });
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: hook.clone(),
        limits: reconciliation_limits(),
    };
    let result = backup(&f.store, &request, &filesystem, &CancelToken::new());
    assert!(hook.done.load(std::sync::atomic::Ordering::Acquire));
    assert!(
        !f.destination
            .join(request.operation_id.to_string())
            .exists(),
        "RenameEntry hook published a substituted partial bundle: {result:?}"
    );
    assert!(
        matches!(result, Err(RecoveryError::Verification { .. })),
        "{result:?}"
    );
    f.assert_source_usable();
}
#[test]
fn shared_metadata_quota_cannot_borrow_an_unlimited_verification_budget() {
    let f = Fixture::new();
    let mut request = f.request();
    request.metadata_byte_limit = 1;
    let filesystem = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &CancelToken::new()),
        Err(fl_store::recovery::RecoveryError::Quota { .. })
    ));
    assert!(
        !f.destination
            .join(request.operation_id.to_string())
            .exists()
    );
    f.assert_source_usable();
}
#[test]
fn protected_config_snapshots_allow_only_named_descriptors_and_exclude_key_aliases() {
    use fl_store::recovery::{
        backup::{ApprovedConfig, ExcludedVaultKey},
        manifest::MemberKind,
    };
    use std::os::unix::fs::MetadataExt;
    for key_alias in [0, 1, 2, 3] {
        let f = Fixture::new();
        let mut request = f.request();
        let key = f._root.path().join("vault.key");
        fs::write(&key, b"fixture-only-key").unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        let config = f._root.path().join("config.toml");
        match key_alias {
            0 => {
                fs::write(&config, b"fixture_credentials = 'not-real'\n").unwrap();
                fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
            }
            1 => fs::hard_link(&key, &config).unwrap(),
            2 => symlink(&key, &config).unwrap(),
            _ => {}
        }
        let key_m = fs::metadata(&key).unwrap();
        let path = if key_alias == 3 {
            key.clone()
        } else {
            config.clone()
        };
        let m = fs::metadata(&path).unwrap();
        request.excluded_vault_key = Some(ExcludedVaultKey {
            path: key,
            identity: (key_m.dev(), key_m.ino()),
        });
        request.configs = vec![ApprovedConfig {
            kind: MemberKind::ProtectedConfig,
            path,
            identity: (m.dev(), m.ino()),
            byte_limit: 4096,
        }];
        let filesystem = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
        let result = backup(&f.store, &request, &filesystem, &CancelToken::new());
        if key_alias == 0 {
            let PublicationOutcome::Durable(bundle) = result.unwrap() else {
                panic!("not durable")
            };
            assert_eq!(
                fs::read(bundle.path().join("config.toml")).unwrap(),
                b"fixture_credentials = 'not-real'\n"
            );
            assert_eq!(
                fs::metadata(bundle.path().join("config.toml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                0o600
            );
            assert!(!bundle.path().join("vault.key").exists());
        } else {
            assert!(matches!(result, Err(RecoveryError::RefusedInput { .. })));
            assert!(
                !f.destination
                    .join(request.operation_id.to_string())
                    .exists()
            );
        }
    }
}

struct WriteFault {
    mode: u8,
    reached: std::sync::atomic::AtomicBool,
}
impl RecoveryIo for WriteFault {
    fn write_at(&self, file: &fs::File, bytes: &[u8], offset: u64) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        self.reached
            .store(true, std::sync::atomic::Ordering::Release);
        match self.mode {
            0 => file.write_at(&bytes[..bytes.len().min(13)], offset),
            1 => Err(io::Error::from_raw_os_error(libc::ENOSPC)),
            2 => Err(io::Error::from_raw_os_error(libc::EACCES)),
            _ => Ok(0),
        }
    }
}
#[test]
fn short_writes_retry_but_enospc_permission_and_zero_writes_abort_privately() {
    for mode in 0..4 {
        let f = Fixture::new();
        let request = f.request();
        let hook = Arc::new(WriteFault {
            mode,
            reached: Default::default(),
        });
        let filesystem = FaultFs {
            root: f.destination.clone(),
            io: hook.clone(),
            limits: reconciliation_limits(),
        };
        let result = backup(&f.store, &request, &filesystem, &CancelToken::new());
        assert!(hook.reached.load(std::sync::atomic::Ordering::Acquire));
        if mode == 0 {
            assert!(matches!(result, Ok(PublicationOutcome::Durable(_))))
        } else {
            assert!(matches!(result, Err(RecoveryError::Destination { .. })));
            assert!(
                !f.destination
                    .join(request.operation_id.to_string())
                    .exists()
            );
        }
        f.assert_source_usable();
    }
}
struct CancelCopy {
    cancel: CancelToken,
    reached: std::sync::atomic::AtomicBool,
}
impl RecoveryIo for CancelCopy {
    fn before(&self, event: RecoveryEvent) -> io::Result<()> {
        if event == RecoveryEvent::Write(Member::VerificationCopy) {
            self.reached
                .store(true, std::sync::atomic::Ordering::Release);
            self.cancel.cancel();
        }
        Ok(())
    }
}
#[test]
fn cancellation_during_actual_verifier_copy_keeps_source_and_old_bundle_usable() {
    let f = Fixture::new();
    let old_request = f.request();
    let native = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
    let PublicationOutcome::Durable(old) =
        backup(&f.store, &old_request, &native, &CancelToken::new()).unwrap()
    else {
        panic!("old backup")
    };
    let old_bytes = fs::read(old.path().join("image.fls")).unwrap();
    let request = f.request();
    let cancel = CancelToken::new();
    let hook = Arc::new(CancelCopy {
        cancel: cancel.clone(),
        reached: Default::default(),
    });
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: hook.clone(),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &cancel),
        Err(fl_store::recovery::RecoveryError::Cancelled { .. })
    ));
    assert!(hook.reached.load(std::sync::atomic::Ordering::Acquire));
    assert!(
        !f.destination
            .join(request.operation_id.to_string())
            .exists()
    );
    assert_eq!(old_bytes, fs::read(old.path().join("image.fls")).unwrap());
    f.assert_source_usable();
}
#[test]
fn every_catalog_failure_is_unknown_and_repairable_without_changing_old_backup() {
    for event in [
        RecoveryEvent::Sync(Directory::StagingParent),
        RecoveryEvent::Flush(Member::Receipt),
        RecoveryEvent::CatalogPublish,
        RecoveryEvent::Sync(Directory::CatalogParent),
    ] {
        let f = Fixture::new();
        let old_request = f.request();
        let native = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
        let PublicationOutcome::Durable(old) =
            backup(&f.store, &old_request, &native, &CancelToken::new()).unwrap()
        else {
            panic!("old backup")
        };
        let old_bytes = fs::read(old.path().join("image.fls")).unwrap();
        let request = f.request();
        let faults = Arc::new(Fault::default());
        *faults.failure.lock().unwrap() = Some(event);
        let filesystem = FaultFs {
            root: f.destination.clone(),
            io: faults.clone(),
            limits: reconciliation_limits(),
        };
        assert!(matches!(
            backup(&f.store, &request, &filesystem, &CancelToken::new()).unwrap(),
            PublicationOutcome::OutcomeUnknown { .. }
        ));
        assert!(faults.seen.lock().unwrap().contains(&event));
        *faults.failure.lock().unwrap() = None;
        assert!(matches!(
            reconcile_publication(&request.operation_id.to_string(), &filesystem).unwrap(),
            PublicationOutcome::Durable(_)
        ));
        assert_eq!(old_bytes, fs::read(old.path().join("image.fls")).unwrap());
        f.assert_source_usable();
    }
}
struct KillAt {
    event: RecoveryEvent,
    marker: PathBuf,
}
impl RecoveryIo for KillAt {
    fn before(&self, event: RecoveryEvent) -> io::Result<()> {
        if event == self.event {
            fs::write(&self.marker, b"reached")?;
            unsafe { libc::raise(libc::SIGKILL) };
        }
        Ok(())
    }
}
#[test]
fn backup_crash_child() {
    let Some(root) = std::env::var_os("FERRO_RECOVERY_CRASH_ROOT") else {
        return;
    };
    assert!(std::env::var_os("FL_DB").is_none());
    let root = PathBuf::from(root);
    assert_eq!(
        fs::read_to_string(root.join("fixture-authority")).unwrap(),
        std::env::var("FERRO_RECOVERY_FIXTURE_TOKEN").unwrap()
    );
    let source = root.join("source.fls");
    let destination = root.join("backups");
    let owner = FerroStore::open(&source).unwrap();
    let mut request = fixture_request(&root, &source, &destination);
    request.operation_id =
        uuid::Uuid::parse_str(&std::env::var("FERRO_RECOVERY_OPERATION").unwrap()).unwrap();
    let event = match std::env::var("FERRO_RECOVERY_CRASH_POINT")
        .unwrap()
        .as_str()
    {
        "write" => RecoveryEvent::Write(Member::Image),
        "rename" => RecoveryEvent::Sync(Directory::PublicationParent),
        "receipt" => RecoveryEvent::Sync(Directory::CatalogParent),
        _ => panic!("unknown fixture crash point"),
    };
    let filesystem = FaultFs {
        root: destination,
        io: Arc::new(KillAt {
            event,
            marker: root.join("kill-reached"),
        }),
        limits: reconciliation_limits(),
    };
    let _ = backup(&owner, &request, &filesystem, &CancelToken::new());
    panic!("kill boundary was not reached");
}
#[test]
fn forced_termination_reconciles_only_complete_operation_without_source_reads() {
    use std::os::unix::process::ExitStatusExt;
    for point in ["write", "rename", "receipt"] {
        let f = Fixture::new();
        let request = f.request();
        let operation = request.operation_id.to_string();
        let token = uuid::Uuid::now_v7().to_string();
        fs::write(f._root.path().join("fixture-authority"), &token).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "backup_crash_child", "--nocapture"])
            .env_clear()
            .env("FERRO_RECOVERY_CRASH_ROOT", f._root.path())
            .env("FERRO_RECOVERY_FIXTURE_TOKEN", token)
            .env("FERRO_RECOVERY_OPERATION", &operation)
            .env("FERRO_RECOVERY_CRASH_POINT", point)
            .env_remove("AGENT_PERSONA")
            .env_remove("AGENT_NAME")
            .env_remove("FL_HOSTED_ID")
            .env_remove("AGENT_HYDRATION_GENERATION")
            .output()
            .unwrap();
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGKILL),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(f._root.path().join("kill-reached")).unwrap(),
            b"reached"
        );
        let hidden = f._root.path().join("source-hidden");
        fs::rename(&f.source, &hidden).unwrap();
        let filesystem = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
        let result = reconcile_publication(&operation, &filesystem);
        if point == "write" {
            assert!(matches!(result, Err(RecoveryError::Quarantined { .. })));
            assert!(!f.destination.join(&operation).exists());
        } else {
            assert!(matches!(result, Ok(PublicationOutcome::Durable(_))))
        }
        fs::rename(hidden, &f.source).unwrap();
        f.assert_source_usable();
    }
}

#[test]
fn reconciliation_resyncs_every_complete_member_before_catalog_attestation() {
    for event in [
        RecoveryEvent::Flush(Member::Image),
        RecoveryEvent::Flush(Member::Manifest),
        RecoveryEvent::Flush(Member::Validation),
        RecoveryEvent::Flush(Member::Profile),
        RecoveryEvent::Flush(Member::Report),
    ] {
        let f = Fixture::new();
        let request = f.request();
        let faults = Arc::new(Fault::default());
        *faults.failure.lock().unwrap() = Some(RecoveryEvent::Sync(Directory::PublicationParent));
        let filesystem = FaultFs {
            root: f.destination.clone(),
            io: faults.clone(),
            limits: reconciliation_limits(),
        };
        assert!(matches!(
            backup(&f.store, &request, &filesystem, &CancelToken::new()).unwrap(),
            PublicationOutcome::OutcomeUnknown { .. }
        ));
        faults.seen.lock().unwrap().clear();
        *faults.failure.lock().unwrap() = Some(event);
        assert!(matches!(
            reconcile_publication(&request.operation_id.to_string(), &filesystem).unwrap(),
            PublicationOutcome::OutcomeUnknown { .. }
        ));
        assert!(faults.seen.lock().unwrap().contains(&event));
        assert!(
            !f.destination
                .join(".catalog")
                .join(format!("{}.json", request.operation_id))
                .exists()
        );
        f.assert_source_usable();
    }
}
#[test]
fn incomplete_catalog_temp_is_preserved_and_durable_bundle_receipt_is_repaired() {
    let f = Fixture::new();
    let request = f.request();
    let faults = Arc::new(Fault::default());
    *faults.failure.lock().unwrap() = Some(RecoveryEvent::Sync(Directory::PublicationParent));
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: faults.clone(),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &CancelToken::new()).unwrap(),
        PublicationOutcome::OutcomeUnknown { .. }
    ));
    let temp = f
        .destination
        .join(".catalog")
        .join(format!("{}.tmp", request.operation_id));
    fs::write(&temp, b"{incomplete fixture receipt").unwrap();
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).unwrap();
    *faults.failure.lock().unwrap() = None;
    assert!(matches!(
        reconcile_publication(&request.operation_id.to_string(), &filesystem).unwrap(),
        PublicationOutcome::Durable(_)
    ));
    assert_eq!(
        fs::read(
            f.destination
                .join(".quarantine")
                .join(format!("{}-catalog-temp", request.operation_id))
        )
        .unwrap(),
        b"{incomplete fixture receipt"
    );
    assert!(
        f.destination
            .join(request.operation_id.to_string())
            .join("image.fls")
            .is_file()
    );
}
#[test]
fn mismatched_committed_receipt_quarantines_operation_without_deleting_members() {
    let f = Fixture::new();
    let request = f.request();
    let filesystem = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &CancelToken::new()).unwrap(),
        PublicationOutcome::Durable(_)
    ));
    let receipt = f
        .destination
        .join(".catalog")
        .join(format!("{}.json", request.operation_id));
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    json["image_sha256"] = json!("f".repeat(64));
    fs::write(&receipt, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(matches!(
        reconcile_publication(&request.operation_id.to_string(), &filesystem),
        Err(fl_store::recovery::RecoveryError::Quarantined { .. })
    ));
    assert!(!receipt.exists());
    assert!(
        f.destination
            .join(".quarantine")
            .join(request.operation_id.to_string())
            .join("receipt.json")
            .is_file()
    );
    assert!(
        f.destination
            .join(".quarantine")
            .join(request.operation_id.to_string())
            .is_dir()
    );
    assert!(
        f.destination
            .join(request.operation_id.to_string())
            .join("image.fls")
            .is_file()
    );
}

struct CapacityFault {
    available: u64,
    asked: std::sync::atomic::AtomicBool,
    captured: std::sync::atomic::AtomicBool,
}
impl RecoveryIo for CapacityFault {
    fn available_bytes(&self, _: &fs::File) -> io::Result<u64> {
        self.asked.store(true, std::sync::atomic::Ordering::Release);
        Ok(self.available)
    }
    fn before(&self, event: RecoveryEvent) -> io::Result<()> {
        if event == RecoveryEvent::Capture {
            self.captured
                .store(true, std::sync::atomic::Ordering::Release);
        }
        Ok(())
    }
}
#[test]
fn actual_capacity_and_stated_margin_are_admitted_before_capture() {
    for (available, margin) in [(0, 0), (4095, 4096), (131072, 8192)] {
        let f = Fixture::new();
        let mut request = f.request();
        request.space_margin_bytes = margin;
        let hook = Arc::new(CapacityFault {
            available,
            asked: Default::default(),
            captured: Default::default(),
        });
        let filesystem = FaultFs {
            root: f.destination.clone(),
            io: hook.clone(),
            limits: reconciliation_limits(),
        };
        assert!(matches!(
            backup(&f.store, &request, &filesystem, &CancelToken::new()),
            Err(fl_store::recovery::RecoveryError::Quota { .. })
        ));
        assert!(hook.asked.load(std::sync::atomic::Ordering::Acquire));
        assert!(!hook.captured.load(std::sync::atomic::Ordering::Acquire));
        f.assert_source_usable();
    }
}
#[test]
fn oversized_shared_directory_still_admits_operations() {
    let f = Fixture::new();
    let request = f.request();
    let catalog = f.destination.join(".catalog");
    fs::create_dir(&catalog).unwrap();
    fs::set_permissions(&catalog, fs::Permissions::from_mode(0o700)).unwrap();
    for n in 0..512 {
        fs::write(catalog.join(format!("fixture-entry-{n:04}")), b"").unwrap();
    }
    assert!(fs::metadata(&catalog).unwrap().len() > 4096);
    let faults = Arc::new(Fault::default());
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: faults.clone(),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        backup(&f.store, &request, &filesystem, &CancelToken::new()).unwrap(),
        PublicationOutcome::Durable(_)
    ));
    assert!(
        faults
            .seen
            .lock()
            .unwrap()
            .contains(&RecoveryEvent::Capture)
    );
}
struct ObserveAllocation {
    root: PathBuf,
    cap: u64,
    maximum: std::sync::atomic::AtomicU64,
    copy_seen: std::sync::atomic::AtomicBool,
}
fn allocated_tree(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(path).unwrap();
    let mut bytes = m.blocks() * 512;
    if m.is_dir() {
        for item in fs::read_dir(path).unwrap() {
            bytes += allocated_tree(&item.unwrap().path())
        }
    }
    bytes
}
impl RecoveryIo for ObserveAllocation {
    fn before(&self, event: RecoveryEvent) -> io::Result<()> {
        let bytes = allocated_tree(&self.root);
        assert!(
            bytes <= self.cap,
            "allocated {bytes}, admitted {}",
            self.cap
        );
        self.maximum
            .fetch_max(bytes, std::sync::atomic::Ordering::AcqRel);
        if event == RecoveryEvent::Write(Member::VerificationCopy) {
            self.copy_seen
                .store(true, std::sync::atomic::Ordering::Release);
        }
        Ok(())
    }
}
#[test]
fn measured_image_ratio_and_allocation_peak_include_both_independent_images() {
    let f = Fixture::new();
    let request = f.request();
    let source = fs::metadata(&f.source).unwrap().len();
    let hook = Arc::new(ObserveAllocation {
        root: f.destination.clone(),
        cap: source * 2,
        maximum: Default::default(),
        copy_seen: Default::default(),
    });
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: hook.clone(),
        limits: reconciliation_limits(),
    };
    let PublicationOutcome::Durable(bundle) =
        backup(&f.store, &request, &filesystem, &CancelToken::new()).unwrap()
    else {
        panic!("not durable")
    };
    let image = fs::metadata(bundle.path().join("image.fls")).unwrap().len();
    assert!(hook.copy_seen.load(std::sync::atomic::Ordering::Acquire));
    for item in fs::read_dir(bundle.path()).unwrap() {
        assert_eq!(
            fs::metadata(item.unwrap().path())
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
    }
    assert_eq!(
        fs::metadata(bundle.path()).unwrap().permissions().mode() & 0o7777,
        0o700
    );
    println!(
        "fixture=repeated-update-unknown source_bytes={source} image_bytes={image} image_to_source={:.8} peak_allocated_bytes={} disk_cap={}",
        image as f64 / source as f64,
        hook.maximum.load(std::sync::atomic::Ordering::Acquire),
        source * 2
    );
}

struct AdvanceSource {
    owner: Arc<FerroStore>,
    advanced: std::sync::atomic::AtomicBool,
}
impl RecoveryIo for AdvanceSource {
    fn before(&self, event: RecoveryEvent) -> io::Result<()> {
        if event == RecoveryEvent::Write(Member::Image)
            && !self
                .advanced
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            self.owner
                .put_doc("fixture_unknown", "after-cut", &json!({"committed":true}))
                .unwrap();
        }
        Ok(())
    }
}
#[test]
fn source_commits_continue_while_bundle_remains_at_the_captured_sequence() {
    let f = Fixture::new();
    let request = f.request();
    let source = Arc::new(f.store);
    let hook = Arc::new(AdvanceSource {
        owner: source.clone(),
        advanced: Default::default(),
    });
    let filesystem = FaultFs {
        root: f.destination,
        io: hook.clone(),
        limits: reconciliation_limits(),
    };
    let PublicationOutcome::Durable(bundle) =
        backup(&source, &request, &filesystem, &CancelToken::new()).unwrap()
    else {
        panic!("not durable")
    };
    assert!(hook.advanced.load(std::sync::atomic::Ordering::Acquire));
    let restored =
        fl_store::recovery::open_recovery_image_existing(&bundle.path().join("image.fls")).unwrap();
    assert_eq!(
        restored.get_doc("fixture_unknown", "after-cut").unwrap(),
        None
    );
    assert_eq!(
        source.get_doc("fixture_unknown", "after-cut").unwrap(),
        Some(json!({"committed":true}))
    );
    assert_eq!(
        restored.get_doc("fixture_unknown", "one").unwrap().unwrap()["n"],
        23
    );
}

// These mutations catch stale pre-rename verification and unchecked receipt
// returns. Same-content replacements must also preserve the held inode binding.
#[derive(Clone, Copy, Debug)]
enum PublicationMutation {
    ImageBytes,
    ImageInode,
    DirectoryInode,
    ReceiptMissing,
    ReceiptBytes,
    ReceiptInode,
}
struct MutatePublication {
    root: PathBuf,
    operation: String,
    event: RecoveryEvent,
    mutation: PublicationMutation,
    done: std::sync::atomic::AtomicBool,
}
impl RecoveryIo for MutatePublication {
    fn before(&self, event: RecoveryEvent) -> io::Result<()> {
        if event != self.event || self.done.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Ok(());
        }
        let bundle = self.root.join(&self.operation);
        let image = bundle.join("image.fls");
        let receipt = self
            .root
            .join(".catalog")
            .join(format!("{}.json", self.operation));
        match self.mutation {
            PublicationMutation::ImageBytes => fs::write(image, b"changed image")?,
            PublicationMutation::ImageInode | PublicationMutation::ReceiptInode => {
                let path = if matches!(self.mutation, PublicationMutation::ImageInode) {
                    image
                } else {
                    receipt
                };
                let held = path.with_extension("held");
                fs::rename(&path, &held)?;
                fs::copy(&held, &path)?;
                fs::remove_file(held)?;
            }
            PublicationMutation::DirectoryInode => {
                let held = self.root.join(format!("{}-held", self.operation));
                fs::rename(&bundle, &held)?;
                fs::create_dir(&bundle)?;
                fs::set_permissions(&bundle, fs::Permissions::from_mode(0o700))?;
                for member in fs::read_dir(&held)? {
                    let member = member?;
                    // Move the original members: only the directory inode changes.
                    fs::rename(member.path(), bundle.join(member.file_name()))?;
                }
            }
            PublicationMutation::ReceiptMissing => fs::remove_file(receipt)?,
            PublicationMutation::ReceiptBytes => fs::write(receipt, b"{corrupt receipt")?,
        }
        Ok(())
    }
}

fn publication_mutation_matrix(reconciliation: Option<bool>) {
    let mut failures = Vec::new();
    for event in [
        RecoveryEvent::Sync(Directory::PublicationParent),
        RecoveryEvent::Sync(Directory::StagingParent),
        RecoveryEvent::CatalogPublish,
        RecoveryEvent::Sync(Directory::CatalogParent),
    ] {
        for mutation in [
            PublicationMutation::ImageBytes,
            PublicationMutation::ImageInode,
            PublicationMutation::DirectoryInode,
            PublicationMutation::ReceiptMissing,
            PublicationMutation::ReceiptBytes,
            PublicationMutation::ReceiptInode,
        ] {
            if (matches!(
                mutation,
                PublicationMutation::ReceiptMissing
                    | PublicationMutation::ReceiptBytes
                    | PublicationMutation::ReceiptInode
            ) && event != RecoveryEvent::Sync(Directory::CatalogParent))
                || (reconciliation == Some(true) && event == RecoveryEvent::CatalogPublish)
            {
                continue;
            }
            let f = Fixture::new();
            let native = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
            let PublicationOutcome::Durable(old) =
                backup(&f.store, &f.request(), &native, &CancelToken::new()).unwrap()
            else {
                panic!("old backup")
            };
            let old_bytes = fs::read(old.path().join("image.fls")).unwrap();
            let request = f.request();
            if let Some(existing_receipt) = reconciliation {
                if existing_receipt {
                    assert!(matches!(
                        backup(&f.store, &request, &native, &CancelToken::new()).unwrap(),
                        PublicationOutcome::Durable(_)
                    ));
                } else {
                    let fault = Arc::new(Fault::default());
                    *fault.failure.lock().unwrap() =
                        Some(RecoveryEvent::Sync(Directory::PublicationParent));
                    let interrupted = FaultFs {
                        root: f.destination.clone(),
                        io: fault,
                        limits: reconciliation_limits(),
                    };
                    assert!(matches!(
                        backup(&f.store, &request, &interrupted, &CancelToken::new()).unwrap(),
                        PublicationOutcome::OutcomeUnknown { .. }
                    ));
                    // Reconcile a complete staging bundle, exercising its rename too.
                    fs::rename(
                        f.destination.join(request.operation_id.to_string()),
                        f.destination
                            .join(".staging")
                            .join(request.operation_id.to_string()),
                    )
                    .unwrap();
                }
            }
            let hook = Arc::new(MutatePublication {
                root: f.destination.clone(),
                operation: request.operation_id.to_string(),
                event,
                mutation,
                done: Default::default(),
            });
            let filesystem = FaultFs {
                root: f.destination.clone(),
                io: hook.clone(),
                limits: reconciliation_limits(),
            };
            let result = if reconciliation.is_some() {
                reconcile_publication(&request.operation_id.to_string(), &filesystem)
            } else {
                backup(&f.store, &request, &filesystem, &CancelToken::new())
            };
            assert!(
                hook.done.load(std::sync::atomic::Ordering::Acquire),
                "unreached: {reconciliation:?} {event:?} {mutation:?}"
            );
            if !matches!(result, Err(fl_store::recovery::RecoveryError::Quarantined { operation_id }) if operation_id == request.operation_id)
            {
                failures.push(format!("{event:?} {mutation:?}: {result:?}"));
            } else {
                assert!(
                    f.destination
                        .join(".quarantine")
                        .join(request.operation_id.to_string())
                        .is_dir()
                );
                assert!(
                    f.destination
                        .join(request.operation_id.to_string())
                        .join("image.fls")
                        .is_file()
                );
                {
                    assert!(
                        !f.destination
                            .join(".catalog")
                            .join(format!("{}.json", request.operation_id))
                            .exists(),
                        "receipt published after invalid final bundle"
                    );
                }
            }
            assert_eq!(old_bytes, fs::read(old.path().join("image.fls")).unwrap());
            f.assert_source_usable();
        }
    }
    assert!(
        failures.is_empty(),
        "stale publication attestations: {failures:#?}"
    );
}
#[test]
fn backup_revalidates_final_bundle_and_committed_receipt() {
    publication_mutation_matrix(None);
}
#[test]
fn staging_reconciliation_revalidates_final_bundle_and_committed_receipt() {
    publication_mutation_matrix(Some(false));
}
#[test]
fn existing_receipt_reconciliation_revalidates_after_catalog_sync() {
    publication_mutation_matrix(Some(true));
}

#[test]
fn quarantine_parent_sync_failure_is_unknown_and_preserves_other_backup() {
    let f = Fixture::new();
    let native = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
    let PublicationOutcome::Durable(old) =
        backup(&f.store, &f.request(), &native, &CancelToken::new()).unwrap()
    else {
        panic!("old backup")
    };
    let old_bytes = fs::read(old.path().join("image.fls")).unwrap();
    let request = f.request();
    assert!(matches!(
        backup(&f.store, &request, &native, &CancelToken::new()).unwrap(),
        PublicationOutcome::Durable(_)
    ));
    let receipt = f
        .destination
        .join(".catalog")
        .join(format!("{}.json", request.operation_id));
    fs::write(&receipt, b"{corrupt receipt").unwrap();
    let faults = Arc::new(Fault::default());
    let event = RecoveryEvent::Sync(Directory::QuarantineParent);
    *faults.failure.lock().unwrap() = Some(event);
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: faults.clone(),
        limits: reconciliation_limits(),
    };
    assert!(
        matches!(reconcile_publication(&request.operation_id.to_string(), &filesystem).unwrap(), PublicationOutcome::OutcomeUnknown { operation_id } if operation_id == request.operation_id)
    );
    assert!(faults.seen.lock().unwrap().contains(&event));
    assert!(
        f.destination
            .join(".quarantine")
            .join(request.operation_id.to_string())
            .is_dir()
    );
    assert!(!receipt.exists());
    assert_eq!(
        fs::read(
            f.destination
                .join(".quarantine")
                .join(request.operation_id.to_string())
                .join("receipt.json")
        )
        .unwrap(),
        b"{corrupt receipt"
    );
    assert_eq!(old_bytes, fs::read(old.path().join("image.fls")).unwrap());
    f.assert_source_usable();
}

#[test]
fn quarantine_retry_withdraws_receipt_and_resyncs_after_failure() {
    for event in [
        RecoveryEvent::CatalogPublish,
        RecoveryEvent::Sync(Directory::QuarantineParent),
        RecoveryEvent::Sync(Directory::CatalogParent),
    ] {
        let f = Fixture::new();
        let r = f.request();
        let operation = r.operation_id.to_string();
        let native = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
        backup(&f.store, &r, &native, &CancelToken::new()).unwrap();
        let receipt = f
            .destination
            .join(".catalog")
            .join(format!("{operation}.json"));
        fs::write(&receipt, b"{corrupt receipt").unwrap();
        let faults = Arc::new(Fault::default());
        *faults.failure.lock().unwrap() = Some(event);
        let filesystem = FaultFs {
            root: f.destination.clone(),
            io: faults.clone(),
            limits: reconciliation_limits(),
        };
        let first = reconcile_publication(&operation, &filesystem);
        assert!(
            matches!(first, Ok(PublicationOutcome::OutcomeUnknown { operation_id }) if operation_id == r.operation_id)
        );
        let evidence = f
            .destination
            .join(".quarantine")
            .join(&operation)
            .join("receipt.json");
        assert_eq!(receipt.exists(), event == RecoveryEvent::CatalogPublish);
        assert_eq!(evidence.exists(), event != RecoveryEvent::CatalogPublish);
        // A retry must still reach the durability barriers, even after rename succeeded.
        *faults.failure.lock().unwrap() = Some(RecoveryEvent::Sync(Directory::CatalogParent));
        faults.seen.lock().unwrap().clear();
        assert!(matches!(
            reconcile_publication(&operation, &filesystem),
            Err(RecoveryError::Destination { operation_id }) if operation_id == r.operation_id
        ));
        assert!(
            faults
                .seen
                .lock()
                .unwrap()
                .contains(&RecoveryEvent::Sync(Directory::CatalogParent))
        );
        assert!(!receipt.exists());
        assert_eq!(fs::read(&evidence).unwrap(), b"{corrupt receipt");
        *faults.failure.lock().unwrap() = None;
        faults.seen.lock().unwrap().clear();
        assert!(matches!(
            reconcile_publication(&operation, &filesystem),
            Err(RecoveryError::Quarantined { .. })
        ));
        for directory in [Directory::CatalogParent, Directory::QuarantineParent] {
            assert!(
                faults
                    .seen
                    .lock()
                    .unwrap()
                    .contains(&RecoveryEvent::Sync(directory))
            );
        }
        assert!(!receipt.exists());
        assert_eq!(fs::read(&evidence).unwrap(), b"{corrupt receipt");
    }
}

#[test]
fn exact_two_images_plus_reserve_quota_is_admitted() {
    let f = Fixture::new();
    let mut r = f.request();
    r.image_byte_limit = None;
    use std::os::fd::AsRawFd;
    let root = fs::File::open(&f.destination).unwrap();
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    assert_eq!(
        unsafe { libc::fstatvfs(root.as_raw_fd(), stat.as_mut_ptr()) },
        0
    );
    let unit = unsafe { stat.assume_init() }.f_frsize;
    let reserve = (32 * 129 + 26) * unit + r.metadata_byte_limit;
    r.limits.byte_limit = 2 * fs::metadata(&f.source).unwrap().len() + reserve;
    assert!(matches!(
        backup(
            &f.store,
            &r,
            &NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits()),
            &CancelToken::new()
        )
        .unwrap(),
        PublicationOutcome::Durable(_)
    ));
}

#[test]
fn default_ceiling_is_admitted_with_bounded_reserve() {
    let f = Fixture::new();
    let mut r = f.request();
    r.image_byte_limit = None;
    assert!(matches!(
        backup(
            &f.store,
            &r,
            &NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits()),
            &CancelToken::new()
        )
        .unwrap(),
        PublicationOutcome::Durable(_)
    ));
}
#[test]
fn operation_lock_is_reclaimed_and_store_lock_has_diagnostics() {
    let f = Fixture::new();
    let r = f.request();
    backup(
        &f.store,
        &r,
        &NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits()),
        &CancelToken::new(),
    )
    .unwrap();
    let names: Vec<_> = fs::read_dir(f.destination.join(".locks"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(names.len(), 1);
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&names[0]).unwrap()).unwrap();
    assert_eq!(v["operation_id"], r.operation_id.to_string());
    assert_eq!(v["pid"], std::process::id());
    assert!(v["started_at_unix_ms"].as_u64().unwrap() > 0);
}

struct ExhaustedOpen;
impl RecoveryIo for ExhaustedOpen {
    fn before(&self, e: RecoveryEvent) -> io::Result<()> {
        if e == RecoveryEvent::OpenFile {
            Err(io::Error::from_raw_os_error(libc::EMFILE))
        } else {
            Ok(())
        }
    }
}
#[test]
fn emfile_during_reconciliation_is_retryable_without_quarantine() {
    let f = Fixture::new();
    let r = f.request();
    let native = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
    backup(&f.store, &r, &native, &CancelToken::new()).unwrap();
    let receipt = f
        .destination
        .join(".catalog")
        .join(format!("{}.json", r.operation_id));
    let bytes = fs::read(&receipt).unwrap();
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: Arc::new(ExhaustedOpen),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        reconcile_publication(&r.operation_id.to_string(), &filesystem),
        Err(RecoveryError::Transient {
            errno: libc::EMFILE,
            ..
        })
    ));
    assert!(
        !f.destination
            .join(".quarantine")
            .join(r.operation_id.to_string())
            .exists()
    );
    assert_eq!(fs::read(receipt).unwrap(), bytes);
    assert!(matches!(
        reconcile_publication(&r.operation_id.to_string(), &native).unwrap(),
        PublicationOutcome::Durable(_)
    ));
}
struct ExhaustedRead {
    errno: i32,
    json_only: bool,
    reached: std::sync::atomic::AtomicBool,
}
impl RecoveryIo for ExhaustedRead {
    fn read_exact_at(&self, file: &fs::File, bytes: &mut [u8], offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        let mut prefix = [0];
        file.read_exact_at(&mut prefix, 0)?;
        if self.json_only == (prefix[0] == b'{') {
            self.reached
                .store(true, std::sync::atomic::Ordering::Release);
            Err(io::Error::from_raw_os_error(self.errno))
        } else {
            file.read_exact_at(bytes, offset)
        }
    }
}
#[test]
fn transient_reads_leave_good_bundle_and_receipt_untouched() {
    for errno in [
        libc::ENOMEM,
        libc::EAGAIN,
        libc::EMFILE,
        libc::ENFILE,
        libc::EINTR,
    ] {
        for json_only in [false, true] {
            let f = Fixture::new();
            let r = f.request();
            let operation = r.operation_id.to_string();
            let native = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
            backup(&f.store, &r, &native, &CancelToken::new()).unwrap();
            let receipt = f
                .destination
                .join(".catalog")
                .join(format!("{operation}.json"));
            let receipt_bytes = fs::read(&receipt).unwrap();
            let bundle = f.destination.join(&operation);
            let members: Vec<_> = fs::read_dir(&bundle)
                .unwrap()
                .map(|entry| {
                    let path = entry.unwrap().path();
                    let bytes = fs::read(&path).unwrap();
                    (path, bytes)
                })
                .collect();
            let hook = Arc::new(ExhaustedRead {
                errno,
                json_only,
                reached: Default::default(),
            });
            let filesystem = FaultFs {
                root: f.destination.clone(),
                io: hook.clone(),
                limits: reconciliation_limits(),
            };
            assert!(
                matches!(reconcile_publication(&operation, &filesystem), Err(RecoveryError::Transient { errno: actual, .. }) if actual == errno)
            );
            assert!(hook.reached.load(std::sync::atomic::Ordering::Acquire));
            assert!(!f.destination.join(".quarantine").join(&operation).exists());
            assert_eq!(fs::read(&receipt).unwrap(), receipt_bytes);
            for (path, bytes) in members {
                assert_eq!(fs::read(path).unwrap(), bytes);
            }
            assert!(matches!(
                reconcile_publication(&operation, &native).unwrap(),
                PublicationOutcome::Durable(_)
            ));
        }
    }
}

#[test]
fn thousand_operations_do_not_exhaust_shared_directories() {
    let f = Fixture::new();
    let mut r = f.request();
    let native = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
    let mut good = 0;
    let mut bad = 0;
    use std::os::unix::fs::MetadataExt;
    fn shared_bytes(root: &Path) -> u64 {
        ["", ".locks", ".staging", ".catalog", ".quarantine"]
            .iter()
            .map(|n| {
                fs::metadata(root.join(n))
                    .map(|m| m.blocks() * 512)
                    .unwrap_or(0)
            })
            .sum()
    }
    let mut total_growth = 0u64;
    let mut maximum_growth = 0u64;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    use std::os::fd::AsRawFd;
    let held = fs::File::open(&f.destination).unwrap();
    assert_eq!(
        unsafe { libc::fstatvfs(held.as_raw_fd(), stat.as_mut_ptr()) },
        0
    );
    let unit = unsafe { stat.assume_init() }.f_frsize;
    let directory_reserve = 32 * 129 * unit;

    for n in 0..1000 {
        let native = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
        let before = shared_bytes(&f.destination);
        r.operation_id = uuid::Uuid::now_v7();
        r.limits = reconciliation_limits();
        r.limits.operation_id = r.operation_id;
        r.capture.deadline = Instant::now() + Duration::from_secs(30);
        if n % 3 == 0 {
            let fault = Arc::new(Fault::default());
            *fault.failure.lock().unwrap() = Some(RecoveryEvent::Write(Member::Image));
            let filesystem = FaultFs {
                root: f.destination.clone(),
                io: fault,
                limits: reconciliation_limits(),
            };
            assert!(matches!(
                backup(&f.store, &r, &filesystem, &CancelToken::new()),
                Err(RecoveryError::Destination { .. })
            ));
            bad += 1;
        } else {
            assert!(matches!(
                backup(&f.store, &r, &native, &CancelToken::new()).unwrap(),
                PublicationOutcome::Durable(_)
            ));
            if n % 3 == 1 {
                fs::write(
                    f.destination
                        .join(r.operation_id.to_string())
                        .join("image.fls"),
                    b"broken",
                )
                .unwrap();
                assert!(matches!(
                    reconcile_publication(&r.operation_id.to_string(), &native),
                    Err(RecoveryError::Quarantined { .. })
                ));
                bad += 1;
            } else {
                good += 1;
            }
        }
        let operation = r.operation_id.to_string();
        let private_dirs = [
            f.destination.join(&operation),
            f.destination.join(".staging").join(&operation),
            f.destination.join(".quarantine").join(&operation),
        ]
        .iter()
        .map(|p| fs::metadata(p).map(|m| m.blocks() * 512).unwrap_or(0))
        .sum::<u64>();
        let growth = (shared_bytes(&f.destination) + private_dirs).saturating_sub(before);
        assert!(
            growth <= directory_reserve,
            "operation={n} actual_directory_growth={growth} reserve={directory_reserve}"
        );
        total_growth += growth;
        maximum_growth = maximum_growth.max(growth);
    }
    println!(
        "directory_growth_bytes={total_growth} maximum_operation_growth={maximum_growth} directory_reserve_per_operation={directory_reserve} charged_total={} growth_to_charge_ratio={:.8}",
        1000 * directory_reserve,
        total_growth as f64 / (1000 * directory_reserve) as f64
    );
    assert_eq!(
        fs::read_dir(f.destination.join(".locks")).unwrap().count(),
        1
    );
    assert!(matches!(
        backup(&f.store, &f.request(), &native, &CancelToken::new()).unwrap(),
        PublicationOutcome::Durable(_)
    ));
    let status = fl_store::recovery::backup::publication_status(&f.request(), &native).unwrap();
    assert_eq!(status.quarantined_count, 667);
    let mut insufficient = f.request();
    insufficient.limits.query_work_limit = 0;
    assert!(matches!(
        fl_store::recovery::backup::publication_status(&insufficient, &native),
        Err(RecoveryError::Quota { .. })
    ));
    assert!(status.quarantined_allocated_bytes > 0);
    assert!(status.derived_reserve_bytes < 512 * 1024 * 1024);
    println!(
        "operations=1000 receipts={good} quarantined={bad} physical_bytes={}",
        allocated_tree(&f.destination)
    );
    f.assert_source_usable();
}

#[test]
fn default_reserve_exceeding_quota_by_one_unit_refuses_before_capture() {
    let f = Fixture::new();
    let mut r = f.request();
    r.image_byte_limit = None;
    let root = fs::File::open(&f.destination).unwrap();
    use std::os::fd::AsRawFd;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    assert_eq!(
        unsafe { libc::fstatvfs(root.as_raw_fd(), stat.as_mut_ptr()) },
        0
    );
    let unit = unsafe { stat.assume_init() }.f_frsize;
    let reserve = (32 * 129 + 26) * unit + r.metadata_byte_limit;
    r.limits.byte_limit = 2 * fs::metadata(&f.source).unwrap().len() + reserve - unit;
    let hook = Arc::new(Fault::default());
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: hook.clone(),
        limits: reconciliation_limits(),
    };
    assert!(matches!(
        backup(&f.store, &r, &filesystem, &CancelToken::new()),
        Err(RecoveryError::Quota { .. })
    ));
    assert!(!hook.seen.lock().unwrap().contains(&RecoveryEvent::Capture));
}

fn mutation_event(e: RecoveryEvent) -> bool {
    matches!(
        e,
        RecoveryEvent::CreateDirectory
            | RecoveryEvent::LockFile
            | RecoveryEvent::Allocate
            | RecoveryEvent::Create(_)
            | RecoveryEvent::Write(_)
            | RecoveryEvent::RenameEntry
    )
}
struct AllocationFault {
    target: Option<usize>,
    errno: i32,
    seen: Mutex<Vec<RecoveryEvent>>,
    reached: std::sync::atomic::AtomicBool,
    prepared: std::sync::atomic::AtomicBool,
}
impl RecoveryIo for AllocationFault {
    fn before(&self, e: RecoveryEvent) -> io::Result<()> {
        if matches!(e, RecoveryEvent::Flush(_)) {
            self.prepared
                .store(true, std::sync::atomic::Ordering::Release);
        }
        if !mutation_event(e) {
            return Ok(());
        }
        let mut seen = self.seen.lock().unwrap();
        let index = seen.len();
        seen.push(e);
        if self.target == Some(index) {
            self.reached
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(io::Error::from_raw_os_error(self.errno));
        }
        Ok(())
    }
}
fn complete_bundle(path: &Path) {
    use sha2::{Digest, Sha256};
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(path.join("manifest.json")).unwrap()).unwrap();
    let image = fs::read(path.join("image.fls")).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&image)),
        manifest["image_sha256"].as_str().unwrap()
    );
    for member in manifest["retained_members"].as_array().unwrap() {
        let bytes = fs::read(path.join(member["relative_name"].as_str().unwrap())).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            member["sha256"].as_str().unwrap()
        );
    }
}
fn space_failure_case(phase: u8, target: Option<usize>, errno: i32) -> Vec<RecoveryEvent> {
    let f = Fixture::new();
    let native = NativeRecoveryFs::new(f.destination.clone(), reconciliation_limits());
    let old_request = f.request();
    let PublicationOutcome::Durable(old) =
        backup(&f.store, &old_request, &native, &CancelToken::new()).unwrap()
    else {
        panic!()
    };
    let old_image = fs::read(old.path().join("image.fls")).unwrap();
    let old_receipt = f
        .destination
        .join(".catalog")
        .join(format!("{}.json", old_request.operation_id));
    let old_receipt_bytes = fs::read(&old_receipt).unwrap();
    let mut request = f.request();
    // Include every allowlisted configuration copy in the actual-write trace.
    if phase == 0 {
        use fl_store::recovery::{
            backup::{ApprovedConfig, ExcludedVaultKey},
            manifest::{Availability, EffectiveCodeIndexConfig, MemberKind},
        };
        use std::os::unix::fs::MetadataExt;
        request.code_index_configuration = Availability::Available(EffectiveCodeIndexConfig {
            configuration_version: 1,
            effective_configuration_sha256: "a".repeat(64),
            source_root_identity: Availability::Unavailable,
            repository: Availability::Unavailable,
            source_revision: Availability::Unavailable,
            scan_rules: vec![],
            ignore_rules: vec![],
            languages_and_parser_versions: vec![],
            embedding_space: "fixture".into(),
            embedding_dimensions: 1,
            embedding_configuration: json!({}),
            ranking_version: "fixture-v1".into(),
            rebuild_vector_budget: 1,
            rebuild_time_budget_ms: 1,
            defaults_build_identity: "fixture-build".into(),
            recovery_dependencies: vec![],
        });
        let key = f._root.path().join("vault.key");
        fs::write(&key, b"fixture-key").unwrap();
        let m = fs::metadata(&key).unwrap();
        request.excluded_vault_key = Some(ExcludedVaultKey {
            path: key,
            identity: (m.dev(), m.ino()),
        });
        for (name, kind) in [
            ("config.toml", MemberKind::ProtectedConfig),
            ("warrants.json", MemberKind::Warrants),
            ("vault.enc", MemberKind::VaultCiphertext),
        ] {
            let path = f._root.path().join(name);
            fs::write(&path, b"fixture-only").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let m = fs::metadata(&path).unwrap();
            request.configs.push(ApprovedConfig {
                kind,
                path,
                identity: (m.dev(), m.ino()),
                byte_limit: 1024,
            });
        }
    } else {
        backup(&f.store, &request, &native, &CancelToken::new()).unwrap();
        let receipt = f
            .destination
            .join(".catalog")
            .join(format!("{}.json", request.operation_id));
        if phase == 1 {
            fs::write(receipt, b"{corrupt").unwrap();
        } else {
            fs::remove_file(receipt).unwrap();
            if phase == 2 {
                fs::write(
                    f.destination
                        .join(".catalog")
                        .join(format!("{}.tmp", request.operation_id)),
                    b"{partial",
                )
                .unwrap();
                fs::set_permissions(
                    f.destination
                        .join(".catalog")
                        .join(format!("{}.tmp", request.operation_id)),
                    fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            } else {
                fs::rename(
                    f.destination.join(request.operation_id.to_string()),
                    f.destination
                        .join(".staging")
                        .join(request.operation_id.to_string()),
                )
                .unwrap();
            }
        }
    }
    let hook = Arc::new(AllocationFault {
        target,
        errno,
        seen: Mutex::new(vec![]),
        reached: Default::default(),
        prepared: Default::default(),
    });
    let filesystem = FaultFs {
        root: f.destination.clone(),
        io: hook.clone(),
        limits: reconciliation_limits(),
    };
    let result = if phase == 0 {
        backup(&f.store, &request, &filesystem, &CancelToken::new())
    } else {
        reconcile_publication(&request.operation_id.to_string(), &filesystem)
    };
    if target.is_some() {
        assert!(
            hook.reached.load(std::sync::atomic::Ordering::Acquire),
            "phase={phase} target={target:?}"
        );
        if f.destination
            .join(request.operation_id.to_string())
            .exists()
            && (phase == 0 || hook.prepared.load(std::sync::atomic::Ordering::Acquire))
        {
            assert!(
                matches!(result, Ok(PublicationOutcome::OutcomeUnknown { .. })),
                "phase={phase} target={target:?}: {result:?}"
            );
            complete_bundle(&f.destination.join(request.operation_id.to_string()));
        } else {
            assert!(
                matches!(result, Err(RecoveryError::Destination { .. })),
                "phase={phase} target={target:?}: {result:?}"
            );
        }
    } else if phase == 1 {
        assert!(matches!(result, Err(RecoveryError::Quarantined { .. })));
    } else {
        assert!(matches!(result, Ok(PublicationOutcome::Durable(_))));
    }
    if phase == 1 {
        let receipt = f
            .destination
            .join(".catalog")
            .join(format!("{}.json", request.operation_id));
        let evidence = f
            .destination
            .join(".quarantine")
            .join(request.operation_id.to_string())
            .join("receipt.json");
        assert_ne!(
            receipt.exists(),
            evidence.exists(),
            "receipt evidence must have exactly one location"
        );
        assert_eq!(
            fs::read(if receipt.exists() {
                &receipt
            } else {
                &evidence
            })
            .unwrap(),
            b"{corrupt"
        );
        assert!(matches!(
            reconcile_publication(&request.operation_id.to_string(), &native),
            Err(RecoveryError::Quarantined { .. })
        ));
        assert!(!receipt.exists());
        assert_eq!(fs::read(evidence).unwrap(), b"{corrupt");
    }
    assert_eq!(fs::read(old.path().join("image.fls")).unwrap(), old_image);
    assert_eq!(fs::read(old_receipt).unwrap(), old_receipt_bytes);
    f.assert_source_usable();
    assert!(
        fs::read_dir(f.destination.join(".locks"))
            .unwrap()
            .all(|e| !e
                .unwrap()
                .file_name()
                .as_encoded_bytes()
                .starts_with(b"operation-")),
        "operation lock leaked after phase={phase} target={target:?}"
    );
    hook.seen.lock().unwrap().clone()
}
#[test]
fn enospc_and_edquot_at_every_actual_entry_write_and_allocation_are_clean() {
    for phase in 0..4 {
        let trace = space_failure_case(phase, None, 0);
        assert!(!trace.is_empty());
        for errno in [libc::ENOSPC, libc::EDQUOT] {
            for index in 0..trace.len() {
                space_failure_case(phase, Some(index), errno);
            }
        }
        println!(
            "phase={phase} allocation_points={} injected_errors=ENOSPC,EDQUOT",
            trace.len()
        );
    }
}
