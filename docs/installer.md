# Installer: manifest, receipt, plan and `fl install --dry-run`

`fl install` plans an install of the FerroLoop products. Only `--dry-run` exists. It reads a
release manifest, the install receipt if one exists, and the paths the plan names, and it
writes nothing. Without `--dry-run` it exits 3 with
`installing is not implemented in this release; use --dry-run`.

Code: `fl_core::install` (pure types, the planner and the read-only probe `FsProbe`),
`crates/cli/src/cmd/install.rs` (the command). `fl doctor` ([doctor.md](doctor.md)) reads the same
receipt with the same probe.

## Products and profiles

| Product | Dependencies (required) | Optional modules |
|---|---|---|
| `ferroloop` | `ferrostore` | none. Required in every install. |
| `ferrostore` | none | none. Hidden default dependency: never offered, always present. |
| `ferrowire` | `ferrostore` | none |
| `ferrostate` | `ferrostore` | none |
| `ferrogate` | none | none |
| `ferrotrack` | `ferrostore` | none |
| `ferrowatch` | `ferrostore` | none |
| `ferrodeck` | `ferrostore`, `ferrowire` | `ferrogate`, `ferrowatch`, `ferrotrack` |
| `ferrostep` | `ferroloop` | none |

| Profile | Products |
|---|---|
| `minimal` | `ferroloop` (+ hidden `ferrostore`) |
| `headless` | minimal + `ferrowire`, `ferrostate`, `ferrogate`, `ferrowatch`, `ferrotrack` |
| `full` | every product |
| `lab` | a superset of `full`: the same products, plus the lab-only actions (`ferrostore@.service`) |

The product ids are a closed set in code (`fl_core::install::Product`). A manifest that names
any other id is rejected. `.fl/architecture.toml` stays the architecture model: each product
only names the components that implement it, and the test `install_manifest_matches_the_code`
checks those names, and the declared dependencies, against the model.

## The three schemas

Each is JSON with an explicit `schema_version` (currently `1`). A newer version is refused
before the rest of the document is read.

**`release-manifest.json`** (`ReleaseManifest`): `release` (version, kind `release` or
`source_build`, commit, toolchain), `compat` (protocol major, store format range), `artifacts`
(id, target os/arch, archive path, sha256, size, provided products), `products` (dependencies,
supported targets, files, aliases, actions), `profiles`. Unknown fields are rejected.
`validate()` rejects: an unsupported schema version, a release version that is not a plain name
(letters, digits, `.` and `-`, up to 64, never `.`, `..` or a leading `-`: the receipt refuses the
same names), unknown products, duplicate products, a
missing `ferroloop`/`ferrostore`, a dependency on an undeclared product, a dependency cycle, a
product that is both required and optional, a `ferroloop` that does not require `ferrostore`,
duplicate or ambiguous artifacts, a hash that is not 64 lowercase hex digits, a size outside
`1..=2 GiB`, an archive or in-release path that is absolute, contains `..`, a NUL, a
backslash, an empty segment, or is longer or deeper than the bound, too many products,
artifacts, files, aliases, actions or profiles, a signed release with no commit or no artifacts,
a source-build manifest with artifacts, a profile without `ferroloop`, a `lab` that is not a
superset of `full`, a `by_profile` approval on `init_credentials` or `migrate_state`, and a
service unit the product does not ship. A manifest larger than 1 MiB is refused unread.

The built-in manifest (`default_manifest()`) is a `source_build` manifest with no artifacts.
`--manifest PATH` replaces it.

**`install-receipt.json`** (`InstallReceipt`): what an install owns: the release, the profile,
the explicit product selection, the layout, artifact hashes, owned paths (file hash or symlink
target), aliases, units (hash and release), catalog entries and config blocks (hashes), and a
journal of the install transaction (types only; a later phase writes it). It records no
credential: no field can hold one, and `deny_unknown_fields` refuses a document that carries
any other field.

**The plan** (`InstallPlan`, printed by `--json`): `schema_version`, release, compat, host,
profile, management, status, layout, products, not-installed, module routes, artifacts,
paths, actions, conflicts, unmanaged paths, notes. It has no timestamp and no random value.

## Plan semantics

1. Order of refusal. The CLI first checks the request and the layout (including that `HOME`
   exists when the default prefix is used: without it the prefix cannot be located and the
   command exits 3, pass `--prefix`), reads the manifest, and reads the install receipt. Then
   the planner refuses, in order: `--adopt`, an invalid manifest, a malformed selection, a
   removal that breaks the selection, an unsupported platform or architecture, a missing
   artifact. Only after all of those does it look at the paths of the plan. A bad receipt and
   an unsupported platform are both exit 3.
2. **Selection.** The base is `--profile`, else the receipt's recorded selection, else
   `minimal`. `--add` and `--remove` adjust it. Only the real dependency closure is added, and
   each product that was not picked says why (`included because ferrodeck needs ferrowire`).
   A removal that a remaining product still needs is refused.
3. **Not installed.** Every product outside the selection is listed with the CLI answer
   (`<product> is not installed (fl install --add <product>)`, exit 3) and the hub answer
   (`404 module_not_installed`). An optional module of a selected consumer that is not selected
   is also listed as a module route.
4. **Paths.** Under the prefix: `releases/<version>+<sha12>/` (`+unstamped` when the manifest
   has no commit), `current` (symlink to the release), `receipt.json`, `install.lock`, the
   launcher `fl` and the aliases `agent-msg` and `agent-bus-mcp` (symlinks in the launcher
   directory). Each path is marked `create`, `keep`, `replace`, `repoint`, `exists`, `update`
   or `conflict`.
5. **Layout.** Default: `~/.local/share/fl`, launcher in `~/.local/bin`, units in
   `~/.config/systemd/user`, shown with `~` unexpanded. `$FL_INSTALL_ROOT=R` puts all three
   under `R` (`R/.local/share/fl`, `R/.local/bin`, `R/.config/systemd/user`). `--prefix P`
   puts the install in `P`, the launcher in `P/bin`, units in `P/units`, and wins over
   `$FL_INSTALL_ROOT`. A prefix must be absolute, free of `.`/`..`, and not `/`, `/usr`,
   `/etc`, `/bin`, `/sbin`, `/lib*`, `/boot`, `/sys`, `/proc`, `/dev`, a bare `/home`, `/var`,
   `/opt` or `/tmp`, or a home directory itself: installs are per-user, never system-wide.
6. **Actions.** Creating or importing MCP catalogs, changing PATH, registering services,
   enabling start at boot, starting listeners, initializing credentials and migrating state are
   separate planned actions. A plain install performs none of them. Each is marked
   `by_profile` (the selected profile approves it: catalog entries and service registration
   in the profiles the manifest names) or `requires_approval`. Enabling at boot, starting
   listeners, changing PATH, initializing credentials and migrating state are always
   `requires_approval`. Service actions apply on Linux only. The plan never touches
   `.gitignore`, trust settings or account settings.
7. **Unmanaged vs managed.** With no receipt, the install is unmanaged: whatever exists at a
   planned path is reported, claims no ownership, and is a conflict. `--adopt` exits 3
   (`--adopt is not implemented in this release`). A receipt that exists but cannot be read, is
   a symlink, or is invalid is refused (exit 3), never treated as unmanaged.
8. **Conflicts.** Computed from the receipt and a read-only probe (`lstat`, `readlink`,
   read). A path the receipt does not own (`foreign`), an owned path whose content or link
   target changed (`edited`), an unowned symlink (`symlink`), a service unit without the
   `# generated by fl install` marker (`unmarked_unit`), a file where a directory belongs
   (`type_mismatch`), or a path that cannot be inspected (`unreadable`) blocks the plan:
   `status` is `needs_decision` and nothing is overwritten. Existing shared directories
   (`prefix`, `releases`) are not conflicts. A prefix or `releases` directory that is itself a
   symlink (for example `~/.local/share/fl -> /data/fl`) is a `symlink` conflict whose detail
   names the link target; the install does not follow it. Pass the real directory as
   `--prefix`, or decide about the link.
   A repeated install with an unchanged receipt reports no conflict: files, directories and
   links the receipt owns are `keep`, but the receipt is `update`, `install.lock` is `create`,
   and a unit written by an older release is `replace`.
9. **Unit marker.** A unit counts as the installer's only if one of its first 4 KiB of lines
   STARTS with `# generated by fl install <release>`. A foreign file that merely mentions the
   text does not.
10. **Bounds.** The probe hashes at most 256 MiB per file and opens files without blocking and
    without following a symlink; a larger file, or one that is not a regular file when opened,
    is `unreadable` or not a file, never read in full. Names in a manifest or receipt (aliases,
    units, catalog entries, artifact ids) are `[A-Za-z0-9._+@-]`, at most 80 bytes, never `.`
    or `..`, never starting with `-`.
11. **Actions left out.** An action the manifest does not plan in the selected profile is
    listed as a note (`action X is not planned in profile Y`), never dropped silently.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | A plan was printed (including a plan with conflicts: read `status`). |
| 2 | The request is not well formed: unknown product or profile, a product both added and removed, a relative or malformed prefix or `$FL_INSTALL_ROOT`, an unknown flag, `--db`. |
| 3 | The plan is refused: no `--dry-run`, `--adopt`, an invalid or unreadable manifest or receipt, a forbidden prefix, a removal of a required or needed product, an unsupported platform, a missing artifact. |

## What dry-run does NOT do

It creates, modifies and deletes nothing: no directory, link, unit, catalog, PATH entry,
receipt or lock file. It starts no process and no service, makes no network call, opens no
store, reads no project config and no credential, and never reads `$HOME` except to look at
the paths the plan names. It does not download, verify a signature, build from source, adopt,
upgrade, roll back, garbage-collect or uninstall; those are later phases.
