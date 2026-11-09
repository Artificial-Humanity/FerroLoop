# Codex Trust Preflight Implementation Plan

> **For agentic workers:** Use the repository-required `subagent-dev` skill: a fresh implementer, then the `code-review` loop and final branch review.

**Goal:** Refuse untrusted hosted Codex launches before lifecycle mutations.

**Architecture:** A read-only helper in existing `fl-mcp` Codex configuration code
serves both MCP warning policy and lifecycle refusal. Lifecycle pins cwd and
CODEX_HOME and checks independently of adapter selection.

**Tech Stack:** Rust, existing TOML parser, fixture tests.

**Spec:** `docs/superpowers/specs/2026-11-09-codex-trust-preflight-design.md`

## Global Constraints

No config writes or live launches. Config limit 1 MiB; each Git indirection file
limit 8 KiB. Keep D2b hydration outcomes and D3/D5/D6 outside this PR. Preserve
architecture guards and machine author/Fiona coauthor identity.

## Config-location clarification

Ferris selected option B on 2026-11-09: future hosted launches explicitly receive
nonempty hub CODEX_HOME, else home_override/.codex for isolated callers, else
hub HOME/.codex. Empty CODEX_HOME remains unset, matching existing MCP tests.
This intentionally makes the checked hub location the future child's location;
running agents are untouched. See the spec's rollout note. Test fixtures scope
ambient-input replacement to their own thread and never change process HOME.

## Review Focus

- An explicit worktree denial must override trusted main-root fallback.
- The child must use the cwd/config that passed preflight, including fallbacks.
- Malformed or oversized config must fail without disclosing its contents.
- Missing registry or host-adapter fallback must not skip preflight.
- Refused remodel must preserve the running source and every durable record.

### Task 1: Shared trust parser and lifecycle integration

**Files:** Modify `crates/mcp/src/vendor/codex.rs`, `vendor/mod.rs`,
`crates/cli/src/cmd/mcp.rs`, `crates/hub/src/lifecycle.rs`; use inline unit tests
and CLI black-box tests under `crates/cli/tests/` as appropriate. Add fixture
support only to affected tests. No new vendor-specific hub/wire/host source file.

**Interfaces:** Expose a read-only
`check_codex_project_trust(cwd: &Path, config_path: &Path)` result with canonical
cwd, trust key and safe actionable diagnostic information. The implementer may
choose concrete result types appropriate to existing MCP error conventions;
document the final public signature in this plan. CLI keeps plan filtering and
warning formatting, while lifecycle maps refusal to a 4xx and pins launch inputs.

Shared public signature: `check_codex_project_trust(cwd: &Path, config_path:
&Path) -> Result<CodexProjectTrust, CodexTrustError>`, reexported from
`fl_mcp::vendor`. The success carries canonical `cwd` and the matching
`trust_key`; the error displays paths, a safe reason and an exact TOML entry.

- [x] Add failing shared-parser tests for trusted/untrusted/malformed input,
      no-Git/nearest-root/worktree inheritance and explicit denial precedence,
      byte bounds, missing/unreadable/non-regular files and diagnostic redaction.
- [x] Extract parser semantics from CLI, implement bounded reads, preserve existing
      MCP warnings, and remove duplicate CLI parsing/root-walking functions.
- [x] Add failing lifecycle regressions for all three paths proving no registration,
      spawn, teardown, handoff, route or record mutation on refusal. Cover missing
      registry and adapter fallback, valid launch input pinning, malformed config,
      absent cwd and explicit invalid cwd. Tests use private fixtures only.
- [x] Implement preflight and pinning at the pre-mutation boundaries; update trusted
      fixtures for unrelated existing Codex launch tests rather than disabling checks.
- [x] Run focused MCP/hub/CLI tests, then `cargo test --workspace`,
      `cargo clippy --all-targets --workspace -- -D warnings`,
      `cargo fmt --all --check`, and `git diff --check`.
- [x] Commit with `Co-authored-by: Fiona <fiona@artificialhumanity.io>`; verify clean
      worktree and report exact source SHA, tests and any limitations.
- [ ] Fresh independent code review; fresh fixer/scoped re-review if needed.
- [ ] Final branch review, current-main integration and separate PR through
      `fl github pr create`; obtain Ferris exact-head approval and green CI before merge.
