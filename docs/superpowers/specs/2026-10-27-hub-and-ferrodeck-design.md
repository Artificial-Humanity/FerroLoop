# FerroLoop — Sub-project 5: Hub, Architecture Conformance (`fl arch`), and FerroDeck Integration (`fl hub`) Specification

**Date:** 2026-10-27  
**Author:** Ferris (`ferris`, FerroLoop Lead)  
**Status:** Approved by Owner (2026-10-27)

---

## 0. Purpose and Scope

Sub-projects 1–4 and the GitHub-Level Agent Rights Hierarchy delivered the durable state database (`FORMAT_WITH_STATE = 6`), FerroWire agent messaging (`FORMAT_WITH_WIRE = 7`), five-vendor CLI adapters with token/cost/liveness telemetry (`claude`, `codex`, `agy`, `muse`, `grok`), tier-aware pull-request governance, and the directed workflow graph engine (`fl flow`, `FORMAT_WITH_WORKFLOW = 8`).

This specification defines **Sub-project 5: Hub, Architecture Conformance (`fl arch`), and FerroDeck Command-Center Integration (`fl hub`)**:
1. **Declarative Architecture Model, LikeC4 Export, and Conformance Gate (`fl_core::arch`, `fl arch`):** Committed `.fl/architecture.toml` (or `.json`) declaring system components, path globs, package mappings, and allowed dependency edges; deterministic LikeC4 `.c4` DSL generation (`export_likec4_dsl`); and executable code-versus-model conformance checking (`check_architecture_conformance` and `fl arch check`).
2. **Hub Authentication, Authority Tiers, and Static `FerroDeck` Serving (`fl_hub::auth`, `[hub]` config):** Pairing flow (`fl hub pair`, `POST /api/v1/auth/pair`), `HttpOnly; SameSite=Strict` session cookies, optional Tailscale `Tailscale-User-Login` trust, two-tier authority enforcement (`HubAuthority::Owner` vs. `HubAuthority::Peer`), and static asset / SPA serving for **FerroDeck** (`FerroLoop/ui/dist`).
3. **Versioned `/api/v1/*` REST API & Host Discovery (`fl_hub::api`):** Unified JSON REST endpoints backing all `FerroDeck` views:
   - Tab 1 (Structured Chat): `GET /api/v1/agents`, `GET /api/v1/agents/liveness`, `GET /api/v1/sessions`, `GET /api/v1/sessions/:id/transcript`, and `POST /api/v1/sessions/:id/prompt`.
   - Tab 2 (Git Worktrees & PR Inspector): `GET /api/v1/worktrees`, `GET /api/v1/worktrees/:id/diff`, `GET /api/v1/prs`, `POST /api/v1/prs/:number/review`, and `POST /api/v1/prs/:number/merge`.
   - Tab 3 (FerroWire / Bus Mail): `GET /api/v1/wire/messages`, `POST /api/v1/wire/send`, and `POST /api/v1/wire/ack`.
   - Workflows, State, Stats, & Architecture: `/api/v1/workflows`, `/api/v1/workflow-runs`, `/api/v1/state`, `/api/v1/stats`, and `/api/v1/architecture`.
4. **Multiplexed `/ws/v1` Real-Time Event & Terminal Stream, CLI (`fl hub`), and Dual-Mode Deployment:** RFC 6455 WebSocket handler on `/ws/v1` (aliased at `/api/v1/stream`) with `{ channel, event, data }` frames for `wire`, `session`, `workflow`, and Owner-only interactive `terminal` (`xterm.js`) streams, plus `fl hub` CLI subcommands (`serve`, `pair`, `token`, `status`), `contrib/ferroloop-hub.service`, `Dockerfile`, and `compose.yaml`.

---

## 1. Declarative Architecture Model, LikeC4 Export, and Conformance Gate (`crates/core/src/arch.rs`, `crates/cli/src/cmd/arch.rs`)

### 1.1 Domain Types (`fl_core::arch`)

```rust
pub const DEFAULT_ARCH_MANIFEST_PATH: &str = ".fl/architecture.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchComponentKind {
    Crate,
    Service,
    Ui,
    Store,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchComponent {
    pub id: String,
    pub title: String,
    #[serde(default = "default_arch_kind")]
    pub kind: ArchComponentKind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchEdge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchitectureModel {
    pub system_id: String,
    pub title: String,
    #[serde(default)]
    pub strict_coverage: bool,
    #[serde(default)]
    pub components: Vec<ArchComponent>,
    #[serde(default)]
    pub edges: Vec<ArchEdge>,
}
```

### 1.2 Static Validation (`validate_architecture_model`)

`validate_architecture_model(model: &ArchitectureModel) -> Result<(), ArchError>` verifies:
1. `system_id` is a valid slug (`validate_slug(&model.system_id)`).
2. `title` is non-empty after trimming.
3. `components` is non-empty, each `component.id` is a valid unique slug (`validate_slug(&component.id)`), and `component.title` is non-empty.
4. Every non-`External` component has at least one non-empty path pattern in `paths` or a `package` name.
5. Every `edge` references existing `from` and `to` component IDs, has `from != to` (no self-loops), and contains no duplicate `(from, to)` pairs.

### 1.3 Conformance Checking (`check_architecture_conformance`)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageDep {
    pub from_package: String,
    pub to_package: String,
    pub manifest_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceReference {
    pub from_path: String,
    pub target_component_or_package: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArchViolation {
    UndeclaredDependency {
        from_component: String,
        to_component: String,
        evidence: String,
    },
    UnmappedPackage {
        package: String,
        manifest_path: String,
    },
    UnmappedFile {
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchConformanceReport {
    pub system_id: String,
    pub passed: bool,
    pub checked_packages: usize,
    pub checked_files: usize,
    pub violations: Vec<ArchViolation>,
}
```

`check_architecture_conformance(model: &ArchitectureModel, package_deps: &[PackageDep], source_refs: &[SourceReference], repo_files: &[String]) -> Result<ArchConformanceReport, ArchError>`:
1. Validates `model` via `validate_architecture_model`.
2. Maps each `PackageDep` whose `from_package` and `to_package` belong to components in `model`:
   - If `from_component != to_component` and no `ArchEdge { from: from_component, to: to_component, .. }` exists, records `ArchViolation::UndeclaredDependency`.
   - If `model.strict_coverage` is `true` and a workspace `from_package` is not mapped to any component, records `ArchViolation::UnmappedPackage`.
3. Maps each `SourceReference` (`from_path` matched against component `paths` globs, referencing `target_component_or_package` by component `id` or `package`):
   - If `from_component != to_component` and no declared `ArchEdge` exists from `from_component` to `to_component`, records `ArchViolation::UndeclaredDependency`.
4. If `model.strict_coverage` is `true`, checks each path in `repo_files` against component `paths` globs and records `ArchViolation::UnmappedFile` for any unmatched path.
5. Sets `passed = violations.is_empty()`.

### 1.4 Deterministic LikeC4 Export (`export_likec4_dsl`)

`export_likec4_dsl(model: &ArchitectureModel) -> Result<String, ArchError>` validates `model` and produces a deterministic LikeC4 `.c4` document containing:
- `specification { element system; element crate; element service; element ui; element store; element external; }`
- `model { <system_id> = system '<title>' { ... components and edges ... } }`
- `views { view index of <system_id> { title '<title> — System Landscape'; include *; } }`

### 1.5 CLI Subcommands (`fl arch`)

- `fl arch init [--path <manifest_path>] [--force] [--json]`: Scans Cargo workspace members (or top-level directories) and writes a valid `.fl/architecture.toml`.
- `fl arch show [--path <manifest_path>] [--json]`: Loads, validates, and prints the architecture model.
- `fl arch check [--path <manifest_path>] [--json]`: Scans workspace `Cargo.toml` dependencies and source files, runs `check_architecture_conformance`, prints human or JSON output, and exits `0` when `passed == true` or `1` when `passed == false`.
- `fl arch likec4 [--path <manifest_path>] [--output <file>]`: Prints or writes the LikeC4 `.c4` DSL export.

---

## 2. Hub Configuration, Authentication, Authority Tiers, and Static Serving (`crates/hub`)

### 2.1 `[hub]` Configuration (`crates/cli/src/config.rs`)

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubConfig {
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub ui_dir: Option<PathBuf>,
    #[serde(default)]
    pub owner_token_file: Option<PathBuf>,
    #[serde(default)]
    pub trust_tailscale_headers: bool,
    #[serde(default)]
    pub enable_terminal: Option<bool>,
    #[serde(default)]
    pub session_ttl_hours: Option<u64>,
}
```

### 2.2 Authority Tiers & Pairing (`fl_hub::auth`)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum HubAuthority {
    Owner { login: String },
    Peer { agent: Option<String> },
}
```

1. **Owner Token Resolution:**
   - Reads `$FL_HUB_OWNER_TOKEN` (if non-empty) or `[hub].owner_token_file` (trimmed).
2. **One-Time Pairing Codes:**
   - Stored in the workspace state store as a sensitive runtime entry (`StateScope::Workspace`, key `hub/pairing/<code>`) or in-memory pairing table with an expiration timestamp (`expires_at_ms`) and `used: bool`.
   - `fl hub pair` creates a 10-minute single-use code, persists it in the workspace state store so a running `fl hub serve` process sharing the store recognizes it immediately, and prints `http://<listen>/pair?code=<code>` (or `/?pair=<code>`).
3. **Request Authentication Order:**
   - `POST /api/v1/auth/pair` accepts `{ "code": "..." }` or `{ "token": "..." }`. When valid, it mints a random 256-bit session token, stores the session with `expires_at`, and sets `Set-Cookie: fl_hub_session=<token>; HttpOnly; SameSite=Strict; Path=/; Max-Age=<ttl_secs>` and returns `{ "authenticated": true, "authority": { "tier": "owner", "login": "lmcfarlin" }, "session_token": "<token>" }`.
   - Every `/api/v1/*` and `/ws/v1` request (except `/api/v1/health` and `POST /api/v1/auth/pair`) checks:
     1. Cookie `fl_hub_session=<token>` or `Authorization: Bearer <session_token>` matching an active owner session -> `HubAuthority::Owner`.
     2. `Authorization: Bearer <owner_token>` matching the configured owner token (constant-time comparison) -> `HubAuthority::Owner`.
     3. Header `Tailscale-User-Login` when `trust_tailscale_headers == true` -> `HubAuthority::Owner { login }`.
     4. `Authorization: Bearer <wire_token>` matching `[wire].token_file` or `$FL_WIRE_TOKEN` (constant-time comparison) -> `HubAuthority::Peer { agent: header_x_fl_agent }`.
     5. Otherwise returns HTTP `401 Unauthorized` with `{ "error": "unauthorized" }`.

### 2.3 Static `FerroDeck` Serving
- For `GET` and `HEAD` requests outside `/api/*` and `/ws/*`:
  - Resolves path relative to `ui_dir` (`FerroLoop/ui/dist`), rejecting any path containing `..` or null bytes with `400 Bad Request`.
  - If the file exists in `ui_dir`, serves it with matching `Content-Type`.
  - Otherwise, if `ui_dir/index.html` exists, serves `index.html` (`200 OK`, `text/html; charset=utf-8`) for SPA routing.
  - If `ui_dir` is not configured or does not exist, `GET /` serves a built-in HTML status page (`200 OK`) displaying `"FerroDeck Hub Online"`.

---

## 3. Versioned `/api/v1/*` REST API (`crates/hub/src/api.rs`)

All `/api/v1/*` responses return `application/json`.

1. **Health & Auth:**
   - `GET /api/v1/health`: Unauthenticated health check `{ "status": "ok", "service": "fl-hub" }`.
   - `POST /api/v1/auth/pair`: Exchanges `{ "code" | "token" }` for an owner session cookie + token.
   - `GET /api/v1/auth/session`: Returns `{ "authenticated": true, "authority": HubAuthority }`.
   - `POST /api/v1/auth/logout`: Invalidates the current session cookie/token.
2. **Agents, Liveness & Structured Chat (`FerroDeck` Tab 1):**
   - `GET /api/v1/agents`: Returns `{ "agents": Vec<AgentRecord>, "areas": Vec<AreaRecord>, "routes": Vec<DiscoveredSessionSummary> }`.
   - `GET /api/v1/agents/liveness`: Returns per-agent liveness evaluation (`evaluate_liveness`) combining `RuntimeEntry` (`agent/liveness`) and discovered host sessions.
   - `POST /api/v1/agents/:name/recast` (Owner only, else `403`): Accepts `{ "project", "role", "extra_areas"?, "create_area"?, "replace_lead"? }` and moves an existing agent to another project or role. It keeps `id`, `display_name`, `email` and (when `extra_areas` is omitted) the extra areas. Unknown agent → `404`; agent not `active` → `409` (reactivate it first); missing area without `create_area: true` → `409` with `missing_areas`; another active Lead of the project when the new role is `lead` and `replace_lead` is not `true` → `409` with `current_lead` (when both apply, one `409` carries both and nothing is written). With `replace_lead: true` that Lead is retired. Returns `{ "agent": AgentRecord, "retires_lead": string | null, "creates_areas": string[], "retired_lead"?: AgentRecord }` and broadcasts `agents:recast` with `{ agent, from, to, retires_lead, creates_areas, retired_lead? }`. It changes only the registry, not sessions, routes or wire state. Recasting a project's only Lead away leaves the project with no Lead, and PR policy then falls back to the LabManager tier. Recast does not enforce LabManager uniqueness (the store has no such rule; PR policy uses the first active `lab_manager`).
   - `GET /api/v1/sessions`: Returns active interactive sessions (from `discover_route_table` and `~/.gemini/antigravity-cli/brain/*/`, `~/.claude/`, `~/.codex/`) plus recorded store `Attempt` runs.
   - `GET /api/v1/sessions/:id/transcript`: Parses and returns structured turns:
     ```rust
     pub struct TranscriptTurn {
         pub index: usize,
         pub role: String, // "user" | "assistant" | "system"
         pub content: String,
         pub thinking: Option<String>,
         pub tool_calls: Vec<TranscriptToolCall>,
         pub created_at: Option<String>,
     }
     ```
   - `POST /api/v1/sessions/:id/prompt`: Accepts `{ "prompt": String, "by": Option<String>, "urgency": Option<MessageUrgency> }`, records a FerroWire message to the target agent/session, and dispatches it via `fl-wire`'s vendor push dispatcher.
3. **Git Worktrees & Pull-Request Inspector (`FerroDeck` Tab 2):**
   - `GET /api/v1/worktrees`: Runs `git worktree list --porcelain` on the project repository and returns each worktree's `id` (URL-safe slug/basename), `path`, `head`, `branch`, `dirty_files: Vec<String>`, and `is_clean: bool`.
   - `GET /api/v1/worktrees/:id/diff`: Runs `git status --porcelain` and `git diff HEAD` inside the matching worktree and returns `{ "id", "path", "branch", "head", "files": Vec<WorktreeFileStatus>, "diff": String }`.
   - `GET /api/v1/prs`: Lists open pull requests (when a GitHub tracker is configured) or returns `{ "prs": [] }` for local-only stores, enriched with `PrPolicyEvaluation`.
   - `POST /api/v1/prs/:number/review` & `POST /api/v1/prs/:number/merge`: Executes tier-checked PR review or author-owned PR merge (`authorize_agent_action` / `authorize_agent_merge` + `verify_merge_approvals`).
4. **FerroWire Bus (`FerroDeck` Tab 3):**
   - `GET /api/v1/wire/messages`: Query params `?to=<agent>&unread=<bool>&thread=<iri>`, returns `{ "messages": Vec<WireInboxItem> }` (or all messages when `to` is omitted).
   - `POST /api/v1/wire/send`: Sends a FerroWire message (`SendWireArgs`) and broadcasts a `wire:message_sent` event over `/ws/v1`.
   - `POST /api/v1/wire/ack`: Acknowledges a message (`{ "id": "<id_or_handle>", "by": "<agent>" }`) and broadcasts `wire:message_acked`.
5. **Workflows (`fl flow`), State, Stats & Architecture:**
   - `GET /api/v1/workflows`: Lists `WorkflowDef` definitions (`list_workflow_defs`).
   - `GET /api/v1/workflow-runs`: Lists `WorkflowRun` records (`list_workflow_runs`).
   - `GET /api/v1/workflow-runs/:id`: Returns a `WorkflowRun` and its curated step handoffs (`StateScope::Workflow`).
   - `POST /api/v1/workflow-runs`: Starts a workflow run (`start_workflow_run`) and broadcasts `workflow:run_updated`.
   - `POST /api/v1/workflow-runs/:id/handoff`: Records a curated step handoff (`record_step_handoff`).
   - `POST /api/v1/workflow-runs/:id/approve` & `POST /api/v1/workflow-runs/:id/reject`: Enforces `authorize_step_sign_off` (requiring `HubAuthority::Owner` when `ApprovalTier::Owner` is required; `HubAuthority::Peer` refused with `403` for `Owner`-tier approvals), advances the workflow run, and broadcasts `workflow:run_updated`.
   - `POST /api/v1/workflow-runs/:id/abort`: Aborts a workflow run.
   - `GET /api/v1/state` & `POST /api/v1/state`: Lists or upserts state entries (`StateStore`), hiding `sensitive: true` values unless the caller has `HubAuthority::Owner`.
   - `GET /api/v1/stats`: Returns adapter attempt token/cost totals and `audit_regret` reports.
   - `GET /api/v1/architecture`: Loads `.fl/architecture.toml` (if present), runs `check_architecture_conformance` and `export_likec4_dsl`, and returns `{ "model": Option<ArchitectureModel>, "likec4_dsl": Option<String>, "conformance": Option<ArchConformanceReport> }`.

---

## 4. Multiplexed `/ws/v1` WebSocket, Interactive Terminal, CLI (`fl hub`), and Packaging

### 4.1 WebSocket Envelope & Event Broadcast (`/ws/v1` & `/api/v1/stream`)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubWsFrame {
    pub channel: String,
    pub event: String,
    #[serde(default)]
    pub data: serde_json::Value,
}
```

- Upon connection, the server sends `{ "channel": "system", "event": "connected", "data": { "authority": authority } }`.
- Server broadcast hub (`tokio::sync::broadcast::Sender<HubWsFrame>`) pushes `wire`, `session`, and `workflow` frames to all connected WebSocket clients.

### 4.2 Interactive Terminal Channel (`terminal`, `HubAuthority::Owner` only)

- Requires `HubAuthority::Owner` and `enable_terminal == true`. Any `terminal` frame sent by `HubAuthority::Peer` or when `enable_terminal == false` receives:
  `{ "channel": "terminal", "event": "error", "data": { "status": 403, "message": "owner authority required for terminal sessions" } }`.
- Supported client events on `"channel": "terminal"`:
  - `"open"`: `{ "session_id": "term-1", "cwd": "/optional/path", "cols": 80, "rows": 24, "command": ["/bin/bash", "-i"] }` -> spawns an interactive shell/process (via `script -qefc` / PTY or piped shell process), streams stdout/stderr chunks back as `{ "channel": "terminal", "event": "output", "data": { "session_id": "term-1", "data": "..." } }`, and emits `"opened"` / `"exited"`.
  - `"input"`: `{ "session_id": "term-1", "data": "ls -la\n" }` -> writes bytes to the session's stdin.
  - `"resize"`: `{ "session_id": "term-1", "cols": 120, "rows": 40 }` -> updates terminal dimensions.
  - `"close"`: `{ "session_id": "term-1" }` -> terminates the session process.

### 4.3 CLI Subcommands (`crates/cli/src/cmd/hub.rs`)

- `fl hub serve [--listen <addr>] [--ui-dir <path>] [--owner-token-file <path>] [--trust-tailscale-headers] [--no-terminal]`
- `fl hub pair [--url <base_url>] [--ttl-secs <secs>] [--json]`
- `fl hub token [--generate] [--output <path>] [--json]`
- `fl hub status [--url <base_url>] [--json]`

### 4.4 Dual-Mode Deployment Artifacts

- `.fl/architecture.toml`: Committed architecture manifest for `FerroLoop` itself (`fl_core`, `fl_store`, `fl_github`, `fl_mcp`, `fl_exec`, `fl_wire`, `fl_hub`, `fl_cli`, `ferrodeck_ui`) so `fl arch check` passes on the FerroLoop repository out of the box.
- `Dockerfile` and `compose.yaml`: Multi-stage container build for `fl hub serve` on port `8420`.
- `contrib/ferroloop-hub.service`: Systemd user service template for `ai-lab-0`.

---

## 5. Testing Strategy

1. **`fl-core` Unit Tests (`crates/core/src/arch.rs`):**
   - `validate_architecture_model` tests for duplicate components, self-loop edges, unknown edge endpoints, and invalid slugs.
   - `check_architecture_conformance` tests for clean pass, `UndeclaredDependency` detection from Cargo package deps and source references, and `strict_coverage` unmapped package/file detection.
   - `export_likec4_dsl` deterministic output snapshot test.
2. **`fl-hub` Unit & Integration Tests (`crates/hub/src/lib.rs`):**
   - Pairing and session cookie exchange (`POST /api/v1/auth/pair`), `Tailscale-User-Login` trust, and `401` refusal on unauthenticated requests.
   - Authority boundary enforcement: `Peer` token can read `/api/v1/workflows`, send/ack `/api/v1/wire/send`, and record handoffs, but gets `403 Forbidden` on `Owner`-tier step sign-off and `/ws/v1` `terminal` frames.
   - Static `ui_dir` serving, SPA fallback to `index.html`, and `..` path traversal refusal.
   - Live `/ws/v1` event broadcast (`wire:message_sent`, `workflow:run_updated`) and interactive `terminal` open/input/output/close round-trip.
3. **`fl-cli` End-to-End Tests (`crates/cli/tests/hub.rs`):**
   - `fl arch init`, `fl arch show`, `fl arch check` (against FerroLoop's `.fl/architecture.toml`), and `fl arch likec4`.
   - `fl hub token --generate`, `fl hub pair`, `fl hub serve` (ephemeral port), `fl hub status`, REST API round-trips (`/api/v1/agents`, `/api/v1/worktrees`, `/api/v1/wire/send`, `/api/v1/workflow-runs`, `/api/v1/architecture`), and `/ws/v1` WebSocket streaming.
