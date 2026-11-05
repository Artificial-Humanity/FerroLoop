# Build provenance

Every `fl`, `ferrostore` and Hub binary says which source it was built from. A build made from
a source tree reports its actual commit and whether the tree had changes; it never poses as an
official release.

## Stamps

A `build.rs` in `fl-cli`, `fl-store` (for `ferrostore`) and `fl-hub` runs `git rev-parse HEAD`
and `git status --porcelain` and embeds three values. It needs no dependency; the logic is one
shared, tested file, `crates/core/build/stamp.rs`, that each `build.rs` includes by path.

| Stamp | Value |
|---|---|
| `FL_BUILD_COMMIT` | The 40-digit `HEAD` commit, or `unknown`. |
| `FL_BUILD_DIRTY` | `true` when a file the binary is built from differs from `HEAD` (staged or not, or new and not ignored), `false`, or `unknown`. "Built from" is the closure below. |
| `FL_BUILD_TIME` | Unix seconds. `SOURCE_DATE_EPOCH` when it is set to a non-negative integer, else the `HEAD` commit time (`git log -1 --format=%ct`), else the clock when there is no git. |
| `FL_RELEASE_ID` | Only when `$FL_RELEASE_ID` is set (and not empty) at build time AND the tree is clean. A dirty tree, or one whose dirty flag is `unknown`, reports no release id. |

The build time is the commit time, not the clock, on purpose: the same sources always stamp
the same bytes, so a binary is reproducible. A build with no git has only the clock.

A tree is a checkout only when the `.git` found above it (a directory, or the file of a linked
worktree) sits in a directory that holds `crates/core/build/stamp.rs`. A copy of the sources
under some other repository, such as a registry or tarball copy in a git-tracked home
directory, is not stamped with that repository's commit. A tree that is not a checkout
(a tarball, `cargo publish`, a sandbox without `git`) builds with `unknown` for the commit and
the dirty flag. It does not fail. The search honours `GIT_CEILING_DIRECTORIES`. The git
commands run with `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`,
`GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES` and `GIT_NAMESPACE` removed, and
`git status` runs with `--no-optional-locks` so reading the status never rewrites the index.

### What the stamp is computed from, and when it reruns

**Cargo rebuilds a package, and everything that depends on it, whenever its build script
reruns, even when the script prints exactly what it printed before.** This was measured on this
workspace (cargo verbose output, a script rerun with byte-identical output): `fl-store` was
recompiled, then `fl-wire`, `fl-watch`, `fl-hub` and `fl`. So the stamp cannot lean on
"identical output costs nothing". Instead each script names no more than the inputs of its own
binary, and the dirty flag is computed from exactly those:

* **Inputs.** For the package and every crate it depends on by path (its "closure"):
  `src/`, `build/`, `build.rs` and `Cargo.toml` of the crate, plus the workspace `Cargo.toml`
  and `Cargo.lock`. `fl-store` (and so `ferrostore`) builds from `fl-store` and `fl-core`;
  `fl-hub` from `fl-core`, `fl-exec`, `fl-gate`, `fl-github`, `fl-install`, `fl-store`,
  `fl-watch` and `fl-wire`; `fl` from all of them, `fl-mcp` and `fl-browse`. Each `build.rs` lists its closure, and a test
  (`crates/core/tests/build_stamp.rs`) checks every list against the `Cargo.toml` files.
* **Dirty.** `git status --porcelain` over those inputs only. A change in another crate, in
  `tests/`, in `docs/` or in `ui/` does not make the binary dirty, because it is not in the
  binary. The same edit made to an input is dirty whether staged or not.
* **Rerun.** The script reruns when an input changes, and when `HEAD`, the branch ref it names
  (in the shared git directory for a linked worktree) or `packed-refs` change, and when
  `SOURCE_DATE_EPOCH` or `FL_RELEASE_ID` change. It does NOT rerun for the git index, so a
  `git add` rebuilds nothing, and an edit to a crate outside the closure rebuilds only what it
  would have rebuilt anyway. A commit or a branch switch moves the commit and rebuilds the
  binaries, which is right: their stamp changes.

Measured with these scripts: touching the git index rebuilt nothing; touching a source file of
`fl-cli` rebuilt `fl` only; of `fl-hub`, `fl-hub` and `fl`; of `fl-core`, everything that
depends on it (as the edit itself requires).

The dirty flag is read when the script reruns. A change to a file that is not an input is
not seen, by design. A build with no git has no dirty flag (`unknown`) and its clock stamp
changes on each rerun.

### Packaging

These crates are workspace-internal. Each `build.rs` includes `../core/build/stamp.rs` with
`#[path]`, a file outside the crate's own directory, so `cargo package` (and `cargo publish`)
of `fl-cli`, `fl-store` or `fl-hub` is not supported as it stands. Building from a source
tarball of the whole workspace works; the stamps are then `unknown`.

The `source` field is `source-build` for every build today: no release channel exists yet. A
build of a clean tree that sets `FL_RELEASE_ID` reports it in `release_id` and still says `source-build`.

## `fl --version`

```text
$ fl --version
fl 0.1.0 (9fe688d41719, dirty)
```

`fl --version`, `fl -V` and `fl version` print one human line: the crate version, then the
first 12 digits of the commit, then `dirty` for a tree with changes (`dirty unknown` when git
could not say, `unknown commit` when there was no checkout, `release <id>` when a release id
was set).

`fl --version --json`, `fl -V --json` and `fl version --json` print one JSON object, the same
from each:

```json
{
  "name": "fl",
  "version": "0.1.0",
  "commit": "9fe688d41719150f520a2dbab359264764c05fac",
  "commit_short": "9fe688d41719",
  "dirty": true,
  "build_time_unix": 1793683357,
  "source": "source-build",
  "release_id": null,
  "protocol": { "ferrowire": { "current": 2 } },
  "schema": {
    "manifest": { "min": 1, "max": 3 },
    "warrant_ledger": { "current": 1 },
    "vault": { "current": 1 }
  },
  "store_formats": {
    "redb_schema": { "min": 2, "max": 9 },
    "ferrostore_container": { "min": 1, "max": 2 }
  }
}
```

`dirty`, `build_time_unix` and `release_id` are `null` when unknown. Keys are sorted. `fl version`
needs no store and no valid config. `fl --version --json` is recognised only as exactly those
two flags (in either order); any other arguments next to `--version` are clap's, and print the
human line.

Version groups hold what the code checks by:

* `{ "min": a, "max": b }`: a range the code opens. A value outside it is refused.
* `{ "current": n }`: a version matched exactly. The code defines no range for it.

| Field | Constant | Rule in the code |
|---|---|---|
| `protocol.ferrowire` | `fl_wire::peer::WIRE_PROTOCOL_VERSION` | Peer handshake: any other version is refused. |
| `schema.manifest` | `MANIFEST_FORMAT_WITHOUT_LEDGER` ..= `MANIFEST_FORMAT` | Manifest import: outside the range is refused. |
| `schema.warrant_ledger` | `fl_gate::warrant::WARRANT_SCHEMA_VERSION` | Any other version is refused. |
| `schema.vault` | `fl_gate::VAULT_SCHEMA_VERSION` | Any other version is refused. |
| `store_formats.redb_schema` | `FORMAT_VERSION` ..= `FORMAT_WITH_AREA_DELETE` (the ladder `check_format` accepts) | Both engines open this ladder. A new store starts at `min`; the first write of a newer kind raises it, and an older build refuses the raised store. |
| `store_formats.ferrostore_container` | `CONTAINER_FORMAT_OLDEST` ..= `CONTAINER_FORMAT_NEWEST` | The `.fls` container: this build reads both and writes the newest. |

## `ferrostore --version`

`ferrostore --version` prints `ferrostore 0.1.0 (abc123456789)`. `ferrostore --version --json`
prints the same stamp fields (with `"name": "ferrostore"`) and `store_formats`.

## `GET /api/v1/version`

Any authenticated caller, Owner or Peer, may read it (the same tier as `GET /api/v1/settings`);
an unauthenticated request is a 401. The body is the Hub's own build, with `name` `fl-hub`, and
two more keys:

```json
{
  "name": "fl-hub",
  "version": "0.1.0",
  "commit": "9fe688d41719150f520a2dbab359264764c05fac",
  "commit_short": "9fe688d41719",
  "dirty": false,
  "build_time_unix": 1793683357,
  "source": "source-build",
  "release_id": null,
  "ui": { "commit": "9fe688d41719", "build_time_unix": 1793683400, "version": "0.1.0" },
  "code_ui_agree": true
}
```

* `ui` is the UI bundle's `build-info.json`, read from the Hub's UI directory
  (`<ui_dir>/build-info.json`). It is `null`, never an error, when the Hub serves no UI, the
  file is missing, is not a regular file (a symlink is not followed), is larger than 16 KiB,
  or is not a JSON object with a hex `commit` of 7 to 64 digits. The Hub echoes only
  `commit`, `build_time_unix` (an integer or `null`), `version` (a string of at most 64
  characters, or `null`) and `dirty` (when it is a boolean). It echoes no path and no other key.
* `code_ui_agree` is `true` when the UI was built from the Hub's commit, `false` when not, and
  `null` when either commit is unknown. The UI commit may be abbreviated (7 digits or more): it
  agrees with a Hub commit it is a prefix of.

The UI build writes the file; its expected shape is:

```json
{ "commit": "<git rev-parse HEAD>", "build_time_unix": 1793683400, "version": "0.1.0" }
```
