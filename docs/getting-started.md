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
   Compiling fl-cli v0.1.0 (/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/crates/cli)
    Finished `release` profile [optimized] target(s) in 0.82s
```

The binary is at `target/release/fl`.

```
$ target/release/fl --version
fl 0.1.0
```

Every command below passes `--db <path>` explicitly so the walkthrough doesn't touch
whatever store you already have. In real use you can skip it: `fl` falls back to
`$FL_DB`, then an XDG data directory, creating whichever directory it lands on.

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

$ fl --db /tmp/gs-demo/store.redb project add /tmp/gs-demo/project
1	/tmp/gs-demo/project
```

The `1` is the project's id. Everything below uses `--project 1`.

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
$ fl --db /tmp/gs-demo/store.redb transition add \
    --project 1 --name launch --from review --to done --regret high --gate 2
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
error: selector is not valid: project 1 declares no transition named `no-such-transition`. Add it with `fl transition add`, or name one of the existing ones.
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
$ fl --db /tmp/gs-demo/store.redb gate affirm 2 --by you
2	18da2e3ac0ac4353119805fba23f143ee5215e88

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
3	tune the learning rate
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

$ fl --db /tmp/gs-demo/store.redb gate affirm 2 --by you
2	16bc83526da0269acbd3a135ab6317d98d7d670a
```

Now have a reviewer raise a claim about it:

```
$ fl --db /tmp/gs-demo/store.redb finding raise --record 3 \
    --claim "the validator accepts a negative learning_rate" --by reviewer
4	raised	the validator accepts a negative learning_rate
```

`raised` is as far as that gets on its own. Try to hand it to somebody to fix:

```
$ fl --db /tmp/gs-demo/store.redb finding assign 4 --to fixer
error: this finding has no reproduction, so it cannot be assigned. Attach a check that fails because of the defect, or withdraw the finding.
$ echo "exit: $?"
exit: 2
```

Two ways forward, and the refusal names both. Attach a reproduction, or withdraw it.

### A passing gate is not a reproduction

The obvious move is to point at a check you already have:

```
$ fl --db /tmp/gs-demo/store.redb finding reproduce 4 --gate 2
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
5	rate-positive	16bc83526da0269acbd3a135ab6317d98d7d670a

$ fl --db /tmp/gs-demo/store.redb gate run 5
FAIL	rate-positive	predicate, 1 examined
$ echo "exit: $?"
exit: 1
```

It fails, so it is admissible:

```
$ fl --db /tmp/gs-demo/store.redb finding reproduce 4 --gate 5
4	reproduced	gate 5 failed over 1 items

$ fl --db /tmp/gs-demo/store.redb finding assign 4 --to fixer
4	assigned	fixer
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
6	epochs-present	16bc83526da0269acbd3a135ab6317d98d7d670a

$ fl --db /tmp/gs-demo/store.redb gate run 6
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

$ fl --db /tmp/gs-demo/store.redb finding verify 4
REPRODUCTION	passes over 1 items  (stale: the gate's population moved since it was stamped)
NEIGHBOURS	2 checked, 1 regressed
REGRESSION	epochs-present	FAIL	predicate, 1 examined  (stale: the gate's population moved since it was stamped)
NEIGHBOUR	config-parses	passes  (stale: the gate's population moved since it was stamped)
OPEN	4	the repair is not done
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

$ fl --db /tmp/gs-demo/store.redb finding verify 4
REPRODUCTION	passes over 1 items  (stale: the gate's population moved since it was stamped)
NEIGHBOURS	2 checked, 0 regressed
NEIGHBOUR	config-parses	passes  (stale: the gate's population moved since it was stamped)
NEIGHBOUR	epochs-present	passes  (stale: the gate's population moved since it was stamped)
CLOSED	4
$ echo "exit: $?"
exit: 0
```

The finding closed because a program said so. Nobody was asked whether they were finished.

## 12. A claim that cannot be reproduced gets withdrawn

The other exit exists because a review that cannot kill its own findings just accumulates
them. Some claims are taste, and taste has no failing check:

```
$ fl --db /tmp/gs-demo/store.redb finding raise --record 3 \
    --claim "the config layout feels wrong" --by reviewer
7	raised	the config layout feels wrong

$ fl --db /tmp/gs-demo/store.redb finding withdraw 7 \
    --reason "no reproduction is possible: this is taste, not a defect"
7	withdrawn	no reproduction is possible: this is taste, not a defect
```

Withdrawal is not free. It is counted, and it is counted against whoever raised the claim:

```
$ fl --db /tmp/gs-demo/store.redb finding list --project 1
4	fixed	reviewer	the validator accepts a negative learning_rate
7	withdrawn	reviewer	the config layout feels wrong
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
8	touched-recently	f8044e6e44d6af8fedb5805527d4fcf65e9ea6f8

$ fl --db /tmp/gs-demo/store.redb gate run 8
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
9	listed-configs	f8044e6e44d6af8fedb5805527d4fcf65e9ea6f8

$ fl --db /tmp/gs-demo/store.redb gate run 9
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
10	broken-lister	f8044e6e44d6af8fedb5805527d4fcf65e9ea6f8

$ fl --db /tmp/gs-demo/store.redb gate run 10
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
3	todo	tune the learning rate
```

Only `review → done` is declared here, as the transition `launch` from
[section 4](#4-wire-the-gate-into-a-transition). Nothing declares `todo → review`, so that
move has nothing to bypass and proceeds — and says which it was, because "allowed" and "not
checked" must not read the same:

```
$ fl --db /tmp/gs-demo/store.redb record move 3 --to review
3	review	ungated: project 1 declares no transition from `todo` to `review`
$ echo "exit: $?"
exit: 0
```

`review → done` is a different matter. `launch` covers it, so `launch` runs:

```
$ fl --db /tmp/gs-demo/store.redb record move 3 --to done
FAIL	launch	config-parses	stale, 1 examined	25ms  (stale)
REFUSED	3	stays `review`
$ echo "exit: $?"
exit: 1
```

Refused, and for the staleness reason from [section 8](#8-staleness-and-gate-affirm) — the
config has moved several times since the gate was last stamped. The exit code is the same `1`
that `check` would have given, and the record has not moved:

```
$ fl --db /tmp/gs-demo/store.redb record list --project 1
3	review	tune the learning rate
```

Look at the gate, affirm it, and the same move goes through:

```
$ fl --db /tmp/gs-demo/store.redb gate affirm 2 --by you
2	f8044e6e44d6af8fedb5805527d4fcf65e9ea6f8

$ fl --db /tmp/gs-demo/store.redb record move 3 --to done
PASS	launch	config-parses	1 examined	25ms
3	done
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
