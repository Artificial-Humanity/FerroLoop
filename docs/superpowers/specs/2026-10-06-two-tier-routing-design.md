# Two-tier routing and escalation — design

**Date:** 2026-10-06
**Status:** Design approved by the owner section by section, 2026-10-05/06; this written spec
awaits the owner's review. Sub-project 4 of 4.
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

An untagged statement is descriptive, not binding.

---

## 0. Where this sits

The identity spec (`2026-09-23-identity-and-store-roles-design.md` §0.1) names a two-tier
configuration as the flow we recommend: the local tracker carries developer-level items
(code, code quality, unit tests) and is not closed to humans; GitHub carries human-level
items (design review, product review, security review, and code-quality items that escaped
the local phase or were escalated past it). Sub-projects 2 and 3 built the GitHub tracker and
ledger. Each project still binds exactly one tracker (identity spec §3.3). This sub-project
lets a project bind two, and decides the two questions that spec left open (§7): which
tracker receives a new item, and how an item moves between the tiers.

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
   project may add or rename areas; an area the manifest does not declare is refused.
7. **An area is required only where it routes something:** in a two-tier project. In a
   one-tier project it is optional.
8. **Exactly two tiers, with fixed names `local` and `github`.** Named trackers, any number
   of them, are the roadmap (§7).
9. **The routing lives in a routing tracker** that implements the `Tracker` trait over the
   two tiers, not in each CLI command or in the gate engine.
10. **An item's area is also a GitHub label** (`fl:area/<name>`), so GitHub's own filters show
    it (owner, 2026-10-06).
11. **`fl routing remove` is refused while any item in either tier names the area**
    (owner, 2026-10-06).

### 0.2 Out of scope

* Moving an item from GitHub back to the local tier.
* Escalation rules in config (decision 4), and the ladder's other rungs (decision 1).
* Named trackers (decision 8).
* Any change to the ledger's routing (the GitHub ledger spec §2): ledger entries follow the
  records they concern, as today.

---

## 1. Data

### 1.1 The area

* `Record` and `Finding` gain `area: Option<String>`. An item written before this change
  reads as having no area.
* In the local store the field is serialized with the item. In GitHub it is a field of the
  issue's fl block (`Meta.area`, skipped when absent), so an fl that predates it still reads
  the block.
* **On GitHub the area is also a label, `fl:area/<name>`** (decision 10). It is one of fl's
  labels: the create's label call adds it with the kind and state labels, fl creates the label
  in the repository the first time an area is used there, and `fl github repair` restores it
  from the block. The block stays the source of truth: an issue whose area label is missing or
  differs from its block reads as diverged, as a wrong state label does today. The area never
  changes after creation, so the label adds nothing to an update's conflict window.
* An area is a name the manifest declares (§1.2). An item keeps its area for its whole life;
  escalation does not change it. *(Release scope.)*

### 1.2 The routing map

* The map — area name → `local` | `github` — is authored in the project's store, as gates
  are, and exported into `.fl/manifest.json` as a `routing` section. A manifest edited by hand
  is refused, as today: the hash covers the new section too.
* **Manifest format 3** adds `routing`. An export writes 3 exactly when the project has a
  routing map, and otherwise writes 2 or 1 as today, so a project without routing still
  exports a manifest every older fl reads.
* A machine that imports the manifest imports the map with it, so every machine and every
  agent routes the same way. *(Invariant — one routing rule per project, not per machine.)*
* Commands:
  * `fl routing set <area> <tier>` — the project's first `set` writes the starting set first:
    `code` and `tests` → `local`; `design`, `product` and `security` → `github`.
  * `fl routing set <area> <tier>` on an area that exists changes its tier for new items
    only; items already made stay where they are.
  * `fl routing remove <area>` — **refused while any item in either tier names the area**
    (decision 11). The refusal gives the count and lists up to ten of the items. Finding them
    reads both tiers, so a tier that cannot be read refuses the removal too: fl never removes
    an area it could not check.
  * `fl routing show`.

### 1.3 When a project has two tiers

A project is **two-tier** when its routing map names both `local` and `github` **and** the
machine's config binds a GitHub repository for it (the `tracker` binding of
`$XDG_CONFIG_HOME/fl/config.toml`, unchanged). The binding is the `github` tier; the local
store is the `local` tier.

* A project whose map routes to `github` on a machine with no binding refuses to create a
  `github`-tier item and names the missing config entry. fl never falls back to `local`.
  *(Invariant — routing never changes tier silently.)*
* A project with a binding and no routing map is mode A or B as today: one tracker, GitHub.

### 1.4 The local store's format

The routing map, the area field and the escalation records of §3 are new tables or fields.
As with the ledger root (format 4), the store raises its format to **5** the first time one
of them is written; an older fl refuses a format-5 store. A store that never routes stays
at its current format. *(Release scope.)*

---

## 2. The routing tracker

`fl-core::TieredTracker { local, github, map }` implements `Tracker` over the two tiers. The
CLI builds it for a two-tier project and passes it where it passes one tracker today; the
gate engine does not change. It follows the composable wrappers the code already has
(`CatalogChecked`, `KindRouted`).

### 2.1 Create

* The tier comes from `--tier local|github` when given, else from the item's area through the
  map. The area is recorded either way.
* In a two-tier project an item with no area is refused, and the error lists the areas the
  manifest declares. An area the manifest does not declare is refused the same way.
* **Security findings.** The GitHub tracker's rule stays: a security finding goes only to a
  private repository (GitHub tracker spec, `SecurityNotPrivate`). When the map sends a
  security finding to a public repository, fl refuses and the error says to use
  `--tier local`. It never moves the finding by itself. *(Invariant.)*

### 2.2 Lookup

* By IRI: the router asks the tier that owns it. A `urn:uuid:` IRI is local; an issue URL of
  the bound repository is GitHub. A local tombstone (§3) resolves to the IRI it points to.
* An IRI or handle that resolves in both tiers is refused with every candidate listed; the CLI
  never picks one. *(Invariant — identity spec §4.)*

### 2.3 Handles

In a two-tier project the two kinds of handle look different, so one handle never means two
items:

* `#41` and `owner/repo#41` always mean a GitHub issue.
* A bare `41` means local item 41. A bare number that is not a local item but is a GitHub
  issue is refused with the hint "did you mean `#41`?".

In a one-tier GitHub project a bare `41` still means issue 41, as today.

### 2.4 Lists

* `fl record list` and `fl finding list` read both tiers, merge the results, and show each
  item's tier. `--tier local|github` reads one.
* If either tier cannot be read, the merged list is an **error, never a partial list**; when
  GitHub is the tier that failed, the error suggests `--tier local`. *(Invariant — a list that
  cannot see its whole population fails.)*
* `fl finding list --record <id>` reads both tiers: a record's findings may be split.
* An item marked "escalating" (§3.2) is listed with that mark.

### 2.5 References across tiers

* The router checks a reference — a finding's record — in the tier that owns it. If that tier
  cannot be reached, the result is an error, never "no such record". *(Invariant — identity
  spec §3.4.)* The inner trackers then store a reference into the other tier without checking
  it.
* **One GitHub rule changes.** Today a GitHub finding's record must be an fl issue of the same
  repository (GitHub tracker spec §3.1). In a two-tier project it may also be a local record;
  the issue then shows that record's title and its IRI as plain text, since a reader on GitHub
  cannot open a local item. `fl` resolves the IRI.
* A local finding may name a GitHub record the same way.

---

## 3. Escalation

### 3.1 Commands

`fl record escalate <id> --by <who> --reason <text>`, and `fl finding escalate` with the same
options. Both refuse an item that is not local, an item already escalated (naming where it
went), and a project that is not two-tier.

### 3.2 Steps

Each step can be run again, and running the command again resumes from where it stopped.

1. **Mark the local item "escalating"**, with who, when, why. From then on the local store
   refuses writes to it, and the refusal names `fl record escalate <id>` (or
   `fl finding escalate <id>`) to finish.
2. **Create the GitHub issue** with the item's title, state, area and fl block; its aliases
   include the old local IRI and the item's own aliases. The issue's text names who escalated
   it, why, and the old IRI. **The create key is derived from the old IRI**, so a run that
   stopped here finds the issue on the next run (the create-key search, GitHub tracker spec
   §3.3) and never makes a second one.
3. **Replace the local item with a tombstone** — the old IRI, the new IRI, who, when, why.
   The tombstone is a new local table (§1.4).

The item is never live in both tiers: step 1 blocks local writes before step 2 can make the
GitHub copy. *(Invariant — one live copy.)* An item left "escalating" is listed (§2.4), so it
cannot be forgotten.

### 3.3 The `needs_human` trigger

In a two-tier project, `fl record move <id> --to needs_human` on a local record runs the
transition as today — gates, ledger entry, then the state change (evidence before state,
identity spec §3.5) — and then runs the escalation. If the escalation fails, the record stays
local, in `needs_human`, marked "escalating"; the command exits with an error naming
`fl record escalate <id>` to finish it.

The ladder, when it comes, ends by moving a record to `needs_human`; escalation follows from
that with no new mechanism.

### 3.4 History

* Ledger entries stay under the old IRI; nothing is copied. The GitHub issue names the old
  IRI, so its history can be found.
* An escalation is a move, not a transition: it writes no ledger entry of its own. The
  tombstone records who, when and why.
* The findings of an escalated record stay where they are. Their record reference is not
  rewritten: it resolves through the tombstone to the new IRI. This avoids one write per
  finding, and the crash windows those writes would add.

---

## 4. Errors

Every refusal names its cause and what to do:

| case | what fl says |
|---|---|
| no area, two-tier project | the areas the manifest declares |
| an area the manifest does not declare | the name, and the declared areas |
| the `github` tier, no binding on this machine | the missing config entry; never a fallback |
| a security finding routed to a public repository | use `--tier local` |
| a tier that cannot be reached | an error in lists, lookups and reference checks — never "no such item", never a partial list |
| a write to an item marked "escalating" | `fl … escalate <id>` to finish |
| a bare handle that is only a GitHub issue | did you mean `#41`? |
| a handle or IRI that resolves in both tiers | every candidate |
| an escalation of an item already escalated | where it went |
| `fl routing remove` on an area items still name | the count, and up to ten of the items |
| an issue whose `fl:area/…` label differs from its block | diverged; `fl github repair` |

---

## 5. Testing

* **Unit tests** in the module they test: the routing map and its starting set, the refusal
  to remove an area in use (in each tier, and when a tier cannot be read), manifest
  format 3 (and format 2 still exported without routing), handle parsing in each mode, the
  escalation steps, the store's format raise to 5.
* **`TieredTracker` over two in-memory trackers:** create routing by area and by `--tier`,
  refusals of §4, merged lists, a failed tier as an error, references across tiers, tombstone
  resolution.
* **Escalation over the local store and the fake GitHub, stopped after each step:** each rerun
  completes it, with no duplicate issue and never two live copies.
* **The `needs_human` trigger:** an escalation that fails after the transition leaves the
  record local, in `needs_human`, marked "escalating", and the ledger entry written.
* **CLI black-box tests** in `tests/`: `--area`, `--tier`, `fl routing …`, the escalate
  commands, the list output with its tier column.
* **The area label:** a GitHub item carries `fl:area/<name>`; a missing or wrong area label
  reads as diverged and `fl github repair` restores it.
* **One live test** on the private throwaway repository: escalate a local record; the issue
  exists, carries the old IRI and its area label, and the local item is a tombstone.

---

## 6. Open

None. The two questions left open when this spec was written — the area as a GitHub label,
and removing an area in use — were decided by the owner on 2026-10-06 (decisions 10 and 11).

---

## 7. Roadmap

* **Named trackers** (owner, 2026-10-05: "I see B as the long-term roadmap option"): any number of
  trackers, each named, the map routing an area to a name — for example a public repository
  for product items and a private one for security items. The manifest's tier names are
  already strings, so the format does not change.
* **Escalation rules in config** (owner, 2026-10-05: "C is ultimately the goal"): for example,
  escalate a record whose attempts have spent more than a limit. They plug into §3 as a new
  trigger.
* **The ladder** (ratified decision 14): its last rung moves a record to `needs_human`, which
  §3.3 already turns into an escalation.
