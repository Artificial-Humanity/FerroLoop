# GitHub ledger store — design

**Date:** 2026-09-30
**Status:** Approved by the owner 2026-09-30. Sub-project 3 of 4. Plan A written; plan B follows A's merge.
**Scope:** A `Ledger` backed by an append-only branch in the GitHub repository that already
backs the tracker (mode B), the routing that keeps every run locally and sends decisions to
GitHub, one comment per decision on the issue it concerns, and the setup, disclosure and
tamper rules that go with them.

---

## Reading this document

Constraints carry a status tag, as in the identity spec:

| tag | meaning |
|---|---|
| **Invariant** | Must hold in any version. Violating it breaks the product's premise. |
| **Release scope** | True of this sub-project's release. A later version can change it. |
| **Open** | Undecided. Named here so it is not decided by accident. |

An untagged statement is descriptive, not binding.

Each section of this design was reviewed against the code by an independent reviewer before
the owner approved it; the reviews' findings are folded in.

---

## 0. Where this sits

The identity spec (`2026-09-23-identity-and-store-roles-design.md`) split persistence into
the roles Catalog, Tracker and Ledger, and named four sub-projects. The GitHub tracker
(sub-project 2) gave mode A: the tracker in GitHub Issues, the ledger local. This is
sub-project 3, which completes mode B:

| role | mode B (this sub-project) |
|---|---|
| Catalog | the local store, and the committed manifest on other machines (unchanged) |
| Tracker | GitHub Issues (unchanged) |
| Ledger | **the local store, for every run; and an append-only branch in the tracker's repository, for every decision** |

The ledger holds `GateRun` (one execution of one gate) and `Attempt` (one runner
invocation). Both are append-only evidence.

### 0.1 Owner decisions this design rests on (2026-09-30)

1. **Purpose:** evidence visible on issues, evidence shared across machines, and a durable
   audit trail.
2. **Disclosure:** on a repository that is not private, nothing machine-specific is
   published: `output_excerpt` becomes `null` (a run's and an attempt's), an error's detail is
   withheld — a verdict carries no error class to publish instead, so the published copy says
   only that the gate errored — and `paths_touched` becomes a count. Visibility is read live, and a failed read is an ERROR
   (§5). `internal` counts as not private, as in the tracker.
3. **Reuse:** runs from other machines are stored and shown only. A transition still runs its
   own gates; fl does not accept another machine's pass in place of its own run.
4. **On issues:** one comment per decision, posted after the state change so it can say what
   happened (§4).
5. **Storage:** an append-only branch, `fl/ledger`, in the tracker's repository (§3). Chosen
   over issue comments as the ledger (editable, deletable, one gate's history spread over
   every issue, tighter rate limits) and over check runs and commit statuses (App only,
   140 characters, invisible on issues).
6. **Write path:** only decisions go to GitHub; the local store keeps every run. A plain
   `fl check` stays local and needs no network. If GitHub cannot be reached, a decision is
   refused.
7. **Findings:** `fl finding reproduce` and `fl finding verify` are decisions. Their runs are
   tagged with the finding's record and go to GitHub.
8. **Attempts:** fl checks GitHub before running the adapter and refuses to spend if it
   cannot be reached. An attempt whose flush fails after it ran is kept locally, reported,
   and published by the next successful flush.
9. **No ruleset:** a ruleset protecting `fl/ledger` is optional. With one, rewriting the
   branch is prevented and detected; without one — GitHub's Free plan offers no rulesets on
   private repositories — it is detected only. fl states which mode is in force (§6).
10. **An unreadable line** is quarantined by an append, never removed (§3.6).
11. **Refused decisions** are flushed too, and their comment says they were refused.
12. **The Free plan is the baseline.** Every core feature must work on GitHub Free. A
    paid-plan feature may be an option, but it must be non-critical or replaceable by a local
    option. Decision 9 applies this rule.
13. **`fl finding reproduce` checks the finding's state before its gate runs** (confirmed while
    planning), so every refusal after the run is a verdict, recorded and flushed.
14. **An attempt that ran but could not be published exits with the attempt's own code** (0 or 1)
    and prints the publish failure as a warning (2026-10-01). Exit 2 means "refused" everywhere
    else, and a script that retries on it must never re-run an attempt that was already paid for.

### 0.2 Out of scope

* Trusting another machine's pass in place of a local run (decision 3). *(Open.)*
* Two-tier routing and escalation between trackers (sub-project 4).
* Moving entries recorded before mode B was switched on into GitHub (§10).
* GitHub Enterprise Server.

---

## 1. Components

### 1.1 `fl-github::ledger::GithubLedger` (new)

Implements `Ledger` against the branch (§3) and posts decision comments (§4). It shares the
tracker's `Client`, credentials, origin guard and rate-limit handling.

### 1.2 `fl-core::SplitLedger { local, github }` (new)

Implements `Ledger` for mode B by routing between the local store and `GithubLedger` (§2).

### 1.3 Entry identity (`fl-core`)

`GateRun` and `Attempt` gain:

* `id: Iri` — `urn:uuid:` version 7, minted when the entry is created;
* `at` — RFC 3339 UTC.

Entries written before this change have neither and are never published. A new `Decision`
entry (§2.3) carries the same two fields.

So that a published copy reads back under decision 2: `GateRun.output_excerpt` and
`Attempt.output_excerpt` become nullable on the wire, and `Attempt.paths_touched` holds either
the list or a count. `id` and `at` are optional with a serde default, so entries stored before
this change still load, and an older fl still reads new ones; `at` has one fixed-width RFC 3339
spelling, so its string order is time order.

### 1.4 The `Ledger` trait

`Ledger` gains `flush(&self, decision: Decision) -> Result<Flushed, StoreError>`, whose
default does nothing. `Flushed` names the ledger commit, when there was one, and what stayed
local — entries of records another repository owns, or everything when no cut-over is
recorded — so the command can say so.

### 1.5 Binding

```toml
[[project]]
root = "/home/you/code/app"
store = "/home/you/.local/share/fl/app.redb"
tracker = { github = "owner/repo", credential = "env", ledger = "github" }
```

`ledger` is optional; its only accepted value is `"github"`, and any other value is a config
error that names it. The ledger always lives in the tracker's repository.

### 1.6 Client

A 403 or 429 whose message is GitHub's documented "secondary rate limit" becomes
`StoreError::RateLimited`, with the reset time when a header gives one, instead of
`Backend`. *(Modelled from GitHub's documentation; not provoked in a live test, because
doing so would abuse the API.)*

---

## 2. Routing and the write path

### 2.1 Where each entry goes

| operation | local store | GitHub ledger |
|---|---|---|
| a run with no record (plain `fl check`, `fl gate run`) | appended at once | never |
| a run tied to a record (a move, `check --record`, `finding reproduce` / `verify`) | appended at once | at the decision's flush |
| an attempt | appended at once | at the attempt's flush |

Every entry is appended to the local store exactly where it is appended today: `run_gate`
appends each run before its pass mark, and runs survive an evaluation that errors partway.
*(Invariant — the local store keeps every run.)*

A flush publishes every local entry that is tied to a record this binding owns, carries an
`id`, was recorded after the binding's **cut-over**, and is not yet published — and so also
carries an attempt left behind by an earlier failed flush (decision 8).

* **Ownership** is a local check, with no network: the record's IRI names this binding's
  repository. An entry whose record another binding owns is skipped and reported, never an
  error that blocks the decision.
* **The cut-over** is recorded, per repository `node_id`, when the GitHub ledger is switched on
  (`init`, §6.1). Entries recorded before it stay local: moving them is out of scope (§0.2).
  Ids are UUID version 7, so "after the cut-over" is an id comparison.
* Runs with no record are never candidates, so the set a flush scans stays bounded by what is
  actually waiting.

### 2.2 Evidence before state

The flush — the ledger commit — comes before the state change it supports. *(Invariant.)*
A flush failure refuses the decision: no state change. `SplitLedger::flush` returns only after
the GitHub commit has landed.

Flush sites:

| decision | where it flushes |
|---|---|
| record move | inside `move_record`, after every covering transition is evaluated and before `set_record_state` — one flush per move, however many transitions |
| `check --record` | at the end (no state change follows) |
| `finding reproduce`, `finding verify` | before `update_finding`, or at the end when the verdict refuses and nothing is updated; the runs carry the finding's record |
| attempt | after the local append; a failure does not refuse (§7) |

A refused decision is flushed too (decision 11).

### 2.3 The `Decision` entry

The flush commit also appends a `Decision` line: `id`, `at`, its kind (`move`,
`check`, `reproduce`, `verify`, `attempt`), the record or finding, the outcome composed by
the caller from the reports it already holds (the transitions evaluated and which refused; the
states for a move; a verify's reproduction and regressions; an attempt's status), and the ids
of the runs or attempt it rests on. The audit trail records verdicts as well as runs, and a
comment can be rendered from the ledger alone (§4).

### 2.4 Pre-flight

Before any gate or adapter runs, every decision in mode B checks, cheapest first: visibility
(live, every decision); the ruleset's presence, which sets the mode (§6); the branch; the head
descends from the anchor and from the last head seen (§3.5); and `ensure_publishable(project,
None)`, so no gate IRI the manifest does not list reaches the shared ledger. The ruleset,
branch and anchor results are cached for the life of one command. A failure refuses the
decision before anything runs or is spent.

### 2.5 Reads

Reads merge the local store and GitHub, de-duplicated by `id` and ordered by `at`, then `id`.
The same `id` with different content is an ERROR — except the fields decision 2 blanks, which
the local copy supplies whatever the repository's visibility is now (it may have changed since
the entry was published).

* `gate_runs(gate)`: if GitHub cannot be read, an ERROR. Unreachable is not empty.
  *(Invariant.)*
* `attempts(project)` for `fl stats`: if GitHub cannot be read, or under `--db`, the report
  covers the local store and says it is local only.

### 2.6 Call sites rewired

`check --record` and `fl attempt` are wired to the local store today, and `Ctx::roles()` binds
the ledger to the local store; all three move to the bound ledger. `finding reproduce` and
`verify` tag their runs with the finding's record. `fl stats` gains a rule for when it needs
the ledger binding.

---

## 3. The branch

### 3.1 Layout (format 1)

`fl/ledger` is an orphan branch — its own history, sharing no commit with the code — and holds
no `.github/`, so an append never starts a workflow.

```text
format                              the text 1
README.md                           what this branch is; never edit it by hand
runs/<gate-key>/<n>.jsonl           gate runs, one directory per gate
attempts/<project-key>/<n>.jsonl    attempts, one directory per project
decisions/<record-key>/<n>.jsonl    decisions, one directory per record or finding
quarantine.jsonl                    one line per quarantined entry
```

* A key is the first 32 hex digits of the SHA-256 of the IRI.
* Segments roll over at 256 KB. Only the last segment of a directory grows; a closed segment
  never changes. Readers list the whole directory, so no segment can be missed.
* Each line is one JSON object: the entry with `id`, `at` and `by` (the GitHub identity that
  wrote it). No machine names. Decision 2's projection applies on a repository that is not
  private.
* A reader that finds a `format` other than `1` refuses and says to upgrade fl. A new field in
  any entry means a new format. *(Invariant.)*

### 3.2 Appending

1. Read the head of `fl/ledger` and the last segment of each directory being appended to.
2. Run the tamper checks (§3.5).
3. Add the lines whose `id` is not already in the file. An entry's file follows only from its
   own `gate`, `project` or record field, never from alias resolution, so this is enough to
   make a retry safe.
4. Commit with one GraphQL `createCommitOnBranch`, with `expectedHeadOid` set to the head from
   step 1. GitHub refuses the commit if someone appended first, and signs it otherwise.
5. The head moved: back to step 1, up to five tries. A timeout: back to step 1 — step 3 skips
   whatever landed, and when nothing is left fl makes no commit but still marks the entries
   published. Any other failure: an ERROR.
6. Record the new head as the last seen. `SplitLedger`, which knows what was pending, marks the
   ids published once the remote publish returns. Both are kept keyed by repository `node_id`
   in additive local tables.

Two machines appending at once cannot both land a commit on the same head; the one refused
reads again and adds only what is missing. No entry is lost or duplicated. *(Invariant.)*

### 3.3 Reading

GraphQL lists a directory's segments with their object ids; fl downloads only segments it has
not cached, so a closed segment is fetched once. Lines are parsed strictly. A quarantined line
is skipped and reported. An unreadable line is an ERROR naming the file, the line, the commit
that added it and the quarantine command.

### 3.4 Cost

A decision costs about three requests, plus one per new segment, plus its comment. GitHub
answers a rate limit, primary or secondary, as `RateLimited` with the reset time when known;
the decision is refused.

### 3.5 Tampering

A ruleset (§6) makes GitHub refuse a rewrite or deletion of the branch. Nothing on GitHub
stops a person with write access from committing an edit to an existing line; fl detects
that edit, it cannot prevent it. On every read and every append fl checks:

1. the head descends from the ledger's first commit, the **anchor**, recorded in the committed
   manifest (§6), so a new machine has one;
2. the head descends from the last head this machine saw — a `behind` answer is read again a
   few times before it counts, because GitHub's replicas can briefly lag;
3. a closed segment's content never changes;
4. the open segment starts with fl's cached copy;
5. the same `id` never appears with different content, locally or on GitHub;
6. every line's `gate`, `project` or record matches the directory it is in;
7. the `format` is one fl knows.

`fl github ledger verify` goes further: it walks every commit from the anchor and checks each
one only adds lines or segments, reporting the first that does anything else. It costs about
one request per commit plus the segments it must compare, and is run by hand.

### 3.6 Quarantine

`fl github ledger quarantine <file> <line> --by <name> --reason <text>` appends a line to
`quarantine.jsonl` naming the segment and line. Readers skip the line and report it. The
damage and its repair both stay in the history. *(Invariant — nothing is ever removed.)*

---

## 4. Decision comments

### 4.1 When and where

Order: the ledger commit, then the state change, then the comment. The evidence is the ledger
commit, not the comment.

A finding decision comments on the finding's issue; a move, a `check --record` and an attempt
on the record's issue. A transferred issue gets it at its current location. One move is one
comment.

### 4.2 Content

The comment is rendered only from the ledger — the `Decision` line and the entries it names —
plus, when posted live, one line saying whether the state change completed.

* A header: what was decided, the outcome, who, and a link to the ledger commit.
* A table: gate, verdict, population, short commit, duration; for an attempt, the adapter,
  status, duration, tokens and cost. Cells are escaped for `|`, newlines and backticks.
* Excerpts only on a private repository, in a `<details>` block inside a fenced code block
  whose fence is longer than any backtick run in the text. Mentions and references inside a
  code block neither notify nor link.
* Every other user-supplied string has markdown and HTML escaped, and `@` and `#`
  neutralised, so a comment never notifies anyone or links an issue by accident.
* The body is at most 60,000 bytes of UTF-8 — under GitHub's 65,536-character limit however
  it is counted — truncating excerpts first and saying so, so the render always fits.

### 4.3 Recovery

A comment that fails to post leaves the state change standing; fl reports it and names
`fl github ledger comment <record>`, which renders every missing comment for that record from
the ledger. Each comment carries `<!-- fl:decision {"id":"<decision id>"} -->`; the command
lists the issue's comments, every page, and posts only decisions with no marker. A comment
posted seconds earlier may not be listed yet, so at worst it posts a harmless duplicate. The
ledger is the record and a comment is its view: a comment someone edits still counts as posted.

Decisions made before the GitHub ledger was switched on get no retroactive comments.

### 4.4 The tracker is unaffected

GitHub lists a comment on the timeline as a `commented` item with an id (measured on
`fl-live-test`). The tracker's conflict window counts only label, state and title events, so
fl's comments are never taken for someone else's change.

---

## 5. Disclosure

* Visibility is read once per decision, in the pre-flight, and used for the flush and the
  comment moments later. A change of visibility within those seconds is accepted.
* On a repository that is not private, decision 2's projection applies to every ledger line
  and every comment.
* Security findings live only in private repositories (tracker spec §6), so their decisions
  carry excerpts under the same rule.
* **Known limit:** if a private repository is later made public, every earlier excerpt in the
  ledger's history becomes public, and removing it would need the history rewrite the ledger
  exists to forbid. Comments can be edited; the ledger cannot. The docs and `init` say so.

---

## 6. Setup

### 6.1 `fl github ledger init`

Run once per repository, by a person. It can be run again.

1. Refuses unless the binding says `ledger = "github"`.
2. Refuses if a branch named `fl` exists: git cannot hold both `fl` and `fl/ledger`.
3. Creates the first commit — `format` and `README.md`, no parent, no `.github/` — through the
   REST Git Data API (trees, commits, refs; `createCommitOnBranch` needs an existing branch).
4. Records the cut-over (§2.1), and that commit's id as `ledger_root`: in the local store, keyed by repository
   `node_id`, and in the manifest, which becomes **format 2** exactly when it carries a root
   (the root names its repository's `node_id`; importing records it and refuses a different
   one). Every later export writes `ledger_root` from the store, resolving the project's
   repository through the config binding; an export that cannot tell the repository — under
   `--db`, for a store another IRI selected, or when the store holds a root but no binding for
   the configured name — refuses rather than drop the root. A store
   holding a root is store **format 4**, so an older fl, which could export without it, cannot
   open it. A new fl refuses a manifest format it does not know and says to upgrade; an older
   fl gives its own refusal. Format 1 manifests still import. The person commits the
   manifest, which is how every machine gets its anchor.
5. If the branch already exists and the manifest lacks `ledger_root` — `init` stopped after
   step 3 — it walks to the branch's first commit and asks the person to confirm it is the one
   they created. If both exist, it says the ledger is set up and stops. If the manifest has
   `ledger_root` but the branch is gone, it refuses: the ledger was deleted, and creating a new
   one would hide that.
6. Prints the optional admin step, which fl cannot do because its credential must not have
   Administration permission: a ruleset on `refs/heads/fl/ledger` with `non_fast_forward` and
   `deletion`, as a ready `gh api` command. Where the plan offers rulesets it is recommended;
   on Free, for a private repository, it is not available, and the ledger runs detection-only.
7. Recommends protecting the default branch as well, because Contents: write lets fl's
   credential push to any branch.
8. Prints the disclosure limit (§5).

### 6.2 Modes

| ruleset on `fl/ledger` | mode | guarantee |
|---|---|---|
| present and active, with `non_fast_forward` and `deletion` | **protected** | GitHub prevents a rewrite or deletion; fl detects edits (§3.5) |
| absent, or unavailable on the plan | **detection-only** | fl detects a rewrite, a deletion or an edit (§3.5); nothing prevents them |

`init`, `fl github whoami` and the docs state the mode in force. A ruleset that exists but is
inactive, or lacks either rule, is reported as detection-only, naming what is missing. The mode
is a plan feature, not a correctness one: every purpose of decision 1 works in both.
*(Invariant — decision 12.)*

### 6.3 Permissions

The credential — the environment token or the App — needs Contents: read/write, Issues:
read/write and Metadata: read. Fine-grained tokens expose no list of their permissions, so a
missing write permission is found by the first flush or comment, whose error names it (from
GitHub's `x-accepted-github-permissions` header). A credential that stays misconfigured
refuses every move, check and finding decision, and keeps each attempt locally with a
repeated error, until it is fixed. The docs and `init` say so.

---

## 7. Errors

Every error exits 2 with `error: …` and says what to do — except the attempt row below, which is
a warning (decision 14).

| condition | what fl does |
|---|---|
| branch missing, and no `ledger_root` | refuses; names `fl github ledger init` |
| branch missing, but the manifest has `ledger_root` | the ledger was deleted; refuses, and `init` refuses too |
| `ledger_root` missing from the manifest | refuses; commit the manifest from `init`, or run `init` again |
| head does not descend from the anchor or the last seen (after re-reads for lag) | the ledger was rewritten; names `fl github ledger verify` |
| a closed segment changed; the open segment does not extend the cached copy; the same id with different content; a line in the wrong directory | tampering, naming the file and commit |
| unknown ledger `format`, or unknown manifest format | names upgrading fl |
| an unreadable line | names the quarantine command |
| rate limited, primary or secondary | refuses the decision; gives the reset time when known |
| unreachable in the pre-flight | refuses; nothing ran |
| unreachable at a move's, check's or finding's flush | refuses; no state change; the runs stay local |
| unreachable at an attempt's flush | not refused — it already ran: kept locally, outcome printed, a warning, published next time; exits with the attempt's own code (decision 14) |
| a comment fails to post | the state change stands; names `fl github ledger comment <record>` |
| an unknown `ledger` value | config error |

---

## 8. Testing

### 8.1 The fake

The in-process fake GitHub gains: the REST Git Data API (refs, trees, commits, blobs); GraphQL
`createCommitOnBranch` with `expectedHeadOid` (a knob: the head moved) and directory listing
with object ids; `rules/branches` (knobs: absent, inactive, missing a rule); paginated issue
comments; and the secondary-rate-limit response.

### 8.2 Conformance

A `Bound`-based ledger suite runs `SplitLedger` over `MemStore` and the fake: runs tied to a
record, `flush`, merge by id, the local store succeeding while GitHub fails, and `NotOwned`
kept by checking ownership against the local catalog.

### 8.3 The battery

* Two flushes racing: both land, none lost.
* A timeout then a retry: no duplicate, and no empty commit.
* Each tamper check of §3.5 on its own, plus a `behind` answer that catches up: no alarm.
* Quarantine; an unknown ledger format; an unknown manifest format.
* Segment rollover, and a directory of many segments.
* A refused decision is flushed.
* Evidence before state, confirmed by mutation: moving the flush after the state change fails
  the test.
* The attempt pre-flight refuses before the adapter runs; an attempt whose flush fails is kept
  and published by the next flush.
* `init` stopped after each step, then run again.
* Comment recovery, and marker de-duplication across pages.
* Escaping: gate names containing `|`, backticks, `@`, `#`, `<!--` and newlines.
* The 60,000-byte cap.
* Disclosure by field — `output_excerpt` is `null`, an error's detail is the fixed withheld
  text, `paths_touched` is a number — and a scan of every published line and comment for absolute
  paths, `$HOME` and the hostname.
* Both modes of §6.2, including a ruleset that is inactive or missing a rule.

Every guard gets a mutation check.

### 8.4 Live tests

Ignored by default, run by the owner's arrangement against throwaway repositories:

* on a private repository without a ruleset (detection-only): `init`; two real flushes racing;
  `createCommitOnBranch` refused when the head moved; a decision comment round trip with its
  marker; a hand commit editing a line is detected;
* on a repository with a ruleset on `fl/ledger` — on Free, a public throwaway repository
  holding only test data: `rules/branches` readable by the token, and GitHub **refusing** a
  force update and a deletion of `fl/ledger`.

A secondary rate limit is not provoked live.

---

## 9. Documents updated in the same change

* `docs/github-ledger.md` (new), linked from `docs/README.md` and `docs/github-tracker.md`.
* The identity spec: §0.1's mode B row points here.
* The manifest's format note: format 2 and `ledger_root`.

---

## 10. Open questions

* **Trusting another machine's pass** in place of a local run. *(Open — decision 3.)*
* **Backfilling** entries recorded before mode B was switched on. *(Open.)*
* **Restricting who can push** to `fl/ledger` to fl's App, on plans whose rulesets allow a
  bypass list — a hardening option, never required (decision 12). *(Open.)*
* **Retention:** a directory grows without limit; segments bound each file, not the whole.
  *(Open.)*
* **A private repository made public** publishes the ledger's excerpts (§5). *(Open — no
  remedy short of a history rewrite.)*
