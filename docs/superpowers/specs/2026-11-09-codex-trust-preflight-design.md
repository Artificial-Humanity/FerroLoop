# Codex trust preflight (Issue 383 D2a)

Ferris authorized implementation on 2026-11-09 after the source audit at
`408b2b99`. This PR prevents a hosted Codex launch from silently waiting at its
folder-trust dialog. Hydration acceptance is separate D2b work requiring its
own design review. No live launches or config writes are authorized here.

## Approach

Extract the existing MCP trust parser into `fl-mcp`'s Codex configuration module.
Both CLI MCP warnings and hub lifecycle refusal use it. Hub already depends on
`fl-mcp`; this avoids parser copies, a new dependency, or an adapter-registry
fallback that can skip the guard. Retain the existing MCP warning-only policy.

The alternative of checking only `CodexHostAdapter::prepare_launch` would miss
legacy adapter fallback and remodel planning. A new core adapter trait is
unnecessary for this bounded configuration check.

## Contract

- The helper receives explicit cwd and config path and reads no ambient env.
- Preserve nearest canonical Git-root trust keys and linked-worktree main-root
  fallback. The first existing project entry decides, including explicit denial.
  Arbitrary ancestor trust does not count. Without Git, use canonical cwd.
- Bound configuration reads to 1 MiB and Git indirection reads to 8 KiB each.
  Reject malformed, oversized, unreadable, missing or non-regular inputs without
  treating them as trusted. Detect excess bytes even when metadata understates
  size. Do not include config contents in diagnostics.
- Errors name the effective cwd and config file, plus an exact TOML project entry
  that the operator can deliberately add or correct. Malformed config diagnostics
  may include line/column but never the offending text. Never write config.
- Before registration/spawn for spin-up, before restart mutations/spawn, and in
  remodel planning before handoff/teardown, run the check for hosted Codex launches
  when runtime spawning is enabled. Non-hosted/no-spawn paths do not launch Codex.
- Canonicalize and pin valid cwd, using current directory once when unresolved;
  reject an explicit nonexistent cwd. Pass that same path to preflight and spawn.
- Resolve and pin config home: nonempty hub `CODEX_HOME` wins, otherwise
  lifecycle `home_override` selects its `.codex` directory for isolated callers,
  otherwise hub `HOME/.codex`. Empty `CODEX_HOME` is unset, consistently with
  existing MCP behavior. Resolve relative config-home paths to absolute paths.
  Put the checked directory in final launch `CODEX_HOME` after either adapter
  path; do not change global HOME. MCP retains ambient CODEX_HOME/HOME precedence.
- Refusal is a typed lifecycle 4xx and CLI nonzero, before durable state changes.
  Neither absent host adapter registry nor legacy launch fallback bypasses it.
- Preserve other vendors' behavior and existing permission/ownership checks.

## Config-location decision (2026-11-09, Ferris)

Option B explicitly makes **future hosted Codex launches** use the hub-selected
config location above. Previously native children inherited the host daemon's
HOME and neither adapter explicitly passed CODEX_HOME. Running agents remain
untouched. On ai-lab-0 the hub and fl-host user services run as the same user with
the same HOME, so their default effective location is unchanged; Jarvis verifies
that HOME agreement at rollout. An explicit hub CODEX_HOME now controls future
hosted launches too. No host protocol query or environment discovery is added.

## Verification

Fixture tests cover trusted/untrusted and malformed config; nearest-root and
worktree fallback/denial precedence; bounds and redacted diagnostics; exact
child cwd/config; and unchanged registry, hosted record, routes and handoff on
refused spin-up, restart and remodel. Existing MCP warning tests must still pass.
Use fake runtimes or private host fixtures only. Run the repository verification
trio and independent review before a separate D2a PR.
