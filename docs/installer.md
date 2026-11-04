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

Each is JSON with an explicit `schema_version` (the manifest and the plan are `1`; the receipt is
`1`, or `2` when its release carries a `build` block, below). A version a reader does not know is
refused before the rest of the document is read.

**`release-manifest.json`** (`ReleaseManifest`): `release` (version, kind `release` or
`source_build`, commit, toolchain, and for a source build of a commit `build`: the provenance
record below), `compat` (protocol major, store format range), `artifacts`
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
a source-build manifest with artifacts and no `build` record (a source build of a commit names
its provenance and lists at most the ONE artifact it built; it also names its commit), a signed
release with a `build` record, a `build` record whose toolchain lines are empty, longer than 200
bytes or hold a control character, a profile without `ferroloop`, a `lab` that is not a
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

**Receipt schema 1 and 2.** A receipt written for a signed release (or by any earlier installer)
is `schema_version` 1 and has no `build` key. A receipt whose release is a source build of a
commit is `schema_version` 2 and carries `release.build`: `reviewed` (was the commit an ancestor of
`origin/main`), `rustc` and `cargo` (their `-V` first lines), `build_id` (`<sha12>-<digest12>`) and
`build_time_unix` (the commit time). The two are tied together: version 1 with the block, version
2 without it, a block on a `release`-kind receipt or on one without a commit, and a block with
unbounded text are all refused. A reader that knows only version 1 answers
`unsupported receipt schema_version 2` instead of a confusing unknown-field error, so the field
is explicit; this release reads both. `fl doctor` reads both: `G7.receipt-integrity` passes a
valid receipt and WARNS for a source build with `reviewed: false`.

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
`size` is the total bytes of those files. An archive and a download are not
unpacked or fetched in this release. The built-in manifest is a source build with no artifacts,
so `--yes` needs `--manifest` or `--from-git` (which builds its own artifact).

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
The conflict check an installer makes before it takes the lock can land in the middle of
another installer's transaction (its links made, its receipt not yet written). So when a lock
file exists, a conflict seen there is decided again under the lock: a second installer waits up
to `--lock-timeout` for the first, and if a long-lived installer holds the lock it exits 3
(`another install is running (pid N)`) instead of reporting the conflict. With no lock file no
other installer can be running, and a conflict still exits 3 at once with the planner's text
before anything is created. If the conflict is still there under the lock, the answer is the
same exit 3 and the same text.

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
passes the in-release path rules, every link is absolute, normalised, under the declared root
and in the launcher directory, backups sit under `<prefix>/backups/`, and the layout in the
journal equals the layout of this run. A journal that fails any check is refused with exit 3
and nothing is touched.

**Directories the install made.** The journal records every directory the transaction created,
the missing ancestors of the prefix included (a fresh `--prefix /new/dir/fl` makes `/new` and
`/new/dir`, which lie above the declared root). One rule applies when it is written, checked
and undone: an entry is a plain absolute path (no `.` or `..`) on the way to, or inside, the
prefix or the launcher directory, and never `/`, a system directory, the home directory or
one above it. An undo removes each with `rmdir` (empty directories only), deepest first, and
only those the journal lists, so it never goes below that floor and never removes a directory
that holds anything.

**The release directory in an undo.** `releases/<name>` is removed by an undo only when it is
provably this transaction's: the journal holds a `published` line (written after the rename of
the staging directory), or the directory holds exactly the files this transaction staged (same
paths, hashes and sizes), which covers a crash between the rename and its journal line. Any
other directory there is somebody else's: it is neither adopted by a resume nor removed by an
undo, and the install is refused over it. "Exactly what was staged" means the same files (hash,
size and mode bits), the same subdirectories (`0555`) and nothing more. An undo also never
removes a release that the committed receipt lists or that `current` resolves to, so a journal
cannot take a committed release away; created directories inside the install root must have no
symlinked component. The residual limit: a journal is a file of the same user, so anyone who can
write it can still make an undo `rmdir` an EMPTY directory on the way to the install, or restore
a backup the journal names; the checks bound that to the install root and its ancestors, never to
a directory that holds anything.

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

**Unit templates.** The repository ships `contrib/units/{ferrostore.service,
ferrostore@.service, ferrowire.service, ferroloop-hub.service}` (`systemd --user`). A release
stages them verbatim under `contrib/units/`; a template is not a unit yet. Its first line is
`# generated by fl install {{RELEASE}}` (the shape of the marker in rule 9) and it has two
more placeholders, `{{LAUNCHER}}` (`<bin_dir>/fl`) and `{{PREFIX}}`. `fl_core::install::render_unit`
fills them and refuses a value that would break an `ExecStart` word (a relative path, white
space, a quote, `%`, `$`); it is the only place the placeholders are defined, and no installer
step calls it in this release. A placeholder is not `@NAME@`, because systemd reads a leading
`@` of an `ExecStart` as an argv[0] override. Each unit runs one real command and holds no
secret:

| Unit | `ExecStart` | Ordering |
| --- | --- | --- |
| `ferrostore.service` | `{{LAUNCHER}} store serve` | |
| `ferrostore@.service` | `{{PREFIX}}/current/bin/ferrostore serve --store %f` | |
| `ferrowire.service` | `{{LAUNCHER}} wire daemon` | `PartOf=` and `After=ferrostore.service` |
| `ferroloop-hub.service` | `{{LAUNCHER}} hub serve --listen 127.0.0.1:8420 --ui-dir {{PREFIX}}/current/ui/dist` | `Wants=` and `After=` `ferrostore.service ferrowire.service` |

The instance of `ferrostore@.service` is the store's absolute path as `systemd-escape --path`
writes it; `%f` unescapes it, and a missing store is refused (no `--create`). Its start limit
is `StartLimitIntervalSec=60`, `StartLimitBurst=5`: with `RestartSec=3` an instance that fails
at once would otherwise retry every 3 seconds for ever, and five starts in 60 seconds stop it.
The other units set no start limit; with `Restart=on-failure` and `RestartSec=3` they stay
inside systemd's default (5 starts in 10 seconds) and retry every 3 seconds. The hub gets its
bundle from `--ui-dir`, which wins over `[hub] ui_dir`. The hub unit sets no
`WorkingDirectory`, so the hub starts in the home directory and that is its repository root.
It is the headless, UI-serving shape: the views that need a repository (worktrees, worktree
diffs, architecture, commands and skills, repository paths) show nothing useful, code search
answers empty and never scans the home directory (`$HOME` as the root is the only case the hub
refuses), and an owner terminal session starts in the home directory. A hub bound to a
repository sets `[hub] repo_root = "<repo>"` in the `fl` config (`fl hub serve --repo-root
<repo>` wins over it). The path is absolute and an existing directory, else the hub exits 2
before it listens; it need not be a git work tree. Without either, the root is the working
directory. The default UI directory is `<repo_root>/ui/dist`; `--ui-dir` and `[hub] ui_dir`
win over it. The store binding (`fl hub serve`, `fl hub pair`) still follows the working
directory, on purpose, so both commands use the same store. A `repo_root` of `/` or of a
system directory is allowed (not refused), but code search then scans that whole tree and the
`$HOME` guard no longer protects it. The store units set `NoNewPrivileges=yes`; the hub
does not, because an owner terminal session runs `sudo`. `render_unit` refuses a launcher or
prefix that has white space, `%`, `$`, a quote, a backslash or a brace, so such an install
prefix cannot get a unit (choose another `--prefix`). The tests parse every template, check
each `ExecStart` against the built `fl` and `ferrostore` `--help`, and check the staging of a
`--from-git` build. `contrib/ferroloop-hub.service` is the
reference of a hand-deployed unit and is not a template.

**What this release does not do:** `--adopt` and `--gc` (each exits 3 with
`not implemented in this release`), downloads, signatures, unpacking archives, upgrade
preflight, a rollback between releases, service activation, shell `PATH` edits, MCP catalog
writes, credential setup and state migration.

## `fl install --from-git <rev>`

```
fl install --from-git REV [--repo PATH] [--allow-unreviewed] [--with-ui [--npm-scripts]]
           [--build-timeout SECONDS]
           [--yes | --dry-run] [--profile P] [--add X]... [--remove X]... [--prefix PATH]
           [--replace PATH]... [--keep PATH]... [--lock-timeout SECONDS] [--json]
```

Builds one commit of a git checkout and installs the result through the same transaction as
`--artifact-dir`; the two differ only in where the artifact tree comes from. It is the source
route: the one that works before signed releases exist.

**The reviewed-commit rule.** `REV` must resolve to one commit
(`git rev-parse --verify REV^{commit}`) of `--repo` (default: the checkout holding the current
directory; a bare repository or a non-checkout is refused). That commit must be an ancestor of
the LOCAL `refs/remotes/origin/main` (`git merge-base --is-ancestor`). Nothing is ever fetched:
a note says when `origin/main` is more than 7 days old, and an absent `origin/main` means
nothing is reviewed. `--allow-unreviewed` builds a commit that fails the rule anyway: it prints a
loud warning, the receipt records `reviewed: false`, and `fl doctor` warns for as long as it is
installed. On a commit that passes the rule the flag changes nothing (`reviewed: true`).

`REV` is one argument after `--end-of-options`, never through a shell. It must not be empty, start
with `-`, `^` or `!`, contain whitespace or a control character, or be a range (exit 2). The
checkout may not overlap the install prefix in either direction (the build directory is never
inside the source repository).

**What it proves and what it does not.** The built binaries are run with `--version --json` and must
report THIS commit, a clean tree, `source-build` and the commit's time; the tree digest pins the
artifact in a manifest made in memory a moment later. That JSON is SELF-REPORTED by a binary the
build just produced, so the check catches accidents (a wrong or dirty tree, a stale `target/`, a
binary of another commit), not malice: a hostile `build.rs` or proc macro can print matching JSON.
It shows the installed bytes are the ones that were built and checked. It does NOT show that the
commit is the maintainers' or that it was reviewed by anyone: the reviewed-commit rule (an
ancestor of the LOCAL `origin/main` ref, which anyone with the checkout can move) is the trust
boundary, and `--allow-unreviewed` removes it. `--locked` makes cargo use `Cargo.lock`, whose
checksums pin the bytes of every registry crate (git and path dependencies are pinned by their
revision or path); the toolchain itself is whatever is on `PATH`. The build is not guaranteed
reproducible. The result is not a signed release and says so: kind `source_build`, no release id.

**Order of events.**

1. Read-only: open the checkout, resolve the revision, apply the review rule, read the commit's
   file list, plan the install, and check every conflict decision. A refusal here costs nothing
   and writes nothing. `--dry-run` (and a run without `--yes`) stops here: it prints the
   resolved commit, its review status, the build directory, the exact `cargo` command, and
   the install plan, and builds nothing.
2. If the receipt already records this commit's source build (same release, same artifact, same
   review status) and the engine's idempotence test passes, it reports `already installed` and
   does not rebuild.
3. Build in `<prefix>/build/<sha12>/`. 4. Assemble and verify. 5. Install (`fl install --yes`
   semantics, one journaled transaction). 6. Remove the build directory.

**The git commands.** Only object-reading plumbing runs against your checkout: `rev-parse`,
`merge-base`, `ls-tree`, `cat-file`, `log`. Each runs with an EMPTY environment (no `GIT_DIR`,
no credentials), `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`, `HOME` pointing nowhere,
and `-c` overrides for the config keys that make git run a program:
`core.hooksPath=/dev/null`, `core.fsmonitor=false`, `core.attributesFile=/dev/null`,
`log.showSignature=false`, `core.useReplaceRefs=false`, `core.commitGraph=false`,
`protocol.ext.allow=never`, `protocol.allow=never` (and `--no-pager --no-optional-locks
--no-replace-objects`). The environment also holds `GIT_NO_REPLACE_OBJECTS=1`,
`GIT_NO_LAZY_FETCH=1`, `GIT_ALLOW_PROTOCOL=none` (git documents it as overriding every
`protocol.*.allow` in a repository's config), `GIT_GRAFT_FILE=/dev/null` and
`GIT_SHALLOW_FILE=/dev/null`. So `refs/replace/*`, `info/grafts` (which can ADD parents, making
any commit an "ancestor" of `origin/main`), a `shallow` file, a forged commit-graph and a
partial-clone (promisor) configuration cannot change what a sha means, and no command can
fetch: a missing object is an error. Text taken from the repository or from git's messages is
shown with its control characters escaped. Nothing checks out, merges, commits, fetches or
writes in your repository; `cat-file --batch` runs in its own process group under a deadline.

**Why not `git worktree add` or `git archive`.** A worktree writes metadata into your repository
and runs your checkout's filters and hooks. `git archive` applies `.gitattributes`
(`export-ignore`, `export-subst`) and so can produce bytes that are not what the commit holds.
Instead the installer writes the commit's blobs itself from `git ls-tree` and `git cat-file
--batch`: byte for byte, no attributes, no filters, no hooks. Every path is checked first (no
absolute path, `..`, empty segment, backslash or control character, and no `.git` under any
spelling a file system folds into it: case, trailing dots or spaces, `::$INDEX_ALLOCATION`, the
`git~1` short name, HFS+ ignorable code points; bounded count, symlinks and submodules included,
and size) and one bad path refuses the whole commit. Symlinks are not written (a note counts them);
a submodule becomes an empty directory. The build scripts need a `HEAD` to stamp, so the tree
gets a FRESH `.git` (no template, empty config) whose `HEAD` is the commit, whose index is the
commit's tree, and whose only tie to your repository is an `objects/info/alternates` line
(read-only). `git status` there is clean exactly when the files match the commit, which is what
the built binary's `dirty` flag reports; a binary that says `dirty: true` is refused.

**The build directory** is `<prefix>/build/<sha12>/` (`src/`, `home/`, `tmp/`, `out/`,
`build.log`, and `.fl-install-build`, a marker naming the commit and the installer's pid). It is
mode 0700. A directory already there is cleaned only when its marker names the same commit and
the pid it records is not running; a live installer's directory is "another install is running"; any other
directory (no marker, another commit's marker, a symlink) is never touched and is a refusal. It
is removed after a successful install. After a failure once the build has started it is KEPT
and its path is in the error; a failure before that (a missing tool) removes it and every
directory made for it. A failed build leaves its (large) directory until the next attempt at the
same commit cleans it, or you remove it. The marker check is a pid test, so a recycled pid makes
a stale directory look busy (a refusal that names the directory; remove it by hand), and a crash
between making the directory and writing its marker leaves one that is treated as foreign.

**The build.** `cargo build --release --locked -p fl-cli [-p fl-store] --bin fl [--bin ferrostore]`
in `src/`: one `--bin` per binary the selected products need (every profile needs `fl` and
`ferrostore`). It runs through an argument vector in its own process group, under
`--build-timeout` (default 2700 seconds, for the whole build); on the timeout, and also after the
child exits, the whole process group is killed, so no process that stayed in that group outlives
the build. A process that leaves the group (`setsid`, a double fork into a new session) can
survive; the installer does not set `PR_SET_CHILD_SUBREAPER` or otherwise track such processes. Its
output goes to `build.log` (bounded at 32 MiB per step). The environment is an allow-list:

| Variable | Value |
|---|---|
| `PATH` | the caller's, without relative entries |
| `CARGO_HOME`, `RUSTUP_HOME` | the caller's if absolute; else `~/.cargo`, `~/.rustup` when they exist |
| `RUSTUP_TOOLCHAIN`, `CARGO_NET_OFFLINE`, `SSL_CERT_FILE`, `SSL_CERT_DIR` | the caller's, if set |
| `HOME`, `XDG_CONFIG_HOME`, `TMPDIR` | inside the build directory |
| `SOURCE_DATE_EPOCH` | the commit time (the caller's value is ignored) |
| `CARGO_TERM_COLOR=never`, `CARGO_INCREMENTAL=0`, `LC_ALL=C` | fixed |
| `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_TERMINAL_PROMPT=0`, `GIT_NO_REPLACE_OBJECTS=1`, `GIT_CEILING_DIRECTORIES` | fixed |

Nothing else is passed from the environment: no token, vault, cloud or registry credential
variable, no `RUSTFLAGS`, no `FL_RELEASE_ID`, no `GIT_DIR`. This is hygiene, not isolation. The
real `CARGO_HOME` is shared with the build (a `credentials.toml` in it is readable by the build;
there is no overlay), and the commit's build scripts and procedural macros run as you, with your
file system and network, so `--allow-unreviewed` runs code nobody reviewed.

**The UI.** The default is NO UI build: UI assets are optional in this release, FerroDeck shows
"Not installed" for the UI, and the hub falls back as it does today. `--with-ui` additionally
runs `npm ci --no-audit --no-fund --no-update-notifier --ignore-scripts` and `npm run build` in
`ui/` (the commit needs `ui/package.json` and `ui/package-lock.json`), under the same
allow-listed environment and a build-local `HOME` (so a private-registry `~/.npmrc` is not
read), and installs `ui/dist` with the FerroDeck product. Dependency lifecycle scripts do NOT run
by default: they are arbitrary code from the npm registry. `--npm-scripts` (only with
`--with-ui`) drops `--ignore-scripts`, for a UI whose dependencies need a postinstall step; the
risk is running whatever the lock file's packages say to run. `npm run build` itself is the
commit's code, like the Rust build.

**What is assembled.** `out/<id>/` holds `bin/<name>` copied from `target/release/` (each is run
with `--version --json` and must report the commit), the commit's `contrib/units/*` (units the
commit does not ship are not offered, and neither is any action that names them; the plan says
which), `ui/dist` with `--with-ui`, and `release.json` (version, commit, `source-build`,
`reviewed`, `rustc`, `cargo`, `build_id`, build time). The manifest takes its compat ranges from
the built `fl`. The artifact's hash is the tree digest of this directory. The installed
`release.json` is written by the transaction from the receipt's release record. The release
directory is `<version>+<sha12>`; the plan's `release` gains `commit` (and `build`) for a source
build, in the text and the JSON (additive: a plain plan prints neither); the receipt records the source build, the commit, the review
flag, the toolchain and the build id, and no path into the source repository.

**Installing the same commit again.** If the receipt already records this commit's source build
with the same review status, the run reports `already installed` and does not rebuild, whatever
toolchain is on `PATH` now. If the review status differs (it was installed with
`--allow-unreviewed` and `origin/main` has since caught up, or the reverse), the run is refused
before anything is built: a release directory is immutable and its `release.json` records how it
was built, so its status is not changed in place (removing a release is not implemented in this
release; use another `--prefix`).

**Only `--from-git` records a build.** `release.build`, and a `source_build` manifest with
artifacts, are provenance claims. `--manifest` refuses a manifest that carries `release.build`
(exit 3), and the transaction refuses any manifest making the claim unless it comes from the
source-build path itself, which produced the artifact.

**Limits.** The build needs the toolchain (`cargo`, `rustc`, and `npm` for `--with-ui`) on `PATH`
and network access for crates the locked build has not cached; the build directory needs the
disk of a release build under the install root's file system. Only the binaries `fl` and
`ferrostore` have a build recipe. `--adopt`, `--gc`, signing, upgrade preflight and service
activation are not part of this command.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | A plan was printed (including a plan with conflicts: read `status`); an install, `--resume` or `--rollback` completed; the install was already there. |
| 1 | A step failed and the transaction was undone (or the undo failed too: run `--resume`); a file system error such as a read-only root; `--from-git`: the build failed, timed out, or produced no binary (the build directory and `build.log` are kept). |
| 2 | The request is not well formed: unknown product or profile, a product both added and removed, a relative or malformed prefix or `$FL_INSTALL_ROOT`, an unknown flag, `--db`, `--dry-run` with `--yes`, `--resume` with `--rollback`, `--yes` with `--resume`, a decision for a path that is not a conflict, `--yes` without `--artifact-dir` when a release must be staged, a revision that is empty, starts with `-`, `^` or `!`, has whitespace or a control character, or is a range, `--from-git` with `--artifact-dir`, `--manifest`, `--resume` or `--rollback`, `--repo`/`--allow-unreviewed`/`--with-ui`/`--npm-scripts` without `--from-git`, `--npm-scripts` without `--with-ui`. |
| 3 | Refused: a `--manifest` that carries `release.build`, the same commit again with another review status, no `--dry-run` or `--yes` (the plan is printed first), `--adopt`, `--gc`, a commit that is not an ancestor of `origin/main` (or no `origin/main`) without `--allow-unreviewed`, a revision that names no commit, not a git checkout, a checkout that overlaps the install root, a build directory this installer did not make, a missing `cargo`/`rustc`/`npm`, a built binary that does not report the commit, a clean tree and the commit time, an invalid or unreadable manifest or receipt, a source-build manifest, a forbidden prefix or one that resolves into a system tree, a symlinked prefix component, an artifact that fails its size or hash or holds a symlink, an unresolved conflict, another installer holding the lock, a corrupt journal, a removal of a required or needed product, an unsupported platform, a missing artifact. |
| 70 | Test builds only: a simulated crash returned as an error. |

## What `--dry-run` does NOT do

It creates, modifies and deletes nothing: no directory, link, unit, catalog, PATH entry,
receipt, journal or lock file. It starts no process and no service (`--from-git --dry-run`
starts `git`, and only the read-only plumbing listed below), makes no network call,
opens no store, reads no project config and no credential, and never reads `$HOME` except to
look at the paths the plan names.

## Tests

The `--from-git` tests (`crates/cli/tests/install_from_git.rs`, `crates/install/src/fromgit_tests.rs`)
build a throwaway git repository with three commits and a fake `origin/main`, and put a FAKE
`cargo`, `rustc` and `npm` first on a PATH they construct (`fl_install::fixture::gitrepo`); the
real toolchain and the network never run. The black-box tests drive the real process-spawn path
(environment dump, process group, timeout, the arguments `npm` receives); the in-process tests
inject a `Builder`. A repository armed with hooks, filters, `core.fsmonitor`, a `gpg.program` and
a signed commit, and a global config armed the same way, must leave no sentinel file; a repository
with `refs/replace/*`, an `info/grafts` that adds a parent, and a partial-clone config over a
deleted blob must change neither the ancestor result nor the extracted tree, and must not fetch.
One test (`real_cargo_stamps_the_synthetic_tree`, `#[ignore]`, never run in CI) runs the real
`cargo` and the real `crates/core/build/stamp.rs` against a synthetic workspace; run it by hand
with `cargo test -p fl-cli --test install_from_git -- --ignored real_cargo_stamps`.

Every run is confined to a temporary root. The crash seam is `fl_install::Faults`: a `FaultPoint`
(`before` or `after` any step) makes the transaction return a simulated crash with nothing
undone; the `fl` binary reads `FL_INSTALL_FAULT` only when `fl-install` is built with the
`fault-injection` feature, which only fl-cli's tests enable; an optimized build with it is a
compile error. Release builds are `cargo build --release --locked -p fl-cli`, never `--all-targets`
or `--tests`. The tests crash a fresh install, an
upgrade and a `--replace` at every point and check, by full directory snapshots, that `--resume`,
`--rollback` and the next install each end in exactly "fully installed" or exactly the
pre-state.
