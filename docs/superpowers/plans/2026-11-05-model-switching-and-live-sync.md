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

1. `crates/hub/src/discovery.rs` `generator_model_from_json`: fallback to the two `planModel`
   pointers in spec C when no step names a model.
2. Make sure the value resolves for placeholders (`MODEL_PLACEHOLDER_M26/M39/M404/M405`,
   `MODEL_OPENAI_GPT_OSS_120B_MEDIUM`) and effort-suffixed ids through the existing
   `resolve_agy_model_info` / `split_agy_thinking_suffix`; add the missing handling.
3. Tests with realistic JSON: step model wins over `planModel`; `userConfig` before
   `lastUserConfig`; no steps at all; an empty string; placeholder and suffixed forms; truncated
   body still uses the string scan. Check the real shape against a live language server when one
   is running (read-only) and say in the report if it could not be checked.

## Task 3 – Override supersession (Rust)

1. `crates/hub/src/session_actions.rs`: `ModelOverride::new`, the baseline and `confirmed` state,
   and the mutable apply per the spec A table. Update every construction site
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

## Final review

Whole-branch `code-review` on `origin/main..HEAD` against the spec, then `cargo test`,
`cargo clippy --all-targets`, `cargo fmt --check`, `cd ui && npx tsc --noEmit && npm test`, then
the pull request and a review request to Ferris.
