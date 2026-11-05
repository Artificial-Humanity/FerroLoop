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
* `offered_windows`: the sizes the model offers, ascending and without duplicates, when a vendor
  file lists them (today only Grok `context_windows`). Omitted when unknown. A cap is not a list
  of sizes: Codex `max_context_window` is not offered (see the evidence table).

Each vendor's branch in `context_usage_for_transcript` takes `max_tokens` from the best evidence
it has and sets `source` honestly. The constant 1,000,000 stays only as an `assumed` fallback where
no evidence exists on this machine. The per-vendor rules are fixed by the Task 1 audit and
recorded in the evidence table below.

### Evidence (Task 1 audit, 2026-11-05)

Read from the real files on the development machine, read-only. "Operator edit" marks a value an
operator changed by hand in a vendor file: it is real for that machine but is never a test
fixture. A hub that finds the vendor file reads it per session: the vendor directory is the
`.grok`, `.codex` or Muse data (`model-catalog`) directory above the session's transcript.

| Vendor | Effective window source | Offered sizes source | Result when absent | Status |
| --- | --- | --- | --- | --- |
| Grok | `signals.json` `contextWindowTokens` (present in 103 of 103 session files: 256000 in 101, 500000 in 2 operator-edited ones), else `summary.json` top-level `context_window` (1 of 106 files, 500000, an operator edit). | `~/.grok/models_cache.json` `models.<id>.info.context_windows` (`[256000, 500000]` for every listed model). The model is `summary.json` `current_model_id`, else `signals.json` `primaryModelId`. | The same cache's `info.context_window` (256000) = `catalog`; no cache or model not listed = the registry default (generic 500000) `assumed`. | VERIFIED. `config.toml` `[model."<id>"] context_window` (500000, an operator edit) is a third source the CLI may honour; the hub does not read it. INFERRED that the CLI honours it. |
| Codex | `config.toml` `model_context_window` (existing precedence, kept) = `reported`; else the rollout `token_count` `payload.info.model_context_window` = `reported`. | None. The cache `models_cache.json` holds per model `context_window` (272000 or 500000) and `max_context_window` (872000 for every model): a cap that `model_context_window` may be set up to, not a list of selectable sizes. | The cache `context_window` of the rollout's model (else `config.toml` `model`) = `catalog`; else the registry default `assumed`. | VERIFIED for the files. NOTE: rollouts on this machine report 828400, which is 95 percent (`effective_context_window_percent`) of the configured 872000, so the existing rule (config first) shows 872000 where the rollout says 828400. Kept; see Open point 1. |
| Claude | None. No transcript row, `message.model`/`requestedModel` (plain `claude-opus-5-5` style, no `[1m]` tag), `modelUsage` (token and cost counters only) or `~/.claude/settings.json` key (`model` is the alias `sonnet`) names a window. The text `[1m]` appears only in message content. | None. | 1,000,000 `assumed`. | VERIFIED absent. Auto `compact_boundary.preTokens` on this machine run from 203794 to 968577 with no per-session field to tell a 200k from a 1M session, so no `used_tokens` rule is added. |
| Muse | The `model_completed` event carries the model (`payload.event.model`; `runtime.session.metadata` `payload.record.model_id` as fallback) but no window. `~/.local/share/muse/model-catalog/*.json` `rows[].context_limit` for that model (1007997 for every listed text model) = `catalog`. | None (one limit per model). | 1,000,000 `assumed` (no catalog, model not listed, or `context_limit` null as for `muse-image-1.0`). | VERIFIED. |
| Antigravity | None. `transcript.jsonl` `PLANNER_RESPONSE` rows carry `input_tokens`, `cache_read_tokens`, `output_tokens` only. The hub parses only `metadata.generatorModel` from the live trajectory; no on-disk file under `~/.gemini/antigravity-cli` names a model window. | None found on disk. | 1,000,000 `assumed`. | VERIFIED absent on disk. NOT checked: a live `GetCascadeTrajectory` `modelUsage` or `agy models` response (needs a running language server; out of scope here). |

Open points:

1. Codex: when `config.toml` sets `model_context_window` and the rollout reports a smaller
   effective window (95 percent), which should the meter show? Phase 1 keeps the config value
   (existing behaviour, labelled `reported`).
2. Claude: a 200k or 1M choice (`[1m]`) is not recorded anywhere readable, so Phase 2 cannot show
   it as `reported` unless the session is launched by FerroLoop with a known model argument.

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
