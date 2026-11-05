# FerroDeck Settings → Projects tab – implementation plan (Issue #310, Phase 1 UI)

Spec: [2026-11-05-ferrodeck-settings-projects-tab-design.md](../specs/2026-11-05-ferrodeck-settings-projects-tab-design.md).
All work is under `ui/`. Verification for every task: `cd ui && npx tsc --noEmit && npm test`.
The API contract in the spec comes from the Lead (2026-11-05); confirm it against Feira's merged handlers.

## Task 1 – Types and client

1. `ui/src/types/api.ts`: add `ProjectKind`, `ProjectSummary`, `CreateProjectRequest`,
   `UpdateProjectRequest`, `DeleteProjectRequest`, `DeleteProjectResponse` and a typed 409 error
   carrying `active_agent_count` and `active_agents`. Keep `ProjectsListResponse` working for
   existing callers.
2. `ui/src/api/client.ts`: `listProjects()` (returns `ProjectSummary[]`; tolerates an old hub by mapping a legacy
   `{id}` entry to `slug: id`, `kind: "project"`, `active_agent_count: 0`), `createProject`,
   `updateProject(slug, body)`, `deleteProject(slug, body)`. Reuse the existing request helper and
   error handling. `listProjectsArtifactsDirs` keeps its current behavior.
3. Tests: request method, URL, body and error mapping for each call; mapping of an old
   response; slug is URL-encoded.

## Task 2 – Tab shell, list and View Agents

1. New `ui/src/components/settings/ProjectsSettingsTab.tsx` (do not grow `SettingsModal.tsx`).
   Props: `apiClient`, `agents` (for counts fallback), `onViewAgents(slug)`, `onProjectsChanged()`,
   `artifactsDir` helpers.
2. `SettingsModal.tsx`: add `"projects"` to `SettingsTabId`; add the option
   (`settings-tab-option-projects`) and the strip button after Agents, hidden when
   `initialProject` is set; render the tab body. `onViewAgents` sets the project filter and the
   active tab to `agents`.
3. Cards per the spec. Move the per-project artifacts-directory row from the Agents tab
   (`settings-project-artifacts-dir-row`) into the card and reuse `openProjectArtifactsDirModal`.
   Keep the test ids so existing tests keep working.
4. Tests: tab order and visibility, hidden under `initialProject`, card content for a project, a
   git project and a virtual group, View Agents, artifacts-dir row now in the card.

## Task 3 – Create form

1. `ui/src/components/settings/NewProjectModal.tsx`: kind selector, per-kind field sets, slug
   auto-derived from the name (lowercase, `a-z0-9-`, editable until touched), validation,
   server error display, busy state, Escape and focus return.
2. Wire the `+ Create Project / Group` button in the tab; on success call `onProjectsChanged()`.
3. Tests: field sets per kind, exact request body for both kinds (hidden fields absent), slug
   derivation, required path, `git_init` default off, server error shown.

## Task 4 – Edit and delete

1. Edit modal (name, purpose, GitHub tracker) calling `updateProject`.
2. Delete dialog per the spec: agent guard (reassign dropdown excludes the entry itself, or
   decommission), file-deletion checkboxes default off, warning badge for `delete_dir`, confirm
   disabled until the guard is satisfied.
3. Tests: guard shown only with active agents; exact request body for reassign, decommission,
   store only, dir only; checkboxes default off; cancel sends nothing.

## Task 5 – One project source for the whole app

1. `ui/src/hooks/useProjects.ts`: loads `listProjects()`, falls back to the current hard-coded
   list when the endpoint is unavailable, exposes `projects`, `refresh()`.
2. `App.tsx` owns it and passes `projects` down. Replace the three `CANONICAL_PROJECTS` copies
   (`RosterSidebar`, `SettingsModal`, `AgentSettingsModal`) and feed the Project switcher and
   the Spin-Up project dropdown from it. A virtual group is its own roster group.
3. `onProjectsChanged` from Settings calls `refresh()`; a deleted entry that is the active
   filter resets the filter to all.
4. Tests: virtual group appears in the switcher, the roster and the Spin-Up dropdown after a
   create; a deleted entry disappears; endpoint failure falls back to the old list.

## Final review

Whole-branch `code-review` on `origin/main..HEAD` against the spec. Confirm the contract against
Feira's merged handlers, then open the pull request and ask
Ferris for review.
