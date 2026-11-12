//! Purge tests use private native-filesystem fixtures only.
#![cfg(target_os = "linux")]
mod recovery_task6_support;
use fl_store::recovery::{admission::*, manifest::BundleManifestV1, purge::*};
use recovery_task6_support::*;
use std::{fs, os::unix::fs::PermissionsExt};
#[test]
fn purge_defaults_to_exact_dry_run_and_requires_matching_confirmation() {
    let f = Fixture::new();
    let bundle = f.bundle();
    let op = uuid::Uuid::parse_str(bundle.path().file_name().unwrap().to_str().unwrap()).unwrap();
    let manifest: BundleManifestV1 =
        serde_json::from_slice(&fs::read(bundle.path().join("manifest.json")).unwrap()).unwrap();
    use sha2::{Digest, Sha256};
    let manifest_sha256 = format!(
        "{:x}",
        Sha256::digest(fs::read(bundle.path().join("manifest.json")).unwrap())
    );
    let entry = f.backups.join(".quarantine").join(op.to_string());
    fs::create_dir(&entry).unwrap();
    fs::set_permissions(&entry, fs::Permissions::from_mode(0o700)).unwrap();
    // An isolated interrupted quarantine: receipt withdrawal has not run yet.
    let lease_req = f.lease_request(
        OfflineAction::Purge {
            quarantined_operation: op,
            manifest_sha256,
        },
        op,
        manifest.validation_profile_sha256,
        &f.source,
    );
    let lease = create_offline_lease(&lease_req, &f.filesystem(), &limits()).unwrap();
    let request = PurgeRequest {
        operation_id: op,
        confirm: None,
        reference_closure: None,
    };
    let before = fs::read(f.backups.join(".catalog").join(format!("{op}.json"))).unwrap();
    let report = purge_quarantined(&request, &lease, &f.filesystem(), &limits()).unwrap();
    assert!(report.pending_receipt_withdrawal);
    assert!(!report.audit_durable);
    assert!(!report.complete);
    assert!(
        report
            .members
            .iter()
            .any(|m| m.relative_name == format!("{op}/image.fls")
                && m.bytes > 0
                && m.sha256.len() == 64)
    );
    assert_eq!(
        fs::read(f.backups.join(".catalog").join(format!("{op}.json"))).unwrap(),
        before
    );
    assert!(fs::read_dir(&entry).unwrap().next().is_none());
    let mut bad = request.clone();
    bad.confirm = Some(uuid::Uuid::now_v7());
    assert!(purge_quarantined(&bad, &lease, &f.filesystem(), &limits()).is_err());
    let mut confirm = request;
    confirm.confirm = Some(op);
    assert!(purge_quarantined(&confirm, &lease, &f.filesystem(), &limits()).is_err());
    confirm.reference_closure = Some(ReferenceClosureEvidence {
        catalog_set_sha256: report.catalog_set_sha256,
        non_ferroloop_writers_excluded: true,
        evidence_sha256: "b".repeat(64),
    });
    let done = purge_quarantined(&confirm, &lease, &f.filesystem(), &limits()).unwrap();
    assert!(done.complete && done.audit_durable);
    assert!(
        done.members
            .iter()
            .all(|m| m.state == PurgeMemberState::Removed)
    );
    assert!(f.source.exists());
    assert!(!bundle.path().join("image.fls").exists());
    assert!(entry.is_dir());
}
use fl_store::recovery::{
    OperationLimits,
    fs::{Directory, RecoveryEvent, RecoveryFs, RecoveryIo},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
struct HookFs<'a> {
    f: &'a Fixture,
    io: Arc<dyn RecoveryIo>,
    limits: OperationLimits,
}
impl RecoveryFs for HookFs<'_> {
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
fn quarantined(f: &Fixture) -> (uuid::Uuid, OfflineLease) {
    let bundle = f.bundle();
    let op = uuid::Uuid::parse_str(bundle.path().file_name().unwrap().to_str().unwrap()).unwrap();
    let manifest: BundleManifestV1 =
        serde_json::from_slice(&fs::read(bundle.path().join("manifest.json")).unwrap()).unwrap();
    use sha2::{Digest, Sha256};
    let manifest_sha256 = format!(
        "{:x}",
        Sha256::digest(fs::read(bundle.path().join("manifest.json")).unwrap())
    );
    let entry = f.backups.join(".quarantine").join(op.to_string());
    fs::create_dir(&entry).unwrap();
    fs::set_permissions(&entry, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(
        f.backups.join(".catalog").join(format!("{op}.json")),
        entry.join("receipt.json"),
    )
    .unwrap();
    let request = f.lease_request(
        OfflineAction::Purge {
            quarantined_operation: op,
            manifest_sha256,
        },
        op,
        manifest.validation_profile_sha256,
        &f.source,
    );
    (
        op,
        create_offline_lease(&request, &f.filesystem(), &limits()).unwrap(),
    )
}
fn inspection(op: uuid::Uuid) -> PurgeRequest {
    PurgeRequest {
        operation_id: op,
        confirm: None,
        reference_closure: None,
    }
}
fn confirmation(report: &PurgeReport) -> PurgeRequest {
    PurgeRequest {
        operation_id: report.operation_id,
        confirm: Some(report.operation_id),
        reference_closure: Some(ReferenceClosureEvidence {
            catalog_set_sha256: report.catalog_set_sha256.clone(),
            non_ferroloop_writers_excluded: true,
            evidence_sha256: "d".repeat(64),
        }),
    }
}
#[test]
fn purge_refuses_live_active_referenced_or_replaced_material() {
    let f = Fixture::new();
    let (op, lease) = quarantined(&f);
    let request = inspection(op);
    // A completed batch is a live material reference even after its member's
    // receipt has been withdrawn. No scheduler history scanning substitutes for it.
    let receipt: fl_store::recovery::catalog::PublicationReceiptV1 = serde_json::from_slice(
        &fs::read(
            f.backups
                .join(".quarantine")
                .join(op.to_string())
                .join("receipt.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let manifest: BundleManifestV1 = serde_json::from_slice(
        &fs::read(f.backups.join(op.to_string()).join("manifest.json")).unwrap(),
    )
    .unwrap();
    let batch = uuid::Uuid::now_v7();
    let batch_path = f.backups.join(format!(".batch-{batch}.json"));
    let record = fl_store::recovery::control::DurableBatchV1 {
        version: 1,
        batch_id: batch,
        members: vec![fl_store::recovery::control::DurableBatchMember {
            operation_id: op,
            catalog_identity: manifest.catalog_store_identity,
            sequence: manifest.published_sequence,
            manifest_sha256: receipt.manifest_sha256,
            shared_config: None,
        }],
    };
    fs::write(&batch_path, serde_json::to_vec(&record).unwrap()).unwrap();
    fs::set_permissions(&batch_path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(purge_quarantined(&request, &lease, &f.filesystem(), &limits()).is_err());
    fs::remove_file(batch_path).unwrap();
    let report = purge_quarantined(&request, &lease, &f.filesystem(), &limits()).unwrap();
    let mut changed = confirmation(&report);
    changed
        .reference_closure
        .as_mut()
        .unwrap()
        .catalog_set_sha256 = "0".repeat(64);
    assert!(purge_quarantined(&changed, &lease, &f.filesystem(), &limits()).is_err());
    let original = f.backups.join(op.to_string()).join("image.fls");
    let saved = f.root.path().join("saved.fls");
    fs::rename(&original, &saved).unwrap();
    std::os::unix::fs::symlink(&saved, &original).unwrap();
    assert!(purge_quarantined(&confirmation(&report), &lease, &f.filesystem(), &limits()).is_err());
    assert!(saved.exists());
}
#[test]
fn purge_audit_is_durable_before_every_unlink() {
    let f = Fixture::new();
    let (op, lease) = quarantined(&f);
    let report = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    struct AuditHook {
        root: std::path::PathBuf,
        op: uuid::Uuid,
        seen: AtomicBool,
    }
    impl RecoveryIo for AuditHook {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::PurgeUnlink {
                let path = self
                    .root
                    .join(".catalog")
                    .join(format!("purge-{}.intent.json", self.op));
                assert!(path.is_file());
                let audit: serde_json::Value =
                    serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
                assert_eq!(audit["operation_id"], self.op.to_string());
                assert!(
                    audit["members"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|m| m["sha256"].as_str().unwrap().len() == 64)
                );
                self.seen.store(true, Ordering::Release);
            }
            Ok(())
        }
    }
    let hook = Arc::new(AuditHook {
        root: f.backups.clone(),
        op,
        seen: AtomicBool::new(false),
    });
    let filesystem = HookFs {
        f: &f,
        io: hook.clone(),
        limits: limits(),
    };
    let done = purge_quarantined(&confirmation(&report), &lease, &filesystem, &limits()).unwrap();
    assert!(done.complete && hook.seen.load(Ordering::Acquire));
    let after = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    assert!(after.complete);
    assert!(
        after
            .members
            .iter()
            .all(|m| m.state == PurgeMemberState::Removed)
    );
}
#[test]
fn purge_resumes_interrupted_unlink_from_same_audited_identity_set() {
    let f = Fixture::new();
    let (op, lease) = quarantined(&f);
    let report = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    struct Cut(AtomicBool);
    impl RecoveryIo for Cut {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::Sync(Directory::PurgeParent)
                && !self.0.swap(true, Ordering::AcqRel)
            {
                return Err(std::io::Error::other("cut after unlink"));
            }
            Ok(())
        }
    }
    let filesystem = HookFs {
        f: &f,
        io: Arc::new(Cut(AtomicBool::new(false))),
        limits: limits(),
    };
    assert!(purge_quarantined(&confirmation(&report), &lease, &filesystem, &limits()).is_err());
    let interrupted =
        purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    assert!(
        interrupted
            .members
            .iter()
            .any(|m| m.state == PurgeMemberState::OutcomeUnknown)
    );
    assert!(!interrupted.complete);
    let done = purge_quarantined(
        &confirmation(&interrupted),
        &lease,
        &f.filesystem(),
        &limits(),
    )
    .unwrap();
    assert!(done.complete);
    assert!(f.source.exists());
}
#[test]
fn purge_inspection_requires_existing_stable_catalog_lock_and_never_bootstraps_it() {
    let f = Fixture::new();
    let (op, lease) = quarantined(&f);
    let lock = f.backups.join(".locks/catalog.lock");
    fs::rename(&lock, f.root.path().join("held-catalog-lock")).unwrap();
    let error = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap_err();
    assert!(error.to_string().contains("bootstrap"));
    assert!(!lock.exists());
    assert!(f.backups.join(op.to_string()).join("image.fls").exists());
}
#[test]
fn purge_refuses_shared_publication_lock_and_publication_waits_for_entire_purge() {
    use std::{
        os::fd::AsRawFd,
        sync::{Mutex, mpsc},
        time::Duration,
    };
    let f = Fixture::new();
    let (op, lease) = quarantined(&f);
    let report = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    let shared = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.backups.join(".locks/catalog.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(shared.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        0
    );
    struct CancelWait(fl_store::recovery::CancelToken);
    impl RecoveryIo for CancelWait {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::CatalogLockWait {
                self.0.cancel();
            }
            Ok(())
        }
    }
    let l = limits();
    let filesystem = HookFs {
        f: &f,
        io: Arc::new(CancelWait(l.cancel.clone())),
        limits: l.clone(),
    };
    assert!(matches!(
        purge_quarantined(&confirmation(&report), &lease, &filesystem, &l),
        Err(fl_store::recovery::RecoveryError::Cancelled { .. })
    ));
    drop(shared);
    let g = Fixture::new();
    let mut publication = g.backup_request();
    publication.destination = f.backups.clone();
    let published_op = publication.operation_id;
    let (paused_tx, paused_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (wait_tx, wait_rx) = mpsc::channel();
    struct Pause {
        sent: AtomicBool,
        paused: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl RecoveryIo for Pause {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::PurgeUnlink && !self.sent.swap(true, Ordering::AcqRel) {
                self.paused.send(()).map_err(std::io::Error::other)?;
                self.release
                    .lock()
                    .unwrap()
                    .recv()
                    .map_err(std::io::Error::other)?;
            }
            Ok(())
        }
    }
    struct Wait {
        sent: AtomicBool,
        waiting: mpsc::Sender<()>,
    }
    impl RecoveryIo for Wait {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::CatalogLockWait && !self.sent.swap(true, Ordering::AcqRel) {
                self.waiting.send(()).map_err(std::io::Error::other)?;
            }
            Ok(())
        }
    }
    std::thread::scope(|scope| {
        let purge_io = Arc::new(Pause {
            sent: AtomicBool::new(false),
            paused: paused_tx,
            release: Mutex::new(release_rx),
        });
        let purge_fs = HookFs {
            f: &f,
            io: purge_io,
            limits: limits(),
        };
        let confirm = confirmation(&report);
        let lease_ref = &lease;
        let purger =
            scope.spawn(move || purge_quarantined(&confirm, lease_ref, &purge_fs, &limits()));
        paused_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let pub_fs = HookFs {
            f: &f,
            io: Arc::new(Wait {
                sent: AtomicBool::new(false),
                waiting: wait_tx,
            }),
            limits: limits(),
        };
        let source = &g.source;
        let publisher = scope.spawn(move || {
            let owner = fl_store::FerroStore::open(source).unwrap();
            fl_store::recovery::backup::backup(
                &owner,
                &publication,
                &pub_fs,
                &fl_store::recovery::CancelToken::new(),
            )
        });
        wait_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        assert!(
            !f.backups
                .join(".catalog")
                .join(format!("{published_op}.json"))
                .exists()
        );
        release_tx.send(()).unwrap();
        assert!(purger.join().unwrap().unwrap().complete);
        assert!(matches!(
            publisher.join().unwrap().unwrap(),
            fl_store::recovery::backup::PublicationOutcome::Durable(_)
        ));
    });
    assert!(
        f.backups
            .join(".catalog")
            .join(format!("{published_op}.json"))
            .exists()
    );
}

fn empty_fixture() -> Fixture {
    use recovery_task6_support::test_support as support;
    let root = support::fixture_root();
    let source = root.path().join("source.fls");
    let backups = root.path().join("backups");
    fs::create_dir(&backups).unwrap();
    fs::set_permissions(&backups, fs::Permissions::from_mode(0o700)).unwrap();
    drop(fl_store::FerroStore::open(&source).unwrap());
    Fixture {
        root,
        source,
        backups,
    }
}
#[test]
fn purge_every_audit_write_sync_unlink_cut_preserves_other_material() {
    use fl_store::recovery::fs::Member;
    use std::sync::{Mutex, atomic::AtomicUsize};
    struct Cut {
        at: usize,
        count: AtomicUsize,
        events: Mutex<Vec<RecoveryEvent>>,
        errno: i32,
    }
    fn boundary(e: RecoveryEvent) -> bool {
        matches!(
            e,
            RecoveryEvent::Create(_)
                | RecoveryEvent::Write(_)
                | RecoveryEvent::Flush(_)
                | RecoveryEvent::Sync(_)
                | RecoveryEvent::PurgeUnlink
                | RecoveryEvent::RenameEntry
                | RecoveryEvent::CatalogPublish
                | RecoveryEvent::CreateDirectory
        )
    }
    impl RecoveryIo for Cut {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if boundary(e) {
                self.events.lock().unwrap().push(e);
                if self.count.fetch_add(1, Ordering::AcqRel) == self.at {
                    return Err(std::io::Error::from_raw_os_error(self.errno));
                }
            }
            Ok(())
        }
    }
    let trace = empty_fixture();
    let (op, lease) = quarantined(&trace);
    let report =
        purge_quarantined(&inspection(op), &lease, &trace.filesystem(), &limits()).unwrap();
    let hooks = Arc::new(Cut {
        at: usize::MAX,
        count: AtomicUsize::new(0),
        events: Mutex::new(vec![]),
        errno: libc::EIO,
    });
    let filesystem = HookFs {
        f: &trace,
        io: hooks.clone(),
        limits: limits(),
    };
    assert!(
        purge_quarantined(&confirmation(&report), &lease, &filesystem, &limits())
            .unwrap()
            .complete
    );
    let events = hooks.events.lock().unwrap().clone();
    assert!(events.contains(&RecoveryEvent::Write(Member::PurgeAudit)));
    assert!(events.contains(&RecoveryEvent::Flush(Member::PurgeAudit)));
    assert!(events.contains(&RecoveryEvent::Sync(Directory::CatalogParent)));
    assert!(events.contains(&RecoveryEvent::Sync(Directory::PurgeParent)));
    assert!(events.contains(&RecoveryEvent::PurgeUnlink));
    let unlink = events
        .iter()
        .position(|e| *e == RecoveryEvent::PurgeUnlink)
        .unwrap();
    let write = events[..unlink]
        .iter()
        .rposition(|e| *e == RecoveryEvent::Write(Member::PurgeAudit))
        .unwrap();
    let flush = events[write + 1..unlink]
        .iter()
        .position(|e| *e == RecoveryEvent::Flush(Member::PurgeAudit))
        .unwrap()
        + write
        + 1;
    assert!(
        events[flush + 1..unlink].contains(&RecoveryEvent::Sync(Directory::CatalogParent)),
        "audit write, file flush, and catalog sync must precede every unlink"
    );

    for (cut, event) in events.iter().enumerate() {
        let f = empty_fixture();
        let (op, lease) = quarantined(&f);
        let report =
            purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
        let source = fs::read(&f.source).unwrap();
        let foreign = f.root.path().join("unrelated-history.redb");
        fs::write(&foreign, b"retain unrelated bytes").unwrap();
        let errno = match cut % 3 {
            0 => libc::EIO,
            1 => libc::ENOSPC,
            _ => libc::EDQUOT,
        };
        let hook = Arc::new(Cut {
            at: cut,
            count: AtomicUsize::new(0),
            events: Mutex::new(vec![]),
            errno,
        });
        let filesystem = HookFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        let result = purge_quarantined(&confirmation(&report), &lease, &filesystem, &limits());
        assert!(
            result.is_err(),
            "cut {cut} {event:?} unexpectedly completed"
        );
        assert!(hook.count.load(Ordering::Acquire) > cut);
        let audit = f
            .backups
            .join(".catalog")
            .join(format!("purge-{op}.intent.json"));
        let valid_audit = fs::read(audit)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some();
        if !valid_audit {
            for member in &report.members {
                assert!(
                    f.backups.join(&member.relative_name).exists(),
                    "deleted before readable audit at {cut} {event:?}"
                );
            }
        }
        let inspected = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits());
        let mut recovered = false;
        if let Ok(report) = inspected
            && let Ok(done) =
                purge_quarantined(&confirmation(&report), &lease, &f.filesystem(), &limits())
        {
            assert!(done.complete);
            recovered = true;
        }
        assert_eq!(fs::read(&f.source).unwrap(), source);
        assert_eq!(fs::read(foreign).unwrap(), b"retain unrelated bytes");
        eprintln!("purge_cut index={cut} event={event:?} errno={errno} recovered={recovered}");
    }
}

#[test]
fn purge_large_valid_catalog_accounts_retained_evidence_with_scan_scratch() {
    use fl_store::recovery::{backup::*, catalog::PublicationReceiptV1, control::*};
    let f = Fixture::new();
    let (op, lease) = quarantined(&f);
    let other = Fixture::new();
    let owner = fl_store::FerroStore::open(&other.source).unwrap();
    owner
        .put_doc(
            "fixture_unknown",
            "different",
            &serde_json::json!({"fixture":"different contents"}),
        )
        .unwrap();
    let mut request = other.backup_request();
    request.destination = f.backups.clone();
    let PublicationOutcome::Durable(bundle) = backup(
        &owner,
        &request,
        &f.filesystem(),
        &fl_store::recovery::CancelToken::new(),
    )
    .unwrap() else {
        panic!("not durable")
    };
    drop(owner);
    let other_op =
        uuid::Uuid::parse_str(bundle.path().file_name().unwrap().to_str().unwrap()).unwrap();
    let receipt: PublicationReceiptV1 = serde_json::from_slice(
        &fs::read(f.backups.join(".catalog").join(format!("{other_op}.json"))).unwrap(),
    )
    .unwrap();
    let manifest: BundleManifestV1 =
        serde_json::from_slice(&fs::read(bundle.path().join("manifest.json")).unwrap()).unwrap();
    let baseline = limits();
    purge_quarantined(&inspection(op), &lease, &f.filesystem(), &baseline).unwrap();
    let scan_peak = baseline.admitted_memory_bytes();
    for _ in 0..40 {
        let batch_id = uuid::Uuid::now_v7();
        let record = DurableBatchV1 {
            version: 1,
            batch_id,
            members: vec![DurableBatchMember {
                operation_id: other_op,
                catalog_identity: manifest.catalog_store_identity.clone(),
                sequence: manifest.published_sequence,
                manifest_sha256: receipt.manifest_sha256.clone(),
                shared_config: None,
            }],
        };
        let path = f.backups.join(format!(".batch-{batch_id}.json"));
        fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let generous = limits();
    purge_quarantined(&inspection(op), &lease, &f.filesystem(), &generous).unwrap();
    let mut bounded = limits();
    bounded.memory_byte_limit = scan_peak + 4096;
    assert!(
        matches!(
            purge_quarantined(&inspection(op), &lease, &f.filesystem(), &bounded),
            Err(fl_store::recovery::RecoveryError::Quota {
                resource: "working memory",
                ..
            })
        ),
        "retained catalog evidence and per-bundle/batch scratch must share one memory ceiling (scan peak {scan_peak})"
    );
    assert!(bounded.admitted_memory_bytes() <= bounded.memory_byte_limit);
    assert!(f.backups.join(op.to_string()).join("image.fls").exists());
    assert!(
        fs::read_dir(f.backups.join(".quarantine").join(op.to_string()))
            .unwrap()
            .all(|e| e.unwrap().file_name() == "receipt.json")
    );
}

#[test]
fn purge_identical_independent_backups_are_not_references() {
    let f = Fixture::new();
    let (op, lease) = quarantined(&f);
    let other = f.bundle();
    let original_image = fs::read(f.backups.join(op.to_string()).join("image.fls")).unwrap();
    assert_eq!(
        fs::read(other.path().join("image.fls")).unwrap(),
        original_image
    );
    let report = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    assert!(
        purge_quarantined(&confirmation(&report), &lease, &f.filesystem(), &limits())
            .unwrap()
            .complete
    );
    assert!(other.path().join("image.fls").exists());
}

#[test]
fn purge_refuses_explicit_dependency_from_live_receipt_manifest() {
    use fl_store::recovery::{
        backup::*,
        profiles::{DependencyPolicy, ReferenceDependency},
    };
    let f = Fixture::new();
    let (op, lease) = quarantined(&f);
    let mut request = f.backup_request();
    request.profile.dependencies.push(ReferenceDependency {
        reference_sha256: "e".repeat(64),
        catalog_identity: op.to_string(),
        required_sequence: None,
        policy: DependencyPolicy::ReconcileBeforeAdmission,
    });
    let owner = fl_store::FerroStore::open(&f.source).unwrap();
    assert!(matches!(
        backup(
            &owner,
            &request,
            &f.filesystem(),
            &fl_store::recovery::CancelToken::new()
        )
        .unwrap(),
        PublicationOutcome::Durable(_)
    ));
    drop(owner);
    let error = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap_err();
    assert!(error.to_string().contains("reference closure"), "{error}");
    assert!(f.backups.join(op.to_string()).join("image.fls").exists());
}

// New replay tests are black-box recovery operations over private native fixtures.
// Cut before unlink N: the preceding members already have durable completion rows.
struct PurgeReplayCut {
    stop: usize,
    seen: std::sync::atomic::AtomicUsize,
}
impl RecoveryIo for PurgeReplayCut {
    fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
        if e == RecoveryEvent::PurgeUnlink && self.seen.fetch_add(1, Ordering::AcqRel) == self.stop
        {
            return Err(std::io::Error::other("fixture replay checkpoint"));
        }
        Ok(())
    }
}
fn admitted_purge(f: &Fixture, op: uuid::Uuid, lease: &OfflineLease, stop: usize) -> PurgeReport {
    let report = purge_quarantined(&inspection(op), lease, &f.filesystem(), &limits()).unwrap();
    let hook = Arc::new(PurgeReplayCut {
        stop,
        seen: std::sync::atomic::AtomicUsize::new(0),
    });
    let filesystem = HookFs {
        f,
        io: hook.clone(),
        limits: limits(),
    };
    assert!(purge_quarantined(&confirmation(&report), lease, &filesystem, &limits()).is_err());
    assert_eq!(hook.seen.load(Ordering::Acquire), stop + 1);
    assert!(
        f.backups
            .join(".catalog")
            .join(format!("purge-{op}.intent.json"))
            .exists()
    );
    report
}
fn assert_replay_refuses_before_unlink(
    f: &Fixture,
    op: uuid::Uuid,
    lease: &OfflineLease,
    report: &PurgeReport,
) {
    let hooks = Arc::new(PurgeReplayCut {
        stop: usize::MAX,
        seen: std::sync::atomic::AtomicUsize::new(0),
    });
    let filesystem = HookFs {
        f,
        io: hooks.clone(),
        limits: limits(),
    };
    assert!(purge_quarantined(&inspection(op), lease, &filesystem, &limits()).is_err());
    assert!(purge_quarantined(&confirmation(report), lease, &filesystem, &limits()).is_err());
    assert_eq!(
        hooks.seen.load(Ordering::Acquire),
        0,
        "invalid replay mutated a member"
    );
    assert!(
        !f.backups
            .join(".catalog")
            .join(format!("purge-{op}.complete.json"))
            .exists()
    );
}
#[test]
fn purge_replay_refuses_restored_live_receipt_and_conflicting_target_before_unlink() {
    for restored_receipt in [true, false] {
        let f = empty_fixture();
        let (op, lease) = quarantined(&f);
        let report = admitted_purge(&f, op, &lease, 0);
        if restored_receipt {
            let receipt = f
                .backups
                .join(".quarantine")
                .join(op.to_string())
                .join("receipt.json");
            let live = f.backups.join(".catalog").join(format!("{op}.json"));
            fs::copy(receipt, &live).unwrap();
            fs::set_permissions(&live, fs::Permissions::from_mode(0o600)).unwrap();
        } else {
            let duplicate = f.backups.join(".staging").join(op.to_string());
            fs::create_dir(&duplicate).unwrap();
            fs::set_permissions(duplicate, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let before = fs::read(f.backups.join(op.to_string()).join("image.fls")).unwrap();
        assert_replay_refuses_before_unlink(&f, op, &lease, &report);
        assert_eq!(
            fs::read(f.backups.join(op.to_string()).join("image.fls")).unwrap(),
            before
        );
    }
}
#[test]
fn purge_replay_refuses_unknown_names_changed_survivors_and_missing_geometry() {
    for change in 0..4 {
        let f = empty_fixture();
        let (op, lease) = quarantined(&f);
        let report = admitted_purge(&f, op, &lease, 0);
        match change {
            0 | 1 => {
                let parent = if change == 0 {
                    f.backups.join(op.to_string())
                } else {
                    f.backups.join(".quarantine").join(op.to_string())
                };
                let extra = parent.join("unrelated.bin");
                fs::write(&extra, b"not admitted").unwrap();
                fs::set_permissions(extra, fs::Permissions::from_mode(0o600)).unwrap();
            }
            2 => {
                // Preserve the exact payload while replacing the admitted inode.
                let image = f.backups.join(op.to_string()).join("image.fls");
                let saved = f.root.path().join("saved-image");
                fs::rename(&image, &saved).unwrap();
                fs::copy(saved, &image).unwrap();
                fs::set_permissions(image, fs::Permissions::from_mode(0o600)).unwrap();
            }
            3 => {
                let intent = f
                    .backups
                    .join(".catalog")
                    .join(format!("purge-{op}.intent.json"));
                let mut old: serde_json::Value =
                    serde_json::from_slice(&fs::read(&intent).unwrap()).unwrap();
                old.as_object_mut().unwrap().remove("geometry");
                fs::write(intent, serde_json::to_vec(&old).unwrap()).unwrap();
            }
            _ => unreachable!(),
        }
        assert_replay_refuses_before_unlink(&f, op, &lease, &report);
    }
}
#[test]
fn purge_replay_refuses_empty_target_and_quarantine_directory_replacement() {
    for quarantine in [false, true] {
        let f = empty_fixture();
        let (op, lease) = quarantined(&f);
        let report =
            purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
        assert!(
            purge_quarantined(&confirmation(&report), &lease, &f.filesystem(), &limits())
                .unwrap()
                .complete
        );
        let replaced = if quarantine {
            f.backups.join(".quarantine").join(op.to_string())
        } else {
            f.backups.join(op.to_string())
        };
        assert!(fs::read_dir(&replaced).unwrap().next().is_none());
        let held = f.root.path().join("old-directory");
        fs::rename(&replaced, &held).unwrap();
        fs::create_dir(&replaced).unwrap();
        fs::set_permissions(&replaced, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).is_err());
        assert!(
            purge_quarantined(&confirmation(&report), &lease, &f.filesystem(), &limits()).is_err()
        );
        assert!(held.is_dir() && replaced.is_dir());
    }
}
#[test]
fn purge_replay_refuses_changed_completion_and_requires_nonmanifest_durability() {
    // A completion for the wrong member must be detected before touching any survivor.
    let f = empty_fixture();
    let (op, lease) = quarantined(&f);
    let report = admitted_purge(&f, op, &lease, 1);
    let completion = f
        .backups
        .join(".catalog")
        .join(format!("purge-{op}.member-0.json"));
    let mut bad: serde_json::Value =
        serde_json::from_slice(&fs::read(&completion).unwrap()).unwrap();
    bad["member_index"] = serde_json::json!(999);
    fs::write(&completion, serde_json::to_vec(&bad).unwrap()).unwrap();
    assert_replay_refuses_before_unlink(&f, op, &lease, &report);

    // Simulate the final unlink succeeding before its parent sync/completion row.
    // This is recoverable only after every nonmanifest completion was durable.
    for remove_predecessor in [false, true] {
        let f = empty_fixture();
        let (op, lease) = quarantined(&f);
        let before =
            purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
        let report = admitted_purge(&f, op, &lease, before.members.len() - 1);
        let manifest = f.backups.join(op.to_string()).join("manifest.json");
        assert!(manifest.is_file());
        fs::remove_file(&manifest).unwrap();
        if remove_predecessor {
            fs::remove_file(
                f.backups
                    .join(".catalog")
                    .join(format!("purge-{op}.member-0.json")),
            )
            .unwrap();
            assert_replay_refuses_before_unlink(&f, op, &lease, &report);
        } else {
            let partial =
                purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
            assert!(!partial.complete);
            assert_eq!(
                partial
                    .members
                    .iter()
                    .filter(|m| m.state == PurgeMemberState::OutcomeUnknown)
                    .count(),
                1
            );
            assert!(
                purge_quarantined(&confirmation(&partial), &lease, &f.filesystem(), &limits())
                    .unwrap()
                    .complete
            );
        }
    }
}
#[test]
fn purge_rechecks_live_receipt_after_unlink_event_before_removing_member() {
    struct RestoreReceipt {
        path: std::path::PathBuf,
        bytes: Vec<u8>,
        seen: AtomicBool,
    }
    impl RecoveryIo for RestoreReceipt {
        fn before(&self, e: RecoveryEvent) -> std::io::Result<()> {
            if e == RecoveryEvent::PurgeUnlink && !self.seen.swap(true, Ordering::AcqRel) {
                fs::write(&self.path, &self.bytes)?;
                fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        }
    }
    let f = empty_fixture();
    let (op, lease) = quarantined(&f);
    let report = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    let hook = Arc::new(RestoreReceipt {
        path: f.backups.join(".catalog").join(format!("{op}.json")),
        bytes: fs::read(
            f.backups
                .join(".quarantine")
                .join(op.to_string())
                .join("receipt.json"),
        )
        .unwrap(),
        seen: AtomicBool::new(false),
    });
    let filesystem = HookFs {
        f: &f,
        io: hook.clone(),
        limits: limits(),
    };
    assert!(purge_quarantined(&confirmation(&report), &lease, &filesystem, &limits()).is_err());
    assert!(hook.seen.load(Ordering::Acquire));
    assert!(
        report
            .members
            .iter()
            .all(|m| f.backups.join(&m.relative_name).is_file())
    );
}

#[test]
fn purge_replays_the_admitted_staged_location_without_adopting_a_final_directory() {
    let f = empty_fixture();
    let (op, lease) = quarantined(&f);
    let final_path = f.backups.join(op.to_string());
    let staged = f.backups.join(".staging").join(op.to_string());
    fs::rename(&final_path, &staged).unwrap();
    let report = admitted_purge(&f, op, &lease, 0);
    let partial = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    assert!(
        partial
            .members
            .iter()
            .any(|m| m.relative_name == format!(".staging/{op}/image.fls"))
    );
    assert!(
        purge_quarantined(&confirmation(&report), &lease, &f.filesystem(), &limits())
            .unwrap()
            .complete
    );
    assert!(!final_path.exists() && staged.is_dir());
}
#[test]
fn purge_audit_authorization_reads_the_held_descriptor_and_refuses_replacement_or_mutation() {
    use std::os::{fd::AsRawFd, unix::fs::FileExt};
    struct ReplaceAudit {
        path: std::path::PathBuf,
        saved: std::path::PathBuf,
        replacement: Vec<u8>,
        same_inode: bool,
        seen: AtomicBool,
    }
    impl RecoveryIo for ReplaceAudit {
        fn read_exact_at(
            &self,
            file: &fs::File,
            bytes: &mut [u8],
            offset: u64,
        ) -> std::io::Result<()> {
            let observed = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            if observed == self.path && !self.seen.swap(true, Ordering::AcqRel) {
                if !self.same_inode {
                    fs::rename(&self.path, &self.saved)?;
                }
                fs::write(&self.path, &self.replacement)?;
                fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
            }
            file.read_exact_at(bytes, offset)
        }
    }
    for same_inode in [false, true] {
        let f = empty_fixture();
        let (op, lease) = quarantined(&f);
        let report = admitted_purge(&f, op, &lease, 0);
        let path = f
            .backups
            .join(".catalog")
            .join(format!("purge-{op}.intent.json"));
        let mut replacement: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        replacement["actor"] = serde_json::json!("other-fixture-operator");
        let replacement = serde_json::to_vec(&replacement).unwrap();
        let hook = Arc::new(ReplaceAudit {
            path: path.clone(),
            saved: f.root.path().join("held-intent"),
            replacement: replacement.clone(),
            same_inode,
            seen: AtomicBool::new(false),
        });
        let filesystem = HookFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        assert!(purge_quarantined(&confirmation(&report), &lease, &filesystem, &limits()).is_err());
        assert!(hook.seen.load(Ordering::Acquire));
        assert_eq!(fs::read(path).unwrap(), replacement);
        assert!(
            report
                .members
                .iter()
                .all(|m| f.backups.join(&m.relative_name).is_file())
        );
    }
}

#[test]
fn purge_refuses_authority_changes_at_the_final_member_open_before_unlink() {
    // Arm at PurgeUnlink, then mutate at the next descriptor-open boundary.
    // Older code rechecked authority before this OpenFile and deleted a member
    // before discovering the restored receipt or changed audit after unlink.
    struct LateAuthorityChange {
        live_receipt: std::path::PathBuf,
        receipt_bytes: Vec<u8>,
        audit: std::path::PathBuf,
        saved_audit: std::path::PathBuf,
        change: u8,
        armed: AtomicBool,
        changed: AtomicBool,
    }
    impl RecoveryIo for LateAuthorityChange {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event == RecoveryEvent::PurgeUnlink {
                self.armed.store(true, Ordering::Release);
            }
            if event == RecoveryEvent::OpenFile
                && self.armed.load(Ordering::Acquire)
                && !self.changed.swap(true, Ordering::AcqRel)
            {
                match self.change {
                    0 => {
                        fs::write(&self.live_receipt, &self.receipt_bytes)?;
                        fs::set_permissions(&self.live_receipt, fs::Permissions::from_mode(0o600))?;
                    }
                    1 | 2 => {
                        let mut audit: serde_json::Value =
                            serde_json::from_slice(&fs::read(&self.audit)?)?;
                        audit["actor"] = serde_json::json!("late-fixture-operator");
                        if self.change == 2 {
                            fs::rename(&self.audit, &self.saved_audit)?;
                        }
                        fs::write(&self.audit, serde_json::to_vec(&audit)?)?;
                        fs::set_permissions(&self.audit, fs::Permissions::from_mode(0o600))?;
                    }
                    _ => unreachable!(),
                }
            }
            Ok(())
        }
    }
    for change in 0..3 {
        let f = empty_fixture();
        let (op, lease) = quarantined(&f);
        let report =
            purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
        let member_bytes: Vec<_> = report
            .members
            .iter()
            .map(|member| {
                (
                    member.relative_name.clone(),
                    fs::read(f.backups.join(&member.relative_name)).unwrap(),
                )
            })
            .collect();
        let catalog = f.backups.join(".catalog");
        let hook = Arc::new(LateAuthorityChange {
            live_receipt: catalog.join(format!("{op}.json")),
            receipt_bytes: fs::read(
                f.backups
                    .join(".quarantine")
                    .join(op.to_string())
                    .join("receipt.json"),
            )
            .unwrap(),
            audit: catalog.join(format!("purge-{op}.intent.json")),
            saved_audit: f.root.path().join("saved-late-audit"),
            change,
            armed: AtomicBool::new(false),
            changed: AtomicBool::new(false),
        });
        let filesystem = HookFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        assert!(purge_quarantined(&confirmation(&report), &lease, &filesystem, &limits()).is_err());
        assert!(
            hook.changed.load(Ordering::Acquire),
            "late OpenFile mutation was not exercised"
        );
        for (name, bytes) in member_bytes {
            assert_eq!(
                fs::read(f.backups.join(&name)).unwrap(),
                bytes,
                "member {name} removed before final authority check"
            );
        }
        assert!(!catalog.join(format!("purge-{op}.member-0.json")).exists());
        assert!(!catalog.join(format!("purge-{op}.complete.json")).exists());
        if change == 0 {
            assert_eq!(fs::read(&hook.live_receipt).unwrap(), hook.receipt_bytes);
        } else {
            let changed: serde_json::Value =
                serde_json::from_slice(&fs::read(&hook.audit).unwrap()).unwrap();
            assert_eq!(changed["actor"], "late-fixture-operator");
            assert_eq!(hook.saved_audit.exists(), change == 2);
        }
    }
}

#[test]
fn purge_manifest_final_hash_preserves_predecessor_authority_on_late_changes() {
    struct LateManifestRead {
        predecessor: std::path::PathBuf,
        predecessor_bytes: Vec<u8>,
        completion: std::path::PathBuf,
        completion_bytes: Vec<u8>,
        saved_completion: std::path::PathBuf,
        manifest_identity: (u64, u64),
        reads: std::sync::atomic::AtomicUsize,
        armed: AtomicBool,
        changed: AtomicBool,
        change: u8,
    }
    impl RecoveryIo for LateManifestRead {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event == RecoveryEvent::PurgeUnlink {
                self.armed.store(true, Ordering::Release);
            }
            Ok(())
        }
        fn read_exact_at(
            &self,
            file: &std::fs::File,
            bytes: &mut [u8],
            offset: u64,
        ) -> std::io::Result<()> {
            use std::os::unix::fs::{FileExt, MetadataExt};
            let metadata = file.metadata()?;
            if self.armed.load(Ordering::Acquire)
                && (metadata.dev(), metadata.ino()) == self.manifest_identity
                && !self.changed.swap(true, Ordering::AcqRel)
            {
                self.reads.fetch_add(1, Ordering::AcqRel);
                match self.change {
                    0 => {
                        fs::write(&self.predecessor, &self.predecessor_bytes)?;
                        fs::set_permissions(&self.predecessor, fs::Permissions::from_mode(0o600))?;
                    }
                    1 => {
                        // Even a byte-identical replacement cannot be adopted.
                        fs::rename(&self.completion, &self.saved_completion)?;
                        fs::write(&self.completion, &self.completion_bytes)?;
                        fs::set_permissions(&self.completion, fs::Permissions::from_mode(0o600))?;
                    }
                    2 => {
                        // Remains valid JSON, but changes the held inode binding.
                        let mut changed = self.completion_bytes.clone();
                        changed.push(b' ');
                        fs::write(&self.completion, changed)?;
                    }
                    _ => unreachable!(),
                }
            }
            file.read_exact_at(bytes, offset)
        }
    }
    for change in 0..3 {
        let f = empty_fixture();
        let (op, lease) = quarantined(&f);
        let first = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
        let manifest_index = first
            .members
            .iter()
            .position(|m| m.relative_name.ends_with("/manifest.json"))
            .unwrap();
        assert_eq!(manifest_index + 1, first.members.len());
        assert!(manifest_index > 0);
        let predecessor = f.backups.join(&first.members[0].relative_name);
        let predecessor_bytes = fs::read(&predecessor).unwrap();
        let report = admitted_purge(&f, op, &lease, manifest_index);
        let catalog = f.backups.join(".catalog");
        let audit = catalog.join(format!("purge-{op}.intent.json"));
        let audit_bytes = fs::read(&audit).unwrap();
        let completions: Vec<_> = (0..manifest_index)
            .map(|index| {
                let path = catalog.join(format!("purge-{op}.member-{index}.json"));
                let bytes = fs::read(&path).unwrap();
                assert!(
                    bytes.len() < 65536,
                    "fixture completion must require one Read"
                );
                (path, bytes)
            })
            .collect();
        let manifest_path = f
            .backups
            .join(&report.members[manifest_index].relative_name);
        let manifest_bytes = fs::read(&manifest_path).unwrap();
        let hook = Arc::new(LateManifestRead {
            predecessor,
            predecessor_bytes,
            completion: completions[0].0.clone(),
            completion_bytes: completions[0].1.clone(),
            saved_completion: f.root.path().join("saved-late-completion"),
            // Match the held manifest inode, so the mutation occurs inside
            // its final hash rather than during a predecessor completion read.
            manifest_identity: {
                use std::os::unix::fs::MetadataExt;
                let metadata = fs::metadata(&manifest_path).unwrap();
                (metadata.dev(), metadata.ino())
            },
            reads: std::sync::atomic::AtomicUsize::new(0),
            armed: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            change,
        });
        let filesystem = HookFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        assert!(purge_quarantined(&confirmation(&report), &lease, &filesystem, &limits()).is_err());
        assert!(
            hook.changed.load(Ordering::Acquire),
            "final manifest hash hook must fire"
        );
        assert_eq!(hook.reads.load(Ordering::Acquire), 1);
        assert_eq!(fs::read(&manifest_path).unwrap(), manifest_bytes);
        assert_eq!(fs::read(&audit).unwrap(), audit_bytes);
        assert!(
            !catalog
                .join(format!("purge-{op}.member-{manifest_index}.json"))
                .exists()
        );
        assert!(!catalog.join(format!("purge-{op}.complete.json")).exists());
        for (index, (path, bytes)) in completions.iter().enumerate() {
            let mut expected = bytes.clone();
            if change == 2 && index == 0 {
                expected.push(b' ');
            }
            assert_eq!(
                fs::read(path).unwrap(),
                expected,
                "existing authority changed"
            );
        }
        assert_eq!(hook.saved_completion.exists(), change == 1);
        if change == 1 {
            assert_eq!(
                fs::read(&hook.saved_completion).unwrap(),
                hook.completion_bytes
            );
        }
        if change == 0 {
            assert_eq!(fs::read(&hook.predecessor).unwrap(), hook.predecessor_bytes);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum PurgeLeaseExpiryAt {
    AuditRead,
    Unlink(usize),
    FinalRead(usize),
}
fn assert_purge_stops_at_lease_expiry(at: PurgeLeaseExpiryAt) {
    use std::os::unix::fs::{FileExt, MetadataExt};
    use std::sync::atomic::AtomicUsize;
    struct ExpiringIo {
        at: PurgeLeaseExpiryAt,
        issued: u128,
        expires: u128,
        audit: std::path::PathBuf,
        member_identities: Vec<(u64, u64)>,
        unlinks: AtomicUsize,
        expired: AtomicBool,
    }
    impl RecoveryIo for ExpiringIo {
        fn unix_ms(&self) -> std::io::Result<u128> {
            Ok(if self.expired.load(Ordering::Acquire) {
                self.expires
            } else {
                self.issued
            })
        }
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event == RecoveryEvent::PurgeUnlink {
                let index = self.unlinks.fetch_add(1, Ordering::AcqRel);
                if matches!(self.at, PurgeLeaseExpiryAt::Unlink(stop) if index == stop) {
                    self.expired.store(true, Ordering::Release);
                }
            }
            Ok(())
        }
        fn read_exact_at(
            &self,
            file: &std::fs::File,
            bytes: &mut [u8],
            offset: u64,
        ) -> std::io::Result<()> {
            let metadata = file.metadata()?;
            let identity = (metadata.dev(), metadata.ino());
            if matches!(self.at, PurgeLeaseExpiryAt::AuditRead) {
                let audit = fs::metadata(&self.audit)?;
                if identity == (audit.dev(), audit.ino()) {
                    // Expire immediately before the first existing audit read.
                    self.expired.store(true, Ordering::Release);
                }
            }
            file.read_exact_at(bytes, offset)?;
            if let PurgeLeaseExpiryAt::FinalRead(index) = self.at
                && self.unlinks.load(Ordering::Acquire) == index + 1
                && identity == self.member_identities[index]
                && offset + bytes.len() as u64 == metadata.len()
            {
                // The PurgeUnlink seam has passed; this is the final held
                // member digest read, not the earlier verification hash.
                self.expired.store(true, Ordering::Release);
            }
            Ok(())
        }
    }
    let f = empty_fixture();
    let (op, lease) = quarantined(&f);
    // Persist the exact intent without removing any member. Replaying it
    // also exercises expiry during its read, before the unlink loop starts.
    let report = admitted_purge(&f, op, &lease, 0);
    assert!(report.members.len() > 1);
    let member_bytes: Vec<_> = report
        .members
        .iter()
        .map(|member| fs::read(f.backups.join(&member.relative_name)).unwrap())
        .collect();
    let request: OfflineLeaseRequest =
        serde_json::from_slice(&fs::read(lease.path()).unwrap()).unwrap();
    let catalog = f.backups.join(".catalog");
    let audit = catalog.join(format!("purge-{op}.intent.json"));
    let audit_before = fs::read(&audit).unwrap();
    let hook = Arc::new(ExpiringIo {
        at,
        issued: request.issued_at_unix_ms,
        expires: request.expires_at_unix_ms,
        audit: audit.clone(),
        member_identities: report.members.iter().map(|m| m.identity).collect(),
        unlinks: AtomicUsize::new(0),
        expired: AtomicBool::new(false),
    });
    let filesystem = HookFs {
        f: &f,
        io: hook.clone(),
        limits: limits(),
    };
    let error =
        purge_quarantined(&confirmation(&report), &lease, &filesystem, &limits()).unwrap_err();
    assert!(error.to_string().contains("lease"), "{at:?}: {error}");
    assert!(
        hook.expired.load(Ordering::Acquire),
        "{at:?}: hook not reached"
    );
    let (removed, seams) = match at {
        PurgeLeaseExpiryAt::AuditRead => (0, 0),
        PurgeLeaseExpiryAt::Unlink(index) | PurgeLeaseExpiryAt::FinalRead(index) => {
            (index, index + 1)
        }
    };
    assert_eq!(hook.unlinks.load(Ordering::Acquire), seams, "{at:?}");
    for (index, member) in report.members.iter().enumerate() {
        let path = f.backups.join(&member.relative_name);
        if index < removed {
            assert!(!path.exists());
        } else {
            assert_eq!(
                fs::read(path).unwrap(),
                member_bytes[index],
                "{at:?}: member {index}"
            );
        }
        assert_eq!(
            catalog
                .join(format!("purge-{op}.member-{index}.json"))
                .exists(),
            index < removed,
            "only members removed before expiry may have completion rows"
        );
    }
    assert_eq!(fs::read(audit).unwrap(), audit_before);
    assert!(!catalog.join(format!("purge-{op}.complete.json")).exists());
    // Issue separate renewed authority at the injected expiry; do not
    // extend or rewrite the old lease, or move its clock backwards.
    struct RenewedClock(u128);
    impl RecoveryIo for RenewedClock {
        fn unix_ms(&self) -> std::io::Result<u128> {
            Ok(self.0)
        }
    }
    let old_lease_bytes = fs::read(lease.path()).unwrap();
    let mut renewed_request = request.clone();
    renewed_request.issued_at_unix_ms = request.expires_at_unix_ms;
    renewed_request.expires_at_unix_ms = request.expires_at_unix_ms + 600000;
    renewed_request.exclusion.evidence = "renewed fixture administrative exclusion".into();
    let renewed_fs = HookFs {
        f: &f,
        io: Arc::new(RenewedClock(renewed_request.issued_at_unix_ms)),
        limits: limits(),
    };
    let renewed = create_offline_lease(&renewed_request, &renewed_fs, &limits()).unwrap();
    assert_ne!(renewed.path(), lease.path());
    assert_eq!(fs::read(lease.path()).unwrap(), old_lease_bytes);
    // Expiry returns Err, never a successful partial/complete report.
    // Renewed-authority inspection reconstructs the durable partial outcome.
    let partial = purge_quarantined(&inspection(op), &renewed, &renewed_fs, &limits()).unwrap();
    assert!(partial.audit_durable && !partial.complete);
    for (index, member) in partial.members.iter().enumerate() {
        assert_eq!(
            member.state,
            if index < removed {
                PurgeMemberState::Removed
            } else {
                PurgeMemberState::Present
            }
        );
    }
    let resumed =
        purge_quarantined(&confirmation(&partial), &renewed, &renewed_fs, &limits()).unwrap();
    assert!(resumed.complete && resumed.audit_durable);
    assert!(
        resumed
            .members
            .iter()
            .all(|m| m.state == PurgeMemberState::Removed)
    );
    assert_eq!(fs::read(lease.path()).unwrap(), old_lease_bytes);
}

#[test]
fn purge_lease_expiry_before_audit_read_retains_all_members() {
    assert_purge_stops_at_lease_expiry(PurgeLeaseExpiryAt::AuditRead);
}

#[test]
fn purge_lease_expiry_at_first_unlink_retains_all_members() {
    assert_purge_stops_at_lease_expiry(PurgeLeaseExpiryAt::Unlink(0));
}

#[test]
fn purge_lease_expiry_after_first_final_read_retains_all_members() {
    assert_purge_stops_at_lease_expiry(PurgeLeaseExpiryAt::FinalRead(0));
}

#[test]
fn purge_lease_expiry_at_later_unlink_retains_pending_members() {
    assert_purge_stops_at_lease_expiry(PurgeLeaseExpiryAt::Unlink(1));
}

#[test]
fn purge_lease_expiry_after_later_final_read_retains_pending_members() {
    assert_purge_stops_at_lease_expiry(PurgeLeaseExpiryAt::FinalRead(1));
}
