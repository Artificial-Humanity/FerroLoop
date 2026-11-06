# Native Agent Host Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `subagent-dev` to implement this plan task by task, including its per-task `code-review` loop and final whole-branch review. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver an authenticated, vendor-neutral `fl-host` daemon that owns pipe children on Linux, macOS, and Windows, without changing live agent routes.

**Architecture:** `fl-host-proto` owns typed messages, bounded framing, platform transport and blocking/async clients. `fl-host` owns the standalone executable, child supervision and session inventory. The hub remains the future vendor-RPC owner and the only component that admits automated vendor turns.

**Tech Stack:** Workspace Rust edition 2024 / Rust 1.98, serde, serde_json, tokio, libc on Unix, windows-sys on Windows. No PTY dependency in this slice.

**Spec:** [Approved Issue #217 source snapshot](../specs/2026-11-03-native-agent-host-design.md), v2 §§1–3 and 7 as amended by v3 §4 and v4.5 §§A, C–F. Read the amendments before implementing the older text.

## Global Constraints

- This is the pipe-child foundation from PR 1, not the full host or an agent migration. No vendor CLI, inference call, automatic restart, route replacement or live tmux teardown belongs in this branch.
- Later slices add PTYs, full ack/replay, service units and cgroups, the hub SessionController and persistence, Codex, Claude, the remaining adapters, console integration, and finally the migration. Codex precedes Claude per Ferris's latest handoff.
- Keep the host standalone. It must not depend on `fl-cli`, `fl-hub`, `fl-wire`, `fl-exec` or `fl-store`. Neither hub nor wire links a PTY library.
- `fl-host-proto` and `fl-host` compile and run tests on all three operating systems from the first code PR. Windows acceptance includes the real `fl-host.exe`, not merely protocol unit tests.
- Keep `test, clippy, fmt` unchanged. Add `fl-host (macos)` and `fl-host (windows)` jobs. The code PR touches `.github/` and needs owner review under `WORKFLOW.md`. Prepare the complete reviewed PR before requesting that approval. Do not change branch-protection settings.
- Define bounds in `fl-host-proto`: frame payload 256 KiB; event payload 1 MiB of line plus 4 KiB envelope; stdout line 1 MiB; stdin 64 frames and 256 KiB with 5 s write-progress timeout; stderr tail 64 KiB; sessions 64 by default; subscribers 16. Reject malformed lengths before allocation. Payload means the bytes after the type byte; check both framing overhead and payload without integer overflow.
- A serialized line can exceed one frame. Fragment complete logical events with `more`, bound reassembly, and never deserialize a truncated JSON fragment. The daemon is not allowed to truncate pipe output silently.
- Host events carry `session_epoch` and `seq`, never `thread_epoch`. A host-issued `controller_lease` fences every `send`/`write` at the actual stdin write boundary. Reclaiming a lease cannot permit a queued stale write to escape.
- Unix containment is best effort until the delegated Linux cgroup slice. Windows children are born in a non-inheritable kill-on-close Job and execute only after containment is verified.
- No automated-execution admission ships in this slice. The later controller must enforce the whole-slice token/cost proof and atomic reservations from v4.5 §E. Existing `fl-core::slice_proof` is input to that work, not evidence that a vendor qualifies.
- Work only in isolated worktrees. Every commit uses the machine author and the responsible agent's co-author trailer. The PR author owns the merge after current-head approval and green checks.

## Review Focus

- An unrelated same-user file or live socket occupies the endpoint: refuse without unlinking it; a second daemon cannot replace the live daemon's token or endpoint.
- A client sends a valid frame length then stalls: handshake and incomplete-frame reads have finite, named deadlines; an idle session with no RPC is not a partially read frame.
- A child floods stderr while stdout is idle: bounded stderr drainage continues without blocking the child or growing memory.
- A session leader exits before its descendants: preserve containment ownership until explicit cleanup; a kill must not touch a sibling session or a reused PID.
- A new controller takes over while old stdin writes are queued: stale queued writes fail before any bytes reach the child, including during partial writes.

## Delivery ownership and sequence

Felicia owns the implementation branch and integration. Tasks are sequential on that branch. Use fresh implementer and reviewer subagents under `subagent-dev`; do not have multiple agents mutate its manifests or lockfile concurrently. Ferris reviews the result before the owner-tier CI review request.

Feira reviews the protocol/transport contract independently. Freya reviews the process-containment and cross-platform acceptance matrix. Felix prepares the later console and controller UI integration inventory without changing frontend code for this slice.

The coordinator can adjust private helper names, but public interface changes require a single update to this plan before a dependent task starts. A task may introduce smaller helper modules under its assigned directory.

### Task 1: Protocol types, frame bounds and authenticated transport

**Files:**
- Create: `crates/host-proto/Cargo.toml`, `src/lib.rs`, `src/types.rs`, `src/frame.rs`, `src/transport/mod.rs`, `src/transport/unix.rs`, `src/transport/windows.rs`, `src/client.rs`.
- Create: `crates/host-proto/tests/transport.rs`.
- Modify: workspace `Cargo.toml`, `Cargo.lock`, `.fl/architecture.toml`.

**Interfaces:**
- Produces: `HostError`, `HostEndpoint`, `HostConfig`, `SessionId`, `SessionEpoch`, `ControllerLease`, `HostRequest`, `HostResponse`, `HostEvent`, `SpawnSpec`, `HostedChild`, `ChildState` and `ChildExit`, publicly re-exported from `fl_host_proto`.
- `HostEndpoint::discover() -> Result<HostEndpoint, HostError>` resolves `FL_HOST_SOCK` before platform defaults. Authentication material is separate from the public endpoint string and is never logged.
- `AsyncHostClient::connect(&HostEndpoint) -> Result<Self, HostError>` completes authenticated hello/welcome. `request(&mut self, HostRequest) -> Result<HostResponse, HostError>` correlates an explicit request id. `BlockingHostClient` exposes the same semantics without requiring a caller-owned tokio runtime.
- The listener exports an authenticated async connection over one platform-neutral stream abstraction usable by the daemon. Define its concrete API in `src/transport/mod.rs` in this task; later tasks consume it directly, never duplicate handshake logic.
- Requests in this slice: `Hello`, `Spawn`, `List`, `Kill`, `Events`, `ClaimLease`, `Send`, `Signal`. `SpawnSpec` contains session id, pipe kind, argv, cwd, environment override map and an explicit environment allow-list. PTY requests return typed `Unsupported` until the next slice.
- `ChildState` distinguishes `LeaderExited` from `CleanupComplete`: a leader status never implies descendant cleanup.
- `ClaimLease { session, session_epoch }` returns a new monotonic lease. `Send` carries session, expected epoch, lease and one line. Events carry epoch and monotonic per-session sequence. The host never interprets vendor JSON inside a line.

- [ ] **Step 1:** Write unit tests `frame_rejects_oversize_before_payload_read`, `frame_rejects_zero_and_overflow_lengths`, `event_fragments_round_trip_at_max_line`, `truncated_event_never_becomes_a_line`, and type round-trip tests. Assert the constants from Global Constraints and a one-byte-over-limit refusal.
- [ ] **Step 2:** Write black-box transport tests `same_user_handshake_succeeds`, `wrong_token_fails`, `wrong_proto_major_fails_without_restart`, `stalled_handshake_expires`, `partial_frame_expires`, `occupied_non_socket_is_preserved`, and `second_daemon_cannot_replace_endpoint`. Windows tests exercise explicit current-SID ACLs, first-instance protection and remote-client rejection. Unix tests assert directory mode 0700, socket/token mode 0600 and peer-UID validation through the platform helper.
- [ ] **Step 3:** Run `cargo test -p fl-host-proto` and record the initial failure. Add the crate with bounded codecs, raw binary payload support and authenticated platform transport. Bind-before-listen permission setup must match `crates/store/src/ferrostore/server.rs::bind_private_unix`; do not add a dependency on the store. macOS uses its per-user temporary directory, not a global predictable `/tmp` path. Windows uses a SID-specific named pipe, no TCP fallback.
- [ ] **Step 4:** Implement the exclusive daemon identity guard and token publication behind the transport module. Acquire the lock before touching the token or endpoint. Preserve live endpoints and refuse incompatible protocol majors without restarting anything. Choose finite handshake/frame deadlines as named constants and document the chosen values in module comments.
- [ ] **Step 5:** Run `cargo test -p fl-host-proto` and `cargo clippy -p fl-host-proto --all-targets -- -D warnings`. Commit the passing deliverable with the agent trailer. Obtain task review before Task 2.

### Task 2: Pipe child supervision and containment

**Files:**
- Create: `crates/host/Cargo.toml`, `src/lib.rs`, `src/main.rs`, `src/child/mod.rs`, `src/child/unix.rs`, `src/child/windows.rs`, `src/supervisor.rs`, `src/survivor_registry.rs`, `src/env.rs`.
- Create: `crates/host/src/bin/fl-host-testchild.rs`, `crates/host/tests/containment.rs`.
- Modify: workspace `Cargo.toml`, `Cargo.lock`, `.fl/architecture.toml`.

**Interfaces:**
- Consumes the types and authenticated transport from Task 1.
- Produces `fl_host::serve(config: HostConfig) -> Result<(), HostError>` for the standalone binary and future CLI entry point. The binary handles `--session-supervisor` before normal daemon startup on Unix.
- Child supervision returns a per-session owner handle, bounded stdin writer, stdout reader and stderr reader. The owner retains Unix control/status FDs or Windows Job/process handles, exposes separate `LeaderExited` (exact leader status, once) and `CleanupComplete` notifications, and remains alive after leader exit until cleanup ownership is resolved. Store exact internal signatures in `src/child/mod.rs` before creating either OS implementation.
- Unix daemon reaps only the per-session supervisor. That supervisor is the sole vendor-subtree waiter, sets Linux `PR_SET_CHILD_SUBREAPER` before vendor creation, creates an isolated vendor session and retains subtree ownership after leader exit. Adoption is guaranteed only on Linux within v4.5 §D's limits; macOS has no subreaper. No process-wide `waitpid(-1)` loop belongs in the main daemon.
- Each Unix owner retains a daemon-exclusive control-pipe writer or socket endpoint and a status reader. Control EOF requests bounded TERM-to-KILL cleanup by the supervisor; EOF is never proof that the subtree exited. Close all control/status copies in vendors and helpers, and all unrelated copies in sibling supervisors. Each supervisor closes every control-writer copy and keeps only its intended control-reader/status-writer endpoints; set `FD_CLOEXEC` wherever compatible with that handoff. The supervisor survives leader exit and reports cleanup separately; timeouts or unprovable survivors preserve the record and alert.
- `src/survivor_registry.rs` owns the Unix durable survivor registry at `<runtime dir>/fl/host-children.json` from v2 §3, with v4.5 §D's best-effort limits. Persist session/epoch ownership, supervisor and vendor PIDs, process groups and their start-time identity guards on spawn and exit. A leader exit updates status; it never by itself removes the cleanup owner or its record. Remove an entry only after its recorded cleanup ownership is resolved.
- On all three platforms, the same module persists a per-session `session_epoch` high-water mark before releasing a spawned child or acknowledging spawn. Retain that mark after cleanup records are retired, and allocate only strictly newer epochs after daemon restart; refuse corrupt state or counter exhaustion rather than resetting an epoch. Persist the epoch and, on Unix, its survivor identity together before execution is released. This prevents a respawn from accepting an old controller's writes under a reused epoch (v4.5 §A).
- Under Task 1's exclusive daemon lock, recover the registry and finish bounded cleanup of provably owned survivors before admitting new sessions. On Linux, acquire stable pidfds first, then validate durable identity against `/proc/<pid>/stat` while the handle's identity remains pinned; refuse any target whose ownership cannot be proven. A start-time check followed by a numeric PID kill is forbidden. Invalid or unresolved identities retain an actionable cleanup alert and cause bounded startup refusal rather than a guessed kill.
- Linux retains pidfds acquired while each live identity is pinned or its child remains unreaped, and signals individual processes only with `pidfd_send_signal`. Define `check_pidfd_support` to probe pidfd availability/permission and return a typed containment refusal when unavailable or disallowed; do not fall back to numeric PID signalling. Serialize signalling and reaping. Numeric process-group signals are allowed only while a retained owner or an unreaped group leader anchors that exact group identity; never signal a saved/reaped PGID. On macOS, `proc_pidinfo` supports identity validation but does not close the check-to-kill race: signal only pinned owned children or anchored owned groups; detached/reparented targets that cannot be pinned retain the documented best-effort residual and refusal.
- Unix spawn has an explicit publication handshake: keep vendor execution gated until a complete supervisor/vendor/group identity record is durably published, then release execution. Acknowledge spawn only after successful Linux subreaper/Unix session setup, durable publication and an exec-success acknowledgement from the vendor launch path; setup/exec failure returns typed spawn failure and cleans up. Write a private temporary file, sync it, atomically replace the registry and sync its directory; exit/status updates use the same ordering. A lost handshake makes gated children exit and requests bounded supervisor cleanup. Restart never treats temporary, truncated or partially written records as cleanup authority; test crashes on both sides of publication. This registry is host cleanup state, not the later hub SessionController persistence.
- Windows uses `STARTUPINFOEX` with `PROC_THREAD_ATTRIBUTE_JOB_LIST` in `CreateProcessW(CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT)` so the child is born in the per-session `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` Job, including if the daemon dies during creation. Verify Job membership before `ResumeThread`; acknowledge spawn only after durable epoch publication, containment and resume succeed. The Job is non-inheritable, `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` permits only intended stdio handles, and no breakaway flags are allowed. Creation/assignment/verification/resume failure terminates and waits for any created child before returning typed spawn failure. Account for Windows argument quoting without a shell.
- Retain the Windows Job after leader exit; collect exact leader status through its process handle and confirm descendant cleanup through stable Job accounting/notifications and retained process handles before `CleanupComplete`. Never infer completion from the leader or a saved PID list. Vendors, helpers and sibling sessions must inherit no Job handles. The daemon backstop Job remains optional, with refused setup logged as in the approved spec.
- The creation-time Job list is a Lead implementation refinement of approved pre-execution Job assignment, closing the daemon-death window before post-creation assignment; the archived source snapshot remains unchanged. Primary references: Linux [`pidfd_send_signal`](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html), [`PR_SET_CHILD_SUBREAPER`](https://man7.org/linux/man-pages/man2/PR_SET_CHILD_SUBREAPER.2const.html), and Windows [`UpdateProcThreadAttribute`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute) (`JOB_LIST` and `HANDLE_LIST`).

- [ ] **Step 1:** Add a test-child binary with modes for echo, exact argv/environment report, chosen exit code, stalled stdin, stderr flood, immediate grandchild, detached grandchild, leader-exits-first, TERM-ignore, simultaneous distinct-status exits and a long-lived child that survives stdin EOF. Readiness uses a pipe message, not sleeps. Pipe children report `isatty == false`.
- [ ] **Step 2:** Add `argv_is_not_shell_code`, `env_is_explicit_and_scrubbed`, `bad_cwd_spawns_nothing`, `exit_code_is_preserved`, `kill_only_one_of_two_sessions`, `immediate_grandchild_is_contained`, `leader_exit_keeps_cleanup_owner` and `daemon_death_obeys_platform_contract` integration tests. Add a Unix detached-child case with PID/start-time identity checks and OS-specific residual assertions. Daemon-death cases use the stdin-EOF-surviving fixture so pipe closure cannot masquerade as cleanup. On Windows, assert grandchild exit on both Job kill and daemon death.
- [ ] **Step 2a:** Add `linux_adopted_orphans_stay_with_their_session` (two sessions each adopt orphans, kill one and preserve the other), `linux_known_and_adopted_exits_report_exact_status_once` (simultaneous exits with distinct statuses), `term_ignoring_child_is_killed_within_deadline`, `concurrent_exit_and_kill_preserve_identity`, and `inherited_control_writer_cannot_delay_daemon_death_cleanup`. Exercise vendor/helper/sibling FD closure; the daemon alone must hold the control writer after handoff. Test pidfd support refusal and a target exit/reuse between identity lookup and signalling, including recovery, proving no numeric-kill fallback and no saved/reaped-PGID signal. Linux adoption assertions are not imposed on macOS.
- [ ] **Step 2b:** Add Windows fault-injection barriers at creation, contained-before-resume and running; kill the daemon at each and prove no surviving Job member, including a suspended child. Cover creation/assignment/verification/resume failures and kill-and-wait cleanup, no inherited Job handles in vendors/helpers/siblings, and leader-exits-first with a live descendant: retain the Job and withhold `CleanupComplete` until stable accounting/handles confirm exit.
- [ ] **Step 2c:** Add Unix restart tests `daemon_sigkill_restart_cleans_recorded_survivors`, `leader_exit_preserves_durable_cleanup_owner`, `stale_pid_or_identity_mismatch_never_signals`, `restart_cleanup_isolates_sibling_sessions`, `spawn_record_publication_survives_crash` and `invalid_registry_refuses_startup_within_deadline`. Launch a fresh daemon after SIGKILL with the same isolated runtime directory; cover both supervisor/leader-alive and leader-exited cases. Assert cleanup before a new spawn can succeed, preservation of unrelated live processes, and finite refusal with an alert for unproven ownership. While cleaning one record, assert a sibling survives until its own cleanup. Check durable state across spawn/exit and crashes before/after publication. Use readiness/handshake barriers, not sleeps; assert only v4.5 §D's platform guarantees, explicitly retaining the macOS detached/reparented-descendant residual.
- [ ] **Step 3:** Run `cargo test -p fl-host --test containment` to record the initial failure. Implement both OS paths and scrub the child environment. The host baseline is `PATH`, `HOME`/`USERPROFILE`, `LANG`, `TERM` plus explicit allowed names. Reject disallowed override names. Apply `env_clear()` or the Windows equivalent; do not call the broader vendor credential allow-list as a substitute. No vault secret is sent in this protocol.
- [ ] **Step 4:** Implement bounded TERM-to-KILL escalation, status collection and the survivor-registry publication/recovery contract above. Name finite startup-cleanup deadlines and preserve unresolved ownership on timeout or identity mismatch. Enforce the stable-handle and anchored-group signalling rules above; identity checks alone never authorize a later numeric PID kill. Document Linux-without-cgroups and macOS residuals from v4.5 §D; restart cleanup does not guarantee termination of detached descendants whose ownership can no longer be proven. Attempt the Windows daemon backstop Job, logging failure without claiming an unavailable guarantee.
- [ ] **Step 5:** Run focused containment tests and `cargo clippy -p fl-host -p fl-host-proto --all-targets -- -D warnings`. Commit with the agent trailer and obtain task review.

### Task 3: Daemon inventory, bounded pipe I/O and epoch/lease fencing

**Files:**
- Create: `crates/host/src/daemon.rs`, `src/session.rs`, `src/input.rs`, `src/output.rs`.
- Create: `crates/host/tests/host_ipc.rs`.
- Modify: `crates/host/src/lib.rs`, `src/main.rs`.

**Interfaces:**
- Consumes Task 1's authenticated connection and request/event types plus Task 2's child handle and `serve` entry point.
- Produces the working `fl-host` executable serving `Spawn`, `Kill`, `List`, `Events`, `ClaimLease`, `Send` and `Signal`. `List` exposes state (including distinct `LeaderExited` and `CleanupComplete`), epoch, pid, start time, exit status and `consumer_stalled`.
- The daemon publishes `Spawned`, complete stdout `Line`, typed protocol failure and `Exited` events. `Exited` reports the exact leader status once and has a reserved notification slot; it does not release the owner. Publish `CleanupComplete` separately only when Task 2 resolves cleanup ownership. Sessions never share sequence counters, stdin queues, stderr tails or kill targets.
- Full durable-consumer ack/replay is deferred to the next slice. This executable is not connected to hub/wire, and no disconnect is represented as guaranteed delivery. Keep bounded unconsumed stdout and back-pressure without silent line drops. A later adapter cannot use it until ack/replay lands.

- [ ] **Step 1:** Add subprocess tests using the real `CARGO_BIN_EXE_fl-host` and `fl-host-testchild`, with isolated endpoint/token directories. Cover spawn/list/lease/send/echo/events/kill/exit, duplicate session refusal and daemon singleton behavior.
- [ ] **Step 2:** Add `reclaim_lease_refuses_stale_send`, `reclaim_lease_fences_already_queued_write`, `stale_epoch_cannot_write_to_respawn`, `daemon_restart_never_reuses_session_epoch`, `stdout_line_overflow_fails_session`, `stdin_queue_refuses_over_capacity`, `stdin_progress_timeout_is_bounded`, `stderr_flood_does_not_stall_stdout`, `maximum_line_is_fragmented_losslessly`, and `no_consumer_sets_stalled_without_unbounded_growth`. The restart test runs on all three platforms: clean up the old child, restart with the same runtime state, respawn the same session id and assert a strictly newer epoch and rejection of old epoch/lease writes before stdin. Include a retired cleanup record to prove its epoch high-water survives removal. Assert both stdin frame count and byte limits independently.
- [ ] **Step 3:** Run `cargo test -p fl-host --test host_ipc` and record the initial failure. Implement session admission up to 64 sessions and 16 subscribers, the 1 MiB partial-line cap, 8 MiB unconsumed protocol buffer, 64 KiB stderr tail, stdin limits and deadlines. The parser must detect newline/length without allocating an unbounded line, and reject a control frame whose decoded content exceeds its logical cap.
- [ ] **Step 4:** Implement lease fencing where bytes leave the stdin writer. Serialize takeover with active writes. If a partial old-lease line cannot safely finish after takeover, return an explicit failure and fence the session rather than writing a corrupt suffix under a new lease. Preserve the exact containment owner and epoch across client reconnects.
- [ ] **Step 5:** Run `cargo test -p fl-host -p fl-host-proto` and the scoped clippy command. Commit with the agent trailer and obtain task review.

### Task 4: Three-platform CI and foundation acceptance

**Files:**
- Modify: `.github/workflows/verify.yml`, `.fl/architecture.toml` if task integration needs corrections.
- Create: `docs/host-foundation.md` describing the implemented subset and exact test commands.

**Interfaces:**
- Consumes both crates and real executable tests from Tasks 1–3.
- Produces separate macOS and Windows jobs for `cargo test -p fl-host -p fl-host-proto`, `cargo clippy -p fl-host -p fl-host-proto --all-targets -- -D warnings`, and formatting. Preserve the existing Ubuntu trio as the Linux test path.

- [ ] **Step 1:** Assert architecture coverage includes both crates and exactly their real dependency edges. Run `fl arch --help` to select the existing conformance command; do not invent a command or weaken strict coverage.
- [ ] **Step 2:** Add macOS and Windows jobs without altering the existing required check name or repository protection. These jobs launch real daemon/test-child subprocesses through the integration tests, with no vendor logins, external daemons or inference spend.
- [ ] **Step 3:** Run `cargo fmt --all --check`, `cargo clippy --all-targets --workspace -- -D warnings`, and `cargo test --workspace` on Linux. The integration binaries cover the smoke checks. Record failures with their exact commands and distinguish baseline failures from this branch's failures.
- [ ] **Step 4:** Complete final whole-branch review with `code-review`, then open the owner-tier PR using `fl github pr create`. State that no agent migration occurred and that cross-platform delivery remains unproven until both new runner jobs pass.
- [ ] **Step 5:** After required current-head owner approval and all CI jobs pass, the PR author merges with `gh pr merge <number> --merge` and confirms the merge commit on `origin/main`. Record the implementation, test evidence and next ack/replay/PTY slice in FerroState. Do not deploy this daemon as a replacement for existing sessions in this PR.

## Next plans and migration gate

The next plan implements PTY rendering plus bounded snapshot rows and lossless event ack/replay. Service installation adds Linux delegated cgroups before strong Linux tree-termination claims. The SessionController then supplies durable delivery states, authority, thread ownership, control/prompt lanes, approval requests and restart/budget gates before any hosted adapter.

Codex integration must resume the intended thread through a dedicated owned app-server with an explicit authentication/configuration policy. A discovered shared daemon remains borrowed and must never be killed. Claude follows with its resume/stream protocol and operation conformance tests. Other vendors use their proven transport; the approved v1 design keeps Antigravity discovery-only.

Before migrating a live agent, coordinate with that agent's lead, save its FerroState handoff and vendor thread identity, establish an idle boundary, start/resume through the host, and prove prompt, reply, interrupt, approval, transcript and recovery behavior. Stop that agent's old tmux process only after the replacement route works, with a documented rollback. Do not automatically resend an ambiguous delivery. Migration of the whole roster and removal of legacy tmux delivery remain later milestones.
