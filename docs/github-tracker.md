# Keeping records and findings in GitHub Issues

By default a project's records and findings live in the local store, next to its gates. A
project can instead keep them in one GitHub repository's Issues: every record and every
finding is then an issue, which anyone with access to the repository can read, and which
every machine that works on the project sees the same way.

Only records and findings move. The catalog — the project, its gates and its transitions —
and the ledger of gate runs and attempts stay in the local store. Other machines get the
gates from the committed manifest, as [sharing-gates.md](sharing-gates.md) describes; they
never read another machine's store. Because a finding's reproduction names a gate that other
machines will read, `fl finding reproduce` on a project bound to GitHub first checks that the
committed manifest carries that gate as this store has it, and refuses until it does.

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

Only the commands that read or write records or findings use the binding: `fl record`,
`fl finding`, `fl attempt`, `fl check` with `--record`, and `fl github`. The catalog
commands — `fl project`, `fl gate`, `fl transition`, `fl stats` and `fl manifest` — never
contact GitHub.

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
read and write its issues.

**`credential = "app"`** writes as a GitHub App. The repository owner registers the App with
two repository permissions, **Issues: read and write** and **Metadata: read**, installs it on
the bound repository only, and downloads a private key for it. `private_key` is the path of
that key file: an RSA key in PEM form, as GitHub hands it out. fl signs a short-lived token
with the key, looks up the App's installation on the bound repository, and exchanges the
signed token for an installation token, which it renews before it expires. The key is read
from the file; it never appears on a command line, and a message may name the file's path
but never its contents.

`fl github whoami`, run inside the project's checkout, prints three lines: the identity
GitHub reports for the credential (a user's login, or an App's `<slug>[bot]`), where the
credential came from (the environment variable, or the App's id), and the repository fl
binds. It asks GitHub; it does not repeat the config back.

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
found". A list that meets a pull request carrying fl labels stops with an error naming it;
remove its fl labels (`fl github repair` refuses a pull request).

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
- **Lag.** Measured: the timeline and the edit history lag a write. A create's label events
  appeared 1.5–3.5 s after GitHub answered it; an update's events showed on the first read
  after it (about 0.5 s), and its edit-history entries about 0.5 s later. So fl
  waits for its own write to show — after a create, until the create's label events are in
  the timeline; after an update or a repair, until its own events and edits are in the window
  — for at most 10 s each. Without that, fl's own late events would land in its next write's
  window and be reported as a spurious conflict. A create whose events never show still
  succeeds; an update whose write never shows is an error that says to read the item again.
  Not measured: that once fl's own events show, every event written before them shows too.
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
scan: fl lists every fl record and every fl finding in the repository — two lists, each read
twice when it is longer than one page — and compares every block. It is correct, and costly;
adding an alias makes the same scan to check the alias is not already in use. GitHub's search
is not used, because its index lags and promises no complete result.

## Creates

Each create carries a key fl mints, stored in the block. If GitHub's answer to a create is
lost — a server error, or the connection dropping before an answer — the issue may exist
anyway, so fl searches the repository's fl issues for that key, up to three times a couple of
seconds apart, before it sends the create once more. From the first lost answer on, fl cannot
know whether the issue exists, so every later error carries the advice to list the
repository's fl issues before retrying: a failed search, and any failure of the second
attempt — a server error, a dropped connection, a rate limit, a refused credential or a
rejected request. A plain retry could otherwise make a duplicate.

Once GitHub has answered a create with success, fl never sends it again, even if the answer's
body could not be read: the issue exists. If fl then cannot find it by its key either, it
refuses, and says to list the repository's fl issues before retrying, so the retry makes no
duplicate. `fl record list` and `fl finding list`, or GitHub's issue list filtered by the
`fl:record` or `fl:finding` label, show what is there.

## Limits and costs

- **Rate limits are reported, not waited out.** When GitHub's limit is spent, the command
  fails, naming when it resets if GitHub said.
- **Each update and each repair is several requests.** It reads the issue twice, and the
  timeline and the edit history twice each — once before the write and once after — besides
  the write itself; a repair also posts its comment. A create has no such window: once per
  command fl reads the repository's labels, creating any that are missing, and then sends the
  create; a finding's create first reads the record it names. These costs are input to a
  later rate-limit design.
- **Lists read every page**, oldest issue first, a hundred to a page. A page that fails fails
  the list, never shortening it. A list longer than one page is read twice, and the two reads
  must agree: GitHub pages by offset, so an issue leaving the list mid-read could otherwise
  drop another one silently. If they differ, the command fails and says to retry.
- **A repository with Issues turned off** is refused when fl opens it: there is nowhere to keep
  records and findings.
- **A finding whose record reference predates a rename** costs one lookup each time it is
  read, until the finding is next written and the reference rewritten. A finding in a terminal
  state is never written again, so it keeps costing that lookup.
- **An installation token is not narrowed further** than the App's installation. Install the
  App on the bound repository only.
- **An issue whose body holds fl's block is fl's**, even with its labels removed; such an issue
  drops out of fl's lists, which filter by label, and reads as not an fl item until
  `fl github repair` restores its labels. fl never adopts an issue that has no block.
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
