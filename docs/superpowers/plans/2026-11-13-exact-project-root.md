# Exact Project Root Implementation Plan

> **For agentic workers:** Use the repository's subagent-dev workflow; execute the task with a fresh implementer and independent review.

**Goal:** Allow a named project binding to opt into using its exact registered root for hosted cwd and hydration, preserving existing binding behavior.

**Architecture:** Add a per-binding `cwd = "exact"` policy recognized by both strict CLI config and the hub's TOML reader. The absent policy retains today's workspace-umbrella behavior. Resolve all same-name bindings before selecting a directory; invalid policies, roots, and conflicting root/policy pairs fail closed. Existing hydration already derives its registered project from the binding; verify that the exact nested root and project-key role lookup survive the full spin-up/hydration path.

**Tech stack:** Rust, TOML (`serde`, `toml_edit`), existing hub lifecycle/hydration and CLI tests.

**Spec:** GitHub issue #501 and private `Notes/Kaggle/kaggle-parent-plan.md`.

## Constraints and review focus

- Existing bindings remain unchanged; external roots retain existing behavior.
- Absolute, canonical, existing roots only; refuse ambiguous or invalid bindings instead of selecting a fallback or decoy.
- Duplicate equivalent bindings may deduplicate; the same root with different cwd policies is ambiguous.
- CLI config uses `deny_unknown_fields`; all normal CLI commands must still parse an exact binding, and invalid policy values must fail clearly.
- Project role lookup remains keyed by the project key, never by path components.
- `fl agent area set --path Kaggle/<Competition>/` preserves the nested path; use isolated test state for the live throwaway-key check.

## Task 1: Exact-root binding, hydration and documentation

**Files:** `crates/cli/src/config.rs`, `crates/hub/src/project_init.rs`, `crates/hub/src/lifecycle.rs`, `crates/hub/src/hydration.rs`, relevant CLI integration tests and configuration documentation.

- [ ] Add failing tests for strict CLI parsing of `cwd = "exact"`, the absent default, and invalid values; ensure existing commands accept valid exact bindings.
- [ ] Add lifecycle fixtures for nested exact cwd and unchanged umbrella default, canonical external roots, invalid values, and duplicate bindings including conflicting roots and policies. No invalid or ambiguous binding may fall through to a catalog or workspace decoy.
- [ ] Add hydration fixtures proving nested `AGENTS.md` and role file `.agents/roles/<project-key>/<role>.md` are selected for the exact binding, including a cwd outside the registered project and conflicting bindings.
- [ ] Add a black-box test for `fl agent area set --path Kaggle/<Competition>/` using an isolated store; verify the stored path is exactly nested.
- [ ] Implement the binding policy in both configuration readers and lifecycle resolution. Preserve ambiguous and unavailable-root refusal behavior. Document the exact TOML field and default behavior.
- [ ] Run focused tests and an isolated live throwaway-key check using a temporary workspace, config and store, then commit with Felix's co-author trailer.
- [ ] Perform independent review and fixes, full workspace test / clippy / fmt, then open a separate PR from main.
