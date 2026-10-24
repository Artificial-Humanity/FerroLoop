# FerroWire (agent messaging and cross-boundary carriers) — design

**Date:** 2026-10-24
**Status:** Drafted from the owner's 2026-10-24 brainstorming decisions; awaiting owner spec
review. Sequenced second in the post-`Prove` build order:
1. **State Database and Agent Registry** (shipped in PR #43, `6ea814e`, `FORMAT_WITH_STATE = 6`)
2. **FerroWire (Messaging and Carriers)** (this specification, `FORMAT_WITH_WIRE = 7`)
3. **Adapters and Telemetry**
4. **Workflow Graph (Flow)**
5. **Hub and Command-Center PWA**

**Scope:** A built-in, zero-external-broker messaging and push-delivery system (`fl wire`) for
agent-to-agent and human-to-agent communication across local machine, LAN, Tailscale, and
internet boundaries. Persisted locally in `fl-store` (`FORMAT_WITH_WIRE = 7`), delivered in
sub-millisecond time locally over a Unix domain socket with automatic push into idle Claude Code,
Antigravity, and Codex sessions, bridged across hosts over authenticated WebSocket/TLS (`ws://`
and `wss://`), backed by standard-git store-and-forward (`refs/heads/fl/state`) when offline,
and exposed through `fl wire`, a drop-in `agent-msg` / `agent-bus-claude-hook` compatibility
surface, and the stdio MCP server.

---

## Reading this document

Constraints carry a status tag, as in the identity and state-database specifications:

| tag | meaning |
|---|---|
| **Invariant** | Must hold in any version. Violating it breaks the product's premise. |
| **Release scope** | True of this sub-project's release. A later version can change it. |
| **Open** | Undecided. Named here so it is not decided by accident. |

An untagged statement is descriptive, not binding. `[agent]` marks a ruling the resident
agent made where the owner's decisions left a choice; the owner may reverse any of them.

---

## 0. Where this sits

On `ai-lab-0` today, inter-agent messaging runs through an out-of-tree Python package
(`AI-Lab-AMD/scripts/agent-bus/`) backed by a local `nats-server` JetStream instance
(`nats://127.0.0.1:4222`), a SQLite roster (`~/.cache/agent-bus/registry.db`), and a host
systemd service (`agent-bus-dispatcher.service`). That prototype proved three vital mechanics:

1. **Sub-second event-driven wakeup paired with durable at-least-once queues.**
2. **Direct push into idle vendor sessions** (Claude Code Unix sockets, Antigravity
   `agy agentapi send-message`, and Codex `codex queue`) so agents do not burn tokens or leave
   fragile background `wait` loops running.
3. **Explicit delivery receipts (`ack`) and non-echo broadcasts.**

However, keeping messaging in an external Python + NATS + SQLite stack has three structural
limits:
* **External broker dependency:** every new workstation, container, or remote node must install,
  configure, and supervise `nats-server` and a Python virtualenv alongside `fl`.
* **Split identity and storage:** Sub-project 1 (`6ea814e`) moved the authoritative
  `AgentRegistry` and `StateStore` into `fl-store` (`FORMAT_WITH_STATE = 6`) and synced them on
  `refs/heads/fl/state`, leaving the Python `agent-bus` on a separate SQLite database and NATS
  filestore.
* **Single-host boundary:** `nats://127.0.0.1:4222` is loopback-only, with no built-in
  authenticated WAN/LAN relay or offline git store-and-forward fallback.

Sub-project 2 brings messaging natively into FerroLoop as **FerroWire**: one `fl` binary owns
the durable mailbox (`RedbStore` Format 7), the local Unix-socket bus, the cross-vendor idle
session push dispatcher, the cross-boundary WebSocket/TLS peer relay, and the git
store-and-forward fallback on `refs/heads/fl/state`.

### 0.1 Owner decisions this design rests on (2026-10-24)

1. **Cross-boundary performance parity:** FerroWire must match the sub-second push performance
   of the current local messaging system across machine, LAN, and internet boundaries.
2. **Built-in Rust transport (`Option A`):** `fl` ships its own lightweight Rust transport
   (local Unix socket + outbound/inbound WebSocket/TLS peer and hub links + local `RedbStore`
   persistence + native vendor push dispatcher) with zero external broker dependencies.
3. **Location-transparent persona and group addressing (`Option A`):** messages address an
   agent by case-insensitive persona name (`ferris`), immutable `AgentId` (`urn:uuid:...`),
   global broadcast (`@all`), or live `AgentRegistry` selectors (`@area:<slug>`,
   `@role:<role>`, and `@area:<slug>/<role>`).
4. **Tiered durability and selective git sync (`Option A`):** every message and receipt writes
   immediately to local `RedbStore` and pushes in real time over the live wire; unacknowledged
   mail and durable messages (`Ask`, `Handoff`, or `--durable`) also sync through git
   (`refs/heads/fl/state`) for offline store-and-forward and auditability, while acknowledged
   transient chatter ages out after a configurable retention window.
5. **Pre-shared bearer token (PSK) + TLS/Tailscale (`Option A`):** cross-machine WebSocket
   connections authenticate peers using a shared secret token (HMAC-SHA256 handshake verifying
   the token and workspace identity) over `wss://` across the internet and `ws://` or `wss://`
   on trusted LAN/Tailscale links.
6. **Full message envelope + drop-in `agent-msg` compatibility (`Option A`):** FerroWire
   supports `send`, `broadcast`, `ask`, `reply`, `read`, `ack`, `wait`, `prune`, `daemon`, and
   `hook claude`, alongside a drop-in `agent-msg` / `agent-bus-mcp` / `agent-bus-claude-hook`
   compatibility shim so existing agent sessions, hooks, and instruction files cut over without
   breakage.
7. **Dedicated `crates/wire` crate + `FORMAT_WITH_WIRE = 7` (`Approach 1`):** pure message types
   and `MessageStore` live in `fl-core`, `RedbStore` Format 7 tables live in `fl-store`, git
   store-and-forward lives in `fl-exec`, async Unix-socket/WebSocket networking and vendor push
   dispatch live in a new `crates/wire` (`fl-wire`) crate, and CLI/MCP surfaces live in
   `fl-cli`.

---

## 1. Domain model and `MessageStore` role (`crates/core`)

**Invariant.** `fl-core` stays pure: no I/O, no network, no system clock, no async runtime. All
FerroWire domain types live in `crates/core/src/ferrowire.rs` (`pub mod ferrowire;`), and the
store role trait `MessageStore` lives in `crates/core/src/store.rs`.

### 1.1 Identifiers (`Kind::Message` and `MessageId`)

**Invariant.** `Kind` in `crates/core/src/ids.rs` gains a seventh variant:

```rust
pub enum Kind {
    Project,
    Gate,
    Record,
    Finding,
    State,
    Agent,
    Message,
}
```

with wire name `"message"` and `id_type!(MessageId, Kind::Message);`.

* **Routing in `KindRouted::owner` (`crates/core/src/store.rs`):** The exhaustive match in
  `KindRouted::owner` routes `Kind::Message => self.catalog` alongside `Project`, `Gate`,
  `State`, and `Agent`.
* **`MessageId(pub Iri)`:** An immutable `urn:uuid:<v7>` IRI (or `seq_iri` in `MemStore`).
  Because UUIDv7 encodes the Unix millisecond timestamp in its high 48 bits, `MessageId`
  strings sort in chronological creation order when compared lexicographically.
* **No numeric handle consumption (`[agent]`):** Like `RevisionId` and ledger entries,
  `MessageId` is registered in `IDS` as `Kind::Message` so `locate` and ownership lookups know
  its kind, but it does **not** allocate a sequential numeric handle in `HANDLES` / `HANDLE_OF`
  (message volume is much higher than projects or gates, and messages are referenced by
  `MessageId` URN).
* **`ThreadId`:** `MessageId` is reused as the thread root identifier (`pub type ThreadId = MessageId;`).
  For a root message (`reply_to == None` and no explicit `--thread`), `thread_id == id`.

### 1.2 Recipient addressing (`WireAddress`)

**Invariant.** A message's `to` field is a typed `WireAddress` that serializes to and parses
from a canonical wire string:

```rust
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
```

#### Parsing and validation (`WireAddress::parse`)
1. `"@all"` (or legacy `"all"` in `agent-msg` compatibility mode; normalized on wire to
   `"@all"`) $\rightarrow$ `WireAddress::All`.
2. `"urn:fl:agent:<rest>"` $\rightarrow$ validate and lowercase-normalize `<rest>` with
   `validate_slug` (`crates/core/src/state.rs`) and return `WireAddress::AgentName(name_key)`.
3. `"urn:uuid:..."` $\rightarrow$ parsed as `Iri` and wrapped in `WireAddress::AgentId(AgentId(iri))`.
4. `"@area:<rest>"`:
   * If `<rest>` contains `'/'`, split once into `(<area>, <role>)`, validate `<area>` with
     `validate_slug`, parse `<role>` with `AgentRole::from_wire` (falling back to
     `validate_slug(role).map(AgentRole::Custom)` if not a built-in role), and return
     `WireAddress::AreaRole { area, role }`.
   * Otherwise validate `<rest>` with `validate_slug` and return `WireAddress::Area(area)`.
5. `"@role:<role>"` $\rightarrow$ parse `<role>` with `AgentRole::from_wire` (or
   `validate_slug(role).map(AgentRole::Custom)`) and return `WireAddress::Role(role)`.
6. Any other `@`-prefixed string is refused with `StateError::InvalidAddress`.
7. Any non-`@` string is validated and lowercase-normalized with `validate_slug(raw)` (from
   `crates/core/src/state.rs`, returning ASCII-lowercase `name_key`) $\rightarrow$
   `WireAddress::AgentName(name_key)`.

#### Recipient resolution against `AgentRegistry`
`AgentRecord` (`crates/core/src/agent.rs`) gains a helper method:
```rust
impl AgentRecord {
    pub fn covers_area(&self, slug: &str) -> bool {
        self.project == slug || self.extra_areas.iter().any(|a| a == slug)
    }
}
```
When a message is sent or imported:
* `WireAddress::AgentName(name_key)`: looks up `agent_by_name(&name_key)`.
  * If an `Active` agent exists in the registry, resolves to `vec![(agent.id.0, agent.name_key)]`.
  * **Unregistered persona tolerance (`[agent]`):** If no registered `AgentRecord` exists yet
    for `name_key` (for example, an agent before its first `fl agent register` or in a minimal
    test environment), `MessageStore` synthesizes a deterministic fallback recipient IRI
    `urn:fl:agent:<name_key>` (`Iri::parse(&format!("urn:fl:agent:{name_key}"))`, where
    `name_key` is already guaranteed ASCII lowercase by `validate_slug`) so direct persona
    messaging never drops mail on a cold roster.
* `WireAddress::AgentId(id)`: looks up `get_agent(&id)`. If found, uses `(agent.id.0, agent.name_key)`;
  otherwise uses `(id.0.clone(), id.as_str().to_owned())`.
* `WireAddress::All`: selects all `AgentRecord`s with `status == AgentStatus::Active` whose
  `Some(&agent.id) != sender_id.as_ref()` and `agent.name_key != sender_name`.
* `WireAddress::Area(slug)`: selects all `Active` agents where `agent.covers_area(&slug)`
  (excluding the sender).
* `WireAddress::Role(role)`: selects all `Active` agents where `agent.role == role` (excluding
  the sender).
* `WireAddress::AreaRole { area, role }`: selects all `Active` agents where
  `agent.covers_area(&area) && agent.role == role` (excluding the sender).

#### Candidate recipient matching for `list_wire_inbox` and `update_wire_receipt`
Because a recipient may have received messages under `urn:fl:agent:<name_key>` before
registering, or under an unpublished local `AgentId` that was later aliased during
`import_registry_events` (`alias_unpublished_id_in` in `crates/store/src/state.rs`), both
`list_wire_inbox` and `update_wire_receipt` resolve a caller's `recipient` filter (`name_key` or
`AgentId` IRI) to a candidate set matching any of:
1. `receipt.recipient_id == resolved_agent_id` (after following any `ALIASES` entry in `RedbStore`),
2. `receipt.recipient_id.as_str() == format!("urn:fl:agent:{name_key}")`, or
3. `receipt.recipient_name == name_key`.

### 1.3 Message envelope (`WireMessage`) and delivery receipt (`DeliveryReceipt`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Mail,
    Ask,
    Reply,
    Handoff,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageUrgency {
    Normal,
    Urgent,
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
```

**Invariants on `WireMessage`:**
1. `subject` is trimmed, non-empty, single-line (`!subject.contains('\n') && !subject.contains('\r')`),
   and at most `512` UTF-8 bytes.
2. `body` is non-empty and at most `262_144` UTF-8 bytes (`256 KiB`).
3. `sender_name` is lowercase-normalized (`validate_slug` or `"system"` / `"owner"`
   / `"unknown"`).
4. For `MessageKind::Ask` and `MessageKind::Handoff`, `durable` is always forced to `true`.
5. For `MessageKind::Reply`, `reply_to` and `correlation_id` must both be `Some(...)`.

#### Per-recipient receipt (`DeliveryReceipt`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending = 0,
    Delivered = 1,
    Acked = 2,
    Expired = 3,
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
```

**Invariant (Monotonic Receipt Merge).** `DeliveryReceipt::merge(&self, incoming: &DeliveryReceipt) -> DeliveryReceipt`
combines two receipts for the same `(recipient_id, message_id)` deterministically and
commutatively:
* `status`: higher rank wins (`Pending < Delivered < Acked`, and `Pending < Delivered < Expired`;
  if one is `Acked` and the other is `Expired`, `Acked` wins).
* `attempts`: `self.attempts.max(incoming.attempts)`.
* `updated_at`: `self.updated_at.max(incoming.updated_at)`.
* `acked_at`: earliest `Some(at)` if status is `Acked`, else `None`.

### 1.4 The `MessageStore` trait (`crates/core/src/store.rs`)

```rust
#[derive(Debug, Clone)]
pub struct SendWireArgs {
    /// Optional caller-supplied `MessageId` (`urn:uuid:...`); minted by the store if `None`.
    pub id: Option<MessageId>,
    pub sender_id: Option<AgentId>,
    pub sender_name: String,
    pub sender_host: Option<String>,
    pub to: WireAddress,
    pub kind: MessageKind,
    pub urgency: MessageUrgency,
    pub durable: bool,
    pub subject: String,
    pub body: String,
    pub reply_to: Option<MessageId>,
    pub thread_id: Option<ThreadId>,
    pub deadline_at: Option<At>,
    pub at: At,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireInboxItem {
    pub message: WireMessage,
    pub receipt: DeliveryReceipt,
}

#[derive(Debug, Clone, Default)]
pub struct WireFilter {
    pub recipient: Option<String>,
    pub unread_only: bool,
    pub thread_id: Option<ThreadId>,
    pub correlation_id: Option<MessageId>,
    pub limit: Option<usize>,
}

pub trait MessageStore {
    /// Record a new message and create initial `Pending` receipts for all resolved recipients.
    fn send_wire(
        &self,
        args: SendWireArgs,
    ) -> Result<(WireMessage, Vec<DeliveryReceipt>), StoreError>;

    /// Look up a single message by `MessageId`.
    fn get_wire_message(&self, id: &MessageId) -> Result<Option<WireMessage>, StoreError>;

    /// Query inbox items for a recipient (`name_key` or `AgentId` URN), ordered by `MessageId`.
    fn list_wire_inbox(&self, filter: &WireFilter) -> Result<Vec<WireInboxItem>, StoreError>;

    /// List all messages in a thread (`thread_id`) in chronological `MessageId` order.
    fn list_wire_thread(&self, thread_id: &ThreadId) -> Result<Vec<WireMessage>, StoreError>;

    /// List replies correlated to an `Ask` message (`correlation_id`).
    fn list_wire_replies(&self, correlation_id: &MessageId) -> Result<Vec<WireMessage>, StoreError>;

    /// List all receipts for a given `MessageId`.
    fn list_wire_receipts(
        &self,
        message_id: &MessageId,
    ) -> Result<Vec<DeliveryReceipt>, StoreError>;

    /// Transition a recipient's receipt (`Delivered`, `Acked`, or failed attempt increment).
    fn update_wire_receipt(
        &self,
        message_id: &MessageId,
        recipient: &str,
        status: DeliveryStatus,
        increment_attempts: bool,
        at: At,
    ) -> Result<Option<DeliveryReceipt>, StoreError>;

    /// Export messages after `after_seq` for peer catch-up and `fl/state` sync.
    fn wire_messages_after(
        &self,
        after_seq: u64,
    ) -> Result<Vec<(u64, WireMessage)>, StoreError>;

    /// Export receipt events after `after_seq` for peer catch-up and `fl/state` sync.
    fn wire_receipts_after(
        &self,
        after_seq: u64,
    ) -> Result<Vec<(u64, DeliveryReceipt)>, StoreError>;

    /// Import messages and receipts from a remote peer or `fl/state` branch, enforcing
    /// byte-for-byte immutability on `WireMessage` and monotonic convergence on `DeliveryReceipt`.
    fn import_wire_batch(
        &self,
        messages: &[WireMessage],
        receipts: &[DeliveryReceipt],
    ) -> Result<(), StoreError>;

    /// Delete transient (`durable == false`) messages whose receipts are all `Acked` or
    /// `Expired` and whose `created_at < before`. Returns the count of pruned messages.
    fn prune_wire(&self, before: At) -> Result<usize, StoreError>;
}
```

Also extend `StateSyncCursor` in `crates/core/src/state.rs` with two backward-compatible
`#[serde(default)]` fields matching the existing `last_revision_seq` / `last_registry_seq`
naming convention:
```rust
#[serde(default)]
pub last_wire_msg_seq: u64,
#[serde(default)]
pub last_wire_rcpt_seq: u64,
```

---

## 2. Storage layout (`FORMAT_WITH_WIRE = 7` in `crates/store`)

### 2.1 Format version 7 rules

**Invariant.** `crates/store/src/lib.rs` defines:

```rust
pub const FORMAT_WITH_WIRE: u64 = 7;
```

* `RedbStore::open` accepts store formats `FORMAT_VERSION..=FORMAT_WITH_WIRE` (`2..=7`).
* An existing store at format 2..6 stays at its current format on read-only operations and
  raises to `FORMAT_WITH_WIRE` (`7`) in the same write transaction on the first `MessageStore`
  write (`raise_format(&tx, FORMAT_WITH_WIRE)`).
* An older `fl` binary (compiled with max format 6) refuses a store that has been raised to 7
  with `StoreError::FormatVersion`, preventing an older binary from ignoring FerroWire tables.

### 2.2 `redb` tables (`crates/store/src/ferrowire.rs`)

| Table constant | Table name | Key type | Value type | Purpose |
|---|---|---|---|---|
| `WIRE_MESSAGES` | `"wire_messages"` | `&str` (`MessageId`) | `&str` (JSON `WireMessage`) | Primary message store |
| `WIRE_MSG_LOG` | `"wire_msg_log"` | `u64` (`seq`) | `&str` (`MessageId`) | Append-only sequence log of messages |
| `WIRE_MSG_SEQ` | `"wire_msg_seq"` | `&str` (`MessageId`) | `u64` (`seq`) | Reverse index for deduplicating log entries |
| `WIRE_INBOX` | `"wire_inbox"` | `(&str, &str)` (`(recipient_id, message_id)`) | `&str` (JSON `DeliveryReceipt`) | Per-recipient inbox and receipt state |
| `WIRE_RCPT_LOG` | `"wire_rcpt_log"` | `u64` (`seq`) | `&str` (JSON `DeliveryReceipt`) | Append-only sequence log of receipt updates |
| `WIRE_THREADS` | `"wire_threads"` | `(&str, &str)` (`(thread_id, message_id)`) | `()` | Chronological thread index |
| `WIRE_REPLIES` | `"wire_replies"` | `(&str, &str)` (`(correlation_id, message_id)`) | `()` | Correlated reply index for `Ask` messages |

`META` holds two new sequence counters:
* `"next_wire_msg_seq"` (`u64`)
* `"next_wire_rcpt_seq"` (`u64`)

### 2.3 `RedbStore::verify_wire_invariants` verification

`RedbStore` provides `pub fn verify_wire_invariants(&self) -> Result<(), StoreError>` (distinct
from the private single-IRI `fn check(&self, id: &Iri)` helper in `crates/store/src/lib.rs`):
1. Every `Kind::Message` entry in `IDS` must exist in `WIRE_MESSAGES`.
2. Every row in `WIRE_MESSAGES` must deserialize cleanly as a `WireMessage` whose `id` matches
   its table key and has a `Kind::Message` entry in `IDS`.
3. Every row in `WIRE_INBOX` must deserialize cleanly as a `DeliveryReceipt` whose
   `(recipient_id, message_id)` matches its key and whose `message_id` exists in `WIRE_MESSAGES`.
4. Every `(thread_id, message_id)` in `WIRE_THREADS` and `(correlation_id, message_id)` in
   `WIRE_REPLIES` must reference an existing `message_id` in `WIRE_MESSAGES`.

---

## 3. Local daemon, vendor push dispatcher, and cross-boundary relay (`crates/wire`)

A new workspace crate `crates/wire` (`package.name = "fl-wire"`) depends on `fl-core`,
`fl-store`, `tokio` (`net`, `io-util`, `sync`, `time`, `process`, `signal`),
`tokio-tungstenite` (with `rustls-tls-webpki-roots`), `ring`, `base64`, `serde`, and
`serde_json`.

### 3.1 Identity and route resolution (`crates/wire/src/identity.rs`)

#### Sender and caller persona resolution (`resolve_persona`)
Resolved in strict priority order:
1. Explicit CLI flag (`--from <name>` or `--for <name>`) or MCP tool argument (`from_agent` /
   `agent`).
2. `AGENT_PERSONA` or `AGENT_NAME` environment variable.
3. Vendor session and route-cache inspection (matching `AI-Lab-AMD/scripts/agent-bus/agent_bus/message.py`):
   * If `transcript_path` is provided, scans the first 50 JSONL lines for `"agentName"` or
     `"customTitle"`.
   * Inspects `CLAUDE_CODE_MESSAGING_SOCKET` (`/run/user/<uid>/cc-socks/<pid>.sock`) and walks
     `/proc/<pid>/stat` parent PIDs looking for `~/.claude/sessions/<pid>.json` `"name"` (ignoring
     auto-generated default names where `name.to_ascii_lowercase().starts_with("artificial-humanity-")`).
   * Along the `/proc/<pid>/cmdline` parent chain, checks for `--session-id <id>` and scans
     `~/.claude/projects/**/*.jsonl` (first 30 lines) for a matching `"sessionId"` with
     `"agentName"` or `"customTitle"`.
   * Checks `$XDG_RUNTIME_DIR/fl-wire-routes.json`, `~/.cache/fl/wire-routes.json`, and legacy
     `~/.cache/agent-bus/routes.json` for a route matching `CLAUDE_SESSION_ID`,
     `ANTIGRAVITY_CONVERSATION_ID`, or `CODEX_THREAD_ID`.
   * Walks ancestor directories from `cwd` looking for a registered project lead in
     `AgentRegistry` (or legacy `PERSONA.md`).
4. Falls back to `"unknown"`.

#### Live route table (`RouteEntry`) and passive route discovery
`fl-wire` maintains a host-local route cache at `$XDG_RUNTIME_DIR/fl-wire-routes.json` (falling
back to `~/.cache/fl/wire-routes.json`, and also reading legacy
`~/.cache/agent-bus/routes.json` so sessions registered before cutover remain reachable):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VendorRoute {
    Claude {
        pid: u32,
        session_id: String,
        socket_path: PathBuf,
        key_path: Option<PathBuf>,
    },
    Agy {
        conversation_id: String,
        ls_pid: Option<u32>,
        ls_address: Option<String>,
        csrf_token: Option<String>,
        agentapi_exe: Option<PathBuf>,
    },
    Codex {
        thread_id: String,
        cwd: Option<PathBuf>,
    },
    /// Used by deterministic integration tests to capture pushed deliveries without vendor binaries.
    Webhook {
        socket_path: PathBuf,
    },
}
```

In addition to explicit route registrations from `fl wire` / `agent-msg` CLI calls, the daemon
performs **passive route discovery** before dispatching to a persona (matching
`AI-Lab-AMD/scripts/agent-bus/agent_bus/dispatcher.py:101-294`):
1. `discover_claude_routes`: scans `~/.claude/sessions/*.json` for live PIDs whose `"name"` is
   non-empty and does not start with `"artificial-humanity-"`, extracting `messagingSocketPath`
   and resolving `~/.claude/sessions/<pid>.<sha256(canonical_socket_path)>.key` (falling back to
   `~/.claude/sessions/<pid>.*.key`).
2. `discover_agy_routes`: scans `~/.gemini/antigravity-cli/brain/*/.system_generated/logs/transcript.jsonl`
   for active conversations and pairs them with `ANTIGRAVITY_LS_ADDRESS`,
   `ANTIGRAVITY_CSRF_TOKEN`, and `ANTIGRAVITY_AGENTAPI_EXE` discovered from `/proc/*/environ`
   (or `~/.gemini/antigravity-cli/cli.log` matching
   `Language server listening on random port at (\d+) for HTTP`, sharing the discovered
   host-level `ls_address` and `csrf_token` across active Antigravity routes).
3. `discover_codex_routes`: scans `~/.codex/session_index.jsonl` for active Codex threads.

### 3.2 Native vendor push dispatcher (`crates/wire/src/dispatcher.rs`)

When a message arrives for a recipient on the local host (either from a local `send_wire` or
over WebSocket from a remote peer), the dispatcher attempts immediate push delivery if a live
`VendorRoute` exists (or is passively discovered) for that persona:

1. **Claude Code (`VendorRoute::Claude`):**
   * Verifies `/proc/<pid>` is alive and `socket_path` is a Unix domain socket.
   * Reads the JSON key file (`key_path` or `~/.claude/sessions/<pid>.*.key`) and extracts the
     `"peerToken"` field.
   * Connects to `socket_path` over a Unix stream, sends the auth frame:
     ```json
     {"type":"auth","token":"<peerToken>"}
     ```
     followed by `\n`, and writes the exact Claude Code user message frame:
     ```json
     {
       "type": "user",
       "from": "<sender_name>",
       "priority": "next",
       "message": {
         "role": "user",
         "content": "<formatted_message>"
       }
     }
     ```
     followed by `\n`.
2. **Antigravity (`VendorRoute::Agy`):**
   * Resolves `ANTIGRAVITY_LS_ADDRESS`, `ANTIGRAVITY_CSRF_TOKEN`, `ANTIGRAVITY_AGENTAPI_EXE`
     (defaulting to `~/.local/bin/agy`), and `ANTIGRAVITY_CONVERSATION_ID` (from the route,
     shared host discovery, `/proc/*/environ`, or `~/.gemini/antigravity-cli/cli.log`).
   * Executes `<agentapi_exe> agentapi send-message --title=<header> <conversation_id> <body>`
     with all four `ANTIGRAVITY_*` environment variables set.
3. **Codex (`VendorRoute::Codex`):**
   * Executes `codex queue --thread <thread_id> --message <formatted_message>`.
4. **Test / Custom Unix Socket (`VendorRoute::Webhook`):**
   * Connects to `socket_path` and writes a single JSON line containing the `WireMessage`.

**Invariant (Ack only on verified push).**
* If the vendor push succeeds (exit code `0` or socket frame flushed cleanly), the dispatcher
  calls `update_wire_receipt(..., DeliveryStatus::Acked, true, now)` and broadcasts the
  resulting `DeliveryReceipt` to connected peers.
* If the vendor push fails, the dispatcher calls
  `update_wire_receipt(..., DeliveryStatus::Pending, true, now)`. Once `attempts >= 5`
  (`MAX_DELIVER = 5`), automatic push retries stop and the message waits in `Pending` state for
  an explicit `fl wire read --ack` or Claude Stop hook poll.

### 3.3 Local Unix domain socket protocol (`crates/wire/src/daemon.rs`)

`fl wire daemon` binds a local Unix domain socket at the configured `socket_path` (default:
`$XDG_RUNTIME_DIR/fl-wire.sock`, or `~/.cache/fl/wire.sock` if `XDG_RUNTIME_DIR` is unset) with
mode `0600`.

Frames on the Unix socket are newline-delimited JSON (`LocalFrame`):

* **Client $\rightarrow$ Daemon requests (`LocalRequest`):**
  * `Send(SendWireArgs)` $\rightarrow$ stores in `RedbStore`, fans out to WebSocket peers and
    local vendor dispatcher, returns `LocalResponse::Sent { message, receipts }`.
  * `Read(WireFilter, bool /* ack */)` $\rightarrow$ reads from `RedbStore` (marking `Acked` if
    requested and fanning out receipt updates), returns `LocalResponse::Inbox(Vec<WireInboxItem>)`.
  * `Ack { message_ids: Vec<MessageId>, recipient: String }` $\rightarrow$ marks `Acked`, fans
    out receipt updates, returns `LocalResponse::Acked(Vec<DeliveryReceipt>)`.
  * `RegisterRoute { persona: String, route: VendorRoute }` $\rightarrow$ updates live route
    table and triggers immediate dispatch of any pending messages for `persona`.
  * `Subscribe { recipient: Option<String>, correlation_id: Option<MessageId> }` $\rightarrow$
    keeps the Unix stream open and streams `LocalEvent::Message(WireMessage)` or
    `LocalEvent::Receipt(DeliveryReceipt)` frames in real time (used by `fl wire ask --timeout`
    and `fl wire wait --block`).
  * `Status` $\rightarrow$ returns daemon health, connected WebSocket peers, and active routes.

**Direct-store fallback.** Every CLI and MCP command tries `LocalClient::connect(socket_path)`
with a `50 ms` timeout first; if the socket is absent or refused, it opens `RedbStore` directly
and executes the operation against the local database.

### 3.4 Cross-boundary WebSocket/TLS peer protocol (`crates/wire/src/peer.rs`)

Across LAN, Tailscale, and the internet, `fl wire daemon` can both accept incoming WebSocket
connections (`--listen <host:port>`) and maintain persistent outbound connections (`--peer <ws:// or wss://>`).

#### Handshake and PSK authentication
1. When a WebSocket connection opens, both sides generate a 32-byte random nonce and send a
   `PeerFrame::Challenge { protocol_version: 1, node_id, host, nonce }`.
2. Upon receiving `PeerFrame::Challenge`, each side computes:
   $$\text{mac} = \text{HMAC-SHA256}(\text{token}, \text{"fl-wire-v1:"} \parallel \text{remote\_nonce} \parallel \text{local\_nonce} \parallel \text{host})$$
   and sends `PeerFrame::Auth { mac, last_msg_seq, last_rcpt_seq, last_agent_seq }`.
3. Both sides verify `mac` in constant time (`ring::hmac::verify`). If verification fails or no
   `Auth` frame arrives within `5 s`, the peer closes the WebSocket immediately.

#### Cursor catch-up and live streaming
4. After mutual `Auth` succeeds, each peer queries its local `RedbStore` for:
   * `registry_events_after(remote.last_agent_seq)`
   * `wire_messages_after(remote.last_msg_seq)`
   * `wire_receipts_after(remote.last_rcpt_seq)`
   and streams them in a `PeerFrame::SyncBatch { registry_events, messages, receipts }` frame.
5. On receipt of `SyncBatch` or live `PeerFrame::Publish { message, receipts }` /
   `PeerFrame::ReceiptUpdate(DeliveryReceipt)` / `PeerFrame::RegistryUpdate(RegistryEvent)`:
   * The node imports the records into its local `RedbStore` (idempotent by `MessageId` /
     `event_id` and monotonic for `DeliveryReceipt`).
   * If any newly imported message or receipt was not previously known locally, the node:
     1. Relays it to all *other* connected WebSocket peers (hub-and-spoke / mesh fan-out with
        loop prevention via `MessageId` / monotonic receipt check).
     2. Notifies any matching local Unix-socket `Subscribe` waiters (waking `fl wire ask` or
        `fl wire wait` immediately).
     3. Invokes the local vendor push dispatcher if the recipient has a route on this host.
6. **Keepalive and reconnection:** Peers send WebSocket `Ping` frames every `15 s` (`45 s`
   read timeout) and reconnect outbound `--peer` links with exponential backoff (`100 ms` base,
   doubling up to `5 s`, with jitter).

---

## 4. Git store-and-forward (`refs/heads/fl/state`) and retention (`crates/exec`)

### 4.1 Tree layout on `refs/heads/fl/state`

`crates/exec/src/state_sync.rs` extends the existing `refs/heads/fl/state` orphan-branch tree
(`format`, `README`, `areas.jsonl`, `agents.jsonl`, `revisions/`, `registry/`) with two new
directories under `wire/`:

```text
format                                        # "1\n"
README
areas.jsonl
agents.jsonl
revisions/<entry_hash>/<n:04>.jsonl
registry/<n:04>.jsonl
wire/
  messages/<prefix2>/<message_id_filename>.json
  receipts/<n:04>.jsonl
```

where:
* `<message_id_filename>` is `message_id.as_str().replace(':', "_")` (for example,
  `urn_uuid_0195c8...json`), and `<prefix2>` is the last two hex characters of the UUID stem so
  a single directory never accumulates tens of thousands of blobs.
* `wire/messages/<prefix2>/<message_id_filename>.json` contains the canonical sorted-key JSON of
  `WireMessage` followed by `\n`.
* `wire/receipts/<n:04>.jsonl` contains append-only JSONL segments (rolling at
  `MAX_SEGMENT_BYTES = 262_144`, identical to `registry/<n:04>.jsonl`) of `DeliveryReceipt`
  records.

### 4.2 Selective git export policy

* `StateSyncTarget` in `crates/exec/src/state_sync.rs` gains `pub sync_wire: bool`.
* `sync_state` and `verify_state` widen their store trait bound to
  `S: StateStore + AgentRegistry + MessageStore`.
* During `sync_state` (when `target.sync_wire` is `true`):
  1. **Pull phase:** Reads all new or updated `wire/messages/*/*.json` and `wire/receipts/*.jsonl`
     entries between `cursor.last_pushed_commit` and the remote tip, and calls
     `store.import_wire_batch(&imported_messages, &imported_receipts)`.
  2. **Publish phase:** Queries `store.wire_messages_after(cursor.last_wire_msg_seq)`
     and selects a `WireMessage` for git export if and only if:
     * `message.durable == true` (`Ask`, `Handoff`, or `--durable`), **or**
     * at least one `DeliveryReceipt` for `message.id` in `store.list_wire_receipts(&message.id)`
       is still `Pending` or `Delivered` (unacknowledged).
     All `DeliveryReceipt` updates from `store.wire_receipts_after(cursor.last_wire_rcpt_seq)`
     whose `message_id` is exported (or already present on `fl/state`) are appended to
     `wire/receipts/<n:04>.jsonl`.
  3. **Early-return guard:** `sync_state` only skips committing/pushing when
     `unpublished_revs.is_empty() && unpublished_reg_events.is_empty() && (!target.sync_wire || (unpublished_wire_msgs.is_empty() && unpublished_wire_rcpts.is_empty()))`.
  4. `StateSyncReport` gains `imported_wire_messages: usize`, `imported_wire_receipts: usize`,
     `published_wire_messages: usize`, and `published_wire_receipts: usize`.

### 4.3 Tamper verification (`fl state verify`)

`verify_state` and `verify_append_only_step` in `crates/exec/src/state_sync.rs` enforce two
additional invariants across every commit on `refs/heads/fl/state`:
1. **Immutable message blobs:** Once `wire/messages/<p>/<id>.json` is written in a commit, no
   child commit may delete it or change its bytes; any modification raises
   `StateSyncError::HistoryTampered`.
2. **Append-only receipt segments:** Every `wire/receipts/<n:04>.jsonl` file must be a strict
   byte-prefix extension of its parent commit's file, and every line must parse as a valid
   `DeliveryReceipt`.
3. `StateVerifyReport` gains `verified_wire_messages: usize` and
   `verified_wire_receipts: usize`.

### 4.4 Retention pruning (`fl wire prune`)

`store.prune_wire(before)` scans `WIRE_MESSAGES`:
* A message is eligible for pruning if and only if:
  1. `message.durable == false`,
  2. `message.created_at < before`, and
  3. every `DeliveryReceipt` for `message.id` has `status == DeliveryStatus::Acked` or
     `status == DeliveryStatus::Expired`.
* For each eligible message, `prune_wire` deletes its rows from `WIRE_MESSAGES`, `WIRE_MSG_SEQ`,
  `WIRE_MSG_LOG` (using the `seq` from `WIRE_MSG_SEQ`), `WIRE_INBOX`, `WIRE_RCPT_LOG` (any row
  whose `message_id` matches a pruned message), `WIRE_THREADS`, `WIRE_REPLIES`, and `IDS` in a
  single atomic transaction.
* In addition, `wire_messages_after` and `wire_receipts_after` gracefully skip any sequence
  entry whose `MessageId` was pruned from `WIRE_MESSAGES` rather than returning a decode error.

---

## 5. Configuration, CLI, `agent-msg` shim, and MCP surface (`crates/cli`)

### 5.1 Configuration (`[wire]` in `$XDG_CONFIG_HOME/fl/config.toml`)

`crates/cli/src/config.rs` adds `pub wire: Option<WireConfig>` to `Config`:

```toml
[wire]
# Optional Unix domain socket path for local daemon IPC.
socket = "/run/user/1000/fl-wire.sock"
# Optional TCP address for accepting incoming WebSocket peer connections.
listen = "0.0.0.0:4224"
# Optional outbound WebSocket peer/hub URLs (ws:// or wss://).
peers = ["wss://hub.example.com:4224"]
# Optional path to file containing the pre-shared bearer token (falls back to $FL_WIRE_TOKEN).
token_file = "/home/lmcfarlin/Projects/Artificial-Humanity/.agents/secrets/fl-wire.token"
# Retention window in days for acknowledged transient messages (default: 30).
retention_days = 30
# Whether `fl state sync` exports unacknowledged and durable wire messages (default: true).
sync_wire = true
```

In `Config::sync_target_for_scope` (`crates/cli/src/config.rs`), `StateSyncTarget::sync_wire` is
set to `self.wire.as_ref().and_then(|w| w.sync_wire).unwrap_or(true)` when syncing the workspace
state store (or when a project scope shares `entry.store == ws_store`), and `false` for
standalone project stores that do not host the workspace wire tables.

### 5.2 `fl wire` CLI subcommands (`crates/cli/src/cmd/wire.rs`)

`Command` in `crates/cli/src/main.rs` adds `Wire { #[command(subcommand)] cmd: cmd::wire::WireCmd }`:

* `fl wire send <to> <subject> <body> [--kind mail|handoff|system] [--urgent] [--durable] [--thread <id>] [--from <name>] [--json]`
* `fl wire broadcast <subject> <body> [--urgent] [--durable] [--from <name>] [--json]`
* `fl wire ask <to> <subject> <body> [--timeout <secs>] [--urgent] [--from <name>] [--json]`
  * Sends a durable `MessageKind::Ask` message.
  * When `--timeout <secs>` is passed, waits up to `<secs>` (via Unix-socket `Subscribe` if the
    daemon is active, or `100 ms` store polling fallback) for a `MessageKind::Reply` with
    `correlation_id == ask_msg.id`. If a reply arrives, prints it and exits `0`; on timeout,
    exits with code `124`.
* `fl wire reply <message_id> <body> [--subject <subject>] [--from <name>] [--json]`
  * Resolves `<message_id>` in `MessageStore`, defaults subject to `"Re: <parent.subject>"`
    (without doubling `"Re: "`), sets `thread_id = parent.thread_id`, `reply_to = Some(parent.id)`,
    `correlation_id = Some(parent.correlation_id.unwrap_or(parent.id))`, and sends a
    `MessageKind::Reply` to `parent.sender_id` (or `WireAddress::AgentName(parent.sender_name)`).
* `fl wire read [--ack] [--all] [--thread <id>] [--for <name>] [--json]`
  * Reads inbox messages for the resolved caller (`unread_only = !all`). When `--ack` is set,
    marks each returned receipt as `Acked`.
* `fl wire ack <message_id>... [--for <name>] [--json]`
* `fl wire wait [--ack] [--block] [--timeout <secs>] [--for <name>] [--json]`
  * Registers the caller's vendor route (`Claude`, `Agy`, or `Codex`).
  * If `--block` or `--timeout` is given, waits for the next unread message (exiting `0` when a
    message arrives or `2` on timeout, matching `agent-msg wait` in `cli.py:237`).
  * Otherwise drains any immediately unread messages and exits `0` with the status line:
    `"Central FerroWire dispatcher is active for '<persona>'. No background wait process is needed."`
* `fl wire prune [--older-than-days <days>] [--json]`
* `fl wire status [--json]`
* `fl wire daemon [--listen <addr>] [--peer <url>...] [--socket <path>] [--token-file <path>]`
* `fl wire hook claude`
  * Reads the Claude Code / Codex JSON hook payload from `stdin` (`hook_event_name`, `cwd`,
    `transcript_path`, `session_id` or `thread_id`, `stop_hook_active`).
  * Resolves the persona. If `transcript_path` contains `".codex"`, registers a
    `VendorRoute::Codex { thread_id, cwd }`; otherwise registers the caller's `Claude` route.
  * Reads and acknowledges any pending unread messages, and prints the exact Claude/Codex hook
    JSON response (`{"decision": "block", "reason": ...}` on `"Stop"`, or
    `{"hookSpecificOutput": ...}` on `"UserPromptSubmit" | "PostToolUse" | "SessionStart"`, or
    `{}` when no unread messages exist).

### 5.3 Multi-call binary and `agent-msg` compatibility shim (`crates/cli/src/main.rs`)

Before parsing standard `fl` arguments, `main()` inspects `argv[0]` file stem (and also supports
`fl agent-msg ...` as a hidden subcommand):

1. **`argv[0]` == `"agent-msg"` (or `fl agent-msg ...`):**
   Accepts the exact CLI flags and subcommands of `AI-Lab-AMD/scripts/agent-bus/agent_bus/cli.py`:
   * Global/subcommand flags: `--server <ignored>` (accepted for compatibility), `--from <sender>`,
     `--db <path>`.
   * `send <recipient> <subject> <body>` $\rightarrow$ `fl wire send`
   * `broadcast <subject> <body>` $\rightarrow$ `fl wire broadcast`
   * `read [--unread-only] [--json] [--ack]` $\rightarrow$ `fl wire read`
   * `ack <message_id>` $\rightarrow$ `fl wire ack`
   * `wait [--timeout <secs>] [--ack] [--block]` $\rightarrow$ `fl wire wait` (exiting `2` on
     `--block` / `--timeout` expiry and `0` when non-blocking or when a message arrives,
     matching `cli.py:232-255`).
   * `listen` $\rightarrow$ streams incoming messages to `stdout`
   * `register <name> --project <project> [--role <role>] [--extra-areas <csv>] [--replace]`
     $\rightarrow$ defaults `--role` to `"assistant"`, parses `--role` via
     `AgentRole::from_wire(&raw).or_else(|_| validate_slug(&raw).map(AgentRole::Custom))` so bare
     custom roles like `--role scribe` map to `AgentRole::Custom("scribe")`, sets
     `create_area: true` and `by: name.to_ascii_lowercase()`, registers the agent in
     `AgentRegistry`, records the caller's live vendor route, updates `.agents/REGISTRY.md` if
     present, and exits with code `2` on `StoreError::AgentCollision` or `StoreError::LeadTaken`
     (matching `agent-msg register`).
   * `unregister <name>` $\rightarrow$ retires the agent in `AgentRegistry` (`fl agent retire`).
   * `roster [--json]` $\rightarrow$ renders the workspace agent and area roster (`fl agent roster`).
2. **`argv[0]` == `"agent-bus-claude-hook"`:**
   Runs `fl wire hook claude`.
3. **`argv[0]` == `"agent-bus-mcp"`:**
   Runs the stdio MCP server (`fl state mcp`).

### 5.4 MCP server tools (`crates/cli/src/cmd/state_mcp.rs`)

The stdio MCP server (`fl state mcp`, also reachable via `fl wire mcp` and `agent-bus-mcp`)
adds six FerroWire tools to `tool_definitions()` and `dispatch_tool()`:

1. `send_message`:
   * Arguments: `to` (required), `subject` (required), `body` (required), `kind` (optional:
     `"mail"`, `"handoff"`, `"system"`), `urgent` (optional `bool`), `durable` (optional `bool`),
     `thread_id` (optional), `from_agent` (optional).
2. `broadcast_message`:
   * Arguments: `subject` (required), `body` (required), `urgent` (optional `bool`), `durable`
     (optional `bool`), `from_agent` (optional).
3. `ask_message`:
   * Arguments: `to` (required), `subject` (required), `body` (required), `timeout_secs`
     (optional `u64`), `from_agent` (optional).
4. `reply_message`:
   * Arguments: `message_id` (required), `body` (required), `subject` (optional), `from_agent`
     (optional).
5. `get_messages`:
   * Arguments: `unread_only` (optional `bool`, default `true`), `ack` (optional `bool`, default
     `false`), `agent` (optional), `thread_id` (optional).
   * Returns a JSON array of message objects containing both `"to"` and `"recipient"` (for
     backward compatibility with `agent-bus-mcp` consumers): `id`, `thread_id`, `reply_to`,
     `correlation_id`, `sender`, `to`, `recipient`, `kind`, `urgency`, `durable`, `timestamp`,
     `subject`, `body`, `status`.
6. `ack_message`:
   * Arguments: `message_id` (required), `agent` (optional).

---

## 6. Error handling, security invariants, and testing strategy

### 6.1 Error variants (`StateError` and `StoreError`)

* `StateError` in `crates/core/src/state.rs` (or `WireError` in `crates/core/src/ferrowire.rs`
  wrapped by `StoreError`) defines clear refusal messages for:
  * Empty, multi-line, or oversized (`> 512` bytes) message `subject`.
  * Empty or oversized (`> 262_144` bytes) message `body`.
  * Malformed `WireAddress` selector (`InvalidAddress`).
  * `Reply` missing `reply_to` / `correlation_id`, or referencing an unknown parent `MessageId`
    (`MessageNotFound`).
  * Byte-for-byte tamper detection when `import_wire_batch` encounters an existing `MessageId`
    with differing content (`StoreError::WireMessageTampered`).

### 6.2 Security invariants

1. **Constant-time PSK verification:** WebSocket peer handshakes verify HMAC-SHA256 signatures
   over fresh random 32-byte nonces from both peers using `ring::hmac::verify`, preventing
   replay attacks and timing side channels.
2. **Never log or sync tokens:** Pre-shared tokens (`FL_WIRE_TOKEN` / `token_file`) and vendor
   session keys (`peerToken`, `ANTIGRAVITY_CSRF_TOKEN`) are never written to `RedbStore` or
   `refs/heads/fl/state`.
3. **Unix socket permissions:** The local daemon sets `0600` permissions on its Unix socket
   parent/file so other Unix users on a multi-user system cannot inject frames.

### 6.3 Testing strategy

1. **Shared `MessageStore` conformance suite (`crates/core/src/conformance.rs`):**
   Runs against both `MemStore` and `RedbStore`, testing direct persona addressing, cold-roster
   persona fallback, `@all` (excluding sender), `@area:<slug>`, `@role:<role>`,
   `@area:<slug>/<role>`, `Ask`/`Reply` correlation and threading, monotonic `DeliveryReceipt`
   merges, cursor catch-up (`wire_messages_after` / `wire_receipts_after`), tamper refusal on
   mutated `WireMessage` imports, and `prune_wire` retention rules.
2. **`RedbStore` Format 7 migration and `verify_wire_invariants` tests (`crates/store`):**
   Verifies opening a Format 6 store, raising to Format 7 on first wire write, and catching
   corrupted or orphaned `wire_*` table rows in `RedbStore::verify_wire_invariants`.
3. **Live daemon, Unix socket, WebSocket/TLS peer relay, and push dispatcher tests (`crates/wire`):**
   * Two `fl-wire` daemons with separate `RedbStore` instances connecting over loopback
     WebSocket with PSK authentication: verifies sub-50ms cross-node message delivery, broadcast
     fan-out without echo loops, disconnect/reconnect cursor catch-up, invalid token rejection,
     and automatic push into a mock `VendorRoute::Claude` Unix socket (verifying the exact
     `{"type":"auth"}` and nested `{"type":"user","from":...,"message":{"role":"user","content":...}}`
     frames) and `VendorRoute::Webhook` listener.
4. **Git `fl/state` store-and-forward and tamper tests (`crates/exec`):**
   * Offline node A sends durable and unacknowledged messages, runs `fl state sync` to a bare
     git remote; node B runs `fl state sync`, reads and acknowledges the messages, and syncs the
     `Acked` receipts back to node A; `fl state verify` (`verify_state`) confirms append-only
     receipt segments and detects tampered `wire/messages/` blobs.
5. **End-to-end CLI, `agent-msg` compatibility, Claude hook, and MCP black-box tests (`crates/cli`):**
   * Tests `fl wire send/broadcast/ask/reply/read/ack/wait/prune`, `fl agent-msg` compatibility
     commands (including bare custom roles like `--role scribe`, automatic area creation, and
     exit code `2` on registration collision), `fl wire hook claude` Stop and SessionStart
     payloads (including `.codex` transcript routing), and `fl state mcp` JSON-RPC tool calls.
