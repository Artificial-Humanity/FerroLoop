# Keeping records and findings in GitHub Issues

By default a project's records and findings live in the local store, next to its gates. A
project can instead keep them in one GitHub repository's Issues: every record and every
finding is then an issue, which anyone with access to the repository can read, and which
every machine that works on the project sees the same way.

Only records and findings move. The catalog — the project, its gates and its transitions —
stays in the local store, and so does every gate run and attempt. With `ledger = "github"` in
the binding, each decision also publishes its evidence to the repository's `fl/ledger` branch
and posts a comment on its issue; [github-ledger.md](github-ledger.md) says how. Other machines
get the gates from the committed manifest, as [sharing-gates.md](sharing-gates.md) describes;
they never read another machine's store. Because a finding's reproduction names a gate that
other machines will read, `fl finding reproduce` on a project bound to GitHub first checks that
the committed manifest carries that gate as this store has it, and refuses until it does.

## Binding a project to a repository

The binding lives in the same user-level config file as the project's store,
`$XDG_CONFIG_HOME/fl/config.toml` (default `~/.config/fl/config.toml`), never in the
repository:

```toml
[[project]]
root = "/home/you/code/app"
store = "/home/you/.local/share/fl/app.redb"
tracker = { github = "acme/widgets", credential = "app" }

[github]
app_id = 123456
private_key = "/home/you/.config/fl/widgets-app.pem"
```

`github` names the repository as `owner/repo`: two non-empty parts and no whitespace.
`credential` is `"env"` or `"app"`, and the next section says what each reads. The
`[github]` section is needed only by `credential = "app"`; it holds the App's numeric id and
the absolute path of its private key file. A binding that says `"app"` with no `[github]`
section is refused.

Without a `tracker` key, the project's tracker is the local store, as before. The config is
read with unknown keys refused, so an older `fl`, which has never heard of `tracker`, refuses
the whole file rather than ignoring the key and writing records to a local tracker nobody
chose. Two entries for the same project root that name different stores or trackers are
refused too.

Only the commands that read or write records or findings use the binding — `fl record`,
`fl finding`, `fl attempt`, `fl check` with `--record`, and `fl github` — and `fl stats` when
the binding names the GitHub ledger. The catalog commands — `fl project`, `fl gate`,
`fl transition` and `fl manifest` — never contact GitHub, and `fl stats` does only to count a
project's attempts on its GitHub ledger ([github-ledger.md](github-ledger.md)).

`--db` and `$FL_DB` cannot be combined with a GitHub binding for a command that uses it. The
repository's identity (see [Identity](#identity)) and the project's catalog live in the store
the config entry names, and `--db` would pair GitHub with another store's catalog; such a
command is refused before any store is searched, created or opened, and the refusal says to
drop `--db` and unset `$FL_DB`. The catalog commands keep
`--db` as before. `fl github` needs a binding, and refuses without one, `--db` or not.

A store has one tracker, and a command that uses the tracker takes it from the config entry
whose `store` is the store the command works on — not only from the directory it runs in. A
full IRI can send a command to another project's store; if either that store's project or the
current directory's project is bound to GitHub, the command is refused, naming both, and the
refusal says to run it from the root of the project that holds the item. The same holds when a
GitHub-bound project's store is the default store and the command runs in a directory no entry
covers. Two entries that name the same store with different trackers are refused. Commands
between projects that are all unbound work as before.

## Credentials

The binding names one source for the credential, and fl never falls back from one to the
other: a binding that says `"env"` never uses the App, and one that says `"app"` never reads
a token from the environment. A missing or unusable source is refused, naming the setting.

**`credential = "env"`** reads `FL_GITHUB_TOKEN`, then `GITHUB_TOKEN`, from the environment;
an empty value counts as unset. fl writes as whoever owns the token: a personal token writes
as that person. Running inside GitHub Actions with its own `GITHUB_TOKEN` is untested: what fl
writes as there, and whether `fl github whoami` works with that token, are not yet checked. A
`GITHUB_TOKEN` already set for other tools is used when `FL_GITHUB_TOKEN` is not, so set
`FL_GITHUB_TOKEN` when the two should differ. The token needs to read the repository and to
read and write its issues; with the GitHub ledger, also to read and write its contents.

**`credential = "app"`** writes as a GitHub App. The repository owner registers the App with
the repository permissions **Issues: read and write** and **Metadata: read** — and, when the
binding names the GitHub ledger, **Contents: read and write** — installs it on the bound
repository only, and downloads a private key for it. `private_key` is the path of that key
file: an RSA key in PEM form, as GitHub hands it out. fl signs a short-lived token with the
key, looks up the App's installation on the bound repository, and exchanges the signed token
for an installation token, which it renews before it expires. The key is read from the file; it
never appears on a command line, and a message may name the file's path but never its contents.

`fl github whoami`, run inside the project's checkout, prints the identity GitHub reports for
the credential (a user's login, or an App's `<slug>[bot]`), where the credential came from (the
environment variable, or the App's id), the repository fl binds, and the ledger: `local`, or
`github` followed by the mode in force — `protected`, or `detection-only` with what is missing.
It asks GitHub; it does not repeat the config back.

## What an issue looks like

A record is an issue with two labels, `fl:record` and one state label such as
`fl:record/doing`; its title is the record's title. A finding is an issue labelled
`fl:finding` and one state label such as `fl:finding/reproduced`; its body starts with the
claim, and its title is the claim's first line, cut to GitHub's 256-character limit. A
record's title longer than that is refused before anything is sent, and so is one that starts
or ends with whitespace, which GitHub may trim. fl changes only its own
`fl:` labels and keeps every other label on the issue.

fl creates the labels it needs — one kind label and one label per state, for records and for
findings — the first time a command writes, with the description "managed by fl". It never
relies on GitHub creating a label as a side effect of a write.

An item of a project that routes its items between its local store and GitHub carries a third
fl label, `fl:area/<name>`, naming its area. fl creates an area's label the first time it makes
an item with that area, keeps it through every write and repair, and never deletes it. The area
is also a field of the block below; a block that carries one is written as `fl_format` 2, which
an older fl refuses as a newer format rather than reading half of it.

The fields a label cannot hold are kept in a metadata block at the very end of the body, an
HTML comment GitHub does not render. For a finding it looks like this, the JSON on one line:

```text
<!-- fl:meta
{"fl_format":1,"kind":"finding","state":"raised","project":"urn:uuid:…","record":{"id":"https://github.com/acme/widgets/issues/12","node_id":"I_kw…"},"raised_by":"reviewer","security":false,"also_known_as":[],"create_key":"urn:uuid:…"}
-->
```

The block is fl's record of the protocol, and the state in it is the item's state. Its field
names are `snake_case`. A block with a field this fl does not know, with text after it, or of
another `fl_format` is refused, never half-read.

Open or closed is a projection that fl writes and never reads state from. A record in `done`
and a finding in `fixed` are closed as completed; a finding in `withdrawn` is closed as not
planned; every other state is open. Closing or reopening an issue by hand does not change its
state: it makes the issue disagree with its block.

Every write is checked against GitHub's answer. GitHub silently drops a label the credential
may not set, so a write whose returned labels, body, title or status differ from what fl sent
is an error, never a success.

An issue with no `fl:` label is not an fl item, and neither is a pull request, even one that
carries fl labels: named directly, either is refused as not an fl item, never read as "not
found". fl's lists read GitHub's list of issues, which holds no pull requests, so a pull
request carrying fl labels is not in them.

## Divergence and repair

On every read fl compares the state label, the block and the open/closed status. If they
disagree — someone moved a label or closed the issue on the web, removed the block or damaged
it, or left two state labels — the item is diverged. fl reports every value it found and adopts
neither side. A list that meets a diverged item fails, naming it, rather than leaving it out.

A change to a record's title or to a finding's text is not a divergence: those live only in
the title and the body, and fl reads the new text.

`fl github repair <id> --by <name>` resolves a divergence. `<id>` is the issue number (`41`
or `#41`), `owner/repo#41` for the bound repository, or the issue's URL. The repair rewrites
the fl labels and the open/closed status **from the block**, never from the labels, so it
cannot move an item to a state the protocol never reached. It leaves the title and the body
alone, recreates any fl label that was deleted from the repository, and then posts a comment
on the issue recording that the repair ran and that `<name>` ran it. Anyone may run it;
`--by` is required, is not checked against anything, and is what the comment records. It
prints `repaired`, the issue number and the state. An issue that already agrees with its
block is left as it is, with no comment, and the command prints `consistent`.

Once GitHub has answered the repair's write with success, the repair has landed, at least in
part, so the comment is posted at once — before fl checks the answer or reads the timeline and
edit history again. If that check fails, or the repair crossed someone else's change (see
[Conflicts](#conflicts)), the error says so and also says whether the comment was posted; if the
comment could not be posted, the error asks for it to be added by hand.

A repair refuses an issue whose block is missing, damaged, of an unknown format, or names a
state that does not exist: it has nothing to rewrite from. Restore the block from the issue's
edit history on GitHub first — or, if the issue was never fl's, remove its fl labels instead.
It also refuses a pull request, a deleted issue and a transferred one.

## Conflicts

**Status: measured with a token.** The live tests (below) passed against GitHub on
2026-09-29, writing with a fine-grained token; they have not yet been run writing as the App.

GitHub has no conditional update, so fl cannot prevent two writers from crossing. It detects
it, and a write that crossed another is a `Conflict` error, never a success. Two checks make
that up.

- **The item changed since fl read it.** A command reads an item, may run gates for minutes,
  then writes. Just before it writes, fl reads the issue again and compares its block, text
  and title with what it read earlier in the same command, if it read the item. If they
  differ, the write is refused and nothing is sent. fl remembers what it read only for the
  life of one command.
- **Someone wrote while fl did.** fl reads the issue's timeline and its body edit history
  before its write and again after it. A label added or removed, a close, a reopen or a
  retitle that fl's own write does not explain is someone else's, and so is any body edit
  beyond fl's own. Edits are counted by the edit history's total count, so an edit history
  that shrank — someone deleted an entry — is someone else's change too. Comments and
  mentions change nothing fl reads, and are not conflicts.

Either way, the other change has already landed: read the item again, check it, and retry.
After the second kind of conflict fl's own write has landed as well, and the next read reports
any disagreement as diverged.

The detection rests on a model of what GitHub records. The live tests measured most of it:

- **The total count.** fl counts edits by the edit history's total count, and takes that
  count to include every entry. Measured: it did, on every read.

- **The first-edit blind spot.** fl takes the first edit of an issue's body to add two
  entries to its edit history (the original, then the edit) and every later edit to add one.
  If GitHub records a first edit as one entry, then someone else's body edit that lands in the
  same moment as fl's first edit of that issue is hidden: the count looks like fl's alone.
  Measured: a first edit added two entries and a later one one, as modelled.
- **The edit history's order.** fl reads the last hundred entries and the total count; it
  counts by the total, which does not depend on the order GitHub lists them in, and uses the
  entries only as a second count. An entry deleted and another added in the same window
  cancel out and are not seen. Not measured.
- **Lag.** Measured: the timeline and the edit history lag a write. An update's events showed
  on the first read after it (about 0.5 s), and its edit-history entries about 0.5 s later
  (2026-09-29). Labels set in the create itself showed their events 28–88 s late, once more
  than 180 s; a label added by its own call just after the create showed its event in 1–2 s
  (2026-10-05, measured adding one label per call; fl's call adds two, and its timing is not
  measured separately). So fl creates an issue without labels and then adds them (see
  [Creates](#creates)), and waits for its own write to show — after a create, until its labels
  are in the timeline; after an update or a repair, until its own events and edits are in the
  window — for at most 10 s each. Without that, fl's own late events would land in its next
  write's window and be reported as a spurious conflict. A create whose events never show still
  succeeds; an update whose write never shows is an error that says to read the item again.
  Not measured: that once fl's own events show, every event written before them shows too.
- **A label event recorded twice.** GitHub sometimes records a `labeled` event a second time,
  about 0–1 s after the first (2026-10-05): when one call added two labels, in 4 of 10 calls;
  when each call added one label, in none of 22 probes but in 2 of 33 issues fl created in a
  live run. Adding a label an issue already carries records no event, so a `labeled` event for
  a label already on the issue cannot be anyone's write. fl replays the issue's timeline to
  know which labels are on at each event, and does not count such an event — as a change by
  someone else, or as a sign that its own write shows. It always counts a label removed.
  Measured on 2026-10-05 (one sample): deleting a label from the repository records a
  label-removed event on each issue that carried it, the timeline still names the deleted label
  in its events, and adding it again after re-creating it records a new label-added event, so
  none of that upsets the replay. Not
  measured: that the timeline records every other label change, and that an event that shows
  means every earlier one shows too. A label change with no event would make fl skip a later
  `labeled` event for that label.
- **Line endings.** fl takes a body rewrite that only changes line endings to count as an
  edit. If GitHub records none for it, someone else's edit in the same window can be missed.
  Measured: GitHub recorded one entry for such a rewrite, as modelled.

The live tests (below) check the total count on every read, the first-edit model, one
timeline event for each label added or removed, each close, each reopen and each retitle,
and the lag, by reading at once after each of fl's writes and again two seconds later. They
also count lost updates under concurrent writers over repeated rounds. For line endings they
print whether GitHub records an entry for a rewrite that changes only line endings, and check
that an fl write after one is not a conflict. The order past a hundred entries, and a deleted
entry offset by a new one, are not yet measured.

## Security findings

`fl finding raise --security` marks a finding as a security finding (`"security": true` in the
block). Before it creates the issue, fl reads the repository's visibility, every time, and
writes the finding only to a `private` repository; `public` and `internal` are refused. A
visibility fl cannot read is an error, never a pass. The refusal names the remedies: a local
tracker for the finding, or a private repository. The mark is set when the finding is raised
and never changed by a later write.

Two limits are stated rather than solved. If a private repository is made public later, its
security findings become public with it, and fl cannot prevent that. And fl cannot recognise
a security finding that nobody marked.

## Identity

An item's id is its issue's URL, `https://github.com/acme/widgets/issues/41`. Its handle is
the issue number, which may be typed `41` or `#41`; records and findings share issue numbers,
so `fl record` refuses the number of a finding. `owner/repo#41` names issue 41 of that
repository, and is refused as not held by this tracker unless it names the bound one — by its
current name, or by an old name that still reaches it after a rename. Projects
and gates keep their local handles.

**A renamed repository.** fl records the repository's GitHub node id in the local store the
first time it opens it, and compares it every time after. The same node id under a new name is
a rename or a transfer: fl follows it, and prints a notice on each command that uses the
tracker until the `github` binding in the config names the new name. Issue URLs under the
old name still resolve, at the cost of one lookup each.

**A reused old name.** A different node id means the configured name now reaches another
repository — someone created a new one at the old name. fl refuses every command that uses the
tracker until the binding is corrected. A URL under another repository's name is refused as
not owned, and the message says what fl found: that GitHub leads the name to a repository
under another name, or that the name is simply another repository. fl does not know a
repository's past names, so it cannot say whether that name was once this repository's.

**Deleted and transferred issues.** A deleted issue is reported as deleted, and a transferred
one as moved, naming where it went; neither is "not found".

**Aliases.** An item's other ids are kept in its block. Finding an item by an alias is a full
scan: fl lists every fl record and every fl finding in the repository — two lists — and
compares every block. It is correct, and costly;
adding an alias makes the same scan to check the alias is not already in use. GitHub's search
is not used, because its index lags and promises no complete result.

## Creates

A create is two calls: the first makes the issue with its title and body, and no labels; the
second adds the kind label and the state label. GitHub shows the events of labels added this
way in the timeline within a couple of seconds, but the events of labels set in the create
itself up to minutes late, where fl's next write would report them as someone else's change.

If the second call fails, the issue exists without some or all of fl's labels. fl's lists leave
it out, or report it as diverged if it has some of them, and the error names the issue: run
`fl github repair <number> --by <name>`, which restores its labels from its block. Do not retry
the command that created it; that would make a second issue.

Each create carries a key fl mints, stored in the block. If GitHub's answer to a create is
lost — a server error, or the connection dropping before an answer — the issue may exist
anyway, so fl searches the repository's issues for that key, up to three times a couple of
seconds apart, before it sends the create once more. The search reads every issue, with fl's
labels or without, newest first, and stops at issues created more than 10 minutes before the
attempt began; the margin covers a difference between this machine's clock and GitHub's. An
issue it finds without fl's labels is given them, and the create is not sent again.

From the first lost answer on, fl cannot know whether the issue exists, so every later error
says where to look before retrying: a failed search, and any failure of the second attempt — a
server error, a dropped connection, a rate limit, a refused credential or a rejected request.
Once GitHub has answered a create with success, fl never sends it again, even if the answer's
body could not be read: the issue exists. If fl then cannot find it by its key either, it
refuses with the same advice.

The advice names the title. Look among the repository's newest issues, with labels or without,
for an issue with that title. If it is there, run `fl github repair <number> --by <name>` on it
instead of creating it again: the repair gives it fl's labels from its block, or reports it
consistent if it already has them. An issue fl created may have no labels yet, so
`fl record list`, `fl finding list` and GitHub's list filtered by an `fl:` label do not show
it. A plain retry could make a duplicate.

GitHub's issue list can take minutes to show a new issue. On 2026-10-05 its REST list left a
new issue out for 31–93 s, once for more than 180 s; that its web list lags the same way is
inferred, not measured. If the issue is not there, wait a few minutes and look again before
retrying.

## Limits and costs

- **Rate limits are reported, not waited out.** When GitHub's limit is spent, the command
  fails, naming when it resets if GitHub said.
- **Each update and each repair is several requests.** It reads the issue twice, and the
  timeline and the edit history twice each — once before the write and once after — besides
  the write itself; a repair also posts its comment. A create has no such window: once per
  command fl reads the repository's labels, creating any that are missing, then sends the
  create and adds its labels, and reads the timeline until they show; a finding's create
  first reads the record it names. These costs are input to a later
  rate-limit design.
- **Lists read GitHub's GraphQL API, every page**, oldest issue first, a hundred to a page,
  each page found from the last by a cursor. A page that fails fails the list, never
  shortening it. Measured on 2026-10-05: GitHub's REST issue list left a new issue out for
  31–93 s, once for more than 180 s, and showed a label change 30–100 s late; the GraphQL list
  showed a new issue within 1 s and a label change within 2–3 s. The cursor names the last
  issue read rather than a position count, so an issue leaving the list mid-read is taken not
  to move another one off it, and a list is read once (not measured).
- **A repository with Issues turned off** is refused when fl opens it: there is nowhere to keep
  records and findings.
- **A finding whose record reference predates a rename** costs one lookup each time it is
  read, until the finding is next written and the reference rewritten. A finding in a terminal
  state is never written again, so it keeps costing that lookup.
- **An installation token is not narrowed further** than the App's installation. Install the
  App on the bound repository only.
- **An issue whose body holds fl's block is fl's**, even with its labels removed, or without
  them after a create that stopped between its two calls; such an issue drops out of fl's
  lists, which filter by label, and reads as not an fl item until `fl github repair` restores
  its labels. fl never adopts an issue that has no block. To find one a stopped create left,
  filter the repository's GitHub issue list by `no:label`, open the newest issues there, and
  run `fl github repair <number> --by <name>` on the one whose body ends with fl's block. If
  something else labelled it in the meantime, `no:label` does not show it; look among the
  newest issues instead.
- **A state label without its kind label** reads differently in two places. An issue that
  carries `fl:finding/withdrawn` but not `fl:finding` is left out of `fl finding list`, which
  filters by `fl:finding`, but makes the count of a raiser's withdrawals, which filters by
  `fl:finding/withdrawn`, fail as diverged. `fl github repair` fixes it.
- **A block and a label edited to agree** on the web read as consistent: fl does not check who
  last edited the block.
- **A repository deliberately recreated at the bound name** is refused like any reused name,
  and there is no command yet to accept it.
- **`FL_GITHUB_API_URL` is for tests.** It replaces GitHub's API address, and the credential
  goes wherever it points. fl parses it as a URL and accepts only `https://`, or `http://` to
  this machine — `127.0.0.1`, `localhost` or `[::1]` — and never one with a user name or
  password before the host; anything else is refused. When it is set, every command that talks
  to GitHub prints a notice naming the host it talks to instead. fl names issues by their
  `github.com` URLs, so it is not a way to reach GitHub Enterprise Server.

## The live tests

**Status: passed on 2026-09-29, writing with a fine-grained token.** All three passed. The
concurrency test counted 1 clean round, 9 conflicts caught and 0 updates lost. They have not
yet been run writing as the App.

The tests that run in CI use an in-process fake GitHub. It proves the structure, not how
GitHub behaves, so three more tests in `crates/github/tests/live.rs` run against GitHub
itself: a round trip of a record and a finding; ten rounds of two writers adding to the same
finding at once, which require no update ever to be lost silently; and exact counts of the
edit history and the timeline against the model conflict detection rests on, as
[Conflicts](#conflicts) lists. They are ignored by default, and each fails at once, naming
`FL_GITHUB_LIVE_REPO`, if it is not set.

Run them only against a **private, throwaway** repository: they create issues and never delete
them, and they refuse a repository that is not private. Set `FL_GITHUB_TOKEN` in the
environment (from a secret store, not typed where shell history keeps it), or set both
`FL_GITHUB_APP_ID` and `FL_GITHUB_APP_KEY` — the App's id and the path of its private key —
to write as the App; one without the other is refused, never a fallback to the token. Then:

```text
FL_GITHUB_LIVE_REPO=acme/fl-live cargo test -p fl-github --test live -- --ignored --nocapture --test-threads=1
```

The concurrency test prints how many rounds were clean, how many were caught as conflicts, and
how many were lost; record those counts with the change that ran them. If the edit-history
test fails, the model is wrong: fix the conflict check and the fake together.

The GitHub ledger has live tests of its own in the same file;
[github-ledger.md](github-ledger.md#the-live-tests) says what they need.
