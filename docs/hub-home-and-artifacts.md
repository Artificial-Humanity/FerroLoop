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

Every route needs authentication. Both `POST` routes are **Owner only**: a Peer names itself, so
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

### `GET /api/v1/projects`

```json
{ "projects": [ { "id": "ferroloop", "artifacts_dir": null,
                  "default_artifacts_dir": "...", "effective_artifacts_dir": "..." } ] }
```

The projects are the Hub's available projects (the default one is always present).

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
   put back where possible, the setting is unchanged, and the error is `500`.

The whole move runs while the artifact registry lock is held, so a registry write cannot land in
the old directory half way. `migrated_files` and `migrated_bytes` count regular files, counted
before the move.

With `"migrate": false` only the setting changes. The old files stay where they are, and the Hub
reads and writes the new directory from then on, so the old registry is not seen.

## Serving

`GET /api/v1/artifacts/preview` and the other artifact routes serve files under the default and
the effective system directory (not its `registry.json*` or `versions/`) and under each project
override. The forbidden-path rules still apply, and a directory that resolves into a protected
place is not served, so an override never makes such a path readable.
