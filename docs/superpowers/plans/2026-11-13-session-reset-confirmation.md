# Session reset confirmation implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `subagent-dev` for implementation and `code-review` after each task and for the whole branch. Steps use checkbox syntax for tracking.

**Goal:** Replace unconfirmable Codex/Muse/Grok/HostCli Clear/Reforge with structured, side-effect-free refusals.

**Architecture:** Wire exposes a pure preflight and a serializable refusal; every affected entry refuses before transport. Hub resolves once, preflights before mutation, returns structured HTTP 409 or per-session bulk errors. No native observer or automatic remodel is introduced.

**Tech Stack:** Rust, serde JSON, existing wire/hub test fixtures.

**Spec:** `docs/superpowers/specs/2026-11-13-session-reset-confirmation-design.md`, approved by Ferris with additions in wire message `01a1d630-66d4-729f-b70b-733fc63717b7`.

## Global constraints

- `code=reset_unsupported`, `reason=identity_unconfirmable`, `alternative=remodel_same_vendor`; include canonical `vendor` and plain operator `error`.
- Native affected routes: Codex, Muse, Grok and all HostCli variants, even when hosted/live or carrying placeholder IDs.
- Validate a nonempty Reforge prompt before any transport; refusal never queues slash or startup prompt, writes terminal keys, mutates queue/model choices, or invalidates discovery cache.
- No live-capable tests locally until #539 isolation is merged. Pure/audited fixture tests only; full suite/positive host evidence in CI under Ferris's explicit exception. No live reset.
- Preserve PR #481 hold and all operational HOLDs. One worktree writer/Cargo owner. No push until task and whole-branch review pass.
- Preserve Antigravity in-place rewind, explicitly outside identity contract and needing its own audit; Claude tail belongs to Feira and integrates from main.
- Commits use configured machine identity and `Co-authored-by: Fiona <fiona@artificialhumanity.io>`.

## Review focus

1. Generic HostCli claiming `claude` or `codex` must not bypass refusal by vendor spelling.
2. Missing, stale or live-looking targets cannot trigger terminal probes before refusal.
3. Single/bulk clear and reforge refusals must preserve all raw/canonical/persona aliases.
4. A future caller using ordinary Codex operator delivery must not recreate slash-as-chat.
5. Existing guidance errors and compact behavior must retain their response shapes/semantics.

### Task 1: Wire reset preflight and Codex slash guard

**Files:** Modify `crates/wire/src/dispatcher.rs`; place pure unit tests beside it and any subprocess scenarios in `crates/wire/tests/`.

**Interfaces:** Produce public `SessionResetRefusal` (Serialize, Display, Error) carrying the five fields in Global constraints, and `preflight_session_reset(route: &VendorRoute) -> Result<(), SessionResetRefusal>`. Existing `clear_or_reforge_session(...) -> Result<(), String>` remains source-compatible; serialize the refusal into its String error, retaining every field. No new dependencies. Hub will use the typed preflight directly.

- [ ] Write regression checks for Codex/Muse/Grok/HostCli clear and reforge, including blank/placeholder IDs and whitespace/mixed-case HostCli vendor labels. Assert stable fields and remodel guidance. A valid Reforge prompt cannot be dispatched on refusal; an invalid prompt remains rejected before transport.
- [ ] Establish RED safely: direct API regressions use explicit fixture transports/sentinels and no default live endpoint. Audit transitive calls before running. A new pure preflight test may use a minimal compile scaffold returning Ok to demonstrate the behavioral failure before implementing refusal. Do not run the old positive reset tests locally.
- [ ] Implement pure preflight, invoke it immediately after required-prompt validation, and remove the unverified host/tmux reset branch plus reset-only helpers after checking callers. Keep Claude/Agy/Webhook existing paths. No test-mode or fake-binary bypass.
- [ ] Guard standalone `/new` and `/clear` in `deliver_to_codex_route` before queue/control/host lookup; trim and compare case-insensitively. Return the same structured refusal. Preserve ordinary messages and `/compact` behavior.
- [ ] Update obsolete positive reset tests to assert refusal; preserve interrupt exact-target coverage by removing only reset assertions from its old mixed test. New subprocess scenarios belong in integration tests. Refusal tests should be pure where possible; fixture commands must be explicit and can never fall back to host/tmux/vendor resolution.
- [ ] Run exact audited new tests and compile all wire tests with `cargo test --offline -p fl-wire --no-run`. Run wire all-target strict clippy and fmt. Save command/results and RED/GREEN evidence under the task artifact directory; report unrun tests as CI-only.
- [ ] Commit, verify trailer and clean worktree; return SHA, tests and limitations. Coordinator verifies and dispatches fresh read-only review; fresh fixer/scoped review for accepted findings, maximum three rounds.

### Task 2: Hub refusals, bulk results and runtime documentation

**Files:** Modify `crates/hub/src/api.rs`, relevant pure/fixture tests in its existing test module, `docs/vendor-runtimes.md`.

**Interfaces:** Consume Task1 `preflight_session_reset` and `SessionResetRefusal`. Make `reset_session` return `Result<Value, (u16, Value)>`: old errors retain `{error: text}`, unsupported reset returns the serialized refusal at 409. Update the two production callers and existing test assertions accordingly. Keep bulk compact errors as strings in `{error: text}` while preserving reset refusal fields at top level of each failed result with `session_id`, `ok:false` and `status:409`.

- [ ] Add fixture regressions using a temp home/store/routes and direct queue seeding: single clear/reforge and bulk refusal across native affected variants; assert 409 or individual bulk result, all stable fields, zero `session.cleared`, no startup delivery and unchanged queue/model aliases. Control inherited hydration helper paths explicitly; inject a deterministic reforge preparation closure if needed, without a production bypass.
- [ ] Demonstrate RED on safe fixture tests, then implement. Resolve a route once; preflight it before prompt generation that can spawn a helper, queue/model changes and discovery invalidation. Retain it for dispatch-once so no concurrent rediscovery chooses another route. Nonexistent route errors preserve caller state. Supported fallible guidance preparation must still precede queue/model changes.
- [ ] Return structured refusal at 409 from single endpoint and preserve fields per bulk item. Existing dispatch failures remain 502. Invalidate discovery only after an actual supported reset attempt, and never retry after partial delivery.
- [ ] Update runtime docs with Clear/Reforge behavior per vendor: Claude uses Feira's changed-ID confirmation; unsupported Codex/Muse/Grok/HostCli refuse and offer same-vendor remodel without running it; Antigravity remains in-place rewind, not identity-confirmed and requiring separate audit. Preserve webhook as deterministic test transport, not a native confirmation claim.
- [ ] Run exact audited new and existing guidance refusal fixture tests. Compile hub/wire tests without execution, run workspace all-target strict clippy and fmt; do not run full/live-capable suites locally. Record CI-only evidence honestly.
- [ ] Commit and return SHA/results. Coordinator verifies, fresh task review/fix loop, then final whole-branch review.

## Integration and delivery

- [ ] Integrate Feira's Claude #544/#545 from main when available; ask her to identify final SHA. Do not cherry-pick her whole unrelated branch into ours. Re-review any overlapping composition change.
- [ ] Check main freshness and no unrelated diff. Run permitted compile/lint/fmt and audited exact tests once at final head. Document all full-suite/positive host checks as pending CI.
- [ ] Fresh whole-branch `code-review` on original main base through final head, with approved spec/plan and explicit #539 exception. Resolve accepted findings through fresh fixer/scoped review.
- [ ] Push one ready PR with `fl github pr create`, `Fixes #549`, `Fixes #546`; request Ferris review. PR body states refusal behavior, no automatic remodel, AGY audit limit, Claude integration and exact local versus CI verification.
- [ ] Obtain complete CI evidence before landing; never claim full verification from pure tests. Keep #481 unpushed/unmerged and operational HOLD untouched.

## Saved-route resolution ruling (2026-11-14)

Ferris approved saved-route-only Clear/Reforge resolution and bulk enumeration in wire
`01a1dae3-45a0-720f-b548-3769ba4e3c16`. Unknown/unsaved single targets return HTTP 404
with refresh/register guidance without active discovery or terminal/host/vendor probes.
Bulk reset includes only saved/registered routes; Compact retains existing discovery.
Supported saved reset routes retain normal validation and dispatch behavior. This avoids
probing unsupported routes merely to identify them before refusal, without adding passive
scanner infrastructure.
