# FerroLoop

**FerroLoop** gates the actions that cost you. You declare what has to be true before a step
may happen, `fl` checks it against your working tree, and the step is refused when the check
fails. Its command is `fl`.

**FerroLoop** names both the suite and this CLI; `fl` is the command you type, kept short
deliberately — the same split as Claude Code and `claude`.

---

## What fl does

* **A local store.** Projects, gates, transitions, gate runs and attempts live in an embedded
  database (redb), in one file on your machine, and so do records and findings unless a project
  keeps them in GitHub Issues (see the GitHub tracker below). Nothing needs a server or a cloud
  account.
* **Gates.** A gate names a population to examine (a glob, the files changed since a git ref,
  or the output of a program) and a program to run over it. A gate that examined nothing fails;
  it does not pass. A gate is stamped with the commit it was written against, and
  `fl gate affirm` re-stamps it when a person has looked.
* **Records and transitions.** A record is a unit of work, in one of the states `todo`, `doing`,
  `review`, `done` or `needs_human`. A transition names the states it moves between, the gates
  that must pass first, and a regret level (`low` or `high`). `fl record move` runs the gates of
  a declared transition and refuses the move when they fail. A move that no declared transition
  covers is made anyway, exits `0` and is printed as `ungated`. `fl check` evaluates them
  without moving anything and exits `0` (pass), `1` (a gate failed) or `2` (the check could not run).
* **Findings.** A claim about a record is not actionable until a gate fails because of it:
  `fl finding reproduce` is refused unless the gate currently fails. `fl finding verify` closes a
  finding only when the reproduction now passes and every other gate that passed before still
  passes. A claim that cannot be reproduced is withdrawn, and the withdrawal is counted against
  whoever raised it.
* **Attempts and telemetry.** `fl attempt <record>` runs an execution adapter (`claude`,
  `codex`, `antigravity`, `muse`, or `grok`) against a record under a time limit and spend
  ceiling, recording the outcome (`completed`, `timeout`, `crashed` or `refused`), duration,
  output excerpt, token counts (`tokens_in`, `tokens_out`), cost in micro-USD, and
  `paths_touched`. `fl stats` aggregates attempt outcomes, wall-clock time, token usage, cost,
  and transition regret audits; `fl agent liveness` reports heartbeat health across active
  runs.
* **Agent registry and unified state.** `fl agent` registers project leads, assistants, and
  lab managers, manages coverage areas, and renders `REGISTRY.md`. `fl state` stores scoped
  (`workspace`, `project`, `agent`, `workflow-run`) memory, handoffs, and runtime state with
  owner sensitivity redaction and `MEMORY.md` / `STATE.md` rendering.
* **FerroWire (`fl wire`).** Agent-to-agent messaging across local and remote hosts with
  durable store receipts, native vendor push dispatch (Claude Code Unix socket, Antigravity
  `agentapi`, Codex queue, Muse, Grok, and webhooks), and optional `fl-wire` HTTP/WebSocket
  relay.
* **The GitHub tracker, git ledger, and PR policy (`fl github pr`).** A project can keep its
  records and findings in GitHub Issues ([docs/github-tracker.md](docs/github-tracker.md)) and
  publish decision evidence to an append-only `fl/ledger` branch
  ([docs/github-ledger.md](docs/github-ledger.md)). `fl github pr` evaluates pull-request
  review and merge policy by project and author role, posts structured review verdicts, and
  gates merges on verified approvals.
* **Two-tier routing and escalation.** A routing map sends each new record and finding by its
  area to the local store or to GitHub. `fl record escalate` and `fl finding escalate` move a
  local item to GitHub; a move to `needs_human` escalates a local record
  ([docs/routing.md](docs/routing.md)).
* **Shared gates.** `fl manifest export` writes a project's gates and transitions to
  `.fl/manifest.json`, to commit with the code; `fl manifest import` reads it on another
  machine ([docs/sharing-gates.md](docs/sharing-gates.md)).
* **An MCP catalog.** `fl mcp` keeps one committed catalog of a project's MCP servers,
  `.fl/mcp.toml`, and writes it into the files that Claude Code, Codex, Antigravity, Muse, and
  Grok read their MCP servers from (`fl mcp` also exposes `fl mcp serve` so agents can call
  FerroLoop tools over stdio MCP) ([docs/mcp.md](docs/mcp.md)).
* **Workflows (`fl flow`) and architecture conformance (`fl arch`).** `fl flow` validates,
  imports, and executes multi-step DAG workflows (`.fl/workflows/*.toml`) with automated gate
  checks, review loops, worktree isolation, and human sign-off tiers. `fl arch` validates
  `.fl/architecture.toml`, checks Cargo workspace dependency and source-import conformance, and
  exports LikeC4 DSL.
* **FerroLoop Hub (`fl hub`) and FerroDeck (`ui/`).** `fl hub serve` runs the HTTP/1.1 and
  `/ws/v1` WebSocket daemon with two-tier authentication (`Owner` pairing/session cookies and
  Tailscale header trust vs. `Peer` bearer tokens), host session and transcript discovery,
  git worktree diff inspection, PTY terminal streaming, and static serving for **FerroDeck**,
  the desktop and iPad command-center web application in `ui/`.

fl is self-hosted. Its store is a file on your machine, and the GitHub tracker and ledger are
options you turn on per project.

---

## Installation

Installation is simple: install, uninstall, or install component by component (cherry-pick).
That goal does not imply how many binaries or other parts FerroLoop has.

Today the only route is building from source, below.

### Prerequisites

* Rust 1.98+ (edition 2024) and Cargo
* git: `fl project add` refuses a directory that is not a git working tree with at least one
  commit
* Optional: an execution CLI (`claude`, `codex`, `agy`, `muse`, or `grok`), for `fl attempt`
  and `fl flow`
* Optional: Node.js 24+ and npm, for building the **FerroDeck** frontend bundle in `ui/`
* Optional: a GitHub repository and a token or App, for the tracker, ledger, and `fl github pr`

### Building from source

```console
# Clone the repository
git clone https://github.com/Artificial-Humanity/FerroLoop.git
cd FerroLoop

# Build the release binary
cargo build --release

# (Optional) Build the FerroDeck web UI bundle served by `fl hub serve`
cd ui && npm install && npm run build
```

The compiled binary is `target/release/fl`, and the compiled FerroDeck bundle is `ui/dist/`.

---

## Quick start

To gate a real action end to end — registering a project, writing a gate, wiring it into a
transition, and reading a refusal — see [docs/getting-started.md](docs/getting-started.md). In
short, run these at the root of a git repository with at least one commit, with `fl` on your
`PATH` (otherwise use `target/release/fl`). The handles `1` below assume a fresh store;
otherwise use the handles `fl project add` and `fl gate add` print.

```console
fl project add .
fl gate add --project 1 --name config-parses --kind command --glob "config/*.json" \
    --program python3 --arg=-c --arg="import json,sys;[json.load(open(p)) for p in sys.argv[1:]]"
fl transition add --project 1 --name launch --from review --to done --regret high --gate 1
fl check launch --project 1
```

`fl --help` lists every command.

### Where the store lives

`fl` resolves which store to open, in order:

1. `--db <path>` on the command line.
2. `$FL_DB`.
3. The store bound to the current project in `$XDG_CONFIG_HOME/fl/config.toml` (default
   `~/.config/fl/config.toml`) — the entry whose `root` is the longest ancestor of the
   current directory. For `fl project add <path>`, the project is `<path>`, not the current
   directory.
4. The default: `$XDG_DATA_HOME/fl/fl.redb`, or `~/.local/share/fl/fl.redb`. An empty or
   relative `$XDG_DATA_HOME` is ignored, as is an empty or relative `$XDG_CONFIG_HOME`.

Binding a project keeps its store path out of the repository — a store path belongs to a
machine, and a public repository would publish it:

```toml
# $XDG_CONFIG_HOME/fl/config.toml (default ~/.config/fl/config.toml)
[[project]]
root = "/home/you/code/app"
store = "/home/you/.local/share/fl/app.redb"
```

A full IRI on the command line (rather than a handle such as `3`) selects the store that
holds it, searching the bound store and every store any project is bound to. `--db` and
`$FL_DB` stop that search: they confine the command to the one store they name, and an IRI
that store does not hold is refused, not looked for elsewhere.

A store holds hub pairing codes, session hashes and other records, so it is private to your
Unix user. A store file `fl` creates is mode `0600`, and a store directory `fl` creates is mode
`0700` (only the innermost directory that `fl` makes; the directories above it, such as
`~/.local/share`, keep the default mode), whatever your umask is. `fl` never changes the mode of
a file or directory that already exists.

If an existing store is open to other users, `fl` still opens it and prints a warning with the
fix. The warning is for a person at a terminal: `fl` prints it only when stderr is a terminal,
so hooks, MCP servers, scripts and CI stay quiet. `fl hub serve` is the exception: it records
the warning in the service log on every start, terminal or not. Each path is warned about once
per process. To silence it everywhere, including the hub, set `FL_QUIET_STORE_PERMS=1` (any
non-empty value except `0`).

* A store file with a group or other permission bit is reported: `chmod 600 <store file>`. The
  path in the command is absolute, without `.` or `..`, and in single quotes, so it pastes safely
  whatever the directory is called. A path with a control character or bytes that are not UTF-8
  gets no command: the warning shows it escaped and tells you to run the `chmod` yourself.
* Its directory is reported only when the file is loose as well, or when your group or other
  users can write in the directory: `chmod 700 <directory>`. A directory that others can only
  enter or list, around a `0600` file, is not a finding, because they cannot read the file
  through it. A directory with the sticky bit (such as `/tmp`), the current directory and its
  parents are never reported.

A store configured inside a repository's `.fl/` directory makes a `.fl` that `fl` creates mode
`0700`, so other local users lose access to the tracked files in that directory (the manifest,
the MCP catalog). Git does not track directory modes, so a checkout elsewhere is unaffected; to
share the directory on this machine, `chmod 755 .fl` yourself.

---

## Next horizon

* **Native desktop and mobile packaging** for FerroDeck (`Tauri 2`) alongside the browser and
  iPad PWA served by `fl hub serve`.
* **Cross-tailnet multi-hub federation** for `fl wire` and `fl hub` across distributed lab
  hosts.
* **Cherry-pick binary installer** for installing `fl` and optional components without building
  from source.

---

## Documentation

* [docs/getting-started.md](docs/getting-started.md) — build the CLI and gate a real action, with real commands and their output.
* [docs/sharing-gates.md](docs/sharing-gates.md) — share a project's gates with other machines through a committed manifest.
* [docs/github-tracker.md](docs/github-tracker.md) — keep a project's records and findings in a GitHub repository's Issues: binding, credentials, repair, conflicts and limits.
* [docs/github-ledger.md](docs/github-ledger.md) — publish each decision's evidence to the tracker's repository and comment it on its issue.
* [docs/routing.md](docs/routing.md) — keep developer-level items in the local store and human-level items in GitHub: areas, the routing map, handles and lists.
* [docs/mcp.md](docs/mcp.md) — one committed catalog of a project's MCP servers, written into each agent CLI's own file: the catalog, secrets, switches, commands and refusals.
* [docs/README.md](docs/README.md) — index of the public documentation directory.
* [AGENTS.md](AGENTS.md) — working rules, core stack matrix, and file-naming conventions for agents and contributors.
* [WORKFLOW.md](WORKFLOW.md) — development workflow, review requirements, and commit conventions.
* [PERSONA.md](PERSONA.md) — developer persona and domain expertise.

---

## License

Licensed under [Apache-2.0](LICENSE).
