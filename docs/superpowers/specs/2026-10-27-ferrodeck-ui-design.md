# FerroLoop — FerroDeck Unified Agent Cockpit UI Specification

**Date:** 2026-10-27  
**Author:** Wesley (`wesley`, operating under `ferris`, FerroLoop Lead)  
**Status:** Draft (Pending Owner Review)

---

## 0. Purpose and Scope

In the Artificial Humanity workspace on `ai-lab-0`, twelve resident agents (`jarvis`, `kayla`, `kacey`, `kage`, `sonya`, `sammy`, `penelope`, `katie`, `lucy`, `felicia`, `ferris`, and `wesley`) operate concurrently across four local CLI harnesses:
- **Muse Code (`muse`):** Multi-client Unix socket daemon (`muse serve --listen`), SQLite session catalog (`~/.local/share/muse/session-index.db`), JSONL session logs (`~/.local/share/muse/sessions/.../session.jsonl`), and cross-session CLI prompt injection (`muse session-message`).
- **Antigravity (`agy`):** Brain JSONL transcripts (`~/.gemini/antigravity-cli/brain/<cid>/.system_generated/logs/transcript.jsonl`) and HTTP/CLI prompt injection (`agy agentapi send-message`).
- **Codex (`codex`):** Session index (`~/.codex/session_index.jsonl`), rollout JSONL transcripts (`~/.codex/sessions/.../rollout-*.jsonl`), and prompt queue (`codex queue --thread <id> --message`).
- **Claude Code (`claude`):** Project JSONL transcripts (`~/.claude/projects/.../<sid>.jsonl`) and Unix domain socket prompt delivery (`agent-bus/dispatcher.py`).

Today, observing or steering all twelve agents from a laptop, iPad, or iPhone requires switching between separate terminal windows, inspecting raw JSONL files, or running manual `agent-msg` and `fl` CLI commands.

This specification defines **FerroDeck** (`FerroLoop/ui/`), the unified remote-control web and PWA cockpit for `ai-lab-0` and the v0 user interface of FerroLoop Sub-project 5:
1. **Live 12-Agent Roster & Liveness Sidebar:** Project-grouped agent roster reading live identity, role (`lead`, `assistant`, `lab_manager`, `custom:<slug>`), active harness (`muse`, `agy`, `codex`, `claude`), session binding, unread mail count, and heartbeat liveness.
2. **Claude-Style Structured Chat View:** Normalized turn-by-turn conversation transcript across all four harnesses with collapsible reasoning/thinking blocks, collapsible tool-call/result cards, live streaming updates, and a unified prompt composer.
3. **Git Worktree Diff & Pull-Request Inspector Tab:** Real-time visibility into active workspace repositories and `/home/lmcfarlin/worktrees/*` branches, unified/split file diffs, open GitHub pull requests, and `fl github pr` governance policy (`RequiredTier`, reviewer status, CI verification, and author merge readiness).
4. **NATS JetStream (`agents.>`) & FerroWire Mail Tab:** Per-agent inbox/outbox, global `agents.all` broadcast feed, message acknowledgement, and direct/broadcast message composer.
5. **Live Raw Terminal Pane (`@xterm/xterm`):** Interactive WebSocket PTY terminal attached to an agent's active session or working directory (`Owner` scope only).
6. **Shared `/api/v1` + `/ws/v1` Contract (Approach A):** Clean separation between the React 19 + TypeScript + Vite SPA (`ui/src/`) and the host API/WebSocket surface (`/api/v1/*` and `/ws/v1`), backed initially by a lightweight TypeScript/Node host bridge (`ui/bridge/`) and designed for drop-in cutover to the pure-Rust `crates/hub` (`fl hub serve`) server with zero frontend code changes.

---

## 1. Repository Layout and Packaging (`FerroLoop/ui/`)

All FerroDeck assets reside in `FerroLoop/ui/` so the Rust workspace (`Cargo.toml`) remains untouched until `crates/hub` embeds `ui/dist/`:

```text
FerroLoop/ui/
├── package.json
├── tsconfig.json
├── vite.config.ts
├── index.html
├── Dockerfile
├── compose.yaml
├── public/
│   ├── manifest.webmanifest
│   ├── icon-192.png
│   ├── icon-512.png
│   └── apple-touch-icon.png
├── src/
│   ├── main.tsx
│   ├── App.tsx
│   ├── styles/
│   │   └── tokens.css
│   ├── types/
│   │   ├── api.ts          # 1:1 TypeScript contract for /api/v1 and /ws/v1
│   │   └── msp.d.ts        # Generated via `muse schema generate-ts`
│   ├── api/
│   │   ├── client.ts       # Typed REST wrapper for /api/v1/*
│   │   └── ws.ts           # Multiplexed /ws/v1 client with auto-reconnect
│   ├── components/
│   │   ├── RosterSidebar.tsx
│   │   ├── ContextHeader.tsx
│   │   ├── ChatPane.tsx
│   │   ├── TurnCard.tsx
│   │   ├── ToolCallBlock.tsx
│   │   ├── ThinkingBlock.tsx
│   │   ├── GitPrPane.tsx
│   │   ├── DiffViewer.tsx
│   │   ├── WireMailPane.tsx
│   │   ├── TerminalPane.tsx
│   │   └── PairingModal.tsx
│   └── __tests__/
│       ├── normalizers.test.ts
│       ├── RosterSidebar.test.tsx
│       ├── ChatPane.test.tsx
│       ├── GitPrPane.test.tsx
│       └── WireMailPane.test.tsx
└── bridge/
    ├── index.ts            # Hono + ws server implementing /api/v1 & /ws/v1
    ├── auth.ts             # Cookie + Tailscale-User-Login + token auth
    ├── registry.ts         # Reads ~/.cache/agent-bus/registry.db & routes.json
    ├── transcripts/
    │   ├── index.ts
    │   ├── muse.ts         # Normalizes Muse session.jsonl & session-index.db
    │   ├── agy.ts          # Normalizes Antigravity transcript.jsonl
    │   ├── codex.ts        # Normalizes Codex rollout-*.jsonl
    │   └── claude.ts       # Normalizes Claude Code <sid>.jsonl
    ├── dispatch.ts         # Prompt delivery via muse/agy/codex/claude/agent-msg
    ├── git.ts              # Worktree discovery, git diff, and `fl github pr`
    ├── wire.ts             # NATS JetStream (`agents.>`) & `agent-msg` bridge
    └── pty.ts              # Owner-scoped PTY session multiplexing for xterm.js
```

---

## 2. Shared `/api/v1` and `/ws/v1` Contract

The frontend (`ui/src/`) communicates exclusively with relative `/api/v1/*` HTTP endpoints and the `/ws/v1` (aliased at `/api/v1/stream`) WebSocket endpoint. Both the initial host bridge (`ui/bridge/`) and `crates/hub` (`fl hub serve`) implement this identical contract.

### 2.1 Authentication and Authorization Scopes

```typescript
export type AuthScope = "owner" | "peer" | "unauthenticated";

export interface AuthSessionResponse {
  authenticated: boolean;
  scope: AuthScope;
  principal: string | null; // e.g., "lmcfarlin" or tailscale login
  tailscale_login?: string | null;
}
```

- `POST /api/v1/auth/pair` — Exchanges a pairing secret/token (`{ token: string }`) or trusted Tailscale identity header (`Tailscale-User-Login`) for an `HttpOnly; SameSite=Strict` session cookie (`fl_session`) with `scope: "owner"`.
- `GET /api/v1/auth/session` — Returns the current `AuthSessionResponse`.
- **Scope Enforcement:**
  - `owner` scope is required for `POST /api/v1/sessions/:id/prompt`, `POST /api/v1/wire/send`, `POST /api/v1/workflow-runs/:id/approve|reject`, and WebSocket `channel: "pty"` frames.
  - `[wire]` bearer tokens receive `peer` scope (cannot open raw PTY terminals or approve Owner-tier workflow gates).

### 2.2 Agent Roster and Liveness (`/api/v1/agents`)

```typescript
export type AgentRole = "lead" | "assistant" | "lab_manager" | `custom:${string}`;
export type HarnessKind = "muse" | "agy" | "codex" | "claude" | "unknown";
export type LivenessState = "active" | "idle" | "stale" | "offline";

export interface AgentSummary {
  name: string;               // e.g., "ferris", "wesley", "jarvis"
  display_name: string;       // e.g., "Ferris", "Wesley", "JARVIS"
  project: string;            // e.g., "FerroLoop", "Website", "AI-Lab-AMD"
  role: AgentRole;
  harness: HarnessKind;
  workdir: string;
  active_session_id: string | null;
  liveness: LivenessState;
  last_seen_at: string | null; // ISO-8601
  unread_mail_count: number;
  active_branch: string | null;
}
```

- `GET /api/v1/agents` — Returns `AgentSummary[]` for all registered agents, ordered by project and role rank (`lab_manager`, `lead`, `assistant`, `custom:*`).
- `GET /api/v1/agents/liveness` — Returns lightweight liveness and active-session heartbeat map keyed by agent name.

### 2.3 Sessions, Normalized Transcripts, and Prompt Dispatch (`/api/v1/sessions`)

```typescript
export interface ToolCallItem {
  id: string;
  name: string;
  arguments_json: string;
  status: "running" | "done" | "error";
  result_text?: string;
}

export interface NormalizedTurn {
  id: string;
  session_id: string;
  agent_name: string;
  harness: HarnessKind;
  role: "user" | "assistant" | "system";
  created_at: string;         // ISO-8601
  content: string;            // Markdown prose
  thinking?: string | null;   // Collapsible reasoning trace
  tool_calls: ToolCallItem[]; // Collapsible tool invocations & outputs
  model?: string | null;
}

export interface SessionSummary {
  id: string;
  agent_name: string;
  harness: HarnessKind;
  title: string;
  workdir: string;
  updated_at: string;
}
```

- `GET /api/v1/sessions?agent=<name>` — Lists discovered sessions for an agent across `muse`, `agy`, `codex`, and `claude`.
- `GET /api/v1/sessions/:id/transcript?limit=100&before=<cursor>` — Returns `{ session: SessionSummary, turns: NormalizedTurn[], has_more: boolean }`.
- `POST /api/v1/sessions/:id/prompt` — Body `{ agent_name: string, prompt: string, delivery?: "native" | "nats" }`. Dispatches the prompt into the active session using the agent's native mechanism:
  - `muse`: `muse session-message <session-id> "<prompt>"`
  - `agy`: `agy agentapi send-message` (or NATS push fallback via `agent-msg send <agent>`)
  - `codex`: `codex queue --thread <session-id> --message "<prompt>"`
  - `claude` / fallback: `agent-msg send <agent> "FerroDeck Prompt" "<prompt>"` (delivered automatically by `agent-bus-dispatcher.service`).

### 2.4 Git Worktrees and Pull-Request Inspector (`/api/v1/worktrees`, `/api/v1/prs`)

```typescript
export interface WorktreeSummary {
  id: string;                 // URL-safe slug of worktree path
  project: string;
  path: string;
  branch: string;
  head_sha: string;
  is_dirty: boolean;
  modified_files: {
    path: string;
    status: "M" | "A" | "D" | "R" | "U";
    additions: number;
    deletions: number;
  }[];
  ahead: number;
  behind: number;
}

export interface PullRequestSummary {
  project: string;
  repo: string;               // e.g., "Artificial-Humanity/FerroLoop"
  number: number;
  title: string;
  url: string;
  head_branch: string;
  base_branch: string;
  state: "OPEN" | "MERGED" | "CLOSED";
  co_authors: string[];       // Parsed from Co-authored-by trailers
  required_tier: "lead" | "lab_manager" | "owner";
  required_project?: string;
  review_decision: "APPROVED" | "CHANGES_REQUESTED" | "REVIEW_REQUIRED";
  approved_by: string[];
  ci_status: "SUCCESS" | "PENDING" | "FAILURE" | "UNKNOWN";
  mergeable: boolean;
}
```

- `GET /api/v1/worktrees` — Scans `/home/lmcfarlin/Projects/Artificial-Humanity/*` and `/home/lmcfarlin/worktrees/*`, returning `WorktreeSummary[]`.
- `GET /api/v1/worktrees/:id/diff?file=<optional-path>` — Returns `{ worktree_id: string, unified_diff: string, files: FileDiff[] }`.
- `GET /api/v1/prs?project=<optional-project>` — Returns open pull requests enriched with `fl github pr policy` governance metadata.

### 2.5 NATS JetStream & FerroWire Mail (`/api/v1/wire`)

```typescript
export interface WireMessageItem {
  id: string;
  sender: string;
  recipient: string;          // agent persona name or "all"
  subject: string;
  body: string;
  created_at: string;
  acked: boolean;
  urgency?: "normal" | "urgent";
}
```

- `GET /api/v1/wire/messages?agent=<name|all>&limit=100` — Returns inbox, sent, and broadcast messages from NATS JetStream (`agents.>`) / FerroWire store.
- `POST /api/v1/wire/send` — Body `{ from?: string, to: string, subject: string, body: string, broadcast?: boolean }`. Publishes via `agent-msg send` / `agent-msg broadcast` (and FerroWire when `crates/hub` is active).
- `POST /api/v1/wire/ack` — Body `{ message_id: string, agent: string }`. Acknowledges the message.

### 2.6 Workflow Runs & Hub Telemetry (`/api/v1/workflows`, `/api/v1/state`, `/api/v1/stats`)

To maintain forward compatibility with `crates/hub` (`fl hub serve`), `ui/src/types/api.ts` also defines:
- `GET /api/v1/workflows`, `GET /api/v1/workflow-runs`, `POST /api/v1/workflow-runs/:id/approve`, `POST /api/v1/workflow-runs/:id/reject`
- `GET /api/v1/state`, `GET /api/v1/stats`, `GET /api/v1/architecture`

### 2.7 Multiplexed WebSocket Protocol (`/ws/v1` and `/api/v1/stream`)

All real-time updates travel over a single WebSocket connection using a deterministic JSON envelope:

```typescript
export type WsChannel = "transcript" | "liveness" | "wire" | "pty" | "workflow";

export interface WsEnvelope<T = unknown> {
  channel: WsChannel;
  event: string;
  data: T;
}
```

- **Server-to-Client Events:**
  - `{ channel: "transcript", event: "turn.upsert", data: NormalizedTurn }` — Streamed whenever a harness JSONL file appends a turn, thinking chunk, or tool output.
  - `{ channel: "liveness", event: "agent.status", data: { name: string, liveness: LivenessState, active_session_id: string | null } }`
  - `{ channel: "wire", event: "message.received", data: WireMessageItem }`
  - `{ channel: "pty", event: "stdout", data: { pty_id: string, chunk_b64: string } }`
- **Client-to-Server Events:**
  - `{ channel: "transcript", event: "subscribe", data: { session_id: string } }`
  - `{ channel: "pty", event: "open", data: { agent_name: string, cols: number, rows: number } }`
  - `{ channel: "pty", event: "stdin", data: { pty_id: string, input_b64: string } }`
  - `{ channel: "pty", event: "resize", data: { pty_id: string, cols: number, rows: number } }`

---

## 3. Cockpit Layout, Visual System, and Responsive PWA Design

### 3.1 Aesthetic Direction and Color Tokens

FerroDeck uses a dark, high-contrast industrial cockpit aesthetic tailored for long engineering sessions on macOS, iPadOS, and iOS:
- **Canvas Background (`--bg-canvas`):** `#0A0D12`
- **Elevated Surface (`--bg-surface`):** `#11161F`
- **Interactive Card / Block (`--bg-elevated`):** `#171E29`
- **Hairline Border (`--border-subtle`):** `#232C3B`
- **Primary Text (`--text-primary`):** `#F1F5F9`
- **Secondary / Metadata Text (`--text-muted`):** `#94A3B8`
- **Accent Amber / Ferro Signal (`--accent-primary`):** `#F59E0B`
- **Harness Badges:**
  - `muse`: `#38BDF8` (Sky Cyan)
  - `agy`: `#A855F7` (Electric Violet)
  - `codex`: `#10B981` (Emerald)
  - `claude`: `#F97316` (Warm Coral)
- **Typography:**
  - UI & Chat Prose: `Inter`, `-apple-system`, `BlinkMacSystemFont`, sans-serif
  - Code, Diffs, Tool Calls & PTY Terminal: `JetBrains Mono`, `ui-monospace`, monospace (`font-variant-ligatures: none` inside diffs and terminal)

### 3.2 Desktop and iPad Landscape Layout (`>= 1024px`)

```text
+-----------------------+-------------------------------------------------------------------+
| FERRODECK   [ai-lab-0]| [Agent: Ferris (lead)] [FerroLoop] [muse] [main]   [Split View]   |
|-----------------------+-------------------------------------------------------------------|
| SEARCH AGENTS...      | [1. Chat]   [2. Git & PRs (2)]   [3. Wire Mail (1)]   [4. Term]   |
|                       +-------------------------------------------------------------------+
| AI-LAB-AMD            |                                                                   |
|  * JARVIS [lab_mgr]   |  [User Turn]                                                      |
|                       |  Please review the PR hierarchy spec.                             |
| FERROLOOP             |                                                                   |
|  * Ferris [lead]    < |  [Assistant Turn — Ferris (muse)]                                 |
|  * Felicia [asst]     |  > [Thinking (4.2s)] (collapsed by default, click to expand)      |
|  * Wesley [asst]      |  > [Tool: read_file crates/core/src/pr_policy.rs] (done)          |
|                       |  The PR policy engine checks Co-authored-by trailers...           |
| KAGGLE-GEMMA4         |                                                                   |
|  * Kayla [lead]       |                                                                   |
|  * Kacey [asst]       |                                                                   |
|  * Kage [asst]        |                                                                   |
|  * Katie [scribe]     |                                                                   |
| ... (all 12 agents)   +-------------------------------------------------------------------+
|                       | [Prompt Ferris via muse...]                     [Native v] [Send] |
+-----------------------+-------------------------------------------------------------------+
```

- **Split-View Mode:** Clicking `[Split View]` divides the main workspace into two side-by-side panes: **Chat** on the left (`55%`) and **Git & PRs**, **Wire Mail**, or **Terminal** on the right (`45%`).

### 3.3 Mobile / iPhone Portrait PWA Layout (`< 768px`)

- **Installable PWA:** `manifest.webmanifest` sets `"display": "standalone"`, `"background_color": "#0A0D12"`, `"theme_color": "#0A0D12"`, and Apple mobile web app meta tags (`apple-mobile-web-app-capable`) with `env(safe-area-inset-*)` padding.
- **Navigation:**
  - Tapping the top-left agent pill opens the slide-over **12-Agent Roster Drawer**.
  - A sticky bottom navigation bar switches cleanly between **Chat**, **Git/PRs**, **Mail**, and **Terminal**.

---

## 4. Harness Normalization, Error Handling, and Security

### 4.1 Transcript Normalization Rules

Each harness writes a distinct JSONL format on `ai-lab-0`. The normalizer layer (`ui/bridge/transcripts/*.ts`) converts each format into `NormalizedTurn[]`:
1. **Muse (`~/.local/share/muse/sessions/.../session.jsonl`):** Maps user/assistant message entries, reasoning blocks, and tool call/result records using the schema types exported by `muse schema generate-ts`.
2. **Antigravity (`~/.gemini/antigravity-cli/brain/<cid>/.system_generated/logs/transcript.jsonl`):** Maps `USER_INPUT` steps to `role: "user"` and `PLANNER_RESPONSE` steps to `role: "assistant"`, extracting `thinking` and `tool_calls`.
3. **Codex (`~/.codex/sessions/.../rollout-*.jsonl`):** Maps user prompts, reasoning items, function calls, and assistant responses.
4. **Claude Code (`~/.claude/projects/.../<sid>.jsonl`):** Maps `user` and `assistant` content blocks (`text`, `thinking`, `tool_use`, `tool_result`).

### 4.2 Graceful Degradation

- **Missing or Rotated Session Logs:** If an agent is registered in `registry.db` but has no active transcript file on disk, `GET /api/v1/sessions/:id/transcript` returns an empty `turns: []` list with `session.harness` metadata intact, and the composer automatically defaults to **NATS Delivery (`agent-msg send <agent>`)** so the owner can still message and wake the agent.
- **Subprocess Timeouts:** Every CLI call (`muse`, `agy`, `codex`, `git`, `fl`, `agent-msg`) runs with an explicit timeout (`5000ms` for reads, `10000ms` for prompt/mail dispatch) and structured error payload `{ error: { code: string, message: string } }`.

### 4.3 Security Controls

- **No Shell Interpolation:** All CLI invocations in `ui/bridge/` use `execFile` / argument arrays (`child_process.execFile`), never `exec` with shell string concatenation.
- **Path Traversal Protection:** Worktree diff and transcript queries validate canonical paths against an explicit allowlist of workspace roots (`/home/lmcfarlin/Projects/Artificial-Humanity`, `/home/lmcfarlin/worktrees`, and the four harness session directories).
- **Owner-Only Mutations:** Prompt injection, NATS message dispatch, and WebSocket PTY creation reject requests unless the session holds `scope: "owner"`.

---

## 5. Verification Plan

1. **Unit & Component Tests (`vitest` + `@testing-library/react`):**
   - Transcript normalizer tests for all four harness formats (`muse`, `agy`, `codex`, `claude`) using realistic JSONL fixtures.
   - Component tests for `RosterSidebar` (12 agents grouped by project, role badges, harness pills), `ChatPane` (collapsible `ThinkingBlock` and `ToolCallBlock`, prompt dispatch), `GitPrPane` (worktree file list, diff viewer, PR governance badges), and `WireMailPane` (inbox/broadcast filter and send form).
2. **Integration Smoke Test against Live `ai-lab-0` State:**
   - Verify `GET /api/v1/agents` returns all 12 registered agents from `~/.cache/agent-bus/registry.db`.
   - Verify `GET /api/v1/sessions` and `/transcript` parse live transcripts from `ai-lab-0`.
   - Verify `GET /api/v1/worktrees` and `/diff` return real worktree diffs.
   - Verify `npm run build` produces a clean zero-error TypeScript and Vite production bundle in `FerroLoop/ui/dist/`.
