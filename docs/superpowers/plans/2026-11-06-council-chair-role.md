# Council Chair Role Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: `subagent-dev`; fresh implementer per task, independent code review, fresh fixers/scoped review as needed.

**Goal:** Make editable Council Chair guidance usable in creation and explicit post-reassignment hydration, with neutral labels and unchanged authority boundaries.

**Architecture:** Reuse core `RoleRecord` defaults/overrides, hub catalog and lifecycle paths, and typed authenticated UI clients. Recast remains metadata-only; explicit Reforge applies current registration/catalog guidance. Existing roles and legacy wire IDs remain supported.

**Tech Stack:** Rust, React/TypeScript, Vitest.

**Spec:** `docs/superpowers/specs/2026-11-06-council-chair-role-design.md`.

## Global constraints

- No new `AgentRole` variant, permission/review tier, role pack framework or shared `.agents` file.
- Preserve `custom:lead` / `custom:assistant` / custom Lab Manager as nonstructural, without ID migration.
- No automatic recast delivery/restart or live changes; explicitly communicate Reforge requirement.
- Keep legacy executable choices, file guidance and unknown stored-role fallback compatible.
- No Host, peer production, hooks/wrappers, deployment or activation changes.

## Review focus

- Catalog label or builtin spoofing must not grant structural authority (Tasks 1–3).
- Missing/deleted records and legacy choices must not break existing agents (Tasks 1–3).
- Stale cached/hosted placement must not undo recast during explicit hydration (Task 2).
- Failed or late catalog requests/saves must not falsely report persistence or change a draft (Task 3).
- Guidance is metadata until explicit Reforge; no hidden injection or model-acceptance claim (Tasks 2–3).

### Task 1: Editable default catalog record

Files: `crates/core/src/agent.rs`; only catalog-count assertions in existing core/hub API tests when needed.

Consumes existing `RoleRecord`, `builtin_role_records`, `list_roles`, `get_role`, `upsert_role`.
Produces default `council-chair` record through unchanged catalog APIs. No new public enum or endpoint.

- [ ] Add tests proving Chair default guidance/label/review policy, stored template override, unchanged `custom:council-chair` and structural custom-shadow identity.
- [ ] Run focused tests RED before implementation; preserve logs under `/tmp`, not the product diff.
- [ ] Add the minimal default catalog data; update exact default counts only as required.
- [ ] Run focused/core tests GREEN; commit with machine author and Freya co-author; independent review before advancing.

### Task 2: Catalog-aware creation and explicit hydration

Files: `crates/hub/src/lifecycle.rs`, `crates/hub/src/api.rs`, `crates/hub/src/agents.rs` and their colocated/existing lifecycle tests. Add a small focused helper module only if it avoids duplicated prompt logic.

Consumes Task 1 catalog APIs. Produces store-aware role validation before side effects, current catalog-guided startup/reforge, and truthful metadata-only recast feedback. Preserve existing response fields; any additive status must be explicit, not model consumption.

- [ ] Inspect exact validation, saved hydrate, current registration, hosted metadata and recast paths; report material ambiguity before broad changes.
- [ ] Add meaningful RED tests for catalog Chair spin-up, edited template hydration, recast followed by explicit Reforge with stale hydration, legacy/default fallback, unknown-role refusal before side effects and structural custom-shadow safety.
- [ ] Implement the smallest catalog/current-role integration. No blind overwrite of unrelated hydration content or broad lifecycle precedence refactor; no automatic recast injection/restart.
- [ ] Run focused regressions and hub tests GREEN; commit with Freya trailer; independent review/fix before advancing.

### Task 3: Authenticated catalog UI and neutral labels

Files: `ui/src/types/api.ts`, `ui/src/api/client.ts`, a focused shared role hook/helper as needed, `SpinUpAgentModal.tsx`, `SettingsModal.tsx`, `AgentSettingsModal.tsx`, corresponding existing tests.

Consumes catalog `RoleRecord[]` via `GET /api/v1/roles`, saved `RoleRecord` via Owner `POST /api/v1/roles`, and existing recast/spin-up APIs.
Produces typed `HubApiClient` role methods, catalog-backed choices retaining legacy options, neutral labels with stable values, actual persisted save feedback, and explicit post-recast Reforge wording.

- [ ] Add RED tests for catalog-only Chair choice and correct `custom:council-chair` request in creation and both reassignment controls; retain legacy role choices.
- [ ] Add RED tests for authenticated reads/saves, failed save without false success, current selected/draft values surviving catalog refresh, edited labels and structural custom-shadow IDs.
- [ ] Implement bounded shared role helpers/hook and connect the three selectors/Settings role list; no local success before save acknowledgement. Remove origin prefixes from role display/auto-generated role names, not wire IDs.
- [ ] Run focused UI tests, full UI suite and build GREEN; commit with Freya trailer; independent task review.

### Final verification and publication

- [ ] Whole-branch independent review against the approved scope; fresh fixer/scoped re-review for accepted findings (at most three rounds).
- [ ] `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`; meaningful focused checks as needed, full UI tests/build from Task 3 retained unless later edits justify repetition.
- [ ] Verify clean source/trailers, current main integration and no runtime/shared instruction changes. Open ready PR with `fl github pr create`, send Ferris exact head and evidence. No merge before exact-head approval and required green checks.
