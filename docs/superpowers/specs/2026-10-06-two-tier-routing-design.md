# Two-tier routing and escalation — design

**Date:** 2026-10-06 (rev 2.4, 2026-10-07)
**Status:** Approved by the owner 2026-10-06. Rev 1 (in git at 96e1a41) was approved and then
reviewed against the code; rev 2 folds in that review and the owner's decisions 12–19 on it.
Rev 2.1 folds in the owner's decisions 20–21 and seven corrections found while planning plan A
against the code (`docs/superpowers/plans/2026-10-06-two-tier-routing-a.md`, "Spec defects");
rev 2.2 folds in the owner's decision 22, after a re-review found two ways around decision 21;
rev 2.3 folds in the owner's decision 23, after the final review found a way around decision 22;
rev 2.4 folds in seven `[agent]` corrections found while planning plan B against the code
(`docs/superpowers/plans/2026-10-07-two-tier-routing-b.md`, "Spec defects"), and one `[agent]`
ruling from its review: a marked item's lookup is best effort (§2.2).
Each change is marked here as an owner decision or `[agent]`.
Sub-project 4 of 4. Delivered as two plans (§8).
**Scope:** A project with two trackers — the local store for developer-level items, a GitHub
repository for human-level items — the rule that routes each new item to one of them, and
the escalation that moves an item from the local tier to GitHub.

---

## Reading this document

Constraints carry a status tag, as in the identity spec:

| tag | meaning |
|---|---|
| **Invariant** | Must hold in any version. Violating it breaks the product's premise. |
| **Release scope** | True of this sub-project's release. A later version can change it. |
| **Open** | Undecided. Named here so it is not decided by accident. |

An untagged statement is descriptive, not binding. `[agent]` marks a ruling the controller
made where the owner's decisions left a choice; the owner may reverse any of them.

---

## 0. Where this sits

The identity spec (`2026-09-23-identity-and-store-roles-design.md` §0.1) names a two-tier
configuration as the flow we recommend: the local tracker carries developer-level items
(code, code quality, unit tests) and is not closed to humans; GitHub carries human-level
items (design review, product review, security review, and code-quality items that escaped
the local phase or were escalated past it). Its table gives that configuration a **local**
ledger. Sub-projects 2 and 3 built the GitHub tracker and ledger. Each project still binds
exactly one tracker (identity spec §3.3). This sub-project lets a project bind two, and
decides the two questions that spec left open (§7): which tracker receives a new item, and
how an item moves between the tiers.

### 0.1 Owner decisions this design rests on (2026-10-05/06)

1. **Scope:** routing and escalation between the tiers only. The ladder of ratified decision
   14 — a different agent fixes, then an adjudicator that can strike a finding — comes later;
   this design leaves the escalation's trigger open for it to plug into.
2. **Routing at creation is by subject matter.** Each record and finding carries an
   **area**; a map routes each area to a tier. A flag overrides the map for one item.
3. **Escalation moves the item.** A new GitHub issue is made; the local item becomes a
   tombstone that points to it. Only one live copy exists.
4. **Escalation starts by command, or by a local record moving to `needs_human`.** Rules in
   config — for example, escalate when a record's spend passes a limit — are the goal, and
   out of scope for this phase.
5. **References may cross tiers.** A finding and its record may sit in different tiers.
6. **The areas and the map live in the committed manifest.** fl writes a starting set; a
   project may add and remove areas (decision 17 drops rename); an area the manifest does not
   declare is refused.
7. **An area is required only where it routes something:** in a two-tier project. In a
   one-tier project it is optional. `[agent]` Read with §1.3: "two-tier" is a routed project,
   and a project without a routing map declares no areas, so `--area` is refused there (§1.1).
8. **Exactly two tiers, with fixed names `local` and `github`.** Named trackers, any number
   of them, are the roadmap (§7).
9. **The routing lives in a routing tracker** that implements the `Tracker` trait over the
   two tiers, not in each CLI command or in the gate engine.
10. **An item's area is also a GitHub label** (`fl:area/<name>`), so GitHub's own filters show
    it.
11. **`fl routing remove` is refused while any item in either tier names the area.**
12. **A routed project uses the local ledger in this release.** A routing map together with
    `ledger = "github"` is refused. Two tiers with the GitHub ledger are the roadmap (§7).
    (Review: `SplitLedger::flush` refuses any decision whose record the repository does not
    own, `core/src/split.rs:378`, so every gated move of a local record would fail.)
13. **A map entry can be marked sensitive, and a sensitive area sets the security flag.** The
    starting set marks `security` sensitive. *(Rev 2.1, `[agent]`: only a finding carries the
    flag; a record has none. A record in a sensitive area is refused on a non-private
    repository when it is made, and decision 21 covers what is said about it later — §1.2.)*
14. **An issue whose block carries an area or a local record reference is written with
    `fl_format` 2**, so an older fl says "upgrade fl" rather than "damaged". The GitHub tracker
    spec's §2.3 invariant ("a finding's record carries the `node_id`") is amended for local
    records (§2.5). *(Rev 2.1, `[agent]`: the reference's field is `id`, as in every reference
    the block holds; a local one is `{ id, title }`.)*
15. **An area removed from the map while other machines' local items still name it** stays on
    those items as history; only new items are refused. *(Release scope.)*
16. **When a move to `needs_human` lands but its escalation fails**, the command exits with the
    move's own code and prints a `warning:` naming `fl record escalate <id>`.
17. **No area rename.** Areas are added and removed only.
18. **An escalated record's GitHub issue lists the record's open findings** — claim, state and
    IRI — leaving out security findings. fl warns before it publishes to a non-private
    repository. *(Rev 2.4, `[agent]`: also leaving out findings in a sensitive area or one the
    map no longer declares — decisions 21 and 22.)*
19. **GitHub is opened only when a GitHub-tier item is needed**, so work on local items does
    not need the network or a credential.
20. **A routed store holds exactly one project** (owner, 2026-10-06). Routing is decided per
    store, because handles are numbered per store, so "routed project" and "routed store" are
    one thing (§1.3). `fl routing set` is refused while the store holds any project other than
    the one named; the refusal says to give the project its own store — a config entry with
    its own `store`. `[agent]`: so the rule cannot be walked around, a routed store also
    refuses `fl project add` and an import of another project, and a routed manifest is not
    imported into a store that holds another project.
21. **Refuse, not warn** (owner, 2026-10-06): nothing about an item in a sensitive area, and no
    security finding, reaches a non-private repository — including a GitHub finding, in any
    area, whose record is a local record in a sensitive area (its title would be published).
    That finding is refused, naming `--tier local`; §2.5's warning stays for a local record in
    any other area.
22. **Decision 21 fails closed** (owner, 2026-10-06, after a re-review showed two ways around
    it). `fl routing set` keeps an area's sensitivity unless told otherwise: `--sensitive` sets
    it, `--not-sensitive` clears it, and neither keeps it — a set that only changes a tier
    never clears it. Clearing it is refused while any item in either tier names the area, by
    the same check, with the same count and up to ten items, as `fl routing remove`
    (decision 11). A record whose area the map no longer declares — removed where the project
    is authored while another machine's records still name it — counts as sensitive. A
    finding that decision 21 refuses on GitHub, or that is kept local for it, carries the
    security flag. `[agent]`: a record with no area at all (made before the project was
    routed) is not sensitive.
23. (owner, 2026-10-07) "A later `fl routing set` that adds an area the map does not declare
    must say `--sensitive` or `--not-sensitive`. With `--not-sensitive` fl does the same check
    and prints the same 'this machine only' note as clearing a sensitivity. The first `set`
    keeps its default." `[agent]`: the way around decision 22 it closes — the authoring machine
    removes a sensitive area that only another machine's local records name, sets it again
    with no flag, and exports; the area came back not sensitive, and once imported a finding
    about such a record reached a public repository with only a warning. The refusal comes
    before anything is written and names both flags. The "this machine only" note: a refused
    clearing or re-add says fl reads only this machine's local tier, and one that succeeds —
    adding an area, or clearing a sensitive one — prints a `note:` saying fl checked only this
    machine's local tier; a `--not-sensitive` that changes nothing prints none.

### 0.2 Out of scope

* Moving an item from GitHub back to the local tier.
* Escalation rules in config (decision 4), and the ladder's other rungs (decision 1).
* Named trackers (decision 8).
* The GitHub ledger in a routed project (decision 12).

---

## 1. Data

### 1.1 The area

* `Record` and `Finding` gain `area: Option<String>`. An item written before this change
  reads as having no area.
* **Area names** are lowercase `[a-z0-9-]{1,32}`. `[agent]` GitHub label names are
  case-insensitive and at most 50 characters; `fl:area/` takes 8.
* In the local store the field is serialized with the item. In GitHub it is a field of the
  issue's fl block (`Meta.area`, skipped when absent). A block that carries it is written with
  `fl_format` 2 (decision 14).
* **On GitHub the area is also a label, `fl:area/<name>`** (decision 10).
  * It is one of fl's labels: the create's label call adds it with the kind and state labels;
    every label rewrite keeps it (today `labels_after` replaces all `fl:` labels with kind and
    state, `github/src/meta.rs:267` — it must keep the area label); `fl github repair` restores
    it from the block.
  * fl creates `fl:area/<name>` in the repository the first time an item with that area is
    made there. Removing an area from the map does not delete its label: issues that carry it
    are history. `[agent]`
  * The block stays the source of truth: an issue whose area label is missing or differs from
    its block reads as diverged, as a wrong state label does today.
  * The area never changes after creation, so the label adds nothing to an update's conflict
    window.
* An item keeps its area for its whole life; escalation does not change it.
* **A finding's area** is the one `--area` names, or else its record's area, which the output
  names ("area: code, from its record"). `[agent]` This is explicit inheritance from a
  declared value, not a guess. A finding whose record has no area, raised without `--area`
  in a routed project, is refused.
* `--area` in a project with no routing map is refused, naming `fl routing set`: an area is a
  name the map declares. `[agent]`

### 1.2 The routing map

* The map — area name → `{ tier: local | github, sensitive: bool }` — is authored in the
  project's store, as gates are, and exported into `.fl/manifest.json` as a `routing` section.
  A manifest edited by hand is refused, as today: the hash covers the new section too.
* **Manifest formats.** `[agent]`

  | format | carries | written when |
  |---|---|---|
  | 1 | gates, transitions | no ledger root, no routing |
  | 2 | + `ledger_root` | a ledger root, no routing |
  | 3 | + `routing` (and `ledger_root` if any) | a routing map |

  A project without routing still exports a format every older fl reads.
* A machine that imports the manifest imports the map with it, so every machine and every
  agent routes the same way. *(Invariant — one routing rule per project, not per machine.)*
  For that to hold, a routed create and an escalation require the manifest import to be
  current on an importing machine. `[agent]` *(Rev 2.1, `[agent]`: on the authoring machine
  — where gated moves check nothing today, `ensure_import_current` passing there — a routed
  create compares the working tree's manifest, when there is one, with the store's map, and
  is refused naming `fl manifest export` when they differ. With no manifest, no other machine
  can import the project, so there is no other rule to disagree with.)*
* **Sensitive areas** (decision 13): a finding made with a sensitive area carries the security
  flag, with every consequence the flag has today (a GitHub tracker refuses it on a
  non-private repository). `--security` without a sensitive area still sets the flag. A record
  carries no flag *(rev 2.1, `[agent]`)*: a record made in a sensitive area is refused on a
  non-private repository when it is made, and a finding about a local record in a sensitive
  area — or in an area the map no longer declares — is a security item: it carries the flag,
  and is never sent to a non-private repository (decisions 21, 22).
* Commands *(rev 2.1, `[agent]`: each takes `--project <project>`, as every project-scoped
  command does)*:
  * `fl routing set <area> <tier> [--sensitive | --not-sensitive]` — refused while the store
    holds another project (decision 20). The project's first `set` writes the
    starting set first: `code` and `tests` → `local`; `design` and `product` → `github`;
    `security` → `github`, sensitive. The first `set` also prints the handle change of §2.3.
  * `fl routing set` on an area that exists changes its tier for new items only; items already
    made stay where they are. Without `--sensitive` or `--not-sensitive` the area keeps its
    sensitivity (decision 22). `--not-sensitive` on a sensitive area is **refused while any
    item in either tier names the area**, by the same check as `fl routing remove` below.
  * A later `fl routing set` that adds an area the map does not declare: decision 23.
    `[agent]`: without `--sensitive` or `--not-sensitive` it is refused before anything is
    written; with `--not-sensitive` it is refused while any item in either tier names the
    area, since such an item counts as sensitive (decision 22). The project's first `set` is
    unchanged.
  * `fl routing remove <area>` — **refused while any item in either tier names the area**
    (decision 11). The refusal gives the count and lists up to ten of the items. It finds
    GitHub items by reading their blocks, not by the area label, so an item that lost its
    label is not missed. A tier that cannot be read refuses the removal too. `fl` can check
    only this machine's local tier (decision 15).
  * `fl routing show`.

### 1.3 Routed projects

* A project is **routed** when its manifest (or, on the authoring machine, its store) has a
  routing map. Whether it is routed never depends on the map's current contents. `[agent]`
  (Review: deriving it from "the map names both tiers" lets one `fl routing set` change every
  handle's meaning.)
* **A routed project is a routed store** (decision 20): fl chooses a command's tracker and
  reads its handles per store, before it knows the command's project, and handles are
  numbered per store, so a routed store holds exactly one project.
* The **`github` tier is available** when the machine's config binds a GitHub repository for
  the project (the `tracker` binding of `$XDG_CONFIG_HOME/fl/config.toml`, unchanged). The
  local store is the `local` tier.
* A routed project with no binding works on its local items. A `github`-tier create is refused
  and names the missing config entry; so is an issue URL the local tier does not hold. fl
  never falls back to `local`. *(Invariant — routing never changes tier silently.)* *(Rev 2.1,
  `[agent]`: a list of both tiers needs both, §2.4, so such a machine lists with `--tier
  local`.)*
* A routed project whose binding sets `ledger = "github"` is refused when the command starts,
  naming decision 12. *(Release scope.)*
* A project with a binding and no routing map is mode A or B as today: one tracker, GitHub.
* **An older fl** reading a format-3 manifest refuses it as an unknown format, as it does any
  newer format; it cannot import a routed project. On a machine whose store is already bound
  and that runs an older fl, the project behaves as mode A and creates GitHub items with no
  area. *(Release scope — a known limit.)*

### 1.4 The local store's format

The routing map, the area field, the "escalating" marks and the tombstones of §3 are new
tables or fields. As with the ledger root (format 4), the store raises its format to **5** the
first time one of them is written; an older fl refuses a format-5 store. A store that never
routes stays at its current format. *(Release scope.)* Plans A and B share format 5 only if no
release ships between them; otherwise plan B raises the store to 6, because an fl built from
plan A would open a store holding marks and ignore them. (Owner informed, 2026-10-06.)

---

## 2. The routing tracker

`fl-core::TieredTracker { local, github, map }` implements `Tracker` over the two tiers. The
CLI builds it for a routed project and passes it where it passes one tracker today; the gate
engine does not change. It follows the composable wrappers the code already has. The
`Tracker` conformance suite (identity spec §6.1) runs over it.

### 2.1 Create

* `Tracker::add_record` gains the area, and `Finding` carries it. `[agent]` The tier override
  is not part of the trait: the CLI calls the router's own `create_in(tier, …)` when `--tier`
  is given. Every `Tracker` implementation and wrapper changes with the trait.
* The tier comes from `--tier local|github` when given, else from the item's area through the
  map. The area is recorded either way.
* In a routed project an item with no area, or with an area the map does not declare, is
  refused, and the error lists the declared areas.
* **Security.** The GitHub tracker's rule stays: a security finding goes only to a private
  repository (`SecurityNotPrivate`). When the map sends a security finding to a non-private
  repository, fl refuses and the error says to use `--tier local`. It never moves the finding
  by itself. *(Invariant.)*

### 2.2 Lookup

* By IRI, the router asks the tier that owns it:
  * an issue URL of the bound repository → GitHub. *(Rev 2.1, `[agent]`: "the bound
    repository" is its configured name, or, once GitHub is open, its name now; an old name of
    a renamed repository cannot be told from another repository without a request, so any
    other issue URL is asked of the local tier first, then of GitHub, whose own check resolves
    an old name.)*
  * a `urn:uuid:` IRI → the local store. If the local store holds a tombstone for it, the
    tombstone's target. If the local store does not hold it, the GitHub alias scan, because
    it may be an item escalated on another machine. `[agent]`
* An item the local store does not hold and GitHub does not answer to is reported as **held
  on another machine's local tier, or not existing** — never `NotOwned` as if the id were
  malformed. `[agent]` A local item exists in exactly one store on one machine.
* An item marked "escalating" and the GitHub issue whose alias is its IRI are one item, not
  two: the lookup returns the GitHub issue once it exists. `[agent]` *(Rev 2.4, `[agent]`: best
  effort. The lookup of a marked item asks GitHub by the escalation's own search, and only a
  marked item pays for it; the issue takes over only when it is found and GitHub reads it as an
  fl item. Otherwise — no issue, an issue fl cannot read yet (a stop before its labels), or
  GitHub unbound or out of reach — the local copy answers, as it last was, and the local store
  refuses every write to it (§3.3 step 1), so no write lands in two places. A merged list shows
  the item once, as the issue; `--tier local` shows the marked local item.)*

### 2.3 Handles

In a routed project the two kinds of handle look different, so one handle never means two
items:

* `#41` and `owner/repo#41` always mean a GitHub issue. `[agent]` `Ref` gains a variant for
  `#41`; today `#41` and `41` parse alike (`cli/src/refs.rs:38`).
* A bare `41` means local item 41. A bare number that is not a local item is refused with the
  hint "did you mean `#41`?" when the GitHub tier is available.
* A local handle of an escalated item resolves through its tombstone.
* Output prints GitHub items as `#41` and local items as `41`, so every printed handle can be
  typed back.
* `fl github …` commands keep reading a bare number as an issue: they name GitHub items only.
* **Migration.** The first `fl routing set` prints what changes. `[agent]`
  * In a project that was local-only, `#3` meant local item 3 and now means issue 3.
  * In a project that was GitHub-only, a bare `41` meant issue 41 and now means local item 41;
    local records made before the binding appear again in lists.
  * *(Rev 2.1, `[agent]`)* The import that first brings a routing map to another machine prints
    the same handle change there.

In a project without a routing map, handles are unchanged: in a GitHub project a bare `41`
still means issue 41.

### 2.4 Lists

* `fl record list` and `fl finding list` read both tiers, merge the results, and show each
  item's tier in a tier column. The column appears only in routed projects, so scripts that
  read today's output are unaffected elsewhere. `[agent]` `--tier local|github` reads one.
* If either tier cannot be read, the merged list is an **error, never a partial list**; when
  GitHub is the tier that failed, the error suggests `--tier local`. *(Invariant — a list that
  cannot see its whole population fails.)* A machine with no binding cannot read the GitHub
  tier, so it lists with `--tier local` *(rev 2.1, `[agent]`)*.
* `fl finding list --record <id>` is new: it lists one record's findings, from both tiers.
* An item marked "escalating" is listed with that mark. Tombstones are not listed. *(Rev 2.4,
  `[agent]`: in a routed store's lists the tier column of a marked item reads `escalating`.)*
* The withdrawal counts that `fl finding list` prints sum both tiers.

### 2.5 References across tiers

* The router checks a reference — a finding's record — in the tier that owns it. If that tier
  cannot be reached, the result is an error, never "no such record". *(Invariant — identity
  spec §3.4.)*
* The inner trackers then store a reference into the other tier **only through a value the
  router builds after its check**: a trait method
  `add_finding_checked(finding, checked: ForeignRecord)`, where `ForeignRecord` cannot be
  built outside the router. `[agent]` Today each tracker refuses a record it does not hold
  (`store/src/lib.rs:973`, `github/src/tracker.rs:1864`), and `CatalogChecked` refuses any
  record the catalog's store holds (`core/src/store.rs:431`), which in a routed project is the
  local tier. So the router wraps the GitHub tier with a check of the project only.
* **The GitHub block's record reference** for a local record is `{ id, title }` with no
  `node_id` (decision 14; rev 2.1 — the field is `id`, as in every reference, and `title` lets
  the issue's text be rendered from the block on every rewrite); the block is then
  `fl_format` 2. The issue shows the record's title and IRI
  as plain text, since a reader on GitHub cannot open a local item. `fl` resolves the IRI.
* A local finding may name a GitHub record the same way.
* **Disclosure.** A GitHub finding about a local record publishes that record's title. On a
  non-private repository fl warns before it writes. `[agent]` (As the ledger's decision 16
  does for its own text.) About a local record in a sensitive area — or in an area the map no
  longer declares, which counts as sensitive (decision 22) — fl refuses instead, naming `--tier
  local` (decision 21), and the finding carries the security flag wherever it is placed.
* **Evidence.** Runs and decisions about a finding are tagged with its record's IRI. The router
  resolves that IRI through any tombstone first, so new evidence names the record where it now
  lives. `[agent]`

### 2.6 GitHub opens lazily

Decision 19: the CLI opens the GitHub tier on the first call that needs it — a GitHub-tier
create, a lookup of a GitHub IRI or `#41` handle, a merged list. Commands on local items run
offline and with no credential. *(Release scope.)*

### 2.7 Concurrency

The local store is a redb file that one process opens exclusively for a whole command; a
second process is refused (`store/src/lib.rs:293`). A local item exists in one store on one
machine. So two escalations of one local item cannot interleave, and no other machine can
write it. This design relies on that. *(Invariant for the local tier.)*

---

## 3. Escalation

### 3.1 Commands

`fl record escalate <id> --by <who> --reason <text>`, and `fl finding escalate` with the same
options. Both refuse an item that is not local, an item already escalated (naming where it
went), a project that is not routed, and a project whose GitHub tier is not available. A
finding may be escalated while its record stays local (decision 5).

### 3.2 Checks before anything is written

Everything that would make the GitHub create refuse is checked **before** step 1, so an item
is never marked and then stranded. `[agent]` The command refuses:

* a closed state — a record `done`, a finding `fixed` or `withdrawn` (the GitHub tracker
  creates open issues only);
* a title the GitHub tracker refuses (over 256 characters, or with whitespace at either end);
  *(rev 2.4, `[agent]`: a finding's issue title is its claim's first line, trimmed and cut to
  256, so only a record's title can fail this)*
* a security finding, a sensitive area, or a finding about a local record in a sensitive area
  (decision 21), when the repository is not private;
* a finding whose reproduction gate is not in the committed manifest (GitHub tracker spec
  §4.3);
* an alias that another issue already uses, or a local alias that is an issue URL of the bound
  repository (the one-namespace rule).

On a non-private repository fl warns before it publishes the title, `--reason`, `--by` and,
for a record, the list of its open findings (decision 18).

### 3.3 Steps

Each step can be run again, and running the command again resumes from where it stopped.

1. **Mark the local item "escalating"**, with who, why and the time. From then on the local
   store refuses writes to the item, and the refusal names `fl record escalate <id>` (or
   `fl finding escalate <id>`) to finish. A finding raised against a marked record is not a
   write to it and is allowed.
2. **Find or create the GitHub issue.**
   * The create key is derived from the old IRI. *(Rev 2.4, `[agent]`: the key is the old IRI
     itself. Today's key is random and searched for only inside one create, back to that
     attempt's start, so the escalation has a search and a create of its own: the create
     carries the item's own state and aliases, which no create carried before.)*
   * Before any create, fl searches every issue, labelled or not, newest first, back to the
     mark's time less the create-search margin, for that key. `[agent]` (Review: today's
     search runs only inside one create after an ambiguous failure, and only back to that
     attempt's start, `github/src/tracker.rs:1197`, so a rerun an hour later would make a
     second issue.) A found issue without fl's labels gets them, as in an ordinary create.
   * Only when the search finds nothing does fl create the issue: the item's title, state,
     area and fl block; aliases that include the old local IRI and the item's own aliases.
     The issue's text names who escalated it, why, and the old IRI, and for a record lists its
     open findings (decision 18). *(Rev 2.4, `[agent]`: a finding's issue text is its claim, so
     who, why and the old IRI are a block field, `escalated: { from, by, reason }`, and the issue
     shows a line rendered from it and stripped on read; a block that carries it is written with
     `fl_format` 3, so an older fl says "upgrade fl", not "damaged". A record's open findings are
     listed in its issue's text as they stood at the escalation.)*
3. **Replace the local item with a tombstone** — the old IRI, the new IRI, who, when, why.

The item is never live in both tiers: step 1 blocks local writes before step 2 can make the
GitHub copy, and no other process or machine can write it (§2.7). *(Invariant — one live
copy.)* An item left "escalating" is listed (§2.4), so it cannot be forgotten.

**Abandoning.** `fl record escalate <id> --abandon` (and the finding form) removes the mark,
and only after the step-2 search proves no issue exists. `[agent]` Once the issue exists, the
only way on is step 3.

### 3.4 The `needs_human` trigger

In a routed project, `fl record move <id> --to needs_human` on a local record runs the
transition as today — gates, ledger entry, then the state change (evidence before state,
identity spec §3.5). *(Rev 2.4, `[agent]`: a routed project's ledger is local and writes no
decision of its own, so the move's evidence is its gate runs; an ungated move writes none.)* **If the move landed**, it then runs the escalation; a refused move
escalates nothing. If the escalation fails, the record stays local, in `needs_human`, marked
"escalating" if step 1 ran, and the command exits with the move's own code and a `warning:`
naming `fl record escalate <id>` (decision 16).

The ladder, when it comes, ends by moving a record to `needs_human`; escalation follows from
that with no new mechanism.

### 3.5 History

* Ledger entries stay under the old IRI; nothing is copied. The GitHub issue names the old
  IRI, so its history can be found. The ledger is local in a routed project (decision 12).
* An escalation is a move, not a transition: it writes no ledger entry of its own. The
  tombstone records who, when and why.
* The findings of an escalated record stay where they are. Their record reference is not
  rewritten: it resolves through the tombstone to the new IRI (and new evidence names the new
  IRI, §2.5). This avoids one write per finding, and the crash windows those writes would add.
* `fl github repair` on an escalated issue works as on any issue: its block is the item.

### 3.6 In the local store

* A marked item reads as itself with its mark; a write to it is refused with a new
  `StoreError::Escalating { id, to_finish }`.
* A tombstoned id reads as a new `StoreError::Escalated { from, to }`, which the router follows
  and the CLI never shows when it can follow it. *(Rev 2.1, `[agent]`: rev 2 named it `Moved`,
  which `StoreError::Moved { id, to }` already is — a transferred issue, `core/src/store.rs:155`.)*

---

## 4. Errors

Every refusal names its cause and what to do:

| case | what fl says |
|---|---|
| no area, routed project | the areas the manifest declares |
| an area the manifest does not declare | the name, and the declared areas |
| `--area` in a project with no routing map | `fl routing set` declares areas |
| the `github` tier, no binding on this machine | the missing config entry; never a fallback |
| a routing map with `ledger = "github"` | decision 12: use the local ledger |
| a security finding or sensitive area routed to a non-private repository | use `--tier local` |
| a GitHub finding about a local record in a sensitive area, on a non-private repository | use `--tier local` (decision 21) |
| `fl routing set` while the store holds another project | give the project its own store (decision 20) |
| `fl routing set --not-sensitive` on an area items still name | the count, and up to ten of the items (decision 22) |
| a later `fl routing set` that adds an area with neither `--sensitive` nor `--not-sensitive` | both flags, and that a removed area may have been sensitive (decision 23) `[agent]` |
| a tier that cannot be reached | an error in lists, lookups and reference checks — never "no such item", never a partial list |
| an id no tier holds | held on another machine's local tier, or not existing |
| a write to an item marked "escalating" | `fl … escalate <id>` to finish, or `--abandon` |
| an escalation that the GitHub create would refuse | the reason, before anything is written |
| `--abandon` after the issue exists | the issue; finish the escalation instead |
| a bare handle that is not a local item | did you mean `#41`? |
| an escalation of an item already escalated | where it went |
| `fl routing remove` on an area items still name | the count, and up to ten of the items |
| an issue whose `fl:area/…` label differs from its block | diverged; `fl github repair` |
| a manifest import that is not current | import it, as for gated moves |

---

## 5. Testing

* **Unit tests** in the module they test: the routing map, its starting set and sensitivity;
  the refusal to remove an area in use (in each tier, by block not label, and when a tier
  cannot be read); manifest formats 1–3 chosen as in §1.2; area-name validation; handle
  parsing in each mode; the store's format raise to 5; `Escalating` and `Escalated`; a `set`
  that names no sensitivity keeps the area's (decision 22); a routed store refuses a map while
  it holds another project, and another project once routed, by `add` or by import
  (decision 20).
* **`TieredTracker` over two in-memory trackers**, and the `Tracker` conformance suite over
  it: create routing by area, by inherited area and by `--tier`; the refusals of §4; merged
  lists and the tier column; a failed tier as an error; references across tiers through
  `ForeignRecord` only; tombstone resolution; the alias-scan fallback for an id the local
  store does not hold; evidence tagged with the resolved IRI; a finding about a local record in
  a sensitive area, or in an area the map no longer declares, refused on a non-private
  repository naming `--tier local` and marked as a security finding wherever it is placed
  (decisions 21, 22).
* **Lazy GitHub:** a local-tier command with the fake GitHub down succeeds; a merged list with
  it down is an error that suggests `--tier local`.
* **Escalation over the local store and the fake GitHub:**
  * each pre-check refuses before the mark;
  * stopped after each step, a rerun completes it with no duplicate issue and never two live
    copies — including a rerun more than the create-search margin after a stop between the
    create and its label call;
  * `--abandon` before and after the issue exists;
  * a record's issue lists its open findings, none a security finding or in a sensitive or
    undeclared area *(rev 2.4, `[agent]`)*.
* **The `needs_human` trigger:** a refused move escalates nothing; an escalation that fails
  after a landed move leaves the record local, in `needs_human`, marked, the ledger entry
  written, the move's exit code and the warning.
* **GitHub blocks:** `fl_format` 2 exactly when the block carries an area or a local record
  reference; the area label kept by every label rewrite; a missing or wrong area label reads
  as diverged and `fl github repair` restores it.
* **CLI black-box tests** in `tests/`: `--area`, `--tier`, `--sensitive`, `fl routing …` and
  its migration notice, the escalate commands, `fl finding list --record`, and list output
  with and without the tier column. Decision 20: `fl routing set` with a second project in
  the store is refused, names the remedy and writes nothing. Decision 21: a GitHub finding
  about a sensitive local record on a public repository is refused and nothing is created;
  one about an ordinary local record is warned about. Decision 22: a tier change keeps the
  sensitivity; `--not-sensitive` with items naming the area is refused and writes nothing,
  and with none it clears; the re-review's scenario end to end — an area removed where the
  project is authored, then re-imported, leaves another machine's record protected.
  `[agent]` Decision 23: a later set that adds an area with no flag is refused and writes
  nothing; with `--sensitive` it works; with `--not-sensitive` it is refused while an item
  names the area and works when none does; the first set needs no flag; the final review's
  scenario end to end — remove, set again with no flag — is refused at the set.
* **One live test** on the private throwaway repository: escalate a local record; the issue
  exists, carries the old IRI, its area label and its findings list, and the local item is a
  tombstone. *(Rev 2.4, `[agent]`: over `MemStore` and the router; the live tests drive the
  GitHub tracker directly and fl-github has no redb store. A shared conformance suite holds
  `MemStore` and the redb store to one escalation contract.)*

---

## 6. Open

None. The review of rev 1 raised eight owner decisions (12–19) and the owner decided them on
2026-10-06; planning plan A raised three more (20–22), decided the same day; the rest are
`[agent]` rulings, marked where they are made. `[agent]`: the final review of plan A raised one
more (23), decided 2026-10-07. Planning plan B raised no owner decision; its seven corrections
and its ruling on a marked item's lookup are `[agent]` rulings (rev 2.4).

---

## 7. Roadmap

* **Named trackers** (owner, 2026-10-05: "I see B as the long-term roadmap option"): any number
  of trackers, each named, the map routing an area to a name — for example a public repository
  for product items and a private one for security items. The manifest's tier names are
  already strings, so the format does not change.
* **Escalation rules in config** (owner, 2026-10-05: "C is ultimately the goal"): for example,
  escalate a record whose attempts have spent more than a limit. They plug into §3 as a new
  trigger.
* **The ladder** (ratified decision 14): its last rung moves a record to `needs_human`, which
  §3.4 already turns into an escalation.
* **The GitHub ledger in a routed project** (decision 12): decisions about local-tier items
  would stay local (`SplitLedger` would leave them local rather than refuse them), and the
  pre-flight would skip them.

---

## 8. Delivery

Two plans, as for sub-projects 2 and 3:

* **Plan A — routing.** The area (model, store, block, label), the routing map and its
  commands, manifest format 3, store format 5, `TieredTracker` (create, lookup, handles,
  lists, references across tiers, `ForeignRecord`), lazy GitHub, decisions 12–14 and 20–23
  (`[agent]`: 23 added in rev 2.3), the migration notice.
* **Plan B — escalation.** The pre-checks, the mark, the find-or-create step with its own
  search, the tombstone, `--abandon`, the findings list in the issue, the `needs_human`
  trigger, the live test. Plan B is written after plan A merges.
