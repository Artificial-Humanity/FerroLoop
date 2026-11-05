# FerroDeck Settings → Projects tab (Issue #310, Phase 1 UI)

## Goal

Let an operator create, inspect, edit and delete Projects and Virtual Groups from FerroDeck.
Today `GET /api/v1/projects` returns only artifact-directory info, the project list comes from a
hard-coded canonical list, and a new project needs the CLI. Feira builds the backend
(`/api/v1/projects`: `GET`, `POST`, `PATCH`, `DELETE`). This record is the UI.

## API contract

Feira builds the endpoints on `feira/project-init-and-workspace-agents-310`. This is the contract
the Lead shared on 2026-11-05. It is isolated in `ui/src/types/api.ts` and `HubApiClient`, so a
change touches only those two files and the tab. **Verified against Feira's branch
`feira/project-init-and-workspace-agents-310` at f1a7fb1 (2026-11-05); re-check once it is
merged.**

```ts
type ProjectKind = "project" | "virtual";

interface ProjectSummary {            // GET list entry; POST and PATCH response body
  slug: string;
  id: string;                         // equal to slug
  artifacts_dir: string | null;       // all three are null for a virtual group
  default_artifacts_dir: string | null;
  effective_artifacts_dir: string | null;
  name: string;
  kind: ProjectKind;
  path: string | null;                // null for a virtual group
  has_git: boolean;
  purpose: string | null;
  tracker: { kind: "github"; repo: string; credential: "app" | "env" } | null;
  store_path: string | null;
  active_agent_count: number;
}

interface CreateProjectRequest {
  name: string; slug?: string; kind: ProjectKind;
  path?: string; purpose?: string; git_init?: boolean;
  github?: string; credential?: "app" | "env";
}
interface UpdateProjectRequest {
  name?: string; purpose?: string; path?: string;
  github?: string;   // "" clears the tracker; never send `credential` with it (JSON null reads as omitted)
  credential?: "app" | "env";
}
interface DeleteProjectRequest {
  reassign_to?: string | null; decommission_agents?: boolean;
  delete_store?: boolean; delete_dir?: boolean;
}
interface DeleteProjectResponse {
  deleted: string; reassigned_agents: number; decommissioned_agents: number;
  store_deleted: boolean; dir_deleted: boolean;
  error?: string;   // set only when the store files or directory could not be deleted AFTER unregistering
}
// DELETE answers 409 { error, active_agent_count, active_agents: string[] } when active agents
// exist and neither reassign_to nor decommission_agents is set.
```

* The tab shows the 409 as an inline error that names the active agents, and keeps the dialog
  open on the agent-guard section.
* **Artifact directories.** Every `ProjectSummary` carries `id` (equal to `slug`),
  `artifacts_dir`, `default_artifacts_dir` and `effective_artifacts_dir` (Lead, 2026-11-05), so
  one `GET /api/v1/projects` feeds both the cards and the existing artifacts-directory flow.
  The UI reads the artifact information from the project list entries when they carry it
  (including the create and edit replies, which are merged in at once), and otherwise from the
  separate artifacts-dir list fetched when Settings opens. There is no per-entry
  `GET /api/v1/projects/:id/artifacts-dir` fallback. The create form has no
  artifacts-directory field; the card sets it after creation. A virtual group has no artifacts
  directory (the hub sends explicit `null` values), so its card has no artifacts row.
* The path is editable on `PATCH`. Changing it is a Directory Project action only, and the edit
  form warns that the registered directory changes while files stay where they are.

## Behavior

* **Tab.** `Projects` (`settings-tab-option-projects`) follows `Agents` in the selector and the
  tab strip. It is hidden in project-scoped Settings (`initialProject` set), like System.
* **List.** One card per entry: name, slug badge, kind badge (`Project` or `Virtual Group`),
  `Git` badge when `has_git`, purpose, directory path (`Workspace Root` for a virtual group),
  GitHub tracker, store path (`store_path`, when the project has one), artifacts directory with
  the existing change flow (not for a virtual group), active agent count, and the
  actions `Edit`, `View Agents`, `Delete`.
* **Artifacts directory.** The per-project row moves from the Agents tab into each card. The
  change-directory modal and its handlers are reused, not rewritten. The system-level row stays
  in System.
  Project-scoped Settings (`initialProject`) keeps the artifacts-directory row in its summary
  card, because that view has no Projects tab.
* **Create form.** A modal with a kind selector. `Directory Project`: name and auto-derived slug,
  directory path (required), purpose, `Initialize Git repository (git init)` (default off), GitHub
  tracker and credential (`app` or `env`). `Virtual Group / Scope`: name and slug and purpose only. The
  hidden fields are not sent. Validation is client-side first; the server error text is shown
  in the form.
* **Delete dialog.** When `active_agent_count > 0` the operator must pick one of: reassign the
  agents to another entry, or decommission them first. The confirm button stays disabled until a
  choice is made. For a directory project two checkboxes, both off: delete the FerroStore
  database (`<slug>.fls`) and delete the project directory (`<path>`, with a warning badge when
  checked). The dialog says that unchecked, delete only unregisters the entry. When the reply carries
  `error`, the project is already unregistered but cleanup failed: the tab shows a warning
  (`role="alert"`) with the server text and which parts were deleted (`store_deleted`,
  `dir_deleted`).
* **View Agents.** Switches to the Agents tab with the project filter set to that entry.
* **Switcher, roster, Spin-Up.** After a create, edit or delete the shared project list refreshes,
  so the Project switcher, the roster groups and the Spin-Up project dropdown include virtual
  groups and drop deleted entries without a reload.

## Non-goals

No backend work. No change to the phone or tablet shells beyond what the shared Settings modal
gives them. No editing of kind or slug. No bulk operations.

## Testing

Vitest with a mocked `HubApiClient`. Tab visibility and order; list rendering for a project, a git
project and a virtual group; create form field sets per kind and the exact request body; slug
derivation; validation errors; delete guard with and without active agents and the exact request
body for each choice; file-deletion checkboxes default off; View Agents navigation; list refresh
propagation; an older hub without the new fields.
