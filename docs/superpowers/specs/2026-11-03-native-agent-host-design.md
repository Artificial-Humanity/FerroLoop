# Native agent host — approved source snapshot

This is the design source for Local Record #82 / Issue #217. It preserves the
published v2 specification and v3/v4.5 amendments in order. Later amendments take
precedence. The implementation plan identifies the delivery slice it implements.
The snapshot does not resolve owner decisions or claim runtime conformance.

Sources:

- [v2 specification](https://github.com/Artificial-Humanity/FerroLoop/issues/217#issuecomment-6593894534)
- [v3 amendments](https://github.com/Artificial-Humanity/FerroLoop/issues/217#issuecomment-6593960640)
- [v4.5 amendments](https://github.com/Artificial-Humanity/FerroLoop/issues/217#issuecomment-6594170611)
- [Owner direction: native host on all three platforms](https://github.com/Artificial-Humanity/FerroLoop/issues/217#issuecomment-6593806136)

Snapshot retrieved 2026-11-06. The latest Ferris handoff specifies Codex before
Claude for adapter delivery, after the shared host and controller prerequisites.
This changes delivery order only, not an adapter's acceptance gates.

---

# Issue #217 implementation spec: native `fl host` (Linux, macOS, Windows)

Spec v2, agreed by the Strategy Council: Felicity (Claude) and Fenris (Muse), for Ferris. Fenris wrote §6 and red-teamed the rest. The Owner's ruling is to build the native cross-platform host directly, with no tmux phase.

## 0. Starting facts (origin/main)

- **Spin-up does not start an agent.** It runs `tmux new-session -d` and pushes the hydration prompt to a route found by discovery (`lifecycle.rs:1378-1382`). Nothing in the tree launches the vendor binary, so hosting depends on wrapper scripts that live outside the repo.
- **Prompt delivery to Muse and Grok scrapes the screen**, and so does Claude's fallback path. It runs `load-buffer`, `paste-buffer`, `send-keys Enter`, then `capture-pane` looking for `[Pasted Content` (`dispatcher.rs`).
- **The Claude socket path has gaps.** Its route writes and never reads an acknowledgement. A reset sends `/clear` as text, and an interrupt is possible only through tmux keystrokes.
- **`terminal.rs` (the FerroDeck console) uses pipes, not a PTY**, so a TUI sees `!isatty` and a resize has no effect.
- **The stack does not build on Windows today.** 79 files under `crates/` use `std::os::unix`, and CI runs only on `ubuntu-latest`. "In unison" therefore means the new crates build and test on all three OSes from their first PR. A Windows build of the hub, wire and CLI is its own port track (§7).

## 1. Crate boundary and IPC

**Two new crates**, registered in `.fl/architecture.toml`:

| Crate | Contents | Depends on |
|---|---|---|
| `fl-host-proto` (`crates/host-proto`) | Protocol types, framing, the platform transport (connect and listen), and a blocking client plus a tokio client. It carries no PTY code. | serde, serde_json, tokio (`net`), `windows-sys` (cfg(windows)), libc (cfg(unix)) |
| `fl-host` (`crates/host`) | The daemon: supervisor, PTY and pipe children, screen grid, subscriber fan-out. | `fl-host-proto`, `fl-core` (vendor descriptors only), `portable-pty`, `vt100`, tokio |

**New edges:**

| From | To | Why |
|---|---|---|
| `fl_host` | `fl_host_proto` | serves the protocol |
| `fl_host` | `fl_core` | reads vendor descriptors |
| `fl_hub` | `fl_host_proto` | lifecycle and the console |
| `fl_wire` | `fl_host_proto` | delivery |
| `fl_cli` | `fl_host` | the `fl host serve` subcommand |

The hub and wire never link `portable-pty`.

**Daemon.** The daemon is a mode of the existing binary: `fl host serve`, `fl host status` and `fl host install-unit`. That last command generates the per-user unit:

| OS | Unit | Notes |
|---|---|---|
| Linux | systemd **user** unit | boot persistence needs `loginctl enable-linger` |
| macOS | LaunchAgent with `KeepAlive` | an agent, not a LaunchDaemon, so the user keychain stays reachable |
| Windows | per-user Task Scheduler "at logon" task | not a Windows service, because session 0 has no user profile or credential store for the vendor CLI logins. Installing the task needs no admin rights. |

**Transport,** behind one `HostTransport` in `fl-host-proto`:

- **Linux and macOS:** a Unix domain socket at `<runtime dir>/fl/host.sock`.
  - The runtime dir is `$XDG_RUNTIME_DIR` on Linux and the per-user `confstr(_CS_DARWIN_USER_TEMP_DIR)` on macOS.
  - The directory is 0700 and the socket 0600, created through the store's existing `bind_private_unix` / `socket_path_fits` pattern. `sun_path` is 108 bytes on Linux and 104 on macOS.
  - The daemon checks the peer's credentials, refusing any uid other than its own: `SO_PEERCRED` on Linux, `getpeereid` on macOS.
- **Windows:** a named pipe `\\.\pipe\fl-host-<user SID>`, created with:
  - an explicit DACL that grants only the current user's SID (`windows-sys`);
  - `FILE_FLAG_FIRST_PIPE_INSTANCE`, which stops pipe squatting;
  - `PIPE_REJECT_REMOTE_CLIENTS`.

  tokio's `named_pipe` module carries the stream.
- **On every OS**, the first frame is `hello { token, proto }`. The token is a random value the daemon writes at start to a user-only file (mode 0600, or a user-only ACL). This handshake is defence in depth on top of the OS checks, and it pins the protocol version.
- **Test isolation.** Clients find the daemon through `FL_HOST_SOCK`, which on Windows is the pipe-name suffix. This follows the `FL_DB` / `FL_WIRE_TMUX_BIN` precedent. Black-box tests set it so that each one runs its own isolated daemon and never touches a developer's real host.
- **Which client may call which verb.** `fl-wire` calls only `send`, `write`, `attach` and `events`. Only the hub's lifecycle and `fl cli` call `spawn` and `kill`. A same-user socket cannot enforce this split, which is the same trust as tmux today, where any same-user process can `kill-session`. The FerroWire peer relay must never gain a path to `spawn`.
- **No loopback TCP.** An unauthenticated loopback listener would let any local process spawn agents, so the daemon never opens one. A remote node reaches its own `fl host` only through its own hub, not through the host socket.

**One daemon per user, and no silent version skew:**
- The daemon takes an exclusive lock at start: `flock` on `<runtime dir>/fl/host.lock` on Unix, or a named mutex `Local\fl-host-<SID>` on Windows. A second `fl host serve` exits with "already running (pid N)".
- Clients never start a second host.
- In `hello`, the daemon answers `welcome { daemon_version, proto }`. If the `proto` major differs, the client fails with "fl host <v> is running; `fl host restart` stops its agents". Nothing restarts the daemon automatically.
- `fl host status` shows both versions.

**Start-up:**
- Production uses the per-user unit from `fl host install-unit`, which gives boot persistence.
- Without a unit (development), the first client that finds no daemon starts `fl host serve` detached, and the lock settles any race between two such clients.
- The daemon exits on idle only when it has no children and was started on demand. It never exits while a child lives.

**Framing.** Each frame is a 4-byte big-endian length followed by 1 type byte and the payload.

- Control frames are JSON.
- Output frames are raw bytes (`data { session, seq }` header + bytes), never base64.

The trust boundary is unchanged from tmux: any process of the same user may drive the daemon. Owner-only rules stay enforced in the hub.

## 2. Protocol verbs

| Verb | Request → reply/stream | Notes |
|---|---|---|
| `spawn` | `{ session, kind: pty\|pipe, argv[], cwd, env{}, size? }` → `spawned { pid }` | `argv` is an array and is never run through a shell. `cwd` must exist. `kind: console` is a PTY shell for the FerroDeck console. The daemon starts every child from a scrubbed base environment: `PATH`, `HOME`/`USERPROFILE`, `LANG` and `TERM`, plus the variables in the new descriptor field `host_launch.env_allow`. A `SubscriptionCli` descriptor carries only `default_binary` today (`vendor.rs:43`), so this field is new. The request's `env` is merged on top and must also be in `env_allow`. Vault secrets never travel over the host IPC: a child that needs a credential gets it through FerroGate. |
| `kill` | `{ session, grace_ms }` → `exited` | Terminates the whole process tree (see §3). |
| `list` | → `[{ session, kind, pid, state, started_at, exit? }]` | Gives the hub the truth about each session, replacing the `exists` probe. |
| `write` | `{ session, bytes }` | Input to a PTY. |
| `send` | `{ session, line }` | One NDJSON line to a pipe child's stdin. |
| `resize` | `{ session, cols, rows }` | PTY only. |
| `attach` | `{ session, since_seq? }` → `snapshot { seq, bytes }`, then `data` frames with seq greater than that | The snapshot is `vt100::Screen::contents_formatted()` plus the cursor. If `since_seq` is still in the ring buffer, the daemon replays raw bytes instead of a snapshot. |
| `events` | `{ session? }` → `event` stream | Carries `spawned`, `exited { code, signal }`, and `line { session, json }` for each stdout line of a pipe child. |
| `signal` | `{ session, sig: interrupt\|terminate }` | Acts at process level. Vendor-level cancel verbs are sent by the hub as `send` or `write`. |

**The daemon stays vendor-neutral.** It never parses vendor protocols. Turn boundaries (a stream `result`, Claude's `UserPromptSubmit` and `Stop` hooks) are derived in the hub from `line` events and hook calls.

## 3. Child lifecycle per OS

The code is shared. `cfg` is used only where the OS primitives differ.

**Common to all OSes:**
- Spawn goes through `portable-pty` (`native_pty_system()`) for PTY children, and through `tokio::process` with piped stdio for pipe children.
- PTY children get `TERM=xterm-256color`.
- A reader task feeds every PTY child's output into:
  1. a per-session `vt100::Parser`, with scrollback that is configurable and 2000 lines by default;
  2. a raw ring buffer with `seq` numbers, a hard cap of 4 MiB per session.
- **Back-pressure has two policies, by stream kind:**
  - **PTY viewers (attach):** the child never blocks on a slow viewer. Each subscriber has a bounded queue. When one overflows, the subscriber gets an explicit `resync` frame followed by a fresh snapshot. Nothing is dropped silently, and the grid stays the truth. This deliberately differs from `terminal.rs`, where a slow socket blocks the child: an agent must not stall because a browser tab is slow.
  - **Pipe protocol lines (`events` → `line`):** these are protocol messages, so they are lossless. The daemon buffers up to 8 MiB per session, then stops reading the child's stdout until the hub drains. That is real back-pressure, and nothing is ever dropped.
  - **`exited` on both stream kinds:** a queue slot is reserved for it when the session opens, which reuses the `terminal.rs` pattern.

**Unix (Linux and macOS):**
- Each child is a session leader (`setsid`, via portable-pty for PTYs and `pre_exec` for pipes), so `kill` sends `killpg`: SIGTERM, then SIGKILL after `grace_ms`. This catches the vendor's MCP servers and tool subprocesses.
- PTY masters are `FD_CLOEXEC`.
- Children are reaped on a waiter task.
- Linux pipe children set `PR_SET_PDEATHSIG(SIGTERM)`. Right after the `prctl`, the child checks `getppid()` and exits if it has already been reparented, which closes the race with a daemon that died in between.
- On both Linux and macOS, the daemon records `(pid, pgid, start time)` for each child in `<runtime dir>/fl/host-children.json`, and kills any surviving groups on startup.
  - This is one Unix path. It covers macOS, which has no `pdeathsig`. It also covers a daemon killed with SIGKILL, whose grandchildren that called `setsid` would escape `pdeathsig`.
  - Start time is checked through `proc_pidinfo` on macOS and `/proc/<pid>/stat` on Linux, which guards against pid reuse.
  - The cost is one small write per spawn and per exit.

**Windows:**
- PTYs use ConPTY through portable-pty, which needs Windows 10 1809 or later.
- Every child, PTY or pipe, is placed in a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The whole tree therefore dies with the daemon.
- `kill` writes `0x03` to the ConPTY input, then calls `TerminateJobObject` after `grace_ms`.
- `resize` calls `ResizePseudoConsole`.
- ConPTY re-renders its output, so the bytes are not transparent. On Windows the `vt100` snapshot is the authority for attach, and the raw ring buffer is a convenience.
- **ConPTY drain.** Closing the pseudoconsole while its output pipe is undrained can hang the child. So a PTY `kill` keeps the reader running until EOF, bounded by `terminal.rs`'s `READER_DRAIN_GRACE` / `READER_DRAIN_CEILING` pattern (with named constants in `fl-host`), and only then calls `ClosePseudoConsole`. Unix uses the same bounded drain before it closes the master.

**Daemon death means agent death on every OS.** A PTY child gets SIGHUP when its master closes, and the job kills the tree on Windows. This is inherent to owning the PTYs, so the daemon must stay small, stable and versioned (`proto`). A hub upgrade never restarts it. The hub's existing `restart_policy` on `HostedSessionRecord` respawns agents after a daemon restart.

## 4. Replacements in hub and wire

**`crates/hub/src/lifecycle.rs`:**
- `SessionRuntime` (spawn, kill and exists, each returning bool) becomes `SessionHost`, which returns `Result`s:
  - `spawn(SpawnSpec) -> HostedChild`
  - `kill`
  - `state(session) -> Option<ChildState>`
- The production implementation is `FlHostClient`. `FakeRuntime` becomes `FakeHost`, under `cfg(test)`.
- **Spin-up launches the vendor binary itself.** `VendorDescriptor` (fl-core) gains a `host_launch` field per transport: a `pipe` argv template and/or a `pty` argv template, with `{model}`, `{cwd}` and `{resume}` slots. The per-operation capability table (§6) is recorded beside it.
- Hydration goes to the hosted session directly, not to a discovered route.
- **Liveness.** For hosted sessions, liveness reads `fl host` `list` and `events`. This replaces the wrapper heartbeats and `/proc` probes behind `fl agent liveness` and FerroWatch for those sessions. Unmanaged sessions keep today's sources.
- `HostedSessionRecord.tmux_target` becomes `host_session`. A `serde(alias)` is kept, so an old record reads as unmanaged. `runtime_spawned` keeps its meaning, and the teardown and remodel checks key on `host.state()`.

**`crates/wire/src/dispatcher.rs`:**
- A new `VendorRoute::Hosted { session, vendor, kind }`. The hub writes it at spawn; discovery never does. For the same persona it takes precedence over any discovered route.
- Delivery depends on `kind`:
  - **Pipe:** the vendor adapter encodes the message as that vendor's stream input (a Claude `stream-json` user message, a Codex `app-server` request, Muse MSP, agy `stream-json`), then calls `send`.
  - **PTY:** the hub calls `write` with sanitised bracketed-paste bytes plus Enter, then confirms against the prompt line of an `attach` snapshot. That snapshot check replaces `capture-pane`.
- Reset, interrupt, compact and model switch follow §6.
- Routes for unmanaged sessions (agents started by hand) keep today's discovery and delivery until they are migrated (§7, step 6).

**`crates/hub/src/terminal.rs`:**
- A console session is an `fl host` `console` PTY: `open` becomes `spawn` plus `attach`, `input` becomes `write`, `resize` becomes `resize`, and `close` becomes `kill`. A late join receives a snapshot.
- The frame order (`opened` first, `exited` last) and the bounded stdin queue are kept.
- Output moves from JSON text frames to binary WebSocket frames (a session-id prefix plus bytes), with one copy per hop and no re-encoding. The `ui/` change is coordinated with Felix.
- Attaching to an agent's PTY from FerroDeck uses the same `attach` verb. It is read-only unless the caller is the Owner.

## 5. Dependencies

| Crate | Version | Licence | Purpose |
|---|---|---|---|
| `portable-pty` | 0.9 | MIT | Unix openpty and Windows ConPTY |
| `vt100` | 0.18 | MIT | screen grid and formatted snapshot |
| `windows-sys` | — | MIT/Apache-2.0 | the pipe DACL and Job Objects, behind cfg(windows) |

tokio is already a workspace dependency. `cargo search` on 2026-11-03 gave `portable-pty` 0.9.0 and `vt100` 0.18.1. The version and licence of each dependency are re-checked when it is added (Apache-2.0 project). Both new crates must be registered in `.fl/architecture.toml`, because `strict_coverage = true`.

## 6. Per-operation capability table

This section is Fenris's. Every cell is traced to code on main or to a `--help` run on ai-lab-0.

Legend: **S** = structured protocol, **P** = PTY keystrokes, **U** = unverified, **–** = no path today.

| Vendor | Prompt | Reset | Interrupt | Compact | Model switch | Hosted default in v1 |
|---|---|---|---|---|---|---|
| Claude | S (socket `user` frame, unacked) | `/clear` as text, **U** | P only | – | `/model` as text, **U** | **Pipe**: a persistent `claude --print` with `stream-json` in and out. One child cannot be both pipe and PTY, so interrupt uses the stream's control `interrupt` request, which is how the Agent SDK interrupts (**U**: a conformance test is needed). If that does not work, the message stays Pending and nothing is sent. A `signal interrupt` (SIGINT to the group) is terminate-class: it can kill the CLI, where Escape in the TUI stops only the turn (see the `INTERRUPT_TMUX_KEY` comment in `dispatcher.rs` on `C-c`). So it is a last resort, used only behind its own conformance test. FerroDeck shows a pipe child as its event stream, not as a terminal. |
| Codex | S (`codex queue`) | `/new` as queued text, **U** | P only (app-server cancel **U**) | S (app-server `thread/compact/start`) | **U** | **Pipe**: an `app-server` client. Attach to the shared app-server if it is present; otherwise spawn a dedicated one with an isolated `CODEX_HOME`. Never kill a shared daemon `fl host` did not spawn. |
| Muse | P today; target S (`muse serve` MSP) | P today; MSP new-session verb **U** | P; MSP cancel **U** | – | – | **Pipe**: one `muse serve` per child over stdio, not `--listen`, so the trust ceiling is per process. Pin to the exported JSON schema and add a conformance test. |
| Agy | S (agentapi send) | S (`RevertToCascadeStep(0)`) | S (`CancelCascadeInvocation`) | – | – | **Not hosted in v1.** `fl host` does not own the language server, so agy stays discovery-only and keeps its routes. |
| Grok | P | `/new`, P | P | – | – | **PTY.** It is the only PTY-first vendor until streaming input is proven. |
| Webhook | S (socket line) | – | – | – | – | Out of scope: an external process. The route is kept. |

**Rules:**
- Capabilities live in `VendorDescriptor` as an enum per operation: `Structured { verb }`, `PtyKeys { .. }` or `Unsupported`. The docs are generated from it, never written as prose only.
- PTY keystrokes are used only where the vendor has no structured verb.
- Every **U** cell becomes a named conformance test before its status flips. This covers the Claude `/clear` over the socket and over stream input, the Codex queued slash commands and app-server cancel, and the MSP cancel and new-session verbs.
- Turn boundaries come from protocol events: a stream `result`, Claude's `UserPromptSubmit` and `Stop` hooks, and MSP or app-server events. They no longer come from polling `~/.claude/sessions/<pid>.json`.

## 7. PR sequence

Each PR follows `WORKFLOW.md`.

**CI.**
- `verify.yml` runs on `ubuntu-latest` only today.
- PR 1 adds separate `fl-host (macos)` and `fl-host (windows)` jobs, which run `cargo test`, `clippy` and `fmt` for `-p fl-host -p fl-host-proto`. Without them the `cfg(windows)` code would rot uncompiled.
- The required check `test, clippy, fmt` keeps its name. The new jobs become required once they are stable.
- `.github/` is a protected path, so PR 1 needs the Owner's review.

1. **`fl-host-proto` and the `fl-host` skeleton.**
   - Transport on all three OSes, with auth (peer creds or DACL, plus the token).
   - `hello`, `spawn` and `kill` for pipe children, `list`, `events`, and process-tree kill (killpg or Job Object).
   - Tests on all three runners, using a small `fl-host-testchild` binary. It echoes, reports `isatty` and the window size, handles resize, forks a grandchild, and exits with a given code.
2. **PTY children.** `vt100` grid, ring buffer, `attach`/snapshot/`since_seq`, `resize`, and the slow-subscriber resync. Tests:
   - snapshot equals the replayed grid;
   - a TUI-style redraw survives a late attach;
   - a slow subscriber does not block the child;
   - the grandchild is killed;
   - killing the daemon kills its children.
3. **CLI.** `fl host serve|status|install-unit`, with unit generation for all three OSes.
4. **Hosting.** `SessionHost` and `VendorDescriptor.host_launch` (spin-up launches the binary), `VendorRoute::Hosted`, and the §6 capability enum. Adapters, in this order: Claude pipe, Codex app-server, Muse `serve`, then Grok PTY. Each lands with its conformance tests for its **U** cells.
5. **Console.** `terminal.rs` moves onto the `fl host` console with binary WS output, together with Felix's `ui/` change.
6. **Remove tmux from the hosting path.** `TmuxRuntime` goes in step 4. Legacy tmux delivery for unmanaged sessions is deleted once the Owner confirms that every agent runs under `fl host`.

**Separate track:** a Windows build of `fl-hub`, `fl-wire` and `fl-cli`. It cfg-gates the 79 `std::os::unix` users behind `fl-host-proto`'s transport and the store's existing platform shims. Until it lands, Windows `fl host` is verified by its crate tests and CI, not end to end.

## 8. Owner decisions

1. **Daemon death means agent death.** This is inherent to owning PTYs; agents respawn under `restart_policy`. Is that acceptable?
2. **The unmanaged-session tmux path.** Keep it until every live agent is spun up by `fl host`, then delete it (step 6). Is that acceptable?
3. **Vendor terms.** Is headless or stream use on subscription accounts covered by each vendor's terms and quota accounting? Codex `app-server` is marked experimental.
4. **The Windows track.** Should it be scheduled now, or after Linux and macOS are in production? There are no Windows users today.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01SP4orqRK3R2jpFNhD1ViMA

## Spec v3 addendum — Codex schema evidence and three lifecycle corrections

From Fenris (Muse), incorporating Fiona's Codex review (2026-11-03). Schema claims below were verified locally against the installed `codex-cli 0.166.0` by running `codex app-server generate-json-schema --out <dir>` and reading the exported files; no inference calls were made. Spec v2 stands except as amended here.

### 1. Codex capability cells upgrade from U to S (schema-defined; runtime conformance gate retained)

The exported v2 protocol defines, in `ClientRequest.json` and the per-method `v2/*.json` files:

| Operation | Schema path | Required fields |
|---|---|---|
| Interrupt | `turn/interrupt` (`TurnInterruptParams.json`) | `threadId`, `turnId` |
| Steer a running turn | `turn/steer` (`TurnSteerParams.json`) | `expectedTurnId` (optimistic-concurrency guard) |
| Reset | `thread/start` (new thread semantics, not a queued `/new`) | — |
| Compact | `thread/compact/start` | `threadId` (already wired in `codex_control.rs`) |
| Model switch | `turn/start` carries `model` | applies to that and subsequent turns |

Consequences for §6:

- Codex interrupt is **S (schema-defined)**, not "P only (app-server cancel U)". It needs the *current* `turnId`, so the RPC owner must track turn lifecycle notifications; a blind interrupt without a live turn id is not expressible.
- Codex reset must **not** route `/new` through `codex queue --message`: `dispatcher.rs:542` documents that queue bypasses the TUI slash parser and injects literal text. Reset = `thread/start` plus re-hydration. (`queue --resume` can also exit 0 despite a failed resume — Fiona; treat queue exit status as untrusted and verify via `thread/read`.)
- Codex model switch is **S** via `turn/start.model`, not `/model` as text.
- The runtime conformance tests in §6/§7 are retained: schema presence is not runtime proof, and `app-server` is marked experimental.

### 2. Shared app-server attachment is not a Pipe child (new hosting class)

§6 v2 said "attach to the shared app-server if present". That endpoint is neither a host-owned stdio child nor a PTY: `fl host` cannot spawn, kill, or bound it. It is an **external RPC endpoint**, in the same class as the Agy language server. Therefore:

- Hosted (managed) Codex sessions use a **dedicated** app-server spawned by `fl host` as a Pipe child with an isolated `CODEX_HOME`.
- The shared Codex daemon (`codex app-server daemon`, reached via the control socket / `app-server proxy`) is an **unmanaged/external route**, discovered like Agy's LS; `fl host` never kills it.

### 3. One RPC owner per hosted session (hub/wire split)

The app-server protocol includes server→client approval requests (`CommandExecutionRequestApprovalParams`, `FileChangeRequestApprovalParams`, `ExecCommandApprovalParams`, `ToolRequestUserInputParams`) and turn-id-scoped verbs. Two FerroLoop processes speaking to one session would race request ids, approvals, and ordering. Amendment:

- For a `Hosted` session, exactly **one component owns the vendor RPC conversation**: the hub-side session controller. `fl-wire` does not open its own RPC conversation to a hosted session; delivery to a `Hosted` route is forwarded to the hub (or refused as `Pending` when the hub is unreachable), never sent on a second connection.
- The RPC owner maintains per-session generation fencing: a monotonically increasing generation per spawn, current `threadId`/`turnId`, and single-writer ordering of prompt/reset/interrupt/compact/model operations.
- `fl host` remains vendor-neutral (it owns bytes and processes, not vendor verbs), so this ownership lives hub-side, not in the daemon.

### 4. Process-tree kill corrections (§3)

- **Unix:** a saved parent `pgid` + `killpg` cannot reach descendants that called `setsid` (vendor CLIs spawn MCP servers and shells that may do so). Kill becomes: snapshot the live descendant pid set at kill time (Linux `/proc` walk; macOS `proc_listallpids` + parent chain), `SIGTERM` the union of that set and the child's process group, `SIGKILL` after `grace_ms`. The `(pid, start_time)` guard against pid reuse from v2 applies to every pid in the snapshot. Residual risk (a descendant that double-forks and reparents between snapshot and signal) is documented, not hidden; a per-session cgroup v2 scope on Linux is the named future hardening, not v1 scope.
- **Windows:** Job Object membership must be assigned **before child code runs**: create the process suspended (`CREATE_SUSPENDED`), `AssignProcessToJobObject`, then resume. For Pipe children this means spawning via `windows-sys` directly (or a suspended-spawn helper), not a plain `tokio::process` spawn followed by assignment. The same requirement applies to the ConPTY child process. PR 1's test-child gains a case that forks a grandchild immediately at start; the grandchild must die with the job.

No other §6 cells change. Claude, Muse, Agy, Grok, and Webhook rows in v2 stand as written.

## Spec v4 addendum: controller contracts, delivery, containment, bounds, budget, Windows entry point

From Felicity (Claude). It folds in:
- Fiona's Codex-seat reviews: the 8-finding report, and the 7 contracts on the first v4 draft;
- Fenris's inputs and amendments;
- the scope Ferris confirmed.

**Review status (final, after publication):**
- Felicity (Claude, author): APPROVE.
- Fenris (Muse): APPROVE.
- Ferris (Lead): agrees with the §E rule 5 admission contract.
- Fiona (Codex): APPROVE for these amendments. All her scoped findings are resolved as specification requirements, and runtime and schema conformance and the OS guarantees remain implementation acceptance gates.
- No exception to `AGENTS.md` is granted or implied. Spec v2 and the v3 addendum stand except where this amends them. Codex claims are checked against `codex-cli 0.166.0` `app-server generate-json-schema`. A schema shape is not runtime proof: every vendor operation keeps a named conformance test.

**Correction to v2 §3.** v2 said the existing `restart_policy` respawns agents. That is wrong. `HostedSessionRecord.restart_policy` is stored (`lifecycle.rs:674`) but nothing consumes it, and `restart_hosted_session` (`:1850`) is an explicit action only. Supervision is new work (§E).

### A. One session controller per hosted session

This replaces v2 §4 "wire encodes the message" and builds on v3 §3.

**Ownership.** Each hosted pipe session has exactly one `SessionController` actor in the hub. It owns:
- the protocol connection;
- request ids and responses;
- **server-to-client requests**: command, file and permission approvals, user input, and MCP elicitation;
- the current `threadId` / `turnId`.

**Epochs and leases.** These are three separate counters, so that recovery never drops its own replies.

| Counter | Owner | Changes on | What carries it |
|---|---|---|---|
| `session_epoch` | `fl host`, which is vendor-neutral | spawn and respawn only | every host event, as `{ session_epoch, seq }` |
| `thread_epoch` | the hub's controller | `Reset` and any replacement of the thread | the hub's own `delivery` and `server_request` records |
| `controller_lease` | `fl host` enforces it, the hub holds it | each controller takeover: hub restart, transport reconnect, or an authorized handoff | every `send` and `write` |

- **Contract (Fiona's text, verbatim):** "fl host stamps raw protocol lines only with session_epoch and seq. The hub SessionController derives thread_epoch from the persisted request_id/threadId/turnId ownership map after decoding the vendor protocol. It never assigns an event the current thread_epoch merely because that event arrived after Reset. Lines with no established thread identity remain session-scoped. Durable acknowledgements follow durable mapping/application."
- `fl host` never assigns a `thread_epoch`, because it cannot interpret vendor messages.
- The controller maps each event to its original records by `request_id`, `threadId` and `turnId`. A pipe child that interleaves output from its old and new threads is therefore handled correctly.
- **The lease is enforced at the byte-writing boundary.** The controller takes the lease with a host verb, `claim_lease { session } -> lease`. `send` and `write` carry the lease, and `fl host` refuses a stale one before any byte is written.
- A transport reconnect changes **only** the lease.
- A write made under a stale lease is refused, so a stale controller can never act as a writer.
- An event updates only the records it correlates to, matched by `session_epoch` and then by the vendor ids. An event from an older child or thread never drives a new intent: it is logged and applied only to its own records.

**Intents and authority.**
- The controller receives typed intents: `Prompt`, `SendNow`, `Reset`, `Interrupt`, `Compact` and `SetModel`. `fl-wire` never writes vendor protocol.
- Wire-originated `Prompt` intents ride the persisted FerroWire store. A wire message for a hosted persona stays `Pending` there until the controller consumes it, so "hub unreachable" means the message stays `Pending`, with no new transport.
- Owner and operator intents (`SendNow`, `Reset`, `Interrupt`, `SetModel`, and approval answers) arrive only through the authenticated hub API (`session_actions.rs`).
- Every intent persists its source message id and the caller's authority. The controller rechecks that authority before each privileged dispatch. A `Pending` wire message cannot carry Owner authority to reset, approve or control a session.

**Two lanes, so control is never blocked.**
- The **control lane** carries interrupts, approval answers and `SendNow` (steer). It is processed at once, even while a turn runs.
- The **prompt lane** holds queued prompts, which wait for an idle thread.
- Waiting for a prompt's turn to finish never blocks the control lane.

**Approvals.**
1. A server request is persisted on arrival, before it is shown to anyone (§C).
2. The controller answers it only under the session's configured sandbox and approval policy, or through an authorized hub action at Owner authority.
3. Raw `attach` viewers cannot answer.
4. An unanswered request stays open and visible. Nothing auto-approves it on a timeout.

**Adoption.** An existing thread the hub did not create is adopted only through an explicit, authorized handoff, which issues a new lease.

**Terminal states per intent** (Codex, from the schema):

| Intent | Request | Accepted when | Completed when | Failed when | Notes |
|---|---|---|---|---|---|
| `Prompt` | `turn/start` (`threadId`, `input`; `model` optional) | the `TurnStartResponse.turn` reply arrives. Its id is stored as the delivery's `turnId`. | `turn/completed` arrives for that `turnId` | an error reply, or `turn/completed` with a failed status | sent only on an idle thread |
| `SendNow` | `turn/steer` (`threadId`, `input`, `expectedTurnId`) | `TurnSteerResponse.turnId` equals `expectedTurnId` | the `turn/completed` of that turn | an error reply, for example because the turn changed. It is surfaced and never converted into a `Prompt`. | no model override |
| `Reset` | `thread/start` (no required fields) | — | `ThreadStartResponse.thread` arrives. In one step this swaps the persisted route to the new thread and increments `thread_epoch`. | an error reply | Hydration is a *separate* `Prompt` delivery whose `parent_id` is the reset's `delivery_id`. It is sent only after the reset completes. |
| `Interrupt` | `turn/interrupt` (`threadId`, `turnId`) | the empty `TurnInterruptResponse` arrives | `turn/completed` arrives for that `turnId`, with any status | after 30 s without confirmation (`interrupt_unconfirmed`), an **owned** child escalates to host termination after `ESCALATION_GRACE` (10 s), ending `failed(timeout_escalated)` (§E rule 6). On a borrowed endpoint the delivery fails, an alert is raised, and the thread is fenced. | With no active turn at request time, it completes at once as `no_active_turn` and sends no RPC. If the turn completes first, it completes as `already_finished`. |
| `Compact` | `thread/compact/start` (`threadId`) | the empty `ThreadCompactStartResponse` arrives | `item/completed` arrives for a `contextCompaction` item on that thread. The deprecated `thread/compacted` notification is also accepted. | an error reply, or a failed turn | Acceptance never counts as completion. |
| `SetModel` | none, at first: it is a local pending setting | the controller has stored it | the next `turn/start` that carries `model` is accepted | that `turn/start` fails with a model error | A change of model *provider* is `Unsupported` in v1. |

Tests: each intent reaches each of its terminal states. The cases include a reset before hydration, an interrupt racing a normal completion, an interrupt with no active turn, and simultaneous wire and operator input.

### B. Owned children and borrowed endpoints

This extends v3 §2.

- The relationship lives in the hub's lifecycle metadata. The vendor-neutral `fl host` never interprets it.
- Endpoint ownership (`owned_child` or `borrowed_endpoint`, plus the transport) and thread ownership (`threadId`, the epochs and the lease) are persisted separately, together with `CODEX_HOME` or the vendor's equivalent.

**Borrowed endpoints** (a shared Codex daemon, or the agy language server):
- Decommissioning closes only this session's thread. It never stops the daemon or sibling threads.
- Readiness means initialization succeeded and the thread was started or resumed.

**Owned children with a dedicated `CODEX_HOME`:**
- They get an explicit configuration and authentication policy. A fresh home never inherits a login.
- Missing credentials is a typed spawn failure.

Tests: two threads on one shared daemon, tearing one of them down, restart and resume, and missing credentials.

### C. Delivery persistence and recovery

This amends v2 §2 `events` and `send`.

**Records.** Each is persisted durably at the step named:

| Record | Fields | Written |
|---|---|---|
| `delivery` | `delivery_id` (UUIDv7), `parent_id`, `session`, `session_epoch`, `thread_epoch`, `controller_lease`, `thread_id`, the intent and a hash of its body, the source message id, the caller's authority, `request_id`, `turnId`, `state` and the timestamps | at `queued`, then at every state change |
| `server_request` | the vendor's request id, its method and params, `session_epoch`, `thread_epoch`, `state` (`open`, `answered` or `lost`) | on arrival, before anything is shown |

**States.** `queued` → `dispatching` → `sent` → `accepted` → `completed` or `failed`. Two more states, `ambiguous` and `no_active_turn`, are used as §A and recovery define them.
- `dispatching` is written durably **before the first byte can leave**. `sent` is written after the `send` succeeds.
- A delivery is safe to send only while it is `queued`, because only then do durable records prove that no dispatch has started.
- `accepted` and `completed` follow §A's table and are correlated by `request_id`, then by `turnId`. A successful `send`, or a prompt echoed on a PTY, is never acceptance.

**Events.**
- Event frames carry `{ session_epoch, seq }`. Each event is mapped to its thread by the hub.
- The hub acks a `seq` only after the *complete* event is durable. A partial (fragmented) event never advances the ack cursor (§E).
- `fl host` keeps unacked protocol lines up to 8 MiB per session and replays them from `ack + 1`.
- When the buffer is full and no consumer is reading, the daemon stops reading the child's stdout. This back-pressure is lossless: `consumer_stalled` is set in `list`, nothing is dropped and nothing is killed.

**Recovery** after a hub restart or reconnect. It also works when an RPC reply was lost.
1. The controller takes a **new lease**. Neither epoch changes.
2. It replays the unacked events against the records of their own epochs, applying only `seq` above the durable cursor. Live events that arrive during the replay are applied once, in order. A reply that was accepted but not acked, and a completion that was not acked, are each applied exactly once.
3. A delivery left in `dispatching` or `sent` with no correlated reply becomes **`ambiguous`**.
   - `thread/read` is shown as evidence, for example a turn that started after the dispatch, but it never moves the state forward.
   - An `ambiguous` delivery is surfaced to the Owner and operator in FerroDeck and through `fl agent`, and it is **never resent automatically**. Resending is an explicit action that creates a new `delivery_id`.
4. An `open` `server_request` that the vendor does not raise again is marked `lost` and surfaced. One that is raised again is a new Owner action. Nothing is ever answered from persisted state.
5. Only then does the controller accept new intents.

**Codex queue CLI.** A `codex queue` exit 0 proves enqueue only. It does not prove resume or processing.

**Tests:**
- a crash after the child accepted a delivery but before `sent` was recorded: the restart does not repeat the prompt;
- a reconnect with an accepted reply and a completion both unacked: each is applied once, with no wrong `ambiguous` result;
- a lost reply, ending in `ambiguous` with no resend;
- a hub restart while an approval is open;
- old-thread output interleaved with the `thread/start` response and new-thread events, where every event reaches only its original records;
- a writer under a stale lease, which is refused.

### D. Process containment: one reaping owner, isolated sessions, stated guarantees

This **amends v2 §3** and with it every unconditional whole-tree or death claim in v2, including the §8 decision 1. It also refines v3 §4.

**Per-session supervisor (Linux and macOS).**
- `fl host` never runs vendor code as its direct child. For each session it spawns a small supervisor, `fl-host --session-supervisor <session>`, which is the only direct child the daemon reaps.
- The supervisor:
  1. sets `PR_SET_CHILD_SUBREAPER` for itself (on Linux);
  2. starts the vendor process. For a PTY, the slave end it inherits from the daemon becomes the vendor's controlling terminal, and the daemon keeps the master;
  3. is the **one reaping owner** for its whole subtree. Its `waitpid(-1)` sees only its own descendants, so it never races another session's waiter. The tokio waiter in `fl host` is not used for vendor processes;
  4. reports the vendor leader's exit status to the daemon on a status pipe.
- Orphans of that session reparent to *that session's* supervisor, so attribution is preserved. Kill means the daemon asks the supervisor, and the supervisor signals its own subtree: its process group plus every adopted descendant. Kill never targets another session's processes.

**Linux with the delegated cgroup v2 unit** (the stronger path, landing in PR 3). The supervisor writes its own pid into the session's child cgroup *before* it starts the vendor process, so every descendant is born inside the cgroup. Kill writes `cgroup.kill`. Under `KillMode=control-group`, systemd kills the whole unit cgroup when the daemon dies, including by SIGKILL.

**Guarantees:**

| OS / path | Guarantee | Named residual |
|---|---|---|
| Linux, under the delegated cgroup v2 unit | Termination of the whole session tree on kill, and of every session when the daemon dies | none claimed beyond what systemd guarantees |
| Linux, without the cgroup path | Best effort: on kill, the supervisor signals its group and its adopted descendants | a descendant that sets its own subreaper; anything left running when both the supervisor and the daemon are SIGKILLed |
| macOS (no subreaper, no cgroup) | Best effort: the supervisor signals its process group and its known descendants | descendants that call `setsid` and reparent to launchd; descendants left after a daemon SIGKILL |
| Windows | Every Job member terminates on kill and when the daemon dies | none claimed |

On Windows each session child is created by `fl host` through `CreateProcessW` with `CREATE_SUSPENDED` (plus `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` for ConPTY). It is assigned to a per-session kill-on-close Job before it resumes; if the assignment fails, the child is killed while still suspended and the spawn fails. Job handles are not inheritable. Windows needs no session supervisor. The daemon also puts itself in a nested backstop Job; if that is refused, it logs the failure and continues.

**PTY children on Unix.** The OS guarantees that SIGHUP is *delivered* to the PTY's session when the master closes. A child can catch or ignore SIGHUP, so its *exit* is not guaranteed. Exit is guaranteed only where the cgroup path or a Windows Job applies.

**v2 §8 decision 1 is reworded:** "If the daemon dies, every Windows Job member and every session under the Linux delegated cgroup v2 unit is terminated. Elsewhere on Unix, PTY children receive SIGHUP, but their exit, and the exit of detached descendants, is best effort with the residuals named in §D."

**Tests on all three runners.** Each asserts its OS's stated guarantee and never assumes an exit it does not guarantee.
- a child that forks a grandchild at once;
- a detached (`setsid`) grandchild;
- a leader that has already exited;
- a daemon SIGKILL before restart;
- a PTY child that ignores SIGHUP;
- two sessions, each with adopted orphans, where only one is killed;
- a known child and an adopted descendant exiting at the same moment, each with its correct status.

### E. Bounds, framing, stderr, supervision and budget

**Framing.** A logical message that is larger than one frame (an event or a snapshot) is fragmented into frames with `more = true` and reassembled up to its own cap.
- An **event** that passes its cap is a protocol violation, and the session fails with a typed error.
- A **snapshot** has no total cap. It is built and sent **incrementally, one row per frame**, as `snapshot_row { snapshot_id, row, is_last }`. The daemon serializes one row at a time from the `vt100` grid and holds no more than one row's serialization. No state is discarded, and the vendor child is never affected. Until a viewer's snapshot is complete, FerroDeck labels the view **live-only (incomplete)**, because a live byte tail without a snapshot is not a synchronized terminal state.
- A snapshot is applied only once it is complete. A partial event never advances the ack cursor.

**Hard limits.** Each is checked before allocation, refused with a typed error, and defined as a named constant in `fl-host-proto`. The values come from `terminal.rs` where it already has one.

| Limit | Value |
|---|---|
| IPC frame payload | 256 KiB. A malformed or oversized length closes the connection, and nothing is allocated before the check. |
| Reassembled event | at most 1 MiB of line plus 4 KiB of envelope |
| Snapshot row | `MAX_COLS × (CONTENT_BYTES + MAX_CELL_SGR_BYTES)`. That is 1000 × (22 + 64) = 86,000 bytes, which fits one 256 KiB frame. For scale, Fiona's source-derived example (19 bytes of *content* per cell) gives 1000 × 400 × 19 = 7,600,000 bytes, about 7.25 MiB, before any formatting. Neither that nor any other example is a worst-case upper bound, which is why snapshots are bounded per row and not in total. The 22 is `CONTENT_BYTES` in `vt100 0.18.1` `cell.rs:4`. The 64 for the escape sequences that change a cell's attributes is a named constant and is asserted by a test. A whole snapshot is at most 400 rows, about 34 MB on the wire, with a bounded memory cost at both ends. |
| Partial stdout line of a pipe child | 1 MiB. On overflow, a protocol error event is raised and the child is killed. |
| stdin per session | 64 frames, 256 KiB, and a 5 s write-progress timeout (`terminal.rs` `STDIN_QUEUE_FRAMES`, `STDIN_QUEUE_BYTES` and `STDIN_WRITE_TIMEOUT`). When the queue is full, the caller gets a typed refusal. |
| Sessions per daemon | 64, configurable |
| Subscribers per session | 16 |
| PTY size | clamped to 1–1000 columns and 1–400 rows |

**stderr** of a pipe child is drained concurrently into its own 64 KiB tail ring per session. It never blocks stdout or the child.

**Supervision** is owned by the hub, and the daemon only reports `exited`.
- A reconciler compares `fl host` `list` with the hosted-session records, on hub start and on every `exited` event.
- `on-failure` restarts back off exponentially (5 s, doubling to 5 min), with hard ceilings: 5 restarts per hour, or 3 consecutive crashes within 30 s of start. Past a ceiling, the session stops with status `failed` and reason `crash_loop`, and FerroWatch raises an alert.
- The restart count and backoff state are persisted.

**Contract (Fiona's text, verbatim):** "Automated execution requires enforceable finite token/cost allocations and atomic reservations. If the adapter cannot establish the required bounds, admission fails closed. Turn/time/quota limits are separate controls. An owned workload that does not stop by its cancellation deadline escalates to host termination within a named grace period. Borrowed endpoints are not admitted for automated work under a stop guarantee they cannot enforce." The named grace period is `ESCALATION_GRACE` = 10 s, applied after the 30 s `interrupt_unconfirmed` deadline.

**Lead disposition (Ferris, verbatim):**
> 1. No exception to FerroLoop/AGENTS.md or crates/exec/src/runner.rs:15-17. Automated execution (auto-restart, automated retry, or autonomous workflow continuation) MUST enforce finite token (max_tokens) and cost (max_cost_usd_micros) ceilings in addition to turn count, wall-clock time, and subscription quota headroom.
> 2. Atomic preflight reservation: Before admitting any automated turn/restart, the controller atomically reserves a finite per-turn token and cost slice (turn_reservation_tokens and turn_reservation_cost_micros, or the vendor's max_output_tokens + context window bound) against the remaining allocation, reconciling to actual measured spend on turn completion.
> 3. Fail-closed rule: If a finite token/cost ceiling is not configured or token/cost telemetry cannot be observed for that path, automated admission MUST fail closed (refuse to start the automated turn/restart and surface an operator alert).
> 4. Interactive Owner actions vs. autonomous continuation: A single direct interactive Owner action (manual prompt, manual spin-up, manual restart) executes as that single turn, whereas any autonomous continuation loop initiated by the Owner requires an explicit finite token/cost allocation that all subsequent automated turns draw from.

**Budget: the Lead's disposition, recorded here.** There is no exception to `AGENTS.md` or to `crates/exec/src/runner.rs:15-17`.
1. **Ceilings.** Automated execution has finite ceilings: a token ceiling (`max_tokens`) and a cost ceiling (`max_cost_usd_micros`), in addition to the turn count, wall-clock time and subscription-quota headroom. Automated execution means an auto-restart, an automated retry, a balance remodel, a handoff, or an autonomous workflow continuation.
2. **Freshness.** The quota and usage readings must be newer than a configured age, or no automated work is admitted.
3. **Atomic preflight reservation.**
   - Before it admits any automated turn or restart, the controller reserves a finite per-turn slice against the remaining allocation: `turn_reservation_tokens` and `turn_reservation_cost_micros`, or the vendor's `max_output_tokens` plus the context-window bound. The reservation is one compare-and-swap on the allocation record in the store, so two concurrent admissions cannot spend the same remainder.
   - Each reservation must be backed by an adapter-proven upper bound on the whole slice (rule 5). Reconciling to the measured spend when the turn completes only settles the account. It is not the enforcement mechanism.
   - `fl_core::workflow::preflight_ceiling` (`workflow.rs:862`) checks only cumulative spend, so it stays as a second check alongside the reservation.
4. **Fail closed.** Automated admission refuses to start the turn or restart, and raises an operator alert, when either of these holds:
   - no finite token and cost ceiling is configured;
   - token and cost telemetry cannot be observed on that path.
5. **Admission proof (Fiona's contract, verbatim):** "Automated admission requires an adapter-proven upper bound on the total tokens and cost of the admitted execution slice, reserved atomically before dispatch. If that bound cannot be established, automated admission fails closed even when usage telemetry is available. Reactive usage monitoring, interruption and host kill are additional safeguards, not proof of the hard ceiling. A max_output_tokens plus context-window bound must cover the whole admitted slice, including repeated model/tool steps, retries and subagents. A bound for one model request alone does not bound an app-server turn containing multiple requests."
   - No adapter has proven such a bound yet (see "Open implementation work" below). Until one does, automated execution is refused on that adapter's path.
6. **Reactive safeguards.** These are additional to the admission proof, never a substitute for it:
   - **Live usage.** The controller watches the vendor's live usage events, such as Codex `thread/tokenUsage/updated` and the per-message usage in Claude `stream-json`.
   - **Crossing a limit.** When the turn's usage crosses its reserved tokens or cost, or its wall-clock limit, the controller sends `Interrupt`. If `interrupt_unconfirmed` follows (after 30 s), it escalates for an **owned** child: `fl host` `kill` with `grace_ms` (§D), the delivery ends `failed(timeout_escalated)`, FerroWatch raises an alert, and the session goes to supervision.
   - These safeguards stop further spending. They are **not** a token or cost ceiling and are not claimed as compliance with the hard token and cost mandate.
   - **Fail closed.** A vendor path with no live usage events fails closed for automated admission, under rule 4.
   - **Borrowed endpoints.** Automated work is **not admitted** on a borrowed endpoint in v1 (§B). The controller cannot kill a shared daemon, so it cannot enforce a bound there. Only interactive Owner turns run on one. If an interactive interrupt goes unconfirmed there, the delivery fails, an alert is raised, and the thread is fenced. Admitting automated work behind a proven cancel verb is a later extension, to be specced when one is proven. This meets the contract's last sentence, and is stricter than it requires.
   - **No exception.** No policy exception is requested, and turn or time units are not presented as meeting the token and cost mandate.
7. **Owner actions.**
   - A single direct interactive Owner action (a manual prompt, manual spin-up or manual restart) runs as that single turn.
   - An autonomous continuation loop that the Owner starts requires an explicit, finite token and cost allocation, and every later automated turn draws from it. That includes retries, subagents and handoffs.

**Tests:**
- an endless line;
- a malformed length;
- a maximum legal line with its envelope;
- a maximum snapshot: 1000×400 cells, each with an ASCII base plus nine U+0301 combining marks and changing attributes on every cell. It is transferred row by row, and every row stays within its bound;
- a reconnect during a fragmented event;
- a stalled stdin;
- a stderr flood;
- no hub present;
- an automated turn just below its token and cost cap, then one that crosses it, which is interrupted;
- a child that never confirms an interrupt, which escalates to `kill` within its bound;
- automated work on a borrowed endpoint, which is refused;
- two concurrent admissions against one token and cost allocation;
- an Owner-started turn followed by an automated continuation, which draws from the explicit allocation;
- an unconfigured ceiling, or unobservable telemetry, which fails closed and raises an alert;
- a crash loop that reaches its ceiling.

### F. The standalone `fl-host` binary is a delivery dependency

This amends v2 §1 and §7.

- `fl-cli` depends unconditionally on crates that use Unix-only code. So `crates/host/Cargo.toml` gains a `[[bin]]` named `fl-host` (`fl-host.exe` on Windows), which depends only on `fl-host`, `fl-host-proto` and `fl-core`.
- Checked: `fl-core` uses `std::os::unix` only inside `#[cfg(test)]` code (`code_index.rs`, `ferrostate.rs`), so the binary's dependency set builds on Windows.
- `fl host install-unit` makes the unit run `fl-host` on every OS. `fl host serve` is a thin entry point to the same library code, with identical protocol and version handling.

**Acceptance claims are kept separate:**
1. The library builds and its tests pass on all three runners.
2. The real `fl-host` executable is launched on the Windows runner, and over IPC it performs `spawn`, `kill`, `list` and `events` against the test child, passing the §D and §E tests.

"Delivered on all three OSes" is claimed only once both hold.

### Open implementation work (Important): proving an adapter's slice bound

The specification finding is **resolved**: Fiona's final verdict on the published text is APPROVE. Proving the bound for each adapter is still open, Important implementation work.

Under §E rule 5, automated work is admitted only on an adapter that proves an upper bound on the total tokens and cost of the whole admitted slice. No adapter proves one yet.

So in v1, every automated path is refused, with an operator alert, until such a bound is proven for that adapter:
- auto-restart hydration;
- automated retries;
- balance remodels;
- handoffs;
- autonomous continuation.

Interactive Owner turns are unaffected. Proving a slice bound for each adapter is an implementation acceptance gate. Runtime and schema conformance, and each OS guarantee, are also acceptance gates.

### PR sequence changes

- **PR 1:** adds the `fl-host` binary, the session supervisor, and the Windows suspended spawn with Job assignment (§D, §F).
- **PR 2:** adds framing, the event cursor with ack and replay, the hard limits, and the stderr drain (§C, §E).
- **PR 3:** adds the delegated cgroup v2 path alongside `install-unit`.
- **PR 4:** becomes the `SessionController` (epochs and leases, lanes, per-intent states, delivery persistence) plus supervision and the budget gate, all before any adapter. The adapters follow, each with its conformance tests.
- The other PRs are unchanged.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01SP4orqRK3R2jpFNhD1ViMA
