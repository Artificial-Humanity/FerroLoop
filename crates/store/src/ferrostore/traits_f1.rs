//! Locally transactional F1 preparation rows. Remote handles refuse before I/O.
use super::{FerroStore, doc};
use crate::FORMAT_WITH_F1;
use fl_core::f1_records::{
    AllocationEvidenceRecord, ControlRecordKey, F1PreparationStore, F1RecordError,
    IntentEvidenceRecord, RoutineProposalRecord, SafetyBatch, SafetyChange,
    decode_allocation_record, decode_intent_record, decode_routine_record, encode_record,
};

fn backend_error(e: impl std::fmt::Display) -> F1RecordError {
    F1RecordError::Backend(e.to_string())
}
impl FerroStore {
    fn f1_get(&self, key: &ControlRecordKey) -> Result<Option<Vec<u8>>, F1RecordError> {
        if self.is_remote() {
            return Err(F1RecordError::UnsupportedRemote);
        }
        self.with_read(|tx| {
            tx.get(doc::F1_RECORDS, key.native_key())
                .map(|v| serde_json::to_vec(&v))
                .transpose()
                .map_err(super::backend)
        })
        .map_err(backend_error)
    }
    fn f1_insert(&self, key: ControlRecordKey, bytes: Vec<u8>) -> Result<(), F1RecordError> {
        if self.is_remote() {
            return Err(F1RecordError::UnsupportedRemote);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| F1RecordError::Codec)?;
        self.with_write(|tx| {
            if tx.get(doc::F1_RECORDS, key.native_key()).is_some() {
                return Ok(Err(F1RecordError::Conflict));
            }
            tx.put(doc::F1_RECORDS, key.native_key(), value.clone());
            doc::raise_format(tx, FORMAT_WITH_F1)?;
            Ok(Ok(()))
        })
        .map_err(backend_error)?
    }
    fn f1_batch(&self, batch: &SafetyBatch) -> Result<(), F1RecordError> {
        if self.is_remote() {
            return Err(F1RecordError::UnsupportedRemote);
        }
        self.with_write(|tx| {
            let staged = crate::f1_preparation::stage_batch(batch, |key| {
                tx.get(doc::F1_RECORDS, key)
                    .map(|v| serde_json::to_vec(&v).map_err(|_| F1RecordError::Codec))
                    .transpose()
            });
            let staged = match staged {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            for (key, bytes) in staged {
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(super::backend)?;
                tx.put(doc::F1_RECORDS, key, value);
            }
            doc::raise_format(tx, FORMAT_WITH_F1)?;
            Ok(Ok(()))
        })
        .map_err(backend_error)?
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
    fn insert_proposal(&self, row: RoutineProposalRecord) -> Result<(), F1RecordError> {
        if self.is_remote() {
            return Err(F1RecordError::UnsupportedRemote);
        }
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
        if self.is_remote() {
            return Err(F1RecordError::UnsupportedRemote);
        }
        row.validate_for_insert()?;
        self.f1_insert(
            ControlRecordKey::intent(row.definition.id())?,
            encode_record(&row)?,
        )
    }
    fn insert_allocation(&self, row: AllocationEvidenceRecord) -> Result<(), F1RecordError> {
        if self.is_remote() {
            return Err(F1RecordError::UnsupportedRemote);
        }
        row.validate_for_insert()?;
        self.f1_insert(
            ControlRecordKey::allocation(&row.allocation_id)?,
            encode_record(&row)?,
        )
    }
    fn apply_safety_batch(&self, b: &SafetyBatch) -> Result<(), F1RecordError> {
        self.f1_batch(b)
    }
    fn pause_routine(&self, id: &str, v: u64, reason: &str) -> Result<(), F1RecordError> {
        self.f1_batch(&SafetyBatch {
            changes: vec![SafetyChange::PauseRoutine {
                id: id.into(),
                expected_version: v,
                reason: reason.into(),
            }],
        })
    }
    fn quarantine_intent(&self, id: &str, v: u64) -> Result<(), F1RecordError> {
        self.f1_batch(&SafetyBatch {
            changes: vec![SafetyChange::QuarantineIntent {
                id: id.into(),
                expected_version: v,
            }],
        })
    }
    fn request_allocation_stop(&self, id: &str, v: u64) -> Result<(), F1RecordError> {
        self.f1_batch(&SafetyBatch {
            changes: vec![SafetyChange::RequestAllocationStop {
                id: id.into(),
                expected_version: v,
            }],
        })
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
