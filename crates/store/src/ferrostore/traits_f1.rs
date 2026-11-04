//! Read-only compatibility for local Ferro F1 rows; all mutations refuse before I/O.
use super::{FerroStore, doc};
use fl_core::f1_records::{
    AllocationEvidenceRecord, ControlRecordKey, F1PreparationStore, F1RecordError,
    IntentEvidenceRecord, MAX_RECORD_BYTES, RoutineProposalRecord, SafetyBatch,
    decode_allocation_record, decode_intent_record, decode_routine_record,
};

fn backend_error(e: impl std::fmt::Display) -> F1RecordError {
    F1RecordError::Backend(e.to_string())
}
/// Serialize directly from the transaction's borrowed document into a bounded buffer.
/// The document itself is already resident in the local store; this never clones it.
fn bounded_f1_document(tx: &super::Txn, key: &str) -> Result<Option<Vec<u8>>, F1RecordError> {
    use std::io::{self, Write};
    struct LimitedBuffer {
        bytes: Vec<u8>,
        overflowed: bool,
    }
    impl Write for LimitedBuffer {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if data.len() > MAX_RECORD_BYTES.saturating_sub(self.bytes.len()) {
                self.overflowed = true;
                return Err(io::Error::other("F1 record exceeds limit"));
            }
            self.bytes.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let super::Base::Local(base) = &tx.base else {
        return Err(F1RecordError::UnsupportedRemote);
    };
    let Some(value) = tx.overlay.doc(base, doc::F1_RECORDS, key) else {
        return Ok(None);
    };
    let mut out = LimitedBuffer {
        bytes: Vec::new(),
        overflowed: false,
    };
    if serde_json::to_writer(&mut out, value).is_err() {
        return Err(if out.overflowed {
            F1RecordError::TooLarge
        } else {
            F1RecordError::Codec
        });
    }
    Ok(Some(out.bytes))
}

impl FerroStore {
    /// Test-only fixture for read-only compatibility with existing local rows.
    #[cfg(test)]
    pub(crate) fn seed_f1_test_document(&self, key: &str, value: serde_json::Value) {
        self.with_write(|tx| {
            tx.put(doc::F1_RECORDS, key, value.clone());
            Ok(())
        })
        .unwrap();
    }
    fn f1_get(&self, key: &ControlRecordKey) -> Result<Option<Vec<u8>>, F1RecordError> {
        if self.is_remote() {
            return Err(F1RecordError::UnsupportedRemote);
        }
        self.with_read(|tx| Ok(bounded_f1_document(tx, key.native_key())))
            .map_err(backend_error)?
    }
    // Sync is per-handle, so a local handle cannot rule out another synced handle
    // of the same file. Refuse Ferro F1 mutations until a store-wide fence exists.
    fn f1_refuse_mutation(&self) -> Result<(), F1RecordError> {
        if self.is_remote() {
            Err(F1RecordError::UnsupportedRemote)
        } else {
            Err(F1RecordError::UnsupportedLocal)
        }
    }
}
impl F1PreparationStore for FerroStore {
    fn routine(&self, id: &str) -> Result<Option<RoutineProposalRecord>, F1RecordError> {
        if self.is_remote() {
            return Err(F1RecordError::UnsupportedRemote);
        }
        self.f1_get(&ControlRecordKey::routine(id)?)?
            .map(|v| decode_routine_record(&v, id))
            .transpose()
    }
    fn intent(&self, id: &str) -> Result<Option<IntentEvidenceRecord>, F1RecordError> {
        if self.is_remote() {
            return Err(F1RecordError::UnsupportedRemote);
        }
        self.f1_get(&ControlRecordKey::intent(id)?)?
            .map(|v| decode_intent_record(&v, id))
            .transpose()
    }
    fn allocation(&self, id: &str) -> Result<Option<AllocationEvidenceRecord>, F1RecordError> {
        if self.is_remote() {
            return Err(F1RecordError::UnsupportedRemote);
        }
        self.f1_get(&ControlRecordKey::allocation(id)?)?
            .map(|v| decode_allocation_record(&v, id))
            .transpose()
    }
    fn insert_proposal(&self, _row: RoutineProposalRecord) -> Result<(), F1RecordError> {
        self.f1_refuse_mutation()
    }
    fn insert_intent(&self, _row: IntentEvidenceRecord) -> Result<(), F1RecordError> {
        self.f1_refuse_mutation()
    }
    fn insert_allocation(&self, _row: AllocationEvidenceRecord) -> Result<(), F1RecordError> {
        self.f1_refuse_mutation()
    }
    fn apply_safety_batch(&self, _batch: &SafetyBatch) -> Result<(), F1RecordError> {
        self.f1_refuse_mutation()
    }
    fn pause_routine(&self, _id: &str, _v: u64, _reason: &str) -> Result<(), F1RecordError> {
        self.f1_refuse_mutation()
    }
    fn quarantine_intent(&self, _id: &str, _v: u64) -> Result<(), F1RecordError> {
        self.f1_refuse_mutation()
    }
    fn request_allocation_stop(&self, _id: &str, _v: u64) -> Result<(), F1RecordError> {
        self.f1_refuse_mutation()
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
