//! Data-only, per-record contracts for future routine and intent persistence.
//!
//! Records are evidence, never authorization. This module grants no admission, dispatch,
//! settlement, host control, or durable transaction implementation. Implementations of the
//! preparation trait must authorize callers at their own boundary and execute each CAS in a
//! single backend transaction. In particular, a process-local lock around an unlocked persistent
//! read/write is not an implementation of this contract.

use crate::intent::{CommitteeIntent, DelegateIntent};
use crate::routine::{RoutineSpec, RoutineState};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Each stored row is bounded independently; no aggregate snapshot limit is imposed here.
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_ID_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum F1RecordError {
    #[error("invalid record identity")]
    InvalidIdentity,
    #[error("invalid record")]
    InvalidRecord,
    #[error("record exceeds size limit")]
    TooLarge,
    #[error("record codec error")]
    Codec,
    #[error("record does not exist")]
    Missing,
    #[error("record revision conflict")]
    Conflict,
    #[error("required authority could not be verified")]
    UnverifiedAuthority,
    #[error("routine control operations are not supported over this remote backend")]
    UnsupportedRemote,
    #[error("storage backend failure: {0}")]
    Backend(String),
}

fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}

/// A typed key. Native keys encode the complete ID injectively. Slugs fit the existing
/// 64-byte StateStore limit, but are SHA-256 indexes, not collision-free identities. Any
/// slug-backed store MUST compare the complete stored ID and record kind before every read
/// and CAS and reject an occupied slug whose ID differs, without changing either record.
/// Neither form is an authorization token.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ControlRecordKey {
    native_key: String,
    slug: String,
}
impl ControlRecordKey {
    fn new(kind: &str, id: &str) -> Result<Self, F1RecordError> {
        use sha2::{Digest, Sha256};
        if !valid_id(id) {
            return Err(F1RecordError::InvalidIdentity);
        }
        let mut native_key = format!("f1-{kind}-");
        for byte in id.as_bytes() {
            use std::fmt::Write;
            write!(&mut native_key, "{byte:02x}").expect("writing to String cannot fail");
        }
        let digest = Sha256::digest(id.as_bytes());
        const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
        let mut encoded = String::new();
        let mut acc = 0u32;
        let mut bits = 0u32;
        for byte in digest {
            acc = (acc << 8) | u32::from(byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                encoded.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
            }
            acc &= (1 << bits) - 1;
        }
        if bits > 0 {
            encoded.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
        }
        Ok(Self {
            native_key,
            slug: format!("f1-{kind}-{encoded}"),
        })
    }
    pub fn routine(id: &str) -> Result<Self, F1RecordError> {
        Self::new("r", id)
    }
    pub fn intent(id: &str) -> Result<Self, F1RecordError> {
        Self::new("i", id)
    }
    pub fn allocation(id: &str) -> Result<Self, F1RecordError> {
        Self::new("a", id)
    }
    pub fn native_key(&self) -> &str {
        &self.native_key
    }
    pub fn slug(&self) -> &str {
        &self.slug
    }
}

/// JSON is bounded both before parsing untrusted stored bytes and after encoding.
pub fn encode_record<T: Serialize>(value: &T) -> Result<Vec<u8>, F1RecordError> {
    let bytes = serde_json::to_vec(value).map_err(|_| F1RecordError::Codec)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(F1RecordError::TooLarge);
    }
    Ok(bytes)
}
pub fn decode_record<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, F1RecordError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(F1RecordError::TooLarge);
    }
    serde_json::from_slice(bytes).map_err(|_| F1RecordError::Codec)
}

/// Data-only proposal. A stored record alone never creates an approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineProposalRecord {
    pub version: u64,
    pub spec: RoutineSpec,
}
impl RoutineProposalRecord {
    pub fn validate(&self) -> Result<(), F1RecordError> {
        ControlRecordKey::routine(&self.spec.routine_id)?;
        if self.version == 0
            || self.spec.revision == 0
            || self.spec.approval.is_some()
            || !matches!(
                self.spec.state,
                RoutineState::Proposed | RoutineState::Paused { .. } | RoutineState::Cancelled
            )
        {
            return Err(F1RecordError::InvalidRecord);
        }
        encode_record(self)?;
        Ok(())
    }
}

/// Stored intent data is inert evidence; it is not an admitted or dispatched intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "intent", rename_all = "snake_case")]
pub enum IntentDefinition {
    Delegate(DelegateIntent),
    Committee(CommitteeIntent),
}
impl IntentDefinition {
    pub fn id(&self) -> &str {
        match self {
            Self::Delegate(x) => &x.intent_id,
            Self::Committee(x) => &x.intent_id,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyState {
    Inert,
    Quarantined,
    StopRequested,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentEvidenceRecord {
    pub version: u64,
    pub definition: IntentDefinition,
    pub safety: SafetyState,
}
impl IntentEvidenceRecord {
    pub fn validate(&self) -> Result<(), F1RecordError> {
        ControlRecordKey::intent(self.definition.id())?;
        if self.version == 0
            || matches!(&self.definition, IntentDefinition::Committee(c) if c.validate().is_err())
        {
            return Err(F1RecordError::InvalidRecord);
        }
        encode_record(self)?;
        Ok(())
    }
}

/// An inert allocation description, not proof of available or reserved money.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocationEvidenceRecord {
    pub version: u64,
    pub allocation_id: String,
    pub parent_id: Option<String>,
    pub root_id: String,
    pub source_window_id: String,
    pub deadline_utc: String,
    pub tokens: u64,
    pub cost_micros: u64,
    pub safety: SafetyState,
}
impl AllocationEvidenceRecord {
    pub fn validate(&self) -> Result<(), F1RecordError> {
        ControlRecordKey::allocation(&self.allocation_id)?;
        if self.version == 0
            || !valid_id(&self.root_id)
            || !valid_id(&self.source_window_id)
            || self
                .parent_id
                .as_ref()
                .is_some_and(|id| !valid_id(id) || id == &self.allocation_id)
            || (self.parent_id.is_none() && self.root_id != self.allocation_id)
            || (self.parent_id.is_some() && self.root_id == self.allocation_id)
            || self.tokens == 0
            || self.cost_micros == 0
            || self.deadline_utc.len() > 128
            || chrono::DateTime::parse_from_rfc3339(&self.deadline_utc).is_err()
        {
            return Err(F1RecordError::InvalidRecord);
        }
        encode_record(self)?;
        Ok(())
    }
}

/// Decode a specific row and compare its full ID, so a slug collision or misplaced row
/// fails closed. Backends must also perform this check within every transaction before CAS.
pub fn decode_routine_record(
    bytes: &[u8],
    expected_id: &str,
) -> Result<RoutineProposalRecord, F1RecordError> {
    ControlRecordKey::routine(expected_id)?;
    let row: RoutineProposalRecord = decode_record(bytes)?;
    row.validate()?;
    if row.spec.routine_id != expected_id {
        return Err(F1RecordError::Conflict);
    }
    Ok(row)
}
pub fn decode_intent_record(
    bytes: &[u8],
    expected_id: &str,
) -> Result<IntentEvidenceRecord, F1RecordError> {
    ControlRecordKey::intent(expected_id)?;
    let row: IntentEvidenceRecord = decode_record(bytes)?;
    row.validate()?;
    if row.definition.id() != expected_id {
        return Err(F1RecordError::Conflict);
    }
    Ok(row)
}
pub fn decode_allocation_record(
    bytes: &[u8],
    expected_id: &str,
) -> Result<AllocationEvidenceRecord, F1RecordError> {
    ControlRecordKey::allocation(expected_id)?;
    let row: AllocationEvidenceRecord = decode_record(bytes)?;
    row.validate()?;
    if row.allocation_id != expected_id {
        return Err(F1RecordError::Conflict);
    }
    Ok(row)
}

/// Compute one inert-record transition; a store must still compare and write it atomically.
/// A stop request is evidence only and does not mean that a process was stopped.
pub fn prepare_intent_quarantine(
    current: &IntentEvidenceRecord,
    expected_version: u64,
) -> Result<IntentEvidenceRecord, F1RecordError> {
    current.validate()?;
    if current.version != expected_version || current.safety != SafetyState::Inert {
        return Err(F1RecordError::Conflict);
    }
    let mut next = current.clone();
    next.version = current
        .version
        .checked_add(1)
        .ok_or(F1RecordError::Conflict)?;
    next.safety = SafetyState::Quarantined;
    next.validate()?;
    Ok(next)
}

pub fn prepare_allocation_stop(
    current: &AllocationEvidenceRecord,
    expected_version: u64,
) -> Result<AllocationEvidenceRecord, F1RecordError> {
    current.validate()?;
    if current.version != expected_version || current.safety == SafetyState::StopRequested {
        return Err(F1RecordError::Conflict);
    }
    let mut next = current.clone();
    next.version = current
        .version
        .checked_add(1)
        .ok_or(F1RecordError::Conflict)?;
    next.safety = SafetyState::StopRequested;
    next.validate()?;
    Ok(next)
}

/// A bounded safety request. It describes only an inert record transition; host effects
/// require independent authenticated control and a confirmed result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SafetyChange {
    PauseRoutine {
        id: String,
        expected_version: u64,
        reason: String,
    },
    QuarantineIntent {
        id: String,
        expected_version: u64,
    },
    RequestAllocationStop {
        id: String,
        expected_version: u64,
    },
}

/// At most this many rows may participate in one atomic safety request.
pub const MAX_SAFETY_CHANGES: usize = 32;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafetyBatch {
    pub changes: Vec<SafetyChange>,
}
impl SafetyBatch {
    pub fn validate(&self) -> Result<(), F1RecordError> {
        use std::collections::BTreeSet;
        if self.changes.is_empty() || self.changes.len() > MAX_SAFETY_CHANGES {
            return Err(F1RecordError::InvalidRecord);
        }
        let mut keys = BTreeSet::new();
        for change in &self.changes {
            let (key, version) = match change {
                SafetyChange::PauseRoutine {
                    id,
                    expected_version,
                    reason,
                } => {
                    if reason.trim().is_empty()
                        || reason.len() > 1024
                        || reason.chars().any(char::is_control)
                    {
                        return Err(F1RecordError::InvalidRecord);
                    }
                    (ControlRecordKey::routine(id)?, *expected_version)
                }
                SafetyChange::QuarantineIntent {
                    id,
                    expected_version,
                } => (ControlRecordKey::intent(id)?, *expected_version),
                SafetyChange::RequestAllocationStop {
                    id,
                    expected_version,
                } => (ControlRecordKey::allocation(id)?, *expected_version),
            };
            if version == 0 || !keys.insert(key.native_key().to_owned()) {
                return Err(F1RecordError::InvalidRecord);
            }
        }
        encode_record(self)?;
        Ok(())
    }
}

/// A narrow preparation-only contract. Each implementation must atomically compare the
/// identified row's `version` and apply the requested safety transition in one transaction;
/// failure must not mutate any row. Row identity is immutable. None of these methods grants
/// execution authority, starts/stops a host, or settles/refunds an allocation.
///
/// `request_stop` records an intent to stop; a host must independently authenticate, execute,
/// and confirm any actual containment. Implementations return backend errors on write failure.
pub trait F1PreparationStore {
    fn routine(&self, id: &str) -> Result<Option<RoutineProposalRecord>, F1RecordError>;
    fn intent(&self, id: &str) -> Result<Option<IntentEvidenceRecord>, F1RecordError>;
    fn allocation(&self, id: &str) -> Result<Option<AllocationEvidenceRecord>, F1RecordError>;
    /// Compare and apply every typed safety transition in one backend transaction, or apply
    /// none. The implementation must first validate the batch and verify full row identities,
    /// then fence every expected revision in the same transaction. No host I/O occurs inside it.
    fn apply_safety_batch(&self, batch: &SafetyBatch) -> Result<(), F1RecordError>;

    /// Insert only an unapproved `Proposed` routine with a fresh identity.
    fn insert_proposal(&self, record: RoutineProposalRecord) -> Result<(), F1RecordError>;
    /// Compare the row version, then pause the named routine; no unrelated row may change.
    fn pause_routine(
        &self,
        id: &str,
        expected_version: u64,
        reason: &str,
    ) -> Result<(), F1RecordError>;
    /// Compare the row version, then quarantine the named intent.
    fn quarantine_intent(&self, id: &str, expected_version: u64) -> Result<(), F1RecordError>;
    /// Compare the row version, then record a stop request without claiming it happened.
    fn request_allocation_stop(&self, id: &str, expected_version: u64)
    -> Result<(), F1RecordError>;
    /// No production admission exists in this preparatory contract.
    fn admit(&self) -> Result<(), F1RecordError> {
        Err(F1RecordError::UnverifiedAuthority)
    }
    /// No production effect acceptance exists in this preparatory contract.
    fn accept_effect(&self) -> Result<(), F1RecordError> {
        Err(F1RecordError::UnverifiedAuthority)
    }
    /// No production settlement exists in this preparatory contract.
    fn settle(&self) -> Result<(), F1RecordError> {
        Err(F1RecordError::UnverifiedAuthority)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::{AllocationAmount, Binding, DelegateTarget, QuorumRule, ResultSchema};

    struct RefusalOnly;
    impl F1PreparationStore for RefusalOnly {
        fn routine(&self, _: &str) -> Result<Option<RoutineProposalRecord>, F1RecordError> {
            Ok(None)
        }
        fn intent(&self, _: &str) -> Result<Option<IntentEvidenceRecord>, F1RecordError> {
            Ok(None)
        }
        fn allocation(&self, _: &str) -> Result<Option<AllocationEvidenceRecord>, F1RecordError> {
            Ok(None)
        }
        fn apply_safety_batch(&self, _: &SafetyBatch) -> Result<(), F1RecordError> {
            Err(F1RecordError::Missing)
        }
        fn insert_proposal(&self, _: RoutineProposalRecord) -> Result<(), F1RecordError> {
            Err(F1RecordError::Missing)
        }
        fn pause_routine(&self, _: &str, _: u64, _: &str) -> Result<(), F1RecordError> {
            Err(F1RecordError::Missing)
        }
        fn quarantine_intent(&self, _: &str, _: u64) -> Result<(), F1RecordError> {
            Err(F1RecordError::Missing)
        }
        fn request_allocation_stop(&self, _: &str, _: u64) -> Result<(), F1RecordError> {
            Err(F1RecordError::Missing)
        }
    }
    #[test]
    fn default_authority_sensitive_operations_refuse() {
        assert_eq!(RefusalOnly.admit(), Err(F1RecordError::UnverifiedAuthority));
        assert_eq!(
            RefusalOnly.accept_effect(),
            Err(F1RecordError::UnverifiedAuthority)
        );
        assert_eq!(
            RefusalOnly.settle(),
            Err(F1RecordError::UnverifiedAuthority)
        );
    }

    #[test]
    fn keys_are_typed_collision_free_and_bounded() {
        let a = ControlRecordKey::routine("a:b").unwrap();
        assert_ne!(a.slug(), ControlRecordKey::intent("a:b").unwrap().slug());
        assert_ne!(
            a.native_key(),
            ControlRecordKey::intent("a:b").unwrap().native_key()
        );
        assert!(crate::state::validate_slug(a.slug()).is_ok());
        assert!(
            crate::state::validate_slug(
                ControlRecordKey::allocation(&"x".repeat(128))
                    .unwrap()
                    .slug()
            )
            .is_ok()
        );
        assert_ne!(a.slug(), ControlRecordKey::routine("a-b").unwrap().slug());
        assert_ne!(
            ControlRecordKey::routine("é").unwrap().slug(),
            ControlRecordKey::routine("e").unwrap().slug()
        );
        assert!(ControlRecordKey::routine(&"a".repeat(129)).is_err());
        assert!(ControlRecordKey::routine("a\nb").is_err());
    }
    #[test]
    fn bounded_codec_refuses_oversize_and_corrupt_values() {
        assert_eq!(
            decode_record::<serde_json::Value>(&vec![b' '; MAX_RECORD_BYTES + 1]),
            Err(F1RecordError::TooLarge)
        );
        assert_eq!(
            decode_record::<serde_json::Value>(b"{"),
            Err(F1RecordError::Codec)
        );
        assert_eq!(
            encode_record(&"x".repeat(MAX_RECORD_BYTES)),
            Err(F1RecordError::TooLarge)
        );
    }
    fn evidence(id: &str) -> IntentEvidenceRecord {
        IntentEvidenceRecord {
            version: 1,
            safety: SafetyState::Inert,
            definition: IntentDefinition::Committee(CommitteeIntent {
                intent_id: id.into(),
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
    fn intent_roundtrips_and_validates_individual_row() {
        let x = evidence("intent:1");
        x.validate().unwrap();
        let y: IntentEvidenceRecord = decode_record(&encode_record(&x).unwrap()).unwrap();
        assert_eq!(x, y);
        let bytes = encode_record(&x).unwrap();
        assert_eq!(
            decode_intent_record(&bytes, "another"),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(decode_intent_record(&bytes, "intent:1").unwrap(), x);
        let mut bad = x;
        bad.version = 0;
        assert_eq!(bad.validate(), Err(F1RecordError::InvalidRecord));
    }
    #[test]
    fn allocation_stop_is_a_bounded_record_transition_not_a_host_effect() {
        let row = AllocationEvidenceRecord {
            version: 1,
            allocation_id: "root".into(),
            parent_id: None,
            root_id: "root".into(),
            source_window_id: "window".into(),
            deadline_utc: "2030-01-01T00:00:00Z".into(),
            tokens: 5,
            cost_micros: 7,
            safety: SafetyState::Inert,
        };
        let bytes = encode_record(&row).unwrap();
        assert_eq!(
            decode_allocation_record(&bytes, "wrong"),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(decode_allocation_record(&bytes, "root").unwrap(), row);
        let next = prepare_allocation_stop(&row, 1).unwrap();
        assert_eq!(next.safety, SafetyState::StopRequested);
        assert_eq!(next.version, 2);
        assert_eq!(row.safety, SafetyState::Inert);
        assert_eq!(
            prepare_allocation_stop(&row, 2),
            Err(F1RecordError::Conflict)
        );
        let mut malformed = row.clone();
        malformed.root_id = "other".into();
        assert_eq!(malformed.validate(), Err(F1RecordError::InvalidRecord));
    }

    #[test]
    fn safety_batch_rejects_duplicate_rows_and_unbounded_work() {
        let change = SafetyChange::QuarantineIntent {
            id: "x".into(),
            expected_version: 1,
        };
        assert!(
            SafetyBatch {
                changes: vec![change.clone()]
            }
            .validate()
            .is_ok()
        );
        assert_eq!(
            SafetyBatch {
                changes: vec![change.clone(), change.clone()]
            }
            .validate(),
            Err(F1RecordError::InvalidRecord)
        );
        assert_eq!(
            SafetyBatch { changes: vec![] }.validate(),
            Err(F1RecordError::InvalidRecord)
        );
        assert_eq!(
            SafetyBatch {
                changes: vec![change; MAX_SAFETY_CHANGES + 1]
            }
            .validate(),
            Err(F1RecordError::InvalidRecord)
        );
        assert_eq!(
            SafetyBatch {
                changes: vec![SafetyChange::PauseRoutine {
                    id: "r".into(),
                    expected_version: 1,
                    reason: " ".into()
                }]
            }
            .validate(),
            Err(F1RecordError::InvalidRecord)
        );
    }
    #[test]
    fn quarantining_one_row_cannot_modify_another_and_rejects_stale_version() {
        let current = evidence("id-42");
        let unrelated = evidence("id-41");
        let next = prepare_intent_quarantine(&current, 1).unwrap();
        assert_eq!(next.version, 2);
        assert_eq!(next.safety, SafetyState::Quarantined);
        assert_eq!(current.safety, SafetyState::Inert);
        assert_eq!(unrelated, evidence("id-41"));
        assert_eq!(
            prepare_intent_quarantine(&current, 2),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(
            prepare_intent_quarantine(&next, 2),
            Err(F1RecordError::Conflict)
        );
        let mut overflow = current.clone();
        overflow.version = u64::MAX;
        assert_eq!(
            prepare_intent_quarantine(&overflow, u64::MAX),
            Err(F1RecordError::Conflict)
        );
    }
}
