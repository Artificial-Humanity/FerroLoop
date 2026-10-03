# Keeping the ledger in GitHub

By default every gate run and every attempt is kept in the project's local store, and only
there. A project whose records and findings live in GitHub Issues
([github-tracker.md](github-tracker.md)) can also publish the evidence of each *decision* to
its repository: an append-only branch, `fl/ledger`, that every machine working on the project
reads, and one comment per decision on the issue it concerns.

## What goes where

| what | the local store | `fl/ledger` |
|---|---|---|
| a run tied to no record: a plain `fl check`, `fl gate run` | at once | never |
| a run tied to a record: `fl record move`, `fl check --record`, `fl finding reproduce` and `verify` | at once | with its decision |
| an attempt | at once | with its decision |
| the decision itself | — | at its flush |

The local store keeps every run, whatever happens on GitHub, and a plain `fl check` stays
local and needs no network. A *decision* — a move, a check tied to a record, a reproduction, a
verification, an attempt — publishes itself and the entries it rests on in one commit on
`fl/ledger` **before** the state change it supports. If that commit cannot be made, the
decision is refused and nothing changes; the runs stay in the local store, and the next
decision that reaches the ledger publishes them. An attempt is the exception: it has already
run and cost what it cost, so a failed publish is a warning, fl exits as the attempt did, and
the next decision publishes it.

A decision publishes only entries recorded after this machine's *cut-over* — the moment
`fl github ledger init` ran here — and only those tied to issues of this repository. Earlier
entries stay local.

## Setting it up

1. Name the GitHub ledger in the project's binding:

   ```toml
   tracker = { github = "acme/widgets", credential = "env", ledger = "github" }
   ```

   `ledger` is optional, its only value is `"github"`, and any other value is refused when the
   config is read. The ledger always lives in the tracker's repository.
2. In the project's checkout, run `fl github ledger init`. It refuses a repository that holds
   a branch named `fl`, or a branch under `fl/ledger/`, which git cannot hold beside
   `fl/ledger`, and a repository with no commit at all — push a first commit, then run it
   again. Otherwise it creates `fl/ledger`, a branch of its own that shares no history with
   the code and holds only `format` and `README.md`; records the branch's first commit and
   this machine's cut-over in the local store; and prints the mode in force and what to do
   next.
3. Run `fl manifest export --project <project>` and commit `.fl/manifest.json`. The manifest
   is then format 2: it carries `ledger_root`, the ledger's first commit, which every other
   machine checks the ledger's history against. Until it is committed, every decision is
   refused, saying so.
4. On every other machine, pull, then run `fl github ledger init` there. On a machine that
   imports the manifest rather than authoring the project, `init` first imports the committed
   manifest again — as `fl manifest import` does — so it learns the ledger's first commit
   before it touches GitHub; then it records this machine's own cut-over and changes nothing
   on GitHub. Until it has run, every decision on that machine is refused, naming `init`.

`init` can be run again at any time. On a ledger already set up it says so, records this
machine's cut-over if it had none, and prints the mode. If an earlier run stopped after
creating the branch but before recording its first commit, the next run shows that commit and
asks you to confirm it: `fl github ledger init --confirm <commit>`. If this machine or the
manifest records a first commit but the branch is gone, `init` refuses: the ledger was
deleted, and a new one would hide that.

## The two modes

| ruleset on `fl/ledger` | mode | what holds |
|---|---|---|
| active, with `non_fast_forward` and `deletion` | protected | GitHub refuses a rewrite or a deletion; fl detects an edit |
| none, disabled, evaluate-only, missing a rule, or not offered by the plan | detection-only | fl detects a rewrite, a deletion or an edit; nothing prevents them |

fl's credential must not hold Administration permission, so fl cannot add the ruleset; `init`
prints the `gh api` command an administrator runs. On GitHub Free a private repository has no
rulesets, and the ledger runs detection-only — every feature works in either mode. `init` and
`fl github whoami` state the mode in force, and what is missing. Protect the default branch as
well: Contents: write lets fl's credential push to any branch.

## Permissions

The credential — the token or the App — needs **Contents: read and write**, **Issues: read and
write** and **Metadata: read**. Fine-grained tokens do not list their permissions, so a missing
write permission shows at the first publish or comment, whose error names it. Until it is
granted, every move, check and finding decision is refused, and each attempt is kept locally
with a warning.

## What is published, and the disclosure limit

Each line of the ledger is one run, attempt or decision as JSON, with who wrote it (`by`, the
GitHub identity) and no machine name. On a repository that is not private — `public` or
`internal` — nothing machine-specific is published: an output excerpt becomes `null`, an
error's detail becomes a fixed text saying only that the gate errored, and the paths an
attempt touched become a count. fl reads the repository's visibility live before every
decision, and refuses the decision if it cannot.

**Known limit.** If a private repository is later made public, every excerpt already in the
ledger's history becomes public with it, and removing one would take the history rewrite the
ledger exists to forbid. Comments can be edited; the ledger cannot.

The text you give `fl github ledger quarantine` — `--by` and `--reason` — is published as you
wrote it, permanently, whatever the visibility; on a repository that is not private the
command warns before it appends.

## Decision comments

Once its state change is done, each decision posts one comment on the issue it concerns: a
move, a `check --record` and an attempt on the record's issue; a reproduction and a
verification on the finding's issue. A refused decision gets one too, saying it was refused.
The comment shows what was decided, the outcome, who decided, a link to the ledger commit, a
table of the runs it rests on — or the attempt's adapter, status, duration, tokens and cost —
and one line saying whether the state change completed. Output excerpts appear only on a
private repository, folded in a `<details>` block. Every name is escaped. `@`, `#` and the `-`
of `GH-` are followed by a zero-width space, and a URL is broken by one too — between the two
slashes of `scheme://`, and before the dot of `www.` — so a comment mentions no one and links
no issue and no URL by accident. Its one link is fl's own, to the ledger commit
([Limits](#limits) says what that costs a copied URL). A comment holds at most 60,000 bytes:
past that, excerpts are cut first, then left out, then the table's last rows, and the comment
says so — the ledger commit holds everything. Decisions made before the GitHub ledger was
switched on get no comment.

The ledger is the record and a comment is its view. Each comment carries a hidden marker,
`<!-- fl:decision {"id":"<decision id>"} -->`, and a comment marks the decision its first
marker line outside a code block names, wherever an edit leaves that line. A marker inside a
code block, or a second marker below the first, marks nothing. So a comment fl posted still
counts as posted when someone edits it. A comment counts only when its author is the account
fl posts as, or an account that wrote a decision under that item — the `by` of a decision line
filed there — so a colleague's machine, posting under its own token, is not duplicated, and a
passer-by's comment cannot stand in for one.

If a comment cannot be posted, the decision stands, and so does any state change it made. fl
prints a `warning:` naming the issue and the command that posts the comment later, and exits as
the decision did — a passing check still exits 0, and an attempt exits with its own code. That
command names the record or finding by the full URL its decisions are filed under, so it still
works after the repository is renamed.

`fl github ledger comment <item>` — the record's or finding's issue number (`41` or `#41`),
`owner/repo#41`, or its URL — lists every comment on the issue, every page, and posts each
decision filed under that item whose comment is missing, oldest first, rendered from the
ledger. It prints `posted` and the decision's id for each, then how many it posted and how many
were already there. A decision whose id is not one fl writes gets no comment: the command names
it on stderr, with the quarantine command for its line, and exits 1. Run it again at any time:
a decision whose comment is there is skipped. A comment posted seconds earlier may not be
listed yet, so at worst it posts a harmless duplicate. If the issue was transferred, or its
repository renamed, it prints `moved` and where the issue is now, and posts there. Given a
number, it reads only the decisions filed under the issue's current URL ([Limits](#limits)). A
recovered comment has no line about the state change: the ledger does not record whether it
completed. Run it where the project's catalog is: a comment names gates by the local catalog's
names.

## What a decision costs

A decision that follows another made on this machine makes about ten requests to GitHub's
ledger, besides the tracker's own:

- four for the pre-flight, before any gate runs: the repository's visibility, the rules on
  `fl/ledger`, the branch's head, and its `format` file;
- four for the flush: the head again, one listing of the directories it appends to, who fl
  writes as (once per command), and the commit;
- one download for each segment that grew since this machine last read it — in steady state,
  each directory the previous decision appended to;
- one for the comment.

A `check --record` that follows another on the same record and gate makes eleven: four, four,
two downloads and its comment. A move adds the tracker's write and its checks
([github-tracker.md](github-tracker.md#limits-and-costs)). A head another machine moved adds a
compare. A rate limit, primary or secondary, refuses the decision, naming when it resets if
GitHub says. `fl github ledger verify` costs about one request per commit, plus one per
segment.

## Errors and what to do

Every error exits 2 with `error: …`, except where this table says otherwise.

| what fl says | what to do |
|---|---|
| the repository has no GitHub ledger yet | `fl github ledger init` |
| the repository is empty | push a first commit, then `fl github ledger init` |
| the ledger was deleted | restore `fl/ledger` at a commit that descends from the first commit fl names; `init` will not start a new one |
| this machine records no anchor | `fl manifest import` the committed manifest, or `fl github ledger init` again |
| the committed manifest lacks the ledger's first commit | on the machine that authors the project, `fl manifest export` and commit; elsewhere, pull and `fl manifest import` |
| this machine has no cut-over | `fl github ledger init` here |
| the ledger was rewritten | `fl github ledger verify`, then find out who rewrote it |
| a file changed, a line sits in the wrong directory, or one id has two contents | `fl github ledger verify`; the message names the file and the commit |
| an unreadable line | upgrade fl if a newer one wrote it; otherwise `fl github ledger quarantine <file> <line> --by <name> --reason <text>` |
| an unknown ledger or manifest format | upgrade fl |
| rate limited | decide again after the reset time it names |
| the head moved on every try | decide again: the runs stayed local, and the next decision publishes them |
| GitHub unreachable before anything ran | nothing ran; retry |
| GitHub unreachable at a move's, check's or finding's publish | refused, no state change; the runs stayed local |
| GitHub unreachable at an attempt's publish | a warning, not an error: the attempt is kept locally and published by the next decision; fl exits as the attempt did |
| the attempt could not be saved locally | an error after the attempt's outcome; it is not published; fl exits as the attempt did |
| a comment was not posted | a warning, not an error: run the `fl github ledger comment` command it prints; fl exits as the decision did |
| `skipped`: a decision's id is not one fl writes | someone wrote that line by hand; `fl github ledger verify`, then quarantine it |
| `ledger = "…"` with any value but `"github"` | fix the config |

## Quarantine

A line fl cannot read stops every read of its directory. `fl github ledger quarantine <file>
<line> --by <name> --reason <text>` appends a line to `quarantine.jsonl` naming it; readers
then skip it and say so. Nothing is removed: the damage and its repair both stay in the
history.

## Verify

`fl github ledger verify` walks every commit from the ledger's first to its head and checks
that each only adds lines or segments, reporting the first that does anything else; then it
checks that no id is on two different lines. It prints progress every hundred steps, stops
after `--max-commits` commits (100,000 unless you say), and exits 1 when it found anything.

## Limits

- **One id, two contents.** Reads refuse an id that appears twice with different content in
  one directory, and `verify` checks every directory. But an entry the ledger holds under the
  same id as a local entry, with different content, is caught only by a read that merges the
  two — runs and attempts. Decisions are never read that way, and publishing takes any id
  already on the ledger as published, whatever its content.
- **A renamed repository.** Decisions are filed under the record's or finding's URL as it was
  when the decision was made. `fl github ledger comment` given the issue's number reads only
  the issue's current URL, so after a rename it misses the decisions filed under the old one.
  Run the command a failed comment's warning prints: it names the URL they are filed under.
- **Copied URLs.** Every URL in a comment is broken by a zero-width space, so it is not linked;
  a URL copied from a comment carries that invisible U+200B. Copy IRIs from `fl`'s own output.
- **Whose comments count.** A comment counts as posted when the account fl posts as wrote it,
  or an account that wrote a decision under that item, and only by its first marker line
  outside a code block. A collaborator who wrote a decision under the item can therefore mark
  another decision there as posted, and stop its recovery; anyone a ledger line names could
  alter the ledger itself. A comment by an account that wrote no decision under the item —
  after a switch from a token to the App, say — is not recognised, and recovery posts that
  decision again: a harmless duplicate.
- **Retention.** A ledger directory grows without limit; segments bound each file, not the
  whole.
- **Another machine's pass.** A transition runs its own gates. Runs other machines published
  are shown, never taken in place of a local run.

## The live tests

The tests in CI use an in-process fake GitHub, which proves structure, not how GitHub behaves.
`crates/github/tests/live.rs` checks the fake's reading of GitHub against GitHub itself. The
tests are ignored by default. `FL_GITHUB_LIVE_REPO` is required; the other three variables
are optional, and a ledger test whose variable is unset skips, saying which (`--nocapture`
shows it).

- `FL_GITHUB_LIVE_REPO` — a private, throwaway repository holding at least one commit, since
  `init` refuses an empty repository: `init`, two racing publishes, a stale commit refused, a
  hand edit and an unreadable line named, tree modes, the rules on Free (`200 []`,
  detection-only), branch listing by prefix, an unrelated history, a near-full segment, and a
  decision comment's round trip — including GitHub's rendering of it, which must hold no
  mention and no issue link.
- `FL_GITHUB_LIVE_READ_ONLY_TOKEN` — a fine-grained token on that repository only, with
  Contents: read and Metadata: read: an append is refused, naming the permission.
- `FL_GITHUB_LIVE_EMPTY_REPO` — a private repository with no commit: the rules still read, and
  `init` says to push a first commit.
- `FL_GITHUB_LIVE_PUBLIC_REPO` — a public repository holding only test data and one commit,
  with the ruleset `init` prints, active: the mode reads as protected, and GitHub refuses a
  force update and a deletion of `fl/ledger`. The test first reads the ruleset and refuses to
  write anything unless GitHub says the credential can never bypass it.

They append to `fl/ledger` and never delete it — a ledger under a ruleset cannot be deleted —
and the first run leaves a branch `fl-live/root` at the ledger's first commit; every later run
reads it there, so each test is safe to run again. The hand-edit test leaves two hand commits
for good, so `fl github ledger verify` on the private throwaway reports them. Set the token as
for the tracker's live tests ([github-tracker.md](github-tracker.md#the-live-tests)), then:

```text
FL_GITHUB_LIVE_REPO=acme/fl-live FL_GITHUB_LIVE_PUBLIC_REPO=acme/fl-live-public \
FL_GITHUB_LIVE_EMPTY_REPO=acme/fl-live-empty \
  cargo test -p fl-github --test live -- --ignored --nocapture --test-threads=1
```

A secondary rate limit, and a GraphQL request that runs past GitHub's time limit, are modelled
from GitHub's documentation and not provoked: doing so would abuse the API.
