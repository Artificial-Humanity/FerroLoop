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

- [ ] Add failing layout tests for normal repository/subdirectory, linked worktree,
      no Git, standalone gitfile/submodule, relative gitdir and commondir, symlinks,
      missing target, dangling/unreadable/non-regular/oversized markers and safe errors.
- [ ] Observe RED, extract the existing parser and implement the public layout
      result. Do not add HEAD/objects/refs health validation or repository mutation.
- [ ] Make Codex trust consume the helper. Preserve canonical cwd, root-key-first
      denial precedence, no-Git direct cwd trust, optional commondir behavior,
      diagnostic redaction and config/Git byte limits (1 MiB / 8 KiB).
- [ ] Run `cargo test -p fl-mcp`, relevant hub Codex trust tests and CLI MCP black-box
      tests; run Clippy for affected targets, formatting and diff checks. Record logs.
- [ ] Commit with Fiona trailer; fresh independent task review and fix loop before
      Task 2 consumes this API.

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

- [ ] Add failing path fixtures for case-sensitive project names, cwd already in the
      project and subdirectories, config name differing from root basename, valid
      same-repository worktrees, foreign same-name repositories, malformed/ambiguous
      layout and non-Git registered projects. Assert actual selected instruction
      paths, explicit fallback warnings and no unrelated-repository instructions.
- [ ] Add failing guidance tests for project-role-before-global precedence, symlink
      resolution, missing role/project/workspace files, silent absent STATE.md,
      custom catalog guidance and preserved structural-custom safeguards.
- [ ] Implement explicit context from existing config-entry/catalog facilities and
      workspace-root resolution. Resolve before generating prompts, after the final
      launch cwd is known. Keep launch cwd and permission decisions unchanged.
      Canonical repository identities must match before selecting worktree guidance.
      Reuse existing config parsing; redact parser contents in warning diagnostics.
- [ ] Render absolute existing paths and explicit missing-guidance lines; keep
      logical slugs for registration/FerroState. Preserve authored catalog text and
      identify its workspace-relative base. Preserve the external hydrate override
      and use the same contextual renderer for canonical reforge fallback.
- [ ] Propagate warning lists through spin-up, restart and remodel without dropping
      existing remodel warnings. Add old-response deserialization tests with empty
      defaults and lifecycle assertions for all three paths.
- [ ] Log each guidance warning via the existing hub logging convention with persona
      and session id, including automatic restart callers. Add a private fixture or
      injectable log-capture regression that proves the automatic path leaves a trace.
- [ ] Print warnings in human-readable CLI output using the remodel convention;
      preserve valid JSON stdout and existing hydration/status fields. Test successful
      launches with warnings and JSON/default behavior; do not create a new command.
- [ ] Run focused tests, then `cargo test --workspace`,
      `cargo clippy --all-targets --workspace -- -D warnings`,
      `cargo fmt --all --check`, and `git diff --check`. All fixtures are isolated and
      ordinary CI must create its own prerequisites.
- [ ] Update spec/plan for concrete implementation choices, commit with Fiona trailer,
      verify clean worktree and return SHA/log evidence. Fresh task review/fix loop,
      then final whole-branch review.
- [ ] Note automatic-restart historical UI visibility in #406. Open the PR through
      `fl github pr create`, name top-level response `warnings` fields for Felix, and
      obtain current-head Ferris approval/current CI before author merge.
