# FerroWire Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement FerroWire (`fl wire`), FerroLoop's unified agent-to-agent and human-to-agent messaging, real-time push delivery (`fl-wire` daemon + native Claude Code / Antigravity / Codex push dispatchers + WebSocket/TLS peer relay), offline git `fl/state` store-and-forward, `agent-msg` compatibility shim, and MCP messaging tools.

**Architecture:**
1. **`fl-core` (`crates/core`):** Defines `Kind::Message`, `MessageId`, `ThreadId`, `WireAddress`, `MessageKind`, `MessageUrgency`, `WireMessage`, `DeliveryStatus`, `DeliveryReceipt`, `SendWireArgs`, `WireInboxItem`, `WireFilter`, recipient resolution (`resolve_recipients`), `MessageStore` trait, `MemStore` implementation, and the shared 7-case `conformance::message_store` test suite.
2. **`fl-store` (`crates/store`):** Extends `RedbStore` to **Format 7** (`FORMAT_WITH_WIRE = 7`) with 7 `wire_*` tables (`wire_messages`, `wire_msg_log`, `wire_msg_seq`, `wire_inbox`, `wire_rcpt_log`, `wire_threads`, `wire_replies`), lazy Format 6 $\rightarrow$ 7 migration on first wire write, `MessageStore` implementation, and `verify_wire_invariants`.
3. **`fl-exec` (`crates/exec`):** Extends `state_sync` to selectively export and import unacknowledged (`status != Acked`) and durable (`durable == true`) `WireMessage` blobs (`wire/messages/<prefix2>/<file>.json`) and append-only `DeliveryReceipt` JSONL segments (`wire/receipts/<seg:04>.jsonl`) on `refs/heads/fl/state`, with tamper detection in `verify_state`.
4. **`fl-wire` (`crates/wire`):** Implements caller persona resolution and vendor session discovery (`identity.rs`), native push delivery to Claude Code Unix sockets, Antigravity `agentapi`, Codex queues, and test Webhook sockets (`dispatcher.rs`), zero-new-dependency RFC 6455 WebSocket framing over TCP/TLS (`ws.rs`), HMAC-SHA256 PSK peer authentication and cursor catch-up (`peer.rs`), and the local Unix domain socket + WebSocket peer relay daemon (`daemon.rs`).
5. **`fl-cli` (`crates/cli`):** Adds `[wire]` config parsing (`config.rs`), `fl wire` subcommands (`send`, `broadcast`, `ask`, `reply`, `read`, `ack`, `wait`, `prune`, `status`, `daemon`, `hook claude`, `mcp`), multi-call `argv[0]` dispatch (`agent-msg`, `agent-bus-claude-hook`, `agent-bus-mcp`), and 6 MCP tools (`send_message`, `broadcast_message`, `ask_message`, `reply_message`, `get_messages`, `ack_message`) in `state_mcp.rs`.

**Tech Stack:** Rust (edition 2024), `redb` 4.3, `serde` / `serde_json`, `sha2`, `ring`, `base64`, `rustls` 0.23, `clap` 4, `git` plumbing (`fl-exec`).

**Spec:** `docs/superpowers/specs/2026-10-24-ferrowire-design.md`

## Global Constraints

1. **Zero external crate downloads (`--offline`):** Every crate dependency used by `fl-wire` and the rest of the workspace (`rustls`, `rustls-pki-types`, `webpki-roots`, `ring`, `sha2`, `base64`, `serde`, `serde_json`, `thiserror`) MUST resolve from the existing `Cargo.lock` so `cargo test --workspace --offline` and `cargo clippy --workspace --all-targets -- -D warnings` succeed without network access.
2. **Unhanded `Kind::Message` IRIs:** `MessageId` (`urn:uuid:...`) is owned in `ids` under `Kind::Message`, but never allocates a sequential numeric handle in `handles`/`by_handle`.
3. **Byte-for-byte `WireMessage` immutability & monotonic `DeliveryReceipt` convergence:** Once written, a `WireMessage` never mutates (importing a differing payload with the same `MessageId` fails with `StoreError::WireMessageTampered`). `DeliveryReceipt::merge` is commutative, associative, and idempotent (`Pending < Delivered < Expired < Acked`).
4. **Ack only on verified push:** Automatic push dispatch marks a receipt `Acked` only when the vendor socket/CLI push succeeds; failed push attempts increment `attempts` while keeping `status = Pending` (up to `MAX_DELIVER = 5`).

---

## Task 1: `fl-core` FerroWire Domain Types, Addressing, and `MessageStore` Trait

**Files:**
- Modify: `crates/core/src/ids.rs`
- Modify: `crates/core/src/agent.rs`
- Modify: `crates/core/src/state.rs`
- Create: `crates/core/src/ferrowire.rs`
- Modify: `crates/core/src/store.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/core/tests/wire_refs.rs`
- Modify: `crates/github/src/meta.rs`

**Interfaces:**
- Consumes: `fl_core::iri::Iri`, `fl_core::at::At`, `fl_core::agent::{AgentRecord, AgentRole, AgentStatus}`, `fl_core::state::{StateError, validate_slug}`
- Produces: `Kind::Message`, `MessageId`, `ThreadId`, `WireAddress`, `MessageKind`, `MessageUrgency`, `WireMessage`, `DeliveryStatus`, `DeliveryReceipt`, `SendWireArgs`, `WireInboxItem`, `WireFilter`, `fallback_agent_iri`, `parse_role_selector`, `resolve_recipients`, `MessageStore`, `StoreError::{MessageNotFound, WireMessageTampered}`

- [ ] **Step 1: Add `Kind::Message` and `MessageId` to `crates/core/src/ids.rs`, and update `crates/github/src/meta.rs`**

Apply the following changes to `crates/core/src/ids.rs`:

```diff
--- a/crates/core/src/ids.rs
+++ b/crates/core/src/ids.rs
@@ -12,6 +12,7 @@ pub enum Kind {
     Finding,
     State,
     Agent,
+    Message,
 }
 
 crate::wire::wire_names!(Kind as kind_wire {
@@ -21,6 +22,7 @@ crate::wire::wire_names!(Kind as kind_wire {
     Finding => "finding",
     State => "state",
     Agent => "agent",
+    Message => "message",
 });
 crate::wire::wire_parse!(Kind as kind_parse);
 
@@ -61,6 +63,7 @@ id_type!(RecordId, Kind::Record);
 id_type!(FindingId, Kind::Finding);
 id_type!(StateId, Kind::State);
 id_type!(AgentId, Kind::Agent);
+id_type!(MessageId, Kind::Message);
```

And update the exhaustive `Kind` match in `crates/github/src/meta.rs`:

```diff
--- a/crates/github/src/meta.rs
+++ b/crates/github/src/meta.rs
@@ -50,7 +50,7 @@ impl ItemKind {
         match kind {
             Kind::Record => Some(ItemKind::Record),
             Kind::Finding => Some(ItemKind::Finding),
-            Kind::Project | Kind::Gate | Kind::State | Kind::Agent => None,
+            Kind::Project | Kind::Gate | Kind::State | Kind::Agent | Kind::Message => None,
         }
     }
```

- [ ] **Step 2: Add `AgentRecord::covers_area` in `crates/core/src/agent.rs` and wire error/cursor fields in `crates/core/src/state.rs`**

Apply to `crates/core/src/agent.rs`:

```diff
--- a/crates/core/src/agent.rs
+++ b/crates/core/src/agent.rs
@@ -156,6 +156,10 @@ impl AgentRecord {
     pub fn co_author_trailer(&self) -> String {
         format!("Co-authored-by: {} <{}>", self.display_name, self.email)
     }
+
+    pub fn covers_area(&self, slug: &str) -> bool {
+        self.project == slug || self.extra_areas.iter().any(|a| a == slug)
+    }
 }
```

Apply to `crates/core/src/state.rs`:

```diff
--- a/crates/core/src/state.rs
+++ b/crates/core/src/state.rs
@@ -43,6 +43,16 @@ pub enum StateError {
     UnexpectedSupersededBy(String),
     #[error("memory `{0}` cannot supersede itself")]
     SelfSupersede(StateId),
+    #[error(
+        "invalid wire address `{0}`: expected `<agent>`, `urn:uuid:...`, `@all`, `@area:<slug>`, `@role:<role>`, or `@area:<slug>/<role>`"
+    )]
+    InvalidAddress(String),
+    #[error("message subject must be a single non-empty line of at most 512 bytes")]
+    InvalidMessageSubject,
+    #[error("message body must be non-empty and at most 262144 bytes (got {0} bytes)")]
+    InvalidMessageBody(usize),
+    #[error("reply message requires both `reply_to` and `correlation_id`")]
+    ReplyRequiresParent,
 }
 
 /// Normalize to ASCII lowercase and validate `^[a-z][a-z0-9_-]{0,63}$`.
@@ -415,6 +425,10 @@ pub struct StateSyncCursor {
     pub last_pushed_commit: Option<String>,
     pub last_revision_seq: u64,
     pub last_registry_seq: u64,
+    #[serde(default)]
+    pub last_wire_msg_seq: u64,
+    #[serde(default)]
+    pub last_wire_rcpt_seq: u64,
 }
```

- [ ] **Step 3: Create `crates/core/src/ferrowire.rs`**

Write `crates/core/src/ferrowire.rs`:

```rust
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
    System,
}

crate::wire::wire_names!(MessageKind as message_kind_wire {
    Mail => "mail",
    Ask => "ask",
    Reply => "reply",
    Handoff => "handoff",
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
        let recipient_name = if !self.recipient_name.is_empty()
            && !self.recipient_name.starts_with("urn:")
        {
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

        assert_eq!(
            r_pending.merge(&r_delivered),
            r_delivered.merge(&r_pending)
        );
        let merged_ack_exp = r_expired.merge(&r_acked);
        assert_eq!(merged_ack_exp, r_acked.merge(&r_expired));
        assert_eq!(merged_ack_exp.status, DeliveryStatus::Acked);
        assert_eq!(merged_ack_exp.attempts, 5);
        assert_eq!(merged_ack_exp.updated_at, At::from_unix_millis(30));
        assert_eq!(merged_ack_exp.acked_at, Some(At::from_unix_millis(25)));
    }
}
```

- [ ] **Step 4: Add `MessageStore` and `StoreError` variants to `crates/core/src/store.rs`, export in `crates/core/src/lib.rs`, and add `wire_refs` test in `crates/core/tests/wire_refs.rs`**

Apply to `crates/core/src/store.rs`:

```diff
--- a/crates/core/src/store.rs
+++ b/crates/core/src/store.rs
@@ -3,8 +3,13 @@ use crate::at::At;
 use crate::decision::{Decision, Flushed};
 use crate::escalation::EscalationFault;
 use crate::fault::LedgerFault;
+use crate::ferrowire::{
+    DeliveryReceipt, DeliveryStatus, SendWireArgs, ThreadId, WireFilter, WireInboxItem, WireMessage,
+};
 use crate::finding::Finding;
-use crate::ids::{AgentId, FindingId, GateId, Kind, ProjectId, RecordId, RevisionId, StateId};
+use crate::ids::{
+    AgentId, FindingId, GateId, Kind, MessageId, ProjectId, RecordId, RevisionId, StateId,
+};
 use crate::iri::Iri;
 use crate::log::{Attempt, GateRun};
 use crate::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};
@@ -273,6 +278,13 @@ pub enum StoreError {
     SensitiveDisclosure(StateId),
     #[error("invalid state operation: {0}")]
     InvalidState(String),
+    #[error("no such wire message: {0}")]
+    MessageNotFound(MessageId),
+    #[error(
+        "wire message {id} is recorded differently on this machine and in the incoming batch: {detail}. \
+         A wire message never changes once sent."
+    )]
+    WireMessageTampered { id: MessageId, detail: String },
 }
 
 /// Follow a stored reference. `Ok(None)` from the owning store means the
@@ -600,7 +612,7 @@ impl KindRouted<'_> {
     fn owner(&self, kind: Kind) -> &dyn Handles {
         // Every kind by name: a new kind must be routed on purpose.
         match kind {
-            Kind::Project | Kind::Gate | Kind::State | Kind::Agent => self.catalog,
+            Kind::Project | Kind::Gate | Kind::State | Kind::Agent | Kind::Message => self.catalog,
             Kind::Record | Kind::Finding => self.tracker,
         }
     }
@@ -694,6 +706,67 @@ pub trait StateStore {
     fn set_sync_cursor(&self, cursor: StateSyncCursor) -> Result<(), StoreError>;
 }
 
+/// Durable agent-to-agent and human-to-agent messaging store (FerroWire spec §1.4).
+pub trait MessageStore {
+    /// Record a new message and create initial `Pending` receipts for all resolved recipients.
+    fn send_wire(
+        &self,
+        args: SendWireArgs,
+    ) -> Result<(WireMessage, Vec<DeliveryReceipt>), StoreError>;
+
+    /// Look up a single message by `MessageId`.
+    fn get_wire_message(&self, id: &MessageId) -> Result<Option<WireMessage>, StoreError>;
+
+    /// Query inbox items for a recipient (`name_key` or `AgentId` URN), ordered by `MessageId`.
+    fn list_wire_inbox(&self, filter: &WireFilter) -> Result<Vec<WireInboxItem>, StoreError>;
+
+    /// List all messages in a thread (`thread_id`) in chronological `MessageId` order.
+    fn list_wire_thread(&self, thread_id: &ThreadId) -> Result<Vec<WireMessage>, StoreError>;
+
+    /// List replies correlated to an `Ask` message (`correlation_id`).
+    fn list_wire_replies(&self, correlation_id: &MessageId) -> Result<Vec<WireMessage>, StoreError>;
+
+    /// List all receipts for a given `MessageId`.
+    fn list_wire_receipts(
+        &self,
+        message_id: &MessageId,
+    ) -> Result<Vec<DeliveryReceipt>, StoreError>;
+
+    /// Transition a recipient's receipt (`Delivered`, `Acked`, or failed attempt increment).
+    fn update_wire_receipt(
+        &self,
+        message_id: &MessageId,
+        recipient: &str,
+        status: DeliveryStatus,
+        increment_attempts: bool,
+        at: At,
+    ) -> Result<Option<DeliveryReceipt>, StoreError>;
+
+    /// Export messages after `after_seq` for peer catch-up and `fl/state` sync.
+    fn wire_messages_after(
+        &self,
+        after_seq: u64,
+    ) -> Result<Vec<(u64, WireMessage)>, StoreError>;
+
+    /// Export receipt events after `after_seq` for peer catch-up and `fl/state` sync.
+    fn wire_receipts_after(
+        &self,
+        after_seq: u64,
+    ) -> Result<Vec<(u64, DeliveryReceipt)>, StoreError>;
+
+    /// Import messages and receipts from a remote peer or `fl/state` branch, enforcing
+    /// byte-for-byte immutability on `WireMessage` and monotonic convergence on `DeliveryReceipt`.
+    fn import_wire_batch(
+        &self,
+        messages: &[WireMessage],
+        receipts: &[DeliveryReceipt],
+    ) -> Result<(), StoreError>;
+
+    /// Delete transient (`durable == false`) messages whose receipts are all `Acked` or
+    /// `Expired` and whose `created_at < before`. Returns the count of pruned messages.
+    fn prune_wire(&self, before: At) -> Result<usize, StoreError>;
+}
```

Apply to `crates/core/src/lib.rs`:

```diff
--- a/crates/core/src/lib.rs
+++ b/crates/core/src/lib.rs
@@ -11,6 +11,7 @@ pub mod decision;
 pub mod escalate;
 pub mod escalation;
 pub mod fault;
+pub mod ferrowire;
 pub mod finding;
 pub mod ids;
 pub mod iri;
@@ -37,8 +38,16 @@ pub use decision::{Decision, DecisionKind, Flushed, LeftLocal, Outcome, Transiti
 pub use escalate::Prepared;
 pub use escalation::{EscalationFault, Escalations, Mark, Outgoing, Provenance, Tombstone};
 pub use fault::LedgerFault;
+pub use ferrowire::{
+    DeliveryReceipt, DeliveryStatus, MAX_SUBJECT_BYTES, MAX_WIRE_BODY_BYTES, MessageKind,
+    MessageUrgency, SendWireArgs, ThreadId, WireAddress, WireFilter, WireInboxItem, WireMessage,
+    fallback_agent_iri, parse_role_selector, resolve_recipients, validate_sender_name,
+    validate_subject, validate_wire_body,
+};
 pub use finding::{Finding, FindingError, FindingState};
-pub use ids::{AgentId, FindingId, GateId, Kind, ProjectId, RecordId, RevisionId, StateId};
+pub use ids::{
+    AgentId, FindingId, GateId, Kind, MessageId, ProjectId, RecordId, RevisionId, StateId,
+};
 pub use iri::{Iri, IriError};
 pub use log::{Attempt, AttemptStatus, GateRun, PathsTouched};
 pub use mem::MemStore;
@@ -59,8 +68,8 @@ pub use state::{
     validate_summary,
 };
 pub use store::{
-    AgentRegistry, Bindings, Catalog, CatalogChecked, Handles, KindRouted, Ledger, Roles,
-    StateStore, StoreError, Tracker, as_clause, follow, ledger_root_shape, node_id_shape,
+    AgentRegistry, Bindings, Catalog, CatalogChecked, Handles, KindRouted, Ledger, MessageStore,
+    Roles, StateStore, StoreError, Tracker, as_clause, follow, ledger_root_shape, node_id_shape,
 };
 pub use tiered::{FindingPlacement, Placement, RecordSeen, TieredTracker};
 pub use verdict::{FailReason, Population, Verdict};
```

Apply to `crates/core/tests/wire_refs.rs`:

```diff
--- a/crates/core/tests/wire_refs.rs
+++ b/crates/core/tests/wire_refs.rs
@@ -231,4 +231,33 @@ fn every_reference_field_on_the_wire_is_a_full_iri() {
     assert_every_reference_refuses_a_handle("StateRevision", &state_revision, 4);
     assert_every_reference_refuses_a_handle("AgentRecord", &agent_record, 1);
     assert_every_reference_refuses_a_handle("RegistryEvent", &registry_event, 2);
+
+    let wire_message = WireMessage {
+        id: MessageId(iri(17)),
+        thread_id: MessageId(iri(18)),
+        reply_to: Some(MessageId(iri(19))),
+        correlation_id: Some(MessageId(iri(20))),
+        sender_id: Some(AgentId(iri(21))),
+        sender_name: "ferris".into(),
+        sender_host: Some("ai-lab-0".into()),
+        to: WireAddress::AgentId(AgentId(iri(22))),
+        kind: MessageKind::Reply,
+        urgency: MessageUrgency::Urgent,
+        durable: true,
+        subject: "Re: check".into(),
+        body: "All green".into(),
+        created_at: At::from_unix_millis(7),
+        deadline_at: Some(At::from_unix_millis(8)),
+    };
+    let delivery_receipt = DeliveryReceipt {
+        message_id: MessageId(iri(17)),
+        recipient_id: iri(22),
+        recipient_name: "jarvis".into(),
+        status: DeliveryStatus::Acked,
+        attempts: 1,
+        updated_at: At::from_unix_millis(8),
+        acked_at: Some(At::from_unix_millis(8)),
+    };
+    assert_every_reference_refuses_a_handle("WireMessage", &wire_message, 6);
+    assert_every_reference_refuses_a_handle("DeliveryReceipt", &delivery_receipt, 2);
 }
```

- [ ] **Step 5: Run `fl-core` unit and wire_refs tests**

Run:
```bash
cargo test -p fl-core --lib ferrowire --offline && cargo test -p fl-core --test wire_refs --offline
```
Expected: PASS (`wire_address_parses_and_round_trips_all_selector_variants`, `delivery_receipt_merge_is_commutative_and_monotonic`, `every_reference_field_on_the_wire_is_a_full_iri`).

---

## Task 2: `MemStore` Implementation and Shared `MessageStore` Conformance Suite

**Files:**
- Modify: `crates/core/src/mem.rs`
- Modify: `crates/core/src/conformance.rs`
- Modify: `crates/core/src/mem_issues.rs`
- Modify: `crates/core/src/tiered.rs`
- Modify: `crates/github/src/tracker.rs`
- Modify: `crates/github/src/ledger/fixture.rs`

**Interfaces:**
- Consumes: `fl_core::ferrowire::*`, `fl_core::store::MessageStore`
- Produces: `impl MessageStore for MemStore`, `fl_core::conformance::message_store` (7 conformance test cases), `Bound::wire`

- [ ] **Step 1: Implement `MessageStore` for `MemStore` in `crates/core/src/mem.rs`**

Apply the following diff to `crates/core/src/mem.rs`:

```diff
--- a/crates/core/src/mem.rs
+++ b/crates/core/src/mem.rs
@@ -4,9 +4,14 @@ use crate::agent::{
 };
 use crate::at::At;
 use crate::escalation::{EscalationFault, Escalations, Mark, Tombstone, escalate_command};
+use crate::ferrowire::{
+    DeliveryReceipt, DeliveryStatus, MessageKind, SendWireArgs, ThreadId, WireFilter,
+    WireInboxItem, WireMessage, resolve_recipients, validate_sender_name, validate_subject,
+    validate_wire_body,
+};
 use crate::finding::{Finding, FindingState};
 use crate::ids::{
-    AgentId, FindingId, GateId, Kind, ProjectId, RecordId, RevisionId, StateId, seq_iri,
+    AgentId, FindingId, GateId, Kind, MessageId, ProjectId, RecordId, RevisionId, StateId, seq_iri,
 };
 use crate::iri::Iri;
 use crate::log::{Attempt, GateRun};
@@ -15,11 +20,13 @@ use crate::routing::{ForeignRecord, Routes, RoutingMap, check_foreign_for_local}
 use crate::split::{CachedSegment, LedgerCache, Outbox, Pending};
 use crate::state::{
     MemoryLifecycle, MergeMode, SensitivityPatch, StateConcern, StateConflict, StateEntry,
-    StatePayload, StateRevision, StateScope, StateSyncCursor, UpsertStateArgs,
+    StateError, StatePayload, StateRevision, StateScope, StateSyncCursor, UpsertStateArgs,
     compute_append_merge, compute_last_writer_merge, is_ancestor_in_dag,
     lowest_common_ancestor_in_dag, sorted_parents, validate_slug,
 };
-use crate::store::{AgentRegistry, Catalog, Handles, Ledger, StateStore, StoreError, Tracker};
+use crate::store::{
+    AgentRegistry, Catalog, Handles, Ledger, MessageStore, StateStore, StoreError, Tracker,
+};
 use std::cell::RefCell;
 use std::collections::{BTreeMap, BTreeSet};
 
@@ -92,6 +99,16 @@ struct Inner {
     next_state_rev_seq: u64,
     state_conflicts: BTreeMap<StateId, StateConflict>,
     sync_cursors: BTreeMap<String, StateSyncCursor>,
+
+    wire_messages: BTreeMap<MessageId, WireMessage>,
+    wire_msg_log: BTreeMap<u64, MessageId>,
+    wire_msg_seq: BTreeMap<MessageId, u64>,
+    next_wire_msg_seq: u64,
+    wire_inbox: BTreeMap<(Iri, MessageId), DeliveryReceipt>,
+    wire_rcpt_log: BTreeMap<u64, DeliveryReceipt>,
+    next_wire_rcpt_seq: u64,
+    wire_threads: BTreeSet<(ThreadId, MessageId)>,
+    wire_replies: BTreeSet<(MessageId, MessageId)>,
 }
 
 impl Inner {
@@ -106,6 +123,13 @@ impl Inner {
         id
     }
 
+    fn mint_unhanded(&mut self, kind: Kind) -> Iri {
+        self.next_id += 1;
+        let id = seq_iri(self.next_id);
+        self.owned.insert(id.clone(), kind);
+        id
+    }
+
     fn mint_unindexed(&mut self) -> Iri {
         self.next_id += 1;
         seq_iri(self.next_id)
@@ -132,6 +156,80 @@ impl Inner {
         Ok(())
     }
 
+    /// Register an imported `Kind::Message` IRI in `owned` without allocating a numeric handle.
+    fn index_imported_unhanded(&mut self, kind: Kind, id: &Iri) -> Result<(), StoreError> {
+        let canonical = self.resolve(id);
+        if let Some(&existing) = self.owned.get(&canonical) {
+            if existing == kind {
+                return Ok(());
+            }
+            return Err(StoreError::WrongKind {
+                id: canonical,
+                expected: kind,
+                found: existing,
+            });
+        }
+        self.owned.insert(canonical, kind);
+        Ok(())
+    }
+
+    fn append_wire_message(&mut self, msg: WireMessage) {
+        if !self.wire_msg_seq.contains_key(&msg.id) {
+            self.next_wire_msg_seq += 1;
+            let seq = self.next_wire_msg_seq;
+            self.wire_msg_seq.insert(msg.id.clone(), seq);
+            self.wire_msg_log.insert(seq, msg.id.clone());
+        }
+        self.wire_threads
+            .insert((msg.thread_id.clone(), msg.id.clone()));
+        if let Some(ref corr) = msg.correlation_id {
+            self.wire_replies.insert((corr.clone(), msg.id.clone()));
+        }
+        self.wire_messages.insert(msg.id.clone(), msg);
+    }
+
+    fn append_wire_receipt(&mut self, rcpt: DeliveryReceipt) {
+        self.next_wire_rcpt_seq += 1;
+        let seq = self.next_wire_rcpt_seq;
+        self.wire_rcpt_log.insert(seq, rcpt);
+    }
+
+    fn recipient_matches(&self, rcpt: &DeliveryReceipt, query: &str) -> bool {
+        let trimmed = query.trim();
+        let (agent_id, name_key) = if let Ok(iri) = Iri::parse(trimmed) {
+            let resolved_iri = self.resolve(&iri);
+            if let Some(agent) = self.agents.get(&AgentId(resolved_iri.clone())) {
+                (Some(resolved_iri), Some(agent.name_key.clone()))
+            } else if let Some(rest) = trimmed.strip_prefix("urn:fl:agent:")
+                && let Ok(k) = validate_slug(rest)
+            {
+                let aid = self.agent_by_name.get(&k).map(|id| self.resolve(id.iri()));
+                (aid, Some(k))
+            } else {
+                (Some(resolved_iri), None)
+            }
+        } else if let Ok(k) = validate_slug(trimmed) {
+            let aid = self.agent_by_name.get(&k).map(|id| self.resolve(id.iri()));
+            (aid, Some(k))
+        } else {
+            (None, None)
+        };
+
+        let rcpt_resolved = self.resolve(&rcpt.recipient_id);
+        if let Some(ref aid) = agent_id
+            && &rcpt_resolved == aid
+        {
+            return true;
+        }
+        if let Some(ref nk) = name_key
+            && (&rcpt.recipient_name == nk
+                || rcpt.recipient_id.as_str() == format!("urn:fl:agent:{nk}"))
+        {
+            return true;
+        }
+        rcpt.recipient_id.as_str() == trimmed || rcpt.recipient_name.eq_ignore_ascii_case(trimmed)
+    }
+
     /// Re-point an unpublished local `old_id` to `new_id` of the same `kind`,
     /// preserving any existing numeric handle on `old_id` (spec §3.2, §4.3).
     fn alias_unpublished_id(
@@ -1661,6 +1759,383 @@ impl StateStore for MemStore {
     }
 }
 
+impl MessageStore for MemStore {
+    fn send_wire(
+        &self,
+        args: SendWireArgs,
+    ) -> Result<(WireMessage, Vec<DeliveryReceipt>), StoreError> {
+        let subject =
+            validate_subject(&args.subject).map_err(|e| StoreError::InvalidState(e.to_string()))?;
+        let body =
+            validate_wire_body(&args.body).map_err(|e| StoreError::InvalidState(e.to_string()))?;
+        let sender_name = validate_sender_name(&args.sender_name)
+            .map_err(|e| StoreError::InvalidState(e.to_string()))?;
+
+        let mut s = self.inner.borrow_mut();
+
+        let (parent_thread, correlation_id) = match &args.reply_to {
+            Some(parent_id) => {
+                let Some(parent) = s.wire_messages.get(parent_id).cloned() else {
+                    return Err(StoreError::MessageNotFound(parent_id.clone()));
+                };
+                let corr = if parent.kind == MessageKind::Ask {
+                    Some(parent.id.clone())
+                } else if parent.correlation_id.is_some() {
+                    parent.correlation_id.clone()
+                } else if args.kind == MessageKind::Reply {
+                    Some(parent.id.clone())
+                } else {
+                    None
+                };
+                (Some(parent.thread_id), corr)
+            }
+            None => {
+                if args.kind == MessageKind::Reply {
+                    return Err(StoreError::InvalidState(
+                        StateError::ReplyRequiresParent.to_string(),
+                    ));
+                }
+                (None, None)
+            }
+        };
+
+        let msg_id = match args.id {
+            Some(id) => {
+                s.index_imported_unhanded(Kind::Message, id.iri())?;
+                id
+            }
+            None => MessageId(s.mint_unhanded(Kind::Message)),
+        };
+
+        let thread_id = args
+            .thread_id
+            .or(parent_thread)
+            .unwrap_or_else(|| msg_id.clone());
+        let durable = args.durable || matches!(args.kind, MessageKind::Ask | MessageKind::Handoff);
+
+        let all_agents: Vec<AgentRecord> = s.agents.values().cloned().collect();
+        let sender_id = args.sender_id.or_else(|| {
+            all_agents
+                .iter()
+                .find(|a| a.status == AgentStatus::Active && a.name_key == sender_name)
+                .map(|a| a.id.clone())
+        });
+
+        let msg = WireMessage {
+            id: msg_id.clone(),
+            thread_id,
+            reply_to: args.reply_to,
+            correlation_id,
+            sender_id,
+            sender_name,
+            sender_host: args.sender_host,
+            to: args.to,
+            kind: args.kind,
+            urgency: args.urgency,
+            durable,
+            subject,
+            body,
+            created_at: args.at.clone(),
+            deadline_at: args.deadline_at,
+        };
+        msg.validate()
+            .map_err(|e| StoreError::InvalidState(e.to_string()))?;
+
+        let recipients = resolve_recipients(
+            &msg.to,
+            msg.sender_id.as_ref(),
+            &msg.sender_name,
+            &all_agents,
+        );
+
+        s.append_wire_message(msg.clone());
+
+        let mut receipts = Vec::with_capacity(recipients.len());
+        for (recipient_id, recipient_name) in recipients {
+            let rcpt = DeliveryReceipt {
+                message_id: msg.id.clone(),
+                recipient_id: recipient_id.clone(),
+                recipient_name,
+                status: DeliveryStatus::Pending,
+                attempts: 0,
+                updated_at: args.at.clone(),
+                acked_at: None,
+            };
+            s.wire_inbox
+                .insert((recipient_id, msg.id.clone()), rcpt.clone());
+            s.append_wire_receipt(rcpt.clone());
+            receipts.push(rcpt);
+        }
+
+        Ok((msg, receipts))
+    }
+
+    fn get_wire_message(&self, id: &MessageId) -> Result<Option<WireMessage>, StoreError> {
+        let s = self.inner.borrow();
+        Ok(s.wire_messages.get(id).cloned())
+    }
+
+    fn list_wire_inbox(&self, filter: &WireFilter) -> Result<Vec<WireInboxItem>, StoreError> {
+        let s = self.inner.borrow();
+        let mut items = Vec::new();
+        for rcpt in s.wire_inbox.values() {
+            if let Some(ref rec_query) = filter.recipient
+                && !s.recipient_matches(rcpt, rec_query)
+            {
+                continue;
+            }
+            if filter.unread_only
+                && matches!(
+                    rcpt.status,
+                    DeliveryStatus::Acked | DeliveryStatus::Expired
+                )
+            {
+                continue;
+            }
+            let Some(msg) = s.wire_messages.get(&rcpt.message_id).cloned() else {
+                continue;
+            };
+            if let Some(ref tid) = filter.thread_id
+                && &msg.thread_id != tid
+            {
+                continue;
+            }
+            if let Some(ref cid) = filter.correlation_id
+                && msg.correlation_id.as_ref() != Some(cid)
+            {
+                continue;
+            }
+            items.push(WireInboxItem {
+                message: msg,
+                receipt: rcpt.clone(),
+            });
+        }
+        items.sort_by(|a, b| {
+            (&a.message.id, &a.receipt.recipient_id)
+                .cmp(&(&b.message.id, &b.receipt.recipient_id))
+        });
+        if let Some(limit) = filter.limit
+            && items.len() > limit
+        {
+            items.truncate(limit);
+        }
+        Ok(items)
+    }
+
+    fn list_wire_thread(&self, thread_id: &ThreadId) -> Result<Vec<WireMessage>, StoreError> {
+        let s = self.inner.borrow();
+        let mut out = Vec::new();
+        for (tid, mid) in &s.wire_threads {
+            if tid == thread_id
+                && let Some(msg) = s.wire_messages.get(mid)
+            {
+                out.push(msg.clone());
+            }
+        }
+        out.sort_by(|a, b| a.id.cmp(&b.id));
+        Ok(out)
+    }
+
+    fn list_wire_replies(&self, correlation_id: &MessageId) -> Result<Vec<WireMessage>, StoreError> {
+        let s = self.inner.borrow();
+        let mut out = Vec::new();
+        for (cid, mid) in &s.wire_replies {
+            if cid == correlation_id
+                && let Some(msg) = s.wire_messages.get(mid)
+            {
+                out.push(msg.clone());
+            }
+        }
+        out.sort_by(|a, b| a.id.cmp(&b.id));
+        Ok(out)
+    }
+
+    fn list_wire_receipts(
+        &self,
+        message_id: &MessageId,
+    ) -> Result<Vec<DeliveryReceipt>, StoreError> {
+        let s = self.inner.borrow();
+        if !s.wire_messages.contains_key(message_id) {
+            return Err(StoreError::MessageNotFound(message_id.clone()));
+        }
+        let mut out: Vec<DeliveryReceipt> = s
+            .wire_inbox
+            .values()
+            .filter(|r| &r.message_id == message_id)
+            .cloned()
+            .collect();
+        out.sort_by(|a, b| a.recipient_id.cmp(&b.recipient_id));
+        Ok(out)
+    }
+
+    fn update_wire_receipt(
+        &self,
+        message_id: &MessageId,
+        recipient: &str,
+        status: DeliveryStatus,
+        increment_attempts: bool,
+        at: At,
+    ) -> Result<Option<DeliveryReceipt>, StoreError> {
+        let mut s = self.inner.borrow_mut();
+        if !s.wire_messages.contains_key(message_id) {
+            return Err(StoreError::MessageNotFound(message_id.clone()));
+        }
+        let matching_key = s
+            .wire_inbox
+            .iter()
+            .find(|((_, mid), rcpt)| mid == message_id && s.recipient_matches(rcpt, recipient))
+            .map(|(k, _)| k.clone());
+        let Some(key) = matching_key else {
+            return Ok(None);
+        };
+        let existing = s.wire_inbox.get(&key).cloned().unwrap();
+        let attempts = if increment_attempts {
+            existing.attempts.saturating_add(1)
+        } else {
+            existing.attempts
+        };
+        let incoming = DeliveryReceipt {
+            message_id: existing.message_id.clone(),
+            recipient_id: existing.recipient_id.clone(),
+            recipient_name: existing.recipient_name.clone(),
+            status,
+            attempts,
+            updated_at: at.clone(),
+            acked_at: if status == DeliveryStatus::Acked {
+                Some(at)
+            } else {
+                None
+            },
+        };
+        let merged = existing.merge(&incoming);
+        s.wire_inbox.insert(key, merged.clone());
+        s.append_wire_receipt(merged.clone());
+        Ok(Some(merged))
+    }
+
+    fn wire_messages_after(
+        &self,
+        after_seq: u64,
+    ) -> Result<Vec<(u64, WireMessage)>, StoreError> {
+        let s = self.inner.borrow();
+        let mut out = Vec::new();
+        for (&seq, mid) in s.wire_msg_log.range((after_seq + 1)..) {
+            if let Some(msg) = s.wire_messages.get(mid) {
+                out.push((seq, msg.clone()));
+            }
+        }
+        Ok(out)
+    }
+
+    fn wire_receipts_after(
+        &self,
+        after_seq: u64,
+    ) -> Result<Vec<(u64, DeliveryReceipt)>, StoreError> {
+        let s = self.inner.borrow();
+        Ok(s.wire_rcpt_log
+            .range((after_seq + 1)..)
+            .filter(|(_, rcpt)| s.wire_messages.contains_key(&rcpt.message_id))
+            .map(|(&seq, rcpt)| (seq, rcpt.clone()))
+            .collect())
+    }
+
+    fn import_wire_batch(
+        &self,
+        messages: &[WireMessage],
+        receipts: &[DeliveryReceipt],
+    ) -> Result<(), StoreError> {
+        let mut s = self.inner.borrow_mut();
+        let all_agents: Vec<AgentRecord> = s.agents.values().cloned().collect();
+
+        for msg in messages {
+            msg.validate()
+                .map_err(|e| StoreError::InvalidState(e.to_string()))?;
+            if let Some(existing) = s.wire_messages.get(&msg.id) {
+                if existing != msg {
+                    return Err(StoreError::WireMessageTampered {
+                        id: msg.id.clone(),
+                        detail: "imported wire message differs from stored copy".into(),
+                    });
+                }
+                continue;
+            }
+            s.index_imported_unhanded(Kind::Message, msg.id.iri())?;
+            s.append_wire_message(msg.clone());
+
+            let resolved = resolve_recipients(
+                &msg.to,
+                msg.sender_id.as_ref(),
+                &msg.sender_name,
+                &all_agents,
+            );
+            for (rec_id, rec_name) in resolved {
+                let in_incoming = receipts.iter().any(|r| {
+                    r.message_id == msg.id
+                        && (r.recipient_id == rec_id || r.recipient_name == rec_name)
+                });
+                if !in_incoming && !s.wire_inbox.contains_key(&(rec_id.clone(), msg.id.clone())) {
+                    let rcpt = DeliveryReceipt {
+                        message_id: msg.id.clone(),
+                        recipient_id: rec_id.clone(),
+                        recipient_name: rec_name,
+                        status: DeliveryStatus::Pending,
+                        attempts: 0,
+                        updated_at: msg.created_at.clone(),
+                        acked_at: None,
+                    };
+                    s.wire_inbox.insert((rec_id, msg.id.clone()), rcpt.clone());
+                    s.append_wire_receipt(rcpt);
+                }
+            }
+        }
+
+        for rcpt in receipts {
+            if !s.wire_messages.contains_key(&rcpt.message_id) {
+                return Err(StoreError::MessageNotFound(rcpt.message_id.clone()));
+            }
+            let key = (rcpt.recipient_id.clone(), rcpt.message_id.clone());
+            match s.wire_inbox.get(&key).cloned() {
+                Some(existing) => {
+                    let merged = existing.merge(rcpt);
+                    if merged != existing {
+                        s.wire_inbox.insert(key, merged.clone());
+                        s.append_wire_receipt(merged);
+                    }
+                }
+                None => {
+                    s.wire_inbox.insert(key, rcpt.clone());
+                    s.append_wire_receipt(rcpt.clone());
+                }
+            }
+        }
+        Ok(())
+    }
+
+    fn prune_wire(&self, before: At) -> Result<usize, StoreError> {
+        let mut s = self.inner.borrow_mut();
+        let to_prune: Vec<MessageId> = s
+            .wire_messages
+            .values()
+            .filter(|msg| !msg.durable && msg.created_at < before)
+            .filter(|msg| {
+                s.wire_inbox
+                    .values()
+                    .filter(|r| r.message_id == msg.id)
+                    .all(|r| matches!(r.status, DeliveryStatus::Acked | DeliveryStatus::Expired))
+            })
+            .map(|msg| msg.id.clone())
+            .collect();
+
+        let count = to_prune.len();
+        for mid in to_prune {
+            if let Some(msg) = s.wire_messages.remove(&mid) {
+                s.owned.remove(msg.id.iri());
+                s.wire_threads.remove(&(msg.thread_id, msg.id.clone()));
+                if let Some(corr) = msg.correlation_id {
+                    s.wire_replies.remove(&(corr, msg.id.clone()));
+                }
+                if let Some(seq) = s.wire_msg_seq.remove(&msg.id) {
+                    s.wire_msg_log.remove(&seq);
+                }
+                s.wire_inbox.retain(|(_, m), _| m != &mid);
+                s.wire_rcpt_log.retain(|_, r| r.message_id != mid);
+            }
+        }
+        Ok(count)
+    }
+}
+
 #[cfg(test)]
 mod tests {
     use super::*;
@@ -1676,6 +2151,7 @@ mod tests {
         crate::conformance::ledger_cache(|| (MemStore::default(), ()));
         crate::conformance::agent_registry(|| (MemStore::default(), ()));
         crate::conformance::state_store(|| (MemStore::default(), ()));
+        crate::conformance::message_store(|| (MemStore::default(), ()));
     }
```

- [ ] **Step 2: Add `Bound::wire` and `conformance::message_store` in `crates/core/src/conformance.rs` and update test fixtures**

Apply to `crates/core/src/conformance.rs`:

```diff
--- a/crates/core/src/conformance.rs
+++ b/crates/core/src/conformance.rs
@@ -23,7 +23,9 @@ use crate::model::{
     CommandSpec, GateDef, GateKind, PopulationDelivery, Regret, Selector, State, Transition,
 };
 use crate::split::{Batch, CachedSegment, LedgerCache, Outbox, RemoteLedger, SplitLedger};
-use crate::store::{AgentRegistry, Catalog, Handles, Ledger, StateStore, StoreError, Tracker};
+use crate::store::{
+    AgentRegistry, Catalog, Handles, Ledger, MessageStore, StateStore, StoreError, Tracker,
+};
 use crate::verdict::Verdict;
 use std::cell::RefCell;
 use std::collections::BTreeSet;
@@ -62,6 +64,7 @@ pub struct Bound<'a> {
     pub handles: &'a dyn Handles,
     pub state: &'a dyn StateStore,
     pub agents: &'a dyn AgentRegistry,
+    pub wire: &'a dyn MessageStore,
 }
 
 /// Hands one case its `Bound`. A callback rather than a return value, so a
@@ -74,7 +77,7 @@ pub trait Fixture {
 /// One store backing every role, plus a guard to keep alive.
 pub struct Single<S, G>(pub S, pub G);
 
-impl<S: Catalog + Tracker + Ledger + Handles + StateStore + AgentRegistry, G> Fixture
+impl<S: Catalog + Tracker + Ledger + Handles + StateStore + AgentRegistry + MessageStore, G> Fixture
     for Single<S, G>
 {
     fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
@@ -85,6 +88,7 @@ impl<S: Catalog + Tracker + Ledger + Handles + StateStore + AgentRegistry, G> Fi
             handles: &self.0,
             state: &self.0,
             agents: &self.0,
+            wire: &self.0,
         });
     }
 }
@@ -100,8 +104,10 @@ pub trait SplitFixture {
 /// fixture swaps `MemRemote` for `GithubLedger` over the fake GitHub.
 pub struct SplitOver<S, G>(pub S, pub G);
 
-impl<S: Catalog + Tracker + Ledger + Handles + Outbox + StateStore + AgentRegistry, G> SplitFixture
-    for SplitOver<S, G>
+impl<
+    S: Catalog + Tracker + Ledger + Handles + Outbox + StateStore + AgentRegistry + MessageStore,
+    G,
+> SplitFixture for SplitOver<S, G>
 {
     fn with_split(&self, f: &mut dyn FnMut(&Bound<'_>, &dyn RemoteControl)) {
         self.0
@@ -120,14 +126,17 @@ impl<S: Catalog + Tracker + Ledger + Handles + Outbox + StateStore + AgentRegist
                 handles: &self.0,
                 state: &self.0,
                 agents: &self.0,
+                wire: &self.0,
             },
             &remote,
         );
     }
 }
 
-impl<S: Catalog + Tracker + Ledger + Handles + Outbox + StateStore + AgentRegistry, G> Fixture
-    for SplitOver<S, G>
+impl<
+    S: Catalog + Tracker + Ledger + Handles + Outbox + StateStore + AgentRegistry + MessageStore,
+    G,
+> Fixture for SplitOver<S, G>
 {
     fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
         self.with_split(&mut |b, _| f(b));
@@ -1687,6 +1696,26 @@ fn an_id_has_no_handle_under_any_kind_but_its_own(roles: &Bound<'_>) {
             by: "o".into(),
         })
         .unwrap();
+    let msg = roles
+        .wire
+        .send_wire(crate::ferrowire::SendWireArgs {
+            id: None,
+            sender_id: Some(ag.id.clone()),
+            sender_name: "ferris".into(),
+            sender_host: None,
+            to: crate::ferrowire::WireAddress::All,
+            kind: crate::ferrowire::MessageKind::Mail,
+            urgency: crate::ferrowire::MessageUrgency::Normal,
+            durable: false,
+            subject: "ping".into(),
+            body: "pong".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(1),
+        })
+        .unwrap()
+        .0;
     let held = [
         (Kind::Project, p.0),
         (Kind::Gate, g.0),
@@ -1694,6 +1723,7 @@ fn an_id_has_no_handle_under_any_kind_but_its_own(roles: &Bound<'_>) {
         (Kind::Finding, f.0),
         (Kind::State, st.id.0),
         (Kind::Agent, ag.id.0),
+        (Kind::Message, msg.id.0),
     ];
     for own in Kind::ALL {
         assert!(
@@ -1703,16 +1733,24 @@ fn an_id_has_no_handle_under_any_kind_but_its_own(roles: &Bound<'_>) {
         );
     }
     for (own, id) in &held {
-        let h = roles
-            .handles
-            .handle_of(*own, id)
-            .unwrap()
-            .unwrap_or_else(|| panic!("{id} has no handle as its own kind"));
-        assert_eq!(
-            roles.handles.resolve_handle(*own, h).unwrap().as_ref(),
-            Some(id),
-            "{id}'s handle resolves back to it"
-        );
+        if *own == Kind::Message {
+            assert_eq!(
+                roles.handles.handle_of(*own, id).unwrap(),
+                None,
+                "messages do not consume sequential numeric handles"
+            );
+        } else {
+            let h = roles
+                .handles
+                .handle_of(*own, id)
+                .unwrap()
+                .unwrap_or_else(|| panic!("{id} has no handle as its own kind"));
+            assert_eq!(
+                roles.handles.resolve_handle(*own, h).unwrap().as_ref(),
+                Some(id),
+                "{id}'s handle resolves back to it"
+            );
+        }
         for other in Kind::ALL.iter().filter(|k| *k != own) {
             assert_eq!(
                 roles.handles.handle_of(*other, id).unwrap(),
@@ -2452,6 +2490,8 @@ impl RemoteControl for MemRemote {
 const AGENT_REGISTRY_CASES: usize = 7;
 /// How many cases [`state_store`] runs. Update deliberately — see [`run_suite`].
 const STATE_STORE_CASES: usize = 10;
+/// How many cases [`message_store`] runs. Update deliberately — see [`run_suite`].
+const MESSAGE_STORE_CASES: usize = 7;
 
 pub fn agent_registry<S: AgentRegistry + Catalog + Handles, G>(make: impl Fn() -> (S, G)) {
     let cases: &[fn(&S)] = &[
@@ -2482,6 +2522,21 @@ pub fn state_store<S: StateStore + Catalog + Handles, G>(make: impl Fn() -> (S,
     run_suite("state-store", STATE_STORE_CASES, cases, make);
 }
 
+pub fn message_store<S: MessageStore + AgentRegistry + Catalog + Handles, G>(
+    make: impl Fn() -> (S, G),
+) {
+    let cases: &[fn(&S)] = &[
+        direct_and_unregistered_persona_addressing_round_trips_across_registration::<S>,
+        group_selectors_expand_active_agents_and_exclude_sender::<S>,
+        ask_reply_and_thread_indexing_validate_and_correlate::<S>,
+        receipt_transitions_and_batch_imports_converge_monotonically::<S>,
+        wire_import_deduplicates_and_refuses_tampered_message::<S>,
+        offline_agent_id_collision_alias_preserves_wire_inbox_and_ack::<S>,
+        prune_wire_removes_only_acked_or_expired_transient_messages_before_cutoff::<S>,
+    ];
+    run_suite("message-store", MESSAGE_STORE_CASES, cases, make);
+}
+
 fn area_round_trips_and_normalizes_key<S: AgentRegistry>(s: &S) {
     let a = s
         .upsert_area(crate::agent::AreaRecord {
@@ -3405,3 +3460,612 @@ fn offline_same_key_collision_aliases_unpublished_local_id_to_remote<S: StateSto
         Some(remote_id.0)
     );
 }
+
+fn direct_and_unregistered_persona_addressing_round_trips_across_registration<
+    S: MessageStore + AgentRegistry + Catalog + Handles,
+>(
+    s: &S,
+) {
+    use crate::ferrowire::{
+        DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress, WireFilter,
+        fallback_agent_iri,
+    };
+
+    // 1. Send to an unregistered persona "ferris" -> uses fallback urn:fl:agent:ferris.
+    let (msg1, rcpts1) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: None,
+            sender_name: "jarvis".into(),
+            sender_host: Some("ai-lab-0".into()),
+            to: WireAddress::AgentName("ferris".into()),
+            kind: MessageKind::Mail,
+            urgency: MessageUrgency::Normal,
+            durable: false,
+            subject: "Welcome".into(),
+            body: "Cold roster mail".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(10),
+        })
+        .unwrap();
+    assert_eq!(s.kind_of(msg1.id.iri()).unwrap(), Kind::Message);
+    assert_eq!(s.handle_of(Kind::Message, msg1.id.iri()).unwrap(), None);
+    assert_eq!(rcpts1.len(), 1);
+    assert_eq!(rcpts1[0].recipient_id, fallback_agent_iri("ferris"));
+    assert_eq!(rcpts1[0].recipient_name, "ferris");
+
+    // 2. Register Ferris in the AgentRegistry.
+    let ferris = s
+        .register_agent(crate::agent::RegisterAgentArgs {
+            display_name: "Ferris".into(),
+            email: None,
+            project: "ferroloop".into(),
+            role: crate::agent::AgentRole::Lead,
+            extra_areas: vec![],
+            create_area: true,
+            replace: false,
+            at: At::from_unix_millis(15),
+            by: "owner".into(),
+        })
+        .unwrap();
+
+    // 3. Send a second message to Ferris now that Ferris is registered.
+    let (msg2, rcpts2) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: None,
+            sender_name: "jarvis".into(),
+            sender_host: None,
+            to: WireAddress::AgentName("ferris".into()),
+            kind: MessageKind::Mail,
+            urgency: MessageUrgency::Urgent,
+            durable: false,
+            subject: "Registered mail".into(),
+            body: "Warm roster mail".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(20),
+        })
+        .unwrap();
+    assert_eq!(rcpts2[0].recipient_id, ferris.id.0);
+
+    // 4. Querying inbox by name ("Ferris") or by AgentId URN returns BOTH messages.
+    let by_name = s
+        .list_wire_inbox(&WireFilter {
+            recipient: Some("Ferris".into()),
+            unread_only: true,
+            ..WireFilter::default()
+        })
+        .unwrap();
+    assert_eq!(by_name.len(), 2);
+    assert_eq!(by_name[0].message.id, msg1.id);
+    assert_eq!(by_name[1].message.id, msg2.id);
+
+    let by_id = s
+        .list_wire_inbox(&WireFilter {
+            recipient: Some(ferris.id.to_string()),
+            unread_only: true,
+            ..WireFilter::default()
+        })
+        .unwrap();
+    assert_eq!(by_id.len(), 2);
+
+    // 5. Acking msg1 by Ferris's AgentId URN transitions the pre-registration receipt.
+    let acked = s
+        .update_wire_receipt(
+            &msg1.id,
+            ferris.id.as_str(),
+            DeliveryStatus::Acked,
+            false,
+            At::from_unix_millis(25),
+        )
+        .unwrap()
+        .unwrap();
+    assert_eq!(acked.status, DeliveryStatus::Acked);
+    assert_eq!(acked.acked_at, Some(At::from_unix_millis(25)));
+}
+
+fn group_selectors_expand_active_agents_and_exclude_sender<
+    S: MessageStore + AgentRegistry + Catalog + Handles,
+>(
+    s: &S,
+) {
+    use crate::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
+
+    let jarvis = s
+        .register_agent(crate::agent::RegisterAgentArgs {
+            display_name: "Jarvis".into(),
+            email: None,
+            project: "ai-lab-amd".into(),
+            role: crate::agent::AgentRole::LabManager,
+            extra_areas: vec!["ferroloop".into()],
+            create_area: true,
+            replace: false,
+            at: At::from_unix_millis(10),
+            by: "owner".into(),
+        })
+        .unwrap();
+    let ferris = s
+        .register_agent(crate::agent::RegisterAgentArgs {
+            display_name: "Ferris".into(),
+            email: None,
+            project: "ferroloop".into(),
+            role: crate::agent::AgentRole::Lead,
+            extra_areas: vec![],
+            create_area: true,
+            replace: false,
+            at: At::from_unix_millis(11),
+            by: "owner".into(),
+        })
+        .unwrap();
+    let cogs = s
+        .register_agent(crate::agent::RegisterAgentArgs {
+            display_name: "Cogs".into(),
+            email: None,
+            project: "ferroloop".into(),
+            role: crate::agent::AgentRole::Assistant,
+            extra_areas: vec![],
+            create_area: false,
+            replace: false,
+            at: At::from_unix_millis(12),
+            by: "ferris".into(),
+        })
+        .unwrap();
+    let retired = s
+        .register_agent(crate::agent::RegisterAgentArgs {
+            display_name: "OldBot".into(),
+            email: None,
+            project: "ferroloop".into(),
+            role: crate::agent::AgentRole::Assistant,
+            extra_areas: vec![],
+            create_area: false,
+            replace: false,
+            at: At::from_unix_millis(13),
+            by: "ferris".into(),
+        })
+        .unwrap();
+    s.retire_agent(&retired.id, At::from_unix_millis(14), "ferris")
+        .unwrap();
+
+    // @all from ferris -> jarvis and cogs (excludes sender ferris and retired oldbot).
+    let (_, all_rcpts) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: Some(ferris.id.clone()),
+            sender_name: "ferris".into(),
+            sender_host: None,
+            to: WireAddress::All,
+            kind: MessageKind::Mail,
+            urgency: MessageUrgency::Normal,
+            durable: false,
+            subject: "Broadcast".into(),
+            body: "Hello all".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(20),
+        })
+        .unwrap();
+    let names: Vec<_> = all_rcpts
+        .iter()
+        .map(|r| r.recipient_name.as_str())
+        .collect();
+    assert_eq!(names, vec!["cogs", "jarvis"]);
+
+    // @area:ferroloop from jarvis -> cogs and ferris.
+    let (_, area_rcpts) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: Some(jarvis.id.clone()),
+            sender_name: "jarvis".into(),
+            sender_host: None,
+            to: WireAddress::Area("ferroloop".into()),
+            kind: MessageKind::Mail,
+            urgency: MessageUrgency::Normal,
+            durable: false,
+            subject: "Area".into(),
+            body: "FerroLoop team".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(21),
+        })
+        .unwrap();
+    let area_names: Vec<_> = area_rcpts
+        .iter()
+        .map(|r| r.recipient_name.as_str())
+        .collect();
+    assert_eq!(area_names, vec!["cogs", "ferris"]);
+
+    // @area:ferroloop/lead from cogs -> ferris only.
+    let (_, role_rcpts) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: Some(cogs.id),
+            sender_name: "cogs".into(),
+            sender_host: None,
+            to: WireAddress::AreaRole {
+                area: "ferroloop".into(),
+                role: crate::agent::AgentRole::Lead,
+            },
+            kind: MessageKind::Mail,
+            urgency: MessageUrgency::Normal,
+            durable: false,
+            subject: "Lead only".into(),
+            body: "PR ready".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(22),
+        })
+        .unwrap();
+    assert_eq!(role_rcpts.len(), 1);
+    assert_eq!(role_rcpts[0].recipient_name, "ferris");
+}
+
+fn ask_reply_and_thread_indexing_validate_and_correlate<S: MessageStore>(s: &S) {
+    use crate::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
+
+    // Reply without parent is refused.
+    let err = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: None,
+            sender_name: "ferris".into(),
+            sender_host: None,
+            to: WireAddress::AgentName("jarvis".into()),
+            kind: MessageKind::Reply,
+            urgency: MessageUrgency::Normal,
+            durable: false,
+            subject: "Re: orphan".into(),
+            body: "No parent".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(10),
+        })
+        .unwrap_err();
+    assert!(matches!(err, StoreError::InvalidState(_)));
+
+    // Ask forces durable = true and sets thread_id == id.
+    let (ask, _) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: None,
+            sender_name: "ferris".into(),
+            sender_host: None,
+            to: WireAddress::AgentName("jarvis".into()),
+            kind: MessageKind::Ask,
+            urgency: MessageUrgency::Urgent,
+            durable: false,
+            subject: "GPU lease?".into(),
+            body: "May we run benchmarks?".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: Some(At::from_unix_millis(60_000)),
+            at: At::from_unix_millis(11),
+        })
+        .unwrap();
+    assert!(ask.durable);
+    assert_eq!(ask.thread_id, ask.id);
+
+    // Reply inherits thread_id and sets correlation_id = Some(ask.id).
+    let (reply, _) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: None,
+            sender_name: "jarvis".into(),
+            sender_host: None,
+            to: WireAddress::AgentName("ferris".into()),
+            kind: MessageKind::Reply,
+            urgency: MessageUrgency::Normal,
+            durable: false,
+            subject: "Re: GPU lease?".into(),
+            body: "Approved".into(),
+            reply_to: Some(ask.id.clone()),
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(12),
+        })
+        .unwrap();
+    assert_eq!(reply.thread_id, ask.id);
+    assert_eq!(reply.correlation_id, Some(ask.id.clone()));
+
+    let thread = s.list_wire_thread(&ask.thread_id).unwrap();
+    assert_eq!(thread, vec![ask.clone(), reply.clone()]);
+
+    let replies = s.list_wire_replies(&ask.id).unwrap();
+    assert_eq!(replies, vec![reply]);
+}
+
+fn receipt_transitions_and_batch_imports_converge_monotonically<S: MessageStore>(s: &S) {
+    use crate::ferrowire::{
+        DeliveryReceipt, DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress,
+    };
+
+    let (msg, rcpts) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: None,
+            sender_name: "jarvis".into(),
+            sender_host: None,
+            to: WireAddress::AgentName("ferris".into()),
+            kind: MessageKind::Mail,
+            urgency: MessageUrgency::Normal,
+            durable: true,
+            subject: "Status".into(),
+            body: "Check in".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(10),
+        })
+        .unwrap();
+    let r0 = &rcpts[0];
+    assert_eq!(r0.status, DeliveryStatus::Pending);
+    assert_eq!(r0.attempts, 0);
+
+    // Failed push increments attempts while staying Pending.
+    let r1 = s
+        .update_wire_receipt(
+            &msg.id,
+            "ferris",
+            DeliveryStatus::Pending,
+            true,
+            At::from_unix_millis(15),
+        )
+        .unwrap()
+        .unwrap();
+    assert_eq!(r1.status, DeliveryStatus::Pending);
+    assert_eq!(r1.attempts, 1);
+
+    // Ack transitions to Acked.
+    let r2 = s
+        .update_wire_receipt(
+            &msg.id,
+            "ferris",
+            DeliveryStatus::Acked,
+            false,
+            At::from_unix_millis(20),
+        )
+        .unwrap()
+        .unwrap();
+    assert_eq!(r2.status, DeliveryStatus::Acked);
+    assert_eq!(r2.acked_at, Some(At::from_unix_millis(20)));
+
+    // Out-of-order Delivered receipt with higher attempts merges without downgrading Acked.
+    let late_delivered = DeliveryReceipt {
+        message_id: msg.id.clone(),
+        recipient_id: r0.recipient_id.clone(),
+        recipient_name: "ferris".into(),
+        status: DeliveryStatus::Delivered,
+        attempts: 4,
+        updated_at: At::from_unix_millis(25),
+        acked_at: None,
+    };
+    s.import_wire_batch(&[], &[late_delivered]).unwrap();
+    let final_rcpts = s.list_wire_receipts(&msg.id).unwrap();
+    assert_eq!(final_rcpts.len(), 1);
+    assert_eq!(final_rcpts[0].status, DeliveryStatus::Acked);
+    assert_eq!(final_rcpts[0].attempts, 4);
+    assert_eq!(final_rcpts[0].updated_at, At::from_unix_millis(25));
+    assert_eq!(final_rcpts[0].acked_at, Some(At::from_unix_millis(20)));
+}
+
+fn wire_import_deduplicates_and_refuses_tampered_message<S: MessageStore>(s: &S) {
+    use crate::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
+
+    let (msg, rcpts) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: None,
+            sender_name: "jarvis".into(),
+            sender_host: None,
+            to: WireAddress::AgentName("ferris".into()),
+            kind: MessageKind::Handoff,
+            urgency: MessageUrgency::Normal,
+            durable: false,
+            subject: "Handoff".into(),
+            body: "Original body".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(10),
+        })
+        .unwrap();
+    assert!(msg.durable, "Handoff forces durable = true");
+
+    let msg_log_len = s.wire_messages_after(0).unwrap().len();
+    let rcpt_log_len = s.wire_receipts_after(0).unwrap().len();
+
+    // Re-importing identical message and receipt is a no-op.
+    s.import_wire_batch(std::slice::from_ref(&msg), &rcpts)
+        .unwrap();
+    assert_eq!(s.wire_messages_after(0).unwrap().len(), msg_log_len);
+    assert_eq!(s.wire_receipts_after(0).unwrap().len(), rcpt_log_len);
+
+    // Tampering with an existing message ID is refused.
+    let mut tampered = msg.clone();
+    tampered.body = "Tampered body".into();
+    let err = s.import_wire_batch(&[tampered], &[]).unwrap_err();
+    assert!(matches!(err, StoreError::WireMessageTampered { .. }));
+}
+
+fn offline_agent_id_collision_alias_preserves_wire_inbox_and_ack<
+    S: MessageStore + AgentRegistry + Handles,
+>(
+    s: &S,
+) {
+    use crate::ferrowire::{
+        DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress, WireFilter,
+    };
+
+    let local_ferris = s
+        .register_agent(crate::agent::RegisterAgentArgs {
+            display_name: "Ferris".into(),
+            email: None,
+            project: "ferroloop".into(),
+            role: crate::agent::AgentRole::Lead,
+            extra_areas: vec![],
+            create_area: true,
+            replace: false,
+            at: At::from_unix_millis(10),
+            by: "owner".into(),
+        })
+        .unwrap();
+
+    // Send a message to Ferris while Ferris has `local_ferris.id`.
+    let (msg, _) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: None,
+            sender_name: "jarvis".into(),
+            sender_host: None,
+            to: WireAddress::AgentName("ferris".into()),
+            kind: MessageKind::Mail,
+            urgency: MessageUrgency::Normal,
+            durable: true,
+            subject: "Before alias".into(),
+            body: "Sent to local AgentId".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(12),
+        })
+        .unwrap();
+
+    // Import a remote RegistryEvent that aliases `local_ferris.id` -> `remote_id`.
+    let remote_id = crate::ids::AgentId(crate::ids::seq_iri(999));
+    let mut remote_rec = local_ferris.clone();
+    remote_rec.id = remote_id.clone();
+    let ev = crate::agent::RegistryEvent {
+        event_id: crate::ids::seq_iri(998),
+        at: At::from_unix_millis(20),
+        by: "owner".into(),
+        kind: crate::agent::RegistryEventKind::AgentUpsert(remote_rec),
+    };
+    s.import_registry_events(&[ev]).unwrap();
+
+    // Querying inbox by the new canonical `remote_id` or old `local_ferris.id` or `"ferris"` finds `msg`.
+    for query in [
+        remote_id.as_str(),
+        local_ferris.id.as_str(),
+        "ferris",
+    ] {
+        let inbox = s
+            .list_wire_inbox(&WireFilter {
+                recipient: Some(query.to_string()),
+                unread_only: true,
+                ..WireFilter::default()
+            })
+            .unwrap();
+        assert_eq!(inbox.len(), 1, "query {query} should find the message");
+        assert_eq!(inbox[0].message.id, msg.id);
+    }
+
+    // Acking via the new canonical `remote_id` succeeds.
+    let acked = s
+        .update_wire_receipt(
+            &msg.id,
+            remote_id.as_str(),
+            DeliveryStatus::Acked,
+            false,
+            At::from_unix_millis(30),
+        )
+        .unwrap()
+        .unwrap();
+    assert_eq!(acked.status, DeliveryStatus::Acked);
+}
+
+fn prune_wire_removes_only_acked_or_expired_transient_messages_before_cutoff<S: MessageStore>(
+    s: &S,
+) {
+    use crate::ferrowire::{
+        DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress,
+    };
+
+    // 1. Acked transient message before cutoff -> pruned.
+    let (m_acked, _) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: None,
+            sender_name: "jarvis".into(),
+            sender_host: None,
+            to: WireAddress::AgentName("ferris".into()),
+            kind: MessageKind::Mail,
+            urgency: MessageUrgency::Normal,
+            durable: false,
+            subject: "Transient acked".into(),
+            body: "Prune me".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(10),
+        })
+        .unwrap();
+    s.update_wire_receipt(
+        &m_acked.id,
+        "ferris",
+        DeliveryStatus::Acked,
+        false,
+        At::from_unix_millis(11),
+    )
+    .unwrap();
+
+    // 2. Pending transient message before cutoff -> kept.
+    let (m_pending, _) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: None,
+            sender_name: "jarvis".into(),
+            sender_host: None,
+            to: WireAddress::AgentName("ferris".into()),
+            kind: MessageKind::Mail,
+            urgency: MessageUrgency::Normal,
+            durable: false,
+            subject: "Transient pending".into(),
+            body: "Keep me".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(12),
+        })
+        .unwrap();
+
+    // 3. Acked durable message before cutoff -> kept.
+    let (m_durable, _) = s
+        .send_wire(SendWireArgs {
+            id: None,
+            sender_id: None,
+            sender_name: "jarvis".into(),
+            sender_host: None,
+            to: WireAddress::AgentName("ferris".into()),
+            kind: MessageKind::Mail,
+            urgency: MessageUrgency::Normal,
+            durable: true,
+            subject: "Durable acked".into(),
+            body: "Keep durable".into(),
+            reply_to: None,
+            thread_id: None,
+            deadline_at: None,
+            at: At::from_unix_millis(13),
+        })
+        .unwrap();
+    s.update_wire_receipt(
+        &m_durable.id,
+        "ferris",
+        DeliveryStatus::Acked,
+        false,
+        At::from_unix_millis(14),
+    )
+    .unwrap();
+
+    let pruned = s.prune_wire(At::from_unix_millis(20)).unwrap();
+    assert_eq!(pruned, 1);
+    assert!(s.get_wire_message(&m_acked.id).unwrap().is_none());
+    assert!(s.get_wire_message(&m_pending.id).unwrap().is_some());
+    assert!(s.get_wire_message(&m_durable.id).unwrap().is_some());
+}
```

And wire `wire: &self.catalog` / `wire: &self.local` into `crates/core/src/mem_issues.rs`, `crates/core/src/tiered.rs`, `crates/github/src/tracker.rs`, and `crates/github/src/ledger/fixture.rs`:

```diff
--- a/crates/core/src/mem_issues.rs
+++ b/crates/core/src/mem_issues.rs
@@ -497,6 +497,7 @@ mod tests {
                 handles: &self.catalog,
                 state: &self.catalog,
                 agents: &self.catalog,
+                wire: &self.catalog,
             });
         }
     }
--- a/crates/core/src/tiered.rs
+++ b/crates/core/src/tiered.rs
@@ -1887,6 +1887,7 @@ mod tests {
                 handles: &self.local,
                 state: &self.local,
                 agents: &self.local,
+                wire: &self.local,
             });
         }
     }
--- a/crates/github/src/ledger/fixture.rs
+++ b/crates/github/src/ledger/fixture.rs
@@ -124,6 +124,7 @@ impl SplitFixture for OverFake {
                 handles: &routed,
                 state: &local,
                 agents: &local,
+                wire: &local,
             },
             &Controls {
                 fake: &fake,
--- a/crates/github/src/tracker.rs
+++ b/crates/github/src/tracker.rs
@@ -5239,6 +5239,7 @@ mod tests {
                     handles: &handles,
                     state: &self.catalog,
                     agents: &self.catalog,
+                    wire: &self.catalog,
                 });
             }
         }
```

- [ ] **Step 3: Run `fl-core` and `fl-github` tests**

Run:
```bash
cargo test -p fl-core -p fl-github --offline
```
Expected: PASS (267 `fl-core` + 460 `fl-github` tests pass).

---

## Task 3: `RedbStore` Format 7 (`FORMAT_WITH_WIRE = 7`) in `fl-store`

**Files:**
- Create: `crates/store/src/ferrowire.rs`
- Modify: `crates/store/src/lib.rs`

**Interfaces:**
- Consumes: `fl_core::ferrowire::*`, `fl_core::store::MessageStore`, `RedbStore`, `raise_format`
- Produces: `FORMAT_WITH_WIRE = 7`, `impl MessageStore for RedbStore`, `RedbStore::verify_wire_invariants`

- [ ] **Step 1: Create `crates/store/src/ferrowire.rs`**

Write `crates/store/src/ferrowire.rs`:

```rust
//! `RedbStore` persistence for [`MessageStore`] (`FORMAT_WITH_WIRE = 7`, spec §2).

use fl_core::agent::{AgentRecord, AgentStatus};
use fl_core::at::At;
use fl_core::ferrowire::{
    DeliveryReceipt, DeliveryStatus, MessageKind, SendWireArgs, ThreadId, WireFilter,
    WireInboxItem, WireMessage, resolve_recipients, validate_sender_name, validate_subject,
    validate_wire_body,
};
use fl_core::ids::{Kind, MessageId};
use fl_core::iri::Iri;
use fl_core::state::{StateError, validate_slug};
use fl_core::store::{MessageStore, StoreError};
use redb::{ReadableDatabase, ReadableTable, TableDefinition, TableHandle};

use crate::state::{AGENT_BY_NAME, AGENTS};
use crate::{
    ALIASES, FORMAT_WITH_WIRE, IDS, META, RedbStore, backend, decode, locate_in, mint,
    raise_format,
};

pub(crate) const WIRE_MESSAGES: TableDefinition<&str, &str> = TableDefinition::new("wire_messages");
pub(crate) const WIRE_MSG_LOG: TableDefinition<u64, &str> = TableDefinition::new("wire_msg_log");
pub(crate) const WIRE_MSG_SEQ: TableDefinition<&str, u64> = TableDefinition::new("wire_msg_seq");
pub(crate) const WIRE_INBOX: TableDefinition<(&str, &str), &str> =
    TableDefinition::new("wire_inbox");
pub(crate) const WIRE_RCPT_LOG: TableDefinition<u64, &str> = TableDefinition::new("wire_rcpt_log");
pub(crate) const WIRE_THREADS: TableDefinition<(&str, &str), ()> =
    TableDefinition::new("wire_threads");
pub(crate) const WIRE_REPLIES: TableDefinition<(&str, &str), ()> =
    TableDefinition::new("wire_replies");

const NEXT_WIRE_MSG_SEQ: &str = "next_wire_msg_seq";
const NEXT_WIRE_RCPT_SEQ: &str = "next_wire_rcpt_seq";

impl RedbStore {
    fn wtx_has_table(tx: &redb::WriteTransaction, name: &str) -> Result<bool, StoreError> {
        Ok(tx
            .list_tables()
            .map_err(backend)?
            .any(|h| h.name() == name))
    }

    fn index_message_id_in(
        &self,
        tx: &redb::WriteTransaction,
        id: &Iri,
    ) -> Result<(), StoreError> {
        let already = {
            let ids = tx.open_table(IDS).map_err(backend)?;
            ids.get(id.as_str())
                .map_err(backend)?
                .map(|g| g.value().to_owned())
        };
        if let Some(wire) = already {
            if wire == "alias" {
                return Err(StoreError::WrongKind {
                    id: id.clone(),
                    expected: Kind::Message,
                    found: Kind::Agent,
                });
            }
            let found = Kind::from_wire(&wire)
                .ok_or_else(|| decode(format!("unknown kind `{wire}` for {id}")))?;
            if found != Kind::Message {
                return Err(StoreError::WrongKind {
                    id: id.clone(),
                    expected: Kind::Message,
                    found,
                });
            }
            return Ok(());
        }
        tx.open_table(IDS)
            .map_err(backend)?
            .insert(id.as_str(), Kind::Message.as_wire())
            .map_err(backend)?;
        Ok(())
    }

    fn append_wire_message_in(
        &self,
        tx: &redb::WriteTransaction,
        msg: &WireMessage,
    ) -> Result<(), StoreError> {
        let already_logged = tx
            .open_table(WIRE_MSG_SEQ)
            .map_err(backend)?
            .get(msg.id.as_str())
            .map_err(backend)?
            .is_some();
        if !already_logged {
            let seq = {
                let mut meta = tx.open_table(META).map_err(backend)?;
                let next = meta
                    .get(NEXT_WIRE_MSG_SEQ)
                    .map_err(backend)?
                    .map(|g| g.value())
                    .unwrap_or(0)
                    + 1;
                meta.insert(NEXT_WIRE_MSG_SEQ, next).map_err(backend)?;
                next
            };
            tx.open_table(WIRE_MSG_LOG)
                .map_err(backend)?
                .insert(seq, msg.id.as_str())
                .map_err(backend)?;
            tx.open_table(WIRE_MSG_SEQ)
                .map_err(backend)?
                .insert(msg.id.as_str(), seq)
                .map_err(backend)?;
        }
        tx.open_table(WIRE_THREADS)
            .map_err(backend)?
            .insert((msg.thread_id.as_str(), msg.id.as_str()), ())
            .map_err(backend)?;
        if let Some(ref corr) = msg.correlation_id {
            tx.open_table(WIRE_REPLIES)
                .map_err(backend)?
                .insert((corr.as_str(), msg.id.as_str()), ())
                .map_err(backend)?;
        }
        let json = serde_json::to_string(msg).map_err(backend)?;
        tx.open_table(WIRE_MESSAGES)
            .map_err(backend)?
            .insert(msg.id.as_str(), json.as_str())
            .map_err(backend)?;
        Ok(())
    }

    fn append_wire_receipt_in(
        &self,
        tx: &redb::WriteTransaction,
        rcpt: &DeliveryReceipt,
    ) -> Result<(), StoreError> {
        let seq = {
            let mut meta = tx.open_table(META).map_err(backend)?;
            let next = meta
                .get(NEXT_WIRE_RCPT_SEQ)
                .map_err(backend)?
                .map(|g| g.value())
                .unwrap_or(0)
                + 1;
            meta.insert(NEXT_WIRE_RCPT_SEQ, next).map_err(backend)?;
            next
        };
        let json = serde_json::to_string(rcpt).map_err(backend)?;
        tx.open_table(WIRE_RCPT_LOG)
            .map_err(backend)?
            .insert(seq, json.as_str())
            .map_err(backend)?;
        Ok(())
    }

    fn all_agents_in_wtx(&self, tx: &redb::WriteTransaction) -> Result<Vec<AgentRecord>, StoreError> {
        if !Self::wtx_has_table(tx, AGENTS.name())? {
            return Ok(Vec::new());
        }
        let table = tx.open_table(AGENTS).map_err(backend)?;
        let mut out = Vec::new();
        for row in table.iter().map_err(backend)? {
            let (_, v) = row.map_err(backend)?;
            let rec: AgentRecord = serde_json::from_str(v.value()).map_err(decode)?;
            out.push(rec);
        }
        Ok(out)
    }

    fn resolve_iri_in_rtx(&self, tx: &redb::ReadTransaction, id: &Iri) -> Result<Iri, StoreError> {
        let ids = tx.open_table(IDS).map_err(backend)?;
        match locate_in(&self.label, id, &ids, || {
            tx.open_table(ALIASES).map_err(backend)
        }) {
            Ok((primary, _)) => Ok(primary),
            Err(StoreError::NotOwned { .. }) => Ok(id.clone()),
            Err(e) => Err(e),
        }
    }

    fn resolve_iri_in_wtx(
        &self,
        tx: &redb::WriteTransaction,
        id: &Iri,
    ) -> Result<Iri, StoreError> {
        let ids = tx.open_table(IDS).map_err(backend)?;
        match locate_in(&self.label, id, &ids, || {
            tx.open_table(ALIASES).map_err(backend)
        }) {
            Ok((primary, _)) => Ok(primary),
            Err(StoreError::NotOwned { .. }) => Ok(id.clone()),
            Err(e) => Err(e),
        }
    }

    fn recipient_candidates_in_rtx(
        &self,
        tx: &redb::ReadTransaction,
        query: &str,
    ) -> Result<(Option<Iri>, Option<String>), StoreError> {
        let trimmed = query.trim();
        let agents_table = match tx.open_table(AGENTS) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(backend(e)),
        };
        let by_name_table = match tx.open_table(AGENT_BY_NAME) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(backend(e)),
        };

        let lookup_by_name = |k: &str| -> Result<Option<Iri>, StoreError> {
            let Some(ref t) = by_name_table else {
                return Ok(None);
            };
            let Some(g) = t.get(k).map_err(backend)? else {
                return Ok(None);
            };
            let iri = Iri::parse(g.value()).map_err(decode)?;
            self.resolve_iri_in_rtx(tx, &iri).map(Some)
        };

        if let Ok(iri) = Iri::parse(trimmed) {
            let resolved_iri = self.resolve_iri_in_rtx(tx, &iri)?;
            if let Some(ref at) = agents_table
                && let Some(g) = at.get(resolved_iri.as_str()).map_err(backend)?
            {
                let rec: AgentRecord = serde_json::from_str(g.value()).map_err(decode)?;
                return Ok((Some(resolved_iri), Some(rec.name_key)));
            }
            if let Some(rest) = trimmed.strip_prefix("urn:fl:agent:")
                && let Ok(k) = validate_slug(rest)
            {
                return Ok((lookup_by_name(&k)?, Some(k)));
            }
            return Ok((Some(resolved_iri), None));
        }
        if let Ok(k) = validate_slug(trimmed) {
            return Ok((lookup_by_name(&k)?, Some(k)));
        }
        Ok((None, None))
    }

    fn recipient_candidates_in_wtx(
        &self,
        tx: &redb::WriteTransaction,
        query: &str,
    ) -> Result<(Option<Iri>, Option<String>), StoreError> {
        let trimmed = query.trim();
        let has_agents = Self::wtx_has_table(tx, AGENTS.name())?;
        let has_by_name = Self::wtx_has_table(tx, AGENT_BY_NAME.name())?;

        let lookup_by_name = |k: &str| -> Result<Option<Iri>, StoreError> {
            if !has_by_name {
                return Ok(None);
            }
            let raw = {
                let t = tx.open_table(AGENT_BY_NAME).map_err(backend)?;
                let Some(g) = t.get(k).map_err(backend)? else {
                    return Ok(None);
                };
                g.value().to_owned()
            };
            let iri = Iri::parse(&raw).map_err(decode)?;
            self.resolve_iri_in_wtx(tx, &iri).map(Some)
        };

        if let Ok(iri) = Iri::parse(trimmed) {
            let resolved_iri = self.resolve_iri_in_wtx(tx, &iri)?;
            if has_agents {
                let raw_rec = {
                    let at = tx.open_table(AGENTS).map_err(backend)?;
                    at.get(resolved_iri.as_str())
                        .map_err(backend)?
                        .map(|g| g.value().to_owned())
                };
                if let Some(json) = raw_rec {
                    let rec: AgentRecord = serde_json::from_str(&json).map_err(decode)?;
                    return Ok((Some(resolved_iri), Some(rec.name_key)));
                }
            }
            if let Some(rest) = trimmed.strip_prefix("urn:fl:agent:")
                && let Ok(k) = validate_slug(rest)
            {
                return Ok((lookup_by_name(&k)?, Some(k)));
            }
            return Ok((Some(resolved_iri), None));
        }
        if let Ok(k) = validate_slug(trimmed) {
            return Ok((lookup_by_name(&k)?, Some(k)));
        }
        Ok((None, None))
    }

    fn receipt_matches_candidates(
        rcpt: &DeliveryReceipt,
        rcpt_resolved_id: &Iri,
        agent_id: Option<&Iri>,
        name_key: Option<&str>,
        raw_query: &str,
    ) -> bool {
        let trimmed = raw_query.trim();
        if let Some(aid) = agent_id
            && rcpt_resolved_id == aid
        {
            return true;
        }
        if let Some(nk) = name_key
            && (rcpt.recipient_name == nk
                || rcpt.recipient_id.as_str() == format!("urn:fl:agent:{nk}"))
        {
            return true;
        }
        rcpt.recipient_id.as_str() == trimmed || rcpt.recipient_name.eq_ignore_ascii_case(trimmed)
    }

    /// Verify FerroWire on-disk table invariants (spec §2.3).
    pub fn verify_wire_invariants(&self) -> Result<(), StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let ids = tx.open_table(IDS).map_err(backend)?;
        let messages_table = match tx.open_table(WIRE_MESSAGES) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(backend(e)),
        };

        // 1. Every `Kind::Message` entry in `IDS` must exist in `WIRE_MESSAGES`.
        for row in ids.iter().map_err(backend)? {
            let (k, v) = row.map_err(backend)?;
            if v.value() == Kind::Message.as_wire() {
                let exists = match messages_table {
                    Some(ref t) => t.get(k.value()).map_err(backend)?.is_some(),
                    None => false,
                };
                if !exists {
                    return Err(decode(format!(
                        "IDS holds message `{}` with no row in wire_messages",
                        k.value()
                    )));
                }
            }
        }

        let Some(ref messages) = messages_table else {
            return Ok(());
        };

        // 2. Every row in `WIRE_MESSAGES` must deserialize cleanly as a `WireMessage`
        //    whose `id` matches its table key and has a `Kind::Message` entry in `IDS`.
        for row in messages.iter().map_err(backend)? {
            let (k, v) = row.map_err(backend)?;
            let msg: WireMessage = serde_json::from_str(v.value()).map_err(decode)?;
            if msg.id.as_str() != k.value() {
                return Err(decode(format!(
                    "wire_messages key `{}` does not match message.id `{}`",
                    k.value(),
                    msg.id
                )));
            }
            let kind_wire = ids
                .get(k.value())
                .map_err(backend)?
                .map(|g| g.value().to_owned());
            if kind_wire.as_deref() != Some(Kind::Message.as_wire()) {
                return Err(decode(format!(
                    "wire_messages row `{}` is missing Kind::Message in IDS (found {:?})",
                    k.value(),
                    kind_wire
                )));
            }
        }

        // 3. Every row in `WIRE_INBOX` must deserialize cleanly as a `DeliveryReceipt`
        //    whose `(recipient_id, message_id)` matches its key and whose `message_id` exists in `WIRE_MESSAGES`.
        if let Ok(inbox) = tx.open_table(WIRE_INBOX) {
            for row in inbox.iter().map_err(backend)? {
                let (k, v) = row.map_err(backend)?;
                let (rec_key, msg_key) = k.value();
                let rcpt: DeliveryReceipt = serde_json::from_str(v.value()).map_err(decode)?;
                if rcpt.recipient_id.as_str() != rec_key || rcpt.message_id.as_str() != msg_key {
                    return Err(decode(format!(
                        "wire_inbox key `({rec_key}, {msg_key})` does not match receipt `({}, {})`",
                        rcpt.recipient_id, rcpt.message_id
                    )));
                }
                if messages.get(msg_key).map_err(backend)?.is_none() {
                    return Err(decode(format!(
                        "wire_inbox receipt for `{msg_key}` references missing message"
                    )));
                }
            }
        }

        // 4. Every `(thread_id, message_id)` in `WIRE_THREADS` and `(correlation_id, message_id)`
        //    in `WIRE_REPLIES` must reference an existing `message_id` in `WIRE_MESSAGES`.
        if let Ok(threads) = tx.open_table(WIRE_THREADS) {
            for row in threads.iter().map_err(backend)? {
                let (k, _) = row.map_err(backend)?;
                let (_tid, mid) = k.value();
                if messages.get(mid).map_err(backend)?.is_none() {
                    return Err(decode(format!(
                        "wire_threads entry references missing message `{mid}`"
                    )));
                }
            }
        }
        if let Ok(replies) = tx.open_table(WIRE_REPLIES) {
            for row in replies.iter().map_err(backend)? {
                let (k, _) = row.map_err(backend)?;
                let (_cid, mid) = k.value();
                if messages.get(mid).map_err(backend)?.is_none() {
                    return Err(decode(format!(
                        "wire_replies entry references missing message `{mid}`"
                    )));
                }
            }
        }

        Ok(())
    }
}

impl MessageStore for RedbStore {
    fn send_wire(
        &self,
        args: SendWireArgs,
    ) -> Result<(WireMessage, Vec<DeliveryReceipt>), StoreError> {
        let subject =
            validate_subject(&args.subject).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let body =
            validate_wire_body(&args.body).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let sender_name = validate_sender_name(&args.sender_name)
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;

        let tx = self.db.begin_write().map_err(backend)?;
        raise_format(&tx, FORMAT_WITH_WIRE)?;

        let (parent_thread, correlation_id) = match &args.reply_to {
            Some(parent_id) => {
                let parent_opt: Option<WireMessage> = {
                    let t = tx.open_table(WIRE_MESSAGES).map_err(backend)?;
                    match t.get(parent_id.as_str()).map_err(backend)? {
                        Some(g) => Some(serde_json::from_str(g.value()).map_err(decode)?),
                        None => None,
                    }
                };
                let Some(parent) = parent_opt else {
                    return Err(StoreError::MessageNotFound(parent_id.clone()));
                };
                let corr = if parent.kind == MessageKind::Ask {
                    Some(parent.id.clone())
                } else if parent.correlation_id.is_some() {
                    parent.correlation_id.clone()
                } else if args.kind == MessageKind::Reply {
                    Some(parent.id.clone())
                } else {
                    None
                };
                (Some(parent.thread_id), corr)
            }
            None => {
                if args.kind == MessageKind::Reply {
                    return Err(StoreError::InvalidState(
                        StateError::ReplyRequiresParent.to_string(),
                    ));
                }
                (None, None)
            }
        };

        let msg_id = match args.id {
            Some(id) => {
                self.index_message_id_in(&tx, id.iri())?;
                id
            }
            None => {
                let iri = mint()?;
                self.index_message_id_in(&tx, &iri)?;
                MessageId(iri)
            }
        };

        let thread_id = args
            .thread_id
            .or(parent_thread)
            .unwrap_or_else(|| msg_id.clone());
        let durable = args.durable || matches!(args.kind, MessageKind::Ask | MessageKind::Handoff);

        let all_agents = self.all_agents_in_wtx(&tx)?;
        let sender_id = args.sender_id.or_else(|| {
            all_agents
                .iter()
                .find(|a| a.status == AgentStatus::Active && a.name_key == sender_name)
                .map(|a| a.id.clone())
        });

        let msg = WireMessage {
            id: msg_id.clone(),
            thread_id,
            reply_to: args.reply_to,
            correlation_id,
            sender_id,
            sender_name,
            sender_host: args.sender_host,
            to: args.to,
            kind: args.kind,
            urgency: args.urgency,
            durable,
            subject,
            body,
            created_at: args.at.clone(),
            deadline_at: args.deadline_at,
        };
        msg.validate()
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;

        let recipients = resolve_recipients(
            &msg.to,
            msg.sender_id.as_ref(),
            &msg.sender_name,
            &all_agents,
        );

        self.append_wire_message_in(&tx, &msg)?;

        let mut receipts = Vec::with_capacity(recipients.len());
        {
            let mut inbox = tx.open_table(WIRE_INBOX).map_err(backend)?;
            for (recipient_id, recipient_name) in recipients {
                let rcpt = DeliveryReceipt {
                    message_id: msg.id.clone(),
                    recipient_id: recipient_id.clone(),
                    recipient_name,
                    status: DeliveryStatus::Pending,
                    attempts: 0,
                    updated_at: args.at.clone(),
                    acked_at: None,
                };
                let json = serde_json::to_string(&rcpt).map_err(backend)?;
                inbox
                    .insert((recipient_id.as_str(), msg.id.as_str()), json.as_str())
                    .map_err(backend)?;
                self.append_wire_receipt_in(&tx, &rcpt)?;
                receipts.push(rcpt);
            }
        }

        tx.commit().map_err(backend)?;
        Ok((msg, receipts))
    }

    fn get_wire_message(&self, id: &MessageId) -> Result<Option<WireMessage>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(WIRE_MESSAGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        match table.get(id.as_str()).map_err(backend)? {
            Some(g) => Ok(Some(serde_json::from_str(g.value()).map_err(decode)?)),
            None => Ok(None),
        }
    }

    fn list_wire_inbox(&self, filter: &WireFilter) -> Result<Vec<WireInboxItem>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let inbox = match tx.open_table(WIRE_INBOX) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(backend(e)),
        };
        let messages = match tx.open_table(WIRE_MESSAGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(backend(e)),
        };

        let candidates = match &filter.recipient {
            Some(q) => Some(self.recipient_candidates_in_rtx(&tx, q)?),
            None => None,
        };

        let mut items = Vec::new();
        for row in inbox.iter().map_err(backend)? {
            let (_, v) = row.map_err(backend)?;
            let rcpt: DeliveryReceipt = serde_json::from_str(v.value()).map_err(decode)?;
            if let (Some(q), Some((aid, nk))) = (&filter.recipient, &candidates) {
                let rcpt_resolved = self.resolve_iri_in_rtx(&tx, &rcpt.recipient_id)?;
                if !Self::receipt_matches_candidates(
                    &rcpt,
                    &rcpt_resolved,
                    aid.as_ref(),
                    nk.as_deref(),
                    q,
                ) {
                    continue;
                }
            }
            if filter.unread_only
                && matches!(
                    rcpt.status,
                    DeliveryStatus::Acked | DeliveryStatus::Expired
                )
            {
                continue;
            }
            let Some(msg_guard) = messages.get(rcpt.message_id.as_str()).map_err(backend)? else {
                continue;
            };
            let msg: WireMessage = serde_json::from_str(msg_guard.value()).map_err(decode)?;
            if let Some(ref tid) = filter.thread_id
                && &msg.thread_id != tid
            {
                continue;
            }
            if let Some(ref cid) = filter.correlation_id
                && msg.correlation_id.as_ref() != Some(cid)
            {
                continue;
            }
            items.push(WireInboxItem {
                message: msg,
                receipt: rcpt,
            });
        }

        items.sort_by(|a, b| {
            (&a.message.id, &a.receipt.recipient_id)
                .cmp(&(&b.message.id, &b.receipt.recipient_id))
        });
        if let Some(limit) = filter.limit
            && items.len() > limit
        {
            items.truncate(limit);
        }
        Ok(items)
    }

    fn list_wire_thread(&self, thread_id: &ThreadId) -> Result<Vec<WireMessage>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let threads = match tx.open_table(WIRE_THREADS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(backend(e)),
        };
        let messages = match tx.open_table(WIRE_MESSAGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(backend(e)),
        };

        let mut out = Vec::new();
        for row in threads.range((thread_id.as_str(), "")..).map_err(backend)? {
            let (k, _) = row.map_err(backend)?;
            let (tid, mid) = k.value();
            if tid != thread_id.as_str() {
                break;
            }
            if let Some(g) = messages.get(mid).map_err(backend)? {
                let msg: WireMessage = serde_json::from_str(g.value()).map_err(decode)?;
                out.push(msg);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn list_wire_replies(&self, correlation_id: &MessageId) -> Result<Vec<WireMessage>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let replies = match tx.open_table(WIRE_REPLIES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(backend(e)),
        };
        let messages = match tx.open_table(WIRE_MESSAGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(backend(e)),
        };

        let mut out = Vec::new();
        for row in replies
            .range((correlation_id.as_str(), "")..)
            .map_err(backend)?
        {
            let (k, _) = row.map_err(backend)?;
            let (cid, mid) = k.value();
            if cid != correlation_id.as_str() {
                break;
            }
            if let Some(g) = messages.get(mid).map_err(backend)? {
                let msg: WireMessage = serde_json::from_str(g.value()).map_err(decode)?;
                out.push(msg);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn list_wire_receipts(
        &self,
        message_id: &MessageId,
    ) -> Result<Vec<DeliveryReceipt>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let messages = match tx.open_table(WIRE_MESSAGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(StoreError::MessageNotFound(message_id.clone()));
            }
            Err(e) => return Err(backend(e)),
        };
        if messages.get(message_id.as_str()).map_err(backend)?.is_none() {
            return Err(StoreError::MessageNotFound(message_id.clone()));
        }
        let inbox = match tx.open_table(WIRE_INBOX) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(backend(e)),
        };
        let mut out = Vec::new();
        for row in inbox.iter().map_err(backend)? {
            let (k, v) = row.map_err(backend)?;
            let (_, mid) = k.value();
            if mid == message_id.as_str() {
                let rcpt: DeliveryReceipt = serde_json::from_str(v.value()).map_err(decode)?;
                out.push(rcpt);
            }
        }
        out.sort_by(|a, b| a.recipient_id.cmp(&b.recipient_id));
        Ok(out)
    }

    fn update_wire_receipt(
        &self,
        message_id: &MessageId,
        recipient: &str,
        status: DeliveryStatus,
        increment_attempts: bool,
        at: At,
    ) -> Result<Option<DeliveryReceipt>, StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        raise_format(&tx, FORMAT_WITH_WIRE)?;

        let msg_exists = tx
            .open_table(WIRE_MESSAGES)
            .map_err(backend)?
            .get(message_id.as_str())
            .map_err(backend)?
            .is_some();
        if !msg_exists {
            return Err(StoreError::MessageNotFound(message_id.clone()));
        }

        let (aid, nk) = self.recipient_candidates_in_wtx(&tx, recipient)?;
        let mut matched: Option<(String, DeliveryReceipt)> = None;
        {
            let inbox = tx.open_table(WIRE_INBOX).map_err(backend)?;
            let mut candidates_raw = Vec::new();
            for row in inbox.iter().map_err(backend)? {
                let (k, v) = row.map_err(backend)?;
                let (rec_id_str, mid_str) = k.value();
                if mid_str == message_id.as_str() {
                    let rcpt: DeliveryReceipt = serde_json::from_str(v.value()).map_err(decode)?;
                    candidates_raw.push((rec_id_str.to_owned(), rcpt));
                }
            }
            drop(inbox);
            for (rec_key, rcpt) in candidates_raw {
                let rcpt_resolved = self.resolve_iri_in_wtx(&tx, &rcpt.recipient_id)?;
                if Self::receipt_matches_candidates(
                    &rcpt,
                    &rcpt_resolved,
                    aid.as_ref(),
                    nk.as_deref(),
                    recipient,
                ) {
                    matched = Some((rec_key, rcpt));
                    break;
                }
            }
        }

        let Some((rec_key, existing)) = matched else {
            return Ok(None);
        };

        let attempts = if increment_attempts {
            existing.attempts.saturating_add(1)
        } else {
            existing.attempts
        };
        let incoming = DeliveryReceipt {
            message_id: existing.message_id.clone(),
            recipient_id: existing.recipient_id.clone(),
            recipient_name: existing.recipient_name.clone(),
            status,
            attempts,
            updated_at: at.clone(),
            acked_at: if status == DeliveryStatus::Acked {
                Some(at)
            } else {
                None
            },
        };
        let merged = existing.merge(&incoming);
        let json = serde_json::to_string(&merged).map_err(backend)?;
        tx.open_table(WIRE_INBOX)
            .map_err(backend)?
            .insert((rec_key.as_str(), message_id.as_str()), json.as_str())
            .map_err(backend)?;
        self.append_wire_receipt_in(&tx, &merged)?;
        tx.commit().map_err(backend)?;
        Ok(Some(merged))
    }

    fn wire_messages_after(
        &self,
        after_seq: u64,
    ) -> Result<Vec<(u64, WireMessage)>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let log = match tx.open_table(WIRE_MSG_LOG) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(backend(e)),
        };
        let messages = match tx.open_table(WIRE_MESSAGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(backend(e)),
        };
        let mut out = Vec::new();
        for row in log.range((after_seq + 1)..).map_err(backend)? {
            let (k, v) = row.map_err(backend)?;
            if let Some(msg_guard) = messages.get(v.value()).map_err(backend)? {
                let msg: WireMessage = serde_json::from_str(msg_guard.value()).map_err(decode)?;
                out.push((k.value(), msg));
            }
        }
        Ok(out)
    }

    fn wire_receipts_after(
        &self,
        after_seq: u64,
    ) -> Result<Vec<(u64, DeliveryReceipt)>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let log = match tx.open_table(WIRE_RCPT_LOG) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(backend(e)),
        };
        let messages = match tx.open_table(WIRE_MESSAGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(backend(e)),
        };
        let mut out = Vec::new();
        for row in log.range((after_seq + 1)..).map_err(backend)? {
            let (k, v) = row.map_err(backend)?;
            let rcpt: DeliveryReceipt = serde_json::from_str(v.value()).map_err(decode)?;
            if messages.get(rcpt.message_id.as_str()).map_err(backend)?.is_some() {
                out.push((k.value(), rcpt));
            }
        }
        Ok(out)
    }

    fn import_wire_batch(
        &self,
        messages: &[WireMessage],
        receipts: &[DeliveryReceipt],
    ) -> Result<(), StoreError> {
        if messages.is_empty() && receipts.is_empty() {
            return Ok(());
        }
        let tx = self.db.begin_write().map_err(backend)?;
        raise_format(&tx, FORMAT_WITH_WIRE)?;
        let all_agents = self.all_agents_in_wtx(&tx)?;

        for msg in messages {
            msg.validate()
                .map_err(|e| StoreError::InvalidState(e.to_string()))?;
            let existing: Option<WireMessage> = {
                let t = tx.open_table(WIRE_MESSAGES).map_err(backend)?;
                match t.get(msg.id.as_str()).map_err(backend)? {
                    Some(g) => Some(serde_json::from_str(g.value()).map_err(decode)?),
                    None => None,
                }
            };
            if let Some(ex) = existing {
                if &ex != msg {
                    return Err(StoreError::WireMessageTampered {
                        id: msg.id.clone(),
                        detail: "imported wire message differs from stored copy".into(),
                    });
                }
                continue;
            }
            self.index_message_id_in(&tx, msg.id.iri())?;
            self.append_wire_message_in(&tx, msg)?;

            let resolved = resolve_recipients(
                &msg.to,
                msg.sender_id.as_ref(),
                &msg.sender_name,
                &all_agents,
            );
            for (rec_id, rec_name) in resolved {
                let in_incoming = receipts.iter().any(|r| {
                    r.message_id == msg.id
                        && (r.recipient_id == rec_id || r.recipient_name == rec_name)
                });
                let already_in_inbox = tx
                    .open_table(WIRE_INBOX)
                    .map_err(backend)?
                    .get((rec_id.as_str(), msg.id.as_str()))
                    .map_err(backend)?
                    .is_some();
                if !in_incoming && !already_in_inbox {
                    let rcpt = DeliveryReceipt {
                        message_id: msg.id.clone(),
                        recipient_id: rec_id.clone(),
                        recipient_name: rec_name,
                        status: DeliveryStatus::Pending,
                        attempts: 0,
                        updated_at: msg.created_at.clone(),
                        acked_at: None,
                    };
                    let json = serde_json::to_string(&rcpt).map_err(backend)?;
                    tx.open_table(WIRE_INBOX)
                        .map_err(backend)?
                        .insert((rec_id.as_str(), msg.id.as_str()), json.as_str())
                        .map_err(backend)?;
                    self.append_wire_receipt_in(&tx, &rcpt)?;
                }
            }
        }

        for rcpt in receipts {
            let msg_exists = tx
                .open_table(WIRE_MESSAGES)
                .map_err(backend)?
                .get(rcpt.message_id.as_str())
                .map_err(backend)?
                .is_some();
            if !msg_exists {
                return Err(StoreError::MessageNotFound(rcpt.message_id.clone()));
            }
            let existing: Option<DeliveryReceipt> = {
                let inbox = tx.open_table(WIRE_INBOX).map_err(backend)?;
                match inbox
                    .get((rcpt.recipient_id.as_str(), rcpt.message_id.as_str()))
                    .map_err(backend)?
                {
                    Some(g) => Some(serde_json::from_str(g.value()).map_err(decode)?),
                    None => None,
                }
            };
            match existing {
                Some(ex) => {
                    let merged = ex.merge(rcpt);
                    if merged != ex {
                        let json = serde_json::to_string(&merged).map_err(backend)?;
                        tx.open_table(WIRE_INBOX)
                            .map_err(backend)?
                            .insert(
                                (rcpt.recipient_id.as_str(), rcpt.message_id.as_str()),
                                json.as_str(),
                            )
                            .map_err(backend)?;
                        self.append_wire_receipt_in(&tx, &merged)?;
                    }
                }
                None => {
                    let json = serde_json::to_string(rcpt).map_err(backend)?;
                    tx.open_table(WIRE_INBOX)
                        .map_err(backend)?
                        .insert(
                            (rcpt.recipient_id.as_str(), rcpt.message_id.as_str()),
                            json.as_str(),
                        )
                        .map_err(backend)?;
                    self.append_wire_receipt_in(&tx, rcpt)?;
                }
            }
        }

        tx.commit().map_err(backend)?;
        Ok(())
    }

    fn prune_wire(&self, before: At) -> Result<usize, StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        if !Self::wtx_has_table(&tx, WIRE_MESSAGES.name())? {
            return Ok(0);
        }
        raise_format(&tx, FORMAT_WITH_WIRE)?;

        let all_messages: Vec<WireMessage> = {
            let t = tx.open_table(WIRE_MESSAGES).map_err(backend)?;
            let mut out = Vec::new();
            for row in t.iter().map_err(backend)? {
                let (_, v) = row.map_err(backend)?;
                out.push(serde_json::from_str(v.value()).map_err(decode)?);
            }
            out
        };

        let all_receipts: Vec<DeliveryReceipt> = if Self::wtx_has_table(&tx, WIRE_INBOX.name())? {
            let t = tx.open_table(WIRE_INBOX).map_err(backend)?;
            let mut out = Vec::new();
            for row in t.iter().map_err(backend)? {
                let (_, v) = row.map_err(backend)?;
                out.push(serde_json::from_str(v.value()).map_err(decode)?);
            }
            out
        } else {
            Vec::new()
        };

        let to_prune: Vec<WireMessage> = all_messages
            .into_iter()
            .filter(|msg| !msg.durable && msg.created_at < before)
            .filter(|msg| {
                all_receipts
                    .iter()
                    .filter(|r| r.message_id == msg.id)
                    .all(|r| matches!(r.status, DeliveryStatus::Acked | DeliveryStatus::Expired))
            })
            .collect();

        let count = to_prune.len();
        if count == 0 {
            return Ok(0);
        }

        let pruned_ids: std::collections::BTreeSet<MessageId> =
            to_prune.iter().map(|m| m.id.clone()).collect();

        for msg in to_prune {
            tx.open_table(WIRE_MESSAGES)
                .map_err(backend)?
                .remove(msg.id.as_str())
                .map_err(backend)?;
            tx.open_table(IDS)
                .map_err(backend)?
                .remove(msg.id.as_str())
                .map_err(backend)?;
            tx.open_table(WIRE_THREADS)
                .map_err(backend)?
                .remove((msg.thread_id.as_str(), msg.id.as_str()))
                .map_err(backend)?;
            if let Some(ref corr) = msg.correlation_id {
                tx.open_table(WIRE_REPLIES)
                    .map_err(backend)?
                    .remove((corr.as_str(), msg.id.as_str()))
                    .map_err(backend)?;
            }
            let seq = tx
                .open_table(WIRE_MSG_SEQ)
                .map_err(backend)?
                .remove(msg.id.as_str())
                .map_err(backend)?
                .map(|g| g.value());
            if let Some(s) = seq {
                tx.open_table(WIRE_MSG_LOG)
                    .map_err(backend)?
                    .remove(s)
                    .map_err(backend)?;
            }
            let rcpt_keys: Vec<String> = all_receipts
                .iter()
                .filter(|r| r.message_id == msg.id)
                .map(|r| r.recipient_id.as_str().to_owned())
                .collect();
            let mut inbox = tx.open_table(WIRE_INBOX).map_err(backend)?;
            for rk in rcpt_keys {
                inbox
                    .remove((rk.as_str(), msg.id.as_str()))
                    .map_err(backend)?;
            }
        }

        if Self::wtx_has_table(&tx, WIRE_RCPT_LOG.name())? {
            let mut rcpt_seqs_to_remove = Vec::new();
            {
                let rcpt_log = tx.open_table(WIRE_RCPT_LOG).map_err(backend)?;
                for row in rcpt_log.iter().map_err(backend)? {
                    let (k, v) = row.map_err(backend)?;
                    let rcpt: DeliveryReceipt = serde_json::from_str(v.value()).map_err(decode)?;
                    if pruned_ids.contains(&rcpt.message_id) {
                        rcpt_seqs_to_remove.push(k.value());
                    }
                }
            }
            let mut rcpt_log = tx.open_table(WIRE_RCPT_LOG).map_err(backend)?;
            for seq in rcpt_seqs_to_remove {
                rcpt_log.remove(seq).map_err(backend)?;
            }
        }

        tx.commit().map_err(backend)?;
        Ok(count)
    }
}
```

- [ ] **Step 2: Register `pub mod ferrowire;`, `FORMAT_WITH_WIRE = 7`, and conformance/format tests in `crates/store/src/lib.rs`**

Apply to `crates/store/src/lib.rs`:

```diff
--- a/crates/store/src/lib.rs
+++ b/crates/store/src/lib.rs
@@ -13,6 +13,7 @@ use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableHand
 use std::collections::BTreeSet;
 use std::path::Path;
 
+pub mod ferrowire;
 pub mod manifest;
 pub mod state;
 
@@ -100,6 +101,12 @@ pub const FORMAT_WITH_ROUTING: u64 = 5;
 /// than ignoring its state or agent tables. This build opens 2 to 6.
 pub const FORMAT_WITH_STATE: u64 = 6;
 
+/// ⚠ The format of a store that holds FerroWire messages or delivery receipts.
+/// The first FerroWire write raises the store to 7 in the same transaction, so
+/// an older fl refuses the store with `FormatVersion` rather than ignoring its
+/// FerroWire tables. This build opens 2 to 7.
+pub const FORMAT_WITH_WIRE: u64 = 7;
+
 /// repository `node_id` → the first commit of its `fl/ledger` branch
 /// (GitHub ledger spec §6.1 step 4). Created by the first root recorded.
 const LEDGER_ROOTS: TableDefinition<&str, &str> = TableDefinition::new("ledger_roots");
@@ -473,12 +480,13 @@ impl RedbStore {
                     || v == FORMAT_WITH_IMPORTS
                     || v == FORMAT_WITH_LEDGER_ROOT
                     || v == FORMAT_WITH_ROUTING
-                    || v == FORMAT_WITH_STATE => {}
+                    || v == FORMAT_WITH_STATE
+                    || v == FORMAT_WITH_WIRE => {}
             Some(v) => {
                 return Err(StoreError::FormatVersion {
                     found: v,
                     oldest: FORMAT_VERSION,
-                    newest: FORMAT_WITH_STATE,
+                    newest: FORMAT_WITH_WIRE,
                 });
             }
         }
@@ -1997,7 +2005,7 @@ mod tests {
                 StoreError::FormatVersion {
                     found: Some(1),
                     oldest: 2,
-                    newest: FORMAT_WITH_STATE
+                    newest: FORMAT_WITH_WIRE
                 }
             ),
             "{err:?}"
@@ -2432,6 +2440,7 @@ mod tests {
         fl_core::conformance::ledger_cache(fresh);
         fl_core::conformance::agent_registry(fresh);
         fl_core::conformance::state_store(fresh);
+        fl_core::conformance::message_store(fresh);
     }
 
     #[test]
@@ -3056,7 +3065,7 @@ mod tests {
             let tx = db.begin_write().unwrap();
             tx.open_table(META)
                 .unwrap()
-                .insert(FORMAT_KEY, FORMAT_WITH_STATE + 1)
+                .insert(FORMAT_KEY, FORMAT_WITH_WIRE + 1)
                 .unwrap();
             tx.commit().unwrap();
         }
@@ -3537,4 +3546,64 @@ mod tests {
         let reopened = RedbStore::open(&path).unwrap();
         assert_eq!(reopened.get_state(&st.id).unwrap(), Some(st));
     }
+
+    #[test]
+    fn redb_store_meets_the_message_store_contract() {
+        fl_core::conformance::message_store(fresh);
+    }
+
+    #[test]
+    fn wire_write_raises_the_store_to_format_7_and_verifies_invariants() {
+        use fl_core::at::At;
+        use fl_core::ferrowire::{
+            DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress,
+        };
+        use fl_core::store::MessageStore;
+
+        let dir = tempfile::tempdir().unwrap();
+        let path = dir.path().join("wire.redb");
+        {
+            let _s = RedbStore::open(&path).unwrap();
+        }
+        assert_eq!(format_at(&path), Some(FORMAT_VERSION));
+        let msg = {
+            let s = RedbStore::open(&path).unwrap();
+            let (msg, _) = s
+                .send_wire(SendWireArgs {
+                    id: None,
+                    sender_id: None,
+                    sender_name: "jarvis".into(),
+                    sender_host: Some("ai-lab-0".into()),
+                    to: WireAddress::AgentName("ferris".into()),
+                    kind: MessageKind::Mail,
+                    urgency: MessageUrgency::Normal,
+                    durable: false,
+                    subject: "Format 7 test".into(),
+                    body: "Raise to 7".into(),
+                    reply_to: None,
+                    thread_id: None,
+                    deadline_at: None,
+                    at: At::from_unix_millis(10),
+                })
+                .unwrap();
+            s.verify_wire_invariants().unwrap();
+            msg
+        };
+        assert_eq!(format_at(&path), Some(FORMAT_WITH_WIRE));
+        let reopened = RedbStore::open(&path).unwrap();
+        reopened.verify_wire_invariants().unwrap();
+        assert_eq!(reopened.get_wire_message(&msg.id).unwrap(), Some(msg.clone()));
+
+        reopened
+            .update_wire_receipt(
+                &msg.id,
+                "ferris",
+                DeliveryStatus::Acked,
+                false,
+                At::from_unix_millis(15),
+            )
+            .unwrap();
+        assert_eq!(reopened.prune_wire(At::from_unix_millis(20)).unwrap(), 1);
+        reopened.verify_wire_invariants().unwrap();
+    }
 }
```

- [ ] **Step 3: Run `fl-store` tests**

Run:
```bash
cargo test -p fl-store --offline
```
Expected: PASS (82 tests pass, including `redb_store_meets_the_message_store_contract` and `wire_write_raises_the_store_to_format_7_and_verifies_invariants`).

---

## Task 4: Offline Git Store-and-Forward (`refs/heads/fl/state`) in `fl-exec`

**Files:**
- Modify: `crates/exec/src/state_sync.rs`

**Interfaces:**
- Consumes: `fl_core::ferrowire::{DeliveryReceipt, DeliveryStatus, WireMessage}`, `fl_core::store::MessageStore`, `Git`
- Produces: `StateSyncTarget::sync_wire`, `StateSyncReport::{imported_wire_messages, imported_wire_receipts, published_wire_messages, published_wire_receipts}`, `StateVerifyReport::{verified_wire_messages, verified_wire_receipts}`, `wire/messages/<prefix2>/<file>.json` and `wire/receipts/<seg:04>.jsonl` tree synchronization and verification

- [ ] **Step 1: Extend `crates/exec/src/state_sync.rs` with selective FerroWire export, import, and append-only/immutability verification**

Apply the following diff to `crates/exec/src/state_sync.rs`:

```diff
--- a/crates/exec/src/state_sync.rs
+++ b/crates/exec/src/state_sync.rs
@@ -5,9 +5,11 @@ use std::path::PathBuf;
 
 use fl_core::agent::RegistryEvent;
 use fl_core::at::At;
-use fl_core::ids::StateId;
+use fl_core::ferrowire::{DeliveryReceipt, DeliveryStatus, WireMessage};
+use fl_core::ids::{MessageId, StateId};
+use fl_core::iri::Iri;
 use fl_core::state::{StateRevision, StateScope, StateSyncCursor};
-use fl_core::store::{AgentRegistry, StateStore, StoreError};
+use fl_core::store::{AgentRegistry, MessageStore, StateStore, StoreError};
 use sha2::{Digest, Sha256};
 
 use crate::git::{Git, GitTreeEntry, PushCasOutcome};
@@ -32,6 +34,7 @@ pub struct StateSyncTarget {
     pub branch: String,
     pub private: bool,
     pub sync_registry: bool,
+    pub sync_wire: bool,
     pub scope_filter: Option<StateScope>,
     pub by: String,
 }
@@ -52,8 +55,12 @@ pub struct StateSyncReport {
     pub committed_oid: Option<String>,
     pub imported_registry_events: usize,
     pub imported_revisions: usize,
+    pub imported_wire_messages: usize,
+    pub imported_wire_receipts: usize,
     pub published_registry_events: usize,
     pub published_revisions: usize,
+    pub published_wire_messages: usize,
+    pub published_wire_receipts: usize,
     pub active_conflicts: usize,
 }
 
@@ -67,6 +74,8 @@ pub struct StateVerifyReport {
     pub verified_commits: usize,
     pub verified_revisions: usize,
     pub verified_registry_events: usize,
+    pub verified_wire_messages: usize,
+    pub verified_wire_receipts: usize,
 }
 
 #[derive(Debug, thiserror::Error)]
@@ -122,6 +131,37 @@ struct BranchSnapshot {
     /// `revisions/<hash>/<seg>.jsonl` -> raw bytes
     segments: BTreeMap<(String, u32), Vec<u8>>,
     revisions: Vec<StateRevision>,
+    /// `wire/messages/<prefix2>/<filename>.json` (`<prefix2>/<filename>.json`) -> raw bytes
+    wire_messages_raw: BTreeMap<String, Vec<u8>>,
+    wire_messages: Vec<WireMessage>,
+    /// `wire/receipts/<seg>.jsonl` -> raw bytes
+    wire_receipt_segments: BTreeMap<u32, Vec<u8>>,
+    wire_receipts: Vec<DeliveryReceipt>,
+}
+
+fn wire_message_rel_path(id: &MessageId) -> String {
+    let fname = id.as_str().replace(':', "_");
+    let hex_tail: String = id
+        .as_str()
+        .chars()
+        .rev()
+        .filter(|c| c.is_ascii_hexdigit())
+        .take(2)
+        .collect::<Vec<_>>()
+        .into_iter()
+        .rev()
+        .collect();
+    let prefix = if hex_tail.len() == 2 {
+        hex_tail.to_ascii_lowercase()
+    } else {
+        "00".to_string()
+    };
+    format!("{prefix}/{fname}.json")
+}
+
+fn parse_wire_receipt_segment_path(path: &str) -> Option<u32> {
+    let rest = path.strip_prefix("wire/receipts/")?;
+    let stem = rest.strip_suffix(".jsonl")?;
+    if stem.len() != 4 || !stem.chars().all(|c| c.is_ascii_digit()) {
+        return None;
+    }
+    stem.parse().ok()
 }
 
 fn parse_segment_path(path: &str) -> Option<(String, u32)> {
@@ -246,6 +283,68 @@ fn load_snapshot(
                         });
                     }
                     snap.segments.insert((hash, seg), raw);
+                } else if let Some(rel) = other.strip_prefix("wire/messages/") {
+                    let text = String::from_utf8(raw.clone()).map_err(|e| {
+                        StateSyncError::CorruptJsonl {
+                            path: other.to_owned(),
+                            line: 1,
+                            reason: e.to_string(),
+                        }
+                    })?;
+                    let msg: WireMessage = serde_json::from_str(&text).map_err(|e| {
+                        StateSyncError::CorruptJsonl {
+                            path: other.to_owned(),
+                            line: 1,
+                            reason: e.to_string(),
+                        }
+                    })?;
+                    let expected_rel = wire_message_rel_path(&msg.id);
+                    if rel != expected_rel {
+                        return Err(StateSyncError::HistoryTampered {
+                            commit: commit.to_owned(),
+                            path: other.to_owned(),
+                            reason: format!(
+                                "wire message `{}` belongs at `wire/messages/{expected_rel}`, not `{other}`",
+                                msg.id
+                            ),
+                        });
+                    }
+                    snap.wire_messages.push(msg);
+                    snap.wire_messages_raw.insert(rel.to_owned(), raw);
+                } else if let Some(seg) = parse_wire_receipt_segment_path(other) {
+                    let text = String::from_utf8(raw.clone()).map_err(|e| {
+                        StateSyncError::CorruptJsonl {
+                            path: other.to_owned(),
+                            line: 1,
+                            reason: e.to_string(),
+                        }
+                    })?;
+                    let mut line_count = 0usize;
+                    for (idx, line) in text.lines().enumerate() {
+                        if line.trim().is_empty() {
+                            continue;
+                        }
+                        line_count += 1;
+                        let rcpt: DeliveryReceipt = serde_json::from_str(line).map_err(|e| {
+                            StateSyncError::CorruptJsonl {
+                                path: other.to_owned(),
+                                line: idx + 1,
+                                reason: e.to_string(),
+                            }
+                        })?;
+                        snap.wire_receipts.push(rcpt);
+                    }
+                    if line_count > 1 && raw.len() > MAX_SEGMENT_BYTES {
+                        return Err(StateSyncError::HistoryTampered {
+                            commit: commit.to_owned(),
+                            path: other.to_owned(),
+                            reason: format!(
+                                "multi-line receipt segment size {} exceeds MAX_SEGMENT_BYTES ({MAX_SEGMENT_BYTES})",
+                                raw.len()
+                            ),
+                        });
+                    }
+                    snap.wire_receipt_segments.insert(seg, raw);
                 }
             }
         }
@@ -253,8 +352,9 @@ fn load_snapshot(
     Ok(snap)
 }
 
-/// Verify that every append-only file (`areas.jsonl`, `agents.jsonl`, and `revisions/<hash>/<n>.jsonl`)
-/// in `next` is a byte-for-byte prefix extension of `prev`.
+/// Verify that every append-only file (`areas.jsonl`, `agents.jsonl`, `revisions/<hash>/<n>.jsonl`,
+/// `wire/receipts/<n>.jsonl`) in `next` is a byte-for-byte prefix extension of `prev`, and every
+/// `wire/messages/<prefix2>/<file>.json` in `prev` is present in `next` with identical bytes.
 fn verify_append_only_step(
     commit: &str,
     prev: &BranchSnapshot,
@@ -297,6 +397,40 @@ fn verify_append_only_step(
             });
         }
     }
+    for (rel_path, prev_bytes) in &prev.wire_messages_raw {
+        let path = format!("wire/messages/{rel_path}");
+        let Some(next_bytes) = next.wire_messages_raw.get(rel_path) else {
+            return Err(StateSyncError::HistoryTampered {
+                commit: commit.to_owned(),
+                path,
+                reason: "wire message file was deleted".into(),
+            });
+        };
+        if next_bytes != prev_bytes {
+            return Err(StateSyncError::HistoryTampered {
+                commit: commit.to_owned(),
+                path,
+                reason: "wire message file was modified".into(),
+            });
+        }
+    }
+    for (seg, prev_bytes) in &prev.wire_receipt_segments {
+        let path = format!("wire/receipts/{seg:04}.jsonl");
+        let Some(next_bytes) = next.wire_receipt_segments.get(seg) else {
+            return Err(StateSyncError::HistoryTampered {
+                commit: commit.to_owned(),
+                path,
+                reason: "wire receipt segment file was deleted".into(),
+            });
+        };
+        if !next_bytes.starts_with(prev_bytes) {
+            return Err(StateSyncError::HistoryTampered {
+                commit: commit.to_owned(),
+                path,
+                reason: "wire receipt segment file was modified or truncated".into(),
+            });
+        }
+    }
     Ok(())
 }
 
@@ -408,6 +542,70 @@ fn write_snapshot_tree(
         });
     }
 
+    if !snap.wire_messages_raw.is_empty() || !snap.wire_receipt_segments.is_empty() {
+        let mut wire_entries = Vec::new();
+        if !snap.wire_messages_raw.is_empty() {
+            let mut by_prefix: BTreeMap<String, Vec<GitTreeEntry>> = BTreeMap::new();
+            for (rel_path, bytes) in &snap.wire_messages_raw {
+                if let Some((prefix, file)) = rel_path.split_once('/') {
+                    let oid = Git::hash_object(repo_dir, bytes)?;
+                    by_prefix
+                        .entry(prefix.to_owned())
+                        .or_default()
+                        .push(GitTreeEntry {
+                            mode: "100644".into(),
+                            kind: "blob".into(),
+                            oid,
+                            path: file.to_owned(),
+                        });
+                }
+            }
+            let mut prefix_trees = Vec::new();
+            for (prefix, entries) in by_prefix {
+                let subtree_oid = Git::mktree(repo_dir, &entries)?;
+                prefix_trees.push(GitTreeEntry {
+                    mode: "040000".into(),
+                    kind: "tree".into(),
+                    oid: subtree_oid,
+                    path: prefix,
+                });
+            }
+            let messages_tree_oid = Git::mktree(repo_dir, &prefix_trees)?;
+            wire_entries.push(GitTreeEntry {
+                mode: "040000".into(),
+                kind: "tree".into(),
+                oid: messages_tree_oid,
+                path: "messages".into(),
+            });
+        }
+        if !snap.wire_receipt_segments.is_empty() {
+            let mut rcpt_entries = Vec::new();
+            for (seg, bytes) in &snap.wire_receipt_segments {
+                let oid = Git::hash_object(repo_dir, bytes)?;
+                rcpt_entries.push(GitTreeEntry {
+                    mode: "100644".into(),
+                    kind: "blob".into(),
+                    oid,
+                    path: format!("{seg:04}.jsonl"),
+                });
+            }
+            let receipts_tree_oid = Git::mktree(repo_dir, &rcpt_entries)?;
+            wire_entries.push(GitTreeEntry {
+                mode: "040000".into(),
+                kind: "tree".into(),
+                oid: receipts_tree_oid,
+                path: "receipts".into(),
+            });
+        }
+        let wire_tree_oid = Git::mktree(repo_dir, &wire_entries)?;
+        root_entries.push(GitTreeEntry {
+            mode: "040000".into(),
+            kind: "tree".into(),
+            oid: wire_tree_oid,
+            path: "wire".into(),
+        });
+    }
+
     Ok(Git::mktree(repo_dir, &root_entries)?)
 }
 
@@ -440,6 +638,25 @@ fn append_revision_to_segments(
         .extend_from_slice(line_bytes);
 }
 
+fn append_receipt_to_segments(segments: &mut BTreeMap<u32, Vec<u8>>, rcpt: &DeliveryReceipt) {
+    let mut line = serde_json::to_string(rcpt).expect("DeliveryReceipt serializes");
+    line.push('\n');
+    let line_bytes = line.as_bytes();
+
+    let latest_seg = segments.keys().next_back().copied().unwrap_or(0);
+    let current_len = segments.get(&latest_seg).map_or(0, Vec::len);
+    let target_seg =
+        if current_len > 0 && current_len + line_bytes.len() > MAX_SEGMENT_BYTES {
+            latest_seg + 1
+        } else {
+            latest_seg
+        };
+    segments
+        .entry(target_seg)
+        .or_default()
+        .extend_from_slice(line_bytes);
+}
+
 /// Synchronize `store` with the `<remote>/<branch>` orphan branch in `target.repo_dir`
 /// using the 5-step compare-and-swap protocol (spec §4.2).
 pub fn sync_state<S>(
@@ -448,7 +665,7 @@ pub fn sync_state<S>(
     now: At,
 ) -> Result<StateSyncReport, StateSyncError>
 where
-    S: StateStore + AgentRegistry,
+    S: StateStore + AgentRegistry + MessageStore,
 {
     let local_tracking_ref = format!("refs/fl-sync/{}/{}", target.remote, target.branch);
     let cursor_key = target.cursor_key();
@@ -462,6 +679,8 @@ where
                 last_pushed_commit: None,
                 last_revision_seq: 0,
                 last_registry_seq: 0,
+                last_wire_msg_seq: 0,
+                last_wire_rcpt_seq: 0,
             });
 
         // Step 1: Fetch remote tip into isolated tracking ref.
@@ -518,7 +737,7 @@ where
             base_snapshot = prev_snap;
         }
 
-        // Step 3: Import unseen remote registry events and state revisions.
+        // Step 3: Import unseen remote registry events, state revisions, and wire records.
         let imported_registry_events = if target.sync_registry {
             store.import_registry_events(&base_snapshot.registry_events)?;
             base_snapshot.registry_events.len()
@@ -538,7 +757,17 @@ where
         store.import_state_revisions(&ordered_remote_revs, now.clone())?;
         let imported_revisions = ordered_remote_revs.len();
 
-        // Step 4: Collect unpublished local events/revisions and enforce sensitivity guard.
+        let (imported_wire_messages, imported_wire_receipts) = if target.sync_wire {
+            store.import_wire_batch(&base_snapshot.wire_messages, &base_snapshot.wire_receipts)?;
+            (
+                base_snapshot.wire_messages.len(),
+                base_snapshot.wire_receipts.len(),
+            )
+        } else {
+            (0, 0)
+        };
+
+        // Step 4: Collect unpublished local events/revisions/wire records and enforce sensitivity guard.
         if !target.private {
             for entry in store.list_state(target.scope_filter.as_ref(), None)? {
                 if entry.sensitive || entry.ever_sensitive {
@@ -591,13 +820,99 @@ where
             (Vec::new(), cursor.last_registry_seq)
         };
 
-        if unpublished_revs.is_empty() && unpublished_reg_events.is_empty() {
+        let (unpublished_wire_msgs, unpublished_wire_rcpts, max_wire_msg_seq, max_wire_rcpt_seq) =
+            if target.sync_wire {
+                let local_msgs = store.wire_messages_after(cursor.last_wire_msg_seq)?;
+                let max_msg_seq = local_msgs
+                    .last()
+                    .map(|(seq, _)| *seq)
+                    .unwrap_or(cursor.last_wire_msg_seq);
+                let remote_msg_ids: BTreeSet<MessageId> = base_snapshot
+                    .wire_messages
+                    .iter()
+                    .map(|m| m.id.clone())
+                    .collect();
+                let mut unpublished_msgs = Vec::new();
+                for (_, msg) in local_msgs {
+                    if remote_msg_ids.contains(&msg.id) {
+                        continue;
+                    }
+                    if msg.durable {
+                        unpublished_msgs.push(msg);
+                    } else {
+                        let not_expired = msg.deadline_at.as_ref().is_none_or(|exp| exp > &now);
+                        if not_expired {
+                            let rcpts = store.list_wire_receipts(&msg.id)?;
+                            if rcpts.iter().any(|r| {
+                                matches!(
+                                    r.status,
+                                    DeliveryStatus::Pending | DeliveryStatus::Delivered
+                                )
+                            }) {
+                                unpublished_msgs.push(msg);
+                            }
+                        }
+                    }
+                }
+
+                let mut exported_msg_ids = remote_msg_ids;
+                for m in &unpublished_msgs {
+                    exported_msg_ids.insert(m.id.clone());
+                }
+
+                let mut remote_rcpt_states: BTreeMap<(MessageId, Iri), DeliveryStatus> =
+                    BTreeMap::new();
+                for r in &base_snapshot.wire_receipts {
+                    let key = (r.message_id.clone(), r.recipient_id.clone());
+                    let entry = remote_rcpt_states.entry(key).or_insert(r.status);
+                    if r.status.rank() > entry.rank() {
+                        *entry = r.status;
+                    }
+                }
+
+                let local_rcpts = store.wire_receipts_after(cursor.last_wire_rcpt_seq)?;
+                let max_rcpt_seq = local_rcpts
+                    .last()
+                    .map(|(seq, _)| *seq)
+                    .unwrap_or(cursor.last_wire_rcpt_seq);
+                let mut unpublished_rcpts = Vec::new();
+                for (_, rcpt) in local_rcpts {
+                    if !exported_msg_ids.contains(&rcpt.message_id) {
+                        continue;
+                    }
+                    let key = (rcpt.message_id.clone(), rcpt.recipient_id.clone());
+                    if remote_rcpt_states
+                        .get(&key)
+                        .is_none_or(|rem_state| rcpt.status.rank() > rem_state.rank())
+                    {
+                        remote_rcpt_states.insert(key, rcpt.status);
+                        unpublished_rcpts.push(rcpt);
+                    }
+                }
+
+                (unpublished_msgs, unpublished_rcpts, max_msg_seq, max_rcpt_seq)
+            } else {
+                (
+                    Vec::new(),
+                    Vec::new(),
+                    cursor.last_wire_msg_seq,
+                    cursor.last_wire_rcpt_seq,
+                )
+            };
+
+        if unpublished_revs.is_empty()
+            && unpublished_reg_events.is_empty()
+            && unpublished_wire_msgs.is_empty()
+            && unpublished_wire_rcpts.is_empty()
+        {
             let new_cursor = StateSyncCursor {
                 remote: cursor_key,
                 branch: target.branch.clone(),
                 last_pushed_commit: remote_tip.clone(),
                 last_revision_seq: max_rev_seq,
                 last_registry_seq: max_reg_seq,
+                last_wire_msg_seq: max_wire_msg_seq,
+                last_wire_rcpt_seq: max_wire_rcpt_seq,
             };
             store.set_sync_cursor(new_cursor)?;
             let active_conflicts = store.list_conflicts()?.len();
@@ -608,8 +923,12 @@ where
                 committed_oid: remote_tip,
                 imported_registry_events,
                 imported_revisions,
+                imported_wire_messages,
+                imported_wire_receipts,
                 published_registry_events: 0,
                 published_revisions: 0,
+                published_wire_messages: 0,
+                published_wire_receipts: 0,
                 active_conflicts,
             });
         }
@@ -637,6 +956,17 @@ where
         for rev in &unpublished_revs {
             append_revision_to_segments(&mut next_snapshot.segments, rev);
         }
+        if target.sync_wire {
+            for msg in &unpublished_wire_msgs {
+                let rel_path = wire_message_rel_path(&msg.id);
+                let mut raw = serde_json::to_vec(msg).unwrap();
+                raw.push(b'\n');
+                next_snapshot.wire_messages_raw.insert(rel_path, raw);
+            }
+            for rcpt in &unpublished_wire_rcpts {
+                append_receipt_to_segments(&mut next_snapshot.wire_receipt_segments, rcpt);
+            }
+        }
 
         let tree_oid = write_snapshot_tree(&target.repo_dir, &next_snapshot)?;
         let author = if target.by.trim().is_empty() {
@@ -668,6 +998,8 @@ where
                     last_pushed_commit: Some(commit_oid.clone()),
                     last_revision_seq: max_rev_seq,
                     last_registry_seq: max_reg_seq,
+                    last_wire_msg_seq: max_wire_msg_seq,
+                    last_wire_rcpt_seq: max_wire_rcpt_seq,
                 };
                 store.set_sync_cursor(new_cursor)?;
                 let active_conflicts = store.list_conflicts()?.len();
@@ -678,8 +1010,12 @@ where
                     committed_oid: Some(commit_oid),
                     imported_registry_events,
                     imported_revisions,
+                    imported_wire_messages,
+                    imported_wire_receipts,
                     published_registry_events: unpublished_reg_events.len(),
                     published_revisions: unpublished_revs.len(),
+                    published_wire_messages: unpublished_wire_msgs.len(),
+                    published_wire_receipts: unpublished_wire_rcpts.len(),
                     active_conflicts,
                 });
             }
@@ -699,7 +1035,7 @@ pub fn verify_state<S>(
     target: &StateSyncTarget,
 ) -> Result<StateVerifyReport, StateSyncError>
 where
-    S: StateStore + AgentRegistry,
+    S: StateStore + AgentRegistry + MessageStore,
 {
     let local_tracking_ref = format!("refs/fl-sync/{}/{}", target.remote, target.branch);
     let cursor_key = target.cursor_key();
@@ -723,6 +1059,8 @@ where
             verified_commits: 0,
             verified_revisions: 0,
             verified_registry_events: 0,
+            verified_wire_messages: 0,
+            verified_wire_receipts: 0,
         });
     };
 
@@ -807,6 +1145,19 @@ where
         }
     }
 
+    if target.sync_wire {
+        for msg in &prev.wire_messages {
+            if let Some(local) = store.get_wire_message(&msg.id)?
+                && &local != msg
+            {
+                return Err(StateSyncError::Store(StoreError::WireMessageTampered {
+                    id: msg.id.clone(),
+                    detail: "remote wire message differs from local stored copy".into(),
+                }));
+            }
+        }
+    }
+
     // Verify every local state entry's head_revision exists in its revision history.
     for entry in store.list_state(target.scope_filter.as_ref(), None)? {
         let revs = store.state_revisions(&entry.id)?;
@@ -828,6 +1179,8 @@ where
         verified_commits: commits.len(),
         verified_revisions: prev.revisions.len(),
         verified_registry_events: prev.registry_events.len(),
+        verified_wire_messages: prev.wire_messages.len(),
+        verified_wire_receipts: prev.wire_receipts.len(),
     })
 }
 
@@ -920,6 +1273,7 @@ mod tests {
             branch: "fl/state".into(),
             private: false,
             sync_registry: true,
+            sync_wire: true,
             scope_filter: None,
             by: "ferris".into(),
         };
@@ -933,6 +1287,7 @@ mod tests {
             branch: "fl/state".into(),
             private: false,
             sync_registry: true,
+            sync_wire: true,
             scope_filter: None,
             by: "ferris".into(),
         };
@@ -977,6 +1332,7 @@ mod tests {
             branch: "fl/state".into(),
             private: false,
             sync_registry: false,
+            sync_wire: false,
             scope_filter: None,
             by: "ferris".into(),
         };
@@ -1029,6 +1385,7 @@ mod tests {
             branch: "fl/state".into(),
             private: false,
             sync_registry: false,
+            sync_wire: false,
             scope_filter: None,
             by: "jarvis".into(),
         };
@@ -1045,6 +1402,7 @@ mod tests {
             branch: "fl/state".into(),
             private: true,
             sync_registry: false,
+            sync_wire: false,
             scope_filter: None,
             by: "jarvis".into(),
         };
@@ -1082,6 +1440,7 @@ mod tests {
             branch: "fl/state".into(),
             private: false,
             sync_registry: false,
+            sync_wire: false,
             scope_filter: None,
             by: "agent_a".into(),
         };
@@ -1114,6 +1473,7 @@ mod tests {
             branch: "fl/state".into(),
             private: false,
             sync_registry: false,
+            sync_wire: false,
             scope_filter: None,
             by: "agent_b".into(),
         };
@@ -1158,6 +1518,7 @@ mod tests {
             branch: "fl/state".into(),
             private: false,
             sync_registry: false,
+            sync_wire: false,
             scope_filter: None,
             by: "agent_a".into(),
         };
@@ -1207,6 +1568,7 @@ mod tests {
             branch: "fl/state".into(),
             private: false,
             sync_registry: false,
+            sync_wire: false,
             scope_filter: None,
             by: "agent_a".into(),
         };
@@ -1305,6 +1667,7 @@ mod tests {
             branch: "fl/state".into(),
             private: false,
             sync_registry: false,
+            sync_wire: false,
             scope_filter: None,
             by: "agent_a".into(),
         };
@@ -1344,4 +1707,244 @@ mod tests {
         .unwrap();
         assert_eq!(outcome, PushCasOutcome::Rejected);
     }
+
+    #[test]
+    fn selective_wire_export_and_offline_store_and_forward_round_trip() {
+        use fl_core::ferrowire::{
+            MessageKind, MessageUrgency, SendWireArgs, WireAddress, WireFilter,
+        };
+
+        let (_tmp, clone_a, clone_b) = setup_two_clones();
+        let store_a = MemStore::default();
+        let store_b = MemStore::default();
+
+        // 1. Locally acked transient message on Store A -> skipped by selective export policy.
+        let (acked_transient, _) = store_a
+            .send_wire(SendWireArgs {
+                id: None,
+                sender_id: None,
+                sender_name: "jarvis".into(),
+                sender_host: None,
+                to: WireAddress::AgentName("kayla".into()),
+                kind: MessageKind::Mail,
+                urgency: MessageUrgency::Normal,
+                durable: false,
+                subject: "local ping".into(),
+                body: "already acked locally".into(),
+                reply_to: None,
+                thread_id: None,
+                deadline_at: None,
+                at: At::from_unix_millis(10),
+            })
+            .unwrap();
+        store_a
+            .update_wire_receipt(
+                &acked_transient.id,
+                "kayla",
+                DeliveryStatus::Acked,
+                false,
+                At::from_unix_millis(11),
+            )
+            .unwrap();
+
+        // 2. Expired transient message on Store A -> skipped by selective export policy.
+        let (_expired_transient, _) = store_a
+            .send_wire(SendWireArgs {
+                id: None,
+                sender_id: None,
+                sender_name: "jarvis".into(),
+                sender_host: None,
+                to: WireAddress::AgentName("kayla".into()),
+                kind: MessageKind::Mail,
+                urgency: MessageUrgency::Normal,
+                durable: false,
+                subject: "expired ping".into(),
+                body: "expired before sync".into(),
+                reply_to: None,
+                thread_id: None,
+                deadline_at: Some(At::from_unix_millis(15)),
+                at: At::from_unix_millis(12),
+            })
+            .unwrap();
+
+        // 3. Unacknowledged non-expired transient message -> exported.
+        let (pending_transient, _) = store_a
+            .send_wire(SendWireArgs {
+                id: None,
+                sender_id: None,
+                sender_name: "jarvis".into(),
+                sender_host: None,
+                to: WireAddress::AgentName("kayla".into()),
+                kind: MessageKind::Mail,
+                urgency: MessageUrgency::Normal,
+                durable: false,
+                subject: "offline ping".into(),
+                body: "deliver via git".into(),
+                reply_to: None,
+                thread_id: None,
+                deadline_at: Some(At::from_unix_millis(500)),
+                at: At::from_unix_millis(13),
+            })
+            .unwrap();
+
+        // 4. Durable message (even if already acked) -> exported.
+        let (durable_msg, _) = store_a
+            .send_wire(SendWireArgs {
+                id: None,
+                sender_id: None,
+                sender_name: "jarvis".into(),
+                sender_host: None,
+                to: WireAddress::AgentName("kayla".into()),
+                kind: MessageKind::Handoff,
+                urgency: MessageUrgency::Urgent,
+                durable: true,
+                subject: "durable handoff".into(),
+                body: "permanent record".into(),
+                reply_to: None,
+                thread_id: None,
+                deadline_at: None,
+                at: At::from_unix_millis(14),
+            })
+            .unwrap();
+
+        let target_a = StateSyncTarget {
+            repo_dir: clone_a.clone(),
+            remote: "origin".into(),
+            branch: "fl/state".into(),
+            private: false,
+            sync_registry: true,
+            sync_wire: true,
+            scope_filter: None,
+            by: "jarvis".into(),
+        };
+        let rep_a = sync_state(&store_a, &target_a, At::from_unix_millis(20)).unwrap();
+        assert_eq!(rep_a.published_wire_messages, 2);
+        assert_eq!(rep_a.published_wire_receipts, 2);
+
+        // Store B syncs from origin and receives only the 2 exported messages.
+        let target_b = StateSyncTarget {
+            repo_dir: clone_b,
+            remote: "origin".into(),
+            branch: "fl/state".into(),
+            private: false,
+            sync_registry: true,
+            sync_wire: true,
+            scope_filter: None,
+            by: "kayla".into(),
+        };
+        let rep_b = sync_state(&store_b, &target_b, At::from_unix_millis(25)).unwrap();
+        assert_eq!(rep_b.imported_wire_messages, 2);
+        assert_eq!(rep_b.imported_wire_receipts, 2);
+        assert_eq!(rep_b.published_wire_messages, 0);
+        assert_eq!(rep_b.published_wire_receipts, 0);
+        assert!(store_b.get_wire_message(&acked_transient.id).unwrap().is_none());
+        assert!(store_b.get_wire_message(&pending_transient.id).unwrap().is_some());
+        assert!(store_b.get_wire_message(&durable_msg.id).unwrap().is_some());
+
+        let inbox = store_b
+            .list_wire_inbox(&WireFilter {
+                recipient: Some("kayla".into()),
+                unread_only: true,
+                thread_id: None,
+                correlation_id: None,
+                limit: None,
+            })
+            .unwrap();
+        assert_eq!(inbox.len(), 2);
+
+        // Kayla acknowledges `pending_transient` on Store B and syncs back to origin.
+        store_b
+            .update_wire_receipt(
+                &pending_transient.id,
+                "kayla",
+                DeliveryStatus::Acked,
+                false,
+                At::from_unix_millis(35),
+            )
+            .unwrap();
+        let rep_b2 = sync_state(&store_b, &target_b, At::from_unix_millis(40)).unwrap();
+        assert_eq!(rep_b2.published_wire_messages, 0);
+        assert_eq!(rep_b2.published_wire_receipts, 1);
+
+        // Store A syncs and observes Kayla's Acked receipt.
+        let rep_a2 = sync_state(&store_a, &target_a, At::from_unix_millis(45)).unwrap();
+        assert_eq!(rep_a2.imported_wire_receipts, 3);
+        let rcpts_a = store_a.list_wire_receipts(&pending_transient.id).unwrap();
+        assert_eq!(rcpts_a[0].status, DeliveryStatus::Acked);
+
+        let ver = verify_state(&store_a, &target_a).unwrap();
+        assert_eq!(ver.verified_commits, 2);
+        assert_eq!(ver.verified_wire_messages, 2);
+        assert_eq!(ver.verified_wire_receipts, 3);
+    }
+
+    #[test]
+    fn wire_message_tamper_on_branch_is_detected() {
+        use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
+
+        let (_tmp, clone_a, _clone_b) = setup_two_clones();
+        let store = MemStore::default();
+        let (msg, _) = store
+            .send_wire(SendWireArgs {
+                id: None,
+                sender_id: None,
+                sender_name: "jarvis".into(),
+                sender_host: None,
+                to: WireAddress::AgentName("kayla".into()),
+                kind: MessageKind::Mail,
+                urgency: MessageUrgency::Normal,
+                durable: true,
+                subject: "immutable".into(),
+                body: "original body".into(),
+                reply_to: None,
+                thread_id: None,
+                deadline_at: None,
+                at: At::from_unix_millis(10),
+            })
+            .unwrap();
+
+        let target = StateSyncTarget {
+            repo_dir: clone_a.clone(),
+            remote: "origin".into(),
+            branch: "fl/state".into(),
+            private: false,
+            sync_registry: false,
+            sync_wire: true,
+            scope_filter: None,
+            by: "jarvis".into(),
+        };
+        let rep = sync_state(&store, &target, At::from_unix_millis(15)).unwrap();
+        let parent_commit = rep.committed_oid.unwrap();
+
+        // Craft a commit on origin that deletes `wire/messages/...`
+        let bare = clone_a.parent().unwrap().join("remote.git");
+        let format_oid = Git::hash_object(&bare, b"1\n").unwrap();
+        let root_tree = Git::mktree(
+            &bare,
+            &[GitTreeEntry {
+                mode: "100644".into(),
+                kind: "blob".into(),
+                oid: format_oid,
+                path: "format".into(),
+            }],
+        )
+        .unwrap();
+        let tampered_commit = Git::commit_tree(
+            &bare,
+            &root_tree,
+            Some(&parent_commit),
+            "deleted wire message",
+            "attacker",
+        )
+        .unwrap();
+        Command::new("git")
+            .current_dir(&bare)
+            .args(["update-ref", "refs/heads/fl/state", &tampered_commit])
+            .status()
+            .unwrap();
+
+        let err = verify_state(&store, &target).unwrap_err();
+        assert!(matches!(err, StateSyncError::HistoryTampered { .. }));
+        let _ = msg;
+    }
 }
```

- [ ] **Step 2: Run `fl-exec` tests**

Run:
```bash
cargo test -p fl-exec --offline
```
Expected: PASS (96 tests pass, including `selective_wire_export_and_offline_store_and_forward_round_trip` and `wire_message_tamper_on_branch_is_detected`).


---

## Task 5: `fl-wire` Crate (Identity, Dispatcher, RFC 6455 WebSocket/TLS, Peer Protocol, Local Daemon)

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/wire/Cargo.toml`
- Create: `crates/wire/src/lib.rs`
- Create: `crates/wire/src/identity.rs`
- Create: `crates/wire/src/dispatcher.rs`
- Create: `crates/wire/src/ws.rs`
- Create: `crates/wire/src/peer.rs`
- Create: `crates/wire/src/daemon.rs`

**Interfaces:**
- Consumes: `fl_core::ferrowire::*`, `fl_core::store::{AgentRegistry, MessageStore}`, `fl_store::RedbStore`, `ring`, `rustls`, `sha2`, `base64`
- Produces: `fl_wire::{identity, dispatcher, ws, peer, daemon}`

- [ ] **Step 1: Add `crates/wire` to workspace `Cargo.toml` and create `crates/wire/Cargo.toml` and `crates/wire/src/lib.rs`**

Apply this diff to `Cargo.toml`:

```diff
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -1,5 +1,5 @@
 [workspace]
-members = ["crates/core", "crates/store", "crates/exec", "crates/cli", "crates/github", "crates/mcp"]
+members = ["crates/core", "crates/store", "crates/exec", "crates/cli", "crates/github", "crates/mcp", "crates/wire"]
 resolver = "3"
 
 [workspace.package]
@@ -33,3 +33,6 @@ base64 = "0.22"
 tiny_http = "0.12"
 toml_edit = "0.25"
 indexmap = { version = "2.14", features = ["serde"] }
+rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12", "logging"] }
+rustls-pki-types = "1.15"
+webpki-roots = "1.0"
```

Write `crates/wire/Cargo.toml`:

```toml
[package]
name = "fl-wire"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
anyhow.workspace = true
base64.workspace = true
fl-core = { path = "../core" }
fl-store = { path = "../store" }
ring.workspace = true
rustls.workspace = true
rustls-pki-types.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
thiserror.workspace = true
webpki-roots.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

Write `crates/wire/src/lib.rs`:

```rust
//! FerroWire local Unix-socket daemon, RFC 6455 WebSocket/TLS peer transport,
//! sender identity resolution, and native vendor push dispatcher (spec §3).

pub mod daemon;
pub mod dispatcher;
pub mod identity;
pub mod peer;
pub mod ws;
```

- [ ] **Step 2: Create `crates/wire/src/identity.rs`**

Write `crates/wire/src/identity.rs`:

```rust
//! Sender persona resolution, live vendor route table, and passive route discovery (spec §3.1).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use fl_core::agent::{AgentRole, AgentStatus};
use fl_core::at::At;
use fl_core::state::validate_slug;
use fl_core::store::AgentRegistry;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VendorRoute {
    Claude {
        pid: u32,
        session_id: String,
        socket_path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_path: Option<PathBuf>,
    },
    Agy {
        conversation_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ls_pid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ls_address: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        csrf_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agentapi_exe: Option<PathBuf>,
    },
    Codex {
        thread_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },
    /// Used by deterministic integration tests to capture pushed deliveries without vendor binaries.
    Webhook { socket_path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteEntry {
    pub persona: String,
    pub route: VendorRoute,
    pub updated_at: At,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteTable {
    pub entries: BTreeMap<String, RouteEntry>,
}

pub fn default_routes_path() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(rt);
        if p.is_absolute() {
            return p.join("fl-wire-routes.json");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home);
        if p.is_absolute() {
            return p.join(".cache").join("fl").join("wire-routes.json");
        }
    }
    PathBuf::from("/tmp/fl-wire-routes.json")
}

pub fn legacy_routes_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let p = PathBuf::from(home);
    if p.is_absolute() {
        Some(p.join(".cache").join("agent-bus").join("routes.json"))
    } else {
        None
    }
}

impl RouteTable {
    pub fn load(custom_path: Option<&Path>) -> Self {
        let primary = custom_path
            .map(Path::to_path_buf)
            .unwrap_or_else(default_routes_path);
        let mut table = Self::load_file(&primary).unwrap_or_default();
        if custom_path.is_none()
            && let Some(legacy) = legacy_routes_path()
            && let Some(legacy_table) = Self::load_file(&legacy)
        {
            for (k, v) in legacy_table.entries {
                table.entries.entry(k).or_insert(v);
            }
        }
        table
    }

    fn load_file(path: &Path) -> Option<Self> {
        let raw = fs::read_to_string(path).ok()?;
        if let Ok(table) = serde_json::from_str::<RouteTable>(&raw) {
            return Some(table);
        }
        // Also support flat map `{ "<persona>": RouteEntry }` or legacy agent-bus route map
        if let Ok(entries) = serde_json::from_str::<BTreeMap<String, RouteEntry>>(&raw) {
            return Some(RouteTable { entries });
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw)
            && let Some(obj) = val.as_object()
        {
            let mut entries = BTreeMap::new();
            for (persona, item) in obj {
                let Some(norm) = normalize_persona(persona) else {
                    continue;
                };
                if let Some(route) = parse_legacy_route_item(item) {
                    entries.insert(
                        norm.clone(),
                        RouteEntry {
                            persona: norm,
                            route,
                            updated_at: At::from_unix_millis(0),
                        },
                    );
                }
            }
            if !entries.is_empty() {
                return Some(RouteTable { entries });
            }
        }
        None
    }

    pub fn save(&self, custom_path: Option<&Path>) -> std::io::Result<()> {
        let target = custom_path
            .map(Path::to_path_buf)
            .unwrap_or_else(default_routes_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = target.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
            }
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &target)?;
        Ok(())
    }

    pub fn upsert(&mut self, persona: &str, route: VendorRoute, at: At) {
        if let Some(norm) = normalize_persona(persona) {
            self.entries.insert(
                norm.clone(),
                RouteEntry {
                    persona: norm,
                    route,
                    updated_at: at,
                },
            );
        }
    }

    pub fn get(&self, persona: &str) -> Option<&RouteEntry> {
        let norm = normalize_persona(persona)?;
        self.entries.get(&norm)
    }

    /// Passively discover Claude, Antigravity, and Codex routes from vendor session files.
    pub fn discover_routes(&mut self, home_override: Option<&Path>, at: At) {
        let home = home_override
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        let Some(home) = home else {
            return;
        };
        let check_proc = home_override.is_none();
        self.discover_claude_routes(&home, check_proc, &at);
        self.discover_agy_routes(&home, &at);
        self.discover_codex_routes(&home, &at);
    }

    fn discover_claude_routes(&mut self, home: &Path, check_proc: bool, at: &At) {
        let sessions_dir = home.join(".claude").join("sessions");
        let Ok(read_dir) = fs::read_dir(&sessions_dir) else {
            return;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(pid) = stem.parse::<u32>() else {
                continue;
            };
            if check_proc && !Path::new(&format!("/proc/{pid}")).exists() {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let Some(name) = val.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(persona) = normalize_claude_session_name(name) else {
                continue;
            };
            let Some(sock_str) = val.get("messagingSocketPath").and_then(|v| v.as_str()) else {
                continue;
            };
            let socket_path = PathBuf::from(sock_str);
            let session_id = val
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let key_path = resolve_claude_key_path(&sessions_dir, pid, &socket_path);
            self.upsert(
                &persona,
                VendorRoute::Claude {
                    pid,
                    session_id,
                    socket_path,
                    key_path,
                },
                at.clone(),
            );
        }
    }

    fn discover_agy_routes(&mut self, home: &Path, at: &At) {
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        let mut shared_ls_addr: Option<String> = None;
        let mut shared_csrf: Option<String> = None;
        let mut shared_exe: Option<PathBuf> = None;

        for entry in self.entries.values() {
            if let VendorRoute::Agy {
                ls_address,
                csrf_token,
                agentapi_exe,
                ..
            } = &entry.route
            {
                if shared_ls_addr.is_none() {
                    shared_ls_addr = ls_address.clone();
                }
                if shared_csrf.is_none() {
                    shared_csrf = csrf_token.clone();
                }
                if shared_exe.is_none() {
                    shared_exe = agentapi_exe.clone();
                }
            }
        }

        if shared_ls_addr.is_none()
            && let Ok(log_text) = fs::read_to_string(agy_dir.join("cli.log"))
        {
            for line in log_text.lines().rev() {
                if let Some(idx) = line.find("Language server listening on random port at ") {
                    let rest = &line[idx + "Language server listening on random port at ".len()..];
                    let port: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if !port.is_empty() {
                        shared_ls_addr = Some(format!("127.0.0.1:{port}"));
                        break;
                    }
                }
            }
        }

        if (shared_csrf.is_none() || shared_ls_addr.is_none())
            && let Ok(proc_dir) = fs::read_dir("/proc")
        {
            for entry in proc_dir.flatten() {
                let name = entry.file_name();
                let Some(s) = name.to_str() else {
                    continue;
                };
                if !s.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let Ok(env_bytes) = fs::read(entry.path().join("environ")) else {
                    continue;
                };
                for kv in env_bytes.split(|&b| b == 0) {
                    if let Ok(kv_str) = std::str::from_utf8(kv) {
                        if shared_csrf.is_none()
                            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_CSRF_TOKEN=")
                            && !val.is_empty()
                        {
                            shared_csrf = Some(val.to_string());
                        } else if shared_ls_addr.is_none()
                            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_LS_ADDRESS=")
                            && !val.is_empty()
                        {
                            shared_ls_addr = Some(val.to_string());
                        } else if shared_exe.is_none()
                            && let Some(val) = kv_str.strip_prefix("ANTIGRAVITY_AGENTAPI_EXE=")
                            && !val.is_empty()
                        {
                            shared_exe = Some(PathBuf::from(val));
                        }
                    }
                }
                if shared_csrf.is_some() && shared_ls_addr.is_some() {
                    break;
                }
            }
        }

        // Backfill shared ls_address / csrf_token onto existing Agy routes if missing
        for entry in self.entries.values_mut() {
            if let VendorRoute::Agy {
                ls_address,
                csrf_token,
                agentapi_exe,
                ..
            } = &mut entry.route
            {
                if ls_address.is_none() {
                    *ls_address = shared_ls_addr.clone();
                }
                if csrf_token.is_none() {
                    *csrf_token = shared_csrf.clone();
                }
                if agentapi_exe.is_none() {
                    *agentapi_exe = shared_exe.clone();
                }
            }
        }

        // Scan brain/*/transcript.jsonl for persona registrations
        let brain_dir = agy_dir.join("brain");
        let Ok(read_dir) = fs::read_dir(&brain_dir) else {
            return;
        };
        for entry in read_dir.flatten() {
            let conv_dir = entry.path();
            let Some(conv_id) = conv_dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let transcript = conv_dir
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            if let Some(persona) = scan_transcript_for_persona(&transcript, 50)
                && !self.entries.contains_key(&persona)
            {
                self.upsert(
                    &persona,
                    VendorRoute::Agy {
                        conversation_id: conv_id.to_string(),
                        ls_pid: None,
                        ls_address: shared_ls_addr.clone(),
                        csrf_token: shared_csrf.clone(),
                        agentapi_exe: shared_exe.clone(),
                    },
                    at.clone(),
                );
            }
        }
    }

    fn discover_codex_routes(&mut self, home: &Path, at: &At) {
        let index_path = home.join(".codex").join("session_index.jsonl");
        let Ok(text) = fs::read_to_string(&index_path) else {
            return;
        };
        for line in text.lines() {
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(thread_id) = val
                .get("id")
                .or_else(|| val.get("thread_id"))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let persona_raw = val
                .get("agentName")
                .or_else(|| val.get("persona"))
                .or_else(|| val.get("thread_name"))
                .or_else(|| val.get("title"))
                .and_then(|v| v.as_str());
            let Some(persona) = persona_raw.and_then(normalize_persona) else {
                continue;
            };
            if matches!(
                persona.as_str(),
                "exit" | "quit" | "help" | "clear" | "status" | "compact" | "unknown" | "default"
            ) {
                continue;
            }
            let cwd = val.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
            self.upsert(
                &persona,
                VendorRoute::Codex {
                    thread_id: thread_id.to_string(),
                    cwd,
                },
                at.clone(),
            );
        }
    }
}

fn parse_legacy_route_item(item: &serde_json::Value) -> Option<VendorRoute> {
    let vendor = item
        .get("vendor")
        .or_else(|| item.get("kind"))
        .and_then(|v| v.as_str())?;
    match vendor {
        "claude" => {
            let pid = item.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let session_id = item
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let socket_path = PathBuf::from(item.get("socket_path")?.as_str()?);
            let key_path = item
                .get("key_path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            Some(VendorRoute::Claude {
                pid,
                session_id,
                socket_path,
                key_path,
            })
        }
        "agy" | "antigravity" => {
            let conversation_id = item.get("conversation_id")?.as_str()?.to_string();
            let ls_pid = item
                .get("ls_pid")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32);
            let ls_address = item
                .get("ls_address")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned);
            let csrf_token = item
                .get("csrf_token")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned);
            let agentapi_exe = item
                .get("agentapi_exe")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            Some(VendorRoute::Agy {
                conversation_id,
                ls_pid,
                ls_address,
                csrf_token,
                agentapi_exe,
            })
        }
        "codex" => {
            let thread_id = item.get("thread_id")?.as_str()?.to_string();
            let cwd = item.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
            Some(VendorRoute::Codex { thread_id, cwd })
        }
        "webhook" => {
            let socket_path = PathBuf::from(item.get("socket_path")?.as_str()?);
            Some(VendorRoute::Webhook { socket_path })
        }
        _ => None,
    }
}

pub fn resolve_claude_key_path(
    sessions_dir: &Path,
    pid: u32,
    socket_path: &Path,
) -> Option<PathBuf> {
    let canonical = fs::canonicalize(socket_path).unwrap_or_else(|_| socket_path.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    let exact = sessions_dir.join(format!("{pid}.{hex}.key"));
    if exact.exists() {
        return Some(exact);
    }
    // Fallback: any `<pid>.*.key` in `sessions_dir`
    let prefix = format!("{pid}.");
    if let Ok(read_dir) = fs::read_dir(sessions_dir) {
        for entry in read_dir.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str())
                && name.starts_with(&prefix)
                && name.ends_with(".key")
            {
                return Some(p);
            }
        }
    }
    None
}

pub fn normalize_persona(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    validate_slug(trimmed).ok()
}

pub fn normalize_claude_session_name(raw: &str) -> Option<String> {
    let norm = normalize_persona(raw)?;
    if norm.starts_with("artificial-humanity-") {
        return None;
    }
    Some(norm)
}

pub fn scan_transcript_for_persona(path: &Path, max_lines: usize) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines().take(max_lines) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            for field in ["agentName", "customTitle", "persona"] {
                if let Some(s) = val.get(field).and_then(|v| v.as_str())
                    && let Some(norm) = normalize_claude_session_name(s)
                {
                    return Some(norm);
                }
            }
        }
        let lower = line.to_ascii_lowercase();
        for marker in ["take the name of ", "you will be ", "take up residence in "] {
            if let Some(idx) = lower.find(marker) {
                let after = &line[idx + marker.len()..];
                let token: String = after
                    .trim_start_matches(['"', '\'', '`', ' '])
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
                if let Some(norm) = normalize_claude_session_name(&token) {
                    return Some(norm);
                }
            }
        }
    }
    None
}

/// Capture the current process's vendor environment route so `fl wire` / `agent-msg`
/// can register it in the route table automatically.
pub fn capture_current_env_route() -> Option<VendorRoute> {
    if let Some(sock) = std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET") {
        let socket_path = PathBuf::from(sock);
        let pid = socket_path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or_else(std::process::id);
        let session_id = std::env::var("CLAUDE_SESSION_ID").unwrap_or_default();
        let key_path = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|h| resolve_claude_key_path(&h.join(".claude").join("sessions"), pid, &socket_path));
        return Some(VendorRoute::Claude {
            pid,
            session_id,
            socket_path,
            key_path,
        });
    }
    if let Ok(conv_id) = std::env::var("ANTIGRAVITY_CONVERSATION_ID")
        && !conv_id.trim().is_empty()
    {
        return Some(VendorRoute::Agy {
            conversation_id: conv_id,
            ls_pid: None,
            ls_address: std::env::var("ANTIGRAVITY_LS_ADDRESS").ok(),
            csrf_token: std::env::var("ANTIGRAVITY_CSRF_TOKEN").ok(),
            agentapi_exe: std::env::var_os("ANTIGRAVITY_AGENTAPI_EXE").map(PathBuf::from),
        });
    }
    if let Ok(thread_id) = std::env::var("CODEX_THREAD_ID")
        && !thread_id.trim().is_empty()
    {
        return Some(VendorRoute::Codex {
            thread_id,
            cwd: std::env::current_dir().ok(),
        });
    }
    None
}

/// Resolve the caller's persona name in strict priority order (spec §3.1):
/// 1. Explicit flag/argument (`explicit`)
/// 2. `AGENT_PERSONA` or `AGENT_NAME` environment variable
/// 3. Vendor transcript / session / route-cache inspection
/// 4. CWD ancestor project lead lookup in `AgentRegistry`
/// 5. `"unknown"`
pub fn resolve_persona(
    explicit: Option<&str>,
    transcript_path: Option<&Path>,
    cwd: &Path,
    registry: Option<&dyn AgentRegistry>,
    custom_routes_path: Option<&Path>,
) -> String {
    if let Some(exp) = explicit
        && let Some(norm) = normalize_persona(exp)
    {
        return norm;
    }

    for var in ["AGENT_PERSONA", "AGENT_NAME"] {
        if let Ok(val) = std::env::var(var)
            && let Some(norm) = normalize_persona(&val)
        {
            return norm;
        }
    }

    if let Some(tp) = transcript_path
        && let Some(norm) = scan_transcript_for_persona(tp, 50)
    {
        return norm;
    }

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Some(sock) = std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET")
    {
        let sock_path = PathBuf::from(sock);
        if let Some(pid) = sock_path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
            && let Some(norm) = resolve_claude_pid_persona(&home, pid)
        {
            return norm;
        }
    }

    let routes = RouteTable::load(custom_routes_path);
    if let Ok(claude_sid) = std::env::var("CLAUDE_SESSION_ID")
        && !claude_sid.is_empty()
    {
        for entry in routes.entries.values() {
            if let VendorRoute::Claude { session_id, .. } = &entry.route
                && session_id == &claude_sid
            {
                return entry.persona.clone();
            }
        }
    }
    if let Ok(agy_cid) = std::env::var("ANTIGRAVITY_CONVERSATION_ID")
        && !agy_cid.is_empty()
    {
        for entry in routes.entries.values() {
            if let VendorRoute::Agy {
                conversation_id, ..
            } = &entry.route
                && conversation_id == &agy_cid
            {
                return entry.persona.clone();
            }
        }
    }
    if let Ok(codex_tid) = std::env::var("CODEX_THREAD_ID")
        && !codex_tid.is_empty()
    {
        for entry in routes.entries.values() {
            if let VendorRoute::Codex { thread_id, .. } = &entry.route
                && thread_id == &codex_tid
            {
                return entry.persona.clone();
            }
        }
    }

    if let Some(reg) = registry
        && let Some(lead) = resolve_cwd_lead(cwd, reg)
    {
        return lead;
    }

    if let Some(persona) = resolve_cwd_persona_md(cwd) {
        return persona;
    }

    "unknown".to_string()
}

fn resolve_claude_pid_persona(home: &Path, start_pid: u32) -> Option<String> {
    let sessions_dir = home.join(".claude").join("sessions");
    let projects_dir = home.join(".claude").join("projects");
    let mut pid = start_pid;
    for _ in 0..8 {
        if pid <= 1 {
            break;
        }
        let session_file = sessions_dir.join(format!("{pid}.json"));
        if let Ok(raw) = fs::read_to_string(&session_file)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw)
            && let Some(name) = val.get("name").and_then(|v| v.as_str())
            && let Some(norm) = normalize_claude_session_name(name)
        {
            return Some(norm);
        }
        if let Ok(cmdline_bytes) = fs::read(format!("/proc/{pid}/cmdline")) {
            let args: Vec<&str> = cmdline_bytes
                .split(|&b| b == 0)
                .filter_map(|s| std::str::from_utf8(s).ok())
                .collect();
            for win in args.windows(2) {
                if (win[0] == "--session-id" || win[0] == "--resume") && !win[1].is_empty() {
                    let target_fname = format!("{}.jsonl", win[1]);
                    if let Ok(proj_dirs) = fs::read_dir(&projects_dir) {
                        for pentry in proj_dirs.flatten() {
                            let candidate = pentry.path().join(&target_fname);
                            if candidate.exists()
                                && let Some(norm) = scan_transcript_for_persona(&candidate, 50)
                            {
                                return Some(norm);
                            }
                        }
                    }
                }
            }
        }
        pid = parent_pid_of(pid)?;
    }
    None
}

fn resolve_cwd_persona_md(cwd: &Path) -> Option<String> {
    for ancestor in cwd.ancestors() {
        let p = ancestor.join("PERSONA.md");
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        };
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("You are ") {
                let candidate: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
                if !candidate.eq_ignore_ascii_case("the")
                    && !candidate.eq_ignore_ascii_case("a")
                    && !candidate.eq_ignore_ascii_case("an")
                    && let Some(norm) = normalize_persona(&candidate)
                {
                    return Some(norm);
                }
            }
        }
    }
    None
}

fn parent_pid_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = stat.rfind(')')?;
    let after = stat.get(rparen + 2..)?;
    let mut parts = after.split_whitespace();
    let _state = parts.next()?;
    let ppid = parts.next()?.parse::<u32>().ok()?;
    Some(ppid)
}

fn resolve_cwd_lead(cwd: &Path, registry: &dyn AgentRegistry) -> Option<String> {
    let areas = registry.list_areas().ok()?;
    let agents = registry.list_agents(None, false).ok()?;
    for ancestor in cwd.ancestors() {
        let Some(base) = ancestor.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let norm_base = base.to_ascii_lowercase();
        for area in &areas {
            let repo_match = area.path.as_ref().is_some_and(|rp| {
                let trimmed = rp.trim_end_matches('/');
                trimmed.eq_ignore_ascii_case(base)
            });
            if (area.area_key == norm_base || repo_match)
                && let Some(lead) = agents.iter().find(|a| {
                    a.status == AgentStatus::Active
                        && a.role == AgentRole::Lead
                        && a.project.eq_ignore_ascii_case(&area.area_key)
                })
            {
                return Some(lead.name_key.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::agent::{AreaRecord, RegisterAgentArgs};
    use fl_core::mem::MemStore;
    use tempfile::TempDir;

    #[test]
    fn resolve_persona_respects_priority_and_cwd_lead_fallback() {
        let tmp = TempDir::new().unwrap();
        let store = MemStore::default();
        store
            .upsert_area(
                AreaRecord::new(
                    "FerroLoop",
                    Some("FerroLoop/".into()),
                    "Agent orchestration",
                    At::from_unix_millis(10),
                    "owner",
                )
                .unwrap(),
            )
            .unwrap();
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Ferris".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Lead,
                extra_areas: vec![],
                create_area: false,
                replace: false,
                at: At::from_unix_millis(11),
                by: "owner".into(),
            })
            .unwrap();

        let proj_dir = tmp.path().join("FerroLoop").join("crates");
        fs::create_dir_all(&proj_dir).unwrap();

        // 1. Explicit flag wins
        assert_eq!(
            resolve_persona(Some("Jarvis"), None, &proj_dir, Some(&store), None),
            "jarvis"
        );

        // 2. Transcript scan works when explicit is None
        let transcript = tmp.path().join("transcript.jsonl");
        fs::write(
            &transcript,
            "{\"type\":\"meta\",\"agentName\":\"Kayla\"}\n",
        )
        .unwrap();
        assert_eq!(
            resolve_persona(
                None,
                Some(&transcript),
                &proj_dir,
                Some(&store),
                Some(&tmp.path().join("routes.json")),
            ),
            "kayla"
        );

        // 3. CWD project lead fallback works when neither explicit nor transcript matches
        assert_eq!(
            resolve_persona(
                None,
                None,
                &proj_dir,
                Some(&store),
                Some(&tmp.path().join("routes.json")),
            ),
            "ferris"
        );
    }

    #[test]
    fn route_table_discovers_claude_agy_and_codex_sessions() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();

        // Create mock Claude session + key file
        let claude_sessions = home.join(".claude").join("sessions");
        fs::create_dir_all(&claude_sessions).unwrap();
        let sock_path = home.join("cc.sock");
        fs::write(&sock_path, "").unwrap();
        fs::write(
            claude_sessions.join("4242.json"),
            serde_json::json!({
                "name": "Sonya",
                "sessionId": "sess-123",
                "messagingSocketPath": sock_path,
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            claude_sessions.join("4242.anyhash.key"),
            "{\"peerToken\":\"secret-tok\"}",
        )
        .unwrap();
        // Auto-generated default session name should be ignored
        fs::write(
            claude_sessions.join("4243.json"),
            serde_json::json!({
                "name": "artificial-humanity-1234",
                "sessionId": "sess-ignored",
                "messagingSocketPath": sock_path,
            })
            .to_string(),
        )
        .unwrap();

        // Create mock Antigravity cli.log + brain transcript
        let agy_dir = home.join(".gemini").join("antigravity-cli");
        let conv_logs = agy_dir
            .join("brain")
            .join("conv-abc")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&conv_logs).unwrap();
        fs::write(
            agy_dir.join("cli.log"),
            "2026-10-24 Language server listening on random port at 54321 for HTTP\n",
        )
        .unwrap();
        fs::write(
            conv_logs.join("transcript.jsonl"),
            "{\"customTitle\":\"Kacey\"}\n",
        )
        .unwrap();

        // Create mock Codex session_index.jsonl
        let codex_dir = home.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("session_index.jsonl"),
            "{\"id\":\"thread-99\",\"agentName\":\"Kage\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();

        let mut table = RouteTable::default();
        table.discover_routes(Some(home), At::from_unix_millis(100));

        assert!(matches!(
            table.get("sonya").map(|e| &e.route),
            Some(VendorRoute::Claude { pid: 4242, key_path: Some(_), .. })
        ));
        assert!(table.get("artificial-humanity-1234").is_none());
        assert!(matches!(
            table.get("kacey").map(|e| &e.route),
            Some(VendorRoute::Agy {
                conversation_id,
                ls_address: Some(addr),
                ..
            }) if conversation_id == "conv-abc" && addr == "127.0.0.1:54321"
        ));
        assert!(matches!(
            table.get("kage").map(|e| &e.route),
            Some(VendorRoute::Codex { thread_id, .. }) if thread_id == "thread-99"
        ));
    }
}
```

- [ ] **Step 3: Create `crates/wire/src/dispatcher.rs`**

Write `crates/wire/src/dispatcher.rs`:

```rust
//! Native vendor push dispatcher for Claude Code, Antigravity, Codex, and Webhook routes (spec §3.2).

use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fl_core::at::At;
use fl_core::ferrowire::{DeliveryReceipt, DeliveryStatus, WireMessage};
use fl_core::store::MessageStore;

use crate::identity::{RouteTable, VendorRoute, resolve_claude_key_path};

/// Maximum automatic push delivery attempts before leaving a message `Pending` for explicit poll.
pub const MAX_DELIVER: u32 = 5;

pub fn format_push_header(msg: &WireMessage) -> String {
    format!(
        "[fl-wire {}/{}] from {} (id: {}, thread: {}): {}",
        msg.kind.as_wire(),
        msg.urgency.as_wire(),
        msg.sender_name,
        msg.id,
        msg.thread_id,
        msg.subject
    )
}

pub fn format_push_message(msg: &WireMessage) -> String {
    format!("{}\n\n{}", format_push_header(msg), msg.body)
}

pub fn dispatch_to_route(route: &VendorRoute, msg: &WireMessage) -> Result<(), String> {
    match route {
        VendorRoute::Claude {
            pid,
            socket_path,
            key_path,
            ..
        } => {
            if *pid != 0 && !Path::new(&format!("/proc/{pid}")).exists() {
                return Err(format!("claude pid {pid} is no longer running"));
            }
            let resolved_key = key_path.clone().or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .and_then(|h| {
                        resolve_claude_key_path(
                            &h.join(".claude").join("sessions"),
                            *pid,
                            socket_path,
                        )
                    })
            });
            let Some(kpath) = resolved_key else {
                return Err(format!(
                    "missing claude session key file for pid {pid} ({})",
                    socket_path.display()
                ));
            };
            let key_raw = fs::read_to_string(&kpath)
                .map_err(|e| format!("failed to read {}: {e}", kpath.display()))?;
            let key_json: serde_json::Value = serde_json::from_str(&key_raw)
                .map_err(|e| format!("invalid JSON in {}: {e}", kpath.display()))?;
            let peer_token = key_json
                .get("peerToken")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("missing `peerToken` in {}", kpath.display()))?;

            let mut stream = UnixStream::connect(socket_path).map_err(|e| {
                format!(
                    "failed to connect to claude socket {}: {e}",
                    socket_path.display()
                )
            })?;
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

            let auth_frame = serde_json::json!({
                "type": "auth",
                "token": peer_token,
            });
            let user_frame = serde_json::json!({
                "type": "user",
                "from": msg.sender_name,
                "priority": "next",
                "message": {
                    "role": "user",
                    "content": format_push_message(msg),
                }
            });
            let payload = format!("{auth_frame}\n{user_frame}\n");
            stream
                .write_all(payload.as_bytes())
                .and_then(|()| stream.flush())
                .map_err(|e| format!("failed to write to claude socket: {e}"))?;
            Ok(())
        }
        VendorRoute::Agy {
            conversation_id,
            ls_address,
            csrf_token,
            agentapi_exe,
            ..
        } => {
            let exe = agentapi_exe
                .clone()
                .or_else(|| std::env::var_os("ANTIGRAVITY_AGENTAPI_EXE").map(PathBuf::from))
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .map(|h| h.join(".local").join("bin").join("agy"))
                })
                .unwrap_or_else(|| PathBuf::from("agy"));
            let addr = ls_address
                .clone()
                .or_else(|| std::env::var("ANTIGRAVITY_LS_ADDRESS").ok())
                .unwrap_or_default();
            let csrf = csrf_token
                .clone()
                .or_else(|| std::env::var("ANTIGRAVITY_CSRF_TOKEN").ok())
                .unwrap_or_default();

            let title = format!("--title={}", format_push_header(msg));
            let mut cmd = Command::new(&exe);
            cmd.args(["agentapi", "send-message", &title, conversation_id, &msg.body]);
            cmd.env("ANTIGRAVITY_CONVERSATION_ID", conversation_id);
            cmd.env("ANTIGRAVITY_AGENTAPI_EXE", &exe);
            if !addr.is_empty() {
                cmd.env("ANTIGRAVITY_LS_ADDRESS", &addr);
            }
            if !csrf.is_empty() {
                cmd.env("ANTIGRAVITY_CSRF_TOKEN", &csrf);
            }
            let status = cmd
                .status()
                .map_err(|e| format!("failed to spawn {}: {e}", exe.display()))?;
            if !status.success() {
                return Err(format!("agy send-message exited with {status}"));
            }
            Ok(())
        }
        VendorRoute::Codex { thread_id, cwd } => {
            let mut cmd = Command::new("codex");
            cmd.args([
                "queue",
                "--thread",
                thread_id,
                "--message",
                &format_push_message(msg),
            ]);
            if let Some(dir) = cwd {
                cmd.current_dir(dir);
            }
            let status = cmd
                .status()
                .map_err(|e| format!("failed to spawn codex queue: {e}"))?;
            if !status.success() {
                return Err(format!("codex queue exited with {status}"));
            }
            Ok(())
        }
        VendorRoute::Webhook { socket_path } => {
            let mut stream = UnixStream::connect(socket_path).map_err(|e| {
                format!(
                    "failed to connect to webhook socket {}: {e}",
                    socket_path.display()
                )
            })?;
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            let mut line =
                serde_json::to_string(msg).map_err(|e| format!("serialize message: {e}"))?;
            line.push('\n');
            stream
                .write_all(line.as_bytes())
                .and_then(|()| stream.flush())
                .map_err(|e| format!("failed to write to webhook socket: {e}"))?;
            Ok(())
        }
    }
}

/// Attempt push delivery for all pending recipients of `msg` on this host,
/// updating `store` receipts (`Acked` on verified push, `Pending` with `attempts + 1` on failure)
/// and returning any modified `DeliveryReceipt`s.
pub fn dispatch_message_for_store<S: MessageStore>(
    store: &S,
    routes: &mut RouteTable,
    home_override: Option<&Path>,
    msg: &WireMessage,
    now: At,
) -> Vec<DeliveryReceipt> {
    routes.discover_routes(home_override, now.clone());
    let Ok(receipts) = store.list_wire_receipts(&msg.id) else {
        return Vec::new();
    };
    let mut updated = Vec::new();
    for rcpt in receipts {
        if matches!(rcpt.status, DeliveryStatus::Acked | DeliveryStatus::Expired) {
            continue;
        }
        if rcpt.attempts >= MAX_DELIVER {
            continue;
        }
        let Some(entry) = routes.get(&rcpt.recipient_name).cloned() else {
            continue;
        };
        let target_status = match dispatch_to_route(&entry.route, msg) {
            Ok(()) => DeliveryStatus::Acked,
            Err(_) => DeliveryStatus::Pending,
        };
        if let Ok(Some(new_rcpt)) = store.update_wire_receipt(
            &msg.id,
            &rcpt.recipient_name,
            target_status,
            true,
            now.clone(),
        ) {
            updated.push(new_rcpt);
        }
    }
    updated
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
    use fl_core::mem::MemStore;
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;
    use tempfile::TempDir;

    #[test]
    fn claude_socket_dispatch_sends_auth_and_user_frame_and_acks_receipt() {
        let tmp = TempDir::new().unwrap();
        let sock_path = tmp.path().join("claude.sock");
        let key_path = tmp.path().join("0.key");
        fs::write(&key_path, "{\"peerToken\":\"tok-xyz\"}").unwrap();

        let listener = UnixListener::bind(&sock_path).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let reader = std::io::BufReader::new(stream);
            reader
                .lines()
                .take(2)
                .map(|l| l.unwrap())
                .collect::<Vec<_>>()
        });

        let store = MemStore::default();
        let (msg, _) = store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to: WireAddress::AgentName("sonya".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Urgent,
                durable: false,
                subject: "ping".into(),
                body: "hello sonya".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(10),
            })
            .unwrap();

        let mut routes = RouteTable::default();
        routes.upsert(
            "sonya",
            VendorRoute::Claude {
                pid: 0,
                session_id: "s1".into(),
                socket_path: sock_path,
                key_path: Some(key_path),
            },
            At::from_unix_millis(10),
        );

        let updated =
            dispatch_message_for_store(&store, &mut routes, Some(tmp.path()), &msg, At::from_unix_millis(20));
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].status, DeliveryStatus::Acked);
        assert_eq!(updated[0].attempts, 1);

        let lines = handle.join().unwrap();
        let auth: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(auth["type"], "auth");
        assert_eq!(auth["token"], "tok-xyz");
        let user: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(user["type"], "user");
        assert_eq!(user["from"], "jarvis");
        assert!(user["message"]["content"]
            .as_str()
            .unwrap()
            .contains("hello sonya"));
    }

    #[test]
    fn failed_push_increments_attempts_and_stops_at_max_deliver() {
        let tmp = TempDir::new().unwrap();
        let dead_sock = tmp.path().join("dead.sock");

        let store = MemStore::default();
        let (msg, _) = store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to: WireAddress::AgentName("kayla".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: false,
                subject: "retry test".into(),
                body: "body".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(10),
            })
            .unwrap();

        let mut routes = RouteTable::default();
        routes.upsert(
            "kayla",
            VendorRoute::Webhook {
                socket_path: dead_sock,
            },
            At::from_unix_millis(10),
        );

        for i in 1..=7 {
            let _ = dispatch_message_for_store(
                &store,
                &mut routes,
                Some(tmp.path()),
                &msg,
                At::from_unix_millis(10 + i),
            );
        }

        let rcpts = store.list_wire_receipts(&msg.id).unwrap();
        assert_eq!(rcpts[0].status, DeliveryStatus::Pending);
        assert_eq!(rcpts[0].attempts, MAX_DELIVER);
    }
}
```

- [ ] **Step 4: Create `crates/wire/src/ws.rs` and `crates/wire/src/peer.rs`**

Write `crates/wire/src/ws.rs`:

```rust
//! Minimal RFC 6455 WebSocket framing and client/server upgrade over TCP (`ws://`) and TLS (`wss://`).

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::rand::{SecureRandom, SystemRandom};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use rustls_pki_types::ServerName;
use serde::Serialize;
use serde::de::DeserializeOwned;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_FRAME_PAYLOAD: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("invalid WebSocket URL `{0}`")]
    InvalidUrl(String),
    #[error("WebSocket handshake failed: {0}")]
    Handshake(String),
    #[error("WebSocket protocol error: {0}")]
    Protocol(String),
    #[error("WebSocket closed")]
    Closed,
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsOpcode {
    Continuation = 0x0,
    Text = 0x1,
    Binary = 0x2,
    Close = 0x8,
    Ping = 0x9,
    Pong = 0xA,
}

impl WsOpcode {
    fn from_u8(b: u8) -> Result<Self, WsError> {
        match b {
            0x0 => Ok(Self::Continuation),
            0x1 => Ok(Self::Text),
            0x2 => Ok(Self::Binary),
            0x8 => Ok(Self::Close),
            0x9 => Ok(Self::Ping),
            0xA => Ok(Self::Pong),
            other => Err(WsError::Protocol(format!("unknown opcode 0x{other:x}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsFrame {
    pub opcode: WsOpcode,
    pub payload: Vec<u8>,
}

enum Transport {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}

pub struct WsStream {
    transport: Transport,
    is_client: bool,
    rng: SystemRandom,
}

impl WsStream {
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match &self.transport {
            Transport::Plain(s) => s.set_read_timeout(dur),
            Transport::Tls(s) => s.get_ref().set_read_timeout(dur),
        }
    }

    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match &self.transport {
            Transport::Plain(s) => s.set_write_timeout(dur),
            Transport::Tls(s) => s.get_ref().set_write_timeout(dur),
        }
    }

    /// Connect to a `ws://host:port[/path]` or `wss://host:port[/path]` endpoint.
    pub fn connect(url: &str, timeout: Duration) -> Result<Self, WsError> {
        let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("ws://") {
            (false, r)
        } else {
            return Err(WsError::InvalidUrl(url.to_string()));
        };

        let (host_port, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        if host_port.is_empty() {
            return Err(WsError::InvalidUrl(url.to_string()));
        }
        let default_port = if tls { 443 } else { 80 };
        let (host, port) = split_host_port(host_port, default_port)
            .ok_or_else(|| WsError::InvalidUrl(url.to_string()))?;

        let addr = format!("{host}:{port}")
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("could not resolve {host}:{port}"),
                )
            })?;
        let tcp = TcpStream::connect_timeout(&addr, timeout)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.set_write_timeout(Some(timeout))?;

        let transport = if tls {
            let mut root_store = RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(root_store)
            .with_no_client_auth();
            let server_name = ServerName::try_from(host.to_string())
                .map_err(|_| WsError::InvalidUrl(url.to_string()))?;
            let conn = ClientConnection::new(Arc::new(config), server_name)?;
            Transport::Tls(Box::new(StreamOwned::new(conn, tcp)))
        } else {
            Transport::Plain(tcp)
        };

        let rng = SystemRandom::new();
        let mut key_bytes = [0u8; 16];
        rng.fill(&mut key_bytes)
            .map_err(|_| WsError::Handshake("RNG failure".into()))?;
        let sec_key = BASE64.encode(key_bytes);

        let mut ws = Self {
            transport,
            is_client: true,
            rng,
        };

        let req = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host_port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {sec_key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        ws.transport.write_all(req.as_bytes())?;
        ws.transport.flush()?;

        let headers = read_http_headers(&mut ws.transport)?;
        let mut lines = headers.lines();
        let status_line = lines.next().unwrap_or("");
        if !status_line.starts_with("HTTP/1.1 101") {
            return Err(WsError::Handshake(format!(
                "expected 101 Switching Protocols, got `{status_line}`"
            )));
        }
        let expected_accept = compute_accept_key(&sec_key);
        let mut accept_ok = false;
        for line in lines {
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("sec-websocket-accept")
                && v.trim() == expected_accept
            {
                accept_ok = true;
            }
        }
        if !accept_ok {
            return Err(WsError::Handshake(
                "missing or mismatched Sec-WebSocket-Accept header".into(),
            ));
        }

        Ok(ws)
    }

    /// Accept an inbound TCP stream and complete the server-side RFC 6455 upgrade handshake.
    pub fn accept(tcp: TcpStream, timeout: Duration) -> Result<Self, WsError> {
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.set_write_timeout(Some(timeout))?;
        let mut transport = Transport::Plain(tcp);
        let headers = read_http_headers(&mut transport)?;
        let mut sec_key: Option<String> = None;
        for line in headers.lines().skip(1) {
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("sec-websocket-key")
            {
                sec_key = Some(v.trim().to_string());
            }
        }
        let Some(sec_key) = sec_key else {
            return Err(WsError::Handshake(
                "client upgrade missing Sec-WebSocket-Key".into(),
            ));
        };
        let accept = compute_accept_key(&sec_key);
        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        transport.write_all(resp.as_bytes())?;
        transport.flush()?;

        Ok(Self {
            transport,
            is_client: false,
            rng: SystemRandom::new(),
        })
    }

    pub fn read_frame(&mut self) -> Result<WsFrame, WsError> {
        let mut hdr = [0u8; 2];
        if let Err(e) = self.transport.read_exact(&mut hdr) {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                return Err(WsError::Closed);
            }
            return Err(WsError::Io(e));
        }
        let _fin = (hdr[0] & 0x80) != 0;
        let opcode = WsOpcode::from_u8(hdr[0] & 0x0F)?;
        let masked = (hdr[1] & 0x80) != 0;
        let mut len = (hdr[1] & 0x7F) as usize;

        if len == 126 {
            let mut ext = [0u8; 2];
            self.transport.read_exact(&mut ext)?;
            len = u16::from_be_bytes(ext) as usize;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            self.transport.read_exact(&mut ext)?;
            len = usize::try_from(u64::from_be_bytes(ext))
                .map_err(|_| WsError::Protocol("frame payload too large".into()))?;
        }
        if len > MAX_FRAME_PAYLOAD {
            return Err(WsError::Protocol(format!(
                "frame payload {len} exceeds maximum ({MAX_FRAME_PAYLOAD})"
            )));
        }

        let mut mask = [0u8; 4];
        if masked {
            self.transport.read_exact(&mut mask)?;
        }
        let mut payload = vec![0u8; len];
        if len > 0 {
            self.transport.read_exact(&mut payload)?;
        }
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }
        }
        Ok(WsFrame { opcode, payload })
    }

    pub fn write_frame(&mut self, opcode: WsOpcode, payload: &[u8]) -> Result<(), WsError> {
        let mut buf = Vec::with_capacity(14 + payload.len());
        buf.push(0x80 | (opcode as u8));
        let mask_bit = if self.is_client { 0x80 } else { 0x00 };
        if payload.len() < 126 {
            buf.push(mask_bit | (payload.len() as u8));
        } else if payload.len() <= u16::MAX as usize {
            buf.push(mask_bit | 126);
            buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            buf.push(mask_bit | 127);
            buf.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        if self.is_client {
            let mut mask = [0u8; 4];
            self.rng
                .fill(&mut mask)
                .map_err(|_| WsError::Protocol("RNG failure".into()))?;
            buf.extend_from_slice(&mask);
            for (i, &b) in payload.iter().enumerate() {
                buf.push(b ^ mask[i & 3]);
            }
        } else {
            buf.extend_from_slice(payload);
        }
        self.transport.write_all(&buf)?;
        self.transport.flush()?;
        Ok(())
    }

    pub fn send_json<T: Serialize>(&mut self, value: &T) -> Result<(), WsError> {
        let bytes = serde_json::to_vec(value)?;
        self.write_frame(WsOpcode::Text, &bytes)
    }

    pub fn recv_json<T: DeserializeOwned>(&mut self) -> Result<T, WsError> {
        loop {
            let frame = self.read_frame()?;
            match frame.opcode {
                WsOpcode::Text | WsOpcode::Binary => {
                    return Ok(serde_json::from_slice(&frame.payload)?);
                }
                WsOpcode::Ping => {
                    self.write_frame(WsOpcode::Pong, &frame.payload)?;
                }
                WsOpcode::Pong => continue,
                WsOpcode::Close => {
                    let _ = self.write_frame(WsOpcode::Close, &[]);
                    return Err(WsError::Closed);
                }
                WsOpcode::Continuation => {
                    return Err(WsError::Protocol(
                        "unexpected fragmented continuation frame".into(),
                    ));
                }
            }
        }
    }

    pub fn send_ping(&mut self, payload: &[u8]) -> Result<(), WsError> {
        self.write_frame(WsOpcode::Ping, payload)
    }

    pub fn close(&mut self) -> Result<(), WsError> {
        self.write_frame(WsOpcode::Close, &[])
    }
}

fn split_host_port(host_port: &str, default_port: u16) -> Option<(&str, u16)> {
    if let Some(rest) = host_port.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        if after.is_empty() {
            return Some((host, default_port));
        }
        let port_str = after.strip_prefix(':')?;
        let port = port_str.parse::<u16>().ok()?;
        return Some((host, port));
    }
    if let Some((h, p)) = host_port.rsplit_once(':') {
        let port = p.parse::<u16>().ok()?;
        Some((h, port))
    } else {
        Some((host_port, default_port))
    }
}

fn read_http_headers<R: Read>(reader: &mut R) -> Result<String, WsError> {
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while buf.len() < 16_384 {
        let n = reader.read(&mut byte)?;
        if n == 0 {
            return Err(WsError::Handshake("unexpected EOF reading headers".into()));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            return String::from_utf8(buf)
                .map_err(|e| WsError::Handshake(format!("non-UTF8 HTTP headers: {e}")));
        }
    }
    Err(WsError::Handshake("HTTP headers exceeded 16 KB".into()))
}

fn compute_accept_key(sec_key: &str) -> String {
    let mut input = Vec::with_capacity(sec_key.len() + WS_GUID.len());
    input.extend_from_slice(sec_key.trim().as_bytes());
    input.extend_from_slice(WS_GUID.as_bytes());
    let digest = sha1_digest(&input);
    BASE64.encode(digest)
}

/// Compact RFC 3174 SHA-1 implementation used solely for RFC 6455 `Sec-WebSocket-Accept`.
fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn rfc6455_accept_key_matches_spec_test_vector() {
        assert_eq!(
            compute_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn websocket_client_and_server_exchange_json_and_ping_pong() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut ws = WsStream::accept(tcp, Duration::from_secs(2)).unwrap();
            let val: serde_json::Value = ws.recv_json().unwrap();
            assert_eq!(val["hello"], "world");
            ws.send_ping(b"ping-1").unwrap();
            ws.send_json(&serde_json::json!({"ack": true})).unwrap();
        });

        let mut client =
            WsStream::connect(&format!("ws://{addr}/wire"), Duration::from_secs(2)).unwrap();
        client
            .send_json(&serde_json::json!({"hello": "world"}))
            .unwrap();
        let reply: serde_json::Value = client.recv_json().unwrap();
        assert_eq!(reply["ack"], true);
        server.join().unwrap();
    }
}
```

Write `crates/wire/src/peer.rs`:

```rust
//! Cross-boundary WebSocket/TLS peer protocol, mutual HMAC-SHA256 PSK handshake,
//! and catch-up sync framing (spec §3.4).

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fl_core::agent::RegistryEvent;
use fl_core::ferrowire::{DeliveryReceipt, WireMessage};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::ws::{WsError, WsStream};

pub const WIRE_PROTOCOL_VERSION: u32 = 1;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const PEER_PING_INTERVAL: Duration = Duration::from_secs(15);
pub const PEER_READ_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerFrame {
    Challenge {
        protocol_version: u32,
        node_id: String,
        host: String,
        nonce: String,
    },
    Auth {
        mac: String,
        last_msg_seq: u64,
        last_rcpt_seq: u64,
        last_agent_seq: u64,
    },
    SyncBatch {
        registry_events: Vec<RegistryEvent>,
        messages: Vec<WireMessage>,
        receipts: Vec<DeliveryReceipt>,
    },
    Publish {
        message: WireMessage,
        receipts: Vec<DeliveryReceipt>,
    },
    ReceiptUpdate {
        receipt: DeliveryReceipt,
    },
    RegistryUpdate {
        event: RegistryEvent,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerHandshakeOutcome {
    pub remote_node_id: String,
    pub remote_host: String,
    pub remote_last_msg_seq: u64,
    pub remote_last_rcpt_seq: u64,
    pub remote_last_agent_seq: u64,
}

fn build_mac_input(remote_nonce: &[u8], local_nonce: &[u8], host: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(11 + remote_nonce.len() + local_nonce.len() + host.len());
    msg.extend_from_slice(b"fl-wire-v1:");
    msg.extend_from_slice(remote_nonce);
    msg.extend_from_slice(local_nonce);
    msg.extend_from_slice(host.as_bytes());
    msg
}

pub fn compute_psk_mac(
    token: &[u8],
    remote_nonce: &[u8],
    local_nonce: &[u8],
    host: &str,
) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, token);
    let input = build_mac_input(remote_nonce, local_nonce, host);
    let tag = hmac::sign(&key, &input);
    BASE64.encode(tag.as_ref())
}

pub fn verify_psk_mac(
    token: &[u8],
    remote_nonce: &[u8],
    local_nonce: &[u8],
    host: &str,
    mac_b64: &str,
) -> bool {
    let Ok(mac_bytes) = BASE64.decode(mac_b64.trim()) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, token);
    let input = build_mac_input(remote_nonce, local_nonce, host);
    hmac::verify(&key, &input, &mac_bytes).is_ok()
}

/// Execute the mutual 32-byte nonce + HMAC-SHA256 PSK handshake on `ws` (spec §3.4).
pub fn handshake_peer(
    ws: &mut WsStream,
    local_node_id: &str,
    local_host: &str,
    psk: &[u8],
    last_msg_seq: u64,
    last_rcpt_seq: u64,
    last_agent_seq: u64,
) -> Result<PeerHandshakeOutcome, WsError> {
    ws.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    ws.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let rng = SystemRandom::new();
    let mut local_nonce = [0u8; 32];
    rng.fill(&mut local_nonce)
        .map_err(|_| WsError::Handshake("RNG failure".into()))?;
    let local_nonce_b64 = BASE64.encode(local_nonce);

    ws.send_json(&PeerFrame::Challenge {
        protocol_version: WIRE_PROTOCOL_VERSION,
        node_id: local_node_id.to_string(),
        host: local_host.to_string(),
        nonce: local_nonce_b64,
    })?;

    let challenge: PeerFrame = ws.recv_json()?;
    let PeerFrame::Challenge {
        protocol_version,
        node_id: remote_node_id,
        host: remote_host,
        nonce: remote_nonce_b64,
    } = challenge
    else {
        let _ = ws.close();
        return Err(WsError::Handshake("expected PeerFrame::Challenge".into()));
    };

    if protocol_version != WIRE_PROTOCOL_VERSION {
        let _ = ws.close();
        return Err(WsError::Handshake(format!(
            "unsupported wire protocol version {protocol_version}"
        )));
    }
    let remote_nonce = BASE64
        .decode(remote_nonce_b64.trim())
        .map_err(|_| WsError::Handshake("invalid base64 remote nonce".into()))?;
    if remote_nonce.len() != 32 {
        let _ = ws.close();
        return Err(WsError::Handshake("remote nonce must be 32 bytes".into()));
    }

    let local_mac = compute_psk_mac(psk, &remote_nonce, &local_nonce, local_host);
    ws.send_json(&PeerFrame::Auth {
        mac: local_mac,
        last_msg_seq,
        last_rcpt_seq,
        last_agent_seq,
    })?;

    let auth: PeerFrame = ws.recv_json()?;
    let PeerFrame::Auth {
        mac: remote_mac,
        last_msg_seq: remote_last_msg_seq,
        last_rcpt_seq: remote_last_rcpt_seq,
        last_agent_seq: remote_last_agent_seq,
    } = auth
    else {
        let _ = ws.close();
        return Err(WsError::Handshake("expected PeerFrame::Auth".into()));
    };

    // Remote computed its MAC over (remote's view of remote_nonce = local_nonce,
    // remote's view of local_nonce = remote_nonce, remote_host)
    if !verify_psk_mac(psk, &local_nonce, &remote_nonce, &remote_host, &remote_mac) {
        let _ = ws.close();
        return Err(WsError::Handshake(
            "PSK HMAC-SHA256 authentication failed".into(),
        ));
    }

    ws.set_read_timeout(Some(PEER_READ_TIMEOUT))?;
    Ok(PeerHandshakeOutcome {
        remote_node_id,
        remote_host,
        remote_last_msg_seq,
        remote_last_rcpt_seq,
        remote_last_agent_seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn mutual_psk_handshake_succeeds_for_matching_token_and_rejects_mismatch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // 1. Matching PSK
        let srv = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut ws = WsStream::accept(tcp, Duration::from_secs(2)).unwrap();
            handshake_peer(&mut ws, "node-b", "host-b", b"shared-secret", 10, 20, 5).unwrap()
        });

        let mut cli =
            WsStream::connect(&format!("ws://{addr}/wire"), Duration::from_secs(2)).unwrap();
        let out_a =
            handshake_peer(&mut cli, "node-a", "host-a", b"shared-secret", 1, 2, 3).unwrap();
        let out_b = srv.join().unwrap();

        assert_eq!(out_a.remote_node_id, "node-b");
        assert_eq!(out_a.remote_last_msg_seq, 10);
        assert_eq!(out_b.remote_node_id, "node-a");
        assert_eq!(out_b.remote_last_msg_seq, 1);

        // 2. Mismatched PSK fails authentication
        let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let srv2 = std::thread::spawn(move || {
            let (tcp, _) = listener2.accept().unwrap();
            let mut ws = WsStream::accept(tcp, Duration::from_secs(2)).unwrap();
            handshake_peer(&mut ws, "node-b", "host-b", b"secret-1", 0, 0, 0)
        });
        let mut cli2 =
            WsStream::connect(&format!("ws://{addr2}/wire"), Duration::from_secs(2)).unwrap();
        let res_a = handshake_peer(&mut cli2, "node-a", "host-a", b"wrong-secret", 0, 0, 0);
        let res_b = srv2.join().unwrap();
        assert!(res_a.is_err() || res_b.is_err());
    }
}
```

- [ ] **Step 5: Create `crates/wire/src/daemon.rs`**

Write `crates/wire/src/daemon.rs`:

```rust
//! Local Unix domain socket server/client (`fl-wire.sock`) and multi-peer WebSocket relay daemon (spec §3.3, §3.4).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fl_core::agent::RegistryEvent;
use fl_core::at::At;
use fl_core::ferrowire::{
    DeliveryReceipt, DeliveryStatus, SendWireArgs, WireFilter, WireInboxItem, WireMessage,
};
use fl_core::ids::MessageId;
use fl_core::store::{AgentRegistry, MessageStore};
use serde::{Deserialize, Serialize};

use crate::dispatcher::dispatch_message_for_store;
use crate::identity::{RouteEntry, RouteTable, VendorRoute, normalize_persona};
use crate::peer::{PeerFrame, handshake_peer};
use crate::ws::{WsError, WsStream};

pub const DEFAULT_LOCAL_CONNECT_TIMEOUT: Duration = Duration::from_millis(50);

pub fn default_socket_path() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(rt);
        if p.is_absolute() {
            return p.join("fl-wire.sock");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home);
        if p.is_absolute() {
            return p.join(".cache").join("fl").join("wire.sock");
        }
    }
    PathBuf::from("/tmp/fl-wire.sock")
}

fn now_at() -> At {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    At::from_unix_millis(millis)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LocalRequest {
    Send {
        args: SendWireArgs,
    },
    Read {
        filter: WireFilter,
        ack: bool,
    },
    Ack {
        message_ids: Vec<MessageId>,
        recipient: String,
    },
    RegisterRoute {
        persona: String,
        route: VendorRoute,
    },
    Subscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recipient: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        correlation_id: Option<MessageId>,
    },
    Status,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LocalResponse {
    Sent {
        message: WireMessage,
        receipts: Vec<DeliveryReceipt>,
    },
    Inbox {
        items: Vec<WireInboxItem>,
    },
    Acked {
        receipts: Vec<DeliveryReceipt>,
    },
    RouteRegistered,
    Subscribed,
    Status(DaemonStatus),
    ShuttingDown,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub node_id: String,
    pub host: String,
    pub socket_path: PathBuf,
    pub listen_addr: Option<String>,
    pub connected_peers: Vec<String>,
    pub routes: Vec<RouteEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LocalEvent {
    Message { message: WireMessage },
    Receipt { receipt: DeliveryReceipt },
}

pub struct LocalClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl LocalClient {
    pub fn connect(socket_path: &Path, timeout: Duration) -> io::Result<Self> {
        let stream = UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let writer = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
        })
    }

    pub fn request(&mut self, req: &LocalRequest) -> Result<LocalResponse, String> {
        let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .and_then(|()| self.writer.flush())
            .map_err(|e| e.to_string())?;
        let mut resp_line = String::new();
        let n = self
            .reader
            .read_line(&mut resp_line)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("unexpected EOF from fl-wire daemon".into());
        }
        let resp: LocalResponse =
            serde_json::from_str(&resp_line).map_err(|e| format!("invalid daemon response: {e}"))?;
        if let LocalResponse::Error { message } = resp {
            return Err(message);
        }
        Ok(resp)
    }

    pub fn subscribe(
        &mut self,
        recipient: Option<String>,
        correlation_id: Option<MessageId>,
    ) -> Result<(), String> {
        let resp = self.request(&LocalRequest::Subscribe {
            recipient,
            correlation_id,
        })?;
        match resp {
            LocalResponse::Subscribed => Ok(()),
            other => Err(format!("unexpected subscribe response: {other:?}")),
        }
    }

    pub fn wait_for_event(&mut self, timeout: Duration) -> Result<Option<LocalEvent>, String> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Ok(None),
            Ok(_) => {
                let ev: LocalEvent = serde_json::from_str(&line)
                    .map_err(|e| format!("invalid daemon event: {e}"))?;
                Ok(Some(ev))
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                Ok(None)
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub node_id: String,
    pub host: String,
    pub socket_path: PathBuf,
    pub routes_path: Option<PathBuf>,
    pub home_override: Option<PathBuf>,
    pub listen_addr: Option<String>,
    pub peers: Vec<String>,
    pub psk: Vec<u8>,
}

struct Subscriber {
    recipient: Option<String>,
    correlation_id: Option<MessageId>,
    tx: Sender<LocalEvent>,
}

struct DaemonInner<S> {
    store: Arc<S>,
    config: DaemonConfig,
    routes: Mutex<RouteTable>,
    peers: Mutex<BTreeMap<u64, (String, Sender<PeerFrame>)>>,
    subscribers: Mutex<Vec<Subscriber>>,
    next_peer_id: Mutex<u64>,
    shutdown: AtomicBool,
}

impl<S> DaemonInner<S>
where
    S: MessageStore + AgentRegistry + Send + Sync + 'static,
{
    fn register_peer(&self, label: String, tx: Sender<PeerFrame>) -> u64 {
        let mut id_guard = self.next_peer_id.lock().unwrap();
        *id_guard += 1;
        let id = *id_guard;
        self.peers.lock().unwrap().insert(id, (label, tx));
        id
    }

    fn unregister_peer(&self, id: u64) {
        self.peers.lock().unwrap().remove(&id);
    }

    fn broadcast_to_peers(&self, exclude_peer: Option<u64>, frame: &PeerFrame) {
        let mut peers = self.peers.lock().unwrap();
        peers.retain(|&id, (_, tx)| {
            if Some(id) == exclude_peer {
                return true;
            }
            tx.send(frame.clone()).is_ok()
        });
    }

    fn notify_subscribers_message(&self, msg: &WireMessage, receipts: &[DeliveryReceipt]) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|sub| {
            let matches_corr = sub
                .correlation_id
                .as_ref()
                .is_none_or(|cid| msg.correlation_id.as_ref() == Some(cid));
            let matches_rcpt = sub.recipient.as_ref().is_none_or(|r| {
                let norm = r.to_ascii_lowercase();
                receipts.iter().any(|rcpt| rcpt.recipient_name == norm)
            });
            if matches_corr && matches_rcpt {
                sub.tx
                    .send(LocalEvent::Message {
                        message: msg.clone(),
                    })
                    .is_ok()
            } else {
                true
            }
        });
    }

    fn notify_subscribers_receipt(&self, rcpt: &DeliveryReceipt) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|sub| {
            if sub.correlation_id.is_some() {
                return true;
            }
            let matches_rcpt = sub
                .recipient
                .as_ref()
                .is_none_or(|r| r.eq_ignore_ascii_case(&rcpt.recipient_name));
            if matches_rcpt {
                sub.tx
                    .send(LocalEvent::Receipt {
                        receipt: rcpt.clone(),
                    })
                    .is_ok()
            } else {
                true
            }
        });
    }

    fn dispatch_and_propagate(&self, msg: &WireMessage, now: At) {
        let updated = {
            let mut routes = self.routes.lock().unwrap();
            dispatch_message_for_store(
                self.store.as_ref(),
                &mut routes,
                self.config.home_override.as_deref(),
                msg,
                now,
            )
        };
        for rcpt in updated {
            self.notify_subscribers_receipt(&rcpt);
            self.broadcast_to_peers(None, &PeerFrame::ReceiptUpdate { receipt: rcpt });
        }
    }

    fn dispatch_pending_for_persona(&self, persona: &str, now: At) {
        let filter = WireFilter {
            recipient: Some(persona.to_string()),
            unread_only: true,
            thread_id: None,
            correlation_id: None,
            limit: None,
        };
        if let Ok(items) = self.store.list_wire_inbox(&filter) {
            for item in items {
                self.dispatch_and_propagate(&item.message, now.clone());
            }
        }
    }

    fn handle_local_request(
        &self,
        req: LocalRequest,
    ) -> (LocalResponse, Option<Receiver<LocalEvent>>) {
        let now = now_at();
        match req {
            LocalRequest::Send { mut args } => {
                if args.sender_host.is_none() {
                    args.sender_host = Some(self.config.host.clone());
                }
                match self.store.send_wire(args) {
                    Ok((msg, receipts)) => {
                        self.notify_subscribers_message(&msg, &receipts);
                        self.broadcast_to_peers(
                            None,
                            &PeerFrame::Publish {
                                message: msg.clone(),
                                receipts: receipts.clone(),
                            },
                        );
                        self.dispatch_and_propagate(&msg, now);
                        let latest_receipts = self
                            .store
                            .list_wire_receipts(&msg.id)
                            .unwrap_or(receipts);
                        (
                            LocalResponse::Sent {
                                message: msg,
                                receipts: latest_receipts,
                            },
                            None,
                        )
                    }
                    Err(e) => (
                        LocalResponse::Error {
                            message: e.to_string(),
                        },
                        None,
                    ),
                }
            }
            LocalRequest::Read { filter, ack } => match self.store.list_wire_inbox(&filter) {
                Ok(mut items) => {
                    if ack
                        && let Some(ref rec) = filter.recipient
                    {
                        for item in &mut items {
                            if item.receipt.status != DeliveryStatus::Acked
                                && let Ok(Some(updated)) = self.store.update_wire_receipt(
                                    &item.message.id,
                                    rec,
                                    DeliveryStatus::Acked,
                                    false,
                                    now.clone(),
                                )
                            {
                                item.receipt = updated.clone();
                                self.notify_subscribers_receipt(&updated);
                                self.broadcast_to_peers(
                                    None,
                                    &PeerFrame::ReceiptUpdate { receipt: updated },
                                );
                            }
                        }
                    }
                    (LocalResponse::Inbox { items }, None)
                }
                Err(e) => (
                    LocalResponse::Error {
                        message: e.to_string(),
                    },
                    None,
                ),
            },
            LocalRequest::Ack {
                message_ids,
                recipient,
            } => {
                let mut out = Vec::new();
                for mid in message_ids {
                    match self.store.update_wire_receipt(
                        &mid,
                        &recipient,
                        DeliveryStatus::Acked,
                        false,
                        now.clone(),
                    ) {
                        Ok(Some(rcpt)) => {
                            self.notify_subscribers_receipt(&rcpt);
                            self.broadcast_to_peers(
                                None,
                                &PeerFrame::ReceiptUpdate {
                                    receipt: rcpt.clone(),
                                },
                            );
                            out.push(rcpt);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            return (
                                LocalResponse::Error {
                                    message: e.to_string(),
                                },
                                None,
                            );
                        }
                    }
                }
                (LocalResponse::Acked { receipts: out }, None)
            }
            LocalRequest::RegisterRoute { persona, route } => {
                {
                    let mut routes = self.routes.lock().unwrap();
                    routes.upsert(&persona, route, now.clone());
                    let _ = routes.save(self.config.routes_path.as_deref());
                }
                self.dispatch_pending_for_persona(&persona, now);
                (LocalResponse::RouteRegistered, None)
            }
            LocalRequest::Subscribe {
                recipient,
                correlation_id,
            } => {
                let (tx, rx) = mpsc::channel();
                let norm_rcpt = recipient.as_deref().and_then(normalize_persona);
                self.subscribers.lock().unwrap().push(Subscriber {
                    recipient: norm_rcpt,
                    correlation_id,
                    tx,
                });
                (LocalResponse::Subscribed, Some(rx))
            }
            LocalRequest::Status => {
                let connected_peers = self
                    .peers
                    .lock()
                    .unwrap()
                    .values()
                    .map(|(label, _)| label.clone())
                    .collect();
                let routes = self
                    .routes
                    .lock()
                    .unwrap()
                    .entries
                    .values()
                    .cloned()
                    .collect();
                (
                    LocalResponse::Status(DaemonStatus {
                        node_id: self.config.node_id.clone(),
                        host: self.config.host.clone(),
                        socket_path: self.config.socket_path.clone(),
                        listen_addr: self.config.listen_addr.clone(),
                        connected_peers,
                        routes,
                    }),
                    None,
                )
            }
            LocalRequest::Shutdown => {
                self.shutdown.store(true, Ordering::SeqCst);
                (LocalResponse::ShuttingDown, None)
            }
        }
    }

    fn handle_peer_connection(&self, mut ws: WsStream) -> Result<(), WsError> {
        let last_msg_seq = self
            .store
            .wire_messages_after(0)
            .ok()
            .and_then(|v| v.last().map(|(s, _)| *s))
            .unwrap_or(0);
        let last_rcpt_seq = self
            .store
            .wire_receipts_after(0)
            .ok()
            .and_then(|v| v.last().map(|(s, _)| *s))
            .unwrap_or(0);
        let last_agent_seq = self
            .store
            .registry_events_after(0)
            .ok()
            .and_then(|v| v.last().map(|(s, _)| *s))
            .unwrap_or(0);

        let hs = handshake_peer(
            &mut ws,
            &self.config.node_id,
            &self.config.host,
            &self.config.psk,
            last_msg_seq,
            last_rcpt_seq,
            last_agent_seq,
        )?;

        // Send initial catch-up SyncBatch for everything after remote's cursors
        let registry_events: Vec<RegistryEvent> = self
            .store
            .registry_events_after(hs.remote_last_agent_seq)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, e)| e)
            .collect();
        let messages: Vec<WireMessage> = self
            .store
            .wire_messages_after(hs.remote_last_msg_seq)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, m)| m)
            .collect();
        let receipts: Vec<DeliveryReceipt> = self
            .store
            .wire_receipts_after(hs.remote_last_rcpt_seq)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, r)| r)
            .collect();

        ws.send_json(&PeerFrame::SyncBatch {
            registry_events,
            messages,
            receipts,
        })?;

        let (tx, rx) = mpsc::channel::<PeerFrame>();
        let peer_label = format!("{}@{}", hs.remote_node_id, hs.remote_host);
        let peer_id = self.register_peer(peer_label, tx);

        ws.set_read_timeout(Some(Duration::from_millis(50)))?;
        let mut last_ping = Instant::now();

        let res = 'peer_loop: loop {
            if self.shutdown.load(Ordering::SeqCst) {
                let _ = ws.close();
                break Ok(());
            }

            while let Ok(out_frame) = rx.try_recv() {
                if let Err(e) = ws.send_json(&out_frame) {
                    break 'peer_loop Err(e);
                }
            }

            if last_ping.elapsed() >= crate::peer::PEER_PING_INTERVAL {
                let _ = ws.send_ping(b"ka");
                last_ping = Instant::now();
            }

            match ws.recv_json::<PeerFrame>() {
                Ok(frame) => {
                    self.process_incoming_peer_frame(peer_id, frame);
                }
                Err(WsError::Io(ref e))
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(WsError::Closed) => break Ok(()),
                Err(e) => break Err(e),
            }
        };

        self.unregister_peer(peer_id);
        res
    }

    fn process_incoming_peer_frame(&self, from_peer_id: u64, frame: PeerFrame) {
        let now = now_at();
        match frame {
            PeerFrame::SyncBatch {
                registry_events,
                messages,
                receipts,
            } => {
                if !registry_events.is_empty() {
                    let _ = self.store.import_registry_events(&registry_events);
                }
                let mut new_msgs = Vec::new();
                for m in &messages {
                    if self.store.get_wire_message(&m.id).ok().flatten().is_none() {
                        new_msgs.push(m.clone());
                    }
                }
                if self.store.import_wire_batch(&messages, &receipts).is_ok() {
                    for m in new_msgs {
                        let rcpts = self.store.list_wire_receipts(&m.id).unwrap_or_default();
                        self.notify_subscribers_message(&m, &rcpts);
                        self.broadcast_to_peers(
                            Some(from_peer_id),
                            &PeerFrame::Publish {
                                message: m.clone(),
                                receipts: rcpts,
                            },
                        );
                        self.dispatch_and_propagate(&m, now.clone());
                    }
                    for r in receipts {
                        self.notify_subscribers_receipt(&r);
                    }
                }
            }
            PeerFrame::Publish { message, receipts } => {
                let known = self
                    .store
                    .get_wire_message(&message.id)
                    .ok()
                    .flatten()
                    .is_some();
                if self
                    .store
                    .import_wire_batch(std::slice::from_ref(&message), &receipts)
                    .is_ok()
                    && !known
                {
                    let rcpts = self
                        .store
                        .list_wire_receipts(&message.id)
                        .unwrap_or(receipts);
                    self.notify_subscribers_message(&message, &rcpts);
                    self.broadcast_to_peers(
                        Some(from_peer_id),
                        &PeerFrame::Publish {
                            message: message.clone(),
                            receipts: rcpts,
                        },
                    );
                    self.dispatch_and_propagate(&message, now);
                }
            }
            PeerFrame::ReceiptUpdate { receipt } => {
                let prev_rank = self
                    .store
                    .list_wire_receipts(&receipt.message_id)
                    .ok()
                    .and_then(|list| {
                        list.into_iter()
                            .find(|r| {
                                r.recipient_id == receipt.recipient_id
                                    || r.recipient_name == receipt.recipient_name
                            })
                            .map(|r| r.status.rank())
                    })
                    .unwrap_or(0);
                if self
                    .store
                    .import_wire_batch(&[], std::slice::from_ref(&receipt))
                    .is_ok()
                    && receipt.status.rank() > prev_rank
                {
                    self.notify_subscribers_receipt(&receipt);
                    self.broadcast_to_peers(
                        Some(from_peer_id),
                        &PeerFrame::ReceiptUpdate { receipt },
                    );
                }
            }
            PeerFrame::RegistryUpdate { event } => {
                if self
                    .store
                    .import_registry_events(std::slice::from_ref(&event))
                    .is_ok()
                {
                    self.broadcast_to_peers(
                        Some(from_peer_id),
                        &PeerFrame::RegistryUpdate { event },
                    );
                }
            }
            PeerFrame::Challenge { .. } | PeerFrame::Auth { .. } => {}
        }
    }
}

pub struct DaemonHandle {
    socket_path: PathBuf,
    listen_addr: Option<String>,
    shutdown: Arc<AtomicBool>,
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl DaemonHandle {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn listen_addr(&self) -> Option<&str> {
        self.listen_addr.as_deref()
    }

    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(ref addr) = self.listen_addr {
            let _ = TcpStream::connect(addr);
        }
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn_daemon<S>(store: Arc<S>, mut config: DaemonConfig) -> io::Result<DaemonHandle>
where
    S: MessageStore + AgentRegistry + Send + Sync + 'static,
{
    if let Some(parent) = config.socket_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if config.socket_path.exists() {
        if LocalClient::connect(&config.socket_path, Duration::from_millis(50)).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "fl-wire daemon is already listening at {}",
                    config.socket_path.display()
                ),
            ));
        }
        let _ = fs::remove_file(&config.socket_path);
    }

    let unix_listener = UnixListener::bind(&config.socket_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&config.socket_path, fs::Permissions::from_mode(0o600));
    }
    unix_listener.set_nonblocking(true)?;

    let tcp_listener = if let Some(ref bind_addr) = config.listen_addr {
        let l = TcpListener::bind(bind_addr)?;
        let actual = l.local_addr()?.to_string();
        config.listen_addr = Some(actual);
        l.set_nonblocking(true)?;
        Some(l)
    } else {
        None
    };

    let initial_routes = RouteTable::load(config.routes_path.as_deref());
    let inner = Arc::new(DaemonInner {
        store,
        config: config.clone(),
        routes: Mutex::new(initial_routes),
        peers: Mutex::new(BTreeMap::new()),
        subscribers: Mutex::new(Vec::new()),
        next_peer_id: Mutex::new(0),
        shutdown: AtomicBool::new(false),
    });

    // Spawn outbound peer loops
    for peer_url in config.peers.clone() {
        let inner_peer = Arc::clone(&inner);
        std::thread::spawn(move || {
            let mut backoff = Duration::from_millis(100);
            while !inner_peer.shutdown.load(Ordering::SeqCst) {
                if let Ok(ws) = WsStream::connect(&peer_url, Duration::from_secs(2)) {
                    backoff = Duration::from_millis(100);
                    let _ = inner_peer.handle_peer_connection(ws);
                }
                if inner_peer.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        });
    }

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown_flag);
    let inner_main = Arc::clone(&inner);

    let join_handle = std::thread::spawn(move || {
        while !shutdown_clone.load(Ordering::SeqCst) && !inner_main.shutdown.load(Ordering::SeqCst)
        {
            let mut did_work = false;

            if let Ok((stream, _)) = unix_listener.accept() {
                did_work = true;
                let inner_conn = Arc::clone(&inner_main);
                std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    let Ok(mut writer) = stream.try_clone() else {
                        return;
                    };
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let req: LocalRequest = match serde_json::from_str(&line) {
                        Ok(r) => r,
                        Err(e) => {
                            let err = LocalResponse::Error {
                                message: format!("invalid request JSON: {e}"),
                            };
                            let _ = writeln!(writer, "{}", serde_json::to_string(&err).unwrap());
                            return;
                        }
                    };
                    let (resp, sub_rx) = inner_conn.handle_local_request(req);
                    if writeln!(writer, "{}", serde_json::to_string(&resp).unwrap()).is_err() {
                        return;
                    }
                    let _ = writer.flush();

                    if let Some(rx) = sub_rx {
                        while !inner_conn.shutdown.load(Ordering::SeqCst) {
                            match rx.recv_timeout(Duration::from_millis(100)) {
                                Ok(ev) => {
                                    if writeln!(writer, "{}", serde_json::to_string(&ev).unwrap())
                                        .is_err()
                                    {
                                        break;
                                    }
                                    let _ = writer.flush();
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                    }
                });
            }

            if let Some(ref tcp) = tcp_listener
                && let Ok((stream, _)) = tcp.accept()
            {
                did_work = true;
                let inner_ws = Arc::clone(&inner_main);
                std::thread::spawn(move || {
                    if let Ok(ws) = WsStream::accept(stream, Duration::from_secs(5)) {
                        let _ = inner_ws.handle_peer_connection(ws);
                    }
                });
            }

            if !did_work {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        inner_main.shutdown.store(true, Ordering::SeqCst);
    });

    Ok(DaemonHandle {
        socket_path: config.socket_path,
        listen_addr: config.listen_addr,
        shutdown: shutdown_flag,
        join_handle: Some(join_handle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ferrowire::{MessageKind, MessageUrgency, WireAddress};
    use fl_store::RedbStore;
    use tempfile::TempDir;

    #[test]
    fn two_daemons_sync_over_websocket_push_to_webhook_and_propagate_ack_and_reply() {
        let tmp = TempDir::new().unwrap();
        let store_a = Arc::new(RedbStore::open(&tmp.path().join("a.redb")).unwrap());
        let store_b = Arc::new(RedbStore::open(&tmp.path().join("b.redb")).unwrap());

        // Pre-populate a message on store_a before daemon B connects to verify initial SyncBatch catch-up
        let (boot_msg, _) = store_a
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: Some("node-a".into()),
                to: WireAddress::AgentName("kayla".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "catchup".into(),
                body: "sent before peer connected".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(10),
            })
            .unwrap();

        let sock_a = tmp.path().join("wire-a.sock");
        let daemon_a = spawn_daemon(
            Arc::clone(&store_a),
            DaemonConfig {
                node_id: "node-a".into(),
                host: "host-a".into(),
                socket_path: sock_a.clone(),
                routes_path: Some(tmp.path().join("routes-a.json")),
                home_override: Some(tmp.path().to_path_buf()),
                listen_addr: Some("127.0.0.1:0".into()),
                peers: vec![],
                psk: b"top-secret-psk".to_vec(),
            },
        )
        .unwrap();
        let addr_a = daemon_a.listen_addr().unwrap().to_string();

        // Set up a webhook Unix listener for Kayla on host B to verify push delivery + auto-Ack propagation
        let webhook_sock = tmp.path().join("kayla-webhook.sock");
        let webhook_listener = UnixListener::bind(&webhook_sock).unwrap();

        let sock_b = tmp.path().join("wire-b.sock");
        let _daemon_b = spawn_daemon(
            Arc::clone(&store_b),
            DaemonConfig {
                node_id: "node-b".into(),
                host: "host-b".into(),
                socket_path: sock_b.clone(),
                routes_path: Some(tmp.path().join("routes-b.json")),
                home_override: Some(tmp.path().to_path_buf()),
                listen_addr: None,
                peers: vec![format!("ws://{addr_a}/wire")],
                psk: b"top-secret-psk".to_vec(),
            },
        )
        .unwrap();

        // Wait for initial SyncBatch to reach store_b
        let deadline = Instant::now() + Duration::from_secs(3);
        while store_b.get_wire_message(&boot_msg.id).unwrap().is_none() {
            assert!(Instant::now() < deadline, "timed out waiting for SyncBatch");
            std::thread::sleep(Duration::from_millis(20));
        }

        // Register Kayla's webhook route on daemon B (which also dispatches pending messages)
        let mut client_b = LocalClient::connect(&sock_b, Duration::from_secs(1)).unwrap();
        client_b
            .request(&LocalRequest::RegisterRoute {
                persona: "kayla".into(),
                route: VendorRoute::Webhook {
                    socket_path: webhook_sock,
                },
            })
            .unwrap();

        // Accept the pushed boot_msg on Kayla's webhook socket
        let (wh_stream, _) = webhook_listener.accept().unwrap();
        let mut wh_line = String::new();
        BufReader::new(wh_stream).read_line(&mut wh_line).unwrap();
        let pushed: WireMessage = serde_json::from_str(&wh_line).unwrap();
        assert_eq!(pushed.id, boot_msg.id);

        // Verify Acked receipt propagated back to store_a over WebSocket!
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let rcpts = store_a.list_wire_receipts(&boot_msg.id).unwrap();
            if rcpts[0].status == DeliveryStatus::Acked {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for Acked receipt propagation to store_a"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Now test live Ask -> Subscribe -> Reply across the two daemons
        let mut client_a = LocalClient::connect(&sock_a, Duration::from_secs(1)).unwrap();
        let LocalResponse::Sent {
            message: ask_msg, ..
        } = client_a
            .request(&LocalRequest::Send {
                args: SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name: "jarvis".into(),
                    sender_host: None,
                    to: WireAddress::AgentName("kayla".into()),
                    kind: MessageKind::Ask,
                    urgency: MessageUrgency::Urgent,
                    durable: false,
                    subject: "status?".into(),
                    body: "need status".into(),
                    reply_to: None,
                    thread_id: None,
                    deadline_at: None,
                    at: At::from_unix_millis(50),
                },
            })
            .unwrap()
        else {
            panic!("expected Sent");
        };

        // Drain webhook push for ask_msg
        let _ = webhook_listener.accept().unwrap();

        // Subscribe on daemon A for replies correlated to ask_msg.id
        let mut sub_a = LocalClient::connect(&sock_a, Duration::from_secs(1)).unwrap();
        sub_a
            .subscribe(Some("jarvis".into()), Some(ask_msg.id.clone()))
            .unwrap();

        // Wait until ask_msg is present on daemon B, then send Reply on daemon B
        let deadline = Instant::now() + Duration::from_secs(3);
        while store_b.get_wire_message(&ask_msg.id).unwrap().is_none() {
            assert!(Instant::now() < deadline, "timed out waiting for ask_msg");
            std::thread::sleep(Duration::from_millis(10));
        }

        let mut reply_client_b = LocalClient::connect(&sock_b, Duration::from_secs(1)).unwrap();
        reply_client_b
            .request(&LocalRequest::Send {
                args: SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name: "kayla".into(),
                    sender_host: None,
                    to: WireAddress::AgentName("jarvis".into()),
                    kind: MessageKind::Reply,
                    urgency: MessageUrgency::Normal,
                    durable: false,
                    subject: "Re: status?".into(),
                    body: "all green".into(),
                    reply_to: Some(ask_msg.id.clone()),
                    thread_id: None,
                    deadline_at: None,
                    at: At::from_unix_millis(60),
                },
            })
            .unwrap();

        let ev = sub_a
            .wait_for_event(Duration::from_secs(3))
            .unwrap()
            .expect("expected reply event on subscriber");
        match ev {
            LocalEvent::Message { message } => {
                assert_eq!(message.kind, MessageKind::Reply);
                assert_eq!(message.correlation_id, Some(ask_msg.id));
                assert_eq!(message.body, "all green");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
```

- [ ] **Step 6: Run `fl-wire` tests**

Run:
```bash
cargo test -p fl-wire --offline
```
Expected: PASS (all 8 unit and multi-daemon integration tests pass).


---

## Task 6: `[wire]` Config, `fl wire` Subcommands, `agent-msg` Compatibility Shim, and `fl wire hook claude`

**Files:**
- Modify: `crates/cli/Cargo.toml`
- Modify: `crates/cli/src/config.rs`
- Modify: `crates/cli/src/cmd/mod.rs`
- Modify: `crates/cli/src/cmd/state.rs`
- Create: `crates/cli/src/cmd/wire.rs`
- Modify: `crates/cli/src/main.rs`
- Modify: `docs/getting-started.md`

**Interfaces:**
- Consumes: `fl_wire::{daemon, dispatcher, identity}`, `fl_core::ferrowire::*`, `fl_core::store::{AgentRegistry, MessageStore}`
- Produces: `config::WireConfig`, `cmd::wire::{WireCmd, HookCmd, AgentMsgCli, AgentMsgCmd, run, run_agent_msg, run_claude_hook}`, `argv[0]` dispatch in `crates/cli/src/main.rs` for `agent-msg`, `agent-bus-claude-hook`, and `agent-bus-mcp`

- [ ] **Step 1: Add `fl-wire` dependency to `crates/cli/Cargo.toml`, update `crates/cli/src/config.rs`, `crates/cli/src/cmd/mod.rs`, and `crates/cli/src/cmd/state.rs`**

Apply this diff to `crates/cli/Cargo.toml`, `crates/cli/src/config.rs`, `crates/cli/src/cmd/mod.rs`, and `crates/cli/src/cmd/state.rs`:

```diff
--- a/crates/cli/Cargo.toml
+++ b/crates/cli/Cargo.toml
@@ -15,6 +15,7 @@ fl-store = { path = "../store" }
 fl-exec = { path = "../exec" }
 fl-github = { path = "../github" }
 fl-mcp = { path = "../mcp" }
+fl-wire = { path = "../wire" }
 anyhow.workspace = true
 clap.workspace = true
 tokio.workspace = true
--- a/crates/cli/src/config.rs
+++ b/crates/cli/src/config.rs
@@ -176,9 +176,34 @@ pub struct McpEntry {
     pub enable: Vec<String>,
 }
 
+/// Workspace-level `[wire]` configuration (FerroWire spec §5.1).
+#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
+#[serde(deny_unknown_fields)]
+pub struct WireConfig {
+    /// Optional Unix domain socket path for local daemon IPC.
+    #[serde(default)]
+    pub socket: Option<PathBuf>,
+    /// Optional TCP address for accepting incoming WebSocket peer connections.
+    #[serde(default)]
+    pub listen: Option<String>,
+    /// Optional outbound WebSocket peer/hub URLs (`ws://` or `wss://`).
+    #[serde(default)]
+    pub peers: Vec<String>,
+    /// Optional path to file containing the pre-shared bearer token (falls back to `$FL_WIRE_TOKEN`).
+    #[serde(default)]
+    pub token_file: Option<PathBuf>,
+    /// Retention window in days for acknowledged transient messages (default: 30).
+    #[serde(default)]
+    pub retention_days: Option<u64>,
+    /// Whether `fl state sync` exports unacknowledged and durable wire messages (default: true).
+    #[serde(default)]
+    pub sync_wire: Option<bool>,
+}
+
 #[derive(Debug, Default)]
 pub struct Config {
     pub state: Option<StateConfig>,
+    pub wire: Option<WireConfig>,
     pub projects: Vec<Entry>,
     pub github: Option<GithubApp>,
     pub mcp: Vec<McpEntry>,
@@ -211,7 +236,7 @@ impl Config {
         self.workspace_state_store(default_store)
     }
 
-    /// Resolve the `StateSyncTarget` for `fl state sync` / `fl state verify` (spec §4.3).
+    /// Resolve the `StateSyncTarget` for `fl state sync` / `fl state verify` (spec §4.3, FerroWire §5.1).
     pub fn sync_target_for_scope(
         &self,
         scope: Option<&StateScope>,
@@ -221,6 +246,11 @@ impl Config {
     ) -> StateSyncTarget {
         let ws = self.state.as_ref();
         let ws_store = self.workspace_state_store(default_store);
+        let cfg_sync_wire = self
+            .wire
+            .as_ref()
+            .and_then(|w| w.sync_wire)
+            .unwrap_or(true);
         if let Some(StateScope::Project(slug)) = scope
             && let Some(entry) = self.find_project_entry(slug)
         {
@@ -241,13 +271,14 @@ impl Config {
                 .and_then(|s| s.private)
                 .or_else(|| ws.map(|s| s.private))
                 .unwrap_or(false);
-            let sync_registry = entry.store == ws_store;
+            let shares_ws_store = entry.store == ws_store;
             return StateSyncTarget {
                 repo_dir,
                 remote,
                 branch,
                 private,
-                sync_registry,
+                sync_registry: shares_ws_store,
+                sync_wire: shares_ws_store && cfg_sync_wire,
                 scope_filter: Some(StateScope::Project(slug.clone())),
                 by: by.to_string(),
             };
@@ -273,6 +304,7 @@ impl Config {
             branch,
             private,
             sync_registry: true,
+            sync_wire: cfg_sync_wire,
             scope_filter,
             by: by.to_string(),
         }
@@ -285,6 +317,8 @@ struct File {
     #[serde(default)]
     state: Option<StateConfig>,
     #[serde(default)]
+    wire: Option<WireConfig>,
+    #[serde(default)]
     project: Vec<Entry>,
     #[serde(default)]
     github: Option<GithubApp>,
@@ -444,8 +478,29 @@ pub fn load(path: Option<&Path>) -> Result<Config> {
             );
         }
     }
+    if let Some(w) = &file.wire {
+        if let Some(s) = &w.socket
+            && !s.is_absolute()
+        {
+            bail!(
+                "{}: `wire.socket` must be an absolute path (got `{}`)",
+                path.display(),
+                s.display()
+            );
+        }
+        if let Some(tf) = &w.token_file
+            && !tf.is_absolute()
+        {
+            bail!(
+                "{}: `wire.token_file` must be an absolute path (got `{}`)",
+                path.display(),
+                tf.display()
+            );
+        }
+    }
     Ok(Config {
         state: file.state,
+        wire: file.wire,
         projects: file.project,
         github: file.github,
         mcp: file.mcp,
--- a/crates/cli/src/cmd/mod.rs
+++ b/crates/cli/src/cmd/mod.rs
@@ -15,3 +15,4 @@ pub mod state;
 pub mod state_mcp;
 pub mod stats;
 pub mod transition;
+pub mod wire;
--- a/crates/cli/src/cmd/state.rs
+++ b/crates/cli/src/cmd/state.rs
@@ -970,18 +970,23 @@ pub fn run(env: &StateEnv<'_>, cmd: StateCmd) -> Result<(), String> {
                     "committed_oid": report.committed_oid,
                     "imported_registry_events": report.imported_registry_events,
                     "imported_revisions": report.imported_revisions,
+                    "imported_wire_messages": report.imported_wire_messages,
+                    "imported_wire_receipts": report.imported_wire_receipts,
                     "published_registry_events": report.published_registry_events,
                     "published_revisions": report.published_revisions,
+                    "published_wire_messages": report.published_wire_messages,
+                    "published_wire_receipts": report.published_wire_receipts,
                     "active_conflicts": report.active_conflicts,
                 });
                 println!("{}", serde_json::to_string_pretty(&val).unwrap());
             } else {
                 println!(
-                    "synced {}/{}: imported_revs={} published_revs={} conflicts={}",
+                    "synced {}/{}: imported_revs={} published_revs={} wire_msgs={} conflicts={}",
                     report.remote,
                     report.branch,
                     report.imported_revisions,
                     report.published_revisions,
+                    report.published_wire_messages,
                     report.active_conflicts
                 );
             }
@@ -1009,16 +1014,19 @@ pub fn run(env: &StateEnv<'_>, cmd: StateCmd) -> Result<(), String> {
                     "verified_commits": report.verified_commits,
                     "verified_revisions": report.verified_revisions,
                     "verified_registry_events": report.verified_registry_events,
+                    "verified_wire_messages": report.verified_wire_messages,
+                    "verified_wire_receipts": report.verified_wire_receipts,
                 });
                 println!("{}", serde_json::to_string_pretty(&val).unwrap());
             } else {
                 println!(
-                    "verified {}/{}: commits={} revisions={} registry_events={}",
+                    "verified {}/{}: commits={} revisions={} registry_events={} wire_messages={}",
                     report.remote,
                     report.branch,
                     report.verified_commits,
                     report.verified_revisions,
-                    report.verified_registry_events
+                    report.verified_registry_events,
+                    report.verified_wire_messages
                 );
             }
             Ok(())
```

- [ ] **Step 2: Create `crates/cli/src/cmd/wire.rs`**

Write `crates/cli/src/cmd/wire.rs`:

```rust
//! `fl wire` CLI subcommands, `agent-msg` compatibility shim, and `fl wire hook claude` (spec §5.2, §5.3).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use fl_core::agent::{RegisterAgentArgs, reports_to};
use fl_core::at::At;
use fl_core::ferrowire::{
    DeliveryReceipt, DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, WireAddress,
    WireFilter, WireInboxItem, WireMessage, parse_role_selector,
};
use fl_core::ids::MessageId;
use fl_core::iri::Iri;
use fl_core::state::StateScope;
use fl_core::store::{AgentRegistry, MessageStore, StoreError};
use fl_wire::daemon::{
    DEFAULT_LOCAL_CONNECT_TIMEOUT, DaemonConfig, LocalClient, LocalEvent, LocalRequest,
    LocalResponse, default_socket_path, spawn_daemon,
};
use fl_wire::dispatcher::dispatch_message_for_store;
use fl_wire::identity::{
    RouteTable, VendorRoute, capture_current_env_route, resolve_claude_key_path, resolve_persona,
};
use serde_json::{Value, json};

use crate::cmd::agent::{now_at, render_registry_markdown, resolve_agent, write_atomic_non_symlink};
use crate::cmd::state::StateEnv;

pub const DEFAULT_RETENTION_DAYS: u64 = 30;

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Subcommand)]
pub enum WireCmd {
    /// Send a message to a persona, AgentId URN, or group selector (`@all`, `@area:<slug>`, `@role:<role>`, `@area:<slug>/<role>`).
    Send {
        to: String,
        subject: String,
        body: String,
        #[arg(long, default_value = "mail")]
        kind: String,
        #[arg(long)]
        urgent: bool,
        #[arg(long)]
        durable: bool,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Broadcast a message to all active agents (`@all`), excluding the sender.
    Broadcast {
        subject: String,
        body: String,
        #[arg(long)]
        urgent: bool,
        #[arg(long)]
        durable: bool,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Send a durable `Ask` message and optionally wait up to `--timeout <secs>` for a correlated `Reply`.
    Ask {
        to: String,
        subject: String,
        body: String,
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        urgent: bool,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Reply to a message by `MessageId` (`urn:uuid:...`), inheriting its thread and correlation ID.
    Reply {
        message_id: String,
        body: String,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Read inbox messages for the resolved caller (`unread_only = !all`).
    Read {
        #[arg(long)]
        ack: bool,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long = "for")]
        for_agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Acknowledge one or more messages by `MessageId`.
    Ack {
        #[arg(required = true)]
        message_ids: Vec<String>,
        #[arg(long = "for")]
        for_agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Register the caller's live vendor route and optionally wait for an unread message.
    Wait {
        #[arg(long)]
        ack: bool,
        #[arg(long)]
        block: bool,
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long = "for")]
        for_agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Prune acknowledged or expired transient messages older than `--older-than-days`.
    Prune {
        #[arg(long)]
        older_than_days: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Report local daemon, peer, and route status.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Run the local Unix-socket and WebSocket peer relay daemon.
    Daemon {
        #[arg(long)]
        listen: Option<String>,
        #[arg(long = "peer")]
        peers: Vec<String>,
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Vendor lifecycle hooks (`fl wire hook claude`).
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// Run the stdio MCP server (`fl state mcp` alias).
    Mcp,
}

#[derive(Subcommand)]
pub enum HookCmd {
    /// Handle Claude Code / Codex lifecycle hook (`Stop`, `SessionStart`, `UserPromptSubmit`, `PostToolUse`).
    Claude,
}

/// CLI parser matching `AI-Lab-AMD/scripts/agent-bus/agent_bus/cli.py` for `agent-msg`.
#[derive(Parser)]
#[command(name = "agent-msg", about = "FerroWire agent-msg compatibility CLI")]
pub struct AgentMsgCli {
    #[arg(long, global = true)]
    pub server: Option<String>,
    #[arg(long, global = true)]
    pub from: Option<String>,
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,
    #[command(subcommand)]
    pub cmd: AgentMsgCmd,
}

#[derive(Subcommand)]
pub enum AgentMsgCmd {
    Send {
        recipient: String,
        subject: String,
        body: String,
    },
    Broadcast {
        subject: String,
        body: String,
    },
    Read {
        #[arg(long)]
        unread_only: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        ack: bool,
    },
    Ack {
        message_id: String,
    },
    Wait {
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        ack: bool,
        #[arg(long)]
        block: bool,
    },
    Listen,
    Register {
        name: String,
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "assistant")]
        role: String,
        #[arg(long = "extra-areas")]
        extra_areas: Option<String>,
        #[arg(long)]
        replace: bool,
    },
    Unregister {
        name: String,
    },
    Roster {
        #[arg(long)]
        json: bool,
    },
}

pub fn resolve_socket_path(env: &StateEnv<'_>, explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(s) = std::env::var_os("FL_WIRE_SOCKET") {
        let p = PathBuf::from(s);
        if p.is_absolute() {
            return p;
        }
    }
    if let Some(p) = env.cfg.wire.as_ref().and_then(|w| w.socket.clone()) {
        return p;
    }
    default_socket_path()
}

pub fn resolve_routes_path() -> Option<PathBuf> {
    std::env::var_os("FL_WIRE_ROUTES")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

pub fn resolve_caller(env: &StateEnv<'_>, explicit: Option<&str>, transcript: Option<&Path>) -> String {
    let store = env.open_for_scope(Some(&StateScope::Workspace)).ok();
    let reg = store.as_ref().map(|s| s as &dyn AgentRegistry);
    let rpath = resolve_routes_path();
    let persona = resolve_persona(explicit, transcript, env.cwd, reg, rpath.as_deref());
    if persona != "unknown"
        && let Some(route) = capture_current_env_route()
    {
        record_caller_route(env, &persona, route);
    }
    persona
}

pub fn record_caller_route(env: &StateEnv<'_>, persona: &str, route: VendorRoute) {
    let sock = resolve_socket_path(env, None);
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT) {
        let _ = client.request(&LocalRequest::RegisterRoute {
            persona: persona.to_string(),
            route,
        });
        return;
    }
    let rpath = resolve_routes_path();
    let mut table = RouteTable::load(rpath.as_deref());
    table.upsert(persona, route, now_at());
    let _ = table.save(rpath.as_deref());
}

pub fn parse_message_id(raw: &str) -> Result<MessageId, String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("urn:uuid:") {
        let iri = Iri::parse(trimmed).map_err(|e| format!("invalid message ID `{raw}`: {e}"))?;
        Ok(MessageId(iri))
    } else {
        let with_prefix = format!("urn:uuid:{trimmed}");
        let iri = Iri::parse(&with_prefix).map_err(|e| format!("invalid message ID `{raw}`: {e}"))?;
        Ok(MessageId(iri))
    }
}

pub fn execute_send(
    env: &StateEnv<'_>,
    args: SendWireArgs,
) -> Result<(WireMessage, Vec<DeliveryReceipt>), String> {
    let sock = resolve_socket_path(env, None);
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT) {
        match client.request(&LocalRequest::Send { args })? {
            LocalResponse::Sent { message, receipts } => return Ok((message, receipts)),
            other => return Err(format!("unexpected daemon response: {other:?}")),
        }
    }
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    let (msg, receipts) = store.send_wire(args).map_err(|e| e.to_string())?;
    let rpath = resolve_routes_path();
    let mut routes = RouteTable::load(rpath.as_deref());
    let _ = dispatch_message_for_store(&store, &mut routes, None, &msg, now_at());
    let latest_receipts = store.list_wire_receipts(&msg.id).unwrap_or(receipts);
    Ok((msg, latest_receipts))
}

pub fn execute_read(
    env: &StateEnv<'_>,
    filter: WireFilter,
    ack: bool,
) -> Result<Vec<WireInboxItem>, String> {
    let sock = resolve_socket_path(env, None);
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT) {
        match client.request(&LocalRequest::Read { filter, ack })? {
            LocalResponse::Inbox { items } => return Ok(items),
            other => return Err(format!("unexpected daemon response: {other:?}")),
        }
    }
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    let mut items = store.list_wire_inbox(&filter).map_err(|e| e.to_string())?;
    if ack
        && let Some(ref rec) = filter.recipient
    {
        let now = now_at();
        for item in &mut items {
            if item.receipt.status != DeliveryStatus::Acked
                && let Ok(Some(updated)) = store.update_wire_receipt(
                    &item.message.id,
                    rec,
                    DeliveryStatus::Acked,
                    false,
                    now.clone(),
                )
            {
                item.receipt = updated;
            }
        }
    }
    Ok(items)
}

pub fn execute_ack(
    env: &StateEnv<'_>,
    message_ids: Vec<MessageId>,
    recipient: String,
) -> Result<Vec<DeliveryReceipt>, String> {
    let sock = resolve_socket_path(env, None);
    if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT) {
        match client.request(&LocalRequest::Ack {
            message_ids,
            recipient,
        })? {
            LocalResponse::Acked { receipts } => return Ok(receipts),
            other => return Err(format!("unexpected daemon response: {other:?}")),
        }
    }
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    let now = now_at();
    let mut out = Vec::new();
    for mid in message_ids {
        if let Some(rcpt) = store
            .update_wire_receipt(&mid, &recipient, DeliveryStatus::Acked, false, now.clone())
            .map_err(|e| e.to_string())?
        {
            out.push(rcpt);
        }
    }
    Ok(out)
}

pub fn execute_reply(
    env: &StateEnv<'_>,
    parent_id: &MessageId,
    subject_override: Option<String>,
    body: String,
    sender_name: String,
) -> Result<(WireMessage, Vec<DeliveryReceipt>), String> {
    let parent = {
        let store = env.open_for_scope(Some(&StateScope::Workspace))?;
        store
            .get_wire_message(parent_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no such wire message: {parent_id}"))?
    };
    let subject = match subject_override {
        Some(s) => s,
        None => {
            if parent.subject.starts_with("Re: ") || parent.subject.starts_with("re: ") {
                parent.subject.clone()
            } else {
                format!("Re: {}", parent.subject)
            }
        }
    };
    let to = match parent.sender_id {
        Some(id) => WireAddress::AgentId(id),
        None => WireAddress::AgentName(parent.sender_name.clone()),
    };
    execute_send(
        env,
        SendWireArgs {
            id: None,
            sender_id: None,
            sender_name,
            sender_host: None,
            to,
            kind: MessageKind::Reply,
            urgency: MessageUrgency::Normal,
            durable: parent.durable,
            subject,
            body,
            reply_to: Some(parent.id),
            thread_id: Some(parent.thread_id),
            deadline_at: None,
            at: now_at(),
        },
    )
}

pub fn wait_for_reply(
    env: &StateEnv<'_>,
    ask_id: &MessageId,
    recipient: &str,
    timeout: Duration,
) -> Result<Option<WireMessage>, String> {
    let sock = resolve_socket_path(env, None);
    let deadline = Instant::now() + timeout;

    if let Ok(mut sub_client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT)
        && sub_client
            .subscribe(Some(recipient.to_string()), Some(ask_id.clone()))
            .is_ok()
    {
        // Check if reply already arrived before subscribe completed
        if let Some(reply) = poll_store_for_reply(env, ask_id)? {
            return Ok(Some(reply));
        }
        while Instant::now() < deadline {
            let rem = deadline.saturating_duration_since(Instant::now());
            if let Some(LocalEvent::Message { message }) = sub_client.wait_for_event(rem)?
                && message.correlation_id.as_ref() == Some(ask_id)
            {
                return Ok(Some(message));
            }
        }
        return Ok(None);
    }

    // Store polling fallback when daemon is not running
    loop {
        if let Some(reply) = poll_store_for_reply(env, ask_id)? {
            return Ok(Some(reply));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn poll_store_for_reply(
    env: &StateEnv<'_>,
    ask_id: &MessageId,
) -> Result<Option<WireMessage>, String> {
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    let mut replies = store
        .list_wire_replies(ask_id)
        .map_err(|e| e.to_string())?;
    Ok(replies.pop())
}

pub fn run(env: &StateEnv<'_>, cmd: WireCmd) -> Result<i32, (i32, String)> {
    match cmd {
        WireCmd::Send {
            to,
            subject,
            body,
            kind,
            urgent,
            durable,
            thread,
            from,
            json,
        } => {
            let parsed_to = WireAddress::parse(&to).map_err(|e| (1, e.to_string()))?;
            let parsed_kind = MessageKind::from_wire(&kind).ok_or_else(|| {
                (
                    1,
                    format!(
                        "unknown message kind `{kind}`; expected one of: {}",
                        MessageKind::wire_values()
                    ),
                )
            })?;
            let thread_id = thread
                .as_deref()
                .map(parse_message_id)
                .transpose()
                .map_err(|e| (1, e))?;
            let sender_name = resolve_caller(env, from.as_deref(), None);
            let urgency = if urgent {
                MessageUrgency::Urgent
            } else {
                MessageUrgency::Normal
            };
            let (msg, receipts) = execute_send(
                env,
                SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name,
                    sender_host: None,
                    to: parsed_to,
                    kind: parsed_kind,
                    urgency,
                    durable,
                    subject,
                    body,
                    reply_to: None,
                    thread_id,
                    deadline_at: None,
                    at: now_at(),
                },
            )
            .map_err(|e| (1, e))?;
            print_sent(&msg, &receipts, json);
            Ok(0)
        }
        WireCmd::Broadcast {
            subject,
            body,
            urgent,
            durable,
            from,
            json,
        } => {
            let sender_name = resolve_caller(env, from.as_deref(), None);
            let urgency = if urgent {
                MessageUrgency::Urgent
            } else {
                MessageUrgency::Normal
            };
            let (msg, receipts) = execute_send(
                env,
                SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name,
                    sender_host: None,
                    to: WireAddress::All,
                    kind: MessageKind::Mail,
                    urgency,
                    durable,
                    subject,
                    body,
                    reply_to: None,
                    thread_id: None,
                    deadline_at: None,
                    at: now_at(),
                },
            )
            .map_err(|e| (1, e))?;
            print_sent(&msg, &receipts, json);
            Ok(0)
        }
        WireCmd::Ask {
            to,
            subject,
            body,
            timeout,
            urgent,
            from,
            json,
        } => {
            let parsed_to = WireAddress::parse(&to).map_err(|e| (1, e.to_string()))?;
            let sender_name = resolve_caller(env, from.as_deref(), None);
            let urgency = if urgent {
                MessageUrgency::Urgent
            } else {
                MessageUrgency::Normal
            };
            let now_ms = now_millis();
            let now = At::from_unix_millis(now_ms);
            let deadline_at = timeout.map(|secs| At::from_unix_millis(now_ms + secs * 1000));
            let (ask_msg, receipts) = execute_send(
                env,
                SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name: sender_name.clone(),
                    sender_host: None,
                    to: parsed_to,
                    kind: MessageKind::Ask,
                    urgency,
                    durable: true,
                    subject,
                    body,
                    reply_to: None,
                    thread_id: None,
                    deadline_at,
                    at: now,
                },
            )
            .map_err(|e| (1, e))?;

            let Some(secs) = timeout else {
                print_sent(&ask_msg, &receipts, json);
                return Ok(0);
            };

            match wait_for_reply(env, &ask_msg.id, &sender_name, Duration::from_secs(secs))
                .map_err(|e| (1, e))?
            {
                Some(reply) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "ask": ask_msg,
                                "reply": reply,
                            }))
                            .unwrap()
                        );
                    } else {
                        println!(
                            "[{}] from {}: {}\n{}",
                            reply.id, reply.sender_name, reply.subject, reply.body
                        );
                    }
                    Ok(0)
                }
                None => Err((
                    124,
                    format!(
                        "timed out after {secs}s waiting for reply to {}",
                        ask_msg.id
                    ),
                )),
            }
        }
        WireCmd::Reply {
            message_id,
            body,
            subject,
            from,
            json,
        } => {
            let parent_id = parse_message_id(&message_id).map_err(|e| (1, e))?;
            let sender_name = resolve_caller(env, from.as_deref(), None);
            let (msg, receipts) = execute_reply(env, &parent_id, subject, body, sender_name)
                .map_err(|e| (1, e))?;
            print_sent(&msg, &receipts, json);
            Ok(0)
        }
        WireCmd::Read {
            ack,
            all,
            thread,
            for_agent,
            json,
        } => {
            let recipient = resolve_caller(env, for_agent.as_deref(), None);
            let thread_id = thread
                .as_deref()
                .map(parse_message_id)
                .transpose()
                .map_err(|e| (1, e))?;
            let items = execute_read(
                env,
                WireFilter {
                    recipient: Some(recipient),
                    unread_only: !all,
                    thread_id,
                    correlation_id: None,
                    limit: None,
                },
                ack,
            )
            .map_err(|e| (1, e))?;
            print_inbox_items(&items, json);
            Ok(0)
        }
        WireCmd::Ack {
            message_ids,
            for_agent,
            json,
        } => {
            let recipient = resolve_caller(env, for_agent.as_deref(), None);
            let mut ids = Vec::with_capacity(message_ids.len());
            for raw in &message_ids {
                ids.push(parse_message_id(raw).map_err(|e| (1, e))?);
            }
            let receipts = execute_ack(env, ids, recipient).map_err(|e| (1, e))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&receipts).unwrap());
            } else {
                for r in receipts {
                    println!(
                        "acked\t{}\t{}\t{}",
                        r.message_id,
                        r.recipient_name,
                        r.status.as_wire()
                    );
                }
            }
            Ok(0)
        }
        WireCmd::Wait {
            ack,
            block,
            timeout,
            for_agent,
            json,
        } => {
            let persona = resolve_caller(env, for_agent.as_deref(), None);
            let items = execute_read(
                env,
                WireFilter {
                    recipient: Some(persona.clone()),
                    unread_only: true,
                    thread_id: None,
                    correlation_id: None,
                    limit: None,
                },
                ack,
            )
            .map_err(|e| (1, e))?;

            if !items.is_empty() {
                print_inbox_items(&items, json);
                return Ok(0);
            }

            if !block && timeout.is_none() {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "status": "dispatcher_active",
                            "persona": persona,
                        }))
                        .unwrap()
                    );
                } else {
                    println!(
                        "Central FerroWire dispatcher is active for '{persona}'. No background wait process is needed."
                    );
                }
                return Ok(0);
            }

            let wait_dur = Duration::from_secs(timeout.unwrap_or(300));
            let deadline = Instant::now() + wait_dur;
            let sock = resolve_socket_path(env, None);

            if let Ok(mut sub) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT)
                && sub.subscribe(Some(persona.clone()), None).is_ok()
            {
                while Instant::now() < deadline {
                    let rem = deadline.saturating_duration_since(Instant::now());
                    if let Some(LocalEvent::Message { .. }) =
                        sub.wait_for_event(rem).map_err(|e| (1, e))?
                    {
                        let arrived = execute_read(
                            env,
                            WireFilter {
                                recipient: Some(persona),
                                unread_only: true,
                                thread_id: None,
                                correlation_id: None,
                                limit: None,
                            },
                            ack,
                        )
                        .map_err(|e| (1, e))?;
                        print_inbox_items(&arrived, json);
                        return Ok(0);
                    }
                }
                return Ok(2);
            }

            while Instant::now() < deadline {
                let arrived = execute_read(
                    env,
                    WireFilter {
                        recipient: Some(persona.clone()),
                        unread_only: true,
                        thread_id: None,
                        correlation_id: None,
                        limit: None,
                    },
                    ack,
                )
                .map_err(|e| (1, e))?;
                if !arrived.is_empty() {
                    print_inbox_items(&arrived, json);
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(2)
        }
        WireCmd::Prune {
            older_than_days,
            json,
        } => {
            let days = older_than_days
                .or_else(|| env.cfg.wire.as_ref().and_then(|w| w.retention_days))
                .unwrap_or(DEFAULT_RETENTION_DAYS);
            let cutoff_ms = now_millis().saturating_sub(days * 86_400_000);
            let before = At::from_unix_millis(cutoff_ms);
            let store = env
                .open_for_scope(Some(&StateScope::Workspace))
                .map_err(|e| (1, e))?;
            let pruned = store.prune_wire(before).map_err(|e| (1, e.to_string()))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "pruned": pruned,
                        "older_than_days": days,
                    }))
                    .unwrap()
                );
            } else {
                println!("pruned {pruned} transient wire messages older than {days}d");
            }
            Ok(0)
        }
        WireCmd::Status { json } => {
            let sock = resolve_socket_path(env, None);
            if let Ok(mut client) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT)
                && let Ok(LocalResponse::Status(st)) = client.request(&LocalRequest::Status)
            {
                if json {
                    println!("{}", serde_json::to_string_pretty(&st).unwrap());
                } else {
                    println!(
                        "daemon=active\tnode={}\thost={}\tsocket={}\tpeers={}\troutes={}",
                        st.node_id,
                        st.host,
                        st.socket_path.display(),
                        st.connected_peers.len(),
                        st.routes.len()
                    );
                }
                return Ok(0);
            }
            let rpath = resolve_routes_path();
            let routes = RouteTable::load(rpath.as_deref());
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "daemon": "inactive",
                        "socket_path": sock,
                        "routes": routes.entries.values().collect::<Vec<_>>(),
                    }))
                    .unwrap()
                );
            } else {
                println!(
                    "daemon=inactive\tsocket={}\troutes={}",
                    sock.display(),
                    routes.entries.len()
                );
            }
            Ok(0)
        }
        WireCmd::Daemon {
            listen,
            peers,
            socket,
            token_file,
        } => {
            let sock = resolve_socket_path(env, socket.as_deref());
            let listen_addr = listen
                .or_else(|| env.cfg.wire.as_ref().and_then(|w| w.listen.clone()));
            let mut peer_list = peers;
            if peer_list.is_empty()
                && let Some(cfg_peers) = env.cfg.wire.as_ref().map(|w| &w.peers)
            {
                peer_list = cfg_peers.clone();
            }
            let tfile = token_file
                .or_else(|| env.cfg.wire.as_ref().and_then(|w| w.token_file.clone()));
            let psk = if let Some(tf) = tfile {
                std::fs::read_to_string(&tf)
                    .map_err(|e| (1, format!("could not read {}: {e}", tf.display())))?
                    .trim()
                    .as_bytes()
                    .to_vec()
            } else if let Ok(tok) = std::env::var("FL_WIRE_TOKEN") {
                tok.trim().as_bytes().to_vec()
            } else {
                Vec::new()
            };

            let store = env
                .open_for_scope(Some(&StateScope::Workspace))
                .map_err(|e| (1, e))?;
            let host = std::env::var("HOSTNAME")
                .or_else(|_| {
                    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string())
                })
                .unwrap_or_else(|_| "localhost".into());
            let handle = spawn_daemon(
                Arc::new(store),
                DaemonConfig {
                    node_id: host.clone(),
                    host,
                    socket_path: sock,
                    routes_path: resolve_routes_path(),
                    home_override: None,
                    listen_addr,
                    peers: peer_list,
                    psk,
                },
            )
            .map_err(|e| (1, e.to_string()))?;

            // Keep foreground daemon alive until Shutdown request
            while handle.socket_path().exists() {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(0)
        }
        WireCmd::Hook { cmd } => match cmd {
            HookCmd::Claude => run_claude_hook(env),
        },
        WireCmd::Mcp => crate::cmd::state_mcp::run(env)
            .map(|()| 0)
            .map_err(|e| (1, e)),
    }
}

pub fn run_claude_hook(env: &StateEnv<'_>) -> Result<i32, (i32, String)> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| (1, e.to_string()))?;
    if raw.trim().is_empty() {
        println!("{{}}");
        return Ok(0);
    }
    let payload: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
    if payload
        .get("stop_hook_active")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        println!("{{}}");
        return Ok(0);
    }
    let event_name = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("Stop");
    let transcript_path = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let hook_cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| env.cwd.to_path_buf());
    let session_id = payload
        .get("session_id")
        .or_else(|| payload.get("thread_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let hook_env = StateEnv {
        explicit_db: env.explicit_db,
        default_store: env.default_store,
        cfg: env.cfg,
        cwd: &hook_cwd,
    };

    let persona = resolve_caller(&hook_env, None, transcript_path.as_deref());
    if persona == "unknown" {
        println!("{{}}");
        return Ok(0);
    }

    // Register route for Codex or Claude
    let is_codex = transcript_path
        .as_ref()
        .is_some_and(|p| p.to_string_lossy().contains(".codex"));
    if is_codex && !session_id.is_empty() {
        record_caller_route(
            &hook_env,
            &persona,
            VendorRoute::Codex {
                thread_id: session_id,
                cwd: Some(hook_cwd.clone()),
            },
        );
    } else if let Some(sock) = std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET") {
        let socket_path = PathBuf::from(sock);
        let pid = socket_path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or_else(std::process::id);
        let key_path = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|h| {
                resolve_claude_key_path(&h.join(".claude").join("sessions"), pid, &socket_path)
            });
        record_caller_route(
            &hook_env,
            &persona,
            VendorRoute::Claude {
                pid,
                session_id,
                socket_path,
                key_path,
            },
        );
    }

    let items = execute_read(
        &hook_env,
        WireFilter {
            recipient: Some(persona.clone()),
            unread_only: true,
            thread_id: None,
            correlation_id: None,
            limit: None,
        },
        true,
    )
    .unwrap_or_default();

    if items.is_empty() {
        println!("{{}}");
        return Ok(0);
    }

    let mut formatted = Vec::with_capacity(items.len());
    for item in &items {
        formatted.push(format!(
            "[fl-wire {}/{}] from {} (id: {}): {}\n{}",
            item.message.kind.as_wire(),
            item.message.urgency.as_wire(),
            item.message.sender_name,
            item.message.id,
            item.message.subject,
            item.message.body
        ));
    }
    let combined = formatted.join("\n\n---\n\n");

    let out = match event_name {
        "Stop" => json!({
            "decision": "block",
            "reason": format!("Incoming FerroWire message(s) for '{persona}':\n\n{combined}"),
        }),
        "UserPromptSubmit" | "PostToolUse" | "SessionStart" => json!({
            "hookSpecificOutput": {
                "hookEventName": event_name,
                "additionalContext": format!("Incoming FerroWire message(s) for '{persona}':\n\n{combined}"),
            }
        }),
        _ => json!({}),
    };
    println!("{}", serde_json::to_string(&out).unwrap());
    Ok(0)
}

pub fn run_agent_msg(env: &StateEnv<'_>, cli: AgentMsgCli) -> Result<i32, (i32, String)> {
    let from = cli.from;
    match cli.cmd {
        AgentMsgCmd::Send {
            recipient,
            subject,
            body,
        } => run(
            env,
            WireCmd::Send {
                to: recipient,
                subject,
                body,
                kind: "mail".into(),
                urgent: false,
                durable: false,
                thread: None,
                from,
                json: false,
            },
        ),
        AgentMsgCmd::Broadcast { subject, body } => run(
            env,
            WireCmd::Broadcast {
                subject,
                body,
                urgent: false,
                durable: false,
                from,
                json: false,
            },
        ),
        AgentMsgCmd::Read {
            unread_only,
            json,
            ack,
        } => run(
            env,
            WireCmd::Read {
                ack,
                all: !unread_only,
                thread: None,
                for_agent: from,
                json,
            },
        ),
        AgentMsgCmd::Ack { message_id } => run(
            env,
            WireCmd::Ack {
                message_ids: vec![message_id],
                for_agent: from,
                json: false,
            },
        ),
        AgentMsgCmd::Wait {
            timeout,
            ack,
            block,
        } => run(
            env,
            WireCmd::Wait {
                ack,
                block,
                timeout,
                for_agent: from,
                json: false,
            },
        ),
        AgentMsgCmd::Listen => run_listen_loop(env, from.as_deref()),
        AgentMsgCmd::Register {
            name,
            project,
            role,
            extra_areas,
            replace,
        } => {
            let parsed_role = parse_role_selector(&role).map_err(|e| (1, e.to_string()))?;
            let extras: Vec<String> = extra_areas
                .as_deref()
                .map(|csv| {
                    csv.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let by = name.trim().to_ascii_lowercase();
            let store = env
                .open_for_scope(Some(&StateScope::Workspace))
                .map_err(|e| (1, e))?;
            match store.register_agent(RegisterAgentArgs {
                display_name: name,
                email: None,
                project,
                role: parsed_role,
                extra_areas: extras,
                create_area: true,
                replace,
                at: now_at(),
                by,
            }) {
                Ok(rec) => {
                    if let Some(route) = capture_current_env_route() {
                        record_caller_route(env, &rec.name_key, route);
                    }
                    // Render `.agents/REGISTRY.md` if present in ancestor workspace directory
                    for ancestor in env.cwd.ancestors() {
                        let candidate = ancestor.join(".agents").join("REGISTRY.md");
                        if candidate.exists()
                            && let (Ok(areas), Ok(agents)) =
                                (store.list_areas(), store.list_agents(None, false))
                        {
                            let md = render_registry_markdown(&areas, &agents);
                            let _ = write_atomic_non_symlink(&candidate, md.as_bytes());
                            break;
                        }
                    }
                    let all_active = store.list_agents(None, false).unwrap_or_default();
                    let mgr = reports_to(&rec, &all_active);
                    println!(
                        "Registered {} ({}) as {} in {} (reports to {})",
                        rec.display_name,
                        rec.id,
                        rec.role.as_wire(),
                        rec.project,
                        mgr
                    );
                    Ok(0)
                }
                Err(e @ (StoreError::AgentCollision { .. } | StoreError::LeadTaken { .. })) => {
                    Err((2, e.to_string()))
                }
                Err(e) => Err((1, e.to_string())),
            }
        }
        AgentMsgCmd::Unregister { name } => {
            let store = env
                .open_for_scope(Some(&StateScope::Workspace))
                .map_err(|e| (1, e))?;
            let target = resolve_agent(&store, &name).map_err(|e| (1, e))?;
            let by = target.name_key.clone();
            let retired = store
                .retire_agent(&target.id, now_at(), &by)
                .map_err(|e| (1, e.to_string()))?;
            println!("Retired {} ({})", retired.display_name, retired.id);
            Ok(0)
        }
        AgentMsgCmd::Roster { json } => {
            let store = env
                .open_for_scope(Some(&StateScope::Workspace))
                .map_err(|e| (1, e))?;
            crate::cmd::agent::run(
                &store,
                crate::cmd::agent::AgentCmd::Roster {
                    project: None,
                    include_retired: false,
                    render: None,
                    json,
                },
            )
            .map(|()| 0)
        }
    }
}

fn run_listen_loop(env: &StateEnv<'_>, explicit_from: Option<&str>) -> Result<i32, (i32, String)> {
    let persona = resolve_caller(env, explicit_from, None);
    let sock = resolve_socket_path(env, None);
    if let Ok(mut sub) = LocalClient::connect(&sock, DEFAULT_LOCAL_CONNECT_TIMEOUT)
        && sub.subscribe(Some(persona.clone()), None).is_ok()
    {
        loop {
            if let Some(LocalEvent::Message { message }) = sub
                .wait_for_event(Duration::from_secs(60))
                .map_err(|e| (1, e))?
            {
                println!(
                    "[{}] {} -> {}: {}\n{}",
                    message.id, message.sender_name, persona, message.subject, message.body
                );
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let items = execute_read(
            env,
            WireFilter {
                recipient: Some(persona.clone()),
                unread_only: true,
                thread_id: None,
                correlation_id: None,
                limit: None,
            },
            false,
        )
        .map_err(|e| (1, e))?;
        for item in items {
            if seen.insert(item.message.id.clone()) {
                print_inbox_items(&[item], false);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn print_sent(msg: &WireMessage, receipts: &[DeliveryReceipt], json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "message": msg,
                "receipts": receipts,
            }))
            .unwrap()
        );
    } else {
        let rcpt_list = receipts
            .iter()
            .map(|r| format!("{}:{}", r.recipient_name, r.status.as_wire()))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "sent\t{}\tthread={}\tto={}\treceipts=[{}]",
            msg.id, msg.thread_id, msg.to, rcpt_list
        );
    }
}

fn print_inbox_items(items: &[WireInboxItem], json: bool) {
    if json {
        let legacy_items: Vec<Value> = items.iter().map(inbox_item_to_json).collect();
        println!("{}", serde_json::to_string_pretty(&legacy_items).unwrap());
    } else {
        for item in items {
            println!(
                "[{}] {} -> {} ({}/{} status={}): {}\n{}",
                item.message.id,
                item.message.sender_name,
                item.receipt.recipient_name,
                item.message.kind.as_wire(),
                item.message.urgency.as_wire(),
                item.receipt.status.as_wire(),
                item.message.subject,
                item.message.body
            );
        }
    }
}

pub fn inbox_item_to_json(item: &WireInboxItem) -> Value {
    json!({
        "id": item.message.id,
        "thread_id": item.message.thread_id,
        "reply_to": item.message.reply_to,
        "correlation_id": item.message.correlation_id,
        "sender": item.message.sender_name,
        "to": item.message.to,
        "recipient": item.receipt.recipient_name,
        "kind": item.message.kind,
        "urgency": item.message.urgency,
        "durable": item.message.durable,
        "timestamp": item.message.created_at,
        "subject": item.message.subject,
        "body": item.message.body,
        "status": item.receipt.status,
    })
}
```

- [ ] **Step 3: Wire `Command::Wire`, `fl agent-msg`, and `argv[0]` dispatch into `crates/cli/src/main.rs` and update `docs/getting-started.md`**

Apply this diff to `crates/cli/src/main.rs` and `docs/getting-started.md`:

```diff
--- a/crates/cli/src/main.rs
+++ b/crates/cli/src/main.rs
@@ -69,6 +69,11 @@ enum Command {
         #[command(subcommand)]
         cmd: cmd::state::StateCmd,
     },
+    /// Send, read, acknowledge, and relay FerroWire agent messages.
+    Wire {
+        #[command(subcommand)]
+        cmd: cmd::wire::WireCmd,
+    },
 }
 
 impl Command {
@@ -87,7 +92,10 @@ impl Command {
             Command::Manifest(c) => c.iris(),
             Command::Github(c) => c.iris(),
             Command::Routing(c) => c.iris(),
-            Command::Mcp(_) | Command::Agent { .. } | Command::State { .. } => Vec::new(),
+            Command::Mcp(_)
+            | Command::Agent { .. }
+            | Command::State { .. }
+            | Command::Wire { .. } => Vec::new(),
         }
     }
 
@@ -118,7 +126,10 @@ impl Command {
             Command::Manifest(c) => c.has_handle(),
             Command::Github(c) => c.has_handle(),
             Command::Routing(c) => c.has_handle(),
-            Command::Mcp(_) | Command::Agent { .. } | Command::State { .. } => false,
+            Command::Mcp(_)
+            | Command::Agent { .. }
+            | Command::State { .. }
+            | Command::Wire { .. } => false,
         }
     }
 
@@ -145,7 +156,8 @@ impl Command {
             | Command::Manifest(_)
             | Command::Mcp(_)
             | Command::Agent { .. }
-            | Command::State { .. } => false,
+            | Command::State { .. }
+            | Command::Wire { .. } => false,
         }
     }
 }
@@ -606,17 +618,125 @@ fn tracker_for(
 }
 
 fn main() {
-    let cli = Cli::parse();
-    let code = match run(cli) {
-        Ok(code) => code,
-        Err(e) => {
-            eprintln!("error: {e:#}");
-            2
+    let argv0_stem = std::env::args_os()
+        .next()
+        .and_then(|s| PathBuf::from(s).file_stem().map(ToOwned::to_owned));
+    let argv0_str = argv0_stem.as_deref().and_then(|s| s.to_str());
+
+    let code = match argv0_str {
+        Some("agent-msg") => {
+            let cli = cmd::wire::AgentMsgCli::parse();
+            match run_agent_msg_entry(cli.db.clone(), cli) {
+                Ok(c) => c,
+                Err(e) => {
+                    eprintln!("error: {e:#}");
+                    2
+                }
+            }
+        }
+        Some("agent-bus-claude-hook") => match run_claude_hook_entry() {
+            Ok(c) => c,
+            Err(e) => {
+                eprintln!("error: {e:#}");
+                2
+            }
+        },
+        Some("agent-bus-mcp") => match run_mcp_entry() {
+            Ok(c) => c,
+            Err(e) => {
+                eprintln!("error: {e:#}");
+                2
+            }
+        },
+        _ => {
+            if std::env::args_os()
+                .nth(1)
+                .and_then(|s| s.to_str().map(ToOwned::to_owned))
+                .as_deref()
+                == Some("agent-msg")
+            {
+                let shifted = std::iter::once(std::ffi::OsString::from("agent-msg"))
+                    .chain(std::env::args_os().skip(2));
+                let cli = cmd::wire::AgentMsgCli::parse_from(shifted);
+                match run_agent_msg_entry(cli.db.clone(), cli) {
+                    Ok(c) => c,
+                    Err(e) => {
+                        eprintln!("error: {e:#}");
+                        2
+                    }
+                }
+            } else {
+                let cli = Cli::parse();
+                match run(cli) {
+                    Ok(code) => code,
+                    Err(e) => {
+                        eprintln!("error: {e:#}");
+                        2
+                    }
+                }
+            }
         }
     };
     std::process::exit(code);
 }
 
+fn run_agent_msg_entry(
+    flag_db: Option<PathBuf>,
+    cli: cmd::wire::AgentMsgCli,
+) -> Result<i32> {
+    let cwd = std::env::current_dir().context("could not determine the current directory")?;
+    let cfg = config::load(config::path().as_deref())?;
+    let explicit = explicit_db(flag_db);
+    let default_store = default_state_store_path()?;
+    let state_env = cmd::state::StateEnv {
+        explicit_db: explicit.as_deref(),
+        default_store: &default_store,
+        cfg: &cfg,
+        cwd: &cwd,
+    };
+    match cmd::wire::run_agent_msg(&state_env, cli) {
+        Ok(code) => Ok(code),
+        Err((code, msg)) => {
+            eprintln!("error: {msg}");
+            Ok(code)
+        }
+    }
+}
+
+fn run_claude_hook_entry() -> Result<i32> {
+    let cwd = std::env::current_dir().context("could not determine the current directory")?;
+    let cfg = config::load(config::path().as_deref())?;
+    let explicit = explicit_db(None);
+    let default_store = default_state_store_path()?;
+    let state_env = cmd::state::StateEnv {
+        explicit_db: explicit.as_deref(),
+        default_store: &default_store,
+        cfg: &cfg,
+        cwd: &cwd,
+    };
+    match cmd::wire::run_claude_hook(&state_env) {
+        Ok(code) => Ok(code),
+        Err((code, msg)) => {
+            eprintln!("error: {msg}");
+            Ok(code)
+        }
+    }
+}
+
+fn run_mcp_entry() -> Result<i32> {
+    let cwd = std::env::current_dir().context("could not determine the current directory")?;
+    let cfg = config::load(config::path().as_deref())?;
+    let explicit = explicit_db(None);
+    let default_store = default_state_store_path()?;
+    let state_env = cmd::state::StateEnv {
+        explicit_db: explicit.as_deref(),
+        default_store: &default_store,
+        cfg: &cfg,
+        cwd: &cwd,
+    };
+    match cmd::state_mcp::run(&state_env) {
+        Ok(()) => Ok(0),
+        Err(msg) => {
+            eprintln!("error: {msg}");
+            Ok(1)
+        }
+    }
+}
+
 fn run(cli: Cli) -> Result<i32> {
     let cwd = std::env::current_dir().context("could not determine the current directory")?;
     let cfg = config::load(config::path().as_deref())?;
@@ -683,6 +803,26 @@ fn run(cli: Cli) -> Result<i32> {
                 }
             };
         }
+        Cli {
+            db,
+            command: Command::Wire { cmd },
+        } => {
+            let explicit = explicit_db(db);
+            let default_store = default_state_store_path()?;
+            let state_env = cmd::state::StateEnv {
+                explicit_db: explicit.as_deref(),
+                default_store: &default_store,
+                cfg: &cfg,
+                cwd: &cwd,
+            };
+            return match cmd::wire::run(&state_env, cmd) {
+                Ok(code) => Ok(code),
+                Err((code, msg)) => {
+                    eprintln!("error: {msg}");
+                    Ok(code)
+                }
+            };
+        }
         cli => cli,
     };
     let entries = &cfg.projects;
@@ -987,7 +1127,10 @@ fn run(cli: Cli) -> Result<i32> {
         Command::Manifest(c) => cmd::manifest::run(&store, c, &manifest_binding),
         Command::Github(c) => cmd::github::run(&ctx, c, entry.as_ref().map(|e| e.root.as_path())),
         Command::Routing(c) => cmd::routing::run(&ctx, c, here_for_store.map(|b| b.is_some())),
-        Command::Mcp(_) | Command::Agent { .. } | Command::State { .. } => {
+        Command::Mcp(_)
+        | Command::Agent { .. }
+        | Command::State { .. }
+        | Command::Wire { .. } => {
             unreachable!("dispatched before opening catalog/tracker store")
         }
     };
--- a/docs/getting-started.md
+++ b/docs/getting-started.md
@@ -67,6 +67,7 @@ Commands:
   mcp         A project's MCP servers: one committed catalog, written into each agent CLI's file
   agent       Manage the workspace agent and coverage-area registry
   state       Manage durable memories, handoffs, runtime state, and git state sync
+  wire        Send, read, acknowledge, and relay FerroWire agent messages
   help        Print this message or the help of the given subcommand(s)
 
 Options:
```

- [ ] **Step 4: Run `getting_started` test**

Run:
```bash
cargo test -p fl-cli --test getting_started --offline
```
Expected: PASS.

---

## Task 7: Unified MCP Server Tools (`fl state mcp` / `fl wire mcp` / `agent-bus-mcp`) and End-to-End Integration Tests

**Files:**
- Modify: `crates/cli/src/cmd/state_mcp.rs`
- Modify: `crates/cli/tests/state_sync.rs`
- Create: `crates/cli/tests/wire_e2e.rs`

**Interfaces:**
- Consumes: `cmd::wire::{execute_send, execute_read, execute_ack, execute_reply, wait_for_reply, resolve_caller, inbox_item_to_json}`
- Produces: 6 FerroWire MCP tools (`send_message`, `broadcast_message`, `ask_message`, `reply_message`, `get_messages`, `ack_message`) in `crates/cli/src/cmd/state_mcp.rs` and end-to-end integration tests in `crates/cli/tests/wire_e2e.rs`

- [ ] **Step 1: Add FerroWire MCP tools to `crates/cli/src/cmd/state_mcp.rs` and update `crates/cli/tests/state_sync.rs`**

Apply this diff to `crates/cli/src/cmd/state_mcp.rs` and `crates/cli/tests/state_sync.rs`:

```diff
--- a/crates/cli/src/cmd/state_mcp.rs
+++ b/crates/cli/src/cmd/state_mcp.rs
@@ -6,6 +6,10 @@
 use std::io::{BufRead, Write};
 
 use fl_core::agent::{AgentRole, RegisterAgentArgs, reports_to};
+use fl_core::at::At;
+use fl_core::ferrowire::{
+    MessageKind, MessageUrgency, SendWireArgs, WireAddress, WireFilter,
+};
 use fl_core::state::{
     HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
     RuntimeEntry, SensitivityPatch, StateConcern, StateEntry, StatePayload, StateScope,
@@ -13,12 +17,17 @@ use fl_core::state::{
 };
 use fl_core::store::{AgentRegistry, StateStore};
 use serde_json::{Value, json};
+use std::time::Duration;
 
 use crate::cmd::agent::now_at;
 use crate::cmd::state::{
     DEFAULT_RENDER_MAX_BYTES, StateEnv, parse_concern, parse_memory_basis, parse_memory_category,
     parse_merge_mode, render_state_markdown, resolve_state_in_store,
 };
+use crate::cmd::wire::{
+    execute_ack, execute_read, execute_reply, execute_send, inbox_item_to_json, now_millis,
+    parse_message_id, resolve_caller, wait_for_reply,
+};
 
 fn parse_memory_lifecycle(s: &str) -> Result<MemoryLifecycle, String> {
     MemoryLifecycle::from_wire(s).ok_or_else(|| {
@@ -517,6 +526,210 @@ fn dispatch_tool(env: &StateEnv<'_>, name: &str, args: &Value) -> Result<String,
                 .map_err(|e| e.to_string())?;
             Ok(serde_json::to_string_pretty(&entry).unwrap())
         }
+        "send_message" => {
+            let to_raw = args
+                .get("to")
+                .or_else(|| args.get("recipient"))
+                .and_then(Value::as_str)
+                .ok_or_else(|| "missing required string argument `to`".to_string())?;
+            let to = WireAddress::parse(to_raw).map_err(|e| e.to_string())?;
+            let subject = req_str(args, "subject")?.to_string();
+            let body = req_str(args, "body")?.to_string();
+            let kind = match args.get("kind").and_then(Value::as_str) {
+                Some(k) => MessageKind::from_wire(k).ok_or_else(|| {
+                    format!(
+                        "unknown message kind `{k}`; expected one of: {}",
+                        MessageKind::wire_values()
+                    )
+                })?,
+                None => MessageKind::Mail,
+            };
+            let urgent = args.get("urgent").and_then(Value::as_bool).unwrap_or(false);
+            let durable = args
+                .get("durable")
+                .and_then(Value::as_bool)
+                .unwrap_or(false);
+            let thread_id = args
+                .get("thread_id")
+                .and_then(Value::as_str)
+                .map(parse_message_id)
+                .transpose()?;
+            let from_agent = args.get("from_agent").and_then(Value::as_str);
+            let sender_name = resolve_caller(env, from_agent, None);
+            let urgency = if urgent {
+                MessageUrgency::Urgent
+            } else {
+                MessageUrgency::Normal
+            };
+            let (msg, receipts) = execute_send(
+                env,
+                SendWireArgs {
+                    id: None,
+                    sender_id: None,
+                    sender_name,
+                    sender_host: None,
+                    to,
+                    kind,
+                    urgency,
+                    durable,
+                    subject,
+                    body,
+                    reply_to: None,
+                    thread_id,
+                    deadline_at: None,
+                    at: now_at(),
+                },
+            )?;
+            Ok(serde_json::to_string_pretty(&json!({
+                "status": "sent",
+                "message": msg,
+                "receipts": receipts,
+            }))
+            .unwrap())
+        }
+        "broadcast_message" => {
+            let subject = req_str(args, "subject")?.to_string();
+            let body = req_str(args, "body")?.to_string();
+            let urgent = args.get("urgent").and_then(Value::as_bool).unwrap_or(false);
+            let durable = args
+                .get("durable")
+                .and_then(Value::as_bool)
+                .unwrap_or(false);
+            let from_agent = args.get("from_agent").and_then(Value::as_str);
+            let sender_name = resolve_caller(env, from_agent, None);
+            let urgency = if urgent {
+                MessageUrgency::Urgent
+            } else {
+                MessageUrgency::Normal
+            };
+            let (msg, receipts) = execute_send(
+                env,
+                SendWireArgs {
+                    id: None,
+                    sender_id: None,
+                    sender_name,
+                    sender_host: None,
+                    to: WireAddress::All,
+                    kind: MessageKind::Mail,
+                    urgency,
+                    durable,
+                    subject,
+                    body,
+                    reply_to: None,
+                    thread_id: None,
+                    deadline_at: None,
+                    at: now_at(),
+                },
+            )?;
+            Ok(serde_json::to_string_pretty(&json!({
+                "status": "broadcast",
+                "message": msg,
+                "receipts": receipts,
+            }))
+            .unwrap())
+        }
+        "ask_message" => {
+            let to = WireAddress::parse(req_str(args, "to")?).map_err(|e| e.to_string())?;
+            let subject = req_str(args, "subject")?.to_string();
+            let body = req_str(args, "body")?.to_string();
+            let timeout_secs = args.get("timeout_secs").and_then(Value::as_u64);
+            let from_agent = args.get("from_agent").and_then(Value::as_str);
+            let sender_name = resolve_caller(env, from_agent, None);
+            let now_ms = now_millis();
+            let now = At::from_unix_millis(now_ms);
+            let deadline_at =
+                timeout_secs.map(|s| At::from_unix_millis(now_ms + s * 1000));
+            let (ask_msg, receipts) = execute_send(
+                env,
+                SendWireArgs {
+                    id: None,
+                    sender_id: None,
+                    sender_name: sender_name.clone(),
+                    sender_host: None,
+                    to,
+                    kind: MessageKind::Ask,
+                    urgency: MessageUrgency::Normal,
+                    durable: true,
+                    subject,
+                    body,
+                    reply_to: None,
+                    thread_id: None,
+                    deadline_at,
+                    at: now,
+                },
+            )?;
+            if let Some(secs) = timeout_secs {
+                let reply =
+                    wait_for_reply(env, &ask_msg.id, &sender_name, Duration::from_secs(secs))?;
+                Ok(serde_json::to_string_pretty(&json!({
+                    "ask": ask_msg,
+                    "receipts": receipts,
+                    "reply": reply,
+                }))
+                .unwrap())
+            } else {
+                Ok(serde_json::to_string_pretty(&json!({
+                    "ask": ask_msg,
+                    "receipts": receipts,
+                }))
+                .unwrap())
+            }
+        }
+        "reply_message" => {
+            let parent_id = parse_message_id(req_str(args, "message_id")?)?;
+            let body = req_str(args, "body")?.to_string();
+            let subject = args
+                .get("subject")
+                .and_then(Value::as_str)
+                .map(ToOwned::to_owned);
+            let from_agent = args.get("from_agent").and_then(Value::as_str);
+            let sender_name = resolve_caller(env, from_agent, None);
+            let (msg, receipts) = execute_reply(env, &parent_id, subject, body, sender_name)?;
+            Ok(serde_json::to_string_pretty(&json!({
+                "status": "replied",
+                "message": msg,
+                "receipts": receipts,
+            }))
+            .unwrap())
+        }
+        "get_messages" => {
+            let unread_only = args
+                .get("unread_only")
+                .and_then(Value::as_bool)
+                .unwrap_or(true);
+            let ack = args.get("ack").and_then(Value::as_bool).unwrap_or(false);
+            let agent = args.get("agent").and_then(Value::as_str);
+            let recipient = resolve_caller(env, agent, None);
+            let thread_id = args
+                .get("thread_id")
+                .and_then(Value::as_str)
+                .map(parse_message_id)
+                .transpose()?;
+            let items = execute_read(
+                env,
+                WireFilter {
+                    recipient: Some(recipient),
+                    unread_only,
+                    thread_id,
+                    correlation_id: None,
+                    limit: None,
+                },
+                ack,
+            )?;
+            let out: Vec<Value> = items.iter().map(inbox_item_to_json).collect();
+            Ok(serde_json::to_string_pretty(&out).unwrap())
+        }
+        "ack_message" => {
+            let mid = parse_message_id(req_str(args, "message_id")?)?;
+            let agent = args.get("agent").and_then(Value::as_str);
+            let recipient = resolve_caller(env, agent, None);
+            let receipts = execute_ack(env, vec![mid], recipient)?;
+            Ok(serde_json::to_string_pretty(&json!({
+                "status": "acknowledged",
+                "receipts": receipts,
+            }))
+            .unwrap())
+        }
         other => Err(format!("unknown tool `{other}`")),
     }
 }
@@ -650,6 +863,94 @@ fn tool_definitions() -> Value {
                     "by": { "type": "string" }
                 }
             }
+        },
+        {
+            "name": "send_message",
+            "description": "Send a FerroWire message to an agent persona, AgentId URN, or group selector (@all, @area:<slug>, @role:<role>, @area:<slug>/<role>).",
+            "inputSchema": {
+                "type": "object",
+                "required": ["to", "subject", "body"],
+                "properties": {
+                    "to": { "type": "string" },
+                    "recipient": { "type": "string" },
+                    "subject": { "type": "string" },
+                    "body": { "type": "string" },
+                    "kind": { "type": "string" },
+                    "urgent": { "type": "boolean" },
+                    "durable": { "type": "boolean" },
+                    "thread_id": { "type": "string" },
+                    "from_agent": { "type": "string" }
+                }
+            }
+        },
+        {
+            "name": "broadcast_message",
+            "description": "Broadcast a FerroWire message to all active agents (@all), excluding the sender.",
+            "inputSchema": {
+                "type": "object",
+                "required": ["subject", "body"],
+                "properties": {
+                    "subject": { "type": "string" },
+                    "body": { "type": "string" },
+                    "urgent": { "type": "boolean" },
+                    "durable": { "type": "boolean" },
+                    "from_agent": { "type": "string" }
+                }
+            }
+        },
+        {
+            "name": "ask_message",
+            "description": "Send a durable Ask message and optionally wait for a correlated Reply.",
+            "inputSchema": {
+                "type": "object",
+                "required": ["to", "subject", "body"],
+                "properties": {
+                    "to": { "type": "string" },
+                    "subject": { "type": "string" },
+                    "body": { "type": "string" },
+                    "timeout_secs": { "type": "integer" },
+                    "from_agent": { "type": "string" }
+                }
+            }
+        },
+        {
+            "name": "reply_message",
+            "description": "Reply to a FerroWire message by MessageId, inheriting its thread and correlation ID.",
+            "inputSchema": {
+                "type": "object",
+                "required": ["message_id", "body"],
+                "properties": {
+                    "message_id": { "type": "string" },
+                    "body": { "type": "string" },
+                    "subject": { "type": "string" },
+                    "from_agent": { "type": "string" }
+                }
+            }
+        },
+        {
+            "name": "get_messages",
+            "description": "Read FerroWire inbox messages for the caller or specified agent.",
+            "inputSchema": {
+                "type": "object",
+                "properties": {
+                    "unread_only": { "type": "boolean", "default": true },
+                    "ack": { "type": "boolean", "default": false },
+                    "agent": { "type": "string" },
+                    "thread_id": { "type": "string" }
+                }
+            }
+        },
+        {
+            "name": "ack_message",
+            "description": "Acknowledge a FerroWire message by MessageId.",
+            "inputSchema": {
+                "type": "object",
+                "required": ["message_id"],
+                "properties": {
+                    "message_id": { "type": "string" },
+                    "agent": { "type": "string" }
+                }
+            }
         }
     ])
 }
--- a/crates/cli/tests/state_sync.rs
+++ b/crates/cli/tests/state_sync.rs
@@ -916,7 +916,7 @@ fn mcp_stdio_round_trip_and_releases_redb_lock_between_calls() {
         "method": "tools/list",
         "params": {}
     }));
-    assert_eq!(list_resp["result"]["tools"].as_array().unwrap().len(), 8);
+    assert_eq!(list_resp["result"]["tools"].as_array().unwrap().len(), 14);
 
     let reg_resp = rpc(serde_json::json!({
         "jsonrpc": "2.0",
```

- [ ] **Step 2: Create `crates/cli/tests/wire_e2e.rs`**

Write `crates/cli/tests/wire_e2e.rs`:

```rust
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct TestEnv {
    _tmp: TempDir,
    root: PathBuf,
    cfg_dir: PathBuf,
    data_dir: PathBuf,
    work_dir: PathBuf,
    routes_file: PathBuf,
    sock_file: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let cfg_dir = root.join("config");
        let data_dir = root.join("data");
        let work_dir = root.join("work");
        let routes_file = root.join("routes.json");
        let sock_file = root.join("wire.sock");

        std::fs::create_dir_all(cfg_dir.join("fl")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(work_dir.join(".agents")).unwrap();
        std::fs::write(work_dir.join(".agents/REGISTRY.md"), "# placeholder\n").unwrap();

        let store_path = data_dir.join("fl/fl.redb");
        std::fs::write(
            cfg_dir.join("fl/config.toml"),
            format!(
                "[state]\nstore = \"{}\"\n\n[wire]\nsocket = \"{}\"\nretention_days = 30\nsync_wire = true\n",
                store_path.display(),
                sock_file.display()
            ),
        )
        .unwrap();

        Self {
            _tmp: tmp,
            root,
            cfg_dir,
            data_dir,
            work_dir,
            routes_file,
            sock_file,
        }
    }

    fn cmd_for(&self, bin_path: &std::path::Path) -> Command {
        let mut c = Command::new(bin_path);
        c.current_dir(&self.work_dir)
            .env("XDG_CONFIG_HOME", &self.cfg_dir)
            .env("XDG_DATA_HOME", &self.data_dir)
            .env("FL_WIRE_ROUTES", &self.routes_file)
            .env("FL_WIRE_SOCKET", &self.sock_file)
            .env_remove("FL_DB")
            .env_remove("AGENT_PERSONA")
            .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID");
        c
    }

    fn fl(&self) -> Command {
        self.cmd_for(std::path::Path::new(env!("CARGO_BIN_EXE_fl")))
    }

    fn symlink_bin(&self, name: &str) -> PathBuf {
        let link = self.root.join(name);
        if !link.exists() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_fl"), &link).unwrap();
        }
        link
    }
}

fn run_json(mut cmd: Command) -> Value {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "command failed ({:?}):\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn fl_wire_cli_send_broadcast_ask_reply_read_ack_wait_status_and_prune() {
    let env = TestEnv::new();

    // Register three agents: ferris (ferroloop lead), kayla (kaggle-gemma4 lead), kacey (kaggle-gemma4 scribe)
    let agent_msg_bin = env.symlink_bin("agent-msg");
    for (name, proj, role) in [
        ("Ferris", "ferroloop", "lead"),
        ("Kayla", "kaggle-gemma4", "lead"),
        ("Kacey", "kaggle-gemma4", "scribe"),
    ] {
        let out = env
            .cmd_for(&agent_msg_bin)
            .args(["register", name, "--project", proj, "--role", role])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "register {name} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Verify .agents/REGISTRY.md was updated automatically by agent-msg register
    let reg_md = std::fs::read_to_string(env.work_dir.join(".agents/REGISTRY.md")).unwrap();
    assert!(reg_md.contains("Ferris") && reg_md.contains("custom:scribe"));

    // Verify collision returns exit code 2 without --replace
    let dup = env
        .cmd_for(&agent_msg_bin)
        .args([
            "register",
            "Ferris",
            "--project",
            "ferroloop",
            "--role",
            "lead",
        ])
        .output()
        .unwrap();
    assert_eq!(dup.status.code(), Some(2));

    // Direct send from ferris to kayla
    let mut send_cmd = env.fl();
    send_cmd.args([
        "wire",
        "send",
        "kayla",
        "Spec review",
        "Please check section 3.",
        "--from",
        "ferris",
        "--json",
    ]);
    let sent = run_json(send_cmd);
    let msg_id = sent["message"]["id"].as_str().unwrap().to_string();
    assert_eq!(sent["receipts"].as_array().unwrap().len(), 1);
    assert_eq!(sent["receipts"][0]["recipient_name"], "kayla");

    // Area+role selector send: @area:kaggle-gemma4/scribe -> only kacey
    let mut role_cmd = env.fl();
    role_cmd.args([
        "wire",
        "send",
        "@area:kaggle-gemma4/scribe",
        "Log update",
        "Update experiment table.",
        "--from",
        "ferris",
        "--json",
    ]);
    let role_sent = run_json(role_cmd);
    assert_eq!(role_sent["receipts"].as_array().unwrap().len(), 1);
    assert_eq!(role_sent["receipts"][0]["recipient_name"], "kacey");

    // Broadcast from ferris -> reaches kayla and kacey, not ferris
    let mut bcast_cmd = env.fl();
    bcast_cmd.args([
        "wire",
        "broadcast",
        "Maintenance window",
        "Rebooting in 10m.",
        "--from",
        "ferris",
        "--json",
    ]);
    let bcast = run_json(bcast_cmd);
    assert_eq!(bcast["receipts"].as_array().unwrap().len(), 2);

    // Read kayla's inbox without --ack, then ack explicitly
    let mut read_cmd = env.fl();
    read_cmd.args(["wire", "read", "--for", "kayla", "--json"]);
    let inbox = run_json(read_cmd);
    assert_eq!(inbox.as_array().unwrap().len(), 2);

    let mut ack_cmd = env.fl();
    ack_cmd.args(["wire", "ack", &msg_id, "--for", "kayla", "--json"]);
    let acked = run_json(ack_cmd);
    assert_eq!(acked[0]["status"], "acked");

    // Ask + Reply correlation
    let mut ask_cmd = env.fl();
    ask_cmd.args([
        "wire",
        "ask",
        "kayla",
        "GPU free?",
        "Need 1 GPU for 5m.",
        "--from",
        "ferris",
        "--json",
    ]);
    let ask_sent = run_json(ask_cmd);
    let ask_id = ask_sent["id"].as_str().or_else(|| ask_sent["message"]["id"].as_str()).unwrap().to_string();

    let mut reply_cmd = env.fl();
    reply_cmd.args([
        "wire",
        "reply",
        &ask_id,
        "Yes, GPU 0 is idle.",
        "--from",
        "kayla",
        "--json",
    ]);
    let reply_sent = run_json(reply_cmd);
    assert_eq!(reply_sent["message"]["correlation_id"], ask_id);
    assert_eq!(reply_sent["message"]["subject"], "Re: GPU free?");

    // Non-blocking wait prints dispatcher status and exits 0 when inbox is empty
    let mut drain_cmd = env.fl();
    drain_cmd.args(["wire", "read", "--ack", "--for", "ferris", "--json"]);
    let _ = run_json(drain_cmd);

    let wait_out = env
        .fl()
        .args(["wire", "wait", "--for", "ferris"])
        .output()
        .unwrap();
    assert!(wait_out.status.success());
    assert!(
        String::from_utf8_lossy(&wait_out.stdout)
            .contains("Central FerroWire dispatcher is active for 'ferris'")
    );

    // Status and prune
    let mut status_cmd = env.fl();
    status_cmd.args(["wire", "status", "--json"]);
    let st = run_json(status_cmd);
    assert_eq!(st["daemon"], "inactive");

    let mut prune_cmd = env.fl();
    prune_cmd.args(["wire", "prune", "--older-than-days", "30", "--json"]);
    let pruned = run_json(prune_cmd);
    assert_eq!(pruned["pruned"], 0);

    // Unregister via agent-msg
    let unreg = env
        .cmd_for(&agent_msg_bin)
        .args(["unregister", "kacey"])
        .output()
        .unwrap();
    assert!(unreg.status.success());
}

#[test]
fn claude_and_codex_hook_delivers_and_acks_unread_messages() {
    let env = TestEnv::new();
    let hook_bin = env.symlink_bin("agent-bus-claude-hook");

    // Send a message to jarvis
    let mut send_cmd = env.fl();
    send_cmd.args([
        "wire",
        "send",
        "jarvis",
        "Hook alert",
        "Check systemd unit.",
        "--from",
        "ferris",
    ]);
    assert!(send_cmd.status().unwrap().success());

    // Invoke Stop hook with AGENT_PERSONA=jarvis
    let mut child = env
        .cmd_for(&hook_bin)
        .env("AGENT_PERSONA", "jarvis")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            serde_json::to_vec(&json!({
                "hook_event_name": "Stop",
                "session_id": "sess-123",
                "cwd": env.work_dir,
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let resp: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(resp["decision"], "block");
    assert!(resp["reason"].as_str().unwrap().contains("Hook alert"));

    // Second Stop hook call sees empty inbox and prints {}
    let mut child2 = env
        .cmd_for(&hook_bin)
        .env("AGENT_PERSONA", "jarvis")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child2
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"hook_event_name\":\"Stop\"}")
        .unwrap();
    let out2 = child2.wait_with_output().unwrap();
    let resp2: Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(resp2, json!({}));

    // Send another message and test SessionStart with a .codex transcript path
    let mut send_cmd2 = env.fl();
    send_cmd2.args([
        "wire",
        "send",
        "jarvis",
        "Codex startup",
        "Welcome back.",
        "--from",
        "ferris",
    ]);
    assert!(send_cmd2.status().unwrap().success());

    let codex_transcript = env.root.join(".codex/sessions/t-99.jsonl");
    std::fs::create_dir_all(codex_transcript.parent().unwrap()).unwrap();
    std::fs::write(&codex_transcript, "take the name of JARVIS\n").unwrap();

    let mut child3 = env
        .cmd_for(&hook_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child3
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            serde_json::to_vec(&json!({
                "hook_event_name": "SessionStart",
                "thread_id": "thread-codex-99",
                "transcript_path": codex_transcript,
                "cwd": env.work_dir,
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
    let out3 = child3.wait_with_output().unwrap();
    let resp3: Value = serde_json::from_slice(&out3.stdout).unwrap();
    assert_eq!(
        resp3["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    assert!(
        resp3["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Codex startup")
    );
}

#[test]
fn mcp_server_exposes_and_executes_all_ferrowire_tools() {
    let env = TestEnv::new();
    let mcp_bin = env.symlink_bin("agent-bus-mcp");

    let mut child = env
        .cmd_for(&mcp_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let mut call = |req: Value| -> Value {
        let line = format!("{}\n", serde_json::to_string(&req).unwrap());
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).unwrap();
        serde_json::from_str(&resp_line).unwrap()
    };

    // 1. tools/list
    let list_resp = call(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    }));
    let tools = list_resp["result"]["tools"].as_array().unwrap();
    for expected in [
        "send_message",
        "broadcast_message",
        "ask_message",
        "reply_message",
        "get_messages",
        "ack_message",
    ] {
        assert!(
            tools.iter().any(|t| t["name"] == expected),
            "missing MCP tool {expected}"
        );
    }

    // 2. send_message
    let send_resp = call(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "send_message",
            "arguments": {
                "to": "sonya",
                "subject": "Vocoder checkpoint",
                "body": "Step 50k ready.",
                "from_agent": "ferris"
            }
        }
    }));
    assert_eq!(send_resp["result"]["isError"], false);

    // 3. get_messages for sonya
    let get_resp = call(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "get_messages",
            "arguments": {
                "agent": "sonya",
                "unread_only": true,
                "ack": false
            }
        }
    }));
    let text = get_resp["result"]["content"][0]["text"].as_str().unwrap();
    let msgs: Vec<Value> = serde_json::from_str(text).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["recipient"], "sonya");
    assert_eq!(msgs[0]["to"], "sonya");
    let mid = msgs[0]["id"].as_str().unwrap().to_string();

    // 4. ack_message
    let ack_resp = call(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "ack_message",
            "arguments": {
                "message_id": mid,
                "agent": "sonya"
            }
        }
    }));
    assert_eq!(ack_resp["result"]["isError"], false);

    drop(stdin);
    let _ = child.wait();
}
```

- [ ] **Step 3: Run full workspace clippy and test suite**

Run:
```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --offline
```
Expected: PASS (`0` clippy warnings, all unit and integration tests across `fl-core`, `fl-store`, `fl-exec`, `fl-github`, `fl-mcp`, `fl-wire`, and `fl-cli` pass).
