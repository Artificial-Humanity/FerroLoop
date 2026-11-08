# Rendered PTY Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use the repository's `subagent-dev` skill and `code-review` loop. A fresh implementer and reviewer own each task; the coordinator does not write task code.

**Goal:** Preserve a bounded terminal screen through PTY replay loss and expose opt-in snapshots plus ordered, lease-fenced Unix resize.

**Architecture:** A session-local vt100 projection receives every PTY read before journal admission/drop. The projection mutex orders capture, output and ioctl/parser resize; the existing writer control queue fences resize authority. Protocol capability gating preserves old strict decoders and raw callers.

**Tech Stack:** Rust, Tokio, exact vt100 0.18.1, libc PTY ioctls, existing SliceBudget and host protocol.

**Spec:** `docs/superpowers/specs/2026-11-08-rendered-pty-design.md`, implementing the approved Council Milestone 3 and Ferris's Record185 assignment.

## Global constraints

- Default 40 rows / 120 columns / 512 scrollback rows; cap 128 rows, 256 columns, 8,192 cells and 512 scrollback rows.
- Rendered text at most 196,608 bytes; serialized response fits the existing 256 KiB frame.
- No raw capture/delivery contract change; no ConPTY claim; no new Attach protocol.
- Bound and charge persistent/temporary projection and snapshot storage before allocation.
- No mutex held over blocking output reads, socket writes or admission waits.
- Machine author and `Co-authored-by: Fiona <fiona@artificialhumanity.io>` on every commit.
- Use normal fixture temp directories; never put TMPDIR under this workspace.

## Review focus

1. Journal admission failure still feeds every byte to the screen before dropping replay history (Task1 pump tests, Task3 flood).
2. Split OSC/DCS terminators, UTF-8 continuation bytes and unending strings cannot leak payload into text or grow allocations (Task1).
3. A stale controller, disconnected connection or old epoch cannot resize a replacement session (Task2 authority tests).
4. Geometry growth/resize failure cannot leave the kernel and parser inconsistent or undercharge retained capacity (Tasks1/2/3).
5. Worst-case cell contents and JSON escaping must fit a response; older daemons never receive the new operation without its bit (Task2).

### Task 1: Bounded projection and ordered PTY output

**Files:** Create `crates/host/src/projection.rs`; modify `crates/host/Cargo.toml`, `Cargo.lock`, `crates/host/src/{lib,session,slice_budget}.rs`, and geometry constants/validation in `crates/host-proto/src/extension.rs` as needed. Unit tests stay inside their modules.

**Interfaces:** `projection::ProjectionSnapshot` exposes contents, cursor_row, cursor_col, rows, cols, generation, output_seq, output_bytes. `Session::capture_screen() -> Result<ProjectionSnapshot, ExtensionRefusal>` takes one coherent snapshot. `Session::resize_pty(cols:u16, rows:u16) -> Result<u64, HostError>` returns successful generation. Constructor plumbing may be selected by the implementer but must allocate/reserve the projection before child spawn, initialize from validated PtyOptions, and retain a dedicated master descriptor. Task2 consumes these methods and shared geometry constants.

- [ ] Write failing unit tests for cursor/CR redraws, split UTF-8, alternate-screen restoration, split/string-flood sanitization, dimensions and scrollback bounds, budget refusal/release, failed resize preserving state, and coherent monotonic snapshots.
- [ ] Run focused tests and retain red evidence under scratch/ferroloop/185-task1-*.
- [ ] Pin vt100 and implement the finite filter/projection with charged storage and no callback side effects. Derive allocation bounds from the inspected crate, including capacity after shrinking.
- [ ] Wire every first-class Pty session before its output pump starts. Feed both successful journal admission and fallback scratch reads under one bounded ordering lock; update raw tail and journal watermark within that order. Leave pipe pumps untouched.
- [ ] Implement dedicated-master ioctl + parser resize under that lock; preflight limits/budget, leave parser unchanged on ioctl failure.
- [ ] Add a deterministic pump regression forcing journal admission failure and proving screen state still includes the consumed chunk; check >8 MiB repeated redraw freshness and raw bounds.
- [ ] Run host/protocol tests, fmt, and strict host/protocol all-target Clippy; commit explicit paths with Fiona trailer. Fresh Task1 review must approve before Task2.

### Task 2: Opt-in snapshots and fenced resize protocol

**Files:** Modify `crates/host-proto/src/{extension,client}.rs`, relevant exports/tests; `crates/host/src/{daemon,input,authority}.rs` where needed; add focused host integration cases in `crates/host/tests/host_ipc.rs` or a dedicated `rendered_pty.rs` with isolated fixtures.

**Interfaces:** Export `RENDERED_PTY_FEATURE = 8` and strict `RenderedScreen` schema1 (StreamId plus Task1 snapshot fields). Add `ExtensionRequest::CaptureRendered { stream }` and `ExtensionResponse::RenderedCapture { screen }`. Async/blocking `capture_rendered(stream:StreamId) -> Result<RenderedScreen,HostError>` and `resize_session(stream:StreamId, lease:ControllerLease, cols:u16, rows:u16) -> Result<u64,HostError>` are capability-gated. Reuse reserved Resize/Resized wire fields; return projection_revision as generation.

- [ ] Write failing roundtrip/bounds/unknown-field tests, frozen old Capabilities/Sessions/raw JSON tests, and an older-daemon mock proving no unknown request is sent without capability bit8.
- [ ] Add failing host tests for valid Unix screen capture, pipe WrongMode, non-Unix Unsupported, wrong host/epoch, invalid geometry and snapshot counter consistency.
- [ ] Implement bounded strict screen decoding/validation and response admission covering text, serialization and wire buffers until response completion. Reject oversize coherently; do not truncate.
- [ ] Lookup capture by exact StreamId under existing epoch authority; never fall back to session name. Advertise bit8 only where both projection and resize are implemented.
- [ ] Add CtrlKind::Resize and WriterHandle::resize through the existing bounded control queue, rechecking lease/connection/epoch/liveness at execution. Hold the relevant authority fence through Session::resize_pty and return its generation.
- [ ] Test controller takeover/disconnect while resize is queued, stale stream after respawn, refusal without geometry mutation, bounded pending admission and successful ordered geometry updates. Exercise real PTY ioctls on Unix.
- [ ] Run host/protocol tests, fmt and strict all-target Clippy, including Windows GNU compile; commit with Fiona trailer. Fresh Task2 review must approve before Task3.

### Task 3: CLI and real TUI/resize acceptance

**Files:** Modify `crates/cli/src/cmd/host.rs`; add isolated tests in `crates/cli/tests/host_capture.rs`; extend `crates/host/src/bin/fl-host-testchild.rs` and host subprocess tests; document behavior in `docs/host-rendered-capture.md`.

**Interfaces:** `fl host capture TARGET [--raw | --rendered] [--json]`; default/raw retain existing max-bytes and exact output. JSON requires rendered; explicit max-bytes conflicts with rendered. Use Task2 client methods; no implicit raw fallback or caller migration.

- [ ] Write failing CLI tests for default/raw equality, rendered clean text, JSON identity/cursor/geometry/watermarks, invalid flag combinations and unsupported old daemon behavior.
- [ ] Create finite gated TUI fixture that emits >8 MiB of CR/cursor/ANSI redraws including early escape state and alternate-screen transitions. Final visible text must depend on state older than replay retention; verify exact screen, cursor and dimensions after eviction while raw remains bounded.
- [ ] Exercise unread/stalled replay consumers and the parser's admission-failure path without blocking child drainage. Include OSC/DCS floods and split escape/UTF-8 chunks with strict deadlines.
- [ ] Add a child command reporting TIOCGWINSZ, then coordinate resize/output/capture to assert both kernel and parser dimensions plus generation ordering. Keep Windows asserting Unsupported until native ConPTY exists.
- [ ] Implement CLI flags/output and concise docs describing limits, sanitized controls, raw compatibility and progress-watermark semantics.
- [ ] Run focused CLI/host tests and strict Clippy; commit explicit files with Fiona trailer. Fresh Task3 review follows.

### Final verification and delivery

- [ ] Run cargo fmt --all -- --check, cargo clippy --workspace --all-targets -- -D warnings, cargo test --workspace --no-fail-fast, and Windows GNU host/protocol all-target Clippy. Save logs outside the worktree.
- [ ] Fresh whole-branch code review from e7b0b432 to final head; evaluate actual findings, fresh fixers/scoped reviews for accepted findings (maximum3 rounds).
- [ ] Push feature branch, open ready PR with fl github pr create, request Ferris current-head Reviewer App review, and observe Linux/macOS/Windows CI.
- [ ] Resolve failures, merge only after valid approval/current base/green checks, verify origin/main, mark185 done and save handoff. No service restart/install in this task.
