//! Pure local peer-delivery contracts. Store state is not an output permit.
//!
//! Trusted stores construct transaction contexts and independently verify local
//! lifetime registrations. These serializable descriptors cannot grant authority.

use crate::ferrowire::{DeliveryReceipt, WireMessage};
use crate::{AgentId, AgentRecord, At, MessageId, StoreError};
use serde::{Deserialize, Serialize};
use std::fmt;

pub mod c14n;
pub use c14n::{canonical_generation, canonical_payload};

pub const DELIVERY_SCHEMA_VERSION: u16 = 1;
pub const CANONICALIZER_VERSION: &str = "wire-delivery-c14n-v1";
pub const PEER_RENDERER_VERSION: &str = "wire-peer-envelope-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstanceId(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkerId(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionId(pub [u8; 32]);

/// Secret phase identity. Serialization is for the protected ledger/protocol only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PhaseToken(pub [u8; 32]);

impl fmt::Debug for PhaseToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PhaseToken([REDACTED])")
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Revision(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Push,
    Hook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Queue,
    Steer,
    Stdout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Eligibility {
    Eligible,
    Prepared,
    MutationArmed,
    Settled,
    Held,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    Disabled,
    UnknownIdentity,
    AliasConflict,
    GenerationConflict,
    WrongHome,
    BudgetExhausted,
    Capacity,
    Expired,
    UnsupportedVersion,
    UnsupportedBackend,
    LegacyUncertain,
    ContinuityLost,
    Corrupt,
    Unconfirmed,
    EvidenceOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckProvenance {
    ManualReadAck,
    LegacyAckUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityState {
    Held,
    Released,
    Retired,
}

/// A transport observation, never proof of model consumption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DeliveryEvidence {
    PreMutationRefused,
    DefinitiveNoMutation,
    MutationUnconfirmed,
    VendorQueued,
    AppServerAccepted { thread: String, turn: String },
    HookEmissionCompleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum IdentityResolution {
    Unresolved,
    Unique {
        recipient_id: AgentId,
        binding_revision: Revision,
    },
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryKey {
    pub original_id: MessageId,
    pub recipient_id: AgentId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub recipient_id: AgentId,
    pub binding_revision: Revision,
    pub authority_id: String,
    pub local_epoch: u64,
    pub policy_hash: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutePin {
    pub thread_id: String,
    pub generation: u64,
    pub fingerprint: Digest,
    pub executable: Option<String>,
    pub executable_digest: Option<Digest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifetimeRegistration {
    pub id: Digest,
    pub revision: Revision,
    pub home: Digest,
    pub boot: Digest,
    pub instance: InstanceId,
    pub worker: WorkerId,
    pub connection: ConnectionId,
    pub emitter_scope: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptBinding {
    pub token: PhaseToken,
    pub daemon: InstanceId,
    pub worker: WorkerId,
    pub connection: ConnectionId,
    pub lane: Lane,
    pub lifetime: LifetimeRegistration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenPayload {
    pub renderer: String,
    pub transport: Transport,
    pub expected_turn: Option<String>,
    pub bytes: Vec<u8>,
    pub digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcknowledgmentState {
    pub manual_at: Option<At>,
    pub legacy_unknown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptSummary {
    pub binding: AttemptBinding,
    pub route: RoutePin,
    pub frozen: Option<FrozenPayload>,
    pub prepared_at: At,
    pub armed_at: Option<At>,
    pub evidence: Option<DeliveryEvidence>,
    pub detail: String,
    pub capacity: CapacityState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryRecord {
    pub schema: u16,
    pub key: DeliveryKey,
    pub generation: Digest,
    pub binding: Binding,
    pub revision: Revision,
    pub eligibility: Eligibility,
    pub prepares: u8,
    pub attempts: Vec<AttemptSummary>,
    pub acknowledgment: AcknowledgmentState,
    pub held: Option<HoldReason>,
    pub annotation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveOwner {
    pub key: DeliveryKey,
    pub attempt: AttemptBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityRetirementTarget {
    pub owner: ActiveOwner,
    pub record_revision: Revision,
    pub recipient_revision: Revision,
    pub global_revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipientControl {
    pub binding: Binding,
    pub revision: Revision,
    pub active: Option<ActiveOwner>,
    pub barrier: Option<DeliveryKey>,
    pub identity_conflict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalControl {
    pub revision: Revision,
    pub active_attempts: u8,
    pub active_index: Vec<ActiveOwner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Continuity {
    pub schema: u16,
    pub cutover: At,
    pub policy_hash: Digest,
    pub local_epoch: u64,
    pub enabled: bool,
    pub intact: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdReservation {
    pub original_id: MessageId,
    pub generation: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityIndexRow {
    pub key: String,
    pub resolution: IdentityResolution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityContext {
    pub registry: Option<AgentRecord>,
    pub selector: IdentityResolution,
    pub receipt_alias: IdentityResolution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliverySnapshot {
    pub records: Vec<DeliveryRecord>,
    pub ids: Vec<IdReservation>,
    pub recipients: Vec<RecipientControl>,
    pub identities: Vec<IdentityIndexRow>,
    pub global: GlobalControl,
    pub continuity: Continuity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceView {
    pub lane: Lane,
    pub route: RoutePin,
    pub prepared_at: At,
    pub armed_at: Option<At>,
    pub evidence: Option<DeliveryEvidence>,
    pub detail: String,
    pub payload_digest: Option<Digest>,
    pub capacity: CapacityState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryView {
    pub key: DeliveryKey,
    pub generation: Digest,
    pub binding: Binding,
    pub revision: Revision,
    pub eligibility: Eligibility,
    pub prepares: u8,
    pub attempts: Vec<EvidenceView>,
    pub acknowledgment: AcknowledgmentState,
    pub held: Option<HoldReason>,
    pub annotation: String,
    pub barrier: Option<DeliveryKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareInput {
    pub key: DeliveryKey,
    pub generation: Digest,
    pub binding: Binding,
    pub attempt: AttemptBinding,
    pub route: RoutePin,
    pub now: At,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmInput {
    pub key: DeliveryKey,
    pub revision: Revision,
    pub attempt: AttemptBinding,
    pub route: RoutePin,
    pub frozen: FrozenPayload,
    pub now: At,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseInput {
    pub key: DeliveryKey,
    pub revision: Revision,
    pub attempt: AttemptBinding,
    pub proof: DeliveryEvidence,
    pub now: At,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettleInput {
    pub key: DeliveryKey,
    pub revision: Revision,
    pub attempt: AttemptBinding,
    pub evidence: DeliveryEvidence,
    pub detail: String,
    pub now: At,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualAckInput {
    pub key: DeliveryKey,
    pub at: At,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentInput {
    pub bindings: Vec<Binding>,
    pub continuity: Continuity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionContext {
    pub key: DeliveryKey,
    pub record: Option<DeliveryRecord>,
    pub reservation: Option<IdReservation>,
    pub recipient: RecipientControl,
    pub global: GlobalControl,
    pub continuity: Continuity,
    pub message: Option<WireMessage>,
    pub receipt: Option<DeliveryReceipt>,
    pub identity: IdentityContext,
    pub lifetime: Option<LifetimeRegistration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryDelta {
    pub record: Option<DeliveryRecord>,
    pub reservation: Option<IdReservation>,
    pub recipient: Option<RecipientControl>,
    pub global: Option<GlobalControl>,
    pub receipt: Option<DeliveryReceipt>,
    pub result: DeliveryResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityRetirementDelta {
    pub record: Option<DeliveryRecord>,
    pub recipient: Option<RecipientControl>,
    pub global: Option<GlobalControl>,
    pub result: CapacityRetirementResult,
}

/// Durable state results do not contain an emission permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DeliveryResult {
    Prepared { record: DeliveryRecord },
    Armed { record: DeliveryRecord },
    Applied { view: DeliveryView },
    Refused { reason: HoldReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CapacityRetirementResult {
    Retired { view: DeliveryView },
    AlreadyReleased { view: DeliveryView },
    Unproven,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "input",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DeliveryCommand {
    Prepare(PrepareInput),
    Arm(ArmInput),
    Release(ReleaseInput),
    Settle(SettleInput),
    ManualAck(ManualAckInput),
    InvalidatePrepared {
        key: DeliveryKey,
        revision: Revision,
        now: At,
    },
    Annotate {
        key: DeliveryKey,
        text: String,
    },
    Close {
        key: DeliveryKey,
    },
}

/// Atomic bounded point operations. Implementations independently validate
/// admission and own the retirement verifier; caller descriptors are not proof.
/// No method may materialize retained delivery history on its hot path.
pub trait WireDeliveryStore {
    fn apply_delivery(&self, command: DeliveryCommand) -> Result<DeliveryResult, StoreError>;
    fn inspect_delivery(&self, key: &DeliveryKey) -> Result<Option<DeliveryView>, StoreError>;
    fn active_delivery_owners(&self) -> Result<Vec<CapacityRetirementTarget>, StoreError>;
    fn retire_orphan_capacity(
        &self,
        target: CapacityRetirementTarget,
    ) -> Result<CapacityRetirementResult, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    fn strict<T: DeserializeOwned>(value: Value) {
        let mut with_extra = value.clone();
        with_extra
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), json!(true));
        assert!(serde_json::from_value::<T>(value).is_ok());
        assert!(
            serde_json::from_value::<T>(with_extra).is_err(),
            "{} ignored unknown fields",
            std::any::type_name::<T>()
        );
    }

    #[test]
    fn managed_schema_rejects_unknown_fields_and_variants() {
        strict::<AcknowledgmentState>(json!({"manual_at":null,"legacy_unknown":false}));
        strict::<DeliveryEvidence>(
            json!({"kind":"app_server_accepted","data":{"thread":"thread","turn":"turn"}}),
        );
        strict::<IdentityResolution>(json!({"kind":"unresolved"}));
        strict::<DeliveryEvidence>(json!({"kind":"vendor_queued"}));
        strict::<CapacityRetirementResult>(json!({"kind":"unproven"}));
        let nested = json!({"kind":"app_server_accepted","data":{"thread":"thread","turn":"turn","unexpected":true}});
        assert!(serde_json::from_value::<DeliveryEvidence>(nested).is_err());
        strict::<DeliveryCommand>(json!({"kind":"close","input":{"key":{
            "original_id":"urn:uuid:00000000-0000-7000-8000-000000000001",
            "recipient_id":"urn:uuid:00000000-0000-7000-8000-000000000002"}}}));
        assert!(serde_json::from_value::<HoldReason>(json!("replay_allowed")).is_err());
        assert!(serde_json::from_value::<Digest>(json!([1, 2, 3])).is_err());
        assert_eq!(DELIVERY_SCHEMA_VERSION, 1);
    }

    #[test]
    fn managed_schema_token_debug_is_redacted_and_views_exclude_authority() {
        let token = PhaseToken([171; 32]);
        assert_eq!(format!("{token:?}"), "PhaseToken([REDACTED])");
        let zeros = [0; 32];
        let json = json!({
            "key":{"original_id":"urn:uuid:00000000-0000-7000-8000-000000000001",
                   "recipient_id":"urn:uuid:00000000-0000-7000-8000-000000000002"},
            "generation":zeros,
            "binding":{"recipient_id":"urn:uuid:00000000-0000-7000-8000-000000000002",
                       "binding_revision":1,"authority_id":"local","local_epoch":1,"policy_hash":zeros},
            "revision":1,"eligibility":"mutation_armed","prepares":1,
            "attempts":[{"lane":"push","route":{"thread_id":"thread","generation":1,
                "fingerprint":zeros,"executable":null,"executable_digest":null},
                "prepared_at":"2026-11-06T00:00:00.000Z","armed_at":"2026-11-06T00:00:00.001Z",
                "evidence":null,"detail":"","payload_digest":zeros,"capacity":"held"}],
            "acknowledgment":{"manual_at":null,"legacy_unknown":false},"held":null,"annotation":"","barrier":null
        });
        let view: DeliveryView = serde_json::from_value(json.clone()).unwrap();
        let serialized = serde_json::to_value(view).unwrap();
        assert_eq!(serialized, json);
        let attempt = serialized["attempts"][0].as_object().unwrap();
        for secret in [
            "token",
            "frozen",
            "bytes",
            "connection",
            "worker",
            "daemon",
            "lifetime",
        ] {
            assert!(!attempt.contains_key(secret));
        }
    }
}
