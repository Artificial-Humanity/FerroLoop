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
resolves. The one exception is the pass mark itself, which a local `gate run` or `gate
affirm` is free to set, because that mark is inherently local. Changing anything else is
refused with a remedy: make the change in the store that authors the project, export again,
commit, and import here.

Importing for the first time also changes what kind of store this is, on disk: a store that
has never imported anything opens in any earlier build of `fl`, but the moment it imports a
manifest it is marked as a store that holds imports, and an older `fl` — one that has never
heard of an import and would happily let someone edit an imported gate — refuses to open it
at all, rather than opening it and quietly ignoring the mark.

## Check

`fl manifest check --project <project>` runs the same test the CLI applies for you before it
runs any gate belonging to that project, and reports the same verdict without running
anything. Use it to find out ahead of time whether the manifest at hand is trustworthy.

What it refuses, and the remedy for each:

- **The file was edited by hand.** Its content no longer hashes to what it records. Gates
  are authored in the store that owns the project; export it again from there.
- **A gate changed since the manifest was exported.** Something in the authoring store's
  copy of that gate no longer matches the manifest's copy. Export again, then commit.
- **A gate or a transition is missing from the manifest**, because it was added after the
  last export, or the manifest lists one the authoring store no longer holds. Export again,
  then commit.
- **The manifest is not committed.** Another machine can only resolve a gate through a
  committed manifest, so an uncommitted one is refused even if its content is otherwise
  fine. Commit it.
- **An imported gate was edited locally, or the manifest changed since this store imported
  it.** Either way, the working tree's `.fl/manifest.json` is no longer the one this store's
  copy came from. Run `fl manifest import` again before running its gates.

Every command that runs a gate belonging to an imported project applies the same currency
check first, so a stale import is refused there too, not just under `manifest check` — see
[section 8](getting-started.md#8-staleness-and-gate-affirm) of the getting-started guide for
the analogous staleness check on a gate's population.

`crates/cli/tests/manifest.rs` is the executed version of this page: every scenario above —
export, import, a stale import, a hand-edited manifest, an uncommitted one, a gate or
transition added after export, an imported gate refusing a local edit — is a test there,
driven end to end across two separate stores standing in for two machines.
