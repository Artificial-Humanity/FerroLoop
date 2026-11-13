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
        original_profile: fl_store::recovery::ValidationProfile::new(
            fl_store::recovery::ProfileKind::SmallEmpty,
            f.identity().catalog_key,
        ),
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
            original_profile: fl_store::recovery::ValidationProfile::new(
                fl_store::recovery::ProfileKind::SmallEmpty,
                f.identity().catalog_key,
            ),
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
        image_sha256: j.verification.as_ref().unwrap().image_sha256.clone(),
        published_sequence: j.verification.as_ref().unwrap().published_sequence,
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
    // Each cut interrupts real unfinished relocation work; completed Installed
    // resumes are free and cannot exercise shared attempt exhaustion.
    let (f, journal, lease) = bundleless_fixture_with_bundle(BundlelessLayout::Relocated, false);
    let mut bad = reconciliation(&journal);
    bad.clients.caches_invalidated = false;
    assert!(finalize_restore(&journal, &lease, &bad, &f.filesystem()).is_err());
    assert_eq!(
        restore_status(
            &journal.staging.join("journal.bin"),
            &lease,
            &f.filesystem()
        )
        .unwrap()
        .current_attempt,
        0
    );
    for _ in 0..12 {
        let hook = Arc::new(TerminalCutIo::new(RecoveryEvent::Sync(
            Directory::RestoreDestinationParent,
        )));
        let filesystem = CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        assert!(resume_restore(&journal, &lease, &filesystem).is_err());
        assert_eq!(hook.fired.load(Ordering::Acquire), 1);
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
        (12, 2)
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
        let hook = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreReverseExchange));
        let filesystem = CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        assert!(rollback_restore(&journal, &lease, &filesystem).is_err());
        assert_eq!(hook.fired.load(Ordering::Acquire), 1);
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
        std::thread::sleep(Duration::from_millis(5));
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
                | Member::RestorePin
                | Member::RestoreJournal
                | Member::RestoreOperationLock
                | Member::Image
        ) | RecoveryEvent::Write(
            Member::RestoreGuard
                | Member::RestorePin
                | Member::RestoreJournal
                | Member::RestoreOperationLock
                | Member::Image
                | Member::VerificationCopy
        ) | RecoveryEvent::Flush(
            Member::RestoreGuard
                | Member::RestorePin
                | Member::RestoreJournal
                | Member::RestoreOperationLock
                | Member::Image
                | Member::Original
        ) | RecoveryEvent::Sync(
            Directory::RestoreGuardParent
                | Directory::RestorePinParent
                | Directory::CatalogParent
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
            | RecoveryEvent::RestorePinRemove
            | RecoveryEvent::RestoreOperationLock
            | RecoveryEvent::RenameEntry
            | RecoveryEvent::Publish
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
// The default schedule samples repeated I/O; exhaustive mode preserves every cut.
fn restore_cut_schedule(events: &[RecoveryEvent], exhaustive: bool) -> Vec<usize> {
    if exhaustive {
        return (0..events.len()).collect();
    }
    let mut cuts = std::collections::BTreeSet::new();
    for (index, event) in events.iter().enumerate() {
        // Keep both ends of each repeated I/O kind: admission and completion
        // have different recovery obligations even when the hook is identical.
        if !events[..index].contains(event) || !events[index + 1..].contains(event) {
            cuts.insert(index);
        }
        // Exercise the intent immediately before and durability immediately
        // after each namespace/authority action, including intermediate ones.
        if matches!(
            event,
            RecoveryEvent::Exchange
                | RecoveryEvent::RestoreInstall
                | RecoveryEvent::RestoreRelocate
                | RecoveryEvent::RestoreReverseExchange
                | RecoveryEvent::RestoreGuardRemove
                | RecoveryEvent::RestorePinRemove
                | RecoveryEvent::RestoreOperationLock
                | RecoveryEvent::RenameEntry
                | RecoveryEvent::Publish
        ) {
            cuts.extend(index.saturating_sub(1)..=(index + 1).min(events.len() - 1));
        }
    }
    // Preserve the old cut-index errno mapping, including all three errors
    // whenever the full lane contains them.
    for residue in 0..events.len().min(3) {
        if !cuts.iter().any(|cut| cut % 3 == residue) {
            cuts.insert(residue);
        }
    }
    cuts.into_iter().collect()
}

#[test]
fn restore_cut_schedule_preserves_actions_durability_and_errno_coverage() {
    // Repeated journal writes separate intent/action/durability neighborhoods.
    let mut events = vec![RecoveryEvent::Write(Member::RestoreJournal); 60];
    events[10] = RecoveryEvent::Flush(Member::RestoreJournal);
    events[11] = RecoveryEvent::Exchange;
    events[12] = RecoveryEvent::Sync(Directory::RestoreDestinationParent);
    events[28] = RecoveryEvent::Flush(Member::RestoreJournal);
    events[29] = RecoveryEvent::RestoreRelocate;
    events[30] = RecoveryEvent::Sync(Directory::RestoreStagingParent);
    events[43] = RecoveryEvent::RestoreReverseExchange;
    events[50] = RecoveryEvent::RestoreGuardRemove;
    events[55] = RecoveryEvent::RestorePinRemove;
    let cuts = restore_cut_schedule(&events, false);
    assert!(
        cuts.len() < events.len() / 2,
        "default must sample repeated I/O"
    );
    for required in [
        0, 10, 11, 12, 28, 29, 30, 42, 43, 44, 49, 50, 51, 54, 55, 56, 59,
    ] {
        assert!(
            cuts.contains(&required),
            "missing action/durability cut {required}"
        );
    }
    for event in &events {
        assert!(
            cuts.iter().any(|&cut| events[cut] == *event),
            "missing {event:?}"
        );
    }
    assert!(cuts.windows(2).all(|pair| pair[0] < pair[1]));
    for residue in 0..3 {
        assert!(
            cuts.iter().any(|cut| cut % 3 == residue),
            "missing errno {residue}"
        );
    }
    for len in 0..10 {
        let repeated = vec![RecoveryEvent::Write(Member::RestoreJournal); len];
        let cuts = restore_cut_schedule(&repeated, false);
        for residue in 0..len.min(3) {
            assert!(cuts.iter().any(|cut| cut % 3 == residue));
        }
        assert!(cuts.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(cuts.iter().all(|&cut| cut < len));
    }
}

#[test]
fn restore_cut_schedule_exhaustive_keeps_repeated_events_and_every_index() {
    for len in [0, 1, 2, 3, 17, 120] {
        let events = vec![RecoveryEvent::Flush(Member::RestoreJournal); len];
        assert_eq!(
            restore_cut_schedule(&events, true),
            (0..len).collect::<Vec<_>>()
        );
    }
}

#[test]
fn restore_exchange_and_reverse_exchange_recover_every_cut() {
    let trace = journal_fixture();
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
        let exhaustive = std::env::var("FL_RECOVERY_EXHAUSTIVE").as_deref() == Ok("1");
        let cuts = restore_cut_schedule(&events, exhaustive);
        eprintln!(
            "restore_cut_schedule reverse={reverse_mode} exhaustive={exhaustive} selected={} full={} indices={cuts:?}",
            cuts.len(),
            events.len()
        );
        for cut in cuts {
            let event = &events[cut];
            let f = journal_fixture();
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
                let value: serde_json::Value = serde_json::from_slice(&fs::read(&guard).unwrap())
                    .expect("present guard must contain typed journal binding");
                let path = value["journal"].as_str().expect("guard journal path");
                let j = load_restore_journal(std::path::Path::new(path), &lease, &f.filesystem())
                    .expect(
                        "present guard MUST have loadable admitted journal; matrix cannot skip",
                    );
                if j.phase == RestorePhase::Admitted {
                    assert!(matches!(
                        abandon_restore(&j, &lease, &f.filesystem()).unwrap(),
                        RestoreOutcome::Abandoned { .. }
                    ));
                } else {
                    let clock = ClockFs {
                        native: f.filesystem(),
                        now: j.binding.expires_at_unix_ms + 1,
                    };
                    let renewed = renewed_lease(&j, &clock);
                    let admitted =
                        load_restore_journal(std::path::Path::new(path), &renewed, &clock)
                            .expect("renewed lease must admit exact before/after intent layout");
                    // Durable reverse intent must be continued by typed rollback;
                    // generic resume is not authority to cancel that decision.
                    let result = if admitted.phase == RestorePhase::ReverseIntent {
                        rollback_restore(&admitted, &renewed, &clock)
                    } else {
                        resume_restore(&admitted, &renewed, &clock)
                    };
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
    assert_restore_attempt_expiry_at_journal_boundary(false);
}

#[test]
fn restore_attempt_expiry_after_preattempt_stall_still_charges_at_journal_boundary() {
    assert_restore_attempt_expiry_at_journal_boundary(true);
}

fn assert_restore_attempt_expiry_at_journal_boundary(delay_before_attempt: bool) {
    use std::sync::atomic::AtomicU64;
    struct Expire {
        now: AtomicU64,
        expiry: u64,
        delay_before_attempt: bool,
        stalled: AtomicBool,
        fired: AtomicBool,
    }
    impl RecoveryIo for Expire {
        fn unix_ms(&self) -> std::io::Result<u128> {
            Ok(self.now.load(Ordering::Acquire) as u128)
        }
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            // This event follows the lease's conversion into a real-time cap.
            // A slow precheck must not accidentally consume the expiry scenario.
            if e == RecoveryEvent::RestoreOperationLock
                && self.delay_before_attempt
                && !self.stalled.swap(true, Ordering::AcqRel)
            {
                std::thread::sleep(std::time::Duration::from_millis(750));
            }
            if e == RecoveryEvent::Sync(Directory::RestoreJournalParent) {
                self.fired.store(true, Ordering::Release);
                self.now.store(self.expiry, Ordering::Release);
            }
            Ok(())
        }
    }
    // Interrupt a real install before exchange and load its durable intent.
    // An Installed journal is already complete and resume must be a free no-op.
    let (f, journal, _lease) =
        bundleless_fixture_with_bundle(BundlelessLayout::BeforeExchange, false);
    assert_eq!(journal.phase, RestorePhase::ExchangeIntent);
    assert_eq!(journal.resume_finalize_attempts, 0);
    let now = journal.binding.expires_at_unix_ms + 1;
    let clock = ClockFs {
        native: f.filesystem(),
        now,
    };
    let mut binding = journal.binding.clone();
    binding.issued_at_unix_ms = now;
    // Keep real-time headroom within the fixture's 180-second operation limit;
    // the injected journal-sync clock, rather than runner speed, causes expiry.
    binding.expires_at_unix_ms = now + 60_000;
    binding.expected_file = inspect_file_binding(&f.source, &limits()).unwrap();
    let renewed = create_offline_lease(&binding, &clock, &limits()).unwrap();
    let io = Arc::new(Expire {
        now: AtomicU64::new(now as u64),
        expiry: binding.expires_at_unix_ms as u64,
        delay_before_attempt,
        stalled: AtomicBool::new(false),
        fired: AtomicBool::new(false),
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
    assert_eq!(io.stalled.load(Ordering::Acquire), delay_before_attempt);
    assert!(
        io.fired.load(Ordering::Acquire),
        "expiry must occur at the charged attempt's journal sync"
    );
    assert_eq!(fs::read(&f.source).unwrap(), bytes);
    assert!(f.root.path().join("source.fls.restore-guard").exists());
    let clock = ClockFs {
        native: f.filesystem(),
        now: binding.expires_at_unix_ms + 1,
    };
    let renewed = renewed_lease(&journal, &clock);
    let status = restore_status(&journal.staging.join("journal.bin"), &renewed, &clock).unwrap();
    assert_eq!(
        status.resume_finalize_attempts, 1,
        "the unfinished resume must charge its attempt before injected expiry"
    );
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
            ..
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
fn concurrent_resume_limits() -> OperationLimits {
    let mut l = limits();
    l.deadline = std::time::Instant::now() + std::time::Duration::from_secs(480);
    l
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
        native: fl_store::recovery::fs::NativeRecoveryFs::new(root, concurrent_resume_limits()),
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
    let (mut request, lease) = prepare_restore(&f, &f.source);
    request.limits = concurrent_resume_limits();
    request.limits.operation_id = request.operation_id;
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
                    .recv_timeout(Duration::from_secs(240))
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
            limits: concurrent_resume_limits(),
        };
        let journal_ref = &journal;
        let lease_ref = &lease;
        let first = scope.spawn(move || resume_restore(journal_ref, lease_ref, &filesystem));
        entered_rx.recv_timeout(Duration::from_secs(120)).unwrap();
        let mut command = scrubbed_command(std::env::current_exe().unwrap());
        command
            .args(["--exact", "restore_concurrent_resume_child", "--nocapture"])
            .env("FL_RESTORE_RESUME_CHILD", &input)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let mut message = [0; 7];
                    std::io::Read::read_exact(&mut socket, &mut message).unwrap();
                    assert_eq!(&message, b"waiting");
                    assert!(
                        child.try_wait().unwrap().is_none(),
                        "child passed held lock before release"
                    );
                    break;
                }
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
            std::thread::park_timeout(Duration::from_millis(5));
        }
        release_tx.send(()).unwrap();
        assert!(matches!(
            first.join().unwrap().unwrap(),
            RestoreOutcome::Installed { .. }
        ));
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("waiting resume child exceeded bound");
            }
            std::thread::park_timeout(Duration::from_millis(5));
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
        let trace = journal_fixture();
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
        let exhaustive = std::env::var("FL_RECOVERY_EXHAUSTIVE").as_deref() == Ok("1");
        let cuts = if all {
            restore_cut_schedule(&events, exhaustive)
        } else {
            events
                .iter()
                .enumerate()
                .filter_map(|(cut, event)| {
                    matches!(
                        event,
                        RecoveryEvent::RestoreGuardRemove
                            | RecoveryEvent::Sync(Directory::RestoreGuardParent)
                    )
                    .then_some(cut)
                })
                .collect()
        };
        eprintln!(
            "restore_finalize_schedule original={original} all={all} exhaustive={exhaustive} selected={} full={} indices={cuts:?}",
            cuts.len(),
            events.len()
        );
        for cut in cuts {
            let event = &events[cut];
            let f = journal_fixture();
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
                {
                    let current = load_restore_journal(
                        &journal.staging.join("journal.bin"),
                        &lease,
                        &f.filesystem(),
                    )
                    .expect("present guard MUST have loadable finalization journal");
                    // Retry the exact pre-cut record: its timestamp is accepted evidence.
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

#[test]
fn restore_finalize_refuses_wrong_phase_without_spending_attempts() {
    use fl_store::recovery::readiness::{finalize_restore, finalize_rollback};
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    assert!(
        finalize_rollback(
            &journal,
            &lease,
            &generation_evidence(&journal, false),
            &f.filesystem()
        )
        .is_err()
    );
    let status = restore_status(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(
        (status.resume_finalize_attempts, status.rollback_attempts),
        (0, 0)
    );
    let RestoreOutcome::RolledBack { journal } =
        rollback_restore(&journal, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not rolled back")
    };
    assert!(
        finalize_restore(
            &journal,
            &lease,
            &generation_evidence(&journal, true),
            &f.filesystem()
        )
        .is_err()
    );
    let status = restore_status(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(
        (status.resume_finalize_attempts, status.rollback_attempts),
        (0, 1)
    );
}
#[test]
fn restore_finalize_rollback_refuses_reverse_intent_without_spending_attempts() {
    use std::sync::atomic::AtomicBool;
    struct CutAfterReverse(AtomicBool);
    impl RecoveryIo for CutAfterReverse {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::RestoreReverseExchange {
                self.0.store(true, Ordering::Release);
            }
            if e == RecoveryEvent::Write(Member::RestoreJournal) && self.0.load(Ordering::Acquire) {
                return Err(std::io::Error::other("cut before RolledBack"));
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
    let filesystem = CutFs {
        f: &f,
        io: Arc::new(CutAfterReverse(AtomicBool::new(false))),
        limits: limits(),
    };
    assert!(rollback_restore(&journal, &lease, &filesystem).is_err());
    let path = journal.staging.join("journal.bin");
    let journal = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(journal.phase, RestorePhase::ReverseIntent);
    assert!(
        fl_store::recovery::readiness::finalize_rollback(
            &journal,
            &lease,
            &generation_evidence(&journal, true),
            &f.filesystem()
        )
        .is_err()
    );
    assert_eq!(
        restore_status(&path, &lease, &f.filesystem())
            .unwrap()
            .rollback_attempts,
        1
    );
}

#[test]
fn restore_resume_reverse_intent_refuses_for_free_and_typed_rollback_finishes() {
    struct ReverseCut {
        marker: RecoveryEvent,
        after: bool,
        seen: AtomicBool,
        fired: AtomicBool,
    }
    impl RecoveryIo for ReverseCut {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event == self.marker {
                self.seen.store(true, Ordering::Release);
            }
            let cut = if self.after {
                self.seen.load(Ordering::Acquire)
                    && event == RecoveryEvent::Sync(Directory::RestoreDestinationParent)
            } else {
                event == self.marker
            };
            if cut && !self.fired.swap(true, Ordering::AcqRel) {
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
            Ok(())
        }
    }
    for (absent, after) in [(true, true), (false, false), (false, true)] {
        let f = journal_fixture();
        if absent {
            let writer = FerroStore::open(&f.source).unwrap();
            writer
                .put_doc(
                    "fixture_unknown",
                    "one",
                    &serde_json::json!({"body": "x".repeat(20 << 20)}),
                )
                .unwrap();
            drop(writer);
        }
        let destination = if absent {
            f.root.path().join("absent.fls")
        } else {
            f.source.clone()
        };
        let original = inspect_file_binding(&destination, &limits()).unwrap();
        let (request, lease) = prepare_restore(&f, &destination);
        let RestoreOutcome::Installed { journal } =
            restore_offline(&request, &lease, &f.filesystem()).unwrap()
        else {
            panic!("not installed")
        };
        let cut = Arc::new(ReverseCut {
            marker: if absent {
                RecoveryEvent::RestoreRelocate
            } else {
                RecoveryEvent::RestoreReverseExchange
            },
            after,
            seen: AtomicBool::new(false),
            fired: AtomicBool::new(false),
        });
        let filesystem = CutFs {
            f: &f,
            io: cut.clone(),
            limits: limits(),
        };
        assert!(rollback_restore(&journal, &lease, &filesystem).is_err());
        assert!(cut.fired.load(Ordering::Acquire));
        let path = journal.staging.join("journal.bin");
        let journal = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(journal.phase, RestorePhase::ReverseIntent);
        assert_eq!(
            inspect_file_binding(&destination, &limits()).unwrap() == original,
            after,
            "cut must establish the intended side of the reverse rename"
        );
        let metadata = terminal_negative_metadata(&f, &journal);
        let frames = journal_frame_ranges(&metadata[0]).len();
        let layout = restore_expiry_layout(&destination, &journal.staging);
        // Phase diagnostics precede forward validation even without the bundle.
        let parked = f.backups.join("parked-selected-bundle");
        for retry in 0..14 {
            if retry == 1 {
                fs::rename(request.selected_bundle.path(), &parked).unwrap();
            }
            let error = resume_restore(&journal, &lease, &f.filesystem()).unwrap_err();
            assert!(error.to_string().contains("rollback_restore"), "{error}");
            assert_eq!(terminal_negative_metadata(&f, &journal), metadata);
            assert_eq!(
                restore_expiry_layout(&destination, &journal.staging),
                layout
            );
        }
        let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(
            terminal_negative_counters(&loaded),
            terminal_negative_counters(&journal)
        );
        assert_eq!(
            loaded.forensic_handling_required,
            journal.forensic_handling_required
        );
        assert_eq!(
            journal_frame_ranges(&fs::read(&path).unwrap()).len(),
            frames
        );
        let next = match rollback_restore(&loaded, &lease, &f.filesystem()).unwrap() {
            RestoreOutcome::OriginalAbsenceRestored { journal } if absent => journal,
            RestoreOutcome::RolledBack { journal } if !absent => journal,
            other => panic!("typed rollback did not finish: {other:?}"),
        };
        assert_eq!(next.rollback_attempts, journal.rollback_attempts + 1);
        assert_eq!(
            inspect_file_binding(&destination, &limits()).unwrap(),
            original
        );
        eprintln!("reverse_intent_resume absent={absent} after={after} free=true typed_retry=true");
    }
}

#[test]
fn restore_failed_candidate_copy_keeps_a_loadable_admitted_journal() {
    struct CutCopy;
    impl RecoveryIo for CutCopy {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::Flush(Member::Image) {
                return Err(std::io::Error::other("candidate flush failed"));
            }
            Ok(())
        }
    }
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let original = fs::read(&f.source).unwrap();
    let filesystem = CutFs {
        f: &f,
        io: Arc::new(CutCopy),
        limits: limits(),
    };
    assert!(restore_offline(&request, &lease, &filesystem).is_err());
    let guard: serde_json::Value =
        serde_json::from_slice(&fs::read(f.source.with_extension("fls.restore-guard")).unwrap())
            .unwrap();
    let journal = load_restore_journal(
        std::path::Path::new(guard["journal"].as_str().unwrap()),
        &lease,
        &f.filesystem(),
    )
    .expect("every published guard must name a loadable admitted journal");
    assert_eq!(serde_json::to_value(&journal.phase).unwrap(), "admitted");
    assert_eq!(fs::read(&f.source).unwrap(), original);
}

// Bounded real append history, never padding or a strict-2L exemption.
fn journal_fixture() -> Fixture {
    let root = test_support::fixture_root();
    let source = root.path().join("source.fls");
    let backups = root.path().join("backups");
    fs::create_dir(&backups).unwrap();
    fs::set_permissions(
        &backups,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let owner = FerroStore::open(&source).unwrap();
    for n in 0..80 {
        owner
            .put_doc(
                "fixture_unknown",
                "one",
                &serde_json::json!({"n": n, "body": "x".repeat(256 << 10)}),
            )
            .unwrap();
    }
    drop(owner);
    let bytes = fs::metadata(&source).unwrap().len();
    assert!(
        (20 << 20..=22 << 20).contains(&bytes),
        "bounded physical history: {bytes}"
    );
    Fixture {
        root,
        source,
        backups,
    }
}

// RJR1 frame helpers for restore journal regression tests.
fn journal_frame_ranges(bytes: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut frames = Vec::new();
    let mut offset = 0;
    while bytes.get(offset..offset + 4) == Some(b"RJR1") {
        let len = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let end = offset + 72 + len;
        assert!(end <= bytes.len());
        frames.push(offset..end);
        offset = end;
    }
    frames
}
#[test]
fn restore_last_record_byte_tears_load_previous_phase_without_writing() {
    use std::os::unix::fs::FileExt;
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let original = fs::read(&path).unwrap();
    let frames = journal_frame_ranges(&original);
    let last = frames.last().unwrap();
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    for cut in 0..last.len() {
        let mut tail = vec![0; last.len()];
        tail[..cut].copy_from_slice(&original[last.start..last.start + cut]);
        file.write_all_at(&tail, last.start as u64).unwrap();
        let before = fs::metadata(&path).unwrap();
        let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(loaded.phase, RestorePhase::RelocateIntent, "cut {cut}");
        assert_eq!(
            loaded.torn_tail_bytes,
            tail.iter()
                .rposition(|b| *b != 0)
                .map_or(0, |n| n as u64 + 1),
            "cut {cut}"
        );
        let after = fs::metadata(&path).unwrap();
        assert_eq!(
            (before.mtime(), before.mtime_nsec()),
            (after.mtime(), after.mtime_nsec()),
            "read-only load repaired cut {cut}"
        );
        let mut retained = vec![0; last.len()];
        fs::File::open(&path)
            .unwrap()
            .read_exact_at(&mut retained, last.start as u64)
            .unwrap();
        assert_eq!(retained, tail, "read-only load changed cut {cut}");
    }
}
#[test]
fn restore_corrupt_midfile_record_refuses_and_last_torn_record_repairs_on_append() {
    use std::os::unix::fs::FileExt;
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let original = fs::read(&path).unwrap();
    let frames = journal_frame_ranges(&original);
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    let earlier = &frames[frames.len() - 2];
    file.write_all_at(
        &[original[earlier.start + 72] ^ 1],
        earlier.start as u64 + 72,
    )
    .unwrap();
    assert!(matches!(
        load_restore_journal(&path, &lease, &f.filesystem()),
        Err(fl_store::recovery::RecoveryError::Verification { .. })
    ));
    file.write_all_at(&original[earlier.clone()], earlier.start as u64)
        .unwrap();
    let last = frames.last().unwrap();
    file.write_all_at(&[original[last.start + 72] ^ 1], last.start as u64 + 72)
        .unwrap();
    let torn = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert!(torn.torn_tail_bytes > 0);
    let RestoreOutcome::Installed { journal } =
        resume_restore(&torn, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(loaded.torn_tail_bytes, 0);
    assert_eq!(loaded.phase, RestorePhase::Installed);
    assert_eq!(loaded.current_attempt, journal.current_attempt);
}

// Resetting the fixed-size journal below is fixture-only crash-image injection.
// Each image starts from the same durable installed layout and attempt counter;
// production resume itself always appends and spends a fresh attempt.
fn journal_fixture_write(path: &std::path::Path, bytes: &[u8]) {
    use std::os::unix::fs::FileExt;
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    assert_eq!(file.metadata().unwrap().len(), bytes.len() as u64);
    file.write_all_at(bytes, 0).unwrap();
    file.sync_all().unwrap();
}
fn journal_phase(bytes: &[u8], frame: &std::ops::Range<usize>) -> RestorePhase {
    let value: serde_json::Value =
        serde_json::from_slice(&bytes[frame.start + 72..frame.end]).unwrap();
    serde_json::from_value(value["phase"].clone()).unwrap()
}
#[test]
fn restore_tail_bad_magic_or_previous_hash_uses_bounded_unauthenticated_extent() {
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let original = fs::read(&path).unwrap();
    let frames = journal_frame_ranges(&original);
    let last = frames.last().unwrap();
    for byte in [0, 8, 40, 72] {
        let mut damaged = original.clone();
        damaged[last.start + byte] ^= 1;
        journal_fixture_write(&path, &damaged);
        let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(loaded.phase, RestorePhase::RelocateIntent);
        assert!(loaded.torn_tail_bytes > 72, "corruption offset {byte}");
        assert_eq!(fs::read(&path).unwrap(), damaged, "read-only load wrote");
        let RestoreOutcome::Installed { .. } =
            resume_restore(&loaded, &lease, &f.filesystem()).unwrap()
        else {
            panic!("not installed")
        };
        assert_eq!(
            load_restore_journal(&path, &lease, &f.filesystem())
                .unwrap()
                .torn_tail_bytes,
            0
        );
    }
    // A damaged length cannot bound surviving payload from the same final frame.
    let mut damaged = original.clone();
    damaged[last.start + 4..last.start + 8].copy_from_slice(&u32::MAX.to_le_bytes());
    journal_fixture_write(&path, &damaged);
    let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(loaded.phase, RestorePhase::RelocateIntent);
    assert!(loaded.torn_tail_bytes > 72);
    assert_eq!(fs::read(&path).unwrap(), damaged, "read-only load wrote");
    damaged[last.start + 72..].fill(0);
    journal_fixture_write(&path, &damaged);
    assert!(
        load_restore_journal(&path, &lease, &f.filesystem())
            .unwrap()
            .torn_tail_bytes
            > 0
    );
    // A plausible but unauthenticated length cannot hide a bounded torn tail.
    damaged = original.clone();
    damaged[last.start] ^= 1;
    damaged[last.end] = 1;
    journal_fixture_write(&path, &damaged);
    let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(loaded.phase, RestorePhase::RelocateIntent);
    assert_eq!(loaded.torn_tail_bytes, last.len() as u64 + 1);
    assert_eq!(fs::read(&path).unwrap(), damaged, "read-only load wrote");
}

// Exercise crash images through the public loader and preserve the journal and
// fence byte-for-byte on both recovery and refusal.
fn assert_tail_load_read_only(
    f: &Fixture,
    path: &std::path::Path,
    lease: &OfflineLease,
    image: &[u8],
    expect_torn: bool,
) {
    journal_fixture_write(path, image);
    let guard = f.source.with_extension("fls.restore-guard");
    let guard_before = fs::read(&guard).unwrap();
    let before = fs::metadata(path).unwrap();
    let result = load_restore_journal(path, lease, &f.filesystem());
    if expect_torn {
        let loaded = result.unwrap();
        assert_eq!(loaded.phase, RestorePhase::RelocateIntent);
        assert!(loaded.torn_tail_bytes > 0);
    } else {
        assert!(matches!(
            result,
            Err(fl_store::recovery::RecoveryError::Verification { .. })
        ));
    }
    let after = fs::metadata(path).unwrap();
    assert_eq!(
        (before.mtime(), before.mtime_nsec()),
        (after.mtime(), after.mtime_nsec())
    );
    assert_eq!(fs::read(path).unwrap(), image, "read-only load wrote");
    assert_eq!(fs::read(guard).unwrap(), guard_before, "load changed fence");
}
#[test]
fn restore_tail_page_loss_keeps_prior_authenticated_phase_without_writing() {
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let original = fs::read(&path).unwrap();
    let frames = journal_frame_ranges(&original);
    let last = frames.last().unwrap();
    let first_page = last.start / 4096;
    let last_page = (last.end - 1) / 4096;
    assert!(last_page > first_page, "genuine final frame spans pages");
    for page in first_page..=last_page {
        let mut damaged = original.clone();
        // The first page can also contain the prior authenticated frame.
        let start = (page * 4096).max(last.start);
        let end = ((page + 1) * 4096).min(last.end);
        damaged[start..end].fill(0);
        assert_tail_load_read_only(&f, &path, &lease, &damaged, true);
        eprintln!("page-loss {page}: {start}..{end} recovered");
    }
}
#[test]
fn restore_tail_unauthenticated_header_and_plausible_length_use_one_frame_window() {
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let original = fs::read(&path).unwrap();
    let frames = journal_frame_ranges(&original);
    let last = frames.last().unwrap();
    let window_end = last.start + original.len() / 96;
    assert!(last.end < window_end && window_end < original.len());
    for kind in 0..4 {
        let mut damaged = original.clone();
        match kind {
            0 => damaged[last.start..last.start + 72].fill(0),
            1 => damaged[last.start + 4..last.start + 8].copy_from_slice(&u32::MAX.to_le_bytes()),
            // A partly lost length remains plausible and in-range.
            2 => {
                damaged[last.start + 5..last.start + 8].fill(0);
                assert_ne!(damaged[last.start + 4], 0);
            }
            3 => damaged[last.start + 40] ^= 1,
            _ => unreachable!(),
        }
        damaged[window_end - 1] = 1;
        assert_tail_load_read_only(&f, &path, &lease, &damaged, true);
        damaged[window_end] = 1;
        assert_tail_load_read_only(&f, &path, &lease, &damaged, false);
        eprintln!("unauthenticated kind {kind}: exact window enforced");
    }
}
#[test]
fn restore_tail_unauthenticated_zero_header_cannot_swallow_complete_frame_or_first_frame_loss() {
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let original = fs::read(&path).unwrap();
    let frames = journal_frame_ranges(&original);
    let last = frames.last().unwrap();
    let mut damaged = original.clone();
    damaged[last.start..last.start + 72].fill(0);
    // A complete digest-only frame counts even in the damaged payload, with an
    // unrelated chain and invalid JSON. The scan must not skip to a claimed end.
    let nested = journal_digest_frame(b"not-json", [0xa5; 32]);
    let at = last.start + 73;
    assert!(at + nested.len() < last.end);
    damaged[at..at + nested.len()].copy_from_slice(&nested);
    assert_tail_load_read_only(&f, &path, &lease, &damaged, false);
    let first = &frames[0];
    damaged = original.clone();
    damaged[first.end..].fill(0); // no later complete frame to explain refusal
    damaged[first.start..first.start + 72].fill(0);
    assert_tail_load_read_only(&f, &path, &lease, &damaged, false);
}
#[test]
fn restore_tail_unauthenticated_fallback_does_not_widen_authenticated_invalid_payload() {
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let original = fs::read(&path).unwrap();
    let frames = journal_frame_ranges(&original);
    let last = frames.last().unwrap();
    let previous = original[last.start + 8..last.start + 40]
        .try_into()
        .unwrap();
    let mut typed_invalid: serde_json::Value =
        serde_json::from_slice(&original[last.start + 72..last.end]).unwrap();
    typed_invalid["version"] = serde_json::json!("invalid-version");
    for payload in [
        b"{".to_vec(),
        b"{}".to_vec(),
        serde_json::to_vec(&typed_invalid).unwrap(),
    ] {
        let frame = journal_digest_frame(&payload, previous);
        let mut damaged = original.clone();
        damaged[last.start..].fill(0);
        let end = last.start + frame.len();
        damaged[last.start..end].copy_from_slice(&frame);
        assert_tail_load_read_only(&f, &path, &lease, &damaged, true);
        damaged[end] = 1;
        assert_tail_load_read_only(&f, &path, &lease, &damaged, false);
    }
}

struct JournalBoundaryIo {
    events: Mutex<Vec<RecoveryEvent>>,
    stop: Option<RecoveryEvent>,
}
impl RecoveryIo for JournalBoundaryIo {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        self.events.lock().unwrap().push(event);
        if self.stop == Some(event) {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(())
    }
}
fn journal_boundary_io(stop: Option<RecoveryEvent>) -> Arc<JournalBoundaryIo> {
    Arc::new(JournalBoundaryIo {
        events: Mutex::new(vec![]),
        stop,
    })
}
fn journal_set_existing_layout(f: &Fixture, staging: &std::path::Path, layout: &str) {
    // Initial fixture is Installed: (candidate, absent, original).
    let c = staging.join("candidate.fls");
    let o = staging.join("original.fls");
    match layout {
        "installed" => {}
        "exchanged" => fs::rename(o, c).unwrap(),
        "prepared" => {
            fs::rename(&f.source, &c).unwrap();
            fs::rename(o, &f.source).unwrap();
        }
        "reversed" => {
            let temp = staging.join("fixture-swap");
            fs::rename(&f.source, &temp).unwrap();
            fs::rename(&o, &f.source).unwrap();
            fs::rename(temp, o).unwrap();
        }
        _ => panic!("unknown fixture layout"),
    }
}
#[test]
fn restore_adjacent_intents_admit_only_their_before_and_after_layouts() {
    // Relocate's after-layout belongs to Exchange only after the NEXT intent's
    // action, so Exchange must reject it. Prepared similarly excludes exchange.
    for (phase, allowed) in [
        (RestorePhase::Prepared, [true, false, false, false]),
        (RestorePhase::ExchangeIntent, [true, true, false, false]),
        (RestorePhase::RelocateIntent, [false, true, true, false]),
        (RestorePhase::Installed, [false, false, true, false]),
        (RestorePhase::ReverseIntent, [false, false, true, true]),
        (RestorePhase::RolledBack, [false, false, false, true]),
    ] {
        for (layout, admitted) in ["prepared", "exchanged", "installed", "reversed"]
            .into_iter()
            .zip(allowed)
        {
            let f = journal_fixture();
            let (request, lease) = prepare_restore(&f, &f.source);
            let RestoreOutcome::Installed { journal } =
                restore_offline(&request, &lease, &f.filesystem()).unwrap()
            else {
                panic!("not installed")
            };
            let path = journal.staging.join("journal.bin");
            let installed = fs::read(&path).unwrap();
            // Get real ReverseIntent/RolledBack frames, then restore names to
            // installed before constructing the chosen independent table row.
            let original = if matches!(
                phase,
                RestorePhase::ReverseIntent | RestorePhase::RolledBack
            ) {
                rollback_restore(&journal, &lease, &f.filesystem()).unwrap();
                let bytes = fs::read(&path).unwrap();
                journal_set_existing_layout(&f, &journal.staging, "reversed"); // swap is its own inverse
                bytes
            } else {
                installed
            };
            let frames = journal_frame_ranges(&original);
            let selected = frames
                .iter()
                .find(|range| journal_phase(&original, range) == phase)
                .unwrap();
            let mut prefix = original.clone();
            prefix[selected.end..].fill(0);
            journal_fixture_write(&path, &prefix);
            journal_set_existing_layout(&f, &journal.staging, layout);
            let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
            let io = journal_boundary_io(Some(RecoveryEvent::Write(Member::RestoreJournal)));
            let filesystem = CutFs {
                f: &f,
                io: io.clone(),
                limits: limits(),
            };
            let result = resume_restore(&loaded, &lease, &filesystem);
            let attempted = io
                .events
                .lock()
                .unwrap()
                .contains(&RecoveryEvent::Write(Member::RestoreJournal));
            // Typed rollback takes precedence over every reverse-intent layout.
            if phase == RestorePhase::ReverseIntent {
                assert!(!attempted, "{layout}: {result:?}");
                let error = result.unwrap_err();
                assert!(
                    error.to_string().contains("rollback_restore"),
                    "{layout}: {error}"
                );
            } else {
                let completed =
                    admitted && matches!(phase, RestorePhase::Installed | RestorePhase::RolledBack);
                assert_eq!(
                    attempted,
                    admitted && !completed,
                    "{phase:?} {layout}: {result:?}"
                );
                if completed {
                    assert!(matches!(
                        (phase.clone(), result),
                        (
                            RestorePhase::Installed,
                            Ok(RestoreOutcome::Installed { .. })
                        ) | (
                            RestorePhase::RolledBack,
                            Ok(RestoreOutcome::RolledBack { .. })
                        )
                    ));
                } else if !admitted {
                    assert!(matches!(
                        result,
                        Err(fl_store::recovery::RecoveryError::Verification { .. })
                    ));
                }
            }
            assert_eq!(fs::read(&path).unwrap(), prefix);
        }
    }
}
#[test]
fn restore_corrupt_relocate_intent_before_action_finishes_once_after_action_refuses() {
    for relocated in [false, true] {
        let f = journal_fixture();
        let (request, lease) = prepare_restore(&f, &f.source);
        let io = journal_boundary_io(if relocated {
            None
        } else {
            Some(RecoveryEvent::RestoreRelocate)
        });
        let filesystem = CutFs {
            f: &f,
            io: io.clone(),
            limits: limits(),
        };
        let _ = restore_offline(&request, &lease, &filesystem);
        let guard = f.root.path().join("source.fls.restore-guard");
        let guard_bytes = fs::read(&guard).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&guard_bytes).unwrap();
        let path = std::path::PathBuf::from(value["journal"].as_str().unwrap());
        let original = fs::read(&path).unwrap();
        let frames = journal_frame_ranges(&original);
        let relocate = frames
            .iter()
            .find(|range| journal_phase(&original, range) == RestorePhase::RelocateIntent)
            .unwrap();
        let mut damaged = original.clone();
        damaged[relocate.start + 40] ^= 1;
        damaged[relocate.end..].fill(0);
        journal_fixture_write(&path, &damaged);
        let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(loaded.phase, RestorePhase::ExchangeIntent);
        assert!(loaded.torn_tail_bytes > 0);
        assert_eq!(fs::read(&path).unwrap(), damaged);
        let source_before = fs::read(&f.source).unwrap();
        let io2 = journal_boundary_io(None);
        let filesystem2 = CutFs {
            f: &f,
            io: io2.clone(),
            limits: limits(),
        };
        let result = resume_restore(&loaded, &lease, &filesystem2);
        for marker in [
            RecoveryEvent::RestoreInstall,
            RecoveryEvent::RestoreExchangeProbe,
        ] {
            let count = io
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| **e == marker)
                .count()
                + io2
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| **e == marker)
                    .count();
            assert_eq!(
                count, 1,
                "exactly one {marker:?} across initial plus resume"
            );
        }
        if relocated {
            assert!(matches!(
                result,
                Err(fl_store::recovery::RecoveryError::Verification { .. })
            ));
            assert_eq!(fs::read(&path).unwrap(), damaged);
            assert_eq!(fs::read(&guard).unwrap(), guard_bytes);
            assert_eq!(fs::read(&f.source).unwrap(), source_before);
            assert!(path.parent().unwrap().join("original.fls").exists());
            assert!(
                !io2.events
                    .lock()
                    .unwrap()
                    .contains(&RecoveryEvent::Write(Member::RestoreJournal))
            );
        } else {
            let RestoreOutcome::Installed { journal } = result.unwrap() else {
                panic!("not installed")
            };
            assert_eq!(journal.phase, RestorePhase::Installed);
            assert_eq!(journal.torn_tail_bytes, 0);
            assert_eq!(
                fs::metadata(&f.source).unwrap().ino(),
                journal.candidate_identity.1
            );
            assert!(path.parent().unwrap().join("original.fls").exists());
        }
        assert!(guard.exists());
    }
}

#[test]
fn restore_actions_wait_for_complete_intent_write_file_sync_and_parent_sync() {
    let trace = journal_fixture();
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
        panic!("trace did not install")
    };
    let durable_log = fs::read(journal.staging.join("journal.bin")).unwrap();
    let frames = journal_frame_ranges(&durable_log);
    let events = hooks.events.lock().unwrap().clone();
    for (action, intent) in [
        (RecoveryEvent::RestoreInstall, RestorePhase::ExchangeIntent),
        (RecoveryEvent::RestoreRelocate, RestorePhase::RelocateIntent),
    ] {
        let action_index = events.iter().position(|event| *event == action).unwrap();
        // Every completed append emits exactly one journal-parent sync. Bind
        // the marker to that durable frame, not merely any earlier sync.
        let durable_frames = events[..action_index]
            .iter()
            .filter(|event| **event == RecoveryEvent::Sync(Directory::RestoreJournalParent))
            .count();
        assert_eq!(
            journal_phase(&durable_log, &frames[durable_frames - 1]),
            intent,
            "{action:?} must follow its own durable intent"
        );
        let parent = events[..action_index]
            .iter()
            .rposition(|event| *event == RecoveryEvent::Sync(Directory::RestoreJournalParent))
            .unwrap();
        assert_eq!(
            &events[parent - 3..=parent],
            &[
                RecoveryEvent::Write(Member::RestoreJournal),
                RecoveryEvent::Write(Member::RestoreJournal),
                RecoveryEvent::Flush(Member::RestoreJournal),
                RecoveryEvent::Sync(Directory::RestoreJournalParent),
            ]
        );
        for cut in parent - 3..=parent {
            let f = journal_fixture();
            let (request, lease) = prepare_restore(&f, &f.source);
            let hook = Arc::new(CutIo::new(cut, libc::EIO));
            let filesystem = CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits(),
            };
            let result = restore_offline(&request, &lease, &filesystem);
            assert!(result.is_err() || matches!(result, Ok(RestoreOutcome::OutcomeUnknown { .. })));
            assert!(hook.count.load(Ordering::Acquire) > cut);
            assert!(
                !hook.events.lock().unwrap().contains(&action),
                "{action:?} ran despite failed durability boundary {cut}"
            );
        }
    }
}
struct StopThirdJournalWrite(AtomicUsize, Mutex<Vec<RecoveryEvent>>);
impl RecoveryIo for StopThirdJournalWrite {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        self.1.lock().unwrap().push(event);
        if event == RecoveryEvent::Write(Member::RestoreJournal)
            && self.0.fetch_add(1, Ordering::AcqRel) == 2
        {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(())
    }
}
#[test]
fn restore_durable_attempt_bump_without_action_intent_keeps_before_layout_and_retry_spends_again() {
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let mut bytes = fs::read(&path).unwrap();
    let frame = journal_frame_ranges(&bytes)
        .into_iter()
        .find(|range| journal_phase(&bytes, range) == RestorePhase::Prepared)
        .unwrap();
    bytes[frame.end..].fill(0);
    journal_fixture_write(&path, &bytes);
    journal_set_existing_layout(&f, &journal.staging, "prepared");
    let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    let before_layout = [f.source.clone(), journal.staging.join("candidate.fls")]
        .map(|p| (fs::metadata(&p).unwrap().ino(), fs::read(p).unwrap()));
    // First two writes + file/parent sync durably record the counter bump;
    // third write is the subsequent ExchangeIntent header and fails before I/O.
    let io = Arc::new(StopThirdJournalWrite(
        AtomicUsize::new(0),
        Mutex::new(vec![]),
    ));
    let filesystem = CutFs {
        f: &f,
        io: io.clone(),
        limits: limits(),
    };
    assert!(resume_restore(&loaded, &lease, &filesystem).is_err());
    let after_layout = [f.source.clone(), journal.staging.join("candidate.fls")]
        .map(|p| (fs::metadata(&p).unwrap().ino(), fs::read(p).unwrap()));
    assert_eq!(before_layout, after_layout);
    assert!(!journal.staging.join("original.fls").exists());
    let events = io.1.lock().unwrap().clone();
    assert!(!events.contains(&RecoveryEvent::Exchange));
    assert!(!events.contains(&RecoveryEvent::RestoreRelocate));
    assert!(events.contains(&RecoveryEvent::Flush(Member::RestoreJournal)));
    assert!(events.contains(&RecoveryEvent::Sync(Directory::RestoreJournalParent)));
    let bumped = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(bumped.phase, RestorePhase::Prepared);
    assert_eq!(bumped.current_attempt, loaded.current_attempt + 1);
    assert_eq!(
        bumped.resume_finalize_attempts,
        loaded.resume_finalize_attempts + 1
    );
    assert_eq!(bumped.torn_tail_bytes, 0);
    let RestoreOutcome::Installed { journal: retried } =
        resume_restore(&bumped, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    assert_eq!(retried.current_attempt, bumped.current_attempt + 1);
    assert_eq!(
        retried.resume_finalize_attempts,
        bumped.resume_finalize_attempts + 1
    );
}

fn journal_digest_frame(payload: &[u8], previous: [u8; 32]) -> Vec<u8> {
    let mut hash = sha2::Sha256::new();
    hash.update(previous);
    hash.update(payload);
    let mut frame = Vec::from(&b"RJR1"[..]);
    frame.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    frame.extend_from_slice(&previous);
    frame.extend_from_slice(&hash.finalize());
    frame.extend_from_slice(payload);
    frame
}
#[test]
fn restore_failed_length_cannot_swallow_next_valid_frame() {
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let original = fs::read(&path).unwrap();
    let frames = journal_frame_ranges(&original);
    let penultimate = &frames[frames.len() - 2];
    let next = frames.last().unwrap();
    // These corrupt only the unprotected length. Digest mismatch must scan
    // inside the claimed payload, including the actual next frame boundary.
    for claimed_end in [next.end, next.end + 17, penultimate.end - 1] {
        let mut damaged = original.clone();
        let length = u32::try_from(claimed_end - penultimate.start - 72).unwrap();
        damaged[penultimate.start + 4..penultimate.start + 8]
            .copy_from_slice(&length.to_le_bytes());
        journal_fixture_write(&path, &damaged);
        assert!(
            matches!(
                load_restore_journal(&path, &lease, &f.filesystem()),
                Err(fl_store::recovery::RecoveryError::Verification { .. })
            ),
            "claimed end {claimed_end}"
        );
        assert_eq!(fs::read(&path).unwrap(), damaged, "refusal wrote");
    }
}
#[test]
fn restore_failed_tail_refuses_unaligned_digest_only_frame_inside_payload() {
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let original = fs::read(&path).unwrap();
    let frames = journal_frame_ranges(&original);
    let last = frames.last().unwrap();
    // Deliberately invalid JSON and unrelated previous hash: digest alone
    // makes a later complete frame authoritative evidence against repair.
    let nested = journal_digest_frame(b"not-json", [0xa5; 32]);
    let mut at = last.start + 73;
    if at.is_multiple_of(8) {
        at += 1;
    }
    assert_ne!(at % 8, 0);
    assert!(at + nested.len() <= last.end);
    for bad in [0, 4, 8, 40, 72] {
        let mut damaged = original.clone();
        damaged[at..at + nested.len()].copy_from_slice(&nested);
        if bad == 4 {
            damaged[last.start + 4..last.start + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        } else {
            damaged[last.start + bad] ^= 1;
        }
        journal_fixture_write(&path, &damaged);
        assert!(
            matches!(
                load_restore_journal(&path, &lease, &f.filesystem()),
                Err(fl_store::recovery::RecoveryError::Verification { .. })
            ),
            "corruption offset {bad}"
        );
        assert_eq!(fs::read(&path).unwrap(), damaged, "refusal wrote");
    }
}
#[test]
fn restore_failed_json_path_containing_magic_without_complete_frame_is_torn() {
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let original = fs::read(&path).unwrap();
    let frames = journal_frame_ranges(&original);
    let last = frames.last().unwrap();
    let mut payload: serde_json::Value =
        serde_json::from_slice(&original[last.start + 72..last.end]).unwrap();
    payload["destination"] = serde_json::json!("/fixture/RJR1/json-path");
    let previous = original[last.start + 8..last.start + 40]
        .try_into()
        .unwrap();
    let mut frame = journal_digest_frame(&serde_json::to_vec(&payload).unwrap(), previous);
    frame[40] ^= 1;
    let mut damaged = original.clone();
    damaged[last.start..].fill(0);
    assert!(last.start + frame.len() <= damaged.len());
    damaged[last.start..last.start + frame.len()].copy_from_slice(&frame);
    journal_fixture_write(&path, &damaged);
    let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(loaded.phase, RestorePhase::RelocateIntent);
    assert_eq!(loaded.torn_tail_bytes, frame.len() as u64);
    assert_eq!(fs::read(&path).unwrap(), damaged, "read-only load wrote");
}

#[derive(Clone, Copy, Debug)]
enum TerminalFixtureMode {
    Selected,
    Original,
    Absence,
    Abandon,
    AbandonAbsence,
}
fn terminal_fixture(mode: TerminalFixtureMode) -> (Fixture, RestoreJournal, OfflineLease) {
    let f = journal_fixture();
    let absent = matches!(
        mode,
        TerminalFixtureMode::Absence | TerminalFixtureMode::AbandonAbsence
    );
    if absent {
        // An absent destination uses the compact selected image as its strict-2L
        // baseline. Real live data, rather than overwritten history, must fund it.
        let writer = FerroStore::open(&f.source).unwrap();
        writer
            .put_doc(
                "fixture_unknown",
                "one",
                &serde_json::json!({"body": "x".repeat(20 << 20)}),
            )
            .unwrap();
        drop(writer);
    }
    let destination = if absent {
        f.root.path().join("absent.fls")
    } else {
        f.source.clone()
    };
    let (request, lease) = prepare_restore(&f, &destination);
    if absent {
        let manifest: BundleManifestV1 = serde_json::from_slice(
            &fs::read(request.selected_bundle.path().join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert!(
            manifest.image_bytes >= 20 << 20,
            "absent fixture needs a genuine compact-image baseline"
        );
    }
    let journal = if matches!(
        mode,
        TerminalFixtureMode::Abandon | TerminalFixtureMode::AbandonAbsence
    ) {
        struct FailCopy {
            writes: AtomicUsize,
            copied: AtomicUsize,
        }
        impl RecoveryIo for FailCopy {
            fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
                // Candidate copying uses VerificationCopy; interrupt its second
                // write so this fixture proves a genuine nonempty partial copy.
                if event == RecoveryEvent::Write(Member::VerificationCopy)
                    && self.writes.fetch_add(1, Ordering::AcqRel) == 1
                {
                    return Err(std::io::Error::from_raw_os_error(libc::EIO));
                }
                Ok(())
            }
            fn write_at(
                &self,
                file: &fs::File,
                bytes: &[u8],
                offset: u64,
            ) -> std::io::Result<usize> {
                use std::os::unix::fs::FileExt;
                let written = file.write_at(bytes, offset)?;
                if self.writes.load(Ordering::Acquire) == 1 {
                    self.copied.fetch_add(written, Ordering::AcqRel);
                }
                Ok(written)
            }
        }
        let hook = Arc::new(FailCopy {
            writes: AtomicUsize::new(0),
            copied: AtomicUsize::new(0),
        });
        let filesystem = CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        assert!(restore_offline(&request, &lease, &filesystem).is_err());
        assert_eq!(
            hook.writes.load(Ordering::Acquire),
            2,
            "candidate copy cut was not reached"
        );
        let guard = destination.with_extension("fls.restore-guard");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&guard).expect("copy cut must leave durable guard"))
                .unwrap();
        let journal = load_restore_journal(
            std::path::Path::new(value["journal"].as_str().unwrap()),
            &lease,
            &f.filesystem(),
        )
        .expect("present guard MUST have a loadable admitted journal; do not skip");
        assert_eq!(journal.phase, RestorePhase::Admitted);
        assert!(journal.verification.is_none());
        let copied = hook.copied.load(Ordering::Acquire);
        let candidate = fs::read(journal.staging.join("candidate.fls")).unwrap();
        let image = fs::read(request.selected_bundle.path().join("image.fls")).unwrap();
        assert!(
            copied > 0 && copied < image.len(),
            "copy must be nonempty and incomplete"
        );
        assert_eq!(
            &candidate[..copied],
            &image[..copied],
            "copied prefix differs"
        );
        assert_ne!(candidate, image, "candidate copy unexpectedly completed");
        journal
    } else {
        let RestoreOutcome::Installed { journal } =
            restore_offline(&request, &lease, &f.filesystem()).unwrap()
        else {
            panic!("not installed")
        };
        if matches!(
            mode,
            TerminalFixtureMode::Original | TerminalFixtureMode::Absence
        ) {
            match rollback_restore(&journal, &lease, &f.filesystem()).unwrap() {
                RestoreOutcome::RolledBack { journal }
                | RestoreOutcome::OriginalAbsenceRestored { journal } => journal,
                other => panic!("not restored: {other:?}"),
            }
        } else {
            journal
        }
    };
    (f, journal, lease)
}
fn fixture_lease_request(lease: &OfflineLease) -> OfflineLeaseRequest {
    serde_json::from_slice(&fs::read(lease.path()).unwrap()).unwrap()
}
fn terminal_call(
    mode: TerminalFixtureMode,
    j: &RestoreJournal,
    lease: &OfflineLease,
    filesystem: &dyn RecoveryFs,
    replacement: bool,
) -> Result<RestoreOutcome, fl_store::recovery::RecoveryError> {
    use fl_store::recovery::readiness::*;
    match mode {
        TerminalFixtureMode::Selected | TerminalFixtureMode::Original => {
            let mut evidence =
                generation_evidence(j, matches!(mode, TerminalFixtureMode::Original));
            if replacement {
                evidence.operator = "replacement must be ignored for durable terminal retry".into();
                evidence.identity_confirmed = false;
            }
            if matches!(mode, TerminalFixtureMode::Original) {
                finalize_rollback(j, lease, &evidence, filesystem)
            } else {
                finalize_restore(j, lease, &evidence, filesystem)
            }
        }
        TerminalFixtureMode::Absence => {
            let evidence = AbsenceReconciliationRecord {
                version: if replacement { 999 } else { 1 },
                operation_id: j.operation_id,
                candidate_sha256: j.candidate_sha256.clone(),
                operator: fixture_lease_request(lease).operator.clone(),
                recorded_at_unix_ms: filesystem.io().unix_ms().unwrap(),
                no_client_opened_path_while_installed: true,
                evidence_sha256: "a".repeat(64),
            };
            finalize_original_absence(j, lease, &evidence, filesystem)
        }
        TerminalFixtureMode::Abandon | TerminalFixtureMode::AbandonAbsence => {
            abandon_restore(j, lease, filesystem)
        }
    }
}
#[test]
fn restore_terminal_abandon_partial_copy_fixture_preserves_original_or_absence() {
    for mode in [
        TerminalFixtureMode::Abandon,
        TerminalFixtureMode::AbandonAbsence,
    ] {
        let (f, journal, lease) = terminal_fixture(mode);
        let source = fs::read(&f.source).unwrap();
        let source_inode = fs::metadata(&f.source).unwrap().ino();
        let absent = matches!(mode, TerminalFixtureMode::AbandonAbsence);
        if absent {
            assert!(!journal.destination.exists());
        }
        let guard = journal.destination.with_extension("fls.restore-guard");
        let pin = f
            .backups
            .join(".catalog/restore-pins")
            .join(format!("{}.json", journal.operation_id));
        assert!(guard.exists());
        assert!(pin.exists());
        let candidate = journal.staging.join("candidate.fls");
        let candidate_before = fs::read(&candidate).unwrap();
        assert!(matches!(
            terminal_call(mode, &journal, &lease, &f.filesystem(), false).unwrap(),
            RestoreOutcome::Abandoned { .. }
        ));
        assert_eq!(fs::read(&f.source).unwrap(), source);
        assert_eq!(fs::metadata(&f.source).unwrap().ino(), source_inode);
        if absent {
            assert!(!journal.destination.exists());
        }
        assert!(!guard.exists());
        assert!(!pin.exists());
        assert_eq!(fs::read(&candidate).unwrap(), candidate_before);
        let terminal = load_restore_journal(
            &journal.staging.join("journal.bin"),
            &lease,
            &f.filesystem(),
        )
        .unwrap();
        assert_eq!(terminal.phase, RestorePhase::Abandoned);
        assert_eq!(terminal.rollback_attempts, journal.rollback_attempts + 1);
    }
}
#[test]
fn restore_terminal_absence_fixture_finalizes_with_a_genuine_compact_image_baseline() {
    let (f, journal, lease) = terminal_fixture(TerminalFixtureMode::Absence);
    assert_eq!(journal.phase, RestorePhase::OriginalAbsenceRestored);
    assert!(!journal.destination.exists());
    let source = fs::read(&f.source).unwrap();
    let source_inode = fs::metadata(&f.source).unwrap().ino();
    let candidate = journal.staging.join("candidate.fls");
    let candidate_before = fs::read(&candidate).unwrap();
    assert!(candidate_before.len() >= 20 << 20);
    assert!(matches!(
        terminal_call(
            TerminalFixtureMode::Absence,
            &journal,
            &lease,
            &f.filesystem(),
            false
        )
        .unwrap(),
        RestoreOutcome::OriginalAbsenceFinalized { .. }
    ));
    assert!(!journal.destination.exists());
    assert!(
        !journal
            .destination
            .with_extension("fls.restore-guard")
            .exists()
    );
    assert!(
        !f.backups
            .join(".catalog/restore-pins")
            .join(format!("{}.json", journal.operation_id))
            .exists()
    );
    assert_eq!(fs::read(&f.source).unwrap(), source);
    assert_eq!(fs::metadata(&f.source).unwrap().ino(), source_inode);
    assert_eq!(fs::read(&candidate).unwrap(), candidate_before);
    let terminal = load_restore_journal(
        &journal.staging.join("journal.bin"),
        &lease,
        &f.filesystem(),
    )
    .unwrap();
    assert_eq!(terminal.phase, RestorePhase::OriginalAbsenceFinalized);
    assert_eq!(terminal.rollback_attempts, journal.rollback_attempts + 1);
}
struct TerminalCutIo {
    event: RecoveryEvent,
    fired: AtomicUsize,
    events: Mutex<Vec<RecoveryEvent>>,
}
impl TerminalCutIo {
    fn new(event: RecoveryEvent) -> Self {
        Self {
            event,
            fired: AtomicUsize::new(0),
            events: Mutex::new(vec![]),
        }
    }
}
impl RecoveryIo for TerminalCutIo {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        self.events.lock().unwrap().push(event);
        if event == self.event && self.fired.fetch_add(1, Ordering::AcqRel) == 0 {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(())
    }
}
#[test]
fn restore_terminal_sync_cuts_retry_after_accepting_lease_expires_without_attempt_or_append() {
    let cuts = [
        RecoveryEvent::RestoreGuardRemove,
        RecoveryEvent::Sync(Directory::RestoreGuardParent),
        RecoveryEvent::RestorePinRemove,
        RecoveryEvent::Sync(Directory::RestorePinParent),
        RecoveryEvent::Sync(Directory::CatalogParent),
    ];
    for mode in [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::Abandon,
    ] {
        for cut in cuts {
            let (f, journal, lease) = terminal_fixture(mode);
            let source = fs::read(&f.source).unwrap();
            let source_inode = fs::metadata(&f.source).unwrap().ino();
            let hook = Arc::new(TerminalCutIo::new(cut));
            let filesystem = CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits(),
            };
            let result = terminal_call(mode, &journal, &lease, &filesystem, false);
            assert_eq!(
                hook.fired.load(Ordering::Acquire),
                1,
                "missing cut {mode:?} {cut:?}"
            );
            if cut == RecoveryEvent::RestoreGuardRemove {
                assert!(result.is_err());
            } else {
                assert!(matches!(
                    result.unwrap(),
                    RestoreOutcome::OutcomeUnknown { .. }
                ));
            }
            let events = hook.events.lock().unwrap().clone();
            let mut previous = 0;
            for expected in cuts
                .into_iter()
                .take(cuts.iter().position(|e| *e == cut).unwrap() + 1)
            {
                let position = events
                    .iter()
                    .position(|e| *e == expected)
                    .expect("required preceding cleanup boundary missing");
                assert!(
                    position > previous,
                    "cleanup order violated at {mode:?} {cut:?}: {events:?}"
                );
                previous = position;
            }
            let removal = events
                .iter()
                .position(|e| *e == RecoveryEvent::RestoreGuardRemove)
                .unwrap();
            assert!(
                events[..removal].contains(&RecoveryEvent::Sync(Directory::RestoreJournalParent)),
                "terminal was not durable before guard removal"
            );
            let path = journal.staging.join("journal.bin");
            let terminal = load_restore_journal(&path, &lease, &f.filesystem())
                .expect("terminal record must load after every cleanup cut");
            let log = fs::read(&path).unwrap();
            let counters = (
                terminal.current_attempt,
                terminal.resume_finalize_attempts,
                terminal.rollback_attempts,
            );
            let context = serde_json::to_value(&terminal.terminal_evidence_context).unwrap();
            let clock = ClockFs {
                native: f.filesystem(),
                now: fixture_lease_request(&lease).expires_at_unix_ms + 1,
            };
            assert!(
                terminal_call(mode, &terminal, &lease, &clock, true).is_err(),
                "expired current authority must refuse"
            );
            assert_eq!(fs::read(&path).unwrap(), log);
            let renewed = renewed_lease(&terminal, &clock);
            assert!(
                fixture_lease_request(&renewed).issued_at_unix_ms
                    > terminal
                        .terminal_evidence_context
                        .as_ref()
                        .unwrap()
                        .accepted_at_unix_ms
            );
            // Remove surviving selected catalog material before retry. There is
            // no catalog lookup in cleanup; quarantine is reversible and inert.
            let selected = request_bundle_path(&f, terminal.selected_bundle_operation);
            if selected.exists() {
                fs::rename(&selected, selected.with_extension("terminal-test-retained")).unwrap();
            }
            let done = terminal_call(mode, &terminal, &renewed, &clock, true).unwrap();
            assert!(!matches!(done, RestoreOutcome::OutcomeUnknown { .. }));
            let current = load_restore_journal(&path, &renewed, &clock).unwrap();
            assert_eq!(
                fs::read(&path).unwrap(),
                log,
                "terminal retry appended at {mode:?} {cut:?}"
            );
            assert_eq!(
                (
                    current.current_attempt,
                    current.resume_finalize_attempts,
                    current.rollback_attempts
                ),
                counters
            );
            assert_eq!(
                serde_json::to_value(&current.terminal_evidence_context).unwrap(),
                context
            );
            assert!(
                !terminal
                    .destination
                    .with_extension("fls.restore-guard")
                    .exists()
            );
            assert!(
                !f.backups
                    .join(".catalog/restore-pins")
                    .join(format!("{}.json", terminal.operation_id))
                    .exists()
            );
            for name in ["journal.bin", "journal.bin.lock", "candidate.fls"] {
                if name != "candidate.fls"
                    || matches!(
                        mode,
                        TerminalFixtureMode::Absence
                            | TerminalFixtureMode::Abandon
                            | TerminalFixtureMode::AbandonAbsence
                    )
                {
                    assert!(terminal.staging.join(name).exists());
                }
            }
            assert_eq!(fs::metadata(&f.source).unwrap().ino(), source_inode);
            assert_eq!(fs::read(&f.source).unwrap(), source);
        }
    }
}
fn request_bundle_path(f: &Fixture, operation: uuid::Uuid) -> std::path::PathBuf {
    // Locate by verified catalog API before the retry, never fabricate directory
    // naming assumptions into the product. This is test setup only.
    fl_store::recovery::catalog::select_completed_bundle(operation, &f.filesystem(), &limits())
        .unwrap()
        .path()
        .to_path_buf()
}
#[test]
fn restore_fourth_reserved_attempt_terminal_cleanup_is_free_despite_exhausted_flag() {
    let (f, mut journal, lease) = terminal_fixture(TerminalFixtureMode::Original);
    for _ in 0..2 {
        let RestoreOutcome::RolledBack { journal: next } =
            rollback_restore(&journal, &lease, &f.filesystem()).unwrap()
        else {
            panic!("not rolled back")
        };
        journal = next;
    }
    assert_eq!(journal.rollback_attempts, 3);
    let hook = Arc::new(TerminalCutIo::new(RecoveryEvent::Sync(
        Directory::RestorePinParent,
    )));
    let filesystem = CutFs {
        f: &f,
        io: hook,
        limits: limits(),
    };
    assert!(matches!(
        terminal_call(
            TerminalFixtureMode::Original,
            &journal,
            &lease,
            &filesystem,
            false
        )
        .unwrap(),
        RestoreOutcome::OutcomeUnknown { .. }
    ));
    let path = journal.staging.join("journal.bin");
    let terminal = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(terminal.rollback_attempts, 4);
    assert!(terminal.forensic_handling_required);
    let bytes = fs::read(&path).unwrap();
    assert!(matches!(
        terminal_call(
            TerminalFixtureMode::Original,
            &terminal,
            &lease,
            &f.filesystem(),
            true
        )
        .unwrap(),
        RestoreOutcome::Finalized { .. }
    ));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}
#[test]
fn restore_malformed_absence_attestation_does_not_charge_or_classify_forensics() {
    let (f, journal, lease) = terminal_fixture(TerminalFixtureMode::Absence);
    let path = journal.staging.join("journal.bin");
    let before = fs::read(&path).unwrap();
    assert!(
        terminal_call(
            TerminalFixtureMode::Absence,
            &journal,
            &lease,
            &f.filesystem(),
            true
        )
        .is_err()
    );
    let current = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(current.current_attempt, journal.current_attempt);
    assert!(!current.forensic_handling_required);
    assert!(
        journal
            .destination
            .with_extension("fls.restore-guard")
            .exists()
    );
}

struct ReplaceTerminalMetadata {
    target: RecoveryEvent,
    path: std::path::PathBuf,
    same_inode: bool,
    later_open: bool,
    armed: std::sync::atomic::AtomicBool,
    fired: std::sync::atomic::AtomicBool,
    replacement: Vec<u8>,
}
impl RecoveryIo for ReplaceTerminalMetadata {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        if event == self.target {
            self.armed.store(true, Ordering::Release);
        }
        let trigger = self.armed.load(Ordering::Acquire)
            && if self.later_open {
                event == RecoveryEvent::OpenFile
            } else {
                event == self.target
            };
        if trigger && !self.fired.swap(true, Ordering::AcqRel) {
            if self.same_inode {
                fs::write(&self.path, &self.replacement)?;
            } else {
                let replacement = self.path.with_extension("replacement");
                fs::write(&replacement, &self.replacement)?;
                fs::set_permissions(
                    &replacement,
                    std::os::unix::fs::PermissionsExt::from_mode(0o600),
                )?;
                fs::rename(&replacement, &self.path)?;
            }
        }
        Ok(())
    }
}
#[test]
fn restore_terminal_guard_and_pin_replacement_or_content_mutation_after_decode_are_retained() {
    for target in [
        RecoveryEvent::RestoreGuardRemove,
        RecoveryEvent::RestorePinRemove,
    ] {
        for same_inode in [false, true] {
            for later_open in [false, true] {
                let (f, journal, lease) = terminal_fixture(TerminalFixtureMode::Selected);
                let path = if target == RecoveryEvent::RestoreGuardRemove {
                    journal.destination.with_extension("fls.restore-guard")
                } else {
                    f.backups
                        .join(".catalog/restore-pins")
                        .join(format!("{}.json", journal.operation_id))
                };
                let mut replacement = fs::read(&path).unwrap();
                // Change valid JSON bytes while retaining the same file length.
                let at = replacement.iter().position(|b| *b == b'1').unwrap();
                replacement[at] = b'9';
                let hook = Arc::new(ReplaceTerminalMetadata {
                    target,
                    path: path.clone(),
                    same_inode,
                    later_open,
                    armed: false.into(),
                    fired: false.into(),
                    replacement: replacement.clone(),
                });
                let filesystem = CutFs {
                    f: &f,
                    io: hook.clone(),
                    limits: limits(),
                };
                let result = terminal_call(
                    TerminalFixtureMode::Selected,
                    &journal,
                    &lease,
                    &filesystem,
                    false,
                );
                assert!(
                    hook.fired.load(Ordering::Acquire),
                    "mutation hook missed {target:?}/{same_inode}/{later_open}"
                );
                assert!(
                    result.is_err()
                        || matches!(result.unwrap(), RestoreOutcome::OutcomeUnknown { .. })
                );
                assert_eq!(
                    fs::read(&path).unwrap(),
                    replacement,
                    "replacement was removed at {target:?}/{same_inode}/{later_open}"
                );
                if target == RecoveryEvent::RestoreGuardRemove {
                    assert!(
                        f.backups
                            .join(".catalog/restore-pins")
                            .join(format!("{}.json", journal.operation_id))
                            .exists()
                    );
                }
            }
        }
    }
}
#[test]
fn restore_rollback_uses_immutable_candidate_proof_after_selected_quarantine() {
    let (f, journal, lease) = terminal_fixture(TerminalFixtureMode::Selected);
    let selected = request_bundle_path(&f, journal.selected_bundle_operation);
    fs::rename(&selected, selected.with_extension("quarantined-for-test")).unwrap();
    let RestoreOutcome::RolledBack { journal: current } =
        rollback_restore(&journal, &lease, &f.filesystem()).unwrap()
    else {
        panic!("rollback needs no live selected bundle")
    };
    assert_eq!(current.rollback_attempts, 1);
    assert_eq!(
        inspect_file_binding(&f.source, &limits()).unwrap(),
        journal.original
    );
}
#[cfg(feature = "cli")]
#[test]
fn restore_abandon_cli_retries_under_isolated_process_environment() {
    let (f, journal, lease) = terminal_fixture(TerminalFixtureMode::Abandon);
    let private = f.root.path().join("cli");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(
        &private,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    for sub in ["home", "config", "data", "run", "tmp"] {
        fs::create_dir(private.join(sub)).unwrap();
    }
    let request = private.join("continuation.json");
    fs::write(
        &request,
        serde_json::to_vec(
            &serde_json::json!({ "version": 1, "operation_id": journal.operation_id,
        "destination": journal.destination, "journal": journal.staging.join("journal.bin") }),
        )
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(
        &request,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    for _ in 0..2 {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_ferrostore"))
            .env_clear()
            .env("HOME", private.join("home"))
            .env("XDG_CONFIG_HOME", private.join("config"))
            .env("XDG_DATA_HOME", private.join("data"))
            .env("XDG_RUNTIME_DIR", private.join("run"))
            .env("TMPDIR", private.join("tmp"))
            .current_dir(&private)
            .arg("recovery")
            .arg("--root")
            .arg(&f.backups)
            .arg("--lease")
            .arg(lease.path())
            .arg("--request")
            .arg(&request)
            .arg("abandon")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["outcome"], "abandoned");
    }
    let current = load_restore_journal(
        &journal.staging.join("journal.bin"),
        &lease,
        &f.filesystem(),
    )
    .unwrap();
    assert_eq!(current.rollback_attempts, 1);
    assert_eq!(current.current_attempt, 1);
    assert_eq!(current.phase, RestorePhase::Abandoned);
    assert!(journal.staging.join("candidate.fls").exists());
}

struct RecreateTerminalAbsence {
    destination: std::path::PathBuf,
    fired: AtomicUsize,
}
impl RecoveryIo for RecreateTerminalAbsence {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        if event == RecoveryEvent::RestoreGuardRemove {
            self.fired.fetch_add(1, Ordering::AcqRel);
            fs::write(&self.destination, b"recreated destination must survive")?;
        }
        Ok(())
    }
}
#[test]
fn restore_absence_guard_event_preserves_recreated_entry_on_first_cleanup_and_free_retry() {
    for mode in [
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::AbandonAbsence,
    ] {
        let (f, journal, lease) = terminal_fixture(mode);
        for retry in [false, true] {
            let current = load_restore_journal(
                &journal.staging.join("journal.bin"),
                &lease,
                &f.filesystem(),
            )
            .unwrap();
            let before = fs::read(journal.staging.join("journal.bin")).unwrap();
            let hook = Arc::new(RecreateTerminalAbsence {
                destination: journal.destination.clone(),
                fired: AtomicUsize::new(0),
            });
            let filesystem = CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits(),
            };
            assert!(terminal_call(mode, &current, &lease, &filesystem, retry).is_err());
            assert_eq!(hook.fired.load(Ordering::Acquire), 1);
            assert_eq!(
                fs::read(&journal.destination).unwrap(),
                b"recreated destination must survive"
            );
            assert!(
                journal
                    .destination
                    .with_extension("fls.restore-guard")
                    .exists()
            );
            assert!(
                f.backups
                    .join(".catalog/restore-pins")
                    .join(format!("{}.json", journal.operation_id))
                    .exists()
            );
            if retry {
                assert_eq!(
                    fs::read(journal.staging.join("journal.bin")).unwrap(),
                    before
                );
            }
            // Explicit fixture operator restores the original absence for retry.
            fs::remove_file(&journal.destination).unwrap();
        }
    }
}
#[test]
fn restore_finalize_intent_renews_current_authority_without_replacing_accepting_evidence() {
    let (f, journal, lease) = terminal_fixture(TerminalFixtureMode::Selected);
    let hook = Arc::new(TerminalCutIo::new(RecoveryEvent::Sync(
        Directory::RestoreDestinationParent,
    )));
    let filesystem = CutFs {
        f: &f,
        io: hook,
        limits: limits(),
    };
    assert!(
        terminal_call(
            TerminalFixtureMode::Selected,
            &journal,
            &lease,
            &filesystem,
            false
        )
        .is_err()
    );
    let path = journal.staging.join("journal.bin");
    let intent = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(intent.phase, RestorePhase::FinalizeIntent);
    let accepted = intent.reconciliation.as_ref().unwrap().clone();
    let context = intent.terminal_evidence_context.as_ref().unwrap().clone();
    let clock = ClockFs {
        native: f.filesystem(),
        now: fixture_lease_request(&lease).expires_at_unix_ms + 1,
    };
    let renewed = renewed_lease(&intent, &clock);
    let before = fs::read(&path).unwrap();
    assert!(
        terminal_call(
            TerminalFixtureMode::Selected,
            &intent,
            &renewed,
            &clock,
            true
        )
        .is_err()
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "replacement intent evidence charged or appended"
    );
    assert!(matches!(
        fl_store::recovery::readiness::finalize_restore(&intent, &renewed, &accepted, &clock)
            .unwrap(),
        RestoreOutcome::Finalized { .. }
    ));
    let done = load_restore_journal(&path, &renewed, &clock).unwrap();
    assert_eq!(done.reconciliation.as_ref().unwrap(), &accepted);
    assert_eq!(done.terminal_evidence_context.as_ref().unwrap(), &context);
}

// Both 12-case covering arrays meet the mode/member pair lower bound (4 * 3).
// FL_RECOVERY_EXHAUSTIVE=1 retains the complete cross products and assertions.
// Dimension order: mode, removal target, generation, mutation kind, later open.
const TERMINAL_REMOVAL_DIMENSIONS: [usize; 5] = [4, 2, 3, 2, 2];
const TERMINAL_REMOVAL_PAIRWISE: [[usize; 5]; 12] = [
    [0, 0, 0, 0, 1],
    [0, 1, 1, 0, 0],
    [0, 1, 2, 1, 1],
    [1, 0, 0, 0, 1],
    [1, 0, 1, 1, 1],
    [1, 1, 2, 0, 0],
    [2, 0, 1, 0, 0],
    [2, 0, 2, 0, 1],
    [2, 1, 0, 1, 0],
    [3, 0, 1, 1, 1],
    [3, 0, 2, 0, 0],
    [3, 1, 0, 0, 0],
];
// Dimension order: mode, initial cut, sync target, member, mutation kind, later open.
const TERMINAL_SYNC_DIMENSIONS: [usize; 6] = [4, 2, 3, 3, 2, 2];
const TERMINAL_SYNC_PAIRWISE: [[usize; 6]; 12] = [
    [0, 0, 2, 2, 0, 1],
    [0, 1, 0, 0, 1, 0],
    [0, 1, 1, 1, 0, 1],
    [1, 0, 0, 0, 0, 1],
    [1, 0, 1, 1, 1, 0],
    [1, 1, 2, 2, 1, 0],
    [2, 0, 2, 1, 0, 1],
    [2, 1, 0, 2, 1, 1],
    [2, 1, 1, 0, 0, 0],
    [3, 0, 1, 2, 0, 1],
    [3, 0, 2, 0, 0, 0],
    [3, 1, 0, 1, 1, 1],
];

fn terminal_matrix_cases<const N: usize>(
    exhaustive: bool,
    dimensions: [usize; N],
    pairwise: &[[usize; N]],
) -> Vec<[usize; N]> {
    if !exhaustive {
        return pairwise.to_vec();
    }
    (0..dimensions.iter().product())
        .map(|mut index| {
            let mut row = [0; N];
            for i in (0..N).rev() {
                row[i] = index % dimensions[i];
                index /= dimensions[i];
            }
            row
        })
        .collect()
}

fn assert_terminal_schedule<const N: usize>(
    dimensions: [usize; N],
    pairwise: &[[usize; N]],
    exhaustive_count: usize,
) {
    for (exhaustive, expected) in [(false, 12), (true, exhaustive_count)] {
        let rows = terminal_matrix_cases(exhaustive, dimensions, pairwise);
        assert_eq!(rows.len(), expected, "exhaustive={exhaustive}");
        let unique: std::collections::BTreeSet<_> = rows.iter().collect();
        assert_eq!(unique.len(), rows.len(), "duplicate cases");
        for row in &rows {
            for i in 0..N {
                assert!(row[i] < dimensions[i], "out of range: {row:?}");
            }
        }
        for i in 0..N {
            for j in i + 1..N {
                for a in 0..dimensions[i] {
                    for b in 0..dimensions[j] {
                        assert!(
                            rows.iter().any(|row| row[i] == a && row[j] == b),
                            "missing dimensions {i}/{j}, values {a}/{b}, exhaustive={exhaustive}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn restore_terminal_removal_schedule_covers_all_dimension_pairs() {
    assert_terminal_schedule(TERMINAL_REMOVAL_DIMENSIONS, &TERMINAL_REMOVAL_PAIRWISE, 96);
}

#[test]
fn restore_terminal_sync_schedule_covers_all_dimension_pairs() {
    assert_terminal_schedule(TERMINAL_SYNC_DIMENSIONS, &TERMINAL_SYNC_PAIRWISE, 288);
}

#[test]
fn restore_terminal_all_generations_are_held_across_removal_and_later_authority_open() {
    let modes = [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::Abandon,
    ];
    let targets = [
        RecoveryEvent::RestoreGuardRemove,
        RecoveryEvent::RestorePinRemove,
    ];
    for [mode, target, member, same_inode, later_open] in terminal_matrix_cases(
        std::env::var("FL_RECOVERY_EXHAUSTIVE").as_deref() == Ok("1"),
        TERMINAL_REMOVAL_DIMENSIONS,
        &TERMINAL_REMOVAL_PAIRWISE,
    ) {
        let mode = modes[mode];
        let target = targets[target];
        let same_inode = same_inode != 0;
        let later_open = later_open != 0;
        let (f, journal, lease) = terminal_fixture(mode);
        let cut = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreGuardRemove));
        assert!(
            terminal_call(
                mode,
                &journal,
                &lease,
                &CutFs {
                    f: &f,
                    io: cut,
                    limits: limits()
                },
                false
            )
            .is_err()
        );
        let journal_path = journal.staging.join("journal.bin");
        let terminal = load_restore_journal(&journal_path, &lease, &f.filesystem()).unwrap();
        let log = fs::read(&journal_path).unwrap();
        let paths = [
            terminal.destination.clone(),
            terminal.staging.join("candidate.fls"),
            terminal.staging.join("original.fls"),
        ];
        let path = paths[member].clone();
        let others: Vec<_> = paths
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != member)
            .map(|(_, path)| {
                (
                    path.clone(),
                    fs::read(path).ok(),
                    fs::metadata(path).ok().map(|m| m.ino()),
                )
            })
            .collect();
        let mut replacement = fs::read(&path).unwrap_or_default();
        if replacement.is_empty() {
            replacement.push(0x55);
        } else {
            replacement[0] ^= 0x55;
        }
        let hook = Arc::new(ReplaceTerminalMetadata {
            target,
            path: path.clone(),
            same_inode,
            later_open,
            armed: false.into(),
            fired: false.into(),
            replacement: replacement.clone(),
        });
        let result = terminal_call(
            mode,
            &terminal,
            &lease,
            &CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits(),
            },
            true,
        );
        assert!(
            hook.fired.load(Ordering::Acquire),
            "missed {mode:?}/{target:?}/{member}/{same_inode}/{later_open}"
        );
        if target == RecoveryEvent::RestoreGuardRemove {
            assert!(result.is_err());
        } else {
            assert!(matches!(
                result.unwrap(),
                RestoreOutcome::OutcomeUnknown { .. }
            ));
        }
        assert_eq!(
            fs::read(&path).unwrap(),
            replacement,
            "mutated generation was altered"
        );
        assert_eq!(
            fs::read(&journal_path).unwrap(),
            log,
            "terminal retry appended"
        );
        assert!(
            f.backups
                .join(".catalog/restore-pins")
                .join(format!("{}.json", terminal.operation_id))
                .exists(),
            "pin was removed after a generation mutation"
        );
        if target == RecoveryEvent::RestoreGuardRemove {
            assert!(
                terminal
                    .destination
                    .with_extension("fls.restore-guard")
                    .exists()
            );
        }
        for (path, bytes, inode) in others {
            assert_eq!(fs::read(&path).ok(), bytes);
            assert_eq!(fs::metadata(&path).ok().map(|m| m.ino()), inode);
        }
    }
}
#[test]
fn restore_terminal_absent_guard_and_pin_recheck_authority_and_layout_after_every_sync() {
    let cuts = [
        RecoveryEvent::Sync(Directory::RestoreGuardParent),
        RecoveryEvent::Sync(Directory::RestorePinParent),
    ];
    let boundaries = [
        RecoveryEvent::Sync(Directory::RestoreGuardParent),
        RecoveryEvent::Sync(Directory::RestorePinParent),
        RecoveryEvent::Sync(Directory::CatalogParent),
    ];
    let modes = [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::Abandon,
    ];
    for [mode, initial_cut, target, member, same_inode, later_open] in terminal_matrix_cases(
        std::env::var("FL_RECOVERY_EXHAUSTIVE").as_deref() == Ok("1"),
        TERMINAL_SYNC_DIMENSIONS,
        &TERMINAL_SYNC_PAIRWISE,
    ) {
        let mode = modes[mode];
        let initial_cut = cuts[initial_cut];
        let target = boundaries[target];
        let same_inode = same_inode != 0;
        let later_open = later_open != 0;
        let (f, journal, lease) = terminal_fixture(mode);
        let guard = journal.destination.with_extension("fls.restore-guard");
        let guard_bytes = fs::read(&guard).unwrap();
        let hook = Arc::new(TerminalCutIo::new(initial_cut));
        assert!(matches!(
            terminal_call(
                mode,
                &journal,
                &lease,
                &CutFs {
                    f: &f,
                    io: hook,
                    limits: limits()
                },
                false
            )
            .unwrap(),
            RestoreOutcome::OutcomeUnknown { .. }
        ));
        assert!(!guard.exists());
        let journal_path = journal.staging.join("journal.bin");
        let terminal = load_restore_journal(&journal_path, &lease, &f.filesystem()).unwrap();
        let log = fs::read(&journal_path).unwrap();
        let path = match member {
            0 => lease.path().to_path_buf(),
            1 => terminal.destination.clone(),
            _ => guard.clone(),
        };
        let replacement = match member {
            0 => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                value["operator"] = serde_json::json!("mutated-authority");
                serde_json::to_vec(&value).unwrap()
            }
            1 => {
                let mut bytes = fs::read(&path).unwrap_or_default();
                if bytes.is_empty() {
                    bytes.push(0x33);
                } else {
                    bytes[0] ^= 0x33;
                }
                bytes
            }
            _ => guard_bytes,
        };
        let hook = Arc::new(ReplaceTerminalMetadata {
            target,
            path: path.clone(),
            same_inode,
            later_open,
            armed: false.into(),
            fired: false.into(),
            replacement: replacement.clone(),
        });
        let result = terminal_call(
            mode,
            &terminal,
            &lease,
            &CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits(),
            },
            true,
        );
        assert!(
            hook.fired.load(Ordering::Acquire),
            "missed absent branch {mode:?}/{initial_cut:?}/{target:?}/{member}/{same_inode}/{later_open}"
        );
        assert!(matches!(
            result.unwrap(),
            RestoreOutcome::OutcomeUnknown { .. }
        ));
        assert_eq!(fs::read(&path).unwrap(), replacement);
        assert_eq!(
            fs::read(&journal_path).unwrap(),
            log,
            "terminal retry appended"
        );
        if initial_cut == cuts[0] && target == boundaries[0] {
            assert!(
                f.backups
                    .join(".catalog/restore-pins")
                    .join(format!("{}.json", terminal.operation_id))
                    .exists(),
                "pin removed before safe durable guard absence"
            );
        }
    }
}
struct OneShotAbsenceCandidateReadError {
    inode: u64,
    fired: AtomicUsize,
    events: Mutex<Vec<RecoveryEvent>>,
}
impl RecoveryIo for OneShotAbsenceCandidateReadError {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
    fn read_exact_at(
        &self,
        file: &std::fs::File,
        bytes: &mut [u8],
        offset: u64,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        if file.metadata()?.ino() == self.inode && self.fired.fetch_add(1, Ordering::AcqRel) == 0 {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        file.read_exact_at(bytes, offset)
    }
}
#[test]
fn restore_absence_ordinary_inspection_read_error_has_no_attempt_append_or_forensic_flag() {
    let (f, journal, lease) = terminal_fixture(TerminalFixtureMode::Absence);
    let path = journal.staging.join("journal.bin");
    let before = fs::read(&path).unwrap();
    let candidate = journal.staging.join("candidate.fls");
    let candidate_before = fs::read(&candidate).unwrap();
    let hook = Arc::new(OneShotAbsenceCandidateReadError {
        inode: fs::metadata(&candidate).unwrap().ino(),
        fired: AtomicUsize::new(0),
        events: Mutex::new(vec![]),
    });
    assert!(
        terminal_call(
            TerminalFixtureMode::Absence,
            &journal,
            &lease,
            &CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits()
            },
            false
        )
        .is_err()
    );
    assert_eq!(
        hook.fired.load(Ordering::Acquire),
        1,
        "pre-attempt candidate inspection fault was not reached exactly once"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&candidate).unwrap(), candidate_before);
    let current = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(
        (
            current.current_attempt,
            current.rollback_attempts,
            current.resume_finalize_attempts
        ),
        (
            journal.current_attempt,
            journal.rollback_attempts,
            journal.resume_finalize_attempts
        )
    );
    assert!(!current.forensic_handling_required);
    assert!(
        journal
            .destination
            .with_extension("fls.restore-guard")
            .exists()
    );
    assert!(
        f.backups
            .join(".catalog/restore-pins")
            .join(format!("{}.json", journal.operation_id))
            .exists()
    );
    let events = hook.events.lock().unwrap();
    assert!(!events.contains(&RecoveryEvent::RestoreGuardRemove));
    assert!(!events.contains(&RecoveryEvent::RestorePinRemove));
    assert!(!events.contains(&RecoveryEvent::Sync(Directory::RestoreJournalParent)));
}

#[test]
fn restore_terminal_retry_accepts_whitespace_padded_renewed_lease_above_journal_payload() {
    for mode in [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::Abandon,
    ] {
        let (f, journal, lease) = terminal_fixture(mode);
        let cut = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreGuardRemove));
        assert!(
            terminal_call(
                mode,
                &journal,
                &lease,
                &CutFs {
                    f: &f,
                    io: cut.clone(),
                    limits: limits()
                },
                false
            )
            .is_err()
        );
        assert_eq!(
            cut.fired.load(Ordering::Acquire),
            1,
            "terminal cleanup cut was not reached: {mode:?}"
        );
        let path = journal.staging.join("journal.bin");
        let terminal = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        let log = fs::read(&path).unwrap();
        let counters = (
            terminal.current_attempt,
            terminal.resume_finalize_attempts,
            terminal.rollback_attempts,
        );
        let context = serde_json::to_value(&terminal.terminal_evidence_context).unwrap();
        let clock = ClockFs {
            native: f.filesystem(),
            now: fixture_lease_request(&lease).expires_at_unix_ms + 1,
        };
        let renewed = renewed_lease(&terminal, &clock);
        let renewed_request = fixture_lease_request(&renewed);
        assert!(
            renewed_request.issued_at_unix_ms
                > terminal
                    .terminal_evidence_context
                    .as_ref()
                    .unwrap()
                    .accepted_at_unix_ms
        );

        // The fixed RJR1 schedule caps a frame at capacity / 96 - 72.
        // Persisted lease JSON has the existing independent byte/memory limits;
        // trailing JSON whitespace changes no typed field or accepting evidence.
        let payload_ceiling = terminal.journal_capacity / 96 - 72;
        let mut padded = fs::read(renewed.path()).unwrap();
        let padded_len = usize::try_from(payload_ceiling).unwrap() + 1;
        assert!(
            padded.len() < padded_len,
            "fixture lease must start below frame ceiling"
        );
        padded.resize(padded_len, b' ');
        let padded_bytes = padded.len() as u64;
        assert!(padded_bytes > payload_ceiling);
        assert!(padded_bytes < limits().byte_limit.min(terminal.budget.read_byte_ceiling));
        assert!(
            padded_bytes * 64 + 65536
                < limits()
                    .memory_byte_limit
                    .min(terminal.budget.memory_ceiling)
        );
        fs::write(renewed.path(), &padded).unwrap();
        fs::File::open(renewed.path()).unwrap().sync_all().unwrap();
        let persisted = OfflineLease::load(renewed.path(), &clock, &limits())
            .expect("whitespace-padded current lease remains valid under existing read limits");
        let persisted_request = fixture_lease_request(&persisted);
        assert_eq!(persisted_request, renewed_request);
        let admitted = load_restore_journal(&path, &persisted, &clock)
            .expect("typed terminal admission accepts the padded renewed lease");
        assert_ne!(
            admitted.binding, persisted_request,
            "current lease B must differ from accepting lease A"
        );
        let done = terminal_call(mode, &admitted, &persisted, &clock, true).unwrap();
        assert!(
            !matches!(done, RestoreOutcome::OutcomeUnknown { .. }),
            "cleanup incomplete: {mode:?}"
        );
        let current = load_restore_journal(&path, &persisted, &clock).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            log,
            "terminal retry appended: {mode:?}"
        );
        assert_eq!(
            (
                current.current_attempt,
                current.resume_finalize_attempts,
                current.rollback_attempts
            ),
            counters
        );
        assert_eq!(
            serde_json::to_value(&current.terminal_evidence_context).unwrap(),
            context
        );
        assert_eq!(fs::read(persisted.path()).unwrap(), padded);
        assert!(
            !terminal
                .destination
                .with_extension("fls.restore-guard")
                .exists()
        );
        assert!(
            !f.backups
                .join(".catalog/restore-pins")
                .join(format!("{}.json", terminal.operation_id))
                .exists()
        );
    }
}

// PR481 external review #8: every namespace mutation must durably sync BOTH dirs
// before any following journal append or namespace mutation can begin.
fn assert_restore_namespace_sync_order(events: &[RecoveryEvent], action: RecoveryEvent) {
    let positions: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| (*e == action).then_some(i))
        .collect();
    assert_eq!(
        positions.len(),
        1,
        "missing or duplicated {action:?}: {events:?}"
    );
    for mut position in positions {
        // The typed marker and low-level marker describe the SAME operation.
        if matches!(
            action,
            RecoveryEvent::RestoreInstall | RecoveryEvent::RestoreReverseExchange
        ) {
            assert_eq!(events.get(position + 1), Some(&RecoveryEvent::Exchange));
            position += 1;
        } else if action == RecoveryEvent::RestoreRelocate {
            assert_eq!(events.get(position + 1), Some(&RecoveryEvent::RenameEntry));
            position += 1;
        }
        let suffix = &events[position + 1..];
        let next = suffix
            .iter()
            .position(|e| {
                matches!(
                    e,
                    RecoveryEvent::Write(Member::RestoreJournal)
                        | RecoveryEvent::Exchange
                        | RecoveryEvent::RestoreRelocate
                        | RecoveryEvent::RestoreReverseExchange
                        | RecoveryEvent::RestoreInstall
                        | RecoveryEvent::RenameEntry
                )
            })
            .unwrap_or(suffix.len());
        let before_next = &suffix[..next];
        for dir in [
            Directory::RestoreDestinationParent,
            Directory::RestoreStagingParent,
        ] {
            assert!(
                before_next.contains(&RecoveryEvent::Sync(dir)),
                "{action:?} must sync {dir:?} before following durable phase/rename: {suffix:?}"
            );
        }
        assert!(
            next < suffix.len(),
            "fixture must exercise the following durable phase/rename"
        );
    }
}

#[test]
fn restore_each_forward_reverse_and_relocation_syncs_before_next_phase() {
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    #[derive(Default)]
    struct NamespaceTraceIo {
        events: Mutex<Vec<RecoveryEvent>>,
    }
    impl RecoveryIo for NamespaceTraceIo {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }
    let io = Arc::new(NamespaceTraceIo::default());
    let filesystem = CutFs {
        f: &f,
        io: io.clone(),
        limits: limits(),
    };
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &filesystem).unwrap()
    else {
        panic!("not installed")
    };
    let forward = io.events.lock().unwrap().clone();
    let probes: Vec<_> = forward
        .iter()
        .enumerate()
        .filter_map(|(i, e)| (*e == RecoveryEvent::RestoreExchangeProbe).then_some(i))
        .collect();
    let installs: Vec<_> = forward
        .iter()
        .enumerate()
        .filter_map(|(i, e)| (*e == RecoveryEvent::RestoreInstall).then_some(i))
        .collect();
    assert_eq!(
        probes.len(),
        1,
        "expected one pre-guard exchange probe: {forward:?}"
    );
    assert_eq!(
        installs.len(),
        1,
        "expected one forward install exchange: {forward:?}"
    );
    assert!(
        probes[0] < installs[0],
        "probe must precede forward installation"
    );
    assert_eq!(forward.get(probes[0] + 1), Some(&RecoveryEvent::Exchange));
    assert_eq!(forward.get(installs[0] + 1), Some(&RecoveryEvent::Exchange));
    assert_ne!(
        probes[0] + 1,
        installs[0] + 1,
        "probe and install must use distinct exchanges"
    );
    assert_eq!(
        forward
            .iter()
            .filter(|e| **e == RecoveryEvent::Exchange)
            .count(),
        2
    );
    assert_restore_namespace_sync_order(&forward, RecoveryEvent::RestoreInstall);
    assert_restore_namespace_sync_order(&forward, RecoveryEvent::RestoreRelocate);
    io.events.lock().unwrap().clear();
    assert!(matches!(
        rollback_restore(&journal, &lease, &filesystem).unwrap(),
        RestoreOutcome::RolledBack { .. }
    ));
    assert_restore_namespace_sync_order(
        &io.events.lock().unwrap(),
        RecoveryEvent::RestoreReverseExchange,
    );
}

// All manifests/reports below are produced by backup(), never rewritten fixtures.
#[test]
fn restore_finalize_uses_each_generations_distinct_dependency_profile() {
    use fl_store::recovery::{
        backup::*,
        profiles::{DependencyPolicy, ReferenceDependency},
        readiness::*,
    };
    for original in [false, true] {
        let f = Fixture::new();
        let dependency = |digit: &str| ReferenceDependency {
            reference_sha256: digit.repeat(64),
            catalog_identity: format!("fixture-dependency-{digit}"),
            required_sequence: Some(if digit == "a" { 17 } else { 29 }),
            policy: DependencyPolicy::ReconcileBeforeAdmission,
        };
        let resolution = |d: &ReferenceDependency| DependencyResolution {
            reference_sha256: d.reference_sha256.clone(),
            catalog_identity: d.catalog_identity.clone(),
            observed_sequence: d.required_sequence.unwrap(),
            evidence_sha256: "e".repeat(64),
        };
        let selected_dep = dependency("a");
        let original_dep = dependency("b");
        let publish = |d: &ReferenceDependency| {
            let mut backup_request = f.backup_request();
            backup_request.profile.dependencies = vec![d.clone()];
            let profile = backup_request.profile.clone();
            let owner = FerroStore::open(&f.source).unwrap();
            let PublicationOutcome::Durable(bundle) = backup(
                &owner,
                &backup_request,
                &f.filesystem(),
                &fl_store::recovery::CancelToken::new(),
            )
            .unwrap() else {
                panic!("not durable")
            };
            drop(owner);
            (bundle, profile)
        };
        let (selected_bundle, _) = publish(&selected_dep);
        let (original_bundle, original_profile) = publish(&original_dep);
        assert_ne!(
            fs::read(selected_bundle.path().join("profile.json")).unwrap(),
            fs::read(original_bundle.path().join("profile.json")).unwrap()
        );
        let (mut request, lease) = prepare_restore_bundle(&f, &f.source, selected_bundle);
        request.original_profile = original_profile;
        request.available_dependencies = vec![resolution(&selected_dep), resolution(&original_dep)];
        let RestoreOutcome::Installed { mut journal } =
            restore_offline(&request, &lease, &f.filesystem()).unwrap()
        else {
            panic!("not installed")
        };
        assert_eq!(
            journal.selected_dependencies.references,
            vec![selected_dep.clone()]
        );
        assert_eq!(
            journal.original_dependencies.references,
            vec![original_dep.clone()]
        );
        assert_eq!(journal.verification.as_ref().unwrap().dependency_count, 1);
        assert_eq!(
            journal
                .original_verification
                .as_ref()
                .unwrap()
                .dependency_count,
            1
        );
        if original {
            let RestoreOutcome::RolledBack {
                journal: rolled_back,
            } = rollback_restore(&journal, &lease, &f.filesystem()).unwrap()
            else {
                panic!("not rolled back")
            };
            journal = rolled_back;
        }
        let mut evidence = generation_evidence(&journal, original);
        // Keep the target report identity valid while supplying ONLY the other
        // generation's dependency. An incorrect selected fallback must refuse.
        evidence.dependencies = vec![resolution(if original {
            &selected_dep
        } else {
            &original_dep
        })];
        let path = journal.staging.join("journal.bin");
        let before = fs::read(&path).unwrap();
        let guard = f.source.with_extension("fls.restore-guard");
        let guard_before = fs::read(&guard).unwrap();
        let finalize = |r: &ReconciliationRecord| {
            if original {
                finalize_rollback(&journal, &lease, r, &f.filesystem())
            } else {
                finalize_restore(&journal, &lease, r, &f.filesystem())
            }
        };
        assert!(finalize(&evidence).is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "wrong generation must refuse before charging"
        );
        assert_eq!(
            fs::read(&guard).unwrap(),
            guard_before,
            "rejected operation must preserve guard bytes"
        );
        evidence.dependencies = vec![resolution(if original {
            &original_dep
        } else {
            &selected_dep
        })];
        assert!(matches!(
            finalize(&evidence).unwrap(),
            RestoreOutcome::Finalized { .. }
        ));
        assert!(!f.source.with_extension("fls.restore-guard").exists());
        assert!(original_bundle.path().join("image.fls").exists());
    }
}

#[test]
fn restore_status_rejects_held_lock_b_when_name_has_returned_to_a() {
    use std::os::unix::fs::PermissionsExt;
    struct Swap {
        path: std::path::PathBuf,
        parked: std::path::PathBuf,
        b: std::path::PathBuf,
        state: AtomicUsize,
    }
    impl RecoveryIo for Swap {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event == RecoveryEvent::RestoreOperationLock
                && self.state.load(Ordering::Acquire) == 0
            {
                fs::rename(&self.path, &self.parked)?;
                fs::write(&self.path, b"")?;
                fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
                self.state.store(1, Ordering::Release);
            }
            Ok(())
        }
    }
    struct SwapFs<'a> {
        f: &'a Fixture,
        io: Arc<Swap>,
        limits: OperationLimits,
    }
    impl RecoveryFs for SwapFs<'_> {
        fn root(&self) -> &std::path::Path {
            &self.f.backups
        }
        fn reconciliation_limits(&self) -> &OperationLimits {
            &self.limits
        }
        fn io(&self) -> Arc<dyn RecoveryIo> {
            // acquire() used its original Arc through held-file validation.
            // The next io() is load_journal's lease/guard validation. Restore A
            // so named-lock checks pass while the status lock still holds B.
            if self
                .io
                .state
                .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                fs::rename(&self.io.path, &self.io.b).unwrap();
                fs::rename(&self.io.parked, &self.io.path).unwrap();
            }
            self.io.clone()
        }
    }
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let RestoreOutcome::Installed { journal } =
        restore_offline(&request, &lease, &f.filesystem()).unwrap()
    else {
        panic!("not installed")
    };
    let path = journal.staging.join("journal.bin");
    let before = fs::read(&path).unwrap();
    let guard = f.source.with_extension("fls.restore-guard");
    let guard_before = fs::read(&guard).unwrap();
    let io = Arc::new(Swap {
        path: journal.staging.join("journal.bin.lock"),
        parked: journal.staging.join("parked-a.lock"),
        b: journal.staging.join("held-b.lock"),
        state: AtomicUsize::new(0),
    });
    let filesystem = SwapFs {
        f: &f,
        io: io.clone(),
        limits: limits(),
    };
    assert!(restore_status(&path, &lease, &filesystem).is_err());
    assert_eq!(
        io.state.load(Ordering::Acquire),
        2,
        "replacement race did not execute"
    );
    let a = fs::metadata(&io.path).unwrap();
    let b = fs::metadata(&io.b).unwrap();
    assert_eq!((a.dev(), a.ino()), journal.operation_lock_identity);
    assert_ne!((a.dev(), a.ino()), (b.dev(), b.ino()));
    assert_eq!(fs::read(path).unwrap(), before);
    assert_eq!(
        fs::read(&guard).unwrap(),
        guard_before,
        "rejected operation must preserve guard bytes"
    );
}

// Scratch proposal only. These black-box tests require the fixed-streaming loader
// composed with terminal lease-ceiling runtime; none changes the protocol/caps.
fn terminal_negative_fixture(mode: TerminalFixtureMode) -> (Fixture, RestoreJournal, OfflineLease) {
    let (f, j, lease) = terminal_fixture(mode);
    let cut = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreGuardRemove));
    assert!(
        terminal_call(
            mode,
            &j,
            &lease,
            &CutFs {
                f: &f,
                io: cut.clone(),
                limits: limits()
            },
            false
        )
        .is_err()
    );
    assert_eq!(cut.fired.load(Ordering::Acquire), 1);
    let terminal =
        load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
    let expected = match mode {
        TerminalFixtureMode::Selected => RestorePhase::Finalized,
        TerminalFixtureMode::Original => RestorePhase::OriginalFinalized,
        TerminalFixtureMode::Absence => RestorePhase::OriginalAbsenceFinalized,
        TerminalFixtureMode::Abandon => RestorePhase::Abandoned,
        TerminalFixtureMode::AbandonAbsence => panic!("not in this matrix"),
    };
    assert_eq!(terminal.phase, expected);
    (f, terminal, lease)
}
fn terminal_negative_pin(f: &Fixture, j: &RestoreJournal) -> std::path::PathBuf {
    f.backups
        .join(".catalog/restore-pins")
        .join(format!("{}.json", j.operation_id))
}
fn terminal_negative_metadata(f: &Fixture, j: &RestoreJournal) -> [Vec<u8>; 3] {
    [
        fs::read(j.staging.join("journal.bin")).unwrap(),
        fs::read(j.destination.with_extension("fls.restore-guard")).unwrap(),
        fs::read(terminal_negative_pin(f, j)).unwrap(),
    ]
}
fn terminal_negative_counters(j: &RestoreJournal) -> (u32, u32, u32) {
    (
        j.current_attempt,
        j.resume_finalize_attempts,
        j.rollback_attempts,
    )
}
fn terminal_negative_digest<T: serde::Serialize>(value: &T) -> [u8; 32] {
    sha2::Sha256::digest(serde_json::to_vec(value).unwrap()).into()
}
// Preserve every original prefix frame byte-for-byte. Rebuild only the last
// payload with the real RJR1 chain helper and leave the fixed allocation intact.
fn terminal_negative_reframe_last(j: &RestoreJournal, changed: &RestoreJournal) -> Vec<u8> {
    let path = j.staging.join("journal.bin");
    let before = fs::read(&path).unwrap();
    let ranges = journal_frame_ranges(&before);
    let frame = ranges.last().unwrap();
    assert!(
        ranges.len() > 1,
        "must compare against immutable preceding records"
    );
    let previous: [u8; 32] = before[frame.start + 8..frame.start + 40]
        .try_into()
        .unwrap();
    let payload = serde_json::to_vec(changed).unwrap();
    assert!(payload.len() as u64 <= j.journal_capacity / 96 - 72);
    let replacement = journal_digest_frame(&payload, previous);
    assert!(frame.start + replacement.len() <= before.len());
    let mut after = before.clone();
    after[frame.start..].fill(0);
    after[frame.start..frame.start + replacement.len()].copy_from_slice(&replacement);
    assert_eq!(&after[..frame.start], &before[..frame.start]);
    // Independently validate EVERY header and checksum, including the changed
    // last frame. A bad frame checksum cannot satisfy the refusal assertion.
    let mut previous = [0; 32];
    for range in journal_frame_ranges(&after) {
        assert_eq!(&after[range.start + 8..range.start + 40], &previous);
        let mut hash = sha2::Sha256::new();
        hash.update(previous);
        hash.update(&after[range.start + 72..range.end]);
        previous = hash.finalize().into();
        assert_eq!(&after[range.start + 40..range.start + 72], &previous);
        let _: RestoreJournal =
            serde_json::from_slice(&after[range.start + 72..range.end]).unwrap();
    }
    journal_fixture_write(&path, &after);
    after
}
fn terminal_negative_semantic_refusal(
    mode: TerminalFixtureMode,
    f: &Fixture,
    j: &RestoreJournal,
    lease: &OfflineLease,
    changed: &RestoreJournal,
) {
    use fl_store::recovery::RecoveryError;
    let original = terminal_negative_metadata(f, j);
    let mutated = terminal_negative_reframe_last(j, changed);
    let path = j.staging.join("journal.bin");
    assert!(
        matches!(
            load_restore_journal(&path, lease, &f.filesystem()),
            Err(RecoveryError::Verification { .. })
        ),
        "checksummed semantic mutation must refuse during public load: {mode:?}"
    );
    assert!(matches!(
        restore_status(&path, lease, &f.filesystem()),
        Err(RecoveryError::Verification { .. })
    ));
    assert!(matches!(
        terminal_call(mode, j, lease, &f.filesystem(), true),
        Err(RecoveryError::Verification { .. })
    ));
    let after = terminal_negative_metadata(f, j);
    assert_eq!(after[0], mutated, "refusal repaired, charged or appended");
    assert_eq!(after[1], original[1]);
    assert_eq!(after[2], original[2]);
    journal_fixture_write(&path, &original[0]);
    assert_eq!(
        terminal_negative_counters(&load_restore_journal(&path, lease, &f.filesystem()).unwrap()),
        terminal_negative_counters(j)
    );
}
#[test]
fn restore_terminal_original_report_and_profile_tampering_refuses_before_cleanup() {
    // Genuine fixture backup publications and admission-generated reports only.
    // The altered terminal record must disagree with untouched admission records.
    for mode in [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::Abandon,
    ] {
        let (f, j, lease) = terminal_negative_fixture(mode);
        assert_eq!(
            j.original_verification.is_some(),
            !matches!(mode, TerminalFixtureMode::Absence)
        );
        for mutation in 0..3 {
            if matches!(mode, TerminalFixtureMode::Absence) && mutation != 1 {
                continue;
            }
            let mut changed = j.clone();
            match mutation {
                0 => {
                    changed
                        .original_verification
                        .as_mut()
                        .unwrap()
                        .profile_sha256 = "0".repeat(64)
                }
                1 => changed.original_profile.version += 1,
                2 => changed.original_verification = None,
                _ => unreachable!(),
            }
            terminal_negative_semantic_refusal(mode, &f, &j, &lease, &changed);
        }
    }
}
#[test]
fn restore_terminal_missing_or_mismatched_acceptance_context_refuses_before_cleanup() {
    for mode in [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::Abandon,
    ] {
        let (f, j, lease) = terminal_negative_fixture(mode);
        for mutation in 0..7 {
            let mut changed = j.clone();
            match mutation {
                0 => changed.terminal_evidence_context = None, // includes missing OriginalFinalized context
                1 => {
                    changed
                        .terminal_evidence_context
                        .as_mut()
                        .unwrap()
                        .admission_generation_sha256[0] ^= 1
                }
                2 => {
                    changed
                        .terminal_evidence_context
                        .as_mut()
                        .unwrap()
                        .accepting_lease_sha256[0] ^= 1
                }
                3 => {
                    let c = changed.terminal_evidence_context.as_mut().unwrap();
                    c.accepted_at_unix_ms = c.accepting_lease.expires_at_unix_ms;
                }
                4 => {
                    let c = changed.terminal_evidence_context.as_mut().unwrap();
                    c.mode = if c.mode == TerminalMode::Selected {
                        TerminalMode::Original
                    } else {
                        TerminalMode::Selected
                    };
                }
                5 => {
                    let c = changed.terminal_evidence_context.as_mut().unwrap();
                    c.prepared_report_sha256 =
                        Some(c.prepared_report_sha256.map_or([1; 32], |mut hash| {
                            hash[0] ^= 1;
                            hash
                        }));
                }
                6 => {
                    let c = changed.terminal_evidence_context.as_mut().unwrap();
                    c.reconciliation_sha256 =
                        Some(c.reconciliation_sha256.map_or([1; 32], |mut hash| {
                            hash[0] ^= 1;
                            hash
                        }));
                }
                _ => unreachable!(),
            }
            terminal_negative_semantic_refusal(mode, &f, &j, &lease, &changed);
        }
    }
}
#[test]
fn restore_terminal_accepted_evidence_tampering_refuses_before_cleanup() {
    for mode in [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
    ] {
        let (f, j, lease) = terminal_negative_fixture(mode);
        let mut changed = j.clone();
        if let Some(record) = changed.reconciliation.as_mut() {
            record.clients.clients_invalidated = false;
        } else {
            changed
                .absence_reconciliation
                .as_mut()
                .unwrap()
                .no_client_opened_path_while_installed = false;
        }
        terminal_negative_semantic_refusal(mode, &f, &j, &lease, &changed);
    }
}
#[test]
fn restore_terminal_self_consistent_replacement_context_cannot_replace_prior_acceptance() {
    // Only these two modes have an accepted-context intent immediately before
    // the terminal record. No claim of an external cryptographic anchor or of
    // detecting an attacker rewriting the ENTIRE journal is made here.
    for mode in [TerminalFixtureMode::Selected, TerminalFixtureMode::Original] {
        let (f, j, lease) = terminal_negative_fixture(mode);
        let mut changed = j.clone();
        let c = changed.terminal_evidence_context.as_mut().unwrap();
        c.accepting_lease.operator = "another otherwise valid accepting operator".into();
        c.accepting_lease.expected_file = inspect_file_binding(&j.destination, &limits()).unwrap();
        c.accepting_lease_sha256 = terminal_negative_digest(&c.accepting_lease);
        changed.reconciliation.as_mut().unwrap().operator = c.accepting_lease.operator.clone();
        c.reconciliation_sha256 = Some(terminal_negative_digest(
            changed.reconciliation.as_ref().unwrap(),
        ));
        terminal_negative_semantic_refusal(mode, &f, &j, &lease, &changed);
    }
}
struct TerminalNegativeClockTrace {
    now: u128,
    events: Mutex<Vec<RecoveryEvent>>,
}
impl RecoveryIo for TerminalNegativeClockTrace {
    fn unix_ms(&self) -> std::io::Result<u128> {
        Ok(self.now)
    }
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}
#[test]
fn restore_terminal_wrong_expired_and_persisted_mismatched_renewed_b_are_free_refusals() {
    for mode in [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::Abandon,
    ] {
        let (f, j, lease) = terminal_negative_fixture(mode);
        let before = terminal_negative_metadata(&f, &j);
        let counters = terminal_negative_counters(&j);
        let accepting_request = fixture_lease_request(&lease);
        let now = accepting_request.expires_at_unix_ms + 1;
        let clock = ClockFs {
            native: f.filesystem(),
            now,
        };
        let renewed = renewed_lease(&j, &clock);
        let current_bytes = fs::read(renewed.path()).unwrap();
        let renewed_request: OfflineLeaseRequest = serde_json::from_slice(&current_bytes).unwrap();
        for case in [
            "expired",
            "wrong_operation",
            "wrong_action",
            "wrong_profile",
            "wrong_catalog",
            "wrong_current_file",
            "persisted_mismatch",
        ] {
            let mut request = renewed_request.clone();
            match case {
                "wrong_operation" => {
                    request.operation_id = uuid::Uuid::from_u128(request.operation_id.as_u128() ^ 1)
                }
                "wrong_action" => {
                    let OfflineAction::Restore {
                        bundle_operation, ..
                    } = &mut request.action
                    else {
                        panic!("not restore")
                    };
                    *bundle_operation = uuid::Uuid::from_u128(bundle_operation.as_u128() ^ 1);
                }
                "wrong_profile" => request.profile_sha256 = "0".repeat(64),
                "wrong_catalog" => request.catalog_identity.push_str("-wrong"),
                "wrong_current_file" => {
                    request.expected_file = if request.expected_file == FileBinding::Absent {
                        j.binding.expected_file.clone() // absence's admission is absent too: use a real other file below
                    } else {
                        FileBinding::Absent
                    }
                }
                _ => {}
            }
            if case == "wrong_current_file"
                && request.expected_file == renewed_request.expected_file
            {
                request.expected_file = inspect_file_binding(&f.source, &limits()).unwrap();
                assert_ne!(request.expected_file, renewed_request.expected_file);
            }
            // Each candidate has its own private persisted attestation. Reusing
            // create_offline_lease here collides with renewed B's same-operation,
            // same-issuance O_EXCL publication before reaching restore.
            // Public load checks the real request/attestation constraints while
            // leaving the wrong current-generation binding for restore to reject.
            let candidate_path = renewed.path().parent().unwrap().join(format!(
                "terminal-negative-{}-{case}.json",
                request.operation_id
            ));
            {
                use std::os::unix::fs::OpenOptionsExt;
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&candidate_path)
                    .unwrap();
                serde_json::to_writer(&file, &request).unwrap();
            }
            let candidate = OfflineLease::load(&candidate_path, &clock, &limits()).unwrap();
            // Capture the typed object-equivalent snapshot BEFORE changing its
            // persisted file; rereading after mutation would erase the mismatch.
            let candidate_request = fixture_lease_request(&candidate);
            assert_eq!(candidate_request, request);
            if case == "persisted_mismatch" {
                let mut mismatch = candidate_request.clone();
                mismatch.operator.push_str(" persisted replacement");
                fs::write(candidate.path(), serde_json::to_vec(&mismatch).unwrap()).unwrap();
            }
            let hook = Arc::new(TerminalNegativeClockTrace {
                now: if case == "expired" {
                    candidate_request.expires_at_unix_ms
                } else {
                    now
                },
                events: Mutex::new(vec![]),
            });
            let filesystem = CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits(),
            };
            assert!(
                terminal_call(mode, &j, &candidate, &filesystem, true).is_err(),
                "{mode:?}/{case}"
            );
            let events = hook.events.lock().unwrap();
            assert!(
                !events.contains(&RecoveryEvent::RestoreGuardRemove),
                "guard authorization reached: {mode:?}/{case}"
            );
            assert!(
                !events.contains(&RecoveryEvent::RestorePinRemove),
                "pin authorization reached: {mode:?}/{case}"
            );
            assert!(!events.contains(&RecoveryEvent::Write(Member::RestoreJournal)));
            assert_eq!(
                terminal_negative_metadata(&f, &j),
                before,
                "charged/appended/withdrew at {mode:?}/{case}"
            );
            fs::write(renewed.path(), &current_bytes).unwrap();
            assert_eq!(
                terminal_negative_counters(
                    &load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem())
                        .unwrap()
                ),
                counters
            );
        }
    }
}
// Public I/O trace locates the descriptor open immediately preceding the exact
// guard/pin read used by its removal callback. No assumed global open count.
struct TerminalAuthorizationOpenTrace {
    target: RecoveryEvent,
    target_identity: (u64, u64),
    events: Mutex<Vec<RecoveryEvent>>,
    last_target_open: Mutex<Option<usize>>,
    authorization_open: Mutex<Option<usize>>,
}
impl RecoveryIo for TerminalAuthorizationOpenTrace {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        let mut events = self.events.lock().unwrap();
        events.push(event);
        if event == self.target {
            *self.authorization_open.lock().unwrap() = *self.last_target_open.lock().unwrap();
        }
        Ok(())
    }
    fn read_exact_at(&self, file: &fs::File, bytes: &mut [u8], offset: u64) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        let m = file.metadata()?;
        if (m.dev(), m.ino()) == self.target_identity {
            let events = self.events.lock().unwrap();
            *self.last_target_open.lock().unwrap() =
                events.iter().rposition(|e| *e == RecoveryEvent::OpenFile);
        }
        file.read_exact_at(bytes, offset)
    }
}
struct TerminalReplaceBeforeAuthorizationOpen {
    prefix: Vec<RecoveryEvent>,
    cursor: AtomicUsize,
    path: std::path::PathBuf,
    replacement: Vec<u8>,
    old_identity: (u64, u64),
    new_identity: Mutex<Option<(u64, u64)>>,
}
impl RecoveryIo for TerminalReplaceBeforeAuthorizationOpen {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        let at = self.cursor.fetch_add(1, Ordering::AcqRel);
        if at < self.prefix.len() {
            assert_eq!(
                event, self.prefix[at],
                "independent fixture authorization prefix diverged at {at}"
            );
        }
        if at + 1 == self.prefix.len() {
            assert_eq!(event, RecoveryEvent::OpenFile);
            let m = fs::metadata(&self.path)?;
            assert_eq!((m.dev(), m.ino()), self.old_identity);
            let replacement = self.path.with_extension("before-authorization-open");
            fs::write(&replacement, &self.replacement)?;
            fs::set_permissions(
                &replacement,
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            )?;
            let m = fs::metadata(&replacement)?;
            assert_ne!((m.dev(), m.ino()), self.old_identity);
            *self.new_identity.lock().unwrap() = Some((m.dev(), m.ino()));
            fs::rename(replacement, &self.path)?;
        }
        Ok(())
    }
}
#[test]
fn restore_terminal_guard_and_pin_descriptor_replacement_before_authorization_open_is_retained() {
    for mode in [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::Abandon,
    ] {
        for target in [
            RecoveryEvent::RestoreGuardRemove,
            RecoveryEvent::RestorePinRemove,
        ] {
            let (reference, rj, rl) = terminal_negative_fixture(mode);
            let rp = if target == RecoveryEvent::RestoreGuardRemove {
                rj.destination.with_extension("fls.restore-guard")
            } else {
                terminal_negative_pin(&reference, &rj)
            };
            let m = fs::metadata(rp).unwrap();
            let trace = Arc::new(TerminalAuthorizationOpenTrace {
                target,
                target_identity: (m.dev(), m.ino()),
                events: Mutex::new(vec![]),
                last_target_open: Mutex::new(None),
                authorization_open: Mutex::new(None),
            });
            let done = terminal_call(
                mode,
                &rj,
                &rl,
                &CutFs {
                    f: &reference,
                    io: trace.clone(),
                    limits: limits(),
                },
                true,
            )
            .unwrap();
            assert!(!matches!(done, RestoreOutcome::OutcomeUnknown { .. }));
            let at = trace
                .authorization_open
                .lock()
                .unwrap()
                .expect("target callback must read its exact held descriptor");
            let prefix = trace.events.lock().unwrap()[..=at].to_vec();
            assert_eq!(prefix.last(), Some(&RecoveryEvent::OpenFile));
            assert!(
                !prefix.contains(&target),
                "must replace before callback authorization event"
            );

            let (f, j, lease) = terminal_negative_fixture(mode);
            let before = terminal_negative_metadata(&f, &j);
            let path = if target == RecoveryEvent::RestoreGuardRemove {
                j.destination.with_extension("fls.restore-guard")
            } else {
                terminal_negative_pin(&f, &j)
            };
            let replacement = fs::read(&path).unwrap(); // valid JSON, identical bytes, different inode
            let m = fs::metadata(&path).unwrap();
            let hook = Arc::new(TerminalReplaceBeforeAuthorizationOpen {
                prefix,
                cursor: AtomicUsize::new(0),
                path: path.clone(),
                replacement: replacement.clone(),
                old_identity: (m.dev(), m.ino()),
                new_identity: Mutex::new(None),
            });
            let result = terminal_call(
                mode,
                &j,
                &lease,
                &CutFs {
                    f: &f,
                    io: hook.clone(),
                    limits: limits(),
                },
                true,
            );
            assert!(hook.cursor.load(Ordering::Acquire) >= hook.prefix.len());
            let replaced = hook
                .new_identity
                .lock()
                .unwrap()
                .expect("replacement hook missed");
            let m = fs::metadata(&path).unwrap();
            assert_eq!((m.dev(), m.ino()), replaced);
            assert_eq!(fs::read(&path).unwrap(), replacement);
            if target == RecoveryEvent::RestoreGuardRemove {
                assert!(result.is_err());
                assert_eq!(terminal_negative_metadata(&f, &j), before);
            } else {
                assert!(matches!(
                    result.unwrap(),
                    RestoreOutcome::OutcomeUnknown { .. }
                ));
                assert!(!j.destination.with_extension("fls.restore-guard").exists());
                assert_eq!(fs::read(terminal_negative_pin(&f, &j)).unwrap(), before[2]);
                assert_eq!(fs::read(j.staging.join("journal.bin")).unwrap(), before[0]);
            }
            let loaded =
                load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem())
                    .unwrap();
            assert_eq!(
                terminal_negative_counters(&loaded),
                terminal_negative_counters(&j)
            );
        }
    }
}

fn assert_terminal_resume_refused_unchanged(f: &Fixture, j: &RestoreJournal, lease: &OfflineLease) {
    let before = terminal_negative_metadata(f, j);
    let counters = terminal_negative_counters(j);
    let trace = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreGuardRemove));
    let filesystem = CutFs {
        f,
        io: trace.clone(),
        limits: limits(),
    };
    assert!(
        resume_restore(j, lease, &filesystem).is_err(),
        "terminal resume must refuse before charging or appending"
    );
    assert_eq!(terminal_negative_metadata(f, j), before);
    let loaded =
        load_restore_journal(&j.staging.join("journal.bin"), lease, &f.filesystem()).unwrap();
    assert_eq!(loaded.phase, j.phase);
    assert_eq!(terminal_negative_counters(&loaded), counters);
    assert!(!trace.events.lock().unwrap().iter().any(|event| matches!(
        event,
        RecoveryEvent::Write(Member::RestoreJournal)
            | RecoveryEvent::Flush(Member::RestoreJournal)
            | RecoveryEvent::RestoreInstall
            | RecoveryEvent::RestoreReverseExchange
            | RecoveryEvent::RestoreGuardRemove
            | RecoveryEvent::RestorePinRemove
    )));
}
#[test]
fn restore_resume_refuses_genuine_finalize_intents_without_mutation_and_typed_finalize_retries() {
    for mode in [TerminalFixtureMode::Selected, TerminalFixtureMode::Original] {
        let (f, j, lease) = terminal_fixture(mode);
        let cut = Arc::new(TerminalCutIo::new(RecoveryEvent::Sync(
            Directory::RestoreDestinationParent,
        )));
        assert!(
            terminal_call(
                mode,
                &j,
                &lease,
                &CutFs {
                    f: &f,
                    io: cut.clone(),
                    limits: limits()
                },
                false
            )
            .is_err()
        );
        assert_eq!(cut.fired.load(Ordering::Acquire), 1);
        let intent =
            load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
        assert_eq!(
            intent.phase,
            if matches!(mode, TerminalFixtureMode::Selected) {
                RestorePhase::FinalizeIntent
            } else {
                RestorePhase::FinalizeOriginalIntent
            }
        );
        assert_terminal_resume_refused_unchanged(&f, &intent, &lease);
        let accepted = intent.reconciliation.as_ref().unwrap();
        let outcome = if matches!(mode, TerminalFixtureMode::Selected) {
            fl_store::recovery::readiness::finalize_restore(
                &intent,
                &lease,
                accepted,
                &f.filesystem(),
            )
        } else {
            fl_store::recovery::readiness::finalize_rollback(
                &intent,
                &lease,
                accepted,
                &f.filesystem(),
            )
        };
        assert!(matches!(outcome.unwrap(), RestoreOutcome::Finalized { .. }));
        assert!(
            !intent
                .destination
                .with_extension("fls.restore-guard")
                .exists()
        );
        assert!(!terminal_negative_pin(&f, &intent).exists());
    }
}
#[test]
fn restore_resume_refuses_durable_terminals_without_mutation_and_typed_cleanup_retries() {
    for mode in [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::Abandon,
        TerminalFixtureMode::AbandonAbsence,
    ] {
        let (f, j, lease) = terminal_fixture(mode);
        let cut = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreGuardRemove));
        assert!(
            terminal_call(
                mode,
                &j,
                &lease,
                &CutFs {
                    f: &f,
                    io: cut.clone(),
                    limits: limits()
                },
                false
            )
            .is_err()
        );
        assert_eq!(cut.fired.load(Ordering::Acquire), 1);
        let terminal =
            load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
        let expected = match mode {
            TerminalFixtureMode::Selected => RestorePhase::Finalized,
            TerminalFixtureMode::Original => RestorePhase::OriginalFinalized,
            TerminalFixtureMode::Absence => RestorePhase::OriginalAbsenceFinalized,
            TerminalFixtureMode::Abandon | TerminalFixtureMode::AbandonAbsence => {
                RestorePhase::Abandoned
            }
        };
        assert_eq!(terminal.phase, expected);
        assert_terminal_resume_refused_unchanged(&f, &terminal, &lease);
        terminal_call(mode, &terminal, &lease, &f.filesystem(), true).unwrap();
        assert!(
            !terminal
                .destination
                .with_extension("fls.restore-guard")
                .exists()
        );
        assert!(!terminal_negative_pin(&f, &terminal).exists());
    }
}

fn assert_unverified_candidate_status_read_only(
    f: &Fixture,
    j: &RestoreJournal,
    lease: &OfflineLease,
) {
    let before = terminal_negative_metadata(f, j);
    let source = fs::read(&f.source).unwrap();
    let candidate_path = j.staging.join("candidate.fls");
    let candidate = fs::read(&candidate_path).unwrap();
    let status = restore_status(&j.staging.join("journal.bin"), lease, &f.filesystem()).unwrap();
    let artifact = status
        .artifacts
        .iter()
        .find(|a| a.path == candidate_path)
        .unwrap();
    assert_eq!(artifact.identity, j.candidate_identity);
    assert_eq!(artifact.bytes, candidate.len() as u64);
    assert_eq!(
        artifact.sha256, None,
        "unverified bytes must not claim the selected image digest"
    );
    assert_eq!(artifact.classification, "selected_unverified_candidate");
    assert_eq!(
        status.total_retained_bytes,
        status
            .artifacts
            .iter()
            .map(|a| fs::metadata(&a.path).unwrap().len())
            .sum::<u64>()
    );
    assert_eq!(terminal_negative_metadata(f, j), before);
    assert_eq!(fs::read(&f.source).unwrap(), source);
    assert_eq!(fs::read(candidate_path).unwrap(), candidate);
}
#[test]
fn restore_status_reports_genuine_partial_copy_as_unverified_before_and_after_abandon() {
    let (f, j, lease) = terminal_fixture(TerminalFixtureMode::Abandon);
    assert_unverified_candidate_status_read_only(&f, &j, &lease);
    let cut = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreGuardRemove));
    assert!(
        abandon_restore(
            &j,
            &lease,
            &CutFs {
                f: &f,
                io: cut.clone(),
                limits: limits()
            }
        )
        .is_err()
    );
    assert_eq!(cut.fired.load(Ordering::Acquire), 1);
    let abandoned =
        load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
    assert_eq!(abandoned.phase, RestorePhase::Abandoned);
    assert_unverified_candidate_status_read_only(&f, &abandoned, &lease);
}
#[test]
fn restore_status_reports_actual_empty_candidate_after_guard_durable_allocation_failure() {
    struct FailCandidateAllocation {
        events: Mutex<Vec<RecoveryEvent>>,
        fired: AtomicUsize,
    }
    impl RecoveryIo for FailCandidateAllocation {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            let mut events = self.events.lock().unwrap();
            let guard_durable =
                events.contains(&RecoveryEvent::Sync(Directory::RestoreGuardParent));
            events.push(event);
            if guard_durable && event == RecoveryEvent::Allocate {
                self.fired.fetch_add(1, Ordering::AcqRel);
                return Err(std::io::Error::from_raw_os_error(libc::ENOSPC));
            }
            Ok(())
        }
    }
    let f = journal_fixture();
    let (request, lease) = prepare_restore(&f, &f.source);
    let hook = Arc::new(FailCandidateAllocation {
        events: Mutex::new(vec![]),
        fired: AtomicUsize::new(0),
    });
    assert!(
        restore_offline(
            &request,
            &lease,
            &CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits()
            }
        )
        .is_err()
    );
    assert_eq!(hook.fired.load(Ordering::Acquire), 1);
    let events = hook.events.lock().unwrap();
    let guard = events
        .iter()
        .position(|e| *e == RecoveryEvent::Sync(Directory::RestoreGuardParent))
        .unwrap();
    let allocation = events
        .iter()
        .enumerate()
        .find(|(i, e)| *i > guard && **e == RecoveryEvent::Allocate)
        .unwrap()
        .0;
    assert!(guard < allocation);
    assert!(!events[guard + 1..].contains(&RecoveryEvent::Write(Member::VerificationCopy)));
    drop(events);
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(f.source.with_extension("fls.restore-guard")).unwrap())
            .unwrap();
    let path = std::path::Path::new(value["journal"].as_str().unwrap());
    let j = load_restore_journal(path, &lease, &f.filesystem()).unwrap();
    assert_eq!(j.phase, RestorePhase::Admitted);
    assert!(j.verification.is_none());
    assert!(j.candidate_bytes > 0);
    assert_eq!(
        fs::metadata(j.staging.join("candidate.fls")).unwrap().len(),
        0
    );
    assert_unverified_candidate_status_read_only(&f, &j, &lease);
    let cut = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreGuardRemove));
    assert!(
        abandon_restore(
            &j,
            &lease,
            &CutFs {
                f: &f,
                io: cut.clone(),
                limits: limits()
            }
        )
        .is_err()
    );
    assert_eq!(cut.fired.load(Ordering::Acquire), 1);
    let abandoned = load_restore_journal(path, &lease, &f.filesystem()).unwrap();
    assert_eq!(abandoned.phase, RestorePhase::Abandoned);
    assert_unverified_candidate_status_read_only(&f, &abandoned, &lease);
}

#[test]
fn restore_guard_publication_every_cut_keeps_named_guard_loadable_or_temp_retained() {
    let trace = Fixture::new();
    let (request, lease) = prepare_restore(&trace, &trace.source);
    let hook = Arc::new(CutIo::new(usize::MAX, libc::EIO));
    let filesystem = CutFs {
        f: &trace,
        io: hook.clone(),
        limits: limits(),
    };
    assert!(matches!(
        restore_offline(&request, &lease, &filesystem).unwrap(),
        RestoreOutcome::Installed { .. }
    ));
    let events = hook.events.lock().unwrap().clone();
    let first = events
        .iter()
        .position(|e| *e == RecoveryEvent::Create(Member::RestoreGuard))
        .unwrap();
    let parent_sync = events
        .iter()
        .position(|e| *e == RecoveryEvent::Sync(Directory::RestoreGuardParent))
        .unwrap();
    assert_eq!(
        events.get(parent_sync + 1),
        Some(&RecoveryEvent::Sync(Directory::RestoreStagingParent)),
        "published guard requires the removed temporary name's staging sync"
    );
    let last = parent_sync + 1;
    for (cut, observed_event) in events.iter().enumerate().take(last + 1).skip(first) {
        let f = Fixture::new();
        let (request, lease) = prepare_restore(&f, &f.source);
        let original = fs::read(&f.source).unwrap();
        let original_id = {
            let m = fs::metadata(&f.source).unwrap();
            (m.dev(), m.ino())
        };
        let hook = Arc::new(CutIo::new(cut, libc::EIO));
        let filesystem = CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        let error = restore_offline(&request, &lease, &filesystem).unwrap_err();
        let fenced = f.source.with_extension("fls.restore-guard").exists();
        assert!(
            error.to_string().contains(if fenced {
                "store is fenced"
            } else {
                "store is unfenced"
            }),
            "{error}"
        );
        let fl_store::recovery::RecoveryError::RestoreAdmissionArtifactsRetained {
            staging,
            staging_identity: Some(staging_identity),
            operation_lock_identity: Some(lock_identity),
            source,
            ..
        } = error
        else {
            panic!("publication cut must retain admission artifacts: {error:?}")
        };
        assert!(hook.count.load(Ordering::Acquire) > cut);
        if cut == first {
            assert!(
                matches!(
                    &*source,
                    fl_store::recovery::RecoveryError::Destination { .. }
                ),
                "pre-guard failure lost its typed source: {source:?}"
            );
        }
        let metadata = fs::metadata(&staging).unwrap();
        assert_eq!((metadata.dev(), metadata.ino()), staging_identity);
        let metadata = fs::metadata(staging.join("journal.bin.lock")).unwrap();
        assert_eq!((metadata.dev(), metadata.ino()), lock_identity);
        let guard = f.source.with_extension("fls.restore-guard");
        let temp = staging.join("restore-guard.tmp");
        let temp_before = if temp.exists() {
            let m = fs::metadata(&temp).unwrap();
            Some(((m.dev(), m.ino()), fs::read(&temp).unwrap()))
        } else {
            None
        };
        if guard.exists() {
            let value: serde_json::Value = serde_json::from_slice(&fs::read(&guard).unwrap())
                .expect("present guard must contain typed journal binding");
            assert_eq!(
                value["journal"].as_str().unwrap(),
                staging.join("journal.bin").to_str().unwrap()
            );
            assert!(
                !temp.exists(),
                "successful publication must remove the temporary name"
            );
            assert!(matches!(
                *source,
                fl_store::recovery::RecoveryError::OutcomeUnknown { .. }
            ));
            fresh_probe(&f.source);
        } else if cut > first {
            assert!(temp.exists(), "interrupted private guard must be retained");
        }
        let path = staging.join("journal.bin");
        let journal = load_restore_journal(&path, &lease, &f.filesystem())
            .expect("publication cut must preserve typed recovery");
        assert_eq!(journal.phase, RestorePhase::Admitted);
        let status = restore_status(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(
            status.artifacts.iter().filter(|a| a.path == temp).count(),
            usize::from(temp_before.is_some())
        );
        assert!(matches!(
            abandon_restore(&journal, &lease, &f.filesystem()).unwrap(),
            RestoreOutcome::Abandoned { .. }
        ));
        let status = restore_status(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(status.phase, RestorePhase::Abandoned);
        assert!(!status.guard_present);
        assert!(
            !f.backups
                .join(".catalog/restore-pins")
                .join(format!("{}.json", journal.operation_id))
                .exists()
        );
        if let Some((identity, bytes)) = temp_before {
            let m = fs::metadata(&temp).unwrap();
            assert_eq!((m.dev(), m.ino()), identity);
            assert_eq!(
                fs::read(&temp).unwrap(),
                bytes,
                "typed abandon must retain the unpublished guard bytes"
            );
            assert!(
                status
                    .artifacts
                    .iter()
                    .any(|a| a.path == temp && a.identity == identity)
            );
        }
        let m = fs::metadata(&f.source).unwrap();
        assert_eq!((m.dev(), m.ino()), original_id);
        assert_eq!(fs::read(&f.source).unwrap(), original);
        eprintln!(
            "guard_publication_cut index={cut} event={:?} typed_abandon=true temp_retained={}",
            observed_event,
            temp.exists()
        );
    }
}

struct GuardPublicationReplacement {
    named_guard: std::path::PathBuf,
    temp: bool,
    fired: AtomicUsize,
}
impl RecoveryIo for GuardPublicationReplacement {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        if event != RecoveryEvent::Publish {
            return Ok(());
        }
        let parent = self.named_guard.parent().unwrap();
        let temporary = fs::read_dir(parent)?
            .map(|e| e.map(|e| e.path().join("restore-guard.tmp")))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|p| p.exists());
        // Pin publication uses the same generic event; wait for the actual
        // guard temp so this race remains attached to guard publication.
        if let Some(temporary) = temporary
            && self.fired.fetch_add(1, Ordering::AcqRel) == 0
        {
            let path = if self.temp {
                temporary
            } else {
                self.named_guard.clone()
            };
            fs::write(path, b"foreign publication artifact")?;
        }
        Ok(())
    }
}

#[test]
fn restore_guard_publication_refuses_foreign_named_guard_and_changed_temp_without_overwrite() {
    for temp in [false, true] {
        let f = Fixture::new();
        let (request, lease) = prepare_restore(&f, &f.source);
        let before = fs::read(&f.source).unwrap();
        let named_guard = f.source.with_extension("fls.restore-guard");
        let hook = Arc::new(GuardPublicationReplacement {
            named_guard: named_guard.clone(),
            temp,
            fired: AtomicUsize::new(0),
        });
        let filesystem = CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        let error = restore_offline(&request, &lease, &filesystem).unwrap_err();
        assert!(
            error.to_string().contains(if temp {
                "store is unfenced"
            } else {
                "store is fenced"
            }),
            "{error}"
        );
        let fl_store::recovery::RecoveryError::RestoreAdmissionArtifactsRetained {
            staging, ..
        } = error
        else {
            panic!("publication race must retain artifacts: {error:?}")
        };
        assert_eq!(hook.fired.load(Ordering::Acquire), 1);
        let temporary = staging.join("restore-guard.tmp");
        assert!(temporary.exists());
        if temp {
            assert!(!named_guard.exists());
            assert_eq!(
                fs::read(&temporary).unwrap(),
                b"foreign publication artifact"
            );
            let journal =
                load_restore_journal(&staging.join("journal.bin"), &lease, &f.filesystem())
                    .unwrap();
            assert!(matches!(
                abandon_restore(&journal, &lease, &f.filesystem()).unwrap(),
                RestoreOutcome::Abandoned { .. }
            ));
            assert_eq!(
                fs::read(&temporary).unwrap(),
                b"foreign publication artifact"
            );
        } else {
            assert_eq!(
                fs::read(&named_guard).unwrap(),
                b"foreign publication artifact"
            );
            assert!(
                load_restore_journal(&staging.join("journal.bin"), &lease, &f.filesystem())
                    .is_err()
            );
            fresh_probe(&f.source);
            assert_eq!(
                fs::read(&named_guard).unwrap(),
                b"foreign publication artifact"
            );
        }
        assert_eq!(fs::read(&f.source).unwrap(), before);
    }
}

struct CorruptUnpublishedGuardWrite {
    armed: AtomicUsize,
    fired: AtomicUsize,
}
impl RecoveryIo for CorruptUnpublishedGuardWrite {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        if event == RecoveryEvent::Write(Member::RestoreGuard) {
            self.armed.store(1, Ordering::Release);
        }
        Ok(())
    }
    fn write_at(&self, file: &std::fs::File, bytes: &[u8], offset: u64) -> std::io::Result<usize> {
        use std::os::unix::fs::FileExt;
        if self.armed.swap(0, Ordering::AcqRel) == 1
            && self.fired.fetch_add(1, Ordering::AcqRel) == 0
        {
            let mut corrupted = bytes.to_vec();
            corrupted[0] ^= 0x20;
            file.write_at(&corrupted, offset)
        } else {
            file.write_at(bytes, offset)
        }
    }
}
#[test]
fn restore_guard_publication_checks_actual_temp_hash_before_publish() {
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let before = fs::read(&f.source).unwrap();
    let hook = Arc::new(CorruptUnpublishedGuardWrite {
        armed: AtomicUsize::new(0),
        fired: AtomicUsize::new(0),
    });
    let filesystem = CutFs {
        f: &f,
        io: hook.clone(),
        limits: limits(),
    };
    let error = restore_offline(&request, &lease, &filesystem).unwrap_err();
    let fl_store::recovery::RecoveryError::RestoreAdmissionArtifactsRetained { staging, .. } =
        error
    else {
        panic!("corrupt private guard must refuse before publication: {error:?}")
    };
    assert_eq!(hook.fired.load(Ordering::Acquire), 1);
    assert!(!f.source.with_extension("fls.restore-guard").exists());
    let temporary = staging.join("restore-guard.tmp");
    let retained = fs::read(&temporary).unwrap();
    let journal =
        load_restore_journal(&staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
    assert!(matches!(
        abandon_restore(&journal, &lease, &f.filesystem()).unwrap(),
        RestoreOutcome::Abandoned { .. }
    ));
    assert_eq!(fs::read(&temporary).unwrap(), retained);
    assert_eq!(fs::read(&f.source).unwrap(), before);
}

struct LateGuardPublicationRetained {
    path: std::path::PathBuf,
    identity: (u64, u64),
    bytes: Option<Vec<u8>>,
    guard_bytes: Vec<u8>,
}
struct LateGuardPublicationMutation {
    target: Directory,
    member: u8,
    replacement: bool,
    destination: std::path::PathBuf,
    lease: std::path::PathBuf,
    published: AtomicUsize,
    fired: AtomicUsize,
    retained: Mutex<Option<LateGuardPublicationRetained>>,
}
impl RecoveryIo for LateGuardPublicationMutation {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        if event == RecoveryEvent::Publish {
            let guard_temp_present = fs::read_dir(self.destination.parent().unwrap())?
                .map(|entry| entry.map(|entry| entry.path().join("restore-guard.tmp")))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .any(|path| path.exists());
            if guard_temp_present {
                self.published.store(1, Ordering::Release);
            }
        }
        if self.published.load(Ordering::Acquire) != 1
            || event != RecoveryEvent::Sync(self.target)
            || self
                .fired
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Ok(());
        }
        let named_guard = self.destination.with_extension("fls.restore-guard");
        let staging = || {
            fs::read_dir(self.destination.parent().unwrap())?
                .map(|e| e.map(|e| e.path()))
                .collect::<Result<Vec<_>, std::io::Error>>()?
                .into_iter()
                .find(|p| {
                    p.is_dir()
                        && p.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".restore-")
                })
                .ok_or_else(|| std::io::Error::other("missing operation staging"))
        };
        let path = match self.member {
            0 => named_guard.clone(),
            1 => self.lease.clone(),
            2 => staging()?,
            _ => staging()?.join("journal.bin.lock"),
        };
        let retained_bytes = if self.member == 2 {
            let mut old = path.as_os_str().to_os_string();
            old.push(".held-before-late-replacement");
            fs::rename(&path, std::path::PathBuf::from(old))?;
            fs::create_dir(&path)?;
            fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
            fs::write(path.join("foreign-marker"), b"foreign staging must survive")?;
            None
        } else {
            let original = fs::read(&path)?;
            if self.replacement {
                // Equal bytes on a different inode must not inherit held authority.
                let mut old = path.as_os_str().to_os_string();
                old.push(".held-before-late-replacement");
                fs::rename(&path, std::path::PathBuf::from(old))?;
                fs::write(&path, &original)?;
                fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
            } else if self.member == 1 {
                let mut value: serde_json::Value = serde_json::from_slice(&original)?;
                value["operator"] = serde_json::json!("foreign late authority");
                fs::write(&path, serde_json::to_vec(&value)?)?;
            } else {
                fs::write(&path, b"foreign late publication artifact")?;
            }
            Some(fs::read(&path)?)
        };
        let m = fs::metadata(&path)?;
        *self.retained.lock().unwrap() = Some(LateGuardPublicationRetained {
            path,
            identity: (m.dev(), m.ino()),
            bytes: retained_bytes,
            guard_bytes: fs::read(&named_guard)?,
        });
        Ok(())
    }
}
#[test]
fn restore_guard_publication_rechecks_authority_after_both_post_publish_syncs() {
    for target in [
        Directory::RestoreGuardParent,
        Directory::RestoreStagingParent,
    ] {
        for member in 0..4 {
            for replacement in [false, true] {
                if member == 2 && !replacement {
                    continue;
                }
                let f = Fixture::new();
                let (request, lease) = prepare_restore(&f, &f.source);
                let original = fs::read(&f.source).unwrap();
                let m = fs::metadata(&f.source).unwrap();
                let original_identity = (m.dev(), m.ino());
                let hook = Arc::new(LateGuardPublicationMutation {
                    target,
                    member,
                    replacement,
                    destination: f.source.clone(),
                    lease: lease.path().to_path_buf(),
                    published: AtomicUsize::new(0),
                    fired: AtomicUsize::new(0),
                    retained: Mutex::new(None),
                });
                let filesystem = CutFs {
                    f: &f,
                    io: hook.clone(),
                    limits: limits(),
                };
                let result = restore_offline(&request, &lease, &filesystem);
                assert_eq!(hook.fired.load(Ordering::Acquire), 1);
                assert!(
                    matches!(
                        result,
                        Err(
                            fl_store::recovery::RecoveryError::RestoreAdmissionArtifactsRetained { .. }
                        )
                    ),
                    "late publication mutation must refuse before candidate copy: {target:?}/{member}/{replacement}: {result:?}"
                );
                let retained = hook.retained.lock().unwrap();
                let LateGuardPublicationRetained {
                    path,
                    identity,
                    bytes,
                    guard_bytes,
                } = retained.as_ref().unwrap();
                let m = fs::metadata(path).unwrap();
                assert_eq!((m.dev(), m.ino()), *identity);
                if let Some(bytes) = bytes {
                    assert_eq!(&fs::read(path).unwrap(), bytes);
                } else {
                    assert_eq!(
                        fs::read(path.join("foreign-marker")).unwrap(),
                        b"foreign staging must survive"
                    );
                }
                assert_eq!(
                    &fs::read(f.source.with_extension("fls.restore-guard")).unwrap(),
                    guard_bytes
                );
                let m = fs::metadata(&f.source).unwrap();
                assert_eq!((m.dev(), m.ino()), original_identity);
                assert_eq!(fs::read(&f.source).unwrap(), original);
            }
        }
    }
}

#[test]
fn catalog_large_streamed_config_does_not_reserve_a_parsed_payload() {
    use fl_store::recovery::{
        CancelToken, backup::*, catalog::select_completed_bundle, fs::NativeRecoveryFs,
    };
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let owner = FerroStore::open(&f.source).unwrap();
    let config = f.root.path().join("streamed-config.toml");
    fs::write(&config, vec![b'x'; 8 << 20]).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let id = fs::metadata(&config).unwrap();
    let key = f.root.path().join("excluded-streamed-fixture.key");
    fs::write(&key, b"not a live key").unwrap();
    let key_id = fs::metadata(&key).unwrap();
    let mut request = f.backup_request();
    request.limits.memory_byte_limit = 4 << 30;
    request.metadata_byte_limit = 16 << 20;
    request.configs = vec![ApprovedConfig {
        kind: fl_store::recovery::manifest::MemberKind::ProtectedConfig,
        path: config,
        identity: (id.dev(), id.ino()),
        byte_limit: 8 << 20,
    }];
    request.excluded_vault_key = Some(ExcludedVaultKey {
        path: key,
        identity: (key_id.dev(), key_id.ino()),
    });
    let filesystem = NativeRecoveryFs::new(f.backups.clone(), request.limits.clone());
    let PublicationOutcome::Durable(bundle) =
        backup(&owner, &request, &filesystem, &CancelToken::new()).unwrap()
    else {
        panic!("backup not durable")
    };
    drop(owner);
    let manifest: BundleManifestV1 =
        serde_json::from_slice(&fs::read(bundle.path().join("manifest.json")).unwrap()).unwrap();
    let mut l = limits();
    l.memory_byte_limit = 128 << 20;
    let selected = select_completed_bundle(
        uuid::Uuid::parse_str(&manifest.operation_id).unwrap(),
        &filesystem,
        &l,
    )
    .expect("streamed config length must not become a JSON model reservation");
    assert_eq!(selected.path(), bundle.path());
    assert!(l.admitted_memory_bytes() >= 65536);
    assert!(
        l.admitted_memory_bytes() < 8 << 20,
        "small decoded metadata plus live fixed buffers must not scale with config bytes"
    );
}

fn forensic_admitted_fixture() -> (Fixture, RestoreJournal, OfflineLease, fs::File) {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let bundle = f.bundle();
    // Retain a fixture-only descriptor to inspect bytes without changing the
    // original's unreadable classification during the operation.
    let original = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&f.source)
        .unwrap();
    fs::set_permissions(&f.source, fs::Permissions::from_mode(0o000)).unwrap();
    let (request, lease) = prepare_restore_bundle(&f, &f.source, bundle);
    let hook = journal_boundary_io(Some(RecoveryEvent::Flush(Member::Image)));
    let filesystem = CutFs {
        f: &f,
        io: hook.clone(),
        limits: limits(),
    };
    assert!(restore_offline(&request, &lease, &filesystem).is_err());
    assert!(
        hook.events
            .lock()
            .unwrap()
            .contains(&RecoveryEvent::Flush(Member::Image)),
        "the candidate-copy flush cut must be reached"
    );
    let guard: serde_json::Value =
        serde_json::from_slice(&fs::read(f.source.with_extension("fls.restore-guard")).unwrap())
            .unwrap();
    let path = std::path::Path::new(guard["journal"].as_str().unwrap());
    let journal = load_restore_journal(path, &lease, &f.filesystem()).unwrap();
    assert_eq!(journal.phase, RestorePhase::Admitted);
    assert!(journal.verification.is_none());
    let m = original.metadata().unwrap();
    assert_eq!(
        journal.original,
        FileBinding::Forensic {
            identity: (m.dev(), m.ino()),
            bytes: m.len(),
            read_error: libc::EACCES,
        }
    );
    (f, journal, lease, original)
}

fn forensic_descriptor_digest(file: &fs::File) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let mut hash = sha2::Sha256::new();
    let mut bytes = [0; 65536];
    let mut offset = 0;
    loop {
        let count = file.read_at(&mut bytes, offset).unwrap();
        if count == 0 {
            return hash.finalize().to_vec();
        }
        hash.update(&bytes[..count]);
        offset += count as u64;
    }
}

#[test]
fn restore_forensic_admitted_abandon_preserves_original_and_retained_artifacts() {
    use std::os::unix::fs::PermissionsExt;
    let (f, journal, lease, original) = forensic_admitted_fixture();
    let original_bytes = forensic_descriptor_digest(&original);
    let metadata = original.metadata().unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0);
    let guard = f.source.with_extension("fls.restore-guard");
    let pin = f
        .backups
        .join(".catalog/restore-pins")
        .join(format!("{}.json", journal.operation_id));
    assert!(guard.exists());
    assert!(pin.exists());
    let retained: std::collections::BTreeMap<_, _> = fs::read_dir(&journal.staging)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let path = entry.path();
            let m = entry.metadata().unwrap();
            let digest = if entry.file_name() == "journal.bin" {
                None
            } else {
                Some(forensic_descriptor_digest(&fs::File::open(&path).unwrap()))
            };
            (entry.file_name(), ((m.dev(), m.ino()), m.len(), digest))
        })
        .collect();
    assert!(retained.contains_key(std::ffi::OsStr::new("candidate.fls")));
    assert!(retained.contains_key(std::ffi::OsStr::new("journal.bin.lock")));
    assert!(matches!(
        abandon_restore(&journal, &lease, &f.filesystem()).unwrap(),
        RestoreOutcome::Abandoned { .. }
    ));
    assert!(!guard.exists());
    assert!(!pin.exists());
    assert_eq!(
        inspect_file_binding(&f.source, &limits()).unwrap(),
        journal.original
    );
    assert_eq!(forensic_descriptor_digest(&original), original_bytes);
    let m = fs::metadata(&f.source).unwrap();
    assert_eq!((m.dev(), m.ino()), (metadata.dev(), metadata.ino()));
    assert_eq!(m.len(), metadata.len());
    assert_eq!(m.permissions().mode(), metadata.permissions().mode());
    assert_eq!(
        fs::read_dir(&journal.staging).unwrap().count(),
        retained.len()
    );
    for (name, (identity, len, digest)) in retained {
        let path = journal.staging.join(name);
        let m = fs::metadata(&path).unwrap();
        assert_eq!((m.dev(), m.ino()), identity);
        assert_eq!(m.len(), len);
        if let Some(digest) = digest {
            assert_eq!(
                forensic_descriptor_digest(&fs::File::open(path).unwrap()),
                digest
            );
        }
    }
    let path = journal.staging.join("journal.bin");
    let terminal = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(terminal.phase, RestorePhase::Abandoned);
    assert_eq!(terminal.original, journal.original);
    assert_eq!(terminal.rollback_attempts, journal.rollback_attempts + 1);
    let bytes = fs::read(&path).unwrap();
    assert!(matches!(
        abandon_restore(&terminal, &lease, &f.filesystem()).unwrap(),
        RestoreOutcome::Abandoned { .. }
    ));
    assert_eq!(
        fs::read(path).unwrap(),
        bytes,
        "terminal retry must not append"
    );
}

#[test]
fn restore_forensic_admitted_abandon_refuses_changed_original_binding_without_attempt() {
    use std::os::unix::fs::PermissionsExt;
    for change in ["identity", "size", "readability"] {
        let (f, journal, lease, original) = forensic_admitted_fixture();
        match change {
            "identity" => {
                let replacement = f.source.with_extension("replacement");
                let mut reader = original.try_clone().unwrap();
                let mut writer = fs::File::create(&replacement).unwrap();
                std::io::copy(&mut reader, &mut writer).unwrap();
                assert_eq!(
                    writer.metadata().unwrap().len(),
                    original.metadata().unwrap().len()
                );
                fs::set_permissions(&replacement, fs::Permissions::from_mode(0o000)).unwrap();
                fs::rename(replacement, &f.source).unwrap();
            }
            "size" => original
                .set_len(original.metadata().unwrap().len() + 1)
                .unwrap(),
            "readability" => {
                fs::set_permissions(&f.source, fs::Permissions::from_mode(0o600)).unwrap();
            }
            _ => unreachable!(),
        }
        let changed_binding = inspect_file_binding(&f.source, &limits()).unwrap();
        assert_ne!(changed_binding, journal.original);
        if change == "identity" {
            let FileBinding::Forensic {
                identity,
                bytes,
                read_error,
            } = changed_binding
            else {
                panic!("replacement must remain forensic")
            };
            assert_ne!(
                identity,
                (
                    original.metadata().unwrap().dev(),
                    original.metadata().unwrap().ino()
                )
            );
            assert_eq!(bytes, original.metadata().unwrap().len());
            assert_eq!(read_error, libc::EACCES);
        }
        let path = journal.staging.join("journal.bin");
        let guard = f.source.with_extension("fls.restore-guard");
        let pin = f
            .backups
            .join(".catalog/restore-pins")
            .join(format!("{}.json", journal.operation_id));
        let before = [
            fs::read(&path).unwrap(),
            fs::read(&guard).unwrap(),
            fs::read(&pin).unwrap(),
        ];
        let hook = journal_boundary_io(None);
        let filesystem = CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        assert!(
            abandon_restore(&journal, &lease, &filesystem).is_err(),
            "{change}"
        );
        assert_eq!(
            [
                fs::read(path).unwrap(),
                fs::read(guard).unwrap(),
                fs::read(pin).unwrap()
            ],
            before,
            "changed metadata for {change}"
        );
        let events = hook.events.lock().unwrap();
        assert!(
            !events.contains(&RecoveryEvent::Write(Member::RestoreJournal)),
            "charged attempt for {change}"
        );
        assert!(
            !events.contains(&RecoveryEvent::RestoreGuardRemove),
            "unfenced for {change}"
        );
        assert!(
            !events.contains(&RecoveryEvent::RestorePinRemove),
            "un-pinned for {change}"
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AbsenceResumeSnapshot {
    metadata: [Vec<u8>; 3],
    identities: Vec<(u64, u64)>,
    candidate_sha256: String,
}
fn absence_resume_snapshot(f: &Fixture, j: &RestoreJournal) -> AbsenceResumeSnapshot {
    let candidate = j.staging.join("candidate.fls");
    let paths = [
        j.staging.join("journal.bin"),
        j.destination.with_extension("fls.restore-guard"),
        f.backups
            .join(".catalog/restore-pins")
            .join(format!("{}.json", j.operation_id)),
        candidate.clone(),
    ];
    AbsenceResumeSnapshot {
        metadata: terminal_negative_metadata(f, j),
        identities: paths
            .iter()
            .map(|path| {
                let metadata = fs::metadata(path).unwrap();
                (metadata.dev(), metadata.ino())
            })
            .collect(),
        candidate_sha256: format!("{:x}", sha2::Sha256::digest(fs::read(candidate).unwrap())),
    }
}
#[derive(Default)]
struct AbsenceResumeIo {
    now: Option<u128>,
    events: Mutex<Vec<RecoveryEvent>>,
}
impl RecoveryIo for AbsenceResumeIo {
    fn unix_ms(&self) -> std::io::Result<u128> {
        match self.now {
            Some(now) => Ok(now),
            None => fl_store::recovery::fs::NativeRecoveryIo.unix_ms(),
        }
    }
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}
fn assert_absence_resume_read_only(io: &AbsenceResumeIo) {
    let events = io.events.lock().unwrap();
    assert!(
        events.iter().all(|event| matches!(
            event,
            RecoveryEvent::OpenFile
                | RecoveryEvent::LockFile
                | RecoveryEvent::RestoreOperationLock
                | RecoveryEvent::RestoreOperationLockWait
                | RecoveryEvent::CatalogLock
                | RecoveryEvent::CatalogLockWait
        )),
        "resume performed a mutation: {events:?}"
    );
}
#[test]
fn restore_absence_resume_keeps_completed_rollback_unchanged() {
    let (f, journal, lease) = terminal_fixture(TerminalFixtureMode::Absence);
    assert_eq!(journal.phase, RestorePhase::OriginalAbsenceRestored);
    assert!(!journal.destination.exists());
    let before = absence_resume_snapshot(&f, &journal);
    assert_eq!(before.candidate_sha256, journal.candidate_sha256);
    let io = Arc::new(AbsenceResumeIo::default());
    let filesystem = CutFs {
        f: &f,
        io: io.clone(),
        limits: limits(),
    };
    let outcome = resume_restore(&journal, &lease, &filesystem).unwrap();
    let RestoreOutcome::OriginalAbsenceRestored { journal: resumed } = outcome else {
        panic!("resume reinstalled a completed absent-original rollback: {outcome:?}")
    };
    assert_eq!(resumed.phase, RestorePhase::OriginalAbsenceRestored);
    assert_eq!(
        serde_json::to_value(&resumed).unwrap(),
        serde_json::to_value(&journal).unwrap()
    );
    assert!(!journal.destination.exists());
    assert_eq!(absence_resume_snapshot(&f, &journal), before);
    assert_absence_resume_read_only(&io);
}
#[test]
fn restore_absence_resume_refuses_expired_authority_without_mutation() {
    let (f, journal, lease) = terminal_fixture(TerminalFixtureMode::Absence);
    let before = absence_resume_snapshot(&f, &journal);
    let io = Arc::new(AbsenceResumeIo {
        now: Some(journal.binding.expires_at_unix_ms + 1),
        ..Default::default()
    });
    let filesystem = CutFs {
        f: &f,
        io: io.clone(),
        limits: limits(),
    };
    assert!(resume_restore(&journal, &lease, &filesystem).is_err());
    assert!(!journal.destination.exists());
    assert_eq!(absence_resume_snapshot(&f, &journal), before);
    assert_absence_resume_read_only(&io);
}
#[test]
fn restore_absence_resume_refuses_changed_layout_without_mutation() {
    let (f, journal, lease) = terminal_fixture(TerminalFixtureMode::Absence);
    let candidate = journal.staging.join("candidate.fls");
    let before = absence_resume_snapshot(&f, &journal);
    let parked = journal.staging.join("unexpected-candidate.fls");
    fs::rename(&candidate, &parked).unwrap();
    let metadata_before = terminal_negative_metadata(&f, &journal);
    let io = Arc::new(AbsenceResumeIo::default());
    let filesystem = CutFs {
        f: &f,
        io: io.clone(),
        limits: limits(),
    };
    assert!(resume_restore(&journal, &lease, &filesystem).is_err());
    assert!(!journal.destination.exists());
    assert!(!candidate.exists());
    assert_eq!(terminal_negative_metadata(&f, &journal), metadata_before);
    assert_absence_resume_read_only(&io);
    fs::rename(&parked, &candidate).unwrap();
    assert_eq!(absence_resume_snapshot(&f, &journal), before);
}

use std::sync::atomic::AtomicBool;

// Markers arm the clock, but only inode/digest snapshots prove a syscall did not run.
#[derive(Clone, Copy, Debug)]
enum RestoreExpiryCut {
    BeforeDigest,
    FinalRead,
}
type RestoreExpiryLayout = Vec<Option<((u64, u64), Vec<u8>)>>;
fn restore_expiry_layout(
    destination: &std::path::Path,
    staging: &std::path::Path,
) -> RestoreExpiryLayout {
    [
        destination.to_path_buf(),
        staging.join("candidate.fls"),
        staging.join("original.fls"),
    ]
    .iter()
    .map(|path| {
        fs::metadata(path).ok().map(|m| {
            (
                (m.dev(), m.ino()),
                sha2::Sha256::digest(fs::read(path).unwrap()).to_vec(),
            )
        })
    })
    .collect()
}
struct RestoreExpiryIo {
    marker: RecoveryEvent,
    cut: RestoreExpiryCut,
    destination: std::path::PathBuf,
    issued: u128,
    expires: u128,
    expired: AtomicBool,
    armed: AtomicBool,
    final_identity: Mutex<Option<(u64, u64)>>,
    before_layout: Mutex<Option<RestoreExpiryLayout>>,
    subsequent_actions: AtomicUsize,
}
impl RecoveryIo for RestoreExpiryIo {
    fn unix_ms(&self) -> std::io::Result<u128> {
        Ok(if self.expired.load(Ordering::Acquire) {
            self.expires
        } else {
            self.issued
        })
    }
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        if matches!(
            event,
            RecoveryEvent::RestoreInstall
                | RecoveryEvent::RestoreRelocate
                | RecoveryEvent::RestoreReverseExchange
        ) {
            if self.expired.load(Ordering::Acquire) {
                self.subsequent_actions.fetch_add(1, Ordering::AcqRel);
            }
            if event == self.marker && !self.armed.swap(true, Ordering::AcqRel) {
                let guard: serde_json::Value = serde_json::from_slice(&fs::read(
                    self.destination.with_extension("fls.restore-guard"),
                )?)?;
                let staging = std::path::Path::new(guard["journal"].as_str().unwrap())
                    .parent()
                    .unwrap();
                *self.before_layout.lock().unwrap() =
                    Some(restore_expiry_layout(&self.destination, staging));
                // The exchange's second digest is the destination. Relocation
                // hashes the original now named candidate; absent rollback hashes destination.
                let target = if event == RecoveryEvent::RestoreRelocate
                    && self.destination.exists()
                    && staging.join("candidate.fls").exists()
                {
                    staging.join("candidate.fls")
                } else if self.destination.exists() {
                    self.destination.clone()
                } else {
                    staging.join("candidate.fls")
                };
                let m = fs::metadata(target)?;
                *self.final_identity.lock().unwrap() = Some((m.dev(), m.ino()));
                if matches!(self.cut, RestoreExpiryCut::BeforeDigest) {
                    self.expired.store(true, Ordering::Release);
                }
            }
        }
        Ok(())
    }
    fn read_exact_at(&self, file: &fs::File, bytes: &mut [u8], offset: u64) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(bytes, offset)?;
        let m = file.metadata()?;
        if self.armed.load(Ordering::Acquire)
            && matches!(self.cut, RestoreExpiryCut::FinalRead)
            && *self.final_identity.lock().unwrap() == Some((m.dev(), m.ino()))
            && offset + bytes.len() as u64 == m.len()
        {
            self.expired.store(true, Ordering::Release);
        }
        Ok(())
    }
}
fn restore_expiry_io(
    destination: &std::path::Path,
    lease: &OfflineLease,
    marker: RecoveryEvent,
    cut: RestoreExpiryCut,
) -> Arc<RestoreExpiryIo> {
    let binding = fixture_lease_request(lease);
    Arc::new(RestoreExpiryIo {
        marker,
        cut,
        destination: destination.to_path_buf(),
        issued: binding.issued_at_unix_ms,
        expires: binding.expires_at_unix_ms,
        expired: AtomicBool::new(false),
        armed: AtomicBool::new(false),
        final_identity: Mutex::new(None),
        before_layout: Mutex::new(None),
        subsequent_actions: AtomicUsize::new(0),
    })
}
fn assert_restore_expiry_layout(
    f: &Fixture,
    destination: &std::path::Path,
    hook: &RestoreExpiryIo,
) {
    assert!(
        hook.expired.load(Ordering::Acquire),
        "expiry hook must reach protected action"
    );
    let guard: serde_json::Value =
        serde_json::from_slice(&fs::read(destination.with_extension("fls.restore-guard")).unwrap())
            .unwrap();
    let journal = std::path::Path::new(guard["journal"].as_str().unwrap());
    assert!(journal.exists());
    assert_eq!(
        restore_expiry_layout(destination, journal.parent().unwrap()),
        *hook.before_layout.lock().unwrap().as_ref().unwrap(),
        "namespace changed after lease expiry"
    );
    assert_eq!(hook.subsequent_actions.load(Ordering::Acquire), 0);
    let pins = f.backups.join(".catalog/restore-pins");
    assert!(
        fs::read_dir(pins).unwrap().next().is_some(),
        "restore pin retained"
    );
}
fn assert_forward_restore_expiry(resume: bool) {
    for absent in [false, true] {
        for marker in [
            RecoveryEvent::RestoreInstall,
            RecoveryEvent::RestoreRelocate,
        ] {
            if absent && marker == RecoveryEvent::RestoreRelocate {
                continue;
            }
            for cut in [RestoreExpiryCut::BeforeDigest, RestoreExpiryCut::FinalRead] {
                let f = Fixture::new();
                if absent {
                    let writer = FerroStore::open(&f.source).unwrap();
                    writer
                        .put_doc(
                            "fixture_unknown",
                            "one",
                            &serde_json::json!({"body": "x".repeat(20 << 20)}),
                        )
                        .unwrap();
                    drop(writer);
                }
                let destination = if absent {
                    f.root.path().join("absent.fls")
                } else {
                    f.source.clone()
                };
                let (request, lease) = prepare_restore(&f, &destination);
                let hook = restore_expiry_io(&destination, &lease, marker, cut);
                let filesystem = CutFs {
                    f: &f,
                    io: hook.clone(),
                    limits: limits(),
                };
                let error = if resume {
                    // Cut before any protected namespace operation, then resume
                    // from the real durable intent with separate clock authority.
                    let stop = CutFs {
                        f: &f,
                        io: journal_boundary_io(Some(marker)),
                        limits: limits(),
                    };
                    assert!(restore_offline(&request, &lease, &stop).is_err());
                    let guard: serde_json::Value = serde_json::from_slice(
                        &fs::read(destination.with_extension("fls.restore-guard")).unwrap(),
                    )
                    .unwrap();
                    let j = load_restore_journal(
                        std::path::Path::new(guard["journal"].as_str().unwrap()),
                        &lease,
                        &f.filesystem(),
                    )
                    .unwrap();
                    resume_restore(&j, &lease, &filesystem).unwrap_err()
                } else {
                    restore_offline(&request, &lease, &filesystem).unwrap_err()
                };
                if resume {
                    assert!(
                        matches!(error, fl_store::recovery::RecoveryError::Deadline { .. }),
                        "{error:?}"
                    );
                } else {
                    assert!(
                        matches!(
                            error,
                            fl_store::recovery::RecoveryError::OutcomeUnknown { .. }
                        ),
                        "{error:?}"
                    );
                }
                assert_restore_expiry_layout(&f, &destination, &hook);
                eprintln!(
                    "restore_expiry_case resume={resume} absent={absent} marker={marker:?} cut={cut:?} preserved=true"
                );
            }
        }
    }
}
#[test]
fn restore_lease_expiry_initial_forward_actions_preserve_namespace() {
    assert_forward_restore_expiry(false);
}
#[test]
fn restore_lease_expiry_resumed_forward_actions_preserve_namespace() {
    assert_forward_restore_expiry(true);
}
#[test]
fn restore_lease_expiry_reverse_actions_preserve_namespace() {
    for absent in [false, true] {
        for cut in [RestoreExpiryCut::BeforeDigest, RestoreExpiryCut::FinalRead] {
            let f = Fixture::new();
            if absent {
                let writer = FerroStore::open(&f.source).unwrap();
                writer
                    .put_doc(
                        "fixture_unknown",
                        "one",
                        &serde_json::json!({"body": "x".repeat(20 << 20)}),
                    )
                    .unwrap();
                drop(writer);
            }
            let destination = if absent {
                f.root.path().join("absent.fls")
            } else {
                f.source.clone()
            };
            let (request, lease) = prepare_restore(&f, &destination);
            let RestoreOutcome::Installed { journal } =
                restore_offline(&request, &lease, &f.filesystem()).unwrap()
            else {
                panic!("not installed")
            };
            let marker = if absent {
                RecoveryEvent::RestoreRelocate
            } else {
                RecoveryEvent::RestoreReverseExchange
            };
            let hook = restore_expiry_io(&destination, &lease, marker, cut);
            let filesystem = CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits(),
            };
            let error = rollback_restore(&journal, &lease, &filesystem).unwrap_err();
            assert!(
                matches!(error, fl_store::recovery::RecoveryError::Deadline { .. }),
                "{error:?}"
            );
            assert_restore_expiry_layout(&f, &destination, &hook);
            eprintln!(
                "restore_expiry_case reverse=true absent={absent} cut={cut:?} preserved=true"
            );
        }
    }
}

// Reservation oracle uses independent owned JSON normalization in tests only.
fn reservation_json_bytes<T: serde::Serialize>(v: &T) -> u64 {
    serde_json::to_vec(v).unwrap().len() as u64
}
fn reservation_expected_caps(j: &RestoreJournal) -> (u64, u64, u64) {
    use serde_json::json;
    let mut base = serde_json::to_value(j).unwrap();
    for field in [
        "verification",
        "terminal_evidence_context",
        "reconciliation",
        "absence_reconciliation",
    ] {
        base[field] = serde_json::Value::Null;
    }
    base["phase"] = json!("original_absence_finalized");
    for field in [
        "resume_finalize_attempts",
        "rollback_attempts",
        "current_attempt",
    ] {
        base[field] = json!(u32::MAX);
    }
    base["attempt_deadline_unix_ms"] = json!(u64::MAX);
    base["forensic_handling_required"] = json!(false);
    base["generation_metadata_sha256"] = json!(([255u8; 32]));
    // Value cannot represent u128::MAX: add its 19 extra decimal digits.
    let b = reservation_json_bytes(&base) + 19;
    let mut binding = serde_json::to_value(&j.binding).unwrap();
    binding["expected_file"] = json!({"state":"readable", "identity":[u64::MAX,u64::MAX], "bytes":u64::MAX, "sha256":"f".repeat(64)});
    binding["issued_at_unix_ms"] = json!(u64::MAX);
    binding["expires_at_unix_ms"] = json!(u64::MAX);
    for field in [
        "legacy_and_cached_handles_excluded",
        "external_dispatch_excluded",
        "active_owner_excluded",
    ] {
        binding["exclusion"][field] = json!(false);
    }
    let l = reservation_json_bytes(&binding) + 38
        - reservation_json_bytes(&j.binding.operator)
        - reservation_json_bytes(&j.binding.exclusion.evidence)
        + 128
        + 256;
    let mut report = serde_json::to_value(
        j.verification
            .as_ref()
            .or(j.original_verification.as_ref())
            .unwrap(),
    )
    .unwrap();
    for field in [
        "report_version",
        "profile_version",
        "canonical_encoding_version",
    ] {
        report[field] = json!(u32::MAX);
    }
    for field in [
        "image_bytes",
        "published_sequence",
        "verified_collection_count",
        "unknown_collection_count",
        "query_probe_count",
        "exception_count",
        "dependency_count",
    ] {
        report[field] = json!(u64::MAX);
    }
    for field in [
        "intrinsic_lineage_available",
        "opaque_runtime_references",
        "transient_rpc_dedupe_persisted",
        "durable_rpc_receipts_claimed",
    ] {
        report[field] = json!(false);
    }
    report["readiness"] = json!("dependencies_require_reconciliation");
    report["copy_evidence"] = json!({"verified_file_identity":[u64::MAX,u64::MAX], "source_image_identity":[u64::MAX,u64::MAX], "creation":"caller_supplied_existing", "image_sha256":"f".repeat(64)});
    report["verifier_build_identity"] = serde_json::to_value(&j.binding.build).unwrap();
    let r = reservation_json_bytes(&report);
    assert_eq!(r, 1265 + reservation_json_bytes(&j.binding.build));
    let e = (j.journal_capacity / 96 - 72)
        .checked_sub(b + r + l + 768)
        .unwrap();
    (128, l, e)
}
fn reservation_pad_record(
    j: &RestoreJournal,
    original: bool,
    bytes: u64,
) -> fl_store::recovery::readiness::ReconciliationRecord {
    let mut record = generation_evidence(j, original);
    let n = reservation_json_bytes(&record);
    assert!(n <= bytes, "mandatory record {n} exceeds target {bytes}");
    record.affected_scopes[0].push_str(&"x".repeat((bytes - n) as usize));
    assert_eq!(reservation_json_bytes(&record), bytes);
    record
}
type ReservationArtifacts =
    std::collections::BTreeMap<std::path::PathBuf, (u64, u64, Option<Vec<u8>>)>;
fn reservation_artifacts(f: &Fixture) -> ReservationArtifacts {
    fn visit(root: &std::path::Path, path: &std::path::Path, entries: &mut ReservationArtifacts) {
        let metadata = fs::symlink_metadata(path).unwrap();
        entries.insert(
            path.strip_prefix(root).unwrap().to_path_buf(),
            (
                metadata.dev(),
                metadata.ino(),
                metadata.is_file().then(|| fs::read(path).unwrap()),
            ),
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), entries);
            }
        }
    }
    let mut entries = std::collections::BTreeMap::new();
    visit(f.root.path(), f.root.path(), &mut entries);
    entries
}
fn reservation_refuses_without_artifacts(
    f: &Fixture,
    request: &RestoreRequest,
    lease: &OfflineLease,
) -> fl_store::recovery::RecoveryError {
    let before = reservation_artifacts(f);
    let hook = journal_boundary_io(None);
    let filesystem = CutFs {
        f,
        io: hook.clone(),
        limits: limits(),
    };
    let error = restore_offline(request, lease, &filesystem).unwrap_err();
    let created = hook
        .events
        .lock()
        .unwrap()
        .iter()
        .copied()
        .filter(|event| {
            matches!(
                event,
                RecoveryEvent::CreateDirectory
                    | RecoveryEvent::Create(
                        Member::Image
                            | Member::RestoreProbe
                            | Member::RestoreJournal
                            | Member::RestoreOperationLock
                            | Member::RestorePin
                            | Member::RestoreGuard
                    )
            )
        })
        .collect::<Vec<_>>();
    assert!(
        created.is_empty(),
        "reservation refusal created restore artifacts: {created:?}; {error}"
    );
    assert_eq!(
        reservation_artifacts(f),
        before,
        "reservation refusal changed source, bundle or namespace"
    );
    let diagnostic = error.to_string();
    let fl_store::recovery::RecoveryError::RestoreMetadataReservationQuota {
        offered_bytes,
        required_bytes,
        ..
    } = &error
    else {
        panic!("{diagnostic}")
    };
    assert_eq!(*offered_bytes, request.metadata_byte_limit);
    assert!(
        *required_bytes > *offered_bytes && *required_bytes <= 512 << 20,
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains(&required_bytes.to_string()),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("metadata_byte_limit"), "{diagnostic}");
    assert!(
        diagnostic.contains(&request.metadata_byte_limit.to_string()),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("requires") && diagnostic.contains("increase"),
        "{diagnostic}"
    );
    error
}
#[test]
fn restore_terminal_reservation_padded_admission_refuses_before_guard() {
    let f = journal_fixture();
    let (mut request, lease) = prepare_restore(&f, &f.source);
    let mut binding = fixture_lease_request(&lease);
    binding.exclusion.evidence.push_str(&"x".repeat(3000));
    fs::remove_file(lease.path()).unwrap();
    let lease = create_offline_lease(&binding, &f.filesystem(), &limits()).unwrap();
    let error = reservation_refuses_without_artifacts(&f, &request, &lease);
    let fl_store::recovery::RecoveryError::RestoreMetadataReservationQuota {
        required_bytes, ..
    } = error
    else {
        unreachable!()
    };
    request.metadata_byte_limit = required_bytes + 4096;
    assert!(matches!(
        restore_offline(&request, &lease, &f.filesystem()).unwrap(),
        RestoreOutcome::Installed { .. }
    ));
    eprintln!(
        "reservation padded lease: artifact-free refusal; required metadata plus 4096 bytes admits"
    );
}
fn reservation_exact_lease(j: &RestoreJournal, lease: &OfflineLease, f: &Fixture) -> OfflineLease {
    let mut binding = fixture_lease_request(lease);
    binding.expected_file = inspect_file_binding(&j.destination, &limits()).unwrap();
    binding.operator = reservation_encoded_string("next shift ", 128);
    binding.exclusion.evidence = reservation_encoded_string("new exclusion ticket ", 256);
    fs::remove_file(lease.path()).unwrap();
    create_offline_lease(&binding, &f.filesystem(), &limits()).unwrap()
}
#[test]
fn restore_terminal_reservation_exact_e_and_restart_complete() {
    use fl_store::recovery::readiness::*;
    for original in [false, true] {
        let mode = if original {
            TerminalFixtureMode::Original
        } else {
            TerminalFixtureMode::Selected
        };
        let (f, journal, lease) = terminal_fixture(mode);
        let expected = reservation_expected_caps(&journal);
        let lease = reservation_exact_lease(&journal, &lease, &f);
        let path = journal.staging.join("journal.bin");
        let restarted = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(reservation_expected_caps(&restarted), expected);
        let mut record = reservation_pad_record(
            &restarted,
            original,
            expected.2 - (128 - reservation_json_bytes(&restarted.binding.operator)),
        );
        record.operator = fixture_lease_request(&lease).operator;
        assert_eq!(reservation_json_bytes(&record), expected.2);
        let outcome = if original {
            finalize_rollback(&restarted, &lease, &record, &f.filesystem())
        } else {
            finalize_restore(&restarted, &lease, &record, &f.filesystem())
        };
        assert!(matches!(outcome.unwrap(), RestoreOutcome::Finalized { .. }));
        assert!(
            !journal
                .destination
                .with_extension("fls.restore-guard")
                .exists()
        );
        let done = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(done.reconciliation.as_ref(), Some(&record));
        let bytes = fs::read(&path).unwrap();
        let maximum = journal_frame_ranges(&bytes)
            .iter()
            .map(|frame| frame.end - frame.start - 72)
            .max()
            .unwrap();
        assert!(maximum as u64 <= journal.journal_capacity / 96 - 72);
        eprintln!(
            "reservation combined boundary actual max frame={maximum} P={}",
            journal.journal_capacity / 96 - 72
        );
        assert_eq!(reservation_expected_caps(&done), expected);
        eprintln!("reservation exact E original={original} E={}", expected.2);
    }
    for mode in [TerminalFixtureMode::Absence, TerminalFixtureMode::Abandon] {
        let (f, journal, lease) = terminal_fixture(mode);
        let (o, _, e) = reservation_expected_caps(&journal);
        let lease = reservation_exact_lease(&journal, &lease, &f);
        assert_eq!(
            reservation_json_bytes(&fixture_lease_request(&lease).operator),
            o
        );
        reservation_call_with_operator(mode, &journal, &lease, &f).unwrap();
        let done = load_restore_journal(
            &journal.staging.join("journal.bin"),
            &lease,
            &f.filesystem(),
        )
        .unwrap();
        if let Some(record) = &done.absence_reconciliation {
            assert!(reservation_json_bytes(record) <= e);
        }
        assert!(
            !journal
                .destination
                .with_extension("fls.restore-guard")
                .exists()
        );
        eprintln!("reservation bounded {mode:?} completes with exact O");
    }
}
#[test]
fn restore_terminal_reservation_one_byte_over_caps_is_free() {
    use fl_store::recovery::readiness::finalize_restore;
    for mode in [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::Abandon,
    ] {
        for kind in ["operator", "exclusion", "evidence"] {
            if kind == "evidence" && !matches!(mode, TerminalFixtureMode::Selected) {
                continue;
            }
            let (f, journal, lease) = terminal_fixture(mode);
            let (_, _, e) = reservation_expected_caps(&journal);
            let mut binding = fixture_lease_request(&lease);
            binding.expected_file = inspect_file_binding(&journal.destination, &limits()).unwrap();
            let (resource, offered, limit) = if kind == "operator" {
                binding.operator = reservation_encoded_string("second shift ", 129);
                ("restore accepting operator", 129, 128)
            } else if kind == "exclusion" {
                binding.exclusion.evidence = reservation_encoded_string("renewed exclusion ", 257);
                ("restore accepting lease exclusion evidence", 257, 256)
            } else {
                ("restore accepting evidence", e + 1, e)
            };
            fs::remove_file(lease.path()).unwrap();
            let current = create_offline_lease(&binding, &f.filesystem(), &limits()).unwrap();
            let before = terminal_negative_metadata(&f, &journal);
            let error = if kind == "evidence" {
                let record = reservation_pad_record(&journal, false, e + 1);
                finalize_restore(&journal, &current, &record, &f.filesystem()).unwrap_err()
            } else {
                reservation_call_with_operator(mode, &journal, &current, &f).unwrap_err()
            };
            assert!(
                matches!(error,
                fl_store::recovery::RecoveryError::RestoreTerminalInputQuota {
                    resource: r, offered_bytes: o, limit_bytes: l, ..
                } if r == resource && o == offered && l == limit),
                "{mode:?}/{kind}: {error:?}"
            );
            let diagnostic = error.to_string();
            assert!(
                diagnostic.contains(&format!("offers {offered} encoded bytes")),
                "{diagnostic}"
            );
            assert!(
                diagnostic.contains(&format!("limit {limit} encoded bytes")),
                "{diagnostic}"
            );
            assert_eq!(
                terminal_negative_metadata(&f, &journal),
                before,
                "{mode:?}/{kind} over cap changed durable metadata"
            );
            let loaded = load_restore_journal(
                &journal.staging.join("journal.bin"),
                &current,
                &f.filesystem(),
            )
            .unwrap();
            assert_eq!(
                terminal_negative_counters(&loaded),
                terminal_negative_counters(&journal)
            );
            eprintln!("reservation {mode:?} {kind}+1 free refusal: {diagnostic}");
        }
    }
}

fn reservation_retry_compatibility(historical: u8) {
    use fl_store::recovery::readiness::finalize_restore;
    let (f, journal, lease) = terminal_fixture(TerminalFixtureMode::Selected);
    let cut = CutFs {
        f: &f,
        io: Arc::new(TerminalCutIo::new(RecoveryEvent::Sync(
            Directory::RestoreDestinationParent,
        ))),
        limits: limits(),
    };
    assert!(
        finalize_restore(
            &journal,
            &lease,
            &generation_evidence(&journal, false),
            &cut
        )
        .is_err()
    );
    let path = journal.staging.join("journal.bin");
    let mut intent = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    assert_eq!(intent.phase, RestorePhase::FinalizeIntent);
    if historical == 1 {
        // Synthetic old-context compatibility fixture: authentication,
        // context digest and semantics remain valid; only the new E is exceeded.
        let mut legacy = intent.clone();
        let e = reservation_expected_caps(&intent).2;
        legacy.reconciliation = Some(reservation_pad_record(&intent, false, e + 1));
        legacy.reconciliation.as_mut().unwrap().recorded_at_unix_ms =
            intent.reconciliation.as_ref().unwrap().recorded_at_unix_ms;
        assert_eq!(
            reservation_json_bytes(legacy.reconciliation.as_ref().unwrap()),
            e + 1
        );
        legacy
            .terminal_evidence_context
            .as_mut()
            .unwrap()
            .reconciliation_sha256 = Some(terminal_negative_digest(
            legacy.reconciliation.as_ref().unwrap(),
        ));
        terminal_negative_reframe_last(&intent, &legacy);
        intent = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(
            reservation_json_bytes(intent.reconciliation.as_ref().unwrap()),
            e + 1
        );
    }
    if historical == 2 {
        // Synthetic old-context compatibility, not new writer output. A prior
        // accepting context can have valid authority/evidence beyond new O/X.
        let mut legacy = intent.clone();
        let operator = reservation_encoded_string("historical accepted operator ", 129);
        legacy.reconciliation.as_mut().unwrap().operator = operator.clone();
        let digest = terminal_negative_digest(legacy.reconciliation.as_ref().unwrap());
        let context = legacy.terminal_evidence_context.as_mut().unwrap();
        context.accepting_lease.operator = operator;
        context.accepting_lease.expected_file =
            inspect_file_binding(&intent.destination, &limits()).unwrap();
        context.accepting_lease.exclusion.evidence =
            reservation_encoded_string("historical accepted exclusion ", 257);
        context.accepting_lease_sha256 = terminal_negative_digest(&context.accepting_lease);
        context.reconciliation_sha256 = Some(digest);
        terminal_negative_reframe_last(&intent, &legacy);
        intent = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
    }
    let accepted = intent.reconciliation.as_ref().unwrap().clone();
    let context = intent.terminal_evidence_context.as_ref().unwrap().clone();
    let mut binding = fixture_lease_request(&lease);
    binding
        .operator
        .push_str(" renewed authority longer than admission");
    binding.expected_file = inspect_file_binding(&intent.destination, &limits()).unwrap();
    assert!(
        reservation_json_bytes(&binding.operator)
            > reservation_json_bytes(&intent.binding.operator)
    );
    fs::remove_file(lease.path()).unwrap();
    let current = create_offline_lease(&binding, &f.filesystem(), &limits()).unwrap();
    let cut = CutFs {
        f: &f,
        io: Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreGuardRemove)),
        limits: limits(),
    };
    assert!(finalize_restore(&intent, &current, &accepted, &cut).is_err());
    let terminal = load_restore_journal(&path, &current, &f.filesystem()).unwrap();
    assert_eq!(terminal.phase, RestorePhase::Finalized);
    assert_eq!(terminal.reconciliation.as_ref(), Some(&accepted));
    assert_eq!(terminal.terminal_evidence_context.as_ref(), Some(&context));
    let before = fs::read(&path).unwrap();
    let counters = terminal_negative_counters(&terminal);
    assert!(matches!(
        finalize_restore(&terminal, &current, &accepted, &f.filesystem()).unwrap(),
        RestoreOutcome::Finalized { .. }
    ));
    assert_eq!(fs::read(&path).unwrap(), before, "cleanup appended a frame");
    let done = load_restore_journal(&path, &current, &f.filesystem()).unwrap();
    assert_eq!(terminal_negative_counters(&done), counters);
    assert_eq!(done.terminal_evidence_context.as_ref(), Some(&context));
    eprintln!("reservation retry compatibility historical={historical}: intent + cleanup 2 cases");
}
#[test]
fn restore_terminal_reservation_long_renewal_preserves_acceptance() {
    reservation_retry_compatibility(0);
}
#[test]
fn restore_terminal_reservation_historical_large_accepted_record_retries() {
    reservation_retry_compatibility(1);
}
#[test]
fn restore_terminal_reservation_historical_large_accepting_fields_retry() {
    reservation_retry_compatibility(2);
}
#[test]
fn restore_terminal_reservation_dependency_graph_admission() {
    use fl_store::recovery::{
        backup::*,
        profiles::{DependencyPolicy, ReferenceDependency},
        readiness::*,
    };
    for original in [false, true] {
        for graph in ["unique", "duplicate", "conflict"] {
            let f = Fixture::new();
            let dependency = ReferenceDependency {
                reference_sha256: "a".repeat(64),
                catalog_identity: "reservation-dependency".into(),
                required_sequence: Some(17),
                policy: DependencyPolicy::ReconcileBeforeAdmission,
            };
            let mut dependencies = vec![dependency.clone()];
            if graph != "unique" {
                let mut second = dependency.clone();
                if graph == "conflict" {
                    second.required_sequence = Some(18);
                }
                dependencies.push(second);
            }
            let mut backup_request = f.backup_request();
            if !original {
                backup_request.profile.dependencies = dependencies.clone();
            }
            let owner = FerroStore::open(&f.source).unwrap();
            let PublicationOutcome::Durable(bundle) = backup(
                &owner,
                &backup_request,
                &f.filesystem(),
                &fl_store::recovery::CancelToken::new(),
            )
            .unwrap() else {
                panic!("not durable")
            };
            drop(owner);
            let (mut request, lease) = prepare_restore_bundle(&f, &f.source, bundle);
            // Provision this dependency fixture for fixed future O/X plus
            // conservative maximum-width identities before staging exists.
            request.metadata_byte_limit += if original { 64 << 10 } else { 32 << 10 };
            if original {
                request.original_profile.dependencies = dependencies;
            }
            request.available_dependencies = vec![DependencyResolution {
                reference_sha256: dependency.reference_sha256.clone(),
                catalog_identity: dependency.catalog_identity.clone(),
                observed_sequence: 17,
                evidence_sha256: "e".repeat(64),
            }];
            let before = fs::read(&f.source).unwrap();
            let outcome = restore_offline(&request, &lease, &f.filesystem());
            if graph == "unique" {
                let RestoreOutcome::Installed { mut journal } = outcome.unwrap() else {
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
                if original {
                    let (_, lease_cap, evidence_cap) = reservation_expected_caps(&journal);
                    eprintln!(
                        "original unique graph metadata={} P={} L={lease_cap} E={evidence_cap}",
                        request.metadata_byte_limit,
                        journal.journal_capacity / 96 - 72
                    );
                }
                let mut record = generation_evidence(&journal, original);
                record.dependencies = request.available_dependencies.clone();
                if original {
                    let mut maximum = record.clone();
                    maximum.operator = "x".repeat(126);
                    maximum.recorded_at_unix_ms = u128::MAX;
                    maximum.affected_scopes =
                        vec![journal.original_profile.catalog_identity.clone()];
                    maximum.index = IndexReadiness::Degraded;
                    eprintln!(
                        "original unique mandatory max-O record={}",
                        reservation_json_bytes(&maximum)
                    );
                }
                let outcome = if original {
                    finalize_rollback(&journal, &lease, &record, &f.filesystem())
                } else {
                    finalize_restore(&journal, &lease, &record, &f.filesystem())
                };
                assert!(matches!(outcome.unwrap(), RestoreOutcome::Finalized { .. }));
            } else {
                assert!(
                    outcome.is_err(),
                    "accepted ambiguous {graph} original={original}"
                );
                assert!(!f.source.with_extension("fls.restore-guard").exists());
                assert_eq!(fs::read(&f.source).unwrap(), before);
            }
            eprintln!("reservation dependency original={original} graph={graph}");
        }
    }
}

// A final-name writer leaves an undecodable pin at allocate/write/flush cuts.
// Exercise every publication boundary through the public typed abandon path.
#[test]
fn restore_pin_publication_every_cut_abandons_and_permits_unrelated_purge() {
    use fl_store::recovery::purge::*;
    use std::os::unix::fs::PermissionsExt;
    let trace = Fixture::new();
    let (request, lease) = prepare_restore(&trace, &trace.source);
    let hook = Arc::new(CutIo::new(usize::MAX, libc::EIO));
    restore_offline(
        &request,
        &lease,
        &CutFs {
            f: &trace,
            io: hook.clone(),
            limits: limits(),
        },
    )
    .unwrap();
    let events = hook.events.lock().unwrap().clone();
    let first = events
        .iter()
        .position(|e| *e == RecoveryEvent::Create(Member::RestorePin))
        .unwrap();
    let last = events
        .iter()
        .position(|e| *e == RecoveryEvent::Create(Member::RestoreGuard))
        .unwrap();
    assert!(events[first..last].contains(&RecoveryEvent::Allocate));
    assert!(events[first..last].contains(&RecoveryEvent::Write(Member::RestorePin)));
    assert!(events[first..last].contains(&RecoveryEvent::Flush(Member::RestorePin)));
    assert!(events[first..last].contains(&RecoveryEvent::RenameEntry));
    assert!(events[first..last].contains(&RecoveryEvent::Sync(Directory::RestorePinParent)));
    assert!(events[first..last].contains(&RecoveryEvent::Sync(Directory::CatalogParent)));
    for (cut, observed_event) in events.iter().enumerate().take(last).skip(first) {
        let f = Fixture::new();
        let (request, lease) = prepare_restore(&f, &f.source);
        let unrelated = f.bundle();
        let op =
            uuid::Uuid::parse_str(unrelated.path().file_name().unwrap().to_str().unwrap()).unwrap();
        let manifest_bytes = fs::read(unrelated.path().join("manifest.json")).unwrap();
        let manifest: BundleManifestV1 = serde_json::from_slice(&manifest_bytes).unwrap();
        let purge_request = f.lease_request(
            OfflineAction::Purge {
                quarantined_operation: op,
                manifest_sha256: format!("{:x}", sha2::Sha256::digest(&manifest_bytes)),
            },
            op,
            manifest.validation_profile_sha256,
            &f.source,
        );
        let purge_lease = create_offline_lease(&purge_request, &f.filesystem(), &limits()).unwrap();
        let entry = f.backups.join(".quarantine").join(op.to_string());
        fs::create_dir(&entry).unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o700)).unwrap();
        fs::rename(
            f.backups.join(".catalog").join(format!("{op}.json")),
            entry.join("receipt.json"),
        )
        .unwrap();
        let original = fs::read(&f.source).unwrap();
        let original_metadata = fs::metadata(&f.source).unwrap();
        let hook = Arc::new(CutIo::new(cut, libc::EIO));
        let error = restore_offline(
            &request,
            &lease,
            &CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits(),
            },
        )
        .unwrap_err();
        assert!(hook.count.load(Ordering::Acquire) > cut);
        let fl_store::recovery::RecoveryError::RestoreAdmissionArtifactsRetained {
            staging, ..
        } = error
        else {
            panic!("pin cut must retain typed admission artifacts: {error:?}")
        };
        let journal_path = staging.join("journal.bin");
        let journal = load_restore_journal(&journal_path, &lease, &f.filesystem()).unwrap();
        assert_eq!(journal.phase, RestorePhase::Admitted);
        let pins = f.backups.join(".catalog/restore-pins");
        let final_path = pins.join(format!("{}.json", journal.operation_id));
        let temp_path = pins.join(format!("{}.json.tmp", journal.operation_id));
        if final_path.exists() {
            let pin: serde_json::Value = serde_json::from_slice(&fs::read(&final_path).unwrap())
                .expect("named final pin must always be complete");
            assert_eq!(pin["restore_operation"], journal.operation_id.to_string());
        }
        // The temp is not a pin: unrelated purge works before terminal cleanup.
        let inspect = PurgeRequest {
            operation_id: op,
            confirm: None,
            reference_closure: None,
        };
        let report = purge_quarantined(&inspect, &purge_lease, &f.filesystem(), &limits())
            .expect("pin publication interruption must not block unrelated purge");
        let confirm = PurgeRequest {
            operation_id: op,
            confirm: Some(op),
            reference_closure: Some(ReferenceClosureEvidence {
                catalog_set_sha256: report.catalog_set_sha256,
                non_ferroloop_writers_excluded: true,
                evidence_sha256: "d".repeat(64),
            }),
        };
        assert!(
            purge_quarantined(&confirm, &purge_lease, &f.filesystem(), &limits())
                .unwrap()
                .complete
        );
        assert!(matches!(
            abandon_restore(&journal, &lease, &f.filesystem())
                .expect("every pin publication cut must have a typed exit"),
            RestoreOutcome::Abandoned { .. }
        ));
        assert_eq!(
            load_restore_journal(&journal_path, &lease, &f.filesystem())
                .unwrap()
                .phase,
            RestorePhase::Abandoned
        );
        assert!(!final_path.exists() && !temp_path.exists());
        let now = fs::metadata(&f.source).unwrap();
        assert_eq!(
            (now.dev(), now.ino()),
            (original_metadata.dev(), original_metadata.ino())
        );
        assert_eq!(fs::read(&f.source).unwrap(), original);
        eprintln!(
            "pin_publication_cut index={cut} event={:?} abandon=true unrelated_purge=true",
            observed_event
        );
    }
}

struct PinPublicationRace {
    pins: std::path::PathBuf,
    operation: uuid::Uuid,
    target: RecoveryEvent,
    temp: bool,
    replace: bool,
    fired: AtomicUsize,
}
impl RecoveryIo for PinPublicationRace {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        if event == self.target && self.fired.fetch_add(1, Ordering::AcqRel) == 0 {
            let suffix = if self.temp { "json.tmp" } else { "json" };
            let path = self.pins.join(format!("{}.{suffix}", self.operation));
            if self.replace && path.exists() {
                fs::rename(&path, path.with_extension("parked"))?;
            }
            fs::write(&path, b"foreign pin artifact")?;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}
#[test]
fn restore_pin_publication_never_replaces_a_final_pin_or_changed_temp() {
    for temp in [false, true] {
        let f = Fixture::new();
        let (request, lease) = prepare_restore(&f, &f.source);
        let pins = f.backups.join(".catalog/restore-pins");
        let before = fs::read(&f.source).unwrap();
        let hook = Arc::new(PinPublicationRace {
            pins: pins.clone(),
            operation: request.operation_id,
            target: RecoveryEvent::Publish,
            temp,
            replace: temp,
            fired: AtomicUsize::new(0),
        });
        let error = restore_offline(
            &request,
            &lease,
            &CutFs {
                f: &f,
                io: hook.clone(),
                limits: limits(),
            },
        )
        .unwrap_err();
        assert_eq!(hook.fired.load(Ordering::Acquire), 1);
        assert!(matches!(
            error,
            fl_store::recovery::RecoveryError::RestoreAdmissionArtifactsRetained { .. }
        ));
        let path = pins.join(format!(
            "{}.{}",
            request.operation_id,
            if temp { "json.tmp" } else { "json" }
        ));
        assert_eq!(fs::read(path).unwrap(), b"foreign pin artifact");
        assert!(!f.source.with_extension("fls.restore-guard").exists());
        assert_eq!(fs::read(&f.source).unwrap(), before);
    }
}
#[test]
fn restore_pin_temp_cleanup_rechecks_identity_and_both_names_after_sync_hooks() {
    for target in [
        RecoveryEvent::RestorePinRemove,
        RecoveryEvent::Sync(Directory::RestorePinParent),
        RecoveryEvent::Sync(Directory::CatalogParent),
    ] {
        for temp in [false, true] {
            if target == RecoveryEvent::RestorePinRemove && !temp {
                continue;
            }
            let f = Fixture::new();
            let (request, lease) = prepare_restore(&f, &f.source);
            // Stop before the pin flush: retain a complete but unpublished temp.
            let stop = Arc::new(TerminalCutIo::new(RecoveryEvent::Flush(Member::RestorePin)));
            let error = restore_offline(
                &request,
                &lease,
                &CutFs {
                    f: &f,
                    io: stop.clone(),
                    limits: limits(),
                },
            )
            .unwrap_err();
            assert_eq!(stop.fired.load(Ordering::Acquire), 1);
            let fl_store::recovery::RecoveryError::RestoreAdmissionArtifactsRetained {
                staging,
                ..
            } = error
            else {
                panic!("{error:?}")
            };
            let journal =
                load_restore_journal(&staging.join("journal.bin"), &lease, &f.filesystem())
                    .unwrap();
            let pins = f.backups.join(".catalog/restore-pins");
            let hook = Arc::new(PinPublicationRace {
                pins: pins.clone(),
                operation: request.operation_id,
                target,
                temp,
                replace: target == RecoveryEvent::RestorePinRemove,
                fired: AtomicUsize::new(0),
            });
            assert!(matches!(
                abandon_restore(
                    &journal,
                    &lease,
                    &CutFs {
                        f: &f,
                        io: hook.clone(),
                        limits: limits()
                    }
                )
                .unwrap(),
                RestoreOutcome::OutcomeUnknown { .. }
            ));
            assert_eq!(hook.fired.load(Ordering::Acquire), 1);
            let path = pins.join(format!(
                "{}.{}",
                request.operation_id,
                if temp { "json.tmp" } else { "json" }
            ));
            assert_eq!(fs::read(path).unwrap(), b"foreign pin artifact");
            assert_eq!(
                load_restore_journal(&staging.join("journal.bin"), &lease, &f.filesystem())
                    .unwrap()
                    .phase,
                RestorePhase::Abandoned
            );
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum BundlelessLayout {
    BeforeExchange,
    Exchanged,
    Relocating,
    Relocated,
    AbsentExchanged,
}
struct BundlelessInstallCut {
    layout: BundlelessLayout,
    installed: AtomicBool,
    relocated: AtomicBool,
    fired: AtomicBool,
}
impl RecoveryIo for BundlelessInstallCut {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        let cut = match self.layout {
            BundlelessLayout::BeforeExchange => event == RecoveryEvent::RestoreInstall,
            BundlelessLayout::Exchanged | BundlelessLayout::AbsentExchanged => {
                self.installed.load(Ordering::Acquire)
                    && event == RecoveryEvent::Sync(Directory::RestoreDestinationParent)
            }
            BundlelessLayout::Relocating => event == RecoveryEvent::RestoreRelocate,
            BundlelessLayout::Relocated => {
                self.relocated.load(Ordering::Acquire)
                    && event == RecoveryEvent::Sync(Directory::RestoreDestinationParent)
            }
        };
        if event == RecoveryEvent::RestoreInstall {
            self.installed.store(true, Ordering::Release);
        }
        if event == RecoveryEvent::RestoreRelocate {
            self.relocated.store(true, Ordering::Release);
        }
        if cut && !self.fired.swap(true, Ordering::AcqRel) {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(())
    }
}
fn bundleless_fixture(layout: BundlelessLayout) -> (Fixture, RestoreJournal, OfflineLease) {
    bundleless_fixture_with_bundle(layout, true)
}
fn bundleless_fixture_with_bundle(
    layout: BundlelessLayout,
    remove_bundle: bool,
) -> (Fixture, RestoreJournal, OfflineLease) {
    let f = Fixture::new();
    let absent = matches!(layout, BundlelessLayout::AbsentExchanged);
    if absent {
        // A genuine large live value supplies the unchanged strict-2L baseline.
        let writer = FerroStore::open(&f.source).unwrap();
        writer
            .put_doc(
                "fixture_unknown",
                "one",
                &serde_json::json!({"body":"x".repeat(20 << 20)}),
            )
            .unwrap();
        drop(writer);
    }
    let destination = if absent {
        f.root.path().join("absent.fls")
    } else {
        f.source.clone()
    };
    let (request, lease) = prepare_restore(&f, &destination);
    let hook = Arc::new(BundlelessInstallCut {
        layout,
        installed: AtomicBool::new(false),
        relocated: AtomicBool::new(false),
        fired: AtomicBool::new(false),
    });
    let filesystem = CutFs {
        f: &f,
        io: hook.clone(),
        limits: limits(),
    };
    let outcome = restore_offline(&request, &lease, &filesystem);
    assert!(
        outcome.is_err() || matches!(outcome, Ok(RestoreOutcome::OutcomeUnknown { .. })),
        "{layout:?}: {outcome:?}"
    );
    assert!(hook.fired.load(Ordering::Acquire));
    let guard: serde_json::Value =
        serde_json::from_slice(&fs::read(destination.with_extension("fls.restore-guard")).unwrap())
            .unwrap();
    let j = load_restore_journal(
        std::path::Path::new(guard["journal"].as_str().unwrap()),
        &lease,
        &f.filesystem(),
    )
    .unwrap();
    assert_eq!(
        j.phase,
        if matches!(
            layout,
            BundlelessLayout::Relocating | BundlelessLayout::Relocated
        ) {
            RestorePhase::RelocateIntent
        } else {
            RestorePhase::ExchangeIntent
        }
    );
    if remove_bundle {
        fs::remove_dir_all(request.selected_bundle.path()).unwrap();
    }
    (f, j, lease)
}
fn assert_bundleless_rolled_back(_f: &Fixture, before: &RestoreJournal, after: &RestoreJournal) {
    if matches!(
        before.phase,
        RestorePhase::RolledBack | RestorePhase::OriginalAbsenceRestored
    ) {
        assert_eq!(
            serde_json::to_value(after).unwrap(),
            serde_json::to_value(before).unwrap(),
            "completed rollback retry changed journal state"
        );
    } else {
        assert_eq!(after.rollback_attempts, before.rollback_attempts + 1);
    }
    let candidate_path = if matches!(before.original, FileBinding::Absent) {
        assert_eq!(after.phase, RestorePhase::OriginalAbsenceRestored);
        assert!(!after.destination.exists());
        after.staging.join("candidate.fls")
    } else {
        assert_eq!(after.phase, RestorePhase::RolledBack);
        assert_eq!(
            inspect_file_binding(&after.destination, &limits()).unwrap(),
            before.original
        );
        after.staging.join("original.fls")
    };
    let metadata = fs::metadata(&candidate_path).unwrap();
    assert_eq!((metadata.dev(), metadata.ino()), before.candidate_identity);
    assert_eq!(
        format!(
            "{:x}",
            sha2::Sha256::digest(fs::read(candidate_path).unwrap())
        ),
        before.candidate_sha256
    );
}
#[test]
fn restore_bundleless_post_exchange_rollback_preserves_both_generations() {
    for layout in [
        BundlelessLayout::Exchanged,
        BundlelessLayout::Relocating,
        BundlelessLayout::Relocated,
        BundlelessLayout::AbsentExchanged,
    ] {
        let (f, j, lease) = bundleless_fixture(layout);
        let frames = journal_frame_ranges(&fs::read(j.staging.join("journal.bin")).unwrap()).len();
        let outcome = rollback_restore(&j, &lease, &f.filesystem())
            .unwrap_or_else(|e| panic!("{layout:?}: {e:?}"));
        let next = match outcome {
            RestoreOutcome::RolledBack { journal }
            | RestoreOutcome::OriginalAbsenceRestored { journal } => journal,
            other => panic!("{other:?}"),
        };
        assert_bundleless_rolled_back(&f, &j, &next);
        let expected = if matches!(layout, BundlelessLayout::Exchanged) {
            4
        } else {
            3
        };
        assert_eq!(
            journal_frame_ranges(&fs::read(next.staging.join("journal.bin")).unwrap()).len()
                - frames,
            expected,
            "{layout:?}"
        );
        let loaded =
            load_restore_journal(&next.staging.join("journal.bin"), &lease, &f.filesystem())
                .unwrap();
        assert_eq!(loaded.phase, next.phase);
        eprintln!("bundleless_rollback layout={layout:?} frames={expected}");
    }
}
#[test]
fn restore_bundleless_resume_refusals_do_not_spend_attempts() {
    for layout in [
        BundlelessLayout::BeforeExchange,
        BundlelessLayout::Exchanged,
        BundlelessLayout::Relocating,
        BundlelessLayout::Relocated,
        BundlelessLayout::AbsentExchanged,
    ] {
        let (f, j, lease) = bundleless_fixture(layout);
        let path = j.staging.join("journal.bin");
        let before = fs::read(&path).unwrap();
        for retry in 0..14 {
            assert!(
                resume_restore(&j, &lease, &f.filesystem()).is_err(),
                "{layout:?} retry={retry}"
            );
            assert!(
                fs::read(&path).unwrap() == before,
                "{layout:?} retry={retry} charged a refusal"
            );
        }
        let loaded = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(
            (
                loaded.current_attempt,
                loaded.resume_finalize_attempts,
                loaded.rollback_attempts,
                loaded.forensic_handling_required
            ),
            (
                j.current_attempt,
                j.resume_finalize_attempts,
                j.rollback_attempts,
                j.forensic_handling_required
            )
        );
    }
}
#[test]
fn restore_bundleless_pre_exchange_rollback_refuses_without_attempt() {
    let (f, j, lease) = bundleless_fixture(BundlelessLayout::BeforeExchange);
    let path = j.staging.join("journal.bin");
    let bytes = fs::read(&path).unwrap();
    let layout = restore_expiry_layout(&j.destination, &j.staging);
    let error = rollback_restore(&j, &lease, &f.filesystem()).unwrap_err();
    assert!(error.to_string().contains("abandon_restore"), "{error}");
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(restore_expiry_layout(&j.destination, &j.staging), layout);
}
#[test]
fn restore_bundleless_normalization_cuts_reload_and_rollback() {
    let (f, j, lease) = bundleless_fixture(BundlelessLayout::Exchanged);
    let hook = Arc::new(CutIo::new(usize::MAX, libc::EIO));
    let filesystem = CutFs {
        f: &f,
        io: hook.clone(),
        limits: limits(),
    };
    rollback_restore(&j, &lease, &filesystem).unwrap();
    let events = hook.events.lock().unwrap().clone();
    assert!(events.contains(&RecoveryEvent::RestoreRelocate));
    for (cut, event) in events.iter().enumerate() {
        let (f, j, lease) = bundleless_fixture(BundlelessLayout::Exchanged);
        let hook = Arc::new(CutIo::new(cut, libc::EIO));
        let filesystem = CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        assert!(
            rollback_restore(&j, &lease, &filesystem).is_err(),
            "cut {cut}: {event:?}"
        );
        assert!(hook.count.load(Ordering::Acquire) > cut);
        let loaded = load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem())
            .unwrap_or_else(|e| panic!("cut {cut} {event:?}: {e:?}"));
        let before = fs::read(j.staging.join("journal.bin")).unwrap();
        let frames = journal_frame_ranges(&before).len();
        let RestoreOutcome::RolledBack { journal: next } =
            rollback_restore(&loaded, &lease, &f.filesystem()).unwrap()
        else {
            panic!("cut {cut}: not rolled back")
        };
        assert_bundleless_rolled_back(&f, &loaded, &next);
        let after = fs::read(next.staging.join("journal.bin")).unwrap();
        if loaded.phase == RestorePhase::RolledBack {
            assert_eq!(after, before, "completed cut {cut} appended journal data");
        }
        assert!(
            journal_frame_ranges(&after).len() - frames <= 4,
            "cut {cut}: frame schedule widened"
        );
        eprintln!("bundleless_normalization_cut index={cut} event={event:?} recovered=true");
    }
}
#[test]
fn restore_bundleless_fourth_rollback_attempt_can_finish_normalization() {
    let (f, j, lease) = bundleless_fixture(BundlelessLayout::Exchanged);
    for expected in 1..=3 {
        let hook = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreRelocate));
        let filesystem = CutFs {
            f: &f,
            io: hook,
            limits: limits(),
        };
        assert!(rollback_restore(&j, &lease, &filesystem).is_err());
        let loaded =
            load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
        assert_eq!(loaded.rollback_attempts, expected);
        assert_eq!(loaded.phase, RestorePhase::RelocateIntent);
    }
    let loaded =
        load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
    let frames = journal_frame_ranges(&fs::read(j.staging.join("journal.bin")).unwrap()).len();
    let RestoreOutcome::RolledBack { journal: next } =
        rollback_restore(&loaded, &lease, &f.filesystem()).unwrap()
    else {
        panic!("fourth attempt failed")
    };
    assert_eq!(next.rollback_attempts, 4);
    assert!(next.forensic_handling_required);
    assert_bundleless_rolled_back(&f, &loaded, &next);
    assert_eq!(
        journal_frame_ranges(&fs::read(next.staging.join("journal.bin")).unwrap()).len() - frames,
        3
    );
}

#[test]
fn restore_bundleless_normalization_expiry_preserves_namespace() {
    for cut in [RestoreExpiryCut::BeforeDigest, RestoreExpiryCut::FinalRead] {
        let (f, j, lease) = bundleless_fixture(BundlelessLayout::Exchanged);
        let hook = restore_expiry_io(&j.destination, &lease, RecoveryEvent::RestoreRelocate, cut);
        let filesystem = CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        let error = rollback_restore(&j, &lease, &filesystem).unwrap_err();
        assert!(
            matches!(error, fl_store::recovery::RecoveryError::Deadline { .. }),
            "{error:?}"
        );
        assert_restore_expiry_layout(&f, &j.destination, &hook);
        let loaded =
            load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
        assert_eq!(loaded.phase, RestorePhase::RelocateIntent);
        let RestoreOutcome::RolledBack { journal: next } =
            rollback_restore(&loaded, &lease, &f.filesystem()).unwrap()
        else {
            panic!("expiry retry failed")
        };
        assert_bundleless_rolled_back(&f, &loaded, &next);
    }
}
#[test]
fn restore_bundleless_corrupt_bundle_resume_refuses_without_attempt() {
    for layout in [
        BundlelessLayout::BeforeExchange,
        BundlelessLayout::Relocating,
    ] {
        let (f, j, lease) = bundleless_fixture_with_bundle(layout, false);
        let bundle = request_bundle_path(&f, j.selected_bundle_operation);
        fs::write(bundle.join("manifest.json"), b"{corrupt").unwrap();
        let path = j.staging.join("journal.bin");
        let before = fs::read(&path).unwrap();
        for retry in 0..14 {
            assert!(
                resume_restore(&j, &lease, &f.filesystem()).is_err(),
                "{layout:?} retry={retry}"
            );
            assert!(
                fs::read(&path).unwrap() == before,
                "{layout:?} retry={retry} charged corrupt-bundle refusal"
            );
        }
    }
}

// R3 item 11: observe durable payloads as well as namespace event ordering.
type ResumeDurabilityTrace = Vec<(RecoveryEvent, Option<(RestorePhase, u32)>)>;
struct ResumeDurabilityIo {
    journal: std::path::PathBuf,
    trace: Mutex<ResumeDurabilityTrace>,
}
impl RecoveryIo for ResumeDurabilityIo {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        let committed = if event == RecoveryEvent::Sync(Directory::RestoreJournalParent) {
            let bytes = fs::read(&self.journal).unwrap();
            let frames = journal_frame_ranges(&bytes);
            let last = frames.last().unwrap();
            let value: serde_json::Value =
                serde_json::from_slice(&bytes[last.start + 72..last.end]).unwrap();
            Some((
                journal_phase(&bytes, last),
                value["current_attempt"].as_u64().unwrap() as u32,
            ))
        } else {
            None
        };
        self.trace.lock().unwrap().push((event, committed));
        Ok(())
    }
}
#[test]
fn restore_review11_exchanged_resume_resyncs_before_each_phase_append() {
    let (f, j, lease) = bundleless_fixture_with_bundle(BundlelessLayout::Relocating, false);
    let hook = Arc::new(ResumeDurabilityIo {
        journal: j.staging.join("journal.bin"),
        trace: Mutex::new(vec![]),
    });
    let filesystem = CutFs {
        f: &f,
        io: hook.clone(),
        limits: limits(),
    };
    let RestoreOutcome::Installed { journal: installed } =
        resume_restore(&j, &lease, &filesystem).unwrap()
    else {
        panic!("resume did not install")
    };
    let trace = hook.trace.lock().unwrap();
    let commits: Vec<_> = trace
        .iter()
        .enumerate()
        .filter_map(|(index, (_, payload))| payload.clone().map(|payload| (index, payload)))
        .collect();
    assert_eq!(
        commits.len(),
        3,
        "attempt, relocation intent, installed: {trace:?}"
    );
    assert_eq!(
        commits.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>(),
        vec![
            (RestorePhase::RelocateIntent, j.current_attempt + 1),
            (RestorePhase::RelocateIntent, j.current_attempt + 1),
            (RestorePhase::Installed, j.current_attempt + 1),
        ]
    );
    let relocation = trace
        .iter()
        .position(|(e, _)| *e == RecoveryEvent::RestoreRelocate)
        .unwrap();
    let rename = trace
        .iter()
        .position(|(e, _)| *e == RecoveryEvent::RenameEntry)
        .unwrap();
    assert!(commits[1].0 < relocation && relocation < rename && rename < commits[2].0);
    for (boundary, label) in [
        (commits[0].0, "exchange re-sync"),
        (rename, "relocation re-sync"),
    ] {
        let suffix = &trace[boundary + 1..];
        let write = suffix
            .iter()
            .position(|(e, _)| *e == RecoveryEvent::Write(Member::RestoreJournal))
            .unwrap();
        for directory in [
            Directory::RestoreDestinationParent,
            Directory::RestoreStagingParent,
        ] {
            assert!(
                suffix[..write]
                    .iter()
                    .any(|(e, _)| *e == RecoveryEvent::Sync(directory)),
                "{label}: missing {directory:?} before next journal write: {trace:?}"
            );
        }
    }
    assert!(
        !trace
            .iter()
            .any(|(e, _)| matches!(e, RecoveryEvent::RestoreInstall | RecoveryEvent::Exchange)),
        "resume must not repeat the forward exchange"
    );
    assert_eq!(
        inspect_file_binding(&installed.staging.join("original.fls"), &limits()).unwrap(),
        j.original
    );
    assert_eq!(installed.candidate_identity, j.candidate_identity);
}

fn review11_staging(parent: &std::path::Path, operation: uuid::Uuid) -> std::path::PathBuf {
    let prefix = format!(".restore-{operation}-");
    let entries: Vec<_> = fs::read_dir(parent)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .map(|entry| entry.path())
        .collect();
    assert_eq!(entries.len(), 1);
    entries[0].clone()
}
fn review11_probe_ids(staging: &std::path::Path) -> [(u64, u64); 2] {
    ["exchange-probe-a", "exchange-probe-b"].map(|name| {
        let metadata = fs::metadata(staging.join(name)).unwrap();
        (metadata.dev(), metadata.ino())
    })
}
struct ProbeBeforeGuardIo {
    parent: std::path::PathBuf,
    operation: uuid::Uuid,
    events: Mutex<Vec<RecoveryEvent>>,
    probe_ids: Mutex<Option<[(u64, u64); 2]>>,
    guard_seen: AtomicBool,
    fail_probe: bool,
}
impl RecoveryIo for ProbeBeforeGuardIo {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        self.events.lock().unwrap().push(event);
        if event == RecoveryEvent::RestoreExchangeProbe {
            *self.probe_ids.lock().unwrap() = Some(review11_probe_ids(&review11_staging(
                &self.parent,
                self.operation,
            )));
            if self.fail_probe {
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
        }
        if event == RecoveryEvent::Create(Member::RestoreGuard) {
            self.guard_seen.store(true, Ordering::Release);
            let before = self
                .probe_ids
                .lock()
                .unwrap()
                .expect("guard creation preceded exchange probe");
            assert_ne!(before[0], before[1]);
            let now = review11_probe_ids(&review11_staging(&self.parent, self.operation));
            assert_eq!(
                now,
                [before[1], before[0]],
                "probe did not actually exchange before guard"
            );
            let events = self.events.lock().unwrap();
            let probe = events
                .iter()
                .position(|e| *e == RecoveryEvent::RestoreExchangeProbe)
                .unwrap();
            assert_eq!(events[probe + 1], RecoveryEvent::Exchange);
            let after = &events[probe + 2..events.len() - 1];
            for directory in [
                Directory::RestoreDestinationParent,
                Directory::RestoreStagingParent,
            ] {
                assert!(
                    after.contains(&RecoveryEvent::Sync(directory)),
                    "probe missing {directory:?} before guard: {events:?}"
                );
            }
        }
        Ok(())
    }
}
#[test]
fn restore_review11_native_probe_completes_and_syncs_before_guard() {
    for fail_probe in [false, true] {
        let f = Fixture::new();
        let (request, lease) = prepare_restore(&f, &f.source);
        let original = inspect_file_binding(&f.source, &limits()).unwrap();
        let hook = Arc::new(ProbeBeforeGuardIo {
            parent: f.root.path().to_path_buf(),
            operation: request.operation_id,
            events: Mutex::new(vec![]),
            probe_ids: Mutex::new(None),
            guard_seen: AtomicBool::new(false),
            fail_probe,
        });
        let filesystem = CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        let outcome = restore_offline(&request, &lease, &filesystem);
        if fail_probe {
            assert!(outcome.is_err(), "{outcome:?}");
            assert!(
                hook.probe_ids.lock().unwrap().is_some(),
                "probe cut never reached"
            );
            assert!(!hook.guard_seen.load(Ordering::Acquire));
            assert!(!f.source.with_extension("fls.restore-guard").exists());
            assert_eq!(
                inspect_file_binding(&f.source, &limits()).unwrap(),
                original
            );
            let events = hook.events.lock().unwrap();
            assert!(!events.contains(&RecoveryEvent::RestoreInstall));
        } else {
            assert!(matches!(outcome.unwrap(), RestoreOutcome::Installed { .. }));
            assert!(hook.guard_seen.load(Ordering::Acquire));
            let events = hook.events.lock().unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|e| **e == RecoveryEvent::RestoreExchangeProbe)
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|e| **e == RecoveryEvent::RestoreInstall)
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|e| **e == RecoveryEvent::Exchange)
                    .count(),
                2
            );
        }
    }
}

struct PreparedAbandonCut {
    parent: std::path::PathBuf,
    operation: uuid::Uuid,
    prepared_seen: AtomicBool,
    fired: AtomicBool,
}
impl RecoveryIo for PreparedAbandonCut {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        if event == RecoveryEvent::Sync(Directory::RestoreJournalParent) {
            let path = review11_staging(&self.parent, self.operation).join("journal.bin");
            let bytes = fs::read(path).unwrap();
            let frames = journal_frame_ranges(&bytes);
            if frames
                .last()
                .is_some_and(|last| journal_phase(&bytes, last) == RestorePhase::Prepared)
            {
                self.prepared_seen.store(true, Ordering::Release);
            }
        }
        if event == RecoveryEvent::Write(Member::RestoreJournal)
            && self.prepared_seen.load(Ordering::Acquire)
        {
            self.fired.store(true, Ordering::Release);
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(())
    }
}

#[test]
fn restore_review11_prepared_and_pre_exchange_abandon_retain_generations() {
    for phase in [RestorePhase::Prepared, RestorePhase::ExchangeIntent] {
        let f = Fixture::new();
        let (request, lease) = prepare_restore(&f, &f.source);
        let original = inspect_file_binding(&f.source, &limits()).unwrap();
        let prepared = Arc::new(PreparedAbandonCut {
            parent: f.root.path().to_path_buf(),
            operation: request.operation_id,
            prepared_seen: AtomicBool::new(false),
            fired: AtomicBool::new(false),
        });
        let exchange = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreInstall));
        let io: Arc<dyn RecoveryIo> = if phase == RestorePhase::Prepared {
            prepared.clone()
        } else {
            exchange.clone()
        };
        let filesystem = CutFs {
            f: &f,
            io,
            limits: limits(),
        };
        assert!(restore_offline(&request, &lease, &filesystem).is_err());
        if phase == RestorePhase::Prepared {
            assert!(prepared.fired.load(Ordering::Acquire));
        } else {
            assert_eq!(exchange.fired.load(Ordering::Acquire), 1);
        }
        let staging = review11_staging(f.root.path(), request.operation_id);
        let path = staging.join("journal.bin");
        let j = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(j.phase, phase);
        review14_assert_rollback_action(&f, &j, &lease, "abandon_restore");
        let guard = f.source.with_extension("fls.restore-guard");
        let pin = f
            .backups
            .join(".catalog/restore-pins")
            .join(format!("{}.json", j.operation_id));
        assert!(guard.exists() && pin.exists());
        let retained: std::collections::BTreeMap<_, _> = fs::read_dir(&staging)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let metadata = entry.metadata().unwrap();
                let digest = (entry.file_name() != "journal.bin")
                    .then(|| forensic_descriptor_digest(&fs::File::open(entry.path()).unwrap()));
                (
                    entry.file_name(),
                    ((metadata.dev(), metadata.ino()), metadata.len(), digest),
                )
            })
            .collect();
        assert!(retained.contains_key(std::ffi::OsStr::new("candidate.fls")));
        assert!(matches!(
            abandon_restore(&j, &lease, &f.filesystem()).unwrap(),
            RestoreOutcome::Abandoned { .. }
        ));
        assert!(!guard.exists() && !pin.exists());
        assert_eq!(
            inspect_file_binding(&f.source, &limits()).unwrap(),
            original
        );
        assert_eq!(fs::read_dir(&staging).unwrap().count(), retained.len());
        for (name, (identity, bytes, digest)) in retained {
            let file = staging.join(name);
            let metadata = fs::metadata(&file).unwrap();
            assert_eq!((metadata.dev(), metadata.ino()), identity);
            assert_eq!(metadata.len(), bytes);
            if let Some(digest) = digest {
                assert_eq!(
                    forensic_descriptor_digest(&fs::File::open(file).unwrap()),
                    digest
                );
            }
        }
        let terminal = load_restore_journal(&path, &lease, &f.filesystem()).unwrap();
        assert_eq!(terminal.phase, RestorePhase::Abandoned);
        assert_eq!(terminal.rollback_attempts, j.rollback_attempts + 1);
        let bytes = fs::read(&path).unwrap();
        assert!(matches!(
            abandon_restore(&terminal, &lease, &f.filesystem()).unwrap(),
            RestoreOutcome::Abandoned { .. }
        ));
        assert!(
            fs::read(path).unwrap() == bytes,
            "cleanup retry appended a frame"
        );
    }
}

fn review14_assert_rollback_action(
    f: &Fixture,
    j: &RestoreJournal,
    lease: &OfflineLease,
    action: &str,
) {
    let path = j.staging.join("journal.bin");
    let guard = j.destination.with_extension("fls.restore-guard");
    let pin = terminal_negative_pin(f, j);
    let snapshot = || {
        [&path, &guard, &pin].map(|p| {
            fs::symlink_metadata(p)
                .ok()
                .map(|m| ((m.dev(), m.ino()), fs::read(p).unwrap()))
        })
    };
    let before = snapshot();
    let error = rollback_restore(j, lease, &f.filesystem()).unwrap_err();
    assert!(error.to_string().contains(action), "{:?}: {error}", j.phase);
    assert_eq!(snapshot(), before, "diagnostic refusal changed metadata");
    let loaded = load_restore_journal(&path, lease, &f.filesystem()).unwrap();
    assert_eq!(
        terminal_negative_counters(&loaded),
        terminal_negative_counters(j)
    );
}
#[test]
fn restore_review14_rollback_refusals_name_typed_terminal_actions_without_spending() {
    for mode in [
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
        TerminalFixtureMode::Abandon,
    ] {
        for intent in [true, false] {
            if intent
                && matches!(
                    mode,
                    TerminalFixtureMode::Absence | TerminalFixtureMode::Abandon
                )
            {
                continue;
            }
            let (f, j, lease) = terminal_fixture(mode);
            if matches!(mode, TerminalFixtureMode::Abandon) {
                review14_assert_rollback_action(&f, &j, &lease, "abandon_restore");
                let cut = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreGuardRemove));
                let filesystem = CutFs {
                    f: &f,
                    io: cut.clone(),
                    limits: limits(),
                };
                assert!(abandon_restore(&j, &lease, &filesystem).is_err());
                assert_eq!(cut.fired.load(Ordering::Acquire), 1);
                let journal =
                    load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem())
                        .unwrap();
                assert_eq!(journal.phase, RestorePhase::Abandoned);
                review14_assert_rollback_action(&f, &journal, &lease, "abandon_restore");
                continue;
            }
            let event = if intent {
                RecoveryEvent::Sync(Directory::RestoreDestinationParent)
            } else {
                RecoveryEvent::RestoreGuardRemove
            };
            let cut = Arc::new(TerminalCutIo::new(event));
            let filesystem = CutFs {
                f: &f,
                io: cut.clone(),
                limits: limits(),
            };
            assert!(terminal_call(mode, &j, &lease, &filesystem, false).is_err());
            assert_eq!(cut.fired.load(Ordering::Acquire), 1);
            let loaded =
                load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem())
                    .unwrap();
            let (expected_phase, action) = match (mode, intent) {
                (TerminalFixtureMode::Selected, true) => {
                    (RestorePhase::FinalizeIntent, "finalize_restore")
                }
                (TerminalFixtureMode::Selected, false) => {
                    (RestorePhase::Finalized, "finalize_restore")
                }
                (TerminalFixtureMode::Original, true) => {
                    (RestorePhase::FinalizeOriginalIntent, "finalize_rollback")
                }
                (TerminalFixtureMode::Original, false) => {
                    (RestorePhase::OriginalFinalized, "finalize_rollback")
                }
                (TerminalFixtureMode::Absence, false) => (
                    RestorePhase::OriginalAbsenceFinalized,
                    "finalize_original_absence",
                ),
                _ => unreachable!(),
            };
            assert_eq!(loaded.phase, expected_phase);
            review14_assert_rollback_action(&f, &loaded, &lease, action);
        }
    }
}

// Status is the recovery channel when the operator's accepted request is lost.
type Review15ArtifactSnapshot = Vec<(std::path::PathBuf, Option<((u64, u64), Vec<u8>)>)>;
fn review15_artifact_snapshot(f: &Fixture, j: &RestoreJournal) -> Review15ArtifactSnapshot {
    [
        j.destination.clone(),
        j.staging.join("candidate.fls"),
        j.staging.join("original.fls"),
        j.staging.join("journal.bin"),
        j.staging.join("journal.bin.lock"),
        j.destination.with_extension("fls.restore-guard"),
        terminal_negative_pin(f, j),
    ]
    .into_iter()
    .map(|path| {
        let value = match fs::symlink_metadata(&path) {
            Ok(m) => Some(((m.dev(), m.ino()), fs::read(&path).unwrap())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("snapshot: {e}"),
        };
        (path, value)
    })
    .collect()
}
fn review15_status(f: &Fixture, j: &RestoreJournal, lease: &OfflineLease) -> serde_json::Value {
    let before = review15_artifact_snapshot(f, j);
    let loaded_before =
        load_restore_journal(&j.staging.join("journal.bin"), lease, &f.filesystem()).unwrap();
    let status = restore_status(&j.staging.join("journal.bin"), lease, &f.filesystem()).unwrap();
    let json = serde_json::to_value(&status).unwrap();
    // Assert presence explicitly: JSON indexing alone would disguise absent fields as null.
    assert!(
        json.get("accepted_reconciliation").is_some(),
        "status omits accepted reconciliation"
    );
    assert!(
        json.get("accepted_absence_reconciliation").is_some(),
        "status omits accepted absence reconciliation"
    );
    let decoded: RestoreStatus =
        serde_json::from_slice(&serde_json::to_vec(&status).unwrap()).unwrap();
    let roundtrip = serde_json::to_value(decoded).unwrap();
    assert_eq!(roundtrip, json);
    assert_eq!(
        review15_artifact_snapshot(f, j),
        before,
        "status changed artifact bytes or identities"
    );
    let loaded_after =
        load_restore_journal(&j.staging.join("journal.bin"), lease, &f.filesystem()).unwrap();
    assert_eq!(
        terminal_negative_counters(&loaded_after),
        terminal_negative_counters(&loaded_before)
    );
    roundtrip
}
fn review15_lost_generation_request(original: bool) {
    use fl_store::recovery::readiness::{
        ReconciliationRecord, finalize_restore, finalize_rollback,
    };
    let mode = if original {
        TerminalFixtureMode::Original
    } else {
        TerminalFixtureMode::Selected
    };
    let (f, j, lease) = terminal_fixture(mode);
    let before = review15_status(&f, &j, &lease);
    assert!(before["accepted_reconciliation"].is_null());
    assert!(before["accepted_absence_reconciliation"].is_null());
    let request_path = f.root.path().join("operator-finalize-request.json");
    let evidence = generation_evidence(&j, original);
    fs::write(&request_path, serde_json::to_vec(&evidence).unwrap()).unwrap();
    let cut = Arc::new(TerminalCutIo::new(RecoveryEvent::Sync(
        Directory::RestoreDestinationParent,
    )));
    let filesystem = CutFs {
        f: &f,
        io: cut.clone(),
        limits: limits(),
    };
    let result = if original {
        finalize_rollback(&j, &lease, &evidence, &filesystem)
    } else {
        finalize_restore(&j, &lease, &evidence, &filesystem)
    };
    assert!(result.is_err());
    assert_eq!(cut.fired.load(Ordering::Acquire), 1);
    drop(evidence);
    fs::remove_file(&request_path).unwrap();
    assert!(!request_path.exists());
    let status = review15_status(&f, &j, &lease);
    let phase = if original {
        RestorePhase::FinalizeOriginalIntent
    } else {
        RestorePhase::FinalizeIntent
    };
    assert_eq!(status["phase"], serde_json::to_value(phase).unwrap());
    assert!(status["accepted_absence_reconciliation"].is_null());
    let recovered: ReconciliationRecord =
        serde_json::from_value(status["accepted_reconciliation"].clone()).unwrap();
    // Only the status roundtrip supplies evidence; the loaded journal is an operation handle.
    let intent =
        load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
    let mut changed = recovered.clone();
    changed.clients.evidence_sha256 = if changed.clients.evidence_sha256 == "a".repeat(64) {
        "b".repeat(64)
    } else {
        "a".repeat(64)
    };
    let report = if original {
        intent.original_verification.as_ref().unwrap()
    } else {
        intent.verification.as_ref().unwrap()
    };
    let dependencies = if original {
        &intent.original_dependencies
    } else {
        &intent.selected_dependencies
    };
    assert!(
        fl_store::recovery::readiness::assess_readiness(report, dependencies, &changed)
            .ordinary_admission_ready
    );
    let before = review15_artifact_snapshot(&f, &intent);
    let counters = terminal_negative_counters(&intent);
    let trace = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreGuardRemove));
    let filesystem = CutFs {
        f: &f,
        io: trace.clone(),
        limits: limits(),
    };
    let rejected = if original {
        finalize_rollback(&intent, &lease, &changed, &filesystem)
    } else {
        finalize_restore(&intent, &lease, &changed, &filesystem)
    }
    .unwrap_err();
    assert!(
        matches!(rejected, fl_store::recovery::RecoveryError::RefusedInput { reason, .. } if reason == "intent retry requires the already accepted reconciliation")
    );
    assert_eq!(review15_artifact_snapshot(&f, &intent), before);
    let unchanged =
        load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
    assert_eq!(terminal_negative_counters(&unchanged), counters);
    assert!(!trace.events.lock().unwrap().iter().any(|event| matches!(
        event,
        RecoveryEvent::Write(Member::RestoreJournal)
            | RecoveryEvent::Flush(Member::RestoreJournal)
            | RecoveryEvent::RestoreGuardRemove
            | RecoveryEvent::RestorePinRemove
    )));
    let result = if original {
        finalize_rollback(&intent, &lease, &recovered, &f.filesystem())
    } else {
        finalize_restore(&intent, &lease, &recovered, &f.filesystem())
    };
    assert!(matches!(result.unwrap(), RestoreOutcome::Finalized { .. }));
    assert!(!j.destination.with_extension("fls.restore-guard").exists());
    assert!(!terminal_negative_pin(&f, &j).exists());
}
#[test]
fn restore_review15_status_recovers_lost_selected_request() {
    review15_lost_generation_request(false);
}
#[test]
fn restore_review15_status_recovers_lost_original_request() {
    review15_lost_generation_request(true);
}
#[test]
fn restore_review15_status_recovers_lost_absence_request_for_appendless_cleanup() {
    use fl_store::recovery::readiness::{AbsenceReconciliationRecord, finalize_original_absence};
    let (f, j, lease) = terminal_fixture(TerminalFixtureMode::Absence);
    let before = review15_status(&f, &j, &lease);
    assert!(before["accepted_reconciliation"].is_null());
    assert!(before["accepted_absence_reconciliation"].is_null());
    let request_path = f.root.path().join("operator-absence-request.json");
    let evidence = AbsenceReconciliationRecord {
        version: 1,
        operation_id: j.operation_id,
        candidate_sha256: j.candidate_sha256.clone(),
        operator: fixture_lease_request(&lease).operator,
        recorded_at_unix_ms: f.filesystem().io().unix_ms().unwrap(),
        no_client_opened_path_while_installed: true,
        evidence_sha256: "a".repeat(64),
    };
    fs::write(&request_path, serde_json::to_vec(&evidence).unwrap()).unwrap();
    let cut = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreGuardRemove));
    assert!(
        finalize_original_absence(
            &j,
            &lease,
            &evidence,
            &CutFs {
                f: &f,
                io: cut.clone(),
                limits: limits()
            }
        )
        .is_err()
    );
    assert_eq!(cut.fired.load(Ordering::Acquire), 1);
    drop(evidence);
    fs::remove_file(&request_path).unwrap();
    assert!(!request_path.exists());
    let status = review15_status(&f, &j, &lease);
    assert_eq!(
        status["phase"],
        serde_json::to_value(RestorePhase::OriginalAbsenceFinalized).unwrap()
    );
    assert!(status["accepted_reconciliation"].is_null());
    let recovered: AbsenceReconciliationRecord =
        serde_json::from_value(status["accepted_absence_reconciliation"].clone()).unwrap();
    let terminal =
        load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
    let before = fs::read(j.staging.join("journal.bin")).unwrap();
    let counters = terminal_negative_counters(&terminal);
    let trace = Arc::new(TerminalCutIo::new(RecoveryEvent::RestoreInstall));
    assert!(matches!(
        finalize_original_absence(
            &terminal,
            &lease,
            &recovered,
            &CutFs {
                f: &f,
                io: trace.clone(),
                limits: limits()
            }
        )
        .unwrap(),
        RestoreOutcome::OriginalAbsenceFinalized { .. }
    ));
    assert_eq!(fs::read(j.staging.join("journal.bin")).unwrap(), before);
    let after =
        load_restore_journal(&j.staging.join("journal.bin"), &lease, &f.filesystem()).unwrap();
    assert_eq!(terminal_negative_counters(&after), counters);
    assert!(!trace.events.lock().unwrap().iter().any(|event| matches!(
        event,
        RecoveryEvent::Write(Member::RestoreJournal) | RecoveryEvent::Flush(Member::RestoreJournal)
    )));
    assert!(!j.destination.exists());
    assert!(!j.destination.with_extension("fls.restore-guard").exists());
    assert!(!terminal_negative_pin(&f, &j).exists());
}

// R4 item 2: preserved candidate evidence supplies the forward exit when the
// original cannot provide a verified rollback and the bundle is unavailable.
fn unverified_original_relocation_fixture(
    forensic: bool,
    layout: BundlelessLayout,
) -> (Fixture, RestoreJournal, OfflineLease, fs::File) {
    use std::os::unix::fs::{FileExt, PermissionsExt};
    let f = journal_fixture();
    let bundle = f.bundle();
    let original = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&f.source)
        .unwrap();
    if forensic {
        fs::set_permissions(&f.source, fs::Permissions::from_mode(0o000)).unwrap();
    } else {
        let len = original.metadata().unwrap().len();
        let junk = vec![0xa5; 1 << 20];
        // Destroy both superblock generations and every live frame without
        // changing the inode, length, or readable classification.
        let mut offset = 0;
        while offset < len {
            let count = (len - offset).min(junk.len() as u64) as usize;
            original.write_all_at(&junk[..count], offset).unwrap();
            offset += count as u64;
        }
        original.sync_all().unwrap();
    }
    let (request, lease) = prepare_restore_bundle(&f, &f.source, bundle);
    let hook = Arc::new(BundlelessInstallCut {
        layout,
        installed: AtomicBool::new(false),
        relocated: AtomicBool::new(false),
        fired: AtomicBool::new(false),
    });
    let outcome = restore_offline(
        &request,
        &lease,
        &CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        },
    );
    assert!(outcome.is_err() || matches!(outcome, Ok(RestoreOutcome::OutcomeUnknown { .. })));
    assert!(hook.fired.load(Ordering::Acquire));
    let guard: serde_json::Value =
        serde_json::from_slice(&fs::read(f.source.with_extension("fls.restore-guard")).unwrap())
            .unwrap();
    let path = std::path::Path::new(guard["journal"].as_str().unwrap());
    let j = load_restore_journal(path, &lease, &f.filesystem()).unwrap();
    assert_eq!(j.phase, RestorePhase::RelocateIntent);
    assert!(j.original_verification.is_none());
    assert!(j.original_read_error.is_some());
    assert_eq!(matches!(j.original, FileBinding::Forensic { .. }), forensic);
    assert_eq!(
        matches!(j.original, FileBinding::Readable { .. }),
        !forensic
    );
    let selected = request_bundle_path(&f, j.selected_bundle_operation);
    let candidate = fs::metadata(&j.destination).unwrap();
    assert_eq!((candidate.dev(), candidate.ino()), j.candidate_identity);
    let (retained, absent) = if matches!(layout, BundlelessLayout::Relocating) {
        ("candidate.fls", "original.fls")
    } else {
        ("original.fls", "candidate.fls")
    };
    let retained = fs::metadata(j.staging.join(retained)).unwrap();
    let before = original.metadata().unwrap();
    assert_eq!(
        (retained.dev(), retained.ino()),
        (before.dev(), before.ino())
    );
    assert!(!j.staging.join(absent).exists());
    fs::rename(&selected, selected.with_extension("unavailable-for-test")).unwrap();
    (f, j, lease, original)
}
fn assert_unverified_original_bundleless_resume(forensic: bool, layout: BundlelessLayout) {
    let (f, j, lease, original) = unverified_original_relocation_fixture(forensic, layout);
    let original_bytes = forensic_descriptor_digest(&original);
    let original_metadata = original.metadata().unwrap();
    let guard = fs::read(f.source.with_extension("fls.restore-guard")).unwrap();
    let RestoreOutcome::Installed { journal: installed } =
        resume_restore(&j, &lease, &f.filesystem()).unwrap()
    else {
        panic!("resume did not finish relocation")
    };
    assert_eq!(installed.phase, RestorePhase::Installed);
    assert_eq!(
        installed.resume_finalize_attempts,
        j.resume_finalize_attempts + 1
    );
    assert_eq!(installed.rollback_attempts, j.rollback_attempts);
    assert_eq!(installed.original, j.original);
    assert!(installed.original_verification.is_none());
    assert_eq!(forensic_descriptor_digest(&original), original_bytes);
    let retained = fs::metadata(installed.staging.join("original.fls")).unwrap();
    assert_eq!(
        (retained.dev(), retained.ino()),
        (original_metadata.dev(), original_metadata.ino())
    );
    assert_eq!(retained.mode(), original_metadata.mode());
    assert!(!installed.staging.join("candidate.fls").exists());
    let candidate = fs::metadata(&installed.destination).unwrap();
    assert_eq!((candidate.dev(), candidate.ino()), j.candidate_identity);
    assert_eq!(
        format!(
            "{:x}",
            sha2::Sha256::digest(fs::read(&installed.destination).unwrap())
        ),
        j.candidate_sha256
    );
    assert_eq!(
        fs::read(f.source.with_extension("fls.restore-guard")).unwrap(),
        guard
    );
    let loaded = load_restore_journal(
        &installed.staging.join("journal.bin"),
        &lease,
        &f.filesystem(),
    )
    .unwrap();
    assert_eq!(loaded.phase, RestorePhase::Installed);
    assert_eq!(loaded.candidate_sha256, j.candidate_sha256);
}
#[test]
fn restore_bundleless_unverified_forensic_exchanged_resume() {
    assert_unverified_original_bundleless_resume(true, BundlelessLayout::Relocating);
}
#[test]
fn restore_bundleless_unverified_forensic_installed_resume() {
    assert_unverified_original_bundleless_resume(true, BundlelessLayout::Relocated);
}
#[test]
fn restore_bundleless_unverified_readable_exchanged_resume() {
    assert_unverified_original_bundleless_resume(false, BundlelessLayout::Relocating);
}
#[test]
fn restore_bundleless_unverified_readable_installed_resume() {
    assert_unverified_original_bundleless_resume(false, BundlelessLayout::Relocated);
}
#[test]
fn restore_bundleless_unverified_corrupt_candidate_refuses_without_attempt() {
    use std::os::unix::fs::FileExt;
    for forensic in [true, false] {
        for layout in [BundlelessLayout::Relocating, BundlelessLayout::Relocated] {
            let (f, j, lease, original) = unverified_original_relocation_fixture(forensic, layout);
            let path = j.staging.join("journal.bin");
            let before = fs::read(&path).unwrap();
            let original_bytes = forensic_descriptor_digest(&original);
            let guard_path = f.source.with_extension("fls.restore-guard");
            let guard = fs::read(&guard_path).unwrap();
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&j.destination)
                .unwrap();
            let mut byte = [0];
            file.read_exact_at(&mut byte, 0).unwrap();
            byte[0] ^= 1;
            file.write_all_at(&byte, 0).unwrap();
            file.sync_all().unwrap();
            let candidate_bytes = fs::read(&j.destination).unwrap();
            assert!(resume_restore(&j, &lease, &f.filesystem()).is_err());
            // Exact journal equality proves durable counters and phase unchanged.
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(fs::read(&j.destination).unwrap(), candidate_bytes);
            assert_eq!(forensic_descriptor_digest(&original), original_bytes);
            assert_eq!(fs::read(&guard_path).unwrap(), guard);
            let retained = j
                .staging
                .join(if matches!(layout, BundlelessLayout::Relocating) {
                    "candidate.fls"
                } else {
                    "original.fls"
                });
            let metadata = fs::metadata(retained).unwrap();
            let original_metadata = original.metadata().unwrap();
            assert_eq!(
                (metadata.dev(), metadata.ino()),
                (original_metadata.dev(), original_metadata.ino())
            );
        }
    }
}

// Completed continuations must leave both budgets available for typed finalization.
fn assert_completed_continuation_preserves_finalization(mode: TerminalFixtureMode, resume: bool) {
    let (f, journal, lease) = terminal_fixture(mode);
    let path = journal.staging.join("journal.bin");
    let bytes = fs::read(&path).unwrap();
    let frames = journal_frame_ranges(&bytes).len();
    let artifacts = review15_artifact_snapshot(&f, &journal);
    let state = serde_json::to_value(&journal).unwrap();
    let io = Arc::new(AbsenceResumeIo::default());
    let filesystem = CutFs {
        f: &f,
        io: io.clone(),
        limits: limits(),
    };
    for retry in 0..20 {
        let outcome = if resume {
            resume_restore(&journal, &lease, &filesystem)
        } else {
            rollback_restore(&journal, &lease, &filesystem)
        }
        .unwrap_or_else(|e| panic!("{mode:?} resume={resume} retry={retry}: {e:?}"));
        let next = match (mode, outcome) {
            (TerminalFixtureMode::Selected, RestoreOutcome::Installed { journal })
            | (TerminalFixtureMode::Original, RestoreOutcome::RolledBack { journal })
            | (TerminalFixtureMode::Absence, RestoreOutcome::OriginalAbsenceRestored { journal }) => {
                journal
            }
            (_, other) => panic!("wrong completed outcome: {other:?}"),
        };
        assert_eq!(
            serde_json::to_value(next).unwrap(),
            state,
            "{mode:?} resume={resume} retry={retry}: changed attempts or forensic state"
        );
        let loaded = load_restore_journal(&path, &lease, &filesystem).unwrap();
        assert_eq!(serde_json::to_value(loaded).unwrap(), state);
        let current = fs::read(&path).unwrap();
        assert_eq!(journal_frame_ranges(&current).len(), frames);
        assert_eq!(current, bytes, "completed operation appended journal data");
    }
    assert_eq!(review15_artifact_snapshot(&f, &journal), artifacts);
    assert_absence_resume_read_only(&io);
    let finalized = terminal_call(mode, &journal, &lease, &f.filesystem(), false).unwrap();
    assert!(
        matches!(
            finalized,
            RestoreOutcome::Finalized { .. } | RestoreOutcome::OriginalAbsenceFinalized { .. }
        ),
        "{mode:?}: {finalized:?}"
    );
    assert!(
        !journal
            .destination
            .with_extension("fls.restore-guard")
            .exists()
    );
    assert!(!terminal_negative_pin(&f, &journal).exists());
}
#[test]
fn restore_completed_installed_resumes_preserve_finalization_budget() {
    assert_completed_continuation_preserves_finalization(TerminalFixtureMode::Selected, true);
}
#[test]
fn restore_completed_rolled_back_resumes_preserve_finalization_budget() {
    assert_completed_continuation_preserves_finalization(TerminalFixtureMode::Original, true);
}
#[test]
fn restore_completed_rolled_back_rollbacks_preserve_finalization_budget() {
    assert_completed_continuation_preserves_finalization(TerminalFixtureMode::Original, false);
}
#[test]
fn restore_completed_absence_rollbacks_preserve_finalization_budget() {
    assert_completed_continuation_preserves_finalization(TerminalFixtureMode::Absence, false);
}

#[test]
fn restore_completed_continuations_refuse_expiry_and_replaced_generation_without_mutation() {
    for (mode, resume) in [
        (TerminalFixtureMode::Selected, true),
        (TerminalFixtureMode::Original, true),
        (TerminalFixtureMode::Original, false),
        (TerminalFixtureMode::Absence, false),
    ] {
        let (f, journal, lease) = terminal_fixture(mode);
        let before = review15_artifact_snapshot(&f, &journal);
        let io = Arc::new(AbsenceResumeIo {
            now: Some(journal.binding.expires_at_unix_ms + 1),
            ..Default::default()
        });
        let filesystem = CutFs {
            f: &f,
            io: io.clone(),
            limits: limits(),
        };
        let result = if resume {
            resume_restore(&journal, &lease, &filesystem)
        } else {
            rollback_restore(&journal, &lease, &filesystem)
        };
        assert!(
            result.is_err(),
            "{mode:?} resume={resume}: accepted expired lease"
        );
        assert_eq!(review15_artifact_snapshot(&f, &journal), before);
        assert_absence_resume_read_only(&io);

        // Preserve identical bytes but replace the retained candidate's inode.
        // Completed outcomes must still enforce the admitted generation binding.
        let candidate = match mode {
            TerminalFixtureMode::Selected => journal.destination.clone(),
            TerminalFixtureMode::Original => journal.staging.join("original.fls"),
            TerminalFixtureMode::Absence => journal.staging.join("candidate.fls"),
            _ => unreachable!(),
        };
        let parked = journal.staging.join("retained-candidate-for-test.fls");
        fs::rename(&candidate, &parked).unwrap();
        fs::copy(&parked, &candidate).unwrap();
        assert_ne!(
            fs::metadata(&candidate).unwrap().ino(),
            fs::metadata(&parked).unwrap().ino()
        );
        let changed = review15_artifact_snapshot(&f, &journal);
        let io = Arc::new(AbsenceResumeIo::default());
        let filesystem = CutFs {
            f: &f,
            io: io.clone(),
            limits: limits(),
        };
        let result = if resume {
            resume_restore(&journal, &lease, &filesystem)
        } else {
            rollback_restore(&journal, &lease, &filesystem)
        };
        assert!(
            result.is_err(),
            "{mode:?} resume={resume}: accepted replaced generation"
        );
        assert_eq!(review15_artifact_snapshot(&f, &journal), changed);
        assert_absence_resume_read_only(&io);
        fs::remove_file(&candidate).unwrap();
        fs::rename(parked, candidate).unwrap();
        assert_eq!(review15_artifact_snapshot(&f, &journal), before);
    }
}

// JSON quotes and escaping count toward both accepting-field allowances.
fn reservation_encoded_string(prefix: &str, bytes: u64) -> String {
    let mut value = prefix.to_owned();
    let remaining = bytes.checked_sub(reservation_json_bytes(&value)).unwrap();
    value.push_str(&"\u{1}".repeat((remaining / 6) as usize));
    value.push_str(&"x".repeat((remaining % 6) as usize));
    assert_eq!(reservation_json_bytes(&value), bytes);
    value
}
fn reservation_call_with_operator(
    mode: TerminalFixtureMode,
    j: &RestoreJournal,
    lease: &OfflineLease,
    f: &Fixture,
) -> Result<RestoreOutcome, fl_store::recovery::RecoveryError> {
    use fl_store::recovery::readiness::*;
    if matches!(
        mode,
        TerminalFixtureMode::Selected | TerminalFixtureMode::Original
    ) {
        let mut record = generation_evidence(j, matches!(mode, TerminalFixtureMode::Original));
        record.operator = fixture_lease_request(lease).operator;
        record.recorded_at_unix_ms = f.filesystem().io().unix_ms().unwrap();
        if matches!(mode, TerminalFixtureMode::Original) {
            finalize_rollback(j, lease, &record, &f.filesystem())
        } else {
            finalize_restore(j, lease, &record, &f.filesystem())
        }
    } else {
        terminal_call(mode, j, lease, &f.filesystem(), false)
    }
}
#[test]
fn restore_terminal_reservation_fixed_accepting_allowances_all_modes() {
    for mode in [
        TerminalFixtureMode::Abandon,
        TerminalFixtureMode::Selected,
        TerminalFixtureMode::Original,
        TerminalFixtureMode::Absence,
    ] {
        let (f, journal, lease) = terminal_fixture(mode);
        let mut binding = fixture_lease_request(&lease);
        binding.expected_file = inspect_file_binding(&journal.destination, &limits()).unwrap();
        binding.operator = reservation_encoded_string("second shift operator ", 128);
        binding.exclusion.evidence =
            reservation_encoded_string("renewed exclusion ticket 12345 ", 256);
        assert!(binding.operator.len() > journal.binding.operator.len());
        assert!(
            reservation_json_bytes(&binding.exclusion.evidence)
                > reservation_json_bytes(&journal.binding.exclusion.evidence) + 100
        );
        fs::remove_file(lease.path()).unwrap();
        let current = create_offline_lease(&binding, &f.filesystem(), &limits()).unwrap();
        reservation_call_with_operator(mode, &journal, &current, &f)
            .unwrap_or_else(|error| panic!("{mode:?} fixed accepting allowance: {error}"));
        let path = journal.staging.join("journal.bin");
        let done = load_restore_journal(&path, &current, &f.filesystem()).unwrap();
        let context = done.terminal_evidence_context.as_ref().unwrap();
        assert_eq!(context.accepting_lease.operator, binding.operator);
        assert_eq!(
            context.accepting_lease.exclusion.evidence,
            binding.exclusion.evidence
        );
        let bytes = fs::read(&path).unwrap();
        assert!(
            journal_frame_ranges(&bytes)
                .iter()
                .all(|frame| (frame.end - frame.start - 72) as u64
                    <= journal.journal_capacity / 96 - 72)
        );
        assert!(
            !journal
                .destination
                .with_extension("fls.restore-guard")
                .exists()
        );
        eprintln!("fixed accepting encoded O=128/X=256 complete {mode:?}");
    }
}

#[test]
fn restore_terminal_reservation_default_original_dependency_budget_refuses() {
    use fl_store::recovery::{
        profiles::{DependencyPolicy, ReferenceDependency},
        readiness::{DependencyResolution, IndexReadiness},
    };
    for provisioned in [true, false] {
        let f = Fixture::new();
        let (mut request, lease) = prepare_restore(&f, &f.source);
        // Keep the refusal away from inode/device decimal-width boundaries.
        let catalog_identity = "reservation-dependency-".to_owned() + &"x".repeat(64);
        request.original_profile.dependencies = vec![ReferenceDependency {
            reference_sha256: "a".repeat(64),
            catalog_identity: catalog_identity.clone(),
            required_sequence: Some(17),
            policy: DependencyPolicy::ReconcileBeforeAdmission,
        }];
        request.available_dependencies = vec![DependencyResolution {
            reference_sha256: "a".repeat(64),
            catalog_identity,
            observed_sequence: 17,
            evidence_sha256: "e".repeat(64),
        }];
        assert_eq!(request.metadata_byte_limit, 1 << 20);
        if provisioned {
            request.metadata_byte_limit += 64 << 10;
            let RestoreOutcome::Installed { journal } =
                restore_offline(&request, &lease, &f.filesystem()).unwrap()
            else {
                panic!("not installed")
            };
            let (_, _, provisioned_e) = reservation_expected_caps(&journal);
            let mut default = journal.clone();
            default.journal_capacity -= 64 << 10;
            default.budget.metadata_payload_ceiling -= 64 << 10;
            let (_, _, default_e) = reservation_expected_caps(&default);
            let mut maximum = generation_evidence(&journal, true);
            maximum.dependencies = request.available_dependencies.clone();
            maximum.operator = "x".repeat(126);
            maximum.recorded_at_unix_ms = u128::MAX;
            maximum.affected_scopes = vec![journal.original_profile.catalog_identity.clone()];
            maximum.index = IndexReadiness::Degraded;
            let required = reservation_json_bytes(&maximum);
            assert!(required <= provisioned_e);
            assert!(
                required >= default_e + 128,
                "default E={default_e} required={required}"
            );
            let admission_o = reservation_json_bytes(&journal.binding.operator);
            let admission_x = reservation_json_bytes(&journal.binding.exclusion.evidence);
            let old_e = default_e + (128 - admission_o) + (256 - admission_x);
            let old_required = required - (128 - admission_o);
            assert!(
                old_e >= old_required + 128,
                "old E={old_e} required={old_required}"
            );
            assert!(
                journal.budget.candidate_bytes
                    + journal.budget.rollback_new_bytes
                    + journal.budget.verifier_scratch_bytes
                    + journal.budget.metadata_bytes
                    <= journal.budget.allocation_ceiling
            );
            eprintln!(
                "dependency64 oracle defaultP={} E={default_e} required={required}; oldE={old_e} oldrequired={old_required}; provisionedE={provisioned_e}",
                default.journal_capacity / 96 - 72
            );
        } else {
            reservation_refuses_without_artifacts(&f, &request, &lease);
            eprintln!(
                "default metadata=1048576 original dependency64 refuses fixed future reservation before guard"
            );
        }
    }
}

struct CorruptUnpublishedPinWrite {
    armed: AtomicBool,
    corrupted: Mutex<Option<Vec<u8>>>,
}
impl RecoveryIo for CorruptUnpublishedPinWrite {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        if event == RecoveryEvent::Write(Member::RestorePin) {
            self.armed.store(true, Ordering::Release);
        }
        Ok(())
    }
    fn write_at(&self, file: &fs::File, bytes: &[u8], offset: u64) -> std::io::Result<usize> {
        use std::os::unix::fs::FileExt;
        if self.armed.swap(false, Ordering::AcqRel) {
            let mut corrupted = bytes.to_vec();
            corrupted[0] ^= 0x20;
            *self.corrupted.lock().unwrap() = Some(corrupted.clone());
            file.write_at(&corrupted, offset)
        } else {
            file.write_at(bytes, offset)
        }
    }
}
#[test]
fn restore_pin_publication_checks_actual_temp_hash_before_publish() {
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let source_before = fs::read(&f.source).unwrap();
    let manifest = request.selected_bundle.path().join("manifest.json");
    let manifest_before = fs::read(&manifest).unwrap();
    let hook = Arc::new(CorruptUnpublishedPinWrite {
        armed: AtomicBool::new(false),
        corrupted: Mutex::new(None),
    });
    let error = restore_offline(
        &request,
        &lease,
        &CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        },
    )
    .unwrap_err();
    let fl_store::recovery::RecoveryError::RestoreAdmissionArtifactsRetained { staging, .. } =
        error
    else {
        panic!(
            "corrupt private pin must refuse before publication with retained artifacts: {error:?}"
        )
    };
    let corrupted = hook
        .corrupted
        .lock()
        .unwrap()
        .clone()
        .expect("pin corruption hook fired");
    let pins = f.backups.join(".catalog/restore-pins");
    assert!(
        !pins.join(format!("{}.json", request.operation_id)).exists(),
        "corrupt private pin must never acquire the final published name"
    );
    let temporary = pins.join(format!("{}.json.tmp", request.operation_id));
    assert_eq!(
        fs::read(&temporary).unwrap(),
        corrupted,
        "retain exact corrupted unpublished bytes"
    );
    assert!(staging.join("journal.bin").is_file());
    assert!(staging.join("journal.bin.lock").is_file());
    assert!(!f.source.with_extension("fls.restore-guard").exists());
    assert_eq!(fs::read(&f.source).unwrap(), source_before);
    assert_eq!(fs::read(&manifest).unwrap(), manifest_before);
}

#[test]
fn restore_initial_lease_cap_stops_frozen_wall_clock_namespace_stall() {
    struct FrozenStall {
        now: u128,
        destination: std::path::PathBuf,
        before_layout: Mutex<Option<(std::path::PathBuf, RestoreExpiryLayout)>>,
        stalled: AtomicBool,
        later_actions: AtomicUsize,
    }
    impl RecoveryIo for FrozenStall {
        fn unix_ms(&self) -> std::io::Result<u128> {
            Ok(self.now)
        }
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if matches!(
                event,
                RecoveryEvent::RestoreInstall | RecoveryEvent::RestoreRelocate
            ) {
                if self.stalled.swap(true, Ordering::AcqRel) {
                    self.later_actions.fetch_add(1, Ordering::AcqRel);
                } else {
                    let guard: serde_json::Value = serde_json::from_slice(&fs::read(
                        self.destination.with_extension("fls.restore-guard"),
                    )?)?;
                    let staging = std::path::Path::new(guard["journal"].as_str().unwrap())
                        .parent()
                        .unwrap()
                        .to_path_buf();
                    *self.before_layout.lock().unwrap() = Some((
                        staging.clone(),
                        restore_expiry_layout(&self.destination, &staging),
                    ));
                    std::thread::sleep(std::time::Duration::from_secs(6));
                }
            }
            Ok(())
        }
    }
    let f = Fixture::new();
    let (request, lease) = prepare_restore(&f, &f.source);
    let before = fs::read(&f.source).unwrap();
    let before_identity = fs::metadata(&f.source).unwrap();
    let binding = fixture_lease_request(&lease);
    let hook = Arc::new(FrozenStall {
        now: binding.expires_at_unix_ms - 4000,
        destination: f.source.clone(),
        before_layout: Mutex::new(None),
        stalled: AtomicBool::new(false),
        later_actions: AtomicUsize::new(0),
    });
    let result = restore_offline(
        &request,
        &lease,
        &CutFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        },
    );
    // A slow runner may legitimately exhaust the cap before the action seam.
    // In either case the namespace must remain untouched. Without the initial
    // cap the frozen wall clock permits the install and this assertion fails.
    let after_identity = fs::metadata(&f.source).unwrap();
    assert_eq!(
        (after_identity.dev(), after_identity.ino()),
        (before_identity.dev(), before_identity.ino()),
        "initial monotonic cap must prevent namespace replacement under frozen wall clock"
    );
    assert_eq!(
        sha2::Sha256::digest(fs::read(&f.source).unwrap()),
        sha2::Sha256::digest(&before),
        "initial monotonic cap must preserve source bytes"
    );
    if let Some((staging, layout)) = hook.before_layout.lock().unwrap().as_ref() {
        assert_eq!(
            restore_expiry_layout(&f.source, staging),
            *layout,
            "initial monotonic cap must stop namespace action after real-time stall"
        );
    }
    assert_eq!(hook.later_actions.load(Ordering::Acquire), 0);
    assert!(
        result.is_err(),
        "initial monotonic cap must refuse expired real-time authority: {result:?}"
    );
    eprintln!(
        "frozen_wall_stall_reached={} namespace_preserved=true",
        hook.stalled.load(Ordering::Acquire)
    );
}
