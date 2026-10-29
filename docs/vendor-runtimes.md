# Vendor runtimes

On-disk shapes for the five host runtimes FerroLoop discovers and FerroDeck displays.
Hub reads these files. It does not call quota RPCs, and it does not send OAuth requests.

Versions seen on a live host: Claude Code 2.1.312, OpenAI Codex 0.165.0, xAI Grok CLI 1.0.70.
Antigravity and Meta Muse do not write a version file.

Paths below are under the user's home directory. Do not copy live tokens, CSRF values,
peer credentials, or request ids into this document or into tests.

## What Hub exposes

`SessionContextUsage` is `used_tokens` and `max_tokens`.

`SessionQuotaUsage` is optional on each discovered route session and on
`GET /api/v1/sessions/:id/transcript` next to `context_usage`:

| Field | Meaning |
| --- | --- |
| `tier` | Vendor plan name, when the file has one. |
| `primary_window` | Shorter window. Codex uses 300 minutes. Muse uses this for a quota-exhausted reset. |
| `secondary_window` | Longer window. Codex and Grok use 10080 minutes (7 days). |
| `exhausted_reason` | Vendor string when the account is out of quota. |
| `QuotaWindowUsage.used_percent` | Vendor number, copied as written. Not rescaled. |
| `window_minutes` | Length of the window, when Hub knows it. |
| `resets_at_ms` | Unix milliseconds when the window resets. |

Attempt rows from the store have no `quota_usage`. Claude Code and Antigravity have no
allowance file, so their sessions omit `quota_usage`.

## Antigravity

Home: `~/.gemini/antigravity-cli/`.

### Session discovery and liveness

* Transcript: `brain/<conversation-uuid>/.system_generated/logs/transcript.jsonl`.
* `conversations/<uuid>.db` stores blob steps. Hub does not read it as the transcript.
* `conversation_summaries.db` is an index.
* `presence/*.lock` files are empty. They are not a PID.

### Transcript

JSONL `type` values include `USER_INPUT`, `PLANNER_RESPONSE`, `GENERIC`, `SYSTEM_MESSAGE`,
`CHECKPOINT`, and `ERROR_MESSAGE`.

`PLANNER_RESPONSE` carries `input_tokens`, `cache_read_tokens`, `output_tokens`, optional
`thinking`, and `tool_calls` with `name` and `args`. `ERROR_MESSAGE.error` is a string.

### Context usage

The file has per-turn token counts and no context-window field. Hub scans for the latest
`PLANNER_RESPONSE` whose `input_tokens + cache_read_tokens + output_tokens` is greater than
zero, and reports `max_tokens` `1000000`. A line with a zero sum is skipped. A transcript
with no positive sum reports no context usage.

### Account quota

No allowance file. A live language-server method named `RetrieveUserQuotaSummary` appears in
`cli.log`. Hub does not call it. The response body is not on disk.

### Delivery

FerroLoop sends with `agy agentapi send-message` to the language-server HTTP endpoint, using
the CSRF token and listen address from the route.

## Claude Code 2.1.312

Home: `~/.claude/`.

### Session discovery and liveness

* `sessions/<pid>.json` holds `pid`, `pidDomain`, `procStart`, `sessionId`,
  `messagingSocketPath`, `cwd`, `kind`, `status`, `version`, and `peerProtocol`.
* `messagingSocketPath` is a user socket such as `/run/user/<uid>/cc-socks/<pid>.sock`.
* `sessions/<pid>.<sha256>.key` holds the peer credential for that socket. Hub reads it for
  delivery and does not return it on the API.
* Transcript: `projects/<encoded-cwd>/<sessionId>.jsonl`.

### Transcript

Assistant lines use `message.content` blocks: `text`, `thinking`, `tool_use`, `tool_result`.
`message.usage` has `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`,
and `output_tokens`.

### Context usage

Hub sums those four usage fields on the latest non-sidechain line with a positive total.
`max_tokens` is `1000000`. Sidechain lines (`isSidechain: true`) are skipped.

### Account quota

No 5-hour window and no weekly window on disk. `userType` and `service_tier` are not plan
allowances. `quota_usage` stays empty.

### Delivery

FerroLoop writes an auth frame and a user frame to the session Unix socket.

## OpenAI Codex 0.165.0

Home: `~/.codex/`.

### Session discovery and liveness

* Rollout: `sessions/YYYY/MM/DD/rollout-<timestamp>-<thread-id>.jsonl`.
* `session_meta.payload.cli_version` is the CLI version. `session_meta.payload.id` is the thread id.
* `session_index.jsonl` is `{id, thread_name, updated_at}`. It is not liveness.
* `state_5.sqlite` has `threads.tokens_used`. It has no rate-limit windows.
* Queued prompts live in `queue_1.sqlite` table `queued_items`.

### Transcript

Lines are `type: event_msg`. Context and quota both use `payload.type == token_count`.
Hub also normalizes user, assistant, and tool events from the same file.

### Context usage

Hub takes `payload.info.last_token_usage.total_tokens`, or else
`payload.info.total_token_usage.total_tokens`, from the latest positive `token_count` line.
`max_tokens` is `model_context_window` from `config.toml` when that file sets it, else
`payload.info.model_context_window`, else `500000`.

### Account quota

The same `token_count` line carries `payload.rate_limits` when the CLI persisted it.
Hub keeps the latest such line:

| Hub field | Source |
| --- | --- |
| `tier` | `plan_type` |
| `primary_window.used_percent` | `primary.used_percent` |
| `primary_window.window_minutes` | `300` |
| `primary_window.resets_at_ms` | `primary.resets_at` (Unix seconds) times 1000 |
| `secondary_window.used_percent` | `secondary.used_percent` |
| `secondary_window.window_minutes` | `10080` |
| `secondary_window.resets_at_ms` | `secondary.resets_at` times 1000 |
| `exhausted_reason` | `rate_limit_reached_type`, omitted when null or empty |

The on-disk objects also store `window_minutes` (300 and 10080). Hub still writes the
constants above. A null `rate_limit_reached_type` is omitted.

### Delivery

FerroLoop runs `codex queue --thread`, which inserts into `queue_1.sqlite`. App-server
sockets may exist. Hub does not use them for operator prompts.

## xAI Grok 1.0.70

Home: `~/.grok/`. CLI version is `version.json`. A live session model id looks like
`grok-4.7-build`. The catalog lists `grok-4.7` with context windows 256000 and 500000.

### Session discovery and liveness

* `active_sessions.json`: `{session_id, pid, pid_start, cwd, opened_at}`.
* `pid_start` is process start ticks, not a Unix timestamp.
* Session directory: `sessions/<url-encoded-cwd>/<session_id>/`.
* Files there: `chat_history.jsonl`, `summary.json`, `signals.json`, `usage.json`.

### Transcript

`chat_history.jsonl` lines use `type` `system`, `user`, `assistant`, `reasoning`, and
`tool_result`. Assistant `tool_calls` have `id`, `name`, and `arguments`. Tool results
point back with `tool_call_id`.

### Context usage

Hub prefers `signals.json` `contextTokensUsed` and `contextWindowTokens`.
`contextWindowUsage` is an integer percent and is not the Hub percent field.
If signals have no used count, Hub reads `usage.json` at `/session/totalTokens`.
`summary.json` on current builds has no `context_window`. When used tokens exist and no
window was stored, `max_tokens` is `500000`. `usage.json` also has per-model token and cost
fields. Those are session cost, not the account allowance.

### Account quota

The only account allowance on disk is `logs/unified.jsonl`, message
`billing: fetched credits config`. Hub uses the latest line. It is account-level, so every
Grok session on that home shares it.

| Hub field | Source |
| --- | --- |
| `tier` | `ctx.subscriptionTier` (for example `SuperGrok`) |
| `primary_window` | omitted |
| `secondary_window.used_percent` | `ctx.config.creditUsagePercent`, verbatim |
| `secondary_window.window_minutes` | `10080` |
| `secondary_window.resets_at_ms` | `ctx.config.currentPeriod.end`, or else `ctx.config.billingPeriodEnd` |

`currentPeriod.type` is `USAGE_PERIOD_TYPE_WEEKLY`. There is no 5-hour window in this log.
Timestamps may be `Z` or a numeric offset such as `+00:00`. Hub converts them to Unix
milliseconds.

### Delivery

FerroLoop pastes into the session tmux pane (`load-buffer`, `paste-buffer`, `send-keys Enter`).
The CLI fallback is `grok -s <session_id> -p`.

## Meta Muse

Home: `~/.local/share/muse/`. No version file. The catalog context limit is about 1007997
tokens. A default model id is `muse-wonder-1.0`.

### Session discovery and liveness

* Transcript: `sessions/YYYY/MM/DD/<session-id>/session.jsonl`.
  A direct `sessions/<session-id>/session.jsonl` is also recognized.
* Envelope fields: `schema_version`, `id`, `stream`, `sequence`, `recorded_at` (microseconds),
  `record_type`, `payload_type`, `payload`.
* `session-index.db` indexes sessions. Hub discovers the JSONL files.
* Runtime metadata has `endpoint_hint` and `process_generation_hint`, not a PID file.
* Command sockets live at `runtime/muse/ms-*.sock`. FerroLoop does not speak that protocol.

### Transcript

Session events use `payload_type: runtime.session` and `payload.event.kind`.
`model_completed` carries `usage` (`input_tokens`, `output_tokens`, and related counters)
plus `estimated_prompt_tokens`.

### Context usage

Hub uses `input_tokens + output_tokens` from the latest `model_completed` line when that sum
is positive, otherwise `estimated_prompt_tokens`. `max_tokens` is `1000000`.

### Account quota

No allowance counter file. A `runtime.session` event with `kind`
`model_failure_terminal_intent` or `failed`, whose `reason` contains
`Subscription quota exhausted`, sets:

* `exhausted_reason` to that reason string
* `primary_window.used_percent` to `100`
* `primary_window.window_minutes` omitted
* `primary_window.resets_at_ms` parsed from `Your usage window resets at <ISO-8601>.`

A later `model_completed` event clears the exhaustion. Hub then omits `quota_usage`.
The reset instant comes from the reason text, not from `recorded_at`.

### Delivery

FerroLoop pastes into the session tmux pane. The CLI fallback is
`muse exec --session-id <id> --json`.

## Store guard

HTTP authentication opens the store only long enough to resolve a session token, then drops
it. Owner and peer bearer tokens are compared in memory and do not need a store open.
Each route opens the store again only around B-tree reads and writes.

Session discovery, transcript parsing, `git` and `gh` subprocesses, and prompt delivery run
after that guard is dropped. `GET /api/v1/worktrees` and `GET /api/v1/architecture` do not
open the store. Queue drain snapshots `list_agents` and attempt rows, drops the guard, then
reads transcripts and dispatches prompts.

`GET /api/v1/prs` reads workspace runtime state keys `pr-*`. This tree has no `gh pr` handler
and no `list_pr_reviews` call in the hub crate. Those commands are not inserted here.
