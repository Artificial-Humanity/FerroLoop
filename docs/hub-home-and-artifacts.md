# Hub home and artifact directories

Where the Hub keeps its data, where its artifact registry lives, and how an Owner moves it.

## Layout

* **`[home]`** is the directory that holds the Hub store (the parent of the store path, made
  absolute). In production that is `~/.local/share/fl`. It is not `$HOME`: the settings file is
  `$HOME/.config/fl/ferrodeck-settings.json`.
* **System artifacts directory**: `[home]/artifacts`. It holds the artifact registry
  (`registry.json`), the revision store (`versions/`) and archived copies (`managed/`).
* **Project artifacts directory**: `<system artifacts directory>/<project>`, where `<project>` is
  a lowercase slug (`[a-z0-9_-]`, 1 to 64 characters).
* `[home]/attachments` holds chat attachments. It is never an artifacts directory.

The Hub creates `[home]/artifacts`, `[home]/attachments` and the effective system directory when
it starts serving. A failure is logged and does not stop it.

The registry is session-based and has no project association. Nothing writes into a project's
directory yet, and archive writes are not routed by project. A project directory is a place the
Hub serves files from and a setting a later change can route to.

## Resolution order

| Directory | Value |
| --- | --- |
| System | `artifacts_dir` from the settings, else `[home]/artifacts` |
| Project | `project_artifacts_dirs[<project>]` from the settings, else `<effective system directory>/<project>` |

A stored value is kept only if it is an absolute path with no NUL byte and no `..` component.
Anything else is dropped and the default applies, field by field.

## Endpoints

Every route needs authentication. Every `POST`, `PATCH` and `DELETE` route is **Owner only**: a Peer names itself, so
no finer tier can be trusted, and a Peer gets `403`.

### `GET /api/v1/settings`, `PUT /api/v1/settings`

The stored settings plus three computed fields: `home_dir` (`[home]`), `default_artifacts_dir`
and `effective_artifacts_dir`. `artifacts_dir` is the override or `null`;
`project_artifacts_dirs` is the map. The `PUT` response and the `settings.updated` broadcast have
the same shape. `PUT` cannot set either directory field (`400`, unknown field).

### `POST /api/v1/settings/artifacts-dir`

```json
{ "new_path": "/data/artifacts", "migrate": false }
```

`new_path` is required. `null` or a blank string resets to the default. `migrate` defaults to
`false`; unknown fields are refused. Response `200`:

```json
{
  "old_path": "...", "new_path": "...",
  "migrated_files": 0, "migrated_bytes": 0,
  "artifacts_dir": "... or null",
  "default_artifacts_dir": "...", "effective_artifacts_dir": "..."
}
```

It broadcasts `settings.updated`. Errors:

| Status | Cause |
| --- | --- |
| `400` | `new_path` fails validation, the body is not valid, or the directory cannot be created |
| `403` | the caller is not the Owner |
| `409` | another relocation is running, the settings file is damaged, or (with `migrate`) a name already exists in the target |
| `500` | a move failed; the setting is not changed and the message says how many files had moved |

### `GET /api/v1/projects`, `POST /api/v1/projects`, `PATCH /api/v1/projects/:slug`, `DELETE /api/v1/projects/:slug`

One shape describes a project in the first three: the list, a creation and an update. There are two
kinds ([getting-started.md](getting-started.md#create-a-project-in-one-step-fl-project-create)):
a directory-backed project (`"kind": "project"`) and a virtual group (`"kind": "virtual"`), an
area with no path that names a set of agents.

```json
{ "projects": [
  { "id": "ferroloop", "slug": "ferroloop", "name": "FerroLoop", "kind": "project",
    "path": "/srv/code/ferroloop", "has_git": true, "purpose": "the platform",
    "tracker": { "kind": "github", "repo": "acme/ferroloop", "credential": "app" },
    "store_path": "/home/me/.local/share/fl/ferroloop.fls", "active_agent_count": 5,
    "artifacts_dir": null, "default_artifacts_dir": "...", "effective_artifacts_dir": "..." },
  { "id": "council", "slug": "council", "name": "Council", "kind": "virtual",
    "path": null, "has_git": false, "purpose": "the strategists", "tracker": null,
    "store_path": null, "active_agent_count": 3,
    "artifacts_dir": null, "default_artifacts_dir": null, "effective_artifacts_dir": null }
] }
```

Every field is always present; an absent value is `null`. `slug` (also `id`) never changes; `name`
is the display name. `path` and `store_path` are `null` for a group. `has_git` is true when the
root holds a `.git`, a repository with no commit included; it is read, never made. `tracker` is
`null` or `{ "kind": "github", "repo": "owner/repo", "credential": "app" | "env" }`.
`active_agent_count` counts active agents whose project or extra areas equal the slug or the
area's name, ignoring case. `artifacts_dir`, `default_artifacts_dir` and `effective_artifacts_dir`
are the project's artifacts directories (below); a group has none, so they are `null`. The list is
ordered by slug and holds the Hub's available projects (the default one is always present), every
`[[project]]` block of the Hub's config, and every virtual group.

`POST` and `PATCH` are **Owner only**: they write the Owner's config and may `git init` a path.
Both return the project in the shape above: `201` for a new one, `200` when `POST` found it
already made or `PATCH` set it.

`POST /api/v1/projects` creates a project, with the flow of `fl project create` run by the Hub:

```json
{ "name": "My Project", "slug": "my-project", "kind": "project", "path": "/srv/code/app",
  "purpose": "the app", "git_init": false, "github": "acme/app", "credential": "app" }
```

`kind` is `"project"` (the default) or `"virtual"`; `"virtual": true` is the same as
`"kind": "virtual"`. Unknown fields are refused. `slug` is optional (empty counts as absent):
without it the slug is the config's for that path, else the kebab-case of `name`, else the
directory's lowercase name. For a project, `path` is required, `git_init` defaults to `false` (Git
is not touched unless it is `true`: `git init`, and an empty initial commit when there is no
`HEAD`), `credential` defaults to `app` and needs `github`. `path` must be absolute, with no `..`
and no NUL byte; the Hub also refuses one in a system directory, under a secret's directory
(`.ssh`, `.aws` and the like), or that is, or contains, the Hub's home. For a virtual group,
`name` is required and `path`, `git_init`, `github` and `credential` are ignored. The author
recorded is the caller's name. The config is the Hub's `$XDG_CONFIG_HOME/fl/config.toml` (default
`$HOME/.config/fl/config.toml`), a project's store is `$HOME/.local/share/fl/<slug>.fls`, and the
workspace area goes into the Hub store. A group writes that area and nothing else. It broadcasts
`project.created` on the `projects` channel.

`PATCH /api/v1/projects/:slug` changes any of these, all optional:

```json
{ "name": "My Project", "purpose": "the app", "path": "/srv/code/app2",
  "github": "acme/app", "credential": "env" }
```

`name` is the display name; the slug cannot change, and a `slug` field is refused. `purpose` is
the purpose. `path` moves a directory-backed project to an existing directory, checked as `POST`
checks a path and refused when another project has it; the project keeps its store, gates and
records, which are pointed at the new root. A group has no path and refuses one. `github` sets or
replaces the tracker (`""` clears it) and `credential` applies to it (the current tracker when
`github` is absent, which needs one). A group has no tracker. The config block and the area change
together: the stores are written first, then the config in one atomic replacement, and a failure
puts the stores back. Asking for what is already so changes nothing. A project with a store but no
config block can be renamed but not moved or given a tracker. It broadcasts `project.updated`.

| Status | Cause |
| --- | --- |
| `400` | the body is not valid, nothing is asked for, or the slug, name, path, repository or credential cannot be used, or the path is somewhere the Hub refuses |
| `403` | the caller is not the Owner |
| `404` | (`PATCH`, `DELETE`) no project has that slug (`DELETE`: or no `reassign_to` project) |
| `409` | the slug is registered at another root, the root under another slug, the slug is the other kind (a project's slug for a group, or the reverse), the path is another project's, the tracker is a different one (`POST`), or the store holds another project |
| `500` | git, a store or the config file failed, or the config file is not valid TOML (the message names the file) |

`DELETE /api/v1/projects/:slug` deletes a project or a virtual group, with the flow of
`fl project delete` run by the Hub. **Owner only** (`403` before the slug is looked at). The body
is optional (empty means all defaults); unknown fields are refused:

```json
{ "reassign_to": "other-project", "decommission_agents": false,
  "delete_store": false, "delete_dir": false }
```

The order is fixed: validate everything, then reassign or decommission the active agents, then
unregister (the `[[project]]` block, written atomically, and the area), then delete the store
files, then the directory.

* **Agents.** Active agents are those whose project or extra areas equal the slug or the area's
  name, ignoring case. With some, and neither `reassign_to` nor `decommission_agents: true`, the
  answer is `409` and nothing changes:
  `{"error":"project has active agents","active_agent_count":2,"active_agents":["Ada","Bo"]}`.
  `reassign_to` moves them to that project (a project or virtual group that exists, not this
  slug; `404` for an unknown one, `409` when a Lead would meet the target's Lead).
  `decommission_agents` tears their sessions down and retires them (`POST
  /api/v1/sessions/:id/decommission`, mode `teardown`). Both together are `400`.
* **Default.** Only the `[[project]]` block and the area go; no file is deleted. The Hub lists a
  project from a store file in `$HOME/.local/share/fl`, so the slug is still listed, with a
  `null` path, until `delete_store` removes the file as well.
* **`delete_store: true`** deletes `<slug>.fls` and its `<slug>.fls.sock` and
  `<slug>.fls.migrate.lock` files. The file must be named like a store (`.fls`, or the older
  `.redb`) and be in `$HOME/.local/share/fl` or be the store the project's own config block
  names. The workspace store and another project's store are refused.
* **`delete_dir: true`** deletes the project's registered directory, after these checks, all made
  before anything changes (`400`, with the reason, when one fails): the registered path is
  absolute, has no `..`, is its own canonical path (no symbolic link in it), exists, is a
  directory and not a symbolic link, has at least three path components, and is the path the
  config, the area and the workspace store record. It is not `/`, a workspace root itself (it
  holds `.agents/memory` or `Notes`), the Hub's home directory or one above it, a workspace root
  or one above it, a directory holding another registered project, one holding or inside the
  stores or config directory, or one holding a store or config file. The home directory is the
  Hub's, else `$HOME`; with neither known the directory is not deleted. The workspace roots are
  looked for at or above the Hub's repository, at or above and up to two levels below the home
  directory, and at or above the `fl` executable; with none found anywhere the directory is not
  deleted. Every one of these checks runs again after the agents have moved and right before the
  removal. One change stays possible between that last check and the removal: another process
  that swaps an ancestor of the directory for a symbolic link in that window. `fl` holds no
  handle on the directory, so a directory that others can rename is not safe to delete this way.
* A virtual group refuses `delete_store` and `delete_dir` (`400`). An unknown slug is `404`,
  so a repeat is `404`.

A `200` answers, and broadcasts `project.deleted` on the `projects` channel:

```json
{ "deleted": "my-project", "reassigned_agents": 0, "decommissioned_agents": 0,
  "store_deleted": false, "dir_deleted": false }
```

A failure to delete the store files or the directory happens after the unregistration, which is
not undone: the answer is still `200`, the flag stays `false`, and an `error` field says why.

The lock that serializes edits of `config.toml` is held inside one process. A `fl` command and
the Hub that write the file at the same moment are not serialized, and one block can be lost; run
one writer at a time.

A virtual group has no store and no records: `GET /api/v1/records?project=<slug>` and the
per-project artifacts-directory routes below answer `404` for it.

### `GET /api/v1/projects/:id/artifacts-dir`

One project object as above. `:id` is percent-decoded, trimmed and lowercased, so `FerroLoop`
names `ferroloop`. `400` for a bad slug, `404` for a project that is not available.

### `POST /api/v1/projects/:id/artifacts-dir`

Same body, errors and `200` response as the system route, plus `"project": "<id>"`. It stores the
path in `project_artifacts_dirs`; a reset removes the key.

## Path rules

`new_path` is checked, and symlinks are resolved before every check below, so a link cannot hide
a target. It must:

* be non-empty, at most 4096 bytes, without a NUL byte, absolute, and without `.` or `..`
  components;
* not be `/`;
* not be inside or equal to `/etc /proc /sys /dev /boot /run /bin /sbin /lib /lib64 /usr
  /var/run /root /snap`;
* not have a component named `.ssh .gnupg .aws .kube .docker .azure .password-store .git .fl` or
  `secrets`, and not match the artifact preview's forbidden names (`.env*`, `*.redb`, `*.fls`,
  and the like);
* not be, or contain, `$HOME` or `[home]` (so `/`, `/home`, `$HOME` and the store directory are
  refused), nor be inside `[home]/attachments`. A path inside `[home]` otherwise is fine;
* not be inside or contain the directory it replaces (a move into itself);
* for a project, not contain the system directory or sit inside its `registry.json` or
  `versions/`.

The Hub creates the directory only after every check passed, and only when it will be used.

## Migration

With `"migrate": true`, when the old and new directories differ and the old one exists, the Hub
moves the old directory's contents to the new one:

1. If any top-level name of the old directory already exists in the new one, it answers `409`
   (listing at most five names) and moves nothing.
2. Each top-level entry moves with `rename`. If that crosses devices, the Hub copies the entry
   and removes the source. Symlinks are never followed; a copied symlink stays a symlink. A
   failed copy removes the partial destination and leaves the source.
3. `registry.json` moves last.
4. The setting changes only after the move succeeded. If a move fails, entries already moved are
   put back where possible, the setting is unchanged, and the error is `500`. An entry that
   was copied but whose source could not be fully removed stays in both directories; the error
   names it and says the moved entries were not all put back.

The whole move runs while the artifact registry lock is held, so a registry write cannot land in
the old directory half way. `migrated_files` and `migrated_bytes` count regular files, counted
before the move.

With `"migrate": false` only the setting changes. The old files stay where they are, and the Hub
reads and writes the new directory from then on, so the old registry is not seen.

## Serving

`GET /api/v1/artifacts/preview` and the other artifact routes serve files under the default and
the effective system directory (not its `registry.json*` or `versions/`) and under each project
override. The forbidden-path rules still apply, and a directory that resolves into a protected
place is not served, so an override never makes such a path readable. A project override that
contains the registry's `registry.json` or `versions/` is not served, and `.tmp-*` entries of the
system directory are skipped. A stored directory that resolves into a protected place (for
example a hand-edited `/etc`) is ignored for writes and startup too: the Hub uses the default.

## Known limits

- A request that resolved the artifacts root just before a relocation finished can still write
  one registry update to the old directory. A later migrate back may then report a clash on
  `registry.json`; move or delete the stray file and repeat.
- The check that a target name is free runs before each entry moves, but a file created in the
  target in that instant can still be replaced by the move.

