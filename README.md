# FerroLoop

**FerroLoop** gates the actions that cost you. You declare what has to be true before a step
may happen, `fl` checks it against your working tree, and the step is refused when the check
fails. Its command is `fl`.

**FerroLoop** names both the suite and this CLI; `fl` is the command you type, kept short
deliberately — the same split as Claude Code and `claude`.

---

## What fl does

Self-hosted and local-first: the store is one file on your machine, and the GitHub tracker and
ledger are options you turn on per project.

* **Executable gates.** A gate names a population to examine (a glob, the files changed since a
  git ref, or the output of a program) and a program to run over it. A gate that examined
  nothing fails; it does not pass. `fl check` evaluates gates without moving anything and exits
  `0` (pass), `1` (a gate failed) or `2` (the check could not run).
* **Records and transitions.** A record is a unit of work in one of the states `todo`, `doing`,
  `review`, `done` or `needs_human`. A transition names the states it moves between, the gates
  that must pass first, and a regret level (`low` or `high`). `fl record move` refuses a move
  whose gates fail. Findings follow the same rule: `fl finding reproduce` is refused unless a
  gate currently fails.
* **DAG workflows (`fl flow`).** Author, import and execute multi-step workflow graphs, with
  gate checks, review loops, and human sign-off (`approve` / `reject`) on steps that need it.
* **Execution harnesses (`fl attempt`).** Run one attempt against a record through a vendor
  CLI adapter (`claude`, `codex`, `antigravity`, `muse` or `grok`) under a time limit and spend
  ceiling, and record the outcome, duration, tokens and cost. `fl stats` aggregates them.
* **MCP catalog (`fl mcp`).** One committed catalog of a project's MCP servers,
  `.fl/mcp.toml`, written into each of those five CLIs' own config files
  ([docs/mcp.md](docs/mcp.md)).
* **Architecture conformance (`fl arch`).** Validates `.fl/architecture.toml`, checks Cargo
  workspace dependencies and source imports against it, and exports LikeC4 DSL.
* **And more.** `fl wire` (agent messaging), `fl state` (agent memory), `fl github` (issue
  tracker, ledger and pull-request policy), `fl vault` (secrets vault and MCP gateway), `fl hub`
  (HTTP/WebSocket daemon behind the FerroDeck web app), `fl agent`, `fl routing` and
  `fl manifest`. `fl --help` lists every command.

---

## The Ferro Suite

FerroLoop is developed as one workspace of crates plus a web app. Six parts have their own
repository. Those repositories are extractions: each currently mirrors code that still lives
in this one, and this repository is where the code is built and tested.

| Part | What it is | Where it lives here |
| --- | --- | --- |
| [FerroDeck](https://github.com/Artificial-Humanity/FerroDeck) | The command-center web app (React PWA) for supervising coding agents, and the hub daemon that serves it. | `ui/`, `crates/hub` |
| [FerroStore](https://github.com/Artificial-Humanity/FerroStore) | The embedded store: the redb-backed local store and the single-file `.fls` store (documents, queue, tables, vectors). | `crates/store` |
| [FerroState](https://github.com/Artificial-Humanity/FerroState) | Durable agent memory: scoped memories, handoffs, runtime state, recall and the structural code index. | `crates/core` (state modules), `fl state` |
| [FerroWire](https://github.com/Artificial-Humanity/FerroWire) | Agent-to-agent messaging: mailboxes, receipts, the local daemon, peer relay and push to vendor CLIs. | `crates/wire` |
| [FerroTrack](https://github.com/Artificial-Humanity/FerroTrack) | Two-tier tracking: GitHub Issues as a tier, the `fl/ledger` branch, and the pull-request client. | `crates/github` |
| [FerroGate](https://github.com/Artificial-Humanity/FerroGate) | The zero-knowledge secrets vault, credential proxy and API-to-MCP gateway. | `crates/gate` |
| FerroWatch | Reserved name for observability and monitoring. A roadmap item: no repository and no code yet. | — |

The six repositories are mounted as optional git submodules under `submodules/`. The build does
not use them. See [Submodules](docs/getting-started.md#submodules).

---

## Installation

Today the only route is building from source.

* Rust 1.98+ (edition 2024) and Cargo
* git: `fl project add` refuses a directory that is not a git working tree with at least one
  commit
* Optional: an execution CLI (`claude`, `codex`, `agy`, `muse` or `grok`), for `fl attempt` and
  `fl flow`
* Optional: Node.js and npm, to build the FerroDeck bundle in `ui/`

```console
git clone --recurse-submodules https://github.com/Artificial-Humanity/FerroLoop.git
cd FerroLoop
cargo build --release          # the binary is target/release/fl
(cd ui && npm install && npm run build)   # optional: FerroDeck bundle, ui/dist/
```

`--recurse-submodules` is optional. The submodules are pins to the standalone repositories and
building FerroLoop does not need them; a plain `git clone` builds the same binary.

---

## Quick start

Run these at the root of a git repository with at least one commit, with `fl` on your `PATH`
(otherwise use `target/release/fl`). The handles `1` below assume a fresh store; otherwise use
the handles `fl project add` and `fl gate add` print.

```console
fl project add .
fl gate add --project 1 --name config-parses --kind command --glob "config/*.json" \
    --program python3 --arg=-c --arg="import json,sys;[json.load(open(p)) for p in sys.argv[1:]]"
fl transition add --project 1 --name launch --from review --to done --regret high --gate 1
fl check launch --project 1
```

[docs/getting-started.md](docs/getting-started.md) walks through this end to end, with real
output and a refusal. A store `fl` creates is private to your Unix user: file mode `0600`,
directory `0700`. `FL_QUIET_STORE_PERMS=1` silences the warning about an existing store that
others can reach. Which store `fl` opens, and the permission details, are in
[Where the store lives](docs/getting-started.md#where-the-store-lives).

---

## Documentation

* [docs/getting-started.md](docs/getting-started.md) — build the CLI and gate a real action; where the store lives; the submodules.
* [docs/sharing-gates.md](docs/sharing-gates.md) — share a project's gates with other machines through a committed manifest.
* [docs/github-tracker.md](docs/github-tracker.md) — keep a project's records and findings in a GitHub repository's Issues.
* [docs/github-ledger.md](docs/github-ledger.md) — publish each decision's evidence to the tracker's repository.
* [docs/routing.md](docs/routing.md) — keep developer-level items in the local store and human-level items in GitHub.
* [docs/mcp.md](docs/mcp.md) — one committed catalog of a project's MCP servers, written into each agent CLI's own file.
* [docs/vendor-runtimes.md](docs/vendor-runtimes.md) — on-disk session, transcript and prompt-delivery shapes of the five vendor CLIs.
* [docs/README.md](docs/README.md) — index of the public documentation directory.
* [AGENTS.md](AGENTS.md) — working rules, core stack matrix, and file-naming conventions for agents and contributors.
* [WORKFLOW.md](WORKFLOW.md) — development workflow, review requirements, and commit conventions.
* [PERSONA.md](PERSONA.md) — developer persona and domain expertise.

---

## License

Licensed under [Apache-2.0](LICENSE).
