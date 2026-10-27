# Hub (`fl hub`), Architecture Conformance (`fl arch`), and FerroDeck Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver Sub-project 5: the declarative architecture model, LikeC4 `.c4` exporter, and conformance gate (`fl_core::arch`, `fl arch`); the `crates/hub` (`fl-hub`) HTTP + WebSocket daemon serving `FerroDeck` (`ui/dist/`), tiered session/peer authentication, `/api/v1/*` REST endpoints, and `/ws/v1` live event & Owner-only `xterm.js` terminal streams; plus `fl hub` CLI subcommands, `.fl/architecture.toml`, `Dockerfile`, `compose.yaml`, and `contrib/ferroloop-hub.service`.

**Architecture:** We add `crates/core/src/arch.rs` for pure architecture validation, conformance checking, and LikeC4 `.c4` export; introduce a new workspace crate `crates/hub` (`fl-hub`) implementing the Tokio HTTP/1.1 + RFC 6455 WebSocket server (`auth`, `discovery`, `api`, `ws`, `terminal`); and wire `fl arch` and `fl hub` subcommands and `[hub]` config into `crates/cli`.

**Tech Stack:** Rust 2024 (`1.98`), `serde`, `serde_json`, `toml`, `tokio`, `globset`, `sha2`, `base64`, `clap`, `ureq`.

**Spec:** `docs/superpowers/specs/2026-10-27-hub-and-ferrodeck-design.md`

---

### Task 1: Declarative Architecture Model, Conformance Engine, and LikeC4 Export (`crates/core`)

**Files:**
- Create: `crates/core/src/arch.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/core/tests/wire_refs.rs`

**Interfaces:**
- Consumes: `validate_slug`, `wire_names!`, `wire_parse!`, `globset::{Glob, GlobSetBuilder}`.
- Produces in `crates/core/src/arch.rs` (re-exported in `crates/core/src/lib.rs`):
  - `DEFAULT_ARCH_MANIFEST_PATH` (`".fl/architecture.toml"`)
  - `ArchComponentKind` (`Crate`, `Service`, `Ui`, `Store`, `External`) with `wire_names!` and `wire_parse!`
  - `ArchComponent`, `ArchEdge`, `ArchitectureModel`
  - `PackageDep`, `SourceReference`, `ArchViolation`, `ArchConformanceReport`, `ArchError`
  - `validate_architecture_model(model: &ArchitectureModel) -> Result<(), ArchError>`
  - `check_architecture_conformance(model: &ArchitectureModel, package_deps: &[PackageDep], source_refs: &[SourceReference], repo_files: &[String]) -> Result<ArchConformanceReport, ArchError>`
  - `export_likec4_dsl(model: &ArchitectureModel) -> Result<String, ArchError>`

- [ ] **Step 1: Implement `crates/core/src/arch.rs` with full validation, conformance engine, LikeC4 `.c4` generator, and unit tests**

Implement all types and functions from Spec §1.1–§1.4 in `crates/core/src/arch.rs`:
- `validate_architecture_model`:
  - Validates `system_id` via `validate_slug`.
  - Requires non-empty `title` and non-empty `components`.
  - Validates each `component.id` via `validate_slug`, checks for duplicate component IDs, and requires non-empty `component.title`.
  - For every component where `kind != ArchComponentKind::External`, requires `!component.paths.is_empty() || component.package.as_ref().is_some_and(|p| !p.trim().is_empty())`, and validates that each glob pattern in `paths` compiles via `globset::Glob::new`.
  - Validates every `edge`: `from` and `to` must exist in `components`, `from != to` (no self-loop), and no duplicate `(from, to)` edges.
- `check_architecture_conformance`:
  - Calls `validate_architecture_model(model)?`.
  - Maps packages (`component.package`) and file paths (`component.paths` globs) to component IDs.
  - Checks `package_deps`: for any dependency between two mapped components `from_c != to_c`, verifies an `ArchEdge { from: from_c, to: to_c, .. }` exists; otherwise pushes `ArchViolation::UndeclaredDependency`. If `model.strict_coverage` is `true` and `dep.from_package` is not mapped to any component, pushes `ArchViolation::UnmappedPackage` (deduplicated by package name).
  - Checks `source_refs`: matches `from_path` against component `paths` globs and resolves `target_component_or_package` by component `id` or `package`; if `from_c != to_c` and no `ArchEdge` exists, pushes `ArchViolation::UndeclaredDependency`.
  - If `model.strict_coverage` is `true`, checks each file in `repo_files` against all component `paths` globs and pushes `ArchViolation::UnmappedFile` for any file that matches no component.
- `export_likec4_dsl`:
  - Calls `validate_architecture_model(model)?` and renders deterministic LikeC4 `.c4` DSL (`specification`, `model`, `views`).
- Add unit tests in `crates/core/src/arch.rs`:
  - `validation_rejects_duplicate_components_self_loops_and_unknown_edges`
  - `conformance_detects_undeclared_package_and_source_dependencies_and_strict_unmapped`
  - `export_likec4_dsl_renders_deterministic_c4_specification_model_and_views`

- [ ] **Step 2: Export `arch` in `crates/core/src/lib.rs` and add wire round-trip test in `crates/core/tests/wire_refs.rs`**

In `crates/core/src/lib.rs`, add `pub mod arch;` and re-export all public items from `arch`.
In `crates/core/tests/wire_refs.rs`, add `arch_wire_types_round_trip_cleanly` testing `ArchitectureModel`, `ArchComponentKind`, `ArchViolation`, and `ArchConformanceReport`.

- [ ] **Step 3: Run `fl-core` tests and clippy**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test -p fl-core && cargo clippy -p fl-core --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 4: Commit Task 1**

```bash
git add crates/core/src/arch.rs crates/core/src/lib.rs crates/core/tests/wire_refs.rs
git commit -m "feat(core): add declarative architecture model, conformance engine, and LikeC4 export

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

### Task 2: `crates/hub` (`fl-hub`) Authentication, Host Discovery, Static `FerroDeck` Serving, and `/api/v1/*` REST API

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/hub/Cargo.toml`
- Create: `crates/hub/src/auth.rs`
- Create: `crates/hub/src/discovery.rs`
- Create: `crates/hub/src/api.rs`
- Create: `crates/hub/src/lib.rs`

**Interfaces:**
- Consumes: `fl_core`, `fl_store::RedbStore`, `fl_exec`, `fl_wire`, `fl_github`.
- Produces in `crates/hub`:
  - `HubServerConfig`, `HubAuthority`, `AuthManager`, `PairingInfo`, `generate_hex_token`
  - `DiscoveredSessionSummary`, `TranscriptTurn`, `TranscriptToolCall`, `WorktreeSummary`, `WorktreeDiff`, `discover_sessions`, `read_session_transcript`, `list_git_worktrees`, `read_worktree_diff`, `scan_cargo_architecture_conformance`
  - `HubState`, `handle_http_request` serving `/api/v1/*` and static `ui_dir` assets

- [ ] **Step 1: Create `crates/hub/Cargo.toml` and register `crates/hub` in root `Cargo.toml`**

Add `"crates/hub"` to `[workspace].members` in root `Cargo.toml`.
Create `crates/hub/Cargo.toml` with dependencies on `fl-core`, `fl-store`, `fl-exec`, `fl-wire`, `fl-github`, `anyhow`, `base64`, `globset`, `serde`, `serde_json`, `sha2`, `tokio`, `toml`, and `uuid`.

- [ ] **Step 2: Implement `crates/hub/src/auth.rs` (`HubAuthority`, pairing codes, session cookies, Tailscale header trust, constant-time token check)**

Implement Spec §2.2:
- `HubAuthority::{Owner { login }, Peer { agent }}`
- `AuthManager`:
  - Resolves owner token (`owner_token: Option<String>`) and wire peer token (`peer_token: Option<String>`).
  - Mints and verifies one-time pairing codes persisted in the workspace `RedbStore` under `StateScope::Workspace`, key `"hub/pairing/<code>"` (as a `RuntimeEntry` with `expires_at_ms` and `used: bool`) so a CLI `fl hub pair` invocation immediately registers a code usable against a running `fl hub serve` daemon!
  - Mints and verifies `fl_hub_session` cookies / session bearer tokens stored in `StateScope::Workspace` (`"hub/session/<token_hash>"`) and cached in memory.
  - Authenticates HTTP/WS requests via:
    1. `Cookie: fl_hub_session=<token>` or `Authorization: Bearer <session_token>` -> `HubAuthority::Owner`
    2. `Authorization: Bearer <owner_token>` (constant-time equality) -> `HubAuthority::Owner`
    3. `Tailscale-User-Login` header when `trust_tailscale_headers == true` -> `HubAuthority::Owner { login }`
    4. `Authorization: Bearer <peer_token>` (constant-time equality) -> `HubAuthority::Peer { agent }`
    5. Otherwise returns `None` (`401 Unauthorized`).

- [ ] **Step 3: Implement `crates/hub/src/discovery.rs` (transcript parser, git worktree inspector, Cargo workspace conformance scanner)**

Implement:
- `discover_sessions(store: &RedbStore)`: Combines `fl_wire::discover_route_table()` active sessions and store `Attempt` records into `Vec<DiscoveredSessionSummary>`.
- `read_session_transcript(session_id: &str, store: &RedbStore)`: Reads JSONL transcripts (Antigravity `transcript.jsonl`, Claude JSONL, Codex JSONL, or recorded `Attempt` output) and normalizes them into `Vec<TranscriptTurn>` (`index`, `role`, `content`, `thinking`, `tool_calls`, `created_at`).
- `list_git_worktrees(repo_root: &Path)` and `read_worktree_diff(repo_root: &Path, worktree_id: &str)`: Runs `git worktree list --porcelain`, `git status --porcelain`, and `git diff HEAD`.
- `scan_workspace_for_arch(repo_root: &Path)`: Parses workspace and crate `Cargo.toml` files to extract `Vec<PackageDep>` and walks source files to extract `Vec<SourceReference>` and `Vec<String>` repo files for `check_architecture_conformance`.

- [ ] **Step 4: Implement `crates/hub/src/api.rs` and `crates/hub/src/lib.rs` (all `/api/v1/*` routes and static `FerroDeck` serving)**

Implement all REST endpoints from Spec §3 and static file serving from Spec §2.3:
- `GET /api/v1/health`
- `POST /api/v1/auth/pair`, `GET /api/v1/auth/session`, `POST /api/v1/auth/logout`
- `GET /api/v1/agents`, `GET /api/v1/agents/liveness`
- `GET /api/v1/sessions`, `GET /api/v1/sessions/:id/transcript`, `POST /api/v1/sessions/:id/prompt`
- `GET /api/v1/worktrees`, `GET /api/v1/worktrees/:id/diff`
- `GET /api/v1/prs`, `POST /api/v1/prs/:number/review`, `POST /api/v1/prs/:number/merge`
- `GET /api/v1/wire/messages`, `POST /api/v1/wire/send`, `POST /api/v1/wire/ack`
- `GET /api/v1/workflows`, `GET /api/v1/workflow-runs`, `GET /api/v1/workflow-runs/:id`, `POST /api/v1/workflow-runs`, `POST /api/v1/workflow-runs/:id/step`, `POST /api/v1/workflow-runs/:id/handoff`, `POST /api/v1/workflow-runs/:id/approve`, `POST /api/v1/workflow-runs/:id/reject`, `POST /api/v1/workflow-runs/:id/abort`
- `GET /api/v1/state`, `POST /api/v1/state` (redacting `sensitive: true` entries for `HubAuthority::Peer` and refusing `sensitive: true` writes from `HubAuthority::Peer` with `403 Forbidden`)
- `GET /api/v1/stats`
- `GET /api/v1/architecture`
- Static file serving for `ui_dir` with `..` / null-byte path traversal rejection (`400 Bad Request`), SPA fallback to `index.html`, and built-in HTML status fallback when `ui_dir` is absent.

- [ ] **Step 5: Run `fl-hub` tests and clippy**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test -p fl-hub && cargo clippy -p fl-hub --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit Task 2**

```bash
git add Cargo.toml Cargo.lock crates/hub
git commit -m "feat(hub): add fl-hub auth, discovery, static FerroDeck server, and /api/v1 REST endpoints

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

### Task 3: Multiplexed `/ws/v1` WebSocket Event Bus & Owner-Scoped Interactive Terminal (`crates/hub`)

**Files:**
- Create: `crates/hub/src/ws.rs`
- Create: `crates/hub/src/terminal.rs`
- Modify: `crates/hub/src/api.rs`
- Modify: `crates/hub/src/lib.rs`

**Interfaces:**
- Consumes: `HubAuthority`, `HubState`, `tokio::net::TcpStream`, `tokio::process::Command`, `tokio::sync::broadcast`.
- Produces:
  - `HubWsFrame { channel, event, data }`
  - RFC 6455 WebSocket upgrade and connection loop on `/ws/v1` and `/api/v1/stream`
  - Live broadcast of `wire:message_sent`, `wire:message_acked`, `session:prompt_sent`, and `workflow:run_updated`
  - `terminal` channel handler (`open`, `input`, `resize`, `close` -> `opened`, `output`, `exited`) restricted to `HubAuthority::Owner` when `enable_terminal == true`
  - `HubServer::bind` and `HubServer::serve`

- [ ] **Step 1: Implement `crates/hub/src/terminal.rs` and `crates/hub/src/ws.rs`**

In `crates/hub/src/terminal.rs`:
- Manage per-WebSocket active terminal child processes (`BTreeMap<String, TerminalSession>`).
- Enforce `HubAuthority::Owner` and `enable_terminal == true`; if a `Peer` sends any `terminal` frame or `enable_terminal == false`, reply with `{ "channel": "terminal", "event": "error", "data": { "status": 403, "message": "owner authority required for terminal sessions" } }`.
- Support `"open"` (spawning `/bin/sh` or specified command in `cwd`, streaming stdout/stderr chunks as `"output"` frames and process exit as `"exited"`), `"input"` (writing to child stdin), `"resize"`, and `"close"` (killing child process).

In `crates/hub/src/ws.rs`:
- Perform RFC 6455 handshake on `GET /ws/v1` and `GET /api/v1/stream` after verifying `HubAuthority` (accepting session cookie, `Authorization: Bearer`, `?token=`, or trusted `Tailscale-User-Login`).
- Send initial `{ "channel": "system", "event": "connected", "data": { "authority": authority } }` frame.
- Forward broadcast `HubWsFrame` events (`wire`, `session`, `workflow`) to the client and handle incoming `system:ping` (`system:pong`) and `terminal:*` frames.

- [ ] **Step 2: Add comprehensive integration tests in `crates/hub/src/lib.rs`**

Add integration tests in `crates/hub`:
1. `pairing_cookie_tailscale_and_peer_authority_boundaries`:
   - Tests `401` on unauthenticated request, `fl hub pair` code exchange at `POST /api/v1/auth/pair`, `Set-Cookie: fl_hub_session=...`, `GET /api/v1/auth/session`, `Tailscale-User-Login` header trust, and `Peer` token `403` refusal on `Owner`-tier workflow approval and sensitive state writes.
2. `static_ferrodeck_serving_spa_fallback_and_traversal_refusal`:
   - Tests static asset serving from `ui_dir`, SPA fallback to `index.html` for `/rooms/main`, and `400` refusal on `/..%2fsecret`.
3. `websocket_broadcast_and_owner_only_terminal_session`:
   - Connects to `/ws/v1` as `Owner` and as `Peer`.
   - Verifies `POST /api/v1/wire/send` broadcasts `wire:message_sent` to connected WebSocket clients.
   - Verifies `Peer` sending `terminal:open` receives `403` error frame.
   - Verifies `Owner` sending `terminal:open`, `terminal:input` (`echo hello_ferrodeck\n`), and `terminal:close` receives `terminal:opened` and `terminal:output` containing `hello_ferrodeck`.

- [ ] **Step 3: Run `fl-hub` test suite and clippy**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test -p fl-hub && cargo clippy -p fl-hub --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 4: Commit Task 3**

```bash
git add crates/hub
git commit -m "feat(hub): add multiplexed /ws/v1 event bus and Owner-scoped terminal streaming

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

### Task 4: CLI `fl arch` & `fl hub` Subcommands, `[hub]` Config, `.fl/architecture.toml`, Deployment Artifacts, and E2E Tests (`crates/cli`)

**Files:**
- Modify: `crates/cli/Cargo.toml`
- Modify: `crates/cli/src/config.rs`
- Create: `crates/cli/src/cmd/arch.rs`
- Create: `crates/cli/src/cmd/hub.rs`
- Modify: `crates/cli/src/cmd/mod.rs`
- Modify: `crates/cli/src/main.rs`
- Create: `.fl/architecture.toml`
- Create: `Dockerfile`
- Create: `compose.yaml`
- Create: `contrib/ferroloop-hub.service`
- Create: `crates/cli/tests/hub.rs`

**Interfaces:**
- Consumes: `fl_core::arch::*`, `fl_hub::*`, `Config`.
- Produces:
  - `HubConfig` (`[hub]`) on `Config` in `crates/cli/src/config.rs`
  - `fl arch` (`init`, `show`, `check`, `likec4`) in `crates/cli/src/cmd/arch.rs`
  - `fl hub` (`serve`, `pair`, `token`, `status`) in `crates/cli/src/cmd/hub.rs`
  - Committed `.fl/architecture.toml`, `Dockerfile`, `compose.yaml`, and `contrib/ferroloop-hub.service`
  - End-to-end integration tests in `crates/cli/tests/hub.rs`

- [ ] **Step 1: Add `fl-hub` dependency to `crates/cli/Cargo.toml` and `HubConfig` (`[hub]`) to `crates/cli/src/config.rs`**

Add `fl-hub = { path = "../hub" }` to `crates/cli/Cargo.toml`.
Add `pub struct HubConfig` and `pub hub: Option<HubConfig>` to `Config` and `File` in `crates/cli/src/config.rs` (with unit tests verifying `[hub]` parsing).

- [ ] **Step 2: Implement `crates/cli/src/cmd/arch.rs` and commit `.fl/architecture.toml`**

Implement `ArchCmd` (`Init`, `Show`, `Check`, `Likec4`) in `crates/cli/src/cmd/arch.rs`:
- `fl arch init [--path <path>] [--force] [--json]`
- `fl arch show [--path <path>] [--json]`
- `fl arch check [--path <path>] [--json]` (exits `1` when `report.passed == false`, `0` when `true`)
- `fl arch likec4 [--path <path>] [--output <path>]`
Create `.fl/architecture.toml` describing `FerroLoop`'s components (`fl_core`, `fl_store`, `fl_github`, `fl_mcp`, `fl_exec`, `fl_wire`, `fl_hub`, `fl_cli`, `ferrodeck_ui`) and their allowed dependency edges so `fl arch check` passes on `FerroLoop` itself!

- [ ] **Step 3: Implement `crates/cli/src/cmd/hub.rs`, wire `Cmd::Arch` and `Cmd::Hub` in `crates/cli/src/main.rs`, and add `Dockerfile`, `compose.yaml`, and `contrib/ferroloop-hub.service`**

Implement `HubCmd` (`Serve`, `Pair`, `Token`, `Status`) in `crates/cli/src/cmd/hub.rs`.
Wire `pub mod arch;` and `pub mod hub;` in `crates/cli/src/cmd/mod.rs` and `Cmd::Arch` / `Cmd::Hub` in `crates/cli/src/main.rs`.
Create `Dockerfile`, `compose.yaml`, and `contrib/ferroloop-hub.service`.

- [ ] **Step 4: Create `crates/cli/tests/hub.rs` end-to-end tests**

Create `crates/cli/tests/hub.rs` (with isolated `HOME` and removed ambient `AGENT_PERSONA`, `CLAUDE_CODE_MESSAGING_SOCKET`, `ANTIGRAVITY_CONVERSATION_ID`, `CODEX_THREAD_ID` env vars) testing:
1. `fl_arch_init_show_check_and_likec4_export_end_to_end`:
   - Runs `fl arch check --json` against the repository's `.fl/architecture.toml` and verifies `passed == true`.
   - Tests `fl arch init`, `fl arch show --json`, `fl arch likec4`, and verifies `fl arch check` exits `1` when an undeclared crate dependency is introduced in a temp workspace.
2. `fl_hub_token_pair_serve_status_and_rest_api_end_to_end`:
   - Runs `fl hub token --generate`, `fl hub pair --json`, spawns `fl hub serve` on an ephemeral `127.0.0.1:<port>`, verifies `fl hub status --json`, exchanges the pairing code at `POST /api/v1/auth/pair`, and tests `/api/v1/agents`, `/api/v1/worktrees`, `/api/v1/wire/send`, `/api/v1/workflow-runs`, and `/api/v1/architecture`.

- [ ] **Step 5: Run full workspace verification**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: PASS with zero warnings.

- [ ] **Step 6: Commit Task 4**

```bash
git add .fl/architecture.toml Dockerfile compose.yaml contrib/ferroloop-hub.service crates/cli Cargo.lock
git commit -m "feat(cli): add fl arch and fl hub commands, FerroLoop architecture model, and deployment configs

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```
