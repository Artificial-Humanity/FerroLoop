# Subdirectory Cwd and Reforge Implementation Plan

> **For agentic workers:** Use the repository-mandated `subagent-dev` skill, with fresh implementers and `code-review` gates.

**Goal:** Fix all four #427 follow-ups while preserving runtime cwd and guidance authority.
**Architecture:** Named configuration bindings choose runtime cwd; canonical workspace containment permits only the project umbrella during hydration. Reforge separates required authority checks from optional fallback metadata and refreshes the entire authoritative snapshot after helper failure.
**Tech Stack:** Rust, existing hub lifecycle/hydration APIs, isolated filesystem and store fixtures.
**Spec:** `docs/superpowers/specs/2026-11-09-subdirectory-cwd-427-design.md`

## Global Constraints

Preserve Sonora umbrella cwd; use registered-root guidance; no store schema change. No live runtime/deployment operations. Unit tests remain within their modules; subprocess tests belong under crate `tests/`. Every commit has `Co-authored-by: Fiona <fiona@artificialhumanity.io>`.

## Review Focus

- Alias differs from both root basename and umbrella name: named binding must win (Task 1).
- Workspace ancestors, non-Git siblings and separate-Git siblings still warn (Task 1).
- Canonical paths and symlinks cannot widen workspace trust (Task 1).
- Helper can change registration or hosted metadata before failing: fallback must use fresh, consistent authority and metadata (Task 2).
- Helper success avoids optional metadata reads, while authority failures and catalog/recast overrides retain precedence (Task 2).

### Task 1: Explicit project umbrella cwd and narrow hydration acceptance

**Files:** `crates/hub/src/lifecycle.rs`, `crates/hub/src/hydration.rs`; API/CLI default-cwd callers only if required to thread isolated home/config context.
**Interfaces:** Preserve public default-cwd compatibility where possible. Private `resolve_project_cwd` may accept home/config context. `Context::resolve_with_catalog` retains its existing inputs/output.

- [ ] Add failing tests using non-Git `workspace/Sonora`, Git child `github`, and non-Git/separate-Git sibling fixtures. Prove named bindings with a different alias choose Sonora explicitly, and external roots keep their root.
- [ ] Cover explicit cwd, no binding fallback, same-worktree sibling subproject mapping, malformed/ambiguous bindings, canonical containment and warning negatives.
- [ ] Run focused tests and preserve RED logs.
- [ ] Implement name-based binding selection and workspace-child runtime path. Keep defaults consistent through API and CLI callers.
- [ ] Add only the narrow non-Git direct-workspace-child allowance to hydration; retain registered-root guidance.
- [ ] Prove an isolated Sonora spin-up has `warnings: []` and its persisted cwd remains Sonora. Run affected unit/integration tests, formatting and strict affected Clippy. Commit and return evidence.
- [ ] Fresh read-only task review and bounded fixer/re-review loop.

### Task 2: Lazy, fresh reforge fallback snapshot

**Files:** `crates/hub/src/api.rs` and in-module tests.
**Interfaces:** Preserve `build_reforge_prompt_with_hydrate` and external helper behavior. Refactor private snapshot helpers as needed.

- [ ] Add regressions for helper success without optional metadata reads and helper failure/blank after editing registration/role/hosted metadata. Assert fallback uses current metadata and rejects newly invalid authority.
- [ ] Preserve tests for catalog/recast precedence, sanitized hosted lookup errors, missing registration override behavior and lock-free helper/filesystem execution.
- [ ] Run tests RED, then split authority preparation from optional snapshot reads. If helper fails, re-read authority and fallback metadata together under one new lock, never mixing generations.
- [ ] Run focused reforge tests, strict affected Clippy and formatting; commit with evidence.
- [ ] Fresh read-only task review and bounded fixer/re-review loop.

### Completion

- [ ] Run `cargo test --workspace` (includes isolated integration smoke), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`, and `git diff --check`.
- [ ] Fresh whole-branch review against spec; resolve accepted findings via subagents.
- [ ] Push; open ready PR via `fl github pr create` with the Lead ruling reproduced in its body and `Closes #427`; request Ferris review.
- [ ] Follow WORKFLOW for current-head approval, checks and author-owned merge.
