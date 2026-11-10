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
