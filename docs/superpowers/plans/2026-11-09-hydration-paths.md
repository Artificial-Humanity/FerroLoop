# Hydration Paths Implementation Plan

> **For agentic workers:** Use the repository-required `subagent-dev` skill: a fresh implementer per task, then the `code-review` loop and final branch review.

**Goal:** Generate usable absolute guidance paths, preserve worktree instructions safely, and report missing guidance without refusing launches.

**Architecture:** Extract the bounded Git layout resolver already used by Codex trust into neutral `fl-mcp` utilities. A hub hydration context selects roots and guidance, then returns text plus warnings to lifecycle callers.

**Tech Stack:** Rust, existing TOML/config-entry and filesystem facilities, serde, private fixture tests.

**Spec:** `docs/superpowers/specs/2026-11-09-hydration-paths-design.md`

## Global Constraints

The design is approved and D2b has merged. No live launches, service/config writes,
new dependencies, shell/Git execution for discovery, UI changes, new CLI restart
command, durable warning fields, or hard-coded journalist alias. Preserve runtime
cwd, permission behavior, catalog-role authority and hydration outcome semantics.
Felix owns FerroDeck rendering. Check current main before the final PR.

## Review Focus

- Foreign repositories cannot supply project instructions even when names match.
- Missing or ambiguous roots never become guessed read instructions.
- Extraction preserves Codex explicit denial and linked-worktree trust precedence.
- Authored catalog guidance and external hydrate overrides retain their boundaries.
- Automatic restarts leave warning traces with persona/session identity.

### Task 1: Share bounded Git layout discovery

**Files:** Create `crates/mcp/src/project_paths.rs`; expose it in `crates/mcp/src/lib.rs`;
modify `crates/mcp/src/vendor/codex.rs`. Keep unit tests inline in those modules.

**Interfaces:** Public `GitProjectLayout { worktree_root: PathBuf, git_dir: PathBuf,
common_dir: PathBuf, main_checkout_root: Option<PathBuf> }` and
`resolve_git_project(cwd: &Path) -> Result<Option<GitProjectLayout>, GitProjectError>`.
No Git marker returns None; invalid cwd or present invalid metadata returns a safe
error. `common_dir` is canonical commondir when present, otherwise canonical git dir.
`main_checkout_root` preserves the old Codex fallback when commondir is present.
Keep bounded file reading shared internally so extraction does not duplicate it.

- [x] Add failing layout tests for normal repository/subdirectory, linked worktree,
      no Git, standalone gitfile/submodule, relative gitdir and commondir, symlinks,
      missing target, dangling/unreadable/non-regular/oversized markers and safe errors.
- [x] Observe RED, extract the existing parser and implement the public layout
      result. Do not add HEAD/objects/refs health validation or repository mutation.
- [x] Make Codex trust consume the helper. Preserve canonical cwd, root-key-first
      denial precedence, no-Git direct cwd trust, optional commondir behavior,
      diagnostic redaction and config/Git byte limits (1 MiB / 8 KiB).
- [x] Run `cargo test -p fl-mcp`, relevant hub Codex trust tests and CLI MCP black-box
      tests; run Clippy for affected targets, formatting and diff checks. Record logs.
- [x] Commit with Fiona trailer.
- [x] Fresh independent task review and fix loop before Task 2 consumes this API.
      Approved at `1884a8c1` before the checkpoint (Fiona FerroState handoff).

**Implemented API:** `fl_mcp::project_paths::resolve_git_project(cwd: &Path) ->
Result<Option<GitProjectLayout>, GitProjectError>`, with the four public path fields
specified above. Errors expose safe Display/Debug diagnostics and keep discovered
root context crate-private for Codex's suggested trust key. One crate-private
`read_bounded_file` serves Git metadata and Codex config. Directory `.git` markers
do not inspect `commondir`; only gitfiles do. `main_checkout_root` is the optional
parent of the resolved common directory, not independent checkout verification.
The existing procfs bounded-reader test moved alongside the shared implementation;
Codex's malformed-metadata test additionally checks the root key from a child cwd.

**Task 1 verification:** RED recorded nine intended failures (exit 101). GREEN:
`fl-mcp` 187 unit tests and four catalog tests passed (two existing tests ignored);
three hub `codex_trust` tests and 30 CLI MCP black-box tests passed. Clippy for
`fl-mcp`, `fl-hub` and `fl-cli`, all targets with `-D warnings`, formatting and diff
checks passed. External artifacts: `paths-task1-{red,mcp,hub-codex-trust,cli-mcp,
clippy,fmt,diff}.log`, each retaining its exit code. Full workspace verification
remains at Task 2's final gate; Task 1 independently approved at `1884a8c1`.

### Task 2: Render resolved guidance and propagate warnings

**Files:** Prefer a focused `crates/hub/src/hydration.rs` for context/path/rendering
helpers, registered in `lib.rs`; integrate existing `lifecycle.rs` and `api.rs`
prompt callers, and CLI `cmd/agent.rs`. Unit tests stay inline; CLI/subprocess tests
belong in existing `crates/cli/tests/agent_lifecycle.rs` or `host_lifecycle.rs`.
Update existing record/response test fixtures only where needed.

**Interfaces:** Internal `HydrationPrompt { text: String, warnings: Vec<String> }`
with explicit filesystem context. Consume Task 1's layout API; no parser copy.
Add default-empty `warnings: Vec<String>` to SpinUpResponse and RestartResponse;
default and append to existing RemodelResponse.warnings. Do not store warnings on
HostedSessionRecord. Preserve D3's host_session_id if it lands meanwhile.

- [x] Add failing path fixtures for case-sensitive project names, cwd already in the
      project and subdirectories, config name differing from root basename, valid
      same-repository worktrees, foreign same-name repositories, malformed/ambiguous
      layout and non-Git registered projects. Assert actual selected instruction
      paths, explicit fallback warnings and no unrelated-repository instructions.
- [x] Add failing guidance tests for project-role-before-global precedence, symlink
      resolution, missing role/project/workspace files, silent absent STATE.md,
      custom catalog guidance and preserved structural-custom safeguards.
- [x] Implement explicit context from existing config-entry/catalog facilities and
      workspace-root resolution. Resolve before generating prompts, after the final
      launch cwd is known. Keep launch cwd and permission decisions unchanged.
      Canonical repository identities must match before selecting worktree guidance.
      Reuse existing config parsing; redact parser contents in warning diagnostics.
- [x] Render absolute existing paths and explicit missing-guidance lines; keep
      logical slugs for registration/FerroState. Preserve authored catalog text and
      identify its workspace-relative base. Preserve the external hydrate override
      and use the same contextual renderer for canonical reforge fallback.
- [x] Propagate warning lists through spin-up, restart and remodel without dropping
      existing remodel warnings. Add old-response deserialization tests with empty
      defaults and lifecycle assertions for all three paths.
- [x] Log each guidance warning via the existing hub logging convention with persona
      and session id, including automatic restart callers. Add a private fixture or
      injectable log-capture regression that proves the restart boundary leaves a trace.
      No automatic Rust caller currently generates hydration; see implementation notes.
- [x] Print warnings in human-readable CLI output using the remodel convention;
      preserve valid JSON stdout and existing hydration/status fields. Test successful
      launches with warnings and JSON/default behavior; do not create a new command.
- [x] Run focused tests, then `cargo test --workspace`,
      `cargo clippy --all-targets --workspace -- -D warnings`,
      `cargo fmt --all --check`, and `git diff --check`. All fixtures are isolated and
      ordinary CI must create its own prerequisites.
- [ ] Update spec/plan for concrete implementation choices, commit with Fiona trailer,
      verify clean worktree and return SHA/log evidence. Fresh task review/fix loop,
      then final whole-branch review.
- [ ] Note automatic-restart historical UI visibility in #406. Open the PR through
      `fl github pr create`, name top-level response `warnings` fields for Felix, and
      obtain current-head Ferris approval/current CI before author merge.

### Task 2 WIP checkpoint — 2026-11-09 priority preemption

Task 2 is parked at Ferris's direction for critical #408 work; this checkpoint is
not complete or green. Initial RED (`paths-task2-red.log`) demonstrated missing
absolute registered/worktree instructions and missing lifecycle warning arrays.
Partial implementation adds `hydration.rs` with config-entry resolution, Git
identity guarding, explicit file/catalog guidance and warning logging; connects
spin-up/restart/remodel, canonical reforge fallback and human CLI warnings.

The in-flight focused check (`paths-task2-focused.log`, `cargo test -p fl-hub
hydration_`) failed compilation: `lifecycle.rs` imports `canonical_reforge_prompt`
unconditionally although its compatibility helper is now `#[cfg(test)]`. No full
suite or independent Task 2 review has run. `git diff --check` passed.

Resume by fixing that import boundary, compiling, updating old relative-path test
expectations, and completing the planned path/role/malformed-layout matrix,
default-response and automatic-restart log capture tests. The new log capture hook
is present but its assertion is not yet written. Verify config/catalog ambiguity
and workspace provenance policies, final cwd ordering, external-helper lock scope,
and CLI fixtures before the full verification and independent review gates.


### Task 2 resumed — 2026-11-09

Current main was integrated in `556244ff`, preserving host-session IDs, auto-approval
permissions and #408 behavior. The known test-only import mismatch was reproduced
and corrected. New regressions exposed and corrected ambiguous workspace selection,
foreign cwd workspace borrowing, relative binding resolution against the hub cwd,
and the retained store guard across the external hydrate helper. Existing recast
coverage caught loss of catalog `custom:<slug>` identity; the original spelling is
restored. Missing-guidance responses require updated existing fixture expectations.

The test matrix covers role precedence/symlinks, exact-case registered roots,
same-repository worktrees and subdirectories, foreign and malformed Git metadata,
ambiguous config/catalog/role roots, non-Git projects, missing files, optional legacy
state, authored catalog text, response defaults and warning preservation, restart
log capture, CLI JSON/human diagnostics, and override lock scope. See the spec's
implementation notes for the precise automatic-restart coverage limitation.

Final verification passed on stable source after restoring runtime cwd hints:
`cargo test --workspace` passed 6,697 tests with zero failures and 33 ignored across
111 result groups. Strict workspace all-target Clippy, formatting and diff checks
all passed. The suite includes the private CLI/subprocess integration smoke tests;
the focused CLI lifecycle suite also passed 15/15. The focused hydration regression
set passed 29/29. Native macOS and Windows execution remains CI's responsibility.

Evidence is in `scratch/ferroloop/fiona-383-artifacts/paths-task2-final-results.json`
and its four logs. Earlier failing regressions are preserved in `paths-task2-matrix-red.log`,
`paths-task2-override-red.log`, `paths-task2-relative-red.log`, and `paths-task2-cwd-red.log`.
The first full run (6,696 passing tests, one test-only Clippy warning) is preserved
separately under `paths-task2-before-cwd-fix-*`; the final run followed the narrow
runtime-cwd fix and Clippy cleanup. The API boundary test is
`hydration_provenance_does_not_change_the_runtime_workspace_hint`.

Current main through #417 is included (`7a121ed6` integration). Source implementation
and validation are complete; fresh Task 2 and whole-branch review remain pending.
The coordinator owns publication, review, and landing.
