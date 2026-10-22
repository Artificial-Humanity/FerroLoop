# State Database and Agent Registry — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the two-tier state database (`fl state`) for durable `Memory`, `Handoff`, and `Runtime` state alongside the centralized Agent and Role Registry (`fl agent`), persisted locally in `fl-store` (Format 6), synced across machines over any standard git remote on an orphan `fl/state` branch without touching the working tree, and exposed through the `fl` CLI, a newline-delimited JSON-RPC 2.0 stdio MCP server (`fl state mcp`), and read-only Markdown exports.

**Architecture:** Two new pure role traits (`StateStore` and `AgentRegistry`) and their domain types (`StateEntry`, `StateRevision`, `StatePayload`, `StateScope`, `StateConcern`, `AgentRecord`, `AreaRecord`, `RegistryEvent`) live in `fl-core` and are implemented by both `MemStore` and `RedbStore` (bumping modified stores to `FORMAT_WITH_STATE = 6` across 11 additive `redb` tables while indexing `StateId` and `AgentId` in `IDS`, `HANDLES`, and `HANDLE_OF`). Standard-git synchronization lives in `crates/exec/src/state_sync.rs`, shelling out to git plumbing (`ls-remote --exit-code`, `fetch`, `merge-base --is-ancestor`, `hash-object -w --stdin`, `ls-tree`, `mktree`, `commit-tree`, `push --force-with-lease`) so `HEAD`, `.git/index`, and the working tree are never touched. The CLI adds `fl agent` and `fl state` (including `fl state mcp`, which opens and drops `RedbStore` inside each `tools/call` so the `.redb` file lock is never held while waiting on `stdin`).

**Tech Stack:** Rust 2024 (MSRV 1.98), `redb` 4.3, `serde` / `serde_json` (without `preserve_order`), `clap` 4.6, `sha2` 0.10, `uuid` 1.26 (`v7`), `toml` 1.1, `tempfile` 3.27.

**Spec:** `docs/superpowers/specs/2026-10-22-state-database-design.md`

## Global Constraints

- Edition 2024, `rust-version = "1.98"`, Apache-2.0. No new external dependencies in workspace `Cargo.toml`; add `sha2.workspace = true` to `crates/exec/Cargo.toml` and `tempfile.workspace = true` to `crates/cli/Cargo.toml` `[dependencies]`.
- **Always run `cargo test`, `cargo clippy`, and `cargo fmt` from inside the worktree directory** so `.cargo/config.toml` sets `FL_SET_PROGRAM_TEST_HOOK=1` for black-box CLI subprocess tests (`crates/cli/tests/findings.rs`).
- `fl-core` stays pure: no IO, no clock, no randomness, no new dependency. `MemStore` mints deterministic `seq_iri` IDs and deterministic revision/event timestamps when needed; callers pass `at: At` into mutation methods.
- `serde_json`'s `preserve_order` feature is **forbidden** workspace-wide so `serde_json::Value` and structs serialize with deterministic key ordering for byte-for-byte JSONL tamper verification.
- Every new wire enum uses `wire_names!` + `wire_parse!` (unit variants) or `wire_tags!` (data-carrying variants) in `fl-core`. `StateScope` and `AgentRole` use custom flat-string `Serialize`/`Deserialize` + `as_wire`/`from_wire`.
- `FORMAT_WITH_STATE = 6` in `crates/store/src/lib.rs`: any write to `StateStore` or `AgentRegistry` calls `raise_format(&tx, FORMAT_WITH_STATE)` inside the same `redb` write transaction. A store that never writes state or agents stays at its existing format (`2..=5`).
- Every task ends green on the verification trio:
  1. `cargo test --workspace`
  2. `cargo clippy --all-targets --workspace -- -D warnings`
  3. `cargo fmt --all --check`
- Every commit message ends with the trailer:
  `Co-authored-by: Ferris <ferris@artificialhumanity.io>`

## Review Focus

1. **Disjoint reference syntax (`^[a-z][a-z0-9_-]{0,63}$`):** every `slug`, `area_key`, `name_key`, and custom role slug starts with an ASCII lowercase letter (`[a-z]`), never a digit. Bare digits (`1`, `42`) always parse as `Ref::Handle(u64)`, `urn:uuid:...` as `Ref::Iri`, and `[a-z]...` as a slug or `name_key`. Pinned in Tasks 1 and 6.
2. **Collision and one-lead exit code `2`:** `fl agent register` refuses an existing `name_key` (active or retired) with `StoreError::AgentCollision` and refuses a second active `lead` on the same project with `StoreError::LeadTaken`, exiting `2` unless `--replace` is given. With `--replace`, the existing `AgentId` IRI is preserved and any displaced lead is retired in the same transaction. Pinned in Tasks 2, 3, and 6.
3. **Sticky `sensitive` flag and `ever_sensitive` sync guard:** omitting `--sensitive` / `--not-sensitive` on an update preserves `entry.sensitive`. Once any revision of an entry has `sensitive == true`, `entry.ever_sensitive` stays `true` forever, and `fl state sync` refuses to push to a target with `private == false` (`StoreError::SensitiveDisclosure`) before creating any git blob or commit. Pinned in Tasks 2, 4, and 6.
4. **Offline same-key `StateId` / `AgentId` collision convergence:** when two machines independently mint `S_local` and `S_remote` for the same `(scope, concern, slug)` (or `name_key`) before syncing, the first-published (`S_remote`) wins on sync; `S_local` is converted to `"alias"` in `IDS` and mapped `S_local -> S_remote` in `ALIASES`, the duplicate row under `S_local` is removed, and unpublished local revisions for `S_local` are re-parented/re-keyed to `S_remote` before publication. Pinned in Tasks 2, 3, 4, and 6.
5. **Working-tree and `.git/index` untouched during `fl state sync`:** git plumbing (`hash-object -w --stdin`, `mktree`, `commit-tree`, `push --force-with-lease`) never modifies `HEAD`, `.git/index`, or tracked/untracked files, and sets `GIT_AUTHOR_*` / `GIT_COMMITTER_*` explicitly so clean `$HOME` environments without `~/.gitconfig` succeed. Pinned in Tasks 4 and 6.
6. **MCP stdio server (`fl state mcp`) never holds `RedbStore` open across `stdin` reads:** `main.rs` dispatches `fl state mcp` before opening `RedbStore`, and each `tools/call` opens and drops `RedbStore` inside the call handler so concurrent `fl` CLI commands never block on the `.redb` file lock. Pinned in Task 7.

## Rulings this plan makes

- **`StateStore` and `AgentRegistry` trait signatures take `at: At` explicitly:** because `fl-core` has no clock, callers in `fl-exec` and `fl-cli` pass `fl_exec::stamp::now()` (and `MemStore` / conformance tests pass deterministic `At::from_unix_millis(...)` stamps).
- **`StateSyncCursor` key format:** stored in `state_sync_cursors` under the canonical key `format!("{}::{remote}", repo_dir.display())`.
- **`revisions/<hash>/<n>.jsonl` segment naming:** per spec §4.1, segment `<n>` is formatted as 4-digit zero-padded decimal `format!("{n:04}.jsonl")` starting at `0000.jsonl` (`0000.jsonl`, `0001.jsonl`, …), rolling over to `n + 1` when adding a line (`line.len() + 1`) to a non-empty segment would exceed `262_144` bytes (256 KiB).
- **Atomic non-symlink file writer (`write_atomic_non_symlink`):** shared by `fl agent roster --render <path>` and `fl state render --out <path>`, walking every existing path component from the nearest existing ancestor down to the target with `fs::symlink_metadata` to refuse any symlink before writing via `tempfile::NamedTempFile::new_in` + `persist`.

---

## File Structure

| file | responsibility | tasks |
|---|---|---|
| `crates/core/src/ids.rs` | Add `Kind::State`, `Kind::Agent`, `StateId`, `AgentId`, `RevisionId` | 1 |
| `crates/core/src/state.rs` (new) | `validate_slug`, `StateScope`, `StateConcern`, `MemoryCategory`, `MemoryBasis`, `MemoryLifecycle`, `MergeMode`, `MemoryEntry`, `HandoffEntry`, `RuntimeEntry`, `StatePayload`, `StateEntry`, `StateRevision`, `StateConflict`, `StateSyncCursor`, `StateError`, `SensitivityPatch` | 1 |
| `crates/core/src/agent.rs` (new) | `AreaRecord`, `AgentRole`, `AgentStatus`, `AgentRecord`, `RegistryEvent`, `RegistryEventKind`, `RegisterAgentArgs`, `reports_to` derivation | 1 |
| `crates/core/src/store.rs` | New `StoreError` variants, `StateStore` and `AgentRegistry` traits, `KindRouted::owner` routing `Kind::State \| Kind::Agent` to `self.catalog` | 1 |
| `crates/core/src/lib.rs` | Export `state` and `agent` modules and types | 1 |
| `crates/core/tests/wire_refs.rs` | Extend full-IRI reference scan floor to `StateEntry`, `StateRevision`, `AgentRecord`, `RegistryEvent` | 1 |
| `crates/core/src/mem.rs` | Implement `AgentRegistry` and `StateStore` on `MemStore`, including `alias_unpublished_id`, DAG merge, and offline collision unification | 2 |
| `crates/core/src/conformance.rs` | Add `agent_registry` (`AGENT_REGISTRY_CASES`) and `state_store` (`STATE_STORE_CASES`) suites; update `an_id_has_no_handle_under_any_kind_but_its_own` and `handles_are_per_kind_and_start_at_one` for all 6 `Kind`s | 2 |
| `crates/store/src/state.rs` (new) | Implement `AgentRegistry` and `StateStore` for `RedbStore` with `FORMAT_WITH_STATE = 6` and 11 additive tables | 3 |
| `crates/store/src/lib.rs` | Define `FORMAT_WITH_STATE = 6` and the 11 table definitions, accept `2..=6` in `RedbStore::open`, wire `mod state`, add Format 6 and conformance tests | 3 |
| `crates/exec/Cargo.toml` | Add `sha2.workspace = true` to `[dependencies]` | 4 |
| `crates/exec/src/git.rs` | Add low-level git plumbing helpers (`ls_remote_branch`, `fetch_branch`, `is_ancestor`, `rev_list_first_parent`, `ls_tree`, `cat_blob`, `hash_object`, `mktree`, `commit_tree`, `push_cas`) | 4 |
| `crates/exec/src/state_sync.rs` (new) | Orphan-branch `fl/state` layout, segment rollover (`262_144` bytes), append-only check, 5-step CAS `sync_state`, and `verify_state` | 4 |
| `crates/exec/src/lib.rs` | Export `state_sync` module | 4 |
| `crates/cli/Cargo.toml` | Add `tempfile.workspace = true` to `[dependencies]` | 5 |
| `crates/cli/src/config.rs` | Add `StateConfig`, `ProjectStateConfig`, `Entry.name`, `Entry.state`, workspace/project state store and sync target resolution | 5 |
| `crates/cli/src/cmd/agent.rs` (new) | `fl agent` subcommands (`area set`, `area list`, `register`, `retire`, `roster [--render]`) and `write_atomic_non_symlink` | 6 |
| `crates/cli/src/cmd/state.rs` (new) | `fl state` subcommands (`memory set`, `memory retire`, `handoff set`, `runtime set`, `get`, `list`, `conflicts`, `resolve`, `render`, `sync`, `verify`, `mcp`) | 6, 7 |
| `crates/cli/src/cmd/state_mcp.rs` (new) | Newline-delimited JSON-RPC 2.0 stdio MCP server (`fl state mcp`) with 8 tools and per-call `RedbStore` open/drop | 7 |
| `crates/cli/src/cmd/mod.rs` | Register `pub mod agent;`, `pub mod state;`, `pub mod state_mcp;` | 6, 7 |
| `crates/cli/src/main.rs` | Dispatch `Command::Agent` and `Command::State` (including `fl state mcp` before `RedbStore::open`) and include workspace state store in cross-store IRI search | 5, 6, 7 |
| `crates/cli/tests/state_sync.rs` (new) | End-to-end black-box tests for `fl agent`, `fl state`, multi-machine git sync, conflict resolution, sensitivity guard, Markdown export, and MCP lock release | 6, 7 |

---

### Task 1: Domain types, slugs, wire guards, and store role traits (`fl-core`)

**Files:**
- Modify: `crates/core/src/ids.rs`
- Create: `crates/core/src/state.rs`
- Create: `crates/core/src/agent.rs`
- Modify: `crates/core/src/store.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/core/tests/wire_refs.rs`

**Interfaces:**
- Consumes: `fl_core::iri::Iri`, `fl_core::at::At`, `fl_core::wire::{wire_names, wire_parse, wire_tags}`.
- Produces:
  - `Kind::State`, `Kind::Agent`, `StateId(pub Iri)`, `AgentId(pub Iri)`, `RevisionId(pub Iri)` in `fl_core::ids`.
  - `fl_core::state::{validate_slug, StateError, StateScope, StateConcern, MemoryCategory, MemoryBasis, MemoryLifecycle, MergeMode, SensitivityPatch, MemoryEntry, HandoffEntry, RuntimeEntry, StatePayload, StateEntry, StateRevision, StateConflict, StateSyncCursor, UpsertStateArgs}`.
  - `fl_core::agent::{AreaRecord, AgentRole, AgentStatus, AgentRecord, RegistryEvent, RegistryEventKind, RegisterAgentArgs, reports_to}`.
  - `fl_core::store::{StateStore, AgentRegistry}` and new `StoreError` variants (`AgentCollision`, `LeadTaken`, `NoSuchArea`, `NoSuchAgent`, `NoSuchState`, `StateConflict`, `SensitiveDisclosure`, `InvalidState`).

- [ ] **Step 1: Update `crates/core/src/ids.rs` with `Kind::State`, `Kind::Agent`, `StateId`, `AgentId`, and `RevisionId`**

Replace `crates/core/src/ids.rs` with:

```rust
use crate::iri::Iri;
use serde::{Deserialize, Serialize};

/// What kind of item an id names. Crosses the boundary in the store's
/// ownership index and in handle tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Project,
    Gate,
    Record,
    Finding,
    State,
    Agent,
}

crate::wire::wire_names!(Kind as kind_wire {
    Project => "project",
    Gate => "gate",
    Record => "record",
    Finding => "finding",
    State => "state",
    Agent => "agent",
});
crate::wire::wire_parse!(Kind as kind_parse);

/// A deterministic, UUID-shaped id for stores that must not use a clock or
/// randomness (`MemStore`, and tests). Never used by a store that persists.
pub fn seq_iri(n: u64) -> Iri {
    Iri::parse(&format!("urn:uuid:00000000-0000-7000-8000-{n:012x}"))
        .expect("a formatted urn:uuid is a valid IRI")
}

macro_rules! id_type {
    ($name:ident, $kind:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Iri);

        impl $name {
            pub const KIND: Kind = $kind;
            pub fn iri(&self) -> &Iri {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_type!(ProjectId, Kind::Project);
id_type!(GateId, Kind::Gate);
id_type!(RecordId, Kind::Record);
id_type!(FindingId, Kind::Finding);
id_type!(StateId, Kind::State);
id_type!(AgentId, Kind::Agent);

/// An immutable revision identifier (`urn:uuid:` v7) naming one revision in
/// `state_revisions` and on `fl/state`. Like ledger entry IDs, `RevisionId` is
/// not registered in `HANDLES`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RevisionId(pub Iri);

impl RevisionId {
    pub fn iri(&self) -> &Iri {
        &self.0
    }
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for RevisionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
```

- [ ] **Step 2: Create `crates/core/src/state.rs` with domain types, validation, wire guards, and unit tests**

Create `crates/core/src/state.rs`:

```rust
//! State database domain types: scopes, the three concerns (`Memory`,
//! `Handoff`, `Runtime`), revisions, conflicts, and sync cursors (spec §1, §3).

use crate::at::At;
use crate::ids::{RevisionId, StateId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;

pub const MAX_SUMMARY_CHARS: usize = 240;
pub const MAX_BODY_BYTES: usize = 256 * 1024;
pub const MAX_NOTES_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error(
        "invalid slug `{0}`: must be 1..=64 bytes of ASCII lowercase letters, digits, `-`, or `_`, starting with an ASCII letter (`^[a-z][a-z0-9_-]{{0,63}}$`)"
    )]
    InvalidSlug(String),
    #[error(
        "invalid state scope `{0}`: expected `workspace`, `group:<slug>`, `project:<slug>`, `agent:<name>`, or `workflow:<slug>`"
    )]
    InvalidScope(String),
    #[error("summary must be a single non-empty line of at most 240 characters")]
    InvalidSummary,
    #[error("memory body exceeds maximum size of 262144 bytes (got {0} bytes)")]
    BodyTooLarge(usize),
    #[error("handoff notes exceed maximum size of 65536 bytes (got {0} bytes)")]
    NotesTooLarge(usize),
    #[error("runtime value exceeds maximum serialized size of 262144 bytes (got {0} bytes)")]
    RuntimeValueTooLarge(usize),
    #[error("runtime entry with `merge_mode = \"append\"` requires a JSON array value")]
    AppendRequiresArray,
    #[error("memory with `lifecycle = \"superseded\"` requires `superseded_by`")]
    SupersededRequiresTarget,
    #[error("memory with `lifecycle = \"{0}\"` cannot set `superseded_by`")]
    UnexpectedSupersededBy(String),
    #[error("memory `{0}` cannot supersede itself")]
    SelfSupersede(StateId),
}

/// Normalize to ASCII lowercase and validate `^[a-z][a-z0-9_-]{0,63}$`.
pub fn validate_slug(raw: &str) -> Result<String, StateError> {
    let lower = raw.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return Err(StateError::InvalidSlug(raw.to_string()));
    }
    if !bytes[0].is_ascii_lowercase() {
        return Err(StateError::InvalidSlug(raw.to_string()));
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
    {
        return Err(StateError::InvalidSlug(raw.to_string()));
    }
    Ok(lower)
}

pub fn validate_summary(s: &str) -> Result<String, StateError> {
    if s.contains('\n') || s.contains('\r') {
        return Err(StateError::InvalidSummary);
    }
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_SUMMARY_CHARS {
        return Err(StateError::InvalidSummary);
    }
    Ok(trimmed.to_string())
}

/// Hierarchical state scope (spec §1.2).
/// Order of variants defines the deterministic sort order:
/// `Workspace < Group < Project < Agent < Workflow`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateScope {
    Workspace,
    Group(String),
    Project(String),
    Agent(String),
    Workflow(String),
}

impl StateScope {
    pub fn from_wire(s: &str) -> Result<Self, StateError> {
        if s.eq_ignore_ascii_case("workspace") {
            return Ok(Self::Workspace);
        }
        let Some((prefix, rest)) = s.split_once(':') else {
            return Err(StateError::InvalidScope(s.to_string()));
        };
        let slug =
            validate_slug(rest).map_err(|_| StateError::InvalidScope(s.to_string()))?;
        match prefix.to_ascii_lowercase().as_str() {
            "group" => Ok(Self::Group(slug)),
            "project" => Ok(Self::Project(slug)),
            "agent" => Ok(Self::Agent(slug)),
            "workflow" => Ok(Self::Workflow(slug)),
            _ => Err(StateError::InvalidScope(s.to_string())),
        }
    }

    pub fn as_wire(&self) -> String {
        match self {
            Self::Workspace => "workspace".to_string(),
            Self::Group(s) => format!("group:{s}"),
            Self::Project(s) => format!("project:{s}"),
            Self::Agent(s) => format!("agent:{s}"),
            Self::Workflow(s) => format!("workflow:{s}"),
        }
    }
}

impl fmt::Display for StateScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_wire())
    }
}

impl FromStr for StateScope {
    type Err = StateError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_wire(s)
    }
}

impl Serialize for StateScope {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_wire())
    }
}

impl<'de> Deserialize<'de> for StateScope {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::from_wire(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateConcern {
    Memory,
    Handoff,
    Runtime,
}

crate::wire::wire_names!(StateConcern as state_concern_wire {
    Memory => "memory",
    Handoff => "handoff",
    Runtime => "runtime",
});
crate::wire::wire_parse!(StateConcern as state_concern_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    Warning,
    Rule,
    Decision,
    Note,
}

crate::wire::wire_names!(MemoryCategory as memory_category_wire {
    Warning => "warning",
    Rule => "rule",
    Decision => "decision",
    Note => "note",
});
crate::wire::wire_parse!(MemoryCategory as memory_category_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryBasis {
    Ruling,
    Measured,
    Inference,
}

crate::wire::wire_names!(MemoryBasis as memory_basis_wire {
    Ruling => "ruling",
    Measured => "measured",
    Inference => "inference",
});
crate::wire::wire_parse!(MemoryBasis as memory_basis_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLifecycle {
    Active,
    Retired,
    Superseded,
}

crate::wire::wire_names!(MemoryLifecycle as memory_lifecycle_wire {
    Active => "active",
    Retired => "retired",
    Superseded => "superseded",
});
crate::wire::wire_parse!(MemoryLifecycle as memory_lifecycle_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    Conflict,
    Append,
    LastWriter,
}

crate::wire::wire_names!(MergeMode as merge_mode_wire {
    Conflict => "conflict",
    Append => "append",
    LastWriter => "last_writer",
});
crate::wire::wire_parse!(MergeMode as merge_mode_parse);

/// How an update changes `sensitive` on a `StateEntry` (spec §1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitivityPatch {
    /// Omit flag: keep existing `sensitive` on update, or `false` on create.
    Inherit,
    /// `--sensitive` / `sensitive = true`: mark entry sensitive.
    SetSensitive,
    /// `--not-sensitive` / `clear_sensitive = true`: clear current `sensitive`
    /// while preserving `ever_sensitive` if any prior revision was sensitive.
    ClearSensitive,
}

impl SensitivityPatch {
    pub fn apply(self, existing: Option<bool>) -> bool {
        match self {
            Self::SetSensitive => true,
            Self::ClearSensitive => false,
            Self::Inherit => existing.unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEntry {
    pub category: MemoryCategory,
    pub basis: MemoryBasis,
    pub lifecycle: MemoryLifecycle,
    pub superseded_by: Option<StateId>,
    pub pinned: bool,
    pub summary: String,
    pub body: String,
}

impl MemoryEntry {
    pub fn validate(&self) -> Result<(), StateError> {
        validate_summary(&self.summary)?;
        if self.body.len() > MAX_BODY_BYTES {
            return Err(StateError::BodyTooLarge(self.body.len()));
        }
        match (self.lifecycle, &self.superseded_by) {
            (MemoryLifecycle::Superseded, None) => Err(StateError::SupersededRequiresTarget),
            (MemoryLifecycle::Active | MemoryLifecycle::Retired, Some(_)) => {
                Err(StateError::UnexpectedSupersededBy(
                    self.lifecycle.as_wire().to_string(),
                ))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffEntry {
    pub summary: String,
    pub shipped: Vec<String>,
    pub in_flight: Vec<String>,
    pub blockers: Vec<String>,
    pub next_steps: Vec<String>,
    pub notes: Option<String>,
}

impl HandoffEntry {
    pub fn validate(&self) -> Result<(), StateError> {
        validate_summary(&self.summary)?;
        if let Some(notes) = &self.notes
            && notes.len() > MAX_NOTES_BYTES
        {
            return Err(StateError::NotesTooLarge(notes.len()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeEntry {
    pub value: Value,
    pub merge_mode: MergeMode,
}

impl RuntimeEntry {
    pub fn validate(&self) -> Result<(), StateError> {
        if self.merge_mode == MergeMode::Append && !self.value.is_array() {
            return Err(StateError::AppendRequiresArray);
        }
        let encoded = serde_json::to_string(&self.value).unwrap_or_default();
        if encoded.len() > MAX_BODY_BYTES {
            return Err(StateError::RuntimeValueTooLarge(encoded.len()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatePayload {
    Memory(MemoryEntry),
    Handoff(HandoffEntry),
    Runtime(RuntimeEntry),
}

impl StatePayload {
    pub fn concern(&self) -> StateConcern {
        match self {
            Self::Memory(_) => StateConcern::Memory,
            Self::Handoff(_) => StateConcern::Handoff,
            Self::Runtime(_) => StateConcern::Runtime,
        }
    }

    pub fn validate(&self) -> Result<(), StateError> {
        match self {
            Self::Memory(m) => m.validate(),
            Self::Handoff(h) => h.validate(),
            Self::Runtime(r) => r.validate(),
        }
    }
}

crate::wire::wire_tags!(StatePayload as state_payload_wire {
    StatePayload::Memory(_) => "memory", StatePayload::Memory(MemoryEntry {
        category: MemoryCategory::Rule,
        basis: MemoryBasis::Ruling,
        lifecycle: MemoryLifecycle::Active,
        superseded_by: None,
        pinned: true,
        summary: "one line".into(),
        body: "details".into(),
    });
    StatePayload::Handoff(_) => "handoff", StatePayload::Handoff(HandoffEntry {
        summary: "status".into(),
        shipped: vec![],
        in_flight: vec![],
        blockers: vec![],
        next_steps: vec![],
        notes: None,
    });
    StatePayload::Runtime(_) => "runtime", StatePayload::Runtime(RuntimeEntry {
        value: serde_json::json!(["step-1"]),
        merge_mode: MergeMode::Append,
    });
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateEntry {
    pub id: StateId,
    pub scope: StateScope,
    pub concern: StateConcern,
    pub slug: String,
    pub head_revision: RevisionId,
    pub sensitive: bool,
    pub ever_sensitive: bool,
    pub payload: StatePayload,
    pub created_at: At,
    pub updated_at: At,
    pub updated_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateRevision {
    pub id: RevisionId,
    pub entry_id: StateId,
    pub scope: StateScope,
    pub concern: StateConcern,
    pub slug: String,
    pub parent_revisions: Vec<RevisionId>,
    pub sensitive: bool,
    pub payload: StatePayload,
    pub at: At,
    pub by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConflict {
    pub entry_id: StateId,
    pub local_head: RevisionId,
    pub remote_head: RevisionId,
    pub detected_at: At,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateSyncCursor {
    pub last_remote_commit: Option<String>,
    pub last_published_rev_seq: u64,
    pub last_published_agent_seq: u64,
}

#[derive(Debug, Clone)]
pub struct UpsertStateArgs {
    pub scope: StateScope,
    pub slug: String,
    pub sensitivity: SensitivityPatch,
    pub payload: StatePayload,
    pub at: At,
    pub by: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_requires_leading_ascii_letter_and_normalizes_case() {
        assert_eq!(validate_slug("FerroLoop_1-a").unwrap(), "ferroloop_1-a");
        for bad in [
            "",
            "1abc",
            "42",
            "-abc",
            "_abc",
            "a/b",
            "a\\b",
            "a..b",
            "a b",
            "a\nb",
            &"a".repeat(65),
        ] {
            assert!(
                matches!(validate_slug(bad), Err(StateError::InvalidSlug(_))),
                "expected InvalidSlug for {bad:?}"
            );
        }
    }

    #[test]
    fn state_scope_flat_wire_round_trips_and_sorts_by_hierarchy() {
        let scopes = [
            StateScope::from_wire("Workspace").unwrap(),
            StateScope::from_wire("group:Core-Team").unwrap(),
            StateScope::from_wire("project:FerroLoop").unwrap(),
            StateScope::from_wire("agent:Ferris").unwrap(),
            StateScope::from_wire("workflow:Release_1").unwrap(),
        ];
        let wires: Vec<String> = scopes.iter().map(StateScope::as_wire).collect();
        assert_eq!(
            wires,
            vec![
                "workspace",
                "group:core-team",
                "project:ferroloop",
                "agent:ferris",
                "workflow:release_1",
            ]
        );
        for scope in &scopes {
            let json = serde_json::to_string(scope).unwrap();
            assert_eq!(json, format!("\"{}\"", scope.as_wire()));
            let back: StateScope = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, scope);
        }
        let mut shuffled = scopes.to_vec();
        shuffled.reverse();
        shuffled.sort();
        assert_eq!(shuffled, scopes);

        for bad in ["", "project:", "project:123", "other:abc", "workspace:x"] {
            assert!(StateScope::from_wire(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn append_requires_json_array_and_summary_forbids_newlines() {
        let bad_append = RuntimeEntry {
            value: serde_json::json!({"not": "an array"}),
            merge_mode: MergeMode::Append,
        };
        assert_eq!(
            bad_append.validate(),
            Err(StateError::AppendRequiresArray)
        );
        let ok_append = RuntimeEntry {
            value: serde_json::json!([1, 2]),
            merge_mode: MergeMode::Append,
        };
        assert!(ok_append.validate().is_ok());
        assert_eq!(
            validate_summary("line1\nline2"),
            Err(StateError::InvalidSummary)
        );
        assert_eq!(validate_summary("   "), Err(StateError::InvalidSummary));
    }
}
```

- [ ] **Step 3: Create `crates/core/src/agent.rs` with `AreaRecord`, `AgentRole`, `AgentStatus`, `AgentRecord`, `RegistryEvent`, and `reports_to`**

Create `crates/core/src/agent.rs`:

```rust
//! Agent and role registry domain types (spec §2, activating identity spec §6a).

use crate::at::At;
use crate::ids::AgentId;
use crate::iri::Iri;
use crate::state::{StateError, validate_slug, validate_summary};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaRecord {
    pub area_key: String,
    pub name: String,
    pub path: Option<String>,
    pub purpose: String,
    pub updated_at: At,
    pub by: String,
}

impl AreaRecord {
    pub fn new(
        name: &str,
        path: Option<String>,
        purpose: &str,
        updated_at: At,
        by: &str,
    ) -> Result<Self, StateError> {
        let area_key = validate_slug(name.trim())?;
        let purpose = validate_summary(purpose)?;
        Ok(Self {
            area_key,
            name: name.trim().to_string(),
            path,
            purpose,
            updated_at,
            by: by.trim().to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentRole {
    Lead,
    Assistant,
    LabManager,
    Custom(String),
}

impl AgentRole {
    pub fn from_wire(s: &str) -> Result<Self, StateError> {
        match s.to_ascii_lowercase().as_str() {
            "lead" => Ok(Self::Lead),
            "assistant" => Ok(Self::Assistant),
            "lab_manager" => Ok(Self::LabManager),
            other => {
                if let Some(slug) = other.strip_prefix("custom:") {
                    Ok(Self::Custom(validate_slug(slug)?))
                } else {
                    Ok(Self::Custom(validate_slug(other)?))
                }
            }
        }
    }

    pub fn as_wire(&self) -> String {
        match self {
            Self::Lead => "lead".to_string(),
            Self::Assistant => "assistant".to_string(),
            Self::LabManager => "lab_manager".to_string(),
            Self::Custom(s) => format!("custom:{s}"),
        }
    }
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_wire())
    }
}

impl FromStr for AgentRole {
    type Err = StateError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_wire(s)
    }
}

impl Serialize for AgentRole {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_wire())
    }
}

impl<'de> Deserialize<'de> for AgentRole {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::from_wire(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Active,
    Retired,
}

crate::wire::wire_names!(AgentStatus as agent_status_wire {
    Active => "active",
    Retired => "retired",
});
crate::wire::wire_parse!(AgentStatus as agent_status_parse);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRecord {
    pub id: AgentId,
    pub name_key: String,
    pub display_name: String,
    pub email: String,
    pub project: String,
    pub role: AgentRole,
    pub extra_areas: Vec<String>,
    pub status: AgentStatus,
    pub updated_at: At,
    pub by: String,
}

#[derive(Debug, Clone)]
pub struct RegisterAgentArgs {
    pub display_name: String,
    pub email: Option<String>,
    pub project: String,
    pub role: AgentRole,
    pub extra_areas: Vec<String>,
    pub create_area: bool,
    pub replace: bool,
    pub at: At,
    pub by: String,
}

/// Derive `reports_to` deterministically from registry state (spec §2.3):
/// - `Lead` or `LabManager` -> `"Owner"`
/// - `Assistant` or `Custom(_)` -> `display_name` of the active `Lead` for
///   `agent.project` (or `"Owner"` if none).
pub fn reports_to(agent: &AgentRecord, all_agents: &[AgentRecord]) -> String {
    match agent.role {
        AgentRole::Lead | AgentRole::LabManager => "Owner".to_string(),
        AgentRole::Assistant | AgentRole::Custom(_) => all_agents
            .iter()
            .find(|a| {
                a.status == AgentStatus::Active
                    && a.project == agent.project
                    && a.role == AgentRole::Lead
            })
            .map(|lead| lead.display_name.clone())
            .unwrap_or_else(|| "Owner".to_string()),
    }
}

/// Validate `display_name` and optional `email` per spec §2.2.
/// Returns `(name_key, display_name, email)`.
pub fn validate_agent_identity(
    display_name: &str,
    email: Option<&str>,
) -> Result<(String, String, String), String> {
    let display = display_name.trim();
    let name_key = validate_slug(display).map_err(|e| e.to_string())?;
    if display.to_ascii_lowercase() != name_key {
        return Err(format!(
            "display_name `{display}` must equal `{name_key}` when lowercased"
        ));
    }
    let resolved_email = match email {
        Some(raw) => {
            let trimmed = raw.trim();
            let Some((local, domain)) = trimmed.split_once('@') else {
                return Err(format!("invalid email `{trimmed}`: missing `@`"));
            };
            if local.is_empty()
                || domain.is_empty()
                || local.chars().any(|c| c.is_ascii_uppercase())
            {
                return Err(format!(
                    "email `{trimmed}` must have a non-empty lowercase local-part before `@`"
                ));
            }
            format!("{local}@{}", domain.to_ascii_lowercase())
        }
        None => format!("{name_key}@artificialhumanity.io"),
    };
    Ok((name_key, display.to_string(), resolved_email))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryEventKind {
    AreaUpsert(AreaRecord),
    AgentUpsert(AgentRecord),
    AgentRetire { id: AgentId },
}

crate::wire::wire_tags!(RegistryEventKind as registry_event_kind_wire {
    RegistryEventKind::AreaUpsert(_) => "area_upsert", RegistryEventKind::AreaUpsert(AreaRecord {
        area_key: "ferroloop".into(),
        name: "FerroLoop".into(),
        path: Some("FerroLoop/".into()),
        purpose: "Agent orchestration".into(),
        updated_at: At::from_unix_millis(1),
        by: "owner".into(),
    });
    RegistryEventKind::AgentUpsert(_) => "agent_upsert", RegistryEventKind::AgentUpsert(AgentRecord {
        id: AgentId(crate::ids::seq_iri(1)),
        name_key: "ferris".into(),
        display_name: "Ferris".into(),
        email: "ferris@artificialhumanity.io".into(),
        project: "ferroloop".into(),
        role: AgentRole::Lead,
        extra_areas: vec![],
        status: AgentStatus::Active,
        updated_at: At::from_unix_millis(1),
        by: "owner".into(),
    });
    RegistryEventKind::AgentRetire { .. } => "agent_retire", RegistryEventKind::AgentRetire {
        id: AgentId(crate::ids::seq_iri(1)),
    };
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryEvent {
    pub event_id: Iri,
    pub at: At,
    pub by: String,
    pub kind: RegistryEventKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::seq_iri;

    #[test]
    fn agent_role_flat_wire_round_trips() {
        for (raw, expected) in [
            ("lead", "lead"),
            ("assistant", "assistant"),
            ("lab_manager", "lab_manager"),
            ("custom:Scribe", "custom:scribe"),
            ("scribe", "custom:scribe"),
        ] {
            let role = AgentRole::from_wire(raw).unwrap();
            assert_eq!(role.as_wire(), expected);
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            let back: AgentRole = serde_json::from_str(&json).unwrap();
            assert_eq!(back, role);
        }
        assert!(AgentRole::from_wire("custom:123").is_err());
        assert!(AgentRole::from_wire("").is_err());
    }

    #[test]
    fn reports_to_derives_owner_for_leads_and_active_lead_for_assistants() {
        let lead = AgentRecord {
            id: AgentId(seq_iri(1)),
            name_key: "kayla".into(),
            display_name: "Kayla".into(),
            email: "kayla@artificialhumanity.io".into(),
            project: "kaggle-gemma4".into(),
            role: AgentRole::Lead,
            extra_areas: vec![],
            status: AgentStatus::Active,
            updated_at: At::from_unix_millis(1),
            by: "owner".into(),
        };
        let scribe = AgentRecord {
            id: AgentId(seq_iri(2)),
            name_key: "kacey".into(),
            display_name: "Kacey".into(),
            email: "kacey@artificialhumanity.io".into(),
            project: "kaggle-gemma4".into(),
            role: AgentRole::Custom("scribe".into()),
            extra_areas: vec![],
            status: AgentStatus::Active,
            updated_at: At::from_unix_millis(2),
            by: "owner".into(),
        };
        let all = vec![lead.clone(), scribe.clone()];
        assert_eq!(reports_to(&lead, &all), "Owner");
        assert_eq!(reports_to(&scribe, &all), "Kayla");
    }
}
```

- [ ] **Step 4: Update `crates/core/src/store.rs`, `crates/core/src/lib.rs`, and `crates/core/tests/wire_refs.rs`**

In `crates/core/src/store.rs`:
1. Import the new types at the top:
```rust
use crate::agent::{
    AgentRecord, AgentStatus, AreaRecord, RegisterAgentArgs, RegistryEvent,
};
use crate::at::At;
use crate::ids::{
    AgentId, FindingId, GateId, Kind, ProjectId, RecordId, RevisionId, StateId,
};
use crate::state::{
    MemoryCategory, StateConcern, StateConflict, StateEntry, StateError, StateRevision,
    StateScope, StateSyncCursor, UpsertStateArgs,
};
```
2. Add the new variants to `StoreError`:
```rust
    #[error(
        "agent `{name_key}` is already registered as {existing_id} (status: {}); \
         confirm with the owner whether you are resuming/replacing that agent before passing `--replace`",
        status.as_wire()
    )]
    AgentCollision {
        name_key: String,
        existing_id: AgentId,
        status: AgentStatus,
    },
    #[error(
        "project `{project}` already has an active lead (`{lead_name}`, {lead_id}); \
         confirm with the owner before passing `--replace`"
    )]
    LeadTaken {
        project: String,
        lead_name: String,
        lead_id: AgentId,
    },
    #[error("no such area `{0}` in registry (pass `--create-area` or run `fl agent area set` first)")]
    NoSuchArea(String),
    #[error("no such agent: {0}")]
    NoSuchAgent(String),
    #[error("no such state entry: {0}")]
    NoSuchState(String),
    #[error(
        "state entry {id} has an unresolved conflict between local revision {local_head} and remote revision {remote_head}; \
         run `fl state resolve {id} --keep <revision-id> --by <author>` first"
    )]
    StateConflict {
        id: StateId,
        local_head: RevisionId,
        remote_head: RevisionId,
    },
    #[error(
        "refused: state entry {id} carries sensitive history and cannot be synced to `{remote}` in `{repo}` \
         because `private = true` is not set for that state target in config.toml"
    )]
    SensitiveDisclosure {
        id: StateId,
        repo: String,
        remote: String,
    },
    #[error(transparent)]
    InvalidState(#[from] StateError),
```
3. Update `KindRouted::owner` in `crates/core/src/store.rs`:
```rust
impl KindRouted<'_> {
    fn owner(&self, kind: Kind) -> &dyn Handles {
        // Every kind by name: a new kind must be routed on purpose.
        match kind {
            Kind::Project | Kind::Gate | Kind::State | Kind::Agent => self.catalog,
            Kind::Record | Kind::Finding => self.tracker,
        }
    }
}
```
4. Add `AgentRegistry` and `StateStore` trait definitions to `crates/core/src/store.rs`:
```rust
/// Centralized agent and coverage-area registry (spec §2, identity spec §6a).
pub trait AgentRegistry {
    fn upsert_area(&self, area: AreaRecord) -> Result<AreaRecord, StoreError>;
    fn get_area(&self, area_key: &str) -> Result<Option<AreaRecord>, StoreError>;
    fn list_areas(&self) -> Result<Vec<AreaRecord>, StoreError>;

    fn register_agent(&self, args: RegisterAgentArgs) -> Result<AgentRecord, StoreError>;
    fn retire_agent(&self, id: &AgentId, at: At, by: &str) -> Result<AgentRecord, StoreError>;
    fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, StoreError>;
    fn agent_by_name(&self, name_key: &str) -> Result<Option<AgentRecord>, StoreError>;
    fn list_agents(
        &self,
        project: Option<&str>,
        include_retired: bool,
    ) -> Result<Vec<AgentRecord>, StoreError>;

    /// Append-only `(sequence, RegistryEvent)` log after `after_seq` for `fl/state` sync.
    fn registry_events_after(&self, after_seq: u64) -> Result<Vec<(u64, RegistryEvent)>, StoreError>;
    /// Import remote `RegistryEvent`s during `fl state sync`, deduplicating by `event_id`
    /// with byte-for-byte tamper verification and resolving offline `name_key` collisions.
    fn import_registry_events(&self, events: &[RegistryEvent]) -> Result<(), StoreError>;
}

/// Durable project/agent memory, handoff snapshots, and workflow runtime state (spec §1, §3).
pub trait StateStore {
    fn upsert_state(&self, args: UpsertStateArgs) -> Result<StateEntry, StoreError>;
    fn retire_memory(
        &self,
        id: &StateId,
        superseded_by: Option<&StateId>,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError>;
    fn get_state(&self, id: &StateId) -> Result<Option<StateEntry>, StoreError>;
    fn get_state_by_key(
        &self,
        scope: &StateScope,
        concern: StateConcern,
        slug: &str,
    ) -> Result<Option<StateEntry>, StoreError>;
    fn list_state(
        &self,
        scope: Option<&StateScope>,
        concern: Option<StateConcern>,
        category: Option<MemoryCategory>,
        include_retired: bool,
    ) -> Result<Vec<StateEntry>, StoreError>;
    fn state_revisions(&self, id: &StateId) -> Result<Vec<StateRevision>, StoreError>;
    fn list_conflicts(&self, scope: Option<&StateScope>) -> Result<Vec<StateConflict>, StoreError>;
    fn resolve_conflict(
        &self,
        id: &StateId,
        keep: &RevisionId,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError>;

    /// Append-only `(sequence, StateRevision)` log after `after_seq` for `fl/state` sync.
    fn state_revisions_after(&self, after_seq: u64) -> Result<Vec<(u64, StateRevision)>, StoreError>;
    /// Import remote `StateRevision`s during `fl state sync`, deduplicating by `RevisionId`
    /// with byte-for-byte tamper verification, aliasing unpublished local same-key collisions
    /// (`S_local -> S_remote`), and computing fast-forward, conflict, `append`, or `last_writer`
    /// merges using `merge_at`.
    fn import_state_revisions(
        &self,
        revisions: &[StateRevision],
        merge_at: At,
    ) -> Result<(), StoreError>;

    fn sync_cursor(&self, cursor_key: &str) -> Result<StateSyncCursor, StoreError>;
    fn set_sync_cursor(&self, cursor_key: &str, cursor: &StateSyncCursor) -> Result<(), StoreError>;
}
```

In `crates/core/src/lib.rs`, export `agent` and `state`:
```rust
pub mod agent;
pub mod state;
```
and re-export:
```rust
pub use agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, RegistryEvent,
    RegistryEventKind, reports_to, validate_agent_identity,
};
pub use ids::{AgentId, FindingId, GateId, Kind, ProjectId, RecordId, RevisionId, StateId};
pub use state::{
    HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
    RuntimeEntry, SensitivityPatch, StateConcern, StateConflict, StateEntry, StateError,
    StatePayload, StateRevision, StateScope, StateSyncCursor, UpsertStateArgs, validate_slug,
    validate_summary,
};
pub use store::{
    AgentRegistry, Bindings, Catalog, CatalogChecked, Handles, KindRouted, Ledger, Roles,
    StateStore, StoreError, Tracker, as_clause, follow, ledger_root_shape, node_id_shape,
};
```

In `crates/core/tests/wire_refs.rs`, append checks for `StateEntry`, `StateRevision`, `AgentRecord`, and `RegistryEvent` at the end of `every_reference_field_on_the_wire_is_a_full_iri`:
```rust
    let state_entry = StateEntry {
        id: StateId(iri(11)),
        scope: StateScope::Project("ferroloop".into()),
        concern: StateConcern::Memory,
        slug: "arch".into(),
        head_revision: RevisionId(iri(12)),
        sensitive: false,
        ever_sensitive: false,
        payload: StatePayload::Memory(MemoryEntry {
            category: MemoryCategory::Decision,
            basis: MemoryBasis::Ruling,
            lifecycle: MemoryLifecycle::Superseded,
            superseded_by: Some(StateId(iri(13))),
            pinned: true,
            summary: "summary".into(),
            body: "body".into(),
        }),
        created_at: At::from_unix_millis(4),
        updated_at: At::from_unix_millis(5),
        updated_by: "ferris".into(),
    };
    let state_revision = StateRevision {
        id: RevisionId(iri(12)),
        entry_id: StateId(iri(11)),
        scope: StateScope::Project("ferroloop".into()),
        concern: StateConcern::Memory,
        slug: "arch".into(),
        parent_revisions: vec![RevisionId(iri(14))],
        sensitive: false,
        payload: state_entry.payload.clone(),
        at: At::from_unix_millis(5),
        by: "ferris".into(),
    };
    let agent_record = AgentRecord {
        id: AgentId(iri(15)),
        name_key: "ferris".into(),
        display_name: "Ferris".into(),
        email: "ferris@artificialhumanity.io".into(),
        project: "ferroloop".into(),
        role: AgentRole::Lead,
        extra_areas: vec![],
        status: AgentStatus::Active,
        updated_at: At::from_unix_millis(6),
        by: "owner".into(),
    };
    let registry_event = RegistryEvent {
        event_id: iri(16),
        at: At::from_unix_millis(6),
        by: "owner".into(),
        kind: RegistryEventKind::AgentUpsert(agent_record.clone()),
    };
    assert_every_reference_refuses_a_handle("StateEntry", &state_entry, 3);
    assert_every_reference_refuses_a_handle("StateRevision", &state_revision, 4);
    assert_every_reference_refuses_a_handle("AgentRecord", &agent_record, 1);
    assert_every_reference_refuses_a_handle("RegistryEvent", &registry_event, 2);
```

- [ ] **Step 5: Run `fl-core` unit and wire tests**

Run:
```bash
cargo test -p fl-core --lib state
cargo test -p fl-core --lib agent
cargo test -p fl-core --test wire_refs
```
Expected: PASS (note that `conformance.rs` will fail `an_id_has_no_handle_under_any_kind_but_its_own` until Task 2 adds `Kind::State` and `Kind::Agent` to `held`).

---
### Task 2: `MemStore` and shared conformance suites for `AgentRegistry` and `StateStore` (`fl-core`)

**Files:**
- Modify: `crates/core/src/state.rs` (add shared DAG ancestry / merge helpers used by both `MemStore` and `RedbStore`)
- Modify: `crates/core/src/mem.rs`
- Modify: `crates/core/src/conformance.rs`

**Interfaces:**
- Consumes: `AgentRegistry`, `StateStore`, `StateEntry`, `StateRevision`, `StateConflict`, `StateSyncCursor`, `AgentRecord`, `AreaRecord`, `RegistryEvent` from Task 1.
- Produces:
  - Shared DAG helpers in `fl_core::state`: `is_ancestor_in_dag`, `lowest_common_ancestor_in_dag`, `compute_append_merge`, `compute_last_writer_merge`, `sorted_parents`.
  - `impl AgentRegistry for MemStore` and `impl StateStore for MemStore`.
  - `conformance::agent_registry` (`AGENT_REGISTRY_CASES = 7`) and `conformance::state_store` (`STATE_STORE_CASES = 10`), plus updated `handles_are_per_kind_and_start_at_one` and `an_id_has_no_handle_under_any_kind_but_its_own` covering all 6 `Kind`s.

- [ ] **Step 1: Add shared DAG ancestry and merge helpers to `crates/core/src/state.rs`**

Append these pure DAG helpers to `crates/core/src/state.rs` (before `#[cfg(test)]`) so `MemStore` and `RedbStore` share the exact same merge logic:

```rust
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Deterministic two-parent list sorted by IRI string (spec §3.3).
pub fn sorted_parents(a: &RevisionId, b: &RevisionId) -> Vec<RevisionId> {
    if a.as_str() <= b.as_str() {
        vec![a.clone(), b.clone()]
    } else {
        vec![b.clone(), a.clone()]
    }
}

/// Returns `true` if `candidate_ancestor` is equal to `start` or reachable via
/// `parent_revisions` in `revs`.
pub fn is_ancestor_in_dag(
    revs: &BTreeMap<RevisionId, StateRevision>,
    candidate_ancestor: &RevisionId,
    start: &RevisionId,
) -> bool {
    if candidate_ancestor == start {
        return true;
    }
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(start.clone());
    visited.insert(start.clone());
    while let Some(cur) = queue.pop_front() {
        if let Some(rev) = revs.get(&cur) {
            for p in &rev.parent_revisions {
                if p == candidate_ancestor {
                    return true;
                }
                if visited.insert(p.clone()) {
                    queue.push_back(p.clone());
                }
            }
        }
    }
    false
}

/// Lowest common ancestor of `a` and `b` in `revs` by BFS distance from `a`.
pub fn lowest_common_ancestor_in_dag(
    revs: &BTreeMap<RevisionId, StateRevision>,
    a: &RevisionId,
    b: &RevisionId,
) -> Option<RevisionId> {
    let mut ancestors_b = BTreeSet::new();
    let mut q = VecDeque::new();
    q.push_back(b.clone());
    ancestors_b.insert(b.clone());
    while let Some(cur) = q.pop_front() {
        if let Some(rev) = revs.get(&cur) {
            for p in &rev.parent_revisions {
                if ancestors_b.insert(p.clone()) {
                    q.push_back(p.clone());
                }
            }
        }
    }
    let mut visited_a = BTreeSet::new();
    let mut qa = VecDeque::new();
    qa.push_back(a.clone());
    visited_a.insert(a.clone());
    while let Some(cur) = qa.pop_front() {
        if ancestors_b.contains(&cur) {
            return Some(cur);
        }
        if let Some(rev) = revs.get(&cur) {
            for p in &rev.parent_revisions {
                if visited_a.insert(p.clone()) {
                    qa.push_back(p.clone());
                }
            }
        }
    }
    None
}

/// Compute `MergeMode::Append` array merge from `local`, `remote`, and optional `lca` (spec §3.3).
pub fn compute_append_merge(
    local: &StateRevision,
    remote: &StateRevision,
    lca: Option<&StateRevision>,
) -> Option<StatePayload> {
    let (
        StatePayload::Runtime(RuntimeEntry {
            value: Value::Array(l_arr),
            merge_mode: MergeMode::Append,
        }),
        StatePayload::Runtime(RuntimeEntry {
            value: Value::Array(r_arr),
            merge_mode: MergeMode::Append,
        }),
    ) = (&local.payload, &remote.payload)
    else {
        return None;
    };
    let base_arr: &[Value] = match lca.map(|a| &a.payload) {
        Some(StatePayload::Runtime(RuntimeEntry {
            value: Value::Array(arr),
            ..
        })) => arr.as_slice(),
        _ => &[],
    };
    let mut merged = l_arr.clone();
    for item in r_arr {
        let in_base = base_arr.contains(item);
        if !in_base && !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    Some(StatePayload::Runtime(RuntimeEntry {
        value: Value::Array(merged),
        merge_mode: MergeMode::Append,
    }))
}

/// Compute `MergeMode::LastWriter` payload from `local` and `remote` by
/// comparing `(revision.at.as_str(), revision.id.as_str())` (spec §3.3).
pub fn compute_last_writer_merge(
    local: &StateRevision,
    remote: &StateRevision,
) -> Option<StatePayload> {
    let (
        StatePayload::Runtime(RuntimeEntry {
            merge_mode: MergeMode::LastWriter,
            ..
        }),
        StatePayload::Runtime(RuntimeEntry {
            merge_mode: MergeMode::LastWriter,
            ..
        }),
    ) = (&local.payload, &remote.payload)
    else {
        return None;
    };
    if (remote.at.as_str(), remote.id.as_str()) >= (local.at.as_str(), local.id.as_str()) {
        Some(remote.payload.clone())
    } else {
        Some(local.payload.clone())
    }
}
```

- [ ] **Step 2: Write the failing conformance tests in `crates/core/src/conformance.rs` and update `Bound` in `crates/github/src/tracker.rs`**

In `crates/core/src/conformance.rs`:
1. Add `pub state: &'a dyn StateStore` and `pub agents: &'a dyn AgentRegistry` to `Bound<'a>`, and update `Single` and `SplitOver` to populate them:

```rust
pub struct Bound<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
    pub ledger: &'a dyn Ledger,
    pub handles: &'a dyn Handles,
    pub state: &'a dyn StateStore,
    pub agents: &'a dyn AgentRegistry,
}

impl<S: Catalog + Tracker + Ledger + Handles + StateStore + AgentRegistry, G> Fixture
    for Single<S, G>
{
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
        f(&Bound {
            catalog: &self.0,
            tracker: &self.0,
            ledger: &self.0,
            handles: &self.0,
            state: &self.0,
            agents: &self.0,
        });
    }
}

impl<S: Catalog + Tracker + Ledger + Handles + StateStore + AgentRegistry, G> Fixture
    for SplitOver<S, G>
{
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
        let checked = CatalogChecked {
            catalog: &self.0,
            tracker: &self.0,
        };
        let handles = KindRouted {
            catalog: &self.0,
            tracker: &self.0,
        };
        f(&Bound {
            catalog: &self.0,
            tracker: &checked,
            ledger: &self.0,
            handles: &handles,
            state: &self.0,
            agents: &self.0,
        });
    }
}
```

2. Update `all_roles` and `local_handles` in `crates/core/src/conformance.rs` to require `S: Catalog + Tracker + Ledger + Handles + StateStore + AgentRegistry`, and update `an_id_has_no_handle_under_any_kind_but_its_own` so `held` includes minted `Kind::State` and `Kind::Agent` items:

```rust
    let st = roles
        .state
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "rule".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "ok".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(1),
            by: "o".into(),
        })
        .unwrap();
    let ag = roles
        .agents
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(1),
            by: "o".into(),
        })
        .unwrap();
    let held = [
        (Kind::Project, p.0),
        (Kind::Gate, g.0),
        (Kind::Record, r.0),
        (Kind::Finding, f.0),
        (Kind::State, st.id.0),
        (Kind::Agent, ag.id.0),
    ];
```

3. Update `crates/github/src/tracker.rs` in `mod contract` (`impl Fixture for Split`, lines 5235–5240) to pass `state: &self.catalog, agents: &self.catalog`:

```rust
                f(&Bound {
                    catalog: &self.catalog,
                    tracker: &checked,
                    ledger: &self.catalog,
                    handles: &handles,
                    state: &self.catalog,
                    agents: &self.catalog,
                });
```

4. Append the `agent_registry` (7 cases) and `state_store` (10 cases) conformance suites to `crates/core/src/conformance.rs`:

```rust
/// How many cases [`agent_registry`] runs. Update deliberately — see [`run_suite`].
const AGENT_REGISTRY_CASES: usize = 7;
/// How many cases [`state_store`] runs. Update deliberately — see [`run_suite`].
const STATE_STORE_CASES: usize = 10;

pub fn agent_registry<S: AgentRegistry + Catalog + Handles, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        area_round_trips_and_normalizes_key::<S>,
        register_agent_requires_existing_area_unless_create_area_is_set::<S>,
        agent_resolves_by_case_insensitive_name_and_numeric_handle::<S>,
        agent_collision_refuses_active_and_retired_without_replace_and_preserves_iri_on_replace::<S>,
        one_lead_per_project_refuses_second_lead_and_retires_previous_on_replace::<S>,
        reports_to_tracks_active_project_lead_or_owner::<S>,
        registry_event_import_deduplicates_detects_tamper_and_aliases_offline_collision::<S>,
    ];
    run_suite("agent-registry", AGENT_REGISTRY_CASES, cases, make);
}

pub fn state_store<S: StateStore + Catalog + Handles, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        memory_handoff_and_runtime_crud_and_key_lookup::<S>,
        sensitive_flag_is_sticky_until_explicitly_cleared_and_ever_sensitive_persists::<S>,
        memory_superseded_validates_target_exists_and_is_memory::<S>,
        runtime_append_requires_array_and_rejects_scalar::<S>,
        scope_concern_and_slug_are_immutable_for_an_entry_id::<S>,
        fast_forward_and_already_ahead_revision_imports_converge::<S>,
        true_divergence_creates_conflict_blocks_writes_and_resolves_cleanly::<S>,
        mismatched_runtime_merge_modes_fail_closed_to_conflict::<S>,
        runtime_append_and_last_writer_auto_merge_without_conflict::<S>,
        offline_same_key_collision_aliases_unpublished_local_id_to_remote::<S>,
    ];
    run_suite("state-store", STATE_STORE_CASES, cases, make);
}

fn area_round_trips_and_normalizes_key<S: AgentRegistry>(s: &S) {
    let a = s
        .upsert_area(crate::agent::AreaRecord {
            area_key: " Ferro_Loop ".into(),
            name: "FerroLoop".into(),
            path: Some("FerroLoop/".into()),
            purpose: "Agent orchestration".into(),
            updated_at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    assert_eq!(a.area_key, "ferro_loop");
    assert_eq!(s.get_area("FERRO_LOOP").unwrap(), Some(a.clone()));
    assert_eq!(s.list_areas().unwrap(), vec![a]);
}

fn register_agent_requires_existing_area_unless_create_area_is_set<S: AgentRegistry>(s: &S) {
    let err = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::NoSuchArea(ref k) if k == "ferroloop"));

    let rec = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec!["notes".into()],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(11),
            by: "owner".into(),
        })
        .unwrap();
    assert_eq!(rec.name_key, "ferris");
    assert_eq!(rec.email, "ferris@artificialhumanity.io");
    assert!(s.get_area("ferroloop").unwrap().is_some());
    assert!(s.get_area("notes").unwrap().is_some());
}

fn agent_resolves_by_case_insensitive_name_and_numeric_handle<S: AgentRegistry + Handles>(s: &S) {
    let rec = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();
    assert_eq!(s.agent_by_name("FERRIS").unwrap(), Some(rec.clone()));
    assert_eq!(s.get_agent(&rec.id).unwrap(), Some(rec.clone()));
    let h = s.handle_of(Kind::Agent, rec.id.iri()).unwrap().unwrap();
    assert_eq!(s.resolve_handle(Kind::Agent, h).unwrap(), Some(rec.id.0));
}

fn agent_collision_refuses_active_and_retired_without_replace_and_preserves_iri_on_replace<
    S: AgentRegistry,
>(
    s: &S,
) {
    let first = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Assistant,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();
    let err = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Assistant,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(11),
            by: "owner".into(),
        })
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::AgentCollision {
            status: crate::agent::AgentStatus::Active,
            ..
        }
    ));

    s.retire_agent(&first.id, At::from_unix_millis(12), "owner")
        .unwrap();
    let err_retired = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Assistant,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(13),
            by: "owner".into(),
        })
        .unwrap_err();
    assert!(matches!(
        err_retired,
        StoreError::AgentCollision {
            status: crate::agent::AgentStatus::Retired,
            ..
        }
    ));

    let resumed = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: false,
            replace: true,
            at: At::from_unix_millis(14),
            by: "owner".into(),
        })
        .unwrap();
    assert_eq!(resumed.id, first.id);
    assert_eq!(resumed.status, crate::agent::AgentStatus::Active);
    assert_eq!(resumed.role, crate::agent::AgentRole::Lead);
}

fn one_lead_per_project_refuses_second_lead_and_retires_previous_on_replace<S: AgentRegistry>(
    s: &S,
) {
    let first = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();
    let err = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Cogs".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(11),
            by: "owner".into(),
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::LeadTaken { .. }));

    let second = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Cogs".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: false,
            replace: true,
            at: At::from_unix_millis(12),
            by: "owner".into(),
        })
        .unwrap();
    assert_eq!(second.status, crate::agent::AgentStatus::Active);
    assert_eq!(
        s.get_agent(&first.id).unwrap().unwrap().status,
        crate::agent::AgentStatus::Retired
    );
}

fn reports_to_tracks_active_project_lead_or_owner<S: AgentRegistry>(s: &S) {
    let lead = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();
    let assistant = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Cogs".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Assistant,
            extra_areas: vec![],
            create_area: false,
            replace: false,
            at: At::from_unix_millis(11),
            by: "ferris".into(),
        })
        .unwrap();
    let active = s.list_agents(None, false).unwrap();
    assert_eq!(crate::agent::reports_to(&lead, &active), "Owner");
    assert_eq!(crate::agent::reports_to(&assistant, &active), "Ferris");
}

fn registry_event_import_deduplicates_detects_tamper_and_aliases_offline_collision<
    S: AgentRegistry + Handles,
>(
    s: &S,
) {
    let local = s
        .register_agent(crate::agent::RegisterAgentArgs {
            display_name: "Ferris".into(),
            email: None,
            project: "ferroloop".into(),
            role: crate::agent::AgentRole::Lead,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(10),
            by: "owner".into(),
        })
        .unwrap();
    let events: Vec<_> = s
        .registry_events_after(0)
        .unwrap()
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    // Deduplicate identical re-import.
    s.import_registry_events(&events).unwrap();

    // Tamper check on identical event_id with altered payload.
    let mut tampered = events.last().unwrap().clone();
    tampered.by = "mallory".into();
    assert!(matches!(
        s.import_registry_events(&[tampered]),
        Err(StoreError::Tampered { .. })
    ));

    // Offline collision: remote event has a different AgentId for "ferris".
    let remote_id = crate::ids::AgentId(crate::ids::seq_iri(999));
    let mut remote_rec = local.clone();
    remote_rec.id = remote_id.clone();
    remote_rec.display_name = "Ferris Remote".into();
    let collision_ev = crate::agent::RegistryEvent {
        event_id: crate::ids::seq_iri(998),
        at: At::from_unix_millis(20),
        by: "owner".into(),
        kind: crate::agent::RegistryEventKind::AgentUpsert(remote_rec),
    };
    s.import_registry_events(&[collision_ev]).unwrap();
    let resolved = s.get_agent(&local.id).unwrap().unwrap();
    assert_eq!(resolved.id, remote_id);
    assert_eq!(resolved.display_name, "Ferris Remote");
}

fn memory_handoff_and_runtime_crud_and_key_lookup<S: StateStore>(s: &S) {
    let mem = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "ste-rule".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Memory(crate::state::MemoryEntry {
                kind: crate::state::MemoryKind::User,
                status: crate::state::MemoryStatus::Active,
                summary: "Use STE for owner prose".into(),
                body: "Body".into(),
                tags: vec!["ste".into()],
                source_refs: vec![],
                superseded_by: None,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    assert_eq!(
        s.state_by_key(
            &crate::state::StateScope::Workspace,
            crate::state::StateConcern::Memory,
            "ste-rule"
        )
        .unwrap(),
        Some(mem.clone())
    );
    assert_eq!(s.get_state(&mem.id).unwrap(), Some(mem));
}

fn sensitive_flag_is_sticky_until_explicitly_cleared_and_ever_sensitive_persists<S: StateStore>(
    s: &S,
) {
    let e1 = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "secret-note".into(),
            sensitivity: crate::state::SensitivityPatch::Set(true),
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "s1".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(10),
            by: "ferris".into(),
        })
        .unwrap();
    assert!(e1.sensitive && e1.ever_sensitive);

    // Inherit preserves sensitive = true.
    let e2 = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "secret-note".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "s2".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(11),
            by: "ferris".into(),
        })
        .unwrap();
    assert!(e2.sensitive && e2.ever_sensitive);

    // Explicit Set(false) clears sensitive, but ever_sensitive stays true!
    let e3 = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "secret-note".into(),
            sensitivity: crate::state::SensitivityPatch::Set(false),
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "s3".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(12),
            by: "ferris".into(),
        })
        .unwrap();
    assert!(!e3.sensitive);
    assert!(e3.ever_sensitive);
}

fn memory_superseded_validates_target_exists_and_is_memory<S: StateStore>(s: &S) {
    let handoff = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "h1".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "h".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    // Superseding by a non-memory entry fails.
    assert!(s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "m1".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Memory(crate::state::MemoryEntry {
                kind: crate::state::MemoryKind::Project,
                status: crate::state::MemoryStatus::Superseded,
                summary: "m".into(),
                body: "b".into(),
                tags: vec![],
                source_refs: vec![],
                superseded_by: Some(handoff.id),
            }),
            at: At::from_unix_millis(11),
            by: "jarvis".into(),
        })
        .is_err());
}

fn runtime_append_requires_array_and_rejects_scalar<S: StateStore>(s: &S) {
    assert!(s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "rt-bad".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                schema: "v1".into(),
                merge_mode: crate::state::MergeMode::Append,
                value: serde_json::json!(42),
            }),
            at: At::from_unix_millis(10),
            by: "ferris".into(),
        })
        .is_err());
}

fn scope_concern_and_slug_are_immutable_for_an_entry_id<S: StateStore>(s: &S) {
    let e = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "fixed".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "v1".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let bad_rev = crate::state::StateRevision {
        id: crate::ids::RevisionId(crate::ids::seq_iri(900)),
        entry_id: e.id,
        parents: vec![e.head_rev],
        scope: crate::state::StateScope::Workspace,
        concern: crate::state::StateConcern::Memory,
        slug: "fixed".into(),
        sensitive: false,
        at: At::from_unix_millis(20),
        by: "jarvis".into(),
        payload: crate::state::StatePayload::Memory(crate::state::MemoryEntry {
            kind: crate::state::MemoryKind::User,
            status: crate::state::MemoryStatus::Active,
            summary: "changed concern".into(),
            body: "b".into(),
            tags: vec![],
            source_refs: vec![],
            superseded_by: None,
        }),
    };
    assert!(matches!(
        s.import_state_revisions(&[bad_rev]),
        Err(StoreError::InvalidState(_))
    ));
}

fn fast_forward_and_already_ahead_revision_imports_converge<S: StateStore>(s: &S) {
    let e = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "ff".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "v1".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let r2_id = crate::ids::RevisionId(crate::ids::seq_iri(801));
    let r2 = crate::state::StateRevision {
        id: r2_id.clone(),
        entry_id: e.id.clone(),
        parents: vec![e.head_rev.clone()],
        scope: crate::state::StateScope::Workspace,
        concern: crate::state::StateConcern::Handoff,
        slug: "ff".into(),
        sensitive: false,
        at: At::from_unix_millis(20),
        by: "jarvis".into(),
        payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
            summary: "v2".into(),
            shipped: vec![],
            in_flight: vec![],
            blockers: vec![],
            next_steps: vec![],
            notes: None,
        }),
    };
    s.import_state_revisions(&[r2]).unwrap();
    let after_ff = s.get_state(&e.id).unwrap().unwrap();
    assert_eq!(after_ff.head_rev, r2_id);
    assert!(s.list_conflicts(None).unwrap().is_empty());
}

fn true_divergence_creates_conflict_blocks_writes_and_resolves_cleanly<S: StateStore>(s: &S) {
    let e = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "div".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "base".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let local_head = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "div".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "local".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(11),
            by: "jarvis".into(),
        })
        .unwrap();
    let remote_rev_id = crate::ids::RevisionId(crate::ids::seq_iri(810));
    let remote_rev = crate::state::StateRevision {
        id: remote_rev_id.clone(),
        entry_id: e.id.clone(),
        parents: vec![e.head_rev.clone()],
        scope: crate::state::StateScope::Workspace,
        concern: crate::state::StateConcern::Handoff,
        slug: "div".into(),
        sensitive: false,
        at: At::from_unix_millis(12),
        by: "ferris".into(),
        payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
            summary: "remote".into(),
            shipped: vec![],
            in_flight: vec![],
            blockers: vec![],
            next_steps: vec![],
            notes: None,
        }),
    };
    s.import_state_revisions(&[remote_rev]).unwrap();
    let conflicts = s.list_conflicts(None).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].base_rev, Some(e.head_rev));

    // Normal writes are blocked while conflicted.
    assert!(matches!(
        s.upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "div".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: local_head.payload.clone(),
            at: At::from_unix_millis(13),
            by: "jarvis".into(),
        }),
        Err(StoreError::StateConflict(_))
    ));

    let resolved = s
        .resolve_conflict(
            &e.id,
            local_head.payload,
            crate::state::SensitivityPatch::Inherit,
            At::from_unix_millis(14),
            "jarvis",
        )
        .unwrap();
    assert!(s.list_conflicts(None).unwrap().is_empty());
    let head_rev = s.get_revision(&resolved.head_rev).unwrap().unwrap();
    assert_eq!(head_rev.parents, vec![local_head.head_rev, remote_rev_id]);
}

fn mismatched_runtime_merge_modes_fail_closed_to_conflict<S: StateStore>(s: &S) {
    let base = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "rt-mode".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                schema: "v1".into(),
                merge_mode: crate::state::MergeMode::Append,
                value: serde_json::json!(["a"]),
            }),
            at: At::from_unix_millis(10),
            by: "ferris".into(),
        })
        .unwrap();
    let _local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "rt-mode".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                schema: "v1".into(),
                merge_mode: crate::state::MergeMode::Append,
                value: serde_json::json!(["a", "b"]),
            }),
            at: At::from_unix_millis(11),
            by: "ferris".into(),
        })
        .unwrap();
    let remote = crate::state::StateRevision {
        id: crate::ids::RevisionId(crate::ids::seq_iri(820)),
        entry_id: base.id.clone(),
        parents: vec![base.head_rev],
        scope: crate::state::StateScope::Project("ferroloop".into()),
        concern: crate::state::StateConcern::Runtime,
        slug: "rt-mode".into(),
        sensitive: false,
        at: At::from_unix_millis(12),
        by: "cogs".into(),
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            schema: "v1".into(),
            merge_mode: crate::state::MergeMode::LastWriter,
            value: serde_json::json!(["a", "c"]),
        }),
    };
    s.import_state_revisions(&[remote]).unwrap();
    assert_eq!(s.list_conflicts(None).unwrap().len(), 1);
}

fn runtime_append_and_last_writer_auto_merge_without_conflict<S: StateStore>(s: &S) {
    let base = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "rt-append".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                schema: "v1".into(),
                merge_mode: crate::state::MergeMode::Append,
                value: serde_json::json!([1]),
            }),
            at: At::from_unix_millis(10),
            by: "ferris".into(),
        })
        .unwrap();
    let _local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "rt-append".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                schema: "v1".into(),
                merge_mode: crate::state::MergeMode::Append,
                value: serde_json::json!([1, 2]),
            }),
            at: At::from_unix_millis(11),
            by: "ferris".into(),
        })
        .unwrap();
    let remote = crate::state::StateRevision {
        id: crate::ids::RevisionId(crate::ids::seq_iri(830)),
        entry_id: base.id.clone(),
        parents: vec![base.head_rev],
        scope: crate::state::StateScope::Project("ferroloop".into()),
        concern: crate::state::StateConcern::Runtime,
        slug: "rt-append".into(),
        sensitive: false,
        at: At::from_unix_millis(12),
        by: "cogs".into(),
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            schema: "v1".into(),
            merge_mode: crate::state::MergeMode::Append,
            value: serde_json::json!([1, 3]),
        }),
    };
    s.import_state_revisions(&[remote]).unwrap();
    assert!(s.list_conflicts(None).unwrap().is_empty());
    let merged = s.get_state(&base.id).unwrap().unwrap();
    assert_eq!(
        merged.payload,
        crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            schema: "v1".into(),
            merge_mode: crate::state::MergeMode::Append,
            value: serde_json::json!([1, 2, 3]),
        })
    );
}

fn offline_same_key_collision_aliases_unpublished_local_id_to_remote<S: StateStore>(s: &S) {
    let local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "offline-key".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "local".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let remote_entry_id = crate::ids::StateId(crate::ids::seq_iri(850));
    let remote_rev_id = crate::ids::RevisionId(crate::ids::seq_iri(851));
    let remote_rev = crate::state::StateRevision {
        id: remote_rev_id,
        entry_id: remote_entry_id.clone(),
        parents: vec![],
        scope: crate::state::StateScope::Workspace,
        concern: crate::state::StateConcern::Handoff,
        slug: "offline-key".into(),
        sensitive: false,
        at: At::from_unix_millis(12),
        by: "ferris".into(),
        payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
            summary: "remote".into(),
            shipped: vec![],
            in_flight: vec![],
            blockers: vec![],
            next_steps: vec![],
            notes: None,
        }),
    };
    s.import_state_revisions(&[remote_rev]).unwrap();
    // Local unpublished StateId resolves as an alias to remote_entry_id!
    let via_old = s.get_state(&local.id).unwrap().unwrap();
    assert_eq!(via_old.id, remote_entry_id);
    // And because both had root revisions for the same key, a root-less conflict is recorded.
    let conflicts = s.list_conflicts(None).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].entry_id, remote_entry_id);
    assert_eq!(conflicts[0].base_rev, None);
}
```

- [ ] **Step 3: Implement `AgentRegistry` and `StateStore` on `MemStore` in `crates/core/src/mem.rs`**

In `crates/core/src/mem.rs`, add the state and agent maps to `Inner` and helper methods `mint_unindexed_iri`, `index_imported`, and `alias_unpublished_id`:

```rust
    state_entries: BTreeMap<Iri, StateEntry>,
    state_keys: BTreeMap<(String, String, String), Iri>,
    next_rev_seq: u64,
    state_revisions: BTreeMap<u64, StateRevision>,
    state_revision_ids: BTreeMap<Iri, u64>,
    state_conflicts: BTreeMap<Iri, StateConflict>,
    state_sync_cursors: BTreeMap<String, StateSyncCursor>,

    agents: BTreeMap<Iri, AgentRecord>,
    agent_by_name: BTreeMap<String, Iri>,
    areas: BTreeMap<String, AreaRecord>,
    next_agent_event_seq: u64,
    agent_events: BTreeMap<u64, RegistryEvent>,
    agent_event_ids: BTreeMap<Iri, u64>,
```

Add helper methods on `Inner` in `crates/core/src/mem.rs`:

```rust
    fn mint_unindexed_iri(&mut self) -> Iri {
        self.next_id += 1;
        seq_iri(self.next_id)
    }

    fn index_imported(&mut self, id: &Iri, kind: Kind) -> Result<(), StoreError> {
        if let Some(&existing) = self.owned.get(id) {
            if existing != kind {
                return Err(StoreError::WrongKind {
                    id: id.clone(),
                    expected: kind,
                    actual: existing,
                });
            }
            return Ok(());
        }
        self.owned.insert(id.clone(), kind);
        let h = self.next_handle.entry(kind).or_insert(0);
        *h += 1;
        let handle = *h;
        self.handles.insert((kind, handle), id.clone());
        self.handle_of.insert((kind, id.clone()), handle);
        Ok(())
    }

    fn alias_unpublished_id(
        &mut self,
        local_id: &Iri,
        remote_id: &Iri,
        kind: Kind,
    ) -> Result<(), StoreError> {
        self.index_imported(remote_id, kind)?;
        self.owned.remove(local_id);
        self.aliases.insert(local_id.clone(), remote_id.clone());
        Ok(())
    }

    fn append_revision(&mut self, rev: StateRevision) -> u64 {
        self.next_rev_seq += 1;
        let seq = self.next_rev_seq;
        self.state_revision_ids.insert(rev.id.0.clone(), seq);
        self.state_revisions.insert(seq, rev);
        seq
    }
```

Add `impl AgentRegistry for MemStore` and `impl StateStore for MemStore` in `crates/core/src/mem.rs`:

```rust
impl AgentRegistry for MemStore {
    fn upsert_area(&self, mut area: AreaRecord) -> Result<AreaRecord, StoreError> {
        area.area_key = validate_slug(&area.area_key)?;
        area.purpose = validate_summary(&area.purpose)?;
        let mut s = self.inner.borrow_mut();
        s.areas.insert(area.area_key.clone(), area.clone());
        let event_id = s.mint_unindexed_iri();
        s.next_agent_event_seq += 1;
        let seq = s.next_agent_event_seq;
        let ev = RegistryEvent {
            event_id: event_id.clone(),
            at: area.updated_at.clone(),
            by: area.by.clone(),
            kind: RegistryEventKind::AreaUpsert(area.clone()),
        };
        s.agent_events.insert(seq, ev);
        s.agent_event_ids.insert(event_id, seq);
        Ok(area)
    }

    fn get_area(&self, area_key: &str) -> Result<Option<AreaRecord>, StoreError> {
        let key = validate_slug(area_key)?;
        Ok(self.inner.borrow().areas.get(&key).cloned())
    }

    fn list_areas(&self) -> Result<Vec<AreaRecord>, StoreError> {
        Ok(self.inner.borrow().areas.values().cloned().collect())
    }

    fn register_agent(&self, args: RegisterAgentArgs) -> Result<AgentRecord, StoreError> {
        let (name_key, display_name, email) =
            validate_agent_identity(&args.display_name, args.email.as_deref())
                .map_err(StoreError::Backend)?;
        let project_key = validate_slug(&args.project)?;
        let mut extra_keys = Vec::with_capacity(args.extra_areas.len());
        for extra in &args.extra_areas {
            extra_keys.push(validate_slug(extra)?);
        }

        let mut s = self.inner.borrow_mut();
        for (raw_name, key) in std::iter::once((&args.project, &project_key))
            .chain(args.extra_areas.iter().zip(extra_keys.iter()))
        {
            if !s.areas.contains_key(key) {
                if args.create_area {
                    let area = AreaRecord {
                        area_key: key.clone(),
                        name: raw_name.trim().to_string(),
                        path: None,
                        purpose: format!("Registered area {}", raw_name.trim()),
                        updated_at: args.at.clone(),
                        by: args.by.clone(),
                    };
                    s.areas.insert(key.clone(), area.clone());
                    let event_id = s.mint_unindexed_iri();
                    s.next_agent_event_seq += 1;
                    let seq = s.next_agent_event_seq;
                    s.agent_events.insert(
                        seq,
                        RegistryEvent {
                            event_id: event_id.clone(),
                            at: args.at.clone(),
                            by: args.by.clone(),
                            kind: RegistryEventKind::AreaUpsert(area),
                        },
                    );
                    s.agent_event_ids.insert(event_id, seq);
                } else {
                    return Err(StoreError::NoSuchArea(key.clone()));
                }
            }
        }

        let existing_id = s.agent_by_name.get(&name_key).cloned();
        if let Some(ref ex_iri) = existing_id
            && !args.replace
        {
            let ex = &s.agents[ex_iri];
            return Err(StoreError::AgentCollision {
                name_key,
                existing_id: ex.id.clone(),
                status: ex.status,
            });
        }

        if args.role == AgentRole::Lead {
            let other_lead = s
                .agents
                .values()
                .find(|a| {
                    a.status == AgentStatus::Active
                        && a.project == project_key
                        && a.role == AgentRole::Lead
                        && Some(&a.id.0) != existing_id.as_ref()
                })
                .cloned();
            if let Some(lead) = other_lead {
                if !args.replace {
                    return Err(StoreError::LeadTaken {
                        project: project_key,
                        lead_name: lead.display_name,
                        lead_id: lead.id,
                    });
                }
                let mut retired_lead = lead.clone();
                retired_lead.status = AgentStatus::Retired;
                retired_lead.updated_at = args.at.clone();
                retired_lead.by = args.by.clone();
                s.agents.insert(lead.id.0.clone(), retired_lead);
                let event_id = s.mint_unindexed_iri();
                s.next_agent_event_seq += 1;
                let seq = s.next_agent_event_seq;
                s.agent_events.insert(
                    seq,
                    RegistryEvent {
                        event_id: event_id.clone(),
                        at: args.at.clone(),
                        by: args.by.clone(),
                        kind: RegistryEventKind::AgentRetire { id: lead.id },
                    },
                );
                s.agent_event_ids.insert(event_id, seq);
            }
        }

        let agent_iri = match existing_id {
            Some(iri) => iri,
            None => s.mint(Kind::Agent),
        };
        let record = AgentRecord {
            id: AgentId(agent_iri.clone()),
            name_key: name_key.clone(),
            display_name,
            email,
            project: project_key,
            role: args.role,
            extra_areas: extra_keys,
            status: AgentStatus::Active,
            updated_at: args.at.clone(),
            by: args.by.clone(),
        };
        s.agents.insert(agent_iri, record.clone());
        s.agent_by_name.insert(name_key, record.id.0.clone());

        let event_id = s.mint_unindexed_iri();
        s.next_agent_event_seq += 1;
        let seq = s.next_agent_event_seq;
        s.agent_events.insert(
            seq,
            RegistryEvent {
                event_id: event_id.clone(),
                at: args.at,
                by: args.by,
                kind: RegistryEventKind::AgentUpsert(record.clone()),
            },
        );
        s.agent_event_ids.insert(event_id, seq);
        Ok(record)
    }

    fn retire_agent(&self, id: &AgentId, at: At, by: &str) -> Result<AgentRecord, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&id.0, Kind::Agent)?;
        let primary = s.resolve(&id.0);
        let Some(mut rec) = s.agents.get(&primary).cloned() else {
            return Err(StoreError::NoSuchAgent(id.to_string()));
        };
        rec.status = AgentStatus::Retired;
        rec.updated_at = at.clone();
        rec.by = by.to_string();
        s.agents.insert(primary, rec.clone());

        let event_id = s.mint_unindexed_iri();
        s.next_agent_event_seq += 1;
        let seq = s.next_agent_event_seq;
        s.agent_events.insert(
            seq,
            RegistryEvent {
                event_id: event_id.clone(),
                at,
                by: by.to_string(),
                kind: RegistryEventKind::AgentRetire { id: rec.id.clone() },
            },
        );
        s.agent_event_ids.insert(event_id, seq);
        Ok(rec)
    }

    fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, StoreError> {
        let s = self.inner.borrow();
        let kind = s.check(&id.0)?;
        if kind != Kind::Agent {
            return Ok(None);
        }
        let primary = s.resolve(&id.0);
        Ok(s.agents.get(&primary).cloned())
    }

    fn agent_by_name(&self, name_key: &str) -> Result<Option<AgentRecord>, StoreError> {
        let key = validate_slug(name_key)?;
        let s = self.inner.borrow();
        let Some(id) = s.agent_by_name.get(&key) else {
            return Ok(None);
        };
        let primary = s.resolve(id);
        Ok(s.agents.get(&primary).cloned())
    }

    fn list_agents(
        &self,
        project: Option<&str>,
        include_retired: bool,
    ) -> Result<Vec<AgentRecord>, StoreError> {
        let proj_filter = match project {
            Some(p) => Some(validate_slug(p)?),
            None => None,
        };
        let s = self.inner.borrow();
        let mut out: Vec<AgentRecord> = s
            .agents
            .values()
            .filter(|a| include_retired || a.status == AgentStatus::Active)
            .filter(|a| {
                proj_filter
                    .as_ref()
                    .is_none_or(|p| &a.project == p || a.extra_areas.contains(p))
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name_key.cmp(&b.name_key));
        Ok(out)
    }

    fn registry_events_after(&self, after_seq: u64) -> Result<Vec<(u64, RegistryEvent)>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.agent_events
            .range((after_seq + 1)..)
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }

    fn import_registry_events(&self, events: &[RegistryEvent]) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        for ev in events {
            if let Some(&seq) = s.agent_event_ids.get(&ev.event_id) {
                let stored = &s.agent_events[&seq];
                if serde_json::to_string(stored).unwrap() != serde_json::to_string(ev).unwrap() {
                    return Err(StoreError::Tampered {
                        id: ev.event_id.clone(),
                        detail: "registry event bytes differ from stored event".into(),
                    });
                }
                continue;
            }
            match &ev.kind {
                RegistryEventKind::AreaUpsert(area) => {
                    s.areas.insert(area.area_key.clone(), area.clone());
                }
                RegistryEventKind::AgentUpsert(rec) => {
                    if let Some(local_id) = s.agent_by_name.get(&rec.name_key).cloned()
                        && local_id != rec.id.0
                    {
                        s.agents.remove(&local_id);
                        s.alias_unpublished_id(&local_id, &rec.id.0, Kind::Agent)?;
                        for existing_ev in s.agent_events.values_mut() {
                            match &mut existing_ev.kind {
                                RegistryEventKind::AgentUpsert(a) if a.id.0 == local_id => {
                                    a.id = rec.id.clone();
                                }
                                RegistryEventKind::AgentRetire { id } if id.0 == local_id => {
                                    *id = rec.id.clone();
                                }
                                _ => {}
                            }
                        }
                    } else {
                        s.index_imported(&rec.id.0, Kind::Agent)?;
                    }
                    if rec.role == AgentRole::Lead && rec.status == AgentStatus::Active {
                        for other in s.agents.values_mut() {
                            if other.id != rec.id
                                && other.project == rec.project
                                && other.role == AgentRole::Lead
                                && other.status == AgentStatus::Active
                            {
                                other.status = AgentStatus::Retired;
                            }
                        }
                    }
                    s.agents.insert(rec.id.0.clone(), rec.clone());
                    s.agent_by_name.insert(rec.name_key.clone(), rec.id.0.clone());
                }
                RegistryEventKind::AgentRetire { id } => {
                    let primary = s.resolve(&id.0);
                    if let Some(rec) = s.agents.get_mut(&primary) {
                        rec.status = AgentStatus::Retired;
                        rec.updated_at = ev.at.clone();
                        rec.by = ev.by.clone();
                    }
                }
            }
            s.next_agent_event_seq += 1;
            let seq = s.next_agent_event_seq;
            s.agent_events.insert(seq, ev.clone());
            s.agent_event_ids.insert(ev.event_id.clone(), seq);
        }
        Ok(())
    }
}

impl StateStore for MemStore {
    fn upsert_state(&self, args: UpsertStateArgs) -> Result<StateEntry, StoreError> {
        let scope = args.scope.validated()?;
        let slug = validate_slug(&args.slug)?;
        let mut payload = args.payload;
        validate_payload(&mut payload)?;
        let concern = payload.concern();

        let mut s = self.inner.borrow_mut();
        if let StatePayload::Memory(ref mem) = payload
            && let Some(ref sup) = mem.superseded_by
        {
            s.check_kind(&sup.0, Kind::State)?;
            let sup_primary = s.resolve(&sup.0);
            let Some(target) = s.state_entries.get(&sup_primary) else {
                return Err(StoreError::InvalidState(format!(
                    "superseded_by target `{sup}` does not exist"
                )));
            };
            if target.concern != StateConcern::Memory {
                return Err(StoreError::InvalidState(format!(
                    "superseded_by target `{sup}` is not a memory entry"
                )));
            }
        }

        let key_tuple = (
            scope.as_key_str(),
            concern.as_wire().to_string(),
            slug.clone(),
        );
        let existing_id = s.state_keys.get(&key_tuple).cloned();
        if let Some(ref ex_iri) = existing_id {
            if s.state_conflicts.contains_key(ex_iri) {
                return Err(StoreError::StateConflict(StateId(ex_iri.clone())));
            }
            if let StatePayload::Memory(ref mem) = payload
                && mem.superseded_by.as_ref().is_some_and(|sup| &sup.0 == ex_iri)
            {
                return Err(StoreError::InvalidState(
                    "memory entry cannot supersede itself".into(),
                ));
            }
        }

        let (entry_iri, parents, sensitive, ever_sensitive) = match existing_id {
            Some(iri) => {
                let prev = &s.state_entries[&iri];
                let sens = match args.sensitivity {
                    SensitivityPatch::Inherit => prev.sensitive,
                    SensitivityPatch::Set(v) => v,
                };
                (
                    iri,
                    vec![prev.head_rev.clone()],
                    sens,
                    prev.ever_sensitive || sens,
                )
            }
            None => {
                let iri = s.mint(Kind::State);
                let sens = match args.sensitivity {
                    SensitivityPatch::Inherit => false,
                    SensitivityPatch::Set(v) => v,
                };
                (iri, vec![], sens, sens)
            }
        };

        let rev_id = RevisionId(s.mint_unindexed_iri());
        let rev = StateRevision {
            id: rev_id.clone(),
            entry_id: StateId(entry_iri.clone()),
            parents,
            scope: scope.clone(),
            concern,
            slug: slug.clone(),
            sensitive,
            at: args.at.clone(),
            by: args.by.clone(),
            payload: payload.clone(),
        };
        s.append_revision(rev);

        let entry = StateEntry {
            id: StateId(entry_iri.clone()),
            scope,
            concern,
            slug,
            sensitive,
            ever_sensitive,
            head_rev: rev_id,
            updated_at: args.at,
            by: args.by,
            payload,
        };
        s.state_keys.insert(key_tuple, entry_iri.clone());
        s.state_entries.insert(entry_iri, entry.clone());
        Ok(entry)
    }

    fn get_state(&self, id: &StateId) -> Result<Option<StateEntry>, StoreError> {
        let s = self.inner.borrow();
        let kind = s.check(&id.0)?;
        if kind != Kind::State {
            return Ok(None);
        }
        let primary = s.resolve(&id.0);
        Ok(s.state_entries.get(&primary).cloned())
    }

    fn state_by_key(
        &self,
        scope: &StateScope,
        concern: StateConcern,
        slug: &str,
    ) -> Result<Option<StateEntry>, StoreError> {
        let scope = scope.validated()?;
        let slug = validate_slug(slug)?;
        let key_tuple = (
            scope.as_key_str(),
            concern.as_wire().to_string(),
            slug,
        );
        let s = self.inner.borrow();
        let Some(iri) = s.state_keys.get(&key_tuple) else {
            return Ok(None);
        };
        let primary = s.resolve(iri);
        Ok(s.state_entries.get(&primary).cloned())
    }

    fn list_state(&self, filter: &StateFilter) -> Result<Vec<StateEntry>, StoreError> {
        let scope_filter = match &filter.scope {
            Some(sc) => Some(sc.validated()?),
            None => None,
        };
        let s = self.inner.borrow();
        let mut out: Vec<StateEntry> = s
            .state_entries
            .values()
            .filter(|e| scope_filter.as_ref().is_none_or(|sc| &e.scope == sc))
            .filter(|e| filter.concern.is_none_or(|c| e.concern == c))
            .filter(|e| {
                if filter.include_archived {
                    return true;
                }
                match &e.payload {
                    StatePayload::Memory(m) => m.status == crate::state::MemoryStatus::Active,
                    _ => true,
                }
            })
            .filter(|e| {
                filter.tag.as_ref().is_none_or(|t| match &e.payload {
                    StatePayload::Memory(m) => m.tags.contains(t),
                    _ => false,
                })
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            (
                a.scope.as_key_str(),
                a.concern.as_wire(),
                a.slug.as_str(),
            )
                .cmp(&(
                    b.scope.as_key_str(),
                    b.concern.as_wire(),
                    b.slug.as_str(),
                ))
        });
        Ok(out)
    }

    fn get_revision(&self, id: &RevisionId) -> Result<Option<StateRevision>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.state_revision_ids
            .get(&id.0)
            .and_then(|seq| s.state_revisions.get(seq))
            .cloned())
    }

    fn revisions_for(&self, entry_id: &StateId) -> Result<Vec<StateRevision>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&entry_id.0, Kind::State)?;
        let primary = s.resolve(&entry_id.0);
        Ok(s.state_revisions
            .values()
            .filter(|r| r.entry_id.0 == primary)
            .cloned()
            .collect())
    }

    fn revisions_after(&self, after_seq: u64) -> Result<Vec<(u64, StateRevision)>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.state_revisions
            .range((after_seq + 1)..)
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }

    fn import_state_revisions(&self, revisions: &[StateRevision]) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        for incoming in revisions {
            let scope = incoming.scope.validated()?;
            let slug = validate_slug(&incoming.slug)?;
            let mut payload = incoming.payload.clone();
            validate_payload(&mut payload)?;
            if payload.concern() != incoming.concern {
                return Err(StoreError::InvalidState(
                    "revision concern does not match payload variant".into(),
                ));
            }

            if let Some(&seq) = s.state_revision_ids.get(&incoming.id.0) {
                let stored = &s.state_revisions[&seq];
                if serde_json::to_string(stored).unwrap() != serde_json::to_string(incoming).unwrap()
                {
                    return Err(StoreError::Tampered {
                        id: incoming.id.0.clone(),
                        detail: "state revision bytes differ from stored revision".into(),
                    });
                }
                continue;
            }

            let key_tuple = (
                scope.as_key_str(),
                incoming.concern.as_wire().to_string(),
                slug.clone(),
            );
            let mut rev = incoming.clone();
            rev.scope = scope.clone();
            rev.slug = slug.clone();
            rev.payload = payload;

            // Check offline same-key collision (`S_local != S_remote`).
            if let Some(existing_key_iri) = s.state_keys.get(&key_tuple).cloned()
                && existing_key_iri != rev.entry_id.0
            {
                let remote_iri = rev.entry_id.0.clone();
                s.alias_unpublished_id(&existing_key_iri, &remote_iri, Kind::State)?;
                if let Some(mut local_entry) = s.state_entries.remove(&existing_key_iri) {
                    local_entry.id = StateId(remote_iri.clone());
                    s.state_entries.insert(remote_iri.clone(), local_entry);
                }
                s.state_keys.insert(key_tuple.clone(), remote_iri.clone());
                for existing_rev in s.state_revisions.values_mut() {
                    if existing_rev.entry_id.0 == existing_key_iri {
                        existing_rev.entry_id = StateId(remote_iri.clone());
                    }
                }
                if let Some(mut c) = s.state_conflicts.remove(&existing_key_iri) {
                    c.entry_id = StateId(remote_iri.clone());
                    s.state_conflicts.insert(remote_iri, c);
                }
            } else {
                s.index_imported(&rev.entry_id.0, Kind::State)?;
            }

            let entry_iri = s.resolve(&rev.entry_id.0);
            rev.entry_id = StateId(entry_iri.clone());

            if let Some(existing) = s.state_entries.get(&entry_iri)
                && (existing.scope != rev.scope
                    || existing.concern != rev.concern
                    || existing.slug != rev.slug)
            {
                return Err(StoreError::InvalidState(format!(
                    "revision `{}` mutates immutable (scope, concern, slug) of `{}`",
                    rev.id, existing.id
                )));
            }

            s.append_revision(rev.clone());

            let Some(current) = s.state_entries.get(&entry_iri).cloned() else {
                let entry = StateEntry {
                    id: StateId(entry_iri.clone()),
                    scope: rev.scope.clone(),
                    concern: rev.concern,
                    slug: rev.slug.clone(),
                    sensitive: rev.sensitive,
                    ever_sensitive: rev.sensitive,
                    head_rev: rev.id.clone(),
                    updated_at: rev.at.clone(),
                    by: rev.by.clone(),
                    payload: rev.payload.clone(),
                };
                s.state_keys.insert(key_tuple, entry_iri.clone());
                s.state_entries.insert(entry_iri, entry);
                continue;
            };

            let all_revs: BTreeMap<RevisionId, StateRevision> = s
                .state_revisions
                .values()
                .filter(|r| r.entry_id.0 == entry_iri)
                .map(|r| (r.id.clone(), r.clone()))
                .collect();

            if crate::state::is_ancestor(&current.head_rev, &rev.id, &all_revs) {
                // Case 1: fast-forward.
                let updated = StateEntry {
                    id: current.id,
                    scope: current.scope,
                    concern: current.concern,
                    slug: current.slug,
                    sensitive: rev.sensitive,
                    ever_sensitive: current.ever_sensitive || rev.sensitive,
                    head_rev: rev.id,
                    updated_at: rev.at,
                    by: rev.by,
                    payload: rev.payload,
                };
                s.state_entries.insert(entry_iri, updated);
            } else if crate::state::is_ancestor(&rev.id, &current.head_rev, &all_revs) {
                // Case 2: already ahead — sticky ever_sensitive still latches if rev.sensitive.
                if rev.sensitive && !current.ever_sensitive {
                    let mut updated = current;
                    updated.ever_sensitive = true;
                    s.state_entries.insert(entry_iri, updated);
                }
            } else {
                // Case 3: true divergence.
                let base_rev =
                    crate::state::lowest_common_ancestor(&current.head_rev, &rev.id, &all_revs);
                let local_head_rev = all_revs.get(&current.head_rev).cloned().unwrap();
                let base_state = base_rev.as_ref().and_then(|b| all_revs.get(b));

                let auto_merged = crate::state::try_merge_runtime_append(
                    base_state,
                    &local_head_rev,
                    &rev,
                )
                .or_else(|| crate::state::try_merge_runtime_last_writer(&local_head_rev, &rev));

                if let Some(merged_payload) = auto_merged {
                    let merge_rev_id = RevisionId(s.mint_unindexed_iri());
                    let merged_sens = current.sensitive || rev.sensitive;
                    let merged_at = if rev.at.as_str() >= current.updated_at.as_str() {
                        rev.at.clone()
                    } else {
                        current.updated_at.clone()
                    };
                    let merge_rev = StateRevision {
                        id: merge_rev_id.clone(),
                        entry_id: current.id.clone(),
                        parents: vec![current.head_rev.clone(), rev.id.clone()],
                        scope: current.scope.clone(),
                        concern: current.concern,
                        slug: current.slug.clone(),
                        sensitive: merged_sens,
                        at: merged_at.clone(),
                        by: rev.by.clone(),
                        payload: merged_payload.clone(),
                    };
                    s.append_revision(merge_rev);
                    let updated = StateEntry {
                        id: current.id,
                        scope: current.scope,
                        concern: current.concern,
                        slug: current.slug,
                        sensitive: merged_sens,
                        ever_sensitive: current.ever_sensitive || merged_sens,
                        head_rev: merge_rev_id,
                        updated_at: merged_at,
                        by: rev.by,
                        payload: merged_payload,
                    };
                    s.state_entries.insert(entry_iri, updated);
                } else {
                    let conflict = StateConflict {
                        entry_id: current.id.clone(),
                        base_rev,
                        local_rev: current.head_rev.clone(),
                        remote_rev: rev.id.clone(),
                        detected_at: rev.at.clone(),
                    };
                    s.state_conflicts.insert(entry_iri.clone(), conflict);
                    if rev.sensitive && !current.ever_sensitive {
                        let mut updated = current;
                        updated.ever_sensitive = true;
                        s.state_entries.insert(entry_iri, updated);
                    }
                }
            }
        }
        Ok(())
    }

    fn list_conflicts(
        &self,
        scope: Option<&StateScope>,
    ) -> Result<Vec<StateConflict>, StoreError> {
        let scope_filter = match scope {
            Some(sc) => Some(sc.validated()?),
            None => None,
        };
        let s = self.inner.borrow();
        let mut out: Vec<StateConflict> = s
            .state_conflicts
            .values()
            .filter(|c| {
                scope_filter.as_ref().is_none_or(|sc| {
                    s.state_entries
                        .get(&c.entry_id.0)
                        .is_some_and(|e| &e.scope == sc)
                })
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.entry_id.cmp(&b.entry_id));
        Ok(out)
    }

    fn resolve_conflict(
        &self,
        entry_id: &StateId,
        mut resolved: StatePayload,
        sensitivity: SensitivityPatch,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError> {
        validate_payload(&mut resolved)?;
        let mut s = self.inner.borrow_mut();
        s.check_kind(&entry_id.0, Kind::State)?;
        let primary = s.resolve(&entry_id.0);
        let Some(conflict) = s.state_conflicts.remove(&primary) else {
            return Err(StoreError::InvalidState(format!(
                "state entry `{entry_id}` is not in `state_conflicts`"
            )));
        };
        let prev = s
            .state_entries
            .get(&primary)
            .cloned()
            .ok_or_else(|| StoreError::NoSuchState(entry_id.to_string()))?;
        if resolved.concern() != prev.concern {
            return Err(StoreError::InvalidState(
                "resolved payload concern does not match entry concern".into(),
            ));
        }
        let remote_sens = s
            .state_revision_ids
            .get(&conflict.remote_rev.0)
            .and_then(|seq| s.state_revisions.get(seq))
            .is_some_and(|r| r.sensitive);
        let sens = match sensitivity {
            SensitivityPatch::Inherit => prev.sensitive || remote_sens,
            SensitivityPatch::Set(v) => v,
        };
        let ever_sensitive = prev.ever_sensitive || remote_sens || sens;
        let rev_id = RevisionId(s.mint_unindexed_iri());
        let rev = StateRevision {
            id: rev_id.clone(),
            entry_id: prev.id.clone(),
            parents: vec![conflict.local_rev, conflict.remote_rev],
            scope: prev.scope.clone(),
            concern: prev.concern,
            slug: prev.slug.clone(),
            sensitive: sens,
            at: at.clone(),
            by: by.to_string(),
            payload: resolved.clone(),
        };
        s.append_revision(rev);
        let updated = StateEntry {
            id: prev.id,
            scope: prev.scope,
            concern: prev.concern,
            slug: prev.slug,
            sensitive: sens,
            ever_sensitive,
            head_rev: rev_id,
            updated_at: at,
            by: by.to_string(),
            payload: resolved,
        };
        s.state_entries.insert(primary, updated.clone());
        Ok(updated)
    }

    fn get_sync_cursor(&self, remote_key: &str) -> Result<Option<StateSyncCursor>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .state_sync_cursors
            .get(remote_key)
            .cloned())
    }

    fn set_sync_cursor(
        &self,
        remote_key: &str,
        cursor: &StateSyncCursor,
    ) -> Result<(), StoreError> {
        self.inner
            .borrow_mut()
            .state_sync_cursors
            .insert(remote_key.to_string(), cursor.clone());
        Ok(())
    }
}
```

- [ ] **Step 4: Wire `conformance::agent_registry` and `conformance::state_store` in `crates/core/src/mem.rs` and run `cargo test -p fl-core`**

In `crates/core/src/mem.rs` inside `mem_store_meets_every_role_contract`:
```rust
        crate::conformance::agent_registry(|| (MemStore::default(), ()));
        crate::conformance::state_store(|| (MemStore::default(), ()));
```
Run:
```bash
cargo test -p fl-core
```
Expected: all `fl-core` unit, wire, and conformance tests pass.

- [ ] **Step 5: Commit Tasks 1 & 2 (`fl-core` and `fl-github` test fixture)**

Run the trio:
```bash
cargo test -p fl-core -p fl-github
cargo clippy -p fl-core -p fl-github --all-targets -- -D warnings
cargo fmt --all --check
git add crates/core crates/github
git commit -m "feat(core): add StateStore and AgentRegistry domain types, MemStore, and conformance suites

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

### Task 3: `RedbStore` Format 6 persistence (`fl-store`)

**Files:**
- Create: `crates/store/src/state.rs`
- Modify: `crates/store/src/lib.rs`

**Interfaces:**
- Consumes: `StateStore`, `AgentRegistry`, and DAG helpers from `fl-core`.
- Produces:
  - `pub const FORMAT_WITH_STATE: u64 = 6;` in `fl_store`.
  - 11 additive `redb` tables in `crates/store`: `STATE_ENTRIES`, `STATE_KEYS`, `STATE_REVISIONS`, `STATE_REVISION_IDS`, `STATE_CONFLICTS`, `STATE_SYNC_CURSORS`, `AGENTS`, `AGENT_BY_NAME`, `AREAS`, `AGENT_EVENTS`, `AGENT_EVENT_IDS`.
  - `impl AgentRegistry for RedbStore` and `impl StateStore for RedbStore`.

- [ ] **Step 1: Write the failing Format 6 and conformance tests in `crates/store/src/lib.rs`**

In `crates/store/src/lib.rs` test module, add calls to `fl_core::conformance::agent_registry(fresh)` and `fl_core::conformance::state_store(fresh)` inside `redb_store_meets_every_role_contract`, plus a dedicated Format 6 upgrade test:

```rust
    #[test]
    fn writing_state_or_agent_raises_store_format_to_six_and_leaving_them_untouched_keeps_format_two() {
        let (store, dir) = fresh();
        let path = dir.path().join("t.redb");
        let p = store.add_project("/p").unwrap();
        assert_eq!(store.get_project(&p).unwrap().unwrap().root, "/p");
        drop(store);

        let raw = redb::Database::open(&path).unwrap();
        let rtx = raw.begin_read().unwrap();
        let v = rtx
            .open_table(META)
            .unwrap()
            .get(FORMAT_KEY)
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(v, FORMAT_VERSION);
        drop(rtx);
        drop(raw);

        let store = RedbStore::open(&path).unwrap();
        store
            .upsert_state(fl_core::UpsertStateArgs {
                scope: fl_core::StateScope::Workspace,
                slug: "handoff".into(),
                sensitivity: fl_core::SensitivityPatch::Inherit,
                payload: fl_core::StatePayload::Handoff(fl_core::HandoffEntry {
                    summary: "ready".into(),
                    shipped: vec![],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec![],
                    notes: None,
                }),
                at: fl_core::At::from_unix_millis(10),
                by: "ferris".into(),
            })
            .unwrap();
        drop(store);

        let raw = redb::Database::open(&path).unwrap();
        let rtx = raw.begin_read().unwrap();
        let v = rtx
            .open_table(META)
            .unwrap()
            .get(FORMAT_KEY)
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(v, FORMAT_WITH_STATE);
    }
```

- [ ] **Step 2: Run `cargo test -p fl-store` to confirm it fails**

Run: `cargo test -p fl-store`
Expected: compile error (`FORMAT_WITH_STATE` and `AgentRegistry`/`StateStore` impls for `RedbStore` missing).

- [ ] **Step 3: Define `FORMAT_WITH_STATE = 6` and the 11 additive tables in `crates/store/src/lib.rs` and implement `crates/store/src/state.rs`**

In `crates/store/src/lib.rs`:
1. Add `pub mod state;` and define:
```rust
/// ⚠ The format of a store that holds state entries, revisions, conflicts, or
/// registered agents (state spec §3.1). Any write to the state database or
/// agent registry raises the store to 6 in the same transaction so an older
/// `fl` binary refuses the store with `FormatVersion` rather than ignoring
/// state, conflicts, or sensitivity flags.
pub const FORMAT_WITH_STATE: u64 = 6;

pub(crate) const STATE_ENTRIES: TableDefinition<&str, &str> =
    TableDefinition::new("state_entries");
pub(crate) const STATE_KEYS: TableDefinition<(&str, &str, &str), &str> =
    TableDefinition::new("state_keys");
pub(crate) const STATE_REVISIONS: TableDefinition<u64, &str> =
    TableDefinition::new("state_revisions");
pub(crate) const STATE_REVISION_IDS: TableDefinition<&str, u64> =
    TableDefinition::new("state_revision_ids");
pub(crate) const STATE_CONFLICTS: TableDefinition<&str, &str> =
    TableDefinition::new("state_conflicts");
pub(crate) const STATE_SYNC_CURSORS: TableDefinition<&str, &str> =
    TableDefinition::new("state_sync_cursors");
pub(crate) const AGENTS: TableDefinition<&str, &str> = TableDefinition::new("agents");
pub(crate) const AGENT_BY_NAME: TableDefinition<&str, &str> =
    TableDefinition::new("agent_by_name");
pub(crate) const AREAS: TableDefinition<&str, &str> = TableDefinition::new("areas");
pub(crate) const AGENT_EVENTS: TableDefinition<u64, &str> = TableDefinition::new("agent_events");
pub(crate) const AGENT_EVENT_IDS: TableDefinition<&str, u64> =
    TableDefinition::new("agent_event_ids");

pub(crate) const NEXT_STATE_REV: &str = "next_state_rev";
pub(crate) const NEXT_AGENT_EVENT: &str = "next_agent_event";
```
2. Update `RedbStore::open` format check to accept `FORMAT_WITH_STATE` and report `newest: FORMAT_WITH_STATE`:
```rust
            Some(Some(v))
                if v == FORMAT_VERSION
                    || v == FORMAT_WITH_IMPORTS
                    || v == FORMAT_WITH_LEDGER_ROOT
                    || v == FORMAT_WITH_ROUTING
                    || v == FORMAT_WITH_STATE => {}
            Some(v) => {
                return Err(StoreError::FormatVersion {
                    found: v,
                    oldest: FORMAT_VERSION,
                    newest: FORMAT_WITH_STATE,
                });
            }
```
3. Make `err`, `put_json`, `get_json`, `read_all`, `raise_format`, `mint_in`, `index_in`, `locate`, and `require_kind` `pub(crate)` in `crates/store/src/lib.rs`, and add `pub(crate) fn alias_unpublished_in`:
```rust
pub(crate) fn alias_unpublished_in(
    tx: &redb::WriteTransaction,
    local_id: &Iri,
    remote_id: &Iri,
    kind: Kind,
) -> Result<(), StoreError> {
    index_in(tx, remote_id, kind)?;
    tx.open_table(IDS)
        .map_err(err)?
        .insert(local_id.as_str(), ALIAS_MARK)
        .map_err(err)?;
    tx.open_table(ALIASES)
        .map_err(err)?
        .insert(local_id.as_str(), remote_id.as_str())
        .map_err(err)?;
    Ok(())
}
```
4. Create `crates/store/src/state.rs` with the complete `RedbStore` implementations of `AgentRegistry` and `StateStore`:

```rust
use crate::{
    AGENT_BY_NAME, AGENT_EVENT_IDS, AGENT_EVENTS, AGENTS, AREAS, FORMAT_WITH_STATE, META,
    NEXT_AGENT_EVENT, NEXT_STATE_REV, RedbStore, STATE_CONFLICTS, STATE_ENTRIES, STATE_KEYS,
    STATE_REVISION_IDS, STATE_REVISIONS, STATE_SYNC_CURSORS, alias_unpublished_in, err, get_json,
    index_in, mint_in, put_json, raise_format, read_all,
};
use fl_core::agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, RegistryEvent,
    RegistryEventKind, validate_agent_identity,
};
use fl_core::state::{
    MergeMode, SensitivityPatch, StateConcern, StateConflict, StateEntry, StateFilter,
    StatePayload, StateRevision, StateScope, StateSyncCursor, UpsertStateArgs, validate_payload,
    validate_slug, validate_summary,
};
use fl_core::{AgentId, AgentRegistry, At, Iri, Kind, RevisionId, StateId, StateStore, StoreError};
use redb::{ReadableDatabase, ReadableTable};
use std::collections::BTreeMap;

fn next_seq_in(tx: &redb::WriteTransaction, meta_key: &str) -> Result<u64, StoreError> {
    let mut meta = tx.open_table(META).map_err(err)?;
    let n = meta
        .get(meta_key)
        .map_err(err)?
        .map(|g| g.value())
        .unwrap_or(0)
        + 1;
    meta.insert(meta_key, n).map_err(err)?;
    Ok(n)
}

fn append_agent_event_in(
    tx: &redb::WriteTransaction,
    ev: &RegistryEvent,
) -> Result<u64, StoreError> {
    let seq = next_seq_in(tx, NEXT_AGENT_EVENT)?;
    let raw = serde_json::to_string(ev).map_err(err)?;
    tx.open_table(AGENT_EVENTS)
        .map_err(err)?
        .insert(seq, raw.as_str())
        .map_err(err)?;
    tx.open_table(AGENT_EVENT_IDS)
        .map_err(err)?
        .insert(ev.event_id.as_str(), seq)
        .map_err(err)?;
    Ok(seq)
}

fn append_state_rev_in(
    tx: &redb::WriteTransaction,
    rev: &StateRevision,
) -> Result<u64, StoreError> {
    let seq = next_seq_in(tx, NEXT_STATE_REV)?;
    let raw = serde_json::to_string(rev).map_err(err)?;
    tx.open_table(STATE_REVISIONS)
        .map_err(err)?
        .insert(seq, raw.as_str())
        .map_err(err)?;
    tx.open_table(STATE_REVISION_IDS)
        .map_err(err)?
        .insert(rev.id.0.as_str(), seq)
        .map_err(err)?;
    Ok(seq)
}

impl AgentRegistry for RedbStore {
    fn upsert_area(&self, mut area: AreaRecord) -> Result<AreaRecord, StoreError> {
        area.area_key = validate_slug(&area.area_key)?;
        area.purpose = validate_summary(&area.purpose)?;
        let tx = self.db.begin_write().map_err(err)?;
        raise_format(&tx, FORMAT_WITH_STATE)?;
        put_json(&tx, AREAS, &area.area_key, &area)?;
        let ev = RegistryEvent {
            event_id: Iri::new_uuid_v7(),
            at: area.updated_at.clone(),
            by: area.by.clone(),
            kind: RegistryEventKind::AreaUpsert(area.clone()),
        };
        append_agent_event_in(&tx, &ev)?;
        tx.commit().map_err(err)?;
        Ok(area)
    }

    fn get_area(&self, area_key: &str) -> Result<Option<AreaRecord>, StoreError> {
        let key = validate_slug(area_key)?;
        let tx = self.db.begin_read().map_err(err)?;
        match tx.open_table(AREAS) {
            Ok(t) => get_json(&t, &key),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    fn list_areas(&self) -> Result<Vec<AreaRecord>, StoreError> {
        let tx = self.db.begin_read().map_err(err)?;
        match tx.open_table(AREAS) {
            Ok(t) => read_all(&t, |_: &AreaRecord| true),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(err(e)),
        }
    }

    fn register_agent(&self, args: RegisterAgentArgs) -> Result<AgentRecord, StoreError> {
        let (name_key, display_name, email) =
            validate_agent_identity(&args.display_name, args.email.as_deref())
                .map_err(StoreError::Backend)?;
        let project_key = validate_slug(&args.project)?;
        let mut extra_keys = Vec::with_capacity(args.extra_areas.len());
        for extra in &args.extra_areas {
            extra_keys.push(validate_slug(extra)?);
        }

        let tx = self.db.begin_write().map_err(err)?;
        raise_format(&tx, FORMAT_WITH_STATE)?;

        {
            let areas_tbl = tx.open_table(AREAS).map_err(err)?;
            let mut missing = Vec::new();
            for (raw_name, key) in std::iter::once((&args.project, &project_key))
                .chain(args.extra_areas.iter().zip(extra_keys.iter()))
            {
                if areas_tbl.get(key.as_str()).map_err(err)?.is_none() {
                    if !args.create_area {
                        return Err(StoreError::NoSuchArea(key.clone()));
                    }
                    missing.push((raw_name.trim().to_string(), key.clone()));
                }
            }
            drop(areas_tbl);
            for (raw_name, key) in missing {
                let area = AreaRecord {
                    area_key: key.clone(),
                    name: raw_name.clone(),
                    path: None,
                    purpose: format!("Registered area {raw_name}"),
                    updated_at: args.at.clone(),
                    by: args.by.clone(),
                };
                put_json(&tx, AREAS, &key, &area)?;
                let ev = RegistryEvent {
                    event_id: Iri::new_uuid_v7(),
                    at: args.at.clone(),
                    by: args.by.clone(),
                    kind: RegistryEventKind::AreaUpsert(area),
                };
                append_agent_event_in(&tx, &ev)?;
            }
        }

        let existing_id: Option<Iri> = {
            let by_name = tx.open_table(AGENT_BY_NAME).map_err(err)?;
            by_name
                .get(name_key.as_str())
                .map_err(err)?
                .map(|g| Iri::parse(g.value()))
                .transpose()
                .map_err(err)?
        };

        if let Some(ref ex_iri) = existing_id
            && !args.replace
        {
            let agents_tbl = tx.open_table(AGENTS).map_err(err)?;
            let ex: AgentRecord = get_json(&agents_tbl, ex_iri.as_str())?
                .ok_or_else(|| StoreError::NoSuchAgent(ex_iri.to_string()))?;
            return Err(StoreError::AgentCollision {
                name_key,
                existing_id: ex.id,
                status: ex.status,
            });
        }

        if args.role == AgentRole::Lead {
            let all_agents: Vec<AgentRecord> = {
                let agents_tbl = tx.open_table(AGENTS).map_err(err)?;
                read_all(&agents_tbl, |_: &AgentRecord| true)?
            };
            if let Some(lead) = all_agents.into_iter().find(|a| {
                a.status == AgentStatus::Active
                    && a.project == project_key
                    && a.role == AgentRole::Lead
                    && Some(&a.id.0) != existing_id.as_ref()
            }) {
                if !args.replace {
                    return Err(StoreError::LeadTaken {
                        project: project_key,
                        lead_name: lead.display_name,
                        lead_id: lead.id,
                    });
                }
                let mut retired_lead = lead.clone();
                retired_lead.status = AgentStatus::Retired;
                retired_lead.updated_at = args.at.clone();
                retired_lead.by = args.by.clone();
                put_json(&tx, AGENTS, lead.id.0.as_str(), &retired_lead)?;
                let ev = RegistryEvent {
                    event_id: Iri::new_uuid_v7(),
                    at: args.at.clone(),
                    by: args.by.clone(),
                    kind: RegistryEventKind::AgentRetire { id: lead.id },
                };
                append_agent_event_in(&tx, &ev)?;
            }
        }

        let agent_iri = match existing_id {
            Some(iri) => iri,
            None => mint_in(&tx, Kind::Agent)?,
        };
        let record = AgentRecord {
            id: AgentId(agent_iri.clone()),
            name_key: name_key.clone(),
            display_name,
            email,
            project: project_key,
            role: args.role,
            extra_areas: extra_keys,
            status: AgentStatus::Active,
            updated_at: args.at.clone(),
            by: args.by.clone(),
        };
        put_json(&tx, AGENTS, agent_iri.as_str(), &record)?;
        tx.open_table(AGENT_BY_NAME)
            .map_err(err)?
            .insert(name_key.as_str(), agent_iri.as_str())
            .map_err(err)?;

        let ev = RegistryEvent {
            event_id: Iri::new_uuid_v7(),
            at: args.at,
            by: args.by,
            kind: RegistryEventKind::AgentUpsert(record.clone()),
        };
        append_agent_event_in(&tx, &ev)?;
        tx.commit().map_err(err)?;
        Ok(record)
    }

    fn retire_agent(&self, id: &AgentId, at: At, by: &str) -> Result<AgentRecord, StoreError> {
        let tx = self.db.begin_write().map_err(err)?;
        raise_format(&tx, FORMAT_WITH_STATE)?;
        let primary = self.require_kind(&tx, &id.0, Kind::Agent)?;
        let mut rec: AgentRecord = {
            let t = tx.open_table(AGENTS).map_err(err)?;
            get_json(&t, primary.as_str())?
                .ok_or_else(|| StoreError::NoSuchAgent(id.to_string()))?
        };
        rec.status = AgentStatus::Retired;
        rec.updated_at = at.clone();
        rec.by = by.to_string();
        put_json(&tx, AGENTS, primary.as_str(), &rec)?;
        let ev = RegistryEvent {
            event_id: Iri::new_uuid_v7(),
            at,
            by: by.to_string(),
            kind: RegistryEventKind::AgentRetire { id: rec.id.clone() },
        };
        append_agent_event_in(&tx, &ev)?;
        tx.commit().map_err(err)?;
        Ok(rec)
    }

    fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, StoreError> {
        let tx = self.db.begin_read().map_err(err)?;
        let (primary, kind) = self.locate(&tx, &id.0)?;
        if kind != Kind::Agent {
            return Ok(None);
        }
        match tx.open_table(AGENTS) {
            Ok(t) => get_json(&t, primary.as_str()),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    fn agent_by_name(&self, name_key: &str) -> Result<Option<AgentRecord>, StoreError> {
        let key = validate_slug(name_key)?;
        let tx = self.db.begin_read().map_err(err)?;
        let iri_str = match tx.open_table(AGENT_BY_NAME) {
            Ok(t) => t
                .get(key.as_str())
                .map_err(err)?
                .map(|g| g.value().to_string()),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(err(e)),
        };
        let Some(raw_iri) = iri_str else {
            return Ok(None);
        };
        let iri = Iri::parse(&raw_iri).map_err(err)?;
        let (primary, _) = self.locate(&tx, &iri)?;
        match tx.open_table(AGENTS) {
            Ok(t) => get_json(&t, primary.as_str()),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    fn list_agents(
        &self,
        project: Option<&str>,
        include_retired: bool,
    ) -> Result<Vec<AgentRecord>, StoreError> {
        let proj_filter = match project {
            Some(p) => Some(validate_slug(p)?),
            None => None,
        };
        let tx = self.db.begin_read().map_err(err)?;
        let mut out: Vec<AgentRecord> = match tx.open_table(AGENTS) {
            Ok(t) => read_all(&t, |a: &AgentRecord| {
                (include_retired || a.status == AgentStatus::Active)
                    && proj_filter
                        .as_ref()
                        .is_none_or(|p| &a.project == p || a.extra_areas.contains(p))
            })?,
            Err(redb::TableError::TableDoesNotExist(_)) => vec![],
            Err(e) => return Err(err(e)),
        };
        out.sort_by(|a, b| a.name_key.cmp(&b.name_key));
        Ok(out)
    }

    fn registry_events_after(&self, after_seq: u64) -> Result<Vec<(u64, RegistryEvent)>, StoreError> {
        let tx = self.db.begin_read().map_err(err)?;
        let t = match tx.open_table(AGENT_EVENTS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![]),
            Err(e) => return Err(err(e)),
        };
        let mut out = Vec::new();
        for item in t.range((after_seq + 1)..).map_err(err)? {
            let (k, v) = item.map_err(err)?;
            let ev: RegistryEvent = serde_json::from_str(v.value()).map_err(err)?;
            out.push((k.value(), ev));
        }
        Ok(out)
    }

    fn import_registry_events(&self, events: &[RegistryEvent]) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(err)?;
        raise_format(&tx, FORMAT_WITH_STATE)?;
        for ev in events {
            let existing_seq = {
                let ids_tbl = tx.open_table(AGENT_EVENT_IDS).map_err(err)?;
                ids_tbl
                    .get(ev.event_id.as_str())
                    .map_err(err)?
                    .map(|g| g.value())
            };
            if let Some(seq) = existing_seq {
                let ev_tbl = tx.open_table(AGENT_EVENTS).map_err(err)?;
                let stored_raw = ev_tbl
                    .get(seq)
                    .map_err(err)?
                    .map(|g| g.value().to_string())
                    .unwrap_or_default();
                let incoming_raw = serde_json::to_string(ev).map_err(err)?;
                if stored_raw != incoming_raw {
                    return Err(StoreError::Tampered {
                        id: ev.event_id.clone(),
                        detail: "registry event bytes differ from stored event".into(),
                    });
                }
                continue;
            }

            match &ev.kind {
                RegistryEventKind::AreaUpsert(area) => {
                    put_json(&tx, AREAS, &area.area_key, area)?;
                }
                RegistryEventKind::AgentUpsert(rec) => {
                    let local_id: Option<Iri> = {
                        let by_name = tx.open_table(AGENT_BY_NAME).map_err(err)?;
                        by_name
                            .get(rec.name_key.as_str())
                            .map_err(err)?
                            .map(|g| Iri::parse(g.value()))
                            .transpose()
                            .map_err(err)?
                    };
                    if let Some(ref loc_iri) = local_id
                        && loc_iri != &rec.id.0
                    {
                        tx.open_table(AGENTS)
                            .map_err(err)?
                            .remove(loc_iri.as_str())
                            .map_err(err)?;
                        alias_unpublished_in(&tx, loc_iri, &rec.id.0, Kind::Agent)?;
                    } else {
                        index_in(&tx, &rec.id.0, Kind::Agent)?;
                    }
                    if rec.role == AgentRole::Lead && rec.status == AgentStatus::Active {
                        let all_agents: Vec<AgentRecord> = {
                            let t = tx.open_table(AGENTS).map_err(err)?;
                            read_all(&t, |_: &AgentRecord| true)?
                        };
                        for mut other in all_agents {
                            if other.id != rec.id
                                && other.project == rec.project
                                && other.role == AgentRole::Lead
                                && other.status == AgentStatus::Active
                            {
                                other.status = AgentStatus::Retired;
                                put_json(&tx, AGENTS, other.id.0.as_str(), &other)?;
                            }
                        }
                    }
                    put_json(&tx, AGENTS, rec.id.0.as_str(), rec)?;
                    tx.open_table(AGENT_BY_NAME)
                        .map_err(err)?
                        .insert(rec.name_key.as_str(), rec.id.0.as_str())
                        .map_err(err)?;
                }
                RegistryEventKind::AgentRetire { id } => {
                    let (primary, _) = self.locate(&tx, &id.0)?;
                    let maybe_rec: Option<AgentRecord> = {
                        let t = tx.open_table(AGENTS).map_err(err)?;
                        get_json(&t, primary.as_str())?
                    };
                    if let Some(mut rec) = maybe_rec {
                        rec.status = AgentStatus::Retired;
                        rec.updated_at = ev.at.clone();
                        rec.by = ev.by.clone();
                        put_json(&tx, AGENTS, primary.as_str(), &rec)?;
                    }
                }
            }
            append_agent_event_in(&tx, ev)?;
        }
        tx.commit().map_err(err)?;
        Ok(())
    }
}

impl StateStore for RedbStore {
    fn upsert_state(&self, args: UpsertStateArgs) -> Result<StateEntry, StoreError> {
        let scope = args.scope.validated()?;
        let slug = validate_slug(&args.slug)?;
        let mut payload = args.payload;
        validate_payload(&mut payload)?;
        let concern = payload.concern();

        let tx = self.db.begin_write().map_err(err)?;
        raise_format(&tx, FORMAT_WITH_STATE)?;

        if let StatePayload::Memory(ref mem) = payload
            && let Some(ref sup) = mem.superseded_by
        {
            let sup_primary = self.require_kind(&tx, &sup.0, Kind::State)?;
            let entries_tbl = tx.open_table(STATE_ENTRIES).map_err(err)?;
            let Some(target) = get_json::<StateEntry>(&entries_tbl, sup_primary.as_str())? else {
                return Err(StoreError::InvalidState(format!(
                    "superseded_by target `{sup}` does not exist"
                )));
            };
            if target.concern != StateConcern::Memory {
                return Err(StoreError::InvalidState(format!(
                    "superseded_by target `{sup}` is not a memory entry"
                )));
            }
        }

        let scope_str = scope.as_key_str();
        let concern_str = concern.as_wire();
        let existing_id: Option<Iri> = {
            let keys_tbl = tx.open_table(STATE_KEYS).map_err(err)?;
            keys_tbl
                .get((scope_str.as_str(), concern_str, slug.as_str()))
                .map_err(err)?
                .map(|g| Iri::parse(g.value()))
                .transpose()
                .map_err(err)?
        };

        if let Some(ref ex_iri) = existing_id {
            let conflicts_tbl = tx.open_table(STATE_CONFLICTS).map_err(err)?;
            if conflicts_tbl.get(ex_iri.as_str()).map_err(err)?.is_some() {
                return Err(StoreError::StateConflict(StateId(ex_iri.clone())));
            }
            if let StatePayload::Memory(ref mem) = payload
                && mem.superseded_by.as_ref().is_some_and(|sup| &sup.0 == ex_iri)
            {
                return Err(StoreError::InvalidState(
                    "memory entry cannot supersede itself".into(),
                ));
            }
        }

        let (entry_iri, parents, sensitive, ever_sensitive) = match existing_id {
            Some(iri) => {
                let entries_tbl = tx.open_table(STATE_ENTRIES).map_err(err)?;
                let prev: StateEntry = get_json(&entries_tbl, iri.as_str())?
                    .ok_or_else(|| StoreError::NoSuchState(iri.to_string()))?;
                let sens = match args.sensitivity {
                    SensitivityPatch::Inherit => prev.sensitive,
                    SensitivityPatch::Set(v) => v,
                };
                (
                    iri,
                    vec![prev.head_rev],
                    sens,
                    prev.ever_sensitive || sens,
                )
            }
            None => {
                let iri = mint_in(&tx, Kind::State)?;
                let sens = match args.sensitivity {
                    SensitivityPatch::Inherit => false,
                    SensitivityPatch::Set(v) => v,
                };
                (iri, vec![], sens, sens)
            }
        };

        let rev_id = RevisionId(Iri::new_uuid_v7());
        let rev = StateRevision {
            id: rev_id.clone(),
            entry_id: StateId(entry_iri.clone()),
            parents,
            scope: scope.clone(),
            concern,
            slug: slug.clone(),
            sensitive,
            at: args.at.clone(),
            by: args.by.clone(),
            payload: payload.clone(),
        };
        append_state_rev_in(&tx, &rev)?;

        let entry = StateEntry {
            id: StateId(entry_iri.clone()),
            scope,
            concern,
            slug: slug.clone(),
            sensitive,
            ever_sensitive,
            head_rev: rev_id,
            updated_at: args.at,
            by: args.by,
            payload,
        };
        tx.open_table(STATE_KEYS)
            .map_err(err)?
            .insert(
                (scope_str.as_str(), concern_str, slug.as_str()),
                entry_iri.as_str(),
            )
            .map_err(err)?;
        put_json(&tx, STATE_ENTRIES, entry_iri.as_str(), &entry)?;
        tx.commit().map_err(err)?;
        Ok(entry)
    }

    fn get_state(&self, id: &StateId) -> Result<Option<StateEntry>, StoreError> {
        let tx = self.db.begin_read().map_err(err)?;
        let (primary, kind) = self.locate(&tx, &id.0)?;
        if kind != Kind::State {
            return Ok(None);
        }
        match tx.open_table(STATE_ENTRIES) {
            Ok(t) => get_json(&t, primary.as_str()),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    fn state_by_key(
        &self,
        scope: &StateScope,
        concern: StateConcern,
        slug: &str,
    ) -> Result<Option<StateEntry>, StoreError> {
        let scope = scope.validated()?;
        let slug = validate_slug(slug)?;
        let scope_str = scope.as_key_str();
        let concern_str = concern.as_wire();
        let tx = self.db.begin_read().map_err(err)?;
        let iri_str = match tx.open_table(STATE_KEYS) {
            Ok(t) => t
                .get((scope_str.as_str(), concern_str, slug.as_str()))
                .map_err(err)?
                .map(|g| g.value().to_string()),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(err(e)),
        };
        let Some(raw_iri) = iri_str else {
            return Ok(None);
        };
        let iri = Iri::parse(&raw_iri).map_err(err)?;
        let (primary, _) = self.locate(&tx, &iri)?;
        match tx.open_table(STATE_ENTRIES) {
            Ok(t) => get_json(&t, primary.as_str()),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    fn list_state(&self, filter: &StateFilter) -> Result<Vec<StateEntry>, StoreError> {
        let scope_filter = match &filter.scope {
            Some(sc) => Some(sc.validated()?),
            None => None,
        };
        let tx = self.db.begin_read().map_err(err)?;
        let mut out: Vec<StateEntry> = match tx.open_table(STATE_ENTRIES) {
            Ok(t) => read_all(&t, |e: &StateEntry| {
                scope_filter.as_ref().is_none_or(|sc| &e.scope == sc)
                    && filter.concern.is_none_or(|c| e.concern == c)
                    && (filter.include_archived
                        || match &e.payload {
                            StatePayload::Memory(m) => {
                                m.status == fl_core::state::MemoryStatus::Active
                            }
                            _ => true,
                        })
                    && filter.tag.as_ref().is_none_or(|tag| match &e.payload {
                        StatePayload::Memory(m) => m.tags.contains(tag),
                        _ => false,
                    })
            })?,
            Err(redb::TableError::TableDoesNotExist(_)) => vec![],
            Err(e) => return Err(err(e)),
        };
        out.sort_by(|a, b| {
            (
                a.scope.as_key_str(),
                a.concern.as_wire(),
                a.slug.as_str(),
            )
                .cmp(&(
                    b.scope.as_key_str(),
                    b.concern.as_wire(),
                    b.slug.as_str(),
                ))
        });
        Ok(out)
    }

    fn get_revision(&self, id: &RevisionId) -> Result<Option<StateRevision>, StoreError> {
        let tx = self.db.begin_read().map_err(err)?;
        let seq = match tx.open_table(STATE_REVISION_IDS) {
            Ok(t) => t.get(id.0.as_str()).map_err(err)?.map(|g| g.value()),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(err(e)),
        };
        let Some(seq) = seq else {
            return Ok(None);
        };
        match tx.open_table(STATE_REVISIONS) {
            Ok(t) => match t.get(seq).map_err(err)? {
                Some(g) => Ok(Some(serde_json::from_str(g.value()).map_err(err)?)),
                None => Ok(None),
            },
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    fn revisions_for(&self, entry_id: &StateId) -> Result<Vec<StateRevision>, StoreError> {
        let tx = self.db.begin_read().map_err(err)?;
        let primary = self.require_kind(&tx, &entry_id.0, Kind::State)?;
        let t = match tx.open_table(STATE_REVISIONS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![]),
            Err(e) => return Err(err(e)),
        };
        let mut out = Vec::new();
        for item in t.iter().map_err(err)? {
            let (_, v) = item.map_err(err)?;
            let rev: StateRevision = serde_json::from_str(v.value()).map_err(err)?;
            if rev.entry_id.0 == primary {
                out.push(rev);
            }
        }
        Ok(out)
    }

    fn revisions_after(&self, after_seq: u64) -> Result<Vec<(u64, StateRevision)>, StoreError> {
        let tx = self.db.begin_read().map_err(err)?;
        let t = match tx.open_table(STATE_REVISIONS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![]),
            Err(e) => return Err(err(e)),
        };
        let mut out = Vec::new();
        for item in t.range((after_seq + 1)..).map_err(err)? {
            let (k, v) = item.map_err(err)?;
            let rev: StateRevision = serde_json::from_str(v.value()).map_err(err)?;
            out.push((k.value(), rev));
        }
        Ok(out)
    }

    fn import_state_revisions(&self, revisions: &[StateRevision]) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(err)?;
        raise_format(&tx, FORMAT_WITH_STATE)?;

        for incoming in revisions {
            let scope = incoming.scope.validated()?;
            let slug = validate_slug(&incoming.slug)?;
            let mut payload = incoming.payload.clone();
            validate_payload(&mut payload)?;
            if payload.concern() != incoming.concern {
                return Err(StoreError::InvalidState(
                    "revision concern does not match payload variant".into(),
                ));
            }

            let existing_seq = {
                let ids_tbl = tx.open_table(STATE_REVISION_IDS).map_err(err)?;
                ids_tbl
                    .get(incoming.id.0.as_str())
                    .map_err(err)?
                    .map(|g| g.value())
            };
            if let Some(seq) = existing_seq {
                let revs_tbl = tx.open_table(STATE_REVISIONS).map_err(err)?;
                let stored_raw = revs_tbl
                    .get(seq)
                    .map_err(err)?
                    .map(|g| g.value().to_string())
                    .unwrap_or_default();
                let incoming_raw = serde_json::to_string(incoming).map_err(err)?;
                if stored_raw != incoming_raw {
                    return Err(StoreError::Tampered {
                        id: incoming.id.0.clone(),
                        detail: "state revision bytes differ from stored revision".into(),
                    });
                }
                continue;
            }

            let scope_str = scope.as_key_str();
            let concern_str = incoming.concern.as_wire();
            let mut rev = incoming.clone();
            rev.scope = scope.clone();
            rev.slug = slug.clone();
            rev.payload = payload;

            let existing_key_iri: Option<Iri> = {
                let keys_tbl = tx.open_table(STATE_KEYS).map_err(err)?;
                keys_tbl
                    .get((scope_str.as_str(), concern_str, slug.as_str()))
                    .map_err(err)?
                    .map(|g| Iri::parse(g.value()))
                    .transpose()
                    .map_err(err)?
            };

            if let Some(ref loc_iri) = existing_key_iri
                && loc_iri != &rev.entry_id.0
            {
                let remote_iri = rev.entry_id.0.clone();
                alias_unpublished_in(&tx, loc_iri, &remote_iri, Kind::State)?;

                let local_entry: Option<StateEntry> = {
                    let entries_tbl = tx.open_table(STATE_ENTRIES).map_err(err)?;
                    get_json(&entries_tbl, loc_iri.as_str())?
                };
                if let Some(mut le) = local_entry {
                    tx.open_table(STATE_ENTRIES)
                        .map_err(err)?
                        .remove(loc_iri.as_str())
                        .map_err(err)?;
                    le.id = StateId(remote_iri.clone());
                    put_json(&tx, STATE_ENTRIES, remote_iri.as_str(), &le)?;
                }
                tx.open_table(STATE_KEYS)
                    .map_err(err)?
                    .insert(
                        (scope_str.as_str(), concern_str, slug.as_str()),
                        remote_iri.as_str(),
                    )
                    .map_err(err)?;

                let mut to_rewrite = Vec::new();
                {
                    let revs_tbl = tx.open_table(STATE_REVISIONS).map_err(err)?;
                    for item in revs_tbl.iter().map_err(err)? {
                        let (k, v) = item.map_err(err)?;
                        let mut r: StateRevision = serde_json::from_str(v.value()).map_err(err)?;
                        if &r.entry_id.0 == loc_iri {
                            r.entry_id = StateId(remote_iri.clone());
                            to_rewrite.push((k.value(), serde_json::to_string(&r).map_err(err)?));
                        }
                    }
                }
                {
                    let mut revs_tbl = tx.open_table(STATE_REVISIONS).map_err(err)?;
                    for (seq, raw) in to_rewrite {
                        revs_tbl.insert(seq, raw.as_str()).map_err(err)?;
                    }
                }
            } else {
                index_in(&tx, &rev.entry_id.0, Kind::State)?;
            }

            let (entry_iri, _) = self.locate(&tx, &rev.entry_id.0)?;
            rev.entry_id = StateId(entry_iri.clone());

            let current: Option<StateEntry> = {
                let entries_tbl = tx.open_table(STATE_ENTRIES).map_err(err)?;
                get_json(&entries_tbl, entry_iri.as_str())?
            };

            if let Some(ref existing) = current
                && (existing.scope != rev.scope
                    || existing.concern != rev.concern
                    || existing.slug != rev.slug)
            {
                return Err(StoreError::InvalidState(format!(
                    "revision `{}` mutates immutable (scope, concern, slug) of `{}`",
                    rev.id, existing.id
                )));
            }

            append_state_rev_in(&tx, &rev)?;

            let Some(current) = current else {
                let entry = StateEntry {
                    id: StateId(entry_iri.clone()),
                    scope: rev.scope.clone(),
                    concern: rev.concern,
                    slug: rev.slug.clone(),
                    sensitive: rev.sensitive,
                    ever_sensitive: rev.sensitive,
                    head_rev: rev.id.clone(),
                    updated_at: rev.at.clone(),
                    by: rev.by.clone(),
                    payload: rev.payload.clone(),
                };
                tx.open_table(STATE_KEYS)
                    .map_err(err)?
                    .insert(
                        (scope_str.as_str(), concern_str, slug.as_str()),
                        entry_iri.as_str(),
                    )
                    .map_err(err)?;
                put_json(&tx, STATE_ENTRIES, entry_iri.as_str(), &entry)?;
                continue;
            };

            let all_revs: BTreeMap<RevisionId, StateRevision> = {
                let revs_tbl = tx.open_table(STATE_REVISIONS).map_err(err)?;
                let mut m = BTreeMap::new();
                for item in revs_tbl.iter().map_err(err)? {
                    let (_, v) = item.map_err(err)?;
                    let r: StateRevision = serde_json::from_str(v.value()).map_err(err)?;
                    if r.entry_id.0 == entry_iri {
                        m.insert(r.id.clone(), r);
                    }
                }
                m
            };

            if fl_core::state::is_ancestor(&current.head_rev, &rev.id, &all_revs) {
                let updated = StateEntry {
                    id: current.id,
                    scope: current.scope,
                    concern: current.concern,
                    slug: current.slug,
                    sensitive: rev.sensitive,
                    ever_sensitive: current.ever_sensitive || rev.sensitive,
                    head_rev: rev.id,
                    updated_at: rev.at,
                    by: rev.by,
                    payload: rev.payload,
                };
                put_json(&tx, STATE_ENTRIES, entry_iri.as_str(), &updated)?;
            } else if fl_core::state::is_ancestor(&rev.id, &current.head_rev, &all_revs) {
                if rev.sensitive && !current.ever_sensitive {
                    let mut updated = current;
                    updated.ever_sensitive = true;
                    put_json(&tx, STATE_ENTRIES, entry_iri.as_str(), &updated)?;
                }
            } else {
                let base_rev =
                    fl_core::state::lowest_common_ancestor(&current.head_rev, &rev.id, &all_revs);
                let local_head_rev = all_revs.get(&current.head_rev).cloned().unwrap();
                let base_state = base_rev.as_ref().and_then(|b| all_revs.get(b));

                let auto_merged =
                    fl_core::state::try_merge_runtime_append(base_state, &local_head_rev, &rev)
                        .or_else(|| {
                            fl_core::state::try_merge_runtime_last_writer(&local_head_rev, &rev)
                        });

                if let Some(merged_payload) = auto_merged {
                    let merge_rev_id = RevisionId(Iri::new_uuid_v7());
                    let merged_sens = current.sensitive || rev.sensitive;
                    let merged_at = if rev.at.as_str() >= current.updated_at.as_str() {
                        rev.at.clone()
                    } else {
                        current.updated_at.clone()
                    };
                    let merge_rev = StateRevision {
                        id: merge_rev_id.clone(),
                        entry_id: current.id.clone(),
                        parents: vec![current.head_rev.clone(), rev.id.clone()],
                        scope: current.scope.clone(),
                        concern: current.concern,
                        slug: current.slug.clone(),
                        sensitive: merged_sens,
                        at: merged_at.clone(),
                        by: rev.by.clone(),
                        payload: merged_payload.clone(),
                    };
                    append_state_rev_in(&tx, &merge_rev)?;
                    let updated = StateEntry {
                        id: current.id,
                        scope: current.scope,
                        concern: current.concern,
                        slug: current.slug,
                        sensitive: merged_sens,
                        ever_sensitive: current.ever_sensitive || merged_sens,
                        head_rev: merge_rev_id,
                        updated_at: merged_at,
                        by: rev.by,
                        payload: merged_payload,
                    };
                    put_json(&tx, STATE_ENTRIES, entry_iri.as_str(), &updated)?;
                } else {
                    let conflict = StateConflict {
                        entry_id: current.id.clone(),
                        base_rev,
                        local_rev: current.head_rev.clone(),
                        remote_rev: rev.id.clone(),
                        detected_at: rev.at.clone(),
                    };
                    put_json(&tx, STATE_CONFLICTS, entry_iri.as_str(), &conflict)?;
                    if rev.sensitive && !current.ever_sensitive {
                        let mut updated = current;
                        updated.ever_sensitive = true;
                        put_json(&tx, STATE_ENTRIES, entry_iri.as_str(), &updated)?;
                    }
                }
            }
        }
        tx.commit().map_err(err)?;
        Ok(())
    }

    fn list_conflicts(
        &self,
        scope: Option<&StateScope>,
    ) -> Result<Vec<StateConflict>, StoreError> {
        let scope_filter = match scope {
            Some(sc) => Some(sc.validated()?),
            None => None,
        };
        let tx = self.db.begin_read().map_err(err)?;
        let conflicts_tbl = match tx.open_table(STATE_CONFLICTS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![]),
            Err(e) => return Err(err(e)),
        };
        let entries_tbl = tx.open_table(STATE_ENTRIES).ok();
        let all_conflicts: Vec<StateConflict> = read_all(&conflicts_tbl, |_: &StateConflict| true)?;
        let mut out = Vec::new();
        for c in all_conflicts {
            if let Some(ref sc) = scope_filter {
                let matches = if let Some(ref et) = entries_tbl {
                    get_json::<StateEntry>(et, c.entry_id.0.as_str())?
                        .is_some_and(|e| &e.scope == sc)
                } else {
                    false
                };
                if !matches {
                    continue;
                }
            }
            out.push(c);
        }
        out.sort_by(|a, b| a.entry_id.cmp(&b.entry_id));
        Ok(out)
    }

    fn resolve_conflict(
        &self,
        entry_id: &StateId,
        mut resolved: StatePayload,
        sensitivity: SensitivityPatch,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError> {
        validate_payload(&mut resolved)?;
        let tx = self.db.begin_write().map_err(err)?;
        raise_format(&tx, FORMAT_WITH_STATE)?;
        let primary = self.require_kind(&tx, &entry_id.0, Kind::State)?;

        let conflict: StateConflict = {
            let mut ct = tx.open_table(STATE_CONFLICTS).map_err(err)?;
            let Some(raw) = ct.remove(primary.as_str()).map_err(err)? else {
                return Err(StoreError::InvalidState(format!(
                    "state entry `{entry_id}` is not in `state_conflicts`"
                )));
            };
            serde_json::from_str(raw.value()).map_err(err)?
        };

        let prev: StateEntry = {
            let et = tx.open_table(STATE_ENTRIES).map_err(err)?;
            get_json(&et, primary.as_str())?
                .ok_or_else(|| StoreError::NoSuchState(entry_id.to_string()))?
        };
        if resolved.concern() != prev.concern {
            return Err(StoreError::InvalidState(
                "resolved payload concern does not match entry concern".into(),
            ));
        }

        let remote_sens = {
            let ids_tbl = tx.open_table(STATE_REVISION_IDS).map_err(err)?;
            let revs_tbl = tx.open_table(STATE_REVISIONS).map_err(err)?;
            if let Some(seq) = ids_tbl.get(conflict.remote_rev.0.as_str()).map_err(err)? {
                revs_tbl
                    .get(seq.value())
                    .map_err(err)?
                    .and_then(|g| serde_json::from_str::<StateRevision>(g.value()).ok())
                    .is_some_and(|r| r.sensitive)
            } else {
                false
            }
        };

        let sens = match sensitivity {
            SensitivityPatch::Inherit => prev.sensitive || remote_sens,
            SensitivityPatch::Set(v) => v,
        };
        let ever_sensitive = prev.ever_sensitive || remote_sens || sens;
        let rev_id = RevisionId(Iri::new_uuid_v7());
        let rev = StateRevision {
            id: rev_id.clone(),
            entry_id: prev.id.clone(),
            parents: vec![conflict.local_rev, conflict.remote_rev],
            scope: prev.scope.clone(),
            concern: prev.concern,
            slug: prev.slug.clone(),
            sensitive: sens,
            at: at.clone(),
            by: by.to_string(),
            payload: resolved.clone(),
        };
        append_state_rev_in(&tx, &rev)?;
        let updated = StateEntry {
            id: prev.id,
            scope: prev.scope,
            concern: prev.concern,
            slug: prev.slug,
            sensitive: sens,
            ever_sensitive,
            head_rev: rev_id,
            updated_at: at,
            by: by.to_string(),
            payload: resolved,
        };
        put_json(&tx, STATE_ENTRIES, primary.as_str(), &updated)?;
        tx.commit().map_err(err)?;
        Ok(updated)
    }

    fn get_sync_cursor(&self, remote_key: &str) -> Result<Option<StateSyncCursor>, StoreError> {
        let tx = self.db.begin_read().map_err(err)?;
        match tx.open_table(STATE_SYNC_CURSORS) {
            Ok(t) => get_json(&t, remote_key),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    fn set_sync_cursor(
        &self,
        remote_key: &str,
        cursor: &StateSyncCursor,
    ) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(err)?;
        raise_format(&tx, FORMAT_WITH_STATE)?;
        put_json(&tx, STATE_SYNC_CURSORS, remote_key, cursor)?;
        tx.commit().map_err(err)?;
        Ok(())
    }
}
```

- [ ] **Step 4: Run the verification trio and commit Task 3**

Run:
```bash
cargo test --workspace
cargo clippy --all-targets --workspace -- -D warnings
cargo fmt --all --check
git add crates/core crates/store crates/github
git commit -m "feat(store): persist StateStore and AgentRegistry in RedbStore format 6

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

### Task 4: `fl-exec` git plumbing on `refs/heads/fl/state` (`fl-exec`)

**Files:**
- Modify: `crates/exec/Cargo.toml`
- Modify: `crates/exec/src/git.rs`
- Create: `crates/exec/src/state_sync.rs`
- Modify: `crates/exec/src/lib.rs`

**Interfaces:**
- Consumes: `StateStore`, `AgentRegistry`, `StateRevision`, `RegistryEvent`, `StateSyncCursor`, `StateScope`, `StoreError` from `fl-core`.
- Produces:
  - Plumbing methods on `fl_exec::git::Git` (`ls_remote_branch`, `fetch_branch`, `is_ancestor`, `rev_list_first_parent`, `ls_tree`, `cat_blob`, `hash_object`, `mktree`, `commit_tree`, `push_cas`).
  - `fl_exec::state_sync::{SyncTarget, SyncTargetVisibility, SyncReport, VerifyReport, pull_state, push_state, sync_state, verify_state, STATE_BRANCH, STATE_REF, REMOTE_TRACKING_REF, MAX_SEGMENT_BYTES, MAX_CAS_TRIES}`.

- [ ] **Step 1: Add `sha2.workspace = true` to `crates/exec/Cargo.toml` and write failing plumbing & sync tests**

Update `crates/exec/Cargo.toml` to include `sha2`:

```toml
[package]
name = "fl-exec"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
fl-core = { path = "../core" }
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

Add `pub mod state_sync;` to `crates/exec/src/lib.rs`:

```rust
pub mod gate;
pub mod git;
pub mod stamp;
pub mod state_sync;

pub use gate::{GateFailure, execute_gate, run_shell_command};
pub use git::{Git, GitError};
pub use stamp::{CheckError, Stamp, check_stamp};
pub use state_sync::{
    MAX_CAS_TRIES, MAX_SEGMENT_BYTES, REMOTE_TRACKING_REF, STATE_BRANCH, STATE_REF, SyncReport,
    SyncTarget, SyncTargetVisibility, VerifyReport, pull_state, push_state, sync_state,
    verify_state,
};
```

- [ ] **Step 2: Extend `crates/exec/src/git.rs` with git object/ref plumbing methods**

Append to `impl Git` in `crates/exec/src/git.rs`:

```rust
    /// `git ls-remote --heads <remote> <branch>` -> `Ok(Some(sha))` or `Ok(None)`.
    pub fn ls_remote_branch(
        &self,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>, GitError> {
        let out = self.run(&["ls-remote", "--heads", remote, branch])?;
        let line = match out.lines().next() {
            Some(l) if !l.trim().is_empty() => l,
            _ => return Ok(None),
        };
        let sha = line
            .split_whitespace()
            .next()
            .ok_or_else(|| GitError(format!("malformed ls-remote line: {line}")))?;
        Ok(Some(sha.to_string()))
    }

    /// `git fetch --no-tags --no-write-fetch-head <remote> +refs/heads/<branch>:<tracking_ref>`.
    pub fn fetch_branch(
        &self,
        remote: &str,
        branch: &str,
        tracking_ref: &str,
    ) -> Result<(), GitError> {
        let refspec = format!("+refs/heads/{branch}:{tracking_ref}");
        self.run(&[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            remote,
            &refspec,
        ])?;
        Ok(())
    }

    /// `git merge-base --is-ancestor <older> <newer>`.
    pub fn is_ancestor(&self, older: &str, newer: &str) -> Result<bool, GitError> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["merge-base", "--is-ancestor", older, newer])
            .output()
            .map_err(|e| GitError(format!("failed to invoke git: {e}")))?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(GitError(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            )),
        }
    }

    /// `git rev-list --first-parent --reverse <range>` -> oldest-first commits.
    pub fn rev_list_first_parent(&self, range: &str) -> Result<Vec<String>, GitError> {
        let out = self.run(&["rev-list", "--first-parent", "--reverse", range])?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// `git ls-tree <tree_ish> [path]` -> `(mode, kind, oid, name)` tuples.
    pub fn ls_tree(
        &self,
        tree_ish: &str,
        path: Option<&str>,
    ) -> Result<Vec<(String, String, String, String)>, GitError> {
        let mut args = vec!["ls-tree", tree_ish];
        if let Some(p) = path {
            args.push(p);
        }
        let out = self.run(&args)?;
        let mut entries = Vec::new();
        for line in out.lines().filter(|l| !l.trim().is_empty()) {
            let Some((meta, name)) = line.split_once('\t') else {
                return Err(GitError(format!("malformed ls-tree line: {line}")));
            };
            let mut parts = meta.split_whitespace();
            let mode = parts.next().unwrap_or_default().to_string();
            let kind = parts.next().unwrap_or_default().to_string();
            let oid = parts.next().unwrap_or_default().to_string();
            entries.push((mode, kind, oid, name.to_string()));
        }
        Ok(entries)
    }

    /// `git ls-tree -r <tree_ish>` -> `(oid, full_path)` for all blobs in the tree.
    pub fn ls_tree_recursive(&self, tree_ish: &str) -> Result<Vec<(String, String)>, GitError> {
        let out = self.run(&["ls-tree", "-r", tree_ish])?;
        let mut blobs = Vec::new();
        for line in out.lines().filter(|l| !l.trim().is_empty()) {
            let Some((meta, path)) = line.split_once('\t') else {
                return Err(GitError(format!("malformed ls-tree -r line: {line}")));
            };
            let mut parts = meta.split_whitespace();
            let _mode = parts.next();
            let kind = parts.next().unwrap_or_default();
            let oid = parts.next().unwrap_or_default();
            if kind == "blob" {
                blobs.push((oid.to_string(), path.to_string()));
            }
        }
        Ok(blobs)
    }

    /// `git cat-file blob <oid>` -> raw bytes.
    pub fn cat_blob(&self, oid: &str) -> Result<Vec<u8>, GitError> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["cat-file", "blob", oid])
            .output()
            .map_err(|e| GitError(format!("failed to invoke git: {e}")))?;
        if !out.status.success() {
            return Err(GitError(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(out.stdout)
    }

    /// `git hash-object -w --stdin` -> 40-hex blob OID.
    pub fn hash_object(&self, bytes: &[u8]) -> Result<String, GitError> {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| GitError(format!("failed to spawn git hash-object: {e}")))?;
        child
            .stdin
            .as_mut()
            .expect("stdin piped")
            .write_all(bytes)
            .map_err(|e| GitError(format!("failed to write to git hash-object: {e}")))?;
        let out = child
            .wait_with_output()
            .map_err(|e| GitError(format!("failed to wait on git hash-object: {e}")))?;
        if !out.status.success() {
            return Err(GitError(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// `git mktree` from `(mode, kind, oid, name)` entries -> 40-hex tree OID.
    pub fn mktree(&self, entries: &[(String, String, String, String)]) -> Result<String, GitError> {
        use std::io::Write;
        use std::process::Stdio;
        let mut input = String::new();
        for (mode, kind, oid, name) in entries {
            input.push_str(&format!("{mode} {kind} {oid}\t{name}\n"));
        }
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .arg("mktree")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| GitError(format!("failed to spawn git mktree: {e}")))?;
        child
            .stdin
            .as_mut()
            .expect("stdin piped")
            .write_all(input.as_bytes())
            .map_err(|e| GitError(format!("failed to write to git mktree: {e}")))?;
        let out = child
            .wait_with_output()
            .map_err(|e| GitError(format!("failed to wait on git mktree: {e}")))?;
        if !out.status.success() {
            return Err(GitError(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// `git commit-tree <tree_oid> [-p <parent>] -m <message>` with deterministic
    /// author/committer identity so commits do not depend on ambient git config.
    pub fn commit_tree(
        &self,
        tree_oid: &str,
        parent: Option<&str>,
        message: &str,
        author_name: &str,
        author_email: &str,
    ) -> Result<String, GitError> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo_root)
            .env("GIT_AUTHOR_NAME", author_name)
            .env("GIT_AUTHOR_EMAIL", author_email)
            .env("GIT_COMMITTER_NAME", author_name)
            .env("GIT_COMMITTER_EMAIL", author_email)
            .arg("commit-tree")
            .arg(tree_oid);
        if let Some(p) = parent {
            cmd.args(["-p", p]);
        }
        cmd.args(["-m", message]);
        let out = cmd
            .output()
            .map_err(|e| GitError(format!("failed to invoke git commit-tree: {e}")))?;
        if !out.status.success() {
            return Err(GitError(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// `git push --force-with-lease=<dest_ref>:<expected_old> <remote> <commit_oid>:<dest_ref>`.
    /// Returns `Ok(true)` if pushed, `Ok(false)` on lease rejection, or `Err` on transport failure.
    pub fn push_cas(
        &self,
        remote: &str,
        commit_oid: &str,
        dest_ref: &str,
        expected_old: Option<&str>,
    ) -> Result<bool, GitError> {
        let lease = match expected_old {
            Some(old) => format!("--force-with-lease={dest_ref}:{old}"),
            None => format!("--force-with-lease={dest_ref}:"),
        };
        let refspec = format!("{commit_oid}:{dest_ref}");
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["push", &lease, remote, &refspec])
            .output()
            .map_err(|e| GitError(format!("failed to invoke git push: {e}")))?;
        if out.status.success() {
            return Ok(true);
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if stderr.contains("stale info")
            || stderr.contains("rejected")
            || stderr.contains("fetch first")
            || stderr.contains("non-fast-forward")
        {
            return Ok(false);
        }
        Err(GitError(stderr))
    }
```

- [ ] **Step 3: Create `crates/exec/src/state_sync.rs` implementing pull, push, 5-try CAS sync, and `verify_state`**

Create `crates/exec/src/state_sync.rs`:

```rust
use crate::git::{Git, GitError};
use fl_core::agent::RegistryEvent;
use fl_core::state::{StateRevision, StateScope, StateSyncCursor};
use fl_core::{AgentRegistry, StateStore, StoreError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const STATE_BRANCH: &str = "fl/state";
pub const STATE_REF: &str = "refs/heads/fl/state";
pub const REMOTE_TRACKING_REF: &str = "refs/remotes/fl-state/head";
pub const MAX_SEGMENT_BYTES: usize = 262_144;
pub const MAX_CAS_TRIES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncTargetVisibility {
    /// Confirmed private repository (e.g. via GitHub API or explicit `--allow-sensitive` override).
    Private,
    /// Public, internal, or unverified local/SSH remote.
    NotConfirmedPrivate,
}

#[derive(Debug, Clone)]
pub struct SyncTarget {
    /// Logical key in `state_sync_cursors` (`"workspace"` or `"project:<slug>"`).
    pub cursor_key: String,
    /// Scope filter: `None` syncs workspace + agent registry; `Some(scope)` syncs only that scope.
    pub scope_filter: Option<StateScope>,
    /// Git working/bare clone used to run git plumbing commands.
    pub git: Git,
    /// Remote name or URL passed to `git ls-remote`, `git fetch`, and `git push`.
    pub remote: String,
    /// Branch name on the remote (default `"fl/state"`).
    pub branch: String,
    /// Visibility of the remote for the sensitivity pre-flight check (spec §4.5).
    pub visibility: SyncTargetVisibility,
    /// Author name for commits on `fl/state`.
    pub author_name: String,
    /// Author email for commits on `fl/state`.
    pub author_email: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SyncReport {
    pub pulled_revisions: usize,
    pub pulled_agent_events: usize,
    pub pushed_revisions: usize,
    pub pushed_agent_events: usize,
    pub head_commit: Option<String>,
    pub cas_attempts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VerifyReport {
    pub verified_commits: usize,
    pub verified_segments: usize,
    pub verified_revisions: usize,
    pub verified_agent_events: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FormatJson {
    format: u32,
}

fn git_err(e: GitError) -> StoreError {
    StoreError::Backend(e.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

fn parse_segment_num(filename: &str) -> Option<u32> {
    let stem = filename.strip_prefix("seg-")?.strip_suffix(".jsonl")?;
    if stem.len() != 6 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse::<u32>().ok().filter(|&n| n >= 1)
}

fn format_segment_name(n: u32) -> String {
    format!("seg-{n:06}.jsonl")
}

/// Represent a two-level or three-level directory tree in memory so we can read,
/// append to, and rebuild `fl/state` trees cleanly with `git mktree`.
#[derive(Debug, Clone, Default)]
struct TreeImage {
    /// Map from relative blob path (e.g. `"format.json"`, `"revisions/seg-000001.jsonl"`,
    /// `"agents/seg-000001.jsonl"`) to `(oid, bytes)`.
    blobs: BTreeMap<String, (String, Vec<u8>)>,
}

impl TreeImage {
    fn load(git: &Git, commit_oid: &str) -> Result<Self, StoreError> {
        let mut blobs = BTreeMap::new();
        for (oid, path) in git.ls_tree_recursive(commit_oid).map_err(git_err)? {
            let bytes = git.cat_blob(&oid).map_err(git_err)?;
            blobs.insert(path, (oid, bytes));
        }
        Ok(Self { blobs })
    }

    fn validate_layout(&self) -> Result<(), StoreError> {
        let Some((_, fmt_bytes)) = self.blobs.get("format.json") else {
            return Err(StoreError::Backend(
                "fl/state tree is missing `format.json`".into(),
            ));
        };
        let fmt: FormatJson = serde_json::from_slice(fmt_bytes)
            .map_err(|e| StoreError::Backend(format!("invalid `format.json`: {e}")))?;
        if fmt.format != 1 {
            return Err(StoreError::Backend(format!(
                "unsupported fl/state format {}; expected 1",
                fmt.format
            )));
        }
        for path in self.blobs.keys() {
            if path == "format.json" {
                continue;
            }
            let valid = match path.split_once('/') {
                Some(("revisions" | "agents", file)) => parse_segment_num(file).is_some(),
                _ => false,
            };
            if !valid {
                return Err(StoreError::Backend(format!(
                    "unexpected path `{path}` in fl/state tree"
                )));
            }
        }
        Ok(())
    }

    /// Verify that `newer` only appends to existing segments or adds new segment files
    /// compared to `older` (spec §4.2 step 1 & §4.4).
    fn verify_append_only_since(&self, older: &TreeImage) -> Result<(), StoreError> {
        for (path, (_, old_bytes)) in &older.blobs {
            let Some((_, new_bytes)) = self.blobs.get(path) else {
                return Err(StoreError::Tampered {
                    id: fl_core::ids::seq_iri(0),
                    detail: format!("fl/state segment `{path}` was deleted"),
                });
            };
            if path == "format.json" {
                if old_bytes != new_bytes {
                    return Err(StoreError::Tampered {
                        id: fl_core::ids::seq_iri(0),
                        detail: "fl/state `format.json` was modified".into(),
                    });
                }
                continue;
            }
            if new_bytes.len() < old_bytes.len() || &new_bytes[..old_bytes.len()] != old_bytes.as_slice() {
                return Err(StoreError::Tampered {
                    id: fl_core::ids::seq_iri(0),
                    detail: format!(
                        "fl/state segment `{path}` was truncated or modified in-place"
                    ),
                });
            }
        }
        Ok(())
    }

    fn append_jsonl_records(&mut self, git: &Git, dir: &str, lines: &[String]) -> Result<(), StoreError> {
        if lines.is_empty() {
            return Ok(());
        }
        let prefix = format!("{dir}/");
        let mut max_seg = 0u32;
        for path in self.blobs.keys() {
            if let Some(rest) = path.strip_prefix(&prefix)
                && let Some(n) = parse_segment_num(rest)
            {
                max_seg = max_seg.max(n);
            }
        }
        if max_seg == 0 {
            max_seg = 1;
        }
        let mut cur_path = format!("{dir}/{}", format_segment_name(max_seg));
        let mut cur_bytes = self
            .blobs
            .get(&cur_path)
            .map(|(_, b)| b.clone())
            .unwrap_or_default();

        for line in lines {
            let mut record_bytes = line.as_bytes().to_vec();
            record_bytes.push(b'\n');
            if !cur_bytes.is_empty() && cur_bytes.len() + record_bytes.len() > MAX_SEGMENT_BYTES {
                let oid = git.hash_object(&cur_bytes).map_err(git_err)?;
                self.blobs.insert(cur_path, (oid, cur_bytes));
                max_seg += 1;
                cur_path = format!("{dir}/{}", format_segment_name(max_seg));
                cur_bytes = Vec::new();
            }
            cur_bytes.extend_from_slice(&record_bytes);
        }
        let oid = git.hash_object(&cur_bytes).map_err(git_err)?;
        self.blobs.insert(cur_path, (oid, cur_bytes));
        Ok(())
    }

    fn write_tree(&mut self, git: &Git) -> Result<String, StoreError> {
        if !self.blobs.contains_key("format.json") {
            let bytes = b"{\"format\":1}\n".to_vec();
            let oid = git.hash_object(&bytes).map_err(git_err)?;
            self.blobs.insert("format.json".to_string(), (oid, bytes));
        }
        let mut root_entries = Vec::new();
        let mut subdirs: BTreeMap<String, Vec<(String, String, String, String)>> = BTreeMap::new();

        for (path, (oid, _)) in &self.blobs {
            if let Some((dir, file)) = path.split_once('/') {
                subdirs.entry(dir.to_string()).or_default().push((
                    "100644".to_string(),
                    "blob".to_string(),
                    oid.clone(),
                    file.to_string(),
                ));
            } else {
                root_entries.push((
                    "100644".to_string(),
                    "blob".to_string(),
                    oid.clone(),
                    path.clone(),
                ));
            }
        }
        for (dir, entries) in subdirs {
            let subtree_oid = git.mktree(&entries).map_err(git_err)?;
            root_entries.push((
                "040000".to_string(),
                "tree".to_string(),
                subtree_oid,
                dir,
            ));
        }
        root_entries.sort_by(|a, b| a.3.cmp(&b.3));
        git.mktree(&root_entries).map_err(git_err)
    }
}

/// Pull new revisions and agent events from `<remote>/<branch>` into `store`.
pub fn pull_state<S: StateStore + AgentRegistry + ?Sized>(
    store: &S,
    target: &SyncTarget,
) -> Result<SyncReport, StoreError> {
    let Some(remote_head) = target
        .git
        .ls_remote_branch(&target.remote, &target.branch)
        .map_err(git_err)?
    else {
        return Ok(SyncReport::default());
    };

    target
        .git
        .fetch_branch(&target.remote, &target.branch, REMOTE_TRACKING_REF)
        .map_err(git_err)?;

    let mut cursor = store
        .get_sync_cursor(&target.cursor_key)?
        .unwrap_or_default();

    if cursor.remote_commit.as_deref() == Some(remote_head.as_str()) {
        return Ok(SyncReport {
            head_commit: Some(remote_head),
            ..SyncReport::default()
        });
    }

    if let Some(ref prev_commit) = cursor.remote_commit {
        let is_ff = target
            .git
            .is_ancestor(prev_commit, &remote_head)
            .map_err(git_err)?;
        if !is_ff {
            return Err(StoreError::Tampered {
                id: fl_core::ids::seq_iri(0),
                detail: format!(
                    "remote `{}` branch `{}` non-fast-forwarded from `{prev_commit}` to `{remote_head}`",
                    target.remote, target.branch
                ),
            });
        }
    }

    let head_tree = TreeImage::load(&target.git, &remote_head)?;
    head_tree.validate_layout()?;

    // Verify existing segments were only appended to by comparing against `cursor.segment_offsets`.
    let mut new_revs = Vec::new();
    let mut new_events = Vec::new();

    for (path, (_, bytes)) in &head_tree.blobs {
        if path == "format.json" {
            continue;
        }
        let (prev_off, prev_sha) = match cursor.segment_offsets.get(path) {
            Some((off, sha)) => (*off as usize, Some(sha.as_str())),
            None => (0usize, None),
        };
        if bytes.len() < prev_off {
            return Err(StoreError::Tampered {
                id: fl_core::ids::seq_iri(0),
                detail: format!("segment `{path}` shrank from {prev_off} to {} bytes", bytes.len()),
            });
        }
        if let Some(expected_sha) = prev_sha {
            let actual_sha = sha256_hex(&bytes[..prev_off]);
            if actual_sha != expected_sha {
                return Err(StoreError::Tampered {
                    id: fl_core::ids::seq_iri(0),
                    detail: format!("segment `{path}` prefix sha256 mismatch"),
                });
            }
        }
        let tail = std::str::from_utf8(&bytes[prev_off..])
            .map_err(|e| StoreError::Backend(format!("segment `{path}` is not valid UTF-8: {e}")))?;
        for (idx, line) in tail.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if path.starts_with("revisions/") {
                let rev: StateRevision = serde_json::from_str(line).map_err(|e| {
                    StoreError::Backend(format!("malformed revision at {path}:{}: {e}", idx + 1))
                })?;
                if target
                    .scope_filter
                    .as_ref()
                    .is_none_or(|sc| &rev.scope == sc)
                {
                    new_revs.push(rev);
                }
            } else if path.starts_with("agents/") && target.scope_filter.is_none() {
                let ev: RegistryEvent = serde_json::from_str(line).map_err(|e| {
                    StoreError::Backend(format!("malformed agent event at {path}:{}: {e}", idx + 1))
                })?;
                new_events.push(ev);
            }
        }
        cursor
            .segment_offsets
            .insert(path.clone(), (bytes.len() as u64, sha256_hex(bytes)));
    }

    let pulled_agent_events = new_events.len();
    let pulled_revisions = new_revs.len();
    if !new_events.is_empty() {
        store.import_registry_events(&new_events)?;
    }
    if !new_revs.is_empty() {
        store.import_state_revisions(&new_revs)?;
    }

    // Advance local_rev_seq and local_agent_event_seq to cover any revisions/events now in store
    // that came from the remote so a subsequent push does not re-append them.
    if let Some((last_rev_seq, _)) = store.revisions_after(cursor.local_rev_seq)?.last() {
        cursor.local_rev_seq = *last_rev_seq;
    }
    if target.scope_filter.is_none()
        && let Some((last_ev_seq, _)) = store
            .registry_events_after(cursor.local_agent_event_seq)?
            .last()
    {
        cursor.local_agent_event_seq = *last_ev_seq;
    }

    cursor.remote_commit = Some(remote_head.clone());
    store.set_sync_cursor(&target.cursor_key, &cursor)?;

    Ok(SyncReport {
        pulled_revisions,
        pulled_agent_events,
        pushed_revisions: 0,
        pushed_agent_events: 0,
        head_commit: Some(remote_head),
        cas_attempts: 0,
    })
}

/// Push local state revisions and agent events since the cursor to `<remote>/<branch>`.
/// Performs the pull step first on each attempt and retries up to `MAX_CAS_TRIES` (5) times
/// on `--force-with-lease` CAS contention.
pub fn sync_state<S: StateStore + AgentRegistry + ?Sized>(
    store: &S,
    target: &SyncTarget,
) -> Result<SyncReport, StoreError> {
    let mut total_pulled_revs = 0;
    let mut total_pulled_events = 0;

    for attempt in 1..=MAX_CAS_TRIES {
        // Save the local sequences before pull so any local revisions created prior to pull
        // (plus any auto-merge revisions created during pull) are identified cleanly.
        let cursor_before_pull = store
            .get_sync_cursor(&target.cursor_key)?
            .unwrap_or_default();
        let pre_pull_rev_seq = cursor_before_pull.local_rev_seq;
        let pre_pull_ev_seq = cursor_before_pull.local_agent_event_seq;

        let pull_rep = pull_state_preserving_unpushed_seqs(
            store,
            target,
            pre_pull_rev_seq,
            pre_pull_ev_seq,
        )?;
        total_pulled_revs += pull_rep.pulled_revisions;
        total_pulled_events += pull_rep.pulled_agent_events;

        let mut cursor = store
            .get_sync_cursor(&target.cursor_key)?
            .unwrap_or_default();

        // Collect all revisions after `cursor.local_rev_seq` that match `target.scope_filter`
        // and are not already present in the remote tree.
        let mut tree = match &cursor.remote_commit {
            Some(commit) => TreeImage::load(&target.git, commit)?,
            None => TreeImage::default(),
        };

        let existing_rev_ids = collect_existing_rev_ids(&tree)?;
        let existing_event_ids = collect_existing_event_ids(&tree)?;

        let candidate_revs = store.revisions_after(cursor.local_rev_seq)?;
        let mut max_rev_seq = cursor.local_rev_seq;
        let mut revs_to_push = Vec::new();
        for (seq, rev) in candidate_revs {
            max_rev_seq = max_rev_seq.max(seq);
            if target
                .scope_filter
                .as_ref()
                .is_some_and(|sc| &rev.scope != sc)
            {
                continue;
            }
            if !existing_rev_ids.contains(&rev.id.0.to_string()) {
                revs_to_push.push(rev);
            }
        }

        let mut max_ev_seq = cursor.local_agent_event_seq;
        let mut events_to_push = Vec::new();
        if target.scope_filter.is_none() {
            let candidate_events = store.registry_events_after(cursor.local_agent_event_seq)?;
            for (seq, ev) in candidate_events {
                max_ev_seq = max_ev_seq.max(seq);
                if !existing_event_ids.contains(&ev.event_id.to_string()) {
                    events_to_push.push(ev);
                }
            }
        }

        // Sensitivity pre-flight check (spec §4.5): BEFORE writing any git blob or pushing!
        if target.visibility != SyncTargetVisibility::Private {
            for rev in &revs_to_push {
                let entry_ever_sens = store
                    .get_state(&rev.entry_id)?
                    .is_some_and(|e| e.sensitive || e.ever_sensitive);
                if rev.sensitive || entry_ever_sens {
                    return Err(StoreError::SensitiveDisclosure {
                        entry_id: rev.entry_id.clone(),
                        remote: target.remote.clone(),
                    });
                }
            }
        }

        // No-op push check (spec §4.3 step 2): if nothing to push, advance local seqs and return.
        if revs_to_push.is_empty() && events_to_push.is_empty() {
            if cursor.local_rev_seq != max_rev_seq || cursor.local_agent_event_seq != max_ev_seq {
                cursor.local_rev_seq = max_rev_seq;
                cursor.local_agent_event_seq = max_ev_seq;
                store.set_sync_cursor(&target.cursor_key, &cursor)?;
            }
            return Ok(SyncReport {
                pulled_revisions: total_pulled_revs,
                pulled_agent_events: total_pulled_events,
                pushed_revisions: 0,
                pushed_agent_events: 0,
                head_commit: cursor.remote_commit,
                cas_attempts: 0,
            });
        }

        let rev_lines: Vec<String> = revs_to_push
            .iter()
            .map(|r| serde_json::to_string(r).expect("StateRevision serializes"))
            .collect();
        let ev_lines: Vec<String> = events_to_push
            .iter()
            .map(|e| serde_json::to_string(e).expect("RegistryEvent serializes"))
            .collect();

        tree.append_jsonl_records(&target.git, "revisions", &rev_lines)?;
        tree.append_jsonl_records(&target.git, "agents", &ev_lines)?;
        let tree_oid = tree.write_tree(&target.git)?;
        let msg = format!(
            "fl state sync: {} revision(s), {} agent event(s)",
            revs_to_push.len(),
            events_to_push.len()
        );
        let commit_oid = target
            .git
            .commit_tree(
                &tree_oid,
                cursor.remote_commit.as_deref(),
                &msg,
                &target.author_name,
                &target.author_email,
            )
            .map_err(git_err)?;

        let dest_ref = format!("refs/heads/{}", target.branch);
        let pushed = target
            .git
            .push_cas(
                &target.remote,
                &commit_oid,
                &dest_ref,
                cursor.remote_commit.as_deref(),
            )
            .map_err(git_err)?;

        if pushed {
            for (path, (_, bytes)) in &tree.blobs {
                if path != "format.json" {
                    cursor
                        .segment_offsets
                        .insert(path.clone(), (bytes.len() as u64, sha256_hex(bytes)));
                }
            }
            cursor.remote_commit = Some(commit_oid.clone());
            cursor.local_rev_seq = max_rev_seq;
            cursor.local_agent_event_seq = max_ev_seq;
            store.set_sync_cursor(&target.cursor_key, &cursor)?;
            return Ok(SyncReport {
                pulled_revisions: total_pulled_revs,
                pulled_agent_events: total_pulled_events,
                pushed_revisions: revs_to_push.len(),
                pushed_agent_events: events_to_push.len(),
                head_commit: Some(commit_oid),
                cas_attempts: attempt,
            });
        }
    }

    Err(StoreError::Backend(format!(
        "failed to push `{}` to `{}` after {MAX_CAS_TRIES} CAS attempts",
        target.branch, target.remote
    )))
}

/// Alias for `sync_state` when invoked from `fl state push`.
pub fn push_state<S: StateStore + AgentRegistry + ?Sized>(
    store: &S,
    target: &SyncTarget,
) -> Result<SyncReport, StoreError> {
    sync_state(store, target)
}

fn pull_state_preserving_unpushed_seqs<S: StateStore + AgentRegistry + ?Sized>(
    store: &S,
    target: &SyncTarget,
    pre_pull_rev_seq: u64,
    pre_pull_ev_seq: u64,
) -> Result<SyncReport, StoreError> {
    let rep = pull_state(store, target)?;
    let mut cursor = store
        .get_sync_cursor(&target.cursor_key)?
        .unwrap_or_default();
    cursor.local_rev_seq = pre_pull_rev_seq;
    cursor.local_agent_event_seq = pre_pull_ev_seq;
    store.set_sync_cursor(&target.cursor_key, &cursor)?;
    Ok(rep)
}

fn collect_existing_rev_ids(
    tree: &TreeImage,
) -> Result<std::collections::BTreeSet<String>, StoreError> {
    let mut ids = std::collections::BTreeSet::new();
    for (path, (_, bytes)) in &tree.blobs {
        if !path.starts_with("revisions/") {
            continue;
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|e| StoreError::Backend(format!("invalid UTF-8 in {path}: {e}")))?;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let rev: StateRevision = serde_json::from_str(line)
                .map_err(|e| StoreError::Backend(format!("invalid revision in {path}: {e}")))?;
            ids.insert(rev.id.0.to_string());
        }
    }
    Ok(ids)
}

fn collect_existing_event_ids(
    tree: &TreeImage,
) -> Result<std::collections::BTreeSet<String>, StoreError> {
    let mut ids = std::collections::BTreeSet::new();
    for (path, (_, bytes)) in &tree.blobs {
        if !path.starts_with("agents/") {
            continue;
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|e| StoreError::Backend(format!("invalid UTF-8 in {path}: {e}")))?;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let ev: RegistryEvent = serde_json::from_str(line)
                .map_err(|e| StoreError::Backend(format!("invalid event in {path}: {e}")))?;
            ids.insert(ev.event_id.to_string());
        }
    }
    Ok(ids)
}

/// Full integrity audit of `<remote>/<branch>` (spec §4.4):
/// walks the first-parent commit chain from root to tip, verifies every commit
/// only appends to existing segments or adds new segment files, parses all JSONL lines,
/// checks `RevisionId` parent references and `StateId` key immutability, and checks
/// the local store's cursor against the remote.
pub fn verify_state<S: StateStore + AgentRegistry + ?Sized>(
    store: &S,
    target: &SyncTarget,
) -> Result<VerifyReport, StoreError> {
    let Some(remote_head) = target
        .git
        .ls_remote_branch(&target.remote, &target.branch)
        .map_err(git_err)?
    else {
        return Ok(VerifyReport::default());
    };

    target
        .git
        .fetch_branch(&target.remote, &target.branch, REMOTE_TRACKING_REF)
        .map_err(git_err)?;

    let commits = target
        .git
        .rev_list_first_parent(&remote_head)
        .map_err(git_err)?;
    let mut prev_tree: Option<TreeImage> = None;
    for commit in &commits {
        let tree = TreeImage::load(&target.git, commit)?;
        tree.validate_layout()?;
        if let Some(ref prev) = prev_tree {
            tree.verify_append_only_since(prev)?;
        }
        prev_tree = Some(tree);
    }

    let tip = prev_tree.unwrap_or_default();
    let mut seen_revs = BTreeMap::new();
    let mut entry_keys: BTreeMap<String, (StateScope, fl_core::StateConcern, String)> =
        BTreeMap::new();
    let mut verified_segments = 0usize;
    let mut verified_revisions = 0usize;
    let mut verified_agent_events = 0usize;

    for (path, (_, bytes)) in &tip.blobs {
        if path == "format.json" {
            continue;
        }
        verified_segments += 1;
        let text = std::str::from_utf8(bytes)
            .map_err(|e| StoreError::Backend(format!("segment `{path}` invalid UTF-8: {e}")))?;
        for (line_no, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if path.starts_with("revisions/") {
                let rev: StateRevision = serde_json::from_str(line).map_err(|e| {
                    StoreError::Backend(format!("malformed revision at {path}:{}: {e}", line_no + 1))
                })?;
                for parent in &rev.parents {
                    if !seen_revs.contains_key(parent) {
                        return Err(StoreError::InvalidState(format!(
                            "revision `{}` references unknown parent `{parent}`",
                            rev.id
                        )));
                    }
                }
                let key_triple = (rev.scope.clone(), rev.concern, rev.slug.clone());
                if let Some(prev_key) = entry_keys.get(rev.entry_id.0.as_str()) {
                    if prev_key != &key_triple {
                        return Err(StoreError::InvalidState(format!(
                            "entry `{}` changed immutable key in `{}`",
                            rev.entry_id, rev.id
                        )));
                    }
                } else {
                    entry_keys.insert(rev.entry_id.0.to_string(), key_triple);
                }
                seen_revs.insert(rev.id.clone(), rev);
                verified_revisions += 1;
            } else if path.starts_with("agents/") {
                let _ev: RegistryEvent = serde_json::from_str(line).map_err(|e| {
                    StoreError::Backend(format!(
                        "malformed registry event at {path}:{}: {e}",
                        line_no + 1
                    ))
                })?;
                verified_agent_events += 1;
            }
        }
    }

    if let Some(cursor) = store.get_sync_cursor(&target.cursor_key)?
        && let Some(ref c_commit) = cursor.remote_commit
    {
        let ok = target
            .git
            .is_ancestor(c_commit, &remote_head)
            .map_err(git_err)?;
        if !ok {
            return Err(StoreError::Tampered {
                id: fl_core::ids::seq_iri(0),
                detail: format!(
                    "local cursor commit `{c_commit}` is not an ancestor of `{remote_head}`"
                ),
            });
        }
    }

    Ok(VerifyReport {
        verified_commits: commits.len(),
        verified_segments,
        verified_revisions,
        verified_agent_events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::agent::{AgentRole, RegisterAgentArgs};
    use fl_core::state::{HandoffEntry, SensitivityPatch, StatePayload, UpsertStateArgs};
    use fl_core::{At, MemStore};
    use std::process::Command;
    use tempfile::TempDir;

    fn setup_bare_and_clone() -> (TempDir, Git, String) {
        let dir = TempDir::new().unwrap();
        let bare = dir.path().join("remote.git");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        Command::new("git")
            .args(["init", "--bare", bare.to_str().unwrap()])
            .output()
            .unwrap();
        Command::new("git")
            .args(["init", work.to_str().unwrap()])
            .output()
            .unwrap();
        (
            dir,
            Git::new(&work),
            bare.to_str().unwrap().to_string(),
        )
    }

    #[test]
    fn push_pull_and_verify_round_trip_without_touching_worktree() {
        let (_dir, git, remote) = setup_bare_and_clone();
        let store_a = MemStore::default();
        store_a
            .register_agent(RegisterAgentArgs {
                display_name: "Ferris".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Lead,
                extra_areas: vec![],
                create_area: true,
                replace: false,
                at: At::from_unix_millis(10),
                by: "owner".into(),
            })
            .unwrap();
        store_a
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "handoff".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "shipped".into(),
                    shipped: vec!["core".into()],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec![],
                    notes: None,
                }),
                at: At::from_unix_millis(20),
                by: "ferris".into(),
            })
            .unwrap();

        let target = SyncTarget {
            cursor_key: "workspace".into(),
            scope_filter: None,
            git: git.clone(),
            remote: remote.clone(),
            branch: STATE_BRANCH.into(),
            visibility: SyncTargetVisibility::Private,
            author_name: "Ferris".into(),
            author_email: "ferris@artificialhumanity.io".into(),
        };

        let push_rep = sync_state(&store_a, &target).unwrap();
        assert_eq!(push_rep.pushed_revisions, 1);
        assert_eq!(push_rep.pushed_agent_events, 2); // area_upsert + agent_upsert

        // No-op second sync creates no new commit.
        let noop_rep = sync_state(&store_a, &target).unwrap();
        assert_eq!(noop_rep.pushed_revisions, 0);
        assert_eq!(noop_rep.head_commit, push_rep.head_commit);

        // Fresh store B pulls everything.
        let store_b = MemStore::default();
        let pull_rep = pull_state(&store_b, &target).unwrap();
        assert_eq!(pull_rep.pulled_revisions, 1);
        assert_eq!(pull_rep.pulled_agent_events, 2);
        assert!(store_b.agent_by_name("ferris").unwrap().is_some());
        assert!(
            store_b
                .state_by_key(&StateScope::Workspace, fl_core::StateConcern::Handoff, "handoff")
                .unwrap()
                .is_some()
        );

        let verify_rep = verify_state(&store_b, &target).unwrap();
        assert_eq!(verify_rep.verified_commits, 1);
        assert_eq!(verify_rep.verified_revisions, 1);
        assert_eq!(verify_rep.verified_agent_events, 2);
    }

    #[test]
    fn sensitive_disclosure_blocks_push_to_unconfirmed_remote_even_after_clear() {
        let (_dir, git, remote) = setup_bare_and_clone();
        let store = MemStore::default();
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "secret".into(),
                sensitivity: SensitivityPatch::Set(true),
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "confidential".into(),
                    shipped: vec![],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec![],
                    notes: None,
                }),
                at: At::from_unix_millis(10),
                by: "ferris".into(),
            })
            .unwrap();
        // Clear sensitive on head; ever_sensitive remains true and revision 1 is sensitive!
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "secret".into(),
                sensitivity: SensitivityPatch::Set(false),
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "redacted".into(),
                    shipped: vec![],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec![],
                    notes: None,
                }),
                at: At::from_unix_millis(20),
                by: "ferris".into(),
            })
            .unwrap();

        let target = SyncTarget {
            cursor_key: "workspace".into(),
            scope_filter: None,
            git,
            remote,
            branch: STATE_BRANCH.into(),
            visibility: SyncTargetVisibility::NotConfirmedPrivate,
            author_name: "Ferris".into(),
            author_email: "ferris@artificialhumanity.io".into(),
        };
        assert!(matches!(
            sync_state(&store, &target),
            Err(StoreError::SensitiveDisclosure { .. })
        ));
    }
}
```

- [ ] **Step 4: Run the verification trio and commit Task 4**

Run:
```bash
cargo test --workspace
cargo clippy --all-targets --workspace -- -D warnings
cargo fmt --all --check
git add crates/exec
git commit -m "feat(exec): add fl/state git plumbing sync and integrity verification

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

### Task 5: `fl-cli` `[state]` and `[[project]].state` config and store routing (`fl-cli`)

**Files:**
- Modify: `crates/cli/Cargo.toml`
- Modify: `crates/cli/src/config.rs`
- Modify: `crates/cli/src/refs.rs`

**Interfaces:**
- Consumes: `Kind::State`, `Kind::Agent` from `fl-core`.
- Produces:
  - `StateConfig` (`store`, `remote`, `branch`) and `ProjectStateConfig` (`remote`, `branch`) in `crates/cli/src/config.rs`.
  - Optional `name: Option<String>` and `state: Option<ProjectStateConfig>` on `config::Entry`.
  - `Config::resolve_state_store_path` and `Config::resolve_project_slug` helpers.
  - `refs::show` support for `Kind::State` and `Kind::Agent`.

- [ ] **Step 1: Add `tempfile.workspace = true` to `crates/cli/Cargo.toml` and update `crates/cli/src/refs.rs`**

In `crates/cli/Cargo.toml`, add `tempfile.workspace = true` under `[dependencies]`.
Verify `crates/cli/src/refs.rs` handles all `Kind` variants without wildcard gaps (it already passes `kind` directly to `handles.handle_of(kind, id)` and `handles.resolve_handle(kind, n)`).

- [ ] **Step 2: Add `StateConfig`, `ProjectStateConfig`, `Entry.name`, and `Entry.state` to `crates/cli/src/config.rs`**

In `crates/cli/src/config.rs`:
1. Add `StateConfig` and `ProjectStateConfig`:

```rust
/// Workspace-level state database and `fl/state` remote configuration (state spec §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StateConfig {
    /// Optional path to the shared state `.redb` store (`~/` expanded).
    pub store: Option<PathBuf>,
    /// Git remote URL or local repository path hosting workspace `refs/heads/fl/state`.
    pub remote: Option<String>,
    /// Branch name (defaults to `"fl/state"`).
    pub branch: String,
}

/// Per-project `fl/state` remote override under `[[project]]` (state spec §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectStateConfig {
    /// Git remote name (e.g. `"origin"`), URL, or path.
    pub remote: Option<String>,
    /// Branch name (defaults to `"fl/state"`).
    pub branch: String,
}
```

2. Add `pub state: Option<StateConfig>` to `pub struct Config`, and add `pub name: Option<String>` and `pub state: Option<ProjectStateConfig>` to `pub struct Entry`.
3. Update `RawFile` and `RawProject` in `crates/cli/src/config.rs` with `#[serde(deny_unknown_fields)]`:

```rust
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    github: Option<RawGithub>,
    state: Option<RawState>,
    #[serde(default)]
    project: Vec<RawProject>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    store: Option<String>,
    remote: Option<String>,
    branch: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    root: String,
    name: Option<String>,
    tracker: Option<String>,
    github: Option<RawProjectGithub>,
    ledger: Option<RawProjectLedger>,
    routing: Option<RawRouting>,
    state: Option<RawProjectState>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectState {
    remote: Option<String>,
    branch: Option<String>,
}
```

4. In `parse(text: &str, home: Option<&Path>) -> Result<Config, ConfigError>`, parse `raw.state` (expanding `~/` on `state.store` via `expand_home`, rejecting empty `remote` or `branch`, defaulting `branch` to `"fl/state"`) and `p.name` (validating via `fl_core::state::validate_slug` when present) and `p.state` (defaulting `branch` to `"fl/state"`).
5. Add unit tests in `crates/cli/src/config.rs` covering `[state]` and `[[project]]` `name` + `state = { remote = "origin" }` parsing and `deny_unknown_fields` rejection.

- [ ] **Step 3: Run `cargo test -p fl-cli` and commit Task 5**

Run:
```bash
cargo test -p fl-cli
cargo clippy -p fl-cli --all-targets -- -D warnings
cargo fmt --all --check
git add crates/cli/Cargo.toml crates/cli/src/config.rs
git commit -m "feat(cli): add [state] and [[project]].state configuration

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

### Task 6: `fl agent` and `fl state` CLI subcommands and Markdown exports (`fl-cli`)

**Files:**
- Create: `crates/cli/src/cmd/agent.rs`
- Create: `crates/cli/src/cmd/state.rs`
- Modify: `crates/cli/src/cmd/mod.rs`
- Modify: `crates/cli/src/main.rs`
- Create: `crates/cli/tests/state_sync.rs`

**Interfaces:**
- Consumes: `StateStore`, `AgentRegistry`, `fl_exec::state_sync`, `Config`.
- Produces:
  - `fl agent {area, register, retire, show, roster, export}` in `crates/cli/src/cmd/agent.rs`.
  - `fl state {memory, handoff, runtime, get, list, history, conflicts, resolve, export, pull, push, sync, verify, mcp}` in `crates/cli/src/cmd/state.rs`.
  - Deterministic Markdown renderers (`render_registry_markdown`, `render_memory_markdown`, `render_state_markdown`).
  - Exit code `2` in `main()` when `StoreError::AgentCollision` or `StoreError::LeadTaken` surfaces.

- [ ] **Step 1: Create `crates/cli/src/cmd/agent.rs` with CLI subcommands and `REGISTRY.md` export**

Create `crates/cli/src/cmd/agent.rs`:

```rust
use crate::refs;
use clap::{Args, Subcommand};
use fl_core::agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, reports_to,
};
use fl_core::state::validate_slug;
use fl_core::{AgentId, AgentRegistry, At, Handles, Kind, StoreError};
use std::path::PathBuf;

#[derive(Args)]
pub struct AgentCmd {
    #[command(subcommand)]
    pub action: AgentAction,
}

#[derive(Subcommand)]
pub enum AgentAction {
    /// Manage coverage areas / projects in the agent registry
    Area {
        #[command(subcommand)]
        action: AreaAction,
    },
    /// Register an agent identity, primary project, and role (exit code 2 on collision)
    Register {
        /// Display name (e.g. "Ferris")
        name: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        role: String,
        #[arg(long)]
        email: Option<String>,
        #[arg(long = "extra-area")]
        extra_areas: Vec<String>,
        #[arg(long)]
        create_area: bool,
        #[arg(long)]
        replace: bool,
        #[arg(long, default_value = "cli")]
        by: String,
    },
    /// Retire an active agent
    Retire {
        /// Agent name, numeric handle, or IRI
        agent: String,
        #[arg(long, default_value = "cli")]
        by: String,
    },
    /// Show one agent record
    Show {
        /// Agent name, numeric handle, or IRI
        agent: String,
    },
    /// List registered agents
    Roster {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        include_retired: bool,
    },
    /// Export deterministic REGISTRY.md
    Export {
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum AreaAction {
    /// Create or update a coverage area
    Set {
        key: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        purpose: String,
        #[arg(long, default_value = "cli")]
        by: String,
    },
    /// List coverage areas
    List,
}

pub fn resolve_agent_ref<S: AgentRegistry + Handles + ?Sized>(
    store: &S,
    raw: &str,
) -> Result<AgentRecord, Box<dyn std::error::Error>> {
    if let Ok(key) = validate_slug(raw)
        && let Some(rec) = store.agent_by_name(&key)?
    {
        return Ok(rec);
    }
    let iri = refs::parse(store, Kind::Agent, raw)?;
    store
        .get_agent(&AgentId(iri))?
        .ok_or_else(|| StoreError::NoSuchAgent(raw.to_string()).into())
}

pub fn render_registry_markdown(areas: &[AreaRecord], agents: &[AgentRecord]) -> String {
    let active: Vec<AgentRecord> = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Active)
        .cloned()
        .collect();
    let retired: Vec<AgentRecord> = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Retired)
        .cloned()
        .collect();

    let mut out = String::new();
    out.push_str("# Artificial Humanity — Agent & Project Registry\n\n");
    out.push_str(
        "> **Auto-generated from the FerroLoop State Database (`fl agent export`).**\n\n",
    );
    out.push_str("## Projects & Coverage Areas\n\n");
    out.push_str("| Project | Path | Lead | Residents | Purpose |\n");
    out.push_str("| --- | --- | --- | --- | --- |\n");

    for area in areas {
        let path_col = area
            .path
            .as_deref()
            .map(|p| format!("`{p}`"))
            .unwrap_or_else(|| "—".into());
        let lead_col = active
            .iter()
            .find(|a| {
                a.project == area.area_key
                    && matches!(a.role, AgentRole::Lead | AgentRole::LabManager)
            })
            .map(|a| format!("{} (`{}`)", a.display_name, a.name_key))
            .unwrap_or_else(|| "—".into());
        let residents: Vec<String> = active
            .iter()
            .filter(|a| a.project == area.area_key || a.extra_areas.contains(&area.area_key))
            .map(|a| format!("`{}` ({})", a.name_key, a.role.as_wire()))
            .collect();
        let res_col = if residents.is_empty() {
            "—".into()
        } else {
            residents.join(", ")
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            area.name, path_col, lead_col, res_col, area.purpose
        ));
    }

    out.push_str("\n## Registered Agents\n\n");
    out.push_str(
        "| Agent | Persona Key | Primary Project | Role | Extra Coverage | Reports To | Co-Author Trailer |\n",
    );
    out.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");

    let area_name = |key: &str| -> String {
        areas
            .iter()
            .find(|a| a.area_key == key)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| key.to_string())
    };

    for a in &active {
        let extra = if a.extra_areas.is_empty() {
            "—".into()
        } else {
            a.extra_areas
                .iter()
                .map(|k| area_name(k))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let rep = reports_to(a, &active);
        out.push_str(&format!(
            "| {} | `{}` | {} | `{}` | {} | {} | `{}` |\n",
            a.display_name,
            a.name_key,
            area_name(&a.project),
            a.role.as_wire(),
            extra,
            rep,
            a.co_author_trailer()
        ));
    }

    if !retired.is_empty() {
        out.push_str("\n## Retired Agents\n\n");
        out.push_str("| Agent | Persona Key | Primary Project | Role | Co-Author Trailer |\n");
        out.push_str("| --- | --- | --- | --- | --- |\n");
        for a in &retired {
            out.push_str(&format!(
                "| {} | `{}` | {} | `{}` | `{}` |\n",
                a.display_name,
                a.name_key,
                area_name(&a.project),
                a.role.as_wire(),
                a.co_author_trailer()
            ));
        }
    }
    out
}

pub fn run<S: AgentRegistry + Handles + ?Sized>(
    store: &S,
    cmd: AgentCmd,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd.action {
        AgentAction::Area { action } => match action {
            AreaAction::Set {
                key,
                name,
                path,
                purpose,
                by,
            } => {
                let area = store.upsert_area(AreaRecord {
                    area_key: key,
                    name,
                    path,
                    purpose,
                    updated_at: At::now(),
                    by,
                })?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&area)?);
                } else {
                    println!("area `{}` ({}) saved", area.area_key, area.name);
                }
            }
            AreaAction::List => {
                let areas = store.list_areas()?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&areas)?);
                } else {
                    for a in areas {
                        println!(
                            "{}\t{}\t{}\t{}",
                            a.area_key,
                            a.name,
                            a.path.as_deref().unwrap_or("-"),
                            a.purpose
                        );
                    }
                }
            }
        },
        AgentAction::Register {
            name,
            project,
            role,
            email,
            extra_areas,
            create_area,
            replace,
            by,
        } => {
            let parsed_role = AgentRole::parse_wire(&role)?;
            let rec = store.register_agent(RegisterAgentArgs {
                display_name: name,
                email,
                project,
                role: parsed_role,
                extra_areas,
                create_area,
                replace,
                at: At::now(),
                by,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rec)?);
            } else {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    refs::show(store, Kind::Agent, rec.id.iri())?,
                    rec.display_name,
                    rec.project,
                    rec.role.as_wire(),
                    rec.co_author_trailer()
                );
            }
        }
        AgentAction::Retire { agent, by } => {
            let target = resolve_agent_ref(store, &agent)?;
            let rec = store.retire_agent(&target.id, At::now(), &by)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rec)?);
            } else {
                println!("retired `{}` ({})", rec.name_key, rec.id);
            }
        }
        AgentAction::Show { agent } => {
            let rec = resolve_agent_ref(store, &agent)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rec)?);
            } else {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    refs::show(store, Kind::Agent, rec.id.iri())?,
                    rec.display_name,
                    rec.project,
                    rec.role.as_wire(),
                    rec.status.as_wire(),
                    rec.co_author_trailer()
                );
            }
        }
        AgentAction::Roster {
            project,
            include_retired,
        } => {
            let agents = store.list_agents(project.as_deref(), include_retired)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&agents)?);
            } else {
                let areas = store.list_areas()?;
                print!("{}", render_registry_markdown(&areas, &agents));
            }
        }
        AgentAction::Export { out } => {
            let areas = store.list_areas()?;
            let agents = store.list_agents(None, true)?;
            let md = render_registry_markdown(&areas, &agents);
            if let Some(path) = out {
                std::fs::write(path, md)?;
            } else {
                print!("{md}");
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Create `crates/cli/src/cmd/state.rs` with CLI subcommands, `MEMORY.md` & `STATE.md` exports, and git sync wrappers**

Create `crates/cli/src/cmd/state.rs`:

```rust
use crate::config::Config;
use crate::refs;
use clap::{Args, Subcommand};
use fl_core::state::{
    HandoffEntry, MemoryEntry, MemoryKind, MemoryStatus, MergeMode, RuntimeEntry, SensitivityPatch,
    StateConcern, StateEntry, StateFilter, StatePayload, StateScope, UpsertStateArgs,
    validate_slug,
};
use fl_core::{AgentRegistry, At, Handles, Kind, StateId, StateStore, StoreError};
use fl_exec::git::Git;
use fl_exec::state_sync::{
    STATE_BRANCH, SyncTarget, SyncTargetVisibility, pull_state, push_state, sync_state,
    verify_state,
};
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct StateCmd {
    #[command(subcommand)]
    pub action: StateAction,
}

#[derive(Subcommand)]
pub enum StateAction {
    /// Write or update a memory entry
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// Write or update a handoff entry
    Handoff {
        #[command(subcommand)]
        action: HandoffAction,
    },
    /// Write or update a runtime entry
    Runtime {
        #[command(subcommand)]
        action: RuntimeAction,
    },
    /// Get a state entry by StateId, numeric handle, or --slug
    Get {
        id: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        concern: Option<String>,
        #[arg(long)]
        slug: Option<String>,
    },
    /// List state entries matching filters
    List {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        concern: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        include_archived: bool,
    },
    /// List revision DAG history for an entry
    History {
        entry: String,
    },
    /// List unresolved state conflicts
    Conflicts {
        #[arg(long)]
        scope: Option<String>,
    },
    /// Resolve an entry conflict with a 2-parent merge revision
    Resolve {
        entry: String,
        #[arg(long)]
        take: Option<String>,
        #[arg(long)]
        payload_file: Option<PathBuf>,
        #[arg(long)]
        sensitive: Option<bool>,
        #[arg(long, default_value = "cli")]
        by: String,
    },
    /// Export deterministic MEMORY.md or STATE.md
    Export {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        concern: String,
        #[arg(long)]
        include_sensitive: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Pull state revisions and agent events from fl/state
    Pull {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        remote: Option<String>,
    },
    /// Push local state revisions and agent events to fl/state
    Push {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        allow_sensitive: bool,
    },
    /// Pull, merge, and push state revisions and agent events on fl/state
    Sync {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        allow_sensitive: bool,
    },
    /// Verify the integrity of the remote fl/state log
    Verify {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        remote: Option<String>,
    },
    /// Run the JSON-RPC 2.0 MCP server for state and agent tools over stdio
    Mcp,
}

#[derive(Subcommand)]
pub enum MemoryAction {
    Set {
        slug: String,
        #[arg(long, default_value = "workspace")]
        scope: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        body_file: Option<PathBuf>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long = "source-ref")]
        source_refs: Vec<String>,
        #[arg(long, default_value = "active")]
        status: String,
        #[arg(long)]
        superseded_by: Option<String>,
        #[arg(long)]
        sensitive: Option<bool>,
        #[arg(long, default_value = "cli")]
        by: String,
    },
}

#[derive(Subcommand)]
pub enum HandoffAction {
    Set {
        #[arg(long, default_value = "current")]
        slug: String,
        #[arg(long, default_value = "workspace")]
        scope: String,
        #[arg(long)]
        summary: String,
        #[arg(long = "shipped")]
        shipped: Vec<String>,
        #[arg(long = "in-flight")]
        in_flight: Vec<String>,
        #[arg(long = "blocker")]
        blockers: Vec<String>,
        #[arg(long = "next")]
        next_steps: Vec<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        notes_file: Option<PathBuf>,
        #[arg(long)]
        sensitive: Option<bool>,
        #[arg(long, default_value = "cli")]
        by: String,
    },
}

#[derive(Subcommand)]
pub enum RuntimeAction {
    Set {
        slug: String,
        #[arg(long, default_value = "workspace")]
        scope: String,
        #[arg(long)]
        schema: String,
        #[arg(long, default_value = "conflict")]
        merge_mode: String,
        #[arg(long)]
        value: Option<String>,
        #[arg(long)]
        value_file: Option<PathBuf>,
        #[arg(long)]
        sensitive: Option<bool>,
        #[arg(long, default_value = "cli")]
        by: String,
    },
}

pub fn parse_sensitivity(opt: Option<bool>) -> SensitivityPatch {
    match opt {
        Some(v) => SensitivityPatch::Set(v),
        None => SensitivityPatch::Inherit,
    }
}

pub fn render_memory_markdown(entries: &[StateEntry], include_sensitive: bool) -> String {
    let mut out = String::new();
    out.push_str("# Shared Workspace Memory\n\n");
    out.push_str("> **Auto-generated from the FerroLoop State Database (`fl state export --concern memory`).**\n\n");

    let kinds = [
        (MemoryKind::User, "User Rules & Preferences"),
        (MemoryKind::Project, "Project Facts & Decisions"),
        (MemoryKind::Feedback, "Feedback & Validated Practices"),
        (MemoryKind::Reference, "References & External Pointers"),
    ];

    for (kind, heading) in kinds {
        let items: Vec<&StateEntry> = entries
            .iter()
            .filter(|e| include_sensitive || !e.sensitive)
            .filter(|e| match &e.payload {
                StatePayload::Memory(m) => m.kind == kind && m.status == MemoryStatus::Active,
                _ => false,
            })
            .collect();
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("## {heading}\n\n"));
        for e in items {
            if let StatePayload::Memory(m) = &e.payload {
                out.push_str(&format!("### `{}` — {}\n\n", e.slug, m.summary));
                out.push_str(&format!("{}\n\n", m.body.trim()));
                if !m.tags.is_empty() {
                    out.push_str(&format!("*Tags:* `{}`\n\n", m.tags.join("`, `")));
                }
            }
        }
    }
    out
}

pub fn render_state_markdown(entries: &[StateEntry], include_sensitive: bool) -> String {
    let mut out = String::new();
    out.push_str("# Project State & Handoffs\n\n");
    out.push_str("> **Auto-generated from the FerroLoop State Database (`fl state export --concern handoff`).**\n\n");

    for e in entries.iter().filter(|e| include_sensitive || !e.sensitive) {
        if let StatePayload::Handoff(h) = &e.payload {
            out.push_str(&format!("## `{}` — {}\n\n", e.slug, h.summary));
            out.push_str(&format!("*Updated at `{}` by `{}`*\n\n", e.updated_at, e.by));
            let sections = [
                ("Shipped", &h.shipped),
                ("In Flight", &h.in_flight),
                ("Blockers", &h.blockers),
                ("Next Steps", &h.next_steps),
            ];
            for (title, list) in sections {
                if !list.is_empty() {
                    out.push_str(&format!("### {title}\n\n"));
                    for item in list {
                        out.push_str(&format!("- {item}\n"));
                    }
                    out.push('\n');
                }
            }
            if let Some(ref notes) = h.notes
                && !notes.trim().is_empty()
            {
                out.push_str("### Notes\n\n");
                out.push_str(&format!("{}\n\n", notes.trim()));
            }
        }
    }
    out
}

pub fn resolve_sync_target(
    cfg: &Config,
    scope_opt: Option<&str>,
    remote_override: Option<&str>,
    allow_sensitive: bool,
    scratch_root: &Path,
) -> Result<SyncTarget, Box<dyn std::error::Error>> {
    let scope = match scope_opt {
        Some(raw) => Some(StateScope::parse_wire(raw)?),
        None => None,
    };

    let (cursor_key, remote, branch, work_dir) = match &scope {
        None | Some(StateScope::Workspace) => {
            let st_cfg = cfg.state.as_ref();
            let remote = remote_override
                .map(str::to_string)
                .or_else(|| st_cfg.and_then(|s| s.remote.clone()))
                .ok_or("no workspace `[state].remote` configured and `--remote` not supplied")?;
            let branch = st_cfg
                .map(|s| s.branch.clone())
                .unwrap_or_else(|| STATE_BRANCH.to_string());
            std::fs::create_dir_all(scratch_root)?;
            let out = std::process::Command::new("git")
                .args(["init", "--bare", scratch_root.to_str().unwrap()])
                .output()?;
            if !out.status.success() {
                return Err(format!(
                    "failed to initialize bare state sync git dir: {}",
                    String::from_utf8_lossy(&out.stderr)
                )
                .into());
            }
            (
                "workspace".to_string(),
                remote,
                branch,
                scratch_root.to_path_buf(),
            )
        }
        Some(StateScope::Project(slug)) => {
            let proj_entry = cfg.project.iter().find(|p| {
                p.name.as_deref() == Some(slug.as_str())
                    || p.root
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| validate_slug(n).ok())
                        .as_deref()
                        == Some(slug.as_str())
            });
            let remote = remote_override
                .map(str::to_string)
                .or_else(|| {
                    proj_entry
                        .and_then(|p| p.state.as_ref())
                        .and_then(|s| s.remote.clone())
                })
                .or_else(|| cfg.state.as_ref().and_then(|s| s.remote.clone()))
                .unwrap_or_else(|| "origin".to_string());
            let branch = proj_entry
                .and_then(|p| p.state.as_ref())
                .map(|s| s.branch.clone())
                .unwrap_or_else(|| STATE_BRANCH.to_string());
            let dir = match proj_entry {
                Some(p) if p.root.exists() => p.root.clone(),
                _ => {
                    std::fs::create_dir_all(scratch_root)?;
                    std::process::Command::new("git")
                        .args(["init", "--bare", scratch_root.to_str().unwrap()])
                        .output()?;
                    scratch_root.to_path_buf()
                }
            };
            (format!("project:{slug}"), remote, branch, dir)
        }
    };

    let visibility = if allow_sensitive {
        SyncTargetVisibility::Private
    } else {
        SyncTargetVisibility::NotConfirmedPrivate
    };

    Ok(SyncTarget {
        cursor_key,
        scope_filter: scope,
        git: Git::new(work_dir),
        remote,
        branch,
        visibility,
        author_name: "artificially-human".to_string(),
        author_email: "machine@artificialhumanity.io".to_string(),
    })
}

pub fn run<S: StateStore + AgentRegistry + Handles + ?Sized>(
    store: &S,
    cfg: &Config,
    scratch_git_dir: &Path,
    cmd: StateCmd,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd.action {
        StateAction::Memory { action } => match action {
            MemoryAction::Set {
                slug,
                scope,
                kind,
                summary,
                body,
                body_file,
                tags,
                source_refs,
                status,
                superseded_by,
                sensitive,
                by,
            } => {
                let body_str = match (body, body_file) {
                    (Some(b), _) => b,
                    (None, Some(p)) => std::fs::read_to_string(p)?,
                    (None, None) => return Err("provide `--body` or `--body-file`".into()),
                };
                let sup_id = match superseded_by {
                    Some(raw) => Some(StateId(refs::parse(store, Kind::State, &raw)?)),
                    None => None,
                };
                let entry = store.upsert_state(UpsertStateArgs {
                    scope: StateScope::parse_wire(&scope)?,
                    slug,
                    sensitivity: parse_sensitivity(sensitive),
                    payload: StatePayload::Memory(MemoryEntry {
                        kind: MemoryKind::parse_wire(&kind)?,
                        status: MemoryStatus::parse_wire(&status)?,
                        summary,
                        body: body_str,
                        tags,
                        source_refs,
                        superseded_by: sup_id,
                    }),
                    at: At::now(),
                    by,
                })?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&entry)?);
                } else {
                    println!(
                        "{}\t{}\t{}\t{}",
                        refs::show(store, Kind::State, entry.id.iri())?,
                        entry.scope.as_key_str(),
                        entry.concern.as_wire(),
                        entry.slug
                    );
                }
            }
        },
        StateAction::Handoff { action } => match action {
            HandoffAction::Set {
                slug,
                scope,
                summary,
                shipped,
                in_flight,
                blockers,
                next_steps,
                notes,
                notes_file,
                sensitive,
                by,
            } => {
                let notes_str = match (notes, notes_file) {
                    (Some(n), _) => Some(n),
                    (None, Some(p)) => Some(std::fs::read_to_string(p)?),
                    (None, None) => None,
                };
                let entry = store.upsert_state(UpsertStateArgs {
                    scope: StateScope::parse_wire(&scope)?,
                    slug,
                    sensitivity: parse_sensitivity(sensitive),
                    payload: StatePayload::Handoff(HandoffEntry {
                        summary,
                        shipped,
                        in_flight,
                        blockers,
                        next_steps,
                        notes: notes_str,
                    }),
                    at: At::now(),
                    by,
                })?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&entry)?);
                } else {
                    println!(
                        "{}\t{}\t{}\t{}",
                        refs::show(store, Kind::State, entry.id.iri())?,
                        entry.scope.as_key_str(),
                        entry.concern.as_wire(),
                        entry.slug
                    );
                }
            }
        },
        StateAction::Runtime { action } => match action {
            RuntimeAction::Set {
                slug,
                scope,
                schema,
                merge_mode,
                value,
                value_file,
                sensitive,
                by,
            } => {
                let raw_val = match (value, value_file) {
                    (Some(v), _) => v,
                    (None, Some(p)) => std::fs::read_to_string(p)?,
                    (None, None) => return Err("provide `--value` or `--value-file`".into()),
                };
                let parsed_val: serde_json::Value = serde_json::from_str(&raw_val)?;
                let entry = store.upsert_state(UpsertStateArgs {
                    scope: StateScope::parse_wire(&scope)?,
                    slug,
                    sensitivity: parse_sensitivity(sensitive),
                    payload: StatePayload::Runtime(RuntimeEntry {
                        schema,
                        merge_mode: MergeMode::parse_wire(&merge_mode)?,
                        value: parsed_val,
                    }),
                    at: At::now(),
                    by,
                })?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&entry)?);
                } else {
                    println!(
                        "{}\t{}\t{}\t{}",
                        refs::show(store, Kind::State, entry.id.iri())?,
                        entry.scope.as_key_str(),
                        entry.concern.as_wire(),
                        entry.slug
                    );
                }
            }
        },
        StateAction::Get {
            id,
            scope,
            concern,
            slug,
        } => {
            let entry = if let Some(raw_id) = id {
                let iri = refs::parse(store, Kind::State, &raw_id)?;
                store
                    .get_state(&StateId(iri))?
                    .ok_or_else(|| StoreError::NoSuchState(raw_id))?
            } else {
                let sc = StateScope::parse_wire(scope.as_deref().unwrap_or("workspace"))?;
                let cn = StateConcern::parse_wire(
                    concern
                        .as_deref()
                        .ok_or("provide `<id>` or `--concern` + `--slug`")?,
                )?;
                let sl = slug.ok_or("provide `<id>` or `--concern` + `--slug`")?;
                store
                    .state_by_key(&sc, cn, &sl)?
                    .ok_or_else(|| StoreError::NoSuchState(sl))?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&entry)?);
            } else {
                println!(
                    "{}\t{}\t{}\t{}",
                    refs::show(store, Kind::State, entry.id.iri())?,
                    entry.scope.as_key_str(),
                    entry.concern.as_wire(),
                    entry.slug
                );
            }
        }
        StateAction::List {
            scope,
            concern,
            tag,
            include_archived,
        } => {
            let filter = StateFilter {
                scope: scope.as_deref().map(StateScope::parse_wire).transpose()?,
                concern: concern.as_deref().map(StateConcern::parse_wire).transpose()?,
                tag,
                include_archived,
            };
            let entries = store.list_state(&filter)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                for e in entries {
                    println!(
                        "{}\t{}\t{}\t{}",
                        refs::show(store, Kind::State, e.id.iri())?,
                        e.scope.as_key_str(),
                        e.concern.as_wire(),
                        e.slug
                    );
                }
            }
        }
        StateAction::History { entry } => {
            let iri = refs::parse(store, Kind::State, &entry)?;
            let revs = store.revisions_for(&StateId(iri))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&revs)?);
            } else {
                for r in revs {
                    println!("{}\t{}\t{}", r.id, r.at, r.by);
                }
            }
        }
        StateAction::Conflicts { scope } => {
            let sc = scope.as_deref().map(StateScope::parse_wire).transpose()?;
            let conflicts = store.list_conflicts(sc.as_ref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&conflicts)?);
            } else {
                for c in conflicts {
                    println!(
                        "{}\tlocal={}\tremote={}",
                        refs::show(store, Kind::State, c.entry_id.iri())?,
                        c.local_rev,
                        c.remote_rev
                    );
                }
            }
        }
        StateAction::Resolve {
            entry,
            take,
            payload_file,
            sensitive,
            by,
        } => {
            let iri = refs::parse(store, Kind::State, &entry)?;
            let entry_id = StateId(iri);
            let conflicts = store.list_conflicts(None)?;
            let conflict = conflicts
                .into_iter()
                .find(|c| c.entry_id == entry_id)
                .ok_or_else(|| format!("entry `{entry}` has no active conflict"))?;

            let payload = match (take.as_deref(), payload_file) {
                (Some("local"), None) => {
                    store
                        .get_revision(&conflict.local_rev)?
                        .ok_or("missing local_rev")?
                        .payload
                }
                (Some("remote"), None) => {
                    store
                        .get_revision(&conflict.remote_rev)?
                        .ok_or("missing remote_rev")?
                        .payload
                }
                (None, Some(path)) => {
                    let raw = std::fs::read_to_string(path)?;
                    serde_json::from_str(&raw)?
                }
                _ => return Err("specify either `--take local|remote` or `--payload-file`".into()),
            };
            let resolved = store.resolve_conflict(
                &entry_id,
                payload,
                parse_sensitivity(sensitive),
                At::now(),
                &by,
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resolved)?);
            } else {
                println!(
                    "resolved {}\thead={}",
                    refs::show(store, Kind::State, resolved.id.iri())?,
                    resolved.head_rev
                );
            }
        }
        StateAction::Export {
            scope,
            concern,
            include_sensitive,
            out,
        } => {
            let sc = StateScope::parse_wire(scope.as_deref().unwrap_or("workspace"))?;
            let cn = StateConcern::parse_wire(&concern)?;
            let entries = store.list_state(&StateFilter {
                scope: Some(sc),
                concern: Some(cn),
                tag: None,
                include_archived: false,
            })?;
            let md = match cn {
                StateConcern::Memory => render_memory_markdown(&entries, include_sensitive),
                StateConcern::Handoff => render_state_markdown(&entries, include_sensitive),
                StateConcern::Runtime => serde_json::to_string_pretty(&entries)?,
            };
            if let Some(path) = out {
                std::fs::write(path, md)?;
            } else {
                print!("{md}");
            }
        }
        StateAction::Pull { scope, remote } => {
            let target = resolve_sync_target(
                cfg,
                scope.as_deref(),
                remote.as_deref(),
                false,
                scratch_git_dir,
            )?;
            let rep = pull_state(store, &target)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                println!(
                    "pulled {} revision(s), {} agent event(s)",
                    rep.pulled_revisions, rep.pulled_agent_events
                );
            }
        }
        StateAction::Push {
            scope,
            remote,
            allow_sensitive,
        } => {
            let target = resolve_sync_target(
                cfg,
                scope.as_deref(),
                remote.as_deref(),
                allow_sensitive,
                scratch_git_dir,
            )?;
            let rep = push_state(store, &target)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                println!(
                    "pushed {} revision(s), {} agent event(s)",
                    rep.pushed_revisions, rep.pushed_agent_events
                );
            }
        }
        StateAction::Sync {
            scope,
            remote,
            allow_sensitive,
        } => {
            let target = resolve_sync_target(
                cfg,
                scope.as_deref(),
                remote.as_deref(),
                allow_sensitive,
                scratch_git_dir,
            )?;
            let rep = sync_state(store, &target)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                println!(
                    "synced: pulled {} rev(s)/{} event(s), pushed {} rev(s)/{} event(s)",
                    rep.pulled_revisions,
                    rep.pulled_agent_events,
                    rep.pushed_revisions,
                    rep.pushed_agent_events
                );
            }
        }
        StateAction::Verify { scope, remote } => {
            let target = resolve_sync_target(
                cfg,
                scope.as_deref(),
                remote.as_deref(),
                false,
                scratch_git_dir,
            )?;
            let rep = verify_state(store, &target)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                println!(
                    "verified {} commit(s), {} segment(s), {} revision(s), {} agent event(s)",
                    rep.verified_commits,
                    rep.verified_segments,
                    rep.verified_revisions,
                    rep.verified_agent_events
                );
            }
        }
        StateAction::Mcp => {
            unreachable!("StateAction::Mcp is intercepted before opening RedbStore in main.rs")
        }
    }
    Ok(())
}
```

- [ ] **Step 3: Wire `cmd::agent` and `cmd::state` into `crates/cli/src/cmd/mod.rs` and `crates/cli/src/main.rs` (including exit code `2` on `AgentCollision` / `LeadTaken`)**

In `crates/cli/src/cmd/mod.rs`, add:
```rust
pub mod agent;
pub mod state;
```

In `crates/cli/src/main.rs`:
1. Add `Agent(cmd::agent::AgentCmd)` and `State(cmd::state::StateCmd)` to `enum Command`.
2. In `main()`, inspect errors for `StoreError::AgentCollision` or `StoreError::LeadTaken` and exit with status code `2` (spec §2.4):
```rust
fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            if let Some(
                fl_core::StoreError::AgentCollision { .. } | fl_core::StoreError::LeadTaken { .. },
            ) = e.downcast_ref::<fl_core::StoreError>()
            {
                return ExitCode::from(2);
            }
            ExitCode::FAILURE
        }
    }
}
```
3. In `run(cli: Cli)` in `crates/cli/src/main.rs`, resolve the state store path (`cli.store`, or `cfg.state.as_ref().and_then(|s| s.store.clone())`, or `default_store_path()`) when dispatching `Command::Agent` and `Command::State`.

---

### Task 7: `fl state mcp` stdio server and concurrent lock-freedom verification (`fl-cli`)

**Files:**
- Create: `crates/cli/src/cmd/state_mcp.rs`
- Modify: `crates/cli/src/cmd/mod.rs`
- Modify: `crates/cli/src/main.rs`
- Modify: `crates/cli/tests/state_sync.rs`

**Interfaces:**
- Consumes: `RedbStore`, `Config`, `cmd::agent`, `cmd::state`, `fl_exec::state_sync`.
- Produces:
  - `crates/cli/src/cmd/state_mcp.rs`: newline-delimited JSON-RPC 2.0 MCP server over `stdin`/`stdout` exposing the 8 state/agent tools (`fl_state_get`, `fl_state_list`, `fl_state_upsert_memory`, `fl_state_upsert_handoff`, `fl_state_upsert_runtime`, `fl_state_sync`, `fl_agent_register`, `fl_agent_roster`).
  - Per-call `RedbStore::open(&store_path)` + immediate drop before writing the JSON-RPC response line so the `.redb` file lock is never held while waiting on `stdin`.

- [ ] **Step 1: Create `crates/cli/src/cmd/state_mcp.rs` implementing the 8 MCP tools with per-call `RedbStore::open`**

Create `crates/cli/src/cmd/state_mcp.rs`:

```rust
use crate::cmd::state::{parse_sensitivity, resolve_sync_target};
use crate::config::Config;
use fl_core::agent::{AgentRole, AreaRecord, RegisterAgentArgs, reports_to};
use fl_core::state::{
    HandoffEntry, MemoryEntry, MemoryKind, MemoryStatus, MergeMode, RuntimeEntry, StateConcern,
    StateFilter, StatePayload, StateScope, UpsertStateArgs,
};
use fl_core::{AgentRegistry, At, Kind, StateId, StateStore};
use fl_exec::state_sync::sync_state;
use fl_store::RedbStore;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::Path;

const PROTOCOL_VERSION: &str = "2024-11-05";

pub fn run_stdio(
    store_path: &Path,
    cfg: &Config,
    scratch_git_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    let mut line = String::new();

    while reader.read_line(&mut line)? > 0 {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                });
                writeln!(writer, "{resp}")?;
                writer.flush()?;
                line.clear();
                continue;
            }
        };
        line.clear();

        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let id = msg.get("id").cloned();

        // Notifications have no `id` and expect no response.
        if id.is_none() {
            continue;
        }
        let id = id.unwrap();

        let resp = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "fl-state-mcp", "version": env!("CARGO_PKG_VERSION") }
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tool_definitions() }
            }),
            "tools/call" => {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match dispatch_tool_opening_store(store_path, cfg, scratch_git_dir, name, &args) {
                    Ok(val) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string(&val).unwrap()
                            }],
                            "isError": false
                        }
                    }),
                    Err(err_msg) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": err_msg
                            }],
                            "isError": true
                        }
                    }),
                }
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            }),
        };

        writeln!(writer, "{resp}")?;
        writer.flush()?;
    }
    Ok(())
}

/// Opens `RedbStore` only for the duration of a single tool call and drops it
/// before returning so `fl state mcp` never holds the `.redb` file lock while idle.
fn dispatch_tool_opening_store(
    store_path: &Path,
    cfg: &Config,
    scratch_git_dir: &Path,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let store = RedbStore::open(store_path).map_err(|e| e.to_string())?;
    let res = dispatch_tool(&store, cfg, scratch_git_dir, name, args);
    drop(store);
    res
}

fn dispatch_tool(
    store: &RedbStore,
    cfg: &Config,
    scratch_git_dir: &Path,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    match name {
        "fl_state_get" => {
            if let Some(id_raw) = args.get("id").and_then(Value::as_str) {
                let iri = crate::refs::parse(store, Kind::State, id_raw)
                    .map_err(|e| e.to_string())?;
                let entry = store
                    .get_state(&StateId(iri))
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(entry).unwrap())
            } else {
                let sc = StateScope::parse_wire(
                    args.get("scope")
                        .and_then(Value::as_str)
                        .unwrap_or("workspace"),
                )
                .map_err(|e| e.to_string())?;
                let cn = StateConcern::parse_wire(
                    args.get("concern")
                        .and_then(Value::as_str)
                        .ok_or("missing `concern`")?,
                )
                .map_err(|e| e.to_string())?;
                let sl = args
                    .get("slug")
                    .and_then(Value::as_str)
                    .ok_or("missing `slug`")?;
                let entry = store
                    .state_by_key(&sc, cn, sl)
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(entry).unwrap())
            }
        }
        "fl_state_list" => {
            let filter = StateFilter {
                scope: args
                    .get("scope")
                    .and_then(Value::as_str)
                    .map(StateScope::parse_wire)
                    .transpose()
                    .map_err(|e| e.to_string())?,
                concern: args
                    .get("concern")
                    .and_then(Value::as_str)
                    .map(StateConcern::parse_wire)
                    .transpose()
                    .map_err(|e| e.to_string())?,
                tag: args.get("tag").and_then(Value::as_str).map(str::to_string),
                include_archived: args
                    .get("include_archived")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            let list = store.list_state(&filter).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(list).unwrap())
        }
        "fl_state_upsert_memory" => {
            let scope = StateScope::parse_wire(
                args.get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("workspace"),
            )
            .map_err(|e| e.to_string())?;
            let slug = args
                .get("slug")
                .and_then(Value::as_str)
                .ok_or("missing `slug`")?
                .to_string();
            let kind = MemoryKind::parse_wire(
                args.get("kind")
                    .and_then(Value::as_str)
                    .ok_or("missing `kind`")?,
            )
            .map_err(|e| e.to_string())?;
            let status = MemoryStatus::parse_wire(
                args.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("active"),
            )
            .map_err(|e| e.to_string())?;
            let summary = args
                .get("summary")
                .and_then(Value::as_str)
                .ok_or("missing `summary`")?
                .to_string();
            let body = args
                .get("body")
                .and_then(Value::as_str)
                .ok_or("missing `body`")?
                .to_string();
            let tags = string_vec(args.get("tags"));
            let source_refs = string_vec(args.get("source_refs"));
            let superseded_by = match args.get("superseded_by").and_then(Value::as_str) {
                Some(raw) => Some(StateId(
                    crate::refs::parse(store, Kind::State, raw).map_err(|e| e.to_string())?,
                )),
                None => None,
            };
            let sensitive = args.get("sensitive").and_then(Value::as_bool);
            let by = args
                .get("by")
                .and_then(Value::as_str)
                .unwrap_or("mcp")
                .to_string();
            let entry = store
                .upsert_state(UpsertStateArgs {
                    scope,
                    slug,
                    sensitivity: parse_sensitivity(sensitive),
                    payload: StatePayload::Memory(MemoryEntry {
                        kind,
                        status,
                        summary,
                        body,
                        tags,
                        source_refs,
                        superseded_by,
                    }),
                    at: At::now(),
                    by,
                })
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(entry).unwrap())
        }
        "fl_state_upsert_handoff" => {
            let scope = StateScope::parse_wire(
                args.get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("workspace"),
            )
            .map_err(|e| e.to_string())?;
            let slug = args
                .get("slug")
                .and_then(Value::as_str)
                .unwrap_or("current")
                .to_string();
            let summary = args
                .get("summary")
                .and_then(Value::as_str)
                .ok_or("missing `summary`")?
                .to_string();
            let shipped = string_vec(args.get("shipped"));
            let in_flight = string_vec(args.get("in_flight"));
            let blockers = string_vec(args.get("blockers"));
            let next_steps = string_vec(args.get("next_steps"));
            let notes = args
                .get("notes")
                .and_then(Value::as_str)
                .map(str::to_string);
            let sensitive = args.get("sensitive").and_then(Value::as_bool);
            let by = args
                .get("by")
                .and_then(Value::as_str)
                .unwrap_or("mcp")
                .to_string();
            let entry = store
                .upsert_state(UpsertStateArgs {
                    scope,
                    slug,
                    sensitivity: parse_sensitivity(sensitive),
                    payload: StatePayload::Handoff(HandoffEntry {
                        summary,
                        shipped,
                        in_flight,
                        blockers,
                        next_steps,
                        notes,
                    }),
                    at: At::now(),
                    by,
                })
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(entry).unwrap())
        }
        "fl_state_upsert_runtime" => {
            let scope = StateScope::parse_wire(
                args.get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("workspace"),
            )
            .map_err(|e| e.to_string())?;
            let slug = args
                .get("slug")
                .and_then(Value::as_str)
                .ok_or("missing `slug`")?
                .to_string();
            let schema = args
                .get("schema")
                .and_then(Value::as_str)
                .ok_or("missing `schema`")?
                .to_string();
            let merge_mode = MergeMode::parse_wire(
                args.get("merge_mode")
                    .and_then(Value::as_str)
                    .unwrap_or("conflict"),
            )
            .map_err(|e| e.to_string())?;
            let value = args.get("value").cloned().ok_or("missing `value`")?;
            let sensitive = args.get("sensitive").and_then(Value::as_bool);
            let by = args
                .get("by")
                .and_then(Value::as_str)
                .unwrap_or("mcp")
                .to_string();
            let entry = store
                .upsert_state(UpsertStateArgs {
                    scope,
                    slug,
                    sensitivity: parse_sensitivity(sensitive),
                    payload: StatePayload::Runtime(RuntimeEntry {
                        schema,
                        merge_mode,
                        value,
                    }),
                    at: At::now(),
                    by,
                })
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(entry).unwrap())
        }
        "fl_state_sync" => {
            let scope = args.get("scope").and_then(Value::as_str);
            let remote = args.get("remote").and_then(Value::as_str);
            let allow_sensitive = args
                .get("allow_sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let target =
                resolve_sync_target(cfg, scope, remote, allow_sensitive, scratch_git_dir)
                    .map_err(|e| e.to_string())?;
            let rep = sync_state(store, &target).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(rep).unwrap())
        }
        "fl_agent_register" => {
            let display_name = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or("missing `name`")?
                .to_string();
            let project = args
                .get("project")
                .and_then(Value::as_str)
                .ok_or("missing `project`")?
                .to_string();
            let role = AgentRole::parse_wire(
                args.get("role")
                    .and_then(Value::as_str)
                    .ok_or("missing `role`")?,
            )
            .map_err(|e| e.to_string())?;
            let email = args
                .get("email")
                .and_then(Value::as_str)
                .map(str::to_string);
            let extra_areas = string_vec(args.get("extra_areas"));
            let create_area = args
                .get("create_area")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let replace = args
                .get("replace")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let by = args
                .get("by")
                .and_then(Value::as_str)
                .unwrap_or("mcp")
                .to_string();
            let rec = store
                .register_agent(RegisterAgentArgs {
                    display_name,
                    email,
                    project,
                    role,
                    extra_areas,
                    create_area,
                    replace,
                    at: At::now(),
                    by,
                })
                .map_err(|e| e.to_string())?;
            let active = store.list_agents(None, false).map_err(|e| e.to_string())?;
            Ok(json!({
                "agent": rec,
                "reports_to": reports_to(&rec, &active),
                "co_author_trailer": rec.co_author_trailer(),
            }))
        }
        "fl_agent_roster" => {
            let project = args.get("project").and_then(Value::as_str);
            let include_retired = args
                .get("include_retired")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let areas: Vec<AreaRecord> = store.list_areas().map_err(|e| e.to_string())?;
            let agents = store
                .list_agents(project, include_retired)
                .map_err(|e| e.to_string())?;
            Ok(json!({ "areas": areas, "agents": agents }))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn string_vec(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn tool_definitions() -> Value {
    json!([
        { "name": "fl_state_get", "description": "Fetch one state entry by id or by (scope, concern, slug)", "inputSchema": { "type": "object" } },
        { "name": "fl_state_list", "description": "List state entries matching scope, concern, tag, and archive filters", "inputSchema": { "type": "object" } },
        { "name": "fl_state_upsert_memory", "description": "Create or update a typed memory entry", "inputSchema": { "type": "object", "required": ["slug", "kind", "summary", "body"] } },
        { "name": "fl_state_upsert_handoff", "description": "Create or update a structured handoff entry", "inputSchema": { "type": "object", "required": ["summary"] } },
        { "name": "fl_state_upsert_runtime", "description": "Create or update a runtime entry", "inputSchema": { "type": "object", "required": ["slug", "schema", "value"] } },
        { "name": "fl_state_sync", "description": "Pull, merge, and push state revisions and agent events on fl/state", "inputSchema": { "type": "object" } },
        { "name": "fl_agent_register", "description": "Register an agent identity, project, and role", "inputSchema": { "type": "object", "required": ["name", "project", "role"] } },
        { "name": "fl_agent_roster", "description": "List registered agents and coverage areas", "inputSchema": { "type": "object" } }
    ])
}
```

- [ ] **Step 2: Intercept `Command::State(StateCmd { action: StateAction::Mcp })` in `crates/cli/src/main.rs` before `RedbStore::open`**

In `crates/cli/src/cmd/mod.rs`, add `pub mod state_mcp;`.
In `crates/cli/src/main.rs` right alongside the early `Command::Mcp` check (before `RedbStore::open`), intercept `Command::State(cmd::state::StateCmd { action: cmd::state::StateAction::Mcp })`:

```rust
    if matches!(
        cli.command,
        Command::State(cmd::state::StateCmd {
            action: cmd::state::StateAction::Mcp
        })
    ) {
        let cfg = config::load(cli.config.as_deref())?;
        let store_path = cli
            .store
            .clone()
            .or_else(|| cfg.state.as_ref().and_then(|s| s.store.clone()))
            .unwrap_or_else(default_store_path);
        let scratch = tempfile::TempDir::new()?;
        return cmd::state_mcp::run_stdio(&store_path, &cfg, scratch.path());
    }
```

- [ ] **Step 3: Write black-box CLI + MCP tests in `crates/cli/tests/state_sync.rs`**

Create `crates/cli/tests/state_sync.rs`:

```rust
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn fl_bin() -> String {
    env!("CARGO_BIN_EXE_fl").to_string()
}

#[test]
fn cli_agent_register_enforces_collision_exit_code_two_and_exports_registry_md() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("state.redb");
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "").unwrap();

    let out1 = Command::new(fl_bin())
        .env("HOME", dir.path())
        .args([
            "--store",
            store.to_str().unwrap(),
            "--config",
            cfg.to_str().unwrap(),
            "agent",
            "register",
            "Ferris",
            "--project",
            "FerroLoop",
            "--role",
            "lead",
            "--create-area",
        ])
        .output()
        .unwrap();
    assert!(out1.status.success(), "{}", String::from_utf8_lossy(&out1.stderr));

    // Duplicate registration without --replace exits with code 2!
    let out2 = Command::new(fl_bin())
        .env("HOME", dir.path())
        .args([
            "--store",
            store.to_str().unwrap(),
            "--config",
            cfg.to_str().unwrap(),
            "agent",
            "register",
            "ferris",
            "--project",
            "FerroLoop",
            "--role",
            "lead",
        ])
        .output()
        .unwrap();
    assert_eq!(out2.status.code(), Some(2));

    // Export REGISTRY.md and check deterministic formatting.
    let reg_path = dir.path().join("REGISTRY.md");
    let out3 = Command::new(fl_bin())
        .env("HOME", dir.path())
        .args([
            "--store",
            store.to_str().unwrap(),
            "--config",
            cfg.to_str().unwrap(),
            "agent",
            "export",
            "--out",
            reg_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out3.status.success());
    let md = std::fs::read_to_string(&reg_path).unwrap();
    assert!(md.contains("| Ferris | `ferris` | FerroLoop | `lead` | — | Owner | `Co-authored-by: Ferris <ferris@artificialhumanity.io>` |"));
}

#[test]
fn state_mcp_stdio_releases_redb_lock_between_requests_allowing_concurrent_cli_writes() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("state.redb");
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "").unwrap();

    let mut child = Command::new(fl_bin())
        .env("HOME", dir.path())
        .args([
            "--store",
            store.to_str().unwrap(),
            "--config",
            cfg.to_str().unwrap(),
            "state",
            "mcp",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // 1. Initialize MCP session.
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert!(line.contains("fl-state-mcp"), "{line}");

    // 2. Call `fl_state_upsert_handoff` via MCP.
    line.clear();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"fl_state_upsert_handoff","arguments":{{"summary":"from mcp"}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    stdout.read_line(&mut line).unwrap();
    assert!(line.contains("from mcp"), "{line}");

    // 3. While `fl state mcp` is still running and waiting on stdin, invoke a separate `fl`
    // CLI process that writes to the same `state.redb` — proves the `.redb` lock is NOT held!
    let cli_out = Command::new(fl_bin())
        .env("HOME", dir.path())
        .args([
            "--store",
            store.to_str().unwrap(),
            "--config",
            cfg.to_str().unwrap(),
            "state",
            "memory",
            "set",
            "concurrent-rule",
            "--kind",
            "user",
            "--summary",
            "Written by CLI while MCP is idle",
            "--body",
            "Lock is released between requests.",
        ])
        .output()
        .unwrap();
    assert!(
        cli_out.status.success(),
        "concurrent CLI write failed while MCP process was alive: {}",
        String::from_utf8_lossy(&cli_out.stderr)
    );

    drop(stdin);
    let _ = child.wait();
}
```

- [ ] **Step 4: Run the full workspace verification trio and commit Tasks 6 & 7**

Run:
```bash
cargo test --workspace
cargo clippy --all-targets --workspace -- -D warnings
cargo fmt --all --check
git add crates/cli
git commit -m "feat(cli): add fl agent, fl state, Markdown exports, and fl state mcp server

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```
