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
use std::io::{self, Write};

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
    #[error("routine control operations are not yet supported on this local backend")]
    UnsupportedLocal,
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
    struct BoundedWriter {
        bytes: Vec<u8>,
        exceeded: bool,
    }
    impl Write for BoundedWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if data.len() > MAX_RECORD_BYTES - self.bytes.len() {
                self.exceeded = true;
                return Err(io::Error::other("record size limit"));
            }
            self.bytes.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        exceeded: false,
    };
    if serde_json::to_writer(&mut writer, value).is_err() {
        return Err(if writer.exceeded {
            F1RecordError::TooLarge
        } else {
            F1RecordError::Codec
        });
    }
    Ok(writer.bytes)
}
pub fn decode_record<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, F1RecordError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(F1RecordError::TooLarge);
    }
    serde_json::from_slice(bytes).map_err(|_| F1RecordError::Codec)
}

// A typed read must not silently discard unknown nested data on a later safety update.
// Compare JSON values rather than raw bytes so object order and whitespace are immaterial.
fn decode_lossless<T: DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T, F1RecordError> {
    let original: serde_json::Value = decode_record(bytes)?;
    let row: T = decode_record(bytes)?;
    let encoded = encode_record(&row)?;
    let roundtrip: serde_json::Value = decode_record(&encoded)?;
    if original != roundtrip {
        return Err(F1RecordError::Codec);
    }
    Ok(row)
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
        if matches!(self.spec.state, RoutineState::Proposed) {
            if self.version == u64::MAX || self.spec.revision == u64::MAX {
                return Err(F1RecordError::InvalidRecord);
            }
            // Bound the input before cloning it to calculate safety-transition headroom.
            encode_record(self)?;
            // A pause may need a full-length reason and increment both revision counters.
            // Quotes and backslashes have the largest JSON expansion of permitted reason
            // characters (control characters are refused by SafetyBatch).
            let mut paused = self.clone();
            paused.version = u64::MAX;
            paused.spec.revision = u64::MAX;
            paused.spec.state = RoutineState::Paused {
                reason: "\\".repeat(1024),
            };
            encode_record(&paused)?;
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
    /// Only fresh inert evidence may enter the preparation store through insertion.
    pub fn validate_for_insert(&self) -> Result<(), F1RecordError> {
        if self.version != 1 || self.safety != SafetyState::Inert {
            return Err(F1RecordError::InvalidRecord);
        }
        self.validate()
    }

    pub fn validate(&self) -> Result<(), F1RecordError> {
        ControlRecordKey::intent(self.definition.id())?;
        if self.version == 0
            || (self.safety == SafetyState::Inert && self.version == u64::MAX)
            || matches!(&self.definition, IntentDefinition::Committee(c) if c.validate().is_err())
        {
            return Err(F1RecordError::InvalidRecord);
        }
        if self.safety == SafetyState::Inert {
            encode_record(self)?;
            let mut quarantined = self.clone();
            quarantined.version = u64::MAX;
            quarantined.safety = SafetyState::Quarantined;
            encode_record(&quarantined)?;
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
    /// Only fresh inert evidence may enter the preparation store through insertion.
    pub fn validate_for_insert(&self) -> Result<(), F1RecordError> {
        if self.version != 1 || self.safety != SafetyState::Inert {
            return Err(F1RecordError::InvalidRecord);
        }
        self.validate()
    }

    pub fn validate(&self) -> Result<(), F1RecordError> {
        ControlRecordKey::allocation(&self.allocation_id)?;
        if self.version == 0
            || (self.safety == SafetyState::Inert && self.version >= u64::MAX - 1)
            || (self.safety == SafetyState::Quarantined && self.version == u64::MAX)
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
        if self.safety != SafetyState::StopRequested {
            encode_record(self)?;
            let mut stopped = self.clone();
            stopped.version = u64::MAX;
            stopped.safety = SafetyState::StopRequested;
            encode_record(&stopped)?;
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
    let row: RoutineProposalRecord = decode_lossless(bytes)?;
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
    let row: IntentEvidenceRecord = decode_lossless(bytes)?;
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
    let row: AllocationEvidenceRecord = decode_lossless(bytes)?;
    row.validate()?;
    if row.allocation_id != expected_id {
        return Err(F1RecordError::Conflict);
    }
    Ok(row)
}

/// Compute one inert-record transition; a store must still compare and write it atomically.
/// A stop request is evidence only and does not mean that a process was stopped.
pub fn prepare_routine_pause(
    current: &RoutineProposalRecord,
    expected_version: u64,
    reason: &str,
) -> Result<RoutineProposalRecord, F1RecordError> {
    current.validate()?;
    if current.version != expected_version || !matches!(current.spec.state, RoutineState::Proposed)
    {
        return Err(F1RecordError::Conflict);
    }
    if reason.trim().is_empty() || reason.len() > 1024 || reason.chars().any(char::is_control) {
        return Err(F1RecordError::InvalidRecord);
    }
    let mut next = current.clone();
    next.version = current
        .version
        .checked_add(1)
        .ok_or(F1RecordError::Conflict)?;
    next.spec.revision = current
        .spec
        .revision
        .checked_add(1)
        .ok_or(F1RecordError::Conflict)?;
    next.spec.state = RoutineState::Paused {
        reason: reason.to_owned(),
    };
    next.validate()?;
    Ok(next)
}

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
    /// Insert valid, version-1 inert intent evidence if its identity is absent.
    /// Validate the full existing ID in the same transaction even when using a hashed slug;
    /// an occupied key (duplicate or collision) is `Conflict` and never replaces the row.
    fn insert_intent(&self, record: IntentEvidenceRecord) -> Result<(), F1RecordError>;
    /// Insert valid, version-1 inert allocation evidence if its identity is absent.
    /// Validate the full existing ID in the same transaction even when using a hashed slug;
    /// an occupied key (duplicate or collision) is `Conflict` and never replaces the row.
    fn insert_allocation(&self, record: AllocationEvidenceRecord) -> Result<(), F1RecordError>;
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
        fn insert_intent(&self, _: IntentEvidenceRecord) -> Result<(), F1RecordError> {
            Err(F1RecordError::Missing)
        }
        fn insert_allocation(&self, _: AllocationEvidenceRecord) -> Result<(), F1RecordError> {
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

    #[test]
    fn oversized_serialization_stops_before_processing_the_whole_value() {
        use serde::ser::SerializeSeq;
        use std::cell::Cell;
        struct Many<'a>(&'a Cell<usize>);
        impl Serialize for Many<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut seq = serializer.serialize_seq(None)?;
                for _ in 0..1_000_000 {
                    self.0.set(self.0.get() + 1);
                    seq.serialize_element("0123456789")?;
                }
                seq.end()
            }
        }
        let visited = Cell::new(0);
        assert_eq!(encode_record(&Many(&visited)), Err(F1RecordError::TooLarge));
        assert!(
            visited.get() < 10_000,
            "serialized {} elements",
            visited.get()
        );
    }

    #[test]
    fn nested_unknown_fields_are_rejected_but_reordered_json_is_accepted() {
        let intent = evidence("intent:strict");
        let mut value = serde_json::to_value(&intent).unwrap();
        value["definition"]["intent"]["delegator"]["future_field"] = serde_json::json!(true);
        let bytes = serde_json::to_vec_pretty(&value).unwrap();
        assert_eq!(
            decode_intent_record(&bytes, "intent:strict"),
            Err(F1RecordError::Codec)
        );

        let reordered = format!(
            "{{\n \"safety\": {}, \"definition\": {}, \"version\": {}\n}}",
            serde_json::to_string(&intent.safety).unwrap(),
            serde_json::to_string(&intent.definition).unwrap(),
            intent.version,
        );
        assert_eq!(
            decode_intent_record(reordered.as_bytes(), "intent:strict"),
            Ok(intent)
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
            Err(F1RecordError::InvalidRecord)
        );
    }

    #[test]
    fn accepted_intent_reserves_quarantine_headroom_at_record_limit() {
        let mut row = evidence("near-limit");
        let base = serde_json::to_vec(&row).unwrap().len();
        let IntentDefinition::Committee(ref mut committee) = row.definition else {
            unreachable!()
        };
        committee
            .delegator
            .controller_lease_id
            .push_str(&"x".repeat(MAX_RECORD_BYTES - base));
        assert_eq!(serde_json::to_vec(&row).unwrap().len(), MAX_RECORD_BYTES);
        assert_eq!(row.validate(), Err(F1RecordError::TooLarge));
        while row.validate() == Err(F1RecordError::TooLarge) {
            let IntentDefinition::Committee(ref mut committee) = row.definition else {
                unreachable!()
            };
            committee.delegator.controller_lease_id.pop();
        }
        row.validate().unwrap();
        let mut just_too_large = row.clone();
        let IntentDefinition::Committee(ref mut committee) = just_too_large.definition else {
            unreachable!()
        };
        committee.delegator.controller_lease_id.push('x');
        assert_eq!(just_too_large.validate(), Err(F1RecordError::TooLarge));
        let quarantined = prepare_intent_quarantine(&row, row.version).unwrap();
        assert_eq!(quarantined.safety, SafetyState::Quarantined);
    }

    #[test]
    fn active_evidence_rejects_versions_without_room_for_safety_transition() {
        let mut intent = evidence("i");
        intent.version = u64::MAX;
        assert_eq!(intent.validate(), Err(F1RecordError::InvalidRecord));
        let mut allocation = AllocationEvidenceRecord {
            version: u64::MAX - 1,
            allocation_id: "root".into(),
            parent_id: None,
            root_id: "root".into(),
            source_window_id: "window".into(),
            deadline_utc: "2030-01-01T00:00:00Z".into(),
            tokens: 1,
            cost_micros: 1,
            safety: SafetyState::Inert,
        };
        assert_eq!(allocation.validate(), Err(F1RecordError::InvalidRecord));
        allocation.version = u64::MAX - 2;
        allocation.validate().unwrap();
        allocation.safety = SafetyState::Quarantined;
        allocation.version += 1;
        let stopped = prepare_allocation_stop(&allocation, u64::MAX - 1).unwrap();
        assert_eq!(stopped.version, u64::MAX);
    }

    #[test]
    fn routine_proposals_reserve_space_for_long_escaped_pause_reason() {
        use crate::routine::{Allocation, CatchUp, Concurrency, RoutineWork, Schedule, Window};
        let mut row = RoutineProposalRecord {
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
        };
        let mut huge = row.clone();
        huge.spec.project = "x".repeat(4 * 1024 * 1024);
        assert_eq!(huge.validate(), Err(F1RecordError::TooLarge));
        let base = serde_json::to_vec(&row).unwrap().len();
        row.spec
            .project
            .push_str(&"x".repeat(MAX_RECORD_BYTES - base));
        assert_eq!(row.validate(), Err(F1RecordError::TooLarge));
        while row.validate() == Err(F1RecordError::TooLarge) {
            row.spec.project.pop();
        }
        row.validate().unwrap();
        let mut just_too_large = row.clone();
        just_too_large.spec.project.push('x');
        assert_eq!(just_too_large.validate(), Err(F1RecordError::TooLarge));
        let paused = prepare_routine_pause(&row, row.version, &"\\".repeat(1024)).unwrap();
        assert!(encode_record(&paused).is_ok());
        row.version = u64::MAX;
        assert_eq!(row.validate(), Err(F1RecordError::InvalidRecord));
    }

    #[test]
    fn insert_contract_refuses_noninert_or_noninitial_evidence() {
        let mut intent = evidence("intent-insert");
        assert_eq!(intent.validate_for_insert(), Ok(()));
        intent.version = 2;
        assert_eq!(
            intent.validate_for_insert(),
            Err(F1RecordError::InvalidRecord)
        );
        intent.version = 1;
        intent.safety = SafetyState::Quarantined;
        assert_eq!(
            intent.validate_for_insert(),
            Err(F1RecordError::InvalidRecord)
        );

        let mut allocation = AllocationEvidenceRecord {
            version: 1,
            allocation_id: "root".into(),
            parent_id: None,
            root_id: "root".into(),
            source_window_id: "window".into(),
            deadline_utc: "2030-01-01T00:00:00Z".into(),
            tokens: 1,
            cost_micros: 1,
            safety: SafetyState::Inert,
        };
        assert_eq!(allocation.validate_for_insert(), Ok(()));
        allocation.version = 2;
        assert_eq!(
            allocation.validate_for_insert(),
            Err(F1RecordError::InvalidRecord)
        );
        allocation.version = 1;
        allocation.safety = SafetyState::StopRequested;
        assert_eq!(
            allocation.validate_for_insert(),
            Err(F1RecordError::InvalidRecord)
        );
    }

    // Test double for the insert-if-absent contract only. It is not a persistence backend.
    #[derive(Default)]
    struct InsertOnly(std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>);
    impl InsertOnly {
        fn insert(&self, key: ControlRecordKey, bytes: Vec<u8>) -> Result<(), F1RecordError> {
            let mut rows = self.0.lock().unwrap();
            if rows.contains_key(key.slug()) {
                return Err(F1RecordError::Conflict);
            }
            rows.insert(key.slug().to_owned(), bytes);
            Ok(())
        }
    }
    impl F1PreparationStore for InsertOnly {
        fn routine(&self, _: &str) -> Result<Option<RoutineProposalRecord>, F1RecordError> {
            Err(F1RecordError::Missing)
        }
        fn intent(&self, id: &str) -> Result<Option<IntentEvidenceRecord>, F1RecordError> {
            let key = ControlRecordKey::intent(id)?;
            self.0
                .lock()
                .unwrap()
                .get(key.slug())
                .map(|bytes| decode_intent_record(bytes, id))
                .transpose()
        }
        fn allocation(&self, id: &str) -> Result<Option<AllocationEvidenceRecord>, F1RecordError> {
            let key = ControlRecordKey::allocation(id)?;
            self.0
                .lock()
                .unwrap()
                .get(key.slug())
                .map(|bytes| decode_allocation_record(bytes, id))
                .transpose()
        }
        fn apply_safety_batch(&self, _: &SafetyBatch) -> Result<(), F1RecordError> {
            Err(F1RecordError::Missing)
        }
        fn insert_proposal(&self, _: RoutineProposalRecord) -> Result<(), F1RecordError> {
            Err(F1RecordError::Missing)
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
    fn insert_evidence_is_unique_and_detects_misplaced_row() {
        let store = InsertOnly::default();
        let intent = evidence("intent-original");
        store.insert_intent(intent.clone()).unwrap();
        assert_eq!(
            store.insert_intent(intent.clone()),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(store.intent("intent-original"), Ok(Some(intent.clone())));
        let other_key = ControlRecordKey::intent("intent-collision").unwrap();
        store
            .0
            .lock()
            .unwrap()
            .insert(other_key.slug().into(), encode_record(&intent).unwrap());
        assert_eq!(
            store.intent("intent-collision"),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(
            store.insert_intent(evidence("intent-collision")),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(store.intent("intent-original"), Ok(Some(intent)));

        let allocation = AllocationEvidenceRecord {
            version: 1,
            allocation_id: "root".into(),
            parent_id: None,
            root_id: "root".into(),
            source_window_id: "window".into(),
            deadline_utc: "2030-01-01T00:00:00Z".into(),
            tokens: 1,
            cost_micros: 1,
            safety: SafetyState::Inert,
        };
        store.insert_allocation(allocation.clone()).unwrap();
        assert_eq!(
            store.insert_allocation(allocation.clone()),
            Err(F1RecordError::Conflict)
        );
        assert_eq!(store.allocation("root"), Ok(Some(allocation.clone())));
        let key = ControlRecordKey::allocation("other").unwrap();
        store
            .0
            .lock()
            .unwrap()
            .insert(key.slug().into(), encode_record(&allocation).unwrap());
        assert_eq!(store.allocation("other"), Err(F1RecordError::Conflict));
    }
}
