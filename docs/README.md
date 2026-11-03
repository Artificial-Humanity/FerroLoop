# FerroLoop documentation

Public documentation for FerroLoop. Everything in this directory is published with the
repository. [AGENTS.md](../AGENTS.md) says where the private working notes live and what belongs
in them instead.

File naming in this directory follows
[AGENTS.md § File Naming Conventions](../AGENTS.md#file-naming-conventions).

## Contents

* [getting-started.md](getting-started.md) — build the CLI, gate a real action, and take a
  claim through the finding protocol, with real commands and their output.
* [sharing-gates.md](sharing-gates.md) — share a project's gates with other machines through
  a committed manifest: export, import and check.
* [github-tracker.md](github-tracker.md) — keep a project's records and findings in a GitHub
  repository's Issues: binding, credentials, repair, conflicts and limits.
* [github-ledger.md](github-ledger.md) — publish each decision's evidence to the tracker's
  repository and comment it on its issue: setup, modes, disclosure, comments and their
  recovery, cost, errors and limits.
* [routing.md](routing.md) — keep developer-level items in the local store and human-level
  items in GitHub: areas, the routing map, handles, lists and their limits.
* [mcp.md](mcp.md) — one committed catalog of a project's MCP servers, written into Claude
  Code's, Codex's, Antigravity's, Muse's and Grok's own files (plus `fl mcp serve` stdio MCP
  exposure): the catalog, secrets, each machine's switches, the commands, what fl refuses and
  why.
* [hub-home-and-artifacts.md](hub-home-and-artifacts.md) — where the Hub keeps its data and
  artifact registry, the per-project directories, and the Owner-only endpoints that relocate
  them, with migration.
* [build-provenance.md](build-provenance.md) — how a binary says what it was built from:
  `fl --version [--json]`, `fl version`, `ferrostore --version [--json]`, the Hub's
  `GET /api/v1/version`, the UI bundle's `build-info.json`, and the build-time stamps behind
  them.
* [installer.md](installer.md) — the release manifest and install receipt schemas, the profiles,
  `fl install --dry-run` (the deterministic plan) and `fl install --yes` (the journaled
  transaction: lock, stage, verify, switch `current`, receipt last; resume and rollback),
  the exit codes, and what the installer does not do.
* [doctor.md](doctor.md) — `fl doctor` and the Hub's `GET /api/v1/doctor`: the six observational
  `G7.*` checks, their statuses, the exit codes, the JSON contract, and what doctor never does.
* [vendor-runtimes.md](vendor-runtimes.md) — on-disk session, transcript, context-usage,
  account-quota, and prompt-delivery shapes for Antigravity, Claude Code, Codex, Grok, and
  Muse, and the Hub fields that expose them.
* [vendor-child-env.md](vendor-child-env.md) — the environment a spawned vendor CLI sees: the
  allow-list, each dialect's own credential variables, what is denied, and how to add a variable.

## CLI & Hub capabilities overview

In addition to the guides above, `fl --help` documents the full command surface:

* `fl agent` — agent identity registry, coverage areas, heartbeat liveness, `REGISTRY.md` rendering, and hosted session lifecycle (`spin-up`, two-phase `decommission`, and `remodel`: a move to another vendor that keeps the persona, project, role and worktree, restores the old vendor when the new one fails, and records a `remodel-<persona>` workspace handoff). The hub serves the same lifecycle at `POST /api/v1/sessions/spin-up`, `POST /api/v1/sessions/:id/decommission`, `POST /api/v1/sessions/:id/remodel`, `GET /api/v1/hosted-sessions`, and `POST /api/v1/hosted-sessions/:id/restart`; hosted sessions persist as `hosted-session-*` workspace runtime entries.
* `fl state` — scoped memory, handoffs, and runtime state with owner sensitivity redaction and `MEMORY.md` / `STATE.md` rendering.
* `fl wire` — FerroWire agent-to-agent messaging, receipts, native vendor push dispatch, and WebSocket relay.
* `fl github pr` — pull-request review and merge policy evaluation, structured review markers, and gated merges.
* `fl flow` — DAG workflow validation, import, step execution, review loops, and human approval sign-off.
* `fl arch` — `.fl/architecture.toml` validation, Cargo workspace dependency/source conformance checks, and LikeC4 export.
* `fl hub` — HTTP/1.1 + `/ws/v1` WebSocket daemon (`fl hub serve`), one-time QR/code pairing (`fl hub pair`), PTY terminal streaming, and static serving for **FerroDeck** (`ui/`). The repository its views read (worktrees, diffs, architecture, commands, code search) and where an Owner terminal starts is `--repo-root`, else `[hub].repo_root` (absolute, an existing directory), else the working directory. A request has one of two authorities. **Owner**: a session cookie (from redeeming a `fl hub pair` code, or an operator login), the owner token (`FL_HUB_OWNER_TOKEN` or `[hub].owner_token_file`), or a `Tailscale-User-Login` header when `--trust-tailscale-headers` (or `[hub].trust_tailscale_headers`) is set. **Peer**: a bearer token equal to the FerroWire token (`FL_WIRE_TOKEN` or `[wire].token_file`), naming itself with `X-Fl-Agent`; a Peer is refused Owner-only routes and reads sensitive state redacted. The Hub serves at most 256 connections and 64 WebSocket sessions at once (`503` with `Retry-After` beyond that; Peers hold at most 32 of the sessions), and remembers a rejected session token for 30 seconds so a flood of bad bearers does not open the store each time.

## Root documents

These stay at the repository root and are not duplicated here:

* [README.md](../README.md) — what FerroLoop is, the Ferro Suite, and how to build it.
* [AGENTS.md](../AGENTS.md) — working rules, stack matrix, and conventions.
* [WORKFLOW.md](../WORKFLOW.md) — branching, review, and landing on `main`.
* [PERSONA.md](../PERSONA.md) — the developer persona.
* [LICENSE](../LICENSE) — Apache-2.0.
