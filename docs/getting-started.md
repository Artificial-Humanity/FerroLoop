# Getting started

This walks through gating one real action — a config file has to parse before you're
allowed to call a build "ready to launch" — from an empty database to a refused launch and
back to an allowed one. Every command below was actually run to produce the output shown.

The product is **FerroLoop**. The command you type is `fl` — a short handle, the way
Claude Code's is `claude`. This document uses "FerroLoop" in prose and `fl` in every
transcript.

**Versions this document's output came from:**

```
$ rustc --version
rustc 1.98.1 (48a229cea 2026-09-01)
$ cargo --version
cargo 1.98.1 (797e8a9bc 2026-08-05)
$ git --version
git version 2.53.0
$ python3 --version
Python 3.14.4
```

`python3` is used below as the validator inside one example gate — any program that exits
`0` on success and non-zero on failure works; the tool never parses its output. Check that
whatever program you point a gate at actually exists on your machine before you rely on it.

## 1. Build it

From a checkout of this repository:

```
$ cargo build --release
   Compiling fl-cli v0.1.0 (/path/to/FerroLoop/crates/cli)
    Finished `release` profile [optimized] target(s) in 0.82s
```

The binary is at `target/release/fl`.

```
$ target/release/fl --version
fl 0.1.0 (abc123456789)
```

Every command below passes `--db <path>` explicitly so the walkthrough doesn't touch
whatever store you already have. In real use you can skip it: `fl` falls back to
`$FL_DB`, then an XDG data directory, creating whichever directory it lands on. A store file
it creates is mode `0600` and a directory it creates is `0700`; an existing store or directory is
never changed, only warned about at a terminal when others can reach it (see [Where the store
lives](#where-the-store-lives) below).

```
$ fl --help
Gate an action before it costs you

Usage: fl [OPTIONS] <COMMAND>

Commands:
  project     
  gate        
  transition  
  record      
  check       Evaluate a transition's gates and exit per the check contract
  finding     
  attempt     Run one adapter attempt against a record and record the outcome
  stats       Report what a project's recorded attempts cost
  manifest    Share a project's gates through a committed manifest
  github      GitHub tracker: who fl writes as, and repair of a diverged issue
  routing     Route a project's new items between its local store and GitHub
  mcp         A project's MCP servers: one committed catalog, written into each agent CLI's file
  agent       Manage the workspace agent and coverage-area registry
  role        Manage the configurable role registry (builtins plus custom roles)
  state       Manage durable memories, handoffs, runtime state, and git state sync
  wire        Send, read, acknowledge, and relay FerroWire agent messages
  flow        Author, inspect, and execute directed workflow graphs
  arch        Initialize, inspect, check, and export the declarative architecture model
  hub         Run and manage the FerroLoop Hub daemon and FerroDeck command center
  host        Run and manage the native FerroLoop Agent Host (`fl-host`) daemon and sessions
  store       Look at, and tend, the engine file behind the bound store
  recovery    Inspect recovery metadata without opening stores
  vault       Manage FerroGate zero-knowledge secrets vault, credential proxy, and declarative MCP gateway [alias: gateway]
  auth        Owner warrants: signed, scoped, expiring grants of Owner authority (`grant`, `verify`, `list`, `revoke`)
  watch       FerroWatch: telemetry rollups, agent liveness and alerts over the workspace store
  browse      FerroBrowse: a token-efficient page snapshot and a UI and accessibility gate (no store)
  version     Print how this build was made and the versions it speaks (same as `--version [--json]`)
  install     Plan an install of the FerroLoop products (`--dry-run`), or apply it as a journaled transaction (`--yes`)
  doctor      Check this install's health and change nothing (exit 1 when a check fails)
  help        Print this message or the help of the given subcommand(s)

Options:
      --db <DB>  Path to the store. CONFINES the command to it: an IRI it does not hold is refused, not searched for elsewhere. Falls back to $FL_DB (same), then the project bound in $XDG_CONFIG_HOME/fl/config.toml (default ~/.config/fl/config.toml), then the XDG data directory
  -h, --help     Print help
  -V, --version  Print version
```

## 2. Register a project

A project is a directory. `fl project add` registers any directory, a Git repository or not.
A gate's provenance is a commit, though, so the gate commands (`gate add`, `check`, flows,
manifests) need the project to be a git working tree with a `HEAD` to stamp against, and say so
when it is not. This guide's project is one.

```
$ mkdir -p /tmp/gs-demo/project && cd /tmp/gs-demo/project
$ git init -q
$ git config user.email "you@example.com"
$ git config user.name "You"
$ mkdir config
$ cat > config/settings.json <<'EOF'
{
  "epochs": 10,
  "learning_rate": 0.001
}
EOF
$ git add -A && git commit -qm "initial config"

$ fl --db /tmp/gs-demo/store.redb project add /tmp/gs-demo/project
1	/tmp/gs-demo/project
```

The `1` is the project's **handle**: a short name for it, local to this store. Everything
below uses `--project 1`. Every item also has a full id, an IRI such as
`urn:uuid:0199…`, which is what the store keeps and what every reference between items
holds. A command accepts either; the handle is only there to be typed and read.

### Create a project in one step: `fl project create`

There are two kinds of project. A **directory-backed project** is a directory, whether or not it
is a Git repository. A **virtual group** is a name for a set of agents, with no directory, no
store of its own and no Git, such as `council`. FerroWire reaches its members as `@<slug>`:
every active agent whose project or extra areas include the group. A bare `<slug>` stays the
persona of that name, so `ops` is the persona and `@ops` the group. `@council` reaches the
`council` group's members and also every strategist; `@all` is everyone, so a group cannot be
named `all`. A send to `@<name>` that is not a virtual group is refused, naming the way out
(`@area:<slug>` reaches the agents of a project with a path).

```
fl project create <name> --path <dir> [--slug <slug>] [--git-init]
                  [--github <owner/repo>] [--credential <app|env>] [--purpose <text>]
                  [--by <author>] [--json]
fl project create <name> --virtual [--slug <slug>] [--purpose <text>] [--by <author>] [--json]
fl project init   <dir>  [--name <name>] ...    the same command; the argument is the directory
fl project add    <dir>                         register a directory in the current store
fl project list   [--json]
fl project delete <slug> [--reassign-to <target_slug> | --decommission-agents]
                  [--delete-store] [--delete-dir] [--by <author>] [--json]
```

`<name>` is the name shown for the project (`"My Project"`); its **slug** is the name in
lowercase kebab-case (`my-project`) unless `--slug` gives one. A slug is 1 to 64 of lowercase
letters, digits, `-` and `_`, starting with a letter, and it never changes. `fl project init
<dir>` is `create` under its earlier name: with neither `--path` nor `--virtual` the argument
is the directory, and the slug is, in this order: `--slug`; the slug the config already gives
that root; the slug of `--name`; the directory's lowercase name. A slug names one kind: a project cannot take a virtual
group's slug, nor a group a project's.

A directory-backed project is made in this order, leaving what is already right alone:

1. **Directory.** The path is made if it does not exist. **Git is not touched** unless
   `--git-init` is given: then a directory with no `.git` of its own gets `git init` (also when it
   lies inside another repository, which is left alone), and a repository with no commit gets an
   empty one, message `chore: initial commit`, authored by `--by` (default `fl`). A directory
   with its own `HEAD` is never changed.
2. **Stores.** The project's store `<slug>.fls` goes beside the workspace store (the
   `[state].store` of the config, else `$XDG_DATA_HOME/fl`). It holds the project and its
   routing map: the areas `backlog`, `code`, `design`, `product`, `security`, `tests` and `ux`,
   all routed to the local tier, `security` marked sensitive. A map the store already has is
   kept. The workspace store (`--db` or `$FL_DB` names another) gets the project's area, with
   the name, the root as its path and `--purpose` (default `Project <slug>`; an existing area
   keeps its purpose when none is given).
3. **MCP catalog.** `<root>/.fl/mcp.toml` is written if it is absent, and never changed if it
   exists.
4. **Config.** A `[[project]]` block (`name` is the slug, `root`, `store`) is appended to
   `$XDG_CONFIG_HOME/fl/config.toml`, the rest of the file untouched. The block is written last,
   by replacing the file whole, so a failure earlier leaves the project unregistered and a re-run
   finishes it. `--github owner/repo` adds a `[project.tracker]` with `credential` `app` (the
   default) or `env`; `--credential` needs `--github`. A project that has no tracker gets the one
   asked for; creating never rebinds one that has a tracker (the Hub's
   `PATCH /api/v1/projects/<slug>` does).

A virtual group is one write: its area, with no path, in the workspace store. No directory, no
store file, no config block and no Git; `--virtual` refuses `--path`, `--git-init`, `--github`
and `--credential`. An area made earlier with `fl agent register --create-area` has no path
either, so it is a virtual group already; to make a directory-backed project of that slug, first
give the area its directory with `fl agent area set <slug> --path <dir> --purpose <text> --by
<you>`.

Nothing is changed until the request is checked. fl refuses an empty path, a NUL, a path that is
a file, a slug that another project's block already has (at another root), a root that is already
registered under another slug, a store that holds another project, a root that contains the
stores' directory, and a slug that is the other kind. Running the same command again changes
nothing. `--json` prints the project as the Hub lists it (`slug`, `name`, `kind`, `path`,
`has_git`, `purpose`, `tracker`, `store_path`, `active_agent_count`) plus `registered`, `git`
(`null` unless `--git-init` ran), `mcp_catalog_created`, `areas` and `config`.

`fl project list` shows every directory-backed project in the config and every virtual group,
one per line: kind, slug, path (`-` for a group), tracker repository (`-` for none) and the number
of active agents whose project or extra areas name it. With `--json` it prints the list that
`GET /api/v1/projects` serves ([hub-home-and-artifacts.md](hub-home-and-artifacts.md)), which
also creates (`POST`), updates (`PATCH`) and deletes (`DELETE`) them.

### Delete a project: `fl project delete`

`fl project delete <slug>` removes a project's `[[project]]` block from the config and its area
from the workspace store, and deletes no file. A virtual group loses its area only. A slug that
names nothing is refused (exit 1).

* **Agents.** A project whose active agents (project or extra areas equal the slug or the area's
  name, ignoring case) are not dealt with is refused with exit 2, naming them, and nothing
  changes. `--reassign-to <target_slug>` moves them to that project (a project or a virtual group
  that exists, and not this one; a Lead is refused when the target already has one).
  `--decommission-agents` retires them as `fl agent decommission --mode teardown` does. The two
  flags exclude each other.
* **`--delete-store`** also deletes the project's store file and its `<store>.sock` and
  `<store>.migrate.lock` files. The workspace store and another project's store are refused.
* **`--delete-dir`** also deletes the project's directory, after these checks, all made before
  anything changes. The directory is the registered one; it must exist, be a directory and not
  a symbolic link, resolve to a path of at least three components, and agree with the path the
  config, the area and the workspace store record. It must not be `/`, the home directory or a
  directory above it, the workspace root or a directory above it (the root of the current
  directory's workspace), a directory that holds another registered project, a directory that
  holds or lies inside the stores directory or the config directory, or one that holds a store
  or config file. A virtual group refuses `--delete-store` and `--delete-dir`.

The order is: check everything, reassign or decommission the agents, remove the config block and
the area, delete the store files, delete the directory. A failure to delete the store files or
the directory leaves the project unregistered and is printed as `error` (the flag that did not
happen stays `false`). A Hub lists a project from a store file in its data directory, so it
still lists a project deleted without `--delete-store` there, with no path. `--json` prints the
body `DELETE /api/v1/projects/:slug` answers:

```
{"deleted":"my-project","reassigned_agents":0,"decommissioned_agents":0,
 "store_deleted":false,"dir_deleted":false}
```

An agent started with `--project <slug>` of a virtual group (an area with no path), without
`--cwd`, runs in the workspace root; the project of a directory is left as the request says.

## 3. Write a gate

A gate names a population — the things it must examine — and a program to run against
them. A glob is one of three ways to name that population; [section 13](#13-three-ways-to-name-a-population)
covers the other two. Here the population is every `config/*.json` file, and the program is
`python3 -c '...'` — a validator that parses each one as JSON and fails loudly if it can't.

```
$ fl --db /tmp/gs-demo/store.redb gate add \
    --project 1 --name config-parses --kind command --glob "config/*.json" \
    --program python3 --arg=-c \
    --arg="import json,sys;[json.load(open(p)) for p in sys.argv[1:]]" \
    --authored-by "you"
1	config-parses	324e0d5c85dbc9b6a914aa4d49e57f87b597414b
```

The `1` is the gate's handle, and the hash after it is the commit the gate is stamped
against — the commit that was HEAD when you authored it. Handles are counted per kind of
item, so this is gate `1` although project `1` already exists. The gate's full id, the
IRI, is the `id` field that `gate show 1` prints.

⚠ **A trap worth knowing before you hit it.** `--arg` takes any number of values, so
`--arg -c` (two separate words) makes clap read `-c` as a new flag, not as `--arg`'s value,
and the command is refused with `unexpected argument '-c' found`. Use `--arg=-c` — the
`=` form — for any value that itself looks like a flag. Values that don't start with `-`
work fine either way.

## 4. Wire the gate into a transition

A transition names the states it moves between, the gates that must pass first, and a
**regret** level. `high` regret means: if the gate's stamp has fallen behind HEAD for
anything the gate actually covers, that counts as a failure, not just a warning.

```
$ fl --db /tmp/gs-demo/store.redb transition add \
    --project 1 --name launch --from review --to done --regret high --gate 1
launch
```

## 5. Check before the costly action

```
$ fl --db /tmp/gs-demo/store.redb check launch --project 1
PASS	config-parses	1 examined	25ms
$ echo "exit: $?"
exit: 0
```

`check` is what you run in place of the expensive action's launch step. Exit `0` means
every gate passed — go ahead.

## 6. Watch it catch a defect

This is the failure the tool exists to prevent: a config that was fine when you set the
gate up, broken by a later commit, and never looked at again before the run started.

```
$ cat > config/settings.json <<'EOF'
{
  "epochs": 10,
  "learning_rate": 0.001,,
}
EOF
$ git add -A && git commit -qm "broke the config"

$ fl --db /tmp/gs-demo/store.redb check launch --project 1
FAIL	config-parses	predicate, 1 examined	75ms  (stale)
	| Traceback (most recent call last):
	|   File "<string>", line 1, in <module>
	|     import json,sys;[json.load(open(p)) for p in sys.argv[1:]]
	|                      ~~~~~~~~~^^^^^^^^^
	|   File "/usr/lib/python3.14/json/__init__.py", line 298, in load
	|     return loads(fp.read(),
	|         cls=cls, object_hook=object_hook,
	|         parse_float=parse_float, parse_int=parse_int,
	|         parse_constant=parse_constant, object_pairs_hook=object_pairs_hook, **kw)
	|   File "/usr/lib/python3.14/json/__init__.py", line 352, in loads
	|     return _default_decoder.decode(s)
	|            ~~~~~~~~~~~~~~~~~~~~~~~^^^
	|   File "/usr/lib/python3.14/json/decoder.py", line 345, in decode
	|     obj, end = self.raw_decode(s, idx=_w(s, 0).end())
	|                ~~~~~~~~~~~~~~~^^^^^^^^^^^^^^^^^^^^^^^
	|   File "/usr/lib/python3.14/json/decoder.py", line 361, in raw_decode
	|     obj, end = self.scan_once(s, idx)
	|                ~~~~~~~~~~~~~~^^^^^^^^
	| json.decoder.JSONDecodeError: Expecting property name enclosed in double quotes: line 3 column 26 (char 43)
$ echo "exit: $?"
exit: 1
```

The launch is refused. `FAIL` names the gate (`config-parses`) and the reason (`predicate`
— the program ran over a real, non-empty population and returned non-zero), and the
indented lines are the program's own output, so you can see exactly what it objected to.

## 7. Exit codes

| Code | Meaning |
|---|---|
| `0` | Every gate passed. |
| `1` | At least one gate failed, or an empty population, or staleness. The command ran correctly and told you no. |
| `2` | The check itself couldn't run as asked — a broken instrument, a bad argument, a project or transition that doesn't exist. Not an answer about your code at all. |

An exit-`2` example — asking for a transition that was never declared:

```
$ fl --db /tmp/gs-demo/store.redb check no-such-transition --project 1
error: selector is not valid: the project at /tmp/gs-demo/project declares no transition named `no-such-transition`. Add it with `fl transition add`, or name one of the existing ones.
$ echo "exit: $?"
exit: 2
```

## 8. Staleness and `gate affirm`

Fix the config — but change it, don't just put back exactly what was there before:

```
$ cat > config/settings.json <<'EOF'
{
  "epochs": 12,
  "learning_rate": 0.001
}
EOF
$ git add -A && git commit -qm "fixed the config, raised epochs"

$ fl --db /tmp/gs-demo/store.redb check launch --project 1
FAIL	config-parses	stale, 1 examined	25ms  (stale)
$ echo "exit: $?"
exit: 1
```

The file parses fine now, but the launch is *still* refused. The gate was stamped at the
commit where you ran `gate add`; two commits have happened to `config/settings.json`
since, and the gate never looked at either one. At `--regret high`, that counts as a
failure on its own — a pass whose provenance is behind the code it's supposed to cover is
not trustworthy just because the predicate happens to be true right now.

`gate affirm` is how you clear that, and it means exactly one thing: **a person looked at
the gate against the current commit and confirms it's still the right check.** It does not
re-run anything and does not inspect the diff for you — it only re-stamps the gate's
provenance. Run it after you've actually looked, not as a way to silence the warning.

```
$ fl --db /tmp/gs-demo/store.redb gate affirm 1 --by you
1	18da2e3ac0ac4353119805fba23f143ee5215e88

$ fl --db /tmp/gs-demo/store.redb check launch --project 1
PASS	config-parses	1 examined	25ms
$ echo "exit: $?"
exit: 0
```

## 9. Why an empty population is a failure, not a pass

Delete the only file the gate examines:

```
$ rm config/settings.json
$ git add -A && git commit -qm "removed the config"

$ fl --db /tmp/gs-demo/store.redb check launch --project 1
FAIL	config-parses	empty_population, 0 examined	0ms  (stale)
	| population 0 is below the declared floor of 1
$ echo "exit: $?"
exit: 1
```

The gate's own program was `python3 -c '...'` over the files the glob matches — and with
no files left, that program never even ran; there was nothing to hand it. A command over
zero inputs is trivially true (there's a reason `true` exits `0`), and if the tool read
that as a pass, deleting the thing a gate checks would be a way to make the gate green
instead of a way to fail it. `empty_population` is a distinct failure reason so this reads
as exactly what it is: nothing was examined, so nothing was verified, so the launch is not
allowed. A no-run is not a pass, no matter what ran or didn't.

## 10. The other half: a claim needs a reproduction

Everything above is one person gating their own action. The rest of the tool is for the
case where somebody *else* says your code is wrong.

The rule is one sentence: **a claim is not actionable until a check fails because of it.**
Not because reviewers are untrustworthy, but because agreement is free. Saying "good catch,
fixing that now" costs nothing and proves nothing, and a repair generated from a claim's
*wording* lands in the right file with the wrong content. A failing check is the only thing
that can tell you the defect is real, and later, that it is gone.

Findings attach to a **record** — a unit of work, the thing a finding is about.

```
$ fl --db /tmp/gs-demo/store.redb record add --project 1 --title "tune the learning rate"
1	tune the learning rate
```

Section 9 deleted the config, so put it back — this time with a learning rate that is
negative, which is the defect a reviewer is about to claim:

```
$ cat > config/settings.json <<'EOF'
{
  "epochs": 12,
  "learning_rate": -0.001
}
EOF
$ git add -A && git commit -qm "restored the config"

$ fl --db /tmp/gs-demo/store.redb gate affirm 1 --by you
1	16bc83526da0269acbd3a135ab6317d98d7d670a
```

Now have a reviewer raise a claim about it:

```
$ fl --db /tmp/gs-demo/store.redb finding raise --record 1 \
    --claim "the validator accepts a negative learning_rate" --by reviewer
1	raised	the validator accepts a negative learning_rate
```

`raised` is as far as that gets on its own. Try to hand it to somebody to fix:

```
$ fl --db /tmp/gs-demo/store.redb finding assign 1 --to fixer
error: this finding has no reproduction, so it cannot be assigned. Attach a check that fails because of the defect, or withdraw the finding.
$ echo "exit: $?"
exit: 2
```

Two ways forward, and the refusal names both. Attach a reproduction, or withdraw it.

### A passing gate is not a reproduction

The obvious move is to point at a check you already have:

```
$ fl --db /tmp/gs-demo/store.redb finding reproduce 1 --gate 1
error: gate `config-parses` currently PASSES over 1 items, so it is not a reproduction. A check that already passes cannot tell you whether the defect is absent or whether the check simply does not exercise it. Write one that fails because of the defect.
$ echo "exit: $?"
exit: 2
```

That refusal is the load-bearing one. A green check attached to a claim is worse than no
check, because later it will go on being green and be read as proof the defect was fixed —
when all it ever proved was that it never looked. So write a gate that fails *now*, for the
reason in the claim:

```
$ fl --db /tmp/gs-demo/store.redb gate add \
    --project 1 --name rate-positive --kind command --glob "config/*.json" \
    --program python3 --arg=-c \
    --arg="import json,sys;[sys.exit('learning_rate must be > 0') for p in sys.argv[1:] if json.load(open(p))['learning_rate'] <= 0]" \
    --authored-by reviewer
2	rate-positive	16bc83526da0269acbd3a135ab6317d98d7d670a

$ fl --db /tmp/gs-demo/store.redb gate run 2
FAIL	rate-positive	predicate, 1 examined
$ echo "exit: $?"
exit: 1
```

It fails, so it is admissible:

```
$ fl --db /tmp/gs-demo/store.redb finding reproduce 1 --gate 2
1	reproduced	gate 2 failed over 1 items

$ fl --db /tmp/gs-demo/store.redb finding assign 1 --to fixer
1	assigned	fixer
```

A reproduction and a gate are the same object. The check written to prove a defect exists
is the check that stays behind afterwards to prove it has not come back — which is why
there is no separate concept for one.

## 11. A repair that breaks something else is not done

Say there is a second gate on the project, green today — the sort of check that exists
because somebody once got bitten:

```
$ fl --db /tmp/gs-demo/store.redb gate add \
    --project 1 --name epochs-present --kind command --glob "config/*.json" \
    --program python3 --arg=-c \
    --arg="import json,sys;[sys.exit('epochs is missing') for p in sys.argv[1:] if 'epochs' not in json.load(open(p))]" \
    --authored-by you
3	epochs-present	16bc83526da0269acbd3a135ab6317d98d7d670a

$ fl --db /tmp/gs-demo/store.redb gate run 3
PASS	epochs-present	1 examined
```

Now the fixer rewrites the config, makes the learning rate positive, and drops `epochs` on
the way past:

```
$ cat > config/settings.json <<'EOF'
{
  "learning_rate": 0.001
}
EOF
$ git add -A && git commit -qm "fix: make the learning rate positive"

$ fl --db /tmp/gs-demo/store.redb finding verify 1
REPRODUCTION	passes over 1 items  (stale: the gate's population moved since it was stamped)
NEIGHBOURS	2 checked, 1 regressed
REGRESSION	epochs-present	FAIL	predicate, 1 examined  (stale: the gate's population moved since it was stamped)
NEIGHBOUR	config-parses	passes  (stale: the gate's population moved since it was stamped)
OPEN	1	the repair is not done
$ echo "exit: $?"
exit: 1
```

The reported defect is genuinely fixed — `REPRODUCTION passes`. The finding still does not
close. `verify` re-runs every other gate on the project that was passing before, and one of
them isn't any more.

This is the failure the whole protocol is built around: the most common defect in a review
loop is not the original bug, it is the bug introduced by the fix to it — right file, wrong
content, and nobody looks at the neighbours because the reported thing now works. Note that
`NEIGHBOURS 2 checked, 0 regressed` and a run that checked nothing at all can never print
the same line, so "no regressions" is always distinguishable from "no neighbours were run."

Keep `epochs` this time:

```
$ cat > config/settings.json <<'EOF'
{
  "epochs": 12,
  "learning_rate": 0.001
}
EOF
$ git add -A && git commit -qm "fix: make the learning rate positive, keep epochs"

$ fl --db /tmp/gs-demo/store.redb finding verify 1
REPRODUCTION	passes over 1 items  (stale: the gate's population moved since it was stamped)
NEIGHBOURS	2 checked, 0 regressed
NEIGHBOUR	config-parses	passes  (stale: the gate's population moved since it was stamped)
NEIGHBOUR	epochs-present	passes  (stale: the gate's population moved since it was stamped)
CLOSED	1
$ echo "exit: $?"
exit: 0
```

The finding closed because a program said so. Nobody was asked whether they were finished.

## 12. A claim that cannot be reproduced gets withdrawn

The other exit exists because a review that cannot kill its own findings just accumulates
them. Some claims are taste, and taste has no failing check:

```
$ fl --db /tmp/gs-demo/store.redb finding raise --record 1 \
    --claim "the config layout feels wrong" --by reviewer
2	raised	the config layout feels wrong

$ fl --db /tmp/gs-demo/store.redb finding withdraw 2 \
    --reason "no reproduction is possible: this is taste, not a defect"
2	withdrawn	no reproduction is possible: this is taste, not a defect
```

Withdrawal is not free. It is counted, and it is counted against whoever raised the claim:

```
$ fl --db /tmp/gs-demo/store.redb finding list --project 1
1	fixed	reviewer	the validator accepts a negative learning_rate
2	withdrawn	reviewer	the config layout feels wrong
reviewer	withdrawn: 1
```

Both directions have a price. Raising something you cannot demonstrate shows up under your
name; so the cheap move — raise everything, let the fixer sort it out — stops being cheap.

One thing to know about the value passed to `--state`: it is the same spelling the tool
prints, and nothing else is accepted.

```
$ fl --db /tmp/gs-demo/store.redb finding list --project 1 --state Withdrawn
error: `Withdrawn` is not a finding state. Valid states are: raised, reproduced, assigned, fixed, withdrawn.
$ echo "exit: $?"
exit: 2
```

Everything that crosses the boundary — printed text, stored bytes, JSON, and values you
type back in — uses one snake_case spelling. The list in that refusal is generated from the
same source the parser reads, so it can never offer you a value that would then be rejected.

## 13. Three ways to name a population

Every gate so far used `--glob`. There are three, and a gate must pick exactly one — there
is no default, because a gate that does not say what it examines is the empty-population
failure waiting to happen.

```
$ fl --db /tmp/gs-demo/store.redb gate add --project 1 --name nameless --program true
error: the following required arguments were not provided:
  <--glob <GLOB>|--changed-since <REF>|--population-from <PROGRAM>>

Usage: fl gate add --project <PROJECT> --name <NAME> --program <PROGRAM> <--glob <GLOB>|--changed-since <REF>|--population-from <PROGRAM>>

For more information, try '--help'.
$ echo "exit: $?"
exit: 2
```

**`--changed-since <REF>`** examines whatever differs from a git ref, so the gate's cost
tracks the size of the change rather than the size of the repository:

```
$ fl --db /tmp/gs-demo/store.redb gate add \
    --project 1 --name touched-recently --kind command --changed-since HEAD~1 \
    --program python3 --arg=-c \
    --arg="import json,sys;[json.load(open(p)) for p in sys.argv[1:] if p.endswith('.json')]" \
    --authored-by you
4	touched-recently	f8044e6e44d6af8fedb5805527d4fcf65e9ea6f8

$ fl --db /tmp/gs-demo/store.redb gate run 4
PASS	touched-recently	1 examined
```

⚠ Read that `1 examined` before you trust it. A `--changed-since` population shrinks when
the change is small — which is the point — but it also shrinks to nothing when there is no
change at all, and a gate over nothing fails. That is the same rule as everywhere else, and
it is the reason this selector is safe to use: it cannot quietly examine less than it claims.

**`--population-from <PROGRAM>`** takes the population from a program's stdout, one path per
line, run in the project root. Use it when neither a glob nor a diff says what you mean —
here, "the config files git actually tracks":

```
$ cat > list-configs.sh <<'EOF'
#!/bin/sh
git ls-files 'config/*.json'
EOF
$ chmod +x list-configs.sh

$ fl --db /tmp/gs-demo/store.redb gate add \
    --project 1 --name listed-configs --kind command \
    --population-from /tmp/gs-demo/project/list-configs.sh \
    --program python3 --arg=-c \
    --arg="import json,sys;[json.load(open(p)) for p in sys.argv[1:]]" \
    --authored-by you
5	listed-configs	f8044e6e44d6af8fedb5805527d4fcf65e9ea6f8

$ fl --db /tmp/gs-demo/store.redb gate run 5
PASS	listed-configs	1 examined
```

Pass arguments to it with `--population-arg`, repeatable, and remember the `=` form for a
value that starts with `-`.

### A lister that fails has not told you the population is empty

This is the one trap worth spelling out, because the two outcomes look alike and mean
opposite things. If the listing program exits non-zero, the population is **unknown** — not
empty:

```
$ cat > broken-list.sh <<'EOF'
#!/bin/sh
echo "fatal: not a git repository" >&2
exit 128
EOF
$ chmod +x broken-list.sh

$ fl --db /tmp/gs-demo/store.redb gate add \
    --project 1 --name broken-lister --kind command \
    --population-from /tmp/gs-demo/project/broken-list.sh --program true --authored-by you
6	broken-lister	f8044e6e44d6af8fedb5805527d4fcf65e9ea6f8

$ fl --db /tmp/gs-demo/store.redb gate run 6
ERROR	broken-lister	population command `/tmp/gs-demo/project/broken-list.sh` exited 128, so the population is unknown, not empty: fatal: not a git repository
$ echo "exit: $?"
exit: 2
```

`ERROR` and exit `2`, not `empty_population` and exit `1`. The difference matters because
the two send you to different places: `empty_population` says your file tree has nothing the
gate covers, and would have had you hunting through `config/` for a file that was never the
problem. This says your listing program broke, and hands you its own complaint to read.

## 14. Moving a record is the gated action

`check` tells you whether a transition's gates pass. `record move` performs the state change
those gates exist to protect — so it runs them, and refuses the move if they say no. Running
`check` first is a convenience, not a requirement; the move does not trust you to have done it.

A record starts in `todo`:

```
$ fl --db /tmp/gs-demo/store.redb record list --project 1
1	todo	tune the learning rate
```

Only `review → done` is declared here, as the transition `launch` from
[section 4](#4-wire-the-gate-into-a-transition). Nothing declares `todo → review`, so that
move has nothing to bypass and proceeds — and says which it was, because "allowed" and "not
checked" must not read the same:

```
$ fl --db /tmp/gs-demo/store.redb record move 1 --to review
1	review	ungated: project 1 declares no transition from `todo` to `review`
$ echo "exit: $?"
exit: 0
```

`review → done` is a different matter. `launch` covers it, so `launch` runs:

```
$ fl --db /tmp/gs-demo/store.redb record move 1 --to done
FAIL	launch	config-parses	stale, 1 examined	25ms  (stale)
REFUSED	1	stays `review`
$ echo "exit: $?"
exit: 1
```

Refused, and for the staleness reason from [section 8](#8-staleness-and-gate-affirm) — the
config has moved several times since the gate was last stamped. The exit code is the same `1`
that `check` would have given, and the record has not moved:

```
$ fl --db /tmp/gs-demo/store.redb record list --project 1
1	review	tune the learning rate
```

Look at the gate, affirm it, and the same move goes through:

```
$ fl --db /tmp/gs-demo/store.redb gate affirm 1 --by you
1	f8044e6e44d6af8fedb5805527d4fcf65e9ea6f8

$ fl --db /tmp/gs-demo/store.redb record move 1 --to done
PASS	launch	config-parses	1 examined	25ms
1	done
$ echo "exit: $?"
exit: 0
```

⚠ There is deliberately no `--force`. If a declared transition refuses a move you need to
make, the fix is to change the declaration or fix the gate — both of which leave a record of
what you decided. A bypass flag would leave none, and a gate you can wave through on the
command line is a gate that will be waved through.

## Where this leaves you

You now have both loops.

The gating one: `project add` → `gate add` (a glob and a program) → `transition add
--regret high` → `check` before the costly action, refused on a real defect, refused again
on staleness, and refused a third way on a population that vanished. Point a gate's
`--glob` and `--program` at whatever your own expensive action actually depends on, and
wire it into a transition the same way.

The review one: `record add` → `finding raise` → `finding reproduce` (refused unless the
gate fails) → `finding assign` → `finding verify` (refused unless the reproduction passes
*and* the neighbours still do) → closed by a program, or `finding withdraw`, counted
against whoever raised it.

And they meet at `record move`, which runs whatever transition declares the move it is
asked to perform, and refuses on the same verdict `check` would have printed.

They are the same machinery. A gate is what lets `check` refuse an action, and a gate is
also the only thing that can tell you a fix worked — so a reproduction is just a gate that
was written in response to a claim.

Everything above ran on one machine against one store. When a gate needs to be resolved from
a different checkout — another machine, or a teammate's clone of the same repository — see
[Sharing gates across machines](sharing-gates.md) for `fl manifest export`, `import` and
`check`.

## Where the store lives

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

### Store engine

A store is a FerroStore file by default. The store path you configure stays the same: for a
path that ends in `.redb`, the store lives in the sibling `.fls` file (`fl.redb` is `fl.fls`).
The first open of a store that still has only a `.redb` migrates it. The copy is verified table
by table (row counts and content hashes); if it does not verify, the store stays on the
`.redb`. After a verified copy the `.redb` stays on disk, read-only (mode `0400`), as the
rollback copy.

* `FL_STORE_BACKEND=redb` keeps every store on redb. It is the rollback, and it is one way:
  a process on redb takes no migration lock, and nothing keeps the two files in step. To roll
  back, set the variable for **every** process that uses the store (the hub, the wire daemon,
  every MCP server and every shell): a process without it still reads and writes the `.fls`.
  Then rename the `.fls` aside (for example `mv fl.fls fl.fls.old`) and `chmod 600` the `.redb`.
  Writes made after the migration are in the `.fls` only; writes made after the rollback are in
  the `.redb` only and are lost to the `.fls`. A `.fls` left beside a `.redb` that a process
  opens on redb gets a one-time warning on stderr (it says NEWER when the `.fls` was written
  after the `.redb`); `fl store verify` compares the pair. To move forward again, rename any old
  `.fls` aside before the first open on FerroStore, so that the `.redb` is migrated afresh: a
  stale `.fls` is opened as it is, and the newer `.redb` writes are ignored.
* A `.redb` that another process holds open stays on redb with a one-time warning and no `.fls`
  is made, so the store does not split; the next open after the holder lets go migrates it.
  A migration whose copy does not verify also stays on redb with a one-time warning and no
  `.fls`; `fl store migrate` shows the mismatch.
* A path that ends in `.fls` is always FerroStore.
* `fl store status` shows the engine and the file, `fl store migrate` migrates a store by
  hand, and `fl store verify` compares a `.redb` and `.fls` pair.
* Before the first command on the FerroStore engine, stop any `fl hub` or `fl wire daemon`
  process that still runs on redb, or start it on the FerroStore engine in the same step. Two
  processes that use different engines for one store is the case the migration lock guards
  against.
* The hub migrates only its own store. A project store (`<slug>.redb`) is migrated by the first
  `fl` command that opens it, or by `fl store migrate`; until then the hub does not list that
  project in `available_projects`, and answers `409` for its records.

### Store permissions

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

## Vendors: API models and local engines

`fl attempt <record> --adapter <id>` runs an attempt through any registered vendor, not only the
five subscription CLIs (`claude`, `codex`, `antigravity`, `muse`, `grok`). The other built-in ids
are the direct APIs (`anthropic-api`, `openai-api`, `gemini-api`, `xai-api`, `openrouter-api`,
`kimi-api`) and the local engines (`ollama`, `lemonade`). An API attempt is one request: it sends
the instruction and records the reply, the tokens and the cost. It runs no tools. The attempt is
recorded under the vendor id. A name that is not registered is refused with the list of ids that
are. A workflow step (`fl flow step --adapter <id>`, or a step's `adapter`) takes the same ids,
settings and key lookup, and records the id as the step's adapter.

`[vendors.<id>]` in `config.toml` sets a vendor up. The id is compared without case.

```
# $XDG_CONFIG_HOME/fl/config.toml

# A built-in API vendor: overrides only. They apply when fl builds that vendor's adapter.
[vendors.kimi-api]
base_url = "https://proxy.example.com/v1"   # https, or http to this machine
default_model = "kimi-k2.5"
input_usd_micros_per_mtok = 600000          # also spelled input_usd_micros_per_m
output_usd_micros_per_mtok = 2500000        # also spelled output_usd_micros_per_m

# A new vendor: no recompile.
[vendors.my-vllm]
transport = "http_api"                      # required; "subscription_cli" cannot be added here
dialect = "openai_chat_completions"         # or anthropic_messages, gemini_generate_content
base_url = "http://127.0.0.1:8001/v1"
default_model = "qwen3"
display_name = "Local vLLM"                 # default: the id
env_key = "VLLM_API_KEY"                    # the variable that holds the key
vault_secret = "VLLM_API_KEY"               # the Vault secret; default: env_key
context_window = 32000                      # default 0 (unknown)
mcp_config_rel_path = ".vllm/mcp.json"      # optional; the project MCP file for this vendor
```

* Built-in vendors take `base_url`, `default_model` and the rate card only. They never change
  in the registry. `mcp_config_rel_path`, `dialect`, `env_key`, `vault_secret`,
  `display_name` and `context_window` on a built-in id are errors. A subscription CLI takes
  none of these; its binary and rate card go in `[adapters.<name>]`.
* A new vendor needs `transport`, `dialect`, `base_url` and `default_model`. With neither
  `env_key` nor `vault_secret` it is a keyless local engine.
* `base_url` must be `https://`, or `http://` to `127.0.0.1`, `localhost` or `[::1]`, with no
  user name, query or fragment. fl sends the key there.
* A paid API (a vendor with a key name) needs a rate card, and `output_usd_micros_per_mtok` must
  not be 0: the spend ceiling of an attempt depends on it. fl ships none. A vendor with no key
  name may run with no rate card.
* A config error in `[vendors]` stops every `fl` command, and names the file and the id.

The key is the vendor's environment variable if it is set and not blank. Otherwise fl reads the
vendor's secret from the FerroGate Vault (`fl vault set`), as the agent `fl-exec`, under the
secret's own policy. The secret must list the vendor's host in `--host`, and for a loopback or
LAN engine it needs `--allow-private-network`. If the secret names `--agent` values, `fl-exec`
must be one of them (or `*`). `--method` must allow `POST` if it is set. A secret that is
missing, expired, denied or unreadable, or a Vault that does not exist, gives no key, and the
attempt is refused (exit 1) without a reason that could reveal the secret. fl never creates a
Vault when it looks for a key. Each read is written to `gate-audit.jsonl` beside the Vault.

For the `my-vllm` example, `fl vault set VLLM_API_KEY --stdin --host 127.0.0.1
--allow-private-network` stores a key that only that address can receive.

## Submodules

Six repositories hold standalone extractions of parts of FerroLoop: FerroDeck, FerroStore,
FerroState, FerroWire, FerroTrack and FerroGate. Each is mounted as a git submodule under
`submodules/<Name>`, pinned to a commit of that repository's `main` branch (`.gitmodules` lists
the `https` URLs).

The submodules are optional and the build does not use them. The workspace in `Cargo.toml`
lists `submodules` under `exclude` (so `cargo` run inside one treats it as its own workspace),
`fl arch check` and `fl arch init` skip every path that `exclude` lists (plain paths relative
to the workspace root, as cargo reads them; no globs), and a clone that did not fetch them
builds and tests the same. The crates in `crates/` are the code that is built here; the
submodules only pin the standalone repositories next to it.

```
$ git clone --recurse-submodules https://github.com/Artificial-Humanity/FerroLoop.git
$ git submodule update --init --recursive              # in a clone made without the flag
$ git submodule update --remote submodules/FerroGate   # move one pin to its latest main
$ git submodule status                                 # the pinned commits
```

`git submodule update --remote` changes the pinned commit in your working tree. Commit the
changed `submodules/<Name>` entry to record the new pin.

## Project Director role

Register or spin up an agent with `--role director` for a Project Director who
coordinates a portfolio of projects. Existing `custom:director` registrations
normalize to `director`. Use `@role:director` to reach directors across the
workspace, or `@area:<project>/director` to reach directors covering an area,
including their extra areas. A project's lead remains its first contact; the
role does not grant additional review authority.
