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
                | RecoveryEvent::Allocate
                | RecoveryEvent::Publish
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
        assert!(
            recovered,
            "cut {cut} {event:?} must recover through typed purge retry"
        );
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

#[test]
fn purge_audit_temps_are_ignored_only_with_private_regular_canonical_names() {
    use std::os::unix::fs::MetadataExt;
    for kind in 0..10 {
        let f = empty_fixture();
        let (op, lease) = quarantined(&f);
        let before =
            purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
        let name = match kind {
            0 => format!("purge-{op}.intent.json.tmp"),
            1 => format!("purge-{op}.member-0.json.tmp"),
            2 => format!("purge-{op}.complete.json.tmp"),
            3 => format!("purge-{op}.member-01.json.tmp"),
            4 => format!("purge-{}.intent.json.tmp", uuid::Uuid::nil()),
            5 => format!("purge-{op}.intent.json.tmp.extra"),
            6 => format!("purge-{op}.intent.json"),
            _ => format!("purge-{op}.intent.json.tmp"),
        };
        let path = f.backups.join(".catalog").join(name);
        match kind {
            7 => std::os::unix::fs::symlink("missing", &path).unwrap(),
            8 => fs::create_dir(&path).unwrap(),
            _ => fs::write(&path, b"torn audit temp").unwrap(),
        }
        if kind == 8 {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        } else if kind != 7 {
            fs::set_permissions(
                &path,
                fs::Permissions::from_mode(if kind == 9 { 0o644 } else { 0o600 }),
            )
            .unwrap();
        }
        let meta = fs::symlink_metadata(&path).unwrap();
        let result = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits());
        if kind <= 2 {
            let report = result.expect("private canonical audit temp is not published authority");
            assert_eq!(report.catalog_set_sha256, before.catalog_set_sha256);
            assert!(!report.audit_durable);
        } else {
            assert!(result.is_err(), "kind {kind} must refuse");
        }
        let after = fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            (
                meta.dev(),
                meta.ino(),
                meta.len(),
                meta.mtime(),
                meta.mtime_nsec()
            ),
            (
                after.dev(),
                after.ino(),
                after.len(),
                after.mtime(),
                after.mtime_nsec()
            )
        );
        if kind != 7 && kind != 8 {
            assert_eq!(fs::read(&path).unwrap(), b"torn audit temp");
        }
        for m in before.members {
            assert!(f.backups.join(m.relative_name).exists());
        }
    }
}

#[test]
fn purge_audit_own_torn_temps_retry_without_decoding() {
    for row in ["intent", "member-0", "complete"] {
        let f = empty_fixture();
        let (op, lease) = quarantined(&f);
        let report =
            purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
        let temp = f
            .backups
            .join(".catalog")
            .join(format!("purge-{op}.{row}.json.tmp"));
        fs::write(&temp, b"{").unwrap();
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).unwrap();
        let inspected =
            purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
        assert_eq!(fs::read(&temp).unwrap(), b"{");
        assert_eq!(inspected.catalog_set_sha256, report.catalog_set_sha256);
        assert!(
            purge_quarantined(
                &confirmation(&inspected),
                &lease,
                &f.filesystem(),
                &limits()
            )
            .unwrap()
            .complete
        );
        assert!(!temp.exists(), "own {row} temp remains after retry");
    }
}

#[test]
fn purge_audit_publication_refuses_collisions_replacements_and_expiry() {
    use std::os::{fd::AsRawFd, unix::fs::FileExt};
    struct Race {
        temp: std::path::PathBuf,
        final_path: std::path::PathBuf,
        lock: std::path::PathBuf,
        live_receipt: std::path::PathBuf,
        receipt: Vec<u8>,
        issued: u128,
        expires: u128,
        change: u8,
        fired: AtomicBool,
    }
    impl RecoveryIo for Race {
        fn unix_ms(&self) -> std::io::Result<u128> {
            Ok(if self.change >= 5 && self.fired.load(Ordering::Acquire) {
                self.expires
            } else {
                self.issued
            })
        }
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event != RecoveryEvent::Publish
                || !self.temp.exists()
                || self.change == 6
                || self.fired.swap(true, Ordering::AcqRel)
            {
                return Ok(());
            }
            match self.change {
                0 => {
                    fs::write(&self.final_path, b"foreign final audit")?;
                    fs::set_permissions(&self.final_path, fs::Permissions::from_mode(0o600))?;
                }
                1 | 2 => {
                    fs::rename(&self.temp, self.temp.with_extension("saved"))?;
                    if self.change == 1 {
                        fs::write(&self.temp, b"foreign temp audit")?;
                        fs::set_permissions(&self.temp, fs::Permissions::from_mode(0o600))?;
                    } else {
                        std::os::unix::fs::symlink("foreign-target", &self.temp)?;
                    }
                }
                3 => {
                    fs::rename(&self.lock, self.lock.with_extension("saved"))?;
                    fs::write(&self.lock, b"foreign operation lock")?;
                    fs::set_permissions(&self.lock, fs::Permissions::from_mode(0o600))?;
                }
                4 => {
                    fs::write(&self.live_receipt, &self.receipt)?;
                    fs::set_permissions(&self.live_receipt, fs::Permissions::from_mode(0o600))?;
                }
                5 => {}
                _ => unreachable!(),
            }
            Ok(())
        }
        fn read_exact_at(
            &self,
            file: &fs::File,
            bytes: &mut [u8],
            offset: u64,
        ) -> std::io::Result<()> {
            file.read_exact_at(bytes, offset)?;
            if self.change == 6
                && fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))? == self.temp
            {
                self.fired.store(true, Ordering::Release);
            }
            Ok(())
        }
    }
    for row in ["intent", "member-0", "complete"] {
        for change in 0..7 {
            let f = empty_fixture();
            let (op, lease) = quarantined(&f);
            let report =
                purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
            let binding: OfflineLeaseRequest =
                serde_json::from_slice(&fs::read(lease.path()).unwrap()).unwrap();
            let final_path = f
                .backups
                .join(".catalog")
                .join(format!("purge-{op}.{row}.json"));
            let temp = final_path.with_extension("json.tmp");
            let hook = Arc::new(Race {
                temp: temp.clone(),
                final_path: final_path.clone(),
                lock: f
                    .backups
                    .join(".locks")
                    .join(format!("operation-{op}.lock")),
                live_receipt: f.backups.join(".catalog").join(format!("{op}.json")),
                receipt: fs::read(
                    f.backups
                        .join(".quarantine")
                        .join(op.to_string())
                        .join("receipt.json"),
                )
                .unwrap(),
                issued: binding.issued_at_unix_ms,
                expires: binding.expires_at_unix_ms,
                change,
                fired: AtomicBool::new(false),
            });
            let result = purge_quarantined(
                &confirmation(&report),
                &lease,
                &HookFs {
                    f: &f,
                    io: hook.clone(),
                    limits: limits(),
                },
                &limits(),
            );
            assert!(
                hook.fired.load(Ordering::Acquire),
                "row {row} change {change} seam not reached"
            );
            assert!(result.is_err(), "row {row} change {change} completed");
            match change {
                0 => assert_eq!(fs::read(&final_path).unwrap(), b"foreign final audit"),
                1 => assert_eq!(fs::read(&temp).unwrap(), b"foreign temp audit"),
                2 => assert_eq!(
                    fs::read_link(&temp).unwrap(),
                    std::path::Path::new("foreign-target")
                ),
                3 => assert_eq!(fs::read(&hook.lock).unwrap(), b"foreign operation lock"),
                4 => assert_eq!(fs::read(&hook.live_receipt).unwrap(), hook.receipt),
                5 | 6 => assert!(!final_path.exists(), "expired row must not publish"),
                _ => unreachable!(),
            }
            if change != 0 {
                assert!(!final_path.exists());
            }
            if row == "intent" {
                assert!(
                    report
                        .members
                        .iter()
                        .all(|m| f.backups.join(&m.relative_name).exists())
                );
            }
        }
    }
}

// These controls must fail if either pin refusal/scan, the lease deadline cap,
// or either resumed-audit sync is removed. Mutation copies prove each separately.
fn pin_lifecycle_members(
    f: &Fixture,
    op: uuid::Uuid,
) -> std::collections::BTreeMap<std::path::PathBuf, (u64, u64, u64, String)> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    let mut files = std::collections::BTreeMap::new();
    for directory in [
        f.backups.join(op.to_string()),
        f.backups.join(".quarantine").join(op.to_string()),
    ] {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::metadata(&path).unwrap();
            assert!(metadata.is_file());
            let digest = format!("{:x}", Sha256::digest(fs::read(&path).unwrap()));
            files.insert(
                path,
                (metadata.dev(), metadata.ino(), metadata.len(), digest),
            );
        }
    }
    files
}
fn assert_real_restore_pin_lifecycle(abandon: bool) {
    use fl_store::recovery::{
        ProfileKind, RecoveryError, ValidationProfile, readiness::*, restore::*,
    };
    use sha2::{Digest, Sha256};
    use std::sync::atomic::AtomicUsize;
    struct PinIo {
        fail_copy: bool,
        copy_cuts: AtomicUsize,
        unlinks: AtomicUsize,
    }
    impl RecoveryIo for PinIo {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event == RecoveryEvent::Flush(fl_store::recovery::fs::Member::Image)
                && self.fail_copy
            {
                self.copy_cuts.fetch_add(1, Ordering::AcqRel);
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
            if event == RecoveryEvent::PurgeUnlink {
                self.unlinks.fetch_add(1, Ordering::AcqRel);
            }
            Ok(())
        }
    }
    let f = Fixture::new();
    let bundle = f.bundle();
    let manifest_bytes = fs::read(bundle.path().join("manifest.json")).unwrap();
    let manifest: BundleManifestV1 = serde_json::from_slice(&manifest_bytes).unwrap();
    let op = uuid::Uuid::parse_str(&manifest.operation_id).unwrap();
    let digest = format!("{:x}", Sha256::digest(&manifest_bytes));
    let identity = f.identity();
    let l = limits();
    let restore_binding = f.lease_request(
        OfflineAction::Restore {
            bundle_operation: op,
            bundle_sha256: digest.clone(),
        },
        l.operation_id,
        manifest.validation_profile_sha256.clone(),
        &f.source,
    );
    let mut purge_binding = f.lease_request(
        OfflineAction::Purge {
            quarantined_operation: op,
            manifest_sha256: digest,
        },
        op,
        manifest.validation_profile_sha256,
        &f.source,
    );
    let restore_lease = create_offline_lease(&restore_binding, &f.filesystem(), &l).unwrap();
    let request = RestoreRequest {
        operation_id: l.operation_id,
        store: identity.clone(),
        destination: f.source.clone(),
        staging_parent: f.root.path().to_path_buf(),
        selected_bundle: bundle,
        limits: l,
        metadata_byte_limit: 1 << 20,
        space_margin_bytes: 4096,
        original_profile: ValidationProfile::new(ProfileKind::SmallEmpty, identity.catalog_key),
        available_dependencies: vec![],
    };
    let hook = Arc::new(PinIo {
        fail_copy: abandon,
        copy_cuts: AtomicUsize::new(0),
        unlinks: AtomicUsize::new(0),
    });
    let filesystem = HookFs {
        f: &f,
        io: hook.clone(),
        limits: limits(),
    };
    let journal = if abandon {
        let error = restore_offline(&request, &restore_lease, &filesystem).unwrap_err();
        assert_eq!(
            hook.copy_cuts.load(Ordering::Acquire),
            1,
            "real post-guard copy seam not reached"
        );
        assert!(f.source.with_extension("fls.restore-guard").exists());
        let RecoveryError::OutcomeUnknown { .. } = error else {
            panic!("unexpected copy failure {error:?}")
        };
        let guard: RestoreGuardV1 = serde_json::from_slice(
            &fs::read(f.source.with_extension("fls.restore-guard")).unwrap(),
        )
        .unwrap();
        let j = load_restore_journal(&guard.journal, &restore_lease, &f.filesystem()).unwrap();
        assert_eq!(j.phase, RestorePhase::Admitted);
        j
    } else {
        let RestoreOutcome::Installed { journal } =
            restore_offline(&request, &restore_lease, &filesystem).unwrap()
        else {
            panic!("restore not installed")
        };
        journal
    };
    let pin = f
        .backups
        .join(".catalog/restore-pins")
        .join(format!("{}.json", journal.operation_id));
    assert!(pin.exists());
    let quarantine = f.backups.join(".quarantine").join(op.to_string());
    fs::create_dir(&quarantine).unwrap();
    fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(
        f.backups.join(".catalog").join(format!("{op}.json")),
        quarantine.join("receipt.json"),
    )
    .unwrap();
    purge_binding.expected_file = inspect_file_binding(&f.source, &limits()).unwrap();
    let purge_lease = create_offline_lease(&purge_binding, &f.filesystem(), &limits()).unwrap();
    let before = pin_lifecycle_members(&f, op);
    // Pin refusal must precede closure comparison. A current digest cannot be
    // inspected while pinned; after cleanup use a fresh actual inspection.
    let confirm = PurgeRequest {
        operation_id: op,
        confirm: Some(op),
        reference_closure: Some(ReferenceClosureEvidence {
            catalog_set_sha256: "a".repeat(64),
            non_ferroloop_writers_excluded: true,
            evidence_sha256: "d".repeat(64),
        }),
    };
    let inspected = purge_quarantined(&inspection(op), &purge_lease, &filesystem, &limits());
    let confirmed = purge_quarantined(&confirm, &purge_lease, &filesystem, &limits());
    let reason = "selected bundle is pinned by an unfinished restore";
    eprintln!("pin_lifecycle abandon={abandon} inspect={inspected:?} confirm={confirmed:?}");
    assert!(
        matches!(&inspected, Err(RecoveryError::RefusedInput { reason: actual, .. }) if *actual == reason),
        "inspect must give the specific pinned-bundle refusal: {inspected:?}"
    );
    assert!(
        matches!(&confirmed, Err(RecoveryError::RefusedInput { reason: actual, .. }) if *actual == reason),
        "confirm must give the specific pinned-bundle refusal: {confirmed:?}"
    );
    assert_eq!(hook.unlinks.load(Ordering::Acquire), 0);
    assert_eq!(pin_lifecycle_members(&f, op), before);
    if abandon {
        assert!(matches!(
            abandon_restore(&journal, &restore_lease, &f.filesystem()).unwrap(),
            RestoreOutcome::Abandoned { .. }
        ));
    } else {
        let report = journal.verification.as_ref().unwrap();
        let evidence = ReconciliationRecord {
            version: 1,
            operation_id: journal.operation_id,
            image_sha256: report.image_sha256.clone(),
            published_sequence: report.published_sequence,
            operator: restore_binding.operator,
            recorded_at_unix_ms: f.filesystem().io().unix_ms().unwrap(),
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
        };
        assert!(matches!(
            finalize_restore(&journal, &restore_lease, &evidence, &f.filesystem()).unwrap(),
            RestoreOutcome::Finalized { .. }
        ));
    }
    assert!(!pin.exists());
    assert!(!f.source.with_extension("fls.restore-guard").exists());
    let current = purge_quarantined(&inspection(op), &purge_lease, &filesystem, &limits()).unwrap();
    assert_eq!(
        pin_lifecycle_members(&f, op),
        before,
        "terminal cleanup changed selected bundle"
    );
    assert!(
        purge_quarantined(
            &confirmation(&current),
            &purge_lease,
            &filesystem,
            &limits()
        )
        .unwrap()
        .complete
    );
    assert!(hook.unlinks.load(Ordering::Acquire) > 0);
    assert!(
        current
            .members
            .iter()
            .all(|m| !f.backups.join(&m.relative_name).exists())
    );
}
#[test]
fn purge_pin_real_installed_bundle_refuses_inspect_and_confirm_until_finalize() {
    assert_real_restore_pin_lifecycle(false);
}
#[test]
fn purge_pin_real_admitted_bundle_refuses_inspect_and_confirm_until_abandon() {
    assert_real_restore_pin_lifecycle(true);
}

#[test]
fn purge_frozen_wall_clock_stall_is_stopped_by_lease_deadline_cap() {
    use fl_store::recovery::RecoveryError;
    use std::sync::atomic::AtomicUsize;
    struct FrozenStall {
        now: u128,
        unlinks: AtomicUsize,
    }
    impl RecoveryIo for FrozenStall {
        fn unix_ms(&self) -> std::io::Result<u128> {
            Ok(self.now)
        }
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event == RecoveryEvent::PurgeUnlink
                && self.unlinks.fetch_add(1, Ordering::AcqRel) == 0
            {
                std::thread::sleep(std::time::Duration::from_secs(6));
            }
            Ok(())
        }
    }
    let f = empty_fixture();
    let (op, lease) = quarantined(&f);
    let report = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    let binding: OfflineLeaseRequest =
        serde_json::from_slice(&fs::read(lease.path()).unwrap()).unwrap();
    let before = pin_lifecycle_members(&f, op);
    let hook = Arc::new(FrozenStall {
        now: binding.expires_at_unix_ms - 4000,
        unlinks: AtomicUsize::new(0),
    });
    let result = purge_quarantined(
        &confirmation(&report),
        &lease,
        &HookFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        },
        &limits(),
    );
    assert_eq!(
        hook.unlinks.load(Ordering::Acquire),
        1,
        "the first unlink seam must be reached exactly once"
    );
    assert!(
        matches!(result, Err(RecoveryError::Deadline { .. })),
        "monotonic cap must stop a frozen-wall-clock stall: {result:?}"
    );
    assert_eq!(
        pin_lifecycle_members(&f, op),
        before,
        "deadline must stop before any unlink"
    );
    let catalog = f.backups.join(".catalog");
    assert!(!catalog.join(format!("purge-{op}.complete.json")).exists());
    for index in 0..report.members.len() {
        assert!(
            !catalog
                .join(format!("purge-{op}.member-{index}.json"))
                .exists()
        );
    }
}

#[test]
fn purge_resume_resyncs_existing_audit_file_and_catalog_before_unlink() {
    use fl_store::recovery::fs::Member;
    use std::sync::Mutex;
    struct CutIntentSync {
        intent: std::path::PathBuf,
        fired: AtomicBool,
    }
    impl RecoveryIo for CutIntentSync {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event == RecoveryEvent::Sync(Directory::CatalogParent)
                && self.intent.exists()
                && !self.fired.swap(true, Ordering::AcqRel)
            {
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
            Ok(())
        }
    }
    #[derive(Default)]
    struct Trace {
        events: Mutex<Vec<RecoveryEvent>>,
    }
    impl RecoveryIo for Trace {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }
    let f = empty_fixture();
    let (op, lease) = quarantined(&f);
    let report = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    let intent = f
        .backups
        .join(".catalog")
        .join(format!("purge-{op}.intent.json"));
    let cut = Arc::new(CutIntentSync {
        intent: intent.clone(),
        fired: AtomicBool::new(false),
    });
    assert!(
        purge_quarantined(
            &confirmation(&report),
            &lease,
            &HookFs {
                f: &f,
                io: cut.clone(),
                limits: limits()
            },
            &limits()
        )
        .is_err()
    );
    assert!(
        cut.fired.load(Ordering::Acquire),
        "must interrupt the sync after final intent publication"
    );
    let audit_before = fs::read(&intent).unwrap();
    let inspected = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    assert!(inspected.audit_durable);
    assert!(
        inspected
            .members
            .iter()
            .all(|m| m.state == PurgeMemberState::Present)
    );
    let trace = Arc::new(Trace::default());
    assert!(
        purge_quarantined(
            &confirmation(&inspected),
            &lease,
            &HookFs {
                f: &f,
                io: trace.clone(),
                limits: limits()
            },
            &limits()
        )
        .unwrap()
        .complete
    );
    let events = trace.events.lock().unwrap();
    let first_unlink = events
        .iter()
        .position(|e| *e == RecoveryEvent::PurgeUnlink)
        .expect("resume must reach the unlink seam");
    let before_unlink = &events[..first_unlink];
    let flush = before_unlink
        .iter()
        .position(|e| *e == RecoveryEvent::Flush(Member::PurgeAudit))
        .expect("resume must re-sync the existing audit file before unlink");
    let sync = before_unlink
        .iter()
        .position(|e| *e == RecoveryEvent::Sync(Directory::CatalogParent))
        .expect("resume must re-sync the catalog before unlink");
    assert!(
        flush < sync && sync < first_unlink,
        "resume must order audit file sync, catalog sync, then unlink"
    );
    assert_eq!(fs::read(&intent).unwrap(), audit_before);
}

// A completed set retains its member even after that member's receipt is quarantined.
fn review13_batch_fixture(dependency: bool) -> (Fixture, uuid::Uuid, OfflineLease, uuid::Uuid) {
    use fl_store::recovery::{backup::*, catalog::PublicationReceiptV1, control::*, profiles::*};
    let f = empty_fixture();
    let (target, lease) = quarantined(&f);
    let mut request = f.backup_request();
    if dependency {
        request.profile.dependencies.push(ReferenceDependency {
            reference_sha256: "e".repeat(64),
            catalog_identity: target.to_string(),
            required_sequence: None,
            policy: DependencyPolicy::ReconcileBeforeAdmission,
        });
    }
    let owner = fl_store::FerroStore::open(&f.source).unwrap();
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
    let manifest: BundleManifestV1 =
        serde_json::from_slice(&fs::read(bundle.path().join("manifest.json")).unwrap()).unwrap();
    let other = uuid::Uuid::parse_str(&manifest.operation_id).unwrap();
    let receipt: PublicationReceiptV1 = serde_json::from_slice(
        &fs::read(f.backups.join(".catalog").join(format!("{other}.json"))).unwrap(),
    )
    .unwrap();
    let batch = uuid::Uuid::now_v7();
    let record = DurableBatchV1 {
        version: 1,
        batch_id: batch,
        members: vec![DurableBatchMember {
            operation_id: other,
            catalog_identity: manifest.catalog_store_identity,
            sequence: manifest.published_sequence,
            manifest_sha256: receipt.manifest_sha256,
            shared_config: manifest.shared_config,
        }],
    };
    let path = f.backups.join(format!(".batch-{batch}.json"));
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    (f, target, lease, other)
}
fn review13_quarantine_other(f: &Fixture, other: uuid::Uuid) -> std::path::PathBuf {
    let path = f.backups.join(".quarantine").join(other.to_string());
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(
        f.backups.join(".catalog").join(format!("{other}.json")),
        path.join("receipt.json"),
    )
    .unwrap();
    path
}
#[test]
fn purge_review13_completed_batch_accepts_exclusive_receipt_locations() {
    // Start with the regression: the old reader cannot find this valid receipt.
    for location in ["quarantine", "live-absent", "live-empty"] {
        let (f, target, lease, other) = review13_batch_fixture(false);
        let quarantine = f.backups.join(".quarantine").join(other.to_string());
        if location == "quarantine" {
            review13_quarantine_other(&f, other);
        } else if location == "live-empty" {
            fs::create_dir(&quarantine).unwrap();
            fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let before = if location == "quarantine" {
            pin_lifecycle_members(&f, other)
        } else {
            let mut files = std::collections::BTreeMap::new();
            use sha2::{Digest, Sha256};
            use std::os::unix::fs::MetadataExt;
            for entry in fs::read_dir(f.backups.join(other.to_string())).unwrap() {
                let path = entry.unwrap().path();
                let m = fs::metadata(&path).unwrap();
                files.insert(
                    path.clone(),
                    (
                        m.dev(),
                        m.ino(),
                        m.len(),
                        format!("{:x}", Sha256::digest(fs::read(path).unwrap())),
                    ),
                );
            }
            let path = f.backups.join(".catalog").join(format!("{other}.json"));
            let m = fs::metadata(&path).unwrap();
            files.insert(
                path.clone(),
                (
                    m.dev(),
                    m.ino(),
                    m.len(),
                    format!("{:x}", Sha256::digest(fs::read(path).unwrap())),
                ),
            );
            files
        };
        let report = purge_quarantined(&inspection(target), &lease, &f.filesystem(), &limits())
            .unwrap_or_else(|e| panic!("valid completed batch receipt at {location}: {e:?}"));
        let done =
            purge_quarantined(&confirmation(&report), &lease, &f.filesystem(), &limits()).unwrap();
        assert!(
            done.complete
                && done
                    .members
                    .iter()
                    .all(|m| m.state == PurgeMemberState::Removed)
        );
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::MetadataExt;
        for (path, expected) in before {
            let m = fs::metadata(&path).unwrap();
            assert_eq!(
                (
                    m.dev(),
                    m.ino(),
                    m.len(),
                    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
                ),
                expected
            );
        }
        eprintln!("review13 location={location} purge_complete=true other_retained=true");
    }
}

fn review13_refuses_without_target_unlink(
    f: &Fixture,
    target: uuid::Uuid,
    lease: &OfflineLease,
    report: &PurgeReport,
    io: Arc<dyn RecoveryIo>,
) {
    let before = pin_lifecycle_members(f, target);
    let filesystem = HookFs {
        f,
        io,
        limits: limits(),
    };
    for request in [inspection(target), confirmation(report)] {
        let result = purge_quarantined(&request, lease, &filesystem, &limits());
        assert!(
            result.is_err(),
            "invalid completed-batch receipt accepted: {result:?}"
        );
        assert_eq!(pin_lifecycle_members(f, target), before);
        assert!(
            !f.backups
                .join(".catalog")
                .join(format!("purge-{target}.complete.json"))
                .exists()
        );
    }
}
#[test]
fn purge_review13_completed_batch_refuses_ambiguous_or_invalid_receipts() {
    for change in [
        "neither",
        "both",
        "bad-json",
        "mismatch",
        "stray",
        "symlink-receipt",
        "symlink-directory",
        "regular-directory",
        "live-stray",
        "live-nonregular",
    ] {
        let (f, target, lease, other) = review13_batch_fixture(false);
        let quarantine = review13_quarantine_other(&f, other);
        let receipt = quarantine.join("receipt.json");
        let live = f.backups.join(".catalog").join(format!("{other}.json"));
        let report =
            purge_quarantined(&inspection(target), &lease, &f.filesystem(), &limits()).unwrap();
        match change {
            "neither" => fs::remove_file(&receipt).unwrap(),
            "both" => {
                fs::copy(&receipt, &live).unwrap();
            }
            "bad-json" => fs::write(&receipt, b"{torn").unwrap(),
            "mismatch" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
                value["manifest_sha256"] = serde_json::json!("0".repeat(64));
                fs::write(&receipt, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "stray" => fs::write(quarantine.join("stray"), b"retain").unwrap(),
            "symlink-receipt" => {
                let saved = f.root.path().join("saved-receipt");
                fs::rename(&receipt, &saved).unwrap();
                std::os::unix::fs::symlink(&saved, &receipt).unwrap();
            }
            "symlink-directory" => {
                let saved = f.root.path().join("saved-directory");
                fs::rename(&quarantine, &saved).unwrap();
                std::os::unix::fs::symlink(&saved, &quarantine).unwrap();
            }
            "regular-directory" => {
                fs::rename(&quarantine, f.root.path().join("saved-directory")).unwrap();
                fs::write(&quarantine, b"not a directory").unwrap();
                fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o600)).unwrap();
            }
            "live-stray" => {
                fs::rename(&receipt, &live).unwrap();
                fs::write(quarantine.join("stray"), b"retain").unwrap();
            }
            "live-nonregular" => {
                fs::rename(&receipt, &live).unwrap();
                fs::remove_file(&live).unwrap();
                fs::create_dir(&live).unwrap();
                fs::set_permissions(&live, fs::Permissions::from_mode(0o700)).unwrap();
            }
            _ => unreachable!(),
        }
        let hook = Arc::new(PurgeReplayCut {
            stop: usize::MAX,
            seen: std::sync::atomic::AtomicUsize::new(0),
        });
        review13_refuses_without_target_unlink(&f, target, &lease, &report, hook.clone());
        assert_eq!(hook.seen.load(Ordering::Acquire), 0, "{change}");
        eprintln!("review13 invalid={change} zero_unlinks=true");
    }
}
struct Review13ReceiptRace {
    receipt: std::path::PathBuf,
    quarantine: std::path::PathBuf,
    live: std::path::PathBuf,
    change: u8,
    batch_seen: AtomicBool,
    fired: AtomicBool,
    unlinks: std::sync::atomic::AtomicUsize,
}
impl RecoveryIo for Review13ReceiptRace {
    fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
        if event == RecoveryEvent::PurgeUnlink {
            self.unlinks.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }
    fn read_exact_at(&self, file: &fs::File, bytes: &mut [u8], offset: u64) -> std::io::Result<()> {
        use std::os::{fd::AsRawFd, unix::fs::FileExt};
        file.read_exact_at(bytes, offset)?;
        let path = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(".batch-"))
        {
            self.batch_seen.store(true, Ordering::Release);
        }
        if path == self.receipt
            && self.batch_seen.load(Ordering::Acquire)
            && !self.fired.swap(true, Ordering::AcqRel)
        {
            match self.change {
                0 => {
                    let data = fs::read(&self.receipt)?;
                    fs::rename(&self.receipt, self.receipt.with_extension("saved"))?;
                    fs::write(&self.receipt, data)?;
                    fs::set_permissions(&self.receipt, fs::Permissions::from_mode(0o600))?;
                }
                1 => {
                    fs::rename(&self.quarantine, self.quarantine.with_extension("saved"))?;
                    fs::create_dir(&self.quarantine)?;
                    fs::set_permissions(&self.quarantine, fs::Permissions::from_mode(0o700))?;
                    fs::copy(
                        self.quarantine.with_extension("saved").join("receipt.json"),
                        self.quarantine.join("receipt.json"),
                    )?;
                }
                2 => {
                    let parent = self.quarantine.parent().unwrap();
                    fs::rename(parent, parent.with_extension("saved"))?;
                    fs::create_dir(parent)?;
                    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
                    // The target's quarantine must also survive, by the same inode.
                    for entry in fs::read_dir(parent.with_extension("saved"))? {
                        let entry = entry?;
                        fs::rename(entry.path(), parent.join(entry.file_name()))?;
                    }
                }
                3 => {
                    fs::copy(&self.receipt, &self.live)?;
                }
                4 => {
                    fs::write(self.quarantine.join("stray"), b"late stray")?;
                }
                5 => {
                    fs::create_dir(&self.quarantine)?;
                    fs::set_permissions(&self.quarantine, fs::Permissions::from_mode(0o700))?;
                }
                6 => {
                    fs::rename(&self.quarantine, self.quarantine.with_extension("saved"))?;
                    fs::create_dir(&self.quarantine)?;
                    fs::set_permissions(&self.quarantine, fs::Permissions::from_mode(0o700))?;
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }
}
#[test]
fn purge_review13_completed_batch_rechecks_receipt_locations_after_reads() {
    for change in 0..7 {
        let (f, target, lease, other) = review13_batch_fixture(false);
        let quarantine = f.backups.join(".quarantine").join(other.to_string());
        let live = f.backups.join(".catalog").join(format!("{other}.json"));
        let receipt = if change < 5 {
            review13_quarantine_other(&f, other).join("receipt.json")
        } else {
            live.clone()
        };
        if change == 6 {
            fs::create_dir(&quarantine).unwrap();
            fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let report =
            purge_quarantined(&inspection(target), &lease, &f.filesystem(), &limits()).unwrap();
        let hook = Arc::new(Review13ReceiptRace {
            receipt,
            quarantine,
            live,
            change,
            batch_seen: AtomicBool::new(false),
            fired: AtomicBool::new(false),
            unlinks: std::sync::atomic::AtomicUsize::new(0),
        });
        let before = pin_lifecycle_members(&f, target);
        let filesystem = HookFs {
            f: &f,
            io: hook.clone(),
            limits: limits(),
        };
        let request = confirmation(&report);
        let result = purge_quarantined(&request, &lease, &filesystem, &limits());
        assert!(
            result.is_err(),
            "read race accepted change={change}: {result:?}"
        );
        assert_eq!(pin_lifecycle_members(&f, target), before);
        assert!(
            !f.backups
                .join(".catalog")
                .join(format!("purge-{target}.complete.json"))
                .exists()
        );
        assert!(
            hook.fired.load(Ordering::Acquire),
            "change={change} hook not reached"
        );
        assert_eq!(hook.unlinks.load(Ordering::Acquire), 0, "change={change}");
        eprintln!("review13 race={change} reached=true zero_unlinks=true");
    }
}
#[test]
fn purge_review13_completed_batch_quarantined_dependency_still_refuses() {
    let (f, target, lease, other) = review13_batch_fixture(true);
    review13_quarantine_other(&f, other);
    let before = pin_lifecycle_members(&f, target);
    let hook = Arc::new(PurgeReplayCut {
        stop: usize::MAX,
        seen: std::sync::atomic::AtomicUsize::new(0),
    });
    let filesystem = HookFs {
        f: &f,
        io: hook.clone(),
        limits: limits(),
    };
    let error = purge_quarantined(&inspection(target), &lease, &filesystem, &limits()).unwrap_err();
    assert!(
        matches!(
            error,
            fl_store::recovery::RecoveryError::Verification { .. }
        ),
        "{error:?}"
    );
    assert_eq!(hook.seen.load(Ordering::Acquire), 0);
    assert_eq!(pin_lifecycle_members(&f, target), before);
    assert!(
        !f.backups
            .join(".catalog")
            .join(format!("purge-{target}.complete.json"))
            .exists()
    );
}

#[test]
fn purge_review14_metadata_diagnostics_name_only_root_relative_entries() {
    for case in [
        "pin",
        "stray",
        "intent",
        "member",
        "complete",
        "pin-symlink",
        "stray-symlink",
        "intent-symlink",
        "member-symlink",
        "complete-symlink",
    ] {
        let kind = case.strip_suffix("-symlink").unwrap_or(case);
        let symlink = case.ends_with("-symlink");
        let f = Fixture::new();
        let (op, lease) = quarantined(&f);
        if matches!(kind, "member" | "complete") {
            let report =
                purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
            purge_quarantined(&confirmation(&report), &lease, &f.filesystem(), &limits()).unwrap();
        }
        let name = match kind {
            "pin" => format!(".catalog/restore-pins/{}.json", uuid::Uuid::now_v7()),
            "stray" => ".catalog/stray-metadata.json".into(),
            "intent" => format!(".catalog/purge-{op}.intent.json"),
            "member" => format!(".catalog/purge-{op}.member-0.json"),
            "complete" => format!(".catalog/purge-{op}.complete.json"),
            _ => unreachable!(),
        };
        let path = f.backups.join(&name);
        if kind == "pin" {
            fs::create_dir(path.parent().unwrap()).unwrap();
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let secret = "PRIVATE_METADATA_CONTENT_MUST_NOT_APPEAR";
        if symlink {
            if path.exists() {
                fs::remove_file(&path).unwrap();
            }
            std::os::unix::fs::symlink("PRIVATE_SYMLINK_TARGET_MUST_NOT_APPEAR", &path).unwrap();
        } else {
            fs::write(&path, secret).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        type Snapshot = std::collections::BTreeMap<std::path::PathBuf, (u64, u64, u64, Vec<u8>)>;
        fn snapshot(path: &std::path::Path) -> Snapshot {
            use std::os::unix::fs::MetadataExt;
            let mut rows = std::collections::BTreeMap::new();
            for e in fs::read_dir(path).unwrap() {
                let e = e.unwrap();
                let p = e.path();
                let m = fs::symlink_metadata(&p).unwrap();
                if m.is_dir() {
                    rows.extend(snapshot(&p));
                } else {
                    rows.insert(
                        p,
                        (
                            m.dev(),
                            m.ino(),
                            m.len(),
                            if m.is_file() {
                                fs::read(e.path()).unwrap()
                            } else {
                                vec![]
                            },
                        ),
                    );
                }
            }
            rows
        }
        let before = snapshot(&f.backups);
        let error =
            purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap_err();
        let message = error.to_string();
        if !symlink {
            assert!(
                matches!(&error, fl_store::recovery::RecoveryError::MetadataVerification { entry, source, .. }
                if entry == std::path::Path::new(&name) && matches!(source.as_ref(), fl_store::recovery::RecoveryError::Verification { .. })),
                "wrong diagnostic type: {error:?}"
            );
            assert!(message.contains(&name), "{case}: {message}");
            assert!(
                !message.contains(f.backups.to_str().unwrap()),
                "diagnostic must be root-relative: {message}"
            );
        }
        assert!(!message.contains(secret), "{case}: {message}");
        assert!(
            !message.contains("PRIVATE_SYMLINK_TARGET_MUST_NOT_APPEAR"),
            "{case}: {message}"
        );
        assert_eq!(
            snapshot(&f.backups),
            before,
            "{case}: read-only refusal changed artifacts"
        );
    }
}

// Bind every publication interval to its lexical row and bytes, so the initial
// intent re-sync cannot stand in for a later member or final completion sync.
#[test]
fn purge_every_named_audit_row_is_file_and_catalog_synced_before_next_action() {
    use fl_store::recovery::fs::Member;
    use std::{os::unix::fs::MetadataExt, sync::Mutex};
    type Row = (String, (u64, u64), serde_json::Value);
    struct RowTrace {
        catalog: std::path::PathBuf,
        prefix: String,
        events: Mutex<Vec<(RecoveryEvent, Option<Row>)>>,
    }
    impl RecoveryIo for RowTrace {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            let mut events = self.events.lock().unwrap();
            let row = if event == RecoveryEvent::RenameEntry {
                let temps: Vec<_> = fs::read_dir(&self.catalog)?
                    .map(|e| e.map(|e| e.path()))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .filter(|p| {
                        let name = p.file_name().unwrap().to_string_lossy();
                        name.starts_with(&self.prefix) && name.ends_with(".json.tmp")
                    })
                    .collect();
                assert_eq!(temps.len(), 1, "one audit row must own each rename");
                let path = &temps[0];
                let name = path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .strip_suffix(".tmp")
                    .unwrap()
                    .to_owned();
                let m = fs::metadata(path)?;
                Some((
                    name,
                    (m.dev(), m.ino()),
                    serde_json::from_slice(&fs::read(path)?)?,
                ))
            } else if event == RecoveryEvent::Sync(Directory::CatalogParent) {
                if let Some((_, Some((name, identity, value)))) = events
                    .iter()
                    .rev()
                    .find(|(e, _)| *e == RecoveryEvent::RenameEntry)
                {
                    let path = self.catalog.join(name);
                    let m = fs::metadata(&path)?;
                    assert_eq!(
                        (m.dev(), m.ino()),
                        *identity,
                        "sync must cover renamed inode"
                    );
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&fs::read(path)?)?,
                        *value
                    );
                    assert!(!self.catalog.join(format!("{name}.tmp")).exists());
                    Some((name.clone(), *identity, value.clone()))
                } else {
                    None
                }
            } else {
                None
            };
            events.push((event, row));
            Ok(())
        }
    }
    let f = empty_fixture();
    let (op, lease) = quarantined(&f);
    let report = purge_quarantined(&inspection(op), &lease, &f.filesystem(), &limits()).unwrap();
    let trace = Arc::new(RowTrace {
        catalog: f.backups.join(".catalog"),
        prefix: format!("purge-{op}."),
        events: Mutex::new(Vec::new()),
    });
    let done = purge_quarantined(
        &confirmation(&report),
        &lease,
        &HookFs {
            f: &f,
            io: trace.clone(),
            limits: limits(),
        },
        &limits(),
    )
    .unwrap();
    assert!(done.complete && done.audit_durable);
    // This snapshot is taken after return: the last interval must contain the
    // final completion's sync even though no later unlink can bound it.
    let events = trace.events.lock().unwrap();
    let renames: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, (e, _))| *e == RecoveryEvent::RenameEntry)
        .collect();
    assert_eq!(renames.len(), report.members.len() + 2);
    for (row_index, (at, (_, row))) in renames.iter().enumerate() {
        let (name, identity, value) = row.as_ref().unwrap();
        let expected_name = if row_index == 0 {
            format!("purge-{op}.intent.json")
        } else if row_index == report.members.len() + 1 {
            format!("purge-{op}.complete.json")
        } else {
            format!("purge-{op}.member-{}.json", row_index - 1)
        };
        assert_eq!(*name, expected_name);
        assert_eq!(value["operation_id"], op.to_string());
        if row_index == 0 {
            assert_eq!(
                value["members"].as_array().unwrap().len(),
                report.members.len()
            );
        } else if row_index == report.members.len() + 1 {
            assert!(
                value["member_index"].is_null(),
                "final completion row required"
            );
        } else {
            assert_eq!(value["member_index"], row_index - 1);
        }
        let write = events[..*at]
            .iter()
            .rposition(|(e, _)| *e == RecoveryEvent::Write(Member::PurgeAudit))
            .unwrap();
        assert!(
            events[write + 1..*at]
                .iter()
                .any(|(e, _)| *e == RecoveryEvent::Flush(Member::PurgeAudit)),
            "audit row {name} renamed before its own temporary file sync"
        );
        let end = events[*at + 1..]
            .iter()
            .position(|(e, _)| {
                matches!(
                    e,
                    RecoveryEvent::PurgeUnlink | RecoveryEvent::Write(Member::PurgeAudit)
                )
            })
            .map_or(events.len(), |offset| *at + 1 + offset);
        assert!(
            events[*at + 1..end].iter().any(|(e, row)| *e
                == RecoveryEvent::Sync(Directory::CatalogParent)
                && row
                    .as_ref()
                    .is_some_and(|(n, i, v)| n == name && i == identity && v == value)),
            "audit row {name} renamed without its catalog sync before next unlink/publication/return"
        );
    }
}

// Removing the referenced-member context must fail all three corruption cases.
fn purge_r4_corrupt_completed_batch_member_diagnostic(case: &str) {
    use fl_store::recovery::RecoveryError;
    use std::{collections::BTreeMap, error::Error, os::unix::fs::MetadataExt, path::PathBuf};

    type Snapshot = BTreeMap<PathBuf, (u64, u64, u32, u64, Vec<u8>)>;
    fn snapshot(path: &std::path::Path) -> Snapshot {
        let mut rows = BTreeMap::new();
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            rows.insert(
                path.clone(),
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.mode(),
                    metadata.len(),
                    if metadata.is_file() {
                        fs::read(&path).unwrap()
                    } else {
                        Vec::new()
                    },
                ),
            );
            if metadata.is_dir() {
                rows.extend(snapshot(&path));
            }
        }
        rows
    }

    let (f, target, lease, other) = review13_batch_fixture(false);
    review13_quarantine_other(&f, other);
    assert_ne!(target, other);
    // The completed set and its quarantined receipt permit this unrelated target.
    let report =
        purge_quarantined(&inspection(target), &lease, &f.filesystem(), &limits()).unwrap();
    // Establish the target's existing lock diagnostics before corrupting the
    // unrelated member. A changed closure digest refuses before purge.
    let mut unconfirmed_evidence = confirmation(&report);
    unconfirmed_evidence
        .reference_closure
        .as_mut()
        .unwrap()
        .catalog_set_sha256 = "0".repeat(64);
    assert!(matches!(
        purge_quarantined(&unconfirmed_evidence, &lease, &f.filesystem(), &limits()),
        Err(RecoveryError::RefusedInput {
            reason: "reference closure changed or administrative evidence incomplete",
            ..
        })
    ));
    let source = fs::metadata(&f.source).unwrap();
    let physical_lock =
        f.backups
            .join(".locks")
            .join(format!("{}:{}.lock", source.dev(), source.ino()));
    assert!(physical_lock.is_file());
    let member = f.backups.join(other.to_string());
    let secret = "PRIVATE_CORRUPT_BATCH_CONTENT_MUST_NOT_APPEAR";
    match case {
        "image" => fs::write(member.join("image.fls"), secret).unwrap(),
        "manifest" => fs::write(member.join("manifest.json"), format!("{{{secret}")).unwrap(),
        "report" => fs::remove_file(member.join("report.json")).unwrap(),
        _ => unreachable!(),
    }
    let before = snapshot(f.root.path());
    let hook = Arc::new(PurgeReplayCut {
        stop: usize::MAX,
        seen: std::sync::atomic::AtomicUsize::new(0),
    });
    let filesystem = HookFs {
        f: &f,
        io: hook.clone(),
        limits: limits(),
    };
    for request in [inspection(target), confirmation(&report)] {
        let error = purge_quarantined(&request, &lease, &filesystem, &limits()).unwrap_err();
        assert_eq!(
            hook.seen.load(Ordering::Acquire),
            0,
            "{case}: attempted unlink"
        );
        let mut after = snapshot(f.root.path());
        if request.confirm.is_some() {
            // StoreLock refreshes this pre-existing physical lock's real-time
            // diagnostic before closure. Only its timestamp may differ; all
            // lock identity/metadata and all other artifact bytes stay exact.
            let old = &before.get(&physical_lock).unwrap().4;
            let new = &mut after.get_mut(&physical_lock).unwrap().4;
            let mut previous: serde_json::Value = serde_json::from_slice(old).unwrap();
            let mut current: serde_json::Value = serde_json::from_slice(new).unwrap();
            for value in [&mut previous, &mut current] {
                let object = value.as_object_mut().unwrap();
                assert_eq!(object.len(), 3);
                assert_eq!(object["operation_id"], target.to_string());
                assert_eq!(object["pid"], std::process::id());
                assert!(
                    object
                        .remove("started_at_unix_ms")
                        .unwrap()
                        .as_u64()
                        .unwrap()
                        > 0
                );
            }
            assert_eq!(
                current, previous,
                "{case}: non-timestamp lock diagnostics changed"
            );
            *new = old.clone();
        }
        assert!(
            after == before,
            "{case}: refusal changed artifacts beyond the confirmed physical-lock timestamp"
        );
        assert!(
            matches!(&error, RecoveryError::MetadataVerification { operation_id, entry, source }
                if *operation_id == target
                    && entry == &PathBuf::from(other.to_string())
                    && matches!(source.as_ref(), RecoveryError::Verification { operation_id } if *operation_id == target)),
            "{case}: missing referenced member diagnostic: {error:?}"
        );
        assert!(
            error.source().and_then(|source| source.downcast_ref::<Box<RecoveryError>>())
                .is_some_and(|source| matches!(source.as_ref(), RecoveryError::Verification { operation_id } if *operation_id == target)),
            "{case}: typed verification cause lost: {error:?}"
        );
        let message = error.to_string();
        assert!(message.contains(&other.to_string()), "{case}: {message}");
        assert!(
            !message.contains(f.backups.to_str().unwrap()),
            "{case}: {message}"
        );
        assert!(!message.contains(secret), "{case}: {message}");
        eprintln!(
            "r4 item7 corrupt={case} confirmed={} member={other} typed_source=true zero_unlinks=true exact_artifacts_except_confirmed_lock_timestamp=true",
            request.confirm.is_some()
        );
    }
}

#[test]
fn purge_r4_completed_batch_corrupt_image_names_member() {
    purge_r4_corrupt_completed_batch_member_diagnostic("image");
}
#[test]
fn purge_r4_completed_batch_truncated_manifest_names_member() {
    purge_r4_corrupt_completed_batch_member_diagnostic("manifest");
}
#[test]
fn purge_r4_completed_batch_missing_report_names_member() {
    purge_r4_corrupt_completed_batch_member_diagnostic("report");
}
