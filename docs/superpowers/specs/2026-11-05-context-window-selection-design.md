# Context window selection, Phase 2 (Local Record #134)

Phase 1 (PR #339) made the reported window honest: `SessionContextUsage` carries `source` and
`offered_windows`. Phase 2 adds the controls for **Grok**, the one vendor whose mechanism is
verified. Claude, Codex, Muse and Antigravity stay read-only (see "Why Grok only").

## Rules (agreed with the Lead on 2026-11-05)

* The hub never edits a vendor's own config file. It applies the window per session.
* The window is its own field, never part of the model string, because `matches_live`
  (`crates/hub/src/session_actions.rs`) and the model selector compare the model id.
* The hub validates a choice against what the vendor offers and then shows the EFFECTIVE window
  the vendor reports (Phase 1 `context_usage`). The endpoint reports what the hub did
  (`persisted`, `method`), never what the vendor did.
* Per-vendor defaults live in `ferrodeck-settings.json`; a per-agent override lives on the hosted
  record and on `SpinUpRequest`. Only the Owner may set either.

## What was verified (throwaway sessions, 2026-11-05)

Run on private tmux servers (`tmux -L`), a scratch cwd, no registration with the agent registry,
one model turn each at most. Recorded here so the PR does not need the transcript.

| Item | Result |
| --- | --- |
| Grok 1.0.77, `grok-4.7`: bare `/context-window` opens a picker listing the offered sizes | `256k` and `500k` (VERIFIED) |
| `/context-window 256000` and `/context-window 500k` change the window live; `summary.json` `context_window` follows in about 1.5-2 s | VERIFIED. `signals.json` only exists after the first turn, then shows `contextWindowTokens` |
| `/context-window 400k` (not offered) | refused: `unknown context window '400k'; use one of: 256k, 500k`, nothing changes (VERIFIED) |
| `/model grok-4.7 256k` | works; needs a second Enter when the autocomplete popup is open (VERIFIED) |
| A slash command as the first positional prompt (`grok -m grok-4.7 '/context-window 500k'`) | run as a command, no model turn (VERIFIED). The hub's launch already uses that positional for the agent's startup prompt, so it is not used |
| `GROK_CONFIG` overlay with `models.<id>.context_window` or `model.<id>.context_window` | no effect in either shape (VERIFIED) |
| Claude Code 2.1.318: every Claude 5.5 model shows a 1M window in `/context`, with and without `[1m]` | VERIFIED. So `[1m]` selection could not be shown to change anything on this account |
| Claude `/model <x>` persists the choice as the default for new sessions (writes `~/.claude/settings.json`) | VERIFIED (and a side effect of this check; see the PR) |
| An unquoted `--model sonnet[1m]` in a shell | zsh: `no matches found`; sh/bash pass it literally unless a file like `sonnet1` exists (VERIFIED). The hub does not build a shell string: `tmux new-session -- <argv...>` passes arguments directly and keeps `sonnet[1m]` literal even with `sonnet1` present (VERIFIED on tmux 3.6) |

Host Codex settings are an operator choice and change (JARVIS, 2026-11-05: `model_context_window`
272000 with `model_auto_compact_token_limit` 210000, after 350000 earlier the same day). The hub
reads them as `reported` and never writes them; no test depends on them.

## Why Grok only

Claude: no verified way to select a window (all 5.5 models are 1M here). Codex and Muse: no flag
was tested (Phase 3, with `tmux -f /dev/null` so the resurrect and continuum plugins never run).
Antigravity: no selector exists. These vendors answer a window request with
`<vendor>'s context window is not selectable` (Claude: `... not selectable yet`).

## Design

* **Data.** `context_window: Option<u64>` (tokens) on the hosted-session record, on `SpinUpRequest`
  and in the spin-up response; `context_window_defaults: { "grok": tokens }` in the hub settings
  (PUT merges per vendor; `null` removes one). Absent means the vendor's own default.
* **Validation (`validate_context_window`, `crates/hub/src/context_window.rs`).** Grok: the size
  must be in the model's `context_windows` list in `~/.grok/models_cache.json`, or equal the
  model's own `context_window`. The model id match is exact (case-sensitive, no alias), like
  Phase 1. A model missing from the cache and an unreadable cache give different errors. Every
  other vendor is refused. A stale default is ignored silently at spin-up; it never fails a spin-up.
* **Model ids.** `normalize_plain_model` keeps one trailing `[<1-4 digits>k|m]` tag for Claude ids
  (`opus[1m]`) through spin-up and restart; other vendors drop it as before. The parser works on
  bytes and never panics on non-ASCII input.
* **Endpoint.** `POST /api/v1/sessions/:id/context-window`, Owner only (403 before the body is
  read). Body `{ "context_window": <positive integer> | null }`. A number is validated, stored on
  the hosted record (when one exists) and sent to a Grok route session as the text
  `/context-window <tokens>`, the same way `/model` is dispatched. `null` clears the stored value
  and sends nothing (Grok has no reset). Response: `{ ok, session_id, vendor, context_window,
  persisted, method }` with `method` `operator-prompt` (sent to the live session) or `next-launch`
  (stored only). A number that was neither stored nor sent returns 409 "not applied".
* **Launch.** Nothing applies a stored window at launch yet; a stored window is recorded and shown.
  That is the next task.
* **UI.** Agent Settings gets a window selector next to the model selector (editable for Grok,
  read-only for the others); Spin-Up gets an optional window; Settings -> Runtimes & Quotas gets
  the Grok default. The meter keeps showing the effective window from the vendor.
* **Performance (Lead's review of #339).** The Codex catalog window slices the tail of the
  in-memory rollout instead of re-reading the file; the Grok catalog is read only when the session
  has token usage.

## Not verified

* That Grok applies a window pasted into the live TUI by the hub exactly as when typed by hand
  (the hub dispatches the text the same way as `/model`; the response says what the hub did).
* How Grok behaves when a window smaller than the current usage is chosen (the docs say it starts
  an auto-compact).
* Anything about Claude window selection, Codex and Muse flags, and Antigravity.

## Non-goals

Codex, Muse, Claude and Antigravity selection; writing any vendor file; persisting a window the
vendor refused; resetting Grok's window live.

## Testing

Rust unit tests next to the code (temp dirs, `home_override`, never the real `$HOME`); hub API
tests for the endpoint with a fake tmux; Vitest for the UI. Verification trio before the PR.
