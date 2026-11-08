# Host Safety Baseline Implementation Plan

> **For agentic workers:** Use `subagent-dev` with per-task `code-review` and a final whole-branch review. Keep all work on `fiona/host-safety-baseline-183`.

**Goal:** Keep captured output fresh, expose the running host's build identity, and establish deterministic flood and device baselines for Record 183.

**Architecture:** Capture is a bounded rolling tail independent of delivery retention. Existing reliable delivery obligations remain intact. Build telemetry uses capability negotiation and an opt-in response so strict older decoders receive unchanged messages.

**Tech Stack:** Rust, existing host protocol, isolated subprocess integration tests.

**Spec:** Council-reconciled architecture plan, Milestone 1 / Record 183. The assigned scope is capture freshness, unattended/stalled LegacyPipe and ReliablePipe flood behavior, build identity, and current platform device truth. First-class PTY implementation belongs to Record 184.

## Global Constraints

- Preserve existing protocol wire compatibility and ReliablePipe acknowledgment/lifecycle guarantees.
- Keep the Record 382 worktree separate. Do not change live services or installed binaries.
- Keep unit tests in their module and subprocess tests under `tests/` with isolated endpoints.
- Report the current Unix environment-selected PTY discrepancy explicitly; do not claim Windows ConPTY support.
- LegacyPipe currently promises retention of never-written bytes and stalls when full. Resolve the requested non-wedging behavior against this contract with the Lead before changing delivery loss policy.

## Review Focus

- Multiple overflow writes and a single oversized capture write must retain the newest bytes without exceeding the cap (Task 1).
- Strict old protocol decoders must not receive new unsolicited fields; old hosts must give an explicit unavailable identity (Task 2).
- Build identity must describe the daemon binary, including `fl host serve`, rather than the querying CLI (Task 2).
- Device tests must probe actual child terminal behavior and compare it with reported mode/kind on both platforms (Task 2).
- A full output backlog must preserve lifecycle/control responsiveness, bounded memory, and recovery after consumer progress or teardown (Task 3).

### Task 1: Fresh bounded capture

**Files:** `crates/host/src/session.rs`, focused host integration fixtures if necessary.
**Interface:** Preserve `Session::push_stdout(&[u8])`, `push_stdout_tail(&[u8], usize)`, and `stdout_tail(usize) -> String`.

- [x] Add a failing regression for more than 8 MiB through `push_stdout`, with distinct recent markers, repeated overflow, and oversized single chunks.
- [x] Reuse bounded tail retention for non-reliable and sinkless capture; preserve the existing public capture size limit and reliable capture behavior.
- [x] Exercise the sinkless pump and reliable pump with real output beyond 8 MiB and assert fresh tail bytes and bounded retention.
- [x] Run focused host tests, commit with Fiona's trailer, and pass independent review (`e6dd352d`; 260 host/protocol tests).

### Task 2: Build identity and current device acceptance

**Files:** `crates/host/build.rs`, host daemon/library, `crates/host-proto/src/extension.rs` and client, `crates/cli/src/cmd/host.rs`, host test child and integration tests; existing shared build-stamp tests if their package closure needs updating.
**Interface:** Add a capability-gated host-status request/response and client accessor; keep existing capability response fields unchanged. Return compiled version/commit/dirty/time from the serving process. Preserve unknown identity for old daemons.

- [x] Add failing protocol and host integration tests for negotiated status and compatibility, then implement the smallest additive interface.
- [x] Reuse the standard-library-only shared build-stamp logic without adding a host dependency on core or vendor crates.
- [x] Display daemon build metadata in `fl host status` text and JSON, with clear unsupported/unavailable output for older hosts.
- [x] Probe child `IsTerminal` behavior for plain Pipe and `FL_HOST_PTY=1`, both LegacyPipe and ReliablePipe. Assert current mode/kind reports separately from actual device truth; retain explicit first-class PTY refusal at this milestone.
- [x] Run focused tests and build-stamp validation, commit, and pass independent review (`c7c5b8c5`, fix `4c477008`). Review required tracking the shared build helper in dirty inputs and Cargo watches.

### Task 3: Deterministic flood and stall baseline

**Files:** `crates/host/tests/host_ipc.rs`, `crates/host/tests/reliable_events.rs`, `crates/host/src/bin/fl-host-testchild.rs`, `crates/host/src/output.rs`, and shared test fixtures.
**Interface:** Existing subprocess host RPC and controlled subscriber barriers. Preserve delivery policy unless the Lead explicitly resolves the contract conflict in favor of a change.

- [x] Add bounded fixtures producing more than 4,096 events and more than 8 MiB of carriage-return/ANSI output with a unique final marker.
- [x] Cover sinkless, LegacyPipe sink without a subscriber, LegacyPipe stalled subscriber, ReliablePipe unattended, and ReliablePipe stalled subscriber.
- [x] Assert bounded retention/RSS where supported, responsive control/list operations, honest stall/exit state, and fresh capture after recovery/drainage. Prove that cleanup can terminate a stalled session within a deadline.
- [x] Record whether each mode makes unattended progress or applies recoverable backpressure; do not silently convert lossless delivery to eviction.
- [x] Correct the demonstrated LegacyPipe recovery defect: catch-up must not shed a progressing reader merely because retained records were originally published after it subscribed. Distinguish the current fresh publication from retained catch-up, with a deterministic 128-old/one-new regression and a control proving actual live overflow still sheds.
- [x] Run focused host/protocol tests, commit, and pass independent review.

### Final verification and delivery

- [ ] Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and the host/protocol integration smoke tests on the final source.
- [ ] Complete whole-branch review, resolve accepted findings through fresh fixer/reviewer agents, and save a FerroState handoff.
- [ ] Open the PR with `fl github pr create` and request Ferris's review. Land only after the required current-head approval and checks.
