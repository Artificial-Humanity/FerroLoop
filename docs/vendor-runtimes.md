# Vendor runtimes

On-disk shapes for the five host runtimes FerroLoop discovers and FerroDeck displays.
Hub reads these files. It also asks the local `muse` and `claude` CLIs for account
allowance at most once every 60 seconds. A cold probe waits at most 3 seconds, outside
the store lock. A home override, as in unit tests, does not run the host binaries
unless `FL_HUB_MUSE_BIN` or `FL_HUB_CLAUDE_BIN` is set.

Versions seen on a live host: Claude Code 2.1.312, OpenAI Codex 0.165.0, xAI Grok CLI 1.0.70.
Antigravity and Meta Muse do not write a version file.

Paths below are under the user's home directory. Do not copy live tokens, CSRF values,
peer credentials, or request ids into this document or into tests.

## What Hub exposes

`SessionContextUsage` is `used_tokens` and `max_tokens`, plus `source` (`reported`, `catalog` or
`assumed`: how the window was found; absent in older reports) and `offered_windows` (the sizes the
model offers, when the vendor lists them).

`SessionQuotaUsage` is optional on each discovered route session and on
`GET /api/v1/sessions/:id/transcript` next to `context_usage`:

| Field | Meaning |
| --- | --- |
| `tier` | Vendor plan name, when the file has one. |
| `primary_window` | Shorter window. Codex and Claude use 300 minutes. A Muse CLI `window` uses `windowDurationMins`. A Muse quota-exhausted reset also uses this field. |
| `secondary_window` | Longer window. Codex and Grok use 10080 minutes (7 days). |
| `exhausted_reason` | Vendor string when the account is out of quota. |
| `QuotaWindowUsage.used_percent` | Vendor number, copied as written. Not rescaled. |
| `window_minutes` | Length of the window, when Hub knows it. |
| `resets_at_ms` | Unix milliseconds when the window resets. |

Attempt rows from the store have no `quota_usage`. Claude Code, Antigravity, and Muse
fill `quota_usage` from a direct probe, an override JSON value, or a rate-limit error
in the transcript.

## Clear and Reforge

Clear and Reforge resolve targets only from saved/registered routes. A single request for
an unknown or unsaved session returns HTTP 404 with guidance to refresh or register the
session first; it does not actively discover that target. Bulk Clear/Reforge includes only
saved routes. Compact keeps its existing discovery behavior. Supported saved reset routes
retain their normal validation and dispatch checks.

Codex, Muse, Grok and every generic `HostCli` route refuse both actions before reset
commands, startup prompts, queue/model changes or discovery invalidation. FerroLoop cannot
confirm a changed native session identity bound to the exact terminal for these routes.
This also applies to a `HostCli` route whose vendor label is Claude or Codex.

Single-session requests return HTTP 409 with `code: reset_unsupported`, the canonical
`vendor`, `reason: identity_unconfirmable`, `alternative: remodel_same_vendor` and a plain
`error` message. Bulk actions retain those fields with `session_id`, `ok: false` and
`status: 409` per refused session. The alternative is an explicit remodel to the same
vendor to start a fresh session; FerroLoop does not run it automatically.

Claude Reforge requires a native `sessionId` from its session record before sending
`/clear`. If that identity is unavailable, it refuses before sending keys or a startup
prompt. After `/clear`, it waits up to 20 seconds for a changed `sessionId`; timeout returns
an error without sending the startup prompt or broadcasting `session.cleared`.

Antigravity keeps its in-place cascade rewind described below. It does not establish a new
native session identity and remains outside this confirmation contract, pending a separate
audit. Webhook routes remain a deterministic test transport, not evidence of native reset
confirmation.

## Host reboot behaviour

A host boot can restore tmux sessions (tmux-resurrect with continuum restore). Hub keeps three
rules for what comes back.

**A decommission leaves nothing to restore.** A teardown removes what it stopped from the newest
tmux-resurrect save (the file the `last` link names, else the newest `tmux_resurrect_*.txt`). A
`last` link that points outside the directory is not followed. What goes matches what is gone:

- A session that is gone: its `pane`, `window` and `grouped_session` lines. A `state` line (the
  client's session and last session) is kept; only the field that names the session is emptied.
- One window of a session that stays (a window of the shared `muse` session): its `window` line
  and the `pane` lines of that window. A `pane` line carries only the window index, so Hub maps the
  window name to the index first. The session's other windows, its `state` lines and its
  `grouped_session` lines stay. The shared `muse` session itself is pruned only when it is gone.

The directory is `FL_TMUX_RESURRECT_DIR` when set, else `~/.tmux/resurrect` when it exists, else
`$XDG_DATA_HOME/tmux/resurrect`. A hub that runs with a home override ignores
`FL_TMUX_RESURRECT_DIR`. Hub edits the file itself (a temp file with the permissions of the save,
flushed to disk, then a rename) and does not run the plugin's `save.sh`. The first prune of a save
keeps the old content as `<save>.bak`; a later prune keeps that first backup. A backup that cannot
be written stops the prune. The directory keeps the newest five `tmux_resurrect_*.txt.bak` files;
older ones are removed, and files of other names are never touched. A save that does not name the
session or window is not touched. A continuum save that started before the runtime was killed can
still write it back after the prune.

The result is the `restore_source` field of the teardown response: `pruned`, `clean`, `no_save`,
`running` or `failed`. It is absent when the teardown made no runtime call. `failed` and `no_save`
never fail the teardown. Hub prunes only what is not running after the teardown: a runtime that
Hub started is killed and then pruned, and a runtime that Hub did not start is pruned only if it
is already gone. A session found without a hosted record (a hand-started one) is addressed by the
target a spin-up of its vendor would use (`<Display>` for a session, `muse:<persona>` for Muse)
and follows the same rule. A runtime that still runs and that Hub did not start is neither killed
nor pruned; the result is `running`.

**A restored pane of a retired persona loses its routes; one of an unknown persona is reported.**
When `fl hub serve` starts, Hub lists the tmux panes. For each pane that runs a vendor CLI, the
persona is the pane's tmux session name; an inherited `AGENT_PERSONA` is never used. In the shared
`muse` session the persona is the pane's window name, and a pane with no window name is skipped.
If the persona is not an active agent, Hub leaves the pane running and sends one
`session.restore_orphan` event on the `sessions` channel (`session`, `window`, `pane_id`,
`persona`, `reason` of `retired` or `unknown`, `command`, `boot_id`) and one line to the service
log. The same pane is reported once per boot. The pane check runs on every pass, so a pane that
tmux restores late is still found.

Routes are removed for a `retired` persona only. Hub drops the route keyed by the persona's name,
and every route, whatever persona it is keyed to, that names the retired pane (`tmux_pane`) or its
window (`tmux_target`, `session:window`). Only Grok and Muse routes carry a pane or target. A Codex
or Claude route cannot be matched to a pane, so it is dropped only by its persona key. An
`unknown` persona keeps its routes: Hub's agent registry is not the agent bus's registry, so a live
agent that Hub never registered (a lead, the workspace manager) looks unknown, and its routes must
stay. The route removal is best effort: a later route scan (`RouteTable::discover_routes`) can
find a retired route again from the running vendor transcript. Once #336 is on main, boot
reconcile uses its tombstones, so a scan cannot bring the route back.

**Each expected agent is told once per boot.** The boot is the host `boot_id`
(`/proc/sys/kernel/random/boot_id`). The workspace runtime state entry `host-boot-epoch` holds the
boot, the `expected` agents and the `notified` ones. Who is expected:

- A saved record with a different `boot_id` proves a reboot. A new record opens whatever the
  uptime, and `expected` is the active agents at that moment.
- With no record, the uptime decides: a host up for at most one hour is a reboot, and `expected`
  is the active agents now. An older host expects nobody. The host uptime and `boot_id` must both
  be known, or nobody is expected.
- A record with the same `boot_id` keeps its `expected` list. An agent registered after the record
  was made is never told, and a Hub restart in the same boot tells nobody who was told already.

Hub sends an expected agent that is still active one `system` message with the subject
`Host restarted`, after it registers the routes the sessions prove from their own identity. The
agent's route must pass the delivery check and must be live: it was found after the host booted
(its `updated_at` is at or after the boot time), or a restored vendor-CLI pane of the persona
exists (for Muse, a window of that name). A route saved before the reboot is not enough. No
message goes to a retired agent or an agent that is decommissioning.

The notice is at most once. Hub saves the name in the record before it sends; if the save fails
the notice is not sent, and if the send fails (including a store that cannot be opened for the
send) the name is removed again and the other agents are still told. A name that Hub sent in
this process is also kept in memory. Hub makes passes every 15 seconds. It stops when no expected
agent is pending, tmux has answered and 60 seconds have passed, and it makes at most 20 passes. The
passes also stop when the hub shuts down.

## Runtime matrix: quotas and CLI updates

`GET /api/v1/runtimes/matrix` and `POST /api/v1/runtimes/probe` (owner only) list the CLI
runtimes `claude`, `codex`, `grok`, `agy` and `muse`. Each row carries `update_supported` and
`update_command` (for example `claude update`). The matrix also carries `vendor_quotas`, a map
from adapter id (`claude`, `codex`, `grok`, `antigravity`, `muse`) to a `SessionQuotaUsage`.
For each selected vendor the reading is the one with the most data among its newest route
sessions (four at most are read from disk), else the account probe above. A vendor whose
allowance is unknown has no entry. A hub with a home override never reads the real home.

`POST /api/v1/runtimes/update` (owner only) runs a CLI's own update command. The body is
`{"runtime": "claude" | "codex" | "grok" | "agy" | "muse" | "all"}`; any other body is `400`.
The command is fixed in the hub and runs without a shell and without caller arguments:

| Runtime | Command | Source |
| --- | --- | --- |
| `claude` | `claude update` | `claude --help` lists `update\|upgrade` |
| `codex` | `codex update` | `codex --help` lists `update` |
| `grok` | `grok update` | `grok --help` lists `update` (no flag installs the latest) |
| `agy` | `agy update` | `agy --help` lists `update` ("Update CLI") |
| `muse` | none | no update subcommand; its launcher updates itself in the background |

Each command is stopped after 300 seconds (its process group is killed) and its output is kept
as the last 4 KiB without terminal escapes. One update runs at a time: a second request gets
`409`. One runtime answers with one object, `all` with an array in the order above (`all` holds
the request for as long as the updates take, up to several minutes):
`{runtime, ok, version_before, version_after, status, output_tail}`, where `status` is
`updated`, `unchanged` (both versions known and equal), `unknown_version` (the command succeeded
but the version after it cannot be read; `ok` is true), `failed`, `unsupported`, `not_installed`
or `timeout`. After a run the cached probe is dropped, so the next matrix read shows the new
version.

An update command runs in the hub's effective home with a cleared environment: only `PATH` (the
absolute directories the binary was found in), `HOME`, locale, terminal, user, temp and XDG
directory variables, proxy and CA-certificate variables, `NPM_CONFIG_PREFIX`, `NPM_CONFIG_CACHE`,
`CARGO_HOME` and `RUSTUP_HOME` are passed on. A variable starting with `FL_` or ending in
`_TOKEN`, `_KEY`, `_SECRET` or `_PASSWORD` is never passed. A relative `PATH` entry is not searched.

The matrix reads the vendor quotas within 4 seconds in all; a vendor not read by then has no entry
in `vendor_quotas`. Each reading is kept for 30 seconds.

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

`ERROR_MESSAGE.error` is `exhausted_reason` when the text contains `429`, `quota`, or a
rate-limit phrase. The latest matching line wins.

Hub does not call `RetrieveUserQuotaSummary`. When `FL_AGY_QUOTA_JSON` is a JSON object
or a path to one, Hub maps `tier`, `window` (`usedPercent`, `windowDurationMins`,
`resetsAtMs`), and `weekly` (`usedPercent`, `resetsAtMs`, 10080 minutes). That result is
cached for 60 seconds and merged with the transcript reason. The transcript reason wins
when both are set.

### Delivery

FerroLoop sends with `agy agentapi send-message --title=<title> <conversation_id> <content>` to
the language-server HTTP endpoint, using the CSRF token and listen address from the route. A
failed send reports the first 300 characters of what `agy` printed.

For `Clear` and `Reforge`, FerroLoop calls
`POST /exa.language_server_pb.LanguageServerService/RevertToCascadeStep` with `stepIndex` `0`,
`conversationOnly` `true`, and `overrideConfig.plannerConfig.planModel`
(`MODEL_PLACEHOLDER_M418`, or `defaultOverrideModelConfig.modelOrAlias.model` from
`GetCascadeModelConfigData` if the language server rejects the default placeholder), then
truncates `transcript.jsonl` and `transcript_full.jsonl` to retain only line 1 when its
`step_index` is `0`.

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

`claude auth status --json` supplies `tier` from `subscriptionType` (for example `pro`
or `max`). Hub runs `claude` on `PATH`, or else `~/.local/bin/claude`. The timeout is
3 seconds. The result is cached for 60 seconds.

`primary_window` is `five_hour` (300 minutes). `secondary_window` is `seven_day`
(10080 minutes). `utilization` or `used_percentage` is copied as written. `resets_at`
is an ISO-8601 timestamp converted to Unix milliseconds.

Those windows come from `FL_CLAUDE_USAGE_JSON` (JSON text or a file path), or from
`five_hour` and `seven_day` on `claudeAiOauth`. When neither is present, `rateLimitTier`
is set, and Hub is using the real home directory, Hub sends one request to
`https://api.anthropic.com/api/oauth/usage`. The access token is not logged and is not
returned on the API.

A `system` or `api-request` transcript row, or an assistant `error` field, sets
`exhausted_reason` when the text contains a rate-limit phrase. The latest matching row
wins. Hub merges that reason with the account windows.

### Delivery

FerroLoop writes an auth frame and a user frame to the session Unix socket.

## OpenAI Codex 0.165.0

Home: `~/.codex/`.


### Hosted launch prerequisite

Hosted Codex uses `codex --remote unix://` and requires the shared Codex app-server daemon
before launch. FerroLoop checks that its Unix control socket accepts a connection, with a
250 ms deadline; it sends no RPC or thread request. The socket is
`app-server-control/app-server-control.sock` under the effective `CODEX_HOME` pinned into
the child. `FL_WIRE_CODEX_CONTROL_SOCK` selects delivery traffic, not this launch endpoint.
Non-Unix remote launches refuse because this transport requires Unix sockets.

Spin-up, restart and remodel return HTTP 409 with `code: codex_app_server_unavailable`
when this prerequisite is unavailable. Automatic recovery emits a
`session.prerequisite_unavailable` event. An automatically recoverable stopped record has
`stop_reason: prerequisite_unavailable` and
`launch_prerequisite_detail` containing a bounded socket path and probe result. After a
refusal before any spawn, automatic `on-failure` recovery checks again at most once per
60 seconds per session; its next deadline is saved across Hub restarts. Such refusals spend
no automatic respawn attempts. A successful connection is readiness evidence only; the
launched process must still survive normally.
A refused manual restart preserves a prior `clean_exit`, `crash_loop` or other intentional
stop reason and its recovery state; it does not arm automatic recovery. Only an automatic
restart, or a record already stopped for `runtime_gone` or `prerequisite_unavailable`,
receives the prerequisite retry reason and deadline. If the daemon disappears during initial
startup, an immediate re-probe records the unavailable prerequisite. Once a process really
spawns, its attempt stays charged with the normal deadline of its phase, even if it dies
immediately. Only a refusal before any spawn is free.

FerroLoop does not start the vendor daemon. Start it through a clean systemd user service,
not from an agent command shell: a shared daemon and its updater propagate inherited
`AGENT_PERSONA`, `FL_HOSTED_ID`, `AGENT_HYDRATION_GENERATION` and `CLAUDE_CODE_*` values into
other agents' command shells. Keep those variables out of the user manager's environment too.
The reference service orders itself before Hub (adapt the binary path to the installation):

```ini
# ~/.config/systemd/user/codex-app-server-daemon.service
[Unit]
Description=Shared Codex app-server daemon
Before=ferroloop-hub.service

[Service]
Type=oneshot
RemainAfterExit=yes
UnsetEnvironment=AGENT_PERSONA FL_HOSTED_ID AGENT_HYDRATION_GENERATION
ExecStart=%h/.local/bin/codex app-server daemon start

[Install]
WantedBy=default.target
```

Enable both services for boot; ordering applies when both units are started. Ensure the user
manager supplies no `CLAUDE_CODE_*` identity variables; `UnsetEnvironment` does not accept
wildcards. On a configured host, `systemctl --user restart codex-app-server-daemon.service` is
the reference manual recovery command: `start` is a no-op after daemon death while this
oneshot unit remains active. Avoid starting another daemon from an agent shell. Restarting a
shared daemon can interrupt active clients and requires coordinated workload handling.

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
The CLI fallback is `grok -r <session_id> -p`, which resumes the session. When grok reports that
the session is not found (its file is not on disk yet), FerroLoop runs `grok -s <session_id> -p`
once, which creates it. `-s` alone fails on a session id that is already in use.

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

A later `model_completed` event clears the exhaustion. Hub then omits `quota_usage`
unless the CLI probe still has an allowance. The reset instant comes from the reason
text, not from `recorded_at`.

Hub also runs `muse usage --json` (`muse` on `PATH`, or else `~/.local/bin/muse`) with
the same 3-second timeout and 60-second cache. `allowance.tier` is `tier`.
`allowance.window` is `primary_window` (`usedPercent`, `windowDurationMins`, `resetsAtMs`).
`allowance.weekly` is `secondary_window` (`usedPercent`, `resetsAtMs`, 10080 minutes).
A null `allowance` leaves the on-disk result unchanged. When both exist, the CLI `tier`
and windows are kept, a missing CLI window falls back to the file, and the transcript
`exhausted_reason` is kept.

### Delivery

FerroLoop pastes into the session tmux pane. The CLI fallback is
`muse exec --session-id <id> --json`.

### Hosting

A hosted Muse agent runs as a window named `<persona>` (lowercase) in the one shared tmux
session `muse`; its target is `muse:<persona>`. Spin-up uses `new-window` when the session
exists and `new-session` when not. A window of that name that already exists (for example from
`muse-sessions`) is not touched: the prompt is delivered to it.

Spin-up marks the window it creates. It adds `-P -F '#{window_id}'` to `new-window` or
`new-session`, reads back the window id (`@12`), and runs
`set-option -w -t @12 @fl_hosted <session_id>` (the hosted record id, `muse:<persona>`). If the
id cannot be read, or the mark cannot be set, it removes the new window by id and the spawn
fails. A window is ours only when it carries the mark with our id; the name alone cannot tell
it from a window that someone opened under the same name after ours died.

* Teardown lists the windows with `#{window_id}`, `#{window_name}` and `#{@fl_hosted}`. It runs
  `kill-window -t @id` only when exactly one window has the name and its mark equals the record
  id. A window of the name without the mark, or with another mark, is not killed and not
  reported as stopped. The session is never killed.
* Restart and spin-up clear `runtime_spawned` when no window of the name carries our mark.
  A foreign window of the same name then gives `runtime_spawned=false`; the prompt goes through
  delivery and teardown does not kill it. The launch survival check also asks for the marked
  window, not any window of the name.
* Remodel refuses (409) to leave a Muse window that the record claims but that carries no mark
  of its own, because the teardown would not stop it.
* Read-only checks (`exists`, the adoption of a window that is already there) accept any window
  of the name and never claim ownership.

Discovery also accepts a `<Session>:muse` window and prefers `muse:<persona>` when both exist.

## Store guard

HTTP authentication opens the store only long enough to resolve a session token, then drops
it. Owner and peer bearer tokens are compared in memory and do not need a store open.
Each route opens the store again only around B-tree reads and writes.

Session discovery, transcript parsing, `git` and `gh` subprocesses, quota CLI probes, and
prompt delivery run after that guard is dropped. `GET /api/v1/worktrees` and
`GET /api/v1/architecture` do not open the store. Queue drain returns before it opens the
store when no prompt is queued. When a prompt is queued, drain snapshots attempt rows,
drops the guard, then reads transcripts and dispatches prompts.

`GET /api/v1/prs` reads workspace runtime state keys `pr-*`. This tree has no `gh pr` handler
and no `list_pr_reviews` call in the hub crate. Those commands are not inserted here.

### Hosted restart and bounded crash-loop recovery

`fl agent restart <persona-or-session-id> [--by owner] [--json]` restarts a hosted
record from its saved settings, including vendor, model, thinking level, permission
posture, working directory, node and context. It accepts no launch-setting overrides.
An exact session id wins; a persona must name one eligible saved record. Retired,
non-hosted and decommissioning/decommissioned targets are refused. The command uses
the Owner caller model of `fl agent spin-up` and revalidates selection under the
session lifecycle fence. Explicit spin-up, restart, prepare, teardown and the entire
remodel (including rollback) share a nonblocking OS advisory fence between the CLI
and hub. Its namespace is the canonical host endpoint and normalized persona, so
aliases, different HOME values and workspace stores sharing that endpoint conflict.
Legacy tmux fallback also fences its actual server socket; both runtime endpoints
remain pinned through the operation. A busy persona fence refuses immediately with
HTTP 409 / CLI exit 2 and diagnostic holder pid, operation and start time. The
metadata never permits stealing a lock. Private 0700 directories contain user-owned
0600 lock files that are never removed. Normal fence drop explicitly unlocks the file,
including any descriptor copy inherited by a fork. An abrupt owner exit before drop
can leave ownership briefly in a forked child until exec; a contender may receive a
transient conflict and retry later. Close-on-exec prevents inheritance after exec.
Unsafe or unsupported lock setup fails closed. Ordinary same-hub store contention
waits under owned persona fences, including the post-launch survival-watch reopen.
Different endpoint discoveries accessing one store do not serialize each other;
use the same actual endpoint identity for every participant.

Balance apply acquires its planned persona fences as one sorted group through the
history and audit commit. Its nonblocking admission gate serializes applies within
one hub; `fl watch balance --apply` uses HTTP and has no local apply path. Running
multiple hubs on one runtime is unsupported: global `min_interval_ms` is not
serialized across separate hubs applying disjoint persona batches ([#599](https://github.com/Artificial-Humanity/FerroLoop/issues/599)).

This is the explicit-operation portion of [#592](https://github.com/Artificial-Humanity/FerroLoop/issues/592).
Same-hub background paths share the normalized persona mutex. Cross-process fencing
is still missing for the GET sessions sweep, dead-session reconciliation, automatic
classification and recovery health updates, and background restart/PID paths. These
remain the deferred background portion and must gain file fencing before that issue
can close or the new command can be deployed. No deployment is authorized by this change. The command prints `runtime_spawned`,
`runtime_live`, `hydration_dispatched` and `hydration_status`; it exits non-zero when no
live runtime resulted. `runtime_live` is a serde-default field on the restart response
and reports current-operation evidence rather than a saved ownership flag.

The fast automatic guard remains three respawns in ten minutes, with the existing
30/60/120-second backoff. A fourth failure enters a durable recovery incident. The
first slow launch is due ten minutes later; at most six launches are attempted,
each at least ten minutes after the previous one. A late reconcile pass attempts
one launch rather than catching up. When an old `crash_loop` record has no incident
state at upgrade, its first launch is due about ten minutes after the new hub first
reconciles it; existing stopped records are eligible automatically.
After six unsuccessful recoveries the session stays stopped until manual action.

A successful spawn does not refill the incident budget. The incident resets after
ten minutes of observed healthy running (a proven live process route or an Alive
runtime observation), or after an explicit manual restart persists a live result.
Gone or Unknown runtime observations break the healthy interval. Refused or failed
manual launches retain recovery state. Missing vendor prerequisites before any spawn spend
neither fast nor slow attempts; their persisted readiness deadline never shortens an
existing slow deadline. A real spawn stays charged in both phases, including when a
second readiness probe shows the daemon disappeared during startup, and keeps the normal
phase deadline (fast backoff or ten-minute slow spacing). Other preparation failures
consume an attempt and the current phase's backoff, except `Conflict` errors, which are
not charged. FerroLoop does not start a vendor daemon automatically.

Crash-loop notices go to the active project Lead, using registry project coverage.
If the stopped persona is a Lead, or the project has no Lead, active Lab Managers
also receive the notice. No notice goes to the stopped persona itself. Initial
incident and final exhaustion notices have separate per-recipient delivery state;
failed recipients and newly registered recipients are retried. Notices include the
next due time as UTC text or final-stop status and the saved-settings restart command.
The hub logs fast-guard entry and final exhaustion, emits `session.crash_loop` and
`session.recovery_exhausted` events, and rate-limits warnings when no eligible recipient
can be notified. Unchanged reconcile ticks append no recovery or session revisions.
A recovery-state read or decode error skips that record for the tick rather than resetting
its budget; a health-state write error likewise leaves other records eligible for inspection.

The disjoint runtime record adds serde-default fields `recovery_started_at_ms`,
`recovery_attempts`, `recovery_healthy_since_ms` and
`crash_loop_notified_recipients`. Empty fields are omitted. The existing
`next_allowed_at_ms` stores the phase deadline, and `crash_loop_notified` is retained
for old readers. New delivery decisions use recipient keys `recovery:<name>` and
`exhausted:<name>`, so the legacy aggregate flag cannot suppress required fallback
or exhaustion delivery.

The seven Codex records affected by the 2026-11-17 reboot were already recovered
by hand. This policy bounds future recovery incidents; it does not dispatch a
restart of those sessions during installation or deployment.
