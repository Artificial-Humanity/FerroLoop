//! FerroWire agent messaging domain types, recipient addressing, envelopes,
//! and delivery receipts (spec §1).

use crate::agent::{AgentRecord, AgentRole, AgentStatus, AreaRecord};
use crate::at::At;
use crate::ids::{AgentId, MessageId};
use crate::iri::Iri;
use crate::state::{StateError, validate_slug};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

pub const MAX_SUBJECT_BYTES: usize = 512;
pub const MAX_WIRE_BODY_BYTES: usize = 262_144;
/// Minimum age (`30` days in milliseconds) before a terminal (`Acked` or `Expired`)
/// `durable == true` message becomes eligible for [`crate::store::MessageStore::prune_wire`].
pub const DURABLE_WIRE_RETENTION_MS: u64 = 30 * 86_400 * 1000;

/// Whether `msg` is old enough to be pruned at cutoff `before` once all of its
/// receipts have reached a terminal state (`Acked` or `Expired`).
pub fn wire_message_prune_eligible(msg: &WireMessage, before: &At) -> bool {
    if !msg.durable {
        msg.created_at < *before
    } else {
        msg.created_at
            .unix_millis()
            .saturating_add(DURABLE_WIRE_RETENTION_MS)
            <= before.unix_millis()
    }
}

/// Thread root identifier (`MessageId` of the root message in a thread).
pub type ThreadId = MessageId;

/// Deterministic fallback recipient IRI (`urn:fl:agent:<name_key>`) for a
/// persona addressed before its first `fl agent register` (spec §1.2).
pub fn fallback_agent_iri(name_key: &str) -> Iri {
    let norm = validate_slug(name_key).expect("fallback_agent_iri requires a valid slug");
    Iri::parse(&format!("urn:fl:agent:{norm}")).expect("urn:fl:agent:<slug> is a valid IRI")
}

/// Parse a role selector in `@role:<role>` or `@area:<slug>/<role>` (or `agent-msg register --role`),
/// accepting built-in roles, `custom:<slug>`, or a bare `<slug>` mapped to `AgentRole::Custom`.
pub fn parse_role_selector(raw: &str) -> Result<AgentRole, StateError> {
    AgentRole::from_wire(raw).or_else(|_| validate_slug(raw).map(AgentRole::Custom))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WireAddress {
    /// Case-insensitive persona `name_key`, e.g. `"ferris"` or `"jarvis"`.
    AgentName(String),
    /// Immutable agent IRI, e.g. `urn:uuid:...`.
    AgentId(AgentId),
    /// Global broadcast (`"@all"`).
    All,
    /// Coverage area or project selector (`"@area:<slug>"`).
    Area(String),
    /// Workspace role selector (`"@role:<role>"`, e.g. `"@role:lead"`).
    Role(AgentRole),
    /// Combined area and role selector (`"@area:<slug>/<role>"`, e.g. `"@area:ferroloop/lead"`).
    AreaRole { area: String, role: AgentRole },
    /// The Council (`"@council"`): every active agent that covers the `council` area or has
    /// the `strategist` role, in any project. Each agent is a recipient once.
    Council,
    /// A virtual group (`"@<area_key>"`, e.g. `"@ops"`): every active agent whose project or
    /// extra areas include that area, when the area is virtual (has no path). A bare
    /// `ops` stays the persona `ops`; `@council` and `@all` are never groups.
    Group(String),
}

/// The area whose agents `@council` reaches, besides every strategist.
pub const COUNCIL_AREA: &str = "council";

impl WireAddress {
    pub fn parse(raw: &str) -> Result<Self, StateError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(StateError::InvalidAddress(raw.to_string()));
        }
        if trimmed.eq_ignore_ascii_case("@all") || trimmed.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        // Only the `@` form is the selector: a bare `council` stays a persona name.
        if trimmed.eq_ignore_ascii_case("@council") {
            return Ok(Self::Council);
        }
        if let Some(rest) = trimmed.strip_prefix("urn:fl:agent:") {
            let key =
                validate_slug(rest).map_err(|_| StateError::InvalidAddress(raw.to_string()))?;
            return Ok(Self::AgentName(key));
        }
        if trimmed.starts_with("urn:uuid:") {
            let iri =
                Iri::parse(trimmed).map_err(|_| StateError::InvalidAddress(raw.to_string()))?;
            return Ok(Self::AgentId(AgentId(iri)));
        }
        if let Some(rest) = trimmed.strip_prefix("@area:") {
            if let Some((area_raw, role_raw)) = rest.split_once('/') {
                let area = validate_slug(area_raw)
                    .map_err(|_| StateError::InvalidAddress(raw.to_string()))?;
                let role = parse_role_selector(role_raw)
                    .map_err(|_| StateError::InvalidAddress(raw.to_string()))?;
                return Ok(Self::AreaRole { area, role });
            }
            let area =
                validate_slug(rest).map_err(|_| StateError::InvalidAddress(raw.to_string()))?;
            return Ok(Self::Area(area));
        }
        if let Some(role_raw) = trimmed.strip_prefix("@role:") {
            let role = parse_role_selector(role_raw)
                .map_err(|_| StateError::InvalidAddress(raw.to_string()))?;
            return Ok(Self::Role(role));
        }
        if let Some(group) = trimmed.strip_prefix('@') {
            // `@<area_key>`: a virtual group. `@all` and `@council` were taken above, and a
            // selector with a `:` or `/` is not a slug.
            let key =
                validate_slug(group).map_err(|_| StateError::InvalidAddress(raw.to_string()))?;
            return Ok(Self::Group(key));
        }
        let key =
            validate_slug(trimmed).map_err(|_| StateError::InvalidAddress(raw.to_string()))?;
        Ok(Self::AgentName(key))
    }

    pub fn as_wire(&self) -> String {
        match self {
            Self::AgentName(n) => n.clone(),
            Self::AgentId(id) => id.as_str().to_string(),
            Self::All => "@all".to_string(),
            Self::Area(a) => format!("@area:{a}"),
            Self::Role(r) => format!("@role:{}", r.as_wire()),
            Self::AreaRole { area, role } => format!("@area:{area}/{}", role.as_wire()),
            Self::Council => "@council".to_string(),
            Self::Group(g) => format!("@{g}"),
        }
    }
}

impl fmt::Display for WireAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_wire())
    }
}

impl FromStr for WireAddress {
    type Err = StateError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for WireAddress {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_wire())
    }
}

impl<'de> Deserialize<'de> for WireAddress {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Mail,
    Ask,
    Reply,
    Handoff,
    TaskOffer,
    ApprovalRequest,
    System,
}

crate::wire::wire_names!(MessageKind as message_kind_wire {
    Mail => "mail",
    Ask => "ask",
    Reply => "reply",
    Handoff => "handoff",
    TaskOffer => "task_offer",
    ApprovalRequest => "approval_request",
    System => "system",
});
crate::wire::wire_parse!(MessageKind as message_kind_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageUrgency {
    Normal,
    Urgent,
}

crate::wire::wire_names!(MessageUrgency as message_urgency_wire {
    Normal => "normal",
    Urgent => "urgent",
});
crate::wire::wire_parse!(MessageUrgency as message_urgency_parse);

pub fn validate_subject(raw: &str) -> Result<String, StateError> {
    if raw.contains('\n') || raw.contains('\r') {
        return Err(StateError::InvalidMessageSubject);
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_SUBJECT_BYTES {
        return Err(StateError::InvalidMessageSubject);
    }
    Ok(trimmed.to_string())
}

pub fn validate_wire_body(raw: &str) -> Result<String, StateError> {
    if raw.trim().is_empty() || raw.len() > MAX_WIRE_BODY_BYTES {
        return Err(StateError::InvalidMessageBody(raw.len()));
    }
    Ok(raw.to_string())
}

pub fn validate_sender_name(raw: &str) -> Result<String, StateError> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("system")
        || trimmed.eq_ignore_ascii_case("owner")
        || trimmed.eq_ignore_ascii_case("unknown")
    {
        return Ok(trimmed.to_ascii_lowercase());
    }
    validate_slug(trimmed)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireMessage {
    pub id: MessageId,
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<AgentId>,
    pub sender_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_host: Option<String>,
    pub to: WireAddress,
    pub kind: MessageKind,
    pub urgency: MessageUrgency,
    pub durable: bool,
    pub subject: String,
    pub body: String,
    pub created_at: At,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<At>,
}

impl WireMessage {
    pub fn validate(&self) -> Result<(), StateError> {
        let _ = validate_subject(&self.subject)?;
        let _ = validate_wire_body(&self.body)?;
        let _ = validate_sender_name(&self.sender_name)?;
        if matches!(self.kind, MessageKind::Reply)
            && (self.reply_to.is_none() || self.correlation_id.is_none())
        {
            return Err(StateError::ReplyRequiresParent);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending = 0,
    Delivered = 1,
    Acked = 2,
    Expired = 3,
}

crate::wire::wire_names!(DeliveryStatus as delivery_status_wire {
    Pending => "pending",
    Delivered => "delivered",
    Acked => "acked",
    Expired => "expired",
});
crate::wire::wire_parse!(DeliveryStatus as delivery_status_parse);

impl DeliveryStatus {
    /// Monotonic convergence rank (`Pending < Delivered < Expired < Acked`).
    pub fn rank(self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::Delivered => 1,
            Self::Expired => 2,
            Self::Acked => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryReceipt {
    pub message_id: MessageId,
    pub recipient_id: Iri,
    pub recipient_name: String,
    pub status: DeliveryStatus,
    pub attempts: u32,
    pub updated_at: At,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at: Option<At>,
}

impl DeliveryReceipt {
    /// Deterministic, commutative monotonic receipt merge (spec §1.3).
    pub fn merge(&self, incoming: &DeliveryReceipt) -> DeliveryReceipt {
        let status = if self.status.rank() >= incoming.status.rank() {
            self.status
        } else {
            incoming.status
        };
        let attempts = self.attempts.max(incoming.attempts);
        let updated_at = self.updated_at.clone().max(incoming.updated_at.clone());
        let acked_at = if status == DeliveryStatus::Acked {
            match (&self.acked_at, &incoming.acked_at) {
                (Some(a), Some(b)) => Some(a.clone().min(b.clone())),
                (Some(a), None) => Some(a.clone()),
                (None, Some(b)) => Some(b.clone()),
                (None, None) => Some(updated_at.clone()),
            }
        } else {
            None
        };
        // Order-independent: a real name beats a `urn:` placeholder, which
        // beats none; between two of the same rank the lesser string wins, so
        // replicas merging in opposite orders agree.
        let rank = |n: &str| -> u8 {
            if n.is_empty() {
                0
            } else if n.starts_with("urn:") {
                1
            } else {
                2
            }
        };
        let (a, b) = (&self.recipient_name, &incoming.recipient_name);
        let recipient_name = match rank(a).cmp(&rank(b)) {
            std::cmp::Ordering::Greater => a.clone(),
            std::cmp::Ordering::Less => b.clone(),
            std::cmp::Ordering::Equal => a.min(b).clone(),
        };
        DeliveryReceipt {
            message_id: self.message_id.clone(),
            recipient_id: self.recipient_id.clone(),
            recipient_name,
            status,
            attempts,
            updated_at,
            acked_at,
        }
    }
}

/// Why one item of an imported wire batch was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireImportRejectReason {
    /// The message fails [`WireMessage::validate`]. It was not stored.
    InvalidMessage,
    /// A different message is already stored under this id. The stored copy is kept.
    TamperedMessage,
    /// The message id is already used by an object of another kind. It was not stored.
    IdCollision,
    /// A receipt for a message this store does not hold. It was not stored.
    UnknownMessage,
}

impl fmt::Display for WireImportRejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidMessage => "message failed validation",
            Self::TamperedMessage => "differs from the stored copy",
            Self::IdCollision => "id belongs to another kind of object",
            Self::UnknownMessage => "receipt for an unknown message",
        })
    }
}

/// One message or receipt of an imported batch that was skipped, and why.
///
/// `detail` is the validation text, never a message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireImportRejection {
    pub message_id: MessageId,
    /// `true` for a receipt, `false` for a message.
    pub is_receipt: bool,
    pub reason: WireImportRejectReason,
    pub detail: String,
}

/// What [`MessageStore::import_wire_batch`](crate::store::MessageStore::import_wire_batch)
/// did with a batch. Every item is judged alone: a bad one is listed in `rejected` and skipped,
/// and the good ones are still imported. A skipped item is never stored, so a rejection never
/// means an unchecked message got in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WireImportReport {
    /// Messages stored for the first time by this call.
    pub imported_messages: usize,
    /// Receipts stored or advanced by this call.
    pub imported_receipts: usize,
    pub rejected: Vec<WireImportRejection>,
}

impl WireImportReport {
    pub fn is_clean(&self) -> bool {
        self.rejected.is_empty()
    }

    /// Whether the message `id`, or a receipt for it, was skipped.
    pub fn rejects(&self, id: &MessageId) -> bool {
        self.rejected.iter().any(|r| &r.message_id == id)
    }

    /// Whether the message `id` itself was skipped (not just a receipt for it).
    pub fn rejects_message(&self, id: &MessageId) -> bool {
        self.rejected
            .iter()
            .any(|r| !r.is_receipt && &r.message_id == id)
    }

    /// The first rejection as the error a strict caller would have got from the old
    /// all-or-nothing import, or `None` for a clean batch.
    pub fn first_rejection_error(&self) -> Option<crate::store::StoreError> {
        use crate::store::StoreError;
        let r = self.rejected.first()?;
        Some(match r.reason {
            WireImportRejectReason::TamperedMessage => StoreError::WireMessageTampered {
                id: r.message_id.clone(),
                detail: "imported wire message differs from stored copy".into(),
            },
            WireImportRejectReason::UnknownMessage => {
                StoreError::MessageNotFound(r.message_id.clone())
            }
            WireImportRejectReason::InvalidMessage | WireImportRejectReason::IdCollision => {
                StoreError::InvalidState(r.detail.clone())
            }
        })
    }

    /// Record a skipped item. For store implementations.
    pub fn reject(
        &mut self,
        message_id: &MessageId,
        is_receipt: bool,
        reason: WireImportRejectReason,
        detail: impl Into<String>,
    ) {
        self.rejected.push(WireImportRejection {
            message_id: message_id.clone(),
            is_receipt,
            reason,
            detail: detail.into(),
        });
    }
}

/// Resolve `to` against the current `AgentRegistry` roster (spec §1.2).
/// Returns `(recipient_id, recipient_name)` pairs in deterministic order.
///
/// `areas` is the registry's areas: a [`WireAddress::Group`] reaches its members only when the
/// area it names is there and is virtual, so a mistyped or non-virtual `@name` reaches no one.
pub fn resolve_recipients(
    to: &WireAddress,
    sender_id: Option<&AgentId>,
    sender_name: &str,
    all_agents: &[AgentRecord],
    areas: &[AreaRecord],
) -> Vec<(Iri, String)> {
    let is_sender = |a: &AgentRecord| -> bool {
        sender_id.is_some_and(|sid| sid == &a.id) || a.name_key == sender_name
    };
    match to {
        WireAddress::AgentName(name_key) => {
            if let Some(agent) = all_agents
                .iter()
                .find(|a| a.status == AgentStatus::Active && &a.name_key == name_key)
                .or_else(|| {
                    all_agents
                        .iter()
                        .filter(|a| &a.name_key == name_key)
                        .max_by(|a, b| (&a.updated_at, &a.id).cmp(&(&b.updated_at, &b.id)))
                })
            {
                vec![(agent.id.0.clone(), agent.name_key.clone())]
            } else {
                vec![(fallback_agent_iri(name_key), name_key.clone())]
            }
        }
        WireAddress::AgentId(id) => {
            if let Some(agent) = all_agents.iter().find(|a| &a.id == id) {
                vec![(agent.id.0.clone(), agent.name_key.clone())]
            } else {
                vec![(id.0.clone(), id.as_str().to_owned())]
            }
        }
        WireAddress::All => {
            let mut out: Vec<(Iri, String)> = all_agents
                .iter()
                .filter(|a| a.status == AgentStatus::Active && !is_sender(a))
                .map(|a| (a.id.0.clone(), a.name_key.clone()))
                .collect();
            out.sort_by(|a, b| a.1.cmp(&b.1));
            out
        }
        WireAddress::Area(slug) => {
            let mut out: Vec<(Iri, String)> = all_agents
                .iter()
                .filter(|a| a.status == AgentStatus::Active && a.covers_area(slug) && !is_sender(a))
                .map(|a| (a.id.0.clone(), a.name_key.clone()))
                .collect();
            out.sort_by(|a, b| a.1.cmp(&b.1));
            out
        }
        WireAddress::Role(role) => {
            let mut out: Vec<(Iri, String)> = all_agents
                .iter()
                .filter(|a| a.status == AgentStatus::Active && &a.role == role && !is_sender(a))
                .map(|a| (a.id.0.clone(), a.name_key.clone()))
                .collect();
            out.sort_by(|a, b| a.1.cmp(&b.1));
            out
        }
        WireAddress::Council => {
            let mut out: Vec<(Iri, String)> = all_agents
                .iter()
                .filter(|a| {
                    a.status == AgentStatus::Active
                        && (a.covers_area(COUNCIL_AREA) || a.role == AgentRole::Strategist)
                        && !is_sender(a)
                })
                .map(|a| (a.id.0.clone(), a.name_key.clone()))
                .collect();
            out.sort_by(|a, b| a.1.cmp(&b.1));
            out
        }
        WireAddress::Group(key) => {
            if !areas.iter().any(|a| &a.area_key == key && a.is_virtual()) {
                return Vec::new();
            }
            let mut out: Vec<(Iri, String)> = all_agents
                .iter()
                .filter(|a| a.status == AgentStatus::Active && a.covers_area(key) && !is_sender(a))
                .map(|a| (a.id.0.clone(), a.name_key.clone()))
                .collect();
            out.sort_by(|a, b| a.1.cmp(&b.1));
            out
        }
        WireAddress::AreaRole { area, role } => {
            let mut out: Vec<(Iri, String)> = all_agents
                .iter()
                .filter(|a| {
                    a.status == AgentStatus::Active
                        && a.covers_area(area)
                        && &a.role == role
                        && !is_sender(a)
                })
                .map(|a| (a.id.0.clone(), a.name_key.clone()))
                .collect();
            out.sort_by(|a, b| a.1.cmp(&b.1));
            out
        }
    }
}

/// Refuses a send to `@<area_key>` when that is not a virtual area of `areas`: a typo or a real
/// project's key would otherwise be a send to no one. Every other address passes.
pub fn check_group_address(to: &WireAddress, areas: &[AreaRecord]) -> Result<(), String> {
    let WireAddress::Group(key) = to else {
        return Ok(());
    };
    match areas.iter().find(|a| &a.area_key == key) {
        Some(a) if a.is_virtual() => Ok(()),
        Some(_) => Err(format!(
            "`@{key}` is not a virtual group: `{key}` is an area with a path. Use `@area:{key}`"
        )),
        None => Err(format!(
            "`@{key}` names no virtual group: create one with `fl project create {key} --virtual`"
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendWireArgs {
    /// Optional caller-supplied `MessageId` (`urn:uuid:...`); minted by the store if `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<AgentId>,
    pub sender_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_host: Option<String>,
    pub to: WireAddress,
    pub kind: MessageKind,
    pub urgency: MessageUrgency,
    pub durable: bool,
    pub subject: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<At>,
    pub at: At,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireInboxItem {
    pub message: WireMessage,
    pub receipt: DeliveryReceipt,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    #[serde(default)]
    pub unread_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::seq_iri;

    #[test]
    fn receipt_merge_is_commutative_when_the_names_differ() {
        let base = |name: &str| DeliveryReceipt {
            message_id: MessageId(seq_iri(1)),
            recipient_id: seq_iri(2),
            recipient_name: name.into(),
            status: DeliveryStatus::Delivered,
            attempts: 1,
            updated_at: At::from_unix_millis(5),
            acked_at: None,
        };
        for (a, b) in [
            ("alice", "bob"),
            ("urn:x:1", "urn:x:2"),
            ("urn:x:1", "bob"),
            ("", "bob"),
            ("", "urn:x:1"),
        ] {
            let (ra, rb) = (base(a), base(b));
            assert_eq!(ra.merge(&rb), rb.merge(&ra), "{a:?} vs {b:?}");
        }
        assert_eq!(base("urn:x:1").merge(&base("bob")).recipient_name, "bob");
    }

    #[test]
    fn wire_address_parses_and_round_trips_all_selector_variants() {
        let cases = [
            ("Ferris", WireAddress::AgentName("ferris".into()), "ferris"),
            (
                "urn:fl:agent:Jarvis",
                WireAddress::AgentName("jarvis".into()),
                "jarvis",
            ),
            ("@all", WireAddress::All, "@all"),
            ("all", WireAddress::All, "@all"),
            ("@council", WireAddress::Council, "@council"),
            ("@Council", WireAddress::Council, "@council"),
            ("  @COUNCIL ", WireAddress::Council, "@council"),
            (
                "council",
                WireAddress::AgentName("council".into()),
                "council",
            ),
            ("@ops", WireAddress::Group("ops".into()), "@ops"),
            ("@Ops_2", WireAddress::Group("ops_2".into()), "@ops_2"),
            ("ops", WireAddress::AgentName("ops".into()), "ops"),
            // A selector name that is not a prefix form stays a group, never a selector.
            ("@area", WireAddress::Group("area".into()), "@area"),
            ("@role", WireAddress::Group("role".into()), "@role"),
            (
                "@area:FerroLoop",
                WireAddress::Area("ferroloop".into()),
                "@area:ferroloop",
            ),
            (
                "@role:lead",
                WireAddress::Role(AgentRole::Lead),
                "@role:lead",
            ),
            (
                "@role:scribe",
                WireAddress::Role(AgentRole::Journalist),
                "@role:journalist",
            ),
            (
                "@role:custom:ui_developer",
                WireAddress::Role(AgentRole::UiDeveloper),
                "@role:ui-developer",
            ),
            (
                "@role:reviewer",
                WireAddress::Role(AgentRole::Custom("reviewer".into())),
                "@role:custom:reviewer",
            ),
            (
                "@area:Kaggle-Gemma4/scribe",
                WireAddress::AreaRole {
                    area: "kaggle-gemma4".into(),
                    role: AgentRole::Journalist,
                },
                "@area:kaggle-gemma4/journalist",
            ),
            (
                "@area:Kaggle-Gemma4/auditor",
                WireAddress::AreaRole {
                    area: "kaggle-gemma4".into(),
                    role: AgentRole::Custom("auditor".into()),
                },
                "@area:kaggle-gemma4/custom:auditor",
            ),
            (
                "@area:FerroLoop/lead",
                WireAddress::AreaRole {
                    area: "ferroloop".into(),
                    role: AgentRole::Lead,
                },
                "@area:ferroloop/lead",
            ),
        ];

        for (raw, expected, wire) in cases {
            let parsed = WireAddress::parse(raw).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.as_wire(), wire);
            let json = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, format!("\"{wire}\""));
            let back: WireAddress = serde_json::from_str(&json).unwrap();
            assert_eq!(back, parsed);
        }

        let agent_iri = seq_iri(42);
        let by_id = WireAddress::parse(agent_iri.as_str()).unwrap();
        assert_eq!(by_id, WireAddress::AgentId(AgentId(agent_iri.clone())));
        assert_eq!(by_id.as_wire(), agent_iri.as_str());

        for bad in [
            "",
            "@",
            "@1ops",
            "@o ps",
            "@ops/lead",
            "@ops:x",
            "@area:",
            "@role:",
            "@area:bad/123",
            "123",
        ] {
            assert!(
                matches!(WireAddress::parse(bad), Err(StateError::InvalidAddress(_))),
                "expected InvalidAddress for {bad:?}"
            );
        }
    }

    fn seat(n: u64, name: &str, role: AgentRole, project: &str, extra: &[&str]) -> AgentRecord {
        AgentRecord {
            id: AgentId(seq_iri(n)),
            name_key: name.to_ascii_lowercase(),
            display_name: name.into(),
            email: format!("{}@artificialhumanity.io", name.to_ascii_lowercase()),
            project: project.into(),
            role,
            extra_areas: extra.iter().map(|s| s.to_string()).collect(),
            status: AgentStatus::Active,
            updated_at: At::from_unix_millis(n),
            by: "owner".into(),
        }
    }

    fn names(to: &str, agents: &[AgentRecord], sender: &str) -> Vec<String> {
        let to = WireAddress::parse(to).unwrap();
        resolve_recipients(&to, None, sender, agents, &[])
            .into_iter()
            .map(|(_, n)| n)
            .collect()
    }

    #[test]
    fn at_council_reaches_the_council_area_and_every_strategist_once() {
        let mut retired_strategist = seat(6, "gone", AgentRole::Strategist, "ferroloop", &[]);
        retired_strategist.status = AgentStatus::Retired;
        let mut retired_member = seat(7, "left", AgentRole::Tester, "council", &[]);
        retired_member.status = AgentStatus::Retired;
        let agents = vec![
            // A strategist in a project, and one inside the council area: both in, once each.
            seat(1, "ada", AgentRole::Strategist, "ferroloop", &[]),
            seat(2, "bea", AgentRole::Strategist, "council", &[]),
            // A non-strategist in the council area, by project and by extra area.
            seat(3, "cy", AgentRole::Journalist, "council", &[]),
            seat(4, "di", AgentRole::Assistant, "ferroloop", &["council"]),
            // Neither: other roles and areas.
            seat(5, "ed", AgentRole::Lead, "ferroloop", &[]),
            retired_strategist,
            retired_member,
        ];
        assert_eq!(
            names("@council", &agents, "owner"),
            ["ada", "bea", "cy", "di"]
        );
        // Selectors are case-insensitive, as `@all` and `@area:` are.
        assert_eq!(
            names("@Council", &agents, "owner"),
            names("@council", &agents, "owner")
        );
        // The sender is not a recipient of its own message.
        assert_eq!(names("@council", &agents, "bea"), ["ada", "cy", "di"]);
        // It contains `@role:strategist` and adds the council area.
        let strategists = names("@role:strategist", &agents, "owner");
        assert_eq!(strategists, ["ada", "bea"]);
        assert!(
            strategists
                .iter()
                .all(|n| names("@council", &agents, "owner").contains(n))
        );
    }

    fn virtual_area(key: &str) -> AreaRecord {
        AreaRecord {
            area_key: key.into(),
            name: key.into(),
            path: None,
            purpose: "p".into(),
            updated_at: At::from_unix_millis(1),
            by: "owner".into(),
        }
    }

    fn real_area(key: &str) -> AreaRecord {
        AreaRecord {
            path: Some(format!("/work/{key}")),
            ..virtual_area(key)
        }
    }

    fn group_names(
        to: &str,
        agents: &[AgentRecord],
        areas: &[AreaRecord],
        sender: &str,
    ) -> Vec<String> {
        let to = WireAddress::parse(to).unwrap();
        resolve_recipients(&to, None, sender, agents, areas)
            .into_iter()
            .map(|(_, n)| n)
            .collect()
    }

    #[test]
    fn at_area_key_reaches_the_members_of_a_virtual_area_only() {
        let mut gone = seat(6, "gone", AgentRole::Tester, "ops", &[]);
        gone.status = AgentStatus::Retired;
        let agents = vec![
            seat(1, "ada", AgentRole::Lead, "ops", &[]),
            // A member through an extra area, and one with a different role.
            seat(2, "bea", AgentRole::Tester, "ferroloop", &["ops"]),
            seat(3, "cy", AgentRole::Strategist, "ops", &[]),
            // Not members.
            seat(4, "di", AgentRole::Lead, "ferroloop", &[]),
            seat(5, "ed", AgentRole::Strategist, "ferroloop", &[]),
            gone,
        ];
        let areas = [virtual_area("ops"), real_area("ferroloop")];
        assert_eq!(
            group_names("@ops", &agents, &areas, "owner"),
            ["ada", "bea", "cy"]
        );
        assert_eq!(
            group_names("@OPS", &agents, &areas, "owner"),
            ["ada", "bea", "cy"]
        );
        // The sender is left out, as for `@area:`.
        assert_eq!(group_names("@ops", &agents, &areas, "ada"), ["bea", "cy"]);
        // `@area:ops` reaches the same agents: the group is the virtual spelling of it.
        assert_eq!(
            group_names("@area:ops", &agents, &areas, "owner"),
            group_names("@ops", &agents, &areas, "owner")
        );
        // A real project's key, an unknown key and a registry without areas reach no one.
        assert!(group_names("@ferroloop", &agents, &areas, "owner").is_empty());
        assert!(group_names("@nowhere", &agents, &areas, "owner").is_empty());
        assert!(group_names("@ops", &agents, &[], "owner").is_empty());
    }

    #[test]
    fn a_persona_named_like_an_area_key_is_the_bare_name_and_the_group_is_the_at_form() {
        let agents = vec![
            seat(1, "ops", AgentRole::Tester, "ferroloop", &[]),
            seat(2, "ada", AgentRole::Lead, "ops", &[]),
        ];
        let areas = [virtual_area("ops")];
        // Bare: the persona (not a member of the area, still the only recipient) ...
        assert_eq!(group_names("ops", &agents, &areas, "owner"), ["ops"]);
        // ... `@ops`: the group's members, not the persona.
        assert_eq!(group_names("@ops", &agents, &areas, "owner"), ["ada"]);
    }

    #[test]
    fn at_council_wins_over_a_virtual_council_group_and_keeps_the_strategists() {
        let agents = vec![
            seat(1, "ada", AgentRole::Strategist, "ferroloop", &[]),
            seat(2, "bea", AgentRole::Tester, "council", &[]),
        ];
        let areas = [virtual_area("council")];
        assert_eq!(
            group_names("@council", &agents, &areas, "owner"),
            ["ada", "bea"]
        );
        assert_eq!(
            WireAddress::parse("@council").unwrap(),
            WireAddress::Council
        );
        // `all` is never a group: `@all` is everyone.
        assert_eq!(WireAddress::parse("@all").unwrap(), WireAddress::All);
    }

    #[test]
    fn a_send_to_an_unknown_or_real_area_key_is_refused_with_a_way_out() {
        let areas = [virtual_area("ops"), real_area("ferroloop")];
        let check = |to: &str| check_group_address(&WireAddress::parse(to).unwrap(), &areas);
        assert!(check("@ops").is_ok());
        assert!(check("ops").is_ok());
        assert!(check("@all").is_ok() && check("@council").is_ok());
        assert!(check("@area:ferroloop").is_ok());
        let real = check("@ferroloop").unwrap_err();
        assert!(real.contains("@area:ferroloop"), "{real}");
        let unknown = check("@nowhere").unwrap_err();
        assert!(unknown.contains("--virtual"), "{unknown}");
    }

    #[test]
    fn at_council_is_not_shadowed_by_an_agent_named_council() {
        let agents = vec![
            seat(1, "council", AgentRole::Tester, "ferroloop", &[]),
            seat(2, "ada", AgentRole::Strategist, "ferroloop", &[]),
        ];
        // `@council` is the group (the persona has neither the area nor the role) ...
        assert_eq!(names("@council", &agents, "owner"), ["ada"]);
        // ... and a bare `council` is still the persona.
        assert_eq!(names("council", &agents, "owner"), ["council"]);
    }

    #[test]
    fn delivery_receipt_merge_is_commutative_and_monotonic() {
        let msg_id = MessageId(seq_iri(1));
        let rec_id = seq_iri(2);
        let r_pending = DeliveryReceipt {
            message_id: msg_id.clone(),
            recipient_id: rec_id.clone(),
            recipient_name: "ferris".into(),
            status: DeliveryStatus::Pending,
            attempts: 1,
            updated_at: At::from_unix_millis(10),
            acked_at: None,
        };
        let r_delivered = DeliveryReceipt {
            message_id: msg_id.clone(),
            recipient_id: rec_id.clone(),
            recipient_name: "ferris".into(),
            status: DeliveryStatus::Delivered,
            attempts: 3,
            updated_at: At::from_unix_millis(20),
            acked_at: None,
        };
        let r_expired = DeliveryReceipt {
            message_id: msg_id.clone(),
            recipient_id: rec_id.clone(),
            recipient_name: "ferris".into(),
            status: DeliveryStatus::Expired,
            attempts: 5,
            updated_at: At::from_unix_millis(30),
            acked_at: None,
        };
        let r_acked = DeliveryReceipt {
            message_id: msg_id,
            recipient_id: rec_id,
            recipient_name: "ferris".into(),
            status: DeliveryStatus::Acked,
            attempts: 2,
            updated_at: At::from_unix_millis(25),
            acked_at: Some(At::from_unix_millis(25)),
        };

        assert_eq!(r_pending.merge(&r_delivered), r_delivered.merge(&r_pending));
        let merged_ack_exp = r_expired.merge(&r_acked);
        assert_eq!(merged_ack_exp, r_acked.merge(&r_expired));
        assert_eq!(merged_ack_exp.status, DeliveryStatus::Acked);
        assert_eq!(merged_ack_exp.attempts, 5);
        assert_eq!(merged_ack_exp.updated_at, At::from_unix_millis(30));
        assert_eq!(merged_ack_exp.acked_at, Some(At::from_unix_millis(25)));
    }

    #[test]
    fn retired_agent_recipient_resolves_to_canonical_agent_id_before_fallback_iri() {
        let mut retired = seat(42, "ferris", AgentRole::Lead, "ferroloop", &[]);
        retired.status = AgentStatus::Retired;
        let agents = vec![retired.clone()];

        let resolved = resolve_recipients(
            &WireAddress::AgentName("ferris".into()),
            None,
            "jarvis",
            &agents,
            &[],
        );
        assert_eq!(resolved, vec![(retired.id.0, "ferris".to_string())]);

        let unknown = resolve_recipients(
            &WireAddress::AgentName("never-registered".into()),
            None,
            "jarvis",
            &agents,
            &[],
        );
        assert_eq!(
            unknown,
            vec![(
                fallback_agent_iri("never-registered"),
                "never-registered".to_string()
            )]
        );
    }
}
