# Subdirectory project runtime cwd and reforge freshness

Issue: #427. Lead ruling: Ferris, FerroWire 01a1bf49-fffd-7397-8d59-97efddf68681, 2026-11-09.

A project bound to `workspace/Sonora/github` starts in `workspace/Sonora` so its router and sibling repositories remain available. Keep registered-root guidance. Resolve named bindings explicitly; do not rely on a matching workspace-directory scan. Stored `Project` records contain only ID and root, so named configuration entries supply the name without a schema change.

Hydration accepts a canonical, non-Git cwd containing the registered root only when cwd is a direct child of the established workspace. Workspace ancestors, unrelated siblings, foreign repositories, malformed Git metadata and symlink escapes must not gain trust. A sibling subproject in the same worktree may map silently to the corresponding bound subproject under the existing repository-identity rule.

Keep explicit cwd overrides and roots outside the workspace. Keep catalog/workspace fallback for unbound projects. Invalid or ambiguous named bindings must not select unrelated guidance or a cwd by ordering.

Reforge should avoid optional hosted/project metadata reads when an external hydration prompt succeeds. If that helper fails or returns blank, read fresh registration, role, project and hosted metadata under one store lock, with authority checks on that same snapshot. External helper calls and filesystem rendering remain outside the lock. Recast and catalog-role guidance continue to bypass opaque external prompts; authority errors remain hard errors, optional hosted lookup failures stay sanitized warnings.

Alternatives: moving runtime cwd to the bound Git child skips the router and contradicts the Lead ruling; accepting arbitrary ancestors broadens trust. The selected narrow umbrella rule preserves both runtime layout and guidance provenance.

Verification uses isolated Sonora-shaped fixtures and no live session restarts or deployment.
