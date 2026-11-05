//! Owner-scoped FerroMQ runtime buffering.

use super::{MqDurability, MqRecord, MqResourceProbe, MqSystemResourceProbe, MqTierPolicy};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

/// Independent hard limits bound owner-local metadata not included in payload byte accounting.
pub const MQ_MAX_MEMORY_TOPICS: usize = 1024;
pub const MQ_MAX_MEMORY_GROUPS: usize = 4096;
pub const MQ_MAX_MEMORY_ACKS: usize = 8192;
pub const MQ_MAX_NAME_BYTES: usize = 256;

/// Snapshot of owner-local queue telemetry. It is not shared by independent file handles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MqTelemetry {
    pub topics: Vec<MqTopicTelemetry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MqTopicTelemetry {
    pub topic: String,
    pub memory_messages: usize,
    pub memory_bytes: u64,
    pub spilled_messages_total: u64,
    pub acked_in_memory_total: u64,
    pub active_tier: String,
    pub available_ram_bytes: Option<u64>,
    pub free_disk_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub(super) struct Buffered {
    pub record: MqRecord,
    pub durability: MqDurability,
    pub bytes: u64,
    pub at: Instant,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Topic {
    pub high_water: u64,
    pub persisted_high_water: u64,
    pub messages: BTreeMap<u64, Buffered>,
    pub acks: BTreeMap<String, BTreeSet<u64>>,
    pub groups: BTreeSet<String>,
    pub memory_bytes: u64,
    pub spilled_messages_total: u64,
    pub acked_in_memory_total: u64,
}

pub(super) struct Runtime {
    pub policy: MqTierPolicy,
    pub evict_on_first_ack: bool,
    pub probe: Arc<dyn MqResourceProbe>,
    pub topics: BTreeMap<String, Topic>,
    pub busy: bool,
    pub groups: usize,
    pub acks: usize,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            policy: MqTierPolicy::default(),
            evict_on_first_ack: false,
            probe: Arc::new(MqSystemResourceProbe),
            topics: BTreeMap::new(),
            busy: false,
            groups: 0,
            acks: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    struct Resources;
    impl MqResourceProbe for Resources {
        fn probe(&self, _: &Path) -> MqResourceSnapshot {
            MqResourceSnapshot {
                available_ram_bytes: Some(u64::MAX),
                free_disk_bytes: Some(u64::MAX),
            }
        }
    }

    #[test]
    fn stale_high_water_seeds_adaptive_above_every_stored_message_local_and_remote() {
        fn check(store: &FerroStore) {
            store
                .put_doc("mq_hw:events", "high_water", &json!({"seq": 2}))
                .unwrap();
            store
                .put_doc(
                    "mq:events",
                    "5",
                    &json!({"seq":5,"key":"stored","payload":"old"}),
                )
                .unwrap();
            assert_eq!(
                store
                    .publish_mq_with("events", "adaptive", &json!(1), MqDurability::Adaptive)
                    .unwrap(),
                6
            );
            assert_eq!(store.publish_mq("events", "durable", &json!(2)).unwrap(), 7);
            let records = store.poll_mq_group("events", "g", 10).unwrap();
            assert_eq!(
                records.iter().map(|record| record.seq).collect::<Vec<_>>(),
                [5, 6, 7]
            );
            assert_eq!(
                store.get_doc("mq:events", "5").unwrap().unwrap()["key"],
                "stored"
            );
        }
        check(&FerroStore::in_memory());
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mq.sock");
        let _server = FerroStoreServer::bind_unix(&socket, FerroStore::in_memory()).unwrap();
        let remote = FerroStore::connect_uri(&format!("unix://{}", socket.display())).unwrap();
        check(&remote);
    }

    #[test]
    fn adaptive_seed_rejects_corrupt_or_exhausted_high_water() {
        let corrupt = FerroStore::in_memory();
        corrupt
            .put_doc("mq_hw:events", "high_water", &json!({"seq":"invalid"}))
            .unwrap();
        assert!(
            corrupt
                .publish_mq_with("events", "key", &json!(1), MqDurability::Adaptive)
                .is_err()
        );
        let exhausted = FerroStore::in_memory();
        exhausted
            .put_doc("mq_hw:events", "high_water", &json!({"seq":2}))
            .unwrap();
        exhausted
            .put_doc(
                "mq:events",
                &u64::MAX.to_string(),
                &json!({"seq":u64::MAX,"key":"x","payload":null}),
            )
            .unwrap();
        assert!(
            exhausted
                .publish_mq_with("events", "key", &json!(1), MqDurability::Adaptive)
                .is_err()
        );
    }

    #[test]
    fn durable_only_sequence_behavior_is_unchanged() {
        let store = FerroStore::in_memory();
        store
            .put_doc("mq_hw:events", "high_water", &json!({"seq":2}))
            .unwrap();
        store
            .put_doc(
                "mq:events",
                "5",
                &json!({"seq":5,"key":"stored","payload":null}),
            )
            .unwrap();
        assert_eq!(store.publish_mq("events", "durable", &json!(1)).unwrap(), 3);
    }

    struct Adapter;
    impl SyncAdapter for Adapter {
        fn name(&self) -> &str {
            "mq-test"
        }
        fn on_commit(&self, _: &SyncBatch) -> Result<(), StoreError> {
            Ok(())
        }
    }

    #[test]
    fn owner_wide_pressure_spills_the_previously_buffered_topic() {
        let store = FerroStore::in_memory();
        let policy = MqTierPolicy {
            max_memory_messages: 2,
            ..Default::default()
        };
        store
            .configure_mq(policy, false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("a", "first", &json!(1), MqDurability::Adaptive)
            .unwrap();
        store
            .publish_mq_with("b", "second", &json!(2), MqDurability::Adaptive)
            .unwrap();
        let a = store.mq_telemetry(Some("a")).unwrap();
        assert_eq!(a.topics[0].memory_messages, 0);
        assert_eq!(store.poll_mq_group("a", "g", 10).unwrap().len(), 1);
        assert!(store.get_doc("mq:a", "1").unwrap().is_some());
        assert!(store.get_doc("mq:b", "1").unwrap().is_some());
        assert_eq!(
            store.last_seq().unwrap(),
            1,
            "both topics spill in one commit"
        );
    }

    #[test]
    fn ephemeral_pressure_spills_adaptive_topics_but_never_persists_ephemeral_payloads() {
        let store = FerroStore::in_memory();
        let policy = MqTierPolicy {
            max_memory_messages: 3,
            ..Default::default()
        };
        store
            .configure_mq(policy, false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("a", "ephemeral-a", &json!(1), MqDurability::Ephemeral)
            .unwrap();
        store
            .publish_mq_with("b", "adaptive", &json!(2), MqDurability::Adaptive)
            .unwrap();
        store
            .publish_mq_with("c", "ephemeral-c", &json!(3), MqDurability::Ephemeral)
            .unwrap();
        assert!(store.get_doc("mq:b", "1").unwrap().is_some());
        assert!(store.get_doc("mq:a", "1").unwrap().is_none());
        assert!(store.get_doc("mq:c", "1").unwrap().is_none());
        assert!(
            store
                .publish_mq_with("d", "too-many", &json!(4), MqDurability::Ephemeral)
                .is_err()
        );
        assert_eq!(store.poll_mq_group("a", "g", 10).unwrap().len(), 1);
        assert_eq!(store.poll_mq_group("c", "g", 10).unwrap().len(), 1);
        let telemetry = store.mq_telemetry(None).unwrap();
        assert_eq!(
            telemetry
                .topics
                .iter()
                .map(|t| t.memory_messages)
                .sum::<usize>(),
            2
        );
    }

    #[test]
    fn blocked_cross_topic_spill_retains_all_prior_messages() {
        struct Mutable(std::sync::Mutex<MqResourceSnapshot>);
        impl MqResourceProbe for Mutable {
            fn probe(&self, _: &Path) -> MqResourceSnapshot {
                *self.0.lock().unwrap()
            }
        }
        let store = FerroStore::in_memory();
        let probe = Arc::new(Mutable(std::sync::Mutex::new(MqResourceSnapshot {
            available_ram_bytes: Some(u64::MAX),
            free_disk_bytes: Some(u64::MAX),
        })));
        let policy = MqTierPolicy {
            max_memory_messages: 3,
            ..Default::default()
        };
        store.configure_mq(policy, false, probe.clone()).unwrap();
        store
            .publish_mq_with("a", "adaptive", &json!(1), MqDurability::Adaptive)
            .unwrap();
        store
            .publish_mq_with("b", "ephemeral", &json!(2), MqDurability::Ephemeral)
            .unwrap();
        *probe.0.lock().unwrap() = MqResourceSnapshot {
            available_ram_bytes: Some(u64::MAX),
            free_disk_bytes: Some(0),
        };
        assert!(
            store
                .publish_mq_with("c", "blocked", &json!(3), MqDurability::Adaptive)
                .is_err()
        );
        assert_eq!(store.poll_mq_group("a", "g", 10).unwrap().len(), 1);
        assert_eq!(store.poll_mq_group("b", "g", 10).unwrap().len(), 1);
        assert!(store.get_doc("mq:a", "1").unwrap().is_none());
        assert!(store.get_doc("mq:b", "1").unwrap().is_none());
        assert!(store.poll_mq_group("c", "g", 10).unwrap().is_empty());
    }

    #[test]
    fn idle_expired_work_waits_for_explicit_maintenance_and_reads_do_not_write() {
        let store = FerroStore::in_memory();
        let policy = MqTierPolicy {
            max_memory_lag: Duration::from_secs(1),
            ..Default::default()
        };
        store
            .configure_mq(policy, false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("a", "first", &json!(1), MqDurability::Adaptive)
            .unwrap();
        {
            let mut mq = lock_mutex(&store.mq_runtime);
            mq.topics
                .get_mut("a")
                .unwrap()
                .messages
                .get_mut(&1)
                .unwrap()
                .at = std::time::Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap();
        }
        let before = store.last_seq().unwrap();
        assert_eq!(store.poll_mq_group("a", "g", 10).unwrap().len(), 1);
        assert_eq!(
            store.mq_telemetry(Some("a")).unwrap().topics[0].memory_messages,
            1
        );
        assert_eq!(store.last_seq().unwrap(), before);
        assert_eq!(store.maintain_mq().unwrap(), 1);
        assert_eq!(
            store.mq_telemetry(Some("a")).unwrap().topics[0].memory_messages,
            0
        );
        assert!(store.get_doc("mq:a", "1").unwrap().is_some());
    }

    #[test]
    fn polling_an_unseen_group_does_not_mutate_owner_mq_state() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.fls");
        let store = FerroStore::open(&file).unwrap();
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("a", "first", &json!(1), MqDurability::Adaptive)
            .unwrap();
        let before = store.mq_telemetry(Some("a")).unwrap();
        let sequence = store.last_seq().unwrap();
        let wal = wal_length_on_disk(&file);
        assert_eq!(store.poll_mq_group("a", "new-group", 10).unwrap().len(), 1);
        assert_eq!(store.mq_telemetry(Some("a")).unwrap(), before);
        assert_eq!(store.last_seq().unwrap(), sequence);
        assert_eq!(wal_length_on_disk(&file), wal);
        let mq = lock_mutex(&store.mq_runtime);
        assert_eq!(mq.groups, 0);
        assert!(mq.topics["a"].groups.is_empty());
    }

    #[test]
    fn acknowledging_another_topic_maintains_expired_adaptive_work() {
        let store = FerroStore::in_memory();
        let policy = MqTierPolicy {
            max_memory_lag: Duration::from_secs(1),
            ..Default::default()
        };
        store
            .configure_mq(policy, false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("a", "old", &json!(1), MqDurability::Adaptive)
            .unwrap();
        store
            .publish_mq_with("b", "fresh", &json!(2), MqDurability::Adaptive)
            .unwrap();
        {
            let mut mq = lock_mutex(&store.mq_runtime);
            mq.topics
                .get_mut("a")
                .unwrap()
                .messages
                .get_mut(&1)
                .unwrap()
                .at = std::time::Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap();
        }
        store.ack_mq("b", "g", 1).unwrap();
        assert!(store.get_doc("mq:a", "1").unwrap().is_some());
    }

    #[test]
    fn memory_ack_is_not_reported_as_the_unrelated_undelivered_spill() {
        struct Failing;
        impl SyncAdapter for Failing {
            fn name(&self) -> &str {
                "fail"
            }
            fn on_commit(&self, _: &SyncBatch) -> Result<(), StoreError> {
                Err(StoreError::Backend("offline".into()))
            }
        }
        let store = FerroStore::in_memory();
        store
            .configure_mq(
                MqTierPolicy {
                    max_memory_lag: Duration::from_secs(1),
                    ..Default::default()
                },
                false,
                Arc::new(Resources),
            )
            .unwrap();
        let ephemeral = store
            .publish_mq_with("a", "one", &json!(1), MqDurability::Ephemeral)
            .unwrap();
        store
            .publish_mq_with("b", "old", &json!(2), MqDurability::Adaptive)
            .unwrap();
        lock_mutex(&store.mq_runtime)
            .topics
            .get_mut("b")
            .unwrap()
            .messages
            .get_mut(&1)
            .unwrap()
            .at = std::time::Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap();
        store
            .attach_sync_adapter(SyncMode::WriteThrough, Arc::new(Failing))
            .unwrap();
        assert!(matches!(
            store.ack_mq_with_detail("a", "g", ephemeral),
            Ok(None)
        ));
        assert!(
            store
                .get_doc("mq_ack:a:g", &ephemeral.to_string())
                .unwrap()
                .is_none()
        );
        assert!(store.get_doc("mq:b", "1").unwrap().is_some());
        assert!(store.has_pending_sync());
    }

    #[test]
    fn adapter_cannot_attach_during_ephemeral_admission_that_spills_another_topic() {
        struct Failing;
        impl SyncAdapter for Failing {
            fn name(&self) -> &str {
                "fail"
            }
            fn on_commit(&self, _: &SyncBatch) -> Result<(), StoreError> {
                Err(StoreError::Backend("offline".into()))
            }
        }
        let store = Arc::new(FerroStore::in_memory());
        store
            .configure_mq(
                MqTierPolicy {
                    max_memory_messages: 2,
                    ..Default::default()
                },
                false,
                Arc::new(Resources),
            )
            .unwrap();
        store
            .publish_mq_with("a", "old", &json!(1), MqDurability::Adaptive)
            .unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let publisher = Arc::clone(&store);
        let thread = std::thread::spawn(move || {
            BEFORE_MQ_SPILL.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    ready_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                }))
            });
            publisher.publish_mq_with("b", "ephemeral", &json!(2), MqDurability::Ephemeral)
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let attach_result = store.attach_sync_adapter(SyncMode::WriteThrough, Arc::new(Failing));
        resume_tx.send(()).unwrap();
        let result = thread.join().unwrap();
        assert!(
            attach_result.is_err(),
            "adapter attached while non-durable admission was in flight"
        );
        assert_eq!(result.unwrap(), 1);
        assert!(store.get_doc("mq:a", "1").unwrap().is_some());
        assert!(store.get_doc("mq:b", "1").unwrap().is_none());
        assert!(!store.has_pending_sync());
    }

    #[test]
    fn durable_ack_returns_its_own_sequence_when_later_spill_is_undelivered() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct FailSecond(AtomicUsize);
        impl SyncAdapter for FailSecond {
            fn name(&self) -> &str {
                "fail-second"
            }
            fn on_commit(&self, _: &SyncBatch) -> Result<(), StoreError> {
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(())
                } else {
                    Err(StoreError::Backend("offline".into()))
                }
            }
        }
        let store = FerroStore::in_memory();
        store
            .configure_mq(
                MqTierPolicy {
                    max_memory_lag: Duration::from_secs(1),
                    ..Default::default()
                },
                false,
                Arc::new(Resources),
            )
            .unwrap();
        let durable = store.publish_mq("a", "one", &json!(1)).unwrap();
        store
            .publish_mq_with("b", "old", &json!(2), MqDurability::Adaptive)
            .unwrap();
        lock_mutex(&store.mq_runtime)
            .topics
            .get_mut("b")
            .unwrap()
            .messages
            .get_mut(&1)
            .unwrap()
            .at = std::time::Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap();
        let before = store.last_seq().unwrap();
        store
            .attach_sync_adapter(
                SyncMode::WriteThrough,
                Arc::new(FailSecond(AtomicUsize::new(0))),
            )
            .unwrap();
        assert_eq!(
            store.ack_mq_with_detail("a", "g", durable).unwrap(),
            Some(before + 1)
        );
        assert!(
            store
                .get_doc("mq_ack:a:g", &durable.to_string())
                .unwrap()
                .is_some()
        );
        assert!(store.has_pending_sync());
        assert_eq!(store.last_seq().unwrap(), before + 2);
    }

    #[test]
    fn durable_ack_still_reports_its_own_undelivered_commit() {
        struct Failing;
        impl SyncAdapter for Failing {
            fn name(&self) -> &str {
                "fail"
            }
            fn on_commit(&self, _: &SyncBatch) -> Result<(), StoreError> {
                Err(StoreError::Backend("offline".into()))
            }
        }
        let store = FerroStore::in_memory();
        let id = store.publish_mq("a", "one", &json!(1)).unwrap();
        let before = store.last_seq().unwrap();
        store
            .attach_sync_adapter(SyncMode::WriteThrough, Arc::new(Failing))
            .unwrap();
        let error = store.ack_mq_with_detail("a", "g", id).unwrap_err();
        assert!(matches!(error, StoreError::Undelivered { seq, .. } if seq == before + 1));
        assert!(
            store
                .get_doc("mq_ack:a:g", &id.to_string())
                .unwrap()
                .is_some()
        );
        assert!(store.has_pending_sync());
    }

    #[test]
    fn durable_ack_also_maintains_expired_adaptive_work() {
        let store = FerroStore::in_memory();
        let policy = MqTierPolicy {
            max_memory_lag: Duration::from_secs(1),
            ..Default::default()
        };
        store
            .configure_mq(policy, false, Arc::new(Resources))
            .unwrap();
        store.publish_mq("disk", "durable", &json!(1)).unwrap();
        store
            .publish_mq_with("a", "old", &json!(2), MqDurability::Adaptive)
            .unwrap();
        {
            let mut mq = lock_mutex(&store.mq_runtime);
            mq.topics
                .get_mut("a")
                .unwrap()
                .messages
                .get_mut(&1)
                .unwrap()
                .at = std::time::Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap();
        }
        store.ack_mq("disk", "g", 1).unwrap();
        assert!(store.get_doc("mq:a", "1").unwrap().is_some());
    }

    #[test]
    fn maintenance_spills_all_topics_for_low_ram_and_preserves_data_on_low_disk() {
        struct Mutable(std::sync::Mutex<MqResourceSnapshot>);
        impl MqResourceProbe for Mutable {
            fn probe(&self, _: &Path) -> MqResourceSnapshot {
                *self.0.lock().unwrap()
            }
        }
        let store = FerroStore::in_memory();
        let probe = Arc::new(Mutable(std::sync::Mutex::new(MqResourceSnapshot {
            available_ram_bytes: Some(u64::MAX),
            free_disk_bytes: Some(u64::MAX),
        })));
        store
            .configure_mq(MqTierPolicy::default(), false, probe.clone())
            .unwrap();
        store
            .publish_mq_with("a", "one", &json!(1), MqDurability::Adaptive)
            .unwrap();
        store
            .publish_mq_with("b", "two", &json!(2), MqDurability::Adaptive)
            .unwrap();
        *probe.0.lock().unwrap() = MqResourceSnapshot {
            available_ram_bytes: Some(0),
            free_disk_bytes: Some(0),
        };
        assert!(store.maintain_mq().is_err());
        assert_eq!(
            store.mq_telemetry(Some("a")).unwrap().topics[0].memory_messages,
            1
        );
        assert_eq!(
            store.mq_telemetry(Some("b")).unwrap().topics[0].memory_messages,
            1
        );
        *probe.0.lock().unwrap() = MqResourceSnapshot {
            available_ram_bytes: Some(0),
            free_disk_bytes: Some(u64::MAX),
        };
        assert_eq!(store.maintain_mq().unwrap(), 2);
        assert!(store.get_doc("mq:a", "1").unwrap().is_some());
        assert!(store.get_doc("mq:b", "1").unwrap().is_some());
    }

    #[test]
    fn remote_adapter_receives_exact_owner_batch_after_an_adaptive_flush() {
        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<SyncBatch>>);
        impl SyncAdapter for Recorder {
            fn name(&self) -> &str {
                "recorder"
            }
            fn on_commit(&self, batch: &SyncBatch) -> Result<(), StoreError> {
                self.0.lock().unwrap().push(batch.clone());
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mq.sock");
        let owner = FerroStore::in_memory();
        owner
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        owner
            .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
            .unwrap();
        owner.flush_mq(Some("events")).unwrap();
        let owner_capture = Arc::new(Recorder::default());
        owner
            .attach_sync_adapter(SyncMode::WriteThrough, owner_capture.clone())
            .unwrap();
        let _server = FerroStoreServer::bind_unix(&socket, owner).unwrap();
        let client = FerroStore::connect_uri(&format!("unix://{}", socket.display())).unwrap();
        let client_capture = Arc::new(Recorder::default());
        client
            .attach_sync_adapter(SyncMode::WriteThrough, client_capture.clone())
            .unwrap();
        assert_eq!(client.publish_mq("events", "b", &json!(2)).unwrap(), 2);
        assert_eq!(
            *owner_capture.0.lock().unwrap(),
            *client_capture.0.lock().unwrap()
        );
    }

    #[test]
    fn an_inflight_remote_flush_prevents_racing_adapter_attachment() {
        let store = FerroStore::in_memory();
        let claim = store.mq_begin_remote_flush().unwrap();
        assert!(
            store
                .attach_sync_adapter(SyncMode::WriteThrough, Arc::new(Adapter))
                .is_err()
        );
        drop(claim);
        store
            .attach_sync_adapter(SyncMode::WriteThrough, Arc::new(Adapter))
            .unwrap();
        assert!(store.mq_begin_remote_flush().is_err());
    }

    #[test]
    fn guarded_checkpoint_fences_adaptive_admission_until_it_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FerroStore::open(dir.path().join("s.fls")).unwrap());
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        let disk = match &store.inner {
            Inner::Disk(disk) => disk,
            _ => panic!("disk expected"),
        };
        let held = lock_mutex(&disk.write);
        let child = Arc::clone(&store);
        let thread = std::thread::spawn(move || child.checkpoint_if_mq_idle());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !lock_mutex(&store.mq_runtime).busy {
            assert!(
                std::time::Instant::now() < deadline,
                "checkpoint did not claim MQ"
            );
            std::thread::yield_now();
        }
        assert!(
            store
                .publish_mq_with("events", "x", &json!(1), MqDurability::Adaptive)
                .is_err()
        );
        drop(held);
        thread.join().unwrap().unwrap();
        assert_eq!(
            store
                .publish_mq_with("events", "x", &json!(1), MqDurability::Adaptive)
                .unwrap(),
            1
        );
    }

    #[test]
    fn adaptive_publish_and_group_ack_do_not_commit_to_the_store() {
        let store = FerroStore::in_memory();
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        let before = store.stats().unwrap().last_seq;
        let seq = store
            .publish_mq_with("events", "one", &json!({"v":1}), MqDurability::Adaptive)
            .unwrap();
        assert_eq!(seq, 1);
        assert_eq!(store.poll_mq_group("events", "a", 10).unwrap()[0].seq, seq);
        store.ack_mq("events", "a", seq).unwrap();
        assert!(store.poll_mq_group("events", "a", 10).unwrap().is_empty());
        assert_eq!(store.poll_mq_group("events", "b", 10).unwrap()[0].seq, seq);
        assert_eq!(store.stats().unwrap().last_seq, before);
        let metrics = store.mq_telemetry(Some("events")).unwrap();
        assert_eq!(metrics.topics[0].memory_messages, 1);
        assert_eq!(metrics.topics[0].acked_in_memory_total, 1);
    }

    #[test]
    fn telemetry_includes_a_durable_only_topic() {
        let store = FerroStore::in_memory();
        store.publish_mq("disk-only", "x", &json!(1)).unwrap();
        let telemetry = store.mq_telemetry(Some("disk-only")).unwrap();
        assert_eq!(telemetry.topics.len(), 1);
        assert_eq!(telemetry.topics[0].topic, "disk-only");
        assert_eq!(telemetry.topics[0].memory_messages, 0);
        assert_eq!(telemetry.topics[0].active_tier, "disk");
    }

    #[test]
    fn disk_adaptive_publish_and_ack_leave_wal_unchanged_until_flush() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.fls");
        {
            let store = FerroStore::open(&file).unwrap();
            store
                .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
                .unwrap();
            let before_seq = store.stats().unwrap().last_seq;
            let before_wal = wal_length_on_disk(&file);
            let seq = store
                .publish_mq_with("events", "one", &json!({"v":1}), MqDurability::Adaptive)
                .unwrap();
            store.ack_mq("events", "a", seq).unwrap();
            assert_eq!(store.stats().unwrap().last_seq, before_seq);
            assert_eq!(wal_length_on_disk(&file), before_wal);
            assert_eq!(store.flush_mq(None).unwrap(), 1);
            assert!(wal_length_on_disk(&file) > before_wal);
        }
        let reopened = FerroStore::open(&file).unwrap();
        assert!(
            reopened
                .poll_mq_group("events", "a", 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(reopened.poll_mq_group("events", "b", 10).unwrap().len(), 1);
    }

    #[test]
    fn flush_keeps_high_water_and_acks_after_eviction_without_ephemeral_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.fls");
        {
            let store = FerroStore::open(&file).unwrap();
            store
                .configure_mq(MqTierPolicy::default(), true, Arc::new(Resources))
                .unwrap();
            let a = store
                .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
                .unwrap();
            store.ack_mq("events", "g", a).unwrap();
            let e = store
                .publish_mq_with("events", "e", &json!(2), MqDurability::Ephemeral)
                .unwrap();
            store.ack_mq("events", "g", e).unwrap();
            assert_eq!(store.flush_mq(None).unwrap(), 0);
        }
        let reopened = FerroStore::open(&file).unwrap();
        assert!(
            reopened
                .poll_mq_group("events", "other", 10)
                .unwrap()
                .is_empty()
        );
        assert!(reopened.get_doc("mq_ack:events:g", "1").unwrap().is_some());
        assert!(reopened.get_doc("mq_ack:events:g", "2").unwrap().is_some());
        assert_eq!(reopened.publish_mq("events", "next", &json!(3)).unwrap(), 3);
    }

    #[test]
    fn persisted_unknown_ack_also_filters_later_buffered_message() {
        let store = FerroStore::in_memory();
        store.ack_mq("events", "a", 1).unwrap();
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        assert_eq!(
            store
                .publish_mq_with("events", "x", &json!(1), MqDurability::Adaptive)
                .unwrap(),
            1
        );
        assert!(store.poll_mq_group("events", "a", 10).unwrap().is_empty());
        assert_eq!(store.poll_mq_group("events", "b", 10).unwrap().len(), 1);
    }

    #[test]
    fn inbound_sync_refuses_buffered_work_and_fences_concurrent_admission() {
        let store = FerroStore::in_memory();
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("events", "x", &json!(1), MqDurability::Adaptive)
            .unwrap();
        let batch = SyncBatch {
            seq: 1,
            ops: vec![SyncOp::PutDoc {
                collection: "docs".into(),
                id: "x".into(),
                doc: json!(1),
            }],
        };
        assert!(
            store
                .apply_inbound(vec![batch])
                .unwrap_err()
                .to_string()
                .contains("flush_mq")
        );
        assert_eq!(store.stats().unwrap().last_seq, 0);
        store.flush_mq(None).unwrap();
        let claim = store.mq_begin_inbound_sync().unwrap();
        assert!(
            store
                .publish_mq_with("events", "y", &json!(2), MqDurability::Adaptive)
                .is_err()
        );
        drop(claim);
        let batch = SyncBatch {
            seq: store.last_seq().unwrap() + 1,
            ops: vec![SyncOp::PutDoc {
                collection: "docs".into(),
                id: "x".into(),
                doc: json!(1),
            }],
        };
        store.apply_inbound(vec![batch]).unwrap();
        assert_eq!(store.get_doc("docs", "x").unwrap(), Some(json!(1)));
    }

    #[test]
    fn undelivered_spill_is_finalized_once_without_losing_or_duplicating_records() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Failing(AtomicBool);
        impl SyncAdapter for Failing {
            fn name(&self) -> &str {
                "fail-once"
            }
            fn on_commit(&self, _: &SyncBatch) -> Result<(), StoreError> {
                if self.0.load(Ordering::SeqCst) {
                    Err(StoreError::Backend("adapter offline".into()))
                } else {
                    Ok(())
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.fls");
        let store = FerroStore::open(&file).unwrap();
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("events", "x", &json!(1), MqDurability::Adaptive)
            .unwrap();
        let adapter = Arc::new(Failing(AtomicBool::new(true)));
        store
            .attach_sync_adapter(SyncMode::WriteThrough, adapter.clone())
            .unwrap();
        let error = store.flush_mq(None).unwrap_err();
        assert!(matches!(error, StoreError::Undelivered { .. }), "{error:?}");
        assert_eq!(
            store.mq_telemetry(Some("events")).unwrap().topics[0].memory_messages,
            0
        );
        assert_eq!(store.poll_mq_group("events", "g", 10).unwrap().len(), 1);
        let committed_seq = store.last_seq().unwrap();
        assert_eq!(store.flush_mq(None).unwrap(), 0);
        assert_eq!(store.last_seq().unwrap(), committed_seq);
        adapter.0.store(false, Ordering::SeqCst);
        store.sync_flush().unwrap();
        assert_eq!(store.poll_mq_group("events", "g", 10).unwrap().len(), 1);
    }

    #[test]
    fn adapter_after_memory_admission_gets_the_real_combined_adaptive_batch() {
        struct Recording(std::sync::Mutex<Vec<SyncBatch>>);
        impl SyncAdapter for Recording {
            fn name(&self) -> &str {
                "recording"
            }
            fn on_commit(&self, batch: &SyncBatch) -> Result<(), StoreError> {
                self.0.lock().unwrap().push(batch.clone());
                Ok(())
            }
        }
        let store = FerroStore::in_memory();
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
            .unwrap();
        let adapter = Arc::new(Recording(std::sync::Mutex::new(Vec::new())));
        store
            .attach_sync_adapter(SyncMode::WriteThrough, adapter.clone())
            .unwrap();
        assert_eq!(
            store
                .publish_mq_with("events", "b", &json!(2), MqDurability::Adaptive)
                .unwrap(),
            2
        );
        let batches = adapter.0.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].ops.iter().filter(|op| matches!(op, SyncOp::PutDoc { collection, .. } if collection == "mq:events")).count(), 2);
        drop(batches);
        assert!(
            store
                .publish_mq_with("events", "ephemeral", &json!(3), MqDurability::Ephemeral)
                .is_err()
        );
    }

    #[test]
    fn durable_publish_spills_existing_adaptive_work_in_the_same_commit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.fls");
        {
            let store = FerroStore::open(&file).unwrap();
            store
                .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
                .unwrap();
            assert_eq!(
                store
                    .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
                    .unwrap(),
                1
            );
            store.ack_mq("events", "g", 1).unwrap();
            assert_eq!(store.publish_mq("events", "b", &json!(2)).unwrap(), 2);
            assert_eq!(
                store.last_seq().unwrap(),
                1,
                "one commit contains spill and durable publish"
            );
            assert_eq!(
                store
                    .poll_mq_group("events", "g", 10)
                    .unwrap()
                    .iter()
                    .map(|r| r.seq)
                    .collect::<Vec<_>>(),
                vec![2]
            );
        }
        let reopened = FerroStore::open(&file).unwrap();
        assert_eq!(
            reopened.poll_mq_group("events", "other", 10).unwrap().len(),
            2
        );
    }

    #[test]
    fn single_publish_refuses_combined_buffered_commit_before_mutation() {
        let store = FerroStore::in_memory();
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
            .unwrap();
        assert!(
            store
                .publish_mq_single_detail("events", "b", &json!(2))
                .unwrap_err()
                .to_string()
                .contains("flush")
        );
        assert_eq!(store.last_seq().unwrap(), 0);
        assert_eq!(store.poll_mq_group("events", "g", 10).unwrap().len(), 1);
        store.flush_mq(None).unwrap();
        assert_eq!(
            store
                .publish_mq_single_detail("events", "b", &json!(2))
                .unwrap()
                .0,
            2
        );
    }

    #[test]
    fn in_flight_spill_fences_mutation_without_holding_the_mq_mutex_in_callback() {
        struct Blocking {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl SyncAdapter for Blocking {
            fn name(&self) -> &str {
                "blocking"
            }
            fn on_commit(&self, _: &SyncBatch) -> Result<(), StoreError> {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                Ok(())
            }
        }
        let store = Arc::new(FerroStore::in_memory());
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        store
            .attach_sync_adapter(
                SyncMode::WriteThrough,
                Arc::new(Blocking {
                    entered: entered_tx,
                    release: std::sync::Mutex::new(release_rx),
                }),
            )
            .unwrap();
        let worker = Arc::clone(&store);
        let thread = std::thread::spawn(move || worker.flush_mq(None));
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            store.mq_runtime.try_lock().is_ok(),
            "MQ mutex must be released before callback"
        );
        assert!(store.poll_mq_group("events", "g", 10).is_err());
        assert!(
            store
                .publish_mq_with("events", "b", &json!(2), MqDurability::Adaptive)
                .is_err()
        );
        release_tx.send(()).unwrap();
        assert_eq!(thread.join().unwrap().unwrap(), 1);
        assert_eq!(store.poll_mq_group("events", "g", 10).unwrap().len(), 1);
    }

    #[test]
    fn threshold_spill_persists_messages_and_group_acks_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.fls");
        {
            let store = FerroStore::open(&file).unwrap();
            let policy = MqTierPolicy {
                max_memory_messages: 2,
                max_memory_lag: Duration::from_secs(60),
                ..Default::default()
            };
            store
                .configure_mq(policy, false, Arc::new(Resources))
                .unwrap();
            let first = store
                .publish_mq_with("events", "one", &json!(1), MqDurability::Adaptive)
                .unwrap();
            store.ack_mq("events", "a", first).unwrap();
            let second = store
                .publish_mq_with("events", "two", &json!(2), MqDurability::Adaptive)
                .unwrap();
            assert_eq!(second, first + 1);
            assert_eq!(
                store.mq_telemetry(Some("events")).unwrap().topics[0].memory_messages,
                0
            );
            assert_eq!(
                store
                    .poll_mq_group("events", "a", 10)
                    .unwrap()
                    .iter()
                    .map(|r| r.seq)
                    .collect::<Vec<_>>(),
                vec![second]
            );
        }
        let reopened = FerroStore::open(&file).unwrap();
        assert_eq!(
            reopened
                .poll_mq_group("events", "a", 10)
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(reopened.poll_mq_group("events", "b", 10).unwrap().len(), 2);
    }

    #[test]
    fn byte_watermark_spills_the_message() {
        let store = FerroStore::in_memory();
        let policy = MqTierPolicy {
            max_memory_bytes: 6,
            ..Default::default()
        };
        store
            .configure_mq(policy, false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("events", "k", &json!("abcd"), MqDurability::Adaptive)
            .unwrap();
        assert_eq!(
            store.mq_telemetry(Some("events")).unwrap().topics[0].memory_messages,
            0
        );
        assert_eq!(store.poll_mq_group("events", "g", 10).unwrap().len(), 1);
    }

    #[test]
    fn memory_watermarks_apply_across_distinct_topics() {
        let store = FerroStore::in_memory();
        let policy = MqTierPolicy {
            max_memory_messages: 3,
            ..Default::default()
        };
        store
            .configure_mq(policy, false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("a", "k", &json!(1), MqDurability::Adaptive)
            .unwrap();
        store
            .publish_mq_with("b", "k", &json!(2), MqDurability::Adaptive)
            .unwrap();
        store
            .publish_mq_with("c", "k", &json!(3), MqDurability::Adaptive)
            .unwrap();
        let telemetry = store.mq_telemetry(None).unwrap();
        assert_eq!(
            telemetry
                .topics
                .iter()
                .map(|topic| topic.memory_messages)
                .sum::<usize>(),
            0
        );
        assert_eq!(store.poll_mq_group("a", "g", 10).unwrap().len(), 1);
        assert_eq!(store.poll_mq_group("b", "g", 10).unwrap().len(), 1);
        assert_eq!(store.poll_mq_group("c", "g", 10).unwrap().len(), 1);
    }

    #[test]
    fn metadata_caps_refuse_growth_without_restricting_durable_topics() {
        let store = FerroStore::in_memory();
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        let long = "x".repeat(MQ_MAX_NAME_BYTES + 1);
        assert!(
            store
                .publish_mq_with(&long, "k", &json!(1), MqDurability::Adaptive)
                .is_err()
        );
        store
            .publish_mq_with("events", "k", &json!(1), MqDurability::Adaptive)
            .unwrap();
        assert!(store.ack_mq("events", &long, 1).is_err());
        // Unknown ACKs and Durable-only topic names keep the pre-tier behavior.
        store.ack_mq("events", &long, 99).unwrap();
        assert_eq!(store.publish_mq(&long, "k", &json!(2)).unwrap(), 1);
        for n in 0..MQ_MAX_MEMORY_GROUPS {
            store.ack_mq("events", &format!("g{n}"), 1).unwrap();
        }
        assert_eq!(
            store
                .poll_mq_group("events", "one-too-many", 1)
                .unwrap()
                .len(),
            1
        );
        assert!(store.ack_mq("events", "one-too-many", 1).is_err());
    }

    #[test]
    fn topic_cap_bounds_adaptive_cardinality_but_not_durable_publish() {
        let store = FerroStore::in_memory();
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        for n in 0..MQ_MAX_MEMORY_TOPICS {
            store
                .publish_mq_with(
                    &format!("topic-{n}"),
                    "k",
                    &json!(1),
                    MqDurability::Adaptive,
                )
                .unwrap();
        }
        assert!(
            store
                .publish_mq_with("one-too-many", "k", &json!(1), MqDurability::Adaptive)
                .unwrap_err()
                .to_string()
                .contains("topic limit")
        );
        assert_eq!(store.publish_mq("durable-only", "k", &json!(1)).unwrap(), 1);
    }

    #[test]
    fn ack_metadata_cap_is_released_after_flush() {
        let store = FerroStore::in_memory();
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Resources))
            .unwrap();
        for n in 0..3 {
            store
                .publish_mq_with("events", "k", &json!(n), MqDurability::Adaptive)
                .unwrap();
        }
        for n in 0..(MQ_MAX_MEMORY_ACKS / 3) {
            for seq in 1..=3 {
                store.ack_mq("events", &format!("g{n}"), seq).unwrap();
            }
        }
        let last = format!("g{}", MQ_MAX_MEMORY_ACKS / 3);
        store.ack_mq("events", &last, 1).unwrap();
        store.ack_mq("events", &last, 2).unwrap();
        assert!(
            store
                .ack_mq("events", &last, 3)
                .unwrap_err()
                .to_string()
                .contains("ACK limit")
        );
        store.flush_mq(None).unwrap();
        let next = store
            .publish_mq_with("events", "k", &json!(4), MqDurability::Adaptive)
            .unwrap();
        store.ack_mq("events", "after-flush", next).unwrap();
    }

    #[test]
    fn oldest_unacked_lag_triggers_spill_without_sleeping() {
        let store = FerroStore::in_memory();
        let policy = MqTierPolicy {
            max_memory_lag: Duration::from_secs(1),
            ..Default::default()
        };
        store
            .configure_mq(policy, false, Arc::new(Resources))
            .unwrap();
        store
            .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
            .unwrap();
        {
            let mut mq = lock_mutex(&store.mq_runtime);
            mq.topics
                .get_mut("events")
                .unwrap()
                .messages
                .get_mut(&1)
                .unwrap()
                .at = std::time::Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap();
        }
        store
            .publish_mq_with("events", "b", &json!(2), MqDurability::Adaptive)
            .unwrap();
        assert_eq!(
            store.mq_telemetry(Some("events")).unwrap().topics[0].memory_messages,
            0
        );
    }

    #[test]
    fn low_ram_spills_and_low_or_unknown_disk_refuses_without_losing_buffer() {
        struct Variable(std::sync::Mutex<MqResourceSnapshot>);
        impl MqResourceProbe for Variable {
            fn probe(&self, _: &Path) -> MqResourceSnapshot {
                *self.0.lock().unwrap()
            }
        }
        let resources = Arc::new(Variable(std::sync::Mutex::new(MqResourceSnapshot {
            available_ram_bytes: Some(100),
            free_disk_bytes: Some(200),
        })));
        let store = FerroStore::in_memory();
        let policy = MqTierPolicy {
            min_available_ram_bytes: 50,
            min_free_disk_bytes: 100,
            ..Default::default()
        };
        store
            .configure_mq(policy, false, resources.clone())
            .unwrap();
        assert_eq!(
            store
                .publish_mq_with("events", "a", &json!(1), MqDurability::Adaptive)
                .unwrap(),
            1
        );
        *resources.0.lock().unwrap() = MqResourceSnapshot {
            available_ram_bytes: Some(49),
            free_disk_bytes: None,
        };
        for _ in 0..3 {
            assert!(
                store
                    .publish_mq_with("events", "b", &json!(2), MqDurability::Adaptive)
                    .is_err()
            );
            assert_eq!(
                store.mq_telemetry(Some("events")).unwrap().topics[0].memory_messages,
                1
            );
            assert_eq!(store.poll_mq_group("events", "g", 10).unwrap()[0].seq, 1);
        }
        *resources.0.lock().unwrap() = MqResourceSnapshot {
            available_ram_bytes: Some(49),
            free_disk_bytes: Some(100),
        };
        assert!(
            store
                .publish_mq_with("events", "b", &json!(2), MqDurability::Adaptive)
                .is_err()
        );
        *resources.0.lock().unwrap() = MqResourceSnapshot {
            available_ram_bytes: Some(49),
            free_disk_bytes: Some(101),
        };
        assert_eq!(
            store
                .publish_mq_with("events", "b", &json!(2), MqDurability::Adaptive)
                .unwrap(),
            2
        );
        assert_eq!(
            store.mq_telemetry(Some("events")).unwrap().topics[0].memory_messages,
            0
        );
    }

    #[test]
    fn ephemeral_pressure_is_bounded_and_never_spills_its_payload() {
        let store = FerroStore::in_memory();
        let policy = MqTierPolicy {
            max_memory_messages: 2,
            ..Default::default()
        };
        store
            .configure_mq(policy, false, Arc::new(Resources))
            .unwrap();
        assert_eq!(
            store
                .publish_mq_with("events", "a", &json!(1), MqDurability::Ephemeral)
                .unwrap(),
            1
        );
        for _ in 0..3 {
            assert!(
                store
                    .publish_mq_with("events", "b", &json!(2), MqDurability::Ephemeral)
                    .is_err()
            );
            assert_eq!(
                store.mq_telemetry(Some("events")).unwrap().topics[0].memory_messages,
                1
            );
        }
        assert_eq!(store.stats().unwrap().last_seq, 0);
        store.flush_mq(None).unwrap();
        assert!(store.get_doc("mq:events", "1").unwrap().is_none());
    }
}
