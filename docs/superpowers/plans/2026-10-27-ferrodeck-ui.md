# FerroDeck Unified Agent Cockpit UI (`FerroLoop/ui/`) Implementation Plan

> **For agentic workers:** Execute this plan task-by-task using `subagent-dev` (`.agents/skills/subagent-dev/SKILL.md`) and `code-review` (`.agents/skills/code-review/SKILL.md`). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver **FerroDeck** (`FerroLoop/ui/`), the responsive web and installable macOS / iPadOS / iOS PWA command center served directly by `fl hub serve --ui-dir ui/dist`, implementing the live agent roster & liveness sidebar, Claude-style structured chat & prompt composer, Git worktree diff & PR governance inspector, FerroWire mail center, interactive Owner-only `@xterm/xterm` terminal, and Hub workflows/architecture/telemetry drawer.

**Architecture:** `FerroLoop/ui/` is a standalone React 19 + TypeScript + Vite SPA mapped to the `ferrodeck_ui` component (`paths = ["ui/**"]`, edge `ferrodeck_ui -> fl_hub`) in `.fl/architecture.toml`. It communicates exclusively with `fl-hub`'s `/api/v1/*` REST API and `/ws/v1` WebSocket stream (`crates/hub/src/{api,auth,discovery,terminal,ws}.rs`), and compiles to `FerroLoop/ui/dist/`.

**Tech Stack:** TypeScript 5, React 19, Vite 6, `@xterm/xterm` + `@xterm/addon-fit`, `vitest`, `@testing-library/react`, `@testing-library/jest-dom`, `jsdom`.

**Spec:** `docs/superpowers/specs/2026-10-27-ferrodeck-ui-design.md`

---

### Task 1: Scaffold `FerroLoop/ui/`, PWA Manifest, Design Tokens, Typed `/api/v1` Client, and `/ws/v1` Multiplexer

**Files:**
- Modify: `.gitignore` (ignore `ui/node_modules/` and `ui/dist/`)
- Create: `ui/package.json`
- Create: `ui/tsconfig.json`
- Create: `ui/vite.config.ts`
- Create: `ui/index.html`
- Create: `ui/public/manifest.webmanifest`
- Create: `ui/public/icon-192.svg`
- Create: `ui/public/icon-512.svg`
- Create: `ui/src/styles/tokens.css`
- Create: `ui/src/types/api.ts`
- Create: `ui/src/types/msp.d.ts` (generated via `/home/lmcfarlin/.local/bin/muse schema generate-ts`)
- Create: `ui/src/api/client.ts`
- Create: `ui/src/api/ws.ts`
- Create: `ui/src/__tests__/setup.ts`
- Create: `ui/src/__tests__/api.test.ts`

**Requirements:**
1. Update `.gitignore` to ignore `ui/node_modules/`, `ui/dist/`, and `ui/.vite/`.
2. Configure `ui/package.json` with scripts `dev`, `build` (`tsc --noEmit && vite build`), `lint` (`tsc --noEmit`), and `test` (`vitest run`).
3. Generate `ui/src/types/msp.d.ts` via `/home/lmcfarlin/.local/bin/muse schema generate-ts --out ui/src/types --experimental`.
4. Implement `ui/src/types/api.ts` mirroring `crates/hub/src/{api,auth,discovery,terminal,ws}.rs` 1:1 (`HubAuthority`, `AuthSessionResponse`, `AgentRecord`, `AreaRecord`, `DiscoveredSessionSummary`, `AgentLivenessItem`, `TranscriptTurn`, `TranscriptToolCall`, `WorktreeSummary`, `WorktreeDiff`, `WorktreeFileStatus`, `PullRequestItem`, `WireMessage`, `WireReceipt`, `WireInboxItem`, `WorkflowDef`, `WorkflowRun`, `StateEntry`, `HubStatsResponse`, `ArchitectureResponse`, `HubWsFrame`). Also export `DEFAULT_WORKSPACE_AGENTS` containing the active workspace leads (`jarvis`, `ferris`, `kayla`, `lucy`, `penelope`, `sonya`, `wesley`) as fallback metadata when a fresh local store has no seeded agent rows yet.
5. Implement `ui/src/api/client.ts` (`HubApiClient`) with:
   - Session token storage (`localStorage` + `Authorization: Bearer` header fallback alongside `credentials: "same-origin"` cookies).
   - Methods for `/api/v1/health`, `/api/v1/auth/pair`, `/api/v1/auth/session`, `/api/v1/auth/logout`, `/api/v1/agents`, `/api/v1/agents/liveness`, `/api/v1/sessions`, `/api/v1/sessions/:id/transcript`, `POST /api/v1/sessions/:id/prompt`, `/api/v1/worktrees`, `/api/v1/worktrees/:id/diff`, `/api/v1/prs`, `POST /api/v1/prs/:number/review`, `POST /api/v1/prs/:number/merge`, `/api/v1/wire/messages`, `POST /api/v1/wire/send`, `POST /api/v1/wire/ack`, `/api/v1/workflows`, `/api/v1/workflow-runs`, `/api/v1/workflow-runs/:id`, `POST /api/v1/workflow-runs/:id/approve`, `POST /api/v1/workflow-runs/:id/reject`, `/api/v1/state`, `/api/v1/stats`, and `/api/v1/architecture`.
6. Implement `ui/src/api/ws.ts` (`HubWsClient`) connecting to `/ws/v1` (with `?token=` when a bearer token is held), parsing `HubWsFrame`, dispatching channel subscribers (`system`, `wire`, `session`, `workflow`, `terminal`), and sending frames (`sendFrame(channel, event, data)`).
7. Write unit tests in `ui/src/__tests__/api.test.ts` testing `HubApiClient` (pairing, bearer header injection, URL encoding for session/worktree IDs, error handling) and `HubWsClient` frame routing.

---

### Task 2: Agent Roster Sidebar, Context Header, and Auth Pairing Modal

**Files:**
- Create: `ui/src/components/RosterSidebar.tsx`
- Create: `ui/src/components/ContextHeader.tsx`
- Create: `ui/src/components/PairingModal.tsx`
- Create: `ui/src/__tests__/RosterSidebar.test.tsx`

**Requirements:**
1. `RosterSidebar.tsx`:
   - Merges `AgentRecord[]` from `/api/v1/agents`, `DiscoveredSessionSummary[]` routes, and `AgentLivenessItem[]` from `/api/v1/agents/liveness` (falling back cleanly to `DEFAULT_WORKSPACE_AGENTS` if the store's `agents` list is empty, while still attaching discovered routes).
   - Groups agents by `project` (`AI-Lab-AMD`, `FerroLoop`, `Kaggle-Gemma4`, `Lucida`, `Prosodia`, `Sonora`, `Website`).
   - Supports instant search filtering by agent name, project, role, or adapter.
   - Displays liveness status dot (`alive` emerald, `stale` amber, `dead`/`unknown` slate), display name, role badge (`lead`, `lab_manager`, `assistant`, `custom:*`), active adapter pill (`muse`, `antigravity`, `codex`, `claude`, `grok`), and unread mail badge.
   - Supports mobile slide-over drawer mode (`isOpen`, `onClose`).
2. `ContextHeader.tsx`:
   - Displays the selected agent's identity, project, role, active session selector (`DiscoveredSessionSummary[]` for that agent or all sessions), working directory badge, current `HubAuthority` status pill (`Owner: <login>` or `Peer`), **Split-View toggle button**, and the 5 mode tabs (`Chat`, `Git & PRs`, `Wire Mail`, `Terminal`, `Hub & Arch`).
3. `PairingModal.tsx`:
   - Allows entering a 10-minute `fl hub pair` code or an Owner/Peer bearer token and exchanges it via `POST /api/v1/auth/pair`.
4. `RosterSidebar.test.tsx`:
   - Verifies grouping by project, search filtering, adapter/role badges, unread counts, and clicking an agent row.

---

### Task 3: Claude-Style Structured Chat Pane, Collapsible Thinking & Tool Blocks, and Unified Prompt Composer

**Files:**
- Create: `ui/src/components/ThinkingBlock.tsx`
- Create: `ui/src/components/ToolCallBlock.tsx`
- Create: `ui/src/components/TurnCard.tsx`
- Create: `ui/src/components/ChatPane.tsx`
- Create: `ui/src/__tests__/ChatPane.test.tsx`

**Requirements:**
1. `ThinkingBlock.tsx`:
   - Collapsible reasoning trace card (collapsed by default) with a toggle button (`Thinking trace`), character/line summary, and monospace reasoning body when expanded.
2. `ToolCallBlock.tsx`:
   - Collapsible tool invocation card showing `tool.name` and formatted JSON `tool.arguments` when expanded.
3. `TurnCard.tsx`:
   - Renders a `TranscriptTurn` (`role`: `"user" | "assistant" | "system"`), timestamp, `ThinkingBlock` (when `turn.thinking` is non-empty), `ToolCallBlock` list (when `turn.tool_calls.length > 0`), and formatted prose/code content.
4. `ChatPane.tsx`:
   - Loads `GET /api/v1/sessions/:id/transcript` for the selected session (or shows an agent session empty state if the agent has no active transcript file yet).
   - Subscribes to `/ws/v1` `channel: "session"` events to auto-refresh turns when a prompt is sent or session updates arrive.
   - Provides a unified prompt composer (`textarea` with `Cmd/Ctrl+Enter` shortcut, urgency selector `normal` | `urgent`, optional subject, and `Send Prompt` button) calling `POST /api/v1/sessions/:id/prompt` (using the active `session.id` or `agent.name_key` when no session file exists yet, since `fl-hub`'s `handle_post_session_prompt` resolves persona names directly!).
5. `ChatPane.test.tsx`:
   - Verifies rendering user and assistant turns, expanding/collapsing `ThinkingBlock` and `ToolCallBlock`, and dispatching a prompt via `onSendPrompt` / API client.

---

### Task 4: Git Worktree Diff Inspector and Pull-Request Governance Tab

**Files:**
- Create: `ui/src/components/DiffViewer.tsx`
- Create: `ui/src/components/GitPrPane.tsx`
- Create: `ui/src/__tests__/GitPrPane.test.tsx`

**Requirements:**
1. `DiffViewer.tsx`:
   - Parses unified git diff text (`git diff HEAD` output from `GET /api/v1/worktrees/:id/diff`) into file hunks with syntax-colored addition (`+`), deletion (`-`), and context lines, plus a toggle between **Unified** and **Split** diff layout and optional single-file filter.
2. `GitPrPane.tsx`:
   - Top sub-navigation or side-by-side layout for **Worktrees** (`GET /api/v1/worktrees`) and **Pull Requests** (`GET /api/v1/prs`).
   - Selecting a `WorktreeSummary` loads `GET /api/v1/worktrees/:id/diff`, lists modified files (`WorktreeFileStatus[]`) with status badges (`M`, `A`, `D`, `??`), and renders `DiffViewer`.
   - In the Pull Requests section, renders open PRs (and allows entering a PR number to submit a tier-checked review `POST /api/v1/prs/:number/review` with verdict `approve` | `request_changes` | `comment` or author merge `POST /api/v1/prs/:number/merge`).
3. `GitPrPane.test.tsx`:
   - Verifies worktree list rendering, loading and displaying file diffs in `DiffViewer` (unified & split toggle), and submitting a PR review action.

---

### Task 5: FerroWire Mail Center, Interactive `@xterm/xterm` Terminal Pane, Hub Drawer, App Shell, and Multi-Stage Docker Integration

**Files:**
- Create: `ui/src/components/WireMailPane.tsx`
- Create: `ui/src/components/TerminalPane.tsx`
- Create: `ui/src/components/HubDrawer.tsx`
- Create: `ui/src/App.tsx`
- Create: `ui/src/main.tsx`
- Create: `ui/src/__tests__/WireMailPane.test.tsx`
- Create: `ui/src/__tests__/TerminalPane.test.tsx`
- Modify: `Dockerfile` (add Node UI build stage copying `ui/dist` into `/usr/share/ferroloop/ui` and passing `--ui-dir /usr/share/ferroloop/ui`)

**Requirements:**
1. `WireMailPane.tsx`:
   - Displays messages from `GET /api/v1/wire/messages` with filters for recipient (`Selected Agent` vs. `All / Broadcast`), `Unread Only` toggle, and live updates from `/ws/v1` `channel: "wire"`.
   - Provides a one-click `Acknowledge` button (`POST /api/v1/wire/ack`) on unacknowledged messages.
   - Provides a composer form (`To` selector with all workspace agents + `all` broadcast, `Urgency` selector, `Subject`, `Body`) calling `POST /api/v1/wire/send`.
2. `TerminalPane.tsx`:
   - Integrates `@xterm/xterm` and `@xterm/addon-fit` (with a clean fallback/mock-friendly container when running in jsdom tests).
   - Checks `authority?.tier === "owner"`; if not Owner, shows a clear `Owner authority required for interactive terminal sessions` banner.
   - When Owner, sends `{ channel: "terminal", event: "open", data: { session_id, cwd, cols, rows } }` over `/ws/v1`, streams `event: "output"` chunks into the terminal, forwards terminal input (`onData`) and a quick-command input bar as `event: "input"`, sends `event: "resize"` on window resize, and sends `event: "close"` on unmount or reconnect.
3. `HubDrawer.tsx`:
   - Displays **Workflows (`fl flow`)** (`GET /api/v1/workflows`, `GET /api/v1/workflow-runs`, start run, and Owner/Lead `Approve` / `Reject` buttons on active runs), **Architecture Conformance (`fl arch`)** (`GET /api/v1/architecture` showing `.fl/architecture.toml` pass/fail report, component/edge list, and LikeC4 `.c4` DSL viewer), and **Adapter Telemetry (`fl stats`)** (`GET /api/v1/stats` token/cost/regret metrics).
4. `App.tsx` & `main.tsx`:
   - Handles automatic `?pair=<code>` exchange on startup, session check (`GET /api/v1/auth/session`), `/ws/v1` connection lifecycle, polling/push sync for agents/liveness/sessions/mail, desktop split-view mode (Chat on left + active secondary pane on right), and mobile bottom navigation bar.
5. Update root `Dockerfile` so the container image builds `ui/dist/` and serves it automatically via `fl hub serve --listen 0.0.0.0:8420 --ui-dir /usr/share/ferroloop/ui`.
6. Write tests in `WireMailPane.test.tsx` and `TerminalPane.test.tsx`, run the full frontend verification suite (`npm test`, `npm run lint`, `npm run build`), and verify `fl arch check` and `fl hub serve --ui-dir ui/dist` end-to-end.
