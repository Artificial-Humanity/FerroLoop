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
3. A new Antigravity conversation shows no model before its first turn: discovery
   (`crates/hub/src/discovery.rs`) reads only `metadata.generatorModel` of a step, and a new
   conversation has no planner step yet. The model the user picked is recorded as the REQUESTED
   model on the `CORTEX_STEP_TYPE_USER_INPUT` step itself (`userInput` on the step object). It is
   not the model that runs: an input sent while a tool step runs (`metadata.isInterruptingStep`)
   requests a model that the planner keeps not using, and the next planner steps still report the
   old `generatorModel`.
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
| equals the override: the model is equal ignoring case, and the effort is equal ignoring case, or the override has no effort, or the live effort is `None` (unknown) | set `confirmed`; keep the override; the override's effort is overlaid |
| differs from the baseline and from the override | the vendor reports something newer: **drop the override**; show the live value |
| equals the baseline, differs from the override, `confirmed` | the vendor had applied the choice and has since gone back: **drop the override** |
| equals the baseline, differs from the override, not `confirmed` | the choice has not reached the vendor yet: keep the override |

A live effort of `None` is "unknown", not a mismatch (`ModelOverride::matches_live`): Muse and
Claude without an effort signal never report an effort and the UI always sends one, so without
this rule the override would be dropped the moment the vendor applied the model and the
operator's chosen effort would disappear. A different reported effort is still not a match.

A session with no live model leaves the override alone. A model that discovery took from a
default, not from the session itself, is no live model either: when the Antigravity trajectory
names no usable model (the call timed out or failed, the body was too large, the negative cache
answered, or the placeholder is unknown), `resolve_agy_model_info` falls back to the config's
default model so the display still shows one. That model is marked `model_is_default` (carried
from the model info onto `DiscoveredSessionSummary`), and the apply step treats such a session
exactly like one with no model: no baseline, no confirmation, no drop, the override overlaid
unchanged. Without an override the default still shows. The marker is `#[serde(skip)]`: it never
appears in a JSON response and a payload cannot set it. Override keys by persona are matched as
today. The override is also dropped by the existing clear/reforge/spin-up/teardown paths.

**Rule: a default is not a live report.** A model that came from a default or fallback (a config
default, a settings default, a registry default) rather than from the session itself is not a
live report. It is marked on the in-process summary, never serialized, and the override logic
ignores it. The fallbacks that set the marker:

* Antigravity: the config's default model, when the trajectory names no usable model.
* Muse: the registry default (`MUSE_DEFAULT_MODEL`), when the transcript has no
  `runtime.session.metadata` record with a model id (no transcript, a failed read, or a new session).
* Codex: `~/.codex/config.toml` `model`, when the rollout tail names no `payload.model` (a fresh
  session, or a long turn whose tail is all tool output). A session started with `-m` differs
  from the config.
* Grok: `~/.grok/config.toml` `default`, when `summary.json` is missing, too large, or has no
  `current_model_id`.
* Claude: `~/.claude/settings.json` `model`, when the transcript carries no model signal. A
  `settings.json` written after the transcript stays a deliberate live report and is not marked.

The displayed model is unchanged in each case. Only the override decision differs.

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

The running model is the `metadata.generatorModel` of the LATEST step (scanning `/steps`, else
`/trajectory/steps`, newest first) that has one; on the same step `metadata.modelUsage.model` (same
placeholder form, carried only by `PLANNER_RESPONSE` steps; tool steps carry `generatorModel` but no
`modelUsage`) is used when `generatorModel` is absent or blank. Steps with neither (user inputs,
ephemeral messages) are skipped, so a newer user input never replaces a model that is running.

`planModel` is only the REQUESTED model. It is read only when NO step has a running model, that
is, before a conversation's first turn: the steps are scanned newest first for
`/userInput/userConfig/plannerConfig/planModel`, then `/userInput/lastUserConfig/plannerConfig/planModel`
on the same step, and the first non-blank value wins. Interrupting user inputs
(`metadata.isInterruptingStep`) show why a requested model is not the running one. The
tail-steps call (`GetCascadeTrajectorySteps` window) uses the running-model rule only, so a window
that holds only user inputs names no model and the lookup falls through to the full trajectory;
only the complete-trajectory parse applies the `planModel` fallback. Empty and whitespace-only
values are skipped everywhere. No steps array, an empty array, or no step with any such value gives
no model. There is no `userInput` at the body root or under `/trajectory`, and nothing is read from
there.

Showing a requested-but-different model as "pending" next to the running one is a follow-up and
NOT part of this record.

The value may be a placeholder (the families `MODEL_PLACEHOLDER_M318` to `M320` (Gemini 3.8 Flash
High/Medium/Low), `M400` to `M405` (Claude Opus 5.5 and Claude Sonnet 5.5, Low/Medium/High each),
`M417` to `M419` (Gemini 4 Argon High/Medium/Low), and `MODEL_OPENAI_GPT_OSS_120B_MEDIUM`) or an effort-suffixed id; both resolve through the existing
`resolve_agy_model_info` and `split_agy_thinking_suffix`, which must handle both. The
truncated-body path (`parse_agy_generator_model`) keeps its string scan and never reads
`planModel`; the per-step read applies only to complete JSON.

The response structure (`GetCascadeTrajectory` returns `{trajectory: {steps, generatorMetadata,
metadata, ...}, status, numTotalSteps}`; `GetCascadeTrajectorySteps` returns `{steps}`; planner and
tool steps carry `metadata.generatorModel`, `PLANNER_RESPONSE` steps also `metadata.modelUsage.model`;
`USER_INPUT` steps carry `userInput`) was confirmed
against the live Antigravity language server by the Lead on 2026-11-05, not by this branch's
tests, which use JSON shaped like that response.

### D. UI

* `AgentSettingsModal` fetches `getSessions()` every time it opens (`isOpen` becomes true), and
  keeps the fetched model, effort, adapter, mode and cwd; props are only the initial values.
* When the session adapter is known and is not the vendor that owns the model family, the adapter
  wins: a vendor is inferred from the model id only when the adapter is missing or `unknown`.
  `claude-sonnet-5-5`, `claude-opus-5-5` and `gpt-oss-120b` under `antigravity` stay Antigravity.

## Known limits

* (a) A model switch made in the Antigravity UI with no message sent leaves no step and is not
  discoverable.
* (b) Before a conversation's first turn the requested model may be shown although the turn never
  ran (cancelled or failed), until the planner answers.
* (c) When an override is dropped because the vendor reported a newer model, the hosted record
  still holds the operator's older choice, so a later restart or reforge relaunches that older
  choice (and FerroWatch may show an extra chip). Follow-up.
* (d) The hosted-record model is effort-suffixed for Antigravity after the model endpoint but raw
  after spin-up (the launch maps both). Follow-up.
* (e) An Antigravity model switch is display-only for a running session and applies at the next
  launch.
* The `matches_live` rule in A (live effort `None` is unknown) means an override with an effort
  stays confirmed for a vendor that never reports one; it is dropped only on a different model or
  a different reported effort.

## Non-goals

No live model switch for an Antigravity session (the CLI has no supported path); no change to
Claude, Codex, Muse or Grok launch; no persisted overrides (still lost on hub restart); no
timestamps or turn counts added to discovery.

## Testing

Rust unit tests next to the code (`session_actions.rs`, `lifecycle.rs`, `discovery.rs`) and the
hub API tests for the model endpoint; Vitest for the UI. Verification trio before the PR:
`cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`.
