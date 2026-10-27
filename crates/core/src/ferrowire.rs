//! FerroWire agent messaging domain types, recipient addressing, envelopes,
//! and delivery receipts (spec §1).

use crate::agent::{AgentRecord, AgentRole, AgentStatus};
use crate::at::At;
use crate::ids::{AgentId, MessageId};
use crate::iri::Iri;
use crate::state::{StateError, validate_slug};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

pub const MAX_SUBJECT_BYTES: usize = 512;
pub const MAX_WIRE_BODY_BYTES: usize = 262_144;

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
}

impl WireAddress {
    pub fn parse(raw: &str) -> Result<Self, StateError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(StateError::InvalidAddress(raw.to_string()));
        }
        if trimmed.eq_ignore_ascii_case("@all") || trimmed.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
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
        if trimmed.starts_with('@') {
            return Err(StateError::InvalidAddress(raw.to_string()));
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
        let recipient_name =
            if !self.recipient_name.is_empty() && !self.recipient_name.starts_with("urn:") {
                self.recipient_name.clone()
            } else if !incoming.recipient_name.is_empty() {
                incoming.recipient_name.clone()
            } else {
                self.recipient_name.clone()
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

/// Resolve `to` against the current `AgentRegistry` roster (spec §1.2).
/// Returns `(recipient_id, recipient_name)` pairs in deterministic order.
pub fn resolve_recipients(
    to: &WireAddress,
    sender_id: Option<&AgentId>,
    sender_name: &str,
    all_agents: &[AgentRecord],
) -> Vec<(Iri, String)> {
    let is_sender = |a: &AgentRecord| -> bool {
        sender_id.is_some_and(|sid| sid == &a.id) || a.name_key == sender_name
    };
    match to {
        WireAddress::AgentName(name_key) => {
            if let Some(agent) = all_agents
                .iter()
                .find(|a| a.status == AgentStatus::Active && &a.name_key == name_key)
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
                WireAddress::Role(AgentRole::Custom("scribe".into())),
                "@role:custom:scribe",
            ),
            (
                "@area:Kaggle-Gemma4/scribe",
                WireAddress::AreaRole {
                    area: "kaggle-gemma4".into(),
                    role: AgentRole::Custom("scribe".into()),
                },
                "@area:kaggle-gemma4/custom:scribe",
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

        for bad in ["", "@unknown", "@area:", "@role:", "@area:bad/123", "123"] {
            assert!(
                matches!(WireAddress::parse(bad), Err(StateError::InvalidAddress(_))),
                "expected InvalidAddress for {bad:?}"
            );
        }
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
}
