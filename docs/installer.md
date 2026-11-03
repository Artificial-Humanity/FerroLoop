# Installer: manifest, receipt, plan and `fl install`

`fl install` plans an install of the FerroLoop products and, with `--yes`, applies the SAME plan
as a journaled transaction. `--dry-run` reads a release manifest, the install receipt if one
exists, and the paths the plan names, and writes nothing. `--yes` installs verified files from a
local `--artifact-dir`, switches `current`, writes the launcher and the receipt, and can be
resumed or rolled back after a crash. Without `--dry-run` and without `--yes` it prints the
plan and exits 3 with `pass --yes to apply (or --dry-run)`.

Code: `fl_core::install` (pure types, the planner, the transaction model in `txn.rs`),
`fl_install` (the I/O side: `FsProbe`, the transaction in `engine.rs`, the lock, the journal,
the path guard, the tree reader), `crates/cli/src/cmd/install.rs` (the command). `fl doctor`
([doctor.md](doctor.md)) reads the same receipt with the same probe.

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
journal of committed transactions (one `committed` entry per transaction; `seq` is the
transaction counter and `at_ms` is always 0, because a receipt carries no clock). It records no
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

## The install transaction (`fl install --yes`)

```
fl install --yes [--profile P] [--add X]... [--remove X]... --manifest M --artifact-dir DIR
           [--prefix PATH] [--replace PATH]... [--keep PATH]... [--lock-timeout SECONDS]
fl install --resume | --rollback [--prefix PATH]
```

**Source.** A local directory of EXTRACTED artifacts. Each artifact's `file` in the manifest is a
directory under `--artifact-dir`, and its `sha256` is the TREE DIGEST of that directory: the
SHA-256 over one line `<file sha256> <size> <path>\n` per regular file, sorted by path.
`size` is the total bytes of those files. An archive, a download and a source build are not
unpacked or fetched in this release. The built-in manifest is a source build with no artifacts,
so `--yes` needs `--manifest`.

**Layout.** Under the prefix (mutable data, config, keys and logs are never inside it):

```
releases/<version>+<sha12>/{bin,ui/dist,contrib/units,release.json}   sealed read-only
releases/.stage-<txn>/                                                 while staging
current -> releases/<version>+<sha12>      relative symlink, switched by rename(2)
receipt.json    install.lock    journal.jsonl (only during a transaction)    backups/<n>/
<bin_dir>/fl -> <prefix>/current/bin/fl    the stable launcher (a symlink)
<bin_dir>/agent-msg -> fl, <bin_dir>/agent-bus-mcp -> fl     owned aliases of the launcher
```

The launcher is a symlink into `current`, not a script: the plan already shows it, the doctor
checks it as a link, and one `rename(2)` of `current` moves the launcher, the aliases and every
reader at once. The cost is that `current_exe()` resolves through the links to the versioned
path, so a generated command never uses `current_exe()`; it names `<bin_dir>/fl`. A link target
is written with `~` expanded (a symlink cannot hold `~`); the receipt holds the real target.
Release directories and files are sealed (`0555` binaries and directories, `0444` files). The
installer creates the prefix, `releases/` and the launcher directory `0755` and `backups/` and
staging directories `0700`, whatever the umask.

**Steps.** One journal line is written and flushed BEFORE each step runs.

| Step | What it does |
|---|---|
| `begin` | `journal.jsonl` is created holding the whole intent (release, files and hashes, steps, directories it created, the previous receipt byte for byte). |
| `backup` | Each `--replace` path is moved to `backups/<n>/<role>-<name>`. |
| `stage` | The selected products' files are copied into `releases/.stage-<txn>/`, hashed while copying; `release.json` is written. |
| `verify` | The staged tree is read back and compared with the verified hashes (no missing, extra or changed file) and sealed read-only. |
| `publish` | The staging directory is renamed to `releases/<name>/`. |
| `link_current` | A temp symlink replaces `current` by `rename(2)`. |
| `write_launcher`, `write_aliases` | The launcher and aliases are placed the same way. |
| `write_receipt` | `receipt.json` is written to a temp file, flushed, renamed over the old one, and the directory is flushed. LAST. |
| `commit` | The `commit` line is written and `journal.jsonl` is removed. |

A step the transaction does not need is not in its list (the launcher already right, an
installed release, no `--replace`). The artifacts are hashed BEFORE `begin`: a mismatch, a
symlink, a special file, a name outside `[A-Za-z0-9._+@/-]`, more than 2500 files, or a size
other than the manifest's is refused with nothing created. The directory a transaction creates
(the prefix, `releases/`, the launcher directory) is recorded so a rollback removes exactly it,
and only if it is empty.

**The lock.** `install.lock` is a held `flock(2)`, with the holder's pid written in it. A second
installer waits up to `--lock-timeout` seconds (default 30; 0 does not wait), then exits 3
(`another install is running (pid N)`). A crash releases the lock with the process, so nothing
is stolen and nothing needs breaking; a pid left in the file by a process that no longer exists
is reported as a stale lock and replaced. The file is never removed after a successful install.

**Resume and rollback.** A journal left behind is an interrupted transaction. The next
`fl install --yes`, and `fl install --resume`, find it under the lock:
* a `commit` line is last: the transaction had committed; the leftover journal is removed;
* the staged data was verified (the `verify` line is there, or there was nothing to stage): the
  remaining steps run again, each idempotent, and the transaction completes;
* otherwise, or if a step cannot complete, every step is undone, newest first: the receipt
  goes back to the previous bytes (or is removed), the launcher and aliases are removed or
  pointed back, `current` goes back, the staged and published release this transaction created
  is removed, the backups are moved back, then the directories it created. An undo only touches
  what is still exactly what the transaction placed; a link somebody else put there is left.

`fl install --rollback` always undoes. A failing step (a full disk, a permission error) undoes
the transaction at once and exits 1. A crash during the undo is finished by the next run. A
journal that does not parse is refused (exit 3) and left for a person to read.

**Ownership and decisions.** The plan's conflicts are decided per path. Without a decision the
install exits 3 and lists each conflict with the flags that would resolve it.
* `--replace PATH` moves the existing file, link or directory to `<prefix>/backups/<n>/`
  (`<n>` counts up, no clock) and installs over the empty slot. The backup is owned by the
  receipt, with its hash. A release directory that `current` points to cannot be replaced.
  A conflict inside a release is decided on the release directory.
* `--keep PATH` (launcher or alias only) leaves it alone and does not own it; keeping the
  launcher also skips the aliases.
* A decision for a path that is not a conflict, or both for one path, exits 2.
* A unit file conflict is information: no unit is written in this release.

**Idempotence and versions.** The same install again exits 0 with `already installed` and
rewrites nothing. A different version installs side by side and flips `current`. Installing a
different selection of the SAME release is refused when it needs files the release lacks;
a change that needs only the receipt is a short transaction (`begin`, `write_receipt`,
`commit`). Retention is reported, never applied: `current` plus the two previous releases, the
rest listed as eligible for a later `--gc`.

**Where it may write.** The declared root is `--prefix`, else `$FL_INSTALL_ROOT`, else `$HOME`.
Every path of the install must lie inside it. The root's existing ancestors are resolved first
and the result may not be `/` or inside a system tree (`/usr`, `/etc`, `/bin`, `/lib*`, `/boot`,
`/sys`, `/proc`, `/dev`, ...). The root itself, and every component below it, must not be a
symlink the installer did not create (only `$HOME` may be a link). Nothing outside the root is
created, written or removed.

**Journal checks.** `journal.jsonl` is read back from disk, so nothing in it is trusted before it
is checked: the release name is a plain file name that extends its version, every file path
passes the in-release path rules, every created directory and link is absolute, normalised and
under the declared root (links sit in the launcher directory), backups sit under
`<prefix>/backups/`, and the layout in the journal equals the layout of this run. A journal
that fails any check is refused with exit 3 and nothing is touched. A release directory is
removed by an undo only when the journal records that this transaction renamed the staging
directory onto it (`published`), or the staging directory is gone with no undo begun; a
directory that appeared while the staging directory still exists is left alone.

**Known limit.** Paths are walked with `lstat` and then used by path (no `openat` with a
directory descriptor), so a process of the same user that races the installer can swap a
directory component for a symlink between the check and the use. The guard defends against
what is on disk when the install starts, not against a concurrent hostile process of the same
user; descriptor-relative walking is future work. Staged files are sealed and hashed through
open descriptors (`O_NOFOLLOW`, `fchmod`).

**Separate actions.** Registering services, enabling at boot, starting listeners, MCP catalogs,
the `PATH` change, credentials and state migration are listed after the install as
`not executed: <action> <subject> [not implemented in this release]`, with the commands a
service manager (`systemctl --user`, `launchctl`) WOULD run. The service managers are a trait
(`fl_install::ServiceManager`); the real ones only plan, and the tests use fakes that record
every call. No unit file is written outside a release directory.

**What this release does not do:** `--adopt`, `--gc`, `--from-git` (each exits 3 with
`not implemented in this release`), downloads, signatures, unpacking archives, upgrade
preflight, a rollback between releases, service activation, shell `PATH` edits, MCP catalog
writes, credential setup and state migration.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | A plan was printed (including a plan with conflicts: read `status`); an install, `--resume` or `--rollback` completed; the install was already there. |
| 1 | A step failed and the transaction was undone (or the undo failed too: run `--resume`); a file system error such as a read-only root. |
| 2 | The request is not well formed: unknown product or profile, a product both added and removed, a relative or malformed prefix or `$FL_INSTALL_ROOT`, an unknown flag, `--db`, `--dry-run` with `--yes`, `--resume` with `--rollback`, `--yes` with `--resume`, a decision for a path that is not a conflict, `--yes` without `--artifact-dir` when a release must be staged. |
| 3 | Refused: no `--dry-run` or `--yes` (the plan is printed first), `--adopt`, `--gc`, `--from-git`, an invalid or unreadable manifest or receipt, a source-build manifest, a forbidden prefix or one that resolves into a system tree, a symlinked prefix component, an artifact that fails its size or hash or holds a symlink, an unresolved conflict, another installer holding the lock, a corrupt journal, a removal of a required or needed product, an unsupported platform, a missing artifact. |
| 70 | Test builds only: a simulated crash returned as an error. |

## What `--dry-run` does NOT do

It creates, modifies and deletes nothing: no directory, link, unit, catalog, PATH entry,
receipt, journal or lock file. It starts no process and no service, makes no network call,
opens no store, reads no project config and no credential, and never reads `$HOME` except to
look at the paths the plan names.

## Tests

Every run is confined to a temporary root. The crash seam is `fl_install::Faults`: a `FaultPoint`
(`before` or `after` any step) makes the transaction return a simulated crash with nothing
undone; the `fl` binary reads `FL_INSTALL_FAULT` only when `fl-install` is built with the
`fault-injection` feature, which only fl-cli's tests enable; an optimized build with it is a
compile error. Release builds are `cargo build --release --locked -p fl-cli`, never `--all-targets`
or `--tests`. The tests crash a fresh install, an
upgrade and a `--replace` at every point and check, by full directory snapshots, that `--resume`,
`--rollback` and the next install each end in exactly "fully installed" or exactly the
pre-state.
