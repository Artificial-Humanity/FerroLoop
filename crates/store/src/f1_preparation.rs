//! Fail-closed native F1 boundary and staging for the separate SQLite preparation store.
//!
//! Native stores do not persist F1 preparation records. Remote Ferro rejects every
//! operation before I/O; local native stores refuse preparation and authority methods.
use crate::{AnyStore, FerroStore, RedbStore};
use fl_core::f1_records::{
    AllocationEvidenceRecord, ControlRecordKey, F1PreparationStore, F1RecordError,
    IntentEvidenceRecord, RoutineProposalRecord, SafetyBatch, SafetyChange,
    decode_allocation_record, decode_intent_record, decode_routine_record, encode_record,
    prepare_allocation_stop, prepare_intent_quarantine, prepare_routine_pause,
};

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

    #[test]
    fn sqlite_preparation_does_not_interfere_with_default_redb_migration() {
        use crate::any::{BackendKind, StoreOptions, Unmigrated, open_store_with};
        let dir = tempfile::tempdir().unwrap();
        let native_path = dir.path().join("native.redb");
        {
            drop(RedbStore::open(&native_path).unwrap());
        }
        let sqlite_path = dir.path().join("prep.sqlite");
        let sqlite = crate::SqliteF1PreparationStore::open(&sqlite_path).unwrap();
        sqlite.insert_allocation(allocation()).unwrap();
        sqlite.request_allocation_stop("allocation", 1).unwrap();
        let options = StoreOptions {
            backend: Some(BackendKind::Ferro),
            unmigrated: Unmigrated::Migrate,
            ..StoreOptions::default()
        };
        let native = open_store_with(&native_path, &options).unwrap();
        assert!(native.as_ferro().is_some());
        assert!(native_path.with_extension("fls").is_file());
        assert_eq!(
            sqlite.allocation("allocation").unwrap().unwrap().safety,
            SafetyState::StopRequested
        );
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

    #[test]
    fn sqlite_preparation_rows_survive_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f1.sqlite");
        let store = crate::SqliteF1PreparationStore::open(&path).unwrap();
        store.insert_allocation(allocation()).unwrap();
        assert_eq!(
            store.insert_allocation(allocation()),
            Err(F1RecordError::Conflict)
        );
        store.request_allocation_stop("allocation", 1).unwrap();
        drop(store);
        let reopened = crate::SqliteF1PreparationStore::open(&path).unwrap();
        assert_eq!(
            reopened.allocation("allocation").unwrap().unwrap().safety,
            SafetyState::StopRequested
        );
    }
    #[test]
    fn sqlite_independent_connections_allow_one_cas_winner() {
        use std::sync::{Arc, Barrier};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f1.sqlite");
        let first = crate::SqliteF1PreparationStore::open(&path).unwrap();
        first.insert_allocation(allocation()).unwrap();
        let other = crate::SqliteF1PreparationStore::open(&path).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = [first, other]
            .into_iter()
            .map(|s| {
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    s.request_allocation_stop("allocation", 1)
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| **r == Err(F1RecordError::Conflict))
                .count(),
            1
        );
    }

    #[test]
    fn sqlite_batch_conflict_and_corrupt_row_roll_back() {
        use fl_core::f1_records::{ControlRecordKey, SafetyBatch, SafetyChange};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f1.sqlite");
        let store = crate::SqliteF1PreparationStore::open(&path).unwrap();
        store.insert_allocation(allocation()).unwrap();
        store.insert_intent(intent()).unwrap();
        let batch = SafetyBatch {
            changes: vec![
                SafetyChange::request_allocation_stop("allocation", 1).unwrap(),
                SafetyChange::quarantine_intent("intent", 2).unwrap(),
            ],
        };
        assert_eq!(
            store.apply_safety_batch(&batch),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(
            store.allocation("allocation").unwrap().unwrap().safety,
            SafetyState::Inert
        );
        let raw = rusqlite::Connection::open(&path).unwrap();
        let key = ControlRecordKey::intent("intent").unwrap();
        raw.execute(
            "UPDATE f1_records SET value=?1 WHERE record_key=?2",
            rusqlite::params![vec![b'x'; 65537], key.native_key()],
        )
        .unwrap();
        let batch = SafetyBatch {
            changes: vec![
                SafetyChange::request_allocation_stop("allocation", 1).unwrap(),
                SafetyChange::quarantine_intent("intent", 1).unwrap(),
            ],
        };
        assert_eq!(
            store.apply_safety_batch(&batch),
            Err(F1RecordError::TooLarge)
        );
        assert_eq!(
            store.allocation("allocation").unwrap().unwrap().safety,
            SafetyState::Inert
        );
    }

    #[test]
    fn sqlite_refuses_lossy_nested_data_and_foreign_schema() {
        use fl_core::f1_records::ControlRecordKey;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f1.sqlite");
        let store = crate::SqliteF1PreparationStore::open(&path).unwrap();
        store.insert_proposal(routine()).unwrap();
        let raw = rusqlite::Connection::open(&path).unwrap();
        let key = ControlRecordKey::routine("routine").unwrap();
        let mut row = serde_json::to_value(routine()).unwrap();
        row["spec"]["unknown_future_field"] = serde_json::json!(true);
        raw.execute(
            "UPDATE f1_records SET value=?1 WHERE record_key=?2",
            rusqlite::params![serde_json::to_vec(&row).unwrap(), key.native_key()],
        )
        .unwrap();
        assert_eq!(
            store.pause_routine("routine", 1, "pause"),
            Err(F1RecordError::Codec)
        );
        let foreign = tmp.path().join("foreign.sqlite");
        let db = rusqlite::Connection::open(&foreign).unwrap();
        db.execute_batch("CREATE TABLE other (x TEXT);").unwrap();
        drop(db);
        assert!(crate::SqliteF1PreparationStore::open(&foreign).is_err());
    }

    #[test]
    fn sqlite_many_independent_rows_do_not_block_safety() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f1.sqlite");
        let store = crate::SqliteF1PreparationStore::open(&path).unwrap();
        for n in 0..150 {
            let mut row = routine();
            row.spec.routine_id = format!("row-{n}");
            row.spec.project = "x".repeat(32 * 1024);
            store.insert_proposal(row).unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() > 4 * 1024 * 1024);
        store.pause_routine("row-0", 1, "stop").unwrap();
        assert!(matches!(
            store.routine("row-0").unwrap().unwrap().spec.state,
            fl_core::routine::RoutineState::Paused { .. }
        ));
        assert_eq!(store.admit(), Err(F1RecordError::UnverifiedAuthority));
        assert_eq!(
            store.accept_effect(),
            Err(F1RecordError::UnverifiedAuthority)
        );
        assert_eq!(store.settle(), Err(F1RecordError::UnverifiedAuthority));
    }
    #[test]
    fn sqlite_refuses_extra_triggers_even_after_open() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f1.sqlite");
        let store = crate::SqliteF1PreparationStore::open(&path).unwrap();
        store.insert_allocation(allocation()).unwrap();
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "CREATE TRIGGER extra AFTER UPDATE ON f1_records BEGIN DELETE FROM f1_records; END;",
        )
        .unwrap();
        assert!(store.request_allocation_stop("allocation", 1).is_err());
        let count: i64 = raw
            .query_row("SELECT count(*) FROM f1_records", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(store);
        assert!(crate::SqliteF1PreparationStore::open(&path).is_err());
    }
    #[test]
    fn sqlite_preparation_refuses_memory_pseudopath() {
        assert!(crate::SqliteF1PreparationStore::open(std::path::Path::new(":memory:")).is_err());
    }
    #[test]
    fn sqlite_refuses_similar_reserved_prefix_schema_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let foreign = tmp.path().join("foreign.sqlite");
        let raw = rusqlite::Connection::open(&foreign).unwrap();
        raw.execute_batch(
            "CREATE TABLE sqliteXforeign (x); INSERT INTO sqliteXforeign VALUES (42);",
        )
        .unwrap();
        drop(raw);
        let before = std::fs::read(&foreign).unwrap();
        assert!(crate::SqliteF1PreparationStore::open(&foreign).is_err());
        assert_eq!(std::fs::read(&foreign).unwrap(), before);
        for timing in ["BEFORE INSERT", "AFTER UPDATE"] {
            let path = tmp
                .path()
                .join(format!("{}.sqlite", timing.replace(' ', "-")));
            let store = crate::SqliteF1PreparationStore::open(&path).unwrap();
            store.insert_allocation(allocation()).unwrap();
            let raw = rusqlite::Connection::open(&path).unwrap();
            raw.execute_batch(&format!("CREATE TRIGGER sqliteXevil {timing} ON f1_records BEGIN DELETE FROM f1_records; END;")).unwrap();
            if timing == "BEFORE INSERT" {
                let mut another = allocation();
                another.allocation_id = "another".into();
                another.root_id = "another".into();
                assert!(store.insert_allocation(another).is_err());
            } else {
                assert!(store.request_allocation_stop("allocation", 1).is_err());
            }
            let count: i64 = raw
                .query_row("SELECT count(*) FROM f1_records", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1);
            drop(store);
            assert!(crate::SqliteF1PreparationStore::open(&path).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_refuses_symlinked_parent_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("actual");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, tmp.path().join("link")).unwrap();
        assert!(crate::SqliteF1PreparationStore::open(&tmp.path().join("link/db.sqlite")).is_err());
        assert!(!target.join("db.sqlite").exists());
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_refuses_final_symlink_swap_before_open() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim.sqlite");
        let store = crate::SqliteF1PreparationStore::open(&victim).unwrap();
        store.insert_allocation(allocation()).unwrap();
        drop(store);
        let before = std::fs::read(&victim).unwrap();
        let path = tmp.path().join("requested.sqlite");
        std::fs::File::create(&path).unwrap();
        let replacement = path.clone();
        let link_target = victim.clone();
        crate::f1_sqlite::before_sqlite_open(move || {
            std::fs::remove_file(&replacement).unwrap();
            std::os::unix::fs::symlink(&link_target, &replacement).unwrap();
        });
        assert!(crate::SqliteF1PreparationStore::open(&path).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), before);
    }
    #[test]
    fn sqlite_refuses_forged_reserved_prefix_trigger() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("forged.sqlite");
        let store = crate::SqliteF1PreparationStore::open(&path).unwrap();
        store.insert_allocation(allocation()).unwrap();
        drop(store);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch("PRAGMA writable_schema=ON; INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql) VALUES('trigger','sqlite_evil','f1_records',0,'CREATE TRIGGER sqlite_evil AFTER UPDATE ON f1_records BEGIN DELETE FROM f1_records; END');").unwrap();
        drop(raw);
        assert!(crate::SqliteF1PreparationStore::open(&path).is_err());
        let raw = rusqlite::Connection::open(&path).unwrap();
        let count: i64 = raw
            .query_row("SELECT count(*) FROM f1_records", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
