//! Restore admission is exercised on isolated disk-backed fixtures only.
#![cfg(target_os = "linux")]
use fl_store::FerroStore;
use recovery_task6_support::test_support;
use std::fs;

#[test]
fn restore_completed_not_ready_stays_fenced() {
    let root = test_support::fixture_root();
    let path = root.path().join("fixture.fls");
    fs::write(
        root.path().join("fixture.fls.restore-guard"),
        b"malformed private fence",
    )
    .unwrap();
    assert!(
        FerroStore::open(&path).is_err(),
        "guard must refuse before store creation"
    );
    assert!(!path.exists(), "fenced open created a store");
    assert!(
        FerroStore::auto(&path).is_err(),
        "auto must refuse before socket contact"
    );
    assert!(!path.exists());
}

mod recovery_task6_support;
use fl_store::recovery::{admission::*, manifest::BundleManifestV1, restore::*};
use recovery_task6_support::{Fixture, limits};
use std::os::unix::fs::MetadataExt;
#[test]
fn restore_preserves_original_inode_without_second_image_allocation() {
    let f = Fixture::new();
    let bundle = f.bundle();
    let manifest: BundleManifestV1 =
        serde_json::from_slice(&fs::read(bundle.path().join("manifest.json")).unwrap()).unwrap();
    let l = limits();
    let operation = l.operation_id;
    let binding = f.lease_request(
        OfflineAction::Restore {
            bundle_operation: uuid::Uuid::parse_str(&manifest.operation_id).unwrap(),
            bundle_sha256: format!(
                "{:x}",
                sha2::Sha256::digest(fs::read(bundle.path().join("manifest.json")).unwrap())
            ),
        },
        operation,
        manifest.validation_profile_sha256.clone(),
        &f.source,
    );
    let lease = create_offline_lease(&binding, &f.filesystem(), &l).unwrap();
    let old = fs::metadata(&f.source).unwrap();
    let original_image = fs::read(bundle.path().join("image.fls")).unwrap();
    let bundle_sizes = fs::read_dir(bundle.path())
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), entry.metadata().unwrap().len())
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let bundle_bytes = bundle_sizes.values().sum::<u64>();
    let request = RestoreRequest {
        operation_id: operation,
        store: f.identity(),
        destination: f.source.clone(),
        staging_parent: f.root.path().to_path_buf(),
        selected_bundle: bundle,
        limits: l,
        metadata_byte_limit: 1 << 20,
        space_margin_bytes: 4096,
        available_dependencies: vec![],
    };
    let trace = TraceFs {
        native: f.filesystem(),
        io: Arc::new(TraceIo::default()),
    };
    let outcome = restore_offline(&request, &lease, &trace).unwrap();
    assert_eq!(
        trace.io.images.load(Ordering::Acquire),
        1,
        "restore allocated more than one candidate image"
    );
    assert_eq!(
        trace.io.copies.load(Ordering::Acquire),
        0,
        "candidate verification created a second full image"
    );
    let RestoreOutcome::Installed { journal } = outcome else {
        panic!("restore not durably installed")
    };
    assert_eq!(journal.budget.backup_bytes, bundle_bytes);
    assert!(journal.budget.backup_bytes > manifest.image_bytes);
    assert_eq!(journal.budget.destination_bytes, old.len());
    assert_eq!(
        fs::read_dir(request.selected_bundle.path())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), entry.metadata().unwrap().len())
            })
            .collect::<std::collections::BTreeMap<_, _>>(),
        bundle_sizes
    );
    let rollback = journal.rollback.unwrap();
    let saved = fs::metadata(rollback).unwrap();
    assert_eq!((old.dev(), old.ino()), (saved.dev(), saved.ino()));
    assert_eq!(saved.len(), old.len());
    assert_ne!(fs::metadata(&f.source).unwrap().ino(), old.ino());
    assert_eq!(
        fs::read(request.selected_bundle.path().join("image.fls")).unwrap(),
        original_image
    );
    assert!(
        FerroStore::open(&f.source).is_err(),
        "installation is not finalize"
    );
}
use sha2::Digest;

struct TraceFs {
    native: fl_store::recovery::fs::NativeRecoveryFs,
    io: Arc<TraceIo>,
}
#[derive(Default)]
struct TraceIo {
    images: AtomicUsize,
    copies: AtomicUsize,
}
impl fl_store::recovery::fs::RecoveryIo for TraceIo {
    fn before(&self, event: fl_store::recovery::fs::RecoveryEvent) -> std::io::Result<()> {
        if event == RecoveryEvent::Create(Member::Image) {
            self.images.fetch_add(1, Ordering::AcqRel);
        }
        if event == RecoveryEvent::Create(Member::VerificationCopy) {
            self.copies.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }
}
impl fl_store::recovery::fs::RecoveryFs for TraceFs {
    fn root(&self) -> &std::path::Path {
        fl_store::recovery::fs::RecoveryFs::root(&self.native)
    }
    fn reconciliation_limits(&self) -> &fl_store::recovery::OperationLimits {
        fl_store::recovery::fs::RecoveryFs::reconciliation_limits(&self.native)
    }
    fn io(&self) -> std::sync::Arc<dyn fl_store::recovery::fs::RecoveryIo> {
        self.io.clone()
    }
}

fn prepare_restore(f: &Fixture, destination: &std::path::Path) -> (RestoreRequest, OfflineLease) {
    let bundle = f.bundle();
    prepare_restore_bundle(f, destination, bundle)
}
fn prepare_restore_bundle(
    f: &Fixture,
    destination: &std::path::Path,
    bundle: fl_store::recovery::backup::VerifiedBundle,
) -> (RestoreRequest, OfflineLease) {
    let manifest: BundleManifestV1 =
        serde_json::from_slice(&fs::read(bundle.path().join("manifest.json")).unwrap()).unwrap();
    let l = limits();
    let binding = f.lease_request(
        OfflineAction::Restore {
            bundle_operation: uuid::Uuid::parse_str(&manifest.operation_id).unwrap(),
            bundle_sha256: format!(
                "{:x}",
                sha2::Sha256::digest(fs::read(bundle.path().join("manifest.json")).unwrap())
            ),
        },
        l.operation_id,
        manifest.validation_profile_sha256.clone(),
        destination,
    );
    let lease = create_offline_lease(&binding, &f.filesystem(), &l).unwrap();
    (
        RestoreRequest {
            operation_id: l.operation_id,
            store: f.identity(),
            destination: destination.to_path_buf(),
            staging_parent: destination.parent().unwrap().to_path_buf(),
            selected_bundle: bundle,
            limits: l,
            metadata_byte_limit: 1 << 20,
            space_margin_bytes: 4096,
            available_dependencies: vec![],
        },
        lease,
    )
}
fn reconciliation(j: &RestoreJournal) -> fl_store::recovery::readiness::ReconciliationRecord {
    use fl_store::recovery::readiness::*;
    ReconciliationRecord {
        version: 1,
        operation_id: j.operation_id,
        image_sha256: j.verification.image_sha256.clone(),
        published_sequence: j.verification.published_sequence,
        operator: "fixture operator".into(),
        recorded_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        affected_scopes: vec!["fixture".into()],
        identity_confirmed: true,
        reference_closure_confirmed: true,
        external_effect_coverage_confirmed: true,
        source_publication_resolution_sha256: "d".repeat(64),
        clients: ClientReconciliation {
            clients_invalidated: true,
            rpc_sessions_restarted: true,
            transactions_drained: true,
            caches_invalidated: true,
            retry_assumptions_reconciled: true,
            evidence_sha256: "e".repeat(64),
        },
        dependencies: vec![],
        external_outcomes: vec![],
        unresolved_items: vec![],
        index: IndexReadiness::Lexical,
    }
}
#[test]
fn restore_explicit_rollback_preserves_both_generations() {
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let original = fs::metadata(&f.source).unwrap().ino();
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let installed = fs::metadata(&f.source).unwrap().ino();
    assert!(matches!(
        resume_restore(&journal, &lease, &f.filesystem()).unwrap(),
        RestoreOutcome::Installed { .. }
    ));
    let RestoreOutcome::RolledBack { journal } =
        rollback_restore(&journal, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not rolled back")
    };
    assert_eq!(fs::metadata(&f.source).unwrap().ino(), original);
    assert_eq!(
        fs::metadata(journal.rollback.unwrap()).unwrap().ino(),
        installed
    );
    assert!(FerroStore::open(&f.source).is_err());
}
#[test]
fn restore_finalize_records_evidence_before_guard_removal() {
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    assert!(
        fl_store::recovery::open_recovery_image_existing(&f.source).is_err(),
        "public reader bypassed fence"
    );
    let mut evidence = reconciliation(&journal);
    evidence.clients.clients_invalidated = false;
    assert!(
        fl_store::recovery::readiness::finalize_restore(
            &journal,
            &lease,
            &evidence,
            &f.filesystem()
        )
        .is_err()
    );
    assert!(FerroStore::open(&f.source).is_err());
    evidence.clients.clients_invalidated = true;
    let result = fl_store::recovery::readiness::finalize_restore(
        &journal,
        &lease,
        &evidence,
        &f.filesystem(),
    )
    .unwrap();
    assert!(matches!(result, RestoreOutcome::Finalized { .. }));
    assert!(FerroStore::open(&f.source).is_ok());
}
#[test]
fn restore_rejects_wrong_inventory_identity_before_guard() {
    let f = Fixture::new();
    let (mut request, lease) = prepare_restore(&f, &f.source);
    request.store.filesystem_identity = Some("0:0".into());
    assert!(restore_offline(&request, &lease, &f.filesystem()).is_err());
    assert!(!f.source.with_extension("fls.restore-guard").exists());
}

#[test]
fn restore_absent_target_verifies_single_candidate() {
    let f = Fixture::new();
    let writer = FerroStore::open(&f.source).unwrap();
    writer
        .put_doc(
            "fixture_unknown",
            "one",
            &serde_json::json!({"body":"x".repeat(20<<20)}),
        )
        .unwrap();
    drop(writer);
    let destination = f.root.path().join("absent.fls");
    let (request, lease) = prepare_restore(&f, &destination);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    assert!(journal.rollback.is_none());
    assert_eq!(journal.budget.rollback_new_bytes, 0);
    assert!(
        journal.budget.candidate_bytes + journal.budget.metadata_bytes
            <= 2 * journal.budget.baseline_bytes
    );
    let RestoreOutcome::OriginalAbsenceRestored { journal } =
        rollback_restore(&journal, &lease, &f.filesystem()).unwrap()
    else {
        panic!("absence not restored")
    };
    assert!(!destination.exists());
    let candidate = journal.staging.join("candidate.fls");
    assert!(candidate.exists());
    use fl_store::recovery::readiness::{AbsenceReconciliationRecord, finalize_original_absence};
    let clock = ClockFs {
        native: f.filesystem(),
        now: journal.binding.expires_at_unix_ms + 1,
    };
    assert!(
        finalize_original_absence(
            &journal,
            &lease,
            &AbsenceReconciliationRecord {
                version: 1,
                operation_id: journal.operation_id,
                candidate_sha256: journal.candidate_sha256.clone(),
                operator: "fixture operator".into(),
                recorded_at_unix_ms: clock.now,
                no_client_opened_path_while_installed: true,
                evidence_sha256: "f".repeat(64)
            },
            &clock
        )
        .is_err()
    );
    let renewed = renewed_lease(&journal, &clock);
    let evidence = AbsenceReconciliationRecord {
        version: 1,
        operation_id: journal.operation_id,
        candidate_sha256: journal.candidate_sha256.clone(),
        operator: "fixture operator".into(),
        recorded_at_unix_ms: clock.now,
        no_client_opened_path_while_installed: true,
        evidence_sha256: "f".repeat(64),
    };
    assert!(matches!(
        finalize_original_absence(&journal, &renewed, &evidence, &clock).unwrap(),
        RestoreOutcome::OriginalAbsenceFinalized { .. }
    ));
    assert!(!destination.exists());
    assert!(candidate.exists());
    assert!(!destination.with_extension("fls.restore-guard").exists());
}
#[test]
fn restore_unreadable_original_is_preserved_without_verified_rollback_claim() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let bundle = f.bundle();
    let old = fs::metadata(&f.source).unwrap().ino();
    fs::set_permissions(&f.source, fs::Permissions::from_mode(0o000)).unwrap();
    let (request, lease) = prepare_restore_bundle(&f, &f.source, bundle);
    let outcome = restore_offline(&request, &lease, &f.filesystem()).unwrap();
    let RestoreOutcome::Installed { journal } = outcome else {
        panic!("forensic original prevented installation: {outcome:?}")
    };
    assert!(journal.original_verification.is_none());
    assert!(journal.original_read_error.is_some());
    assert!(matches!(journal.original, FileBinding::Forensic { .. }));
    let path = journal.rollback.as_ref().unwrap();
    assert_eq!(fs::metadata(path).unwrap().ino(), old);
    assert!(rollback_restore(&journal, &lease, &f.filesystem()).is_err());
    assert!(f.root.path().join("source.fls.restore-guard").exists());
    assert!(
        fl_store::recovery::readiness::finalize_rollback(
            &journal,
            &lease,
            &reconciliation(&journal),
            &f.filesystem()
        )
        .is_err()
    );
}
#[test]
fn restore_guard_refuses_cached_server_bind_before_socket_creation() {
    let root = test_support::fixture_root();
    let path = root.path().join("cached.fls");
    let store = FerroStore::open(&path).unwrap();
    fs::write(root.path().join("cached.fls.restore-guard"), b"malformed").unwrap();
    let socket = root.path().join("cached.sock");
    assert!(fl_store::FerroStoreServer::bind_unix(&socket, store).is_err());
    assert!(!socket.exists());
}
#[test]
fn restore_twelve_shared_attempts_preserve_reserved_rollback_and_verified_finalize() {
    use fl_store::recovery::readiness::*;
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let mut bad = reconciliation(&journal);
    bad.clients.caches_invalidated = false;
    for _ in 0..12 {
        assert!(finalize_restore(&journal, &lease, &bad, &f.filesystem()).is_err());
    }
    let path = journal.staging.join("journal.bin");
    let status = restore_status(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(
        (status.resume_finalize_attempts, status.rollback_attempts),
        (12, 0)
    );
    assert!(resume_restore(&journal, &lease, &f.filesystem()).is_err());
    // The reserved finalize action cannot admit the selected backup, even with
    // otherwise complete evidence and even after shared capacity is exhausted.
    assert!(
        finalize_rollback(&journal, &lease, &reconciliation(&journal), &f.filesystem()).is_err()
    );
    assert!(f.root.path().join("source.fls.restore-guard").exists());
    let RestoreOutcome::RolledBack { journal } =
        rollback_restore(&journal, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not rolled back")
    };
    let old = journal.original_verification.as_ref().unwrap();
    let mut evidence = reconciliation(&journal);
    evidence.image_sha256 = old.image_sha256.clone();
    evidence.published_sequence = old.published_sequence;
    assert!(matches!(
        finalize_rollback(&journal, &lease, &evidence, &f.filesystem()).unwrap(),
        RestoreOutcome::Finalized { .. }
    ));
    let done = restore_status(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(
        (done.resume_finalize_attempts, done.rollback_attempts),
        (12, 3)
    );
    assert!(!done.guard_present);
    assert!(done.artifacts.len() >= 3);
    assert_eq!(
        done.total_retained_bytes,
        done.artifacts.iter().map(|a| a.bytes).sum::<u64>()
    );
}
#[test]
fn restore_four_rollback_attempts_stop_all_mutation_without_resetting_shared_capacity() {
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    for _ in 0..4 {
        assert!(
            fl_store::recovery::readiness::finalize_rollback(
                &journal,
                &lease,
                &reconciliation(&journal),
                &f.filesystem()
            )
            .is_err()
        );
    }
    let path = journal.staging.join("journal.bin");
    let status = restore_status(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(
        (status.resume_finalize_attempts, status.rollback_attempts),
        (0, 4)
    );
    assert!(status.forensic_handling_required);
    let before = fs::read(&path).unwrap();
    assert!(resume_restore(&journal, &lease, &f.filesystem()).is_err());
    assert!(rollback_restore(&journal, &lease, &f.filesystem()).is_err());
    assert_eq!(fs::read(path).unwrap(), before);
    assert!(f.root.path().join("source.fls.restore-guard").exists());
}
fn scrubbed_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    for name in [
        "AGENT_PERSONA",
        "AGENT_NAME",
        "FL_HOSTED_ID",
        "AGENT_HYDRATION_GENERATION",
    ] {
        command.env_remove(name);
    }
    command
}
fn bounded_output(mut command: std::process::Command) -> std::process::Output {
    use std::time::{Duration, Instant};
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "fixture child exceeded 15s: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::yield_now();
    }
    child.wait_with_output().unwrap()
}
#[test]
fn restore_fresh_process_admission_probe() {
    let Some(path) = std::env::var_os("FL_RESTORE_ADMISSION_PROBE") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let before = fs::read(&path).ok();
    let identity = fs::metadata(&path).ok().map(|m| (m.dev(), m.ino()));
    let socket = fl_store::default_socket_path_for_store(&path);
    let trap = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    trap.set_nonblocking(true).unwrap();
    assert!(FerroStore::open(&path).is_err());
    assert!(FerroStore::auto(&path).is_err());
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let mut put = scrubbed_command(env!("CARGO_BIN_EXE_ferrostore"));
    put.arg("put")
        .arg("--store")
        .arg(&path)
        .args(["--create", "fixture", "id", "{}"]);
    let result = bounded_output(put);
    assert!(!result.status.success(), "fenced CLI put succeeded");
    assert!(String::from_utf8_lossy(&result.stderr).contains("fenced"));
    let serving = path.with_extension("serve.sock");
    let mut serve = scrubbed_command(env!("CARGO_BIN_EXE_ferrostore"));
    serve
        .arg("serve")
        .arg("--store")
        .arg(&path)
        .arg("--create")
        .arg("--socket")
        .arg(&serving);
    let result = bounded_output(serve);
    assert!(!result.status.success(), "fenced serve succeeded");
    assert!(String::from_utf8_lossy(&result.stderr).contains("fenced"));
    assert!(!serving.exists());
    assert_eq!(fs::read(&path).ok(), before);
    assert_eq!(
        fs::metadata(&path).ok().map(|m| (m.dev(), m.ino())),
        identity
    );
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
fn fresh_probe(path: &std::path::Path) {
    let mut child = scrubbed_command(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "restore_fresh_process_admission_probe",
            "--nocapture",
        ])
        .env("FL_RESTORE_ADMISSION_PROBE", path);
    let result = bounded_output(child);
    assert!(
        result.status.success(),
        "fresh admission probe: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}
use fl_store::recovery::{
    OperationLimits,
    fs::{Directory, Member, RecoveryEvent, RecoveryFs, RecoveryIo},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
fn mutation_boundary(e: RecoveryEvent) -> bool {
    matches!(
        e,
        RecoveryEvent::Create(
            Member::RestoreGuard
                | Member::RestoreJournal
                | Member::RestoreOperationLock
                | Member::Image
        ) | RecoveryEvent::Write(
            Member::RestoreGuard
                | Member::RestoreJournal
                | Member::RestoreOperationLock
                | Member::Image
                | Member::VerificationCopy
        ) | RecoveryEvent::Flush(
            Member::RestoreGuard
                | Member::RestoreJournal
                | Member::RestoreOperationLock
                | Member::Image
                | Member::Original
        ) | RecoveryEvent::Sync(
            Directory::RestoreGuardParent
                | Directory::RestoreJournalParent
                | Directory::RestoreOperationLockParent
                | Directory::RestoreDestinationParent
                | Directory::RestoreStagingParent
        ) | RecoveryEvent::Allocate
            | RecoveryEvent::CreateDirectory
            | RecoveryEvent::Exchange
            | RecoveryEvent::RestoreInstall
            | RecoveryEvent::RestoreRelocate
            | RecoveryEvent::RestoreReverseExchange
            | RecoveryEvent::RestoreGuardRemove
            | RecoveryEvent::RestoreOperationLock
            | RecoveryEvent::RenameEntry
    )
}
struct CutIo {
    at: usize,
    count: AtomicUsize,
    events: Mutex<Vec<RecoveryEvent>>,
    errno: i32,
}
impl CutIo {
    fn new(at: usize, errno: i32) -> Self {
        Self {
            at,
            count: AtomicUsize::new(0),
            events: Mutex::new(vec![]),
            errno,
        }
    }
}
impl RecoveryIo for CutIo {
    fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
        if mutation_boundary(e) {
            self.events.lock().unwrap().push(e);
            if self.count.fetch_add(1, Ordering::AcqRel) == self.at {
                return Err(std::io::Error::from_raw_os_error(self.errno));
            }
        }
        Ok(())
    }
}
struct CutFs<'a> {
    f: &'a Fixture,
    io: Arc<dyn RecoveryIo>,
    limits: OperationLimits,
}
impl RecoveryFs for CutFs<'_> {
    fn root(&self) -> &std::path::Path {
        &self.f.backups
    }
    fn io(&self) -> Arc<dyn RecoveryIo> {
        self.io.clone()
    }
    fn reconciliation_limits(&self) -> &OperationLimits {
        &self.limits
    }
}
#[test]
fn restore_exchange_and_reverse_exchange_recover_every_cut() {
    let trace = Fixture::new();
    let (request, lease) = prepare_restore(&trace, &trace.source);
    let hooks = Arc::new(CutIo::new(usize::MAX, libc::EIO));
    let filesystem = CutFs {
        f: &trace,
        io: hooks.clone(),
        limits: limits(),
    };
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &filesystem).unwrap()
    else {
        panic!("trace install failed")
    };
    let install = hooks.events.lock().unwrap().clone();
    assert!(install.contains(&RecoveryEvent::Flush(Member::Original)));
    assert!(install.contains(&RecoveryEvent::Exchange));
    assert!(install.contains(&RecoveryEvent::Sync(Directory::RestoreDestinationParent)));
    assert!(install.contains(&RecoveryEvent::Sync(Directory::RestoreStagingParent)));
    let hooks = Arc::new(CutIo::new(usize::MAX, libc::EIO));
    let filesystem = CutFs {
        f: &trace,
        io: hooks.clone(),
        limits: limits(),
    };
    rollback_restore(&journal, &lease, &filesystem).unwrap();
    let reverse = hooks.events.lock().unwrap().clone();
    assert!(reverse.contains(&RecoveryEvent::RestoreReverseExchange));
    for (reverse_mode, events) in [(false, install), (true, reverse)] {
        for (cut, event) in events.iter().enumerate() {
            let f = Fixture::new();
            let (request, lease) = prepare_restore(&f, &f.source);
            let old = fs::metadata(&f.source).unwrap().ino();
            let before = fs::read(&f.source).unwrap();
            let installed = if reverse_mode {
                let RestoreOutcome::Installed { journal } =
                    restore_offline(&request, &lease, &f.filesystem()).unwrap()
                else {
                    panic!("setup failed")
                };
                Some(journal)
            } else {
                None
            };
            let errno = match cut % 3 {
                0 => libc::EIO,
                1 => libc::ENOSPC,
                _ => libc::EDQUOT,
            };
            let hook = Arc::new(CutIo::new(cut, errno));
            let filesystem = CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits(),
            };
            let outcome = if let Some(j) = installed.as_ref() {
                rollback_restore(j, &lease, &filesystem)
            } else {
                restore_offline(&request, &lease, &filesystem)
            };
            assert!(
                outcome.is_err() || matches!(outcome, Ok(RestoreOutcome::OutcomeUnknown { .. })),
                "cut {cut} {event:?} did not interrupt"
            );
            assert!(
                hook.count.load(Ordering::Acquire) > cut,
                "missing cut {cut} {event:?}"
            );
            let guard = f.root.path().join("source.fls.restore-guard");
            if guard.exists() {
                fresh_probe(&f.source);
                if let Ok(value) =
                    serde_json::from_slice::<serde_json::Value>(&fs::read(&guard).unwrap())
                    && let Some(path) = value["journal"].as_str()
                    && let Ok(j) =
                        load_restore_journal(std::path::Path::new(path), &lease, &f.filesystem())
                {
                    let clock = ClockFs {
                        native: f.filesystem(),
                        now: j.binding.expires_at_unix_ms + 1,
                    };
                    let renewed = renewed_lease(&j, &clock);
                    let admitted =
                        load_restore_journal(std::path::Path::new(path), &renewed, &clock)
                            .expect("renewed lease must admit exact before/after intent layout");
                    let result = resume_restore(&admitted, &renewed, &clock);
                    assert!(
                        matches!(
                            result,
                            Ok(RestoreOutcome::Installed { .. }
                                | RestoreOutcome::RolledBack { .. })
                        ),
                        "cut {cut} {event:?}: {result:?}"
                    );
                }
            } else {
                assert_eq!(fs::metadata(&f.source).unwrap().ino(), old);
                assert_eq!(fs::read(&f.source).unwrap(), before);
            }
            eprintln!(
                "restore_cut reverse={reverse_mode} index={cut} event={event:?} errno={errno} fenced={}",
                guard.exists()
            );
        }
    }
}

struct ClockIo(u128);
impl fl_store::recovery::fs::RecoveryIo for ClockIo {
    fn unix_ms(&self) -> std::io::Result<u128> {
        Ok(self.0)
    }
}
struct ClockFs {
    native: fl_store::recovery::fs::NativeRecoveryFs,
    now: u128,
}
impl fl_store::recovery::fs::RecoveryFs for ClockFs {
    fn root(&self) -> &std::path::Path {
        fl_store::recovery::fs::RecoveryFs::root(&self.native)
    }
    fn reconciliation_limits(&self) -> &fl_store::recovery::OperationLimits {
        fl_store::recovery::fs::RecoveryFs::reconciliation_limits(&self.native)
    }
    fn io(&self) -> std::sync::Arc<dyn fl_store::recovery::fs::RecoveryIo> {
        std::sync::Arc::new(ClockIo(self.now))
    }
}
#[test]
fn restore_renewed_current_candidate_lease_survives_initial_expiry() {
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let clock = ClockFs {
        native: f.filesystem(),
        now: journal.binding.expires_at_unix_ms + 1,
    };
    let path = journal.staging.join("journal.bin");
    let before = fs::read(&path).unwrap();
    assert!(resume_restore(&journal, &lease, &clock).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    let mut renewed = journal.binding.clone();
    renewed.issued_at_unix_ms = clock.now;
    renewed.expires_at_unix_ms = clock.now + 600000;
    renewed.expected_file = inspect_file_binding(&f.source, &limits()).unwrap();
    let lease = create_offline_lease(&renewed, &clock, &limits()).unwrap();
    let RestoreOutcome::Installed { journal } = resume_restore(&journal, &lease, &clock)
        .expect("renewal must bind installed candidate rather than immutable original")
    else {
        panic!("not installed")
    };
    assert_eq!(journal.binding.expected_file, journal.original);
    let mut evidence = reconciliation(&journal);
    evidence.recorded_at_unix_ms = clock.now;
    assert!(matches!(
        fl_store::recovery::readiness::finalize_restore(&journal, &lease, &evidence, &clock)
            .unwrap(),
        RestoreOutcome::Finalized { .. }
    ));
    assert!(FerroStore::open(&f.source).is_ok());
}

fn renewed_lease(j: &RestoreJournal, clock: &ClockFs) -> OfflineLease {
    let mut binding = j.binding.clone();
    binding.issued_at_unix_ms = clock.now;
    binding.expires_at_unix_ms = clock.now + 600000;
    binding.expected_file = inspect_file_binding(&j.destination, &limits()).unwrap();
    create_offline_lease(&binding, clock, &limits()).unwrap()
}
fn exchange_fixture_names(a: &std::path::Path, b: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    let a = std::ffi::CString::new(a.as_os_str().as_bytes()).unwrap();
    let b = std::ffi::CString::new(b.as_os_str().as_bytes()).unwrap();
    assert_eq!(
        unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                a.as_ptr(),
                libc::AT_FDCWD,
                b.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        },
        0
    );
}
#[test]
fn restore_renewal_rejects_third_inode_changed_digest_and_phase_mismatch_before_attempt() {
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let mut clock = ClockFs {
        native: f.filesystem(),
        now: journal.binding.expires_at_unix_ms + 1,
    };
    let path = journal.staging.join("journal.bin");
    let before = fs::read(&path).unwrap();
    let held = f.root.path().join("held-candidate");
    fs::rename(&f.source, &held).unwrap();
    fs::copy(&held, &f.source).unwrap();
    let third = renewed_lease(&journal, &clock);
    assert!(resume_restore(&journal, &third, &clock).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    fs::remove_file(&f.source).unwrap();
    fs::rename(&held, &f.source).unwrap();
    clock.now += 1;
    let bytes = fs::read(&f.source).unwrap();
    let mut changed = bytes.clone();
    changed[4096] ^= 1;
    fs::write(&f.source, changed).unwrap();
    let changed = renewed_lease(&journal, &clock);
    assert!(resume_restore(&journal, &changed, &clock).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    fs::write(&f.source, bytes).unwrap();
    clock.now += 1;
    // Both names still carry known, checksum-valid generations, but this layout
    // cannot belong to Installed. A renewal cannot reinterpret the journal phase.
    let original = journal.staging.join("original.fls");
    exchange_fixture_names(&f.source, &original);
    let mismatched = renewed_lease(&journal, &clock);
    assert!(resume_restore(&journal, &mismatched, &clock).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    exchange_fixture_names(&f.source, &original);
    clock.now += 1;
    let current = renewed_lease(&journal, &clock);
    let RestoreOutcome::RolledBack { journal } =
        rollback_restore(&journal, &current, &clock).unwrap()
    else {
        panic!("not rolled back")
    };
    clock.now += 1;
    let original = renewed_lease(&journal, &clock);
    let mut evidence = reconciliation(&journal);
    let report = journal.original_verification.as_ref().unwrap();
    evidence.image_sha256 = report.image_sha256.clone();
    evidence.published_sequence = report.published_sequence;
    evidence.recorded_at_unix_ms = clock.now;
    assert!(matches!(
        fl_store::recovery::readiness::finalize_rollback(&journal, &original, &evidence, &clock)
            .unwrap(),
        RestoreOutcome::Finalized { .. }
    ));
    assert_eq!(
        restore_status(&path, &original, &clock).unwrap().phase,
        RestorePhase::OriginalFinalized
    );
}

#[test]
fn restore_attempt_expiry_stops_at_journal_boundary_without_unfencing_or_expanding_ceilings() {
    use std::sync::atomic::AtomicU64;
    struct Expire {
        now: AtomicU64,
        expiry: u64,
    }
    impl RecoveryIo for Expire {
        fn unix_ms(&self) -> std::io::Result<u128> {
            Ok(self.now.load(Ordering::Acquire) as u128)
        }
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::Sync(Directory::RestoreJournalParent) {
                self.now.store(self.expiry, Ordering::Release);
            }
            Ok(())
        }
    }
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let now = journal.binding.expires_at_unix_ms + 1;
    let clock = ClockFs {
        native: f.filesystem(),
        now,
    };
    let mut binding = journal.binding.clone();
    binding.issued_at_unix_ms = now;
    binding.expires_at_unix_ms = now + 500;
    binding.expected_file = inspect_file_binding(&f.source, &limits()).unwrap();
    let renewed = create_offline_lease(&binding, &clock, &limits()).unwrap();
    let io = Arc::new(Expire {
        now: AtomicU64::new(now as u64),
        expiry: binding.expires_at_unix_ms as u64,
    });
    let mut larger = limits();
    larger.byte_limit = 1 << 30;
    larger.memory_byte_limit = 2 << 30;
    let filesystem = CutFs {
        f: &f,
        io: io.clone(),
        limits: larger,
    };
    let bytes = fs::read(&f.source).unwrap();
    assert!(resume_restore(&journal, &renewed, &filesystem).is_err());
    assert_eq!(fs::read(&f.source).unwrap(), bytes);
    assert!(f.root.path().join("source.fls.restore-guard").exists());
    let clock = ClockFs {
        native: f.filesystem(),
        now: binding.expires_at_unix_ms + 1,
    };
    let renewed = renewed_lease(&journal, &clock);
    let status = restore_status(&journal.staging.join("journal.bin"), &renewed, &clock).unwrap();
    assert_eq!(status.resume_finalize_attempts, 1);
    assert_eq!(status.rollback_attempts, 0);
    assert_eq!(status.current_attempt, 1);
    assert_eq!(status.attempt_deadline_unix_ms, binding.expires_at_unix_ms);
    assert_eq!(status.time_remaining_ms, 0);
    assert_eq!(status.budget, journal.budget);
}

#[test]
fn restore_absence_finalize_recreated_destination_preserves_bytes_and_records_forensic_failure() {
    use fl_store::recovery::readiness::{AbsenceReconciliationRecord, finalize_original_absence};
    let f = Fixture::new();
    let writer = FerroStore::open(&f.source).unwrap();
    writer
        .put_doc(
            "fixture_unknown",
            "one",
            &serde_json::json!({"body":"x".repeat(20<<20)}),
        )
        .unwrap();
    drop(writer);
    let destination = f.root.path().join("recreated.fls");
    let (request, lease) = prepare_restore(&f, &destination);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let mut journal = journal;
    for _ in 0..3 {
        let RestoreOutcome::OriginalAbsenceRestored { journal: next } =
            rollback_restore(&journal, &lease, &f.filesystem()).unwrap()
        else {
            panic!("absence not restored")
        };
        journal = next;
    }
    let candidate = journal.staging.join("candidate.fls");
    let before = fs::read(&candidate).unwrap();
    fs::write(&destination, b"unexpected destination generation").unwrap();
    let evidence = AbsenceReconciliationRecord {
        version: 1,
        operation_id: journal.operation_id,
        candidate_sha256: journal.candidate_sha256.clone(),
        operator: "fixture operator".into(),
        recorded_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        no_client_opened_path_while_installed: true,
        evidence_sha256: "f".repeat(64),
    };
    assert!(finalize_original_absence(&journal, &lease, &evidence, &f.filesystem()).is_err());
    let path = journal.staging.join("journal.bin");
    let recorded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert!(
        recorded.forensic_handling_required,
        "recreated destination failure must be durably classified"
    );
    assert_eq!(recorded.rollback_attempts, 4);
    let log = fs::read(&path).unwrap();
    assert!(finalize_original_absence(&journal, &lease, &evidence, &f.filesystem()).is_err());
    assert_eq!(fs::read(path).unwrap(), log);
    assert!(
        !fs::read(destination.with_extension("fls.restore-guard"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(fs::read(&candidate).unwrap(), before);
    assert_eq!(
        fs::read(&destination).unwrap(),
        b"unexpected destination generation"
    );
}

#[test]
fn restore_restart_actions_wait_for_catalog_exclusion_before_recording_attempts() {
    use fl_store::recovery::readiness::*;
    use std::os::fd::AsRawFd;
    struct CancelWait(fl_store::recovery::CancelToken);
    impl RecoveryIo for CancelWait {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::CatalogLockWait {
                self.0.cancel();
            }
            Ok(())
        }
    }
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.backups.join(".locks/catalog.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let path = journal.staging.join("journal.bin");
    let before = fs::read(&path).unwrap();
    let absence = AbsenceReconciliationRecord {
        version: 1,
        operation_id: journal.operation_id,
        candidate_sha256: journal.candidate_sha256.clone(),
        operator: "fixture operator".into(),
        recorded_at_unix_ms: 1,
        no_client_opened_path_while_installed: true,
        evidence_sha256: "a".repeat(64),
    };
    for action in 0..4 {
        let l = limits();
        let filesystem = CutFs {
            f: &f,
            io: Arc::new(CancelWait(l.cancel.clone())),
            limits: l,
        };
        let result = match action {
            0 => resume_restore(&journal, &lease, &filesystem),
            1 => rollback_restore(&journal, &lease, &filesystem),
            2 => finalize_restore(&journal, &lease, &reconciliation(&journal), &filesystem),
            _ => finalize_original_absence(&journal, &lease, &absence, &filesystem),
        };
        assert!(
            matches!(
                result,
                Err(fl_store::recovery::RecoveryError::Cancelled { .. })
            ),
            "restart action {action} did not respect purge exclusion: {result:?}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    drop(held);
    assert!(resume_restore(&journal, &lease, &f.filesystem()).is_ok());
}

#[test]
fn restore_preflight_refuses_aliases_hardlinks_strict_budget_and_missing_capacity_before_guard() {
    use fl_store::recovery::inventory::AliasStatus;
    let f = Fixture::new();
    let (mut request, lease) = prepare_restore(&f, &f.source);
    let before = fs::read(&f.source).unwrap();
    let identity = fs::metadata(&f.source).unwrap().ino();
    let refused = |r: &RestoreRequest, filesystem: &dyn RecoveryFs| {
        assert!(restore_offline(r, &lease, filesystem).is_err());
        assert!(!f.source.with_extension("fls.restore-guard").exists());
        assert!(!fs::read_dir(f.root.path()).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".restore-")
        }));
        assert_eq!(fs::metadata(&f.source).unwrap().ino(), identity);
        assert_eq!(fs::read(&f.source).unwrap(), before);
    };
    request.store.alias_status = AliasStatus::Unknown;
    refused(&request, &f.filesystem());
    request.store.alias_status = AliasStatus::ReviewRequired;
    refused(&request, &f.filesystem());
    request.store.alias_status = AliasStatus::NoAliasObserved;
    let alias = f.root.path().join("alias.fls");
    fs::hard_link(&f.source, &alias).unwrap();
    refused(&request, &f.filesystem());
    fs::remove_file(alias).unwrap();
    let metadata = request.metadata_byte_limit;
    request.metadata_byte_limit = fs::metadata(&f.source).unwrap().len() * 2;
    refused(&request, &f.filesystem());
    request.metadata_byte_limit = metadata;
    let bytes = request.limits.byte_limit;
    request.limits.byte_limit = 4096;
    refused(&request, &f.filesystem());
    request.limits.byte_limit = bytes;
    request.space_margin_bytes = u64::MAX;
    refused(&request, &f.filesystem());
    request.space_margin_bytes = 4096;
    struct NoSpace;
    impl RecoveryIo for NoSpace {
        fn available_bytes(&self, _: &std::fs::File) -> std::io::Result<u64> {
            Ok(0)
        }
    }
    let filesystem = CutFs {
        f: &f,
        io: Arc::new(NoSpace),
        limits: limits(),
    };
    refused(&request, &filesystem);
    request.staging_parent = f.backups.clone();
    refused(&request, &f.filesystem());
}

#[test]
fn restore_operation_lock_is_private_journal_bound_and_retained() {
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin.lock");
    assert!(
        path.exists(),
        "restore operations need a stable cross-process sidecar lock"
    );
    let held = fs::metadata(&path).unwrap();
    assert_eq!(held.mode() & 0o777, 0o600);
    assert_eq!(held.nlink(), 1);
    assert_eq!(held.len(), 0);
    let metadata = serde_json::to_value(&journal).unwrap();
    assert_eq!(
        metadata["operation_lock_identity"],
        serde_json::json!([held.dev(), held.ino()])
    );
    let status = restore_status(
        &journal.staging.join("journal.bin"),
        &lease,
        &f.filesystem(),
    )
    .unwrap();
    assert!(
        status
            .artifacts
            .iter()
            .any(|a| a.path == path && a.identity == (held.dev(), held.ino()))
    );
    let replaced = journal.staging.join("held-operation-lock");
    fs::rename(&path, &replaced).unwrap();
    let initial = fs::read(journal.staging.join("journal.bin")).unwrap();
    assert!(resume_restore(&journal, &lease, &f.filesystem()).is_err());
    assert!(
        !path.exists(),
        "restart must never bootstrap a missing operation lock"
    );
    assert_eq!(
        fs::read(journal.staging.join("journal.bin")).unwrap(),
        initial
    );
    std::os::unix::fs::symlink(&replaced, &path).unwrap();
    assert!(resume_restore(&journal, &lease, &f.filesystem()).is_err());
    assert_eq!(
        fs::read(journal.staging.join("journal.bin")).unwrap(),
        initial
    );
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(journal.staging.join("journal.bin")).unwrap();
    assert!(matches!(resume_restore(&journal, &lease, &f.filesystem()),
        Err(fl_store::recovery::RecoveryError::Verification { operation_id }) if operation_id == journal.operation_id));
    assert_eq!(
        fs::read(journal.staging.join("journal.bin")).unwrap(),
        before
    );
    assert!(f.source.with_extension("fls.restore-guard").exists());
}

#[test]
fn restore_preguard_lock_failure_preserves_typed_cause_original_and_admission_metadata() {
    struct FailLock;
    impl RecoveryIo for FailLock {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::Flush(Member::RestoreOperationLock) {
                return Err(std::io::Error::from_raw_os_error(libc::ENOSPC));
            }
            Ok(())
        }
    }
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let bytes = fs::read(&f.source).unwrap();
    let original = fs::metadata(&f.source).unwrap().ino();
    let filesystem = CutFs {
        f: &f,
        io: Arc::new(FailLock),
        limits: limits(),
    };
    let error = restore_offline(&request, &lease, &filesystem).unwrap_err();
    assert!(std::error::Error::source(&error).and_then(|e|e.downcast_ref::<Box<fl_store::recovery::RecoveryError>>()).is_some_and(|e|matches!(e.as_ref(),fl_store::recovery::RecoveryError::Destination{operation_id}if *operation_id==request.operation_id)),"retained admission failure must preserve exact typed I/O cause: {error:?}");
    match &error {
        fl_store::recovery::RecoveryError::RestoreAdmissionArtifactsRetained {
            operation_id,
            staging,
            staging_identity,
            operation_lock_identity,
            source,
        } => {
            assert_eq!(*operation_id, request.operation_id);
            let held = fs::metadata(staging).unwrap();
            assert_eq!(*staging_identity, Some((held.dev(), held.ino())));
            assert_eq!(
                *operation_lock_identity, None,
                "failed lock admission must not invent a proven lock identity"
            );
            assert!(
                matches!(source.as_ref(),fl_store::recovery::RecoveryError::Destination{operation_id}if *operation_id==request.operation_id)
            );
            assert!(!error.to_string().contains(staging.to_str().unwrap()));
            assert!(staging.join("journal.bin.lock").exists());
        }
        _ => panic!("wrong admission failure variant: {error:?}"),
    }
    assert!(!f.source.with_extension("fls.restore-guard").exists());
    assert_eq!(fs::metadata(&f.source).unwrap().ino(), original);
    assert_eq!(fs::read(&f.source).unwrap(), bytes);
}

struct NativeWithIo {
    native: fl_store::recovery::fs::NativeRecoveryFs,
    io: Arc<dyn RecoveryIo>,
}
impl RecoveryFs for NativeWithIo {
    fn root(&self) -> &std::path::Path {
        self.native.root()
    }
    fn reconciliation_limits(&self) -> &OperationLimits {
        self.native.reconciliation_limits()
    }
    fn io(&self) -> Arc<dyn RecoveryIo> {
        self.io.clone()
    }
}
#[test]
fn restore_concurrent_resume_child() {
    let Some(input) = std::env::var_os("FL_RESTORE_RESUME_CHILD") else {
        return;
    };
    let input: serde_json::Value = serde_json::from_slice(&fs::read(input).unwrap()).unwrap();
    let root = std::path::PathBuf::from(input["root"].as_str().unwrap());
    let lease_path = std::path::PathBuf::from(input["lease"].as_str().unwrap());
    let journal: RestoreJournal = serde_json::from_value(input["journal"].clone()).unwrap();
    struct Notify {
        socket: std::path::PathBuf,
        sent: std::sync::atomic::AtomicBool,
    }
    impl RecoveryIo for Notify {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::RestoreOperationLockWait
                && !self.sent.swap(true, Ordering::AcqRel)
            {
                let mut socket = std::os::unix::net::UnixStream::connect(&self.socket)?;
                std::io::Write::write_all(&mut socket, b"waiting")?;
            }
            Ok(())
        }
    }
    let filesystem = NativeWithIo {
        native: fl_store::recovery::fs::NativeRecoveryFs::new(root, limits()),
        io: Arc::new(Notify {
            socket: input["wait_socket"].as_str().unwrap().into(),
            sent: std::sync::atomic::AtomicBool::new(false),
        }),
    };
    let lease = OfflineLease::load(&lease_path, &filesystem, &limits()).unwrap();
    let RestoreOutcome::Installed { journal } =
        resume_restore(&journal, &lease, &filesystem).unwrap()
    else {
        panic!("not installed")
    };
    assert_eq!(
        journal.current_attempt, 2,
        "waiting process must reread the first process's completed attempt"
    );
}
#[test]
fn restore_concurrent_process_resumes_serialize_and_reread_latest_journal() {
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{atomic::AtomicBool, mpsc},
        time::{Duration, Instant},
    };
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let wait_socket = f.root.path().join("wait.sock");
    let listener = std::os::unix::net::UnixListener::bind(&wait_socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let input = f.root.path().join("resume-child.json");
    fs::write(&input,serde_json::to_vec(&serde_json::json!({"root":f.backups,"lease":lease.path(),"journal":journal,"wait_socket":wait_socket})).unwrap()).unwrap();
    fs::set_permissions(&input, fs::Permissions::from_mode(0o600)).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    struct Pause {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        sent: AtomicBool,
    }
    impl RecoveryIo for Pause {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::Write(Member::RestoreJournal)
                && !self.sent.swap(true, Ordering::AcqRel)
            {
                self.entered.send(()).map_err(std::io::Error::other)?;
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(std::io::Error::other)?;
            }
            Ok(())
        }
    }
    std::thread::scope(|scope| {
        let filesystem = CutFs {
            f: &f,
            io: Arc::new(Pause {
                entered: entered_tx,
                release: Mutex::new(release_rx),
                sent: AtomicBool::new(false),
            }),
            limits: limits(),
        };
        let journal_ref = &journal;
        let lease_ref = &lease;
        let first = scope.spawn(move || resume_restore(journal_ref, lease_ref, &filesystem));
        entered_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let mut command = scrubbed_command(std::env::current_exe().unwrap());
        command
            .args(["--exact", "restore_concurrent_resume_child", "--nocapture"])
            .env("FL_RESTORE_RESUME_CHILD", &input)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match listener.accept() {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("child lock notification: {e}"),
            };
            assert!(
                child.try_wait().unwrap().is_none(),
                "child exited without waiting for operation lock"
            );
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("child did not reach operation-lock wait");
            }
            std::thread::yield_now();
        }
        release_tx.send(()).unwrap();
        assert!(matches!(
            first.join().unwrap().unwrap(),
            RestoreOutcome::Installed { .. }
        ));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("waiting resume child exceeded bound");
            }
            std::thread::yield_now();
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    });
    let status = restore_status(
        &journal.staging.join("journal.bin"),
        &lease,
        &f.filesystem(),
    )
    .unwrap();
    assert_eq!(
        (status.current_attempt, status.resume_finalize_attempts),
        (2, 2)
    );
}

#[test]
fn restore_preflight_gap_revalidates_selected_members_and_holds_operation_before_catalog() {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::AtomicBool;
    struct Gap {
        root: std::path::PathBuf,
        catalog_lock: std::path::PathBuf,
        validation: std::path::PathBuf,
        catalogs: AtomicUsize,
        checked: AtomicBool,
    }
    impl RecoveryIo for Gap {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::Create(Member::RestoreOperationLock) {
                let cat = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&self.catalog_lock)?;
                assert_eq!(
                    unsafe { libc::flock(cat.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0,
                    "catalogSH held while operation lock is created"
                );
            }
            if e == RecoveryEvent::CatalogLock && self.catalogs.fetch_add(1, Ordering::AcqRel) == 1
            {
                let staging = fs::read_dir(&self.root)?
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".restore-")
                    })
                    .unwrap();
                let operation = fs::File::open(staging.join("journal.bin.lock"))?;
                assert_eq!(
                    unsafe { libc::flock(operation.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    -1,
                    "catalog acquisition must occur under held operation lock"
                );
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EWOULDBLOCK)
                );
                let catalog = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&self.catalog_lock)?;
                assert_eq!(
                    unsafe { libc::flock(catalog.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
                fs::write(&self.validation, b"{}")?;
                self.checked.store(true, Ordering::Release);
            }
            Ok(())
        }
    }
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let before = fs::read(&f.source).unwrap();
    let io = Arc::new(Gap {
        root: f.root.path().to_path_buf(),
        catalog_lock: f.backups.join(".locks/catalog.lock"),
        validation: request.selected_bundle.path().join("validation.json"),
        catalogs: AtomicUsize::new(0),
        checked: AtomicBool::new(false),
    });
    let filesystem = CutFs {
        f: &f,
        io: io.clone(),
        limits: limits(),
    };
    let error = restore_offline(&request, &lease, &filesystem).unwrap_err();
    assert!(io.checked.load(Ordering::Acquire));
    assert!(
        matches!(error,fl_store::recovery::RecoveryError::RestoreAdmissionArtifactsRetained{source,..}if matches!(*source,fl_store::recovery::RecoveryError::Verification{..}))
    );
    assert_eq!(fs::read(&f.source).unwrap(), before);
    assert!(!f.source.with_extension("fls.restore-guard").exists());
}

#[test]
fn restore_new_operation_lock_cuts_keep_guard_absent_and_original_unchanged() {
    struct Fail {
        event: RecoveryEvent,
        errno: i32,
    }
    impl RecoveryIo for Fail {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == self.event {
                return Err(std::io::Error::from_raw_os_error(self.errno));
            }
            Ok(())
        }
    }
    let events = [
        RecoveryEvent::RestoreOperationLock,
        RecoveryEvent::Create(Member::RestoreOperationLock),
        RecoveryEvent::Flush(Member::RestoreOperationLock),
        RecoveryEvent::Sync(Directory::RestoreOperationLockParent),
    ];
    for (index, event) in events.into_iter().enumerate() {
        let f = Fixture::new();
        let (request, lease) = prepare_restore(&f, &f.source);
        let before = fs::read(&f.source).unwrap();
        let identity = fs::metadata(&f.source).unwrap().ino();
        let errno = match index % 3 {
            0 => libc::EIO,
            1 => libc::ENOSPC,
            _ => libc::EDQUOT,
        };
        let filesystem = CutFs {
            f: &f,
            io: Arc::new(Fail { event, errno }),
            limits: limits(),
        };
        let error = restore_offline(&request, &lease, &filesystem).unwrap_err();
        assert!(
            matches!(error,fl_store::recovery::RecoveryError::RestoreAdmissionArtifactsRetained{staging_identity:Some(_),operation_lock_identity:None,source,..}if matches!(*source,fl_store::recovery::RecoveryError::Destination{..})),
            "new operation-lock cut {event:?} lost exact admission context"
        );
        assert!(!f.source.with_extension("fls.restore-guard").exists());
        assert_eq!(fs::metadata(&f.source).unwrap().ino(), identity);
        assert_eq!(fs::read(&f.source).unwrap(), before);
        eprintln!(
            "restore_admission_lock_cut event={event:?} errno={errno} source_unchanged=true guard_absent=true"
        );
    }
}

fn generation_evidence(
    j: &RestoreJournal,
    original: bool,
) -> fl_store::recovery::readiness::ReconciliationRecord {
    let mut evidence = reconciliation(j);
    if original {
        let report = j.original_verification.as_ref().unwrap();
        evidence.image_sha256 = report.image_sha256.clone();
        evidence.published_sequence = report.published_sequence;
    }
    evidence
}
fn exercise_finalization_cuts(all: bool) {
    use fl_store::recovery::readiness::{finalize_restore, finalize_rollback};
    for original in [false, true] {
        let trace = Fixture::new();
        let (request, lease) = prepare_restore(&trace, &trace.source);
        let RestoreOutcome::Installed { mut journal } =
            restore_offline(&request, &lease, &trace.filesystem()).unwrap()
        else {
            panic!("not installed")
        };
        if original {
            let RestoreOutcome::RolledBack { journal: next } =
                rollback_restore(&journal, &lease, &trace.filesystem()).unwrap()
            else {
                panic!("not rolled back")
            };
            journal = next;
        }
        let io = Arc::new(CutIo::new(usize::MAX, libc::EIO));
        let filesystem = CutFs {
            f: &trace,
            io: io.clone(),
            limits: limits(),
        };
        let evidence = generation_evidence(&journal, original);
        let done = if original {
            finalize_rollback(&journal, &lease, &evidence, &filesystem)
        } else {
            finalize_restore(&journal, &lease, &evidence, &filesystem)
        };
        assert!(matches!(done.unwrap(), RestoreOutcome::Finalized { .. }));
        let events = io.events.lock().unwrap().clone();
        assert!(events.contains(&RecoveryEvent::RestoreGuardRemove));
        assert!(events.contains(&RecoveryEvent::Sync(Directory::RestoreGuardParent)));
        for (cut, event) in events.iter().enumerate() {
            if !all
                && !matches!(
                    event,
                    RecoveryEvent::RestoreGuardRemove
                        | RecoveryEvent::Sync(Directory::RestoreGuardParent)
                )
            {
                continue;
            }
            let f = Fixture::new();
            let (request, lease) = prepare_restore(&f, &f.source);
            let RestoreOutcome::Installed { mut journal } =
                restore_offline(&request, &lease, &f.filesystem()).unwrap()
            else {
                panic!("not installed")
            };
            if original {
                let RestoreOutcome::RolledBack { journal: next } =
                    rollback_restore(&journal, &lease, &f.filesystem()).unwrap()
                else {
                    panic!("not rolled back")
                };
                journal = next;
            }
            let before = fs::read(&f.source).unwrap();
            let inode = fs::metadata(&f.source).unwrap().ino();
            let errno = match cut % 3 {
                0 => libc::EIO,
                1 => libc::ENOSPC,
                _ => libc::EDQUOT,
            };
            let io = Arc::new(CutIo::new(cut, errno));
            let filesystem = CutFs {
                f: &f,
                io: io.clone(),
                limits: limits(),
            };
            let evidence = generation_evidence(&journal, original);
            let result = if original {
                finalize_rollback(&journal, &lease, &evidence, &filesystem)
            } else {
                finalize_restore(&journal, &lease, &evidence, &filesystem)
            };
            assert!(
                result.is_err() || matches!(result, Ok(RestoreOutcome::OutcomeUnknown { .. })),
                "finalize cut {cut} {event:?} did not interrupt"
            );
            assert!(io.count.load(Ordering::Acquire) > cut);
            assert_eq!(fs::metadata(&f.source).unwrap().ino(), inode);
            assert_eq!(fs::read(&f.source).unwrap(), before);
            if f.source.with_extension("fls.restore-guard").exists() {
                fresh_probe(&f.source);
                if let Ok(current) = load_restore_journal(
                    &journal.staging.join("journal.bin"),
                    &lease,
                    &f.filesystem(),
                ) {
                    let evidence = generation_evidence(&current, original);
                    let result = if original {
                        finalize_rollback(&current, &lease, &evidence, &f.filesystem())
                    } else {
                        finalize_restore(&current, &lease, &evidence, &f.filesystem())
                    };
                    assert!(matches!(result.unwrap(), RestoreOutcome::Finalized { .. }));
                }
            } else {
                assert!(
                    matches!(result, Ok(RestoreOutcome::OutcomeUnknown { .. })),
                    "guard removal sync failure must remain outcome-unknown"
                );
                let status = restore_status(
                    &journal.staging.join("journal.bin"),
                    &lease,
                    &f.filesystem(),
                )
                .unwrap();
                assert_eq!(
                    status.phase,
                    if original {
                        RestorePhase::OriginalFinalized
                    } else {
                        RestorePhase::Finalized
                    }
                );
            }
            eprintln!(
                "restore_finalize_cut original={original} index={cut} event={event:?} errno={errno}"
            );
        }
    }
}
#[test]
fn restore_selected_and_original_finalize_recover_every_completion_and_guard_cut() {
    exercise_finalization_cuts(true);
}
#[test]
fn restore_guard_removal_and_sync_report_unknown_for_both_generation_roles() {
    exercise_finalization_cuts(false);
}

#[test]
fn restore_absent_noreplace_reverse_and_absence_finalize_native_cuts() {
    use fl_store::recovery::readiness::{AbsenceReconciliationRecord, finalize_original_absence};
    struct Cut {
        transition: RecoveryEvent,
        event: RecoveryEvent,
        seen: std::sync::atomic::AtomicBool,
        fired: std::sync::atomic::AtomicBool,
        flushes: AtomicUsize,
        second_flush: bool,
    }
    impl RecoveryIo for Cut {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == self.transition {
                self.seen.store(true, Ordering::Release);
            }
            if self.seen.load(Ordering::Acquire) && e == self.event {
                if self.second_flush && self.flushes.fetch_add(1, Ordering::AcqRel) != 1 {
                    return Ok(());
                }
                self.fired.store(true, Ordering::Release);
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
            Ok(())
        }
    }
    let f = Fixture::new();
    let writer = FerroStore::open(&f.source).unwrap();
    writer
        .put_doc(
            "fixture_unknown",
            "one",
            &serde_json::json!({"body":"x".repeat(20<<20)}),
        )
        .unwrap();
    drop(writer);
    let bundle = f.bundle();
    let operation =
        uuid::Uuid::parse_str(bundle.path().file_name().unwrap().to_str().unwrap()).unwrap();
    let transitions = [
        RecoveryEvent::RestoreInstall,
        RecoveryEvent::RestoreRelocate,
        RecoveryEvent::RestoreOperationLock,
    ];
    let cases = [
        (0, RecoveryEvent::RestoreInstall, false),
        (
            0,
            RecoveryEvent::Sync(Directory::RestoreDestinationParent),
            false,
        ),
        (
            0,
            RecoveryEvent::Sync(Directory::RestoreStagingParent),
            false,
        ),
        (0, RecoveryEvent::Flush(Member::RestoreJournal), false),
        (1, RecoveryEvent::RestoreRelocate, false),
        (
            1,
            RecoveryEvent::Sync(Directory::RestoreDestinationParent),
            false,
        ),
        (
            1,
            RecoveryEvent::Sync(Directory::RestoreStagingParent),
            false,
        ),
        (1, RecoveryEvent::Flush(Member::RestoreJournal), false),
        (2, RecoveryEvent::Flush(Member::RestoreJournal), true),
        (2, RecoveryEvent::RestoreGuardRemove, false),
        (2, RecoveryEvent::Sync(Directory::RestoreGuardParent), false),
    ];
    for (index, (mode, event, second_flush)) in cases.into_iter().enumerate() {
        let destination = f.root.path().join(format!("absent-cut-{index}.fls"));
        let selected = fl_store::recovery::catalog::select_completed_bundle(
            operation,
            &f.filesystem(),
            &limits(),
        )
        .unwrap();
        let (request, lease) = prepare_restore_bundle(&f, &destination, selected);
        let io = Arc::new(Cut {
            transition: transitions[mode],
            event,
            seen: std::sync::atomic::AtomicBool::new(false),
            fired: std::sync::atomic::AtomicBool::new(false),
            flushes: AtomicUsize::new(0),
            second_flush,
        });
        let filesystem = CutFs {
            f: &f,
            io: io.clone(),
            limits: limits(),
        };
        let result = if mode == 0 {
            restore_offline(&request, &lease, &filesystem)
        } else {
            let RestoreOutcome::Installed { journal } =
                restore_offline(&request, &lease, &f.filesystem()).unwrap()
            else {
                panic!("not installed")
            };
            if mode == 1 {
                rollback_restore(&journal, &lease, &filesystem)
            } else {
                let RestoreOutcome::OriginalAbsenceRestored { journal } =
                    rollback_restore(&journal, &lease, &f.filesystem()).unwrap()
                else {
                    panic!("absence not restored")
                };
                let evidence = AbsenceReconciliationRecord {
                    version: 1,
                    operation_id: journal.operation_id,
                    candidate_sha256: journal.candidate_sha256.clone(),
                    operator: "fixture operator".into(),
                    recorded_at_unix_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis(),
                    no_client_opened_path_while_installed: true,
                    evidence_sha256: "f".repeat(64),
                };
                finalize_original_absence(&journal, &lease, &evidence, &filesystem)
            }
        };
        assert!(
            io.fired.load(Ordering::Acquire),
            "missing absent cut mode={mode} event={event:?}"
        );
        assert!(result.is_err() || matches!(result, Ok(RestoreOutcome::OutcomeUnknown { .. })));
        let guard = destination.with_extension("fls.restore-guard");
        if guard.exists() {
            fresh_probe(&destination);
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(&guard).unwrap()).unwrap();
            let path = std::path::Path::new(value["journal"].as_str().unwrap());
            let journal = load_restore_journal(path, &lease, &f.filesystem()).unwrap();
            let clock = ClockFs {
                native: f.filesystem(),
                now: journal.binding.expires_at_unix_ms + 1,
            };
            let renewed = renewed_lease(&journal, &clock);
            let journal = load_restore_journal(path, &renewed, &clock)
                .expect("renewal must admit exact absent transition layout");
            if mode == 0 {
                assert!(matches!(
                    resume_restore(&journal, &renewed, &clock).unwrap(),
                    RestoreOutcome::Installed { .. }
                ));
            } else if mode == 1 {
                assert!(matches!(
                    rollback_restore(&journal, &renewed, &clock).unwrap(),
                    RestoreOutcome::OriginalAbsenceRestored { .. }
                ));
                assert!(!destination.exists());
            } else {
                let evidence = AbsenceReconciliationRecord {
                    version: 1,
                    operation_id: journal.operation_id,
                    candidate_sha256: journal.candidate_sha256.clone(),
                    operator: "fixture operator".into(),
                    recorded_at_unix_ms: clock.now,
                    no_client_opened_path_while_installed: true,
                    evidence_sha256: "f".repeat(64),
                };
                assert!(matches!(
                    finalize_original_absence(&journal, &renewed, &evidence, &clock).unwrap(),
                    RestoreOutcome::OriginalAbsenceFinalized { .. }
                ));
                assert!(!destination.exists());
                assert!(journal.staging.join("candidate.fls").exists());
            }
        } else {
            assert!(matches!(result, Ok(RestoreOutcome::OutcomeUnknown { .. })));
            assert!(!destination.exists());
        }
        eprintln!("restore_absent_cut mode={mode} event={event:?} renewed_exact_layout=true");
    }
}

#[test]
fn restore_existing_bundle_larger_than_allocation_ceiling_survives_restart_actions() {
    use fl_store::recovery::{CancelToken, backup::*, fs::NativeRecoveryFs};
    use std::os::unix::fs::PermissionsExt;
    for rollback in [false, true] {
        let f = Fixture::new();
        fs::remove_file(&f.source).unwrap();
        let owner = FerroStore::open(&f.source).unwrap();
        for n in 0..64 {
            owner
                .put_doc(
                    "fixture_unknown",
                    "one",
                    &serde_json::json!({"n":n,"body":"x".repeat(256<<10)}),
                )
                .unwrap();
        }
        let config = f.root.path().join("fixture-config.toml");
        fs::write(&config, vec![b'x'; 40 << 20]).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let key = f.root.path().join("excluded-fixture.key");
        fs::write(&key, b"not a live key").unwrap();
        let config_id = fs::metadata(&config).unwrap();
        let key_id = fs::metadata(&key).unwrap();
        let mut backup_request = f.backup_request();
        backup_request.limits.memory_byte_limit = 4 << 30;
        backup_request.metadata_byte_limit = 64 << 20;
        backup_request.configs = vec![ApprovedConfig {
            kind: fl_store::recovery::manifest::MemberKind::ProtectedConfig,
            path: config,
            identity: (config_id.dev(), config_id.ino()),
            byte_limit: 40 << 20,
        }];
        backup_request.excluded_vault_key = Some(ExcludedVaultKey {
            path: key,
            identity: (key_id.dev(), key_id.ino()),
        });
        let filesystem = NativeRecoveryFs::new(f.backups.clone(), backup_request.limits.clone());
        let PublicationOutcome::Durable(bundle) =
            backup(&owner, &backup_request, &filesystem, &CancelToken::new()).unwrap()
        else {
            panic!("backup not durable")
        };
        drop(owner);
        let (mut request, lease) = prepare_restore_bundle(&f, &f.source, bundle);
        request.limits.memory_byte_limit = 4 << 30;
        request.metadata_byte_limit = 17 << 20;
        let RestoreOutcome::Installed { journal } =
            restore_offline(&request, &lease, &filesystem).unwrap()
        else {
            panic!("not installed")
        };
        assert!(journal.budget.backup_bytes > journal.budget.allocation_ceiling);
        assert!(
            journal.budget.candidate_bytes + journal.budget.metadata_bytes
                <= journal.budget.allocation_ceiling
        );
        let original_budget = journal.budget.clone();
        let RestoreOutcome::Installed { journal } = resume_restore(&journal, &lease, &filesystem)
            .expect("existing bundle bytes must not be limited by new allocation ceiling")
        else {
            panic!("not resumed")
        };
        assert_eq!(journal.budget, original_budget);
        if rollback {
            let RestoreOutcome::RolledBack { journal } =
                rollback_restore(&journal, &lease, &filesystem).unwrap()
            else {
                panic!("not rolled back")
            };
            assert_eq!(journal.budget, original_budget);
            let old = journal.original_verification.as_ref().unwrap();
            let mut evidence = reconciliation(&journal);
            evidence.image_sha256 = old.image_sha256.clone();
            evidence.published_sequence = old.published_sequence;
            assert!(matches!(
                fl_store::recovery::readiness::finalize_rollback(
                    &journal,
                    &lease,
                    &evidence,
                    &filesystem
                )
                .unwrap(),
                RestoreOutcome::Finalized { .. }
            ));
        } else {
            assert!(matches!(
                fl_store::recovery::readiness::finalize_restore(
                    &journal,
                    &lease,
                    &reconciliation(&journal),
                    &filesystem
                )
                .unwrap(),
                RestoreOutcome::Finalized { .. }
            ));
        }
        let status =
            restore_status(&journal.staging.join("journal.bin"), &lease, &filesystem).unwrap();
        assert_eq!(status.budget, original_budget);
    }
}
