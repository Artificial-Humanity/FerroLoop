# Model switching and live out-of-band attribute sync (Local Record #130)

## Problem

1. `HubState.model_overrides` (set by `POST /api/v1/sessions/:id/model`) is laid over every
   `GET /api/v1/sessions` and `GET /api/v1/agents` summary by `apply_model_overrides`
   (`crates/hub/src/session_actions.rs`), for as long as the hub runs. After an operator uses the
   FerroDeck model or effort selector, a later switch made inside the vendor's own UI (Antigravity,
   Claude, Codex, Muse, Grok) never shows in FerroDeck.
2. For Antigravity the model endpoint is `override-only`: nothing reaches the CLI. A later launch
   (`launch_spec_for`, `crates/hub/src/lifecycle.rs`) passes `--model <id>` with whatever the hosted
   record holds. `agy --model` accepts only effort-suffixed ids (`agy models`: `claude-sonnet-5-5-medium`,
   `gemini-4-argon-high`, `gemini-3.8-flash-low`, `claude-opus-5-5-low`, `gpt-oss-120b-medium`, ...).
   `gpt-oss-120b` exists only as `-medium`.
3. A model switch made in the Antigravity UI before the next turn is not discovered:
   `generator_model_from_json` (`crates/hub/src/discovery.rs`) reads only `metadata.generatorModel`
   of a step, and a new conversation has no step yet.
4. `AgentSettingsModal` skips `getSessions()` whenever `executionMode` is already known, so opening
   Agent Settings never refreshes model, effort, mode or cwd. `inferVendorFromModel` classifies
   `claude-*` and `gpt-*` ids as Anthropic or OpenAI even when the session runs under Antigravity.

## Design

### A. An override yields to a newer live report

`ModelOverride` gains runtime state that the apply step maintains, set by one constructor
(`ModelOverride::new(model, thinking_level)`): a lazily recorded `baseline` (the live model and
effort the first time the override is applied after it was set) and a `confirmed` flag. Every
construction site uses the constructor.

On each apply (`HubState::with_model_overrides`, now under a mutable lock), per session:

| Live (discovered) model + effort vs. | Action |
| --- | --- |
| no baseline yet | record the live pair as the baseline; keep the override |
| equals the override (an absent override effort matches any live effort) | set `confirmed`; keep the override (no visible difference) |
| differs from the baseline and from the override | the vendor reports something newer: **drop the override**; show the live value |
| equals the baseline, differs from the override, `confirmed` | the vendor had applied the choice and has since gone back: **drop the override** |
| equals the baseline, differs from the override, not `confirmed` | the choice has not reached the vendor yet: keep the override |

A session with no live model leaves the override alone. Override keys by persona are matched as
today. The override is also dropped by the existing clear/reforge/spin-up/teardown paths.

**Known limit:** a change made out-of-band *back to the baseline* before the vendor ever
confirmed the override cannot be told apart from "not applied yet", and the override stays. This
is the price of not inventing a timestamp the vendors do not report (`updated_at` is a string and
no vendor gives a turn count we can compare). The vendor-specific pre-turn read in C narrows it
for Antigravity.

### B. Antigravity model ids

A pure function `agy_cli_model_id(model, thinking_level) -> String` in `lifecycle.rs`:

* A model that already ends in `-low`, `-medium` or `-high` is kept; an explicit `thinking_level`
  replaces that suffix.
* Otherwise the effort is appended (`thinking_level`, default `medium`).
* `gpt-oss-120b` is always `-medium` (the only variant the CLI lists).
* The result must still pass `normalize_plain_model` (otherwise it is not passed to `--model`).

`launch_spec_for` applies it for `antigravity` only. `POST /api/v1/sessions/:id/model` for an
Agy route stores the mapped id in the hosted record (so the next launch uses it) and still answers
`override-only`; the response gains `cli_model` with the mapped id. No other vendor changes.

### C. Antigravity discovery before the first turn

`generator_model_from_json` keeps `metadata.generatorModel` of the latest step first, and when no
step names one falls back, in order, to `/userInput/userConfig/plannerConfig/planModel` and
`/userInput/lastUserConfig/plannerConfig/planModel`. The value may be a placeholder
(`MODEL_PLACEHOLDER_M26`, `M39`, `M404`, `M405`, `MODEL_OPENAI_GPT_OSS_120B_MEDIUM`) or an
effort-suffixed id; both resolve through the existing `resolve_agy_model_info` and
`split_agy_thinking_suffix`, which must handle both. The truncated-body path
(`parse_agy_generator_model`) keeps its string scan; the fallback applies only to complete JSON.

### D. UI

* `AgentSettingsModal` fetches `getSessions()` every time it opens (`isOpen` becomes true), and
  keeps the fetched model, effort, adapter, mode and cwd; props are only the initial values.
* When the session adapter is known and is not the vendor that owns the model family, the adapter
  wins: a vendor is inferred from the model id only when the adapter is missing or `unknown`.
  `claude-sonnet-5-5`, `claude-opus-5-5` and `gpt-oss-120b` under `antigravity` stay Antigravity.

## Non-goals

No live model switch for an Antigravity session (the CLI has no supported path); no change to
Claude, Codex, Muse or Grok launch; no persisted overrides (still lost on hub restart); no
timestamps or turn counts added to discovery.

## Testing

Rust unit tests next to the code (`session_actions.rs`, `lifecycle.rs`, `discovery.rs`) and the
hub API tests for the model endpoint; Vitest for the UI. Verification trio before the PR:
`cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`.
