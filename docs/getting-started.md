# Getting started

This walks through gating one real action — a config file has to parse before you're
allowed to call a build "ready to launch" — from an empty database to a refused launch and
back to an allowed one. Every command below was actually run to produce the output shown.

The tool has no ratified name yet. Its binary is `flctl`; this document calls it "the tool"
or `flctl` and nothing else.

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
   Compiling fl-cli v0.1.0 (/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/crates/cli)
    Finished `release` profile [optimized] target(s) in 0.82s
```

The binary is at `target/release/flctl`.

```
$ target/release/flctl --version
flctl 0.1.0
```

Every command below passes `--db <path>` explicitly so the walkthrough doesn't touch
whatever store you already have. In real use you can skip it: `flctl` falls back to
`$FL_DB`, then an XDG data directory, creating whichever directory it lands on.

```
$ flctl --help
Gate an action before it costs you

Usage: flctl [OPTIONS] <COMMAND>

Commands:
  project     
  gate        
  transition  
  record      
  check       Evaluate a transition's gates and exit per the check contract
  finding     
  attempt     Run one adapter attempt against a record and record the outcome
  stats       Report what a project's recorded attempts cost
  help        Print this message or the help of the given subcommand(s)

Options:
      --db <DB>  Path to the store. Falls back to $FL_DB, then the XDG data directory
  -h, --help     Print help
  -V, --version  Print version
```

## 2. Register a project

A project is a git working tree. The tool refuses anything that isn't one, because a
gate's provenance is a commit — there has to be a HEAD to stamp it against.

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

$ flctl --db /tmp/gs-demo/store.redb project add /tmp/gs-demo/project
1	/tmp/gs-demo/project
```

The `1` is the project's id. Everything below uses `--project 1`.

## 3. Write a gate

A gate names a glob (the population it examines) and a program to run against every
matching file. Here the population is every `config/*.json` file, and the program is
`python3 -c '...'` — a validator that parses each one as JSON and fails loudly if it can't.

```
$ flctl --db /tmp/gs-demo/store.redb gate add \
    --project 1 --name config-parses --kind command --glob "config/*.json" \
    --program python3 --arg=-c \
    --arg="import json,sys;[json.load(open(p)) for p in sys.argv[1:]]" \
    --authored-by "you"
2	config-parses	324e0d5c85dbc9b6a914aa4d49e57f87b597414b
```

The `2` is the gate's id, and the hash after it is the commit the gate is stamped against
— the commit that was HEAD when you authored it.

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
$ flctl --db /tmp/gs-demo/store.redb transition add \
    --project 1 --name launch --from review --to done --regret high --gate 2
launch
```

## 5. Check before the costly action

```
$ flctl --db /tmp/gs-demo/store.redb check launch --project 1
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

$ flctl --db /tmp/gs-demo/store.redb check launch --project 1
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
$ flctl --db /tmp/gs-demo/store.redb check no-such-transition --project 1
error: selector is not valid: project 1 declares no transition named `no-such-transition`. Add it with `flctl transition add`, or name one of the existing ones.
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

$ flctl --db /tmp/gs-demo/store.redb check launch --project 1
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
$ flctl --db /tmp/gs-demo/store.redb gate affirm 2 --by you
2	18da2e3ac0ac4353119805fba23f143ee5215e88

$ flctl --db /tmp/gs-demo/store.redb check launch --project 1
PASS	config-parses	1 examined	25ms
$ echo "exit: $?"
exit: 0
```

## 9. Why an empty population is a failure, not a pass

Delete the only file the gate examines:

```
$ rm config/settings.json
$ git add -A && git commit -qm "removed the config"

$ flctl --db /tmp/gs-demo/store.redb check launch --project 1
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

## Where this leaves you

You now have the whole loop: `project add` → `gate add` (a glob and a program) →
`transition add --regret high` → `check` before the costly action, refused on a real
defect, refused again on staleness, and refused a third way on a population that
vanished. Point a gate's `--glob` and `--program` at whatever your own expensive action
actually depends on, and wire it into a transition the same way.
