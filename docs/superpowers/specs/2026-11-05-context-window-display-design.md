# Context window display, Phase 1 (Local Record #134)

Phase 1 of the per-agent context window work. It adds no controls. It makes the window the hub
reports correct and honest, and shows it. Phase 2 (selection for Grok and Claude) and Phase 3
(Codex and Muse launch flags) are separate records and PRs.

## Problem

The context meter shows `used / max` for a session, but `max` is partly invented:

* Claude, Antigravity and Muse report a hard-coded 1,000,000 whatever the model or the chosen
  window (`context_usage_for_transcript`, `crates/hub/src/discovery.rs`).
* Grok falls back to 500,000 when `signals.json` and `summary.json` are missing, although the
  model's default is 256,000 (`~/.grok/models_cache.json`).
* `ui/src/components/ChatPane.tsx` (`estimateSessionContextUsage`) has a second hard-coded table
  that is used when the server sends nothing.
* FerroWatch shows no context window at all.

There is no way to tell a measured window from a guess.

## Design

### Hub: one source of truth

`SessionContextUsage` (`used_tokens`, `max_tokens`) gains two fields, both optional on the wire so
older clients keep working:

* `source`: how `max_tokens` was found. `reported` (the session or the vendor said it for this
  session: Grok `signals.json`, a Codex rollout, a vendor setting), `catalog` (the vendor's model
  catalog or cache lists it for the session's model), or `assumed` (a registry default or a
  constant: nothing says it).
* `offered_windows`: the sizes the model offers, ascending, when the vendor lists them (Grok
  `context_windows`, Codex `max_context_window` as the cap, Claude 200k and 1M where the model
  offers 1M). Omitted when unknown.

Each vendor's branch in `context_usage_for_transcript` takes `max_tokens` from the best evidence
it has and sets `source` honestly. The constant 1,000,000 stays only as an `assumed` fallback where
no evidence exists on this machine. The per-vendor rules are fixed by the Task 1 audit (below) and
recorded in this document.

### UI: the server decides

`estimateSessionContextUsage` and its table are removed. The meter in Agent Settings, the roster
and FerroWatch use only `context_usage` from the server. With no `context_usage` the meter shows
what is used and no maximum. An `assumed` window shows a marker ("about 1M, assumed") and its
tooltip says nothing reported it. The offered sizes are shown read-only in Agent Settings.
FerroWatch shows used / max and the source for each agent.

## Audit rules for Task 1

For each vendor the implementer reads the real files on this machine and records, in a table in
this document, the evidence for the effective window and the offered sizes, or says there is none.
Nothing is guessed: where no evidence exists the result is `assumed`. Operator edits of vendor
config files made by hand are operator settings and are never relied on in tests.

## Non-goals

No new controls, no new endpoint, no write to any vendor file, no change to what a session is
launched with (Phase 2), no persisted settings.

## Testing

Rust unit tests next to the code with synthetic vendor files in temp dirs (never the real
`$HOME`); Vitest for the UI. The verification trio before the PR: `cargo test`,
`cargo clippy --all-targets`, `cargo fmt --check`.
