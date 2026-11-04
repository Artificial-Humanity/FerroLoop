//! Per-record, fail-closed F1 preparation on local native backends.
//! Remote Ferro never performs these operations. Records are inert evidence, not authority.

use crate::{AnyStore, FORMAT_WITH_F1, RedbStore};
use fl_core::f1_records::{
    AllocationEvidenceRecord, ControlRecordKey, F1PreparationStore, F1RecordError,
    IntentEvidenceRecord, RoutineProposalRecord, SafetyBatch, SafetyChange,
    decode_allocation_record, decode_intent_record, decode_routine_record, encode_record,
    prepare_allocation_stop, prepare_intent_quarantine, prepare_routine_pause,
};
use redb::{ReadableTable, TableDefinition};

pub(crate) const TABLE: TableDefinition<&str, &str> = TableDefinition::new("__f1_records");
fn backend_error(e: impl std::fmt::Display) -> F1RecordError {
    F1RecordError::Backend(e.to_string())
}

/// Stage only after every row is present, valid and at its expected version.
pub(crate) fn stage_batch(
    batch: &SafetyBatch,
    mut get: impl FnMut(&str) -> Result<Option<Vec<u8>>, F1RecordError>,
) -> Result<Vec<(String, Vec<u8>)>, F1RecordError> {
    batch.validate()?;
    let mut staged = Vec::with_capacity(batch.changes.len());
    for change in &batch.changes {
        let (key, bytes) = match change {
            SafetyChange::PauseRoutine {
                id,
                expected_version,
                reason,
            } => {
                let key = ControlRecordKey::routine(id)?;
                let row = decode_routine_record(
                    &get(key.native_key())?.ok_or(F1RecordError::Missing)?,
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
                    &get(key.native_key())?.ok_or(F1RecordError::Missing)?,
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
                    &get(key.native_key())?.ok_or(F1RecordError::Missing)?,
                    id,
                )?;
                let next = prepare_allocation_stop(&row, *expected_version)?;
                (key, encode_record(&next)?)
            }
        };
        staged.push((key.native_key().to_string(), bytes));
    }
    Ok(staged)
}

impl RedbStore {
    fn f1_get(&self, key: &ControlRecordKey) -> Result<Option<Vec<u8>>, F1RecordError> {
        let tx = self.db.begin_read().map_err(backend_error)?;
        let table = match tx.open_table(TABLE) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend_error(e)),
        };
        let value = table
            .get(key.native_key())
            .map_err(backend_error)?
            .map(|v| v.value().as_bytes().to_vec());
        Ok(value)
    }
    fn f1_insert(&self, key: ControlRecordKey, bytes: Vec<u8>) -> Result<(), F1RecordError> {
        let text = std::str::from_utf8(&bytes).map_err(|_| F1RecordError::Codec)?;
        let tx = self.db.begin_write().map_err(backend_error)?;
        {
            let mut table = tx.open_table(TABLE).map_err(backend_error)?;
            if table
                .get(key.native_key())
                .map_err(backend_error)?
                .is_some()
            {
                return Err(F1RecordError::Conflict);
            }
            table
                .insert(key.native_key(), text)
                .map_err(backend_error)?;
        }
        crate::raise_format(&tx, FORMAT_WITH_F1).map_err(backend_error)?;
        tx.commit().map_err(backend_error)
    }
}

impl F1PreparationStore for RedbStore {
    fn routine(&self, id: &str) -> Result<Option<RoutineProposalRecord>, F1RecordError> {
        self.f1_get(&ControlRecordKey::routine(id)?)?
            .map(|v| decode_routine_record(&v, id))
            .transpose()
    }
    fn intent(&self, id: &str) -> Result<Option<IntentEvidenceRecord>, F1RecordError> {
        self.f1_get(&ControlRecordKey::intent(id)?)?
            .map(|v| decode_intent_record(&v, id))
            .transpose()
    }
    fn allocation(&self, id: &str) -> Result<Option<AllocationEvidenceRecord>, F1RecordError> {
        self.f1_get(&ControlRecordKey::allocation(id)?)?
            .map(|v| decode_allocation_record(&v, id))
            .transpose()
    }
    fn insert_proposal(&self, row: RoutineProposalRecord) -> Result<(), F1RecordError> {
        row.validate()?;
        if row.version != 1 || !matches!(row.spec.state, fl_core::routine::RoutineState::Proposed) {
            return Err(F1RecordError::InvalidRecord);
        }
        self.f1_insert(
            ControlRecordKey::routine(&row.spec.routine_id)?,
            encode_record(&row)?,
        )
    }
    fn insert_intent(&self, row: IntentEvidenceRecord) -> Result<(), F1RecordError> {
        row.validate_for_insert()?;
        self.f1_insert(
            ControlRecordKey::intent(row.definition.id())?,
            encode_record(&row)?,
        )
    }
    fn insert_allocation(&self, row: AllocationEvidenceRecord) -> Result<(), F1RecordError> {
        row.validate_for_insert()?;
        self.f1_insert(
            ControlRecordKey::allocation(&row.allocation_id)?,
            encode_record(&row)?,
        )
    }
    fn apply_safety_batch(&self, batch: &SafetyBatch) -> Result<(), F1RecordError> {
        let tx = self.db.begin_write().map_err(backend_error)?;
        {
            let mut table = tx.open_table(TABLE).map_err(backend_error)?;
            let changes = stage_batch(batch, |key| {
                Ok(table
                    .get(key)
                    .map_err(backend_error)?
                    .map(|v| v.value().as_bytes().to_vec()))
            })?;
            for (key, bytes) in changes {
                let text = std::str::from_utf8(&bytes).map_err(|_| F1RecordError::Codec)?;
                table.insert(key.as_str(), text).map_err(backend_error)?;
            }
        }
        crate::raise_format(&tx, FORMAT_WITH_F1).map_err(backend_error)?;
        tx.commit().map_err(backend_error)
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

macro_rules! delegate_local {
    ($self:ident, $method:ident ( $($arg:expr),* )) => {{
        if $self.is_remote() { Err(F1RecordError::UnsupportedRemote) }
        else if let Some(store) = $self.as_redb() { F1PreparationStore::$method(store, $($arg),*) }
        else if let Some(store) = $self.as_ferro() { F1PreparationStore::$method(store, $($arg),*) }
        else { Err(F1RecordError::UnsupportedLocal) }
    }};
}
impl F1PreparationStore for AnyStore {
    fn routine(&self, id: &str) -> Result<Option<RoutineProposalRecord>, F1RecordError> {
        delegate_local!(self, routine(id))
    }
    fn intent(&self, id: &str) -> Result<Option<IntentEvidenceRecord>, F1RecordError> {
        delegate_local!(self, intent(id))
    }
    fn allocation(&self, id: &str) -> Result<Option<AllocationEvidenceRecord>, F1RecordError> {
        delegate_local!(self, allocation(id))
    }
    fn insert_proposal(&self, row: RoutineProposalRecord) -> Result<(), F1RecordError> {
        delegate_local!(self, insert_proposal(row))
    }
    fn insert_intent(&self, row: IntentEvidenceRecord) -> Result<(), F1RecordError> {
        delegate_local!(self, insert_intent(row))
    }
    fn insert_allocation(&self, row: AllocationEvidenceRecord) -> Result<(), F1RecordError> {
        delegate_local!(self, insert_allocation(row))
    }
    fn apply_safety_batch(&self, batch: &SafetyBatch) -> Result<(), F1RecordError> {
        delegate_local!(self, apply_safety_batch(batch))
    }
    fn pause_routine(&self, id: &str, v: u64, reason: &str) -> Result<(), F1RecordError> {
        delegate_local!(self, pause_routine(id, v, reason))
    }
    fn quarantine_intent(&self, id: &str, v: u64) -> Result<(), F1RecordError> {
        delegate_local!(self, quarantine_intent(id, v))
    }
    fn request_allocation_stop(&self, id: &str, v: u64) -> Result<(), F1RecordError> {
        delegate_local!(self, request_allocation_stop(id, v))
    }
    fn admit(&self) -> Result<(), F1RecordError> {
        if self.is_remote() {
            Err(F1RecordError::UnsupportedRemote)
        } else {
            Err(F1RecordError::UnverifiedAuthority)
        }
    }
    fn accept_effect(&self) -> Result<(), F1RecordError> {
        if self.is_remote() {
            Err(F1RecordError::UnsupportedRemote)
        } else {
            Err(F1RecordError::UnverifiedAuthority)
        }
    }
    fn settle(&self) -> Result<(), F1RecordError> {
        if self.is_remote() {
            Err(F1RecordError::UnsupportedRemote)
        } else {
            Err(F1RecordError::UnverifiedAuthority)
        }
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
    fn local_engines_persist_independent_records_and_atomic_safety() {
        let tmp = tempfile::tempdir().unwrap();
        for store in [
            AnyStore::from(RedbStore::open(&tmp.path().join("f1.redb")).unwrap()),
            AnyStore::from(FerroStore::open(tmp.path().join("f1.fls")).unwrap()),
        ] {
            store.insert_proposal(routine()).unwrap();
            store.insert_allocation(allocation()).unwrap();
            assert_eq!(store.routine("routine").unwrap(), Some(routine()));
            let batch = SafetyBatch {
                changes: vec![
                    fl_core::f1_records::SafetyChange::PauseRoutine {
                        id: "routine".into(),
                        expected_version: 1,
                        reason: "pause".into(),
                    },
                    fl_core::f1_records::SafetyChange::RequestAllocationStop {
                        id: "allocation".into(),
                        expected_version: 2,
                    },
                ],
            };
            assert_eq!(
                store.apply_safety_batch(&batch),
                Err(F1RecordError::Conflict)
            );
            assert_eq!(store.routine("routine").unwrap(), Some(routine()));
            store.request_allocation_stop("allocation", 1).unwrap();
            assert_eq!(
                store.allocation("allocation").unwrap().unwrap().safety,
                SafetyState::StopRequested
            );
        }
    }

    #[test]
    fn both_local_backends_survive_reopen_and_reject_duplicate_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let redb_path = tmp.path().join("rows.redb");
        let ferro_path = tmp.path().join("rows.fls");
        {
            let redb = RedbStore::open(&redb_path).unwrap();
            let ferro = FerroStore::open(&ferro_path).unwrap();
            for store in [
                &redb as &dyn F1PreparationStore,
                &ferro as &dyn F1PreparationStore,
            ] {
                store.insert_proposal(routine()).unwrap();
                store.insert_intent(intent()).unwrap();
                store.insert_allocation(allocation()).unwrap();
                assert_eq!(
                    store.insert_allocation(allocation()),
                    Err(F1RecordError::Conflict)
                );
                store.quarantine_intent("intent", 1).unwrap();
            }
            assert_eq!(redb.format_version().unwrap(), Some(crate::FORMAT_WITH_F1));
            assert_eq!(ferro.format_version().unwrap(), Some(crate::FORMAT_WITH_F1));
            assert!(
                ferro
                    .put_doc("__f1_records", "x", &serde_json::json!(1))
                    .is_err()
            );
        }
        let redb = RedbStore::open(&redb_path).unwrap();
        let ferro = FerroStore::open(&ferro_path).unwrap();
        for store in [
            &redb as &dyn F1PreparationStore,
            &ferro as &dyn F1PreparationStore,
        ] {
            assert_eq!(store.routine("routine").unwrap(), Some(routine()));
            assert_eq!(
                store.intent("intent").unwrap().unwrap().safety,
                SafetyState::Quarantined
            );
            assert_eq!(store.allocation("allocation").unwrap(), Some(allocation()));
        }
    }

    #[test]
    fn migration_copies_and_verifies_f1_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let redb = RedbStore::open(&tmp.path().join("source.redb")).unwrap();
        let ferro = FerroStore::open(tmp.path().join("destination.fls")).unwrap();
        redb.insert_proposal(routine()).unwrap();
        redb.insert_intent(intent()).unwrap();
        redb.insert_allocation(allocation()).unwrap();
        redb.quarantine_intent("intent", 1).unwrap();
        crate::migrate_redb_to_ferrostore(&redb, &ferro).unwrap();
        let report = crate::migrate::verify_redb_vs_ferrostore(&redb, &ferro).unwrap();
        assert!(report.is_clean(), "{:?}", report.mismatches());
        assert_eq!(ferro.routine("routine").unwrap(), Some(routine()));
        assert_eq!(
            ferro.intent("intent").unwrap().unwrap().safety,
            SafetyState::Quarantined
        );
        assert_eq!(ferro.allocation("allocation").unwrap(), Some(allocation()));
    }

    #[test]
    fn concurrent_f1_cas_has_exactly_one_winner_on_both_engines() {
        use std::sync::{Arc, Barrier};
        let tmp = tempfile::tempdir().unwrap();
        for store in [
            Arc::new(AnyStore::from(
                RedbStore::open(&tmp.path().join("race.redb")).unwrap(),
            )),
            Arc::new(AnyStore::from(
                FerroStore::open(tmp.path().join("race.fls")).unwrap(),
            )),
        ] {
            store.insert_allocation(allocation()).unwrap();
            let barrier = Arc::new(Barrier::new(3));
            let mut threads = Vec::new();
            for _ in 0..2 {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                threads.push(std::thread::spawn(move || {
                    barrier.wait();
                    store.request_allocation_stop("allocation", 1)
                }));
            }
            barrier.wait();
            let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
            assert_eq!(
                results.iter().filter(|r| r.is_ok()).count(),
                1,
                "{results:?}"
            );
            assert_eq!(
                results
                    .iter()
                    .filter(|r| matches!(r, Err(F1RecordError::Conflict)))
                    .count(),
                1,
                "{results:?}"
            );
        }
    }

    #[test]
    fn misplaced_or_corrupt_row_fails_closed_without_batch_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let redb = RedbStore::open(&tmp.path().join("corrupt.redb")).unwrap();
        let ferro = FerroStore::open(tmp.path().join("corrupt.fls")).unwrap();
        redb.insert_proposal(routine()).unwrap();
        redb.insert_allocation(allocation()).unwrap();
        {
            let tx = redb.db.begin_write().unwrap();
            {
                let mut rows = tx.open_table(super::TABLE).unwrap();
                let key = fl_core::f1_records::ControlRecordKey::allocation("other").unwrap();
                let bytes = fl_core::f1_records::encode_record(&allocation()).unwrap();
                rows.insert(key.native_key(), std::str::from_utf8(&bytes).unwrap())
                    .unwrap();
            }
            tx.commit().unwrap();
        }
        crate::migrate_redb_to_ferrostore(&redb, &ferro).unwrap();
        for store in [
            &redb as &dyn F1PreparationStore,
            &ferro as &dyn F1PreparationStore,
        ] {
            assert_eq!(store.allocation("other"), Err(F1RecordError::Conflict));
            let changes = SafetyBatch {
                changes: vec![
                    fl_core::f1_records::SafetyChange::PauseRoutine {
                        id: "routine".into(),
                        expected_version: 1,
                        reason: "pause".into(),
                    },
                    fl_core::f1_records::SafetyChange::RequestAllocationStop {
                        id: "other".into(),
                        expected_version: 1,
                    },
                ],
            };
            assert_eq!(
                store.apply_safety_batch(&changes),
                Err(F1RecordError::Conflict)
            );
            assert_eq!(store.routine("routine").unwrap(), Some(routine()));
            assert_eq!(store.allocation("allocation").unwrap(), Some(allocation()));
        }
    }

    #[test]
    fn safety_transition_survives_more_than_four_mib_of_other_rows() {
        let tmp = tempfile::tempdir().unwrap();
        for store in [
            AnyStore::from(RedbStore::open(&tmp.path().join("large.redb")).unwrap()),
            AnyStore::from(FerroStore::open(tmp.path().join("large.fls")).unwrap()),
        ] {
            store.insert_allocation(allocation()).unwrap();
            let mut total = 0;
            for i in 0..75 {
                let mut row = routine();
                row.spec.routine_id = format!("routine-{i}");
                row.spec.project = "x".repeat(60_000);
                total += fl_core::f1_records::encode_record(&row).unwrap().len();
                store.insert_proposal(row).unwrap();
            }
            assert!(total > 4 * 1024 * 1024);
            store.request_allocation_stop("allocation", 1).unwrap();
            assert_eq!(
                store.allocation("allocation").unwrap().unwrap().safety,
                SafetyState::StopRequested
            );
        }
    }

    #[test]
    fn ferro_independent_handles_race_one_cas() {
        use std::sync::{Arc, Barrier};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("independent.fls");
        let first = Arc::new(FerroStore::open(&path).unwrap());
        let second = Arc::new(FerroStore::open(&path).unwrap());
        first.insert_allocation(allocation()).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let threads: Vec<_> = [first, second]
            .into_iter()
            .map(|store| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.request_allocation_stop("allocation", 1)
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(F1RecordError::Conflict)))
                .count(),
            1,
            "{results:?}"
        );
    }

    #[test]
    fn local_ferro_and_redb_refuse_authority_until_verified() {
        let tmp = tempfile::tempdir().unwrap();
        for store in [
            AnyStore::in_memory(),
            AnyStore::from(RedbStore::open(&tmp.path().join("local.redb")).unwrap()),
        ] {
            assert_eq!(store.admit(), Err(F1RecordError::UnverifiedAuthority));
            assert_eq!(
                store.accept_effect(),
                Err(F1RecordError::UnverifiedAuthority)
            );
            assert_eq!(store.settle(), Err(F1RecordError::UnverifiedAuthority));
        }
    }
}
