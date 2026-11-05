# Context window selection, Phase 2: implementation plan (Local Record #134)

Spec: [2026-11-05-context-window-selection-design.md](../specs/2026-11-05-context-window-selection-design.md).

## Task 1: data model, validation, bracket-safe ids, perf (Rust)

1. The two performance fixes from the Phase 1 review.
2. `normalize_plain_model`: accept a trailing `[<digits>k|m]` tag only; check how the argv reaches
   the shell or tmux and quote it; tests that `opus[1m]` survives spin-up and restart.
3. `context_window` on `HostedSessionRecord` (serde default) and `SpinUpRequest`; the spin-up
   handler validates and records it.
4. The validation function and its tests per vendor.
5. `context_window_defaults` in the hub settings (GET and PUT, validated, Owner only).

## Task 2: endpoint (Rust)

`POST /api/v1/sessions/:id/context-window`, Owner only; stores on the hosted record; answers
`next-launch` for now. Hub API tests (403 for a Peer, unknown session 404, a refused size 400
naming the offered sizes, `null` clears, other vendors refused).

## Task 3: UI

Agent Settings selector, Spin-Up field, Runtimes & Quotas defaults; types in `ui/src/types/api.ts`;
tests.

## Task 4: apply (gated on the throwaway-session check)

Launch-time and live apply for Grok and Claude, only for the mechanisms the check confirms.

## Final review

Whole-branch `code-review`, the full verification trio plus the UI checks, then the PR.
