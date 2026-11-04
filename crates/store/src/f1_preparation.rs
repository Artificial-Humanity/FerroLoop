//! Fail-closed F1 boundary for existing store handles.
//!
//! Neither local engine nor remote Ferro yet supplies the required typed atomic
//! per-record control transactions. Refuse without reading or mutating the store.
//! This implementation does not change the default engine or provide admission.

use crate::AnyStore;
use fl_core::f1_records::{
    AllocationEvidenceRecord, F1PreparationStore, F1RecordError, IntentEvidenceRecord,
    RoutineProposalRecord, SafetyBatch,
};

impl AnyStore {
    fn f1_unavailable(&self) -> F1RecordError {
        if self.is_remote() {
            F1RecordError::UnsupportedRemote
        } else {
            F1RecordError::UnsupportedLocal
        }
    }
    fn f1_authority_unavailable(&self) -> F1RecordError {
        if self.is_remote() {
            F1RecordError::UnsupportedRemote
        } else {
            F1RecordError::UnverifiedAuthority
        }
    }
}

impl F1PreparationStore for AnyStore {
    fn routine(&self, _: &str) -> Result<Option<RoutineProposalRecord>, F1RecordError> {
        Err(self.f1_unavailable())
    }
    fn intent(&self, _: &str) -> Result<Option<IntentEvidenceRecord>, F1RecordError> {
        Err(self.f1_unavailable())
    }
    fn allocation(&self, _: &str) -> Result<Option<AllocationEvidenceRecord>, F1RecordError> {
        Err(self.f1_unavailable())
    }
    fn apply_safety_batch(&self, _: &SafetyBatch) -> Result<(), F1RecordError> {
        Err(self.f1_unavailable())
    }
    fn insert_proposal(&self, _: RoutineProposalRecord) -> Result<(), F1RecordError> {
        Err(self.f1_unavailable())
    }
    fn insert_intent(&self, _: IntentEvidenceRecord) -> Result<(), F1RecordError> {
        Err(self.f1_unavailable())
    }
    fn insert_allocation(&self, _: AllocationEvidenceRecord) -> Result<(), F1RecordError> {
        Err(self.f1_unavailable())
    }
    fn pause_routine(&self, _: &str, _: u64, _: &str) -> Result<(), F1RecordError> {
        Err(self.f1_unavailable())
    }
    fn quarantine_intent(&self, _: &str, _: u64) -> Result<(), F1RecordError> {
        Err(self.f1_unavailable())
    }
    fn request_allocation_stop(&self, _: &str, _: u64) -> Result<(), F1RecordError> {
        Err(self.f1_unavailable())
    }
    fn admit(&self) -> Result<(), F1RecordError> {
        Err(self.f1_authority_unavailable())
    }
    fn accept_effect(&self) -> Result<(), F1RecordError> {
        Err(self.f1_authority_unavailable())
    }
    fn settle(&self) -> Result<(), F1RecordError> {
        Err(self.f1_authority_unavailable())
    }
}

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

    #[cfg(unix)]
    #[test]
    fn remote_ferro_refuses_before_read_or_write() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("server.sock");
        let file = tmp.path().join("server.fls");
        let server =
            FerroStoreServer::bind_unix(&socket, FerroStore::open(&file).unwrap()).unwrap();
        let remote = FerroStore::connect_uri(&format!("unix://{}", socket.display())).unwrap();
        let store = AnyStore::from(remote);
        assert!(store.is_remote());
        let before = FerroStore::stats_at(&file).unwrap().last_seq;
        // Close the server before calls: the exact UnsupportedRemote result proves the
        // refusal occurs without trying to read the disconnected backend.
        drop(server);
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

    #[test]
    fn local_ferro_refuses_until_transactional_backend_is_wired() {
        let store = AnyStore::in_memory();
        assert!(!store.is_remote());
        assert_eq!(
            store.routine("routine"),
            Err(F1RecordError::UnsupportedLocal)
        );
        assert_eq!(
            store.insert_allocation(allocation()),
            Err(F1RecordError::UnsupportedLocal)
        );
        assert_eq!(store.admit(), Err(F1RecordError::UnverifiedAuthority));
        assert_eq!(
            store.accept_effect(),
            Err(F1RecordError::UnverifiedAuthority)
        );
        assert_eq!(store.settle(), Err(F1RecordError::UnverifiedAuthority));
    }
    #[test]
    fn local_redb_refuses_until_transactional_backend_is_wired() {
        let tmp = tempfile::tempdir().unwrap();
        let store = AnyStore::from(RedbStore::open(&tmp.path().join("local.redb")).unwrap());
        assert!(!store.is_remote());
        assert_eq!(
            store.routine("routine"),
            Err(F1RecordError::UnsupportedLocal)
        );
        assert_eq!(
            store.insert_allocation(allocation()),
            Err(F1RecordError::UnsupportedLocal)
        );
        assert_eq!(store.admit(), Err(F1RecordError::UnverifiedAuthority));
        assert_eq!(
            store.accept_effect(),
            Err(F1RecordError::UnverifiedAuthority)
        );
        assert_eq!(store.settle(), Err(F1RecordError::UnverifiedAuthority));
    }
}
