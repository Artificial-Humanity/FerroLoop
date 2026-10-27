# FerroLoop — FerroDeck Unified Agent Cockpit UI Specification

**Date:** 2026-10-27  
**Author:** Wesley (`wesley`, operating under `ferris`, FerroLoop Lead)  
**Status:** Draft (Pending Owner Review)

---

## 0. Purpose and Scope

In the Artificial Humanity workspace on `ai-lab-0`, twelve resident agents (`jarvis`, `kayla`, `kacey`, `kage`, `sonya`, `sammy`, `penelope`, `katie`, `lucy`, `felicia`, `ferris`, and `wesley`) operate concurrently across the local CLI harnesses (`muse`, `antigravity` / `agy`, `codex`, `claude`, and `grok`).

With **Sub-project 5** (`crates/hub`, `fl hub serve`, `fl arch` — PR #50), FerroLoop provides a pure-Rust HTTP/1.1 + RFC 6455 WebSocket daemon (`fl hub serve`) exposing the `/api/v1/*` REST API, `/ws/v1` (and `/api/v1/stream`) real-time event and interactive terminal stream, two-tier authentication (`HubAuthority::Owner` and `HubAuthority::Peer`), and static SPA asset serving from `FerroLoop/ui/dist` (`ferrodeck_ui` in `.fl/architecture.toml`).

This specification defines **FerroDeck** (`FerroLoop/ui/`), the unified remote-control web and installable macOS / iPadOS / iOS PWA command center served directly by `fl hub serve`:
1. **Live 12-Agent Roster & Liveness Sidebar:** Project-grouped agent roster reading `GET /api/v1/agents` and `GET /api/v1/agents/liveness`, displaying role badges (`lead`, `assistant`, `lab_manager`, `custom:<slug>`), active harness adapter (`muse`, `antigravity`, `codex`, `claude`), discovered session binding, unread mail badges, and live heartbeat status.
2. **Claude-Style Structured Chat View (Tab 1):** Normalized turn-by-turn conversation transcript (`GET /api/v1/sessions`, `GET /api/v1/sessions/:id/transcript`) with collapsible reasoning/thinking blocks, collapsible tool-call cards, live `/ws/v1` session updates, and a unified prompt composer (`POST /api/v1/sessions/:id/prompt`).
3. **Git Worktree Diff & Pull-Request Inspector (Tab 2):** Real-time worktree status (`GET /api/v1/worktrees`), unified and split file diff viewer (`GET /api/v1/worktrees/:id/diff`), open pull-request governance inspector (`GET /api/v1/prs`), and tier-checked PR review/merge actions (`POST /api/v1/prs/:number/review`, `POST /api/v1/prs/:number/merge`).
4. **FerroWire & Bus Mail Center (Tab 3):** Per-agent inbox, sent messages, thread filter, unread filter (`GET /api/v1/wire/messages`), one-click message acknowledgement (`POST /api/v1/wire/ack`), direct/broadcast composer (`POST /api/v1/wire/send`), and live `/ws/v1` (`channel: "wire"`) push updates.
5. **Interactive Raw Terminal Pane (Tab 4):** Owner-scoped `@xterm/xterm` terminal connected over `/ws/v1` (`channel: "terminal"`, events `"open"`, `"opened"`, `"input"`, `"output"`, `"resize"`, `"close"`, `"exited"`).
6. **Workflows, Architecture Conformance & Telemetry Drawer:** Quick inspection and Owner sign-off for `fl flow` runs (`/api/v1/workflows`, `/api/v1/workflow-runs/:id/approve|reject`), `.fl/architecture.toml` conformance & LikeC4 DSL (`/api/v1/architecture`), and adapter token/cost/regret telemetry (`/api/v1/stats`).

---

## 1. Repository Layout and Architecture Conformance (`FerroLoop/ui/`)

Per `.fl/architecture.toml`, the `ferrodeck_ui` component owns `paths = ["ui/**"]` with a single allowed dependency edge `ferrodeck_ui -> fl_hub`. Building `ui/` outputs static assets to `FerroLoop/ui/dist/`, which `fl hub serve --ui-dir ui/dist` serves at `/`.

```text
FerroLoop/ui/
├── package.json
├── tsconfig.json
├── vite.config.ts
├── index.html
├── public/
│   ├── manifest.webmanifest
│   ├── icon-192.svg
│   ├── icon-512.svg
│   └── apple-touch-icon.svg
└── src/
    ├── main.tsx
    ├── App.tsx
    ├── styles/
    │   └── tokens.css
    ├── types/
    │   ├── api.ts          # Exact 1:1 TypeScript types matching crates/hub/src/{api,auth,discovery,terminal,ws}.rs
    │   └── msp.d.ts        # Generated Muse wire schema (`muse schema generate-ts`)
    ├── api/
    │   ├── client.ts       # Typed REST client for /api/v1/*
    │   └── ws.ts           # Multiplexed /ws/v1 client with auto-reconnect and channel subscriptions
    ├── components/
    │   ├── RosterSidebar.tsx
    │   ├── ContextHeader.tsx
    │   ├── PairingModal.tsx
    │   ├── ChatPane.tsx
    │   ├── TurnCard.tsx
    │   ├── ThinkingBlock.tsx
    │   ├── ToolCallBlock.tsx
    │   ├── GitPrPane.tsx
    │   ├── DiffViewer.tsx
    │   ├── WireMailPane.tsx
    │   ├── TerminalPane.tsx
    │   └── HubDrawer.tsx   # Workflows, Architecture Conformance, & Stats drawer
    └── __tests__/
        ├── api.test.ts
        ├── RosterSidebar.test.tsx
        ├── ChatPane.test.tsx
        ├── GitPrPane.test.tsx
        ├── WireMailPane.test.tsx
        └── TerminalPane.test.tsx
```

---

## 2. Exact `fl-hub` `/api/v1` and `/ws/v1` Wire Contract (`ui/src/types/api.ts`)

`ui/src/types/api.ts` mirrors the Rust structs in `crates/hub/src/{api,auth,discovery,terminal,ws}.rs` 1:1 so `FerroDeck` runs directly against `fl hub serve` with zero translation layer:

### 2.1 Authentication and Authority (`crates/hub/src/auth.rs`)

```typescript
export type HubAuthority =
  | { tier: "owner"; login: string }
  | { tier: "peer"; agent: string | null };

export interface AuthSessionResponse {
  authenticated: boolean;
  authority?: HubAuthority;
  session_token?: string;
}
```

- **Auto-Pairing Support:** When `FerroDeck` loads with `?pair=<code>` or `/pair?code=<code>` (the URL printed by `fl hub pair`), `App.tsx` automatically POSTs `{ code }` to `POST /api/v1/auth/pair`, receives the `fl_hub_session` `HttpOnly; SameSite=Strict` cookie (and `session_token`), strips the one-time query parameter from the URL bar via `history.replaceState`, and enters authenticated `Owner` mode.
- **Manual Pairing / Token Modal (`PairingModal.tsx`):** If `GET /api/v1/auth/session` returns `401 Unauthorized`, `FerroDeck` displays a pairing dialog where the owner can paste either a 10-minute `fl hub pair` code or an owner/peer bearer token.

### 2.2 Agents, Liveness, and Sessions (`crates/hub/src/discovery.rs`, `api.rs`)

```typescript
export interface AgentRecord {
  id: string;
  name_key: string;           // e.g., "ferris", "wesley", "jarvis"
  display_name: string;       // e.g., "Ferris", "Wesley", "JARVIS"
  project: string;
  role: string;               // "lead" | "assistant" | "lab_manager" | "custom:..."
  status: string;             // "active" | "retired"
  covered_areas: string[];
}

export interface DiscoveredSessionSummary {
  id: string;
  source: "route" | "attempt" | string;
  adapter: "claude" | "antigravity" | "codex" | "muse" | "grok" | "webhook" | string;
  persona?: string | null;
  status: string;
  updated_at?: string | null;
  cwd?: string | null;
  record_id?: string | null;
  transcript_path?: string | null;
}

export interface AgentLivenessItem {
  agent: string;
  project: string;
  role: string;
  has_route: boolean;
  adapter: string | null;
  verdict: {
    state: "alive" | "stale" | "dead" | "unknown" | string;
    reason?: string;
  };
}

export interface TranscriptToolCall {
  name: string;
  arguments: unknown;
}

export interface TranscriptTurn {
  index: number;
  role: "user" | "assistant" | "system" | string;
  content: string;
  thinking?: string | null;
  tool_calls: TranscriptToolCall[];
  created_at?: string | null;
}
```

- `GET /api/v1/agents` -> `{ agents: AgentRecord[], areas: AreaRecord[], routes: DiscoveredSessionSummary[] }`
- `GET /api/v1/agents/liveness` -> `{ liveness: AgentLivenessItem[] }`
- `GET /api/v1/sessions` -> `{ sessions: DiscoveredSessionSummary[] }`
- `GET /api/v1/sessions/:id/transcript` -> `{ session_id: string, turns: TranscriptTurn[] }`
- `POST /api/v1/sessions/:id/prompt` -> Body `{ prompt: string, by?: string, urgency?: "normal" | "urgent", subject?: string }` -> dispatches the prompt through `fl-wire`'s vendor push dispatcher and broadcasts `session:prompt_sent` and `wire:message_sent` on `/ws/v1`.

### 2.3 Git Worktrees and Pull Requests (`crates/hub/src/discovery.rs`, `api.rs`)

```typescript
export interface WorktreeSummary {
  id: string;
  path: string;
  head: string;
  branch?: string | null;
  dirty_files: string[];
  is_clean: boolean;
}

export interface WorktreeFileStatus {
  status: string;
  path: string;
}

export interface WorktreeDiff {
  id: string;
  path: string;
  branch?: string | null;
  head: string;
  files: WorktreeFileStatus[];
  diff: string;
}
```

- `GET /api/v1/worktrees` -> `{ worktrees: WorktreeSummary[] }`
- `GET /api/v1/worktrees/:id/diff` -> `WorktreeDiff`
- `GET /api/v1/prs` -> `{ prs: PullRequestRuntimeItem[] }`
- `POST /api/v1/prs/:number/review` & `POST /api/v1/prs/:number/merge` -> Enforces `authorize_agent_action` / `authorize_agent_merge` + `verify_merge_approvals`.

### 2.4 FerroWire Mail (`crates/hub/src/api.rs`)

- `GET /api/v1/wire/messages?to=<agent>&unread=<bool>&thread=<iri>` -> `{ messages: WireInboxItem[] }`
- `POST /api/v1/wire/send` -> Body `{ to: string, subject: string, body: string, from?: string, urgency?: "normal" | "urgent" }` -> sends via FerroWire + host route table and broadcasts `wire:message_sent`.
- `POST /api/v1/wire/ack` -> Body `{ id: string, by?: string }` -> marks receipt `Acked` and broadcasts `wire:message_acked`.

### 2.5 Multiplexed WebSocket & Interactive Terminal (`crates/hub/src/ws.rs`, `terminal.rs`)

```typescript
export interface HubWsFrame<T = unknown> {
  channel: "system" | "wire" | "session" | "workflow" | "terminal" | string;
  event: string;
  data: T;
}
```

- **Connection Handshake:** Connecting to `/ws/v1` (or `/api/v1/stream`) emits `{ channel: "system", event: "connected", data: { authority } }`.
- **Live Push Channels:**
  - `channel: "session"`, `event: "prompt_sent"` -> triggers immediate transcript refresh for the active session.
  - `channel: "wire"`, `event: "message_sent" | "message_acked"` -> updates unread counters and mail feed in real time.
  - `channel: "workflow"`, `event: "run_updated"` -> updates workflow run status and approval gates.
- **Interactive Terminal Channel (`channel: "terminal"`, Owner-only):**
  - Client sends `{ channel: "terminal", event: "open", data: { session_id, cwd, cols, rows, command: ["/bin/bash", "-i"] } }`.
  - Server responds with `{ channel: "terminal", event: "opened", data: { session_id, pid, cwd, cols, rows } }` and streams `{ channel: "terminal", event: "output", data: { session_id, stream: "stdout" | "stderr", data: "<utf8>" } }`.
  - Client sends keystrokes via `{ channel: "terminal", event: "input", data: { session_id, data: "<utf8>" } }`, dimension changes via `{ channel: "terminal", event: "resize", data: { session_id, cols, rows } }`, and cleanup via `{ channel: "terminal", event: "close", data: { session_id } }`.

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
- **Adapter Badges:**
  - `muse`: `#38BDF8` (Sky Cyan)
  - `antigravity` / `agy`: `#A855F7` (Electric Violet)
  - `codex`: `#10B981` (Emerald)
  - `claude`: `#F97316` (Warm Coral)
  - `grok`: `#EAB308` (Gold)
- **Typography:**
  - UI & Chat Prose: `Inter`, `-apple-system`, `BlinkMacSystemFont`, sans-serif
  - Code, Diffs, Tool Calls & Terminal: `JetBrains Mono`, `ui-monospace`, monospace (`font-variant-ligatures: none` inside diffs and terminal)

### 3.2 Desktop and iPad Landscape Layout (`>= 1024px`)

- **Left Rail (`RosterSidebar.tsx`, 270px collapsible):**
  - Search filter + project-grouped roster of all registered agents (with default fallback listing all 12 workspace personas when connected to an empty test store).
  - Each agent row shows: liveness indicator dot (`alive` emerald, `stale` amber, `offline` slate), display name, role pill (`lead`, `asst`, `lab_mgr`, `scribe`), active adapter badge (`muse`, `agy`, `codex`, `claude`), and unread FerroWire mail count.
- **Top Context Header (`ContextHeader.tsx`):**
  - Selected agent identity, project badge, active session selector dropdown, working directory path, authority pill (`Owner: lmcfarlin` or `Peer`), **Split-View toggle** (pins Chat on the left and Git/Mail/Terminal/Hub on the right), and primary navigation tabs:
    1. **Chat** (`ChatPane.tsx`)
    2. **Git & PRs** (`GitPrPane.tsx`)
    3. **Wire Mail** (`WireMailPane.tsx`)
    4. **Terminal** (`TerminalPane.tsx`)
    5. **Hub & Arch** (`HubDrawer.tsx` — Workflows, Architecture Conformance & LikeC4, Telemetry Stats)

### 3.3 Mobile / iPhone Portrait PWA Layout (`< 768px`)

- **Installable PWA:** `manifest.webmanifest` sets `"display": "standalone"`, `"background_color": "#0A0D12"`, `"theme_color": "#0A0D12"`, and Apple mobile web app meta tags (`apple-mobile-web-app-capable`) with `env(safe-area-inset-*)` padding.
- **Navigation:**
  - Tapping the top-left agent selector opens the slide-over **12-Agent Roster Drawer**.
  - A sticky bottom navigation bar switches cleanly between **Chat**, **Git/PRs**, **Mail**, **Terminal**, and **Hub**.

---

## 4. Verification Plan

1. **Frontend Unit & Component Tests (`vitest` + `@testing-library/react`):**
   - `api.test.ts` — Verifies pairing code exchange (`POST /api/v1/auth/pair`), bearer token headers, REST endpoint wrappers, and WebSocket frame parsing.
   - `RosterSidebar.test.tsx` — Verifies project grouping, search filtering, role/adapter badges, and agent selection.
   - `ChatPane.test.tsx` — Verifies turn rendering, collapsible `ThinkingBlock` and `ToolCallBlock` toggles, and `POST /api/v1/sessions/:id/prompt` submission.
   - `GitPrPane.test.tsx` — Verifies worktree list selection, unified/split diff rendering, and PR review/merge action triggers.
   - `WireMailPane.test.tsx` — Verifies inbox/unread filtering, message acking (`POST /api/v1/wire/ack`), and sending direct/broadcast messages (`POST /api/v1/wire/send`).
   - `TerminalPane.test.tsx` — Verifies Owner authority check and `/ws/v1` `channel: "terminal"` `open` / `input` / `output` / `close` lifecycle.
2. **Production Build & Live End-to-End Integration with `fl hub serve`:**
   - Run `npm test`, `npm run lint`, and `npm run build` in `FerroLoop/ui/` to produce `FerroLoop/ui/dist/`.
   - Start `fl hub serve --ui-dir ui/dist` on an ephemeral localhost port, pair via `POST /api/v1/auth/pair`, verify `/` serves the compiled `FerroDeck` SPA (`index.html` and static assets), verify `/api/v1/*` and `/ws/v1` round-trips, and verify `fl arch check` passes with `ferrodeck_ui` (`ui/**`) populated.
