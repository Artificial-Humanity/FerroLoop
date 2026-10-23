# State database and agent registry — design

**Date:** 2026-10-22
**Status:** Drafted from the owner's 2026-10-22 brainstorming decisions; awaiting owner spec
review. Sequenced first in the post-`Prove` build order:
1. **State Database and Agent Registry** (this specification)
2. **FerroWire (Messaging and Carriers)**
3. **Adapters and Telemetry**
4. **Workflow Graph (Flow)**
5. **Hub and Command-Center PWA**

**Scope:** A two-tier state database (`fl state`) for durable project and agent memory,
status handoffs, and live workflow/runtime state, together with the centralized agent and
role registry (`fl agent`, activating `2026-09-23-identity-and-store-roles-design.md` §6a),
persisted locally in `fl-store` (format 6), synced across machines over any standard git
remote on an orphan `fl/state` branch without touching the working tree, and exposed through
the `fl` CLI, a stdio MCP server, and read-only Markdown exports.

---

## Reading this document

Constraints carry a status tag, as in the identity specification:

| tag | meaning |
|---|---|
| **Invariant** | Must hold in any version. Violating it breaks the product's premise. |
| **Release scope** | True of this sub-project's release. A later version can change it. |
| **Open** | Undecided. Named here so it is not decided by accident. |

An untagged statement is descriptive, not binding. `[agent]` marks a ruling the resident
agent made where the owner's decisions left a choice; the owner may reverse any of them.

---

## 0. Where this sits

Until now, FerroLoop has persisted gates, transitions, records, findings, gate runs, and
attempts (`Catalog`, `Tracker`, and `Ledger`), while agents and projects kept their working
memory, handoff state, and agent roster in hand-edited Markdown files (`MEMORY.md`,
`index-*.md`, `STATE.md`, and per-directory `PERSONA.md`). Across sessions and machines,
those files drift:

* **Static per-directory personas break multi-agent projects.** Tying an agent's identity to
  a repository's `PERSONA.md` assumes one agent per project. When assisting agents join a
  project, sessions need manual prompt overrides to ignore `PERSONA.md`, and tools that infer
  identity from the working directory misroute messages.
* **Markdown handoff and memory files grow without structure.** Topic files turn into
  multi-kilobyte logs, index descriptions drift from the files they summarize, retired facts
  stay in place without a pointer to what replaced them, and concurrent edits across
  worktrees risk overwriting another session's state.
* **Workflows and runtime tools have no shared state store.** Upcoming workflow graphs and
  cross-machine coordination need structured JSON state with explicit merge rules (`conflict`,
  `append`, `last_writer`) rather than ad-hoc files.

This sub-project adds two store roles to `fl-core` and `fl-store` — `StateStore` and
`AgentRegistry` — and pairs the local `redb` store with standard-git synchronization on an
orphan `fl/state` branch.

### 0.1 Owner decisions this design rests on (2026-10-22)

1. **Unified state database scope:** one state database holds both durable project and agent
   memory (replacing Markdown handoff files such as `STATE.md` and `MEMORY.md`) and live
   workflow and runtime state.
2. **Two-tier residency:** the state database uses a local embedded store (`fl-store`) on one
   machine, paired with the hub or a standard git remote across machines.
3. **Standard git remotes as carrier:** any standard git remote (GitHub, Forgejo, or bare SSH
   git) may carry durable state and messages without depending on GitHub-only APIs.
4. **CLI and MCP as authority, plus read-only export:** all writes go through `fl state` /
   `fl agent` or MCP tools, and `fl` renders read-only Markdown views (`fl state render` and
   `fl agent roster --render`) when a session or repository needs a file or startup context
   block.
5. **Hierarchical scopes, configurable git remote, and sensitivity guard:** entries carry a
   hierarchical scope (`workspace`, `group`, `project`, `agent`, `workflow`), can sync to a
   configured private git repository separate from a public code repository, and refuse
   non-private remotes when marked sensitive.
6. **Revision log plus materialized current view, with conflict detection:** every write
   records an authored revision; distinct keys merge automatically across machines; same-key
   concurrent edits record a conflict (or merge automatically when a runtime entry declares
   `append` or `last_writer`).
7. **Three-concern hybrid model (`Memory`, `Handoff`, `Runtime`):** durable knowledge
   (`Memory`, with a required one-line summary for capped startup export and an on-demand
   body), status snapshots (`Handoff`, replacing `STATE.md`), and machine workflow state
   (`Runtime`, structured JSON with a merge rule) are distinct concerns sharing one revision
   and sync engine.
8. **Clean store in Sub-project 1:** this release builds `fl state` and `fl agent` CRUD,
   queries, read-only exports, MCP tools, and standard-git sync; a bulk Markdown importer
   (`fl state import`) for legacy `.agents/memory/` and `STATE.md` files is deferred to a
   follow-up.
9. **Centralized, fluid agent identity, project assignment, and role hierarchy in Format 6:**
   Sub-project 1 activates the §6a Agent Registry alongside the State Database in a single
   `fl-store` format bump (format 6). Project domain expertise (`PERSONA.md`) is separated
   from role authority (`lead`, `assistant`, `lab_manager`, or project-specific custom roles)
   and from startup agent registration (`fl agent register <Name> --project <Project> --role
   <role>`).

### 0.2 Out of scope

* **Importing existing Markdown memory and `STATE.md` trees** (`fl state import`) — deferred
  per decision 8. *(Release scope)*
* **Live hub push sync for state** — in this release, cross-machine sync runs over standard
  git remotes (`fl state sync`); live WebSocket hub replication arrives with the Hub
  sub-project. *(Release scope)*
* **Agent-to-agent message delivery (`fl wire`)** — the Agent Registry built here provides the
  identities, addresses, and roles that FerroWire will route over in Sub-project 2. *(Release
  scope)*

---

## 1. Domain model, identity, scopes, and the three concerns (`fl-core`)

### 1.1 Kinds and identifiers

`crates/core/src/ids.rs` adds two variants to `Kind`:

* `Kind::State` (`"state"`), wrapped by `StateId(pub Iri)`. *(Invariant)*
* `Kind::Agent` (`"agent"`), wrapped by `AgentId(pub Iri)`. This activates the reservation in
  `2026-09-23-identity-and-store-roles-design.md` §6a. *(Invariant)*

* **Store-local numeric handles (`Handles`):** per
  `2026-09-23-identity-and-store-roles-design.md` §4, `Handles` assigns bare `u64` numbers per
  `Kind` (`1`, `2`, …) resolved via `store.resolve_handle(Kind::State, n)` and
  `store.resolve_handle(Kind::Agent, n)`. Handles are strictly store-local; only full IRIs
  search across configured stores. For `AgentId`, in addition to its numeric `Handles` entry
  under `Kind::Agent`, its `name_key` resolves case-insensitively to `AgentId` via
  `agent_by_name` (§6a). *(Invariant)*
* **Revision identifiers (`RevisionId`):** every state mutation also mints a
  `RevisionId(pub Iri)` (`urn:uuid:` v7). `[agent]` Like `GateRun` and `Attempt` entry IDs on
  the ledger, `RevisionId` is an immutable IRI that names one revision in history and on
  `fl/state`, but it is not registered in `HANDLES` (so rapid runtime checkpoints do not churn
  short handles).
* **Exhaustive `Kind` routing:** both `Kind::State` and `Kind::Agent` are added to
  `wire_names!` and `wire_parse!`, and every exhaustive match on `Kind` (including
  `KindRouted` and `RedbStore::check`) is extended to handle them. Specifically,
  `KindRouted::owner` routes `Kind::State | Kind::Agent` to `self.catalog` (the local store).
  *(Invariant)*

### 1.2 Hierarchical scopes (`StateScope`)

Every `StateEntry` belongs to one `StateScope`:

```rust
pub enum StateScope {
    Workspace,
    Group(String),
    Project(String),
    Agent(String),
    Workflow(String),
}
```

* **Slug normalization (leading ASCII letter):** the inner string of `Group`, `Project`,
  `Agent`, and `Workflow` (as well as every state entry `slug`, `area_key`, `name_key`, and
  custom role slug) is normalized to ASCII lowercase on parse. It must be 1 to 64 bytes of
  ASCII lowercase letters, digits, `-`, or `_`, **starting with an ASCII lowercase letter**
  (`^[a-z][a-z0-9_-]{0,63}$`), never a digit. Because slugs and `name_key`s always start with
  `[a-z]`, bare digits (`1`, `42`) parse unambiguously as `Ref::Handle(u64)`, `urn:uuid:...`
  parses unambiguously as `Ref::Iri`, and `[a-z]...` parses unambiguously as a `slug` or
  `name_key`. Empty strings, leading digits, path separators (`/` or `\`), `..`, whitespace,
  and control characters are refused with `StateError::InvalidSlug`. *(Invariant)*
* **Canonical wire spelling:** `StateScope` serializes as a flat string in JSON, CLI flags, and
  store records (`"workspace"`, `"group:<slug>"`, `"project:<slug>"`, `"agent:<name_key>"`,
  `"workflow:<slug>"`) via custom `Serialize`/`Deserialize` + `as_wire`/`from_wire` (matching
  `Iri`'s custom string serde pattern). `--scope project:<slug>` takes the normalized
  `<slug>` directly (matching `^[a-z][a-z0-9_-]{0,63}$`), so a state store can hold project
  state before or without a gate catalog. *(Invariant)*
* **Entry keys (`slug`):** within a `(StateScope, StateConcern)` pair, each entry has a human
  key (`slug`) obeying the same `^[a-z][a-z0-9_-]{0,63}$` slug rule. For `Handoff`, `--key`
  defaults to `"current"` when omitted, so a project or agent normally has one canonical
  handoff snapshot at `(scope, handoff, "current")` while still allowing named handoffs when
  needed. `[agent]` *(Release scope)*

### 1.3 Sensitivity (`sensitive: bool`)

Every `StateEntry` carries `sensitive: bool`:

* **Fail-closed sticky flag:** once an entry is created or updated with `sensitive: true`,
  subsequent updates keep `sensitive: true` unless the caller explicitly passes
  `--not-sensitive` (CLI) or `"clear_sensitive": true` (MCP). Omitting the flag on an update
  never clears sensitivity. *(Invariant — mirrors routing decision 22)*
* **Revision inheritance:** if any revision in an entry's history carried `sensitive: true`,
  `fl state sync` treats the entry's entire revision stream as sensitive when checking whether
  a remote may receive it (§4.3), because pushing an entry's revision segment pushes its
  prior revisions too. `[agent]` Clearing `sensitive` on the current view allows non-private
  read-only rendering (`fl state render`), while the raw revision history remains blocked from
  non-private git remotes. *(Invariant)*

### 1.4 The three concerns (`StateConcern` and `StatePayload`)

`StateConcern` has three variants (`memory`, `handoff`, `runtime`), matched by the tagged
union `StatePayload` (guarded by `wire_tags!`):

1. **`StatePayload::Memory(MemoryEntry)`** — durable knowledge replacing `MEMORY.md`,
   `index-*.md`, and topic files:
   * `category: MemoryCategory` — `warning` | `rule` | `decision` | `note` (`wire_names!` +
     `wire_parse!`).
   * `basis: MemoryBasis` — `ruling` | `measured` | `inference` (`wire_names!` +
     `wire_parse!`). Prevents unattributed agent inferences from being read as owner rulings.
     *(Invariant)*
   * `lifecycle: MemoryLifecycle` — `active` | `retired` | `superseded` (`wire_names!` +
     `wire_parse!`).
   * `superseded_by: Option<StateId>` — required when `lifecycle == Superseded`, refused for
     other lifecycles, and must resolve to an existing `StateId` of concern `Memory` (not
     `self`). *(Invariant)*
   * `pinned: bool` — when `true` and `lifecycle == Active`, `fl state render` places the
     entry's one-line summary in the top priority tier of the startup index.
   * `summary: String` — single line (no `\n` or `\r`), non-empty after trimming, maximum 240
     Unicode scalar values. Rendered directly in the startup index so the index and the entry
     never drift. *(Invariant)*
   * `body: String` — detailed prose or structured text fetched on demand by `fl state get`
     (capped at 256 KiB per revision so a single entry stays within one sync segment).
     `[agent]` *(Release scope)*

2. **`StatePayload::Handoff(HandoffEntry)`** — current status snapshot replacing `STATE.md`:
   * `summary: String` — single-line headline (non-empty, <= 240 chars).
   * `shipped: Vec<String>` — completed items on `main`.
   * `in_flight: Vec<String>` — active runs, branches, worktrees, or subagents (empty means
     "nothing in flight").
   * `blockers: Vec<String>` — open blockers or questions awaiting an answer.
   * `next_steps: Vec<String>` — ordered next actions.
   * `notes: Option<String>` — optional additional context (<= 64 KiB).

3. **`StatePayload::Runtime(RuntimeEntry)`** — live workflow and execution state:
   * `value: serde_json::Value` — structured JSON state (<= 256 KiB serialized). Because
     workspace `serde_json` forbids `preserve_order`, object keys serialize in deterministic
     lexicographical order. *(Invariant)*
   * `merge_mode: MergeMode` — `conflict` (default) | `append` | `last_writer` (`wire_names!`
     + `wire_parse!`).
   * **Type invariant for `append`:** when `merge_mode == MergeMode::Append`, `value` must be
     a `serde_json::Value::Array`. Creating or updating an `append` entry with a non-array
     JSON value is refused with `StateError::AppendRequiresArray`. *(Invariant)*

---

## 2. The Agent and Role Registry (`fl-core` + `fl-store`, §6a activated)

### 2.1 Coverage areas and projects (`AreaRecord`)

The registry tracks projects and workspace-wide coverage areas (`AreaRecord`):

* `area_key: String` — lowercase normalized key (`ai-lab-amd`, `ferroloop`, `lab-manager`),
  obeying the slug rule of §1.2.
* `name: String` — display name (`AI-Lab-AMD`, `FerroLoop`, `Lab-Manager`).
* `path: Option<String>` — workspace-relative directory path (`Some("FerroLoop/")`), or
  `None` for pathless workspace-wide roles (`Lab-Manager`).
* `purpose: String` — one-line description (non-empty, <= 240 chars).
* `updated_at: At`, `by: String`.

### 2.2 Agent records (`AgentRecord`)

Each registered agent is stored as an `AgentRecord`:

* `id: AgentId` — immutable `urn:uuid:` v7 IRI (`Kind::Agent`), minted on first registration.
  *(Invariant — identity spec §6a)*
* `name_key: String` — lowercase canonical address (`ferris`, `felicia`, `jarvis`), obeying
  the slug rule of §1.2 (`^[a-z][a-z0-9_-]{0,63}$`). Resolves case-insensitively to `id` via
  `agent_by_name`, in addition to the agent's numeric `Handles` entry (`1`, `2`, …) under
  `Kind::Agent`. *(Invariant — identity spec §6a)*
* `display_name: String` — humanized mixed-case name (`Ferris`, `Felicia`, `JARVIS`), whose
  ASCII lowercase form must equal `name_key`. *(Invariant)*
* `email: String` — lowercase email address (`ferris@artificialhumanity.io`); defaults to
  `<name_key>@artificialhumanity.io` when `--email` is omitted, and its local-part (before
  `@`) must be lowercase. Used to render `Co-authored-by: <display_name> <<email>>`.
  *(Invariant)*
* `project: String` — primary `area_key` (must exist in `areas`, or is auto-created with
  `purpose = "Registered area <name>"` when `--create-area` is passed, satisfying §2.1's
  non-empty `purpose` invariant; by default, registering against an unknown area is refused
  with `StoreError::NoSuchArea` so typos are caught). `[agent]` *(Invariant)*
* `role: AgentRole` — `lead` | `assistant` | `lab_manager` | `custom(String)` (where the
  custom role slug obeys §1.2). Serializes as a flat string in JSON, CLI flags, and store
  records (`"lead"`, `"assistant"`, `"lab_manager"`, `"custom:<slug>"`) via custom
  `Serialize`/`Deserialize` + `as_wire`/`from_wire` (matching `Iri`'s custom string serde
  pattern). *(Invariant)*
* `extra_areas: Vec<String>` — optional additional `area_key`s the agent covers (for example,
  `["lab-manager", "agents", "notes"]`). Each must exist in `areas`.
* `status: AgentStatus` — `active` | `retired`.
* `updated_at: At`, `by: String`.

### 2.3 Reporting hierarchy

An agent's `reports_to` target is derived deterministically from the registry state:

* If `role` is `AgentRole::Lead` or `AgentRole::LabManager`, `reports_to` is `"Owner"`.
* If `role` is `AgentRole::Assistant` or `AgentRole::Custom(_)`, `reports_to` is the
  `display_name` of the active `lead` registered for `agent.project` (or `"Owner"` if that
  project currently has no active `lead`). *(Invariant)*

### 2.4 Collision and one-lead invariants

`AgentRegistry::register_agent` enforces three guards inside the write transaction:

1. **Name collision guard:** if `name_key` already exists in `agent_by_name` (whether
   `active` or `retired`) and `replace == false`, registration is refused with
   `StoreError::AgentCollision { name_key, existing_id, status }` (CLI exit code `2`),
   instructing the caller to confirm with the owner whether it is resuming/replacing that
   agent before passing `--replace`. *(Invariant)*
2. **IRI continuity on `--replace`:** when `name_key` already exists and `replace == true`,
   the existing `AgentId(Iri)` is preserved and its `AgentRecord` is updated in place (setting
   `status = Active`), so an agent's IRI never changes across restarts or `/clear`.
   *(Invariant)*
3. **One-lead-per-project guard:** if `role == AgentRole::Lead` and another `active` agent
   with a different `AgentId` already holds `role == AgentRole::Lead` for the same `project`:
   * If `replace == false`, registration is refused with
     `StoreError::LeadTaken { project, lead_name, lead_id }` (CLI exit code `2`).
   * If `replace == true`, the previous lead's `status` is set to `AgentStatus::Retired` in
     the same transaction before the new lead becomes `active`. *(Invariant)*

---

## 3. Revision log, materialized view, and conflict resolution (`fl-store` Format 6)

### 3.1 Store format 6 (`FORMAT_WITH_STATE = 6`)

`crates/store/src/lib.rs` defines `pub const FORMAT_WITH_STATE: u64 = 6;`.

* Any write to the state database or agent registry calls
  `raise_format(&txn, FORMAT_WITH_STATE)` inside the same `redb` write transaction.
  *(Invariant)*
* `RedbStore::open` accepts format versions `2..=6`. A store that never writes state or
  agents stays at its current format (`2..=5`). An older `fl` binary (`newest < 6`) refuses a
  format-6 store with `StoreError::FormatVersion` instead of silently ignoring state,
  conflicts, or sensitivity flags. *(Invariant)*

### 3.2 Additive `redb` tables and `IDS`/`HANDLES` indexing

All eleven tables below are additive (created on first write; an absent table reads as empty):

| table | key | value | purpose |
|---|---|---|---|
| `state_entries` | `&str` (`StateId` IRI) | `&str` (JSON `StateEntry`) | Current materialized view per entry, including `head_revision: RevisionId`, `ever_sensitive: bool`, `created_at`, `updated_at`, `updated_by`. |
| `state_keys` | `(&str, &str, &str)` (`(scope, concern, slug)`) | `&str` (`StateId` IRI) | `O(1)` lookup by `(scope, concern, slug)`. |
| `state_revisions` | `u64` (internal sequence) | `&str` (JSON `StateRevision`) | Append-only revision log in local arrival order. |
| `state_revision_ids` | `&str` (`RevisionId` IRI) | `u64` (key in `state_revisions`) | Deduplication and parent-ancestry lookup by `RevisionId`. |
| `state_conflicts` | `&str` (`StateId` IRI) | `&str` (JSON `StateConflict`) | Active unresolved conflicts (`local_head`, `remote_head`, `detected_at`). |
| `state_sync_cursors` | `&str` (`(repo_dir, remote)` key) | `&str` (JSON `StateSyncCursor`) | Tracks `last_remote_commit`, `last_published_rev_seq`, and `last_published_agent_seq` per configured `(repo_dir, remote)`. |
| `agents` | `&str` (`AgentId` IRI) | `&str` (JSON `AgentRecord`) | Current materialized agent records. |
| `agent_by_name` | `&str` (`name_key`) | `&str` (`AgentId` IRI) | Case-insensitive address index (`name_key` -> `AgentId`). |
| `areas` | `&str` (`area_key`) | `&str` (JSON `AreaRecord`) | Registered projects and workspace coverage areas. |
| `agent_events` | `u64` (internal sequence) | `&str` (JSON `RegistryEvent`) | Append-only log of area and agent mutations (`event_id: Iri`) for `fl/state` sync. |
| `agent_event_ids` | `&str` (`event_id` IRI) | `u64` (key in `agent_events`) | Deduplication and byte-for-byte tamper verification for imported `RegistryEvent`s. |

* **`IDS` and `HANDLES` indexing:** creating or importing a `StateId` or `AgentId` indexes it
  in the existing `IDS` (`"state"` / `"agent"`), `HANDLES`, and `HANDLE_OF` tables via
  `index_new` (and in `MemStore::owned`), so `owns()`, `check()`, `kind_of()`, and
  `conformance::an_id_has_no_handle_under_any_kind_but_its_own` work across all 6 `Kind`s.
  *(Invariant)*
* **`RegistryEvent` wire structure:** each entry in `agent_events` (and on `fl/state` in
  `areas.jsonl` / `agents.jsonl`) is a `RegistryEvent { event_id: Iri, at: At, by: String,
  kind: RegistryEventKind }` where `RegistryEventKind` (guarded by `wire_tags!`) is
  `AreaUpsert(AreaRecord)` in `areas.jsonl`, or `AgentUpsert(AgentRecord)` /
  `AgentRetire { id: AgentId }` in `agents.jsonl`. *(Invariant)*

### 3.3 Revision DAG and conflict resolution

Each `StateRevision` holds:

* `id: RevisionId`
* `entry_id: StateId`
* `scope: StateScope`
* `concern: StateConcern`
* `slug: String`
* `parent_revisions: Vec<RevisionId>` (empty for the initial revision of an entry; 1 parent
  for a normal update; 2 parents `[first, second]` sorted by IRI for a merge or conflict
  resolution)
* `sensitive: bool`
* `payload: StatePayload`
* `at: At`
* `by: String`

* **Immutability of `(scope, concern, slug)`:** `(scope, concern, slug)` are immutable for a
  given `StateId`. Any local or imported `StateRevision` whose `(scope, concern, slug)` differs
  from the entry's initial revision is refused with `StoreError::Tampered`. *(Invariant)*

**Write and merge rules:**

1. **Refusal while conflicted:** while `state_conflicts` holds `entry_id`, any normal write
   to that entry (`memory set`, `memory retire`, `handoff set`, `runtime set`) is refused
   with `StoreError::StateConflict { id, local_head, remote_head }`, naming
   `fl state resolve <id> --keep <revision-id> --by <author>`. *(Invariant)*
2. **Importing a remote revision chain during `fl state sync`:**
   * Revisions already present in `state_revision_ids` are verified byte-for-byte (after
     canonical JSON re-encoding) against the stored revision; a mismatch fails closed with
     `StoreError::Tampered { id, detail }` (matching the GitHub ledger's immutability rule).
     *(Invariant)*
   * For each entry touched by newly imported remote revisions, let `L` be the local
     `head_revision` before import and `R` be the remote tip revision for that `entry_id`:
     * **Fast-forward (`L` is an ancestor of `R` in `parent_revisions`):** update
       `state_entries[entry_id]` to `R`.
     * **Already ahead (`R` is an ancestor of `L`):** keep `L` as `head_revision`.
     * **True divergence (`L != R` and neither is an ancestor of the other):**
       * If the entry is `Runtime` and `L` and `R` have different `merge_mode` values, sync
         fails closed to `MergeMode::Conflict`. *(Invariant)*
       * For `Memory`, `Handoff`, and `Runtime` with `merge_mode == Conflict` (or mismatched
         `merge_mode`): write `StateConflict { entry_id, local_head: L, remote_head: R }` to
         `state_conflicts` and keep `L` as the readable local head while marking the entry
         conflicted.
       * For `Runtime` with `merge_mode == Append`: compute the lowest common ancestor `A` of
         `L` and `R` (or empty array if none), take the array at `L` and append each element
         from `R`'s array that was added after `A` (preserving order and deduplicating exact
         JSON values added on both sides), and write a new `StateRevision` with
         `parent_revisions = sorted(L, R)`, `at = fl_exec::stamp::now()`, and `by = "fl"`.
       * For `Runtime` with `merge_mode == LastWriter`: choose the winner between `L` and `R`
         by comparing `(revision.at, revision.id.as_str())` (higher wins, deterministic tie
         breaker on IRI) and write a merge `StateRevision` with `parent_revisions =
         sorted(L, R)`, `at = fl_exec::stamp::now()`, and `by = "fl"`. *(Invariant)*
3. **Key collision across distinct `StateId`s minted offline on two machines (first-published
   `StateId` wins):** because `fl/state` is a single linear branch updated with CAS
   (`--force-with-lease`), two machines can never both publish initial revisions for the same
   `(scope, concern, slug)` without one fetching the other's commit first. When a syncing
   machine fetches a remote initial revision for `(scope, concern, slug)` with `S_remote` and
   discovers it already has an **unpublished** local entry for the same `(scope, concern, slug)`
   with `S_local != S_remote`:
   1. `S_remote` becomes the canonical `StateId` for `(scope, concern, slug)`.
   2. `S_local` is converted in `IDS` from `"state"` to `"alias"` and recorded in `ALIASES`
      (`S_local -> S_remote`), removing the superseded `state_entries[S_local]` row and updating
      `state_keys` to `S_remote` in the same transaction so any local caller or `superseded_by`
      reference holding `S_local` resolves to `S_remote` without leaving duplicate primary rows
      (extending `RedbStore` and `MemStore` alias support to allow aliasing an unpublished
      `Kind::State` or `Kind::Agent` ID during sync).
   3. Any local unpublished `StateRevision`s for `S_local` have their `entry_id` updated in
      place to `S_remote` before publication, and their divergence with `S_remote`'s revision
      chain is resolved via rule 2 (`StateConflict`, `Append`, or `LastWriter`).
   4. All revisions for that `(scope, concern, slug)` are stored on `fl/state` under a single
      directory `revisions/<sha256(S_remote)[:32]>/`, and no already-published revision is ever
      mutated. *(Invariant)*
4. **Resolving a conflict (`fl state resolve`):** `fl state resolve <id-or-key> --keep
   <revision-id> --by <author>` verifies inside a single write transaction that the entry
   currently has a `StateConflict { local_head, remote_head }` in `state_conflicts` and that
   `<revision-id>` equals either `local_head` or `remote_head`. It then mints a new
   `StateRevision` with `parent_revisions = sorted(local_head, remote_head)`, the kept
   revision's `payload`, and `sensitive = local.sensitive || remote.sensitive`, updates
   `state_entries[entry_id]` (`head_revision = new_rev_id`), and deletes
   `state_conflicts[entry_id]`. *(Invariant)*

---

## 4. Standard-git sync on `fl/state` and sensitivity guard

### 4.1 Branch layout (`refs/heads/fl/state`)

`fl/state` is an orphan branch with linear first-parent commit history and append-only files:

* `format` — `"1\n"`. A reader that sees a higher number refuses with a message instructing
  the user to upgrade `fl`. *(Invariant)*
* `README` — short human description explaining that `fl/state` is managed by `fl state sync`.
* `areas.jsonl` — one JSON object per line (`RegistryEvent` with
  `RegistryEventKind::AreaUpsert(AreaRecord)`), sorted keys.
* `agents.jsonl` — one JSON object per line (`RegistryEvent` with
  `RegistryEventKind::AgentUpsert(AgentRecord)` or `RegistryEventKind::AgentRetire { id }`),
  sorted keys.
* `revisions/<sha256(primary_state_id_iri)[:32]>/<n>.jsonl` — one `StateRevision` JSON object
  per line, sorted keys. Segment `<n>` starts at `0000` and rolls to `<n+1>` when adding a
  line would make the segment exceed `262_144` bytes (256 KiB), matching `fl/ledger`.
  *(Invariant)*

### 4.2 Git plumbing protocol (no working-tree touch, no vendor APIs)

`fl state sync` shells out only to the `git` CLI in `crates/exec/src/git.rs` against the
configured target repository directory (`repo_dir`):

1. **Probe & fetch:** run
   `git -C <repo_dir> ls-remote --exit-code <remote> refs/heads/fl/state` before `git fetch`:
   * Exit `0` returns the remote OID (`remote_head = Some(oid)`), followed by
     `git -C <repo_dir> fetch --quiet <remote> +refs/heads/fl/state:refs/fl/state/remote/<remote>`.
   * Exit `2` proves the remote was reached and `refs/heads/fl/state` does not exist yet
     (`remote_head = None`).
   * Any other exit code (such as `128`) fails closed with `StoreError::Unreachable` /
     `ExecError::Git`. *(Invariant)*
2. **Verify append-only & read delta:**
   * If `state_sync_cursors` has `last_remote_commit = Some(C)` for `(repo_dir, remote)` and
     `remote_head` is `None`, refuse with `StoreError::Tampered` (remote `fl/state` branch was
     deleted after a prior sync). *(Invariant)*
   * If `last_remote_commit = Some(C)` and `remote_head = Some(R)`, verify via
     `git merge-base --is-ancestor <C> <R>` that the remote branch moved forward linearly, and
     verify that no existing file under `areas.jsonl`, `agents.jsonl`, or `revisions/` shrank
     or altered its prefix bytes.
   * Read only the newly appended lines via `git cat-file blob`, and apply them to `fl-store`
     in a single write transaction (§3.3). Any `Append` or `LastWriter` merge revision is
     synthesized with `at = fl_exec::stamp::now()` only if `head_revision` is not already a
     merge of `[L, R]`.
   * **Registry event replay and offline collision convergence:** events in `areas.jsonl` and
     `agents.jsonl` are deduplicated and verified byte-for-byte against `agent_event_ids`
     (refusing mismatches with `StoreError::Tampered`) and applied in log order:
     * If an imported `AgentUpsert` sets `role == Lead` and `status == Active` for a `project`
       that already has a different active lead, the previous lead is automatically set to
       `Retired` (never failing sync with `LeadTaken`).
     * If an imported `AgentUpsert` has the same `name_key` as an existing unpublished local
       `AgentRecord` with a different `AgentId`, the first-published (remote) `AgentId` wins as
       primary and the unpublished local `AgentId` is converted in `IDS` to `"alias"`,
       removed from `agents[local_id]`, and aliased in `ALIASES` (`local_id -> remote_id`),
       matching §3.3 rule 3. *(Invariant)*
3. **Remote-scoped collection & sensitivity pre-flight (§4.3):** each `StateSyncCursor` is
   keyed by `(repo_dir, remote)`. Collect after `last_published_*_seq` **only** the
   unpublished local `StateRevision`s (and `RegistryEvent`s, when syncing the workspace state
   store) whose scope resolves to that target `(repo_dir, remote)` and that were not just
   imported from `remote_head` in step 2. If any collected `StateRevision`
   belongs to an entry with `ever_sensitive == true` (or `sensitive == true`), verify that the
   target `(repo_dir, remote)` has `private == true` (§4.3) **before** creating any git blob
   or pushing. If there are no unpublished local revisions or registry events for this
   `(repo_dir, remote)`, record `last_remote_commit = remote_head` in `state_sync_cursors` and
   **skip steps 4 and 5** without creating or pushing an empty commit. *(Invariant)*
4. **Build tree & commit without touching index or working tree:** write new/updated segment
   blobs with `git hash-object -w --stdin`, construct the updated tree hierarchy bottom-up per
   directory level (`revisions/<hash>` tree -> `revisions` tree -> root tree) via
   `git ls-tree` and `git mktree` (reading the parent trees from `remote_head` when present),
   and create a commit with `git commit-tree <tree_sha> [-p <remote_head>] -m "fl state sync (<by>)"`
   with environment variables `GIT_AUTHOR_NAME=<by>`, `GIT_AUTHOR_EMAIL=<by>@fl.local`,
   `GIT_COMMITTER_NAME=<by>`, and `GIT_COMMITTER_EMAIL=<by>@fl.local` (where `<by>` comes from
   `fl state sync [--by <author>]`, defaulting to `"fl"`, so commit creation succeeds in clean
   `$HOME` environments without `~/.gitconfig`). `HEAD`, `.git/index`, and every working-tree
   file remain untouched before, during, and after `sync`. *(Invariant)*
5. **Push with CAS (`--force-with-lease`):** push the new commit with
   `git push --porcelain <remote> --force-with-lease=refs/heads/fl/state:<expected_remote_oid> <commit_sha>:refs/heads/fl/state`
   (where `<expected_remote_oid>` is empty `""` when creating the branch for the first time,
   so a concurrent initial push on another machine is caught). If the push is rejected
   because the remote ref moved, repeat from step 1 up to `TRIES = 5`: the next iteration
   fetches only the new remote commits after the already-imported `remote_head` (and reuses
   any already-synthesized merge revision whose parents still match `[L, R]`), so tentative
   merge revisions never stack across CAS retries. After 5 lost races, return
   `StoreError::Contended`. On success, update `state_sync_cursors` in `fl-store`.
   *(Invariant)*

* **`fl state verify`:** probes and fetches `refs/heads/fl/state` for the target
  `(repo_dir, remote)` (using `ls-remote --exit-code` as in step 1), walks the commit graph
  from root to tip verifying linear first-parent history, `format == "1\n"`, append-only
  segment prefixes across every commit, `<= 262_144` byte segment sizes, and byte-for-byte
  agreement with all revisions and registry events stored in `fl-store`. *(Invariant)*

### 4.3 Store selection, configuration, and sensitivity refusal (`config.toml`)

In `$XDG_CONFIG_HOME/fl/config.toml` (and per-project in `[[project]]`), an optional `state`
table configures where `fl state` and `fl agent` store and sync state:

```toml
[state]
# Optional workspace state store path; defaults to $XDG_DATA_HOME/fl/fl.redb
store = "/abs/path/to/workspace-state.redb"
# Default state repository for workspace/group/agent/workflow scopes (e.g. Notes or .agents)
repo = "/abs/path/to/state-repo"
remote = "origin"
private = true

[[project]]
name = "ferroloop"
root = "/abs/path/to/FerroLoop"
store = "/abs/path/to/fl.redb"
# Optional override per project; can point to a private repo even when the code repo is public
state = { repo = "/abs/path/to/private-state-repo", remote = "origin", private = true }
```

* **Workspace state store default:** `[state]` may optionally specify
  `store = "/abs/path/to/state.redb"`; when omitted, the workspace state store defaults to
  `$XDG_DATA_HOME/fl/fl.redb` (or `$HOME/.local/share/fl/fl.redb`).
* **Where `AgentRegistry` and `workspace` / `group:*` / `agent:*` / `workflow:*` scopes live:**
  `fl agent` commands and `fl state` commands for `workspace`, `group:*`, `agent:*`, and
  `workflow:*` scopes operate on the workspace state store (`[state].store` or the XDG default
  store) unless `--db` / `$FL_DB` is explicitly passed (which confines the command to that
  explicit store). *(Invariant)*
* **Where `project:<slug>` scope lives:** if a `[[project]]` entry matches `<slug>` (by an
  optional normalized `name = "<slug>"` field on `[[project]]` or the lowercase basename of
  `[[project]].root`) and defines its own `store`, `project:<slug>` state lives in that
  project's `store` (and syncs using `[[project]].state` if present, or `[state]` otherwise).
  If no matching `[[project]]` defines a separate `store`, `project:<slug>` lives in the
  workspace state store. *(Invariant)*
* **Cross-store IRI lookup:** when `fl state get` / `memory retire` / `resolve` is given a
  full `Iri` (and not confined by `--db` / `$FL_DB`), `choose_store` searches the workspace
  state store and all configured `[[project]].store` files. Numeric handles (`1`, `2`, …) are
  always store-local to the store selected by `--db` / `--scope` / current directory.
  *(Invariant)*
* **Fail-closed sensitivity rule:** `private` defaults to `false` when omitted. If any
  unpublished revision in a `sync` batch for `(repo_dir, remote)` has `sensitive == true` (or
  its entry has `ever_sensitive == true`) and the resolved `state` target has
  `private == false`, `fl state sync` refuses the entire sync with
  `StoreError::SensitiveDisclosure` before creating any git object or pushing. *(Invariant)*
* **Path validation:** `store` and `repo` must be absolute paths (`deny_unknown_fields` on the
  config tables, matching existing `config.toml` rules).

---

## 5. CLI (`fl agent` and `fl state`), MCP server, and read-only exports

### 5.1 `fl agent` CLI

```text
fl agent area set <name> [--path <rel-path>] --purpose <text> --by <author> [--json]
fl agent area list [--json]
fl agent register <name> --project <area> --role <role> [--create-area] [--extra-area <area>...] [--email <email>] [--replace] --by <author> [--json]
fl agent retire <name-or-id> --by <author> [--json]
fl agent roster [--project <area>] [--include-retired] [--render <path>] [--json]
```

* `--role` accepts `lead`, `assistant`, `lab_manager`, or `custom:<slug>`.
* `--create-area` auto-creates `<area>` (and any `--extra-area`) in `areas` with
  `purpose = "Registered area <name>"` if it does not exist yet.
* `<name-or-id>` in `fl agent retire` accepts a full `AgentId` IRI (`urn:uuid:...`), a
  store-local numeric `Kind::Agent` handle (`1`, `2`, …), or a case-insensitive `name_key`
  (`^[a-z][a-z0-9_-]{0,63}$`).
* On `AgentCollision` or `LeadTaken`, `fl agent register` prints an actionable refusal naming
  the existing agent's display name, IRI, project, and role, and exits with code `2`.
* `fl agent roster --render <path>` atomically writes (tempfile + rename, refusing symlinked
  targets or parent directories just like `fl mcp sync`) the Markdown table of Projects/Areas
  and Active Agents (matching `.agents/REGISTRY.md`'s two-table layout).

### 5.2 `fl state` CLI

```text
fl state memory set --scope <scope> --key <slug> --category <warning|rule|decision|note> --basis <ruling|measured|inference> --summary <text> (--body <text> | --body-file <path>) [--pinned | --unpinned] [--sensitive | --not-sensitive] --by <author> [--json]
fl state memory retire <id-or-key> [--scope <scope>] [--superseded-by <id-or-key>] --by <author> [--json]
fl state handoff set --scope <scope> [--key <slug>] --summary <text> [--shipped <item>...] [--in-flight <item>...] [--blocker <item>...] [--next <item>...] [--notes <text>] [--sensitive | --not-sensitive] --by <author> [--json]
fl state runtime set --scope <scope> --key <slug> --value <json> [--merge <conflict|append|last_writer>] [--sensitive | --not-sensitive] --by <author> [--json]
fl state get <id-or-key> [--scope <scope>] [--concern <memory|handoff|runtime>] [--history] [--json]
fl state list [--scope <scope>] [--concern <memory|handoff|runtime>] [--category <cat>] [--include-retired] [--json]
fl state conflicts [--scope <scope>] [--json]
fl state resolve <id-or-key> [--scope <scope>] [--concern <concern>] --keep <revision-id> --by <author> [--json]
fl state render [--scope <scope>...] [--max-bytes <usize>] [--include-sensitive] [--out <path>]
fl state sync [--scope <scope>] [--by <author>] [--json]
fl state verify [--scope <scope>] [--json]
```

* **Resolution of `<id-or-key>`:** because slugs must start with an ASCII letter
  (`^[a-z][a-z0-9_-]{0,63}$`), the three forms are syntactically disjoint:
  1. `urn:uuid:...` parses as `Ref::Iri` and resolves by `StateId` IRI across configured
     stores (§4.3).
  2. Bare digits (`1`, `2`, …) parse as `Ref::Handle(u64)` and resolve strictly in the target
     local store via `store.resolve_handle(Kind::State, n)` (§1.1).
  3. An ASCII-letter string (`[a-z]...`) is treated as a `slug`, which requires `--scope` (and
     `--concern` if the slug exists in more than one concern under that scope, defaulting to
     `handoff` for `"current"` when unambiguous).
* **`fl state render`:** builds a deterministic, read-only Markdown startup summary across
  the requested scopes (defaulting to `workspace` plus the current project's scope when run
  inside a configured project):
  1. Active agent roster for the project/workspace (`fl agent roster` summary).
  2. Current `Handoff` snapshot(s) for the requested scopes (`summary`, `in_flight`,
     `blockers`, `next_steps`, `shipped`).
  3. Active `Memory` index lines (`- [<slug>] (<id>, <category>, <basis>) — <summary>`, where
     `<id>` is the `StateId` IRI), ordered by: (a) priority tier `pinned == true` or
     `category == warning` first, then (b) remaining active memories; within both tier `(a)`
     and tier `(b)`, entries sort deterministically by `(scope, slug)` with `StateScope`
     ordered `Workspace < Group < Project < Agent < Workflow`. Rendering stops before the
     total output (including the truncation footer line itself) exceeds `--max-bytes` (default
     `24_400` bytes, matching the workspace's 24.4 KB index cap), appending a final line
     `_Truncated: N additional active memory entries omitted by byte cap._` when capped.
  4. Entries with `sensitive == true` are omitted from `render` unless `--include-sensitive`
     is explicitly passed, preventing accidental inclusion in public logs or transcripts.
     `[agent]` *(Invariant)*
  5. When `--out <path>` is passed, `fl state render` refuses symlinked targets/parents and
     writes atomically.

### 5.3 MCP stdio server (`fl state mcp`)

`fl state mcp` runs a newline-delimited JSON-RPC 2.0 MCP server over `stdin`/`stdout` (using
no network sockets and no external runtime dependencies), exposing eight tools:

* `agent_roster` (read-only)
* `agent_register`
* `state_render` (read-only)
* `state_get` (read-only)
* `state_list` (read-only)
* `state_set_memory`
* `state_set_handoff`
* `state_set_runtime`

* **Per-call store open/drop (never holding the `redb` file lock while idle):** `redb` takes
  an exclusive OS file lock for the lifetime of `Database`. `fl state mcp` therefore resolves
  the configuration and store path without keeping `RedbStore` open across `stdin` reads, and
  opens and drops `RedbStore` inside each individual JSON-RPC `tools/call` handler. That way
  concurrent `fl` CLI invocations and multiple agent sessions' `fl state mcp` servers never
  contend on the `.redb` file lock while idle. *(Invariant)*
* **Tool input schemas:** each MCP tool's JSON Schema input properties mirror the corresponding
  CLI flags (`scope`, `key`, `category`, `basis`, `summary`, `body`, `pinned`, `sensitive`,
  `clear_sensitive`, `by`, etc.), and `state_set_memory` additionally accepts optional
  `lifecycle` (`"active" | "retired" | "superseded"`) and `superseded_by` so an agent can
  retire or supersede a memory over MCP as well as the CLI.
* Any project can register `fl state mcp` in `.fl/mcp.toml` so `fl mcp sync` writes it into
  Claude Code, Codex, and Antigravity configurations.

---

## 6. Verification and testing

1. **Wire-format population guard (`crates/core/src/wire.rs`) and conformance floors:**
   * `Kind` gains `State` and `Agent`.
   * New `wire_names!` + `wire_parse!` enums: `StateConcern`, `MemoryCategory`,
     `MemoryBasis`, `MemoryLifecycle`, `MergeMode`, `AgentStatus`.
   * New `wire_tags!` enums: `StatePayload` (and `RegistryEventKind`).
   * Custom flat-string serde types (`StateScope` and `AgentRole`) have dedicated
     `as_wire`/`from_wire` and JSON round-trip + invalid-input unit tests.
   * `crates/core/src/conformance.rs` adds `STATE_STORE_CASES` and `AGENT_REGISTRY_CASES`
     case-count floor constants and updates `ALL_ROLES_CASES` and `LOCAL_HANDLES_CASES`
     (including `an_id_has_no_handle_under_any_kind_but_its_own` across all 6 `Kind`s), while
     `crates/core/tests/wire_refs.rs` extends the `--json` reference-field IRI scan floor
     (`2026-09-23-identity-and-store-roles-design.md` §6.5) to cover `StateEntry`,
     `StateRevision`, `AgentRecord`, and `RegistryEvent`.
2. **Store conformance (`crates/core/src/conformance.rs`):**
   * Shared conformance tests run against both `MemStore` and `RedbStore` covering:
     * Format 6 upgrade on first `StateStore` or `AgentRegistry` write (and format preservation
       when neither is written).
     * `AgentRegistry`: area CRUD, `--create-area` default purpose, agent registration,
       case-insensitive `name_key` lookup and numeric `Kind::Agent` handle resolution,
       `AgentCollision` refusal on active and retired names without `replace`, IRI
       preservation on `replace = true`, `LeadTaken` refusal on second active lead, previous
       lead retirement on `replace = true`, and deterministic `reports_to` derivation.
     * `StateStore`: `Memory`, `Handoff`, and `Runtime` CRUD, `(scope, concern, slug)`
       immutability check, sticky `sensitive: true` inheritance, `Superseded` target
       validation, `AppendRequiresArray` validation, revision history ordering, fast-forward
       sync import, true-divergence conflict creation (including mismatched `merge_mode`
       failing closed to `Conflict`), write refusal until `resolve`, `fl state resolve`
       parent-revision merge and conflict clearing, automatic `append` and `last_writer`
       merges, and offline same-key first-published `StateId` / `AgentId` alias unification
       (§3.3 rule 3 & §4.2 step 2).
3. **Black-box CLI and git-plumbing tests (`crates/cli/tests/state_sync.rs`):**
   * Two isolated machines (two `FL_DB` stores and two git checkouts) syncing against a local
     `git init --bare` remote:
     * **Clean merge and no-op push skip:** Machine A writes memory `m-a`, Machine B writes
       handoff `h-b`; both run `fl state sync`; both stores converge with zero conflicts, and
       running `fl state sync` a second time with no new local writes creates no new commit on
       `fl/state`.
     * **Working-tree and index isolation:** before and after `fl state sync`, `git status
       --porcelain` and `.git/index` sha256 in the working repository are asserted unchanged,
       and `HEAD` remains on `main`.
     * **Same-key conflict and resolution:** Machine A and Machine B concurrently update the
       same `memory` key; the second `sync` retries the non-fast-forward push, imports the
       first machine's revision, records a `StateConflict`, refuses subsequent `memory set`
       calls on that key, and converges cleanly after `fl state resolve --keep <rev>` + `sync`.
     * **Offline same-key `StateId` collision:** Machine A and Machine B both create a new
       entry for the same `(scope, concern, slug)` before syncing; Machine A pushes first;
       Machine B syncs, aliases its unpublished `S_local -> S_remote` in `ALIASES`, re-parents
       its local revision under `S_remote`, and converges under a single
       `revisions/<sha256(S_remote)[:32]>/` directory.
     * **`Runtime` merge modes:** concurrent array updates under `merge = append` and scalar
       updates under `merge = last_writer` converge without entering `state_conflicts` and
       without stacking duplicate merge revisions on CAS retry.
     * **Tamper, non-fast-forward, and branch-deletion detection:** `fl state verify` and
       `fl state sync` refuse a remote `fl/state` branch whose existing segment line was
       altered or truncated, or whose `refs/heads/fl/state` branch was deleted after a prior
       sync (`ls-remote --exit-code` returning `2` when `last_remote_commit` is `Some`).
     * **Sensitivity refusal:** a `sensitive: true` entry is refused by `fl state sync` when
       `private = false` (or omitted) in `config.toml`, and `git rev-parse refs/heads/fl/state`
       on the bare remote confirms nothing was pushed.
     * **Read-only export and byte cap:** `fl state render --max-bytes 600` places `Handoff`
       and `pinned` / `warning` memories first (sorted by `(scope, slug)` within each tier),
       respects the 600-byte ceiling including the `_Truncated: ..._` footer line, omits
       `sensitive` entries by default, and refuses symlinked `--out` paths.
     * **MCP stdio round-trip and lock release:** `fl state mcp` handles `initialize`,
       `tools/list`, and `tools/call` for `agent_register`, `agent_roster`,
       `state_set_memory`, and `state_render`, and a concurrent `fl state list` CLI command
       succeeds while `fl state mcp` is waiting on `stdin`.
