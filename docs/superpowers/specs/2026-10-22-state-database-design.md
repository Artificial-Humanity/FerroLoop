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

* `Kind::State` (`"state"`), wrapped by `StateId(pub Iri)`. Its display handle prefix in
  `Handles` is `s` (`s1`, `s2`, …). *(Invariant)*
* `Kind::Agent` (`"agent"`), wrapped by `AgentId(pub Iri)`. Its display handle prefix in
  `Handles` is `a` (`a1`, `a2`, …). This activates the reservation in
  `2026-09-23-identity-and-store-roles-design.md` §6a. *(Invariant)*

Every state mutation also mints a `RevisionId(pub Iri)` (`urn:uuid:` v7). `[agent]` Like
`GateRun` and `Attempt` entry IDs on the ledger, `RevisionId` is an immutable IRI that names
one revision in history and on `fl/state`, but it is not registered in `HANDLES` (so rapid
runtime checkpoints do not churn short handles).

Both `Kind::State` and `Kind::Agent` are added to `wire_names!` and `wire_parse!`, and every
exhaustive match on `Kind` (including `KindRouted` and `RedbStore::check`) is extended to
handle them. *(Invariant)*

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

* **Slug normalization:** the inner string of `Group`, `Project`, `Agent`, and `Workflow` is
  normalized to ASCII lowercase on parse. It must be 1 to 64 bytes of ASCII lowercase
  letters, digits, `-`, or `_`, starting with an ASCII letter or digit (`^[a-z0-9][a-z0-9_-]{0,63}$`).
  Empty strings, path separators (`/` or `\`), `..`, whitespace, and control characters are
  refused with `StateError::InvalidSlug`. *(Invariant)*
* **Canonical wire spelling:** `workspace`, `group:<slug>`, `project:<slug>`,
  `agent:<name_key>`, `workflow:<slug>`. Accepted on the CLI and in JSON/MCP via `--scope`.
  `[agent]` When `--scope project:<id-or-handle>` names a known `ProjectId` IRI or `p1`
  handle in the store, `fl-cli` resolves the project's normalized name (or uses the explicit
  slug when the project is not in the local `Catalog`), so a state store can hold project
  state before or without a gate catalog. *(Release scope)*
* **Entry keys (`slug`):** within a `(StateScope, StateConcern)` pair, each entry has a human
  key (`slug`) obeying the same 1–64 byte slug rule. For `Handoff`, `--key` defaults to
  `"current"` when omitted, so a project or agent normally has one canonical handoff snapshot
  at `(scope, handoff, "current")` while still allowing named handoffs when needed. `[agent]`
  *(Release scope)*

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
  the slug rule of §1.2. Resolves case-insensitively to `id` like a handle. *(Invariant —
  identity spec §6a)*
* `display_name: String` — humanized mixed-case name (`Ferris`, `Felicia`, `JARVIS`), whose
  ASCII lowercase form must equal `name_key`. *(Invariant)*
* `email: String` — lowercase email address (`ferris@artificialhumanity.io`); defaults to
  `<name_key>@artificialhumanity.io` when `--email` is omitted, and its local-part (before
  `@`) must be lowercase. Used to render `Co-authored-by: <display_name> <<email>>`.
  *(Invariant)*
* `project: String` — primary `area_key` (must exist in `areas`, or is auto-created with
  empty purpose only when `--create-area` is passed; by default, registering against an
  unknown area is refused with `StoreError::NoSuchArea` so typos are caught). `[agent]`
  *(Invariant)*
* `role: AgentRole` — `lead` | `assistant` | `lab_manager` | `custom(String)` (where the
  custom role slug obeys §1.2).
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

### 3.2 Additive `redb` tables

All ten tables below are additive (created on first write; an absent table reads as empty):

| table | key | value | purpose |
|---|---|---|---|
| `state_entries` | `&str` (`StateId` IRI) | `&str` (JSON `StateEntry`) | Current materialized view per entry, including `head_revision: RevisionId`, `ever_sensitive: bool`, `created_at`, `updated_at`, `updated_by`. |
| `state_keys` | `(&str, &str, &str)` (`(scope, concern, slug)`) | `&str` (`StateId` IRI) | `O(1)` lookup by `(scope, concern, slug)`. |
| `state_revisions` | `u64` (internal sequence) | `&str` (JSON `StateRevision`) | Append-only revision log in local arrival order. |
| `state_revision_ids` | `&str` (`RevisionId` IRI) | `u64` (key in `state_revisions`) | Deduplication and parent-ancestry lookup by `RevisionId`. |
| `state_conflicts` | `&str` (`StateId` IRI) | `&str` (JSON `StateConflict`) | Active unresolved conflicts (`local_head`, `remote_head`, `detected_at`). |
| `state_sync_cursors` | `&str` (remote key) | `&str` (JSON `StateSyncCursor`) | Tracks `last_remote_commit`, `last_published_rev_seq`, and `last_published_agent_seq` per configured remote. |
| `agents` | `&str` (`AgentId` IRI) | `&str` (JSON `AgentRecord`) | Current materialized agent records. |
| `agent_by_name` | `&str` (`name_key`) | `&str` (`AgentId` IRI) | Case-insensitive address index (`name_key` -> `AgentId`). |
| `areas` | `&str` (`area_key`) | `&str` (JSON `AreaRecord`) | Registered projects and workspace coverage areas. |
| `agent_events` | `u64` (internal sequence) | `&str` (JSON `RegistryEvent`) | Append-only log of area and agent mutations (` event_id: Iri `) for `fl/state` sync. |

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
       * For `Memory`, `Handoff`, and `Runtime` with `merge_mode == Conflict`: write
         `StateConflict { entry_id, local_head: L, remote_head: R }` to `state_conflicts` and
         keep `L` as the readable local head while marking the entry conflicted.
       * For `Runtime` with `merge_mode == Append`: compute the lowest common ancestor `A` of
         `L` and `R` (or empty array if none), take the array at `L` and append each element
         from `R`'s array that was added after `A` (preserving order and deduplicating exact
         JSON values added on both sides), and write a new `StateRevision` with
         `parent_revisions = sorted(L, R)` and `by = "fl"`.
       * For `Runtime` with `merge_mode == LastWriter`: choose the winner between `L` and `R`
         by comparing `(revision.at, revision.id.as_str())` (higher wins, deterministic tie
         breaker on IRI) and write a merge `StateRevision` with `parent_revisions =
         sorted(L, R)` and `by = "fl"`. *(Invariant)*
3. **Key collision across distinct `StateId`s minted offline on two machines:** if Machine 1
   and Machine 2 both create a brand-new entry for the same `(scope, concern, slug)` before
   syncing, they will mint two different `StateId`s (`S1` and `S2`). `[agent]` On sync, the
   deterministic primary `StateId` is `min(S1, S2)` by IRI order, the other `StateId` is
   recorded in `aliases` (`max(S1, S2) -> min(S1, S2)`), both initial revisions are attached
   to the primary `StateId`, and their divergence follows rule 2 above (recording a
   `StateConflict` unless `Runtime` declares `append` or `last_writer`). *(Invariant)*

---

## 4. Standard-git sync on `fl/state` and sensitivity guard

### 4.1 Branch layout (`refs/heads/fl/state`)

`fl/state` is an orphan branch with linear first-parent commit history and append-only files:

* `format` — `"1\n"`. A reader that sees a higher number refuses with a message instructing
  the user to upgrade `fl`. *(Invariant)*
* `README` — short human description explaining that `fl/state` is managed by `fl state sync`.
* `areas.jsonl` — one JSON object per line (`AreaEvent`), sorted keys.
* `agents.jsonl` — one JSON object per line (`AgentEvent`), sorted keys.
* `revisions/<sha256(primary_state_id_iri)[:32]>/<n>.jsonl` — one `StateRevision` JSON object
  per line, sorted keys. Segment `<n>` starts at `0000` and rolls to `<n+1>` when adding a
  line would make the segment exceed `262_144` bytes (256 KiB), matching `fl/ledger`.
  *(Invariant)*

### 4.2 Git plumbing protocol (no working-tree touch, no vendor APIs)

`fl state sync` shells out only to the `git` CLI in `crates/exec/src/git.rs` against the
configured target repository directory (`repo_dir`):

1. **Fetch:** run `git -C <repo_dir> fetch --quiet <remote> +refs/heads/fl/state:refs/fl/state/remote/<remote>`.
   If the remote ref does not exist yet, `remote_head` is `None`.
2. **Verify append-only & read delta:** if `last_remote_commit` is recorded in
   `state_sync_cursors` for `<remote>`, verify via `git merge-base --is-ancestor
   <last_remote_commit> <remote_head>` that the remote branch moved forward linearly, and
   verify that no existing file under `areas.jsonl`, `agents.jsonl`, or `revisions/` shrank
   or altered its prefix bytes. Read only the newly appended lines via `git cat-file blob`,
   and apply them to `fl-store` in a single write transaction (§3.3). For `areas.jsonl` and
   `agents.jsonl`, events apply in log order (with `(at, event_id)` tie-breaking).
3. **Sensitivity pre-flight (§4.3):** collect all local `RegistryEvent`s and `StateRevision`s
   after `last_published_*_seq`. If any unpublished `StateRevision` belongs to an entry with
   `ever_sensitive == true` (or `sensitive == true`), verify that the target remote is
   authorized for sensitive state (§4.3) **before** creating any git blob or pushing.
4. **Build tree & commit without touching index or working tree:** write new/updated segment
   blobs with `git hash-object -w --stdin`, construct the updated tree hierarchy via
   `git mktree` (reading the parent tree from `remote_head` with `git ls-tree`), and create a
   commit with `git commit-tree <tree_sha> [-p <remote_head>] -m "fl state sync (<by>)"`.
   `HEAD`, `.git/index`, and every working-tree file remain untouched before, during, and
   after `sync`. *(Invariant)*
5. **Push with CAS (`--force-with-lease`):** push the new commit with
   `git push --porcelain <remote> --force-with-lease=refs/heads/fl/state:<expected_remote_oid> <commit_sha>:refs/heads/fl/state`
   (where `<expected_remote_oid>` is empty `""` when creating the branch for the first time,
   so a concurrent initial push on another machine is caught). If the push is rejected
   because the remote ref moved, repeat from step 1 up to `TRIES = 5`. After 5 lost races,
   return `StoreError::Contended`. On success, update `state_sync_cursors` in `fl-store`.
   *(Invariant)*

### 4.3 Configuration and sensitivity refusal (`config.toml`)

In `$XDG_CONFIG_HOME/fl/config.toml` (and per-project in `[[project]]`), an optional `state`
table configures where `fl state sync` pushes and pulls:

```toml
[state]
# Default state repository for workspace/group/agent scopes (e.g. a checkout of Notes or .agents)
repo = "/abs/path/to/state-repo"
remote = "origin"
private = true

[[project]]
root = "/abs/path/to/FerroLoop"
store = "/abs/path/to/fl.redb"
# Optional override per project; can point to a private repo even when the code repo is public
state = { repo = "/abs/path/to/private-state-repo", remote = "origin", private = true }
```

* **Fail-closed sensitivity rule:** `private` defaults to `false` when omitted. If any
  unpublished revision in a `sync` batch has `sensitive == true` (or its entry has
  `ever_sensitive == true`) and the resolved `state` target has `private == false`,
  `fl state sync` refuses the entire sync with `StoreError::SensitiveDisclosure` before
  creating any git object or network connection. *(Invariant)*
* **Path validation:** `repo` must be an absolute path (`deny_unknown_fields` on the config
  table, matching existing `config.toml` rules).

---

## 5. CLI (`fl agent` and `fl state`), MCP server, and read-only exports

### 5.1 `fl agent` CLI

```text
fl agent area set <name> [--path <rel-path>] --purpose <text> --by <author> [--json]
fl agent area list [--json]
fl agent register <name> --project <area> --role <role> [--extra-area <area>...] [--email <email>] [--replace] --by <author> [--json]
fl agent retire <name-or-id> --by <author> [--json]
fl agent roster [--project <area>] [--include-retired] [--render <path>] [--json]
```

* `--role` accepts `lead`, `assistant`, `lab_manager`, or `custom:<slug>`.
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
fl state sync [--scope <scope>] [--json]
fl state verify [--scope <scope>] [--json]
```

* **Resolution of `<id-or-key>`:** if the argument parses as an `Iri` (`urn:uuid:...`) or a
  `StateId` handle (`s1`, `s2`), it resolves directly by ID across configured stores.
  Otherwise it is treated as a `slug`, which requires `--scope` (and `--concern` if the slug
  exists in more than one concern under that scope, defaulting to `handoff` for `"current"`
  when unambiguous).
* **`fl state render`:** builds a deterministic, read-only Markdown startup summary across
  the requested scopes (defaulting to `workspace` plus the current project's scope when run
  inside a configured project):
  1. Active agent roster for the project/workspace (`fl agent roster` summary).
  2. Current `Handoff` snapshot(s) for the requested scopes (`summary`, `in_flight`,
     `blockers`, `next_steps`, `shipped`).
  3. Active `Memory` index lines (`- [<slug>] (<id>, <category>, <basis>) — <summary>`),
     ordered by: (a) `pinned == true` or `category == warning` first, then (b) remaining
     active memories in `(scope, slug)` order, stopping before the output exceeds
     `--max-bytes` (default `24_400` bytes, matching the workspace's 24.4 KB index cap) and
     appending a final line `_Truncated: N additional active memory entries omitted by byte
     cap._` when capped.
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

Any project can register `fl state mcp` in `.fl/mcp.toml` so `fl mcp sync` writes it into
Claude Code, Codex, and Antigravity configurations.

---

## 6. Verification and testing

1. **Wire-format population guard (`crates/core/src/wire.rs`):**
   * `Kind` gains `State` and `Agent`.
   * New `wire_names!` + `wire_parse!` enums: `StateConcern`, `MemoryCategory`,
     `MemoryBasis`, `MemoryLifecycle`, `MergeMode`, `AgentStatus`.
   * New `wire_tags!` enums: `StateScope`, `StatePayload`, `AgentRole`.
   * The enum population floor in `wire.rs` is raised by the exact number of added enums so
     an unlisted enum or variant fails compilation or the wire test suite.
2. **Store conformance (`crates/core/src/conformance.rs`):**
   * Shared conformance tests run against both `MemStore` and `RedbStore` covering:
     * Format 6 upgrade on first `StateStore` or `AgentRegistry` write (and format preservation
       when neither is written).
     * `AgentRegistry`: area CRUD, agent registration, case-insensitive `name_key` lookup,
       `AgentCollision` refusal on active and retired names without `replace`, IRI
       preservation on `replace = true`, `LeadTaken` refusal on second active lead, previous
       lead retirement on `replace = true`, and deterministic `reports_to` derivation.
     * `StateStore`: `Memory`, `Handoff`, and `Runtime` CRUD, sticky `sensitive: true`
       inheritance, `Superseded` target validation, `AppendRequiresArray` validation, revision
       history ordering, fast-forward sync import, true-divergence conflict creation and write
       refusal until `resolve`, automatic `append` and `last_writer` merges, and offline
       same-key `StateId` alias unification (§3.3 rule 3).
3. **Black-box CLI and git-plumbing tests (`crates/cli/tests/state_sync.rs`):**
   * Two isolated machines (two `FL_DB` stores and two git checkouts) syncing against a local
     `git init --bare` remote:
     * **Clean merge:** Machine A writes memory `m-a`, Machine B writes handoff `h-b`; both
       run `fl state sync`; both stores converge with zero conflicts.
     * **Working-tree and index isolation:** before and after `fl state sync`, `git status
       --porcelain` and `.git/index` sha256 in the working repository are asserted unchanged,
       and `HEAD` remains on `main`.
     * **Same-key conflict and resolution:** Machine A and Machine B concurrently update the
       same `memory` key; the second `sync` retries the non-fast-forward push, imports the
       first machine's revision, records a `StateConflict`, refuses subsequent `memory set`
       calls on that key, and converges cleanly after `fl state resolve --keep <rev>` + `sync`.
     * **`Runtime` merge modes:** concurrent array updates under `merge = append` and scalar
       updates under `merge = last_writer` converge without entering `state_conflicts`.
     * **Tamper and non-fast-forward detection:** `fl state verify` and `fl state sync` refuse
       a remote `fl/state` branch whose existing segment line was altered or truncated.
     * **Sensitivity refusal:** a `sensitive: true` entry is refused by `fl state sync` when
       `private = false` (or omitted) in `config.toml`, and `git rev-parse refs/heads/fl/state`
       on the bare remote confirms nothing was pushed.
     * **Read-only export and byte cap:** `fl state render --max-bytes 600` places `Handoff`
       and `pinned` / `warning` memories first, respects the 600-byte ceiling, omits
       `sensitive` entries by default, and refuses symlinked `--out` paths.
     * **MCP stdio round-trip:** `fl state mcp` handles `initialize`, `tools/list`, and
       `tools/call` for `agent_register`, `agent_roster`, `state_set_memory`, and
       `state_render`.
