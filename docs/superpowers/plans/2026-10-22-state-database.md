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
    #[error(
        "invalid agent role `{0}`: expected `lead`, `assistant`, `lab_manager`, or `custom:<slug>`"
    )]
    InvalidRole(String),
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
    pub sensitive: bool,
    pub ever_sensitive: bool,
    pub payload: StatePayload,
    pub head_revision: RevisionId,
    pub updated_at: At,
    pub by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateRevision {
    pub revision_id: RevisionId,
    pub entry_id: StateId,
    pub parent_revisions: Vec<RevisionId>,
    pub scope: StateScope,
    pub concern: StateConcern,
    pub slug: String,
    pub sensitive: bool,
    pub ever_sensitive: bool,
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
    pub remote: String,
    pub branch: String,
    pub last_pushed_commit: Option<String>,
    pub last_revision_seq: u64,
    pub last_registry_seq: u64,
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

    pub fn normalized(&self) -> Result<Self, StateError> {
        let raw_key = if self.area_key.trim().is_empty() {
            self.name.trim()
        } else {
            self.area_key.trim()
        };
        let area_key = validate_slug(raw_key)?;
        let name = if self.name.trim().is_empty() {
            area_key.clone()
        } else {
            self.name.trim().to_string()
        };
        let purpose = validate_summary(&self.purpose)?;
        Ok(Self {
            area_key,
            name,
            path: self.path.clone(),
            purpose,
            updated_at: self.updated_at,
            by: self.by.trim().to_string(),
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
        let lower = s.trim().to_ascii_lowercase();
        match lower.as_str() {
            "lead" => Ok(Self::Lead),
            "assistant" => Ok(Self::Assistant),
            "lab_manager" => Ok(Self::LabManager),
            other => {
                if let Some(slug) = other.strip_prefix("custom:") {
                    Ok(Self::Custom(validate_slug(slug)?))
                } else {
                    Err(StateError::InvalidRole(s.to_string()))
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

impl AgentRecord {
    pub fn co_author_trailer(&self) -> String {
        format!("Co-authored-by: {} <{}>", self.display_name, self.email)
    }
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
        ] {
            let role = AgentRole::from_wire(raw).unwrap();
            assert_eq!(role.as_wire(), expected);
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            let back: AgentRole = serde_json::from_str(&json).unwrap();
            assert_eq!(back, role);
        }
        assert!(AgentRole::from_wire("scribe").is_err());
        assert!(AgentRole::from_wire("leed").is_err());
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
    MemoryLifecycle, StateConcern, StateConflict, StateEntry, StateRevision, StateScope,
    StateSyncCursor, UpsertStateArgs,
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
        "state entry {entry_id} has an unresolved conflict between local revision {local_head} and remote revision {remote_head}; \
         run `fl state resolve {entry_id} --keep <revision-id> --by <author>` first"
    )]
    StateConflict {
        entry_id: StateId,
        local_head: RevisionId,
        remote_head: RevisionId,
    },
    #[error(
        "refused: state entry {0} carries sensitive history and cannot be synced to a non-private state target (`private = true` is required in config.toml)"
    )]
    SensitiveDisclosure(StateId),
    #[error("invalid state operation: {0}")]
    InvalidState(String),
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
        scope: &StateScope,
        slug: &str,
        lifecycle: MemoryLifecycle,
        superseded_by: Option<StateId>,
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
    ) -> Result<Vec<StateEntry>, StoreError>;
    fn state_revisions(&self, id: &StateId) -> Result<Vec<StateRevision>, StoreError>;
    fn list_conflicts(&self) -> Result<Vec<StateConflict>, StoreError>;
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

    fn sync_cursor(&self, remote: &str) -> Result<Option<StateSyncCursor>, StoreError>;
    fn set_sync_cursor(&self, cursor: StateSyncCursor) -> Result<(), StoreError>;
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
        head_revision: RevisionId(iri(12)),
        updated_at: At::from_unix_millis(5),
        by: "ferris".into(),
    };
    let state_revision = StateRevision {
        revision_id: RevisionId(iri(12)),
        entry_id: StateId(iri(11)),
        parent_revisions: vec![RevisionId(iri(14))],
        scope: StateScope::Project("ferroloop".into()),
        concern: StateConcern::Memory,
        slug: "arch".into(),
        sensitive: false,
        ever_sensitive: false,
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
pub fn sorted_parents(a: RevisionId, b: RevisionId) -> Vec<RevisionId> {
    if a.as_str() <= b.as_str() {
        vec![a, b]
    } else {
        vec![b, a]
    }
}

/// Returns `true` if `candidate_ancestor` is equal to `start` or reachable via
/// `parent_revisions` in `revs`.
pub fn is_ancestor_in_dag(
    candidate_ancestor: &RevisionId,
    start: &RevisionId,
    revs: &BTreeMap<RevisionId, StateRevision>,
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
    a: &RevisionId,
    b: &RevisionId,
    revs: &BTreeMap<RevisionId, StateRevision>,
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

/// Compute `MergeMode::Append` array merge from `base`, `local`, and `remote` (spec §3.3).
pub fn compute_append_merge(base: Vec<Value>, local: &[Value], remote: &[Value]) -> Vec<Value> {
    let mut merged = local.to_vec();
    for item in remote {
        if !base.contains(item) && !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    merged
}

/// Choose the winning revision for `MergeMode::LastWriter` by comparing
/// `(revision.at.as_str(), revision.revision_id.as_str())` (spec §3.3).
pub fn compute_last_writer_merge<'a>(
    local: &'a StateRevision,
    remote: &'a StateRevision,
) -> &'a StateRevision {
    if (remote.at.as_str(), remote.revision_id.as_str())
        >= (local.at.as_str(), local.revision_id.as_str())
    {
        remote
    } else {
        local
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

2. Update `all_roles` and `local_handles` in `crates/core/src/conformance.rs` to require `S: Catalog + Tracker + Ledger + Handles + StateStore + AgentRegistry`, and update `handles_are_per_kind_and_start_at_one` and `an_id_has_no_handle_under_any_kind_but_its_own` so all 6 `Kind`s (`Project`, `Gate`, `Record`, `Finding`, `State`, `Agent`) are exercised:

```rust
fn handles_are_per_kind_and_start_at_one(roles: &Bound<'_>) {
    let p1 = mk_project(roles.catalog, "a");
    let p2 = mk_project(roles.catalog, "b");
    let g1 = mk_gate(roles.catalog, &p1, "g-a");
    let r1 = mk_record(roles.tracker, "one");
    let r2 = mk_record(roles.tracker, "two");
    let f1 = mk_finding(roles.ledger, &p1);
    let st1 = roles
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
    let ag1 = roles
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

    // Every kind's counter starts at 1 independently.
    assert_eq!(roles.handles.handle_of(Kind::Project, p1.iri()).unwrap(), Some(1));
    assert_eq!(roles.handles.handle_of(Kind::Project, p2.iri()).unwrap(), Some(2));
    assert_eq!(roles.handles.handle_of(Kind::Gate, g1.iri()).unwrap(), Some(1));
    assert_eq!(roles.handles.handle_of(Kind::Record, r1.iri()).unwrap(), Some(1));
    assert_eq!(roles.handles.handle_of(Kind::Record, r2.iri()).unwrap(), Some(2));
    assert_eq!(roles.handles.handle_of(Kind::Finding, f1.iri()).unwrap(), Some(1));
    assert_eq!(roles.handles.handle_of(Kind::State, st1.id.iri()).unwrap(), Some(1));
    assert_eq!(roles.handles.handle_of(Kind::Agent, ag1.id.iri()).unwrap(), Some(1));

    // And resolve_handle is the inverse within that kind.
    assert_eq!(roles.handles.resolve_handle(Kind::Project, 1).unwrap(), Some(p1.0));
    assert_eq!(roles.handles.resolve_handle(Kind::Project, 2).unwrap(), Some(p2.0));
    assert_eq!(roles.handles.resolve_handle(Kind::Gate, 1).unwrap(), Some(g1.0));
    assert_eq!(roles.handles.resolve_handle(Kind::Record, 1).unwrap(), Some(r1.0));
    assert_eq!(roles.handles.resolve_handle(Kind::Record, 2).unwrap(), Some(r2.0));
    assert_eq!(roles.handles.resolve_handle(Kind::Finding, 1).unwrap(), Some(f1.0));
    assert_eq!(roles.handles.resolve_handle(Kind::State, 1).unwrap(), Some(st1.id.0));
    assert_eq!(roles.handles.resolve_handle(Kind::Agent, 1).unwrap(), Some(ag1.id.0));
}
```

And in `an_id_has_no_handle_under_any_kind_but_its_own`:

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
                category: crate::state::MemoryCategory::Rule,
                basis: crate::state::MemoryBasis::Ruling,
                lifecycle: crate::state::MemoryLifecycle::Active,
                superseded_by: None,
                pinned: true,
                summary: "Use STE for owner prose".into(),
                body: "Body".into(),
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    assert_eq!(mem.concern, crate::state::StateConcern::Memory);
    assert_eq!(
        s.get_state_by_key(
            &crate::state::StateScope::Workspace,
            crate::state::StateConcern::Memory,
            "ste-rule",
        )
        .unwrap(),
        Some(mem.clone())
    );
    assert_eq!(s.state_revisions(&mem.id).unwrap().len(), 1);

    let handoff = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "current".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "Task 1 green".into(),
                shipped: vec!["types".into()],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec!["Task 2".into()],
                notes: None,
            }),
            at: At::from_unix_millis(11),
            by: "ferris".into(),
        })
        .unwrap();
    assert_eq!(handoff.concern, crate::state::StateConcern::Handoff);

    let runtime = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Project("ferroloop".into()),
            slug: "gpu_lease".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"holder": "ferris"}),
                merge_mode: crate::state::MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(12),
            by: "ferris".into(),
        })
        .unwrap();
    assert_eq!(runtime.concern, crate::state::StateConcern::Runtime);
    assert_eq!(
        s.list_state(Some(&crate::state::StateScope::Project("ferroloop".into())), None)
            .unwrap()
            .len(),
        2
    );
}

fn sensitive_flag_is_sticky_until_explicitly_cleared_and_ever_sensitive_persists<S: StateStore>(
    s: &S,
) {
    let created = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "token_hint".into(),
            sensitivity: crate::state::SensitivityPatch::SetSensitive,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"v": 1}),
                merge_mode: crate::state::MergeMode::Conflict,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    assert!(created.sensitive);
    assert!(created.ever_sensitive);

    // Omitting sensitivity flag (Inherit) preserves `sensitive == true`.
    let updated = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "token_hint".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"v": 2}),
                merge_mode: crate::state::MergeMode::Conflict,
            }),
            at: At::from_unix_millis(11),
            by: "jarvis".into(),
        })
        .unwrap();
    assert!(updated.sensitive);
    assert!(updated.ever_sensitive);

    // Explicit ClearSensitive sets `sensitive = false` while `ever_sensitive` remains true.
    let cleared = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "token_hint".into(),
            sensitivity: crate::state::SensitivityPatch::ClearSensitive,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"v": 3}),
                merge_mode: crate::state::MergeMode::Conflict,
            }),
            at: At::from_unix_millis(12),
            by: "jarvis".into(),
        })
        .unwrap();
    assert!(!cleared.sensitive);
    assert!(cleared.ever_sensitive);
}

fn memory_superseded_validates_target_exists_and_is_memory<S: StateStore>(s: &S) {
    let old = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "old_rule".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Memory(crate::state::MemoryEntry {
                category: crate::state::MemoryCategory::Rule,
                basis: crate::state::MemoryBasis::Ruling,
                lifecycle: crate::state::MemoryLifecycle::Active,
                superseded_by: None,
                pinned: false,
                summary: "Old rule".into(),
                body: "Old".into(),
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();

    // Superseding by self is rejected.
    let self_err = s
        .retire_memory(
            &crate::state::StateScope::Workspace,
            "old_rule",
            crate::state::MemoryLifecycle::Superseded,
            Some(old.id.clone()),
            At::from_unix_millis(11),
            "jarvis",
        )
        .unwrap_err();
    assert!(matches!(self_err, StoreError::InvalidState(_)));

    // Superseding by a non-Memory entry is rejected.
    let handoff = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "current".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "Handoff".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(12),
            by: "jarvis".into(),
        })
        .unwrap();
    let wrong_concern = s
        .retire_memory(
            &crate::state::StateScope::Workspace,
            "old_rule",
            crate::state::MemoryLifecycle::Superseded,
            Some(handoff.id),
            At::from_unix_millis(13),
            "jarvis",
        )
        .unwrap_err();
    assert!(matches!(wrong_concern, StoreError::InvalidState(_)));

    // Superseding by a valid Memory entry succeeds.
    let new_mem = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "new_rule".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Memory(crate::state::MemoryEntry {
                category: crate::state::MemoryCategory::Rule,
                basis: crate::state::MemoryBasis::Ruling,
                lifecycle: crate::state::MemoryLifecycle::Active,
                superseded_by: None,
                pinned: false,
                summary: "New rule".into(),
                body: "New".into(),
            }),
            at: At::from_unix_millis(14),
            by: "jarvis".into(),
        })
        .unwrap();
    let retired = s
        .retire_memory(
            &crate::state::StateScope::Workspace,
            "old_rule",
            crate::state::MemoryLifecycle::Superseded,
            Some(new_mem.id.clone()),
            At::from_unix_millis(15),
            "jarvis",
        )
        .unwrap();
    match retired.payload {
        crate::state::StatePayload::Memory(m) => {
            assert_eq!(m.lifecycle, crate::state::MemoryLifecycle::Superseded);
            assert_eq!(m.superseded_by, Some(new_mem.id));
        }
        other => panic!("expected Memory payload, got {other:?}"),
    }
}

fn runtime_append_requires_array_and_rejects_scalar<S: StateStore>(s: &S) {
    let err = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "events".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"not": "an array"}),
                merge_mode: crate::state::MergeMode::Append,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidState(_)));
}

fn scope_concern_and_slug_are_immutable_for_an_entry_id<S: StateStore>(s: &S) {
    let entry = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "immutable".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(1),
                merge_mode: crate::state::MergeMode::Conflict,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let bad_rev = crate::state::StateRevision {
        revision_id: crate::ids::RevisionId(crate::ids::seq_iri(900)),
        entry_id: entry.id.clone(),
        parent_revisions: vec![entry.head_revision.clone()],
        scope: crate::state::StateScope::Workspace,
        concern: crate::state::StateConcern::Runtime,
        slug: "renamed".into(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!(2),
            merge_mode: crate::state::MergeMode::Conflict,
        }),
        at: At::from_unix_millis(11),
        by: "jarvis".into(),
    };
    let err = s
        .import_state_revisions(&[bad_rev], At::from_unix_millis(12))
        .unwrap_err();
    assert!(matches!(err, StoreError::Tampered { .. }));
}

fn fast_forward_and_already_ahead_revision_imports_converge<S: StateStore>(s: &S) {
    let v1 = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "ff".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(1),
                merge_mode: crate::state::MergeMode::Conflict,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let r2_id = crate::ids::RevisionId(crate::ids::seq_iri(901));
    let r2 = crate::state::StateRevision {
        revision_id: r2_id.clone(),
        entry_id: v1.id.clone(),
        parent_revisions: vec![v1.head_revision.clone()],
        scope: v1.scope.clone(),
        concern: v1.concern,
        slug: v1.slug.clone(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!(2),
            merge_mode: crate::state::MergeMode::Conflict,
        }),
        at: At::from_unix_millis(20),
        by: "ferris".into(),
    };
    // Remote is descendant of local -> fast-forward.
    s.import_state_revisions(std::slice::from_ref(&r2), At::from_unix_millis(21))
        .unwrap();
    let after_ff = s.get_state(&v1.id).unwrap().unwrap();
    assert_eq!(after_ff.head_revision, r2_id);
    assert!(s.list_conflicts().unwrap().is_empty());

    // Re-importing the ancestor revision when local is already ahead is a no-op.
    let r1 = s
        .state_revisions(&v1.id)
        .unwrap()
        .into_iter()
        .find(|r| r.revision_id == v1.head_revision)
        .unwrap();
    s.import_state_revisions(&[r1], At::from_unix_millis(22))
        .unwrap();
    assert_eq!(s.get_state(&v1.id).unwrap().unwrap().head_revision, r2_id);
}

fn true_divergence_creates_conflict_blocks_writes_and_resolves_cleanly<S: StateStore>(s: &S) {
    let base = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "handoff".into(),
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
    let local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "handoff".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "local".into(),
                shipped: vec!["a".into()],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(20),
            by: "jarvis".into(),
        })
        .unwrap();
    let remote_rev_id = crate::ids::RevisionId(crate::ids::seq_iri(902));
    let remote_rev = crate::state::StateRevision {
        revision_id: remote_rev_id.clone(),
        entry_id: base.id.clone(),
        parent_revisions: vec![base.head_revision.clone()],
        scope: base.scope.clone(),
        concern: base.concern,
        slug: base.slug.clone(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
            summary: "remote".into(),
            shipped: vec!["b".into()],
            in_flight: vec![],
            blockers: vec![],
            next_steps: vec![],
            notes: None,
        }),
        at: At::from_unix_millis(25),
        by: "ferris".into(),
    };
    s.import_state_revisions(&[remote_rev], At::from_unix_millis(30))
        .unwrap();

    let conflicts = s.list_conflicts().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].entry_id, base.id);
    assert_eq!(conflicts[0].local_head, local.head_revision);
    assert_eq!(conflicts[0].remote_head, remote_rev_id);

    // Regular writes are blocked while conflicted.
    let blocked = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "handoff".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Handoff(crate::state::HandoffEntry {
                summary: "blocked".into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(31),
            by: "jarvis".into(),
        })
        .unwrap_err();
    assert!(matches!(blocked, StoreError::StateConflict { .. }));

    // Resolve keeping remote_rev_id produces a 2-parent merge revision.
    let resolved = s
        .resolve_conflict(&base.id, &remote_rev_id, At::from_unix_millis(40), "jarvis")
        .unwrap();
    assert!(s.list_conflicts().unwrap().is_empty());
    let revs = s.state_revisions(&base.id).unwrap();
    let merge_rev = revs
        .iter()
        .find(|r| r.revision_id == resolved.head_revision)
        .unwrap();
    assert_eq!(merge_rev.parent_revisions.len(), 2);
}

fn mismatched_runtime_merge_modes_fail_closed_to_conflict<S: StateStore>(s: &S) {
    let base = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "mode_mismatch".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!([1]),
                merge_mode: crate::state::MergeMode::Append,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let _local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "mode_mismatch".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!([1, 2]),
                merge_mode: crate::state::MergeMode::Append,
            }),
            at: At::from_unix_millis(20),
            by: "jarvis".into(),
        })
        .unwrap();
    let remote = crate::state::StateRevision {
        revision_id: crate::ids::RevisionId(crate::ids::seq_iri(903)),
        entry_id: base.id.clone(),
        parent_revisions: vec![base.head_revision.clone()],
        scope: base.scope.clone(),
        concern: base.concern,
        slug: base.slug.clone(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!([1, 3]),
            merge_mode: crate::state::MergeMode::LastWriter,
        }),
        at: At::from_unix_millis(25),
        by: "ferris".into(),
    };
    s.import_state_revisions(&[remote], At::from_unix_millis(30))
        .unwrap();
    assert_eq!(s.list_conflicts().unwrap().len(), 1);
}

fn runtime_append_and_last_writer_auto_merge_without_conflict<S: StateStore>(s: &S) {
    // 1. Append auto-merge.
    let base = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "log".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(["a"]),
                merge_mode: crate::state::MergeMode::Append,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let _local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "log".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(["a", "b"]),
                merge_mode: crate::state::MergeMode::Append,
            }),
            at: At::from_unix_millis(20),
            by: "jarvis".into(),
        })
        .unwrap();
    let remote = crate::state::StateRevision {
        revision_id: crate::ids::RevisionId(crate::ids::seq_iri(904)),
        entry_id: base.id.clone(),
        parent_revisions: vec![base.head_revision.clone()],
        scope: base.scope.clone(),
        concern: base.concern,
        slug: base.slug.clone(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!(["a", "c"]),
            merge_mode: crate::state::MergeMode::Append,
        }),
        at: At::from_unix_millis(25),
        by: "ferris".into(),
    };
    s.import_state_revisions(&[remote], At::from_unix_millis(30))
        .unwrap();
    assert!(s.list_conflicts().unwrap().is_empty());
    let merged = s.get_state(&base.id).unwrap().unwrap();
    match merged.payload {
        crate::state::StatePayload::Runtime(rt) => {
            assert_eq!(rt.value, serde_json::json!(["a", "b", "c"]));
        }
        other => panic!("expected Runtime, got {other:?}"),
    }

    // 2. LastWriter auto-merge.
    let lw_base = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "cursor".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(1),
                merge_mode: crate::state::MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let _lw_local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "cursor".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!(2),
                merge_mode: crate::state::MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(20),
            by: "jarvis".into(),
        })
        .unwrap();
    let lw_remote = crate::state::StateRevision {
        revision_id: crate::ids::RevisionId(crate::ids::seq_iri(905)),
        entry_id: lw_base.id.clone(),
        parent_revisions: vec![lw_base.head_revision.clone()],
        scope: lw_base.scope.clone(),
        concern: lw_base.concern,
        slug: lw_base.slug.clone(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!(3),
            merge_mode: crate::state::MergeMode::LastWriter,
        }),
        at: At::from_unix_millis(25),
        by: "ferris".into(),
    };
    s.import_state_revisions(&[lw_remote], At::from_unix_millis(30))
        .unwrap();
    assert!(s.list_conflicts().unwrap().is_empty());
    let lw_merged = s.get_state(&lw_base.id).unwrap().unwrap();
    match lw_merged.payload {
        crate::state::StatePayload::Runtime(rt) => {
            assert_eq!(rt.value, serde_json::json!(3));
        }
        other => panic!("expected Runtime, got {other:?}"),
    }
}

fn offline_same_key_collision_aliases_unpublished_local_id_to_remote<S: StateStore + Handles>(
    s: &S,
) {
    let local = s
        .upsert_state(crate::state::UpsertStateArgs {
            scope: crate::state::StateScope::Workspace,
            slug: "shared_slug".into(),
            sensitivity: crate::state::SensitivityPatch::Inherit,
            payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
                value: serde_json::json!({"side": "local"}),
                merge_mode: crate::state::MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    let local_handle = s.handle_of(Kind::State, local.id.iri()).unwrap().unwrap();

    let remote_id = crate::ids::StateId(crate::ids::seq_iri(950));
    let remote_rev = crate::state::StateRevision {
        revision_id: crate::ids::RevisionId(crate::ids::seq_iri(951)),
        entry_id: remote_id.clone(),
        parent_revisions: vec![],
        scope: crate::state::StateScope::Workspace,
        concern: crate::state::StateConcern::Runtime,
        slug: "shared_slug".into(),
        sensitive: false,
        ever_sensitive: false,
        payload: crate::state::StatePayload::Runtime(crate::state::RuntimeEntry {
            value: serde_json::json!({"side": "remote"}),
            merge_mode: crate::state::MergeMode::LastWriter,
        }),
        at: At::from_unix_millis(20),
        by: "ferris".into(),
    };
    s.import_state_revisions(&[remote_rev], At::from_unix_millis(25))
        .unwrap();

    // `local.id` now resolves via `aliases` to `remote_id`, and numeric handle is preserved.
    let by_old_id = s.get_state(&local.id).unwrap().unwrap();
    assert_eq!(by_old_id.id, remote_id);
    assert_eq!(
        s.resolve_handle(Kind::State, local_handle).unwrap(),
        Some(remote_id.0)
    );
}
```

- [ ] **Step 3: Implement `AgentRegistry` and `StateStore` on `MemStore` in `crates/core/src/mem.rs`**

Update `crates/core/src/mem.rs` to add state and agent storage fields to `Inner`, add `alias_unpublished_id`, implement `AgentRegistry` and `StateStore` for `MemStore`, and invoke `conformance::agent_registry` and `conformance::state_store` in `mod tests`:

1. Add imports at the top of `crates/core/src/mem.rs`:

```rust
use crate::agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, RegistryEvent,
    RegistryEventKind, validate_agent_identity,
};
use crate::ids::{AgentId, GateId, Kind, ProjectId, RecordId, RevisionId, StateId, seq_iri};
use crate::state::{
    MemoryLifecycle, MergeMode, SensitivityPatch, StateConcern, StateConflict, StateEntry,
    StatePayload, StateRevision, StateScope, StateSyncCursor, UpsertStateArgs,
    compute_append_merge, compute_last_writer_merge, is_ancestor_in_dag,
    lowest_common_ancestor_in_dag, sorted_parents, validate_slug,
};
use crate::store::{
    AgentRegistry, Catalog, GateFilter, Handles, Ledger, RecordFilter, StateStore, StoreError,
    Tracker,
};
```

*(Remove the old `use crate::ids::{...}` and `use crate::store::{...}` lines that this replaces.)*

2. Extend `struct Inner` in `crates/core/src/mem.rs` with the state and agent maps:

```rust
    areas: BTreeMap<String, AreaRecord>,
    agents: BTreeMap<AgentId, AgentRecord>,
    agent_by_name: BTreeMap<String, AgentId>,
    registry_events: BTreeMap<u64, RegistryEvent>,
    registry_event_ids: BTreeMap<Iri, u64>,
    next_registry_seq: u64,

    state_entries: BTreeMap<StateId, StateEntry>,
    state_by_key: BTreeMap<(String, String, String), StateId>,
    state_revisions: BTreeMap<u64, StateRevision>,
    state_revision_ids: BTreeMap<RevisionId, u64>,
    next_state_rev_seq: u64,
    state_conflicts: BTreeMap<StateId, StateConflict>,
    sync_cursors: BTreeMap<String, StateSyncCursor>,
```

3. Add `index_imported` and `alias_unpublished_id` helpers on `Inner` (inside `impl Inner`):

```rust
    /// Register an imported entity IRI in `ids` and `handle_of` if not already known.
    fn index_imported(&mut self, kind: Kind, id: &Iri) -> Result<(), StoreError> {
        let canonical = self.resolve(id).clone();
        match self.ids.get(&canonical) {
            Some(Slot::Owned(existing)) if *existing == kind => Ok(()),
            Some(Slot::Owned(existing)) => Err(StoreError::WrongKind {
                id: canonical,
                expected: kind,
                found: *existing,
            }),
            Some(Slot::Alias) => Ok(()),
            None => {
                self.index_new(kind, &canonical);
                Ok(())
            }
        }
    }

    /// Re-point an unpublished local `old_id` to `new_id` of the same `kind`,
    /// preserving any existing numeric handle on `old_id` (spec §3.2, §4.3).
    fn alias_unpublished_id(
        &mut self,
        kind: Kind,
        old_id: &Iri,
        new_id: &Iri,
    ) -> Result<(), StoreError> {
        if old_id == new_id {
            return Ok(());
        }
        if let Some(Slot::Owned(existing)) = self.ids.get(old_id)
            && *existing != kind
        {
            return Err(StoreError::WrongKind {
                id: old_id.clone(),
                expected: kind,
                found: *existing,
            });
        }
        self.ids.insert(old_id.clone(), Slot::Alias);
        self.aliases.insert(old_id.clone(), new_id.clone());
        // Repoint any existing transitive aliases that targeted `old_id`.
        for target in self.aliases.values_mut() {
            if target == old_id {
                *target = new_id.clone();
            }
        }
        self.ids.insert(new_id.clone(), Slot::Owned(kind));
        if let Some(h) = self.handle_of.remove(old_id) {
            self.by_handle.insert((kind, h), new_id.clone());
            self.handle_of.insert(new_id.clone(), h);
        } else if !self.handle_of.contains_key(new_id) {
            let n = self.next_handle.entry(kind).or_insert(0);
            *n += 1;
            let h = *n;
            self.by_handle.insert((kind, h), new_id.clone());
            self.handle_of.insert(new_id.clone(), h);
        }
        Ok(())
    }

    fn append_registry_event(&mut self, ev: RegistryEvent) {
        self.next_registry_seq += 1;
        let seq = self.next_registry_seq;
        self.registry_event_ids.insert(ev.event_id.clone(), seq);
        self.registry_events.insert(seq, ev);
    }

    fn append_state_revision(&mut self, rev: StateRevision) {
        self.next_state_rev_seq += 1;
        let seq = self.next_state_rev_seq;
        self.state_revision_ids.insert(rev.revision_id.clone(), seq);
        self.state_revisions.insert(seq, rev);
    }

    fn revisions_for_entry(&self, entry_id: &StateId) -> BTreeMap<RevisionId, StateRevision> {
        self.state_revisions
            .values()
            .filter(|r| &r.entry_id == entry_id)
            .map(|r| (r.revision_id.clone(), r.clone()))
            .collect()
    }

    fn apply_imported_state_revision(
        &mut self,
        rev: StateRevision,
        merge_at: At,
    ) -> Result<(), StoreError> {
        rev.payload
            .validate(&rev.entry_id)
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;
        for parent in &rev.parent_revisions {
            if !self.state_revision_ids.contains_key(parent) {
                return Err(StoreError::InvalidState(format!(
                    "revision `{}` references unknown parent `{}`",
                    rev.revision_id, parent
                )));
            }
        }
        if let StatePayload::Memory(m) = &rev.payload
            && let Some(target_id) = &m.superseded_by
        {
            let resolved_target = StateId(self.resolve(target_id.iri()).clone());
            match self.state_entries.get(&resolved_target) {
                Some(target) if target.concern == StateConcern::Memory => {}
                _ => {
                    return Err(StoreError::InvalidState(format!(
                        "superseded_by target `{target_id}` does not exist as a Memory entry"
                    )));
                }
            }
        }

        let key = (
            rev.scope.as_wire(),
            rev.concern.as_wire().to_string(),
            rev.slug.clone(),
        );

        // Check offline same-key collision (`S_local != S_remote`).
        if let Some(existing_id) = self.state_by_key.get(&key).cloned()
            && existing_id != rev.entry_id
        {
            self.alias_unpublished_id(Kind::State, existing_id.iri(), rev.entry_id.iri())?;
            if let Some(mut local_entry) = self.state_entries.remove(&existing_id) {
                local_entry.id = rev.entry_id.clone();
                self.state_entries.insert(rev.entry_id.clone(), local_entry);
            }
            if let Some(mut conflict) = self.state_conflicts.remove(&existing_id) {
                conflict.entry_id = rev.entry_id.clone();
                self.state_conflicts.insert(rev.entry_id.clone(), conflict);
            }
            for stored_rev in self.state_revisions.values_mut() {
                if stored_rev.entry_id == existing_id {
                    stored_rev.entry_id = rev.entry_id.clone();
                }
            }
            self.state_by_key.insert(key.clone(), rev.entry_id.clone());
        }

        let resolved_entry_id = StateId(self.resolve(rev.entry_id.iri()).clone());
        let mut rev = rev;
        rev.entry_id = resolved_entry_id.clone();
        self.index_imported(Kind::State, rev.entry_id.iri())?;

        // Check immutability if entry already exists.
        if let Some(existing) = self.state_entries.get(&resolved_entry_id)
            && (existing.scope != rev.scope
                || existing.concern != rev.concern
                || existing.slug != rev.slug)
        {
            return Err(StoreError::Tampered {
                id: resolved_entry_id.0,
            });
        }

        self.append_state_revision(rev.clone());

        let Some(existing) = self.state_entries.get(&resolved_entry_id).cloned() else {
            let entry = StateEntry {
                id: resolved_entry_id.clone(),
                scope: rev.scope,
                concern: rev.concern,
                slug: rev.slug,
                sensitive: rev.sensitive,
                ever_sensitive: rev.ever_sensitive || rev.sensitive,
                payload: rev.payload,
                head_revision: rev.revision_id,
                updated_at: rev.at,
                by: rev.by,
            };
            self.state_by_key.insert(key, resolved_entry_id.clone());
            self.state_entries.insert(resolved_entry_id, entry);
            return Ok(());
        };

        let dag = self.revisions_for_entry(&resolved_entry_id);
        let r_local = existing.head_revision.clone();
        let r_remote = rev.revision_id.clone();

        // Case 1: Remote is descendant of local -> fast-forward.
        if is_ancestor_in_dag(&r_local, &r_remote, &dag) {
            let updated = StateEntry {
                id: resolved_entry_id.clone(),
                scope: existing.scope,
                concern: existing.concern,
                slug: existing.slug,
                sensitive: rev.sensitive,
                ever_sensitive: existing.ever_sensitive || rev.ever_sensitive || rev.sensitive,
                payload: rev.payload,
                head_revision: r_remote.clone(),
                updated_at: rev.at,
                by: rev.by,
            };
            self.state_entries.insert(resolved_entry_id.clone(), updated);
            if let Some(c) = self.state_conflicts.get(&resolved_entry_id)
                && is_ancestor_in_dag(&c.local_head, &r_remote, &dag)
                && is_ancestor_in_dag(&c.remote_head, &r_remote, &dag)
            {
                self.state_conflicts.remove(&resolved_entry_id);
            }
            return Ok(());
        }

        // Case 2: Local is already descendant of remote -> no-op.
        if is_ancestor_in_dag(&r_remote, &r_local, &dag) {
            return Ok(());
        }

        // Case 3: True divergence between r_local and r_remote.
        let local_rev = dag.get(&r_local).cloned().ok_or_else(|| {
            StoreError::InvalidState(format!("missing local head revision `{r_local}`"))
        })?;
        let base_rev = lowest_common_ancestor_in_dag(&r_local, &r_remote, &dag)
            .and_then(|id| dag.get(&id).cloned());

        let auto_merged: Option<StatePayload> = match (&local_rev.payload, &rev.payload) {
            (StatePayload::Runtime(l_rt), StatePayload::Runtime(r_rt))
                if l_rt.merge_mode == r_rt.merge_mode =>
            {
                match l_rt.merge_mode {
                    MergeMode::Conflict => None,
                    MergeMode::Append => {
                        let base_arr = match base_rev.as_ref().map(|b| &b.payload) {
                            Some(StatePayload::Runtime(b_rt)) => b_rt.value.as_array().cloned(),
                            _ => Some(Vec::new()),
                        };
                        match (base_arr, l_rt.value.as_array(), r_rt.value.as_array()) {
                            (Some(b), Some(l), Some(r)) => Some(StatePayload::Runtime(
                                crate::state::RuntimeEntry {
                                    value: serde_json::Value::Array(compute_append_merge(b, l, r)),
                                    merge_mode: MergeMode::Append,
                                },
                            )),
                            _ => None,
                        }
                    }
                    MergeMode::LastWriter => {
                        let win_payload = compute_last_writer_merge(&local_rev, &rev).payload.clone();
                        Some(win_payload)
                    }
                }
            }
            _ => None,
        };

        if let Some(merged_payload) = auto_merged {
            let merge_rev_id = RevisionId(self.mint());
            let sensitive = local_rev.sensitive || rev.sensitive;
            let ever_sensitive = existing.ever_sensitive
                || local_rev.ever_sensitive
                || rev.ever_sensitive
                || sensitive;
            let merge_rev = StateRevision {
                revision_id: merge_rev_id.clone(),
                entry_id: resolved_entry_id.clone(),
                parent_revisions: sorted_parents(r_local, r_remote),
                scope: existing.scope.clone(),
                concern: existing.concern,
                slug: existing.slug.clone(),
                sensitive,
                ever_sensitive,
                payload: merged_payload.clone(),
                at: merge_at,
                by: rev.by.clone(),
            };
            self.append_state_revision(merge_rev);
            let updated = StateEntry {
                id: resolved_entry_id.clone(),
                scope: existing.scope,
                concern: existing.concern,
                slug: existing.slug,
                sensitive,
                ever_sensitive,
                payload: merged_payload,
                head_revision: merge_rev_id,
                updated_at: merge_at,
                by: rev.by,
            };
            self.state_entries.insert(resolved_entry_id, updated);
        } else {
            self.state_conflicts.insert(
                resolved_entry_id.clone(),
                StateConflict {
                    entry_id: resolved_entry_id,
                    local_head: r_local,
                    remote_head: r_remote,
                    detected_at: merge_at,
                },
            );
        }
        Ok(())
    }
```

4. Implement `AgentRegistry` and `StateStore` for `MemStore` in `crates/core/src/mem.rs`:

```rust
impl AgentRegistry for MemStore {
    fn upsert_area(&self, area: AreaRecord) -> Result<AreaRecord, StoreError> {
        let normalized = area
            .normalized()
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let mut g = self.0.lock().unwrap();
        let ev = RegistryEvent {
            event_id: g.mint(),
            at: normalized.updated_at,
            by: normalized.by.clone(),
            kind: RegistryEventKind::AreaUpsert(normalized.clone()),
        };
        g.areas
            .insert(normalized.area_key.clone(), normalized.clone());
        g.append_registry_event(ev);
        Ok(normalized)
    }

    fn get_area(&self, area_key: &str) -> Result<Option<AreaRecord>, StoreError> {
        let key = validate_slug(area_key).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let g = self.0.lock().unwrap();
        Ok(g.areas.get(&key).cloned())
    }

    fn list_areas(&self) -> Result<Vec<AreaRecord>, StoreError> {
        let g = self.0.lock().unwrap();
        Ok(g.areas.values().cloned().collect())
    }

    fn register_agent(&self, args: RegisterAgentArgs) -> Result<AgentRecord, StoreError> {
        let (name_key, display_name, email) =
            validate_agent_identity(&args.display_name, args.email.as_deref())
                .map_err(StoreError::InvalidState)?;
        let project =
            validate_slug(&args.project).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let mut extra_areas = Vec::with_capacity(args.extra_areas.len());
        for a in &args.extra_areas {
            extra_areas
                .push(validate_slug(a).map_err(|e| StoreError::InvalidState(e.to_string()))?);
        }
        extra_areas.sort();
        extra_areas.dedup();

        let mut g = self.0.lock().unwrap();

        // Area validation / creation.
        for area_key in std::iter::once(&project).chain(extra_areas.iter()) {
            if !g.areas.contains_key(area_key) {
                if args.create_area {
                    let area = AreaRecord {
                        area_key: area_key.clone(),
                        name: area_key.clone(),
                        path: None,
                        purpose: format!("Registered area {area_key}"),
                        updated_at: args.at,
                        by: args.by.clone(),
                    };
                    let ev = RegistryEvent {
                        event_id: g.mint(),
                        at: args.at,
                        by: args.by.clone(),
                        kind: RegistryEventKind::AreaUpsert(area.clone()),
                    };
                    g.areas.insert(area_key.clone(), area);
                    g.append_registry_event(ev);
                } else {
                    return Err(StoreError::NoSuchArea(area_key.clone()));
                }
            }
        }

        // Check name collision (active or retired).
        let existing_by_name = g
            .agent_by_name
            .get(&name_key)
            .cloned()
            .and_then(|id| g.agents.get(&id).cloned());
        if let Some(ref existing) = existing_by_name
            && !args.replace
        {
            return Err(StoreError::AgentCollision {
                name_key,
                existing_id: existing.id.clone(),
                status: existing.status,
            });
        }

        // Check one-lead-per-project invariant.
        if args.role == AgentRole::Lead {
            let conflicting_lead = g
                .agents
                .values()
                .find(|a| {
                    a.status == AgentStatus::Active
                        && a.project == project
                        && a.role == AgentRole::Lead
                        && a.name_key != name_key
                })
                .cloned();
            if let Some(lead) = conflicting_lead {
                if !args.replace {
                    return Err(StoreError::LeadTaken {
                        project,
                        lead_name: lead.display_name,
                        lead_id: lead.id,
                    });
                }
                let mut retired = lead.clone();
                retired.status = AgentStatus::Retired;
                retired.updated_at = args.at;
                retired.by = args.by.clone();
                g.agents.insert(retired.id.clone(), retired.clone());
                let retire_ev = RegistryEvent {
                    event_id: g.mint(),
                    at: args.at,
                    by: args.by.clone(),
                    kind: RegistryEventKind::AgentRetire { id: retired.id },
                };
                g.append_registry_event(retire_ev);
            }
        }

        let agent_id = match existing_by_name {
            Some(existing) => existing.id,
            None => {
                let raw = g.mint();
                g.index_new(Kind::Agent, &raw);
                AgentId(raw)
            }
        };

        let record = AgentRecord {
            id: agent_id.clone(),
            name_key: name_key.clone(),
            display_name,
            email,
            project,
            role: args.role,
            extra_areas,
            status: AgentStatus::Active,
            updated_at: args.at,
            by: args.by.clone(),
        };
        g.agent_by_name.insert(name_key, agent_id.clone());
        g.agents.insert(agent_id, record.clone());
        let ev = RegistryEvent {
            event_id: g.mint(),
            at: args.at,
            by: args.by,
            kind: RegistryEventKind::AgentUpsert(record.clone()),
        };
        g.append_registry_event(ev);
        Ok(record)
    }

    fn retire_agent(&self, id: &AgentId, at: At, by: &str) -> Result<AgentRecord, StoreError> {
        let mut g = self.0.lock().unwrap();
        let resolved = AgentId(g.check(Kind::Agent, id.iri())?);
        let Some(mut rec) = g.agents.get(&resolved).cloned() else {
            return Err(StoreError::NoSuchAgent(resolved.to_string()));
        };
        rec.status = AgentStatus::Retired;
        rec.updated_at = at;
        rec.by = by.trim().to_string();
        g.agents.insert(resolved.clone(), rec.clone());
        let ev = RegistryEvent {
            event_id: g.mint(),
            at,
            by: rec.by.clone(),
            kind: RegistryEventKind::AgentRetire { id: resolved },
        };
        g.append_registry_event(ev);
        Ok(rec)
    }

    fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, StoreError> {
        let g = self.0.lock().unwrap();
        let Some(resolved) = g.locate(Kind::Agent, id.iri())? else {
            return Ok(None);
        };
        Ok(g.agents.get(&AgentId(resolved)).cloned())
    }

    fn agent_by_name(&self, name: &str) -> Result<Option<AgentRecord>, StoreError> {
        let key = validate_slug(name).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let g = self.0.lock().unwrap();
        let Some(id) = g.agent_by_name.get(&key).cloned() else {
            return Ok(None);
        };
        let resolved = AgentId(g.resolve(id.iri()).clone());
        Ok(g.agents.get(&resolved).cloned())
    }

    fn list_agents(
        &self,
        project: Option<&str>,
        include_retired: bool,
    ) -> Result<Vec<AgentRecord>, StoreError> {
        let project_filter = match project {
            Some(p) => Some(validate_slug(p).map_err(|e| StoreError::InvalidState(e.to_string()))?),
            None => None,
        };
        let g = self.0.lock().unwrap();
        let mut out: Vec<AgentRecord> = g
            .agents
            .values()
            .filter(|a| include_retired || a.status == AgentStatus::Active)
            .filter(|a| {
                project_filter
                    .as_ref()
                    .is_none_or(|p| &a.project == p || a.extra_areas.contains(p))
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name_key.cmp(&b.name_key));
        Ok(out)
    }

    fn registry_events_after(&self, seq: u64) -> Result<Vec<(u64, RegistryEvent)>, StoreError> {
        let g = self.0.lock().unwrap();
        Ok(g.registry_events
            .range((seq + 1)..)
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }

    fn import_registry_events(&self, events: &[RegistryEvent]) -> Result<(), StoreError> {
        let mut g = self.0.lock().unwrap();
        for ev in events {
            if let Some(&seq) = g.registry_event_ids.get(&ev.event_id) {
                let existing = g.registry_events.get(&seq).unwrap();
                if existing != ev {
                    return Err(StoreError::Tampered {
                        id: ev.event_id.clone(),
                    });
                }
                continue;
            }
            match &ev.kind {
                RegistryEventKind::AreaUpsert(area) => {
                    let norm = area
                        .clone()
                        .normalized()
                        .map_err(|e| StoreError::InvalidState(e.to_string()))?;
                    g.areas.insert(norm.area_key.clone(), norm);
                }
                RegistryEventKind::AgentUpsert(rec) => {
                    if let Some(existing_id) = g.agent_by_name.get(&rec.name_key).cloned()
                        && existing_id != rec.id
                    {
                        g.alias_unpublished_id(Kind::Agent, existing_id.iri(), rec.id.iri())?;
                        g.agents.remove(&existing_id);
                    }
                    if rec.role == AgentRole::Lead && rec.status == AgentStatus::Active {
                        for other in g.agents.values_mut() {
                            if other.project == rec.project
                                && other.role == AgentRole::Lead
                                && other.status == AgentStatus::Active
                                && other.id != rec.id
                            {
                                other.status = AgentStatus::Retired;
                            }
                        }
                    }
                    g.index_imported(Kind::Agent, rec.id.iri())?;
                    g.agent_by_name.insert(rec.name_key.clone(), rec.id.clone());
                    g.agents.insert(rec.id.clone(), rec.clone());
                }
                RegistryEventKind::AgentRetire { id } => {
                    let resolved = AgentId(g.resolve(id.iri()).clone());
                    if let Some(mut rec) = g.agents.get(&resolved).cloned() {
                        rec.status = AgentStatus::Retired;
                        rec.updated_at = ev.at;
                        rec.by = ev.by.clone();
                        g.agents.insert(resolved, rec);
                    }
                }
            }
            g.append_registry_event(ev.clone());
        }
        Ok(())
    }
}

impl StateStore for MemStore {
    fn upsert_state(&self, args: UpsertStateArgs) -> Result<StateEntry, StoreError> {
        let slug = validate_slug(&args.slug).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let concern = args.payload.concern();
        let key = (
            args.scope.as_wire(),
            concern.as_wire().to_string(),
            slug.clone(),
        );

        let mut g = self.0.lock().unwrap();
        let existing = g
            .state_by_key
            .get(&key)
            .cloned()
            .and_then(|id| g.state_entries.get(&id).cloned());

        if let Some(ref ex) = existing
            && let Some(conflict) = g.state_conflicts.get(&ex.id)
        {
            return Err(StoreError::StateConflict {
                entry_id: ex.id.clone(),
                local_head: conflict.local_head.clone(),
                remote_head: conflict.remote_head.clone(),
            });
        }

        let entry_id = match existing {
            Some(ref ex) => ex.id.clone(),
            None => {
                let raw = g.mint();
                g.index_new(Kind::State, &raw);
                StateId(raw)
            }
        };

        args.payload
            .validate(&entry_id)
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;

        if let StatePayload::Memory(ref m) = args.payload
            && let Some(ref target_id) = m.superseded_by
        {
            let resolved_target = StateId(g.resolve(target_id.iri()).clone());
            match g.state_entries.get(&resolved_target) {
                Some(target) if target.concern == StateConcern::Memory => {}
                _ => {
                    return Err(StoreError::InvalidState(format!(
                        "superseded_by target `{target_id}` does not exist as a Memory entry"
                    )));
                }
            }
        }

        let (sensitive, ever_sensitive, parent_revisions) = match existing {
            Some(ref ex) => {
                let s = match args.sensitivity {
                    SensitivityPatch::Inherit => ex.sensitive,
                    SensitivityPatch::SetSensitive => true,
                    SensitivityPatch::ClearSensitive => false,
                };
                let ever = ex.ever_sensitive || s;
                (s, ever, vec![ex.head_revision.clone()])
            }
            None => {
                let s = matches!(args.sensitivity, SensitivityPatch::SetSensitive);
                (s, s, Vec::new())
            }
        };

        let rev_id = RevisionId(g.mint());
        let rev = StateRevision {
            revision_id: rev_id.clone(),
            entry_id: entry_id.clone(),
            parent_revisions,
            scope: args.scope.clone(),
            concern,
            slug: slug.clone(),
            sensitive,
            ever_sensitive,
            payload: args.payload.clone(),
            at: args.at,
            by: args.by.trim().to_string(),
        };
        let entry = StateEntry {
            id: entry_id.clone(),
            scope: args.scope,
            concern,
            slug,
            sensitive,
            ever_sensitive,
            payload: args.payload,
            head_revision: rev_id,
            updated_at: args.at,
            by: args.by.trim().to_string(),
        };
        g.append_state_revision(rev);
        g.state_by_key.insert(key, entry_id.clone());
        g.state_entries.insert(entry_id, entry.clone());
        Ok(entry)
    }

    fn retire_memory(
        &self,
        scope: &StateScope,
        slug: &str,
        lifecycle: MemoryLifecycle,
        superseded_by: Option<StateId>,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError> {
        let slug = validate_slug(slug).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        if matches!(lifecycle, MemoryLifecycle::Active) {
            return Err(StoreError::InvalidState(
                "retire_memory requires `retired` or `superseded` lifecycle".into(),
            ));
        }
        let current = self
            .get_state_by_key(scope, StateConcern::Memory, &slug)?
            .ok_or_else(|| StoreError::NoSuchState(format!("{}:memory:{slug}", scope.as_wire())))?;
        let StatePayload::Memory(mut mem) = current.payload else {
            return Err(StoreError::InvalidState(format!(
                "state entry `{}` is not a Memory entry",
                current.id
            )));
        };
        mem.lifecycle = lifecycle;
        mem.superseded_by = superseded_by;
        self.upsert_state(UpsertStateArgs {
            scope: scope.clone(),
            slug,
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Memory(mem),
            at,
            by: by.to_string(),
        })
    }

    fn get_state(&self, id: &StateId) -> Result<Option<StateEntry>, StoreError> {
        let g = self.0.lock().unwrap();
        let Some(resolved) = g.locate(Kind::State, id.iri())? else {
            return Ok(None);
        };
        Ok(g.state_entries.get(&StateId(resolved)).cloned())
    }

    fn get_state_by_key(
        &self,
        scope: &StateScope,
        concern: StateConcern,
        slug: &str,
    ) -> Result<Option<StateEntry>, StoreError> {
        let slug = validate_slug(slug).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let key = (scope.as_wire(), concern.as_wire().to_string(), slug);
        let g = self.0.lock().unwrap();
        let Some(id) = g.state_by_key.get(&key).cloned() else {
            return Ok(None);
        };
        let resolved = StateId(g.resolve(id.iri()).clone());
        Ok(g.state_entries.get(&resolved).cloned())
    }

    fn list_state(
        &self,
        scope: Option<&StateScope>,
        concern: Option<StateConcern>,
    ) -> Result<Vec<StateEntry>, StoreError> {
        let g = self.0.lock().unwrap();
        let mut out: Vec<StateEntry> = g
            .state_entries
            .values()
            .filter(|e| scope.is_none_or(|s| &e.scope == s))
            .filter(|e| concern.is_none_or(|c| e.concern == c))
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            (a.scope.as_wire(), a.concern.as_wire(), &a.slug).cmp(&(
                b.scope.as_wire(),
                b.concern.as_wire(),
                &b.slug,
            ))
        });
        Ok(out)
    }

    fn state_revisions(&self, id: &StateId) -> Result<Vec<StateRevision>, StoreError> {
        let g = self.0.lock().unwrap();
        let resolved = StateId(g.check(Kind::State, id.iri())?);
        Ok(g.state_revisions
            .values()
            .filter(|r| r.entry_id == resolved)
            .cloned()
            .collect())
    }

    fn list_conflicts(&self) -> Result<Vec<StateConflict>, StoreError> {
        let g = self.0.lock().unwrap();
        Ok(g.state_conflicts.values().cloned().collect())
    }

    fn resolve_conflict(
        &self,
        id: &StateId,
        keep: &RevisionId,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError> {
        let mut g = self.0.lock().unwrap();
        let resolved = StateId(g.check(Kind::State, id.iri())?);
        let Some(conflict) = g.state_conflicts.get(&resolved).cloned() else {
            return Err(StoreError::InvalidState(format!(
                "state entry `{resolved}` has no active conflict"
            )));
        };
        if keep != &conflict.local_head && keep != &conflict.remote_head {
            return Err(StoreError::InvalidState(format!(
                "`--keep {keep}` must match either local head `{}` or remote head `{}`",
                conflict.local_head, conflict.remote_head
            )));
        }
        let Some(&seq) = g.state_revision_ids.get(keep) else {
            return Err(StoreError::InvalidState(format!(
                "unknown revision `{keep}`"
            )));
        };
        let chosen_rev = g.state_revisions.get(&seq).cloned().unwrap();
        let local_rev = g
            .state_revision_ids
            .get(&conflict.local_head)
            .and_then(|s| g.state_revisions.get(s))
            .cloned()
            .ok_or_else(|| StoreError::InvalidState("missing local_head revision".into()))?;
        let remote_rev = g
            .state_revision_ids
            .get(&conflict.remote_head)
            .and_then(|s| g.state_revisions.get(s))
            .cloned()
            .ok_or_else(|| StoreError::InvalidState("missing remote_head revision".into()))?;
        let existing = g
            .state_entries
            .get(&resolved)
            .cloned()
            .ok_or_else(|| StoreError::NoSuchState(resolved.to_string()))?;

        let merge_rev_id = RevisionId(g.mint());
        let sensitive = local_rev.sensitive || remote_rev.sensitive;
        let ever_sensitive = existing.ever_sensitive
            || local_rev.ever_sensitive
            || remote_rev.ever_sensitive
            || sensitive;
        let merge_rev = StateRevision {
            revision_id: merge_rev_id.clone(),
            entry_id: resolved.clone(),
            parent_revisions: sorted_parents(conflict.local_head, conflict.remote_head),
            scope: existing.scope.clone(),
            concern: existing.concern,
            slug: existing.slug.clone(),
            sensitive,
            ever_sensitive,
            payload: chosen_rev.payload.clone(),
            at,
            by: by.trim().to_string(),
        };
        g.append_state_revision(merge_rev);
        g.state_conflicts.remove(&resolved);
        let updated = StateEntry {
            id: resolved.clone(),
            scope: existing.scope,
            concern: existing.concern,
            slug: existing.slug,
            sensitive,
            ever_sensitive,
            payload: chosen_rev.payload,
            head_revision: merge_rev_id,
            updated_at: at,
            by: by.trim().to_string(),
        };
        g.state_entries.insert(resolved, updated.clone());
        Ok(updated)
    }

    fn state_revisions_after(&self, seq: u64) -> Result<Vec<(u64, StateRevision)>, StoreError> {
        let g = self.0.lock().unwrap();
        Ok(g.state_revisions
            .range((seq + 1)..)
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }

    fn import_state_revisions(
        &self,
        revs: &[StateRevision],
        merge_at: At,
    ) -> Result<(), StoreError> {
        let mut g = self.0.lock().unwrap();
        for rev in revs {
            if let Some(&seq) = g.state_revision_ids.get(&rev.revision_id) {
                let existing = g.state_revisions.get(&seq).unwrap();
                let mut normalized_rev = rev.clone();
                normalized_rev.entry_id = StateId(g.resolve(rev.entry_id.iri()).clone());
                if existing != &normalized_rev {
                    return Err(StoreError::Tampered {
                        id: rev.revision_id.0.clone(),
                    });
                }
                continue;
            }
            g.apply_imported_state_revision(rev.clone(), merge_at)?;
        }
        Ok(())
    }

    fn sync_cursor(&self, remote: &str) -> Result<Option<StateSyncCursor>, StoreError> {
        let g = self.0.lock().unwrap();
        Ok(g.sync_cursors.get(remote).cloned())
    }

    fn set_sync_cursor(&self, cursor: StateSyncCursor) -> Result<(), StoreError> {
        let mut g = self.0.lock().unwrap();
        g.sync_cursors.insert(cursor.remote.clone(), cursor);
        Ok(())
    }
}
```

5. Add the two new conformance test entries in `crates/core/src/mem.rs` inside `mod tests`:

```rust
    #[test]
    fn passes_agent_registry_conformance() {
        conformance::agent_registry(|| (MemStore::new(), ()));
    }

    #[test]
    fn passes_state_store_conformance() {
        conformance::state_store(|| (MemStore::new(), ()));
    }
```

- [ ] **Step 4: Run `cargo test -p fl-core -- --nocapture` and `cargo test --workspace`**

Run:
```bash
cargo test -p fl-core -- --nocapture
cargo test -p fl-github -- --nocapture
```
Expected: all `fl-core` unit and conformance suites (`catalog`, `tracker`, `ledger`, `local-handles`, `all-roles`, `agent-registry`, `state-store`) pass.

- [ ] **Step 5: Commit Task 2**

```bash
git add crates/core/src/state.rs crates/core/src/conformance.rs crates/core/src/mem.rs crates/github/src/tracker.rs
git commit -m "$(cat <<'EOF'
feat(core): implement StateStore and AgentRegistry on MemStore with conformance suites

Add deterministic 3-way DAG merge helpers in fl-core::state, implement
StateStore and AgentRegistry on MemStore, and add the 7-case
agent_registry and 10-case state_store conformance suites.

Co-authored-by: Ferris <ferris@artificialhumanity.io>
EOF
)"
```

---
### Task 3: RedbStore Format 6 schema and `StateStore` / `AgentRegistry` implementation (`fl-store`)

**Files:**
- Modify: `crates/store/src/lib.rs`
- Create: `crates/store/src/state.rs`

**Interfaces:**
- Consumes: `fl_core::state::*`, `fl_core::agent::*`, `fl_core::store::{StateStore, AgentRegistry, StoreError}`, `RedbStore` internals (`META`, `IDS`, `ALIASES`, `NEXT_HANDLE`, `BY_HANDLE`, `HANDLE_OF`, `backend`, `decode`, `mint`, `index_new`, `raise_format`, `locate`, `locate_for_write`, `check`, `check_kind`).
- Produces:
  - `FORMAT_WITH_STATE: u64 = 6` in `crates/store/src/lib.rs`, accepted by `RedbStore::open` (`2..=FORMAT_WITH_STATE`) and initialized on new stores and via `raise_format(&wtx, FORMAT_WITH_STATE)` on state/agent writes.
  - 11 new additive redb tables (`STATE_ENTRIES`, `STATE_BY_KEY`, `STATE_REVISIONS`, `STATE_REV_BY_ID`, `STATE_CONFLICTS`, `STATE_SYNC`, `AREAS`, `AGENTS`, `AGENT_BY_NAME`, `REGISTRY_EVENTS`, `REGISTRY_EVENT_BY_ID`).
  - `impl AgentRegistry for RedbStore` and `impl StateStore for RedbStore` in `crates/store/src/state.rs`, passing `conformance::agent_registry` and `conformance::state_store` plus Format 5 -> 6 lazy upgrade tests.

- [ ] **Step 1: Update `crates/store/src/lib.rs` to define `FORMAT_WITH_STATE = 6`, the 11 new tables, and `pub(crate)` helpers**

Make the following updates to `crates/store/src/lib.rs`:

1. Add `mod state;` at the top of `crates/store/src/lib.rs` and update the format constants and table definitions:

```rust
mod state;

/// Format version written to new stores before any multi-project write.
const FORMAT_BASE: u64 = 2;
/// Format version once a store holds multiple projects or gate `project_id`.
const FORMAT_MULTI_PROJECT: u64 = 3;
/// Format version once a store holds records or an alias table.
const FORMAT_WITH_TRACKER: u64 = 4;
/// Format version once a store holds findings or per-kind handle tables.
const FORMAT_WITH_LEDGER: u64 = 5;
/// Format version once a store holds state entries, revisions, or agent registry rows.
pub(crate) const FORMAT_WITH_STATE: u64 = 6;

pub(crate) const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");
const GATES: TableDefinition<&str, &str> = TableDefinition::new("gates");
const RECORDS: TableDefinition<&str, &str> = TableDefinition::new("records");
const EVENTS: TableDefinition<(&str, u64), &str> = TableDefinition::new("events");
const FINDINGS: TableDefinition<&str, &str> = TableDefinition::new("findings");
/// Every minted id → the `Kind::as_tag` that owns it, or `"alias"` once folded.
pub(crate) const IDS: TableDefinition<&str, &str> = TableDefinition::new("ids");
/// Non-canonical id → canonical id (single hop; updated transitively on fold).
pub(crate) const ALIASES: TableDefinition<&str, &str> = TableDefinition::new("aliases");
/// `Kind::as_tag` → last assigned handle for that kind.
pub(crate) const NEXT_HANDLE: TableDefinition<&str, u64> = TableDefinition::new("next_handle");
/// `(Kind::as_tag, handle)` → canonical IRI.
pub(crate) const BY_HANDLE: TableDefinition<(&str, u64), &str> = TableDefinition::new("by_handle");
/// Canonical IRI → handle within its kind.
pub(crate) const HANDLE_OF: TableDefinition<&str, u64> = TableDefinition::new("handle_of");

/// Canonical `StateId` IRI → JSON `StateEntry`.
pub(crate) const STATE_ENTRIES: TableDefinition<&str, &str> =
    TableDefinition::new("state_entries");
/// `(scope_wire, concern_wire, slug)` → canonical `StateId` IRI.
pub(crate) const STATE_BY_KEY: TableDefinition<(&str, &str, &str), &str> =
    TableDefinition::new("state_by_key");
/// Local append sequence `u64` → JSON `StateRevision`.
pub(crate) const STATE_REVISIONS: TableDefinition<u64, &str> =
    TableDefinition::new("state_revisions");
/// `RevisionId` IRI → local sequence `u64` in `STATE_REVISIONS`.
pub(crate) const STATE_REV_BY_ID: TableDefinition<&str, u64> =
    TableDefinition::new("state_rev_by_id");
/// Canonical `StateId` IRI → JSON `StateConflict`.
pub(crate) const STATE_CONFLICTS: TableDefinition<&str, &str> =
    TableDefinition::new("state_conflicts");
/// Remote name (`"origin"`) → JSON `StateSyncCursor`.
pub(crate) const STATE_SYNC: TableDefinition<&str, &str> = TableDefinition::new("state_sync");
/// Normalized `area_key` → JSON `AreaRecord`.
pub(crate) const AREAS: TableDefinition<&str, &str> = TableDefinition::new("areas");
/// Canonical `AgentId` IRI → JSON `AgentRecord`.
pub(crate) const AGENTS: TableDefinition<&str, &str> = TableDefinition::new("agents");
/// Normalized `name_key` → canonical `AgentId` IRI.
pub(crate) const AGENT_BY_NAME: TableDefinition<&str, &str> =
    TableDefinition::new("agent_by_name");
/// Local append sequence `u64` → JSON `RegistryEvent`.
pub(crate) const REGISTRY_EVENTS: TableDefinition<u64, &str> =
    TableDefinition::new("registry_events");
/// `event_id` IRI → local sequence `u64` in `REGISTRY_EVENTS`.
pub(crate) const REGISTRY_EVENT_BY_ID: TableDefinition<&str, u64> =
    TableDefinition::new("registry_event_by_id");
```

2. Update `RedbStore::open` in `crates/store/src/lib.rs` to accept `FORMAT_BASE..=FORMAT_WITH_STATE` and open all 11 state/agent tables so read transactions never hit missing-table errors:

```rust
        {
            let wtx = db.begin_write().map_err(backend)?;
            {
                let mut meta = wtx.open_table(META).map_err(backend)?;
                let existing = meta.get("format").map_err(backend)?.map(|v| v.value());
                match existing {
                    None => {
                        meta.insert("format", FORMAT_BASE).map_err(backend)?;
                        meta.insert("seq", 0).map_err(backend)?;
                    }
                    Some(v) if (FORMAT_BASE..=FORMAT_WITH_STATE).contains(&v) => {}
                    Some(other) => return Err(StoreError::UnsupportedFormat(other)),
                }
            }
            let _ = wtx.open_table(PROJECTS).map_err(backend)?;
            let _ = wtx.open_table(GATES).map_err(backend)?;
            let _ = wtx.open_table(RECORDS).map_err(backend)?;
            let _ = wtx.open_table(EVENTS).map_err(backend)?;
            let _ = wtx.open_table(FINDINGS).map_err(backend)?;
            let _ = wtx.open_table(IDS).map_err(backend)?;
            let _ = wtx.open_table(ALIASES).map_err(backend)?;
            let _ = wtx.open_table(NEXT_HANDLE).map_err(backend)?;
            let _ = wtx.open_table(BY_HANDLE).map_err(backend)?;
            let _ = wtx.open_table(HANDLE_OF).map_err(backend)?;
            let _ = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
            let _ = wtx.open_table(STATE_BY_KEY).map_err(backend)?;
            let _ = wtx.open_table(STATE_REVISIONS).map_err(backend)?;
            let _ = wtx.open_table(STATE_REV_BY_ID).map_err(backend)?;
            let _ = wtx.open_table(STATE_CONFLICTS).map_err(backend)?;
            let _ = wtx.open_table(STATE_SYNC).map_err(backend)?;
            let _ = wtx.open_table(AREAS).map_err(backend)?;
            let _ = wtx.open_table(AGENTS).map_err(backend)?;
            let _ = wtx.open_table(AGENT_BY_NAME).map_err(backend)?;
            let _ = wtx.open_table(REGISTRY_EVENTS).map_err(backend)?;
            let _ = wtx.open_table(REGISTRY_EVENT_BY_ID).map_err(backend)?;
            wtx.commit().map_err(backend)?;
        }
```

3. Change `fn backend`, `fn decode`, `fn mint`, `fn index_new`, `fn raise_format`, `fn locate`, `fn locate_for_write`, `fn check`, and `fn check_kind` in `crates/store/src/lib.rs` from private `fn` to `pub(crate) fn`, and add `resolve_in_wtx`, `index_imported_in`, and `alias_unpublished_in` on `RedbStore`:

```rust
    pub(crate) fn resolve_in_wtx(
        &self,
        wtx: &WriteTransaction,
        id: &Iri,
    ) -> Result<Iri, StoreError> {
        let aliases = wtx.open_table(ALIASES).map_err(backend)?;
        if let Some(canon) = aliases.get(id.as_str()).map_err(backend)? {
            return Iri::parse(canon.value()).map_err(|_| StoreError::Corrupt {
                id: id.to_string(),
                reason: "invalid canonical IRI in aliases".into(),
            });
        }
        Ok(id.clone())
    }

    pub(crate) fn index_imported_in(
        &self,
        wtx: &WriteTransaction,
        kind: Kind,
        id: &Iri,
    ) -> Result<(), StoreError> {
        let canonical = self.resolve_in_wtx(wtx, id)?;
        let existing_slot = {
            let ids = wtx.open_table(IDS).map_err(backend)?;
            ids.get(canonical.as_str())
                .map_err(backend)?
                .map(|g| g.value().to_owned())
        };
        match existing_slot {
            Some(slot) if slot == "alias" => Ok(()),
            Some(slot) => Self::check_kind(canonical.as_str(), &slot, kind),
            None => self.index_new(wtx, kind, &canonical),
        }
    }

    pub(crate) fn alias_unpublished_in(
        &self,
        wtx: &WriteTransaction,
        kind: Kind,
        old_id: &Iri,
        new_id: &Iri,
    ) -> Result<(), StoreError> {
        if old_id == new_id {
            return Ok(());
        }
        let tag = kind.as_tag();
        {
            let mut ids = wtx.open_table(IDS).map_err(backend)?;
            let existing = ids
                .get(old_id.as_str())
                .map_err(backend)?
                .map(|g| g.value().to_owned());
            if let Some(ref slot) = existing
                && slot != "alias"
            {
                Self::check_kind(old_id.as_str(), slot, kind)?;
            }
            ids.insert(old_id.as_str(), "alias").map_err(backend)?;
            ids.insert(new_id.as_str(), tag).map_err(backend)?;
        }
        {
            let mut aliases = wtx.open_table(ALIASES).map_err(backend)?;
            let redirect: Vec<String> = aliases
                .iter()
                .map_err(backend)?
                .filter_map(|row| {
                    let (k, v) = row.ok()?;
                    (v.value() == old_id.as_str()).then(|| k.value().to_owned())
                })
                .collect();
            for k in redirect {
                aliases
                    .insert(k.as_str(), new_id.as_str())
                    .map_err(backend)?;
            }
            aliases
                .insert(old_id.as_str(), new_id.as_str())
                .map_err(backend)?;
        }
        {
            let mut next = wtx.open_table(NEXT_HANDLE).map_err(backend)?;
            let mut by_h = wtx.open_table(BY_HANDLE).map_err(backend)?;
            let mut h_of = wtx.open_table(HANDLE_OF).map_err(backend)?;
            let old_h = h_of
                .remove(old_id.as_str())
                .map_err(backend)?
                .map(|g| g.value());
            if let Some(h) = old_h {
                by_h.insert((tag, h), new_id.as_str()).map_err(backend)?;
                h_of.insert(new_id.as_str(), h).map_err(backend)?;
            } else if h_of.get(new_id.as_str()).map_err(backend)?.is_none() {
                let n = next
                    .get(tag)
                    .map_err(backend)?
                    .map(|v| v.value())
                    .unwrap_or(0)
                    + 1;
                next.insert(tag, n).map_err(backend)?;
                by_h.insert((tag, n), new_id.as_str()).map_err(backend)?;
                h_of.insert(new_id.as_str(), n).map_err(backend)?;
            }
        }
        Ok(())
    }
```

- [ ] **Step 2: Create `crates/store/src/state.rs` implementing `AgentRegistry` and `StateStore` for `RedbStore`**

Create `crates/store/src/state.rs`:

```rust
use std::collections::BTreeMap;

use fl_core::agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, RegistryEvent,
    RegistryEventKind, validate_agent_identity,
};
use fl_core::at::At;
use fl_core::ids::{AgentId, Kind, RevisionId, StateId};
use fl_core::iri::Iri;
use fl_core::state::{
    MemoryLifecycle, MergeMode, SensitivityPatch, StateConcern, StateConflict, StateEntry,
    StatePayload, StateRevision, StateScope, StateSyncCursor, UpsertStateArgs,
    compute_append_merge, compute_last_writer_merge, is_ancestor_in_dag,
    lowest_common_ancestor_in_dag, sorted_parents, validate_slug,
};
use fl_core::store::{AgentRegistry, StateStore, StoreError};
use redb::{ReadableDatabase, ReadableTable, WriteTransaction};

use crate::{
    AGENT_BY_NAME, AGENTS, AREAS, FORMAT_WITH_STATE, META, REGISTRY_EVENT_BY_ID, REGISTRY_EVENTS,
    RedbStore, STATE_BY_KEY, STATE_CONFLICTS, STATE_ENTRIES, STATE_REV_BY_ID, STATE_REVISIONS,
    STATE_SYNC, backend, decode,
};

impl RedbStore {
    fn next_meta_counter(&self, wtx: &WriteTransaction, key: &str) -> Result<u64, StoreError> {
        let mut meta = wtx.open_table(META).map_err(backend)?;
        let next = meta
            .get(key)
            .map_err(backend)?
            .map(|v| v.value())
            .unwrap_or(0)
            + 1;
        meta.insert(key, next).map_err(backend)?;
        Ok(next)
    }

    fn append_registry_event_in(
        &self,
        wtx: &WriteTransaction,
        ev: &RegistryEvent,
    ) -> Result<u64, StoreError> {
        let seq = self.next_meta_counter(wtx, "registry_event_seq")?;
        let json = serde_json::to_string(ev).expect("RegistryEvent is always serializable");
        let mut events = wtx.open_table(REGISTRY_EVENTS).map_err(backend)?;
        let mut by_id = wtx.open_table(REGISTRY_EVENT_BY_ID).map_err(backend)?;
        events.insert(seq, json.as_str()).map_err(backend)?;
        by_id
            .insert(ev.event_id.as_str(), seq)
            .map_err(backend)?;
        Ok(seq)
    }

    fn append_state_revision_in(
        &self,
        wtx: &WriteTransaction,
        rev: &StateRevision,
    ) -> Result<u64, StoreError> {
        let seq = self.next_meta_counter(wtx, "state_rev_seq")?;
        let json = serde_json::to_string(rev).expect("StateRevision is always serializable");
        let mut revs = wtx.open_table(STATE_REVISIONS).map_err(backend)?;
        let mut by_id = wtx.open_table(STATE_REV_BY_ID).map_err(backend)?;
        revs.insert(seq, json.as_str()).map_err(backend)?;
        by_id
            .insert(rev.revision_id.as_str(), seq)
            .map_err(backend)?;
        Ok(seq)
    }

    fn load_entry_dag_in(
        &self,
        wtx: &WriteTransaction,
        entry_id: &StateId,
    ) -> Result<BTreeMap<RevisionId, StateRevision>, StoreError> {
        let revs = wtx.open_table(STATE_REVISIONS).map_err(backend)?;
        let mut dag = BTreeMap::new();
        for row in revs.iter().map_err(backend)? {
            let (seq, raw) = row.map_err(backend)?;
            let rev: StateRevision =
                decode(raw.value(), &format!("state_revision#{}", seq.value()))?;
            if &rev.entry_id == entry_id {
                dag.insert(rev.revision_id.clone(), rev);
            }
        }
        Ok(dag)
    }

    fn apply_imported_state_revision_in(
        &self,
        wtx: &WriteTransaction,
        rev: StateRevision,
        merge_at: At,
    ) -> Result<(), StoreError> {
        rev.payload
            .validate(&rev.entry_id)
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;

        {
            let by_id = wtx.open_table(STATE_REV_BY_ID).map_err(backend)?;
            for parent in &rev.parent_revisions {
                if by_id.get(parent.as_str()).map_err(backend)?.is_none() {
                    return Err(StoreError::InvalidState(format!(
                        "revision `{}` references unknown parent `{}`",
                        rev.revision_id, parent
                    )));
                }
            }
        }

        if let StatePayload::Memory(ref m) = rev.payload
            && let Some(ref target_id) = m.superseded_by
        {
            let resolved_target = self.resolve_in_wtx(wtx, target_id.iri())?;
            let entries = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
            let raw = entries
                .get(resolved_target.as_str())
                .map_err(backend)?
                .map(|g| g.value().to_owned());
            match raw {
                Some(json) => {
                    let target: StateEntry = decode(&json, resolved_target.as_str())?;
                    if target.concern != StateConcern::Memory {
                        return Err(StoreError::InvalidState(format!(
                            "superseded_by target `{target_id}` is not a Memory entry"
                        )));
                    }
                }
                None => {
                    return Err(StoreError::InvalidState(format!(
                        "superseded_by target `{target_id}` does not exist"
                    )));
                }
            }
        }

        let scope_wire = rev.scope.as_wire();
        let concern_wire = rev.concern.as_wire();

        // Check offline same-key collision (`S_local != S_remote`).
        let existing_key_id = {
            let by_key = wtx.open_table(STATE_BY_KEY).map_err(backend)?;
            by_key
                .get((scope_wire.as_str(), concern_wire, rev.slug.as_str()))
                .map_err(backend)?
                .map(|g| g.value().to_owned())
        };
        if let Some(local_raw) = existing_key_id {
            let local_iri = Iri::parse(&local_raw).map_err(|_| StoreError::Corrupt {
                id: local_raw.clone(),
                reason: "invalid StateId in state_by_key".into(),
            })?;
            let local_id = StateId(local_iri);
            if local_id != rev.entry_id {
                self.alias_unpublished_in(wtx, Kind::State, local_id.iri(), rev.entry_id.iri())?;
                // Re-key STATE_ENTRIES.
                {
                    let mut entries = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
                    let old_json = entries
                        .remove(local_id.as_str())
                        .map_err(backend)?
                        .map(|g| g.value().to_owned());
                    if let Some(raw) = old_json {
                        let mut entry: StateEntry = decode(&raw, local_id.as_str())?;
                        entry.id = rev.entry_id.clone();
                        let updated_raw = serde_json::to_string(&entry).unwrap();
                        entries
                            .insert(rev.entry_id.as_str(), updated_raw.as_str())
                            .map_err(backend)?;
                    }
                }
                // Re-key STATE_CONFLICTS.
                {
                    let mut conflicts = wtx.open_table(STATE_CONFLICTS).map_err(backend)?;
                    let old_conflict = conflicts
                        .remove(local_id.as_str())
                        .map_err(backend)?
                        .map(|g| g.value().to_owned());
                    if let Some(raw) = old_conflict {
                        let mut c: StateConflict = decode(&raw, local_id.as_str())?;
                        c.entry_id = rev.entry_id.clone();
                        let updated_raw = serde_json::to_string(&c).unwrap();
                        conflicts
                            .insert(rev.entry_id.as_str(), updated_raw.as_str())
                            .map_err(backend)?;
                    }
                }
                // Rewrite unpublished STATE_REVISIONS whose entry_id was `local_id`.
                {
                    let mut revs = wtx.open_table(STATE_REVISIONS).map_err(backend)?;
                    let mut to_rewrite = Vec::new();
                    for row in revs.iter().map_err(backend)? {
                        let (seq, raw) = row.map_err(backend)?;
                        let mut stored: StateRevision =
                            decode(raw.value(), &format!("state_revision#{}", seq.value()))?;
                        if stored.entry_id == local_id {
                            stored.entry_id = rev.entry_id.clone();
                            to_rewrite.push((seq.value(), serde_json::to_string(&stored).unwrap()));
                        }
                    }
                    for (seq, json) in to_rewrite {
                        revs.insert(seq, json.as_str()).map_err(backend)?;
                    }
                }
                // Update STATE_BY_KEY.
                {
                    let mut by_key = wtx.open_table(STATE_BY_KEY).map_err(backend)?;
                    by_key
                        .insert(
                            (scope_wire.as_str(), concern_wire, rev.slug.as_str()),
                            rev.entry_id.as_str(),
                        )
                        .map_err(backend)?;
                }
            }
        }

        let resolved_entry_id = StateId(self.resolve_in_wtx(wtx, rev.entry_id.iri())?);
        let mut rev = rev;
        rev.entry_id = resolved_entry_id.clone();
        self.index_imported_in(wtx, Kind::State, rev.entry_id.iri())?;

        let existing: Option<StateEntry> = {
            let entries = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
            let raw = entries
                .get(resolved_entry_id.as_str())
                .map_err(backend)?
                .map(|g| g.value().to_owned());
            match raw {
                Some(s) => Some(decode(&s, resolved_entry_id.as_str())?),
                None => None,
            }
        };

        if let Some(ref ex) = existing
            && (ex.scope != rev.scope || ex.concern != rev.concern || ex.slug != rev.slug)
        {
            return Err(StoreError::Tampered {
                id: resolved_entry_id.0,
            });
        }

        self.append_state_revision_in(wtx, &rev)?;

        let Some(existing) = existing else {
            let entry = StateEntry {
                id: resolved_entry_id.clone(),
                scope: rev.scope,
                concern: rev.concern,
                slug: rev.slug.clone(),
                sensitive: rev.sensitive,
                ever_sensitive: rev.ever_sensitive || rev.sensitive,
                payload: rev.payload,
                head_revision: rev.revision_id,
                updated_at: rev.at,
                by: rev.by,
            };
            let entry_json = serde_json::to_string(&entry).unwrap();
            {
                let mut entries = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
                entries
                    .insert(resolved_entry_id.as_str(), entry_json.as_str())
                    .map_err(backend)?;
            }
            {
                let mut by_key = wtx.open_table(STATE_BY_KEY).map_err(backend)?;
                by_key
                    .insert(
                        (scope_wire.as_str(), concern_wire, rev.slug.as_str()),
                        resolved_entry_id.as_str(),
                    )
                    .map_err(backend)?;
            }
            return Ok(());
        };

        let dag = self.load_entry_dag_in(wtx, &resolved_entry_id)?;
        let r_local = existing.head_revision.clone();
        let r_remote = rev.revision_id.clone();

        // Case 1: Remote is descendant of local -> fast-forward.
        if is_ancestor_in_dag(&r_local, &r_remote, &dag) {
            let updated = StateEntry {
                id: resolved_entry_id.clone(),
                scope: existing.scope,
                concern: existing.concern,
                slug: existing.slug,
                sensitive: rev.sensitive,
                ever_sensitive: existing.ever_sensitive || rev.ever_sensitive || rev.sensitive,
                payload: rev.payload,
                head_revision: r_remote.clone(),
                updated_at: rev.at,
                by: rev.by,
            };
            let updated_json = serde_json::to_string(&updated).unwrap();
            {
                let mut entries = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
                entries
                    .insert(resolved_entry_id.as_str(), updated_json.as_str())
                    .map_err(backend)?;
            }
            {
                let mut conflicts = wtx.open_table(STATE_CONFLICTS).map_err(backend)?;
                let existing_c = conflicts
                    .get(resolved_entry_id.as_str())
                    .map_err(backend)?
                    .map(|g| g.value().to_owned());
                if let Some(raw_c) = existing_c {
                    let c: StateConflict = decode(&raw_c, resolved_entry_id.as_str())?;
                    if is_ancestor_in_dag(&c.local_head, &r_remote, &dag)
                        && is_ancestor_in_dag(&c.remote_head, &r_remote, &dag)
                    {
                        conflicts
                            .remove(resolved_entry_id.as_str())
                            .map_err(backend)?;
                    }
                }
            }
            return Ok(());
        }

        // Case 2: Local is already descendant of remote -> no-op.
        if is_ancestor_in_dag(&r_remote, &r_local, &dag) {
            return Ok(());
        }

        // Case 3: True divergence between r_local and r_remote.
        let local_rev = dag.get(&r_local).cloned().ok_or_else(|| {
            StoreError::InvalidState(format!("missing local head revision `{r_local}`"))
        })?;
        let base_rev = lowest_common_ancestor_in_dag(&r_local, &r_remote, &dag)
            .and_then(|id| dag.get(&id).cloned());

        let auto_merged: Option<StatePayload> = match (&local_rev.payload, &rev.payload) {
            (StatePayload::Runtime(l_rt), StatePayload::Runtime(r_rt))
                if l_rt.merge_mode == r_rt.merge_mode =>
            {
                match l_rt.merge_mode {
                    MergeMode::Conflict => None,
                    MergeMode::Append => {
                        let base_arr = match base_rev.as_ref().map(|b| &b.payload) {
                            Some(StatePayload::Runtime(b_rt)) => b_rt.value.as_array().cloned(),
                            _ => Some(Vec::new()),
                        };
                        match (base_arr, l_rt.value.as_array(), r_rt.value.as_array()) {
                            (Some(b), Some(l), Some(r)) => Some(StatePayload::Runtime(
                                fl_core::state::RuntimeEntry {
                                    value: serde_json::Value::Array(compute_append_merge(b, l, r)),
                                    merge_mode: MergeMode::Append,
                                },
                            )),
                            _ => None,
                        }
                    }
                    MergeMode::LastWriter => {
                        let win_payload = compute_last_writer_merge(&local_rev, &rev).payload.clone();
                        Some(win_payload)
                    }
                }
            }
            _ => None,
        };

        if let Some(merged_payload) = auto_merged {
            let merge_rev_id = RevisionId(self.mint());
            let sensitive = local_rev.sensitive || rev.sensitive;
            let ever_sensitive = existing.ever_sensitive
                || local_rev.ever_sensitive
                || rev.ever_sensitive
                || sensitive;
            let merge_rev = StateRevision {
                revision_id: merge_rev_id.clone(),
                entry_id: resolved_entry_id.clone(),
                parent_revisions: sorted_parents(r_local, r_remote),
                scope: existing.scope.clone(),
                concern: existing.concern,
                slug: existing.slug.clone(),
                sensitive,
                ever_sensitive,
                payload: merged_payload.clone(),
                at: merge_at,
                by: rev.by.clone(),
            };
            self.append_state_revision_in(wtx, &merge_rev)?;
            let updated = StateEntry {
                id: resolved_entry_id.clone(),
                scope: existing.scope,
                concern: existing.concern,
                slug: existing.slug,
                sensitive,
                ever_sensitive,
                payload: merged_payload,
                head_revision: merge_rev_id,
                updated_at: merge_at,
                by: rev.by,
            };
            let updated_json = serde_json::to_string(&updated).unwrap();
            let mut entries = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
            entries
                .insert(resolved_entry_id.as_str(), updated_json.as_str())
                .map_err(backend)?;
        } else {
            let conflict = StateConflict {
                entry_id: resolved_entry_id.clone(),
                local_head: r_local,
                remote_head: r_remote,
                detected_at: merge_at,
            };
            let conflict_json = serde_json::to_string(&conflict).unwrap();
            let mut conflicts = wtx.open_table(STATE_CONFLICTS).map_err(backend)?;
            conflicts
                .insert(resolved_entry_id.as_str(), conflict_json.as_str())
                .map_err(backend)?;
        }
        Ok(())
    }
}

impl AgentRegistry for RedbStore {
    fn upsert_area(&self, area: AreaRecord) -> Result<AreaRecord, StoreError> {
        let normalized = area
            .normalized()
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let wtx = self.db.begin_write().map_err(backend)?;
        self.raise_format(&wtx, FORMAT_WITH_STATE)?;
        let json = serde_json::to_string(&normalized).unwrap();
        {
            let mut areas = wtx.open_table(AREAS).map_err(backend)?;
            areas
                .insert(normalized.area_key.as_str(), json.as_str())
                .map_err(backend)?;
        }
        let ev = RegistryEvent {
            event_id: self.mint(),
            at: normalized.updated_at,
            by: normalized.by.clone(),
            kind: RegistryEventKind::AreaUpsert(normalized.clone()),
        };
        self.append_registry_event_in(&wtx, &ev)?;
        wtx.commit().map_err(backend)?;
        Ok(normalized)
    }

    fn get_area(&self, area_key: &str) -> Result<Option<AreaRecord>, StoreError> {
        let key = validate_slug(area_key).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let rtx = self.db.begin_read().map_err(backend)?;
        let areas = rtx.open_table(AREAS).map_err(backend)?;
        let Some(raw) = areas.get(key.as_str()).map_err(backend)? else {
            return Ok(None);
        };
        Ok(Some(decode(raw.value(), &key)?))
    }

    fn list_areas(&self) -> Result<Vec<AreaRecord>, StoreError> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let areas = rtx.open_table(AREAS).map_err(backend)?;
        let mut out = Vec::new();
        for row in areas.iter().map_err(backend)? {
            let (k, v) = row.map_err(backend)?;
            out.push(decode(v.value(), k.value())?);
        }
        Ok(out)
    }

    fn register_agent(&self, args: RegisterAgentArgs) -> Result<AgentRecord, StoreError> {
        let (name_key, display_name, email) =
            validate_agent_identity(&args.display_name, args.email.as_deref())
                .map_err(StoreError::InvalidState)?;
        let project =
            validate_slug(&args.project).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let mut extra_areas = Vec::with_capacity(args.extra_areas.len());
        for a in &args.extra_areas {
            extra_areas
                .push(validate_slug(a).map_err(|e| StoreError::InvalidState(e.to_string()))?);
        }
        extra_areas.sort();
        extra_areas.dedup();

        let wtx = self.db.begin_write().map_err(backend)?;
        self.raise_format(&wtx, FORMAT_WITH_STATE)?;

        // 1. Validate / create areas.
        for area_key in std::iter::once(&project).chain(extra_areas.iter()) {
            let exists = {
                let areas = wtx.open_table(AREAS).map_err(backend)?;
                areas.get(area_key.as_str()).map_err(backend)?.is_some()
            };
            if !exists {
                if args.create_area {
                    let area = AreaRecord {
                        area_key: area_key.clone(),
                        name: area_key.clone(),
                        path: None,
                        purpose: format!("Registered area {area_key}"),
                        updated_at: args.at,
                        by: args.by.clone(),
                    };
                    let area_json = serde_json::to_string(&area).unwrap();
                    {
                        let mut areas = wtx.open_table(AREAS).map_err(backend)?;
                        areas
                            .insert(area_key.as_str(), area_json.as_str())
                            .map_err(backend)?;
                    }
                    let ev = RegistryEvent {
                        event_id: self.mint(),
                        at: args.at,
                        by: args.by.clone(),
                        kind: RegistryEventKind::AreaUpsert(area),
                    };
                    self.append_registry_event_in(&wtx, &ev)?;
                } else {
                    return Err(StoreError::NoSuchArea(area_key.clone()));
                }
            }
        }

        // 2. Check existing agent by name_key.
        let existing_by_name: Option<AgentRecord> = {
            let by_name = wtx.open_table(AGENT_BY_NAME).map_err(backend)?;
            let agents = wtx.open_table(AGENTS).map_err(backend)?;
            let id_opt = by_name
                .get(name_key.as_str())
                .map_err(backend)?
                .map(|g| g.value().to_owned());
            match id_opt {
                Some(id_str) => {
                    let raw = agents
                        .get(id_str.as_str())
                        .map_err(backend)?
                        .map(|g| g.value().to_owned());
                    match raw {
                        Some(json) => Some(decode(&json, &id_str)?),
                        None => None,
                    }
                }
                None => None,
            }
        };

        if let Some(ref existing) = existing_by_name
            && !args.replace
        {
            return Err(StoreError::AgentCollision {
                name_key,
                existing_id: existing.id.clone(),
                status: existing.status,
            });
        }

        // 3. Check one-lead-per-project invariant.
        if args.role == AgentRole::Lead {
            let mut conflicting_lead: Option<AgentRecord> = None;
            {
                let agents = wtx.open_table(AGENTS).map_err(backend)?;
                for row in agents.iter().map_err(backend)? {
                    let (k, v) = row.map_err(backend)?;
                    let rec: AgentRecord = decode(v.value(), k.value())?;
                    if rec.status == AgentStatus::Active
                        && rec.project == project
                        && rec.role == AgentRole::Lead
                        && rec.name_key != name_key
                    {
                        conflicting_lead = Some(rec);
                        break;
                    }
                }
            }
            if let Some(lead) = conflicting_lead {
                if !args.replace {
                    return Err(StoreError::LeadTaken {
                        project,
                        lead_name: lead.display_name,
                        lead_id: lead.id,
                    });
                }
                let mut retired = lead.clone();
                retired.status = AgentStatus::Retired;
                retired.updated_at = args.at;
                retired.by = args.by.clone();
                let retired_json = serde_json::to_string(&retired).unwrap();
                {
                    let mut agents = wtx.open_table(AGENTS).map_err(backend)?;
                    agents
                        .insert(retired.id.as_str(), retired_json.as_str())
                        .map_err(backend)?;
                }
                let retire_ev = RegistryEvent {
                    event_id: self.mint(),
                    at: args.at,
                    by: args.by.clone(),
                    kind: RegistryEventKind::AgentRetire { id: retired.id },
                };
                self.append_registry_event_in(&wtx, &retire_ev)?;
            }
        }

        let agent_id = match existing_by_name {
            Some(existing) => existing.id,
            None => {
                let raw = self.mint();
                self.index_new(&wtx, Kind::Agent, &raw)?;
                AgentId(raw)
            }
        };

        let record = AgentRecord {
            id: agent_id.clone(),
            name_key: name_key.clone(),
            display_name,
            email,
            project,
            role: args.role,
            extra_areas,
            status: AgentStatus::Active,
            updated_at: args.at,
            by: args.by.clone(),
        };
        let record_json = serde_json::to_string(&record).unwrap();
        {
            let mut by_name = wtx.open_table(AGENT_BY_NAME).map_err(backend)?;
            by_name
                .insert(name_key.as_str(), agent_id.as_str())
                .map_err(backend)?;
            let mut agents = wtx.open_table(AGENTS).map_err(backend)?;
            agents
                .insert(agent_id.as_str(), record_json.as_str())
                .map_err(backend)?;
        }
        let ev = RegistryEvent {
            event_id: self.mint(),
            at: args.at,
            by: args.by,
            kind: RegistryEventKind::AgentUpsert(record.clone()),
        };
        self.append_registry_event_in(&wtx, &ev)?;
        wtx.commit().map_err(backend)?;
        Ok(record)
    }

    fn retire_agent(&self, id: &AgentId, at: At, by: &str) -> Result<AgentRecord, StoreError> {
        let wtx = self.db.begin_write().map_err(backend)?;
        self.raise_format(&wtx, FORMAT_WITH_STATE)?;
        let Some(resolved) = self.locate_for_write(&wtx, Kind::Agent, id.iri())? else {
            return Err(StoreError::NotFound(id.0.clone()));
        };
        let mut rec: AgentRecord = {
            let agents = wtx.open_table(AGENTS).map_err(backend)?;
            let Some(raw) = agents.get(resolved.as_str()).map_err(backend)? else {
                return Err(StoreError::NoSuchAgent(resolved.to_string()));
            };
            decode(raw.value(), resolved.as_str())?
        };
        rec.status = AgentStatus::Retired;
        rec.updated_at = at;
        rec.by = by.trim().to_string();
        let rec_json = serde_json::to_string(&rec).unwrap();
        {
            let mut agents = wtx.open_table(AGENTS).map_err(backend)?;
            agents
                .insert(resolved.as_str(), rec_json.as_str())
                .map_err(backend)?;
        }
        let ev = RegistryEvent {
            event_id: self.mint(),
            at,
            by: rec.by.clone(),
            kind: RegistryEventKind::AgentRetire {
                id: AgentId(resolved),
            },
        };
        self.append_registry_event_in(&wtx, &ev)?;
        wtx.commit().map_err(backend)?;
        Ok(rec)
    }

    fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, StoreError> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let Some(resolved) = self.locate(&rtx, Kind::Agent, id.iri())? else {
            return Ok(None);
        };
        let agents = rtx.open_table(AGENTS).map_err(backend)?;
        let Some(raw) = agents.get(resolved.as_str()).map_err(backend)? else {
            return Ok(None);
        };
        Ok(Some(decode(raw.value(), resolved.as_str())?))
    }

    fn agent_by_name(&self, name: &str) -> Result<Option<AgentRecord>, StoreError> {
        let key = validate_slug(name).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let rtx = self.db.begin_read().map_err(backend)?;
        let by_name = rtx.open_table(AGENT_BY_NAME).map_err(backend)?;
        let Some(id_guard) = by_name.get(key.as_str()).map_err(backend)? else {
            return Ok(None);
        };
        let id_iri = Iri::parse(id_guard.value()).map_err(|_| StoreError::Corrupt {
            id: key,
            reason: "invalid AgentId in agent_by_name".into(),
        })?;
        drop(id_guard);
        let Some(resolved) = self.locate(&rtx, Kind::Agent, &id_iri)? else {
            return Ok(None);
        };
        let agents = rtx.open_table(AGENTS).map_err(backend)?;
        let Some(raw) = agents.get(resolved.as_str()).map_err(backend)? else {
            return Ok(None);
        };
        Ok(Some(decode(raw.value(), resolved.as_str())?))
    }

    fn list_agents(
        &self,
        project: Option<&str>,
        include_retired: bool,
    ) -> Result<Vec<AgentRecord>, StoreError> {
        let project_filter = match project {
            Some(p) => Some(validate_slug(p).map_err(|e| StoreError::InvalidState(e.to_string()))?),
            None => None,
        };
        let rtx = self.db.begin_read().map_err(backend)?;
        let agents = rtx.open_table(AGENTS).map_err(backend)?;
        let mut out = Vec::new();
        for row in agents.iter().map_err(backend)? {
            let (k, v) = row.map_err(backend)?;
            let rec: AgentRecord = decode(v.value(), k.value())?;
            if !include_retired && rec.status != AgentStatus::Active {
                continue;
            }
            if let Some(ref p) = project_filter
                && &rec.project != p
                && !rec.extra_areas.contains(p)
            {
                continue;
            }
            out.push(rec);
        }
        out.sort_by(|a, b| a.name_key.cmp(&b.name_key));
        Ok(out)
    }

    fn registry_events_after(&self, seq: u64) -> Result<Vec<(u64, RegistryEvent)>, StoreError> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let events = rtx.open_table(REGISTRY_EVENTS).map_err(backend)?;
        let mut out = Vec::new();
        for row in events.range((seq + 1)..).map_err(backend)? {
            let (k, v) = row.map_err(backend)?;
            let ev: RegistryEvent =
                decode(v.value(), &format!("registry_event#{}", k.value()))?;
            out.push((k.value(), ev));
        }
        Ok(out)
    }

    fn import_registry_events(&self, events: &[RegistryEvent]) -> Result<(), StoreError> {
        let wtx = self.db.begin_write().map_err(backend)?;
        self.raise_format(&wtx, FORMAT_WITH_STATE)?;
        for ev in events {
            let existing_seq = {
                let by_id = wtx.open_table(REGISTRY_EVENT_BY_ID).map_err(backend)?;
                by_id
                    .get(ev.event_id.as_str())
                    .map_err(backend)?
                    .map(|g| g.value())
            };
            if let Some(seq) = existing_seq {
                let tbl = wtx.open_table(REGISTRY_EVENTS).map_err(backend)?;
                let raw = tbl
                    .get(seq)
                    .map_err(backend)?
                    .ok_or_else(|| StoreError::Corrupt {
                        id: ev.event_id.to_string(),
                        reason: "registry_event_by_id points to missing sequence".into(),
                    })?;
                let existing: RegistryEvent = decode(raw.value(), ev.event_id.as_str())?;
                if &existing != ev {
                    return Err(StoreError::Tampered {
                        id: ev.event_id.clone(),
                    });
                }
                continue;
            }

            match &ev.kind {
                RegistryEventKind::AreaUpsert(area) => {
                    let norm = area
                        .clone()
                        .normalized()
                        .map_err(|e| StoreError::InvalidState(e.to_string()))?;
                    let json = serde_json::to_string(&norm).unwrap();
                    let mut areas = wtx.open_table(AREAS).map_err(backend)?;
                    areas
                        .insert(norm.area_key.as_str(), json.as_str())
                        .map_err(backend)?;
                }
                RegistryEventKind::AgentUpsert(rec) => {
                    let existing_id_str = {
                        let by_name = wtx.open_table(AGENT_BY_NAME).map_err(backend)?;
                        by_name
                            .get(rec.name_key.as_str())
                            .map_err(backend)?
                            .map(|g| g.value().to_owned())
                    };
                    if let Some(old_str) = existing_id_str {
                        let old_iri = Iri::parse(&old_str).map_err(|_| StoreError::Corrupt {
                            id: old_str.clone(),
                            reason: "invalid AgentId in agent_by_name".into(),
                        })?;
                        if &old_iri != rec.id.iri() {
                            self.alias_unpublished_in(&wtx, Kind::Agent, &old_iri, rec.id.iri())?;
                            let mut agents = wtx.open_table(AGENTS).map_err(backend)?;
                            agents.remove(old_str.as_str()).map_err(backend)?;
                        }
                    }
                    if rec.role == AgentRole::Lead && rec.status == AgentStatus::Active {
                        let mut to_retire = Vec::new();
                        {
                            let agents = wtx.open_table(AGENTS).map_err(backend)?;
                            for row in agents.iter().map_err(backend)? {
                                let (k, v) = row.map_err(backend)?;
                                let mut other: AgentRecord = decode(v.value(), k.value())?;
                                if other.project == rec.project
                                    && other.role == AgentRole::Lead
                                    && other.status == AgentStatus::Active
                                    && other.id != rec.id
                                {
                                    other.status = AgentStatus::Retired;
                                    to_retire.push((
                                        other.id.to_string(),
                                        serde_json::to_string(&other).unwrap(),
                                    ));
                                }
                            }
                        }
                        if !to_retire.is_empty() {
                            let mut agents = wtx.open_table(AGENTS).map_err(backend)?;
                            for (id_s, json) in to_retire {
                                agents
                                    .insert(id_s.as_str(), json.as_str())
                                    .map_err(backend)?;
                            }
                        }
                    }
                    self.index_imported_in(&wtx, Kind::Agent, rec.id.iri())?;
                    let rec_json = serde_json::to_string(rec).unwrap();
                    {
                        let mut by_name = wtx.open_table(AGENT_BY_NAME).map_err(backend)?;
                        by_name
                            .insert(rec.name_key.as_str(), rec.id.as_str())
                            .map_err(backend)?;
                        let mut agents = wtx.open_table(AGENTS).map_err(backend)?;
                        agents
                            .insert(rec.id.as_str(), rec_json.as_str())
                            .map_err(backend)?;
                    }
                }
                RegistryEventKind::AgentRetire { id } => {
                    let resolved = self.resolve_in_wtx(&wtx, id.iri())?;
                    let mut agents = wtx.open_table(AGENTS).map_err(backend)?;
                    let raw = agents
                        .get(resolved.as_str())
                        .map_err(backend)?
                        .map(|g| g.value().to_owned());
                    if let Some(json) = raw {
                        let mut rec: AgentRecord = decode(&json, resolved.as_str())?;
                        rec.status = AgentStatus::Retired;
                        rec.updated_at = ev.at;
                        rec.by = ev.by.clone();
                        let updated_json = serde_json::to_string(&rec).unwrap();
                        agents
                            .insert(resolved.as_str(), updated_json.as_str())
                            .map_err(backend)?;
                    }
                }
            }
            self.append_registry_event_in(&wtx, ev)?;
        }
        wtx.commit().map_err(backend)?;
        Ok(())
    }
}

impl StateStore for RedbStore {
    fn upsert_state(&self, args: UpsertStateArgs) -> Result<StateEntry, StoreError> {
        let slug = validate_slug(&args.slug).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let concern = args.payload.concern();
        let scope_wire = args.scope.as_wire();
        let concern_wire = concern.as_wire();

        let wtx = self.db.begin_write().map_err(backend)?;
        self.raise_format(&wtx, FORMAT_WITH_STATE)?;

        let existing: Option<StateEntry> = {
            let by_key = wtx.open_table(STATE_BY_KEY).map_err(backend)?;
            let entries = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
            let id_opt = by_key
                .get((scope_wire.as_str(), concern_wire, slug.as_str()))
                .map_err(backend)?
                .map(|g| g.value().to_owned());
            match id_opt {
                Some(id_str) => {
                    let raw = entries
                        .get(id_str.as_str())
                        .map_err(backend)?
                        .map(|g| g.value().to_owned());
                    match raw {
                        Some(json) => Some(decode(&json, &id_str)?),
                        None => None,
                    }
                }
                None => None,
            }
        };

        if let Some(ref ex) = existing {
            let conflicts = wtx.open_table(STATE_CONFLICTS).map_err(backend)?;
            if let Some(raw_c) = conflicts.get(ex.id.as_str()).map_err(backend)? {
                let c: StateConflict = decode(raw_c.value(), ex.id.as_str())?;
                return Err(StoreError::StateConflict {
                    entry_id: ex.id.clone(),
                    local_head: c.local_head,
                    remote_head: c.remote_head,
                });
            }
        }

        let entry_id = match existing {
            Some(ref ex) => ex.id.clone(),
            None => {
                let raw = self.mint();
                self.index_new(&wtx, Kind::State, &raw)?;
                StateId(raw)
            }
        };

        args.payload
            .validate(&entry_id)
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;

        if let StatePayload::Memory(ref m) = args.payload
            && let Some(ref target_id) = m.superseded_by
        {
            let resolved_target = self.resolve_in_wtx(&wtx, target_id.iri())?;
            let entries = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
            let raw = entries
                .get(resolved_target.as_str())
                .map_err(backend)?
                .map(|g| g.value().to_owned());
            match raw {
                Some(json) => {
                    let target: StateEntry = decode(&json, resolved_target.as_str())?;
                    if target.concern != StateConcern::Memory {
                        return Err(StoreError::InvalidState(format!(
                            "superseded_by target `{target_id}` is not a Memory entry"
                        )));
                    }
                }
                None => {
                    return Err(StoreError::InvalidState(format!(
                        "superseded_by target `{target_id}` does not exist"
                    )));
                }
            }
        }

        let (sensitive, ever_sensitive, parent_revisions) = match existing {
            Some(ref ex) => {
                let s = match args.sensitivity {
                    SensitivityPatch::Inherit => ex.sensitive,
                    SensitivityPatch::SetSensitive => true,
                    SensitivityPatch::ClearSensitive => false,
                };
                let ever = ex.ever_sensitive || s;
                (s, ever, vec![ex.head_revision.clone()])
            }
            None => {
                let s = matches!(args.sensitivity, SensitivityPatch::SetSensitive);
                (s, s, Vec::new())
            }
        };

        let rev_id = RevisionId(self.mint());
        let rev = StateRevision {
            revision_id: rev_id.clone(),
            entry_id: entry_id.clone(),
            parent_revisions,
            scope: args.scope.clone(),
            concern,
            slug: slug.clone(),
            sensitive,
            ever_sensitive,
            payload: args.payload.clone(),
            at: args.at,
            by: args.by.trim().to_string(),
        };
        let entry = StateEntry {
            id: entry_id.clone(),
            scope: args.scope,
            concern,
            slug: slug.clone(),
            sensitive,
            ever_sensitive,
            payload: args.payload,
            head_revision: rev_id,
            updated_at: args.at,
            by: args.by.trim().to_string(),
        };

        self.append_state_revision_in(&wtx, &rev)?;
        let entry_json = serde_json::to_string(&entry).unwrap();
        {
            let mut entries = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
            entries
                .insert(entry_id.as_str(), entry_json.as_str())
                .map_err(backend)?;
            let mut by_key = wtx.open_table(STATE_BY_KEY).map_err(backend)?;
            by_key
                .insert(
                    (scope_wire.as_str(), concern_wire, slug.as_str()),
                    entry_id.as_str(),
                )
                .map_err(backend)?;
        }
        wtx.commit().map_err(backend)?;
        Ok(entry)
    }

    fn retire_memory(
        &self,
        scope: &StateScope,
        slug: &str,
        lifecycle: MemoryLifecycle,
        superseded_by: Option<StateId>,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError> {
        let slug = validate_slug(slug).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        if matches!(lifecycle, MemoryLifecycle::Active) {
            return Err(StoreError::InvalidState(
                "retire_memory requires `retired` or `superseded` lifecycle".into(),
            ));
        }
        let current = self
            .get_state_by_key(scope, StateConcern::Memory, &slug)?
            .ok_or_else(|| StoreError::NoSuchState(format!("{}:memory:{slug}", scope.as_wire())))?;
        let StatePayload::Memory(mut mem) = current.payload else {
            return Err(StoreError::InvalidState(format!(
                "state entry `{}` is not a Memory entry",
                current.id
            )));
        };
        mem.lifecycle = lifecycle;
        mem.superseded_by = superseded_by;
        self.upsert_state(UpsertStateArgs {
            scope: scope.clone(),
            slug,
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Memory(mem),
            at,
            by: by.to_string(),
        })
    }

    fn get_state(&self, id: &StateId) -> Result<Option<StateEntry>, StoreError> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let Some(resolved) = self.locate(&rtx, Kind::State, id.iri())? else {
            return Ok(None);
        };
        let entries = rtx.open_table(STATE_ENTRIES).map_err(backend)?;
        let Some(raw) = entries.get(resolved.as_str()).map_err(backend)? else {
            return Ok(None);
        };
        Ok(Some(decode(raw.value(), resolved.as_str())?))
    }

    fn get_state_by_key(
        &self,
        scope: &StateScope,
        concern: StateConcern,
        slug: &str,
    ) -> Result<Option<StateEntry>, StoreError> {
        let slug = validate_slug(slug).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let scope_wire = scope.as_wire();
        let concern_wire = concern.as_wire();
        let rtx = self.db.begin_read().map_err(backend)?;
        let by_key = rtx.open_table(STATE_BY_KEY).map_err(backend)?;
        let Some(id_guard) = by_key
            .get((scope_wire.as_str(), concern_wire, slug.as_str()))
            .map_err(backend)?
        else {
            return Ok(None);
        };
        let id_iri = Iri::parse(id_guard.value()).map_err(|_| StoreError::Corrupt {
            id: slug,
            reason: "invalid StateId in state_by_key".into(),
        })?;
        drop(id_guard);
        let Some(resolved) = self.locate(&rtx, Kind::State, &id_iri)? else {
            return Ok(None);
        };
        let entries = rtx.open_table(STATE_ENTRIES).map_err(backend)?;
        let Some(raw) = entries.get(resolved.as_str()).map_err(backend)? else {
            return Ok(None);
        };
        Ok(Some(decode(raw.value(), resolved.as_str())?))
    }

    fn list_state(
        &self,
        scope: Option<&StateScope>,
        concern: Option<StateConcern>,
    ) -> Result<Vec<StateEntry>, StoreError> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let entries = rtx.open_table(STATE_ENTRIES).map_err(backend)?;
        let mut out = Vec::new();
        for row in entries.iter().map_err(backend)? {
            let (k, v) = row.map_err(backend)?;
            let entry: StateEntry = decode(v.value(), k.value())?;
            if scope.is_some_and(|s| &entry.scope != s) {
                continue;
            }
            if concern.is_some_and(|c| entry.concern != c) {
                continue;
            }
            out.push(entry);
        }
        out.sort_by(|a, b| {
            (a.scope.as_wire(), a.concern.as_wire(), &a.slug).cmp(&(
                b.scope.as_wire(),
                b.concern.as_wire(),
                &b.slug,
            ))
        });
        Ok(out)
    }

    fn state_revisions(&self, id: &StateId) -> Result<Vec<StateRevision>, StoreError> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let resolved = StateId(self.check(&rtx, Kind::State, id.iri())?);
        let revs = rtx.open_table(STATE_REVISIONS).map_err(backend)?;
        let mut out = Vec::new();
        for row in revs.iter().map_err(backend)? {
            let (seq, raw) = row.map_err(backend)?;
            let rev: StateRevision =
                decode(raw.value(), &format!("state_revision#{}", seq.value()))?;
            if rev.entry_id == resolved {
                out.push(rev);
            }
        }
        Ok(out)
    }

    fn list_conflicts(&self) -> Result<Vec<StateConflict>, StoreError> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let conflicts = rtx.open_table(STATE_CONFLICTS).map_err(backend)?;
        let mut out = Vec::new();
        for row in conflicts.iter().map_err(backend)? {
            let (k, v) = row.map_err(backend)?;
            out.push(decode(v.value(), k.value())?);
        }
        Ok(out)
    }

    fn resolve_conflict(
        &self,
        id: &StateId,
        keep: &RevisionId,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError> {
        let wtx = self.db.begin_write().map_err(backend)?;
        self.raise_format(&wtx, FORMAT_WITH_STATE)?;
        let Some(resolved_iri) = self.locate_for_write(&wtx, Kind::State, id.iri())? else {
            return Err(StoreError::NotFound(id.0.clone()));
        };
        let resolved = StateId(resolved_iri);

        let conflict: StateConflict = {
            let conflicts = wtx.open_table(STATE_CONFLICTS).map_err(backend)?;
            let Some(raw) = conflicts.get(resolved.as_str()).map_err(backend)? else {
                return Err(StoreError::InvalidState(format!(
                    "state entry `{resolved}` has no active conflict"
                )));
            };
            decode(raw.value(), resolved.as_str())?
        };

        if keep != &conflict.local_head && keep != &conflict.remote_head {
            return Err(StoreError::InvalidState(format!(
                "`--keep {keep}` must match either local head `{}` or remote head `{}`",
                conflict.local_head, conflict.remote_head
            )));
        }

        let (chosen_rev, local_rev, remote_rev): (StateRevision, StateRevision, StateRevision) = {
            let by_id = wtx.open_table(STATE_REV_BY_ID).map_err(backend)?;
            let revs = wtx.open_table(STATE_REVISIONS).map_err(backend)?;
            let load_rev = |rev_id: &RevisionId| -> Result<StateRevision, StoreError> {
                let Some(seq_guard) = by_id.get(rev_id.as_str()).map_err(backend)? else {
                    return Err(StoreError::InvalidState(format!(
                        "unknown revision `{rev_id}`"
                    )));
                };
                let seq = seq_guard.value();
                let raw = revs
                    .get(seq)
                    .map_err(backend)?
                    .ok_or_else(|| StoreError::Corrupt {
                        id: rev_id.to_string(),
                        reason: "state_rev_by_id points to missing sequence".into(),
                    })?;
                decode(raw.value(), rev_id.as_str())
            };
            (
                load_rev(keep)?,
                load_rev(&conflict.local_head)?,
                load_rev(&conflict.remote_head)?,
            )
        };

        let existing: StateEntry = {
            let entries = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
            let raw = entries
                .get(resolved.as_str())
                .map_err(backend)?
                .ok_or_else(|| StoreError::NoSuchState(resolved.to_string()))?;
            decode(raw.value(), resolved.as_str())?
        };

        let merge_rev_id = RevisionId(self.mint());
        let sensitive = local_rev.sensitive || remote_rev.sensitive;
        let ever_sensitive = existing.ever_sensitive
            || local_rev.ever_sensitive
            || remote_rev.ever_sensitive
            || sensitive;
        let merge_rev = StateRevision {
            revision_id: merge_rev_id.clone(),
            entry_id: resolved.clone(),
            parent_revisions: sorted_parents(conflict.local_head, conflict.remote_head),
            scope: existing.scope.clone(),
            concern: existing.concern,
            slug: existing.slug.clone(),
            sensitive,
            ever_sensitive,
            payload: chosen_rev.payload.clone(),
            at,
            by: by.trim().to_string(),
        };
        self.append_state_revision_in(&wtx, &merge_rev)?;

        let updated = StateEntry {
            id: resolved.clone(),
            scope: existing.scope,
            concern: existing.concern,
            slug: existing.slug,
            sensitive,
            ever_sensitive,
            payload: chosen_rev.payload,
            head_revision: merge_rev_id,
            updated_at: at,
            by: by.trim().to_string(),
        };
        let updated_json = serde_json::to_string(&updated).unwrap();
        {
            let mut conflicts = wtx.open_table(STATE_CONFLICTS).map_err(backend)?;
            conflicts.remove(resolved.as_str()).map_err(backend)?;
            let mut entries = wtx.open_table(STATE_ENTRIES).map_err(backend)?;
            entries
                .insert(resolved.as_str(), updated_json.as_str())
                .map_err(backend)?;
        }
        wtx.commit().map_err(backend)?;
        Ok(updated)
    }

    fn state_revisions_after(&self, seq: u64) -> Result<Vec<(u64, StateRevision)>, StoreError> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let revs = rtx.open_table(STATE_REVISIONS).map_err(backend)?;
        let mut out = Vec::new();
        for row in revs.range((seq + 1)..).map_err(backend)? {
            let (k, v) = row.map_err(backend)?;
            let rev: StateRevision =
                decode(v.value(), &format!("state_revision#{}", k.value()))?;
            out.push((k.value(), rev));
        }
        Ok(out)
    }

    fn import_state_revisions(
        &self,
        revs: &[StateRevision],
        merge_at: At,
    ) -> Result<(), StoreError> {
        let wtx = self.db.begin_write().map_err(backend)?;
        self.raise_format(&wtx, FORMAT_WITH_STATE)?;
        for rev in revs {
            let existing_seq = {
                let by_id = wtx.open_table(STATE_REV_BY_ID).map_err(backend)?;
                by_id
                    .get(rev.revision_id.as_str())
                    .map_err(backend)?
                    .map(|g| g.value())
            };
            if let Some(seq) = existing_seq {
                let tbl = wtx.open_table(STATE_REVISIONS).map_err(backend)?;
                let raw = tbl
                    .get(seq)
                    .map_err(backend)?
                    .ok_or_else(|| StoreError::Corrupt {
                        id: rev.revision_id.to_string(),
                        reason: "state_rev_by_id points to missing sequence".into(),
                    })?;
                let existing: StateRevision = decode(raw.value(), rev.revision_id.as_str())?;
                let mut normalized_rev = rev.clone();
                normalized_rev.entry_id = StateId(self.resolve_in_wtx(&wtx, rev.entry_id.iri())?);
                if existing != normalized_rev {
                    return Err(StoreError::Tampered {
                        id: rev.revision_id.0.clone(),
                    });
                }
                continue;
            }
            self.apply_imported_state_revision_in(&wtx, rev.clone(), merge_at)?;
        }
        wtx.commit().map_err(backend)?;
        Ok(())
    }

    fn sync_cursor(&self, remote: &str) -> Result<Option<StateSyncCursor>, StoreError> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let sync = rtx.open_table(STATE_SYNC).map_err(backend)?;
        let Some(raw) = sync.get(remote).map_err(backend)? else {
            return Ok(None);
        };
        Ok(Some(decode(raw.value(), remote)?))
    }

    fn set_sync_cursor(&self, cursor: StateSyncCursor) -> Result<(), StoreError> {
        let wtx = self.db.begin_write().map_err(backend)?;
        self.raise_format(&wtx, FORMAT_WITH_STATE)?;
        let json = serde_json::to_string(&cursor).unwrap();
        {
            let mut sync = wtx.open_table(STATE_SYNC).map_err(backend)?;
            sync.insert(cursor.remote.as_str(), json.as_str())
                .map_err(backend)?;
        }
        wtx.commit().map_err(backend)?;
        Ok(())
    }
}
```

- [ ] **Step 3: Add `conformance::agent_registry`, `conformance::state_store`, and Format 6 tests in `crates/store/src/lib.rs`**

In `crates/store/src/lib.rs` inside `mod tests`:
1. Update `rejects_unknown_format_version` to write format `7` (instead of `6`) and assert `Err(StoreError::UnsupportedFormat(7))`:

```rust
    #[test]
    fn rejects_unknown_format_version() {
        let (s, dir) = temp_store();
        let path = dir.path().join("store.redb");
        {
            let wtx = s.db.begin_write().unwrap();
            wtx.open_table(META).unwrap().insert("format", 7).unwrap();
            wtx.commit().unwrap();
        }
        drop(s);
        let err = RedbStore::open(&path).err().expect("format 7 must fail");
        assert!(matches!(err, StoreError::UnsupportedFormat(7)));
    }
```

2. Add the new conformance and Format 6 upgrade tests to `mod tests` in `crates/store/src/lib.rs`:

```rust
    #[test]
    fn passes_agent_registry_conformance() {
        conformance::agent_registry(temp_store);
    }

    #[test]
    fn passes_state_store_conformance() {
        conformance::state_store(temp_store);
    }

    #[test]
    fn state_or_agent_write_raises_format_to_six_and_preserves_existing_rows() {
        let (s, dir) = temp_store();
        let path = dir.path().join("store.redb");
        assert_eq!(read_format(&s), 2);

        let st = fl_core::store::StateStore::upsert_state(
            &s,
            fl_core::state::UpsertStateArgs {
                scope: fl_core::state::StateScope::Workspace,
                slug: "rule".into(),
                sensitivity: fl_core::state::SensitivityPatch::Inherit,
                payload: fl_core::state::StatePayload::Handoff(fl_core::state::HandoffEntry {
                    summary: "v6".into(),
                    shipped: vec![],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec![],
                    notes: None,
                }),
                at: At::from_unix_millis(10),
                by: "jarvis".into(),
            },
        )
        .unwrap();
        assert_eq!(read_format(&s), 6);
        drop(s);

        let reopened = RedbStore::open(&path).unwrap();
        assert_eq!(read_format(&reopened), 6);
        assert_eq!(
            fl_core::store::StateStore::get_state(&reopened, &st.id).unwrap(),
            Some(st)
        );
    }
```

- [ ] **Step 4: Run `cargo test -p fl-store -- --nocapture`**

Run:
```bash
cargo test -p fl-store -- --nocapture
```
Expected: all `fl-store` unit and conformance tests pass.

- [ ] **Step 5: Commit Task 3**

```bash
git add crates/store/src/lib.rs crates/store/src/state.rs
git commit -m "$(cat <<'EOF'
feat(store): add RedbStore Format 6 state and agent registry tables

Extend RedbStore to Format 6 with the 11 additive state and agent
registry tables, implement StateStore and AgentRegistry, and run the
conformance and format-upgrade suites against RedbStore.

Co-authored-by: Ferris <ferris@artificialhumanity.io>
EOF
)"
```

---
### Task 4: Git-backed `fl/state` orphan-branch CAS sync and verification (`fl-exec`)

**Files:**
- Modify: `crates/exec/Cargo.toml`
- Modify: `crates/exec/src/git.rs`
- Create: `crates/exec/src/state_sync.rs`
- Modify: `crates/exec/src/lib.rs`

**Interfaces:**
- Consumes: `fl_core::state::*`, `fl_core::agent::*`, `fl_core::store::{StateStore, AgentRegistry, StoreError}`, `fl_exec::git::{Git, ExecError}`, `sha2::Sha256`.
- Produces:
  - Low-level git plumbing helpers on `fl_exec::git::Git` (`ls_remote_branch`, `fetch_branch`, `is_ancestor`, `rev_list_first_parent`, `ls_tree`, `cat_blob`, `hash_object`, `mktree`, `commit_tree`, `push_cas`, `GitTreeEntry`, `PushCasOutcome`).
  - `fl_exec::state_sync::{StateSyncTarget, StateSyncReport, StateVerifyReport, StateSyncError, MAX_SEGMENT_BYTES, MAX_CAS_TRIES, entry_id_hash, sync_state, verify_state}`.

- [ ] **Step 1: Add `sha2` dependency to `crates/exec/Cargo.toml`**

Update `crates/exec/Cargo.toml`:

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
thiserror.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 2: Add low-level git plumbing methods to `crates/exec/src/git.rs`**

Append `GitTreeEntry`, `PushCasOutcome`, and the 10 plumbing methods to `crates/exec/src/git.rs` (inside `impl Git`), plus a private `git_with_stdin` helper before `mod tests`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTreeEntry {
    pub mode: String,
    pub kind: String,
    pub oid: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushCasOutcome {
    Updated,
    Rejected,
}

fn git_with_stdin(root: &Path, args: &[&str], stdin_bytes: &[u8]) -> Result<String, ExecError> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| ExecError::Spawn {
            program: format!("git {}", args.join(" ")),
            source,
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_bytes)
            .map_err(|source| ExecError::Spawn {
                program: format!("git {}", args.join(" ")),
                source,
            })?;
    }
    let output = child
        .wait_with_output()
        .map_err(|source| ExecError::Spawn {
            program: format!("git {}", args.join(" ")),
            source,
        })?;
    if !output.status.success() {
        return Err(ExecError::Failed {
            program: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
```

And inside `impl Git`:

```rust
    /// Query `git ls-remote --refs <remote> refs/heads/<branch>` and return the remote commit SHA if present.
    pub fn ls_remote_branch(
        root: &Path,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>, ExecError> {
        let refspec = format!("refs/heads/{branch}");
        let raw = out(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["ls-remote", "--refs", remote, &refspec]),
            "git ls-remote",
        )?;
        let Some(first_line) = raw.lines().next() else {
            return Ok(None);
        };
        let Some((sha, _)) = first_line.split_once('\t') else {
            return Ok(None);
        };
        if sha.trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(sha.trim().to_owned()))
        }
    }

    /// Fetch `+refs/heads/<branch>:<local_ref>` from `<remote>` without touching `HEAD` or the working tree.
    pub fn fetch_branch(
        root: &Path,
        remote: &str,
        branch: &str,
        local_ref: &str,
    ) -> Result<(), ExecError> {
        let spec = format!("+refs/heads/{branch}:{local_ref}");
        out(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["fetch", "--no-tags", "--quiet", remote, &spec]),
            "git fetch",
        )?;
        Ok(())
    }

    /// Return `true` iff `ancestor` is an ancestor of `descendant` in `root`'s object database.
    pub fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool, ExecError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .output()
            .map_err(|source| ExecError::Spawn {
                program: "git merge-base --is-ancestor".into(),
                source,
            })?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(ExecError::Failed {
                program: "git merge-base --is-ancestor".into(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
        }
    }

    /// Return commits in oldest-to-newest first-parent order along `tip` (or `after..tip` when `after` is `Some`).
    pub fn rev_list_first_parent(
        root: &Path,
        after: Option<&str>,
        tip: &str,
    ) -> Result<Vec<String>, ExecError> {
        let range = match after {
            Some(base) => format!("{base}..{tip}"),
            None => tip.to_owned(),
        };
        let raw = out(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["rev-list", "--first-parent", "--reverse", &range]),
            "git rev-list --first-parent",
        )?;
        Ok(raw
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    /// List all blobs recursively under `treeish` (`git ls-tree -r <treeish>`).
    pub fn ls_tree(root: &Path, treeish: &str) -> Result<Vec<GitTreeEntry>, ExecError> {
        let raw = out(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["ls-tree", "-r", treeish]),
            "git ls-tree",
        )?;
        let mut entries = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Some((meta, path)) = line.split_once('\t') else {
                continue;
            };
            let mut parts = meta.split_whitespace();
            let (Some(mode), Some(kind), Some(oid)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            entries.push(GitTreeEntry {
                mode: mode.to_owned(),
                kind: kind.to_owned(),
                oid: oid.to_owned(),
                path: path.to_owned(),
            });
        }
        Ok(entries)
    }

    /// Read raw blob bytes by object ID (`git cat-file blob <oid>`).
    pub fn cat_blob(root: &Path, oid: &str) -> Result<Vec<u8>, ExecError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["cat-file", "blob", oid])
            .output()
            .map_err(|source| ExecError::Spawn {
                program: "git cat-file blob".into(),
                source,
            })?;
        if !output.status.success() {
            return Err(ExecError::Failed {
                program: "git cat-file blob".into(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(output.stdout)
    }

    /// Write a blob into the object database via `git hash-object -w --stdin`.
    pub fn hash_object(root: &Path, content: &[u8]) -> Result<String, ExecError> {
        git_with_stdin(root, &["hash-object", "-w", "--stdin"], content)
    }

    /// Build a single-level tree object via `git mktree` from `(mode, kind, oid, name)` entries.
    pub fn mktree(root: &Path, entries: &[GitTreeEntry]) -> Result<String, ExecError> {
        let mut sorted = entries.to_vec();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let mut input = String::new();
        for e in &sorted {
            input.push_str(&format!("{} {} {}\t{}\n", e.mode, e.kind, e.oid, e.path));
        }
        git_with_stdin(root, &["mktree"], input.as_bytes())
    }

    /// Create a commit object via `git commit-tree` without touching `HEAD` or any working tree.
    pub fn commit_tree(
        root: &Path,
        tree_oid: &str,
        parent_oid: Option<&str>,
        message: &str,
        by: &str,
    ) -> Result<String, ExecError> {
        let author = if by.trim().is_empty() { "fl" } else { by.trim() };
        let email = format!("{author}@fl.local");
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(root)
            .env("GIT_AUTHOR_NAME", author)
            .env("GIT_AUTHOR_EMAIL", &email)
            .env("GIT_COMMITTER_NAME", author)
            .env("GIT_COMMITTER_EMAIL", &email)
            .arg("commit-tree")
            .arg(tree_oid);
        if let Some(p) = parent_oid {
            cmd.arg("-p").arg(p);
        }
        cmd.arg("-m").arg(message);
        out(&mut cmd, "git commit-tree")
    }

    /// Push `commit_oid` to `refs/heads/<branch>` on `<remote>` with `--force-with-lease` compare-and-swap.
    pub fn push_cas(
        root: &Path,
        remote: &str,
        branch: &str,
        expected_old_oid: Option<&str>,
        new_oid: &str,
    ) -> Result<PushCasOutcome, ExecError> {
        let lease = match expected_old_oid {
            Some(old) => format!("--force-with-lease=refs/heads/{branch}:{old}"),
            None => format!("--force-with-lease=refs/heads/{branch}:"),
        };
        let refspec = format!("{new_oid}:refs/heads/{branch}");
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["push", "--porcelain", &lease, remote, &refspec])
            .output()
            .map_err(|source| ExecError::Spawn {
                program: "git push --force-with-lease".into(),
                source,
            })?;
        if output.status.success() {
            return Ok(PushCasOutcome::Updated);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined = format!("{stdout}\n{stderr}");
        if combined.contains("stale info")
            || combined.contains("rejected")
            || combined.contains("fetch first")
            || combined.contains("non-fast-forward")
        {
            Ok(PushCasOutcome::Rejected)
        } else {
            Err(ExecError::Failed {
                program: "git push --force-with-lease".into(),
                stderr: stderr.trim().to_owned(),
            })
        }
    }
```

- [ ] **Step 3: Create `crates/exec/src/state_sync.rs` implementing Spec §4.1–§4.3**

Create `crates/exec/src/state_sync.rs`:

```rust
//! Git-backed CAS state synchronization over the `fl/state` orphan branch (spec §4).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use fl_core::agent::{AreaRecord, RegistryEvent};
use fl_core::at::At;
use fl_core::ids::StateId;
use fl_core::state::{StateRevision, StateSyncCursor};
use fl_core::store::{AgentRegistry, StateStore, StoreError};
use sha2::{Digest, Sha256};

use crate::git::{ExecError, Git, GitTreeEntry, PushCasOutcome};

/// Maximum size in bytes of a single `revisions/<entry_hash>/<n:04>.jsonl` segment file.
pub const MAX_SEGMENT_BYTES: usize = 262_144;

/// Maximum compare-and-swap push attempts before returning `StoreError::Contended`.
pub const MAX_CAS_TRIES: usize = 5;

const STATE_BRANCH_FORMAT: &str = "1\n";
const STATE_BRANCH_README: &str = "\
This orphan branch (`fl/state`) is managed automatically by FerroLoop (`fl state sync`).
Do not edit files on this branch by hand; use `fl state` and `fl agent` CLI commands.
";

#[derive(Debug, Clone)]
pub struct StateSyncTarget {
    pub repo_dir: PathBuf,
    pub remote: String,
    pub branch: String,
    pub private: bool,
    pub sync_registry: bool,
    pub by: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSyncReport {
    pub remote: String,
    pub branch: String,
    pub previous_commit: Option<String>,
    pub committed_oid: Option<String>,
    pub imported_registry_events: usize,
    pub imported_revisions: usize,
    pub published_registry_events: usize,
    pub published_revisions: usize,
    pub active_conflicts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateVerifyReport {
    pub remote: String,
    pub branch: String,
    pub tip_commit: Option<String>,
    pub verified_commits: usize,
    pub verified_revisions: usize,
    pub verified_registry_events: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum StateSyncError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Git(#[from] ExecError),
    #[error(
        "remote branch `{branch}` on `{remote}` was rewritten (expected ancestor `{expected_ancestor}`, found `{remote_tip}`)"
    )]
    NonFastForward {
        remote: String,
        branch: String,
        expected_ancestor: String,
        remote_tip: String,
    },
    #[error("unsupported fl/state branch format `{0}` (expected `1`)")]
    UnsupportedBranchFormat(String),
    #[error("tampered history on `fl/state` in commit `{commit}` at path `{path}`: {reason}")]
    HistoryTampered {
        commit: String,
        path: String,
        reason: String,
    },
    #[error("corrupt JSONL in `{path}` line {line}: {reason}")]
    CorruptJsonl {
        path: String,
        line: usize,
        reason: String,
    },
}

/// First 32 lowercase hex chars of SHA-256 of the canonical `StateId` IRI string (spec §4.1).
pub fn entry_id_hash(id: &StateId) -> String {
    let mut hasher = Sha256::new();
    hasher.update(id.as_str().as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for b in &digest[..16] {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[derive(Default, Clone)]
struct BranchSnapshot {
    format: Option<String>,
    areas_events_raw: Vec<u8>,
    registry_events_raw: Vec<u8>,
    registry_events: Vec<RegistryEvent>,
    /// `revisions/<hash>/<seg>.jsonl` -> raw bytes
    segments: BTreeMap<(String, u32), Vec<u8>>,
    revisions: Vec<StateRevision>,
}

fn parse_segment_path(path: &str) -> Option<(String, u32)> {
    let rest = path.strip_prefix("revisions/")?;
    let (hash, file) = rest.split_once('/')?;
    if hash.len() != 32 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let stem = file.strip_suffix(".jsonl")?;
    if stem.len() != 4 || !stem.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: u32 = stem.parse().ok()?;
    Some((hash.to_owned(), n))
}

fn load_snapshot(repo_dir: &std::path::Path, commit: &str) -> Result<BranchSnapshot, StateSyncError> {
    let entries = Git::ls_tree(repo_dir, commit)?;
    let mut snap = BranchSnapshot::default();
    for entry in entries {
        if entry.kind != "blob" {
            continue;
        }
        let raw = Git::cat_blob(repo_dir, &entry.oid)?;
        match entry.path.as_str() {
            "format" => {
                let text = String::from_utf8_lossy(&raw).trim().to_owned();
                if text != "1" {
                    return Err(StateSyncError::UnsupportedBranchFormat(text));
                }
                snap.format = Some(text);
            }
            "README" => {}
            "areas.jsonl" => {
                let text =
                    String::from_utf8(raw.clone()).map_err(|e| StateSyncError::CorruptJsonl {
                        path: entry.path.clone(),
                        line: 1,
                        reason: e.to_string(),
                    })?;
                for (idx, line) in text.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let ev: RegistryEvent =
                        serde_json::from_str(line).map_err(|e| StateSyncError::CorruptJsonl {
                            path: entry.path.clone(),
                            line: idx + 1,
                            reason: e.to_string(),
                        })?;
                    snap.registry_events.push(ev);
                }
                snap.areas_events_raw = raw;
            }
            "agents.jsonl" => {
                let text =
                    String::from_utf8(raw.clone()).map_err(|e| StateSyncError::CorruptJsonl {
                        path: entry.path.clone(),
                        line: 1,
                        reason: e.to_string(),
                    })?;
                for (idx, line) in text.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let ev: RegistryEvent =
                        serde_json::from_str(line).map_err(|e| StateSyncError::CorruptJsonl {
                            path: entry.path.clone(),
                            line: idx + 1,
                            reason: e.to_string(),
                        })?;
                    snap.registry_events.push(ev);
                }
                snap.registry_events_raw = raw;
            }
            other => {
                if let Some((hash, seg)) = parse_segment_path(other) {
                    let text = String::from_utf8(raw.clone()).map_err(|e| {
                        StateSyncError::CorruptJsonl {
                            path: other.to_owned(),
                            line: 1,
                            reason: e.to_string(),
                        }
                    })?;
                    for (idx, line) in text.lines().enumerate() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let rev: StateRevision = serde_json::from_str(line).map_err(|e| {
                            StateSyncError::CorruptJsonl {
                                path: other.to_owned(),
                                line: idx + 1,
                                reason: e.to_string(),
                            }
                        })?;
                        snap.revisions.push(rev);
                    }
                    snap.segments.insert((hash, seg), raw);
                }
            }
        }
    }
    Ok(snap)
}

/// Verify that every append-only file (`areas.jsonl`, `agents.jsonl`, and `revisions/<hash>/<n>.jsonl`)
/// in `next` is a byte-for-byte prefix extension of `prev`.
fn verify_append_only_step(
    commit: &str,
    prev: &BranchSnapshot,
    next: &BranchSnapshot,
) -> Result<(), StateSyncError> {
    if !prev.areas_events_raw.is_empty()
        && !next.areas_events_raw.starts_with(&prev.areas_events_raw)
    {
        return Err(StateSyncError::HistoryTampered {
            commit: commit.to_owned(),
            path: "areas.jsonl".into(),
            reason: "areas.jsonl is not a prefix extension of its parent commit".into(),
        });
    }
    if !prev.registry_events_raw.is_empty()
        && !next
            .registry_events_raw
            .starts_with(&prev.registry_events_raw)
    {
        return Err(StateSyncError::HistoryTampered {
            commit: commit.to_owned(),
            path: "agents.jsonl".into(),
            reason: "agents.jsonl is not a prefix extension of its parent commit".into(),
        });
    }
    for (key, prev_bytes) in &prev.segments {
        let path = format!("revisions/{}/{:04}.jsonl", key.0, key.1);
        let Some(next_bytes) = next.segments.get(key) else {
            return Err(StateSyncError::HistoryTampered {
                commit: commit.to_owned(),
                path,
                reason: "segment file was deleted".into(),
            });
        };
        if !next_bytes.starts_with(prev_bytes) {
            return Err(StateSyncError::HistoryTampered {
                commit: commit.to_owned(),
                path,
                reason: "segment file was modified or truncated".into(),
            });
        }
    }
    Ok(())
}

/// Order revisions so parents always appear before children.
fn topo_sort_revisions(revs: &[StateRevision]) -> Vec<StateRevision> {
    let mut remaining: BTreeMap<String, StateRevision> = revs
        .iter()
        .map(|r| (r.revision_id.to_string(), r.clone()))
        .collect();
    let mut emitted = BTreeSet::new();
    let mut ordered = Vec::with_capacity(revs.len());
    while !remaining.is_empty() {
        let ready: Vec<String> = remaining
            .iter()
            .filter(|(_, r)| {
                r.parent_revisions.iter().all(|p| {
                    let k = p.to_string();
                    !remaining.contains_key(&k) || emitted.contains(&k)
                })
            })
            .map(|(k, _)| k.clone())
            .collect();
        if ready.is_empty() {
            for (_, r) in remaining {
                ordered.push(r);
            }
            break;
        }
        for k in ready {
            if let Some(r) = remaining.remove(&k) {
                emitted.insert(k);
                ordered.push(r);
            }
        }
    }
    ordered
}

/// Build the nested git tree for `BranchSnapshot` using `git hash-object` and `git mktree`.
fn write_snapshot_tree(
    repo_dir: &std::path::Path,
    snap: &BranchSnapshot,
) -> Result<String, StateSyncError> {
    let mut root_entries = Vec::new();

    let format_oid = Git::hash_object(repo_dir, STATE_BRANCH_FORMAT.as_bytes())?;
    root_entries.push(GitTreeEntry {
        mode: "100644".into(),
        kind: "blob".into(),
        oid: format_oid,
        path: "format".into(),
    });

    let readme_oid = Git::hash_object(repo_dir, STATE_BRANCH_README.as_bytes())?;
    root_entries.push(GitTreeEntry {
        mode: "100644".into(),
        kind: "blob".into(),
        oid: readme_oid,
        path: "README".into(),
    });

    if !snap.areas_events_raw.is_empty() {
        let oid = Git::hash_object(repo_dir, &snap.areas_events_raw)?;
        root_entries.push(GitTreeEntry {
            mode: "100644".into(),
            kind: "blob".into(),
            oid,
            path: "areas.jsonl".into(),
        });
    }

    if !snap.registry_events_raw.is_empty() {
        let oid = Git::hash_object(repo_dir, &snap.registry_events_raw)?;
        root_entries.push(GitTreeEntry {
            mode: "100644".into(),
            kind: "blob".into(),
            oid,
            path: "agents.jsonl".into(),
        });
    }

    if !snap.segments.is_empty() {
        let mut by_hash: BTreeMap<String, Vec<GitTreeEntry>> = BTreeMap::new();
        for ((hash, seg), bytes) in &snap.segments {
            let oid = Git::hash_object(repo_dir, bytes)?;
            by_hash.entry(hash.clone()).or_default().push(GitTreeEntry {
                mode: "100644".into(),
                kind: "blob".into(),
                oid,
                path: format!("{seg:04}.jsonl"),
            });
        }
        let mut hash_trees = Vec::new();
        for (hash, entries) in by_hash {
            let subtree_oid = Git::mktree(repo_dir, &entries)?;
            hash_trees.push(GitTreeEntry {
                mode: "040000".into(),
                kind: "tree".into(),
                oid: subtree_oid,
                path: hash,
            });
        }
        let revisions_tree_oid = Git::mktree(repo_dir, &hash_trees)?;
        root_entries.push(GitTreeEntry {
            mode: "040000".into(),
            kind: "tree".into(),
            oid: revisions_tree_oid,
            path: "revisions".into(),
        });
    }

    Ok(Git::mktree(repo_dir, &root_entries)?)
}

fn append_revision_to_segments(
    segments: &mut BTreeMap<(String, u32), Vec<u8>>,
    rev: &StateRevision,
) {
    let hash = entry_id_hash(&rev.entry_id);
    let mut line = serde_json::to_string(rev).expect("StateRevision serializes");
    line.push('\n');
    let line_bytes = line.as_bytes();

    let latest_seg = segments
        .keys()
        .filter(|(h, _)| h == &hash)
        .map(|(_, n)| *n)
        .max()
        .unwrap_or(0);

    let key = (hash.clone(), latest_seg);
    let current_len = segments.get(&key).map(|b| b.len()).unwrap_or(0);
    let target_seg = if current_len > 0 && current_len + line_bytes.len() > MAX_SEGMENT_BYTES {
        latest_seg + 1
    } else {
        latest_seg
    };
    segments
        .entry((hash, target_seg))
        .or_default()
        .extend_from_slice(line_bytes);
}

/// Synchronize `store` with the `<remote>/<branch>` orphan branch in `target.repo_dir`
/// using the 5-step compare-and-swap protocol (spec §4.2).
pub fn sync_state<S>(
    store: &S,
    target: &StateSyncTarget,
    now: At,
) -> Result<StateSyncReport, StateSyncError>
where
    S: StateStore + AgentRegistry,
{
    let local_tracking_ref = format!("refs/fl-sync/{}/{}", target.remote, target.branch);

    for _attempt in 0..MAX_CAS_TRIES {
        let cursor = store
            .sync_cursor(&target.remote)?
            .unwrap_or_else(|| StateSyncCursor {
                remote: target.remote.clone(),
                branch: target.branch.clone(),
                last_pushed_commit: None,
                last_revision_seq: 0,
                last_registry_seq: 0,
            });

        // Step 1: Fetch remote tip into isolated tracking ref.
        let remote_tip =
            Git::ls_remote_branch(&target.repo_dir, &target.remote, &target.branch)?;
        if remote_tip.is_some() {
            Git::fetch_branch(
                &target.repo_dir,
                &target.remote,
                &target.branch,
                &local_tracking_ref,
            )?;
        }

        // Step 2: Verify remote ancestry and append-only invariants.
        if let Some(ref last_pushed) = cursor.last_pushed_commit {
            match remote_tip {
                Some(ref tip) => {
                    if !Git::is_ancestor(&target.repo_dir, last_pushed, tip)? {
                        return Err(StateSyncError::NonFastForward {
                            remote: target.remote.clone(),
                            branch: target.branch.clone(),
                            expected_ancestor: last_pushed.clone(),
                            remote_tip: tip.clone(),
                        });
                    }
                }
                None => {
                    return Err(StateSyncError::NonFastForward {
                        remote: target.remote.clone(),
                        branch: target.branch.clone(),
                        expected_ancestor: last_pushed.clone(),
                        remote_tip: "<deleted>".into(),
                    });
                }
            }
        }

        let mut base_snapshot = BranchSnapshot::default();
        if let Some(ref tip) = remote_tip {
            let commits = Git::rev_list_first_parent(
                &target.repo_dir,
                cursor.last_pushed_commit.as_deref(),
                tip,
            )?;
            let mut prev_snap = match cursor.last_pushed_commit {
                Some(ref base_sha) => load_snapshot(&target.repo_dir, base_sha)?,
                None => BranchSnapshot::default(),
            };
            for sha in &commits {
                let next_snap = load_snapshot(&target.repo_dir, sha)?;
                verify_append_only_step(sha, &prev_snap, &next_snap)?;
                prev_snap = next_snap;
            }
            base_snapshot = prev_snap;
        }

        // Step 3: Import unseen remote registry events and state revisions.
        let imported_registry_events = if target.sync_registry {
            store.import_registry_events(&base_snapshot.registry_events)?;
            base_snapshot.registry_events.len()
        } else {
            0
        };
        let ordered_remote_revs = topo_sort_revisions(&base_snapshot.revisions);
        if !target.private {
            for rev in &ordered_remote_revs {
                if rev.sensitive || rev.ever_sensitive {
                    return Err(StateSyncError::Store(StoreError::SensitiveDisclosure(
                        rev.entry_id.clone(),
                    )));
                }
            }
        }
        store.import_state_revisions(&ordered_remote_revs, now)?;
        let imported_revisions = ordered_remote_revs.len();

        // Step 4: Collect unpublished local events/revisions and enforce sensitivity guard.
        if !target.private {
            for entry in store.list_state(None, None)? {
                if entry.sensitive || entry.ever_sensitive {
                    return Err(StateSyncError::Store(StoreError::SensitiveDisclosure(
                        entry.id,
                    )));
                }
            }
        }

        let all_local_revs = store.state_revisions_after(0)?;
        let max_rev_seq = all_local_revs.last().map(|(seq, _)| *seq).unwrap_or(0);
        let remote_rev_ids: BTreeSet<_> = base_snapshot
            .revisions
            .iter()
            .map(|r| r.revision_id.clone())
            .collect();
        let unpublished_revs: Vec<StateRevision> = all_local_revs
            .into_iter()
            .map(|(_, r)| r)
            .filter(|r| !remote_rev_ids.contains(&r.revision_id))
            .collect();

        if !target.private {
            for rev in &unpublished_revs {
                if rev.sensitive || rev.ever_sensitive {
                    return Err(StateSyncError::Store(StoreError::SensitiveDisclosure(
                        rev.entry_id.clone(),
                    )));
                }
            }
        }

        let (unpublished_reg_events, max_reg_seq) = if target.sync_registry {
            let all_reg = store.registry_events_after(0)?;
            let max_seq = all_reg.last().map(|(seq, _)| *seq).unwrap_or(0);
            let remote_ev_ids: BTreeSet<_> = base_snapshot
                .registry_events
                .iter()
                .map(|e| e.event_id.clone())
                .collect();
            let unpublished: Vec<RegistryEvent> = all_reg
                .into_iter()
                .map(|(_, e)| e)
                .filter(|e| !remote_ev_ids.contains(&e.event_id))
                .collect();
            (unpublished, max_seq)
        } else {
            (Vec::new(), cursor.last_registry_seq)
        };

        if unpublished_revs.is_empty() && unpublished_reg_events.is_empty() {
            let new_cursor = StateSyncCursor {
                remote: target.remote.clone(),
                branch: target.branch.clone(),
                last_pushed_commit: remote_tip.clone(),
                last_revision_seq: max_rev_seq,
                last_registry_seq: max_reg_seq,
            };
            store.set_sync_cursor(new_cursor)?;
            let active_conflicts = store.list_conflicts()?.len();
            return Ok(StateSyncReport {
                remote: target.remote.clone(),
                branch: target.branch.clone(),
                previous_commit: remote_tip.clone(),
                committed_oid: remote_tip,
                imported_registry_events,
                imported_revisions,
                published_registry_events: 0,
                published_revisions: 0,
                active_conflicts,
            });
        }

        let mut next_snapshot = base_snapshot.clone();
        if target.sync_registry {
            for ev in &unpublished_reg_events {
                let mut line = serde_json::to_string(ev).unwrap();
                line.push('\n');
                match ev.kind {
                    fl_core::agent::RegistryEventKind::AreaUpsert(_) => {
                        next_snapshot
                            .areas_events_raw
                            .extend_from_slice(line.as_bytes());
                    }
                    fl_core::agent::RegistryEventKind::AgentUpsert(_)
                    | fl_core::agent::RegistryEventKind::AgentRetire { .. } => {
                        next_snapshot
                            .registry_events_raw
                            .extend_from_slice(line.as_bytes());
                    }
                }
            }
        }
        for rev in &unpublished_revs {
            append_revision_to_segments(&mut next_snapshot.segments, rev);
        }

        let tree_oid = write_snapshot_tree(&target.repo_dir, &next_snapshot)?;
        let author = if target.by.trim().is_empty() {
            "fl"
        } else {
            target.by.trim()
        };
        let commit_msg = format!("fl state sync ({author})");
        let commit_oid = Git::commit_tree(
            &target.repo_dir,
            &tree_oid,
            remote_tip.as_deref(),
            &commit_msg,
            author,
        )?;

        match Git::push_cas(
            &target.repo_dir,
            &target.remote,
            &target.branch,
            remote_tip.as_deref(),
            &commit_oid,
        )? {
            PushCasOutcome::Updated => {
                // Step 5: Persist updated sync cursor.
                let new_cursor = StateSyncCursor {
                    remote: target.remote.clone(),
                    branch: target.branch.clone(),
                    last_pushed_commit: Some(commit_oid.clone()),
                    last_revision_seq: max_rev_seq,
                    last_registry_seq: max_reg_seq,
                };
                store.set_sync_cursor(new_cursor)?;
                let active_conflicts = store.list_conflicts()?.len();
                return Ok(StateSyncReport {
                    remote: target.remote.clone(),
                    branch: target.branch.clone(),
                    previous_commit: remote_tip,
                    committed_oid: Some(commit_oid),
                    imported_registry_events,
                    imported_revisions,
                    published_registry_events: unpublished_reg_events.len(),
                    published_revisions: unpublished_revs.len(),
                    active_conflicts,
                });
            }
            PushCasOutcome::Rejected => continue,
        }
    }

    Err(StateSyncError::Store(StoreError::Contended {
        store: format!("{} ({})", target.repo_dir.display(), target.remote),
        tries: MAX_CAS_TRIES as u32,
    }))
}

/// Verify integrity of the `<remote>/<branch>` orphan branch and local `store` (spec §4.2, §5.2).
pub fn verify_state<S>(
    store: &S,
    target: &StateSyncTarget,
) -> Result<StateVerifyReport, StateSyncError>
where
    S: StateStore + AgentRegistry,
{
    let local_tracking_ref = format!("refs/fl-sync/{}/{}", target.remote, target.branch);
    let remote_tip = Git::ls_remote_branch(&target.repo_dir, &target.remote, &target.branch)?;
    let Some(tip) = remote_tip else {
        return Ok(StateVerifyReport {
            remote: target.remote.clone(),
            branch: target.branch.clone(),
            tip_commit: None,
            verified_commits: 0,
            verified_revisions: 0,
            verified_registry_events: 0,
        });
    };

    Git::fetch_branch(
        &target.repo_dir,
        &target.remote,
        &target.branch,
        &local_tracking_ref,
    )?;

    if let Some(cursor) = store.sync_cursor(&target.remote)?
        && let Some(ref last_pushed) = cursor.last_pushed_commit
        && !Git::is_ancestor(&target.repo_dir, last_pushed, &tip)?
    {
        return Err(StateSyncError::NonFastForward {
            remote: target.remote.clone(),
            branch: target.branch.clone(),
            expected_ancestor: last_pushed.clone(),
            remote_tip: tip,
        });
    }

    let commits = Git::rev_list_first_parent(&target.repo_dir, None, &tip)?;
    let mut prev = BranchSnapshot::default();
    for sha in &commits {
        let next = load_snapshot(&target.repo_dir, sha)?;
        verify_append_only_step(sha, &prev, &next)?;
        if !target.private {
            for rev in &next.revisions {
                if rev.sensitive || rev.ever_sensitive {
                    return Err(StateSyncError::Store(StoreError::SensitiveDisclosure(
                        rev.entry_id.clone(),
                    )));
                }
            }
        }
        prev = next;
    }

    Ok(StateVerifyReport {
        remote: target.remote.clone(),
        branch: target.branch.clone(),
        tip_commit: Some(tip),
        verified_commits: commits.len(),
        verified_revisions: prev.revisions.len(),
        verified_registry_events: prev.registry_events.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::agent::{AgentRole, RegisterAgentArgs};
    use fl_core::mem::MemStore;
    use fl_core::state::{
        HandoffEntry, MergeMode, RuntimeEntry, SensitivityPatch, StatePayload, StateScope,
        UpsertStateArgs,
    };
    use std::process::Command;
    use tempfile::TempDir;

    fn setup_two_clones() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("remote.git");
        let clone_a = tmp.path().join("clone_a");
        let clone_b = tmp.path().join("clone_b");

        assert!(
            Command::new("git")
                .args(["init", "--bare", bare.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        for clone in [&clone_a, &clone_b] {
            assert!(
                Command::new("git")
                    .args(["init", clone.to_str().unwrap()])
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(clone)
                    .args(["remote", "add", "origin", bare.to_str().unwrap()])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        (tmp, clone_a, clone_b)
    }

    #[test]
    fn two_stores_sync_registry_and_state_without_touching_worktree() {
        let (_tmp, clone_a, clone_b) = setup_two_clones();
        let store_a = MemStore::new();
        let store_b = MemStore::new();

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
        let entry = store_a
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "current".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "Synced handoff".into(),
                    shipped: vec!["task-1".into()],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec!["task-2".into()],
                    notes: None,
                }),
                at: At::from_unix_millis(11),
                by: "ferris".into(),
            })
            .unwrap();

        let target_a = StateSyncTarget {
            repo_dir: clone_a.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: true,
            by: "ferris".into(),
        };
        let rep_a = sync_state(&store_a, &target_a, At::from_unix_millis(12)).unwrap();
        assert_eq!(rep_a.published_revisions, 1);
        assert_eq!(rep_a.published_registry_events, 2);

        let target_b = StateSyncTarget {
            repo_dir: clone_b,
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: true,
            by: "ferris".into(),
        };
        let rep_b = sync_state(&store_b, &target_b, At::from_unix_millis(13)).unwrap();
        assert_eq!(rep_b.imported_revisions, 1);
        assert_eq!(rep_b.imported_registry_events, 2);
        assert_eq!(store_b.get_state(&entry.id).unwrap(), Some(entry));
        assert!(store_b.agent_by_name("ferris").unwrap().is_some());

        let ver = verify_state(&store_b, &target_b).unwrap();
        assert_eq!(ver.verified_commits, 1);
        assert_eq!(ver.verified_revisions, 1);
    }

    #[test]
    fn segment_rolls_over_when_exceeding_max_segment_bytes() {
        let (_tmp, clone_a, _clone_b) = setup_two_clones();
        let store = MemStore::new();
        let big_notes = "x".repeat(60_000);
        for i in 0..6 {
            store
                .upsert_state(UpsertStateArgs {
                    scope: StateScope::Workspace,
                    slug: "big_handoff".into(),
                    sensitivity: SensitivityPatch::Inherit,
                    payload: StatePayload::Handoff(HandoffEntry {
                        summary: format!("rev {i}"),
                        shipped: vec![],
                        in_flight: vec![],
                        blockers: vec![],
                        next_steps: vec![],
                        notes: Some(big_notes.clone()),
                    }),
                    at: At::from_unix_millis(10 + i),
                    by: "ferris".into(),
                })
                .unwrap();
        }
        let target = StateSyncTarget {
            repo_dir: clone_a.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: false,
            by: "ferris".into(),
        };
        let rep = sync_state(&store, &target, At::from_unix_millis(100)).unwrap();
        let tree = Git::ls_tree(&clone_a, rep.committed_oid.as_ref().unwrap()).unwrap();
        let seg_paths: Vec<_> = tree
            .iter()
            .filter(|e| e.path.starts_with("revisions/"))
            .map(|e| e.path.as_str())
            .collect();
        assert!(seg_paths.iter().any(|p| p.ends_with("/0000.jsonl")));
        assert!(seg_paths.iter().any(|p| p.ends_with("/0001.jsonl")));
    }

    #[test]
    fn refuses_to_sync_sensitive_or_ever_sensitive_entries() {
        let (_tmp, clone_a, _clone_b) = setup_two_clones();
        let store = MemStore::new();
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "secret_hint".into(),
                sensitivity: SensitivityPatch::SetSensitive,
                payload: StatePayload::Runtime(RuntimeEntry {
                    value: serde_json::json!({"secret": true}),
                    merge_mode: MergeMode::Conflict,
                }),
                at: At::from_unix_millis(10),
                by: "jarvis".into(),
            })
            .unwrap();
        // Even after clearing sensitivity, ever_sensitive remains true and blocks git sync.
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "secret_hint".into(),
                sensitivity: SensitivityPatch::ClearSensitive,
                payload: StatePayload::Runtime(RuntimeEntry {
                    value: serde_json::json!({"secret": false}),
                    merge_mode: MergeMode::Conflict,
                }),
                at: At::from_unix_millis(11),
                by: "jarvis".into(),
            })
            .unwrap();

        let target = StateSyncTarget {
            repo_dir: clone_a.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: false,
            by: "jarvis".into(),
        };
        let err = sync_state(&store, &target, At::from_unix_millis(12)).unwrap_err();
        assert!(matches!(
            err,
            StateSyncError::Store(StoreError::SensitiveDisclosure(_))
        ));

        // When `private: true` is configured on the target, sync succeeds.
        let private_target = StateSyncTarget {
            repo_dir: clone_a,
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: true,
            sync_registry: false,
            by: "jarvis".into(),
        };
        let rep = sync_state(&store, &private_target, At::from_unix_millis(13)).unwrap();
        assert_eq!(rep.published_revisions, 2);
    }
}
```

- [ ] **Step 4: Export `state_sync` from `crates/exec/src/lib.rs`**

Update `crates/exec/src/lib.rs` so all existing exports remain intact and `pub mod state_sync;` is added:

```rust
//! Shell, git, and gate execution for FerroLoop.
//!
//! `fl-exec` is the only crate below `fl-cli` that spawns processes or touches
//! a working tree. It depends on `fl-core` for domain types (`Verdict`,
//! `GateSpec`, `GateRun`) and nothing else in the workspace.

pub mod command;
pub mod git;
pub mod stamp;
pub mod state_sync;

pub use command::{CommandRunner, GateOutcome, RunReport};
pub use git::{ExecError, Git};
pub use stamp::{StampError, read_gate_trailers, stamp_head};
```

- [ ] **Step 5: Run `cargo test -p fl-exec -- --nocapture`**

Run:
```bash
cargo test -p fl-exec -- --nocapture
```
Expected: all existing `fl-exec` tests (`git`, `stamp`, `command`) and the new `state_sync` tests pass.

- [ ] **Step 6: Commit Task 4**

```bash
git add crates/exec/Cargo.toml crates/exec/src/git.rs crates/exec/src/state_sync.rs crates/exec/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(exec): implement git-backed fl/state orphan-branch CAS sync and verification

Add git object/tree/CAS plumbing to fl_exec::git::Git and implement the
5-step compare-and-swap sync protocol, 256 KiB segment rollover,
append-only history check, and sensitivity guard in fl_exec::state_sync.

Co-authored-by: Ferris <ferris@artificialhumanity.io>
EOF
)"
```

---
### Task 5: Workspace & per-project state configuration and store routing (`fl-cli`)

**Files:**
- Modify: `crates/cli/Cargo.toml`
- Modify: `crates/cli/src/config.rs`
- Modify: `crates/cli/src/main.rs`

**Interfaces:**
- Consumes: `fl_core::state::{StateScope, validate_slug}`, `fl_exec::state_sync::StateSyncTarget`.
- Produces:
  - `StateConfig` and `ProjectStateConfig` in `crates/cli/src/config.rs`.
  - `Entry.name: Option<String>` and `Entry.state: Option<ProjectStateConfig>`.
  - `Config.state: Option<StateConfig>`, `Config::workspace_state_store`, `Config::find_project_entry`, `Config::state_store_for_scope`, and `Config::sync_target_for_scope`.
  - Cross-store IRI lookup in `crates/cli/src/main.rs` (`choose_store`) extended to include `[state].store`.

- [ ] **Step 1: Add `tempfile.workspace = true` to `[dependencies]` in `crates/cli/Cargo.toml`**

Update `crates/cli/Cargo.toml` so `tempfile` is available in `[dependencies]` for atomic file writes in `fl agent roster --render` and `fl state render --out`:

```toml
[package]
name = "fl-cli"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "fl"
path = "src/main.rs"

[dependencies]
clap.workspace = true
fl-core = { path = "../core" }
fl-exec = { path = "../exec" }
fl-github = { path = "../github" }
fl-mcp = { path = "../mcp" }
fl-store = { path = "../store" }
serde.workspace = true
serde_json.workspace = true
tempfile.workspace = true
toml.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 2: Extend `crates/cli/src/config.rs` with `[state]` and per-project `state` / `name` configuration**

Update `crates/cli/src/config.rs` while preserving all existing structs, fields, functions, and unit tests:

```rust
//! Workspace configuration (`$XDG_CONFIG_HOME/fl/config.toml`).
//!
//! Maps project working trees to their `.redb` stores without leaving a
//! bread-crumb in the project directory unless `fl init --local` was used.

use std::path::{Path, PathBuf};

use fl_core::state::{StateScope, validate_slug};
use fl_exec::state_sync::StateSyncTarget;
use serde::{Deserialize, Serialize};

/// Default relative store path inside a `--local` project root.
pub const DEFAULT_STORE_REL: &str = ".fl/fl.redb";

/// Default branch name for git-backed state synchronization.
pub const DEFAULT_STATE_BRANCH: &str = "fl/state";

/// Workspace-level `[state]` configuration (spec §4.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    /// Optional workspace state store path; defaults to `$XDG_DATA_HOME/fl/fl.redb`.
    pub store: Option<PathBuf>,
    /// Default git repository for workspace/group/agent/workflow state sync.
    pub repo: Option<PathBuf>,
    /// Git remote name (defaults to `"origin"`).
    pub remote: Option<String>,
    /// Git branch name (defaults to `"fl/state"`).
    pub branch: Option<String>,
    /// Whether the target repository is private and permitted to receive sensitive entries.
    #[serde(default)]
    pub private: bool,
}

/// Per-project `[[project]].state` override (spec §4.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectStateConfig {
    /// Optional override repository for this project's `fl/state` branch.
    pub repo: Option<PathBuf>,
    /// Optional override remote name (defaults to `[state].remote` or `"origin"`).
    pub remote: Option<String>,
    /// Optional override branch name (defaults to `"fl/state"`).
    pub branch: Option<String>,
    /// Whether this project's state target is private.
    pub private: Option<bool>,
}

/// One `[[project]]` entry in `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Optional normalized project slug (`^[a-z][a-z0-9_-]{0,63}$`).
    pub name: Option<String>,
    /// Canonical path of the project root (must be absolute in the file).
    pub root: PathBuf,
    /// Optional explicit `.redb` path (must be absolute if set). Defaults to
    /// `<root>/.fl/fl.redb`.
    pub store: Option<PathBuf>,
    /// Optional external tracker binding, e.g. `"github:owner/repo"`.
    pub tracker: Option<String>,
    /// Optional separate `.redb` path for the findings ledger (must be
    /// absolute if set). When omitted, findings live in `store`.
    pub ledger: Option<PathBuf>,
    /// Optional per-project state sync override.
    pub state: Option<ProjectStateConfig>,
}

impl Entry {
    pub fn store_path(&self) -> PathBuf {
        self.store
            .clone()
            .unwrap_or_else(|| self.root.join(DEFAULT_STORE_REL))
    }

    /// Matches `<slug>` by explicit `name` or by lowercase basename of `root` (spec §4.3).
    pub fn matches_slug(&self, slug: &str) -> bool {
        if let Some(ref name) = self.name {
            return name.eq_ignore_ascii_case(slug);
        }
        self.root
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|base| base.eq_ignore_ascii_case(slug))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    state: Option<StateConfig>,
    #[serde(default, rename = "project")]
    projects: Vec<Entry>,
}

/// Loaded configuration together with the path it came from.
#[derive(Debug, Default)]
pub struct Config {
    pub path: PathBuf,
    pub state: Option<StateConfig>,
    pub projects: Vec<Entry>,
}

impl Config {
    /// Workspace state store path (`[state].store` or `default_store`).
    pub fn workspace_state_store(&self, default_store: &Path) -> PathBuf {
        self.state
            .as_ref()
            .and_then(|s| s.store.clone())
            .unwrap_or_else(|| default_store.to_path_buf())
    }

    /// Find a `[[project]]` entry matching `slug` by `name` or lowercase basename of `root`.
    pub fn find_project_entry(&self, slug: &str) -> Option<&Entry> {
        self.projects.iter().find(|e| e.matches_slug(slug))
    }

    /// Resolve which `.redb` store holds state for `scope` (spec §4.3):
    /// - `project:<slug>` -> matching `[[project]].store` if that entry defines its own `store`,
    ///   otherwise the workspace state store.
    /// - All other scopes (`workspace`, `group:*`, `agent:*`, `workflow:*`) -> workspace state store.
    pub fn state_store_for_scope(&self, scope: &StateScope, default_store: &Path) -> PathBuf {
        if let StateScope::Project(slug) = scope
            && let Some(entry) = self.find_project_entry(slug)
            && entry.store.is_some()
        {
            return entry.store_path();
        }
        self.workspace_state_store(default_store)
    }

    /// Resolve the `StateSyncTarget` for `fl state sync` / `fl state verify` (spec §4.3).
    pub fn sync_target_for_scope(
        &self,
        scope: Option<&StateScope>,
        cwd: &Path,
        by: &str,
    ) -> Result<StateSyncTarget, String> {
        let ws = self.state.as_ref();
        if let Some(StateScope::Project(slug)) = scope
            && let Some(entry) = self.find_project_entry(slug)
        {
            let proj_state = entry.state.as_ref();
            let repo_dir = proj_state
                .and_then(|s| s.repo.clone())
                .or_else(|| ws.and_then(|s| s.repo.clone()))
                .unwrap_or_else(|| entry.root.clone());
            let remote = proj_state
                .and_then(|s| s.remote.clone())
                .or_else(|| ws.and_then(|s| s.remote.clone()))
                .unwrap_or_else(|| "origin".to_string());
            let branch = proj_state
                .and_then(|s| s.branch.clone())
                .or_else(|| ws.and_then(|s| s.branch.clone()))
                .unwrap_or_else(|| DEFAULT_STATE_BRANCH.to_string());
            let private = proj_state
                .and_then(|s| s.private)
                .or_else(|| ws.map(|s| s.private))
                .unwrap_or(false);
            let sync_registry = entry.store.is_none();
            return Ok(StateSyncTarget {
                repo_dir,
                remote,
                branch,
                private,
                sync_registry,
                by: by.to_string(),
            });
        }

        let repo_dir = ws
            .and_then(|s| s.repo.clone())
            .unwrap_or_else(|| cwd.to_path_buf());
        let remote = ws
            .and_then(|s| s.remote.clone())
            .unwrap_or_else(|| "origin".to_string());
        let branch = ws
            .and_then(|s| s.branch.clone())
            .unwrap_or_else(|| DEFAULT_STATE_BRANCH.to_string());
        let private = ws.map(|s| s.private).unwrap_or(false);
        Ok(StateSyncTarget {
            repo_dir,
            remote,
            branch,
            private,
            sync_registry: true,
            by: by.to_string(),
        })
    }
}
```

And update `parse` in `crates/cli/src/config.rs` to validate `[state]` and `[[project]]` state/name fields:

```rust
fn parse(raw: &str, source: &Path) -> Result<File, String> {
    let mut parsed: File =
        toml::from_str(raw).map_err(|e| format!("{}: {e}", source.display()))?;
    if let Some(ref st) = parsed.state {
        if let Some(ref s) = st.store
            && !s.is_absolute()
        {
            return Err(format!(
                "{}: `state.store` must be an absolute path: {}",
                source.display(),
                s.display()
            ));
        }
        if let Some(ref r) = st.repo
            && !r.is_absolute()
        {
            return Err(format!(
                "{}: `state.repo` must be an absolute path: {}",
                source.display(),
                r.display()
            ));
        }
    }
    let mut seen_names = std::collections::BTreeSet::new();
    for entry in &mut parsed.projects {
        if let Some(ref raw_name) = entry.name {
            let norm = validate_slug(raw_name)
                .map_err(|e| format!("{}: invalid project `name`: {e}", source.display()))?;
            if !seen_names.insert(norm.clone()) {
                return Err(format!(
                    "{}: duplicate project name `{norm}`",
                    source.display()
                ));
            }
            entry.name = Some(norm);
        }
        if !entry.root.is_absolute() {
            return Err(format!(
                "{}: project `root` must be an absolute path: {}",
                source.display(),
                entry.root.display()
            ));
        }
        if let Some(s) = &entry.store
            && !s.is_absolute()
        {
            return Err(format!(
                "{}: project `store` must be an absolute path: {}",
                source.display(),
                s.display()
            ));
        }
        if let Some(l) = &entry.ledger
            && !l.is_absolute()
        {
            return Err(format!(
                "{}: project `ledger` must be an absolute path: {}",
                source.display(),
                l.display()
            ));
        }
        if let Some(ref pst) = entry.state
            && let Some(ref r) = pst.repo
            && !r.is_absolute()
        {
            return Err(format!(
                "{}: project `state.repo` must be an absolute path: {}",
                source.display(),
                r.display()
            ));
        }
    }
    Ok(parsed)
}
```

And in `load()` in `crates/cli/src/config.rs`, pass `state: parsed.state`:

```rust
    let parsed = parse(&raw, &path)?;
    Ok(Config {
        path,
        state: parsed.state,
        projects: parsed.projects,
    })
```

And in `write_init()` in `crates/cli/src/config.rs`, initialize the new `Option` fields on `Entry` as `None`:

```rust
    file.projects.push(Entry {
        name: None,
        root,
        store,
        tracker: None,
        ledger: None,
        state: None,
    });
```

- [ ] **Step 3: Add unit tests for `[state]` and per-project state routing in `crates/cli/src/config.rs`**

Update existing `Entry` literals in `mod tests` in `crates/cli/src/config.rs` (if any construct `Entry` directly) and append these tests to `mod tests`:

```rust
    #[test]
    fn parses_workspace_and_project_state_config_and_routes_scopes() {
        let dir = TempDir::new().unwrap();
        let cfg_file = dir.path().join("config.toml");
        let ws_store = dir.path().join("ws-state.redb");
        let ws_repo = dir.path().join("notes");
        let proj_root = dir.path().join("FerroLoop");
        let proj_store = dir.path().join("ferroloop.redb");
        let proj_state_repo = dir.path().join("private-state");
        std::fs::write(
            &cfg_file,
            format!(
                "[state]\nstore = \"{ws}\"\nrepo = \"{wr}\"\nremote = \"origin\"\nprivate = true\n\n\
                 [[project]]\nname = \"ferroloop\"\nroot = \"{pr}\"\nstore = \"{ps}\"\n\
                 state = {{ repo = \"{psr}\", remote = \"upstream\", private = false }}\n",
                ws = ws_store.display(),
                wr = ws_repo.display(),
                pr = proj_root.display(),
                ps = proj_store.display(),
                psr = proj_state_repo.display(),
            ),
        )
        .unwrap();

        let parsed = parse(&std::fs::read_to_string(&cfg_file).unwrap(), &cfg_file).unwrap();
        let cfg = Config {
            path: cfg_file,
            state: parsed.state,
            projects: parsed.projects,
        };
        let default_store = dir.path().join("default.redb");

        assert_eq!(cfg.workspace_state_store(&default_store), ws_store);
        assert_eq!(
            cfg.state_store_for_scope(&StateScope::Workspace, &default_store),
            ws_store
        );
        assert_eq!(
            cfg.state_store_for_scope(
                &StateScope::Project("ferroloop".into()),
                &default_store
            ),
            proj_store
        );
        assert_eq!(
            cfg.state_store_for_scope(&StateScope::Project("other".into()), &default_store),
            ws_store
        );

        let proj_target = cfg
            .sync_target_for_scope(
                Some(&StateScope::Project("ferroloop".into())),
                dir.path(),
                "ferris",
            )
            .unwrap();
        assert_eq!(proj_target.repo_dir, proj_state_repo);
        assert_eq!(proj_target.remote, "upstream");
        assert!(!proj_target.private);
    }

    #[test]
    fn rejects_relative_state_store_or_repo_path() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("config.toml");
        let err = parse("[state]\nstore = \"relative/state.redb\"\n", &cfg).unwrap_err();
        assert!(err.contains("must be an absolute path"), "got: {err}");
    }
```

- [ ] **Step 4: Update `choose_store` in `crates/cli/src/main.rs` to include `[state].store` in cross-store IRI searches**

In `crates/cli/src/main.rs`, update `choose_store` so that when `input` is a full IRI (`refs::Ref::Iri(iri)`) and not confined by `--db` / `$FL_DB`, it also checks `[state].store` (if configured) before or alongside `cfg.projects`:

```rust
    if let Some(cfg) = cfg {
        if let Some(ref st) = cfg.state
            && let Some(ref ws_path) = st.store
            && ws_path != primary_path
            && ws_path.exists()
        {
            let other = RedbStore::open(ws_path).map_err(|e| e.to_string())?;
            if fl_core::store::Handles::kind_of(&other, &iri)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                return Ok(other);
            }
        }
        for entry in &cfg.projects {
            let path = entry.store_path();
            if path == primary_path || !path.exists() {
                continue;
            }
            let other = RedbStore::open(&path).map_err(|e| e.to_string())?;
            if fl_core::store::Handles::kind_of(&other, &iri)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                return Ok(other);
            }
        }
    }
```

- [ ] **Step 5: Run `cargo test -p fl-cli --lib -- --nocapture`**

Run:
```bash
cargo test -p fl-cli --lib -- --nocapture
```
Expected: all `fl-cli` unit tests in `config.rs` and `refs.rs` pass.

- [ ] **Step 6: Commit Task 5**

```bash
git add crates/cli/Cargo.toml crates/cli/src/config.rs crates/cli/src/main.rs
git commit -m "$(cat <<'EOF'
feat(cli): add workspace and per-project state config and store routing

Extend config.toml with [state] and [[project]].state / name fields,
resolve state stores and sync targets per StateScope, and include the
workspace state store in cross-store IRI lookup.

Co-authored-by: Ferris <ferris@artificialhumanity.io>
EOF
)"
```

---
### Task 6: `fl agent` and `fl state` CLI commands and read-only Markdown exports (`fl-cli`)

**Files:**
- Create: `crates/cli/src/cmd/agent.rs`
- Create: `crates/cli/src/cmd/state.rs`
- Modify: `crates/cli/src/cmd/mod.rs`
- Modify: `crates/cli/src/main.rs`

**Interfaces:**
- Consumes: `fl_core::agent::*`, `fl_core::state::*`, `fl_core::store::{AgentRegistry, StateStore, Handles, StoreError}`, `fl_exec::state_sync::{sync_state, verify_state}`, `fl_store::RedbStore`, `crate::config::Config`, `crate::refs::Ref`.
- Produces:
  - `crates/cli/src/cmd/agent.rs` (`AgentCmd`, `run`, `render_registry_markdown`, `write_atomic_non_symlink`, exit code `2` on `AgentCollision` / `LeadTaken`).
  - `crates/cli/src/cmd/state.rs` (`StateCmd`, `MemoryCmd`, `HandoffCmd`, `RuntimeCmd`, `run`, `resolve_state_entry`, `render_state_markdown`).
  - `Command::Agent` and `Command::State` wired in `crates/cli/src/main.rs`.

- [ ] **Step 1: Create `crates/cli/src/cmd/agent.rs` implementing Spec §5.1**

Create `crates/cli/src/cmd/agent.rs`:

```rust
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use fl_core::agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, reports_to,
};
use fl_core::at::At;
use fl_core::ids::{AgentId, Kind};
use fl_core::state::validate_slug;
use fl_core::store::{AgentRegistry, Handles, StoreError};

use crate::refs::Ref;

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Manage workspace coverage areas and projects (`areas` table).
    Area {
        #[command(subcommand)]
        cmd: AreaCmd,
    },
    /// Register or resume an agent identity in the workspace registry.
    Register {
        /// Display name (`Ferris`, `JARVIS`, `Kayla`); lowercased form is `name_key`.
        name: String,
        /// Primary project / area slug.
        #[arg(long)]
        project: String,
        /// Role (`lead`, `assistant`, `lab_manager`, or `custom:<slug>`).
        #[arg(long)]
        role: String,
        /// Create `--project` and any `--extra-area` in `areas` if not yet present.
        #[arg(long)]
        create_area: bool,
        /// Additional coverage area slugs.
        #[arg(long = "extra-area")]
        extra_areas: Vec<String>,
        /// Optional explicit lowercase email (defaults to `<name_key>@artificialhumanity.io`).
        #[arg(long)]
        email: Option<String>,
        /// Replace an existing agent session or retire a previous project Lead.
        #[arg(long)]
        replace: bool,
        /// Author recording this registration.
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Mark a registered agent as `retired`.
    Retire {
        /// Agent IRI (`urn:uuid:...`), numeric handle (`1`, `2`, …), or `name_key`.
        agent: String,
        /// Author recording this retirement.
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// List active (or all) registered agents and optionally render `REGISTRY.md`.
    Roster {
        /// Filter to agents covering this project/area slug.
        #[arg(long)]
        project: Option<String>,
        /// Include retired agents in the output.
        #[arg(long)]
        include_retired: bool,
        /// Atomically write the two-table Markdown roster to `<path>`.
        #[arg(long)]
        render: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum AreaCmd {
    /// Create or update a coverage area / project row.
    Set {
        /// Display name (`FerroLoop`, `AI-Lab-AMD`, `Lab-Manager`); lowercased to `area_key`.
        name: String,
        /// Optional workspace-relative directory path (e.g. `FerroLoop/`).
        #[arg(long)]
        path: Option<String>,
        /// One-line description of the area's purpose (<= 240 chars).
        #[arg(long)]
        purpose: String,
        /// Author recording this update.
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// List all registered coverage areas.
    List {
        #[arg(long)]
        json: bool,
    },
}

/// Write `content` atomically to `path` via a sibling tempfile + rename,
/// refusing symlinked targets or symlinked parent directories (spec §5.1, §5.2).
pub fn write_atomic_non_symlink(path: &Path, content: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if let Ok(meta) = std::fs::symlink_metadata(parent)
        && meta.file_type().is_symlink()
    {
        return Err(format!(
            "refusing to write under symlinked parent directory `{}`",
            parent.display()
        ));
    }
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(format!(
            "refusing to overwrite symlinked file `{}`",
            path.display()
        ));
    }
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| format!("failed to create tempfile in `{}`: {e}", parent.display()))?;
    tmp.write_all(content)
        .map_err(|e| format!("failed to write tempfile for `{}`: {e}", path.display()))?;
    tmp.flush()
        .map_err(|e| format!("failed to flush tempfile for `{}`: {e}", path.display()))?;
    tmp.persist(path)
        .map_err(|e| format!("failed to persist `{}`: {}", path.display(), e.error))?;
    Ok(())
}

/// Resolve `<name-or-id>` to an `AgentRecord` (spec §5.1):
/// 1. `urn:uuid:...` -> `get_agent`
/// 2. Bare digits (`1`, `2`, …) -> `resolve_handle(Kind::Agent, n)` -> `get_agent`
/// 3. Slug (`[a-z]...`) -> `agent_by_name`
pub fn resolve_agent<S: AgentRegistry + Handles>(
    store: &S,
    input: &str,
) -> Result<AgentRecord, String> {
    if let Ok(r) = Ref::parse(input) {
        let iri = match r {
            Ref::Iri(iri) => iri,
            Ref::Handle(h) => store
                .resolve_handle(Kind::Agent, h)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no agent with handle {h} in this store"))?,
        };
        return store
            .get_agent(&AgentId(iri))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no agent matching `{input}`"));
    }
    store
        .agent_by_name(input)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no agent matching `{input}`"))
}

/// Format the two-table Markdown roster (`Projects and Coverage Areas` + `Active Agents`)
/// matching `.agents/REGISTRY.md` (spec §5.1).
pub fn render_registry_markdown(areas: &[AreaRecord], agents: &[AgentRecord]) -> String {
    let mut out = String::from(
        "# Agent and Project Registry\n\n\
         ## Projects and Coverage Areas\n\n\
         | Key | Name | Path | Purpose |\n\
         | --- | --- | --- | --- |\n",
    );
    let mut sorted_areas = areas.to_vec();
    sorted_areas.sort_by(|a, b| a.area_key.cmp(&b.area_key));
    for a in &sorted_areas {
        let path_cell = a
            .path
            .as_deref()
            .map(|p| format!("`{p}`"))
            .unwrap_or_else(|| "—".to_string());
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            a.area_key, a.name, path_cell, a.purpose
        ));
    }

    out.push_str(
        "\n## Active Agents\n\n\
         | Persona | ID | Project | Extra Areas | Role | Reports To | Email |\n\
         | --- | --- | --- | --- | --- | --- | --- |\n",
    );
    let mut active: Vec<&AgentRecord> = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Active)
        .collect();
    active.sort_by(|a, b| a.name_key.cmp(&b.name_key));
    for ag in active {
        let extra = if ag.extra_areas.is_empty() {
            "—".to_string()
        } else {
            ag.extra_areas
                .iter()
                .map(|x| format!("`{x}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mgr = reports_to(ag, agents);
        out.push_str(&format!(
            "| **{}** (`{}`) | `{}` | `{}` | {} | `{}` | {} | `{}` |\n",
            ag.display_name,
            ag.name_key,
            ag.id,
            ag.project,
            extra,
            ag.role.as_wire(),
            mgr,
            ag.email,
        ));
    }
    out
}

pub fn run<S: AgentRegistry + Handles>(store: &S, cmd: AgentCmd) -> Result<(), (i32, String)> {
    match cmd {
        AgentCmd::Area { cmd } => match cmd {
            AreaCmd::Set {
                name,
                path,
                purpose,
                by,
                json,
            } => {
                let rec = AreaRecord::new(&name, path, &purpose, At::now(), &by)
                    .map_err(|e| (1, e.to_string()))?;
                let saved = store.upsert_area(rec).map_err(|e| (1, e.to_string()))?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&saved).unwrap());
                } else {
                    println!(
                        "{}\t{}\t{}\t{}",
                        saved.area_key,
                        saved.name,
                        saved.path.as_deref().unwrap_or("-"),
                        saved.purpose
                    );
                }
                Ok(())
            }
            AreaCmd::List { json } => {
                let areas = store.list_areas().map_err(|e| (1, e.to_string()))?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&areas).unwrap());
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
                Ok(())
            }
        },
        AgentCmd::Register {
            name,
            project,
            role,
            create_area,
            extra_areas,
            email,
            replace,
            by,
            json,
        } => {
            let parsed_role = AgentRole::from_wire(&role).map_err(|e| (1, e.to_string()))?;
            let args = RegisterAgentArgs {
                display_name: name,
                email,
                project,
                role: parsed_role,
                extra_areas,
                create_area,
                replace,
                at: At::now(),
                by,
            };
            match store.register_agent(args) {
                Ok(rec) => {
                    let all_active = store.list_agents(None, false).unwrap_or_default();
                    let mgr = reports_to(&rec, &all_active);
                    if json {
                        let mut val = serde_json::to_value(&rec).unwrap();
                        if let Some(obj) = val.as_object_mut() {
                            obj.insert("reports_to".into(), serde_json::Value::String(mgr));
                            obj.insert(
                                "co_author_trailer".into(),
                                serde_json::Value::String(rec.co_author_trailer()),
                            );
                        }
                        println!("{}", serde_json::to_string_pretty(&val).unwrap());
                    } else {
                        let h = store
                            .handle_of(Kind::Agent, rec.id.iri())
                            .ok()
                            .flatten()
                            .unwrap_or(0);
                        println!(
                            "{h}\t{}\t{}\t{}\t{}\treports_to={mgr}\t{}",
                            rec.id,
                            rec.display_name,
                            rec.project,
                            rec.role.as_wire(),
                            rec.co_author_trailer()
                        );
                    }
                    Ok(())
                }
                Err(
                    e @ (StoreError::AgentCollision { .. } | StoreError::LeadTaken { .. }),
                ) => Err((2, e.to_string())),
                Err(other) => Err((1, other.to_string())),
            }
        }
        AgentCmd::Retire { agent, by, json } => {
            let target = resolve_agent(store, &agent).map_err(|e| (1, e))?;
            let retired = store
                .retire_agent(&target.id, At::now(), &by)
                .map_err(|e| (1, e.to_string()))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&retired).unwrap());
            } else {
                println!("{}\t{}\tretired", retired.id, retired.display_name);
            }
            Ok(())
        }
        AgentCmd::Roster {
            project,
            include_retired,
            render,
            json,
        } => {
            if let Some(ref p) = project {
                let _ = validate_slug(p).map_err(|e| (1, e.to_string()))?;
            }
            let agents = store
                .list_agents(project.as_deref(), include_retired)
                .map_err(|e| (1, e.to_string()))?;
            let all_agents = store.list_agents(None, true).map_err(|e| (1, e.to_string()))?;
            if let Some(ref out_path) = render {
                let areas = store.list_areas().map_err(|e| (1, e.to_string()))?;
                let md = render_registry_markdown(&areas, &all_agents);
                write_atomic_non_symlink(out_path, md.as_bytes()).map_err(|e| (1, e))?;
            }
            if json {
                let enriched: Vec<serde_json::Value> = agents
                    .iter()
                    .map(|a| {
                        let mut v = serde_json::to_value(a).unwrap();
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert(
                                "reports_to".into(),
                                serde_json::Value::String(reports_to(a, &all_agents)),
                            );
                        }
                        v
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&enriched).unwrap());
            } else {
                for a in &agents {
                    let mgr = reports_to(a, &all_agents);
                    println!(
                        "{}\t{}\t{}\t{}\t{}\treports_to={mgr}\t{}",
                        a.name_key,
                        a.display_name,
                        a.project,
                        a.role.as_wire(),
                        a.status.as_wire(),
                        a.email
                    );
                }
            }
            Ok(())
        }
    }
}
```

- [ ] **Step 2: Create `crates/cli/src/cmd/state.rs` implementing Spec §5.2**

Create `crates/cli/src/cmd/state.rs`:

```rust
use std::path::{Path, PathBuf};

use clap::Subcommand;
use fl_core::agent::{AgentRecord, AgentStatus, AreaRecord, reports_to};
use fl_core::at::At;
use fl_core::ids::{Kind, RevisionId, StateId};
use fl_core::iri::Iri;
use fl_core::state::{
    HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
    RuntimeEntry, SensitivityPatch, StateConcern, StateEntry, StatePayload, StateScope,
    UpsertStateArgs, validate_slug,
};
use fl_core::store::{AgentRegistry, Handles, StateStore};
use fl_exec::state_sync::{sync_state, verify_state};
use fl_store::RedbStore;

use crate::cmd::agent::write_atomic_non_symlink;
use crate::config::Config;
use crate::refs::Ref;

/// Default byte ceiling for `fl state render` (24.4 KB, matching workspace index cap).
pub const DEFAULT_RENDER_MAX_BYTES: usize = 24_400;

#[derive(Subcommand)]
pub enum StateCmd {
    /// Manage durable `Memory` entries.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Manage `Handoff` status snapshots.
    Handoff {
        #[command(subcommand)]
        cmd: HandoffCmd,
    },
    /// Manage structured JSON `Runtime` entries.
    Runtime {
        #[command(subcommand)]
        cmd: RuntimeCmd,
    },
    /// Fetch a state entry (and optional revision history) by IRI, handle, or key.
    Get {
        /// `urn:uuid:...` IRI, numeric handle (`1`, `2`, …), or entry slug (`[a-z]...`).
        id_or_key: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        concern: Option<String>,
        #[arg(long)]
        history: bool,
        #[arg(long)]
        json: bool,
    },
    /// List state entries filtered by scope, concern, or memory category.
    List {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        concern: Option<String>,
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        include_retired: bool,
        #[arg(long)]
        json: bool,
    },
    /// List unresolved `StateConflict`s.
    Conflicts {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Resolve an active `StateConflict` by selecting `--keep <revision-id>`.
    Resolve {
        /// `urn:uuid:...` IRI, numeric handle, or entry slug.
        id_or_key: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        concern: Option<String>,
        /// Winning revision IRI (`local_head` or `remote_head`).
        #[arg(long)]
        keep: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Render a deterministic read-only Markdown startup summary.
    Render {
        #[arg(long = "scope")]
        scopes: Vec<String>,
        #[arg(long, default_value_t = DEFAULT_RENDER_MAX_BYTES)]
        max_bytes: usize,
        #[arg(long)]
        include_sensitive: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Synchronize state and registry revisions with the remote `fl/state` orphan branch.
    Sync {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Verify `fl/state` orphan-branch integrity and append-only invariants.
    Verify {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Run the newline-delimited JSON-RPC 2.0 MCP server over stdio.
    Mcp,
}

#[derive(Subcommand)]
pub enum MemoryCmd {
    /// Create or update a `Memory` entry.
    Set {
        #[arg(long)]
        scope: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        category: String,
        #[arg(long)]
        basis: String,
        #[arg(long)]
        summary: String,
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        #[arg(long = "body-file", conflicts_with = "body")]
        body_file: Option<PathBuf>,
        #[arg(long, conflicts_with = "unpinned")]
        pinned: bool,
        #[arg(long, conflicts_with = "pinned")]
        unpinned: bool,
        #[arg(long, conflicts_with = "not_sensitive")]
        sensitive: bool,
        #[arg(long = "not-sensitive", conflicts_with = "sensitive")]
        not_sensitive: bool,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Retire or supersede an existing `Memory` entry.
    Retire {
        /// `urn:uuid:...` IRI, numeric handle, or memory slug.
        id_or_key: String,
        #[arg(long)]
        scope: Option<String>,
        /// Target `Memory` entry (IRI, handle, or slug in the same scope) that supersedes this entry.
        #[arg(long = "superseded-by")]
        superseded_by: Option<String>,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum HandoffCmd {
    /// Create or update a `Handoff` snapshot (`--key` defaults to `"current"`).
    Set {
        #[arg(long)]
        scope: String,
        #[arg(long, default_value = "current")]
        key: String,
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
        #[arg(long, conflicts_with = "not_sensitive")]
        sensitive: bool,
        #[arg(long = "not-sensitive", conflicts_with = "sensitive")]
        not_sensitive: bool,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum RuntimeCmd {
    /// Create or update a structured JSON `Runtime` entry.
    Set {
        #[arg(long)]
        scope: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        value: String,
        #[arg(long, default_value = "conflict")]
        merge: String,
        #[arg(long, conflicts_with = "not_sensitive")]
        sensitive: bool,
        #[arg(long = "not-sensitive", conflicts_with = "sensitive")]
        not_sensitive: bool,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
}

pub fn sensitivity_from_flags(sensitive: bool, not_sensitive: bool) -> SensitivityPatch {
    if sensitive {
        SensitivityPatch::SetSensitive
    } else if not_sensitive {
        SensitivityPatch::ClearSensitive
    } else {
        SensitivityPatch::Inherit
    }
}

/// Resolve `<id-or-key>` in `store` per Spec §5.2:
/// 1. `urn:uuid:...` -> `get_state`
/// 2. Bare digits (`1`, `2`, …) -> `resolve_handle(Kind::State, n)` -> `get_state`
/// 3. ASCII-letter slug (`[a-z]...`) -> requires `scope`; if `concern` is given, uses
///    `get_state_by_key`, otherwise checks all 3 concerns (defaulting to `Handoff` for
///    `"current"` when present and unambiguous, or failing if ambiguous).
pub fn resolve_state_in_store<S: StateStore + Handles>(
    store: &S,
    id_or_key: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
) -> Result<StateEntry, String> {
    if let Ok(r) = Ref::parse(id_or_key) {
        let iri = match r {
            Ref::Iri(iri) => iri,
            Ref::Handle(h) => store
                .resolve_handle(Kind::State, h)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no state entry with handle {h} in this store"))?,
        };
        return store
            .get_state(&StateId(iri))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no state entry matching `{id_or_key}`"));
    }

    let slug = validate_slug(id_or_key).map_err(|e| e.to_string())?;
    let Some(scope) = scope else {
        return Err(format!(
            "resolving state slug `{slug}` requires `--scope <scope>`"
        ));
    };
    if let Some(c) = concern {
        return store
            .get_state_by_key(scope, c, &slug)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "no state entry for `({}, {}, {slug})`",
                    scope.as_wire(),
                    c.as_wire()
                )
            });
    }

    let mut matches = Vec::new();
    for c in [
        StateConcern::Memory,
        StateConcern::Handoff,
        StateConcern::Runtime,
    ] {
        if let Some(e) = store
            .get_state_by_key(scope, c, &slug)
            .map_err(|e| e.to_string())?
        {
            matches.push(e);
        }
    }
    match matches.len() {
        0 => Err(format!(
            "no state entry with slug `{slug}` in scope `{}`",
            scope.as_wire()
        )),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => {
            if slug == "current"
                && let Some(h) = matches
                    .iter()
                    .find(|e| e.concern == StateConcern::Handoff)
                    .cloned()
            {
                return Ok(h);
            }
            Err(format!(
                "slug `{slug}` exists in multiple concerns under `{}`; pass `--concern <memory|handoff|runtime>`",
                scope.as_wire()
            ))
        }
    }
}

fn scope_rank(scope: &StateScope) -> (u8, String) {
    match scope {
        StateScope::Workspace => (0, String::new()),
        StateScope::Group(s) => (1, s.clone()),
        StateScope::Project(s) => (2, s.clone()),
        StateScope::Agent(s) => (3, s.clone()),
        StateScope::Workflow(s) => (4, s.clone()),
    }
}

/// Render the deterministic read-only startup Markdown summary (spec §5.2):
/// 1. Active agent roster summary.
/// 2. Current `Handoff` snapshot(s) for the requested scopes.
/// 3. Active `Memory` index lines ordered by priority tier (`pinned || category == Warning`
///    first, then remaining active memories), sorted by `(scope_rank, slug)` within each tier,
///    capped so total output (including the `_Truncated: ..._` footer line) never exceeds `max_bytes`.
pub fn render_state_markdown(
    areas: &[AreaRecord],
    agents: &[AgentRecord],
    entries: &[StateEntry],
    max_bytes: usize,
    include_sensitive: bool,
) -> String {
    let visible: Vec<&StateEntry> = entries
        .iter()
        .filter(|e| include_sensitive || !e.sensitive)
        .collect();

    let mut out = String::from("# FerroLoop State Summary\n\n");

    // 1. Active Agents section.
    out.push_str("## Active Agents\n\n");
    let mut active_agents: Vec<&AgentRecord> = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Active)
        .collect();
    active_agents.sort_by(|a, b| a.name_key.cmp(&b.name_key));
    if active_agents.is_empty() {
        out.push_str("_No active agents registered._\n\n");
    } else {
        for a in active_agents {
            let mgr = reports_to(a, agents);
            let area_name = areas
                .iter()
                .find(|ar| ar.area_key == a.project)
                .map(|ar| ar.name.as_str())
                .unwrap_or(a.project.as_str());
            out.push_str(&format!(
                "- **{}** (`{}`, `{}`, project `{}` / {}, reports to {})\n",
                a.display_name,
                a.name_key,
                a.role.as_wire(),
                a.project,
                area_name,
                mgr
            ));
        }
        out.push('\n');
    }

    // 2. Current Handoff section.
    let mut handoffs: Vec<(&StateEntry, &HandoffEntry)> = visible
        .iter()
        .filter_map(|e| match &e.payload {
            StatePayload::Handoff(h) => Some((*e, h)),
            _ => None,
        })
        .collect();
    handoffs.sort_by(|(a, _), (b, _)| {
        (scope_rank(&a.scope), &a.slug).cmp(&(scope_rank(&b.scope), &b.slug))
    });
    if !handoffs.is_empty() {
        out.push_str("## Handoff\n\n");
        for (entry, h) in handoffs {
            out.push_str(&format!(
                "### `{}` / `{}` — {}\n",
                entry.scope.as_wire(),
                entry.slug,
                h.summary
            ));
            if !h.in_flight.is_empty() {
                out.push_str(&format!("- **In flight:** {}\n", h.in_flight.join("; ")));
            }
            if !h.blockers.is_empty() {
                out.push_str(&format!("- **Blockers:** {}\n", h.blockers.join("; ")));
            }
            if !h.next_steps.is_empty() {
                out.push_str(&format!("- **Next steps:** {}\n", h.next_steps.join("; ")));
            }
            if !h.shipped.is_empty() {
                out.push_str(&format!("- **Shipped:** {}\n", h.shipped.join("; ")));
            }
            out.push('\n');
        }
    }

    // 3. Active Memory Index section.
    let mut memories: Vec<(&StateEntry, &MemoryEntry)> = visible
        .iter()
        .filter_map(|e| match &e.payload {
            StatePayload::Memory(m) if m.lifecycle == MemoryLifecycle::Active => Some((*e, m)),
            _ => None,
        })
        .collect();
    memories.sort_by(|(ea, ma), (eb, mb)| {
        let tier_a = !(ma.pinned || ma.category == MemoryCategory::Warning);
        let tier_b = !(mb.pinned || mb.category == MemoryCategory::Warning);
        (tier_a, scope_rank(&ea.scope), &ea.slug).cmp(&(tier_b, scope_rank(&eb.scope), &eb.slug))
    });

    out.push_str("## Memory Index\n\n");
    if out.len() > max_bytes {
        out.truncate(max_bytes);
        return out;
    }

    let total_memories = memories.len();
    for (idx, (entry, mem)) in memories.iter().enumerate() {
        let line = format!(
            "- [{}] ({}, {}, {}) — {}\n",
            entry.slug,
            entry.id,
            mem.category.as_wire(),
            mem.basis.as_wire(),
            mem.summary
        );
        let remaining_after_this = total_memories - (idx + 1);
        let reserve_footer = if remaining_after_this > 0 {
            format!(
                "_Truncated: {} additional active memory entries omitted by byte cap._\n",
                remaining_after_this
            )
            .len()
        } else {
            0
        };

        if out.len() + line.len() + reserve_footer > max_bytes {
            let omitted = total_memories - idx;
            let footer = format!(
                "_Truncated: {omitted} additional active memory entries omitted by byte cap._\n"
            );
            if out.len() + footer.len() <= max_bytes {
                out.push_str(&footer);
            }
            break;
        }
        out.push_str(&line);
    }

    out
}

pub struct StateEnv<'a> {
    pub explicit_db: Option<&'a Path>,
    pub default_store: &'a Path,
    pub cfg: Option<&'a Config>,
    pub cwd: &'a Path,
}

impl<'a> StateEnv<'a> {
    pub fn open_for_scope(&self, scope: Option<&StateScope>) -> Result<RedbStore, String> {
        let path = if let Some(db) = self.explicit_db {
            db.to_path_buf()
        } else if let (Some(cfg), Some(sc)) = (self.cfg, scope) {
            cfg.state_store_for_scope(sc, self.default_store)
        } else if let Some(cfg) = self.cfg {
            cfg.workspace_state_store(self.default_store)
        } else {
            self.default_store.to_path_buf()
        };
        RedbStore::open(&path).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn open_for_id_or_key(
        &self,
        id_or_key: &str,
        scope: Option<&StateScope>,
    ) -> Result<RedbStore, String> {
        let primary_path = if let Some(db) = self.explicit_db {
            db.to_path_buf()
        } else if let (Some(cfg), Some(sc)) = (self.cfg, scope) {
            cfg.state_store_for_scope(sc, self.default_store)
        } else if let Some(cfg) = self.cfg {
            cfg.workspace_state_store(self.default_store)
        } else {
            self.default_store.to_path_buf()
        };
        let primary =
            RedbStore::open(&primary_path).map_err(|e| format!("{}: {e}", primary_path.display()))?;
        if self.explicit_db.is_some() {
            return Ok(primary);
        }
        let Ok(Ref::Iri(iri)) = Ref::parse(id_or_key) else {
            return Ok(primary);
        };
        if Handles::kind_of(&primary, &iri)
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Ok(primary);
        }
        if let Some(cfg) = self.cfg {
            let ws_path = cfg.workspace_state_store(self.default_store);
            if ws_path != primary_path && ws_path.exists() {
                let other = RedbStore::open(&ws_path).map_err(|e| e.to_string())?;
                if Handles::kind_of(&other, &iri)
                    .map_err(|e| e.to_string())?
                    .is_some()
                {
                    return Ok(other);
                }
            }
            for entry in &cfg.projects {
                let path = entry.store_path();
                if path == primary_path || !path.exists() {
                    continue;
                }
                let other = RedbStore::open(&path).map_err(|e| e.to_string())?;
                if Handles::kind_of(&other, &iri)
                    .map_err(|e| e.to_string())?
                    .is_some()
                {
                    return Ok(other);
                }
            }
        }
        Ok(primary)
    }
}

pub fn run(env: &StateEnv<'_>, cmd: StateCmd) -> Result<(), String> {
    match cmd {
        StateCmd::Memory { cmd } => match cmd {
            MemoryCmd::Set {
                scope,
                key,
                category,
                basis,
                summary,
                body,
                body_file,
                pinned,
                unpinned,
                sensitive,
                not_sensitive,
                by,
                json,
            } => {
                let parsed_scope = StateScope::from_wire(&scope).map_err(|e| e.to_string())?;
                let parsed_cat: MemoryCategory = category.parse()?;
                let parsed_basis: MemoryBasis = basis.parse()?;
                let resolved_body = match (body, body_file) {
                    (Some(b), None) => b,
                    (None, Some(path)) => std::fs::read_to_string(&path)
                        .map_err(|e| format!("{}: {e}", path.display()))?,
                    _ => return Err("provide either `--body <text>` or `--body-file <path>`".into()),
                };
                let store = env.open_for_scope(Some(&parsed_scope))?;
                let existing = store
                    .get_state_by_key(&parsed_scope, StateConcern::Memory, &key)
                    .map_err(|e| e.to_string())?;
                let resolved_pinned = if pinned {
                    true
                } else if unpinned {
                    false
                } else if let Some(StateEntry {
                    payload: StatePayload::Memory(ref m),
                    ..
                }) = existing
                {
                    m.pinned
                } else {
                    false
                };
                let entry = store
                    .upsert_state(UpsertStateArgs {
                        scope: parsed_scope,
                        slug: key,
                        sensitivity: sensitivity_from_flags(sensitive, not_sensitive),
                        payload: StatePayload::Memory(MemoryEntry {
                            category: parsed_cat,
                            basis: parsed_basis,
                            lifecycle: MemoryLifecycle::Active,
                            superseded_by: None,
                            pinned: resolved_pinned,
                            summary,
                            body: resolved_body,
                        }),
                        at: At::now(),
                        by,
                    })
                    .map_err(|e| e.to_string())?;
                print_state_entry(&store, &entry, json);
                Ok(())
            }
            MemoryCmd::Retire {
                id_or_key,
                scope,
                superseded_by,
                by,
                json,
            } => {
                let parsed_scope = scope
                    .as_deref()
                    .map(StateScope::from_wire)
                    .transpose()
                    .map_err(|e| e.to_string())?;
                let store = env.open_for_id_or_key(&id_or_key, parsed_scope.as_ref())?;
                let target_entry = resolve_state_in_store(
                    &store,
                    &id_or_key,
                    parsed_scope.as_ref(),
                    Some(StateConcern::Memory),
                )?;
                let (lifecycle, sup_id) = match superseded_by {
                    Some(ref raw_sup) => {
                        let sup_entry = resolve_state_in_store(
                            &store,
                            raw_sup,
                            Some(&target_entry.scope),
                            Some(StateConcern::Memory),
                        )?;
                        (MemoryLifecycle::Superseded, Some(sup_entry.id))
                    }
                    None => (MemoryLifecycle::Retired, None),
                };
                let updated = store
                    .retire_memory(
                        &target_entry.scope,
                        &target_entry.slug,
                        lifecycle,
                        sup_id,
                        At::now(),
                        &by,
                    )
                    .map_err(|e| e.to_string())?;
                print_state_entry(&store, &updated, json);
                Ok(())
            }
        },
        StateCmd::Handoff { cmd } => match cmd {
            HandoffCmd::Set {
                scope,
                key,
                summary,
                shipped,
                in_flight,
                blockers,
                next_steps,
                notes,
                sensitive,
                not_sensitive,
                by,
                json,
            } => {
                let parsed_scope = StateScope::from_wire(&scope).map_err(|e| e.to_string())?;
                let store = env.open_for_scope(Some(&parsed_scope))?;
                let entry = store
                    .upsert_state(UpsertStateArgs {
                        scope: parsed_scope,
                        slug: key,
                        sensitivity: sensitivity_from_flags(sensitive, not_sensitive),
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
                print_state_entry(&store, &entry, json);
                Ok(())
            }
        },
        StateCmd::Runtime { cmd } => match cmd {
            RuntimeCmd::Set {
                scope,
                key,
                value,
                merge,
                sensitive,
                not_sensitive,
                by,
                json,
            } => {
                let parsed_scope = StateScope::from_wire(&scope).map_err(|e| e.to_string())?;
                let merge_mode: MergeMode = merge.parse()?;
                let json_val: serde_json::Value = serde_json::from_str(&value)
                    .map_err(|e| format!("invalid `--value` JSON: {e}"))?;
                let store = env.open_for_scope(Some(&parsed_scope))?;
                let entry = store
                    .upsert_state(UpsertStateArgs {
                        scope: parsed_scope,
                        slug: key,
                        sensitivity: sensitivity_from_flags(sensitive, not_sensitive),
                        payload: StatePayload::Runtime(RuntimeEntry {
                            value: json_val,
                            merge_mode,
                        }),
                        at: At::now(),
                        by,
                    })
                    .map_err(|e| e.to_string())?;
                print_state_entry(&store, &entry, json);
                Ok(())
            }
        },
        StateCmd::Get {
            id_or_key,
            scope,
            concern,
            history,
            json,
        } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let parsed_concern: Option<StateConcern> =
                concern.as_deref().map(str::parse).transpose()?;
            let store = env.open_for_id_or_key(&id_or_key, parsed_scope.as_ref())?;
            let entry =
                resolve_state_in_store(&store, &id_or_key, parsed_scope.as_ref(), parsed_concern)?;
            if history {
                let revs = store
                    .state_revisions(&entry.id)
                    .map_err(|e| e.to_string())?;
                if json {
                    let out = serde_json::json!({
                        "entry": entry,
                        "revisions": revs,
                    });
                    println!("{}", serde_json::to_string_pretty(&out).unwrap());
                } else {
                    print_state_entry(&store, &entry, false);
                    for r in revs {
                        println!(
                            "  rev {}\tparents={}\tat={}\tby={}",
                            r.revision_id,
                            r.parent_revisions
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(","),
                            r.at,
                            r.by
                        );
                    }
                }
            } else {
                print_state_entry(&store, &entry, json);
            }
            Ok(())
        }
        StateCmd::List {
            scope,
            concern,
            category,
            include_retired,
            json,
        } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let parsed_concern: Option<StateConcern> =
                concern.as_deref().map(str::parse).transpose()?;
            let parsed_cat: Option<MemoryCategory> =
                category.as_deref().map(str::parse).transpose()?;
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let entries: Vec<StateEntry> = store
                .list_state(parsed_scope.as_ref(), parsed_concern)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|e| match &e.payload {
                    StatePayload::Memory(m) => {
                        (include_retired || m.lifecycle == MemoryLifecycle::Active)
                            && parsed_cat.is_none_or(|c| m.category == c)
                    }
                    _ => parsed_cat.is_none(),
                })
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&entries).unwrap());
            } else {
                for e in &entries {
                    print_state_entry(&store, e, false);
                }
            }
            Ok(())
        }
        StateCmd::Conflicts { scope, json } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let mut conflicts = store.list_conflicts().map_err(|e| e.to_string())?;
            if let Some(ref sc) = parsed_scope {
                conflicts.retain(|c| {
                    store
                        .get_state(&c.entry_id)
                        .ok()
                        .flatten()
                        .is_some_and(|e| &e.scope == sc)
                });
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&conflicts).unwrap());
            } else {
                for c in conflicts {
                    println!(
                        "{}\tlocal={}\tremote={}\tdetected_at={}",
                        c.entry_id, c.local_head, c.remote_head, c.detected_at
                    );
                }
            }
            Ok(())
        }
        StateCmd::Resolve {
            id_or_key,
            scope,
            concern,
            keep,
            by,
            json,
        } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let parsed_concern: Option<StateConcern> =
                concern.as_deref().map(str::parse).transpose()?;
            let keep_iri = Iri::parse(&keep).map_err(|e| format!("invalid `--keep` IRI: {e}"))?;
            let store = env.open_for_id_or_key(&id_or_key, parsed_scope.as_ref())?;
            let entry =
                resolve_state_in_store(&store, &id_or_key, parsed_scope.as_ref(), parsed_concern)?;
            let resolved = store
                .resolve_conflict(&entry.id, &RevisionId(keep_iri), At::now(), &by)
                .map_err(|e| e.to_string())?;
            print_state_entry(&store, &resolved, json);
            Ok(())
        }
        StateCmd::Render {
            scopes,
            max_bytes,
            include_sensitive,
            out,
        } => {
            let mut parsed_scopes = Vec::new();
            for raw in &scopes {
                parsed_scopes
                    .push(StateScope::from_wire(raw).map_err(|e| e.to_string())?);
            }
            let ws_store = env.open_for_scope(Some(&StateScope::Workspace))?;
            let areas = ws_store.list_areas().map_err(|e| e.to_string())?;
            let agents = ws_store
                .list_agents(None, false)
                .map_err(|e| e.to_string())?;

            let mut entries = Vec::new();
            if parsed_scopes.is_empty() {
                entries = ws_store
                    .list_state(None, None)
                    .map_err(|e| e.to_string())?;
            } else {
                for sc in &parsed_scopes {
                    let st = env.open_for_scope(Some(sc))?;
                    entries.extend(st.list_state(Some(sc), None).map_err(|e| e.to_string())?);
                }
            }

            let md = render_state_markdown(&areas, &agents, &entries, max_bytes, include_sensitive);
            if let Some(ref path) = out {
                write_atomic_non_symlink(path, md.as_bytes())?;
            } else {
                print!("{md}");
            }
            Ok(())
        }
        StateCmd::Sync { scope, by, json } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let target = match env.cfg {
                Some(cfg) => cfg.sync_target_for_scope(parsed_scope.as_ref(), env.cwd, &by)?,
                None => fl_exec::state_sync::StateSyncTarget {
                    repo_dir: env.cwd.to_path_buf(),
                    remote: "origin".into(),
                    branch: crate::config::DEFAULT_STATE_BRANCH.into(),
                    private: false,
                    sync_registry: true,
                    by,
                },
            };
            let report = sync_state(&store, &target, At::now()).map_err(|e| e.to_string())?;
            if json {
                let val = serde_json::json!({
                    "remote": report.remote,
                    "branch": report.branch,
                    "previous_commit": report.previous_commit,
                    "committed_oid": report.committed_oid,
                    "imported_registry_events": report.imported_registry_events,
                    "imported_revisions": report.imported_revisions,
                    "published_registry_events": report.published_registry_events,
                    "published_revisions": report.published_revisions,
                    "active_conflicts": report.active_conflicts,
                });
                println!("{}", serde_json::to_string_pretty(&val).unwrap());
            } else {
                println!(
                    "synced {}/{}: imported_revs={} published_revs={} conflicts={}",
                    report.remote,
                    report.branch,
                    report.imported_revisions,
                    report.published_revisions,
                    report.active_conflicts
                );
            }
            Ok(())
        }
        StateCmd::Verify { scope, json } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let target = match env.cfg {
                Some(cfg) => cfg.sync_target_for_scope(parsed_scope.as_ref(), env.cwd, "fl")?,
                None => fl_exec::state_sync::StateSyncTarget {
                    repo_dir: env.cwd.to_path_buf(),
                    remote: "origin".into(),
                    branch: crate::config::DEFAULT_STATE_BRANCH.into(),
                    private: false,
                    sync_registry: true,
                    by: "fl".into(),
                },
            };
            let report = verify_state(&store, &target).map_err(|e| e.to_string())?;
            if json {
                let val = serde_json::json!({
                    "remote": report.remote,
                    "branch": report.branch,
                    "tip_commit": report.tip_commit,
                    "verified_commits": report.verified_commits,
                    "verified_revisions": report.verified_revisions,
                    "verified_registry_events": report.verified_registry_events,
                });
                println!("{}", serde_json::to_string_pretty(&val).unwrap());
            } else {
                println!(
                    "verified {}/{}: commits={} revisions={} registry_events={}",
                    report.remote,
                    report.branch,
                    report.verified_commits,
                    report.verified_revisions,
                    report.verified_registry_events
                );
            }
            Ok(())
        }
        StateCmd::Mcp => unreachable!("`fl state mcp` is dispatched before opening RedbStore"),
    }
}

fn print_state_entry<S: Handles>(store: &S, entry: &StateEntry, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(entry).unwrap());
    } else {
        let h = store
            .handle_of(Kind::State, entry.id.iri())
            .ok()
            .flatten()
            .unwrap_or(0);
        println!(
            "{h}\t{}\t{}\t{}\t{}\trev={}",
            entry.id,
            entry.scope.as_wire(),
            entry.concern.as_wire(),
            entry.slug,
            entry.head_revision
        );
    }
}
```

- [ ] **Step 3: Register `agent` and `state` in `crates/cli/src/cmd/mod.rs` and wire `Command::Agent` and `Command::State` in `crates/cli/src/main.rs`**

1. Update `crates/cli/src/cmd/mod.rs`:

```rust
pub mod agent;
pub mod finding;
pub mod gate;
pub mod init;
pub mod mcp;
pub mod project;
pub mod record;
pub mod run;
pub mod state;
```

2. Update `crates/cli/src/main.rs` to add `Agent` and `State` variants to `Command` and dispatch them before the gate/record/finding `RedbStore::open(&resolved.store)` call (so `fl agent` and `fl state` open only the target state store, and `fl agent register` exits with status `2` on `AgentCollision` or `LeadTaken`):

```rust
    /// Manage the workspace agent and coverage-area registry.
    Agent {
        #[command(subcommand)]
        cmd: cmd::agent::AgentCmd,
    },
    /// Manage durable memories, handoffs, runtime state, and git state sync.
    State {
        #[command(subcommand)]
        cmd: cmd::state::StateCmd,
    },
```

And in `run()` in `crates/cli/src/main.rs` (right after handling `Command::Mcp`):

```rust
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let loaded_cfg = if cli.db.is_none() {
        config::load(&xdg_cfg)?
    } else {
        None
    };

    if let Command::Agent { cmd } = cli.cmd {
        let store_path = match &cli.db {
            Some(explicit) => explicit.clone(),
            None => match &loaded_cfg {
                Some(cfg) => cfg.workspace_state_store(&default_store),
                None => default_store.clone(),
            },
        };
        let store = RedbStore::open(&store_path).map_err(|e| (1, e.to_string()))?;
        return cmd::agent::run(&store, cmd);
    }

    if let Command::State { cmd } = cli.cmd {
        let state_env = cmd::state::StateEnv {
            explicit_db: cli.db.as_deref(),
            default_store: &default_store,
            cfg: loaded_cfg.as_ref(),
            cwd: &cwd,
        };
        return cmd::state::run(&state_env, cmd).map_err(|e| (1, e));
    }
```

And update `fn main()` and `fn run() -> Result<(), (i32, String)>` in `crates/cli/src/main.rs` so errors carry an exit code (`1` by default, `2` for `AgentCollision` / `LeadTaken`):

```rust
fn main() {
    if let Err((code, e)) = run() {
        eprintln!("error: {e}");
        std::process::exit(code);
    }
}
```

*(Map existing `Result<_, String>` calls in `run()` with `.map_err(|e| (1, e))?`.)*

- [ ] **Step 4: Run `cargo test -p fl-cli -- --nocapture`**

Run:
```bash
cargo test -p fl-cli -- --nocapture
```
Expected: `fl-cli` compiles cleanly and all existing CLI unit and integration tests pass.

- [ ] **Step 5: Commit Task 6**

```bash
git add crates/cli/src/cmd/agent.rs crates/cli/src/cmd/state.rs crates/cli/src/cmd/mod.rs crates/cli/src/main.rs
git commit -m "$(cat <<'EOF'
feat(cli): add fl agent and fl state subcommands and Markdown renderers

Implement fl agent (area, register, retire, roster --render) with exit
code 2 on collision/lead guards and fl state (memory, handoff, runtime,
get, list, conflicts, resolve, render, sync, verify).

Co-authored-by: Ferris <ferris@artificialhumanity.io>
EOF
)"
```

---
### Task 7: `fl state mcp` stdio server and end-to-end black-box test suite (`fl-cli`)

**Files:**
- Create: `crates/cli/src/cmd/state_mcp.rs`
- Modify: `crates/cli/src/cmd/mod.rs`
- Modify: `crates/cli/src/cmd/state.rs`
- Create: `crates/cli/tests/state_sync.rs`

**Interfaces:**
- Consumes: `crate::cmd::state::{StateEnv, resolve_state_in_store, render_state_markdown, sensitivity_from_flags, DEFAULT_RENDER_MAX_BYTES}`, `crate::cmd::agent::render_registry_markdown`, `fl_core::agent::*`, `fl_core::state::*`, `fl_core::store::{AgentRegistry, StateStore}`.
- Produces:
  - `crates/cli/src/cmd/state_mcp.rs` implementing the newline-delimited JSON-RPC 2.0 MCP server (`fl state mcp`) with the 8 tools (`agent_roster`, `agent_register`, `state_render`, `state_get`, `state_list`, `state_set_memory`, `state_set_handoff`, `state_set_runtime`) and per-call `RedbStore` open/drop.
  - `crates/cli/tests/state_sync.rs` covering all 9 Spec §6.3 black-box scenarios.

- [ ] **Step 1: Create `crates/cli/src/cmd/state_mcp.rs` implementing Spec §5.3**

Create `crates/cli/src/cmd/state_mcp.rs`:

```rust
//! Newline-delimited JSON-RPC 2.0 MCP server over `stdin`/`stdout` (spec §5.3).
//!
//! Opens and drops `RedbStore` inside each individual `tools/call` invocation so the
//! exclusive `redb` file lock is never held while waiting on `stdin`.

use std::io::{BufRead, Write};

use fl_core::agent::{AgentRole, RegisterAgentArgs, reports_to};
use fl_core::at::At;
use fl_core::state::{
    HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
    RuntimeEntry, SensitivityPatch, StateConcern, StateEntry, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_core::store::{AgentRegistry, StateStore};
use serde_json::{Value, json};

use crate::cmd::state::{DEFAULT_RENDER_MAX_BYTES, StateEnv, render_state_markdown, resolve_state_in_store};

pub fn run(env: &StateEnv<'_>) -> Result<(), String> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {
                        "code": -32700,
                        "message": format!("parse error: {e}"),
                    }
                });
                write_line(&stdout, &err_resp)?;
                continue;
            }
        };

        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        // Notifications have no `id` and expect no response.
        let Some(req_id) = id else {
            continue;
        };

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "fl-state-mcp",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }
            }),
            "ping" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {}
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "tools": tool_definitions(),
                }
            }),
            "tools/call" => {
                let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match dispatch_tool(env, tool_name, &args) {
                    Ok(payload) => json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": payload,
                            }],
                            "isError": false,
                        }
                    }),
                    Err(err_msg) => json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": err_msg,
                            }],
                            "isError": true,
                        }
                    }),
                }
            }
            other => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {
                    "code": -32601,
                    "message": format!("method not found: {other}"),
                }
            }),
        };

        write_line(&stdout, &response)?;
    }
    Ok(())
}

fn write_line(stdout: &std::io::Stdout, val: &Value) -> Result<(), String> {
    let mut out = stdout.lock();
    let s = serde_json::to_string(val).map_err(|e| e.to_string())?;
    out.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
    out.write_all(b"\n").map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

fn parse_sensitivity(args: &Value) -> SensitivityPatch {
    if args
        .get("sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        SensitivityPatch::SetSensitive
    } else if args
        .get("clear_sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        SensitivityPatch::ClearSensitive
    } else {
        SensitivityPatch::Inherit
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required string argument `{key}`"))
}

fn dispatch_tool(env: &StateEnv<'_>, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "agent_roster" => {
            let project = args.get("project").and_then(Value::as_str);
            let include_retired = args
                .get("include_retired")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let store = env.open_for_scope(Some(&StateScope::Workspace))?;
            let agents = store
                .list_agents(project, include_retired)
                .map_err(|e| e.to_string())?;
            let all_agents = store.list_agents(None, true).map_err(|e| e.to_string())?;
            let enriched: Vec<Value> = agents
                .iter()
                .map(|a| {
                    let mut v = serde_json::to_value(a).unwrap();
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert(
                            "reports_to".into(),
                            Value::String(reports_to(a, &all_agents)),
                        );
                    }
                    v
                })
                .collect();
            Ok(serde_json::to_string_pretty(&enriched).unwrap())
        }
        "agent_register" => {
            let display_name = req_str(args, "name")?.to_string();
            let project = req_str(args, "project")?.to_string();
            let role_raw = req_str(args, "role")?;
            let role = AgentRole::from_wire(role_raw).map_err(|e| e.to_string())?;
            let by = req_str(args, "by")?.to_string();
            let email = args
                .get("email")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let create_area = args
                .get("create_area")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let replace = args
                .get("replace")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let extra_areas: Vec<String> = args
                .get("extra_areas")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default();

            let store = env.open_for_scope(Some(&StateScope::Workspace))?;
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
            let all_active = store.list_agents(None, false).unwrap_or_default();
            let mgr = reports_to(&rec, &all_active);
            let mut val = serde_json::to_value(&rec).unwrap();
            if let Some(obj) = val.as_object_mut() {
                obj.insert("reports_to".into(), Value::String(mgr));
                obj.insert(
                    "co_author_trailer".into(),
                    Value::String(rec.co_author_trailer()),
                );
            }
            Ok(serde_json::to_string_pretty(&val).unwrap())
        }
        "state_render" => {
            let max_bytes = args
                .get("max_bytes")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_RENDER_MAX_BYTES);
            let include_sensitive = args
                .get("include_sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut parsed_scopes = Vec::new();
            if let Some(arr) = args.get("scopes").and_then(Value::as_array) {
                for v in arr {
                    if let Some(s) = v.as_str() {
                        parsed_scopes
                            .push(StateScope::from_wire(s).map_err(|e| e.to_string())?);
                    }
                }
            } else if let Some(s) = args.get("scope").and_then(Value::as_str) {
                parsed_scopes.push(StateScope::from_wire(s).map_err(|e| e.to_string())?);
            }

            let (areas, agents, mut entries) = {
                let ws_store = env.open_for_scope(Some(&StateScope::Workspace))?;
                let areas = ws_store.list_areas().map_err(|e| e.to_string())?;
                let agents = ws_store.list_agents(None, false).map_err(|e| e.to_string())?;
                let initial_entries = if parsed_scopes.is_empty() {
                    ws_store.list_state(None, None).map_err(|e| e.to_string())?
                } else {
                    Vec::new()
                };
                (areas, agents, initial_entries)
            };
            for sc in &parsed_scopes {
                let st = env.open_for_scope(Some(sc))?;
                entries.extend(st.list_state(Some(sc), None).map_err(|e| e.to_string())?);
            }
            Ok(render_state_markdown(
                &areas,
                &agents,
                &entries,
                max_bytes,
                include_sensitive,
            ))
        }
        "state_get" => {
            let id_or_key = req_str(args, "id_or_key")?;
            let parsed_scope = args
                .get("scope")
                .and_then(Value::as_str)
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let parsed_concern: Option<StateConcern> = args
                .get("concern")
                .and_then(Value::as_str)
                .map(str::parse)
                .transpose()?;
            let history = args
                .get("history")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let store = env.open_for_id_or_key(id_or_key, parsed_scope.as_ref())?;
            let entry =
                resolve_state_in_store(&store, id_or_key, parsed_scope.as_ref(), parsed_concern)?;
            if history {
                let revs = store
                    .state_revisions(&entry.id)
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::to_string_pretty(&json!({
                    "entry": entry,
                    "revisions": revs,
                }))
                .unwrap())
            } else {
                Ok(serde_json::to_string_pretty(&entry).unwrap())
            }
        }
        "state_list" => {
            let parsed_scope = args
                .get("scope")
                .and_then(Value::as_str)
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let parsed_concern: Option<StateConcern> = args
                .get("concern")
                .and_then(Value::as_str)
                .map(str::parse)
                .transpose()?;
            let parsed_cat: Option<MemoryCategory> = args
                .get("category")
                .and_then(Value::as_str)
                .map(str::parse)
                .transpose()?;
            let include_retired = args
                .get("include_retired")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let entries: Vec<StateEntry> = store
                .list_state(parsed_scope.as_ref(), parsed_concern)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|e| match &e.payload {
                    StatePayload::Memory(m) => {
                        (include_retired || m.lifecycle == MemoryLifecycle::Active)
                            && parsed_cat.is_none_or(|c| m.category == c)
                    }
                    _ => parsed_cat.is_none(),
                })
                .collect();
            Ok(serde_json::to_string_pretty(&entries).unwrap())
        }
        "state_set_memory" => {
            let scope = StateScope::from_wire(req_str(args, "scope")?).map_err(|e| e.to_string())?;
            let key = req_str(args, "key")?.to_string();
            let by = req_str(args, "by")?.to_string();
            let lifecycle: MemoryLifecycle = match args.get("lifecycle").and_then(Value::as_str) {
                Some(s) => s.parse()?,
                None => MemoryLifecycle::Active,
            };
            let store = env.open_for_scope(Some(&scope))?;
            if matches!(lifecycle, MemoryLifecycle::Retired | MemoryLifecycle::Superseded)
                && args.get("summary").is_none()
            {
                let sup_id = match args.get("superseded_by").and_then(Value::as_str) {
                    Some(raw_sup) => {
                        let sup_entry = resolve_state_in_store(
                            &store,
                            raw_sup,
                            Some(&scope),
                            Some(StateConcern::Memory),
                        )?;
                        Some(sup_entry.id)
                    }
                    None => None,
                };
                let updated = store
                    .retire_memory(&scope, &key, lifecycle, sup_id, At::now(), &by)
                    .map_err(|e| e.to_string())?;
                return Ok(serde_json::to_string_pretty(&updated).unwrap());
            }

            let category: MemoryCategory = req_str(args, "category")?.parse()?;
            let basis: MemoryBasis = req_str(args, "basis")?.parse()?;
            let summary = req_str(args, "summary")?.to_string();
            let body = req_str(args, "body")?.to_string();
            let pinned = args.get("pinned").and_then(Value::as_bool).unwrap_or(false);
            let superseded_by = match args.get("superseded_by").and_then(Value::as_str) {
                Some(raw_sup) => {
                    let sup_entry = resolve_state_in_store(
                        &store,
                        raw_sup,
                        Some(&scope),
                        Some(StateConcern::Memory),
                    )?;
                    Some(sup_entry.id)
                }
                None => None,
            };
            let entry = store
                .upsert_state(UpsertStateArgs {
                    scope,
                    slug: key,
                    sensitivity: parse_sensitivity(args),
                    payload: StatePayload::Memory(MemoryEntry {
                        category,
                        basis,
                        lifecycle,
                        superseded_by,
                        pinned,
                        summary,
                        body,
                    }),
                    at: At::now(),
                    by,
                })
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&entry).unwrap())
        }
        "state_set_handoff" => {
            let scope = StateScope::from_wire(req_str(args, "scope")?).map_err(|e| e.to_string())?;
            let key = args
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("current")
                .to_string();
            let summary = req_str(args, "summary")?.to_string();
            let by = req_str(args, "by")?.to_string();
            let str_vec = |field: &str| -> Vec<String> {
                args.get(field)
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let shipped = str_vec("shipped");
            let in_flight = str_vec("in_flight");
            let blockers = str_vec("blockers");
            let next_steps = str_vec("next_steps");
            let notes = args
                .get("notes")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let store = env.open_for_scope(Some(&scope))?;
            let entry = store
                .upsert_state(UpsertStateArgs {
                    scope,
                    slug: key,
                    sensitivity: parse_sensitivity(args),
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
            Ok(serde_json::to_string_pretty(&entry).unwrap())
        }
        "state_set_runtime" => {
            let scope = StateScope::from_wire(req_str(args, "scope")?).map_err(|e| e.to_string())?;
            let key = req_str(args, "key")?.to_string();
            let by = req_str(args, "by")?.to_string();
            let value = args
                .get("value")
                .cloned()
                .ok_or_else(|| "missing required argument `value`".to_string())?;
            let merge_mode: MergeMode = match args.get("merge_mode").and_then(Value::as_str) {
                Some(m) => m.parse()?,
                None => MergeMode::Conflict,
            };
            let store = env.open_for_scope(Some(&scope))?;
            let entry = store
                .upsert_state(UpsertStateArgs {
                    scope,
                    slug: key,
                    sensitivity: parse_sensitivity(args),
                    payload: StatePayload::Runtime(RuntimeEntry { value, merge_mode }),
                    at: At::now(),
                    by,
                })
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&entry).unwrap())
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "agent_roster",
            "description": "List registered agents (and derived reports_to) from the workspace state store.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "include_retired": { "type": "boolean" }
                }
            }
        },
        {
            "name": "agent_register",
            "description": "Register or resume an agent identity in the workspace state store.",
            "inputSchema": {
                "type": "object",
                "required": ["name", "project", "role", "by"],
                "properties": {
                    "name": { "type": "string" },
                    "project": { "type": "string" },
                    "role": { "type": "string" },
                    "email": { "type": "string" },
                    "extra_areas": { "type": "array", "items": { "type": "string" } },
                    "create_area": { "type": "boolean" },
                    "replace": { "type": "boolean" },
                    "by": { "type": "string" }
                }
            }
        },
        {
            "name": "state_render",
            "description": "Render the deterministic read-only startup Markdown summary.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scopes": { "type": "array", "items": { "type": "string" } },
                    "max_bytes": { "type": "integer" },
                    "include_sensitive": { "type": "boolean" }
                }
            }
        },
        {
            "name": "state_get",
            "description": "Fetch a single state entry (and optional revision history) by IRI, handle, or slug.",
            "inputSchema": {
                "type": "object",
                "required": ["id_or_key"],
                "properties": {
                    "id_or_key": { "type": "string" },
                    "scope": { "type": "string" },
                    "concern": { "type": "string" },
                    "history": { "type": "boolean" }
                }
            }
        },
        {
            "name": "state_list",
            "description": "List state entries filtered by scope, concern, or memory category.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": { "type": "string" },
                    "concern": { "type": "string" },
                    "category": { "type": "string" },
                    "include_retired": { "type": "boolean" }
                }
            }
        },
        {
            "name": "state_set_memory",
            "description": "Create, update, retire, or supersede a durable Memory entry.",
            "inputSchema": {
                "type": "object",
                "required": ["scope", "key", "by"],
                "properties": {
                    "scope": { "type": "string" },
                    "key": { "type": "string" },
                    "category": { "type": "string" },
                    "basis": { "type": "string" },
                    "lifecycle": { "type": "string" },
                    "superseded_by": { "type": "string" },
                    "pinned": { "type": "boolean" },
                    "summary": { "type": "string" },
                    "body": { "type": "string" },
                    "sensitive": { "type": "boolean" },
                    "clear_sensitive": { "type": "boolean" },
                    "by": { "type": "string" }
                }
            }
        },
        {
            "name": "state_set_handoff",
            "description": "Create or update a Handoff status snapshot.",
            "inputSchema": {
                "type": "object",
                "required": ["scope", "summary", "by"],
                "properties": {
                    "scope": { "type": "string" },
                    "key": { "type": "string" },
                    "summary": { "type": "string" },
                    "shipped": { "type": "array", "items": { "type": "string" } },
                    "in_flight": { "type": "array", "items": { "type": "string" } },
                    "blockers": { "type": "array", "items": { "type": "string" } },
                    "next_steps": { "type": "array", "items": { "type": "string" } },
                    "notes": { "type": "string" },
                    "sensitive": { "type": "boolean" },
                    "clear_sensitive": { "type": "boolean" },
                    "by": { "type": "string" }
                }
            }
        },
        {
            "name": "state_set_runtime",
            "description": "Create or update a structured JSON Runtime entry.",
            "inputSchema": {
                "type": "object",
                "required": ["scope", "key", "value", "by"],
                "properties": {
                    "scope": { "type": "string" },
                    "key": { "type": "string" },
                    "value": {},
                    "merge_mode": { "type": "string" },
                    "sensitive": { "type": "boolean" },
                    "clear_sensitive": { "type": "boolean" },
                    "by": { "type": "string" },
                }
            }
        }
    ])
}
```

- [ ] **Step 2: Register `pub mod state_mcp;` in `crates/cli/src/cmd/mod.rs` and wire `StateCmd::Mcp` in `crates/cli/src/cmd/state.rs`**

1. Add `pub mod state_mcp;` to `crates/cli/src/cmd/mod.rs`.
2. In `crates/cli/src/cmd/state.rs`, replace the `StateCmd::Mcp` arm with:

```rust
        StateCmd::Mcp => crate::cmd::state_mcp::run(env),
```

- [ ] **Step 3: Create `crates/cli/tests/state_sync.rs` covering all 9 Spec §6.3 black-box scenarios**

Create `crates/cli/tests/state_sync.rs`:

```rust
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

struct Machine {
    cfg_dir: PathBuf,
    data_dir: PathBuf,
    repo_dir: PathBuf,
    store_path: PathBuf,
}

impl Machine {
    fn fl(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fl"));
        cmd.current_dir(&self.repo_dir)
            .env("XDG_CONFIG_HOME", &self.cfg_dir)
            .env("XDG_DATA_HOME", &self.data_dir)
            .env_remove("FL_DB");
        cmd
    }
}

fn setup_two_machines(private_remote: bool) -> (TempDir, PathBuf, Machine, Machine) {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("remote.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", bare.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );

    let make_machine = |label: &str| -> Machine {
        let root = tmp.path().join(label);
        let cfg_dir = root.join("config");
        let data_dir = root.join("data");
        let repo_dir = root.join("repo");
        let store_path = data_dir.join("fl/fl.redb");
        std::fs::create_dir_all(cfg_dir.join("fl")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&repo_dir).unwrap();

        assert!(
            Command::new("git")
                .args(["init", "-b", "main", repo_dir.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo_dir)
                .args(["remote", "add", "origin", bare.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo_dir.join("README.md"), "# Working Tree\n").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo_dir)
                .args(["add", "README.md"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo_dir)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@fl.local")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@fl.local")
                .args(["commit", "-m", "initial"])
                .status()
                .unwrap()
                .success()
        );

        std::fs::write(
            cfg_dir.join("fl/config.toml"),
            format!(
                "[state]\nstore = \"{}\"\nrepo = \"{}\"\nremote = \"origin\"\nprivate = {}\n",
                store_path.display(),
                repo_dir.display(),
                private_remote,
            ),
        )
        .unwrap();

        Machine {
            cfg_dir,
            data_dir,
            repo_dir,
            store_path,
        }
    };

    let a = make_machine("machine_a");
    let b = make_machine("machine_b");
    (tmp, bare, a, b)
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
fn clean_merge_and_noop_push_skip_across_two_machines() {
    let (_tmp, _bare, a, b) = setup_two_machines(false);

    let mut cmd_a = a.fl();
    cmd_a.args([
        "state",
        "memory",
        "set",
        "--scope",
        "workspace",
        "--key",
        "m-a",
        "--category",
        "rule",
        "--basis",
        "ruling",
        "--summary",
        "Rule from A",
        "--body",
        "Details A",
        "--by",
        "jarvis",
        "--json",
    ]);
    let mem_a = run_json(cmd_a);

    let mut cmd_b = b.fl();
    cmd_b.args([
        "state",
        "handoff",
        "set",
        "--scope",
        "workspace",
        "--key",
        "h-b",
        "--summary",
        "Handoff from B",
        "--shipped",
        "task-1",
        "--by",
        "ferris",
        "--json",
    ]);
    let handoff_b = run_json(cmd_b);

    let mut sync_a1 = a.fl();
    sync_a1.args(["state", "sync", "--by", "jarvis", "--json"]);
    let rep_a1 = run_json(sync_a1);
    assert_eq!(rep_a1["published_revisions"], 1);

    let mut sync_b1 = b.fl();
    sync_b1.args(["state", "sync", "--by", "ferris", "--json"]);
    let rep_b1 = run_json(sync_b1);
    assert_eq!(rep_b1["imported_revisions"], 1);
    assert_eq!(rep_b1["published_revisions"], 1);
    assert_eq!(rep_b1["active_conflicts"], 0);

    let mut sync_a2 = a.fl();
    sync_a2.args(["state", "sync", "--by", "jarvis", "--json"]);
    let rep_a2 = run_json(sync_a2);
    assert_eq!(rep_a2["published_revisions"], 0);
    assert_eq!(rep_a2["committed_oid"], rep_b1["committed_oid"]);

    // Second sync with no local writes creates no new commit.
    let mut sync_a3 = a.fl();
    sync_a3.args(["state", "sync", "--by", "jarvis", "--json"]);
    let rep_a3 = run_json(sync_a3);
    assert_eq!(rep_a3["committed_oid"], rep_b1["committed_oid"]);

    // Both entries exist on Machine A.
    let mut list_a = a.fl();
    list_a.args(["state", "list", "--json"]);
    let all_a = run_json(list_a);
    let ids: Vec<&str> = all_a
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&mem_a["id"].as_str().unwrap()));
    assert!(ids.contains(&handoff_b["id"].as_str().unwrap()));
}

#[test]
fn working_tree_and_index_remain_untouched_during_sync() {
    let (_tmp, _bare, a, _b) = setup_two_machines(false);
    let index_before = std::fs::read(a.repo_dir.join(".git/index")).unwrap();
    let head_before = Command::new("git")
        .arg("-C")
        .arg(&a.repo_dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap()
        .stdout;

    let mut set = a.fl();
    set.args([
        "state",
        "handoff",
        "set",
        "--scope",
        "workspace",
        "--summary",
        "Check isolation",
        "--by",
        "ferris",
        "--json",
    ]);
    let _ = run_json(set);

    let mut sync = a.fl();
    sync.args(["state", "sync", "--by", "ferris", "--json"]);
    let _ = run_json(sync);

    let index_after = std::fs::read(a.repo_dir.join(".git/index")).unwrap();
    let head_after = Command::new("git")
        .arg("-C")
        .arg(&a.repo_dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap()
        .stdout;
    let status_out = Command::new("git")
        .arg("-C")
        .arg(&a.repo_dir)
        .args(["status", "--porcelain"])
        .output()
        .unwrap()
        .stdout;

    assert_eq!(index_before, index_after);
    assert_eq!(head_before, head_after);
    assert!(status_out.is_empty());
}

#[test]
fn same_key_conflict_blocks_writes_and_resolves_cleanly() {
    let (_tmp, _bare, a, b) = setup_two_machines(false);

    // Base memory synced to both A and B.
    let mut base_cmd = a.fl();
    base_cmd.args([
        "state", "memory", "set", "--scope", "workspace", "--key", "shared-rule",
        "--category", "rule", "--basis", "ruling", "--summary", "Base", "--body", "v0",
        "--by", "jarvis", "--json",
    ]);
    let _ = run_json(base_cmd);
    let mut s1 = a.fl();
    s1.args(["state", "sync", "--json"]);
    let _ = run_json(s1);
    let mut s2 = b.fl();
    s2.args(["state", "sync", "--json"]);
    let _ = run_json(s2);

    // Concurrent divergent edits on A and B.
    let mut edit_a = a.fl();
    edit_a.args([
        "state", "memory", "set", "--scope", "workspace", "--key", "shared-rule",
        "--category", "rule", "--basis", "ruling", "--summary", "From A", "--body", "vA",
        "--by", "jarvis", "--json",
    ]);
    let entry_a = run_json(edit_a);

    let mut edit_b = b.fl();
    edit_b.args([
        "state", "memory", "set", "--scope", "workspace", "--key", "shared-rule",
        "--category", "rule", "--basis", "ruling", "--summary", "From B", "--body", "vB",
        "--by", "ferris", "--json",
    ]);
    let _entry_b = run_json(edit_b);

    let mut push_a = a.fl();
    push_a.args(["state", "sync", "--json"]);
    let _ = run_json(push_a);

    let mut push_b = b.fl();
    push_b.args(["state", "sync", "--json"]);
    let rep_b = run_json(push_b);
    assert_eq!(rep_b["active_conflicts"], 1);

    // Regular writes on B are refused until conflict is resolved.
    let mut blocked = b.fl();
    blocked.args([
        "state", "memory", "set", "--scope", "workspace", "--key", "shared-rule",
        "--category", "rule", "--basis", "ruling", "--summary", "Blocked", "--body", "vX",
        "--by", "ferris",
    ]);
    assert!(!blocked.output().unwrap().status.success());

    // Resolve keeping A's head_revision and sync.
    let keep_rev = entry_a["head_revision"].as_str().unwrap();
    let mut resolve = b.fl();
    resolve.args([
        "state", "resolve", "shared-rule", "--scope", "workspace", "--concern", "memory",
        "--keep", keep_rev, "--by", "ferris", "--json",
    ]);
    let resolved = run_json(resolve);
    assert_eq!(resolved["payload"]["summary"], "From A");

    let mut sync_b_final = b.fl();
    sync_b_final.args(["state", "sync", "--json"]);
    let rep_b_final = run_json(sync_b_final);
    assert_eq!(rep_b_final["active_conflicts"], 0);
}

#[test]
fn offline_same_key_state_id_collision_aliases_to_remote() {
    let (_tmp, _bare, a, b) = setup_two_machines(false);

    let mut create_a = a.fl();
    create_a.args([
        "state", "runtime", "set", "--scope", "workspace", "--key", "lease",
        "--value", "{\"holder\":\"a\"}", "--merge", "last_writer", "--by", "jarvis", "--json",
    ]);
    let entry_a = run_json(create_a);

    let mut create_b = b.fl();
    create_b.args([
        "state", "runtime", "set", "--scope", "workspace", "--key", "lease",
        "--value", "{\"holder\":\"b\"}", "--merge", "last_writer", "--by", "ferris", "--json",
    ]);
    let entry_b = run_json(create_b);
    assert_ne!(entry_a["id"], entry_b["id"]);

    let mut sync_a = a.fl();
    sync_a.args(["state", "sync", "--json"]);
    let _ = run_json(sync_a);

    let mut sync_b = b.fl();
    sync_b.args(["state", "sync", "--json"]);
    let _ = run_json(sync_b);

    // Querying B by its old local StateId resolves via alias to A's canonical StateId.
    let old_b_id = entry_b["id"].as_str().unwrap();
    let mut get_b = b.fl();
    get_b.args(["state", "get", old_b_id, "--json"]);
    let resolved_b = run_json(get_b);
    assert_eq!(resolved_b["id"], entry_a["id"]);
}

#[test]
fn runtime_append_and_last_writer_merge_modes_converge_without_conflict() {
    let (_tmp, _bare, a, b) = setup_two_machines(false);

    let mut base = a.fl();
    base.args([
        "state", "runtime", "set", "--scope", "workspace", "--key", "events",
        "--value", "[\"init\"]", "--merge", "append", "--by", "jarvis", "--json",
    ]);
    let _ = run_json(base);
    let mut s1 = a.fl();
    s1.args(["state", "sync", "--json"]);
    let _ = run_json(s1);
    let mut s2 = b.fl();
    s2.args(["state", "sync", "--json"]);
    let _ = run_json(s2);

    let mut up_a = a.fl();
    up_a.args([
        "state", "runtime", "set", "--scope", "workspace", "--key", "events",
        "--value", "[\"init\",\"from-a\"]", "--merge", "append", "--by", "jarvis", "--json",
    ]);
    let _ = run_json(up_a);

    let mut up_b = b.fl();
    up_b.args([
        "state", "runtime", "set", "--scope", "workspace", "--key", "events",
        "--value", "[\"init\",\"from-b\"]", "--merge", "append", "--by", "ferris", "--json",
    ]);
    let _ = run_json(up_b);

    let mut push_a = a.fl();
    push_a.args(["state", "sync", "--json"]);
    let _ = run_json(push_a);

    let mut push_b = b.fl();
    push_b.args(["state", "sync", "--json"]);
    let rep_b = run_json(push_b);
    assert_eq!(rep_b["active_conflicts"], 0);

    let mut get_b = b.fl();
    get_b.args([
        "state", "get", "events", "--scope", "workspace", "--concern", "runtime", "--json",
    ]);
    let merged = run_json(get_b);
    assert_eq!(
        merged["payload"]["value"],
        serde_json::json!(["init", "from-b", "from-a"])
    );
}

#[test]
fn tamper_and_branch_deletion_are_refused_by_sync_and_verify() {
    let (_tmp, bare, a, _b) = setup_two_machines(false);

    let mut set = a.fl();
    set.args([
        "state", "handoff", "set", "--scope", "workspace", "--summary", "Untampered",
        "--by", "jarvis", "--json",
    ]);
    let _ = run_json(set);

    let mut sync = a.fl();
    sync.args(["state", "sync", "--json"]);
    let _ = run_json(sync);

    // Delete refs/heads/fl/state on the bare remote after a prior sync -> verify & sync fail.
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&bare)
            .args(["branch", "-D", "fl/state"])
            .status()
            .unwrap()
            .success()
    );

    let mut sync_after_del = a.fl();
    sync_after_del.args(["state", "sync"]);
    let out = sync_after_del.output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn sensitive_entry_is_refused_on_non_private_remote() {
    let (_tmp, bare, a, _b) = setup_two_machines(false);

    let mut set = a.fl();
    set.args([
        "state", "runtime", "set", "--scope", "workspace", "--key", "secret-key",
        "--value", "{\"token\":\"redact\"}", "--sensitive", "--by", "jarvis", "--json",
    ]);
    let _ = run_json(set);

    let mut sync = a.fl();
    sync.args(["state", "sync"]);
    let out = sync.output().unwrap();
    assert!(!out.status.success());

    let ls = Command::new("git")
        .arg("-C")
        .arg(&bare)
        .args(["ls-remote", "--refs", ".", "refs/heads/fl/state"])
        .output()
        .unwrap();
    assert!(ls.stdout.is_empty());
}

#[test]
fn render_orders_tiers_respects_byte_cap_omits_sensitive_and_refuses_symlinks() {
    let (tmp, _bare, a, _b) = setup_two_machines(false);

    // Register agent and verify exit code 2 on collision without --replace.
    let mut reg1 = a.fl();
    reg1.args([
        "agent", "register", "Ferris", "--project", "ferroloop", "--role", "lead",
        "--create-area", "--by", "owner", "--json",
    ]);
    let _ = run_json(reg1);

    let mut reg_collision = a.fl();
    reg_collision.args([
        "agent", "register", "Ferris", "--project", "ferroloop", "--role", "lead",
        "--by", "owner",
    ]);
    assert_eq!(reg_collision.output().unwrap().status.code(), Some(2));

    // Add normal, warning, and sensitive memories.
    let mut m_note = a.fl();
    m_note.args([
        "state", "memory", "set", "--scope", "workspace", "--key", "aaa-note",
        "--category", "note", "--basis", "measured", "--summary", "Normal note",
        "--body", "body", "--by", "ferris", "--json",
    ]);
    let _ = run_json(m_note);

    let mut m_warn = a.fl();
    m_warn.args([
        "state", "memory", "set", "--scope", "workspace", "--key", "zzz-warn",
        "--category", "warning", "--basis", "ruling", "--summary", "Critical warning",
        "--body", "body", "--by", "ferris", "--json",
    ]);
    let _ = run_json(m_warn);

    let mut m_sens = a.fl();
    m_sens.args([
        "state", "memory", "set", "--scope", "workspace", "--key", "secret-mem",
        "--category", "warning", "--basis", "ruling", "--summary", "Hidden summary",
        "--body", "body", "--sensitive", "--by", "ferris", "--json",
    ]);
    let _ = run_json(m_sens);

    let mut render = a.fl();
    render.args(["state", "render", "--max-bytes", "600"]);
    let out = render.output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.len() <= 600, "len {} > 600:\n{text}", text.len());
    assert!(!text.contains("Hidden summary"));
    assert!(text.find("zzz-warn").unwrap() < text.find("aaa-note").unwrap());

    // Refuse symlinked --out path.
    let real_target = tmp.path().join("real.md");
    std::fs::write(&real_target, "orig").unwrap();
    let symlink_path = tmp.path().join("symlink.md");
    std::os::unix::fs::symlink(&real_target, &symlink_path).unwrap();

    let mut render_sym = a.fl();
    render_sym.args(["state", "render", "--out", symlink_path.to_str().unwrap()]);
    assert!(!render_sym.output().unwrap().status.success());
}

#[test]
fn mcp_stdio_round_trip_and_releases_redb_lock_between_calls() {
    let (_tmp, _bare, a, _b) = setup_two_machines(false);

    let mut child = a
        .fl()
        .args(["state", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let mut child_in = child.stdin.take().unwrap();
    let mut child_out = BufReader::new(child.stdout.take().unwrap());

    let mut rpc = |req: Value| -> Value {
        writeln!(child_in, "{}", serde_json::to_string(&req).unwrap()).unwrap();
        child_in.flush().unwrap();
        let mut line = String::new();
        child_out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    let init_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    }));
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "fl-state-mcp");

    let list_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    assert_eq!(list_resp["result"]["tools"].as_array().unwrap().len(), 8);

    let reg_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "agent_register",
            "arguments": {
                "name": "Ferris",
                "project": "ferroloop",
                "role": "lead",
                "create_area": true,
                "by": "owner"
            }
        }
    }));
    assert_eq!(reg_resp["result"]["isError"], false);

    let mem_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "state_set_memory",
            "arguments": {
                "scope": "workspace",
                "key": "mcp-rule",
                "category": "rule",
                "basis": "ruling",
                "summary": "Written via MCP",
                "body": "MCP body",
                "pinned": true,
                "by": "ferris"
            }
        }
    }));
    assert_eq!(mem_resp["result"]["isError"], false);

    // While `fl state mcp` is alive and waiting on stdin, a separate `fl state list`
    // process must succeed without blocking on the redb file lock.
    let mut cli_list = a.fl();
    cli_list.args(["state", "list", "--json"]);
    let listed = run_json(cli_list);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let render_resp = rpc(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "state_render",
            "arguments": {}
        }
    }));
    assert_eq!(render_resp["result"]["isError"], false);
    assert!(
        render_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Written via MCP")
    );

    drop(child_in);
    let status = child.wait().unwrap();
    assert!(status.success());
}
```

- [ ] **Step 4: Run the full workspace test suite and clippy**

Run:
```bash
cargo test --workspace -- --nocapture
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: all unit, conformance, and black-box integration tests across `fl-core`, `fl-store`, `fl-exec`, `fl-github`, `fl-mcp`, and `fl-cli` pass with zero warnings.

- [ ] **Step 5: Commit Task 7**

```bash
git add crates/cli/src/cmd/state_mcp.rs crates/cli/src/cmd/mod.rs crates/cli/src/cmd/state.rs crates/cli/tests/state_sync.rs
git commit -m "$(cat <<'EOF'
feat(cli): add fl state mcp stdio server and end-to-end state sync tests

Implement the newline-delimited JSON-RPC 2.0 MCP server (fl state mcp)
with per-call RedbStore open/drop and add the black-box CLI and
multi-machine git sync test suite.

Co-authored-by: Ferris <ferris@artificialhumanity.io>
EOF
)"
```
