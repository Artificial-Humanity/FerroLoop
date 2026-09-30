# GitHub tracker store — design

**Date:** 2026-09-26
**Status:** Design, awaiting the owner's review. Sub-project 2 of 4. Not started.
**Scope:** A `Tracker` backed by the Issues of one GitHub repository (mode A), the credentials
it writes with, the disclosure rule for security findings, and the committed manifest that
lets another machine resolve the gates an issue names.

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

The identity spec (`2026-09-23-identity-and-store-roles-design.md`) split persistence into
three roles — Catalog, Tracker, Ledger — and named four sub-projects. This is the second:

| role | mode A (this sub-project) |
|---|---|
| Catalog | the local store — on a machine that does not author the project, a store the committed manifest was imported into |
| Tracker | **GitHub Issues** |
| Ledger | the local store |

In GitHub mode the issues are the **system of record**, not a mirror of a local tracker.
GitHub mode is a permanently supported configuration. The local tracker stays the default.

### 0.1 Owner decisions this design rests on (2026-09-26)

1. **Catalog location:** the local store stays the authority for gates. A committed manifest
   with a provenance stamp carries them to every other reader (§4).
2. **Disclosure:** a security finding on a public repository is refused. Visibility is read
   live, and an unknown visibility is an ERROR (§6).
3. **Edits in the GitHub web interface:** fl owns its state inside the issue. A web edit that
   disagrees with it is reported as diverged, never adopted (§3.4).
4. **Credentials:** the App's installation token is the primary source and gives agents a bot
   identity; a token from the environment is the second source (§5).
5. **Scope:** tracker store, credentials, disclosure refusal and manifest. `fl` in CI on pull
   requests, the source and sink adapters, and the router stay on the roadmap in the
   review-layer spec.
6. **Encoding:** a state label plus a metadata block in the issue body (§3), chosen over a
   bot-owned comment (N+1 reads per list) and over GitHub's native issue fields (not
   available to every repository).
7. **Other machines import the manifest** into their local store (§4.2), chosen over a
   read-only manifest catalog. Found while planning: a gate run writes a pass mark into the
   catalog and a run into the ledger, and the ledger refuses a gate its store does not hold,
   so a read-only catalog could not run a gate at all. The alternatives were to allow gate
   runs only on the authoring machine, or to let the ledger hold rows for gates it does not
   own; the second weakens the ledger's ownership check.

### 0.1a Owner decisions, 2026-09-27

8. **No downgrade import.** An older checkout whose manifest lacks a gate the store imported is
   refused; the refusal says to check out a commit whose manifest lists it.
9. **Anyone may run `fl github repair`.** It requires `--by <name>` and records it in a comment on
   the issue. It rewrites only from the block, so it cannot move an item past the protocol.
10. **The live test** runs against a private throwaway repository in the organization. The owner
    registers the App and installs it there when the live test is ready; until then the live test
    uses an environment token.
11. **No dogfooding in this sub-project.** FerroLoop's own repository keeps its local tracker until
    plan B is merged and the live test passes; then the owner decides.
12. Plan A's two agent rulings are **kept**: the first import raises the store to format 3, and
    transitions mirror the manifest on re-import.

### 0.1b Owner decisions, 2026-09-28

13. **`webpki-roots` is accepted.** Its CA data is licensed CDLA-Permissive-2.0; that is compatible
    enough for this project (§1.1).
14. **`repair` restores an issue whose block is intact but whose fl labels are gone.** The block is
    fl's record, so it is enough to make the issue fl's again; §3.5 does not apply to `repair`.

### 0.2 Out of scope

* The GitHub ledger (sub-project 3) and its rate-limit design.
* More than one tracker per project, routing between trackers, and escalation between the
  tiers (sub-project 4). Escalation is not designed early, by the owner's instruction.
* `fl` in CI, observations, adapters, the router (review-layer spec, roadmap).
* Webhooks. The local side polls; nothing is hosted.

---

## 1. Components

### 1.1 `fl-github` (new crate)

Depends on `fl-core` only.

* **`GithubTracker`** implements `Tracker` and `Handles` for one repository.
* **`Client`** is a blocking HTTP client over the REST API, plus the one GraphQL query that
  looks an item up by `node_id` (§2.3). The traits and the CLI are synchronous, so the crate
  has no async code.
* **`Credentials`** is an interface with two implementations, `AppCredentials` and
  `EnvToken` (§5).

**Dependencies.** A blocking HTTP client with TLS (`ureq` with `rustls`), a JWT signer for
RS256 (§5.2), and a SHA-256 implementation for the manifest (§4). The plan pins each and
records its licence. Each must be compatible with Apache-2.0; `webpki-roots` (CDLA-Permissive-2.0)
is accepted (§0.1b, 13). No dependency may need a
runtime service or a program outside the binary. *(Invariant — the single-binary rule.)* A
subprocess to the `gh` CLI was rejected on that rule.

### 1.2 Manifest import (in `fl-store`)

`RedbStore` gains an import of the committed manifest (§4.2). It writes the manifest's project,
gates and transitions under **their existing IRIs** and marks the project as imported, with the
manifest's hash. The store then owns those IRIs, so the ledger and the pass marks work without
change. There is no separate manifest catalog type.

### 1.3 Role binding

The existing `Roles` value binds each role to a store. In GitHub mode:

* **Catalog** — the local store. On the authoring machine it holds the project as authored;
  on any other machine it holds the project as imported. §4.3 and §4.5 say how each is kept
  consistent with the manifest.
* **Tracker** — `GithubTracker`.
* **Ledger** — the local store.

The engine's flows do not change shape: `move_record`, `attach_reproduction` and
`verify_finding` already take `Roles`. The one engine change is §4.2's: a failed pass-mark
write stops being discarded. The ledger write still precedes the tracker write, so the
evidence-before-state rule of identity spec §3.5 holds across the two stores.
*(Invariant, restated.)*

**Checks that cross stores move to the binding.** A tracker that does not back the catalog
cannot know whether a project exists. With a split binding, the check that a project is held
— before `add_record`, `list_records`, `list_findings` and `add_finding` — is made against the
catalog by the binding, as identity spec §3.4 requires for every reference into another store.
A tracker that also backs the catalog keeps making the check itself. Either way, a list over a
project nobody holds is refused, never empty. *(Invariant.)*

### 1.4 Configuration

The user-level `config.toml` gains a tracker binding per project root:

```toml
[[project]]
root = "/path/to/checkout"
store = "/path/to/fl.redb"
tracker = { github = "owner/repo", credential = "app" }
```

With no `tracker` key the tracker is local, as today. The App settings (§5.2) live in the same
file. Nothing in it is committed to a repository, because it holds machine-specific paths.

The config is parsed with unknown fields denied, so an older `fl` refuses a file with a
`tracker` key. It never ignores the key and writes to a local tracker nobody chose.
*(Invariant — keep `deny_unknown_fields`.)*

### 1.5 Correction to the identity spec

Identity spec §2.2 said that a GitHub project's id is its repository URL. With the catalog
local (decision 1), a project stays a catalog item with a `urn:uuid:` id; the repository is a
**tracker binding**, not a project. That line is corrected in the same change as this spec.

---

## 2. Identity

### 2.1 Ids and handles

* An item's id is the canonical issue URL, `https://github.com/<owner>/<repo>/issues/<n>`.
* GitHub's `node_id` for the issue is the match key.
* The handle is `#41` for the bound repository. `owner/repo#41` for any other repository is
  parsed and answered `NotOwned`, since one tracker binds one repository.
* A handle is display only, as everywhere (identity spec §4). *(Invariant.)*

Issue numbers are shared by records and findings. `handle_of(kind, id)` answers `None` when
the issue is not of `kind`, and `resolve_handle(kind, n)` answers `None` when issue `n` is an
fl item of another kind. The local rule that handles start at one for each kind does not apply
here.

### 2.2 Ownership

`GithubTracker` owns an issue URL whose repository is the bound one, **as identified by
`node_id`, not by name** (§2.4). It owns no `urn:uuid:`. Asked for an id it does not own, it
answers `NotOwned` and names the repository it searched.

### 2.3 References carry the `node_id`

Every reference that an issue's metadata block stores — a finding's record — carries the
`node_id` as well as the URL. fl resolves such a reference by `node_id` (one GraphQL `node`
lookup), never by URL. *(Invariant.)* A rename, a transfer, or a reuse of the old name cannot
then misdirect it.

*As implemented (plan B, Task 5):* a URL under the bound repository's **current** name is
trusted without a lookup, because the repository itself is bound by `node_id` at open (§2.4);
every other URL is resolved by `node_id`. This avoids one lookup per reference in the common case
and never trusts an old name.

### 2.4 Renamed and transferred repositories

After a rename, or a transfer to another owner, GitHub redirects the old URLs — **until someone
creates a new repository at the old name**, after which the old URL reaches a different
repository and `#41` there is a different issue. A URL alone is not a safe reference.

* **The binding records the repository's `node_id`.** At first use fl reads the repository and
  writes its `node_id` to the local store. At each open fl reads the repository again:
  * same `node_id`, different name → a rename or transfer. fl continues and prints a notice
    that the configured name is old. fl does not edit the user's config file.
  * different `node_id` → the name was reused. **ERROR**; every read and write is refused until
    the binding is corrected. *(Invariant.)*
* **A URL with no `node_id`** — typed by a person, or held in the local ledger — whose
  repository name is not the current one costs one call to find which repository that name
  reaches now. fl owns it only if that repository's `node_id` is the binding's. A failed call
  is an ERROR, never "not owned". *(Invariant.)*
* **Known gap.** After the old name is reused, a typed old URL answers `NotOwned`, and the
  message says the name now reaches a different repository. *(Release scope.)*
* **The id moves; the old URL becomes an alias**, as identity spec §2.2 already provides. fl
  rewrites the URLs in an issue's metadata block at its next write to that issue. The local
  ledger is append-only and keeps the old URLs; the rule above resolves them.

### 2.5 Aliases

An issue's aliases live in its metadata block. A lookup by an alias that is not a URL of the
bound repository is a **full scan** of the fl issues — every page, compared against every
block. It is correct and it is expensive: one full list per lookup. The search API is not used,
because its index lags and it does not promise a complete result. *(Release scope — revisit
with the rate-limit design of sub-project 3.)*

---

## 3. How an item lives in an issue

### 3.1 Mapping

| fl item | issue |
|---|---|
| Record | label `fl:record` and one state label, for example `fl:record/doing`. Title = the record title. |
| Finding | label `fl:finding` and one state label, for example `fl:finding/reproduced`. Body = the claim. Title = the claim's first line. |

The metadata block is an HTML comment at the end of the body. GitHub does not render it:

```text
<!-- fl:meta
{"fl_format":1,"kind":"finding","state":"reproduced","project":"urn:uuid:…",
 "record":{"id":"https://github.com/o/r/issues/12","node_id":"I_kw…"},
 "reproduction":"urn:uuid:…","raised_by":"…","assigned_to":null,
 "security":false,"withdrawn_reason":null,"also_known_as":[],"create_key":"urn:uuid:…"}
-->
```

Field names and enum values are `snake_case` (decision 33). `raised_by` is the fl actor; the
GitHub author is the bot or token owner. These are two facts and fl keeps both.

### 3.2 Open or closed is a projection

fl writes it; fl never reads state from it.

| state | issue |
|---|---|
| record `done` | closed, reason `completed` |
| finding `fixed` | closed, reason `completed` |
| finding `withdrawn` | closed, reason `not_planned` |
| every other state | open |

### 3.3 Writes

* **One change is one `PATCH`.** It sets the labels, the body and the open/closed status
  together. fl replaces only its own `fl:*` labels and keeps every other label.
* **The response is the postcondition.** GitHub silently drops labels that a caller may not
  set. After every create and update, fl checks that the returned labels, block and status are
  the ones it sent. A mismatch is an ERROR, never success. *(Invariant.)*
* **Labels are created explicitly.** fl creates its missing `fl:*` labels through the labels
  API at first use, and treats a failure there as an ERROR. It never relies on a label being
  created as a side effect.
* **No compare-and-swap.** GitHub offers no conditional update on an issue. After each write fl
  reads the issue's timeline (label and state events) and its body edit history (GraphQL
  `userContentEdits`) for the window between its read and its write. A change by another actor
  in that window is a **conflict ERROR**, never success. This is detection, not prevention: the
  other write has already landed, and the next read reports any inconsistency as diverged.
  *(Release scope — measured in the live test, §8.3, before any claim about it is made.)*
* **The timeline and the edit history lag a write.** Measured live on 2026-09-29: a create's
  `labeled` events appeared 1.5–3.5 s after GitHub answered it; an update's events showed on
  the first read after it (about 0.5 s) and its edit-history entries about 0.5 s later. So after a create fl reads the timeline until the
  create's `labeled` events show, and after an update or a repair it reads the window until
  its own events and edits show — each for at most 10 s. A create whose events never show
  still succeeds, and the next write refuses as a conflict. An update whose write never shows
  is an ERROR that says to read the item again. Without the wait, fl's own late events land
  in the next write's window and read as someone else's. *(Release scope. Modelled, not
  measured: once fl's own events show, every event written before them shows too.)*
* **A create cannot duplicate.** A create carries a `create_key` that fl mints. After a timeout
  or a connection failure, fl lists the fl issues created since the attempt — the list
  endpoint, not search — and looks for that key before it sends the create again.
  *(Invariant.)*

### 3.4 Reads and divergence

A read compares the state label, the metadata block, and the open/closed status. Each of these
is **diverged**, and the read reports every value it found:

* the label and the block disagree;
* the open/closed status disagrees with §3.2;
* two state labels, or none;
* a block that is missing, damaged, or of an unknown `fl_format`.

fl never adopts a web change silently. *(Invariant.)* A person resolves a divergence with
`fl github repair <id>`, which rewrites the label and the open/closed status **from the block**
and adds a comment naming who ran it. The block is fl's record of the protocol, so a repair can
never move a finding to a state the protocol did not reach. If the block itself is missing or
damaged, `repair` refuses; the person restores the block from the issue's edit history first.

### 3.5 What is not an fl item

An issue with no `fl:` label, or a pull request (the issues API returns both), is refused as
**not an fl item**. It is not "not found", and fl does not adopt it. The one exception is `fl github repair`:
an issue that still carries an intact fl block is restored from it, labels included (§0.1b, 14).

### 3.6 Deleted and moved issues

| GitHub answer | fl result |
|---|---|
| 410 Gone | a tombstone: not found, reported as deleted |
| 301 to another repository (the issue was transferred) | **ERROR** naming the new location |
| 404 for an issue of the bound repository, access verified at open | not found |
| a URL resolves to a different `node_id` than the stored reference | dangling |

### 3.7 Lists

A list follows every page. A failure on any page is an ERROR, never a short list.
*(Invariant.)* `list_records` and `list_findings` filter by label and then by the block's
project. `withdrawals_by` lists withdrawn findings and counts those whose block names the
actor.

---

## 4. The manifest

### 4.1 Export

`fl manifest export` writes `.fl/manifest.json` in the project root:

* `format_version`;
* the project IRI, every gate definition, and every transition of the project. A gate's
  `last_pass_commit` is **not** exported: it is a pass mark earned on one machine (§4.2);
* a provenance stamp: the commit at export and the export time. It holds **no store path** —
  a store path is specific to a machine, and the repository can be public;
* `content_sha256` over a canonical form of the content.

All gates are exported, not only those an issue names: the whole set is simpler, and any gate
can become a reproduction. The export prints each gate it writes, because a gate command can
name local paths and the file is about to enter the repository.

### 4.2 Import

On a machine that does not author the project, `fl manifest import` reads the file and writes
its project, gates and transitions into the local store, in one transaction, under their
existing IRIs. The store records the project as **imported**, with the manifest's
`content_sha256`.

* **A hash mismatch** means a hand edit, and the import refuses the file. The store where the
  project is authored is the only place a gate is authored. *(Invariant.)*
* **An authoring store refuses an import** of its own project. It already holds the source.
* **A re-import** replaces the definitions with the manifest's. A gate whose definition did not
  change keeps its local pass mark; a gate whose definition changed loses it, because the mark
  was earned by another definition. A re-import that would remove a gate the store holds is
  refused, and names the gate: removing a gate removes a neighbour from every future verify.
  *(Release scope.)*
* **An imported definition cannot be edited locally.** The store refuses any gate update that
  changes more than `last_pass_commit`, refuses `add_gate` and `add_transition` on an imported
  project, and names the remedy: change it where it is authored, export, commit, import.
  *(Invariant.)*
* **Pass marks are local to each machine.** The ledger is local in mode A, so a neighbour
  gate that passed on one machine and never ran on another is not a neighbour there.
* **A failed pass-mark write is an ERROR.** The engine discards that error today
  (`let _ = catalog.update_gate(..)` in `evaluate.rs`); a lost mark silently removes a
  neighbour from later verifies, so the discard is removed. *(Invariant.)*

### 4.3 Stale detection

Before fl writes a gate IRI into an issue — when it attaches a reproduction — it compares that
gate in the store with the gate in the manifest, ignoring `last_pass_commit`. If they differ, or the gate is absent from the
manifest, fl refuses: *run `fl manifest export`, then commit*. *(Invariant.)*

### 4.4 The file must be committed

Before the same write, fl checks that the manifest is tracked by git and has no local changes,
and refuses otherwise. fl cannot check that the commit was pushed, and says so in the refusal's
documentation. *(Release scope.)*

### 4.5 The imported copy must match the manifest

On an importing machine, before fl runs a gate or writes a gate IRI into an issue, it compares
the manifest's `content_sha256` with the hash recorded at import. If they differ, fl refuses:
*run `fl manifest import`*. On the authoring machine the stale check of §4.3 applies instead.
Either way, the store and the manifest must agree, and fl never picks one of the two silently.
*(Invariant.)*

### 4.6 `CODEOWNERS`

The spec recommends a `CODEOWNERS` entry for `.fl/manifest.json`, as review-layer spec §2.1
does, so that widening a gate is a reviewed act. fl does not enforce it.

---

## 5. Credentials

### 5.1 One named source per binding

The binding names its source: `credential = "app"` or `credential = "env"`. fl never falls back
from one to the other. A missing or unusable source is a refusal that names the setting.
*(Invariant.)*

### 5.2 `app`

The config holds the App id and the path to its private key. fl signs a short-lived JWT
(RS256), finds the App's installation for the bound repository, and exchanges the JWT for an
installation token. It renews that token before it expires. The key path is read from config;
**no secret ever appears in argv**. *(Invariant.)*

The App needs two repository permissions: **Issues: read and write**, and **Metadata: read**.
Registering and installing it is the owner's act. The App's name is configuration, not code.

### 5.3 `env`

fl reads `FL_GITHUB_TOKEN`, then `GITHUB_TOKEN`, from the environment only. In GitHub Actions
the second writes as the Actions bot. A personal token writes as that person, not as a bot.

### 5.4 Identity is visible

`fl github whoami` prints the identity fl writes as, and the source it came from.

---

## 6. Disclosure

* `fl finding raise --security` marks the finding (`"security": true` in the block).
* Before it creates the issue, fl reads the repository's visibility. **Only `private` passes.**
  `public` and `internal` are refused, and the refusal names the remedy: a local tracker, or a
  private repository. A failed read is an ERROR — an unknown visibility is not a pass.
  *(Invariant.)*
* **Known limits, stated rather than solved.** If a private repository becomes public, its
  security findings become public, and fl cannot prevent that. fl cannot detect a security
  finding that nobody marked.

---

## 7. Error handling

The identity spec's principle carries over unchanged: every failure path distinguishes
"nothing" from "didn't look". *(Invariant.)*

| situation | outcome |
|---|---|
| the bound repository's `node_id` changed (name reused) | **ERROR**, every operation refused |
| the bound repository was renamed or transferred | continue, with a notice |
| the credential source is missing or refused | refusal naming the setting; no fallback |
| a rate-limit response | **ERROR** naming the reset time; fl does not wait silently |
| a GitHub error fl cannot classify | **ERROR**, never "not found" |
| a page of a list fails | **ERROR**, never a short list |
| a write's response differs from the request | **ERROR** |
| another actor wrote in the write window | conflict **ERROR** |
| the label, block and status disagree | diverged, naming every value |
| an issue with no `fl:` label, or a pull request | not an fl item |
| 410 | deleted (tombstone) |
| transferred issue | **ERROR** naming the new location |
| security finding, repository not `private` | refused |
| visibility read fails | **ERROR** |
| manifest hand-edited, stale, or uncommitted | refused, naming the remedy |
| an import into the store that authors the project | refused |
| a re-import that removes a gate | refused, naming the gate |
| a local edit of an imported gate or transition | refused, naming the remedy |
| a pass mark cannot be written | **ERROR** |

New `StoreError` variants: `Imported` (plan A); and `Deleted`, `Diverged`, `Conflict`,
`NotAnFlItem`, `Moved`, `RateLimited`, `RepositoryReplaced`, `Credential`, `SecurityNotPrivate`
(plan B). Their exact shape is the plans'.

---

## 8. Testing

### 8.1 The conformance suites are the contract

`GithubTracker` passes the same `tracker` cases as the local stores. It gets no GitHub-only
suite and no privileged path. It runs over an in-process fake GitHub server. *(Invariant.)*

The suites change, and none of the changes is specific to GitHub:

1. **The harness binds each role to its own store.** Today `tracker` requires one store that
   backs the catalog too, and `all_roles` requires one store that backs everything. Local
   stores keep binding one store to every role; the GitHub run binds `GithubTracker` with a
   `MemStore` catalog and ledger.
2. **Cross-store checks run through the binding** (§1.3), so the case *a list over a project
   this store never held is refused* exercises the binding when the roles are split.
3. **"Handles start at one for each kind"** is a property of the local store, not of the
   `Handles` contract, and moves to the local-store tests. *"An id has no handle under any kind
   but its own"* stays shared.
4. The declared case counts (`TRACKER_CASES` and the rest) change in the same commit, with the
   reason in the pull request.

### 8.2 The "didn't look" battery

Each row is a reproduction, mutation-tested by deleting its guard and watching it go red:

| scenario | required outcome |
|---|---|
| label and block disagree | diverged |
| another actor writes in the window | conflict ERROR |
| page 3 of a list fails | ERROR, not a short list |
| create times out, retry | exactly one issue |
| the response drops a label | ERROR |
| repository renamed | continue, notice |
| old name reused by another repository | ERROR |
| `--security` on a public repository | refused |
| visibility read fails | ERROR |
| a genuinely empty repository | an empty list, clean |

The last row makes the others meaningful: without it, the battery passes against a store that
errors unconditionally.

### 8.3 Live smoke test

A small test against a throwaway repository runs only when `FL_GITHUB_LIVE_REPO` is set. It
exists because a fake cannot fail the way GitHub fails. It also **measures** the conflict
detection of §3.3 under concurrent writers, over repeated rounds with a failure count: one
clean round is not evidence.

### 8.4 Manifest

A hand edit, a stale gate, a missing gate and an uncommitted file each give a refusal, and each
refusal test fails when its refusal is removed. So do: an import into the authoring store, a
re-import that removes a gate, a local edit of an imported gate, and a manifest that changed
since import. A re-import keeps the pass mark of an unchanged gate and drops the mark of a
changed one. A pass-mark write that fails makes the gate run an ERROR.

### 8.5 Existing tests

All existing tests carry over. No test is deleted without a stated reason in the pull request.
`docs/getting-started.md` stays local-mode and executed; GitHub mode gets its own section only
when the live test backs it.

---

## 9. Documents updated in the same change

* **Identity spec §2.2** — a GitHub project's id is not its repository URL (§1.5); and §2.6's
  "a URL names its home" is qualified by §2.4 here.
* **Review-layer spec** — the App is no longer deferred for the tracker; §2.1's manifest is
  built by this sub-project; one-way reconciliation still holds for observations, which remain
  roadmap.

---

## 10. Open questions

* **Adopting an existing issue.** fl refuses an issue with no `fl:` label (§3.5). Whether a
  person may later bring an existing issue under fl is *(Open)*.
* **Trust in the block.** A person with write access can edit the block and the label so that
  they agree, and the read then finds no divergence. Detecting a block edited by an identity
  other than fl's needs the edit history on every read. *(Open — Release scope: not detected.)*
* **Alias scans at scale** (§2.5). *(Open)* — sub-project 3.
* **Accepting a deliberately recreated repository.** §2.4 refuses a bound name that now reaches a
  different repository, and no command accepts one that was recreated on purpose. *(Open.)*
* **Escalation between the tiers**, **routing between trackers** — sub-project 4, unchanged.
