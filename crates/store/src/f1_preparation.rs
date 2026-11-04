//! Per-record, fail-closed F1 preparation on redb.
//! Ferro F1 mutations and redb-to-Ferro migration with F1 rows are refused until
//! reserved-record sync and cross-handle fencing are implemented. Existing local
//! Ferro F1 rows remain readable; remote Ferro refuses all F1 operations.
//! Records are inert evidence, not authority.

use crate::{AnyStore, FORMAT_WITH_F1, RedbStore};
use fl_core::f1_records::{
    AllocationEvidenceRecord, ControlRecordKey, F1PreparationStore, F1RecordError,
    IntentEvidenceRecord, MAX_RECORD_BYTES, RoutineProposalRecord, SafetyBatch, SafetyChange,
    decode_allocation_record, decode_intent_record, decode_routine_record, encode_record,
    prepare_allocation_stop, prepare_intent_quarantine, prepare_routine_pause,
};
use redb::{ReadableTable, TableDefinition};

pub(crate) const TABLE: TableDefinition<&str, &str> = TableDefinition::new("__f1_records");
fn backend_error(e: impl std::fmt::Display) -> F1RecordError {
    F1RecordError::Backend(e.to_string())
}

/// Check the borrowed redb value before allocating a copy.
fn bounded_redb_bytes(value: &str) -> Result<Vec<u8>, F1RecordError> {
    if value.len() > MAX_RECORD_BYTES {
        return Err(F1RecordError::TooLarge);
    }
    Ok(value.as_bytes().to_vec())
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
        table
            .get(key.native_key())
            .map_err(backend_error)?
            .map(|v| bounded_redb_bytes(v.value()))
            .transpose()
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
                table
                    .get(key)
                    .map_err(backend_error)?
                    .map(|v| bounded_redb_bytes(v.value()))
                    .transpose()
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
    fn ferro_mutations_refuse_before_committing_even_with_write_through_adapter() {
        use crate::ferrostore::{SyncAdapter, SyncBatch, SyncMode};
        use std::sync::Arc;
        struct Rejecting;
        impl SyncAdapter for Rejecting {
            fn name(&self) -> &str {
                "rejecting"
            }
            fn on_commit(&self, _: &SyncBatch) -> Result<(), fl_core::StoreError> {
                Err(fl_core::StoreError::Backend(
                    "injected delivery failure".into(),
                ))
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("local.fls");
        let first = FerroStore::open(&path).unwrap();
        let second = FerroStore::open(&path).unwrap();
        first
            .attach_sync_adapter(SyncMode::WriteThrough, Arc::new(Rejecting))
            .unwrap();
        let before = FerroStore::stats_at(&path).unwrap().last_seq;
        assert_eq!(
            first.insert_allocation(allocation()),
            Err(F1RecordError::UnsupportedLocal)
        );
        assert_eq!(
            second.insert_allocation(allocation()),
            Err(F1RecordError::UnsupportedLocal)
        );
        assert_eq!(
            first.request_allocation_stop("allocation", 1),
            Err(F1RecordError::UnsupportedLocal)
        );
        assert_eq!(
            first.apply_safety_batch(&SafetyBatch { changes: vec![] }),
            Err(F1RecordError::UnsupportedLocal)
        );
        let mut invalid = allocation();
        invalid.allocation_id.clear();
        assert_eq!(
            first.insert_allocation(invalid),
            Err(F1RecordError::UnsupportedLocal)
        );
        assert_eq!(FerroStore::stats_at(&path).unwrap().last_seq, before);
        assert_eq!(first.allocation("allocation").unwrap(), None);
    }

    #[test]
    fn ferro_sync_attach_and_another_handle_f1_write_can_race_without_committing_f1() {
        use crate::ferrostore::{SyncAdapter, SyncBatch, SyncMode};
        use std::sync::{Arc, Barrier};
        struct Rejecting;
        impl SyncAdapter for Rejecting {
            fn name(&self) -> &str {
                "rejecting"
            }
            fn on_commit(&self, _: &SyncBatch) -> Result<(), fl_core::StoreError> {
                Err(fl_core::StoreError::Backend("injected failure".into()))
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("race.fls");
        let first = Arc::new(FerroStore::open(&path).unwrap());
        let second = Arc::new(FerroStore::open(&path).unwrap());
        let before = FerroStore::stats_at(&path).unwrap().last_seq;
        let barrier = Arc::new(Barrier::new(3));
        let attach = {
            let first = Arc::clone(&first);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                first.attach_sync_adapter(SyncMode::WriteThrough, Arc::new(Rejecting))
            })
        };
        let write = {
            let second = Arc::clone(&second);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                second.insert_allocation(allocation())
            })
        };
        barrier.wait();
        assert!(attach.join().unwrap().is_ok());
        assert_eq!(write.join().unwrap(), Err(F1RecordError::UnsupportedLocal));
        assert_eq!(FerroStore::stats_at(&path).unwrap().last_seq, before);
        assert_eq!(second.allocation("allocation").unwrap(), None);
    }

    #[test]
    fn migration_of_f1_rows_is_refused_without_changing_source_or_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let redb = RedbStore::open(&tmp.path().join("source.redb")).unwrap();
        let path = tmp.path().join("destination.fls");
        let ferro = FerroStore::open(&path).unwrap();
        redb.insert_proposal(routine()).unwrap();
        let before = FerroStore::stats_at(&path).unwrap().last_seq;
        let err = crate::migrate_redb_to_ferrostore(&redb, &ferro).unwrap_err();
        assert!(err.to_string().contains("F1"), "{err}");
        assert_eq!(redb.routine("routine").unwrap(), Some(routine()));
        assert_eq!(FerroStore::stats_at(&path).unwrap().last_seq, before);
        assert_eq!(ferro.routine("routine").unwrap(), None);
    }

    #[test]
    fn auto_migration_refusal_keeps_f1_redb_available_with_explicit_backend() {
        use crate::any::{BackendKind, StoreOptions, Unmigrated, open_store_with};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("source.redb");
        {
            let store = RedbStore::open(&path).unwrap();
            store.insert_allocation(allocation()).unwrap();
        }
        let options = StoreOptions {
            backend: Some(BackendKind::Ferro),
            unmigrated: Unmigrated::Migrate,
            ..StoreOptions::default()
        };
        let err = match open_store_with(&path, &options) {
            Ok(_) => panic!("F1 auto-migration must be refused"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("FL_STORE_BACKEND=redb"), "{err}");
        assert!(!path.with_extension("fls").exists());
        let store = RedbStore::open(&path).unwrap();
        assert_eq!(store.allocation("allocation").unwrap(), Some(allocation()));
        store.request_allocation_stop("allocation", 1).unwrap();
    }

    #[test]
    fn redb_persists_independent_records_and_atomic_safety() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&tmp.path().join("f1.redb")).unwrap();
        store.insert_proposal(routine()).unwrap();
        store.insert_allocation(allocation()).unwrap();
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

    #[test]
    fn redb_survives_reopen_and_rejects_duplicate_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rows.redb");
        {
            let store = RedbStore::open(&path).unwrap();
            store.insert_proposal(routine()).unwrap();
            store.insert_intent(intent()).unwrap();
            store.insert_allocation(allocation()).unwrap();
            assert_eq!(
                store.insert_allocation(allocation()),
                Err(F1RecordError::Conflict)
            );
            store.quarantine_intent("intent", 1).unwrap();
            assert_eq!(store.format_version().unwrap(), Some(crate::FORMAT_WITH_F1));
        }
        let store = RedbStore::open(&path).unwrap();
        assert_eq!(store.routine("routine").unwrap(), Some(routine()));
        assert_eq!(
            store.intent("intent").unwrap().unwrap().safety,
            SafetyState::Quarantined
        );
        assert_eq!(store.allocation("allocation").unwrap(), Some(allocation()));
    }

    #[test]
    fn preexisting_ferro_f1_rows_are_read_only() {
        use fl_core::f1_records::ControlRecordKey;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("legacy.fls");
        let ferro = FerroStore::open(&path).unwrap();
        let key = ControlRecordKey::allocation("allocation").unwrap();
        ferro.seed_f1_test_document(
            key.native_key(),
            serde_json::to_value(allocation()).unwrap(),
        );
        assert_eq!(ferro.allocation("allocation").unwrap(), Some(allocation()));
        let before = FerroStore::stats_at(&path).unwrap().last_seq;
        assert_eq!(
            ferro.request_allocation_stop("allocation", 1),
            Err(F1RecordError::UnsupportedLocal)
        );
        assert_eq!(FerroStore::stats_at(&path).unwrap().last_seq, before);
        drop(ferro);
        let reopened = FerroStore::open(&path).unwrap();
        assert_eq!(
            reopened.allocation("allocation").unwrap(),
            Some(allocation())
        );
    }

    #[test]
    fn concurrent_redb_f1_cas_has_exactly_one_winner() {
        use std::sync::{Arc, Barrier};
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(AnyStore::from(
            RedbStore::open(&tmp.path().join("race.redb")).unwrap(),
        ));
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

    #[test]
    fn misplaced_or_corrupt_row_fails_closed_without_batch_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let redb = RedbStore::open(&tmp.path().join("corrupt.redb")).unwrap();
        redb.insert_proposal(routine()).unwrap();
        redb.insert_allocation(allocation()).unwrap();
        let tx = redb.db.begin_write().unwrap();
        {
            let mut rows = tx.open_table(super::TABLE).unwrap();
            let key = fl_core::f1_records::ControlRecordKey::allocation("other").unwrap();
            let bytes = fl_core::f1_records::encode_record(&allocation()).unwrap();
            rows.insert(key.native_key(), std::str::from_utf8(&bytes).unwrap())
                .unwrap();
        }
        tx.commit().unwrap();
        assert_eq!(redb.allocation("other"), Err(F1RecordError::Conflict));
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
            redb.apply_safety_batch(&changes),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(redb.routine("routine").unwrap(), Some(routine()));
        assert_eq!(redb.allocation("allocation").unwrap(), Some(allocation()));
    }

    #[test]
    fn unknown_nested_row_refuses_redb_read_and_rolls_back_batch() {
        use fl_core::f1_records::{ControlRecordKey, SafetyChange};
        let tmp = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&tmp.path().join("unknown.redb")).unwrap();
        store.insert_proposal(routine()).unwrap();
        store.insert_allocation(allocation()).unwrap();
        let mut json = serde_json::to_value(routine()).unwrap();
        json["spec"]["future_field"] = serde_json::json!({"new": true});
        let text = serde_json::to_string_pretty(&json).unwrap();
        let key = ControlRecordKey::routine("routine").unwrap();
        let tx = store.db.begin_write().unwrap();
        {
            let mut rows = tx.open_table(super::TABLE).unwrap();
            rows.insert(key.native_key(), text.as_str()).unwrap();
        }
        tx.commit().unwrap();
        assert_eq!(store.routine("routine"), Err(F1RecordError::Codec));
        let batch = SafetyBatch {
            changes: vec![
                SafetyChange::RequestAllocationStop {
                    id: "allocation".into(),
                    expected_version: 1,
                },
                SafetyChange::PauseRoutine {
                    id: "routine".into(),
                    expected_version: 1,
                    reason: "pause".into(),
                },
            ],
        };
        assert_eq!(store.apply_safety_batch(&batch), Err(F1RecordError::Codec));
        assert_eq!(store.allocation("allocation"), Ok(Some(allocation())));
        let read = store.db.begin_read().unwrap();
        let rows = read.open_table(super::TABLE).unwrap();
        assert_eq!(rows.get(key.native_key()).unwrap().unwrap().value(), text);
    }

    #[test]
    fn oversized_redb_and_ferro_rows_are_bounded_and_redb_batch_rolls_back() {
        use fl_core::f1_records::{ControlRecordKey, MAX_RECORD_BYTES, SafetyChange};
        let tmp = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&tmp.path().join("oversized.redb")).unwrap();
        store.insert_proposal(routine()).unwrap();
        let key = ControlRecordKey::allocation("oversized").unwrap();
        let data = "x".repeat(MAX_RECORD_BYTES + 1);
        let value = serde_json::to_string(&data).unwrap();
        let tx = store.db.begin_write().unwrap();
        {
            let mut rows = tx.open_table(super::TABLE).unwrap();
            rows.insert(key.native_key(), value.as_str()).unwrap();
        }
        tx.commit().unwrap();
        assert_eq!(store.allocation("oversized"), Err(F1RecordError::TooLarge));
        let batch = SafetyBatch {
            changes: vec![
                SafetyChange::PauseRoutine {
                    id: "routine".into(),
                    expected_version: 1,
                    reason: "pause".into(),
                },
                SafetyChange::RequestAllocationStop {
                    id: "oversized".into(),
                    expected_version: 1,
                },
            ],
        };
        assert_eq!(
            store.apply_safety_batch(&batch),
            Err(F1RecordError::TooLarge)
        );
        assert_eq!(store.routine("routine").unwrap(), Some(routine()));
        let ferro = FerroStore::open(tmp.path().join("oversized.fls")).unwrap();
        ferro.seed_f1_test_document(key.native_key(), serde_json::Value::String(data));
        assert_eq!(ferro.allocation("oversized"), Err(F1RecordError::TooLarge));
        assert_eq!(
            ferro.apply_safety_batch(&batch),
            Err(F1RecordError::UnsupportedLocal)
        );
    }

    #[test]
    fn safety_transition_survives_more_than_four_mib_of_other_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&tmp.path().join("large.redb")).unwrap();
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

    #[test]
    fn ferro_independent_handles_refuse_writes_even_when_they_race() {
        use std::sync::{Arc, Barrier};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("independent.fls");
        let handles = [
            Arc::new(FerroStore::open(&path).unwrap()),
            Arc::new(FerroStore::open(&path).unwrap()),
        ];
        let barrier = Arc::new(Barrier::new(3));
        let threads: Vec<_> = handles
            .into_iter()
            .map(|store| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.insert_allocation(allocation())
                })
            })
            .collect();
        barrier.wait();
        for thread in threads {
            assert_eq!(thread.join().unwrap(), Err(F1RecordError::UnsupportedLocal));
        }
        let store = FerroStore::open(&path).unwrap();
        assert_eq!(store.allocation("allocation").unwrap(), None);
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
