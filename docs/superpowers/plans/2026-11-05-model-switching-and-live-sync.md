# Model switching and live out-of-band sync – implementation plan (Local Record #130)

Spec: [2026-11-05-model-switching-and-live-sync-design.md](../specs/2026-11-05-model-switching-and-live-sync-design.md).
Rust tasks: `cargo test -p fl-hub <filter>` while iterating; `cargo clippy -p fl-hub --all-targets`
and `cargo fmt --check` before each commit. UI task: `cd ui && npx tsc --noEmit && npm test`.

## Task 1 – Antigravity model ids (Rust)

1. `crates/hub/src/lifecycle.rs`: `agy_cli_model_id(model, thinking_level)` per spec B, with unit
   tests (suffix kept, suffix replaced by an explicit effort, appended, default `medium`,
   `gpt-oss-120b` forced to `-medium`, ids that `normalize_plain_model` rejects).
2. `launch_spec_for`: for `antigravity`, pass the mapped id to `--model`. Other vendors unchanged
   (assert their argv in tests).
3. `crates/hub/src/api.rs` `handle_post_session_model`: for an Agy route, store the mapped id in the
   hosted record (`set_hosted_session_model`) and add `cli_model` to the response; method stays
   `override-only`. Non-Agy responses keep their exact shape. Extend the hub API tests.

## Task 2 – Pre-turn discovery (Rust)

1. `crates/hub/src/discovery.rs`: the running model is the `/metadata/generatorModel` of the latest
   step that has one (scanning `/steps`, else `/trajectory/steps`, newest first), else that step's
   `/metadata/modelUsage/model`. Only when no step has one (before the first turn) the complete
   trajectory falls back to the requested model: `/userInput/userConfig/plannerConfig/planModel`,
   then `/userInput/lastUserConfig/plannerConfig/planModel` on the same step, newest step first
   (non-empty after trimming). The tail path (`GetCascadeTrajectorySteps`) never reads `planModel`,
   so a window of user inputs only falls through to the full trajectory. Nothing is read from the
   body root or `/trajectory`. The structure was confirmed against the live Antigravity language
   server by the Lead on 2026-11-05, not by this branch's tests.
2. Make sure the value resolves for placeholders (`MODEL_PLACEHOLDER_M26/M39/M404/M405`,
   `MODEL_OPENAI_GPT_OSS_120B_MEDIUM`) and effort-suffixed ids through the existing
   `resolve_agy_model_info` / `split_agy_thinking_suffix`; add the missing handling.
3. Tests with JSON shaped like the real response (steps with `"type":
   "CORTEX_STEP_TYPE_USER_INPUT"` etc.), under `/steps` and `/trajectory/steps`: a newest `USER_INPUT`
   step (also with `isInterruptingStep: true`) after planner steps keeps the planner's model; a
   planner step after a user input gives its model; `modelUsage.model` when `generatorModel` is
   absent, `generatorModel` preferred when both are present; a tool step with `generatorModel`
   counts; only user inputs gives `planModel` from the complete parse (`userConfig` before
   `lastUserConfig`, newest step first) and nothing from the tail parse; the stub-server tail case
   with only a user input falls through to the full trajectory, and the first-turn case resolves
   `planModel`; blank values ignored; placeholder and suffixed forms resolve; truncated body still
   uses the string scan and never reads `planModel`.

## Task 3 – Override supersession (Rust)

1. `crates/hub/src/session_actions.rs`: `ModelOverride::new`, the baseline and `confirmed` state,
   and the mutable apply per the spec A table (`matches_live`: a live effort of `None` is unknown,
   so an override with an effort is confirmed and kept by a live B/`None`, and a different reported
   effort is still no match). Update every construction site
   (`api.rs`, `lib.rs`, `remodel_api_tests.rs`, unit tests) to use the constructor.
2. `crates/hub/src/lib.rs` `with_model_overrides` applies under the mutable lock.
3. Tests, one per table row, plus: a persona-keyed override, an override with no effort, a
   session with no live model, two sessions with independent state, and the clear paths still
   removing the override.

## Task 4 – UI

1. `ui/src/components/AgentSettingsModal.tsx`: always fetch sessions on open (spec D); the
   fetched model, effort, adapter, mode and cwd take precedence over the initial props; stale
   values reset on close or on a target change.
2. `ui/src/components/ChatPane.tsx` `inferVendorFromModel` callers and the same logic in
   `AgentSettingsModal`: the adapter wins when known.
3. Tests: the modal refetches on every open even with an `executionMode` prop; a changed model on
   the second open shows; Antigravity sessions with `claude-sonnet-5-5`, `claude-opus-5-5` and
   `gpt-oss-120b` stay Antigravity; a session with no adapter still infers from the model.

Known limits and follow-ups (not part of this record) are listed in the spec's "Known limits".

## Final review

Whole-branch `code-review` on `origin/main..HEAD` against the spec, then `cargo test`,
`cargo clippy --all-targets`, `cargo fmt --check`, `cd ui && npx tsc --noEmit && npm test`, then
the pull request and a review request to Ferris.
