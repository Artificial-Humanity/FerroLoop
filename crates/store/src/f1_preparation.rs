//! Fail-closed native F1 preparation boundary.
//!
//! Native stores do not persist F1 preparation records. Remote Ferro rejects every
//! operation before I/O; local native stores refuse preparation and authority methods.
use crate::{AnyStore, FerroStore, RedbStore};
use fl_core::f1_records::{
    AllocationEvidenceRecord, F1PreparationStore, F1RecordError, IntentEvidenceRecord,
    RoutineProposalRecord, SafetyBatch,
};

macro_rules! unsupported_impl {
    ($type:ty, $error:expr, $authority:expr) => {
        impl F1PreparationStore for $type {
            fn routine(&self, _: &str) -> Result<Option<RoutineProposalRecord>, F1RecordError> {
                Err($error(self))
            }
            fn intent(&self, _: &str) -> Result<Option<IntentEvidenceRecord>, F1RecordError> {
                Err($error(self))
            }
            fn allocation(
                &self,
                _: &str,
            ) -> Result<Option<AllocationEvidenceRecord>, F1RecordError> {
                Err($error(self))
            }
            fn insert_proposal(&self, _: RoutineProposalRecord) -> Result<(), F1RecordError> {
                Err($error(self))
            }
            fn insert_intent(&self, _: IntentEvidenceRecord) -> Result<(), F1RecordError> {
                Err($error(self))
            }
            fn insert_allocation(&self, _: AllocationEvidenceRecord) -> Result<(), F1RecordError> {
                Err($error(self))
            }
            fn apply_safety_batch(&self, _: &SafetyBatch) -> Result<(), F1RecordError> {
                Err($error(self))
            }
            fn pause_routine(&self, _: &str, _: u64, _: &str) -> Result<(), F1RecordError> {
                Err($error(self))
            }
            fn quarantine_intent(&self, _: &str, _: u64) -> Result<(), F1RecordError> {
                Err($error(self))
            }
            fn request_allocation_stop(&self, _: &str, _: u64) -> Result<(), F1RecordError> {
                Err($error(self))
            }
            fn admit(&self) -> Result<(), F1RecordError> {
                Err($authority(self))
            }
            fn accept_effect(&self) -> Result<(), F1RecordError> {
                Err($authority(self))
            }
            fn settle(&self) -> Result<(), F1RecordError> {
                Err($authority(self))
            }
        }
    };
}
fn local(_: &RedbStore) -> F1RecordError {
    F1RecordError::UnsupportedLocal
}
fn local_authority(_: &RedbStore) -> F1RecordError {
    F1RecordError::UnverifiedAuthority
}
fn ferro(s: &FerroStore) -> F1RecordError {
    if s.is_remote() {
        F1RecordError::UnsupportedRemote
    } else {
        F1RecordError::UnsupportedLocal
    }
}
fn ferro_authority(s: &FerroStore) -> F1RecordError {
    if s.is_remote() {
        F1RecordError::UnsupportedRemote
    } else {
        F1RecordError::UnverifiedAuthority
    }
}
fn any(s: &AnyStore) -> F1RecordError {
    if s.is_remote() {
        F1RecordError::UnsupportedRemote
    } else {
        F1RecordError::UnsupportedLocal
    }
}
fn any_authority(s: &AnyStore) -> F1RecordError {
    if s.is_remote() {
        F1RecordError::UnsupportedRemote
    } else {
        F1RecordError::UnverifiedAuthority
    }
}
unsupported_impl!(RedbStore, local, local_authority);
unsupported_impl!(FerroStore, ferro, ferro_authority);
unsupported_impl!(AnyStore, any, any_authority);

#[cfg(test)]
mod tests {
    use crate::{AnyStore, FerroStore, FerroStoreServer, RedbStore};
    use fl_core::f1_records::{
        AllocationEvidenceRecord, F1PreparationStore, F1RecordError, SafetyBatch, SafetyState,
    };

    fn allocation() -> AllocationEvidenceRecord {
        AllocationEvidenceRecord {
            version: 1,
            allocation_id: "allocation".into(),
            parent_id: None,
            root_id: "allocation".into(),
            source_window_id: "window".into(),
            deadline_utc: "2030-01-01T00:00:00Z".into(),
            tokens: 10,
            cost_micros: 10,
            safety: SafetyState::Inert,
        }
    }

    fn routine() -> fl_core::f1_records::RoutineProposalRecord {
        use fl_core::routine::{
            Allocation, CatchUp, Concurrency, RoutineSpec, RoutineState, RoutineWork, Schedule,
            Window,
        };
        fl_core::f1_records::RoutineProposalRecord {
            version: 1,
            spec: RoutineSpec {
                routine_id: "routine".into(),
                revision: 1,
                project: "p".into(),
                schedule: Schedule::Interval {
                    every_secs: 300,
                    anchor_utc: "2030-01-01T00:00:00Z".into(),
                },
                work: RoutineWork::ExecutableOnly {
                    gate_or_hook_id: "gate".into(),
                },
                allocation: Allocation {
                    per_firing_tokens: 1,
                    per_firing_cost_micros: 1,
                    window: Window::Day,
                    window_tokens: 1,
                    window_cost_micros: 1,
                },
                concurrency: Concurrency::Forbid,
                catch_up: CatchUp::Skip,
                misfire_grace_secs: 300,
                max_firing_secs: 3600,
                max_firings_per_day: 24,
                expires_at: "2030-03-01T00:00:00Z".into(),
                state: RoutineState::Proposed,
                approval: None,
            },
        }
    }
    fn intent() -> fl_core::f1_records::IntentEvidenceRecord {
        use fl_core::f1_records::{IntentDefinition, IntentEvidenceRecord};
        use fl_core::intent::{
            AllocationAmount, Binding, CommitteeIntent, DelegateTarget, QuorumRule, ResultSchema,
        };
        IntentEvidenceRecord {
            version: 1,
            safety: SafetyState::Inert,
            definition: IntentDefinition::Committee(CommitteeIntent {
                intent_id: "intent".into(),
                delegator: Binding {
                    incarnation_id: "inc".into(),
                    thread_epoch: 1,
                    controller_lease_id: "lease".into(),
                },
                members: vec![
                    DelegateTarget::BoundIncarnation {
                        incarnation_id: "a".into(),
                    },
                    DelegateTarget::BoundIncarnation {
                        incarnation_id: "b".into(),
                    },
                ],
                per_member: AllocationAmount {
                    tokens: 1,
                    cost_micros: 1,
                },
                quorum: QuorumRule::All,
                deadline_utc: "2030-01-01T00:00:00Z".into(),
                result_schema: ResultSchema::Verdict,
            }),
        }
    }

    #[test]
    fn native_preparation_is_unsupported_and_does_not_change_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native.redb");
        let redb = AnyStore::from(RedbStore::open(&path).unwrap());
        let ferro_path = dir.path().join("native.fls");
        let ferro = AnyStore::from(FerroStore::open(&ferro_path).unwrap());
        let before_format = redb.format_version().unwrap();
        let before_seq = FerroStore::stats_at(&ferro_path).unwrap().last_seq;
        for store in [&redb, &ferro] {
            for target in [
                store as &dyn F1PreparationStore,
                store
                    .as_redb()
                    .map(|s| s as &dyn F1PreparationStore)
                    .or_else(|| store.as_ferro().map(|s| s as &dyn F1PreparationStore))
                    .unwrap(),
            ] {
                assert_eq!(
                    target.routine("routine"),
                    Err(F1RecordError::UnsupportedLocal)
                );
                assert_eq!(
                    target.intent("intent"),
                    Err(F1RecordError::UnsupportedLocal)
                );
                assert_eq!(
                    target.allocation("allocation"),
                    Err(F1RecordError::UnsupportedLocal)
                );
                assert_eq!(
                    target.insert_proposal(routine()),
                    Err(F1RecordError::UnsupportedLocal)
                );
                assert_eq!(
                    target.insert_intent(intent()),
                    Err(F1RecordError::UnsupportedLocal)
                );
                assert_eq!(
                    target.insert_allocation(allocation()),
                    Err(F1RecordError::UnsupportedLocal)
                );
                assert_eq!(
                    target.apply_safety_batch(&SafetyBatch { changes: vec![] }),
                    Err(F1RecordError::UnsupportedLocal)
                );
                assert_eq!(
                    target.pause_routine("r", 1, "stop"),
                    Err(F1RecordError::UnsupportedLocal)
                );
                assert_eq!(
                    target.quarantine_intent("i", 1),
                    Err(F1RecordError::UnsupportedLocal)
                );
                assert_eq!(
                    target.request_allocation_stop("a", 1),
                    Err(F1RecordError::UnsupportedLocal)
                );
                assert_eq!(target.admit(), Err(F1RecordError::UnverifiedAuthority));
                assert_eq!(
                    target.accept_effect(),
                    Err(F1RecordError::UnverifiedAuthority)
                );
                assert_eq!(target.settle(), Err(F1RecordError::UnverifiedAuthority));
            }
        }
        assert_eq!(redb.format_version().unwrap(), before_format);
        assert_eq!(
            FerroStore::stats_at(&ferro_path).unwrap().last_seq,
            before_seq
        );
    }

    #[test]
    fn migration_refuses_unexpected_f1_table_even_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let source = RedbStore::open(&dir.path().join("source.redb")).unwrap();
        let tx = source.db.begin_write().unwrap();
        drop(
            tx.open_table(redb::TableDefinition::<&str, &str>::new("__f1_records"))
                .unwrap(),
        );
        tx.commit().unwrap();
        let target_path = dir.path().join("target.fls");
        let target = FerroStore::open(&target_path).unwrap();
        let before = FerroStore::stats_at(&target_path).unwrap().last_seq;
        assert!(crate::migrate_redb_to_ferrostore(&source, &target).is_err());
        assert_eq!(FerroStore::stats_at(&target_path).unwrap().last_seq, before);
    }

    #[cfg(unix)]
    #[test]
    fn remote_ferro_refuses_before_read_or_write() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("server.sock");
        let file = tmp.path().join("server.fls");
        let server =
            FerroStoreServer::bind_unix(&socket, FerroStore::open(&file).unwrap()).unwrap();
        let remote = FerroStore::connect_uri(&format!("unix://{}", socket.display())).unwrap();
        // Direct remote engine calls refuse before even validating invalid caller data.
        assert_eq!(remote.routine(""), Err(F1RecordError::UnsupportedRemote));
        assert_eq!(remote.admit(), Err(F1RecordError::UnsupportedRemote));
        let store = AnyStore::from(remote);
        assert!(store.is_remote());
        let before = FerroStore::stats_at(&file).unwrap().last_seq;
        // Close the server before calls: the exact UnsupportedRemote result proves the
        // refusal occurs without trying to read the disconnected backend.
        drop(server);
        let direct = store.as_ferro().unwrap();
        assert_eq!(direct.routine(""), Err(F1RecordError::UnsupportedRemote));
        assert_eq!(direct.intent(""), Err(F1RecordError::UnsupportedRemote));
        assert_eq!(direct.allocation(""), Err(F1RecordError::UnsupportedRemote));
        let oversized = "x".repeat(2 * 1024 * 1024);
        for target in [direct as &dyn F1PreparationStore, &store] {
            assert_eq!(
                target.pause_routine(&oversized, 0, &oversized),
                Err(F1RecordError::UnsupportedRemote)
            );
            assert_eq!(
                target.quarantine_intent(&oversized, 0),
                Err(F1RecordError::UnsupportedRemote)
            );
            assert_eq!(
                target.request_allocation_stop(&oversized, 0),
                Err(F1RecordError::UnsupportedRemote)
            );
        }
        assert_eq!(
            store.routine("routine"),
            Err(F1RecordError::UnsupportedRemote)
        );
        assert_eq!(
            store.intent("intent"),
            Err(F1RecordError::UnsupportedRemote)
        );
        assert_eq!(
            store.allocation("allocation"),
            Err(F1RecordError::UnsupportedRemote)
        );
        assert_eq!(
            store.insert_proposal(routine()),
            Err(F1RecordError::UnsupportedRemote)
        );
        assert_eq!(
            store.insert_intent(intent()),
            Err(F1RecordError::UnsupportedRemote)
        );
        assert_eq!(
            store.insert_allocation(allocation()),
            Err(F1RecordError::UnsupportedRemote)
        );
        assert_eq!(
            store.apply_safety_batch(&SafetyBatch { changes: vec![] }),
            Err(F1RecordError::UnsupportedRemote)
        );
        assert_eq!(
            store.pause_routine("routine", 1, "reason"),
            Err(F1RecordError::UnsupportedRemote)
        );
        assert_eq!(
            store.quarantine_intent("intent", 1),
            Err(F1RecordError::UnsupportedRemote)
        );
        assert_eq!(
            store.request_allocation_stop("allocation", 1),
            Err(F1RecordError::UnsupportedRemote)
        );
        assert_eq!(store.admit(), Err(F1RecordError::UnsupportedRemote));
        assert_eq!(store.accept_effect(), Err(F1RecordError::UnsupportedRemote));
        assert_eq!(store.settle(), Err(F1RecordError::UnsupportedRemote));
        assert_eq!(FerroStore::stats_at(&file).unwrap().last_seq, before);
    }
}
