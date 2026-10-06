# Sharing gates across machines

A gate lives in the store that authors it — the machine where `gate add` was run. An issue,
a pull request, or a teammate's checkout can name that gate, but they cannot open that
store. They need some other way to resolve what the gate is and what it runs, from nothing
but the repository they already have.

That is what the manifest is for. `.fl/manifest.json`, committed alongside the code, is a
project's gates and transitions exported from the authoring store. Any other machine reads
it, imports it into its own store, and from then on can run those gates against its own
working tree — without ever talking to the authoring store.

## Export, then commit

On the machine that authors the project, `fl manifest export --project <project>` writes
`.fl/manifest.json` at the project's root. It prints every gate it wrote, alongside the
command that gate runs — worth reading before you commit, since a gate's command can name a
path local to this machine, and that string is about to become public.

Exporting only writes the file. It is not shared until it is committed, and `fl` cannot see
whether a commit was pushed — only that the working tree has one. Commit it the same way you
would any other file that belongs in the repository.

## Import, elsewhere

On another machine, inside a clone of the same repository, `fl manifest import` reads
`.fl/manifest.json` from the current directory (or `--root <path>`) and writes its project,
gates and transitions into the local store, under their own ids. From that point the local
store can run those gates, evaluate transitions that use them, and reproduce or verify
findings against them — the same commands that would run on the authoring machine.

A pass mark, though, never travels. It is earned by actually running a gate on a particular
machine's working tree, and it stays there: importing a manifest again later updates the
gates and transitions, but never manufactures a pass mark for a run that did not happen here.

A gate imported this way cannot be edited locally — not its command, its population, or
anything else that would make this copy disagree with the manifest every other reader
resolves. `gate affirm` is one of the edits this refuses: affirming re-stamps a gate's
authorship, which belongs to whoever authors the project, not to whoever happens to run it
elsewhere. The one thing that does update locally is the pass mark itself, and only by
actually running the gate — through `gate run`, `check`, a gated `record move`, or `finding
reproduce`/`verify` — because that mark is inherently local: it records what happened on
*this* machine's working tree, not a re-authoring of the gate. Changing anything else is
refused with a remedy: make the change in the store that authors the project, export again,
commit, and import here.

Importing for the first time also changes what kind of store this is, on disk: a store that
has never imported anything opens in any earlier build of `fl`, but the moment it imports a
manifest it is marked as a store that holds imports — format 3 — and an older `fl` — one that
has never heard of an import and would happily let someone edit an imported gate — refuses to
open it at all, rather than opening it and quietly ignoring the mark.

A store that records a GitHub ledger root is format 4. Importing a manifest that carries one
(manifest format 2: it carries `ledger_root`, the first commit of the project's GitHub ledger —
see [github-ledger.md](github-ledger.md)) raises the store to format 4 — the same thing happens
when the GitHub ledger is set up directly. An older `fl` refuses a format 4 store — it would
otherwise export the manifest without the root every other machine checks the ledger against. The
remedy is to upgrade `fl`: nothing is wrong with the store, and starting a new one would lose its
history. From this release on, `fl` itself says so when it meets a store newer than it reads.

A project that routes its items between its local store and GitHub exports its routing map
too: the manifest is then format 3 (with its ledger root, if it has one), and the map is
covered by the hash like everything else in it. Importing such a manifest writes the map and
raises the store to format 5, which an older `fl` refuses — it would route nothing. A
re-import of a manifest that has no routing map, into a store that imported one, is refused:
the checked-out manifest is older than the one this store imported. A project without
routing still exports format 1 or 2, exactly as before.

## Check

`fl manifest check --project <project>` is a *superset* of the check a gate-running command
(`gate run`, `check`, a gated `record move`, `finding reproduce`/`verify`) applies for you
before it runs a gate: that check is only the currency check below, it runs only on an
importing machine, and on the machine that authors the project those same commands apply no
manifest check at all. `fl manifest check` additionally verifies, on the authoring machine,
that every gate and the transitions still match what the store holds, and, on either machine,
that the manifest is committed.

What it refuses, and the remedy for each:

- **The file was edited by hand.** Its content no longer hashes to what it records. On the
  machine that authors the project, export it again from there. On an importing machine
  export is refused (`NotAuthoring`) — the remedy there is to restore the committed file
  instead, with `git checkout -- .fl/manifest.json` or by pulling. (A field added by hand
  inside a gate that `fl` does not know about is dropped when the manifest is read back, so it
  never changes the hash — and it has no effect either.)
- **A gate changed since the manifest was exported.** Something in the authoring store's
  copy of that gate no longer matches the manifest's copy. Export again, then commit.
- **A gate or a transition is missing from the manifest**, because it was added after the
  last export, or the manifest lists one the authoring store no longer holds. Export again,
  then commit.
- **The manifest is not committed.** Another machine can only resolve a gate through a
  committed manifest, so an uncommitted one is refused even if its content is otherwise
  fine. Commit it.
- **The manifest changed since this store imported it.** The working tree's
  `.fl/manifest.json` is no longer the one this store's copy came from. Run `fl manifest
  import` again before running its gates.

An imported gate edited locally is not a `manifest check` verdict at all: that edit — to its
command, its population, or anything else that would make this copy disagree with the
manifest — is refused by the store at the moment it is attempted, with its own remedy, and the
manifest on disk is left untouched.

Every command that runs a gate belonging to an imported project applies the same currency
check first, so a stale import is refused there too, not just under `manifest check` — see
[section 8](getting-started.md#8-staleness-and-gate-affirm) of the getting-started guide for
the analogous staleness check on a gate's population.

## Pitfalls

- Don't run `fl project add .` on an importing machine before `fl manifest import`. If you do,
  the import is refused because the root is already taken by the project `project add` just
  registered — and there is no command that removes a project, so recovery means starting over
  with a fresh store.
- Importing into a second store on the machine that authors the project makes every one of
  that project's gates held by two stores at once. From then on, any command naming one of
  those gates or that project by IRI must pass `--db <path>` (or set `$FL_DB`) to say which
  store it means, or it is refused as held by more than one store.
- An older checkout can lag behind the manifest a store already imported. If `git checkout`
  moves the working tree to a commit whose manifest doesn't list a gate this store holds, `fl
  manifest import` there is refused (`WouldRemoveGate`, naming the gate) rather than silently
  dropping it. The usual fix is to check out a commit whose manifest lists the gate again, not
  to change anything where the project is authored.

`crates/cli/tests/manifest.rs` is the executed version of this page: every scenario above —
export, import, a stale import, a hand-edited manifest, an uncommitted one, a gate or
transition added after export, an imported gate refusing a local edit — is a test there,
driven end to end across two separate stores standing in for two machines.
