# Antigravity CLI under the FerroLoop host

This document records how FerroLoop runs the Antigravity CLI (`agy`): every place the code
touches it, how a hosted Antigravity session behaves through its life, how messages reach it,
and the gaps we know of. It combines two reviews done on 2026-11-10:

- a code review of `origin/main` at `8b632756`;
- a runtime review on `ai-lab-0` with `agy` 1.5.3 (daemon), 1.5.4 and 1.5.5.

Line references are at `8b632756`. Claims marked *(inferred)* were not observed directly.
Everything else was either read in the code or observed on the host.

## Summary

| Topic | What is true today |
| --- | --- |
| Launch | `agy <posture flags> --remote-control [--model <base>-<low/medium/high>] --prompt-interactive "<hydration prompt>"` in an fl-host PTY. There is no resume flag. |
| Language server | Every `agy` process runs **one language server inside its own process**, on two random loopback ports (gRPC and HTTP). There is no shared server. |
| Delivery | One `agy agentapi send-message --title=<t> <conversation_id> <body>` per message. It returns in about 0.1 s after queuing; the queued message runs after the current turn. If the command fails, FerroLoop falls back to a bracketed paste into the PTY. |
| Restart | Always a **fresh conversation**. Context is lost; only the FerroState handoff carries over. `agy --conversation <id>` *can* resume with context, but FerroLoop does not use it. |
| Decommission and remodel | FerroLoop deletes the conversation on the language server (`DeleteCascadeTrajectory`). Brain files stay on disk. |
| Self-update | `agy` replaces itself in place (same pid, `execve` of a renamed `.old` binary). The relaunch starts a **new conversation** and leaves stale state behind (see [Self-update](#self-update)). |
| Remote control | Each `--remote-control` process registers its own instance in the Antigravity app, with a random name. The named instance `AI-Lab-Antigravity` is the separate `agy remote-control serve` daemon. |
| Hooks | FerroLoop installs no Antigravity hooks. The agent runs `fl` commands itself from the hydration prompt; the workspace MCP file provides `fl wire mcp` and `fl state mcp`. |

## Components on the host

```
systemd --user
├── antigravity-cli-daemon.service: agy remote-control serve       (not managed by FerroLoop)
│     language server in process, MCP children from the global config
│     instance name: userSettings.cliRemoteControlHostname ("AI-Lab-Antigravity")
└── ferroloop-host.service: fl host serve
      └── fl --session-supervisor (one per hosted session, PTY leader)
            └── agy --mode auto --remote-control --model ... --prompt-interactive ...
                  ├── language server (same process; HTTP and gRPC on random 127.0.0.1 ports)
                  ├── MCP servers (global + <workspace>/.agents/mcp_config.json)
                  └── tool commands (direct children; ANTIGRAVITY_* env, see below)
```

- **Remote control** is a WebRTC DataChannel. Signalling polls `jetski-webchannel.googleapis.com:443`; TURN runs over TCP 3478. No inbound port is opened.
- **The app can drive any `--remote-control` process**, including telling it to update (`ApplyUpdate`).
- **A clean exit unregisters the instance.** Both SIGINT and SIGTERM give a clean exit. A crash, SIGKILL or update relaunch leaves a stale entry in the app.
- **Each `--remote-control` start also creates one empty side conversation.**

## On-disk state

Everything lives under the app data directory `~/.gemini/antigravity-cli/`. Every `agy` process on the host shares it.

| Path | Content |
| --- | --- |
| `brain/<conversation_id>/` | Transcripts (`.system_generated/logs/transcript.jsonl`, `transcript_full.jsonl`), tool outputs (`steps/<n>/output.txt`), tasks, injected messages (`messages/<uuid>.json`, shown as `SYSTEM_MESSAGE` steps) |
| `conversations/<id>.db` | One SQLite file per conversation; `conversation_summaries.db` beside them |
| `cache/last_conversations.json` | Workspace directory → last conversation id (used by `agy -c`) |
| `cache/conversation_metadata.json` | Title, step count, workspace and project per conversation |
| `presence/<conversation_id>.lock` | Exclusive `flock` held by the process that owns the conversation |
| `log/cli-YYYYMMDD_HHMMSS.log`, `cli.log` | One log per process that starts a language server; `cli.log` points at the newest |
| `bin/agentapi` | Shim rewritten on every tool call to `exec "<that session's agy>" agentapi "$@"`. The last writer wins |
| `settings.json` | `model`, `toolPermission`, `permissions.allow`, `trustedWorkspaces`, … `/model` in any session rewrites the global `model`. Accepting a trust prompt appends to `trustedWorkspaces` |
| `updater/`, `last_check.timestamp` | Self-update lock and a 15-minute check throttle |

Under `~/.gemini/config/`:

- `config.json`: plugin enablement and the remote-control host names.
- `mcp_config.json`: the **global** MCP servers.
- `hooks.json`: Antigravity hooks; not written by FerroLoop.
- `projects/<id>.json`: projects.

Sign-in material lives in `~/.gemini/antigravity-cli/antigravity-oauth-token` and related files. FerroLoop never reads it.

**Identifiers.** The conversation id is the same value everywhere:
- `ANTIGRAVITY_CONVERSATION_ID`;
- the brain directory name and the `.db` name;
- the presence lock;
- the `cascadeId` in language-server RPCs.

`ANTIGRAVITY_TRAJECTORY_ID` is a different id.

### Logs

- **Which commands write a log.** An interactive session, `agy -p`, `agy models` and `agy agents` each create exactly one new log and repoint `cli.log`. `agy --version`, `changelog`, `mcp list`, `plugin list`, `remote-control status` and `agentapi send-message` create none.
- **Shared log files.** Two processes that start in the same second **share one log file** and interleave their lines. We saw one agent's language-server ports in another agent's log. `agy --log-file <path>` overrides the location.
- **Port announcement format** (the source line numbers vary by version):

  ```
  I1110 18:29:08.229862  29 server.go:2341] Starting language server process with pid 1556425
  I1110 18:29:08.230293  29 server.go:2400] Language server version: 1.5.5
  I1110 18:29:08.230580  29 server.go:1107] Language server listening on random port at 42005 for HTTPS (gRPC)
  I1110 18:29:08.230609  29 server.go:1115] Language server listening on random port at 40937 for HTTP
  ```

- **No pruning.** We saw no retention limit: 74 files back to 2026-10-29, 143 MB in all. Logs contain the account email and summaries of agent actions.

## Environment

- **Tool commands only.** `agy` exports these only to the commands it runs for the agent: `ANTIGRAVITY_AGENT`, `AGENTAPI_EXE`, `APP_DATA_DIR`, `CONVERSATION_ID`, `CSRF_TOKEN`, `LS_ADDRESS`, `LS_VERSION`, `PROJECT_ID`, `SOURCE_METADATA`, `TRAJECTORY_ID`. It also puts `~/.gemini/antigravity-cli/bin` first on `PATH`.
- **Not the processes themselves.** The `agy` process, its language server, its MCP servers and the daemon carry none of these variables.
- **Launch environment.** FerroLoop starts hosted `agy` with a reduced environment: `AGENT_PERSONA`, `AGENT_HYDRATION_GENERATION`, `FL_HOSTED_ID=antigravity:<persona>`, `PATH`, `TERM`, `COLORTERM`, plus the baseline (`vendor_adapter.rs:584-608`, `lifecycle.rs:1003-1021, 1703-1739`). Sign-in works with that environment: `agy` falls back to its token file when no keyring is available.

## MCP configuration

| File | Loaded by `agy`? | Written by |
| --- | --- | --- |
| `~/.gemini/config/mcp_config.json` (global) | Yes, at start | Not by FerroLoop |
| `<workspace>/.agents/mcp_config.json` | Yes, at the first turn; JSON with comments is accepted | `fl mcp` sync (`crates/mcp/src/vendor/antigravity.rs`), strict JSON, atomic, owned |
| `~/.gemini/antigravity-cli/mcp_config.json` | **No** | `fl vault install --user`, `fl state install --user` (`cli/src/cmd/vault.rs:685-811`) |

`agy mcp list` shows only the global file.

## Lifecycle of a hosted session

### Spin-up

Code: `lifecycle.rs:5550-6080`; adapter `vendor_adapter.rs:928-1016`.

1. **Records.** The hub writes the hosted record `antigravity:<persona>`. The fl-host session is `<Persona>.antigravity` and the tmux-compatible target is `<Persona>:antigravity`.
2. **argv.**
   - The binary is `FL_WIRE_AGY_BIN`, else `~/.local/bin/agy`, else `agy` on `PATH`.
   - The posture flags are below.
   - `--remote-control`.
   - `--model <base>-<effort>` when a model is set. The effort is low, medium or high, and defaults to medium.
   - `--prompt-interactive "<hydration prompt>"`.
3. **Trust prompt.** The hub watches the PTY for the trust prompt ("Do you trust the contents of this project?") and answers Enter. The prompt still runs after that, and the folder is added to `trustedWorkspaces`.
4. **Hydration.** Following the hydration prompt, the agent runs `fl wire register <Name> --replace` in a tool command. That captures the route from the `ANTIGRAVITY_*` variables (`identity.rs:4865-4876`) and confirms hydration.

| Posture | `agy` flags |
| --- | --- |
| ask | not supported |
| accept-edits | `--mode accept-edits` |
| auto | `--mode auto` |
| plan | `--mode plan` |
| bypass | `--dangerously-skip-permissions` |

Rule-level `--settings` output exists in the compiler but has no production caller.

### Message delivery

Code: `dispatcher.rs:1378-1497`.

- **The command.** FerroWire runs `<agentapi exe> agentapi send-message --title=<title> <conversation_id> <body>`.
- **Its environment.** The child gets an allow-list of `GEMINI_*`, `GOOGLE_*`, `ANTIGRAVITY_*` and `AGY_*` variables, plus the route's `ANTIGRAVITY_LS_ADDRESS` and `ANTIGRAVITY_CSRF_TOKEN`.
- **Timing.** The command returns after queuing. The message runs when the current turn ends.
- **The language server matters.** Whichever language server the address names takes the message. **If it is not the server that runs the conversation, it loads the conversation and runs a turn itself, headless.** The reply never appears in the agent's session. With no address the command fails ("ANTIGRAVITY_LS_ADDRESS is not set"); with no csrf it fails as unauthenticated.
- **Fallback.** On a non-zero exit or a 60 s timeout, FerroWire pastes the body into the fl-host PTY (`dispatcher.rs:1283-1376`).
- **No busy handling.** There is no busy, hold or settle logic for Antigravity.

**Reset** sends `RevertToCascadeStep` to step 0 and truncates the transcripts. **Interrupt** sends `CancelCascadeInvocation`. Both use raw HTTP to the language server with the `x-codeium-csrf-token` header (`dispatcher.rs:1520-1632, 1759-1790, 1884-2010, 2147-2337`). Reset has no PTY fallback. Interrupt falls back to fl-host `Interrupt`; SIGINT makes `agy` **exit** (cleanly), not cancel the turn.

### Route discovery

Code: `identity.rs:2601-2916`, run at most every 60 s by the daemon and every 2.5 s by the hub.

1. **Read the logs.** Read `cli.log` and the 8 newest `log/cli-*.log`. Take the newest "listening on random port … for HTTP" line that is not followed by a shutdown line. This sets one host-wide shared address.
2. **Read process environments.** Read `/proc/*/environ` of every same-user process for `ANTIGRAVITY_LS_ADDRESS`, `CSRF_TOKEN` and `AGENTAPI_EXE`, paired by port.
3. **Fall back to stored routes.** Fall back to the stored Antigravity routes, newest first.
4. **Refresh routes.** For every Antigravity route:
   - **Keep its own pair** if its address is listening and it has a csrf.
   - **Otherwise take the shared pair** if `ls_pid` is unset, dead, or equal to the shared pid.
   - **Otherwise clear** a dead address.
   - **Explicit routes** keep their conversation id: passive discovery never replaces them (`identity.rs:2178-2183`).
5. **Create passive routes** from brain transcripts. A transcript's first 50 lines are matched for "take the name of …" and similar markers.

A route made by `fl wire register` has `ls_pid` unset, so it always qualifies for the shared pair once its own port stops listening. Every `agy` process now has its own language server, so the shared pair can belong to **another agent or to the daemon**. Together with the headless-turn behaviour above, mail can then be answered by a turn the agent never sees.

### Model and effort changes

- **In FerroDeck (#463)** a change only stores an override for agy (`api.rs:7187-7360`, `method: override-only`). It applies at the next launch.
- **`/model` inside the agent's session** changes the model live, with the same process and ports. FerroLoop shows the change through discovery but does **not** store it on the hosted record. The next restart therefore reverts it.
- **The global setting.** `/model` also rewrites the global `settings.json` `model`.

### Restart and respawn

Code: `lifecycle.rs:7549-8095`; respawn #483 `7014-7174, 8212-8434`.

- **Always a new launch.** Every restart, crash respawn and host restart starts a new `agy` with a new conversation, new ports and a new csrf. The previous conversation is not resumed; its context is lost, and the agent continues from its FerroState handoff.
- **The stale route lingers.** The old explicit route stays in `routes.json` until the new session runs `fl wire register --replace`. In between, discovery can point it at the shared address (see above), and mail still reports `Acked`.
- **Resume is possible.** `agy --conversation <id>` resumes a conversation **with its context**, as long as the conversation was not deleted. An unknown id prints a warning and silently starts a new conversation, with exit code 0.

### Remodel and decommission

Code: `lifecycle.rs:7303-7490, 6616-6862, 9017-9255`.

Teardown follows these steps:
1. Tombstones the conversation ids.
2. Sends `DeleteCascadeTrajectory` for each one, best effort with a 2 s budget, to the route's language server.
3. Removes the routes.
4. Stops the fl-host session.

Brain directories stay on disk. A remodel is a teardown followed by a spin-up, with the `remodel-<persona>` handoff.

### Self-update

`agy` updates itself, either in the background on its own schedule (at most every 15 minutes) or when the app sends `ApplyUpdate` over remote control. It renames the running binary to `~/.local/bin/agy.<nanoseconds>.old`, writes the new version to `agy`, and **re-executes itself in place**: same pid, `comm` becomes `agy.<digits>`, and the executable is the `.old` path, which is deleted later.

Observed with Gregory at 18:29 on 2026-11-10:

- **New conversation.** The kept argv re-ran `--prompt-interactive`, so the hydration prompt started **a new conversation**. The old conversation lost its continuity.
- **New ports and a new log.** The process came back with a new log, new ports and *(inferred)* a new csrf.
- **Stale app entry.** A new remote-control instance was registered, and the old one stayed in the app.
- **Zombie children.** The old MCP children were left as zombies.
- **Screen in the wrong place.** Because the inherited stdout and stderr were the log file, the relaunched process drew its screen into the log, not into the PTY.
- **Broken shim.** The `bin/agentapi` shim and the session's `ANTIGRAVITY_AGENTAPI_EXE` pointed at the `.old` path, which broke when that file was deleted. FerroLoop already replaces an `.old` exe in routes (`identity.rs:839-871`).

The daemon cannot update itself after its `.old` binary is deleted: `fork/exec …agy.<n>.old: no such file or directory`. It stays on its old version until it is restarted.

## Known gaps

Each gap links to its issue. Gap 12 has no issue yet.

| # | Severity | Gap | Issue |
| --- | --- | --- | --- |
| 1 | High | A route without its own live language server takes the host-wide newest one, which may belong to another agent or the daemon. That server then runs the message as a headless turn the agent never sees. | #489 |
| 2 | High | Restart, respawn and host restart lose the conversation, although `agy --conversation <id>` can resume it. Remodel and decommission delete it. | #490 |
| 3 | High | A self-update relaunch silently starts a new conversation, writes the screen into a log file, and leaves stale instances and zombie processes. The app can trigger it at any time on any `--remote-control` session. | #491 |
| 4 | Medium | The stale explicit route lingers after a restart until `register --replace`; if hydration fails it never heals, and mail still reports `Acked`. | #490 |
| 5 | Medium | Logs from processes started in the same second share one file, so ports can be attributed to the wrong process. FerroLoop does not use `--log-file`. | #489 |
| 6 | Medium | `ls_pid` means three different things (TUI pid, any process with the env, or unset), and liveness and endpoint sharing depend on it. With one language server per process, the hosted record pid is the right liveness anchor. | #489 |
| 7 | Medium | Model switches made inside the session are not stored on the hosted record; FerroDeck's switch is override-only; the FerroDeck settings modal offers `xhigh`, which the hub refuses. | #492 |
| 8 | Medium | `fl vault install --user` / `fl state install --user` write `~/.gemini/antigravity-cli/mcp_config.json`, which `agy` does not read, and replace a file that does not parse with `{}`. | #492 |
| 9 | Low | The PTY fallback after a failed `send-message` has no idempotency check. Now that `send-message` is known to return at once, the risk is limited to errors after queuing. | #492 |
| 10 | Low | Interrupt falls back to a signal that makes `agy` exit; reset has no fallback. | #492 |
| 11 | Low | `runtime_matrix` counts sessions with adapter `agy`, but sessions are labelled `antigravity`, so it always shows 0 (`runtime_matrix.rs:789-798`). | #492 |
| 12 | Low | The csrf token is stored in plain text in `routes.json`. | — |
| 13 | Low | The FerroDeck spin-up preview shows wrong Antigravity flags and paths (`--yolo`, `~/.gemini/antigravity/settings.json`, `terminalAutoExecutionPolicy`). | #492 |

### Stale documents (tracked in #492)

- `docs/vendor-runtimes.md:191`: the hub does call `RetrieveUserQuotaSummary`.
- `docs/vendor-child-env.md:152`: hosted PTY sessions do not inherit the full environment.
- `docs/permissions.md:250-251`: says spin-up is not wired.
- `specs/2026-11-03-native-agent-host-design.md:164, 196`: "Agy not hosted in v1".
- The hydration prompt says the session "has just been reset (`/clear`)" on a fresh Antigravity launch.

## Operating notes

- **Before restarting an Antigravity agent, check that its conversation still exists.** Remodel and decommission delete it, so `--conversation` cannot bring it back afterwards.
- **Do not run `agy -p`, `agy models` or `agy agents` on the host while Antigravity agents are live.** Each creates a log, moves `cli.log`, and can change route discovery for up to a minute. `agy --version` and `agy agentapi …` are safe.
- **The remote-control daemon is separate from FerroLoop.** Restart it (`systemctl --user restart antigravity-cli-daemon.service`) after an update so that it runs the current binary. Restarting it does not affect hosted agents.
- **Grey instances in the app are stale registrations** from relaunches or crashes. They can be removed in the app.
