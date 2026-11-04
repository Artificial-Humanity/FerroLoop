//! Explicitly selected, local-only SQLite storage for inert F1 preparation records.
//!
//! This is a separate database, not the hub's native Ferro/redb control ledger. Its
//! transactions cannot coordinate with hub records and it does not authorize or dispatch
//! work. No caller should treat it as the production admission store.
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use fl_core::f1_records::{
    AllocationEvidenceRecord, ControlRecordKey, F1PreparationStore, F1RecordError,
    IntentEvidenceRecord, MAX_RECORD_BYTES, RoutineProposalRecord, SafetyBatch, SafetyChange,
    decode_allocation_record, decode_intent_record, decode_routine_record, encode_record,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

#[cfg(all(test, unix))]
thread_local! {
    static BEFORE_SQLITE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
}

#[cfg(all(test, unix))]
pub(crate) fn before_sqlite_open(f: impl FnOnce() + 'static) {
    BEFORE_SQLITE_OPEN.with(|hook| *hook.borrow_mut() = Some(Box::new(f)));
}

const APPLICATION_ID: i64 = 0x4631_5052; // F1PR
const SCHEMA_VERSION: i64 = 1;
const CREATE_TABLE: &str = "CREATE TABLE f1_records (record_key TEXT PRIMARY KEY NOT NULL, value BLOB NOT NULL) WITHOUT ROWID";

fn check_schema(db: &Connection) -> Result<(), F1RecordError> {
    let app: i64 = db
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .map_err(backend)?;
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(backend)?;
    let count: i64 = db
        .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
        .map_err(backend)?;
    let sql: Option<String> = db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='f1_records'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(backend)?;
    if app != APPLICATION_ID
        || version != SCHEMA_VERSION
        || count != 1
        || sql.as_deref() != Some(CREATE_TABLE)
    {
        return Err(F1RecordError::InvalidRecord);
    }
    Ok(())
}

fn backend(e: impl std::fmt::Display) -> F1RecordError {
    F1RecordError::Backend(e.to_string())
}

/// Reject symlinked ancestors and traversal. The caller must also keep the parent
/// directories trusted and fenced against concurrent replacement throughout open/use:
/// SQLite's no-follow flag protects the final component, not mutable ancestors.
fn check_parent(path: &Path) -> Result<(), F1RecordError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(backend)?.join(path)
    };
    let parent = absolute.parent().ok_or(F1RecordError::InvalidRecord)?;
    let mut prefix = PathBuf::new();
    for component in parent.components() {
        match component {
            Component::ParentDir => return Err(F1RecordError::InvalidRecord),
            Component::CurDir => continue,
            other => prefix.push(other.as_os_str()),
        }
        let meta = prefix.symlink_metadata().map_err(backend)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(F1RecordError::InvalidRecord);
        }
    }
    Ok(())
}

/// Separate opt-in local SQLite preparation database. Operations require external caller
/// authorization; records are inert evidence, never authority or admission.
pub struct SqliteF1PreparationStore {
    db: Mutex<Connection>,
}

impl SqliteF1PreparationStore {
    /// Open an existing F1 preparation database or initialize an empty file. Refuses foreign
    /// databases and unsupported schemas. This does not open or modify the hub's store.
    /// The caller must ensure the parent path is trusted and cannot be replaced concurrently
    /// while the database or its SQLite sidecars are in use.
    pub fn open(path: &Path) -> Result<Self, F1RecordError> {
        if path.as_os_str().is_empty()
            || path == Path::new(":memory:")
            || path
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(F1RecordError::InvalidRecord);
        }
        check_parent(path)?;
        // Create only new files with private permissions. Existing files are never chmodded.
        crate::private::create_private_file(path).map_err(backend)?;
        #[cfg(all(test, unix))]
        BEFORE_SQLITE_OPEN.with(|hook| {
            if let Some(f) = hook.borrow_mut().take() {
                f()
            }
        });
        let mut db = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(backend)?;
        db.busy_timeout(Duration::from_secs(5)).map_err(backend)?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(backend)?;
        let app: i64 = tx
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(backend)?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(backend)?;
        if app == 0 && version == 0 {
            let count: i64 = tx
                .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
                .map_err(backend)?;
            if count != 0 {
                return Err(F1RecordError::InvalidRecord);
            }
            tx.execute_batch(CREATE_TABLE).map_err(backend)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)
                .map_err(backend)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(backend)?;
        } else if app != APPLICATION_ID || version != SCHEMA_VERSION {
            return Err(F1RecordError::InvalidRecord);
        }
        check_schema(&tx)?;
        tx.commit().map_err(backend)?;
        Ok(Self { db: Mutex::new(db) })
    }
    fn read_from(db: &Connection, key: &str) -> Result<Option<Vec<u8>>, F1RecordError> {
        // Check the stored blob length before asking SQLite to materialize the value.
        // Callers hold a read or write transaction, so the row cannot change between queries.
        let size: Option<(i64, String)> = db
            .query_row(
                "SELECT length(value), typeof(value) FROM f1_records WHERE record_key = ?1",
                [key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(backend)?;
        match size {
            None => Ok(None),
            Some((size, kind))
                if kind == "blob" && size >= 0 && size as u64 <= MAX_RECORD_BYTES as u64 =>
            {
                db.query_row(
                    "SELECT value FROM f1_records WHERE record_key = ?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(backend)
            }
            Some((_, kind)) if kind != "blob" => Err(F1RecordError::Codec),
            Some(_) => Err(F1RecordError::TooLarge),
        }
    }
    fn get(&self, key: &ControlRecordKey) -> Result<Option<Vec<u8>>, F1RecordError> {
        let mut db = self.db.lock().map_err(backend)?;
        let tx = db.transaction().map_err(backend)?;
        check_schema(&tx)?;
        let result = Self::read_from(&tx, key.native_key())?;
        tx.commit().map_err(backend)?;
        Ok(result)
    }
    fn insert(&self, key: ControlRecordKey, value: Vec<u8>) -> Result<(), F1RecordError> {
        let mut db = self.db.lock().map_err(backend)?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(backend)?;
        check_schema(&tx)?;
        if Self::read_from(&tx, key.native_key())?.is_some() {
            return Err(F1RecordError::Conflict);
        }
        tx.execute(
            "INSERT INTO f1_records(record_key,value) VALUES(?1,?2)",
            params![key.native_key(), value],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)
    }
}

impl F1PreparationStore for SqliteF1PreparationStore {
    fn routine(&self, id: &str) -> Result<Option<RoutineProposalRecord>, F1RecordError> {
        self.get(&ControlRecordKey::routine(id)?)?
            .map(|v| decode_routine_record(&v, id))
            .transpose()
    }
    fn intent(&self, id: &str) -> Result<Option<IntentEvidenceRecord>, F1RecordError> {
        self.get(&ControlRecordKey::intent(id)?)?
            .map(|v| decode_intent_record(&v, id))
            .transpose()
    }
    fn allocation(&self, id: &str) -> Result<Option<AllocationEvidenceRecord>, F1RecordError> {
        self.get(&ControlRecordKey::allocation(id)?)?
            .map(|v| decode_allocation_record(&v, id))
            .transpose()
    }
    fn insert_proposal(&self, row: RoutineProposalRecord) -> Result<(), F1RecordError> {
        row.validate()?;
        if row.version != 1 || !matches!(row.spec.state, fl_core::routine::RoutineState::Proposed) {
            return Err(F1RecordError::InvalidRecord);
        }
        self.insert(
            ControlRecordKey::routine(&row.spec.routine_id)?,
            encode_record(&row)?,
        )
    }
    fn insert_intent(&self, row: IntentEvidenceRecord) -> Result<(), F1RecordError> {
        row.validate_for_insert()?;
        self.insert(
            ControlRecordKey::intent(row.definition.id())?,
            encode_record(&row)?,
        )
    }
    fn insert_allocation(&self, row: AllocationEvidenceRecord) -> Result<(), F1RecordError> {
        row.validate_for_insert()?;
        self.insert(
            ControlRecordKey::allocation(&row.allocation_id)?,
            encode_record(&row)?,
        )
    }
    fn apply_safety_batch(&self, batch: &SafetyBatch) -> Result<(), F1RecordError> {
        batch.validate()?;
        let mut db = self.db.lock().map_err(backend)?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(backend)?;
        check_schema(&tx)?;
        let changes = crate::f1_preparation::stage_batch(batch, |key| Self::read_from(&tx, key))?;
        for (key, value) in changes {
            if tx
                .execute(
                    "UPDATE f1_records SET value=?1 WHERE record_key=?2",
                    params![value, key],
                )
                .map_err(backend)?
                != 1
            {
                return Err(F1RecordError::Conflict);
            }
        }
        tx.commit().map_err(backend)
    }
    fn pause_routine(&self, id: &str, version: u64, reason: &str) -> Result<(), F1RecordError> {
        self.apply_safety_batch(&SafetyBatch {
            changes: vec![SafetyChange::pause_routine(id, version, reason)?],
        })
    }
    fn quarantine_intent(&self, id: &str, version: u64) -> Result<(), F1RecordError> {
        self.apply_safety_batch(&SafetyBatch {
            changes: vec![SafetyChange::quarantine_intent(id, version)?],
        })
    }
    fn request_allocation_stop(&self, id: &str, version: u64) -> Result<(), F1RecordError> {
        self.apply_safety_batch(&SafetyBatch {
            changes: vec![SafetyChange::request_allocation_stop(id, version)?],
        })
    }
}
