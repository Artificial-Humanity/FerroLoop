# Stable identity and store roles — design

**Date:** 2026-09-23
**Status:** Approved design. Sub-project 1 of 4. Not started.
**Scope:** Replace the `u64` counter ids with IRIs, and split persistence into three
roles that a store backs. Local stores only. No GitHub code.

---

## Reading this document

Constraints carry a status tag:

| tag | meaning |
|---|---|
| **Invariant** | Must hold in any version. Violating it breaks the product's premise. |
| **Release scope** | True of this sub-project's release. A later version can change it. |
| **Open** | Undecided. Named here so it is not decided by accident. |

An untagged statement is descriptive, not binding.

---

## 0. Why now

The ratified-decisions record marked stable identity `[open]` on 2026-09-21.
`ProjectId`, `RecordId`, `GateId` and `FindingId` are each `pub struct X(pub u64)`:
counters scoped to one local redb file. Two installations both have gate 3, and
nothing tells them apart. Decision 34 (JSON-LD as a forward inclusion) needs an
`@id` for each node, and it rests on this.

On 2026-09-23 the owner moved the GitHub integration earlier. GitHub Issues becomes a
**tracker that a store can back**, used before the local tracker matures. It is also a
permanently supported configuration for organizations that choose it. The local store
stays the default. That decision makes identity urgent: once one `fl` talks to a
GitHub tracker and a local store at the same time, an id must work in both.

### 0.1 The configurations this design has to allow

| configuration | tracker | ledger | status |
|---|---|---|---|
| Local | local | local | the product default |
| GitHub mode A | GitHub | local, or a CI artifact | closer to our own end state |
| GitHub mode B | GitHub | GitHub | where we start |
| Two-tier | local for developer-level items, GitHub for human-level items | local | the flow we recommend |

In the two-tier flow the two trackers carry **different subject matter**. The local
tracker is agent- and developer-facing: code, code quality, unit tests. It is not
closed to humans. GitHub carries human-level items: design review, product review,
security review, and code-quality items that escaped the local phase or were escalated
past it. The two overlap only where the local phase let something through.

### 0.2 The four sub-projects

1. **Identity and store roles** — this document.
2. **GitHub tracker store** (mode A). Includes the App, for a bot identity, and the
   disclosure rule for security findings on a public repo. Updates the review-layer
   spec (`2026-09-22-github-review-layer-design.md`), whose Release-scope items this
   change reverses.
3. **GitHub ledger store** (completes mode B). Rate limits are the main design problem.
4. **Two-tier routing and escalation.**

### 0.3 Identity priorities

The owner chose to decide the identity format now and build only what milestone 2
needs. The order in which identity must then serve real cases:

1. `fl` in CI, whose store meets a local one.
2. Cross-vendor exchange (Claude, Codex, Gemini).
3. Moving or merging stores.

---

## 1. Terms

* A **store** is a backing entity: a redb file, an in-memory map for tests, and later a
  GitHub repository.
* A **role** is a set of operations that a store backs. There are three:

| role | holds | character |
|---|---|---|
| **Catalog** | Project, GateDef, Transition | Definitions. Rarely changed. Each belongs to a repository. |
| **Tracker** | Record, Finding | Mutable state that people discuss. |
| **Ledger** | GateRun, Attempt | Append-only evidence. |

* A store backs one or more roles. In this sub-project one redb store backs all three.
* An **id** is an IRI that names one item. It is the only thing a reference stores.
* A **handle** is a short name that a person types and reads. It is display only.

---

## 2. Identity

### 2.1 Type

One validated `Iri` newtype. The per-kind wrappers stay and wrap `Iri` in place of
`u64`: `ProjectId(Iri)`, `GateId(Iri)`, `RecordId(Iri)`, `FindingId(Iri)`. The compiler
still refuses a finding id where a gate id is expected. On the wire an id is a plain
string.

### 2.2 Minting

* **Local stores** mint `urn:uuid:<UUIDv7>`, lowercase. UUIDv7 is unique without
  coordination and sorts by creation time.
* **GitHub** (sub-project 2) uses the canonical issue URL as the id, and also stores
  GitHub's `node_id` as the match key. A repository rename changes the URL and does not
  change the `node_id`. When they disagree, `node_id` wins: record the new URL and keep
  the old one as an alias.
* **Projects.** A local project gets a `urn:uuid:`. A GitHub project's id is its
  repository URL.

**Dependency.** The `uuid` crate with the `v7` feature (MIT OR Apache-2.0). Minting
needs randomness, and the standard library has no source of it. Our own v7 code over
`getrandom` would still need one crate.

A UUIDv7 contains its creation time, so a public id shows when an item was made. A
GitHub issue shows that too. This is a known choice, not an oversight.

### 2.3 Normalization

An id is normalized once, when it is minted or when it enters the process. After that,
ids compare as exact strings. **No comparison runs on an id that was not normalized.**
*(Invariant)*

### 2.4 Parse and resolve are different steps

A parse accepts any absolute IRI. Resolution refuses an IRI that no configured store
owns. A local store asked for a GitHub URL answers "not mine". It does not claim that
the item is absent. *(Invariant)*

### 2.5 Aliases

An item that can move carries `also_known_as: Vec<Iri>`. A lookup checks aliases as
well as the primary id. This is the only hook for moving stores that this sub-project
builds. There is no move tooling.

### 2.6 Several local stores

A local store is multi-tenant: one store holds many projects. It already works this way.

* **Binding.** Each project binds exactly one local store through a config value. Several
  projects can share a store, and a project can have a store of its own.
* **Where the config lives.** A user-level file, `$XDG_CONFIG_HOME/fl/config.toml`, maps a
  project root to a store path. Precedence: `--db`, then `$FL_DB`, then this file, then the
  XDG default (`$XDG_DATA_HOME/fl/fl.redb`). The file is user-level, not in the repository,
  because a store path is specific to a machine, and a public repository would publish it.
* **Ownership.** `urn:uuid:` does not say which local store minted an id. So a local store
  owns a `urn:uuid:` **only if it holds that id**, live or deleted. *(Invariant)*
* **Tombstones.** Deleting an item leaves a tombstone. "This store had it and it is gone"
  stays distinct from "no store has it". *(Invariant)*
* **An id that no store holds** is `NotOwned`, and the message lists every store it
  searched. The output states exactly where it looked. *(Invariant)*

GitHub stores need none of this, because a URL names its home.

---

## 3. The store roles

### 3.1 Three traits

`Catalog`, `Tracker` and `Ledger` replace the single `Store` trait in
`crates/core/src/store.rs`:

* `Catalog` — projects, gates, transitions.
* `Tracker` — records, findings, `withdrawals_by`.
* `Ledger` — append-only gate runs and attempts.

There is no supertrait that means "all three". A store implements the roles it backs.
`RedbStore` and `MemStore` implement all three.

### 3.2 The engine asks for only what it uses

Engine code takes the roles an operation touches, and no more. Gate evaluation takes a
`Catalog` and a `Ledger`. The type signature then shows which roles each operation
touches. A GitHub store in sub-projects 2 and 3 implements traits. It does not require
an engine rewrite.

### 3.3 Resolution

Each store implements `owns(&Iri) -> bool`. A small value binds each role to the store
that backs it and routes each lookup to its owner. Its shape allows several trackers
for sub-project 4. This sub-project binds exactly one store to each role.

### 3.4 References between roles

A store checks only references that stay inside it. A reference into another store —
for example a local gate run that names a GitHub record — is checked at the engine layer
through the role binding. **If that check cannot reach the other store, the result is
ERROR, never "no such record".** *(Invariant)*

### 3.5 Evidence before state

A split loses the single-file transaction, so the write order carries the safety. The
ledger entry is written before the tracker state changes. *(Invariant)* A crash between
the two leaves evidence with no state change, which is safe to retry. The reverse order
leaves a state change with no evidence, which is a pass over a population that nothing
examined.

The code already writes in this order: `record move` evaluates the transition, which
appends gate runs, and only then calls `set_record_state`. This design makes the order a
rule and adds a test that holds it.

### 3.6 Out of scope

* Moving the catalog into the repository's acceptance manifest (sub-project 2). In GitHub
  mode, an issue's reproduction must name a gate that a reader of the issue can resolve,
  so the catalog must then live in the repository. That work is not done here.
* Any GitHub code.
* Choosing which tracker receives a new item (sub-project 4).

---

## 4. Handles

* A handle is display only. It never appears on the wire, in a reference between items,
  or in JSON output. **Every stored reference is a full IRI.** *(Invariant)*
* **Local handles** are sequential per store and per kind, stored in the local store as
  a table from handle to IRI. *(Owner, 2026-09-24: per store, not per project, so that the
  commands that name an item without naming its project keep working. `gate 3` and
  `finding 3` can both exist; every command already names the kind.)* Nothing depends on a handle. If the table is lost, every IRI
  still works and only the short names are gone. A handle is never reused.
* **GitHub handles** are `#41` for the configured repository and `owner/repo#41` for any
  other.
* **Input.** The CLI accepts a handle or a full IRI. The store bound to the role resolves a
  handle. In local mode `fl finding 41` is local finding 41. In GitHub mode it is issue #41.
* **Ambiguity is refused.** When sub-project 4 allows two trackers, a handle that resolves
  in both is refused with every candidate listed. The CLI never picks one. *(Invariant)*
* **Output.** Human output shows the handle. `--json` and all output that crosses a
  process boundary show the IRI, with the handle only as a labeled display field.

---

## 5. Error handling

The principle: every failure path distinguishes "nothing" from "didn't look".
*(Invariant)*

| situation | outcome |
|---|---|
| The input is not a valid IRI or handle | refused at parse, naming the input |
| No configured store owns the IRI | **ERROR**: `no store owns <iri>`, listing every store searched, never "not found" |
| The owning store holds a tombstone for the IRI | not found, reported as deleted |
| The store is unreachable, or its I/O fails | **ERROR** |
| The handle does not exist | not found |
| The handle table cannot be read | **ERROR**, never "not found" |
| A reference points to an item that no longer exists | reported as **dangling**, never dropped from a list |
| A minted IRI already exists | the insert is refused; an insert never overwrites |
| An alias resolves to more than one item | refused, listing every match |

### 5.1 Format version

The local store writes `format_version` to its meta table when it is created.

* A store with tables and **no** version key predates this change. It is refused. It is
  never treated as a new store, because a missing key that reads as "fresh" would make an
  old store look empty — a vacuous pass.
* The refusal names the version found, the version expected, and the remedy. There is no
  migration: start a new store, or keep the version that wrote the old one. The existing
  `Decode` message already names this remedy, and it stays.

### 5.2 `StoreError`

The `NoSuch*` variants carry an `Iri`. New variants: `NotOwned { id, searched }`, `Unreachable`,
`Dangling { from, to }`, `FormatVersion { found, expected }`.

---

## 6. Testing

### 6.1 One conformance suite per role

The suites for `Catalog`, `Tracker` and `Ledger` are generic over the store and run
against `MemStore` and `RedbStore`. In sub-projects 2 and 3, a GitHub store must pass
**the same suites**. It gets no suite of its own and no privileged path. The suite is the
contract.

### 6.2 Identity

* Normalization is idempotent and folds mixed-case input to one form.
* A parse failure, `NotOwned`, and not-found are three distinct errors.
* The original defect as a regression test: two independent stores each mint a gate, and
  the two ids differ.

### 6.3 Evidence before state

A ledger that fails on append leaves the record's state unchanged. The test is verified by
mutation: reverse the two writes and the test must go red.

### 6.4 Old stores

A fixture in the old format — `u64` tables, no version key — is refused with
`FormatVersion`. A new empty store is accepted. The two cases are separate tests, so that
"refuses everything" cannot pass as "refuses the old store".

### 6.4a Several stores

Two stores each hold one project. An id from store 1 resolves only in store 1. An id that
neither holds is `NotOwned`, and the message names both stores. A deleted id resolves to
its tombstone, not to `NotOwned`. Config precedence is tested tier by tier.

### 6.5 Handles

* A handle is never reused after a reopen.
* A scan of all `--json` output checks that every reference field holds an IRI. The scan
  enumerates fields from the types, not from a hand-written list, and carries a floor on
  the field count, so a scan that silently shrinks fails.

### 6.6 Existing tests

`crates/cli/tests/getting_started.rs` runs all 65 commands of `docs/getting-started.md`,
and the guide shows ids. The guide is updated in the same change, and `VERIFIED_COMMANDS`
stays accurate. All 172 existing tests carry over. No test is deleted without a stated
reason in the pull request.

---

## 6a. Reserved: the agent registry (FerroWire)

FerroWire is the communication switchboard between agents, users, and system-generated
messages. The owner, 2026-09-25: the store also holds an **agent registry** for it. It is
reserved here and built with FerroWire, which the owner placed in the **second phase** — near,
not a distant roadmap item — immediately after the GitHub work of §0.2.

* **A fourth role.** The registry is a role a store backs, beside Catalog, Tracker and Ledger.
  *(Invariant)*
* **An agent's id is an IRI**, minted like any other local id. *(Invariant)*
* **An agent's address works like a handle.** It is the short name a person or agent types
  (`Cyndi`), and it resolves to the agent's IRI. Addresses are **case-insensitive**: `Cyndi` and
  `cyndi` are one agent. An address names an agent, not a session. *(Invariant — owner,
  2026-09-02)* The IRI rules in §2.3 do not change for this.
* **Not decided here:** what an agent record holds (vendor, capabilities, status and so on).
  That is FerroWire's design. *(Open)*
* **Cost of building it later:** a new kind and new tables bump the store format version, so
  stores written before then are refused (§5.1). *(Release scope)*

## 7. Open questions

* **Escalation between the two tiers.** *(Open)* The owner: it "deserves further
  discussion", saved for the two-tier design in sub-project 4. Do not design it early.
* **Where the catalog lives in GitHub mode.** *(Open)* §3.6 states the constraint: an
  issue's reproduction must name a gate its reader can resolve. The owner wants to discuss
  it further. The design belongs to sub-project 2.
* **Routing new items between trackers.** *(Open)* The owner wants to brainstorm it.
  Sub-project 4.

Settled on review, 2026-09-24: several local stores — see §2.6.
