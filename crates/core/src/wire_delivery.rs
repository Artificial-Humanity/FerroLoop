//! Pure local peer-delivery contracts. Store state is not an output permit.
//!
//! Trusted stores construct transaction contexts and independently verify local
//! lifetime registrations. These serializable descriptors cannot grant authority.

use crate::ferrowire::{DeliveryReceipt, WireMessage};
use crate::{AgentId, AgentRecord, At, MessageId, StoreError};
use serde::{Deserialize, Serialize};
use std::fmt;

#[cfg(any(test, feature = "conformance"))]
#[doc(hidden)]
pub mod conformance;

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

impl DeliveryCommand {
    pub fn key(&self) -> &DeliveryKey {
        match self {
            Self::Prepare(x) => &x.key,
            Self::Arm(x) => &x.key,
            Self::Release(x) => &x.key,
            Self::Settle(x) => &x.key,
            Self::ManualAck(x) => &x.key,
            Self::InvalidatePrepared { key, .. }
            | Self::Annotate { key, .. }
            | Self::Close { key } => key,
        }
    }
}

impl DeliveryRecord {
    /// Redacted inspection; this view cannot be used to reconstruct a grant.
    pub fn view(&self, recipient: &RecipientControl) -> DeliveryView {
        DeliveryView {
            key: self.key.clone(),
            generation: self.generation,
            binding: self.binding.clone(),
            revision: self.revision,
            eligibility: self.eligibility,
            prepares: self.prepares,
            attempts: self
                .attempts
                .iter()
                .map(|a| EvidenceView {
                    lane: a.binding.lane,
                    route: a.route.clone(),
                    prepared_at: a.prepared_at.clone(),
                    armed_at: a.armed_at.clone(),
                    evidence: a.evidence.clone(),
                    detail: a.detail.clone(),
                    payload_digest: a.frozen.as_ref().map(|f| f.digest),
                    capacity: a.capacity,
                })
                .collect(),
            acknowledgment: self.acknowledgment.clone(),
            held: self.held,
            annotation: self.annotation.clone(),
            barrier: recipient.barrier.clone(),
        }
    }
}

fn refused(reason: HoldReason) -> DeliveryDelta {
    DeliveryDelta {
        record: None,
        reservation: None,
        recipient: None,
        global: None,
        receipt: None,
        result: DeliveryResult::Refused { reason },
    }
}

fn bump(revision: &mut Revision) -> Result<(), StoreError> {
    revision.0 = revision
        .0
        .checked_add(1)
        .ok_or_else(|| StoreError::Backend("delivery revision exhausted".into()))?;
    Ok(())
}

pub(crate) fn valid_global(global: &GlobalControl) -> bool {
    global.active_index.len() <= 16
        && usize::from(global.active_attempts) == global.active_index.len()
        && global.active_index.iter().enumerate().all(|(i, owner)| {
            global.active_index[..i]
                .iter()
                .all(|previous| previous.key.recipient_id != owner.key.recipient_id)
        })
}

fn valid_record(record: &DeliveryRecord, key: &DeliveryKey) -> bool {
    record.schema == DELIVERY_SCHEMA_VERSION
        && &record.key == key
        && record.binding.recipient_id == key.recipient_id
        && record.attempts.len() <= 3
        && usize::from(record.prepares) == record.attempts.len()
        && record
            .attempts
            .iter()
            .filter(|a| a.frozen.is_some())
            .count()
            <= 1
        && record
            .attempts
            .iter()
            .filter(|a| a.capacity == CapacityState::Held)
            .count()
            <= 1
}

fn exact_lifetime(attempt: &AttemptBinding, admitted: Option<&LifetimeRegistration>) -> bool {
    admitted == Some(&attempt.lifetime)
        && attempt.daemon == attempt.lifetime.instance
        && attempt.worker == attempt.lifetime.worker
        && attempt.connection == attempt.lifetime.connection
}

fn identity_reason(ctx: &TransitionContext) -> Option<HoldReason> {
    if ctx.recipient.identity_conflict {
        return Some(HoldReason::AliasConflict);
    }
    let expected = IdentityResolution::Unique {
        recipient_id: ctx.key.recipient_id.clone(),
        binding_revision: ctx.recipient.binding.binding_revision,
    };
    if ctx.identity.selector == IdentityResolution::Conflict
        || ctx.identity.receipt_alias == IdentityResolution::Conflict
    {
        return Some(HoldReason::AliasConflict);
    }
    if ctx.identity.selector != expected
        || ctx.identity.receipt_alias != expected
        || ctx.identity.registry.as_ref().is_none_or(|a| {
            a.id != ctx.key.recipient_id || a.status != crate::agent::AgentStatus::Active
        })
    {
        return Some(HoldReason::UnknownIdentity);
    }
    None
}

fn immutable_reason(
    ctx: &TransitionContext,
    generation: Digest,
    now: &At,
) -> Result<Option<HoldReason>, StoreError> {
    if !ctx.continuity.intact {
        return Ok(Some(HoldReason::ContinuityLost));
    }
    if ctx.continuity.schema != DELIVERY_SCHEMA_VERSION {
        return Ok(Some(HoldReason::UnsupportedVersion));
    }
    if !ctx.continuity.enabled {
        return Ok(Some(HoldReason::Disabled));
    }
    let binding = &ctx.recipient.binding;
    if binding.authority_id != "ai-lab-0"
        || binding.local_epoch != ctx.continuity.local_epoch
        || binding.policy_hash != ctx.continuity.policy_hash
    {
        return Ok(Some(HoldReason::WrongHome));
    }
    if let Some(reason) = identity_reason(ctx) {
        return Ok(Some(reason));
    }
    let Some(message) = &ctx.message else {
        return Ok(Some(HoldReason::Corrupt));
    };
    let Some(receipt) = &ctx.receipt else {
        return Ok(Some(HoldReason::Corrupt));
    };
    if message.id != ctx.key.original_id
        || receipt.message_id != ctx.key.original_id
        || receipt.recipient_id != *ctx.key.recipient_id.iri()
    {
        return Ok(Some(HoldReason::Corrupt));
    }
    use crate::ferrowire::WireAddress;
    let selector_bounded = match &message.to {
        WireAddress::AgentName(name) | WireAddress::Area(name) | WireAddress::Group(name) => {
            name.len() <= 4096
        }
        WireAddress::AgentId(id) => id.as_str().len() <= 4096,
        WireAddress::Role(role) => role.as_wire().len() <= 4096,
        WireAddress::AreaRole { area, role } => area.len() <= 4096 && role.as_wire().len() <= 4096,
        WireAddress::All | WireAddress::Council => true,
    };
    if !selector_bounded
        || [
            Some(message.id.as_str()),
            Some(message.thread_id.as_str()),
            message.reply_to.as_ref().map(|id| id.as_str()),
            message.correlation_id.as_ref().map(|id| id.as_str()),
            message.sender_id.as_ref().map(|id| id.as_str()),
            Some(ctx.key.recipient_id.as_str()),
            Some(receipt.recipient_name.as_str()),
        ]
        .into_iter()
        .flatten()
        .any(|s| s.len() > 4096)
    {
        return Ok(Some(HoldReason::EvidenceOverflow));
    }
    if canonical_generation(message)? != generation
        || ctx
            .reservation
            .as_ref()
            .is_some_and(|r| r.original_id != ctx.key.original_id || r.generation != generation)
    {
        return Ok(Some(HoldReason::GenerationConflict));
    }
    if message.created_at <= ctx.continuity.cutover {
        return Ok(Some(HoldReason::LegacyUncertain));
    }
    if message
        .deadline_at
        .as_ref()
        .is_some_and(|deadline| deadline <= now)
        || receipt.status == crate::ferrowire::DeliveryStatus::Expired
    {
        return Ok(Some(HoldReason::Expired));
    }
    if message.subject.len() > 512
        || message.body.len() > 262_144
        || message.sender_name.len() > 4096
        || message.sender_host.as_ref().is_some_and(|s| s.len() > 4096)
    {
        return Ok(Some(HoldReason::EvidenceOverflow));
    }
    if receipt.status != crate::ferrowire::DeliveryStatus::Pending {
        return Ok(Some(HoldReason::LegacyUncertain));
    }
    Ok(None)
}

fn summary_reservation(attempt: &AttemptSummary) -> AttemptSummary {
    let mut compact = attempt.clone();
    if let Some(frozen) = &mut compact.frozen {
        frozen.bytes.clear();
    }
    // Reserve the largest bounded excerpt before Arm so settlement never has
    // to discard a transport observation to fit a summary.
    compact.detail = "\0".repeat(300);
    compact.evidence = Some(match &compact.frozen {
        Some(frozen) if frozen.transport == Transport::Steer => {
            DeliveryEvidence::AppServerAccepted {
                thread: compact.route.thread_id.clone(),
                turn: frozen.expected_turn.clone().unwrap_or_default(),
            }
        }
        _ => DeliveryEvidence::MutationUnconfirmed,
    });
    compact
}

fn bounded_summary(attempt: &AttemptSummary) -> Result<bool, StoreError> {
    Ok(serde_json::to_vec(&summary_reservation(attempt))
        .map_err(|e| StoreError::Backend(e.to_string()))?
        .len()
        <= 8192
        && attempt.route.thread_id.len() <= 4096
        && attempt
            .route
            .executable
            .as_ref()
            .is_none_or(|s| s.len() <= 4096))
}

// Leave fixed room for the full annotation and safety flags. Frozen bytes are
// separately bounded; their metadata is part of the compact row limit.
fn bounded_record(record: &DeliveryRecord) -> Result<bool, StoreError> {
    let mut compact = record.clone();
    compact.annotation.clear();
    for attempt in &mut compact.attempts {
        *attempt = summary_reservation(attempt);
    }
    Ok(serde_json::to_vec(&compact)
        .map_err(|e| StoreError::Backend(e.to_string()))?
        .len()
        <= 32_768 - 4096 - 1024)
}

fn release_capacity(
    record: &mut DeliveryRecord,
    index: usize,
    recipient: &mut RecipientControl,
    global: &mut GlobalControl,
    state: CapacityState,
) -> Result<bool, StoreError> {
    if record.attempts[index].capacity != CapacityState::Held {
        return Ok(false);
    }
    let owner = ActiveOwner {
        key: record.key.clone(),
        attempt: record.attempts[index].binding.clone(),
    };
    if recipient.active.as_ref() != Some(&owner)
        || !global.active_index.contains(&owner)
        || !valid_global(global)
    {
        return Err(StoreError::Backend(
            "delivery capacity ownership is corrupt".into(),
        ));
    }
    recipient.active = None;
    global.active_index.retain(|item| item != &owner);
    global.active_attempts -= 1;
    record.attempts[index].capacity = state;
    bump(&mut recipient.revision)?;
    bump(&mut global.revision)?;
    Ok(true)
}

fn applied(
    record: DeliveryRecord,
    recipient: RecipientControl,
    global: GlobalControl,
    capacity_changed: bool,
    recipient_changed: bool,
    receipt: Option<DeliveryReceipt>,
) -> DeliveryDelta {
    let result = DeliveryResult::Applied {
        view: record.view(&recipient),
    };
    DeliveryDelta {
        record: Some(record),
        reservation: None,
        recipient: recipient_changed.then_some(recipient),
        global: capacity_changed.then_some(global),
        receipt,
        result,
    }
}

/// Pure predicates and bounded deltas. A caller-created context is not authority;
/// stores must independently obtain every point predicate inside their transaction.
pub fn transition(
    ctx: TransitionContext,
    command: DeliveryCommand,
) -> Result<DeliveryDelta, StoreError> {
    if !valid_global(&ctx.global) {
        return Ok(refused(HoldReason::Corrupt));
    }
    let recipient_owners: Vec<_> = ctx
        .global
        .active_index
        .iter()
        .filter(|o| o.key.recipient_id == ctx.key.recipient_id)
        .collect();
    let capacity_consistent = match &ctx.recipient.active {
        Some(owner) => recipient_owners.as_slice() == [owner],
        None => recipient_owners.is_empty(),
    };
    if command.key() != &ctx.key
        || ctx.recipient.binding.recipient_id != ctx.key.recipient_id
        || !capacity_consistent
        || ctx
            .record
            .as_ref()
            .is_some_and(|r| !valid_record(r, &ctx.key))
        || ctx
            .message
            .as_ref()
            .is_some_and(|m| m.id != ctx.key.original_id)
        || ctx.receipt.as_ref().is_some_and(|r| {
            r.message_id != ctx.key.original_id || r.recipient_id != *ctx.key.recipient_id.iri()
        })
    {
        return Ok(refused(HoldReason::Corrupt));
    }
    if let DeliveryCommand::Prepare(input) = command {
        if ctx.record.is_some() && ctx.reservation.is_none() {
            return Ok(refused(HoldReason::Corrupt));
        }
        if ctx.record.as_ref().is_some_and(|r| r.prepares >= 3) {
            let mut delta = refused(HoldReason::BudgetExhausted);
            if let Some(mut record) = ctx.record
                && record.eligibility == Eligibility::Eligible
            {
                record.eligibility = Eligibility::Held;
                record.held = Some(HoldReason::BudgetExhausted);
                bump(&mut record.revision)?;
                delta.record = Some(record);
            }
            return Ok(delta);
        }
        if ctx.record.as_ref().is_some_and(|r| {
            r.eligibility != Eligibility::Eligible
                || r.acknowledgment.manual_at.is_some()
                || r.acknowledgment.legacy_unknown
        }) {
            return Ok(refused(HoldReason::Unconfirmed));
        }
        if ctx.record.as_ref().is_some_and(|r| {
            r.attempts.iter().any(|a| {
                a.armed_at.is_some()
                    && (a.evidence != Some(DeliveryEvidence::DefinitiveNoMutation)
                        || a.capacity != CapacityState::Released)
            })
        }) {
            return Ok(refused(HoldReason::Corrupt));
        }
        if let Some(reason) = immutable_reason(&ctx, input.generation, &input.now)? {
            return Ok(refused(reason));
        }
        if input.binding != ctx.recipient.binding
            || ctx
                .record
                .as_ref()
                .is_some_and(|r| r.generation != input.generation || r.binding != input.binding)
        {
            return Ok(refused(HoldReason::GenerationConflict));
        }
        if !exact_lifetime(&input.attempt, ctx.lifetime.as_ref()) {
            return Ok(refused(HoldReason::UnsupportedBackend));
        }
        if ctx.recipient.barrier.is_some() {
            return Ok(refused(HoldReason::Unconfirmed));
        }
        if ctx.global.active_attempts >= 16 || ctx.recipient.active.is_some() {
            return Ok(refused(HoldReason::Capacity));
        }
        let summary = AttemptSummary {
            binding: input.attempt.clone(),
            route: input.route,
            frozen: None,
            prepared_at: input.now,
            armed_at: None,
            evidence: None,
            detail: String::new(),
            capacity: CapacityState::Held,
        };
        if summary.route.thread_id.is_empty()
            || input.binding.authority_id.len() > 4096
            || !bounded_summary(&summary)?
        {
            return Ok(refused(HoldReason::EvidenceOverflow));
        }
        let mut record = ctx.record.unwrap_or_else(|| DeliveryRecord {
            schema: DELIVERY_SCHEMA_VERSION,
            key: ctx.key.clone(),
            generation: input.generation,
            binding: input.binding,
            revision: Revision(0),
            eligibility: Eligibility::Eligible,
            prepares: 0,
            attempts: Vec::new(),
            acknowledgment: AcknowledgmentState::default(),
            held: None,
            annotation: String::new(),
        });
        if record
            .attempts
            .iter()
            .any(|a| a.binding.token == input.attempt.token)
        {
            return Ok(refused(HoldReason::Corrupt));
        }
        for prior in &mut record.attempts {
            prior.frozen = None;
        }
        record.attempts.push(summary);
        record.prepares += 1;
        record.eligibility = Eligibility::Prepared;
        record.held = None;
        bump(&mut record.revision)?;
        if !bounded_record(&record)? {
            return Ok(refused(HoldReason::EvidenceOverflow));
        }
        let owner = ActiveOwner {
            key: ctx.key.clone(),
            attempt: input.attempt,
        };
        let mut recipient = ctx.recipient;
        recipient.active = Some(owner.clone());
        bump(&mut recipient.revision)?;
        let mut global = ctx.global;
        global.active_attempts += 1;
        global.active_index.push(owner);
        bump(&mut global.revision)?;
        return Ok(DeliveryDelta {
            reservation: Some(IdReservation {
                original_id: ctx.key.original_id,
                generation: input.generation,
            }),
            recipient: Some(recipient),
            global: Some(global),
            receipt: None,
            result: DeliveryResult::Prepared {
                record: record.clone(),
            },
            record: Some(record),
        });
    }
    let Some(mut record) = ctx.record.clone() else {
        return Ok(refused(HoldReason::Corrupt));
    };
    if ctx.reservation.as_ref().is_none_or(|r| {
        r.original_id != record.key.original_id || r.generation != record.generation
    }) {
        return Ok(refused(HoldReason::Corrupt));
    }
    let mut recipient = ctx.recipient.clone();
    let mut global = ctx.global.clone();
    let mut capacity_changed = false;
    let mut recipient_changed = false;
    let mut receipt = None;
    match command {
        DeliveryCommand::Prepare(_) => unreachable!(),
        DeliveryCommand::Arm(input) => {
            if record.revision != input.revision || record.eligibility != Eligibility::Prepared {
                return Ok(refused(HoldReason::Corrupt));
            }
            let Some(index) = record.attempts.len().checked_sub(1) else {
                return Ok(refused(HoldReason::Corrupt));
            };
            let attempt = &record.attempts[index];
            if attempt.armed_at.is_some()
                || attempt.frozen.is_some()
                || attempt.evidence.is_some()
                || attempt.capacity != CapacityState::Held
            {
                return Ok(refused(HoldReason::Corrupt));
            }
            if attempt.binding != input.attempt
                || attempt.route != input.route
                || !exact_lifetime(&input.attempt, ctx.lifetime.as_ref())
            {
                return Ok(refused(HoldReason::Corrupt));
            }
            if input.now < attempt.prepared_at
                || input
                    .now
                    .unix_millis()
                    .saturating_sub(attempt.prepared_at.unix_millis())
                    >= 20_000
            {
                return Ok(refused(HoldReason::Expired));
            }
            if let Some(reason) = immutable_reason(&ctx, record.generation, &input.now)? {
                return Ok(refused(reason));
            }
            if record.binding != recipient.binding || recipient.barrier.is_some() {
                return Ok(refused(HoldReason::GenerationConflict));
            }
            let owner = ActiveOwner {
                key: record.key.clone(),
                attempt: input.attempt.clone(),
            };
            if recipient.active.as_ref() != Some(&owner) || !global.active_index.contains(&owner) {
                return Ok(refused(HoldReason::Corrupt));
            }
            let frozen = &input.frozen;
            if frozen.renderer != PEER_RENDERER_VERSION {
                return Ok(refused(HoldReason::UnsupportedVersion));
            }
            if frozen.bytes.len() > 1_048_576 + 14
                || frozen
                    .expected_turn
                    .as_ref()
                    .is_some_and(|s| s.len() > 4096)
                || (input.attempt.lane == Lane::Hook) != (frozen.transport == Transport::Stdout)
                || (frozen.transport == Transport::Steer
                    && frozen.expected_turn.as_ref().is_none_or(|s| s.is_empty()))
            {
                return Ok(refused(HoldReason::EvidenceOverflow));
            }
            if canonical_payload(record.generation, &frozen.renderer, &frozen.bytes)?
                != frozen.digest
            {
                return Ok(refused(HoldReason::GenerationConflict));
            }
            record.attempts[index].frozen = Some(input.frozen);
            record.attempts[index].armed_at = Some(input.now);
            record.eligibility = Eligibility::MutationArmed;
            if !bounded_summary(&record.attempts[index])? || !bounded_record(&record)? {
                return Ok(refused(HoldReason::EvidenceOverflow));
            }
            recipient.barrier = Some(record.key.clone());
            bump(&mut recipient.revision)?;
            bump(&mut record.revision)?;
            return Ok(DeliveryDelta {
                record: Some(record.clone()),
                reservation: None,
                recipient: Some(recipient),
                global: None,
                receipt: None,
                result: DeliveryResult::Armed { record },
            });
        }
        DeliveryCommand::Release(input) => {
            if record.revision != input.revision {
                return Ok(refused(HoldReason::Corrupt));
            }
            let Some(index) = record
                .attempts
                .iter()
                .position(|a| a.binding == input.attempt)
            else {
                return Ok(refused(HoldReason::Corrupt));
            };
            if index + 1 != record.attempts.len()
                || !matches!(
                    record.eligibility,
                    Eligibility::Prepared | Eligibility::MutationArmed
                )
                || !matches!(
                    input.proof,
                    DeliveryEvidence::PreMutationRefused | DeliveryEvidence::DefinitiveNoMutation
                )
                || record.attempts[index].capacity == CapacityState::Retired
                || (record.attempts[index].armed_at.is_some()
                    && input.proof != DeliveryEvidence::DefinitiveNoMutation)
            {
                return Ok(refused(HoldReason::Unconfirmed));
            }
            record.attempts[index].evidence = Some(input.proof);
            record.attempts[index].frozen = None;
            capacity_changed = release_capacity(
                &mut record,
                index,
                &mut recipient,
                &mut global,
                CapacityState::Released,
            )?;
            recipient_changed = capacity_changed;
            if recipient.barrier.as_ref() == Some(&record.key) {
                recipient.barrier = None;
                if !recipient_changed {
                    bump(&mut recipient.revision)?;
                }
                recipient_changed = true;
            }
            record.eligibility = if record.prepares < 3 {
                Eligibility::Eligible
            } else {
                Eligibility::Held
            };
            record.held = (record.prepares >= 3).then_some(HoldReason::BudgetExhausted);
        }
        DeliveryCommand::Settle(input) => {
            let Some(index) = record
                .attempts
                .iter()
                .position(|a| a.binding == input.attempt)
            else {
                return Ok(refused(HoldReason::Corrupt));
            };
            let attempt = &record.attempts[index];
            if attempt.armed_at.is_none()
                || matches!(input.evidence, DeliveryEvidence::PreMutationRefused)
            {
                return Ok(refused(HoldReason::Unconfirmed));
            }
            let detail: String = input.detail.chars().take(300).collect();
            if attempt.evidence.as_ref() == Some(&input.evidence) && attempt.detail == detail {
                return Ok(DeliveryDelta {
                    result: DeliveryResult::Applied {
                        view: record.view(&recipient),
                    },
                    ..refused(HoldReason::Unconfirmed)
                });
            }
            if record.revision != input.revision {
                return Ok(refused(HoldReason::Corrupt));
            }
            if attempt.evidence.is_some() {
                record.eligibility = Eligibility::Held;
                record.held = Some(HoldReason::Unconfirmed);
                if recipient.barrier.as_ref() != Some(&record.key) {
                    recipient.barrier = Some(record.key.clone());
                    bump(&mut recipient.revision)?;
                    recipient_changed = true;
                }
            } else {
                let retired = attempt.capacity == CapacityState::Retired;
                let frozen = attempt.frozen.as_ref();
                let valid = match (&input.evidence, frozen) {
                    (DeliveryEvidence::VendorQueued, Some(f)) => f.transport == Transport::Queue,
                    (DeliveryEvidence::HookEmissionCompleted, Some(f)) => {
                        f.transport == Transport::Stdout
                    }
                    (DeliveryEvidence::AppServerAccepted { thread, turn }, Some(f)) => {
                        f.transport == Transport::Steer
                            && thread == &attempt.route.thread_id
                            && f.expected_turn.as_ref() == Some(turn)
                    }
                    (
                        DeliveryEvidence::MutationUnconfirmed
                        | DeliveryEvidence::DefinitiveNoMutation,
                        _,
                    ) => true,
                    _ => false,
                };
                if !valid {
                    return Ok(refused(HoldReason::Unconfirmed));
                }
                let overflow = input.detail.chars().count() > 300
                    || match &input.evidence {
                        DeliveryEvidence::AppServerAccepted { thread, turn } => {
                            thread.len() > 4096 || turn.len() > 4096
                        }
                        _ => false,
                    };
                record.attempts[index].evidence = Some(input.evidence.clone());
                record.attempts[index].detail = detail;
                capacity_changed = release_capacity(
                    &mut record,
                    index,
                    &mut recipient,
                    &mut global,
                    CapacityState::Released,
                )?;
                recipient_changed = capacity_changed;
                if retired {
                    // A retired scope keeps its eligibility and ordering fence permanently.
                    // A late, still-authorized response records only evidence/receipt.
                } else if input.evidence == DeliveryEvidence::MutationUnconfirmed || overflow {
                    record.eligibility = Eligibility::Held;
                    record.held = Some(if overflow {
                        HoldReason::EvidenceOverflow
                    } else {
                        HoldReason::Unconfirmed
                    });
                    if recipient.barrier.as_ref() != Some(&record.key) {
                        recipient.barrier = Some(record.key.clone());
                        if !recipient_changed {
                            bump(&mut recipient.revision)?;
                        }
                        recipient_changed = true;
                    }
                } else {
                    if recipient.barrier.as_ref() == Some(&record.key) {
                        recipient.barrier = None;
                        if !recipient_changed {
                            bump(&mut recipient.revision)?;
                        }
                        recipient_changed = true;
                    }
                    if input.evidence == DeliveryEvidence::DefinitiveNoMutation {
                        record.attempts[index].frozen = None;
                        record.eligibility = if record.prepares < 3
                            && record.acknowledgment.manual_at.is_none()
                            && !record.acknowledgment.legacy_unknown
                        {
                            Eligibility::Eligible
                        } else {
                            Eligibility::Held
                        };
                        record.held = (record.prepares >= 3).then_some(HoldReason::BudgetExhausted);
                    } else {
                        record.eligibility = Eligibility::Settled;
                        record.held = None;
                    }
                }
                if matches!(
                    input.evidence,
                    DeliveryEvidence::VendorQueued
                        | DeliveryEvidence::AppServerAccepted { .. }
                        | DeliveryEvidence::HookEmissionCompleted
                ) && let Some(mut projected) = ctx.receipt.clone()
                {
                    if projected.status == crate::ferrowire::DeliveryStatus::Pending {
                        projected.status = crate::ferrowire::DeliveryStatus::Delivered;
                    }
                    projected.attempts = projected.attempts.max(u32::from(record.prepares));
                    projected.updated_at = projected.updated_at.max(input.now);
                    receipt = Some(projected);
                }
            }
        }
        DeliveryCommand::ManualAck(input) => {
            if record.acknowledgment.manual_at.is_some() {
                return Ok(DeliveryDelta {
                    result: DeliveryResult::Applied {
                        view: record.view(&recipient),
                    },
                    ..refused(HoldReason::Unconfirmed)
                });
            }
            record.acknowledgment.manual_at = Some(input.at.clone());
            if record.eligibility == Eligibility::Prepared {
                let index = record.attempts.len() - 1;
                capacity_changed = release_capacity(
                    &mut record,
                    index,
                    &mut recipient,
                    &mut global,
                    CapacityState::Released,
                )?;
                recipient_changed = capacity_changed;
                record.eligibility = Eligibility::Held;
            }
            if let Some(mut projected) = ctx.receipt.clone() {
                projected.status = crate::ferrowire::DeliveryStatus::Acked;
                projected.acked_at = projected.acked_at.or(Some(input.at.clone()));
                projected.updated_at = projected.updated_at.max(input.at);
                receipt = Some(projected);
            }
        }
        DeliveryCommand::InvalidatePrepared { revision, now, .. } => {
            if record.revision != revision || record.eligibility != Eligibility::Prepared {
                return Ok(refused(HoldReason::Corrupt));
            }
            let index = record.attempts.len() - 1;
            // The trusted store may invalidate a disconnected unarmed worker before expiry too.
            let _ = now;
            capacity_changed = release_capacity(
                &mut record,
                index,
                &mut recipient,
                &mut global,
                CapacityState::Released,
            )?;
            recipient_changed = capacity_changed;
            record.eligibility = if record.prepares < 3 {
                Eligibility::Eligible
            } else {
                Eligibility::Held
            };
            record.held = (record.prepares >= 3).then_some(HoldReason::BudgetExhausted);
        }
        DeliveryCommand::Annotate { text, .. } => {
            if record.annotation.len().saturating_add(text.len()) > 4096 {
                record.held = Some(HoldReason::EvidenceOverflow);
                if record.eligibility == Eligibility::Prepared {
                    let index = record.attempts.len() - 1;
                    capacity_changed = release_capacity(
                        &mut record,
                        index,
                        &mut recipient,
                        &mut global,
                        CapacityState::Released,
                    )?;
                    recipient_changed = capacity_changed;
                }
                record.eligibility = Eligibility::Held;
            } else {
                record.annotation.push_str(&text);
            }
        }
        DeliveryCommand::Close { .. } => {
            if record.eligibility == Eligibility::Prepared {
                let index = record.attempts.len() - 1;
                capacity_changed = release_capacity(
                    &mut record,
                    index,
                    &mut recipient,
                    &mut global,
                    CapacityState::Released,
                )?;
                recipient_changed = capacity_changed;
            }
            record.eligibility = Eligibility::Held;
            if record.held.is_none() {
                record.held = Some(HoldReason::Disabled);
            }
        }
    }
    bump(&mut record.revision)?;
    Ok(applied(
        record,
        recipient,
        global,
        capacity_changed,
        recipient_changed,
        receipt,
    ))
}

/// Exact CAS/delta only. This function cannot establish that an emitter ended;
/// only the store-owned verifier may authorize committing its returned delta.
pub fn transition_capacity_retirement(
    ctx: TransitionContext,
    target: CapacityRetirementTarget,
) -> Result<CapacityRetirementDelta, StoreError> {
    let unchanged = |result| CapacityRetirementDelta {
        record: None,
        recipient: None,
        global: None,
        result,
    };
    let Some(mut record) = ctx.record else {
        return Ok(unchanged(CapacityRetirementResult::Stale));
    };
    if ctx.key != target.owner.key || !valid_record(&record, &ctx.key) || !valid_global(&ctx.global)
    {
        return Ok(unchanged(CapacityRetirementResult::Stale));
    }
    let Some(index) = record
        .attempts
        .iter()
        .position(|a| a.binding == target.owner.attempt)
    else {
        return Ok(unchanged(CapacityRetirementResult::Stale));
    };
    if record.attempts[index].capacity != CapacityState::Held {
        return Ok(unchanged(CapacityRetirementResult::AlreadyReleased {
            view: record.view(&ctx.recipient),
        }));
    }
    if record.attempts[index].armed_at.is_none()
        || record.revision != target.record_revision
        || ctx.recipient.revision != target.recipient_revision
        || ctx.global.revision != target.global_revision
        || ctx.recipient.active.as_ref() != Some(&target.owner)
        || !ctx.global.active_index.contains(&target.owner)
        || ctx.recipient.barrier.as_ref() != Some(&record.key)
    {
        return Ok(unchanged(CapacityRetirementResult::Stale));
    }
    let mut recipient = ctx.recipient;
    let mut global = ctx.global;
    release_capacity(
        &mut record,
        index,
        &mut recipient,
        &mut global,
        CapacityState::Retired,
    )?;
    bump(&mut record.revision)?;
    Ok(CapacityRetirementDelta {
        result: CapacityRetirementResult::Retired {
            view: record.view(&recipient),
        },
        record: Some(record),
        recipient: Some(recipient),
        global: Some(global),
    })
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
