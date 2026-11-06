# Context window selection, Phase 2: implementation plan (Local Record #134)

Spec: [2026-11-05-context-window-selection-design.md](../specs/2026-11-05-context-window-selection-design.md).

## Task 1: data model, validation, bracket-safe ids, perf (Rust). Done

1. The two performance fixes from the Phase 1 review.
2. `normalize_plain_model`: accept a trailing `[<digits>k|m]` tag only; check how the argv reaches
   the shell or tmux and quote it; tests that `opus[1m]` survives spin-up and restart.
3. `context_window` on `HostedSessionRecord` (serde default) and `SpinUpRequest`; the spin-up
   handler validates and records it.
4. The validation function and its tests per vendor.
5. `context_window_defaults` in the hub settings (GET and PUT, validated, Owner only).

## Task 2: endpoint and live apply for Grok (Rust). Done

`POST /api/v1/sessions/:id/context-window`, Owner only. For a Grok session whose terminal is live
the hub pastes `/context-window N` into it and answers `method: "operator-prompt"`. When the
terminal is not live the window is saved on the hosted record
and the hub answers `method: "stored"`. A stored window is NOT applied to anything: it is only
recorded. Hub API tests: 403 for a Peer, unknown session 404, a refused size 400 naming the offered
sizes, `null` clears, other vendors refused, a record-only session is stored.

## Task 3: Agent Settings selector (UI). Done

The "Context window" select in Agent Settings (owner, Grok, at least two offered sizes), types in
`ui/src/types/api.ts`, tests. The effective window is the session's reported `max_tokens`. After an
`operator-prompt` answer the select shows the requested size for at most 25 seconds while it waits
for a changed report; after a `stored` answer it shows the effective window again.

## Task 4: apply at launch, Spin-Up field, Runtimes & Quotas default. NOT built

Nothing safely applies a stored window at launch yet, so these stay out until something does:

* A slash command cannot be the first positional prompt of a launch, because the hub's launch
  already uses that positional for the agent's startup prompt.
* Sending the command after launch is unverified: when the terminal accepts it, and whether Grok
  applies it before the first turn, are not known.
* Without launch-time apply, a Spin-Up field and a Runtimes & Quotas Grok default would promise
  something that does not happen.

`context_window_defaults` (Task 1) and the stored `context_window` on the record stay as data.

## Final review

Whole-branch `code-review`, the full verification trio plus the UI checks, then the PR.
