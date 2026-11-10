//! Dormant owner scheduling: all endpoints and sinks in these fixtures are inert.
use fl_store::recovery::policy::{RecoveryPolicy, Schedule, SchedulerClock};

#[test]
fn defaults_never_enable_recurrence_and_missing_limits_refuse() {
    let policy = RecoveryPolicy::default();
    assert!(!policy.enabled);
    assert!(!policy.coordinator);
    assert!(policy.validate().is_ok());
    assert!(
        serde_json::from_value::<RecoveryPolicy>(serde_json::json!({
            "version": 1, "enabled": true, "coordinator": true
        }))
        .is_err()
    );
}

#[test]
fn utc_ticks_share_batch_and_overdue_does_not_invent_global_sequence() {
    let mut clock = SchedulerClock::default();
    let schedules = vec![
        Schedule {
            store_id: "workspace".into(),
            endpoint: "unix:///fixture/workspace.sock".into(),
            interval_seconds: 3600,
            prospective_bundle_bytes: 32 << 20,
            catalog_identity: "catalog:fixture".into(),
        },
        Schedule {
            store_id: "ferroloop".into(),
            endpoint: "unix:///fixture/ferroloop.sock".into(),
            interval_seconds: 3600,
            prospective_bundle_bytes: 32 << 20,
            catalog_identity: "catalog:fixture".into(),
        },
        Schedule {
            store_id: "thecouncil".into(),
            endpoint: "unix:///fixture/unserved.sock".into(),
            interval_seconds: 21600,
            prospective_bundle_bytes: 32 << 20,
            catalog_identity: "catalog:fixture".into(),
        },
    ];
    assert!(clock.due(&schedules, 3599).is_empty());
    assert_eq!(clock.due(&schedules, 3600), vec!["workspace", "ferroloop"]);
    clock.admit(&["workspace".into(), "ferroloop".into()], 3600);
    assert!(clock.due(&schedules, 3600).is_empty());
    assert_eq!(clock.due(&schedules, 21600).len(), 3);
    let restored: SchedulerClock =
        serde_json::from_slice(&serde_json::to_vec(&clock).unwrap()).unwrap();
    assert_eq!(
        restored.due(&schedules, 21600),
        clock.due(&schedules, 21600)
    );
}

#[test]
fn owner_controls_default_to_disabled_and_keep_data_listener_available() {
    let root = tempfile::tempdir().unwrap();
    let store = fl_store::FerroStore::in_memory();
    let socket = root.path().join("owner.sock");
    let mut server = fl_store::FerroStoreServer::bind_unix(&socket, store).unwrap();
    let client =
        fl_store::FerroStore::connect_uri(&format!("unix://{}", socket.display())).unwrap();
    let request = fl_store::recovery::control::BatchRequest {
        policy_id: "fixture".into(),
        batch_id: uuid::Uuid::now_v7(),
        members: vec![fl_store::recovery::control::MemberRequest {
            store_id: "workspace".into(),
            operation_id: uuid::Uuid::now_v7(),
            scheduled_at_unix_seconds: None,
            byte_cap: None,
        }],
    };
    assert!(
        client
            .recovery_control("recovery_start", &request)
            .unwrap_err()
            .to_string()
            .contains("recovery policy is absent")
    );
    client
        .put_doc("fixture", "after", &serde_json::json!({"ok":true}))
        .unwrap();
    assert!(client.get_doc("fixture", "after").unwrap().is_some());
    server.shutdown().unwrap();
}

#[cfg(target_os = "linux")]
mod owner_fixtures {
    use super::*;
    use fl_store::recovery::{
        ProfileKind, ValidationProfile,
        control::{BatchRequest, MemberRequest, MemberStatus},
        fs::{Member, RecoveryEvent, RecoveryIo},
        inventory::{BindingInput, InventoryInputs, collect_inventory},
        policy::RecoveryLimits,
    };
    use fl_store::{FerroStore, FerroStoreServer};
    use std::{
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };
    struct Fixture {
        root: tempfile::TempDir,
        source: PathBuf,
        destination: PathBuf,
        policy: RecoveryPolicy,
    }
    impl Fixture {
        fn new() -> (Self, FerroStore) {
            let root = tempfile::tempdir_in(
                std::env::var_os("FL_RECOVERY_TEST_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir),
            )
            .unwrap();
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let source = root.path().join("source.fls");
            let destination = root.path().join("backups");
            std::fs::create_dir(&destination).unwrap();
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700)).unwrap();
            let store = FerroStore::open(&source).unwrap();
            for n in 0..24 {
                store
                    .put_doc(
                        "fixture",
                        "one",
                        &serde_json::json!({"n":n,"body":"x".repeat(4096)}),
                    )
                    .unwrap();
            }
            let inventory = collect_inventory(&InventoryInputs {
                roots: vec![root.path().into()],
                bindings: vec![BindingInput::config(
                    "workspace",
                    source.clone(),
                    Some("ferro".into()),
                    "fixture",
                )],
                ..Default::default()
            })
            .unwrap();
            let identity = inventory
                .stores
                .iter()
                .find(|s| s.identity.canonical_path.as_ref() == Some(&source))
                .unwrap()
                .identity
                .clone();
            let policy = RecoveryPolicy {
                policy_id: "fixture".into(),
                store_id: "workspace".into(),
                destination: destination.clone(),
                source_map_sha256: "a".repeat(64),
                profile: ValidationProfile::new(ProfileKind::SmallEmpty, identity.catalog_key),
                limits: Some(RecoveryLimits {
                    retained_pin_bytes: 64 << 20,
                    preparation_bytes: 128 << 20,
                    memory_bytes: 256 << 20,
                    disk_bytes: 64 << 20,
                    image_bytes: 16 << 10,
                    metadata_bytes: 128 << 10,
                    margin_bytes: 4096,
                    total_seconds: 30,
                }),
                ..RecoveryPolicy::default()
            };
            (
                Self {
                    root,
                    source,
                    destination,
                    policy,
                },
                store,
            )
        }
        fn request(&self) -> BatchRequest {
            BatchRequest {
                policy_id: self.policy.policy_id.clone(),
                batch_id: uuid::Uuid::now_v7(),
                members: vec![MemberRequest {
                    store_id: self.policy.store_id.clone(),
                    operation_id: uuid::Uuid::now_v7(),
                    scheduled_at_unix_seconds: None,
                    byte_cap: None,
                }],
            }
        }
        fn socket(&self) -> PathBuf {
            self.root.path().join("owner.sock")
        }
        fn client(&self) -> FerroStore {
            FerroStore::connect_uri(&format!("unix://{}", self.socket().display())).unwrap()
        }
    }
    fn finished(
        client: &FerroStore,
        request: &BatchRequest,
    ) -> fl_store::recovery::control::BatchReport {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let report = client.recovery_control("recovery_status", request).unwrap();
            if report.members.iter().all(|m| {
                !matches!(
                    m.status,
                    MemberStatus::Running | MemberStatus::OutcomeUnknown
                )
            }) {
                return report;
            }
            assert!(
                Instant::now() < deadline,
                "bounded recovery fixture timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    struct Slow {
        event: RecoveryEvent,
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        once: AtomicBool,
    }
    impl RecoveryIo for Slow {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event == self.event && !self.once.swap(true, Ordering::SeqCst) {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            }
            Ok(())
        }
    }
    #[test]
    fn slow_encode_and_verify_keep_handshakes_reads_writes_and_duplicate_admission_available() {
        for event in [
            RecoveryEvent::Write(Member::Image),
            RecoveryEvent::Create(Member::VerificationCopy),
        ] {
            let (f, owner) = Fixture::new();
            let (entered_tx, entered) = mpsc::sync_channel(1);
            let (release, release_rx) = mpsc::sync_channel(1);
            let io = Arc::new(Slow {
                event,
                entered: entered_tx,
                release: Mutex::new(release_rx),
                once: AtomicBool::new(false),
            });
            let mut server = FerroStoreServer::bind_unix(f.socket(), owner).unwrap();
            server
                .configure_recovery_with_io(f.policy.clone(), io)
                .unwrap();
            let client = f.client();
            let request = f.request();
            client.recovery_control("recovery_start", &request).unwrap();
            entered.recv_timeout(Duration::from_secs(10)).unwrap();
            let second = f.client();
            second
                .put_doc("fixture", "during", &serde_json::json!({"ok":true}))
                .unwrap();
            assert!(second.get_doc("fixture", "during").unwrap().is_some());
            let repeated = client.recovery_control("recovery_start", &request).unwrap();
            assert_eq!(
                repeated.members[0].operation_id,
                request.members[0].operation_id
            );
            assert!(
                client
                    .recovery_control("recovery_start", &f.request())
                    .is_err()
            );
            release.send(()).unwrap();
            let report = finished(&client, &request);
            assert!(report.complete, "{report:?}");
            assert!(
                report.overdue,
                "disabled recurrence must stay visibly overdue"
            );
            drop(second);
            drop(client);
            server.shutdown().unwrap();
        }
    }
    struct CaptureCount {
        count: AtomicUsize,
        fail: bool,
    }
    impl RecoveryIo for CaptureCount {
        fn before(&self, event: RecoveryEvent) -> std::io::Result<()> {
            if event == RecoveryEvent::Capture {
                self.count.fetch_add(1, Ordering::SeqCst);
                if self.fail {
                    return Err(std::io::Error::other("fixture capture refusal"));
                }
            }
            Ok(())
        }
    }
    #[test]
    fn capture_failure_keeps_data_users_available_and_does_not_retry_on_poll() {
        let (f, owner) = Fixture::new();
        let io = Arc::new(CaptureCount {
            count: AtomicUsize::new(0),
            fail: true,
        });
        let mut server = FerroStoreServer::bind_unix(f.socket(), owner).unwrap();
        server
            .configure_recovery_with_io(f.policy.clone(), io.clone())
            .unwrap();
        let client = f.client();
        let request = f.request();
        client.recovery_control("recovery_start", &request).unwrap();
        assert!(!finished(&client, &request).complete);
        client.recovery_control("recovery_start", &request).unwrap();
        for _ in 0..3 {
            assert!(
                !client
                    .recovery_control("recovery_status", &request)
                    .unwrap()
                    .complete
            );
        }
        client
            .put_doc("fixture", "after", &serde_json::json!({"ok":true}))
            .unwrap();
        assert!(client.get_doc("fixture", "after").unwrap().is_some());
        assert_eq!(io.count.load(Ordering::SeqCst), 1);
        drop(client);
        server.shutdown().unwrap();
    }
    #[test]
    fn restart_reconciles_the_same_operation_without_a_second_capture() {
        let (f, owner) = Fixture::new();
        let request = f.request();
        let mut server = FerroStoreServer::bind_unix(f.socket(), owner).unwrap();
        server.configure_recovery(f.policy.clone()).unwrap();
        let client = f.client();
        client.recovery_control("recovery_start", &request).unwrap();
        let original = finished(&client, &request);
        assert!(original.complete);
        drop(client);
        server.shutdown().unwrap();
        drop(server);
        let journal_path = std::fs::read_dir(&f.destination)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".scheduler-")
                    && p.extension().is_some_and(|s| s == "json")
            })
            .unwrap();
        let mut journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
        journal["batches"][request.batch_id.to_string()]["members"][0]["status"] = "running".into();
        journal["batches"][request.batch_id.to_string()]["complete"] = false.into();
        std::fs::write(journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
        let io = Arc::new(CaptureCount {
            count: AtomicUsize::new(0),
            fail: true,
        });
        let owner = FerroStore::open(&f.source).unwrap();
        let mut server = FerroStoreServer::bind_unix(f.socket(), owner).unwrap();
        server
            .configure_recovery_with_io(f.policy.clone(), io.clone())
            .unwrap();
        let client = f.client();
        let restored = finished(&client, &request);
        assert!(restored.complete);
        assert_eq!(restored.members[0].sequence, original.members[0].sequence);
        assert_eq!(io.count.load(Ordering::SeqCst), 0);
        drop(client);
        server.shutdown().unwrap();
    }
    #[test]
    fn coordinator_publishes_only_closed_durable_sets_and_configs_once_per_batch() {
        use fl_store::recovery::{
            backup::{ApprovedConfig, ExcludedVaultKey},
            manifest::MemberKind,
            profiles::{DependencyPolicy, ReferenceDependency},
        };
        use std::os::unix::fs::MetadataExt;
        for mode in ["complete", "unserved_council", "sequence_mismatch"] {
            let (mut workspace, owner_workspace) = Fixture::new();
            let (mut project, owner_project) = Fixture::new();
            project.policy.store_id = "project".into();
            project.policy.destination = workspace.destination.clone();
            workspace.policy.coordinator = true;
            workspace.policy.members = vec![
                Schedule {
                    store_id: "workspace".into(),
                    endpoint: format!("unix://{}", workspace.socket().display()),
                    interval_seconds: 3600,
                    prospective_bundle_bytes: 32 << 20,
                    catalog_identity: workspace.policy.profile.catalog_identity.clone(),
                },
                Schedule {
                    store_id: "project".into(),
                    endpoint: format!("unix://{}", project.socket().display()),
                    interval_seconds: 3600,
                    prospective_bundle_bytes: 32 << 20,
                    catalog_identity: project.policy.profile.catalog_identity.clone(),
                },
            ];
            if mode == "unserved_council" {
                workspace.policy.members.push(Schedule {
                    store_id: "council".into(),
                    endpoint: format!(
                        "unix://{}/council-unserved.sock",
                        workspace.root.path().display()
                    ),
                    interval_seconds: 21600,
                    prospective_bundle_bytes: 32 << 20,
                    catalog_identity: "catalog:council".into(),
                });
            }
            workspace
                .policy
                .profile
                .dependencies
                .push(ReferenceDependency {
                    reference_sha256: "a".repeat(64),
                    catalog_identity: project.policy.profile.catalog_identity.clone(),
                    required_sequence: Some(
                        owner_project.stats().unwrap().last_seq
                            + u64::from(mode == "sequence_mismatch"),
                    ),
                    policy: DependencyPolicy::CoordinatedRecoveryRequired,
                });
            let key = workspace.root.path().join("vault.key");
            std::fs::write(&key, b"EXCLUDED-FIXTURE-VAULT-KEY").unwrap();
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
            let km = std::fs::metadata(&key).unwrap();
            workspace.policy.excluded_vault_key = Some(ExcludedVaultKey {
                path: key,
                identity: (km.dev(), km.ino()),
            });
            for (name, kind, bytes) in [
                (
                    "config.toml",
                    MemberKind::ProtectedConfig,
                    b"fixture=true".as_slice(),
                ),
                ("warrants.json", MemberKind::Warrants, b"{}".as_slice()),
            ] {
                let path = workspace.root.path().join(name);
                std::fs::write(&path, bytes).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
                let m = std::fs::metadata(&path).unwrap();
                workspace.policy.configs.push(ApprovedConfig {
                    kind,
                    path,
                    identity: (m.dev(), m.ino()),
                    byte_limit: 4096,
                });
            }
            let mut server_project =
                FerroStoreServer::bind_unix(project.socket(), owner_project).unwrap();
            server_project
                .configure_recovery(project.policy.clone())
                .unwrap();
            let mut server_workspace =
                FerroStoreServer::bind_unix(workspace.socket(), owner_workspace).unwrap();
            server_workspace
                .configure_recovery(workspace.policy.clone())
                .unwrap();
            let request = BatchRequest {
                policy_id: "fixture".into(),
                batch_id: uuid::Uuid::now_v7(),
                members: workspace
                    .policy
                    .members
                    .iter()
                    .map(|m| MemberRequest {
                        store_id: m.store_id.clone(),
                        operation_id: uuid::Uuid::now_v7(),
                        scheduled_at_unix_seconds: None,
                        byte_cap: None,
                    })
                    .collect(),
            };
            let client = workspace.client();
            client.recovery_control("recovery_start", &request).unwrap();
            let report = finished(&client, &request);
            assert_eq!(report.complete, mode == "complete", "{mode}: {report:?}");
            assert_eq!(
                report
                    .members
                    .iter()
                    .filter(|m| m.status == MemberStatus::Durable)
                    .count(),
                2,
                "{mode}: {report:?}"
            );
            assert_eq!(
                workspace
                    .destination
                    .join(format!(".batch-{}.json", request.batch_id))
                    .exists(),
                mode == "complete"
            );
            let object = workspace
                .destination
                .join(".batch-config")
                .join(request.batch_id.to_string());
            assert_eq!(std::fs::read_dir(&object).unwrap().count(), 3);
            assert!(!object.join("vault.key").exists());
            for member in report
                .members
                .iter()
                .filter(|m| m.status == MemberStatus::Durable)
            {
                let leaf = workspace.destination.join(member.operation_id.to_string());
                assert!(!leaf.join("config.toml").exists());
                assert!(!leaf.join("warrants.json").exists());
                let manifest: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(leaf.join("manifest.json")).unwrap())
                        .unwrap();
                assert_eq!(
                    manifest["shared_config"]["batch_id"],
                    request.batch_id.to_string()
                );
            }
            drop(client);
            server_workspace.shutdown().unwrap();
            server_project.shutdown().unwrap();
        }
    }
    struct OutcomeSink(&'static str);
    impl fl_store::recovery::control::RecoveryEventSink for OutcomeSink {
        fn emit(
            &self,
            _: &fl_store::recovery::control::RecoveryEventEnvelope,
            _: &str,
            deadline: Instant,
        ) -> Result<(), fl_store::recovery::RecoveryError> {
            if self.0 == "timeout" {
                std::thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(100),
                );
                Ok(())
            } else {
                Err(fl_store::recovery::RecoveryError::Refused(
                    "inert destination unavailable".into(),
                ))
            }
        }
    }
    #[test]
    fn real_durable_backup_survives_failed_timed_out_and_full_pending_outbox() {
        use fl_store::recovery::control::{
            DeliveryStatus, EventDelivery, RecoveryEvent as Event, RecoveryEventEnvelope,
            RecoveryEventOutcome, RecoveryOutcomeCode, RecoverySizes,
        };
        for mode in ["failure", "timeout", "full"] {
            let (f, owner) = Fixture::new();
            let mut events = Vec::new();
            let mut deliveries = std::collections::BTreeMap::new();
            for _ in 0..if mode == "full" { 32 } else { 1 } {
                let operation_id = uuid::Uuid::now_v7();
                let sink_id = "inert:fixture".to_owned();
                deliveries.insert(
                    format!("{operation_id}:{sink_id}"),
                    EventDelivery {
                        attempts: 0,
                        status: DeliveryStatus::Pending,
                    },
                );
                events.push(RecoveryEventEnvelope {
                    batch_id: uuid::Uuid::now_v7(),
                    sink_id,
                    event: Event {
                        operation_id,
                        catalog_identity: f.policy.profile.catalog_identity.clone(),
                        outcome: RecoveryEventOutcome {
                            code: RecoveryOutcomeCode::ScheduledOwnerFailure,
                            consecutive_failures: 1,
                        },
                        sizes: RecoverySizes::default(),
                    },
                });
            }
            let name = fl_store::recovery::inventory::metadata_digest(f.policy.store_id.as_bytes());
            let journal_path = f.destination.join(format!(".scheduler-{name}.json"));
            std::fs::write(&journal_path, serde_json::to_vec(&serde_json::json!({"batches": {}, "events":events, "deliveries":deliveries, "outbox_full": mode == "full"})).unwrap()).unwrap();
            std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o600))
                .unwrap();
            let mut server = FerroStoreServer::bind_unix(f.socket(), owner).unwrap();
            server
                .configure_recovery_with_sink(f.policy.clone(), Arc::new(OutcomeSink(mode)))
                .unwrap();
            let client = f.client();
            let request = f.request();
            client.recovery_control("recovery_start", &request).unwrap();
            let report = finished(&client, &request);
            assert!(report.complete, "{mode}: {report:?}");
            assert_eq!(report.members[0].status, MemberStatus::Durable);
            assert_eq!(report.outbox_full, mode == "full");
            client
                .put_doc("fixture", "after_sink", &serde_json::json!({"ok":true}))
                .unwrap();
            assert!(client.get_doc("fixture", "after_sink").unwrap().is_some());
            let journal: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
            assert!(
                journal["deliveries"]
                    .as_object()
                    .unwrap()
                    .values()
                    .all(|d| d["status"] == "pending")
            );
            assert!(
                f.destination
                    .join(".catalog")
                    .join(format!("{}.json", request.members[0].operation_id))
                    .exists()
            );
            drop(client);
            server.shutdown().unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn owner_rejects_a_client_cap_below_its_configured_physical_bound() {
    use fl_store::recovery::control::{BatchRequest, MemberRequest};
    use fl_store::recovery::policy::RecoveryLimits;
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let policy = RecoveryPolicy {
        policy_id: "fixture".into(),
        store_id: "workspace".into(),
        destination: root.path().into(),
        source_map_sha256: "a".repeat(64),
        limits: Some(RecoveryLimits {
            retained_pin_bytes: 1 << 20,
            preparation_bytes: 2 << 20,
            memory_bytes: 32 << 20,
            disk_bytes: 64 << 20,
            image_bytes: 4096,
            metadata_bytes: 128 << 10,
            margin_bytes: 4096,
            total_seconds: 30,
        }),
        ..RecoveryPolicy::default()
    };
    let scheduler = fl_store::recovery::scheduler::RecoveryScheduler::start(
        fl_store::FerroStore::in_memory(),
        policy,
        None,
    )
    .unwrap();
    let request = BatchRequest {
        policy_id: "fixture".into(),
        batch_id: uuid::Uuid::now_v7(),
        members: vec![MemberRequest {
            store_id: "workspace".into(),
            operation_id: uuid::Uuid::now_v7(),
            scheduled_at_unix_seconds: None,
            byte_cap: Some(1),
        }],
    };
    assert!(scheduler.request_batch(&request).is_err());
}
