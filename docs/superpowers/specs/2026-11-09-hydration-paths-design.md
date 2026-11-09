# Hydration paths and missing guidance (Issue 383 D6+D5)

Status: approved by Ferris on 2026-11-09, including the automatic-restart logging
addition below. D2b merged in #407; implementation may proceed.

## Problem and outcome

Generated startup prompts interpolate a logical project slug into relative paths.
An agent already running in `Kaggle-Gemma4/` can consequently be told to read
`kaggle-gemma4/AGENTS.md` beneath that cwd, and `.agents` is resolved against the
wrong base. Built-in roles also assume a role file exists. The local
`journalist.md` now exists as a symlink; that does not justify a product-specific
`journalist -> scribe` alias.

Render absolute paths from resolved filesystem context, preserving actual case.
When guidance is absent, continue with an explicit missing-guidance prompt line
and operator warnings. Keep FerroState primary and missing legacy STATE.md silent.

## Approaches and selected policy

Ferris selected resolved paths with warnings, rather than a new launch refusal.
Changing the child cwd to make existing relative strings work would lose requested
worktree context and would not fix project-name case. The selected approach keeps
runtime cwd and permission behavior unchanged and supplies context to the prompt.

For project instructions, Ferris selected worktree-first resolution with a
repository identity guard, then the registered project root. Workspace `.agents`
and `Notes` remain anchored to the umbrella workspace. No shell or Git command is
run to resolve these paths.

## Filesystem context

Resolve context before building the startup prompt on spin-up, restart and remodel.
Keep the logical project slug for identity, registration and FerroState scopes;
never use that slug as though it were a filesystem path.

- Resolve the registered project root from the existing project binding/catalog
  facilities. A configured project name may differ from its root basename; reuse
  the existing config-entry parser rather than introduce another TOML reader.
  Retain the existing case-aware workspace-directory fallback only when no binding
  exists. Canonicalize and deduplicate candidates; an ambiguous binding produces a
  warning and no guessed instruction root.
- Resolve the umbrella workspace from supplied workspace context or the existing
  workspace-root finder applied to known roots. Do not treat an arbitrary parent
  directory as a proven workspace. No recursive filesystem search.
- Find the nearest `.git` marker above the effective cwd. Use that working-tree
  root only when its canonical repository identity matches the registered root:
  canonical `commondir` when present, otherwise canonical git dir. This covers
  normal checkouts, linked worktrees and standalone gitfiles.
- An unrelated or ambiguous cwd repository falls back to the registered root and
  adds `cwd is outside project <name>`. Missing/malformed repository metadata cannot
  authorize a worktree override. A non-Git registered project remains usable from
  its registered root; it does not acquire an unrelated worktree override.
- If no project or workspace root can be established, report unavailable guidance
  explicitly. Do not construct actionable read instructions for invented paths.

The #405 gitfile/commondir parser is shared, not copied. Extract its generic bounded
layout resolution into `fl-mcp`'s public project-path utilities, consumed by both
Codex trust checking and the hub. `fl-mcp` currently has no dependency on `fl-core`;
this avoids adding one or placing a vendor-specific resolver in the hub. Keep the
existing Codex trust precedence, no-Git behavior, diagnostics, 1 MiB config cap and
8 KiB indirection caps unchanged. This extraction is a separate reviewed task.

Task 1 preserves the original marker-shape boundary: a directory `.git` marker
uses its own canonical path as identity and does not read `commondir`. A gitfile
resolves its canonical target and optional `commondir`; no HEAD/objects/refs health
checks are added. The `main_checkout_root` field is the parent of that resolved
`commondir`, preserving Codex's fallback convention, not independent proof of a
checkout. Task 2 must use this API conservatively for repository identity and warn
and fall back for unsupported or ambiguous layouts.

## Guidance selection and rendering

- Resolve a built-in or legacy file role in this order:
  `.agents/roles/<Project>/<role>.md`, then `.agents/roles/<role>.md`.
  Match the project's existing directory with preserved filesystem case, follow
  symlinks, and emit the absolute resolved file path. Preserve the existing shared
  UI-developer role mapping; add no journalist alias.
- If neither role candidate exists, emit an explicit missing-guidance line and a
  warning such as `role file not found: looked in <absolute paths>`.
- Retain current catalog role templates and structural-custom authority rules.
  Catalog-guided roles do not acquire a fictitious required role file. Keep authored
  guidance unchanged and state its workspace-relative base explicitly.
- Resolve workspace AGENTS.md and project AGENTS.md, WORKFLOW.md and PERSONA.md
  against their respective roots. Emit read instructions only for present regular
  files. Missing required guidance emits prompt text and a warning; no new refusal.
- Include the optional legacy `Notes/<Project>/STATE.md` reference only when found.
  Its absence emits no warning. FerroState commands and precedence remain intact.
- The introductory residence line names the resolved context instead of instructing
  the agent to descend into another slug-named directory from its current cwd.

Use one internal result carrying prompt text and warnings. Build role guidance and
path context explicitly rather than replace arbitrary text inside authored catalog
templates. Keep the canonical reset/reforge fallback consistent with this renderer;
preserve the existing external hydrate override and recast safeguards.

## Public response and CLI contract

Add `#[serde(default)] warnings: Vec<String>` to `SpinUpResponse` and
`RestartResponse`; apply the same default to the existing `RemodelResponse.warnings`
and append guidance warnings without losing existing remodel warnings. Empty means
no warning. JSON field names are `warnings` on each top-level response.

Human-readable CLI output prints warnings using the existing remodel convention.
JSON output carries the list without contaminating JSON stdout. No new restart
command is introduced if a direct CLI restart command does not exist.

Ferris assigned FerroDeck warning display to Felix, alongside hydration_status.
This backend PR names the response fields for that follow-up and does not change UI.
Warnings are response data; this does not add a durable warnings field to hosted
records or alter hydration_status/dispatch evidence.

Log every guidance warning through the hub's existing logging convention with the
persona and session id. Boot reconciliation and rehydration after a host restart
must therefore leave a trace even when no human receives the response. Do not add
a persistent warnings field. Record the remaining historical UI visibility gap in
the existing follow-up #406.

## Verification and boundaries

Tests cover actual-case project directories and a child cwd already in the project;
project-specific/global role precedence; symlinked journalist guidance; missing role,
workspace and project files; silent absent legacy state; custom catalog guidance;
worktree/subdirectory instruction selection; foreign-repository fallback; ambiguous
and malformed git metadata; config names differing from root basenames; and legacy
response deserialization with empty warnings. Check all three lifecycle paths and
CLI warning/JSON behavior with private fixtures.

Run the full Rust verification trio and fresh independent task/branch reviews.
No live agent launches, service changes, config writes, file creation to conceal
missing guidance, new vendor behavior, or hard-coded role aliases.


## Implementation notes — 2026-11-09

Configuration bindings take precedence over catalog roots. A configured root must
be absolute and resolve to a directory; relative roots have no defined base and
produce an unavailable-root warning instead of borrowing the hub process cwd.
Canonical duplicates collapse to one root. Lifecycle runtime cwd hints remain
unchanged; the hydration boundary separately proves workspace markers before
using those hints for guidance. Without explicit workspace context,
workspace discovery uses registered roots and requires one canonical workspace.
A bound project never borrows workspace guidance from an unrelated cwd. Conflicting
project or workspace roots remain unresolved with warnings. Case-insensitive role
directory collisions fall back to global role guidance with a warning.

The renderer uses the selected project checkout for its three project files and
the umbrella workspace for role files and legacy notes. Catalog roles retain their
`custom:<slug>` registration spelling and authored text. Legacy unknown file roles
retain their original spelling. The external hydrate helper runs after releasing
the hub store guard; canonical fallback reopens the store only when needed.

Warnings are logged at the spin-up/restart/reforge lifecycle boundary, independently
of whether a caller consumes response warnings. The restart regression captures each
warning with persona and session identity, and checks that records do not persist
warnings. Current Rust callers of `restart_hosted_session` consist of the owner HTTP
handler; `boot_reconcile` sends a fixed host-restart notice and does not generate
hydration guidance. Thus this is boundary-level logging coverage, not an end-to-end
automatic boot-reconciliation hydration test. No new replay or automatic restart
behavior is introduced. Historical UI visibility is already tracked in
[#406](https://github.com/Artificial-Humanity/FerroLoop/issues/406#issuecomment-6731954317).
