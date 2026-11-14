# Session reset confirmation (#549, #546)

## Intent and contract

Clear and Reforge must never report a fresh session merely because a command was
written or queued. For the affected Codex, Muse, Grok and generic host/tmux reset
paths, confirmation requires a changed native thread/session identity bound to the
same exact terminal. Timeout or missing identity proof means an actionable error,
no startup prompt and no `session.cleared` event. An empty composer, process survival,
a changed hydration generation and a successful exit code are insufficient.

PR #481 and operational HOLD remain independent. No live reset, deployment,
restart or live backup/restore/purge is part of this work.

## Evidence

- `clear_or_reforge_session` sends Codex `/new` to ordinary operator delivery;
  `deliver_to_codex_route` invokes `codex queue`, bypassing slash parsing (#549).
- `send_tmux_reset_keys` writes C-u, raw slash and one Enter; it does not verify
  composition, submission or native identity (#546). C-u cannot safely remove an
  unknown multiline composer.
- Host `SessionSummary` identifies a host stream/epoch and child PID, not a native
  conversation. Native Codex/Muse routes do not carry a PID.
- Current Codex/Muse discovery uses persona/transcript age. A new file or matching
  cwd alone cannot establish ownership by the terminal being reset.
- Codex `thread/read` validates an already known thread; the existing control
  integration does not expose a terminal-to-current-thread binding. Ferris verified
  that the hosted Codex TUI holds no rollout FD: the shared app-server owns it.
- Installed Codex/Muse packages contain stripped binaries, with no verified
  native-identity observer available in their launchers or repository integration.
- Grok active-session metadata contains PID/native ID, but the reset path neither
  anchors an exact process generation nor validates metadata transitions. It has
  no implemented reset confirmation mechanism today.

## Approaches

1. **Refuse unsupported reset paths before side effects (recommended now).**
   Replace the unverified reset branch with a refusal that explains identity
   confirmation is unavailable and points operators to same-vendor remodel.
   Codex/Muse/Grok/HostCli use this refusal for both Clear and Reforge. This removes
   the false-success path without claiming unsupported vendor capabilities.
   The tradeoff is that reset remains unavailable on these vendors until an exact
   observer is implemented. Ordinary messaging and interrupt remain supported.
2. **Build native identity bindings now.** Add per-vendor terminal-bound observers,
   retain runtime generation, confirm reset, atomically refresh route state and
   deliver to the new identity. This restores functionality where source evidence
   exists, but Codex/Muse still need an established native interface; inferring
   ownership from transcript age is unacceptable. This is a larger follow-up.

The Lead explicitly authorized refusal when an exact observer is absent. Ferris approved
approach 1 and this scope on 2026-11-13 (wire 01a1d630-66d4-729f-b70b-733fc63717b7).

## Proposed change and scope

- Add a side-effect-free reset preflight in wire. Unsupported native reset routes
  return a stable diagnostic including vendor, unconfirmed identity reason and the
  same-vendor remodel alternative. Validate a required Reforge prompt first.
- Call preflight at the public reset entry and in the hub before deleting queued
  prompts or model overrides. On preflight refusal preserve both; return an error
  and do not publish `session.cleared` for single or bulk actions.
- Remove the unused unverified `alive_tmux_reset_target` / `send_tmux_reset_keys`
  reset path after checking no other callers need them. Preserve interrupt/paste
  target exactness tests under their corresponding supported operations.
- Defensively reject standalone `/new` and `/clear` in Codex operator delivery
  before any queue/headless/TUI fallback; ordinary text and `/compact` retain their
  existing behavior. This guard prevents future callers recreating #549.
- Feira owns Claude #544/#545. Her strengthened native-ID settle gate and timeout
  refusal will be integrated from main after her PR; do not duplicate her helpers.
- Antigravity's explicit in-place cascade rewind protocol is outside these two
  issues; preserve it pending Lead confirmation that a separate policy audit is
  sufficient. Webhook remains a deterministic test transport, not native identity
  evidence; do not add a production bypass for fake binaries or missing records.

## Verification and delivery

Wire unit regressions cover Clear/Reforge for Codex and every affected route
variant, required-prompt precedence, zero reset keys/queue calls and explicit
same-vendor remodel guidance. Codex guard tests prove `/new` and `/clear` cannot
reach queue while normal messages and `/compact` keep their paths. Hub fixtures
cover single and bulk refusals, no cleared event, no startup prompt and preserved
queue/model choices. Put subprocess scenarios in `crates/*/tests/` where needed;
pure unit checks stay beside their modules.

Run only audited pure/fixture tests locally. Do not run live-capable host-runtime
or full suites on ai-lab-0 while #539 isolation remains unmerged. Positive host
checks and the full suite obtain evidence in CI on runners without live services;
record this explicit Lead-granted verification exception in the PR. No tests of a
live agent reset. Perform subagent implementation, review/fix/re-review, final
review, then one PR with `Fixes #549` and `Fixes #546` for Ferris's review.

## Approved additions

Wire errors and single-session hub responses expose `code: reset_unsupported`,
`vendor`, `reason: identity_unconfirmable`, `alternative: remodel_same_vendor`,
and a plain operator `error` message. Hub preflight refusal is HTTP 409. Bulk
results retain those fields per session with `ok: false`; the overall bulk
request retains its normal response status. Preflight must precede queue/model
mutation and discovery-cache invalidation. There is no automatic remodel.

Update `docs/vendor-runtimes.md` so Clear/Reforge behavior and alternative match
these errors. Claude confirmed reset is owned by Feira's change and must be
integrated from main before release; Antigravity in-place rewind remains outside
this identity-confirmation contract and needs a separate audit. State this limit
in the PR body. Never use a fake binary/test environment to bypass production
preflight. The proposed verified Enter retry is deferred with native observers:
with no exact observer, no reset command is written at all.

## Saved-route resolution ruling (2026-11-14)

Ferris approved saved-route-only Clear/Reforge resolution and bulk enumeration in wire
`01a1dae3-45a0-720f-b548-3769ba4e3c16`. Unknown/unsaved single targets return HTTP 404
with refresh/register guidance without active discovery or terminal/host/vendor probes.
Bulk reset includes only saved/registered routes; Compact retains existing discovery.
Supported saved reset routes retain normal validation and dispatch behavior. This avoids
probing unsupported routes merely to identify them before refusal, without adding passive
scanner infrastructure.
