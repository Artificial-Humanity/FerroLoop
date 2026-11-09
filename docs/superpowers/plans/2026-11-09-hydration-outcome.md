# Hydration Outcome Implementation Plan

> **For agentic workers:** Use the repository-required `subagent-dev` skill: a fresh implementer, then the `code-review` loop and final branch review.

**Goal:** Report hydration acceptance separately from the existing delivery boolean.

**Architecture:** Add a defaulted tri-state to lifecycle records and responses. Preserve typed delivery evidence at the existing dispatch boundary, ordinary delivery intent and safe retry policy. Fresh argv launches remain unconfirmed.

**Tech Stack:** Rust, serde, existing lifecycle fixtures and CLI black-box tests.

**Spec:** `docs/superpowers/specs/2026-11-09-hydration-outcome-design.md`

## Global Constraints

Implement only after D2a merges. Merge current main before beginning. No live
launches, service changes, vendor polling, callbacks or new acknowledgement
commands. Keep `hydration_dispatched` behavior compatible. No UI changes; Felix
owns presentation. Do not modify #402 receipt semantics or elevate delivery intent.

Coordination: Feira owns D3. She adds `host_session_id` immediately after
`tmux_target` in SpinUpResponse and RemodelResponse and populates their literals.
Place `hydration_status` after `hydration_dispatched`, preserve her field when
integrating main, and leave host addressing to her branch.

## Review Focus

- Old records deserialize as unconfirmed regardless of their old boolean.
- A live process waiting on a dialog or delayed startup is not acceptance evidence.
- An ambiguous transport failure must not cause a second send.
- Restart/remodel must replace stale confirmation with this attempt's outcome.
- No-route or proven non-delivery differs from an uncertain transport failure.

### Task 1: Additive hydration status through lifecycle and CLI

**Files:** Modify `crates/hub/src/lifecycle.rs`, `crates/cli/src/cmd/agent.rs`,
and `crates/cli/tests/agent_lifecycle.rs`. Keep unit fixtures inline in lifecycle.
Update existing record literals in other test modules only as required. Consume
existing wire typed outcomes; do not change dispatcher behavior.

**Interfaces:** Add public `HydrationStatus::{Confirmed, Unconfirmed, Failed}`,
serialized as `confirmed`, `unconfirmed`, `failed`, with Default=Unconfirmed.
Add `#[serde(default)] hydration_status: HydrationStatus` to HostedSessionRecord,
SpinUpResponse and RemodelResponse (RestartResponse already contains the record).
Use a private outcome carrying both `dispatched: bool` and `status` so existing
callers cannot accidentally derive the boolean from confirmation.

- [ ] Add failing serialization tests: missing field defaults to unconfirmed even
      with legacy boolean true; each status round-trips using the exact strings.
- [ ] Add failing lifecycle tests for live dialog and delayed startup fixtures:
      legacy dispatched remains true when handed off, new status is unconfirmed,
      and delivery count is one. Known immediate exit reports false/failed.
- [ ] Add failing delivery tests: AppServerAccepted for the exact submission is
      confirmed; VendorQueued, TransportAccepted and CompactionAccepted are
      unconfirmed; no route/NotConnected are failed; ambiguous Failed is
      unconfirmed. An ambiguous error or accepted queue never triggers resend.
      Keep existing rediscovery retry only for proven NotConnected.
- [ ] Add restart/remodel coverage showing previous confirmation does not survive
      into a new attempt, including the no-runtime/non-spawned path.
- [ ] Observe focused test failures, then implement the enum, fields and private
      outcome. Preserve boolean results and ordinary intent. Use existing typed
      dispatch instead of flattening success/errors to strings too early; never
      infer acceptance from PID, argv, echo, route discovery or idle status.
- [ ] Expose status in CLI JSON and append a `hydration_status=<value>` field to
      human-readable spin-up/remodel output without reordering existing fields.
      Update black-box assertions and test legacy record decoding.
- [ ] Run focused lifecycle/CLI checks, then `cargo test --workspace`,
      `cargo clippy --all-targets --workspace -- -D warnings`,
      `cargo fmt --all --check`, and `git diff --check`. Private fixtures must
      create their own prerequisites and pass in CI without live sockets.
- [ ] Commit with Fiona coauthor trailer, verify clean worktree and return exact
      commit and validation evidence. Fresh independent task and branch review.
- [ ] Link existing follow-up [#406](https://github.com/Artificial-Humanity/FerroLoop/issues/406)
      for eventual confirmation and generation-fenced registration; do not create
      a duplicate. Explain in the PR
      that ordinary fresh launches remain unconfirmed until such a signal exists.
- [ ] Open a separate PR with `fl github pr create`, request Ferris exact-head
      review and merge only after current-head approval and green CI.
