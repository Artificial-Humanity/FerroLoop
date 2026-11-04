//! Thread-safe, per-record in-memory preparation store. It grants no authority and
//! performs no host effects. A process-local mutex is sufficient only for this memory store.

use crate::f1_records::{
    AllocationEvidenceRecord, ControlRecordKey, F1PreparationStore, F1RecordError,
    IntentEvidenceRecord, RoutineProposalRecord, SafetyBatch, SafetyChange,
    decode_allocation_record, decode_intent_record, decode_routine_record, encode_record,
    prepare_allocation_stop, prepare_intent_quarantine, prepare_routine_pause,
};
use crate::routine::RoutineState;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

/// Each key holds one independently bounded serialized row; there is no global snapshot.
#[derive(Debug, Default)]
pub struct MemoryF1PreparationStore {
    rows: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryF1PreparationStore {
    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<String, Vec<u8>>>, F1RecordError> {
        self.rows
            .lock()
            .map_err(|_| F1RecordError::Backend("memory store lock poisoned".into()))
    }

    fn insert(&self, key: ControlRecordKey, value: Vec<u8>) -> Result<(), F1RecordError> {
        let mut rows = self.lock()?;
        if rows.contains_key(key.native_key()) {
            return Err(F1RecordError::Conflict);
        }
        rows.insert(key.native_key().to_owned(), value);
        Ok(())
    }
}

impl F1PreparationStore for MemoryF1PreparationStore {
    fn routine(&self, id: &str) -> Result<Option<RoutineProposalRecord>, F1RecordError> {
        let key = ControlRecordKey::routine(id)?;
        self.lock()?
            .get(key.native_key())
            .map(|v| decode_routine_record(v, id))
            .transpose()
    }
    fn intent(&self, id: &str) -> Result<Option<IntentEvidenceRecord>, F1RecordError> {
        let key = ControlRecordKey::intent(id)?;
        self.lock()?
            .get(key.native_key())
            .map(|v| decode_intent_record(v, id))
            .transpose()
    }
    fn allocation(&self, id: &str) -> Result<Option<AllocationEvidenceRecord>, F1RecordError> {
        let key = ControlRecordKey::allocation(id)?;
        self.lock()?
            .get(key.native_key())
            .map(|v| decode_allocation_record(v, id))
            .transpose()
    }
    fn insert_proposal(&self, record: RoutineProposalRecord) -> Result<(), F1RecordError> {
        record.validate()?;
        if record.version != 1 || !matches!(record.spec.state, RoutineState::Proposed) {
            return Err(F1RecordError::InvalidRecord);
        }
        self.insert(
            ControlRecordKey::routine(&record.spec.routine_id)?,
            encode_record(&record)?,
        )
    }
    fn insert_intent(&self, record: IntentEvidenceRecord) -> Result<(), F1RecordError> {
        record.validate_for_insert()?;
        self.insert(
            ControlRecordKey::intent(record.definition.id())?,
            encode_record(&record)?,
        )
    }
    fn insert_allocation(&self, record: AllocationEvidenceRecord) -> Result<(), F1RecordError> {
        record.validate_for_insert()?;
        self.insert(
            ControlRecordKey::allocation(&record.allocation_id)?,
            encode_record(&record)?,
        )
    }
    fn apply_safety_batch(&self, batch: &SafetyBatch) -> Result<(), F1RecordError> {
        batch.validate()?;
        let mut rows = self.lock()?;
        let mut staged = Vec::with_capacity(batch.changes.len());
        for change in &batch.changes {
            let (key, value) = match change {
                SafetyChange::PauseRoutine {
                    id,
                    expected_version,
                    reason,
                } => {
                    let key = ControlRecordKey::routine(id)?;
                    let row = decode_routine_record(
                        rows.get(key.native_key()).ok_or(F1RecordError::Missing)?,
                        id,
                    )?;
                    let next = prepare_routine_pause(&row, *expected_version, reason)?;
                    (key, encode_record(&next)?)
                }
                SafetyChange::QuarantineIntent {
                    id,
                    expected_version,
                } => {
                    let key = ControlRecordKey::intent(id)?;
                    let row = decode_intent_record(
                        rows.get(key.native_key()).ok_or(F1RecordError::Missing)?,
                        id,
                    )?;
                    let next = prepare_intent_quarantine(&row, *expected_version)?;
                    (key, encode_record(&next)?)
                }
                SafetyChange::RequestAllocationStop {
                    id,
                    expected_version,
                } => {
                    let key = ControlRecordKey::allocation(id)?;
                    let row = decode_allocation_record(
                        rows.get(key.native_key()).ok_or(F1RecordError::Missing)?,
                        id,
                    )?;
                    let next = prepare_allocation_stop(&row, *expected_version)?;
                    (key, encode_record(&next)?)
                }
            };
            staged.push((key.native_key().to_owned(), value));
        }
        for (key, value) in staged {
            rows.insert(key, value);
        }
        Ok(())
    }
    fn pause_routine(
        &self,
        id: &str,
        expected_version: u64,
        reason: &str,
    ) -> Result<(), F1RecordError> {
        self.apply_safety_batch(&SafetyBatch {
            changes: vec![SafetyChange::PauseRoutine {
                id: id.into(),
                expected_version,
                reason: reason.into(),
            }],
        })
    }
    fn quarantine_intent(&self, id: &str, expected_version: u64) -> Result<(), F1RecordError> {
        self.apply_safety_batch(&SafetyBatch {
            changes: vec![SafetyChange::QuarantineIntent {
                id: id.into(),
                expected_version,
            }],
        })
    }
    fn request_allocation_stop(
        &self,
        id: &str,
        expected_version: u64,
    ) -> Result<(), F1RecordError> {
        self.apply_safety_batch(&SafetyBatch {
            changes: vec![SafetyChange::RequestAllocationStop {
                id: id.into(),
                expected_version,
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::f1_records::{
        AllocationEvidenceRecord, F1PreparationStore, F1RecordError, SafetyBatch, SafetyChange,
        SafetyState,
    };
    use std::sync::{Arc, Barrier};

    fn allocation(id: &str) -> AllocationEvidenceRecord {
        AllocationEvidenceRecord {
            version: 1,
            allocation_id: id.into(),
            parent_id: None,
            root_id: id.into(),
            source_window_id: "window".into(),
            deadline_utc: "2030-01-01T00:00:00Z".into(),
            tokens: 10,
            cost_micros: 10,
            safety: SafetyState::Inert,
        }
    }

    #[test]
    fn concurrent_compare_and_swap_allows_one_winner() {
        let store = Arc::new(MemoryF1PreparationStore::default());
        store.insert_allocation(allocation("a")).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.request_allocation_stop("a", 1)
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = workers.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| **r == Err(F1RecordError::Conflict))
                .count(),
            1
        );
        let row = store.allocation("a").unwrap().unwrap();
        assert_eq!((row.version, row.safety), (2, SafetyState::StopRequested));
    }

    #[test]
    fn failed_batch_rolls_back_every_row() {
        let store = MemoryF1PreparationStore::default();
        store.insert_allocation(allocation("a")).unwrap();
        store.insert_allocation(allocation("b")).unwrap();
        let batch = SafetyBatch {
            changes: vec![
                SafetyChange::RequestAllocationStop {
                    id: "a".into(),
                    expected_version: 1,
                },
                SafetyChange::RequestAllocationStop {
                    id: "b".into(),
                    expected_version: 2,
                },
            ],
        };
        assert_eq!(
            store.apply_safety_batch(&batch),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(store.allocation("a").unwrap(), Some(allocation("a")));
        assert_eq!(store.allocation("b").unwrap(), Some(allocation("b")));
    }

    #[test]
    fn duplicate_and_invalid_insert_are_refused_without_replacement() {
        let store = MemoryF1PreparationStore::default();
        store.insert_allocation(allocation("a")).unwrap();
        let mut changed = allocation("a");
        changed.tokens = 20;
        assert_eq!(
            store.insert_allocation(changed),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(store.allocation("a").unwrap(), Some(allocation("a")));
        let mut invalid = allocation("b");
        invalid.version = 2;
        assert_eq!(
            store.insert_allocation(invalid),
            Err(F1RecordError::InvalidRecord)
        );
        assert_eq!(store.allocation("b"), Ok(None));
    }

    #[test]
    fn independent_rows_are_not_limited_by_a_global_snapshot() {
        let store = MemoryF1PreparationStore::default();
        for i in 0..24000 {
            store
                .insert_allocation(allocation(&format!("item-{i}")))
                .unwrap();
        }
        assert!(
            store
                .rows
                .lock()
                .unwrap()
                .values()
                .map(Vec::len)
                .sum::<usize>()
                > 4 * 1024 * 1024
        );
        store.request_allocation_stop("item-0", 1).unwrap();
        assert_eq!(
            store.allocation("item-23999").unwrap(),
            Some(allocation("item-23999"))
        );
        assert_eq!(store.admit(), Err(F1RecordError::UnverifiedAuthority));
        assert_eq!(
            store.accept_effect(),
            Err(F1RecordError::UnverifiedAuthority)
        );
        assert_eq!(store.settle(), Err(F1RecordError::UnverifiedAuthority));
    }

    #[test]
    fn mixed_batch_commits_all_types_and_respects_fresh_versions() {
        use crate::f1_records::{IntentDefinition, IntentEvidenceRecord, RoutineProposalRecord};
        use crate::intent::{
            AllocationAmount, Binding, CommitteeIntent, DelegateTarget, QuorumRule, ResultSchema,
        };
        use crate::routine::{
            Allocation, CatchUp, Concurrency, RoutineSpec, RoutineState, RoutineWork, Schedule,
            Window,
        };
        let store = MemoryF1PreparationStore::default();
        let proposal = RoutineProposalRecord {
            version: 1,
            spec: RoutineSpec {
                routine_id: "r".into(),
                revision: 1,
                project: "p".into(),
                schedule: Schedule::Interval {
                    every_secs: 300,
                    anchor_utc: "2030-01-01T00:00:00Z".into(),
                },
                work: RoutineWork::ExecutableOnly {
                    gate_or_hook_id: "g".into(),
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
        };
        let intent = IntentEvidenceRecord {
            version: 1,
            safety: SafetyState::Inert,
            definition: IntentDefinition::Committee(CommitteeIntent {
                intent_id: "i".into(),
                delegator: Binding {
                    incarnation_id: "delegator".into(),
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
        };
        store.insert_proposal(proposal.clone()).unwrap();
        store.insert_intent(intent.clone()).unwrap();
        store.insert_allocation(allocation("root")).unwrap();
        assert_eq!(
            store.insert_proposal(proposal),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(store.insert_intent(intent), Err(F1RecordError::Conflict));
        let batch = SafetyBatch {
            changes: vec![
                SafetyChange::PauseRoutine {
                    id: "r".into(),
                    expected_version: 1,
                    reason: "operator".into(),
                },
                SafetyChange::QuarantineIntent {
                    id: "i".into(),
                    expected_version: 1,
                },
                SafetyChange::RequestAllocationStop {
                    id: "root".into(),
                    expected_version: 1,
                },
            ],
        };
        store.apply_safety_batch(&batch).unwrap();
        assert!(matches!(
            store.routine("r").unwrap().unwrap().spec.state,
            RoutineState::Paused { .. }
        ));
        assert_eq!(
            store.intent("i").unwrap().unwrap().safety,
            SafetyState::Quarantined
        );
        assert_eq!(
            store.allocation("root").unwrap().unwrap().safety,
            SafetyState::StopRequested
        );
        assert_eq!(
            store.apply_safety_batch(&batch),
            Err(F1RecordError::Conflict)
        );
    }

    #[test]
    fn malformed_or_misplaced_stored_rows_fail_closed() {
        let store = MemoryF1PreparationStore::default();
        let key = ControlRecordKey::allocation("wanted").unwrap();
        store
            .rows
            .lock()
            .unwrap()
            .insert(key.native_key().into(), b"{".to_vec());
        assert_eq!(store.allocation("wanted"), Err(F1RecordError::Codec));
        assert_eq!(
            store.request_allocation_stop("wanted", 1),
            Err(F1RecordError::Codec)
        );
        store.rows.lock().unwrap().insert(
            key.native_key().into(),
            encode_record(&allocation("other")).unwrap(),
        );
        assert_eq!(store.allocation("wanted"), Err(F1RecordError::Conflict));
        assert_eq!(
            store.insert_allocation(allocation("wanted")),
            Err(F1RecordError::Conflict)
        );
    }
}
