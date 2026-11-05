# Context window selection, Phase 2 (Local Record #134)

Phase 1 (PR #339) made the reported window honest: `SessionContextUsage` carries `source` and
`offered_windows`. Phase 2 adds the controls for the two vendors whose mechanisms the vendors
document, Grok and Claude. Codex and Muse (Phase 3) stay read-only until a throwaway session shows
that a launch flag works. Antigravity has no selector and stays read-only.

## Rules (agreed with the Lead on 2026-11-05)

* The hub never edits a vendor's own config file. It applies the window per session.
* The window is its own field, never part of the model string, because `matches_live`
  (`crates/hub/src/session_actions.rs`) and the model selector compare the model id.
* The hub validates a choice against what the vendor offers and then shows the EFFECTIVE window
  the vendor reports (Phase 1 `context_usage`), not the requested one. A choice the vendor ignores
  or refuses is shown as such.
* Per-vendor defaults live in `ferrodeck-settings.json`; a per-agent override lives on the hosted
  record and on `SpinUpRequest`. Only the Owner may set either.

## What is and is not verified

| Item | Status |
| --- | --- |
| Grok offers `[256000, 500000]` per model (`~/.grok/models_cache.json` `context_windows`) | VERIFIED (Phase 1 evidence) |
| Grok selects a window with the slash command `/context-window <N>` (session scoped) or `/model <name> <N>` | documented in Grok's user guide; NOT tested |
| A slash command works as the first positional prompt at launch | UNKNOWN |
| Grok's `GROK_CONFIG` overlay honours a `models.<id>.context_window` table | UNKNOWN (docs allow the `models` table; `model` singular is probably dropped) |
| Claude selects 1M with the `[1m]` suffix on the model name (`--model opus[1m]`, `/model opus[1m]`) | strings in the binary; NOT tested |
| Claude's 200k and 1M sizes | INFERRED from the binary, no catalog file on this machine |
| `normalize_plain_model` drops ids with brackets, so `opus[1m]` is lost at spin-up and restart | VERIFIED by reading `crates/hub/src/lifecycle.rs` |

The apply step for a live session and at launch (Task 4) is gated on a throwaway-session check of
the UNKNOWN rows; Tasks 1 to 3 need no live session.

## Design

* **Data.** `context_window: Option<u64>` (tokens) on the hosted-session record and on
  `SpinUpRequest`; `context_window_defaults: { vendor -> tokens }` in the hub settings. Absent =
  the vendor's own default.
* **Validation.** One function maps (vendor, model, tokens) to accepted or a refusal that names
  the offered sizes. Grok: the model's `context_windows` from the cache (fallback: refuse, say the
  list is unavailable). Claude: 200,000 (no tag) or 1,000,000 (`[1m]`). Every other vendor: refused
  with "this vendor's window is not selectable".
* **Bracket-safe model ids.** `normalize_plain_model` accepts a trailing `[<digits>m]` / `[<digits>k]`
  tag (and nothing else with brackets), and the launch path quotes it so a shell never globs it.
  Tests: the id survives spin-up and restart.
* **Endpoint.** `POST /api/v1/sessions/:id/context-window` (Owner only, 403 otherwise) takes
  `{ "context_window": N | null }`, validates, stores it on the hosted record, and answers with
  how it applies: `live` (sent to the running session), `next-launch` (stored; applies when the
  session is next launched) or `unsupported`. Until Task 4 is verified every vendor answers
  `next-launch`.
* **UI.** Agent Settings gets a window selector next to the model selector, listing the offered
  sizes and read-only for a vendor that offers none; Spin-Up gets an optional window; Settings ->
  Runtimes & Quotas gets the per-vendor defaults. The meter keeps showing the effective window.
* **Performance (Lead's review of #339).** `codex_catalog_window` reads the transcript tail from
  the in-memory `raw` instead of re-opening the file; the Grok branch reads `models_cache.json`
  only when the session has token usage.

## Non-goals

Codex, Muse and Antigravity selection; writing any vendor file; changing a session's window by
editing its config; persisting a window the vendor refused.

## Testing

Rust unit tests next to the code (temp dirs, `home_override`, never the real `$HOME`); hub API
tests for the endpoint; Vitest for the UI. Verification trio before the PR.
