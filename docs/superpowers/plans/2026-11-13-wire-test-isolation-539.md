# fl-wire Test Isolation Implementation Plan

> **For agentic workers:** Use subagent-driven development. Execute and review the task before opening a PR.

**Goal:** Prevent the fl-wire test binary from reaching live host, wire, hub, or tmux endpoints, while allowing explicitly controlled mock fixtures.

**Architecture:** Extend the reviewed pre-main test isolation pattern from #536 to fl-wire. Audit actual socket and terminal entry points first; retain scoped fixture access and preserve ordinary executable behavior even when Cargo features unify. Audit fl-cli separately against the same constraints.

**Tech Stack:** Rust, Cargo libtest, isolated Unix sockets, strace.

**Spec:** GitHub issue #539 and Ferris's instruction to start with fl-wire and fl-cli.

## Constraints and review focus

- Never run an unguarded test against a live endpoint; before guards exist, block all connect syscalls, disable real tmux, and isolate HOME.
- Use /var/tmp for worktrees and target directories; leave normal production binaries unaffected.
- A renamed test executable, manipulated temporary environment, unregistered fixture, symlink, or unrelated concurrent mock must not permit a live connection.
- Keep supported Linux, macOS, and Windows test binaries fail-closed; mock fixtures must still work.
- Audit every fl-wire test binary and list all non-fixture socket destinations and attempts; distinguish blocked audit failures from genuine test failures.

### Task 1: Isolate and audit fl-wire test binaries

**Files:** `crates/wire/Cargo.toml`, `crates/wire/src/lib.rs`, relevant `crates/wire/src/*` and shared test-guard code in `crates/host-proto/src/*`; tests alongside affected code or `crates/wire/tests/`.

- [ ] Map every executable test target and all host, wire, hub and terminal discovery paths; collect fault-injected traces without reaching live sockets.
- [ ] Write tests that fail safely before the guard: unregistered defaults refuse, registered fixtures work, fixture deregistration refuses, symlinks and manipulated temp paths refuse, and production executables retain normal behavior.
- [ ] Implement the smallest shared pre-main guard, adapting #536's exact fixture registration, for all relevant fl-wire test binaries and real tmux entry points. Add controlled fixture registration to tests that require mock endpoints.
- [ ] Run guarded full fl-wire suites, blocking or privately redirecting sockets; run fault-injected traces over each test binary and verify no non-fixture Unix destination. Check fmt, clippy, and production executable behavior.
- [ ] Commit with Felix co-author trailer; obtain independent review and fix any findings before opening a PR.

### Task 2: Audit fl-cli separately

**Files:** `crates/cli` test binaries and their shared dependencies; use a separate branch and PR.

- [ ] Build and trace each CLI test binary with connections blocked and real tmux disabled; classify attempted non-fixture destinations and subprocesses.
- [ ] Reuse the reviewed shared guard where applicable; create a scoped follow-on plan and implementation for CLI-specific sockets and spawned executables.
- [ ] Verify and review as above; report remaining crate audits under #539 without claiming the whole issue complete.
