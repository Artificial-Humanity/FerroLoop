# Two-tier routing, plan B — escalation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move a local record or finding to GitHub on command, or when a local record moves to `needs_human`: check everything the create would refuse first, mark the local item "escalating", find or create its GitHub issue by a search of its own, then replace the local item with a tombstone that every lookup, handle, list and piece of evidence follows — never two live copies, and a rerun always resumes.

**Architecture:** `fl-core` gains `escalation.rs` (the mark, the tombstone, the `Escalations` trait every local store implements, the item that goes out, `EscalationFault`) and `escalate.rs` (the router's `prepare_escalation` / `escalate` / `abandon_escalation`). `StoreError` gains `Escalating`, `Escalated` and `Escalation`. `MemStore` and `RedbStore` keep marks and tombstones in tables of their own and refuse writes to a marked or tombstoned item; the router follows a tombstone in `route` and resolves a finding's record through one. `GithubTier` gains three methods (a create-key search, an alias check, an escalation create) that `MemIssues`, the GitHub tracker and the CLI's lazy tier implement; the GitHub tracker writes an escalated issue with the item's own state and aliases, its old IRI as the create key, and a provenance line rendered from a new block field (`fl_format` 3). The CLI adds `fl record escalate` / `fl finding escalate` (with `--abandon`), runs the escalation after a landed move to `needs_human`, and shows `escalating` in the tier column.

**Tech Stack:** Rust 2024 (`rust-version = "1.98"`), redb 4.3, serde/serde_json, thiserror 2, clap 4, ureq 3, assert_cmd/predicates for black-box tests. No new crates.

**Spec:** `docs/superpowers/specs/2026-10-06-two-tier-routing-design.md` rev 2.4 (rev 2.3 at `1f1f61f`, amended with this plan by the `[agent]` rulings under "Spec defects" below and by plan ruling 23) — §2.2, §2.3, §2.4, §2.5 ("Evidence"), all of §3, §4's escalation rows, §5's escalation bullets and its one live test, and §8's "Plan B". The GitHub tracker spec (`2026-09-26-github-tracker-design.md`) §2.3, §3.1–§3.4 and §4.3, and plan A (`docs/superpowers/plans/2026-10-06-two-tier-routing-a.md`, "Plan B — what this plan leaves"), are the ground this plan builds on.

**Branch:** `ferris/routing-b`, off `main` once this plan is merged.

---

## Global Constraints

- Verification trio, all green before every commit: `cargo fmt --all --check`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`. Each task runs `cargo fmt --all` first, so code blocks here need not be in rustfmt's exact layout; lines stay within 100 columns.
- Unit tests live in `#[cfg(test)] mod tests` inside the module they test; black-box CLI tests live in `crates/cli/tests/`; an `fl-exec` test that drives a real store lives in `crates/exec/tests/`.
- **No test contacts the network**, except the one live test in `crates/github/tests/live.rs`, which is `#[ignore]`d and runs only by hand against the private throwaway repository. Everywhere else GitHub is the in-process fake (`fl_github::fake::FakeGithub` on `127.0.0.1`, reached by the binary through `FL_GITHUB_API_URL`), or, inside `fl-core`, `mem_issues::MemIssues`.
- `fl-core` stays pure: "No IO, no async, no clock, no network" (`crates/core/src/lib.rs:1`). The escalation takes the time as an argument (`now_ms: u64`, unix milliseconds); the CLI reads the clock.
- Spec values, verbatim: "Mark the local item "escalating", with who, why and the time. From then on the local store refuses writes to the item, and the refusal names `fl record escalate <id>` (or `fl finding escalate <id>`) to finish. A finding raised against a marked record is not a write to it and is allowed." (§3.3 step 1); "Before any create, fl searches every issue, labelled or not, newest first, back to the mark's time less the create-search margin, for that key." (§3.3 step 2); "Replace the local item with a tombstone — the old IRI, the new IRI, who, when, why." (§3.3 step 3); "`--abandon` … removes the mark, and only after the step-2 search proves no issue exists. … Once the issue exists, the only way on is step 3." (§3.3); "a closed state — a record `done`, a finding `fixed` or `withdrawn`" (§3.2); "An escalated record's GitHub issue lists the record's open findings — claim, state and IRI — leaving out security findings." (decision 18); "the command exits with the move's own code and prints a `warning:` naming `fl record escalate <id>`" (decision 16).
- Spec invariants, verbatim: "The item is never live in both tiers" — "one live copy" (§3.3); "Each step can be run again, and running the command again resumes from where it stopped." (§3.3); "Everything that would make the GitHub create refuse is checked before step 1, so an item is never marked and then stranded." (§3.2); "routing never changes tier silently" (§1.3); "If that tier cannot be reached, the result is an error, never 'no such record'" (§2.5); "An escalation is a move, not a transition: it writes no ledger entry of its own." (§3.5).
- **Every existing configuration keeps working:** a store that holds no routing map behaves exactly as before, and a routed store with no mark and no tombstone behaves exactly as plan A left it. Every existing test passes, except the assertions a task changes on purpose (named in that task).
- `snake_case` on every wire (`crates/core/src/wire.rs`).
- **Every guard gets a mutation check**: revert it, watch the named test go red, restore, confirm with `cmp` against a saved copy. Each task's mutation step lists every guard the task adds — each conjunct of a compound condition, each match arm that must honour an input, and ordering where order matters. Where a line looks like a guard but no input can tell it apart, the step says so and why.
- **A unit-test filter in a mutation step names the full module path** (`cargo test -p fl-core --lib escalate::tests::`), or it matches nothing. A black-box filter names the test file (`cargo test -p fl-cli --test escalation -- <name>`). At most one filter goes before `--`.
- **A test never asserts with a substring another code path also produces.** Each task names the unique phrase it asserts.
- **No plan names, task numbers or review labels in code comments** ("Task 7", "plan B", "ruling 9", "the review's scenario"). Cite the spec by section ("routing spec §3.3"), or say the reason. No decision numbers in user-facing help or error text.
- **A change to shared plumbing states its blast radius** in the task that makes it.
- Line numbers cite `1f1f61f`; an earlier task's edits shift them. Find the named item, not the number.
- Commits are authored by the machine account (`WORKFLOW.md`) and end with `Co-authored-by: Ferris <Ferris@artificialhumanity.io>`. Stage explicit paths — never `git add -A` — and after each commit run `git status --porcelain`, which must print nothing.
- The repository is public: no machine paths, host names, user names or lab names in code, tests, messages or docs. Fake repositories are `acme/widgets`.

## Review Focus

1. **A rerun after a stop between the create and its label call — and after more than the create-search margin.** The rerun finds the unlabelled issue by its create key, labels it, writes the tombstone, and never makes a second issue. Task 4 (`an_escalation_stopped_before_its_labels_is_found_and_finished_after_the_margin`), Task 7 (`a_rerun_after_each_step_finishes_with_one_issue_and_one_live_copy`).
2. **A write to a marked item, by any path** — `record move`, `finding assign | withdraw | reproduce | verify`, an alias — is refused naming the escalate command, before any gate runs, until its issue exists and GitHub reads it as an fl item (from then on the old id is the issue, plan ruling 23); a finding raised about a marked record is allowed. Task 1 (`a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed`), Task 2 (same, over redb), Task 6 (`a_marked_item_is_its_issue_once_the_issue_exists`), Task 9 (`a_move_of_a_marked_record_is_refused_before_its_gates_run`, `a_reproduction_or_verification_of_a_marked_finding_is_refused_before_its_gate_runs`).
3. **The old local handle or IRI after the escalation** — a lookup, a move, `finding list --record`, a finding's evidence — reaches the issue, never "no such record" and never the dead local row. Task 6 (`a_tombstoned_id_is_followed_to_its_issue`), Task 10 (`an_escalated_records_old_handle_moves_its_issue`, `crates/exec/tests/escalated_evidence.rs`).
4. **An escalation to a public repository.** A sensitive area, a security finding, or a finding about a local record in a sensitive or undeclared area is refused before the mark; anything else is warned about before it is published, naming what will be published. Task 7 (`nothing_sensitive_is_escalated_to_a_public_repository`), Task 8 (`an_escalation_to_a_public_repository_warns_what_it_publishes`).
5. **The `needs_human` trigger when the escalation fails.** The move's ledger evidence is written, the record stays local in `needs_human`, marked if step 1 ran, the command exits with the move's own code and prints the warning; a refused move escalates nothing. Task 9 (`a_landed_move_whose_escalation_fails_warns_and_keeps_the_moves_code`, `a_refused_move_to_needs_human_escalates_nothing`).

## Rulings this plan makes

The spec leaves these open or ambiguous. Each says why, and what it costs if wrong.

1. **Plan ruling: the create key of an escalated issue is the item's old local IRI.** It is unique, known before step 1, survives every rerun, and is already an alias of the issue. *If wrong:* none; the key is opaque to every other reader.
2. **Plan ruling: the escalation's provenance — who, why, the old IRI — is a new block field, `escalated: { from, by, reason }`, and the issue shows a line rendered from it**, `Escalated from the local tier by <by>: <reason>. Its local IRI was <from>.`, escaped, stripped on read like plan A's record line. A finding's text is its claim, so a paragraph in the prose would become part of the claim; a block-derived line survives every rewrite of either kind. *If wrong:* wording.
3. **Plan ruling: a block that carries `escalated` is written with `fl_format` 3** (decision 14's reasoning: `Meta` refuses unknown fields, so without the raise an fl built from plan A would read the issue as damaged rather than say "upgrade fl"). `fl_format` stays computed, never set by hand. *If wrong:* none.
4. **Plan ruling: an escalated record's open findings are listed in the record's issue text** (a record's prose is otherwise empty and every update keeps it): `Open findings when this record was escalated:` then one line per finding, `- <state>: <claim> — <IRI>`, escaped, both tiers, only findings that are neither security findings nor in a sensitive or undeclared area (decisions 18, 21, 22). The list is the state at the escalation; it is not kept current. *If wrong:* a reader sees a list as of the escalation.
5. **Plan ruling: the mark and the tombstone live in their own tables** (`ESCALATING`, `TOMBSTONES`), keyed by the item's primary IRI, behind a new trait `Escalations` that `MemStore` and `RedbStore` implement. The item's row, its id, its handle and its aliases stay: the id must still choose this store and the handle must still resolve (spec §2.3, "A local handle of an escalated item resolves through its tombstone"). *If wrong:* none.
6. **Plan ruling: the tables are additive under store format 5**; the first mark raises a store below 5 to 5. No release has shipped since plan A (no tag; the version is `0.1.0`), so no fl that opens format 5 and ignores marks exists outside a developer's checkout (spec §1.4). *If wrong:* an fl built from plan A's merge opens a store with marks and ignores them.
7. **Plan ruling: a local store answers a tombstoned id with `StoreError::Escalated { from, to }` on every read and write, and lists leave it out**; a marked item reads as itself, a write to it is `StoreError::Escalating { id, to_finish }`, and a finding added about a marked record is allowed while one about a tombstoned record is `Escalated` (the router follows it and writes the finding through a `ForeignRecord`). *If wrong:* none — the router follows both.
8. **Plan ruling: the router follows a tombstone in `route` only, once.** `route`'s action takes the id it should act on, so the hop asks GitHub for the tombstone's target directly rather than resolving the old IRI by the alias scan. A tombstone that points to an issue GitHub no longer holds is GitHub's answer for that issue, not "held elsewhere". *If wrong:* none.
9. **Plan ruling: a finding's record is shown as the record now is.** `TieredTracker::get_finding` and `findings` replace a local finding's record IRI by its tombstone's target, so new evidence names the record where it lives (spec §2.5) and `finding list --record` matches the issue. Stores ignore the record on `update_finding` (plan A ruling 29), so the rewrite is never written back. *If wrong:* none.
10. **Plan ruling: the escalation is split like placement — `prepare_escalation` makes every check and writes nothing; `escalate` runs the three steps.** The CLI checks the committed manifest and warns between the two. *If wrong:* a name.
11. **Plan ruling: the pre-checks that need the working tree run in the CLI** — the routing-currency check (`ensure_routing_current`) and, for a finding with a reproduction gate, `ensure_publishable` (GitHub tracker spec §4.3). Everything else — closed states, the title, sensitivity and visibility, aliases, GitHub available — runs in the router. *If wrong:* none; the two CLI checks need `RedbStore` and git.
12. **Plan ruling: a rerun resumes with the mark's own who and why.** `--by` and `--reason` are still required, so a rerun reads like a first run; when they differ from the mark's, a `note:` says the mark's are kept. *If wrong:* a person retypes two values.
13. **Plan ruling: a rerun on a marked item searches first.** If the issue exists, the pre-checks are skipped — the issue already passed them — and the rerun labels it if it has no fl labels, then writes the tombstone. If not, every pre-check runs again (the repository may have turned public). *If wrong:* none.
14. **Plan ruling: the alias check reads aliases by kind label, as `add_alias` does**; an issue with no fl labels is invisible to it. The one issue that can be unlabelled because of this escalation — its own, stopped between the create and the label call — is found by the step-2 search, which reads every issue. *If wrong:* an unlabelled issue made by hand with a copied block and the same alias is not seen; `fl github repair` labels it.
15. **Plan ruling: the `needs_human` trigger escalates with `--by fl` and the reason `the record was moved to needs_human`.** `fl record move` takes no actor. *If wrong:* the issue names `fl`, not the person who moved the record; `fl record escalate` names them.
16. **Plan ruling: "the ledger entry written" in §3.4 and §5 is the move's own evidence — its gate runs.** In a routed store the ledger is the local store, whose `flush` writes no decision (decision 12), so a gated move records its gate runs and an ungated move records nothing; the trigger adds nothing (§3.5). *If wrong:* none; the spec's tests assert the runs.
17. **Plan ruling: a write that runs a gate before it reaches the store is refused before the gate runs, for a marked item** — `fl record move` of a marked record, and `fl finding reproduce` and `fl finding verify` of a marked finding — by the CLI reading the mark, with the store's own `StoreError::Escalating` naming the escalate command. `move_record`, `attach_reproduction` and `verify_finding` reach the store's write only after the gate has run (`move_record` then turns the error into an untyped `ExecError`, after appending the gate runs); checking first keeps the refusal typed and the ledger clean. *If wrong:* none.
18. **Plan ruling: in a routed store's lists, the tier column of a marked item reads `escalating`**, not `local`: the item is between the tiers, and §2.4 says it "is listed with that mark". *If wrong:* a script that reads the column sees a third value.
19. **Plan ruling: `fl record escalate` prints `<old handle>\tescalated\t#<n>`** on stdout; `--abandon` prints `<handle>\tabandoned`. *If wrong:* wording.
20. **Plan ruling: a record with no area escalates to an issue with no area, and is not sensitive** (spec decision 22's `[agent]` ruling, unchanged). A finding inherits nothing at escalation: its area is the one it was made with. *If wrong:* none.
21. **Plan ruling: the escalation is refused in a store with no routing map, and in a routed store whose project has no GitHub binding on this machine** — the router's `Unrouted` and `TierUnavailable` faults, unchanged. *If wrong:* none.
22. **Plan ruling: the live test drives the router over `MemStore` and the real `GithubTracker`** in `crates/github/tests/live.rs`, through `impl GithubTier for GithubTracker`. fl-github does not depend on fl-store or the CLI; `MemStore` implements `Escalations` like `RedbStore`, and the shared conformance cases prove the two alike. *If wrong:* the live test does not cover redb, which the conformance suite does.
23. `[agent]` **Plan ruling: a marked item's issue takes over, best effort** (spec §2.2: "the lookup returns the GitHub issue once it exists"). For an id the local tier holds marked, `route` asks GitHub by the escalation's own search — the item's primary IRI, back to the mark's time — and acts on the issue only when the search finds it and GitHub reads it as an fl item. In every other case — no issue found, an issue fl cannot read (`NotAnFlItem` after a stop between the create and its labels, or any error reading it), GitHub unbound, GitHub unreachable — the local copy answers, and the local store refuses every write to it with `Escalating`. Reads see the item's last state; a finding raised about it lands on the local IRI and resolves through the tombstone later (§2.5, §3.5); no write can land in two places. A merged list leaves out a marked local item when a GitHub item in the same list names its IRI as an alias, with no further request; `--tier local` still lists it, `escalating`. *Cost:* every routed call on a marked item pays the search and one read of a found issue, with no cache: `fl finding raise --record <marked>` searches twice (the placement's read of the record, then the write's). *If wrong:* while the issue is unreadable or GitHub out of reach, a read shows the local item's last state rather than an error, and a write is refused naming the escalate command, which finishes the escalation.

## Spec defects this plan found

All are amended in the spec's rev 2.4 (same file), each marked `[agent]`.

1. **§3.3 "The create key is derived from the old IRI."** Today's create key is minted at random by `Meta::new` (`crates/github/src/meta.rs:139`) and searched for only inside one create, back to that attempt's start (`crates/github/src/tracker.rs:966`). Plan ruling 1: the key *is* the old IRI, and the search is the escalation's own.
2. **§3.3 step 2 "The issue's text names who escalated it, why, and the old IRI".** A finding's issue text is its claim (`finding_from`), and `Meta` refuses unknown fields (`#[serde(deny_unknown_fields)]`). Plan rulings 2, 3.
3. **§3.3 step 2 "the item's title, state, area … aliases".** No create path carries a state other than `todo`/`raised` or any alias (`add_record_with_area` pins `Todo`; `finding_meta` drops `also_known_as`). The escalation gets a create of its own (Task 4).
4. **§3.2 "a title the GitHub tracker refuses".** A finding's issue title is `title_of(claim)` — trimmed and cut to 256 — so only a record's title can fail. Task 7's check applies to records.
5. **§3.4, §5 "the ledger entry written".** A routed store's ledger is local and writes no decision. Plan ruling 16.
6. **§2.4 "An item marked 'escalating' is listed with that mark."** No column for it exists. Plan ruling 18.
7. **§5 "One live test … the local item is a tombstone".** The live tests drive `GithubTracker` over `MemStore`; fl-github has no `RedbStore`. Plan ruling 22.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/core/src/escalation.rs` | create | `Mark`, `Tombstone`, `Escalations`, `Outgoing`, `Provenance`, `EscalationFault` |
| `crates/core/src/escalate.rs` | create | the router's `prepare_escalation`, `escalate`, `abandon_escalation`; `Prepared` |
| `crates/core/src/store.rs` | modify | `StoreError::{Escalating, Escalated, Escalation}` |
| `crates/core/src/mem.rs` | modify | `Escalations for MemStore`; refusals; lists leave tombstones out |
| `crates/core/src/conformance.rs` | modify | `escalations(make)`: the shared cases |
| `crates/core/src/routing.rs` | modify | `GithubTier`'s three new methods |
| `crates/core/src/mem_issues.rs` | modify | the three methods, create keys, knobs |
| `crates/core/src/tiered.rs` | modify | `escalations` field; `route` passes the id, follows a tombstone and reaches a marked item's issue; records resolved; merged lists show a marked item with an issue once; `escalating` |
| `crates/core/src/lib.rs` | modify | modules and re-exports |
| `crates/store/src/lib.rs` | modify | `ESCALATING`, `TOMBSTONES`; `Escalations for RedbStore`; refusals |
| `crates/github/src/meta.rs` | modify | `Meta.escalated`, `fl_format` 3, the provenance line |
| `crates/github/src/tracker.rs` | modify | `find_by_key`, `alias_taken`, `create_escalated`; `impl GithubTier for GithubTracker` |
| `crates/github/tests/live.rs` | modify | the escalation live test |
| `crates/cli/src/tiers.rs` | modify | `LazyGithub`'s three new methods |
| `crates/cli/src/main.rs` | modify | `escalations: &store` |
| `crates/cli/src/cmd/escalate.rs` | create | the shared CLI escalation: pre-checks, warning, output, the trigger's call, the marked refusal |
| `crates/cli/src/cmd/record.rs`, `finding.rs`, `mod.rs` | modify | `escalate` subcommands; the trigger; a marked item refused before a gate runs; `escalating` in lists |
| `crates/cli/tests/escalation.rs` | create | black-box escalation tests |
| `crates/exec/tests/escalated_evidence.rs` | create | evidence names the escalated record's issue |
| `docs/routing.md`, `docs/github-tracker.md` | modify | escalation for a person; block format 3 |
| `docs/superpowers/specs/2026-10-06-two-tier-routing-design.md` | modified with this plan (rev 2.4) | the seven defects; no task edits it |

---

## Interfaces every task shares

These are fixed. A task that produces one writes it exactly as here; a task that consumes one uses it exactly as here.

```rust
// crates/core/src/escalation.rs  (Task 1)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mark { pub by: String, pub reason: String, pub at_ms: u64 }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone { pub from: Iri, pub to: Iri, pub by: String, pub reason: String, pub at_ms: u64 }

/// The local store's side of an escalation (routing spec §3.3, §3.6). Every
/// id is resolved to its primary first; an id the store does not hold is
/// `NotOwned`, except in `mark_of` and `tombstone_of`, which answer `None`.
pub trait Escalations {
    /// Step 1. Refuses an item already marked (`AlreadyMarked`) or tombstoned
    /// (`Escalated`), and any kind but a record or a finding (`WrongKind`).
    fn mark(&self, id: &Iri, mark: &Mark) -> Result<(), StoreError>;
    fn mark_of(&self, id: &Iri) -> Result<Option<Mark>, StoreError>;
    /// `--abandon`. Refuses an item with no mark (`NotMarked`).
    fn unmark(&self, id: &Iri) -> Result<(), StoreError>;
    /// Step 3, in one write: the tombstone from the mark's who, why and time,
    /// and the mark removed. Refuses an item with no mark (`NotMarked`).
    fn tombstone(&self, id: &Iri, to: &Iri) -> Result<Tombstone, StoreError>;
    fn tombstone_of(&self, id: &Iri) -> Result<Option<Tombstone>, StoreError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance { pub from: Iri, pub by: String, pub reason: String }

/// What an escalation writes to GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    /// A record and its open findings in both tiers that are neither
    /// security findings nor in a sensitive or undeclared area (decisions
    /// 18, 21, 22).
    Record { record: Record, findings: Vec<Finding> },
    /// A finding, and its record as the router read it (where it lives now).
    Finding { finding: Finding, record: crate::tiered::RecordSeen },
}
impl Outgoing { pub fn id(&self) -> &Iri; pub fn kind(&self) -> Kind; pub fn title(&self) -> &str; }

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EscalationFault {
    NotLocal { id: Iri },                                   // "is not a local item"
    AlreadyEscalated { id: Iri, to: Iri },                  // "was escalated already"
    Closed { id: Iri, state: String },                      // "is in a closed state"
    Title { id: Iri, why: String },                         // "cannot be an issue's title"
    AliasTaken { alias: Iri, issue: Iri },                  // "already names"
    AlreadyMarked { id: Iri, kind: Kind },                  // "is marked escalating already"
    NotMarked { id: Iri },                                  // "is not marked escalating"
    IssueExists { id: Iri, kind: Kind, issue: Iri },        // "its issue exists"
}

// crates/core/src/store.rs  (Task 1)
// StoreError gains:
//   Escalating { id: Iri, to_finish: String }    — to_finish is `fl <kind> escalate <id>`
//   Escalated { from: Iri, to: Iri }
//   Escalation(#[from] EscalationFault)          — #[error(transparent)]

// crates/core/src/routing.rs  (Task 5) — GithubTier gains:
//   fn find_escalated(&self, key: &Iri, since_ms: u64) -> Result<Option<Iri>, StoreError>;
//   fn alias_taken(&self, alias: &Iri) -> Result<Option<Iri>, StoreError>;
//   fn create_escalated(&self, item: &Outgoing, from: &Provenance, since_ms: u64)
//       -> Result<Iri, StoreError>;   // searches first; labels a found unlabelled issue

// crates/core/src/tiered.rs  (Task 6)
// TieredTracker gains `pub escalations: &'a dyn Escalations`;
// route<T>(&self, id: &Iri, act: impl Fn(&dyn Tracker, &Iri) -> Result<T, StoreError>)
// pub fn escalating(&self, id: &Iri) -> Result<Option<Mark>, StoreError>

// crates/core/src/escalate.rs  (Task 7)
// pub struct Prepared  (private fields) with
//   id() -> &Iri, kind() -> Kind, outgoing() -> &Outgoing,
//   resumes() -> Option<&Mark>, found() -> Option<&Iri>
// impl TieredTracker:
//   pub fn prepare_escalation(&self, id: &Iri, kind: Kind) -> Result<Prepared, StoreError>
//   pub fn escalate(&self, at: &Prepared, by: &str, reason: &str, now_ms: u64)
//       -> Result<Iri, StoreError>
//   pub fn abandon_escalation(&self, id: &Iri, kind: Kind) -> Result<(), StoreError>

// crates/github/src/tracker.rs  (Task 4; Task 5 adds `impl GithubTier for GithubTracker`)
//   pub fn find_by_key(&self, key: &str, since_ms: u64) -> Result<Option<IssueView>, StoreError>
//   pub fn alias_taken(&self, alias: &Iri) -> Result<Option<Iri>, StoreError>
//   pub fn create_escalated(&self, item: &Outgoing, from: &Provenance, since_ms: u64)
//       -> Result<Iri, StoreError>

// crates/cli/src/cmd/escalate.rs  (Task 8)
//   pub fn run(ctx: &Ctx<'_>, kind: Kind, id: &Ref, by: Option<&str>, reason: Option<&str>,
//       abandon: bool) -> Result<i32>        // Options: `--abandon` takes neither
//   pub fn after_landed_move(ctx: &Ctx<'_>, record: &Record, shown: &str)   // Task 9 calls it
```

---

## Tasks

| # | Task | Crates |
|---|---|---|
| 1 | The mark, the tombstone and the escalation faults; `MemStore` refuses writes to a marked or tombstoned item | core |
| 2 | The local store keeps marks and tombstones | store |
| 3 | An escalated issue's block carries its provenance (`fl_format` 3) | github |
| 4 | The GitHub tracker finds, checks and creates an escalated issue | github |
| 5 | What the router asks of the GitHub tier for an escalation | core, github, cli |
| 6 | The router follows a tombstone and sees a mark | core, cli, exec |
| 7 | The router escalates: pre-checks, mark, find or create, tombstone, abandon | core |
| 8 | `fl record escalate` and `fl finding escalate` | cli |
| 9 | A landed move to `needs_human` escalates the record | cli |
| 10 | Lists, handles and evidence after an escalation | cli, exec |
| 11 | The escalation live test | github |

---

### Task 1: The mark, the tombstone and the escalation faults; `MemStore` refuses writes to a marked or tombstoned item

An escalation marks the local item "escalating" with who, why and the time, and from then on the local store refuses writes to it, naming `fl record escalate <id>` (or `fl finding escalate <id>`) to finish; a finding raised against a marked record is not a write to it and is allowed (routing spec §3.3 step 1). Once the issue exists the item is replaced by a tombstone — the old IRI, the new IRI, who, when, why (§3.3 step 3) — and a tombstoned id reads as `StoreError::Escalated { from, to }`, which the router follows (§3.6); tombstones are not listed (§2.4). This task gives `fl-core` the types every later task shares (`Mark`, `Tombstone`, `Escalations`, `Provenance`, `Outgoing`, `EscalationFault`), the three new `StoreError` variants, `MemStore`'s marks and tombstones (plan rulings 5, 7: tables of their own keyed by the primary IRI; the item's row, id, handle and aliases stay), and a shared conformance suite that `RedbStore` runs in Task 2.

**Blast radius:** `StoreError` gains three variants. No code matches it exhaustively (only `matches!`), so the workspace compiles unchanged, and `is_transient` counts none of them. `MemStore`'s `Tracker` methods gain refusals that fire only for an item with a mark or a tombstone, which nothing creates outside this task's tests — every existing configuration behaves exactly as before. `conformance.rs` gains a suite and a fixture trait; no existing suite changes.

**Files:**
- Create: `crates/core/src/escalation.rs` (`Mark`, `Tombstone`, `Escalations`, `Provenance`, `Outgoing`, `escalate_command`, `EscalationFault`; tests)
- Modify: `crates/core/src/store.rs` (`StoreError::{Escalating, Escalated, Escalation}`; the transience test)
- Modify: `crates/core/src/mem.rs` (`Inner.escalating`, `Inner.tombstones`, `live`, `writable`; the `Tracker` refusals and list filters; `impl Escalations for MemStore`; tests)
- Modify: `crates/core/src/conformance.rs` (`EscalationBound`, `EscalationFixture`, `escalations`, `ESCALATION_CASES` and eight cases)
- Modify: `crates/core/src/lib.rs` (`pub mod escalation;` and its re-exports)

**Interfaces:**
- Consumes: `RecordSeen` (`crate::tiered`); `Kind::as_wire`; `MemStore`'s `Inner::{check, resolve}`; `conformance::{Single, stranger, assert_all_not_owned}`.
- Produces (exactly as "Interfaces every task shares"): `pub struct Mark { pub by: String, pub reason: String, pub at_ms: u64 }`; `pub struct Tombstone { pub from: Iri, pub to: Iri, pub by: String, pub reason: String, pub at_ms: u64 }`; `pub trait Escalations { fn mark(&self, id: &Iri, mark: &Mark) -> Result<(), StoreError>; fn mark_of(&self, id: &Iri) -> Result<Option<Mark>, StoreError>; fn unmark(&self, id: &Iri) -> Result<(), StoreError>; fn tombstone(&self, id: &Iri, to: &Iri) -> Result<Tombstone, StoreError>; fn tombstone_of(&self, id: &Iri) -> Result<Option<Tombstone>, StoreError>; }`; `pub struct Provenance { pub from: Iri, pub by: String, pub reason: String }`; `pub enum Outgoing { Record { record: Record, findings: Vec<Finding> }, Finding { finding: Finding, record: RecordSeen } }` with `id(&self) -> &Iri`, `kind(&self) -> Kind`, `title(&self) -> &str` (a record's title, a finding's claim); `pub enum EscalationFault` with the eight variants; `StoreError::{Escalating { id: Iri, to_finish: String }, Escalated { from: Iri, to: Iri }, Escalation(#[from] EscalationFault)}`; `impl Escalations for MemStore`.
- Produces, beyond the shared block: `pub fn escalate_command(kind: Kind, id: &Iri) -> String` in `escalation.rs` (`fl <kind> escalate <id>` — what `Escalating.to_finish`, `AlreadyMarked` and `IssueExists` name, and what `RedbStore` uses in Task 2); in `conformance`: `pub struct EscalationBound<'a> { pub catalog: &'a dyn Catalog, pub tracker: &'a dyn Tracker, pub escalations: &'a dyn Escalations }`, `pub trait EscalationFixture { fn with_escalations(&self, f: &mut dyn FnMut(&EscalationBound<'_>)); }` (implemented for `Single<S, G>` where `S: Catalog + Tracker + Escalations`), `pub fn escalations<F: EscalationFixture>(make: impl Fn() -> F)`, and the case `pub fn a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed(b: &EscalationBound<'_>)`, public so a store's tests can run it by name.
- Unique phrases: `is not a local item`, `was escalated already`, `is in a closed state`, `cannot be an issue's title`, `already names`, `is marked escalating already`, `is not marked escalating`, `its issue exists`, `so this store refuses to change it` (`Escalating`), `was escalated to GitHub and is now` (`Escalated`).

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/escalation.rs` holding, for now, only its tests, and in `crates/core/src/lib.rs`, after `pub mod decision;`, add `pub mod escalation;`. The file's tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{FindingId, ProjectId, RecordId, seq_iri};
    use crate::model::State;
    use crate::routing::Tier;

    fn issue(n: u64) -> Iri {
        Iri::parse(&format!("https://github.com/acme/widgets/issues/{n}")).unwrap()
    }

    // Routing spec §4: every escalation refusal names its cause and what to
    // do — each in a phrase no other refusal says, so a test that asserts
    // one phrase can tell which refusal it got.
    #[test]
    fn every_escalation_refusal_names_its_remedy() {
        let id = seq_iri(1);
        let said: Vec<(StoreError, &str)> = vec![
            (
                EscalationFault::NotLocal { id: issue(3) }.into(),
                "is not a local item",
            ),
            (
                EscalationFault::AlreadyEscalated {
                    id: id.clone(),
                    to: issue(7),
                }
                .into(),
                "was escalated already",
            ),
            (
                EscalationFault::Closed {
                    id: id.clone(),
                    state: "done".into(),
                }
                .into(),
                "is in a closed state",
            ),
            (
                EscalationFault::Title {
                    id: id.clone(),
                    why: "it is over 256 characters".into(),
                }
                .into(),
                "cannot be an issue's title",
            ),
            (
                EscalationFault::AliasTaken {
                    alias: issue(4),
                    issue: issue(4),
                }
                .into(),
                "already names",
            ),
            (
                EscalationFault::AlreadyMarked {
                    id: id.clone(),
                    kind: Kind::Finding,
                }
                .into(),
                "is marked escalating already",
            ),
            (
                EscalationFault::NotMarked { id: id.clone() }.into(),
                "is not marked escalating",
            ),
            (
                EscalationFault::IssueExists {
                    id: id.clone(),
                    kind: Kind::Record,
                    issue: issue(7),
                }
                .into(),
                "its issue exists",
            ),
            (
                StoreError::Escalating {
                    id: id.clone(),
                    to_finish: escalate_command(Kind::Record, &id),
                },
                "so this store refuses to change it",
            ),
            (
                StoreError::Escalated {
                    from: id.clone(),
                    to: issue(7),
                },
                "was escalated to GitHub and is now",
            ),
        ];
        let messages: Vec<String> = said.iter().map(|(e, _)| e.to_string()).collect();
        for (i, (_, phrase)) in said.iter().enumerate() {
            for (j, msg) in messages.iter().enumerate() {
                assert_eq!(msg.contains(phrase), i == j, "{phrase:?} in {msg}");
            }
        }
        let finish = format!("`fl finding escalate {id}`");
        assert!(messages[5].contains(&finish), "{}", messages[5]);
        assert!(messages[5].contains("`--abandon`"), "{}", messages[5]);
        let finish = format!("`fl record escalate {id}`");
        assert!(messages[7].contains(&finish), "{}", messages[7]);
        assert!(messages[7].contains(issue(7).as_str()), "{}", messages[7]);
        assert!(messages[1].contains(issue(7).as_str()), "{}", messages[1]);
        assert!(messages[8].contains(&finish), "{}", messages[8]);
        let abandon = format!("`fl record escalate {id} --abandon`");
        assert!(messages[8].contains(&abandon), "{}", messages[8]);
        assert!(messages[9].contains(issue(7).as_str()), "{}", messages[9]);
    }

    #[test]
    fn the_command_that_finishes_an_escalation_names_the_kind_and_the_id() {
        assert_eq!(
            escalate_command(Kind::Finding, &seq_iri(4)),
            format!("fl finding escalate {}", seq_iri(4))
        );
        assert_eq!(
            escalate_command(Kind::Record, &seq_iri(4)),
            format!("fl record escalate {}", seq_iri(4))
        );
    }

    #[test]
    fn an_outgoing_item_answers_its_id_kind_and_title() {
        let p = ProjectId(seq_iri(1));
        let record = Record {
            id: RecordId(seq_iri(2)),
            project: p.clone(),
            title: "the title".into(),
            state: State::NeedsHuman,
            also_known_as: vec![],
            area: Some("design".into()),
        };
        let mut finding = Finding::raise(p, record.id.clone(), "rev", "the claim");
        finding.id = FindingId(seq_iri(3));
        let out = Outgoing::Record {
            record: record.clone(),
            findings: vec![finding.clone()],
        };
        assert_eq!(
            (out.id(), out.kind(), out.title()),
            (record.id.iri(), Kind::Record, "the title")
        );
        let out = Outgoing::Finding {
            finding: finding.clone(),
            record: RecordSeen {
                id: record.id.clone(),
                title: "the title".into(),
                tier: Tier::Local,
            },
        };
        assert_eq!(
            (out.id(), out.kind(), out.title()),
            (finding.id.iri(), Kind::Finding, "the claim")
        );
    }

    // The store keeps both as JSON (routing spec §3.6): snake_case, and a
    // field this version does not know is refused rather than dropped.
    #[test]
    fn a_mark_and_a_tombstone_are_kept_as_json_and_refuse_unknown_fields() {
        let mark = Mark {
            by: "alice".into(),
            reason: "needs a design review".into(),
            at_ms: 5,
        };
        let json = serde_json::to_string(&mark).unwrap();
        assert_eq!(
            json,
            r#"{"by":"alice","reason":"needs a design review","at_ms":5}"#
        );
        assert_eq!(serde_json::from_str::<Mark>(&json).unwrap(), mark);
        let extra = r#"{"by":"a","reason":"r","at_ms":5,"to":"x"}"#;
        assert!(serde_json::from_str::<Mark>(extra).is_err());
        let tomb = Tombstone {
            from: seq_iri(2),
            to: issue(7),
            by: "alice".into(),
            reason: "r".into(),
            at_ms: 5,
        };
        let json = serde_json::to_string(&tomb).unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"from":"{}","to":"{}","by":"alice","reason":"r","at_ms":5}}"#,
                seq_iri(2),
                issue(7)
            )
        );
        assert_eq!(serde_json::from_str::<Tombstone>(&json).unwrap(), tomb);
        let extra = json.replace("\"at_ms\":5", "\"at_ms\":5,\"why\":\"x\"");
        assert!(serde_json::from_str::<Tombstone>(&extra).is_err());
    }
}
```

In `crates/core/src/store.rs`, inside `mod tests`, in `only_an_unreachable_a_rate_limited_or_a_contended_ledger_is_transient`, at the end of the `lasting` array (after the `StoreError::Ledger(LedgerFault::NotSetUp { … })` element), add:

```rust
            // Routing spec §3.6: an escalation is finished or abandoned, and a
            // tombstone is followed — waiting changes neither.
            StoreError::Escalating {
                id: seq_iri(1),
                to_finish: "fl record escalate x".into(),
            },
            StoreError::Escalated {
                from: seq_iri(1),
                to: seq_iri(2),
            },
            StoreError::Escalation(crate::escalation::EscalationFault::NotMarked {
                id: seq_iri(1),
            }),
```

In `crates/core/src/conformance.rs`, after `use crate::decision::{…};`, add:

```rust
use crate::escalation::{EscalationFault, Escalations, Mark, Tombstone, escalate_command};
```

After `const LEDGER_CACHE_CASES: usize = 6;`, add:

```rust
/// How many cases [`escalations`] runs. Update deliberately — see [`run_suite`].
const ESCALATION_CASES: usize = 8;
```

After `pub fn ledger_cache` (its closing brace), add:

```rust
/// The roles an escalation case uses (routing spec §3.3, §3.6): a local
/// store's catalog and tracker, and its marks and tombstones — one store.
pub struct EscalationBound<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
    pub escalations: &'a dyn Escalations,
}

/// Hands one escalation case its [`EscalationBound`], as [`Fixture`] does.
pub trait EscalationFixture {
    fn with_escalations(&self, f: &mut dyn FnMut(&EscalationBound<'_>));
}

impl<S: Catalog + Tracker + Escalations, G> EscalationFixture for Single<S, G> {
    fn with_escalations(&self, f: &mut dyn FnMut(&EscalationBound<'_>)) {
        f(&EscalationBound {
            catalog: &self.0,
            tracker: &self.0,
            escalations: &self.0,
        });
    }
}

/// What every local store does with an escalation's mark and tombstone
/// (routing spec §3.3, §3.6): a marked item reads as itself and refuses
/// every write, naming the command that finishes it; a tombstoned item
/// reads as `Escalated` from every name it has, refuses every write, and
/// is left out of every list.
pub fn escalations<F: EscalationFixture>(make: impl Fn() -> F) {
    let cases: &[fn(&EscalationBound<'_>)] = &[
        a_mark_reads_back_and_a_second_mark_is_refused,
        unmarking_clears_the_mark_and_a_second_unmark_is_refused,
        a_tombstone_is_made_from_the_mark_and_replaces_it,
        a_tombstoned_item_reads_as_escalated_and_refuses_every_write,
        a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed,
        a_tombstoned_item_is_left_out_of_every_list,
        an_id_this_store_never_held_has_no_mark_and_no_tombstone,
        an_alias_reaches_the_primary_in_every_escalation_method,
    ];
    assert_eq!(
        cases.len(),
        ESCALATION_CASES,
        "the escalation suite lists {} cases but declares {ESCALATION_CASES}. A case was \
         added or removed: if that was deliberate, update the count beside the list; if not, \
         restore the case",
        cases.len()
    );
    for case in cases {
        let fixture = make();
        fixture.with_escalations(&mut |b| case(b));
    }
}
```

Before `/// A well-formed id that no store in these tests ever mints.` (`fn stranger`), add:

```rust
/// A project, a record in it, and a finding about the record.
fn escalation_world(b: &EscalationBound<'_>) -> (ProjectId, RecordId, FindingId) {
    let p = b.catalog.add_project("/p").unwrap();
    let r = b.tracker.add_record(&p, "look at the layout").unwrap();
    let f = b
        .tracker
        .add_finding(Finding::raise(
            p.clone(),
            r.clone(),
            "rev",
            "the margin is off",
        ))
        .unwrap();
    (p, r, f)
}

fn sample_mark() -> Mark {
    Mark {
        by: "alice".into(),
        reason: "needs a design review".into(),
        at_ms: 1_000,
    }
}

fn sample_issue(n: u64) -> Iri {
    Iri::parse(&format!("https://github.com/acme/widgets/issues/{n}")).unwrap()
}

/// `Escalating`, naming `id` and the command that finishes its escalation.
fn assert_escalating(err: StoreError, id: &Iri, kind: Kind) {
    assert!(
        matches!(
            err,
            StoreError::Escalating { id: ref i, ref to_finish }
                if i == id && *to_finish == escalate_command(kind, id)
        ),
        "{err:?}"
    );
}

/// `Escalated`, from `from` to `to`.
fn assert_escalated(err: StoreError, from: &Iri, to: &Iri) {
    assert!(
        matches!(err, StoreError::Escalated { from: ref f, to: ref t } if f == from && t == to),
        "{err:?}"
    );
}

/// Routing spec §3.3 step 1: the mark holds who, why and when; an item is
/// marked once; only a record or a finding is escalated.
pub fn a_mark_reads_back_and_a_second_mark_is_refused(b: &EscalationBound<'_>) {
    let (p, r, f) = escalation_world(b);
    let later = Mark {
        by: "bob".into(),
        reason: "again".into(),
        at_ms: 2_000,
    };
    for (id, kind) in [(r.iri(), Kind::Record), (f.iri(), Kind::Finding)] {
        assert_eq!(b.escalations.mark_of(id).unwrap(), None);
        b.escalations.mark(id, &sample_mark()).unwrap();
        assert_eq!(b.escalations.mark_of(id).unwrap(), Some(sample_mark()));
        let err = b.escalations.mark(id, &later).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Escalation(EscalationFault::AlreadyMarked { id: ref i, kind: k })
                    if i == id && k == kind
            ),
            "{err:?}"
        );
        assert_eq!(
            b.escalations.mark_of(id).unwrap(),
            Some(sample_mark()),
            "the first mark is kept"
        );
    }
    let err = b.escalations.mark(p.iri(), &sample_mark()).unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::WrongKind {
                found: Kind::Project,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(b.escalations.mark_of(p.iri()).unwrap(), None);
}

/// Routing spec §3.3, "Abandoning": the mark is removed, and an item with
/// no mark has nothing to abandon.
pub fn unmarking_clears_the_mark_and_a_second_unmark_is_refused(b: &EscalationBound<'_>) {
    let (_p, r, _f) = escalation_world(b);
    b.escalations.mark(r.iri(), &sample_mark()).unwrap();
    b.escalations.unmark(r.iri()).unwrap();
    assert_eq!(b.escalations.mark_of(r.iri()).unwrap(), None);
    let err = b.escalations.unmark(r.iri()).unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Escalation(EscalationFault::NotMarked { id: ref i }) if i == r.iri()
        ),
        "{err:?}"
    );
    b.tracker.set_record_state(&r, State::Doing).unwrap();
    b.escalations.mark(r.iri(), &sample_mark()).unwrap();
}

/// Routing spec §3.3 step 3: the tombstone takes the mark's who, why and
/// time, and replaces the mark in the same write; with no mark there is no
/// escalation to finish.
pub fn a_tombstone_is_made_from_the_mark_and_replaces_it(b: &EscalationBound<'_>) {
    let (_p, r, _f) = escalation_world(b);
    let err = b
        .escalations
        .tombstone(r.iri(), &sample_issue(7))
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Escalation(EscalationFault::NotMarked { id: ref i }) if i == r.iri()
        ),
        "{err:?}"
    );
    assert_eq!(b.escalations.tombstone_of(r.iri()).unwrap(), None);
    b.escalations.mark(r.iri(), &sample_mark()).unwrap();
    let tomb = b.escalations.tombstone(r.iri(), &sample_issue(7)).unwrap();
    assert_eq!(
        tomb,
        Tombstone {
            from: r.iri().clone(),
            to: sample_issue(7),
            by: "alice".into(),
            reason: "needs a design review".into(),
            at_ms: 1_000,
        }
    );
    assert_eq!(
        b.escalations.tombstone_of(r.iri()).unwrap(),
        Some(tomb.clone())
    );
    assert_eq!(
        b.escalations.mark_of(r.iri()).unwrap(),
        None,
        "the mark is gone"
    );
    assert_eq!(
        b.catalog.kind_of(r.iri()).unwrap(),
        Kind::Record,
        "the id still chooses this store"
    );
    let err = b.escalations.mark(r.iri(), &sample_mark()).unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
    let err = b
        .escalations
        .tombstone(r.iri(), &sample_issue(8))
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Escalation(EscalationFault::NotMarked { .. })
        ),
        "{err:?}"
    );
    assert_eq!(b.escalations.tombstone_of(r.iri()).unwrap(), Some(tomb));
}

/// Routing spec §3.3 step 1, §3.6: a marked item reads as itself and is
/// listed, and every write to it is refused naming the command that
/// finishes the escalation. A finding raised about a marked record is not a
/// write to it.
pub fn a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed(
    b: &EscalationBound<'_>,
) {
    let (p, r, f) = escalation_world(b);
    let other = b.tracker.add_record(&p, "unmarked").unwrap();
    b.escalations.mark(r.iri(), &sample_mark()).unwrap();
    b.escalations.mark(f.iri(), &sample_mark()).unwrap();

    assert_eq!(b.tracker.get_record(&r).unwrap().unwrap().id, r);
    let held = b.tracker.get_finding(&f).unwrap().unwrap();
    assert_eq!(held.id, f);
    let records: Vec<RecordId> = b
        .tracker
        .list_records(&p)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(records, vec![r.clone(), other.clone()]);
    assert_eq!(b.tracker.list_findings(&p).unwrap().len(), 1);

    let err = b.tracker.set_record_state(&r, State::Doing).unwrap_err();
    assert_escalating(err, r.iri(), Kind::Record);
    assert_eq!(
        b.tracker.get_record(&r).unwrap().unwrap().state,
        State::Todo
    );
    let mut changed = held;
    changed.withdraw("not concrete").unwrap();
    let err = b.tracker.update_finding(&changed).unwrap_err();
    assert_escalating(err, f.iri(), Kind::Finding);
    assert_eq!(
        b.tracker.get_finding(&f).unwrap().unwrap().state,
        FindingState::Raised
    );
    let err = b.tracker.add_alias(r.iri(), sample_issue(41)).unwrap_err();
    assert_escalating(err, r.iri(), Kind::Record);
    let err = b.tracker.add_alias(f.iri(), sample_issue(42)).unwrap_err();
    assert_escalating(err, f.iri(), Kind::Finding);
    assert!(
        b.tracker.get_record(&RecordId(sample_issue(41))).is_err(),
        "no alias was added"
    );

    let raised = b
        .tracker
        .add_finding(Finding::raise(p, r.clone(), "rev", "and the gutter"))
        .unwrap();
    assert_eq!(b.tracker.get_finding(&raised).unwrap().unwrap().record, r);
    b.tracker.set_record_state(&other, State::Doing).unwrap();
}

/// Routing spec §3.6: a tombstoned id reads as `Escalated`, every write to
/// it is refused the same way, and so is a finding raised about a
/// tombstoned record — the router follows the tombstone instead.
pub fn a_tombstoned_item_reads_as_escalated_and_refuses_every_write(b: &EscalationBound<'_>) {
    let (p, r, f) = escalation_world(b);
    let held = b.tracker.get_finding(&f).unwrap().unwrap();
    b.escalations.mark(r.iri(), &sample_mark()).unwrap();
    b.escalations.tombstone(r.iri(), &sample_issue(7)).unwrap();
    b.escalations.mark(f.iri(), &sample_mark()).unwrap();
    b.escalations.tombstone(f.iri(), &sample_issue(8)).unwrap();

    assert_escalated(
        b.tracker.get_record(&r).unwrap_err(),
        r.iri(),
        &sample_issue(7),
    );
    assert_escalated(
        b.tracker.get_finding(&f).unwrap_err(),
        f.iri(),
        &sample_issue(8),
    );
    let err = b.tracker.set_record_state(&r, State::Doing).unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
    let mut changed = held;
    changed.withdraw("not concrete").unwrap();
    let err = b.tracker.update_finding(&changed).unwrap_err();
    assert_escalated(err, f.iri(), &sample_issue(8));
    let err = b.tracker.add_alias(r.iri(), sample_issue(41)).unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
    let err = b.tracker.add_alias(f.iri(), sample_issue(42)).unwrap_err();
    assert_escalated(err, f.iri(), &sample_issue(8));
    let err = b
        .tracker
        .add_finding(Finding::raise(p, r.clone(), "rev", "and the gutter"))
        .unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
}

/// Routing spec §2.4: tombstones are not listed, and a withdrawal is
/// counted once — by the tier the finding lives in now.
pub fn a_tombstoned_item_is_left_out_of_every_list(b: &EscalationBound<'_>) {
    let p = b.catalog.add_project("/p").unwrap();
    let kept = b.tracker.add_record(&p, "kept").unwrap();
    let gone = b.tracker.add_record(&p, "gone").unwrap();
    let mut findings = vec![];
    for claim in ["stays", "goes"] {
        let id = b
            .tracker
            .add_finding(Finding::raise(p.clone(), kept.clone(), "hasty", claim))
            .unwrap();
        let mut f = b.tracker.get_finding(&id).unwrap().unwrap();
        f.withdraw("not concrete").unwrap();
        b.tracker.update_finding(&f).unwrap();
        findings.push(id);
    }
    assert_eq!(b.tracker.withdrawals_by("hasty").unwrap(), 2);
    for (id, n) in [(gone.iri(), 7), (findings[1].iri(), 8)] {
        b.escalations.mark(id, &sample_mark()).unwrap();
        b.escalations.tombstone(id, &sample_issue(n)).unwrap();
    }
    let records: Vec<RecordId> = b
        .tracker
        .list_records(&p)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(records, vec![kept]);
    let listed: Vec<FindingId> = b
        .tracker
        .list_findings(&p)
        .unwrap()
        .into_iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(listed, vec![findings[0].clone()]);
    assert_eq!(b.tracker.withdrawals_by("hasty").unwrap(), 1);
}

/// `mark_of` and `tombstone_of` answer `None` for an id the store does not
/// hold — the router asks before it knows — while the writes refuse it as
/// `NotOwned`, never as "not marked".
pub fn an_id_this_store_never_held_has_no_mark_and_no_tombstone(b: &EscalationBound<'_>) {
    let _ = escalation_world(b);
    let id = stranger();
    assert_eq!(b.escalations.mark_of(&id).unwrap(), None);
    assert_eq!(b.escalations.tombstone_of(&id).unwrap(), None);
    assert_eq!(b.escalations.mark_of(&sample_issue(7)).unwrap(), None);
    assert_all_not_owned(
        &id,
        vec![
            ("mark", b.escalations.mark(&id, &sample_mark())),
            ("unmark", b.escalations.unmark(&id)),
            (
                "tombstone",
                b.escalations.tombstone(&id, &sample_issue(7)).map(|_| ()),
            ),
        ],
    );
}

/// Every escalation method resolves an alias to the item's primary IRI:
/// the mark, the tombstone and the refusals all name the primary.
pub fn an_alias_reaches_the_primary_in_every_escalation_method(b: &EscalationBound<'_>) {
    let (p, r, f) = escalation_world(b);
    let ra = sample_issue(41);
    let fa = sample_issue(42);
    b.tracker.add_alias(r.iri(), ra.clone()).unwrap();
    b.tracker.add_alias(f.iri(), fa.clone()).unwrap();

    b.escalations.mark(&ra, &sample_mark()).unwrap();
    assert_eq!(b.escalations.mark_of(r.iri()).unwrap(), Some(sample_mark()));
    assert_eq!(b.escalations.mark_of(&ra).unwrap(), Some(sample_mark()));
    let err = b.escalations.mark(r.iri(), &sample_mark()).unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Escalation(EscalationFault::AlreadyMarked { id: ref i, .. })
                if i == r.iri()
        ),
        "{err:?}"
    );
    let err = b
        .tracker
        .set_record_state(&RecordId(ra.clone()), State::Doing)
        .unwrap_err();
    assert_escalating(err, r.iri(), Kind::Record);
    b.escalations.unmark(&ra).unwrap();
    assert_eq!(b.escalations.mark_of(r.iri()).unwrap(), None);

    b.escalations.mark(&fa, &sample_mark()).unwrap();
    let tomb = b.escalations.tombstone(&fa, &sample_issue(8)).unwrap();
    assert_eq!(&tomb.from, f.iri(), "the tombstone names the primary");
    assert_eq!(
        b.escalations.tombstone_of(f.iri()).unwrap(),
        Some(tomb.clone())
    );
    assert_eq!(b.escalations.tombstone_of(&fa).unwrap(), Some(tomb));
    let err = b.tracker.get_finding(&FindingId(fa)).unwrap_err();
    assert_escalated(err, f.iri(), &sample_issue(8));

    b.escalations.mark(&ra, &sample_mark()).unwrap();
    b.escalations.tombstone(&ra, &sample_issue(7)).unwrap();
    let err = b.tracker.get_record(&RecordId(ra.clone())).unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
    let err = b
        .tracker
        .add_finding(Finding::raise(p, RecordId(ra), "rev", "c"))
        .unwrap_err();
    assert_escalated(err, r.iri(), &sample_issue(7));
}
```

In `crates/core/src/mem.rs`, inside `mod tests`, before `a_split_ledger_over_a_mem_store_meets_the_ledger_contracts`, add:

```rust
    #[test]
    fn mem_store_meets_the_escalation_contract() {
        use crate::conformance::Single;
        crate::conformance::escalations(|| Single(MemStore::default(), ()));
    }

    // Routing spec §3.3 step 1: the shared case, run on its own by name.
    #[test]
    fn a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed() {
        use crate::conformance::{EscalationFixture, Single};
        Single(MemStore::default(), ()).with_escalations(&mut |b| {
            crate::conformance::a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed(
                b,
            )
        });
    }

    // Routing spec §2.3: a local handle of an escalated item resolves
    // through its tombstone, so the handle and the row stay.
    #[test]
    fn an_escalated_items_handle_still_resolves_to_its_id() {
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let mark = Mark {
            by: "alice".into(),
            reason: "r".into(),
            at_ms: 1,
        };
        s.mark(r.iri(), &mark).unwrap();
        let to = Iri::parse("https://github.com/acme/widgets/issues/7").unwrap();
        s.tombstone(r.iri(), &to).unwrap();
        assert_eq!(
            s.resolve_handle(Kind::Record, 1).unwrap(),
            Some(r.0.clone())
        );
        assert_eq!(s.handle_of(Kind::Record, r.iri()).unwrap(), Some(1));
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-core --lib`
Expected: FAIL to compile — `error[E0432]: unresolved imports` for `crate::escalation::{EscalationFault, Escalations, Mark, Tombstone, escalate_command}`: the module holds only its tests, so `Mark`, `Tombstone`, `Outgoing`, `EscalationFault`, `escalate_command` and `StoreError::Escalating` do not exist.

- [ ] **Step 3: Implement**

At the top of `crates/core/src/escalation.rs`, above `#[cfg(test)] mod tests`, add:

```rust
//! Escalation (routing spec §3): the local store's side of moving a record
//! or a finding to GitHub — the mark that blocks local writes while the
//! issue is found or made, the tombstone that replaces the item once it
//! exists — and the refusals the escalation names.

use crate::finding::Finding;
use crate::ids::Kind;
use crate::iri::Iri;
use crate::model::Record;
use crate::store::StoreError;
use crate::tiered::RecordSeen;
use serde::{Deserialize, Serialize};

/// Step 1 of an escalation (routing spec §3.3): who escalated the item, why,
/// and when, in unix milliseconds. While it stands the local store refuses
/// every write to the item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mark {
    pub by: String,
    pub reason: String,
    pub at_ms: u64,
}

/// Step 3 (routing spec §3.3): what replaces the local item once its issue
/// exists — the old IRI, the new one, and the mark's who, why and time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    pub from: Iri,
    pub to: Iri,
    pub by: String,
    pub reason: String,
    pub at_ms: u64,
}

/// The local store's side of an escalation (routing spec §3.3, §3.6). Every
/// id is resolved to its primary first; an id the store does not hold is
/// `NotOwned`, except in `mark_of` and `tombstone_of`, which answer `None`.
pub trait Escalations {
    /// Step 1. Refuses an item already marked (`AlreadyMarked`) or tombstoned
    /// (`Escalated`), and any kind but a record or a finding (`WrongKind`).
    fn mark(&self, id: &Iri, mark: &Mark) -> Result<(), StoreError>;
    fn mark_of(&self, id: &Iri) -> Result<Option<Mark>, StoreError>;
    /// `--abandon`. Refuses an item with no mark (`NotMarked`).
    fn unmark(&self, id: &Iri) -> Result<(), StoreError>;
    /// Step 3, in one write: the tombstone from the mark's who, why and time,
    /// and the mark removed. Refuses an item with no mark (`NotMarked`).
    fn tombstone(&self, id: &Iri, to: &Iri) -> Result<Tombstone, StoreError>;
    fn tombstone_of(&self, id: &Iri) -> Result<Option<Tombstone>, StoreError>;
}

/// Who escalated an item, why, and the IRI it had in the local tier — what
/// the issue says of where it came from (routing spec §3.3 step 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub from: Iri,
    pub by: String,
    pub reason: String,
}

/// What an escalation writes to GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    /// A record and its open findings in both tiers that are neither
    /// security findings nor in a sensitive or undeclared area (decisions
    /// 18, 21, 22).
    Record {
        record: Record,
        findings: Vec<Finding>,
    },
    /// A finding, and its record as the router read it (where it lives now).
    Finding {
        finding: Finding,
        record: RecordSeen,
    },
}

impl Outgoing {
    /// The item's local IRI.
    pub fn id(&self) -> &Iri {
        match self {
            Outgoing::Record { record, .. } => record.id.iri(),
            Outgoing::Finding { finding, .. } => finding.id.iri(),
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Outgoing::Record { .. } => Kind::Record,
            Outgoing::Finding { .. } => Kind::Finding,
        }
    }

    /// A record's title, or a finding's claim: the text the issue's title
    /// is made from.
    pub fn title(&self) -> &str {
        match self {
            Outgoing::Record { record, .. } => &record.title,
            Outgoing::Finding { finding, .. } => &finding.claim,
        }
    }
}

/// The command that finishes the escalation of `id`, a `kind` (routing spec
/// §3.3 step 1): what every refusal of a marked item names.
pub fn escalate_command(kind: Kind, id: &Iri) -> String {
    format!("fl {} escalate {id}", kind.as_wire())
}

/// Why an escalation was refused (routing spec §3.1, §3.2, §4): each names
/// its cause and what to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EscalationFault {
    #[error(
        "{id} is not a local item, so there is nothing to escalate: only an item in the local \
         tier moves to GitHub"
    )]
    NotLocal { id: Iri },
    #[error("{id} was escalated already, to {to}. Read and change it there")]
    AlreadyEscalated { id: Iri, to: Iri },
    #[error(
        "{id} is in a closed state, `{state}`, and GitHub takes open items only, so it cannot be \
         escalated. Nothing was written; it stays in the local tier"
    )]
    Closed { id: Iri, state: String },
    #[error(
        "the title of {id} cannot be an issue's title: {why}. Nothing was written; it stays in \
         the local tier"
    )]
    Title { id: Iri, why: String },
    #[error(
        "the alias {alias} already names {issue} on GitHub, and one id names one item across \
         both tiers, so the escalation would give it two meanings. Nothing was written"
    )]
    AliasTaken { alias: Iri, issue: Iri },
    #[error(
        "{id} is marked escalating already. Run `{}` to finish the escalation, or add \
         `--abandon` to stop it",
        escalate_command(*kind, id)
    )]
    AlreadyMarked { id: Iri, kind: Kind },
    #[error("{id} is not marked escalating, so there is no escalation to finish or abandon")]
    NotMarked { id: Iri },
    #[error(
        "the escalation of {id} cannot be abandoned: its issue exists, {issue}. Finish it with \
         `{}` instead, which replaces the local item with a tombstone",
        escalate_command(*kind, id)
    )]
    IssueExists { id: Iri, kind: Kind, issue: Iri },
}
```

In `crates/core/src/lib.rs`, after `pub use decision::{…};`, add:

```rust
pub use escalation::{EscalationFault, Escalations, Mark, Outgoing, Provenance, Tombstone};
```

In `crates/core/src/store.rs`, after `use crate::decision::{Decision, Flushed};`, add `use crate::escalation::EscalationFault;`, and in `enum StoreError`, after the `Routing(#[from] RoutingFault)` variant, add:

```rust
    /// ⚠ The item is marked escalating (routing spec §3.3 step 1, §3.6): the
    /// escalation is moving its one live copy to GitHub, so a local write
    /// could be lost. `to_finish` is `fl <kind> escalate <id>`.
    #[error(
        "{id} is marked escalating, so this store refuses to change it. Run `{to_finish}` to \
         finish the escalation, or `{to_finish} --abandon` to stop it"
    )]
    Escalating { id: Iri, to_finish: String },
    /// ⚠ The item was escalated (routing spec §3.6): the store holds only its
    /// tombstone, which the router follows to `to`.
    #[error(
        "{from} was escalated to GitHub and is now {to}. Read and change it there: this store \
         keeps only its tombstone"
    )]
    Escalated { from: Iri, to: Iri },
    /// ⚠ The escalation refused (routing spec §3.2, §4).
    #[error(transparent)]
    Escalation(#[from] EscalationFault),
```

In `crates/core/src/mem.rs`, before `use crate::finding::{Finding, FindingState};`, add:

```rust
use crate::escalation::{EscalationFault, Escalations, Mark, Tombstone, escalate_command};
```

In `struct Inner`, after the `segments` field, add:

```rust
    /// primary → the mark of an escalation under way (routing spec §3.3).
    escalating: BTreeMap<Iri, Mark>,
    /// primary → the tombstone of an escalated item. The item's row, handle
    /// and aliases stay: its id and handle still resolve here (§2.3).
    tombstones: BTreeMap<Iri, Tombstone>,
```

In `impl Inner`, after `fn resolve`, add:

```rust
    /// `check`, then the primary `id` names and its kind — unless the item
    /// was escalated: a tombstoned item reads as `Escalated`, under every
    /// name it has (routing spec §3.6).
    fn live(&self, id: &Iri) -> Result<(Iri, Kind), StoreError> {
        let kind = self.check(id)?;
        let primary = self.resolve(id);
        if let Some(t) = self.tombstones.get(&primary) {
            return Err(StoreError::Escalated {
                from: primary,
                to: t.to.clone(),
            });
        }
        Ok((primary, kind))
    }

    /// `live`, and then refuse an item marked escalating: a write would
    /// change the copy the escalation is moving (routing spec §3.3 step 1).
    fn writable(&self, id: &Iri) -> Result<Iri, StoreError> {
        let (primary, kind) = self.live(id)?;
        if self.escalating.contains_key(&primary) {
            return Err(StoreError::Escalating {
                to_finish: escalate_command(kind, &primary),
                id: primary,
            });
        }
        Ok(primary)
    }
```

In `impl Tracker for MemStore`, replace `get_record`, `list_records`, `set_record_state`, `get_finding`, `list_findings` and `withdrawals_by` with:

```rust
    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        let s = self.inner.borrow();
        let (target, _) = s.live(&id.0)?;
        Ok(s.records.get(&target).cloned())
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.records
            .values()
            .filter(|r| r.project == *project && !s.tombstones.contains_key(r.id.iri()))
            .cloned()
            .collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        // `id` may be an alias: resolve to the primary key `records` is
        // actually keyed by.
        let target = s.writable(&id.0)?;
        let rec = s
            .records
            .get_mut(&target)
            .ok_or_else(|| StoreError::NoSuchRecord(id.clone()))?;
        rec.state = state;
        Ok(())
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        let s = self.inner.borrow();
        let (target, _) = s.live(&id.0)?;
        Ok(s.findings.get(&target).cloned())
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.findings
            .values()
            .filter(|f| f.project == *project && !s.tombstones.contains_key(f.id.iri()))
            .cloned()
            .collect())
    }

    /// An escalated finding is counted by the tier it lives in now.
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        let s = self.inner.borrow();
        Ok(s.findings
            .values()
            .filter(|f| f.raised_by == actor && f.state == FindingState::Withdrawn)
            .filter(|f| !s.tombstones.contains_key(f.id.iri()))
            .count() as u64)
    }
```

In `add_finding`, replace the comment above `let record_primary = s.resolve(&finding.record.0);` and that line with:

```rust
        // `record` may have been given as an alias: resolve to the primary,
        // so two findings raised against the same record always agree on
        // which IRI names it. A marked record takes findings (routing spec
        // §3.3 step 1); an escalated one is `Escalated`.
        let (record_primary, _) = s.live(&finding.record.0)?;
```

In `update_finding`, delete `s.check(&finding.id.0)?;`, and replace `let target = s.resolve(&finding.id.0);` with:

```rust
        let target = s.writable(&finding.id.0)?;
```

In `add_alias`, replace `let resolved = s.resolve(primary);` with:

```rust
        let resolved = s.writable(primary)?;
```

After `impl Tracker for MemStore` (before `impl Ledger for MemStore`), add:

```rust
impl Escalations for MemStore {
    fn mark(&self, id: &Iri, mark: &Mark) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        let (primary, kind) = s.live(id)?;
        if !matches!(kind, Kind::Record | Kind::Finding) {
            return Err(StoreError::WrongKind {
                id: id.clone(),
                expected: Kind::Record,
                found: kind,
            });
        }
        if s.escalating.contains_key(&primary) {
            return Err(EscalationFault::AlreadyMarked { id: primary, kind }.into());
        }
        s.escalating.insert(primary, mark.clone());
        Ok(())
    }

    fn mark_of(&self, id: &Iri) -> Result<Option<Mark>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.escalating.get(&s.resolve(id)).cloned())
    }

    fn unmark(&self, id: &Iri) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check(id)?;
        let primary = s.resolve(id);
        if s.escalating.remove(&primary).is_none() {
            return Err(EscalationFault::NotMarked { id: primary }.into());
        }
        Ok(())
    }

    fn tombstone(&self, id: &Iri, to: &Iri) -> Result<Tombstone, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check(id)?;
        let primary = s.resolve(id);
        let Some(mark) = s.escalating.remove(&primary) else {
            return Err(EscalationFault::NotMarked { id: primary }.into());
        };
        let tomb = Tombstone {
            from: primary.clone(),
            to: to.clone(),
            by: mark.by,
            reason: mark.reason,
            at_ms: mark.at_ms,
        };
        s.tombstones.insert(primary, tomb.clone());
        Ok(tomb)
    }

    fn tombstone_of(&self, id: &Iri) -> Result<Option<Tombstone>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.tombstones.get(&s.resolve(id)).cloned())
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-core --lib`
Expected: PASS — among them `escalation::tests::`, `store::tests::only_an_unreachable_a_rate_limited_or_a_contended_ledger_is_transient`, `mem::tests::mem_store_meets_the_escalation_contract`, `mem::tests::a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed` and `mem::tests::an_escalated_items_handle_still_resolves_to_its_id`.

- [ ] **Step 5: Mutation checks**

The suite runs its cases in order and stops at the first that fails; the tombstoned case runs before the marked one, so a mutation that drops both refusals from one write method fails in the tombstoned case, and one that drops only the mark fails in the marked case. "Suite" below is `cargo test -p fl-core --lib mem::tests::mem_store_meets_the_escalation_contract`; "marked" is `cargo test -p fl-core --lib mem::tests::a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed`. Each names the case the suite's panic is in.

1. `live`'s tombstone check: never match (`.filter(|_| false)`) → suite red (`a_tombstone_is_made_from_the_mark_and_replaces_it`: a tombstoned item can be marked again).
2. `live`'s `Escalated` names the primary: write `from: id.clone()` → suite red (`an_alias_reaches_the_primary_in_every_escalation_method`).
3. `writable`'s mark check: `if false` → marked red.
4. `writable` names the item's kind: `escalate_command(Kind::Record, &primary)` → marked red (the finding's command).
5. `get_record` reads through `live`: `s.check(&id.0)?; let target = s.resolve(&id.0);` → suite red (`a_tombstoned_item_reads_as_escalated_and_refuses_every_write`).
6. `get_finding` the same → suite red (the tombstoned case).
7. `set_record_state` asks `writable`: `s.check(&id.0)?; let target = s.resolve(&id.0);` → suite red (the tombstoned case); `let target = s.live(&id.0)?.0;` → marked red.
8. `update_finding` asks `writable`: the same two mutations → suite red (the tombstoned case), marked red.
9. `add_alias` asks `writable`: `s.resolve(primary)` → suite red (the tombstoned case); `s.live(primary)?.0` → marked red.
10. `add_finding` asks `live` of the record: `s.resolve(&finding.record.0)` → suite red (the tombstoned case: a finding about an escalated record is written).
11. `add_finding` allows a marked record: ask `s.writable(&finding.record.0)?` instead → marked red.
12. `list_records` leaves tombstones out: drop `&& !s.tombstones.contains_key(r.id.iri())` → suite red (`a_tombstoned_item_is_left_out_of_every_list`).
13. `list_findings` the same → suite red (the lists case).
14. `withdrawals_by` the same: delete its tombstone `.filter(…)` → suite red (the lists case).
15. `mark`'s kind check: `if false` → suite red (`a_mark_reads_back_and_a_second_mark_is_refused`: a project is marked).
16. `mark`'s second-mark check: `if false` → suite red (the mark case).
17. `AlreadyMarked` carries the item's kind: `kind: Kind::Record` → suite red (the mark case, on the finding).
18. `mark` keys the mark by the primary: `s.escalating.insert(id.clone(), …)` → suite red (the alias case).
19. `mark_of` answers `None` for an id not held: ask `s.check(id)?` first → suite red (`an_id_this_store_never_held_has_no_mark_and_no_tombstone`).
20. `tombstone_of` the same → suite red (the not-held case).
21. `mark_of` resolves an alias: `s.escalating.get(id)` → suite red (the alias case); `tombstone_of`: `s.tombstones.get(id)` → suite red (the alias case).
22. `unmark` refuses no mark: `… .is_none() && false` → suite red (`unmarking_clears_the_mark_and_a_second_unmark_is_refused`).
23. `unmark` removes the mark: `s.escalating.get(&primary)` → suite red (the unmark case).
24. `unmark` refuses an id not held: delete `s.check(id)?;` → suite red (the not-held case: `NotMarked`, not `NotOwned`).
25. `tombstone` refuses no mark: take a default `Mark` when none → suite red (the tombstone case).
26. `tombstone` removes the mark: `s.escalating.get(&primary).cloned()` → suite red (the tombstone case: "the mark is gone").
27. `tombstone` copies the mark: `by: String::new()` → suite red (the tombstone case); `at_ms: 0` → the same.
28. `tombstone` names the primary: `from: id.clone()` → suite red (the alias case).
29. `tombstone` refuses an id not held: delete `s.check(id)?;` → suite red (the not-held case).
30. `Outgoing::title` of a finding is its claim: `&finding.raised_by` → `cargo test -p fl-core --lib escalation::tests::` red; `id` of a finding: `finding.record.iri()` → red; `kind`: `Kind::Record` → red.
31. `Mark` refuses unknown fields: delete its `#[serde(deny_unknown_fields)]` → `escalation::tests::` red; `Tombstone`'s → red.
32. `escalate_command` spells the kind on the wire: `format!("fl {:?} escalate {id}", kind)` → `escalation::tests::` red.
33. None of the three is transient: add `| StoreError::Escalating { .. }` to `is_transient` → `cargo test -p fl-core --lib store::tests::only_an_unreachable_a_rate_limited_or_a_contended_ledger_is_transient` red; `Escalated { .. }` → red; `Escalation(_)` → red.

Not observable:
- In `mark`, the tombstone refusal comes before the second-mark refusal; no item has both, because `tombstone` removes the mark in the same write, so the order cannot be told apart.
- `mark`'s `WrongKind` names the id as given rather than the primary: only a project or a gate reaches that arm, and `add_alias` refuses an alias for either, so the two are always the same id.
- `an_escalated_items_handle_still_resolves_to_its_id` and the tombstone case's `kind_of` assertion pin what this task leaves alone — the handle tables and `owned` are never touched by a tombstone — so they guard no line it adds.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1102 passed, 18 ignored.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/escalation.rs crates/core/src/lib.rs crates/core/src/store.rs crates/core/src/mem.rs crates/core/src/conformance.rs
git commit -m "feat(core): the escalation's mark and tombstone; MemStore refuses writes to either

fl-core gains escalation.rs: the mark, the tombstone, the Escalations
trait a local store implements, the item that goes out, its provenance,
and the eight refusals an escalation names. StoreError gains Escalating,
Escalated and Escalation. MemStore keeps marks and tombstones beside its
rows, keyed by the primary IRI: a marked item reads as itself and refuses
every write, naming the command that finishes it, while a finding about a
marked record is allowed; a tombstoned item reads as Escalated, refuses
every write and every new finding, and is left out of every list. The
shared escalation cases run over MemStore. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

---

### Task 2: The local store keeps marks and tombstones

An escalation marks the local item "escalating", and from then on the local store refuses writes to it, naming the command that finishes it; a finding raised against a marked record is allowed (routing spec §3.3 step 1). Once the issue exists, the item is replaced by a tombstone (§3.3 step 3) that every read of the id answers with (§3.6), and lists leave it out (§2.4); "running the command again resumes from where it stopped" (§3.3), so both are the store's, not the process's. Task 1 gave `MemStore` this behaviour and the shared conformance suite. This task gives `RedbStore` the same: two additive tables, `ESCALATING` and `TOMBSTONES`, keyed by the item's primary IRI (plan ruling 5), read as empty until their first write; the first mark raises a store below format 5 to 5 in the mark's own write transaction (plan ruling 6); the `Tracker` refusals are `MemStore`'s (plan ruling 7). No row, id, handle or alias is deleted: the id must still choose this store and the handle must still resolve (§2.3). redb opens the store for one process at a time (§2.7), and each write checks the mark and the tombstone inside its own write transaction, so a check holds when its write lands.

**Blast radius:** `fl-store` only. `set_record_state` and `update_finding` move from a read followed by a separate write to one write transaction (`rewrite`) that locates, checks and writes; `add_finding` now resolves its record inside the transaction that inserts the finding (`insert_new_in`); `locate`'s lookup moves into `locate_in`, shared with the write-side `locate_for_write`. Their results for every existing input are unchanged — every existing test passes unchanged. A read of `get_project`/`get_gate`, and every list, now also reads `TOMBSTONES`, which is absent (and so empty) in every store that never escalated. No `fl-core` signature changes.

**Files:**
- Modify: `crates/store/src/lib.rs` (`ESCALATING`, `TOMBSTONES`; `FORMAT_WITH_ROUTING`'s doc; `locate_in`, `json_at`, `additive`, `additive_in_write`, `refuse_escalated`, `refuse_unwritable`, `mint`; `insert_new_in`, `insert_in`, `locate_for_write`, `held`, `tombstoned`, `rewrite`; `get_json`'s tombstone check; the `Tracker` refusals and list filters; `impl Escalations for RedbStore`; tests)

**Interfaces:**
- Consumes (Task 1): `fl_core::escalation::{Escalations, Mark, Tombstone, EscalationFault, escalate_command}`; `StoreError::{Escalating, Escalated, Escalation}`; `fl_core::conformance::{escalations, EscalationFixture, Single, a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed}`.
- Produces: `impl Escalations for RedbStore`, with exactly `MemStore`'s semantics (the shared suite proves it). `RedbStore`'s `Tracker`: `get_record`/`get_finding` of a tombstoned id → `Escalated { from: <primary>, to }`; `set_record_state`, `update_finding`, `add_alias` → `Escalated` for a tombstoned item, `Escalating { id: <primary>, to_finish: escalate_command(kind, primary) }` for a marked one; `add_finding` about a tombstoned record → `Escalated`, about a marked record allowed; `list_records`, `list_findings`, `withdrawals_by` leave tombstoned items out. `handle_of`, `resolve_handle`, `owns` and `kind_of` still answer for an escalated item. No new public item; no new format constant.
- Unique phrases: none new. The tests match on `StoreError` variants and reuse Task 1's cases.

- [ ] **Step 1: Write the failing tests**

In `crates/store/src/lib.rs`, inside `mod tests`, after `redb_store_meets_every_role_contract`, add:

```rust
    #[test]
    fn redb_store_meets_the_escalation_contract() {
        use fl_core::conformance::Single;
        fl_core::conformance::escalations(|| {
            let (s, g) = fresh();
            Single(s, g)
        });
    }

    // Routing spec §3.3 step 1: the shared case, run on its own by name.
    #[test]
    fn a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed() {
        use fl_core::conformance::{EscalationFixture, Single};
        let (s, g) = fresh();
        Single(s, g).with_escalations(&mut |b| {
            fl_core::conformance::a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed(
                b,
            )
        });
    }
```

At the end of `mod tests` (after `the_local_store_keeps_a_checked_reference_to_a_github_record`), add:

```rust
    fn sample_mark() -> Mark {
        Mark {
            by: "alice".into(),
            reason: "needs a design review".into(),
            at_ms: 1_000,
        }
    }

    fn issue(n: u64) -> Iri {
        Iri::parse(&format!("https://github.com/acme/widgets/issues/{n}")).unwrap()
    }

    // Routing spec §3.3: "running the command again resumes from where it
    // stopped" — so the mark and the tombstone are the store's, and a new
    // process reads them back.
    #[test]
    fn marks_and_tombstones_survive_a_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let (r, f, tomb) = {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let r = s.add_record(&p, "t").unwrap();
            let f = s
                .add_finding(Finding::raise(p, r.clone(), "rev", "c"))
                .unwrap();
            s.mark(r.iri(), &sample_mark()).unwrap();
            s.mark(f.iri(), &sample_mark()).unwrap();
            let tomb = s.tombstone(f.iri(), &issue(7)).unwrap();
            (r, f, tomb)
        };
        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.mark_of(r.iri()).unwrap(), Some(sample_mark()));
        assert_eq!(s.tombstone_of(r.iri()).unwrap(), None);
        assert_eq!(s.tombstone_of(f.iri()).unwrap(), Some(tomb));
        assert_eq!(s.mark_of(f.iri()).unwrap(), None);
        let err = s.set_record_state(&r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Escalating { .. }), "{err:?}");
        let err = s.get_finding(&f).unwrap_err();
        assert!(matches!(err, StoreError::Escalated { .. }), "{err:?}");
    }

    // Routing spec §1.4, §3.6: an older fl would ignore a mark and let a
    // write through, so the first mark raises the store to 5. A store that
    // never marks stays where it was, and holds neither table: a write that
    // only looks for a mark or a tombstone creates nothing.
    #[test]
    fn the_first_mark_raises_the_store_to_format_5_and_a_store_that_never_marks_stays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.redb");
        let r = {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let r = s.add_record(&p, "t").unwrap();
            let f = s
                .add_finding(Finding::raise(p, r.clone(), "rev", "c"))
                .unwrap();
            s.set_record_state(&r, State::Doing).unwrap();
            let mut held = s.get_finding(&f).unwrap().unwrap();
            held.withdraw("not concrete").unwrap();
            s.update_finding(&held).unwrap();
            s.add_alias(r.iri(), issue(41)).unwrap();
            assert_eq!(s.mark_of(r.iri()).unwrap(), None);
            r
        };
        assert_eq!(format_at(&path), Some(FORMAT_VERSION));
        {
            let db = redb::Database::open(&path).unwrap();
            let tx = db.begin_read().unwrap();
            for table in [ESCALATING, TOMBSTONES] {
                assert!(
                    matches!(
                        tx.open_table(table),
                        Err(redb::TableError::TableDoesNotExist(_))
                    ),
                    "{} was created",
                    table.name()
                );
            }
        }
        RedbStore::open(&path)
            .unwrap()
            .mark(r.iri(), &sample_mark())
            .unwrap();
        assert_eq!(format_at(&path), Some(FORMAT_WITH_ROUTING));

        let path = dir.path().join("b.redb");
        let r = {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            s.set_ledger_root("node", "abc").unwrap();
            s.add_record(&p, "t").unwrap()
        };
        assert_eq!(format_at(&path), Some(FORMAT_WITH_LEDGER_ROOT));
        RedbStore::open(&path)
            .unwrap()
            .mark(r.iri(), &sample_mark())
            .unwrap();
        assert_eq!(format_at(&path), Some(FORMAT_WITH_ROUTING));
    }

    // Routing spec §2.3: a local handle of an escalated item resolves
    // through its tombstone, so its id, its handle and its row stay, and the
    // id still chooses this store.
    #[test]
    fn an_escalated_items_handle_still_resolves_and_the_store_still_owns_it() {
        let (s, _d) = fresh();
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        s.mark(r.iri(), &sample_mark()).unwrap();
        s.tombstone(r.iri(), &issue(7)).unwrap();
        assert_eq!(
            s.resolve_handle(Kind::Record, 1).unwrap(),
            Some(r.0.clone())
        );
        assert_eq!(s.handle_of(Kind::Record, r.iri()).unwrap(), Some(1));
        assert!(s.owns(r.iri()).unwrap());
        assert_eq!(s.kind_of(r.iri()).unwrap(), Kind::Record);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-store --lib`
Expected: FAIL to compile — `error[E0425]` (cannot find type `Mark`; cannot find value `ESCALATING`, `TOMBSTONES`), `error[E0277]` (the trait bound `RedbStore: Escalations` is not satisfied) and `error[E0599]` (no method named `mark`, `tombstone`, `mark_of` found for struct `RedbStore`).

- [ ] **Step 3: Implement**

In `crates/store/src/lib.rs`, before `use fl_core::finding::{Finding, FindingState};`, add:

```rust
use fl_core::escalation::{EscalationFault, Escalations, Mark, Tombstone, escalate_command};
```

Replace `use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};` with:

```rust
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableHandle};
use std::collections::BTreeSet;
```

Replace `FORMAT_WITH_ROUTING`'s doc comment, keeping the constant, so the two read:

```rust
/// ⚠ The format of a store that holds a routing map, an item with an area,
/// or an escalation's mark or tombstone (routing spec §1.4, §3.6). An older
/// fl would read such an item and drop its area, route nothing, or ignore a
/// mark and write to the item it guards — so the first such write raises
/// the store to 5 in the same transaction, and an older fl refuses it. A
/// tombstone is written only over a mark, so its store is 5 already. This
/// build opens 2 to 5. A store that never routes stays where it was.
pub const FORMAT_WITH_ROUTING: u64 = 5;
```

After `const ROUTING: …` (before `const NEXT_RUN`), add:

```rust
/// primary IRI → the mark of an escalation under way, as JSON (`Mark`;
/// routing spec §3.3 step 1). Created by the first mark, which raises the
/// store to [`FORMAT_WITH_ROUTING`] in the same transaction. While a mark
/// stands, every write to its item is refused.
const ESCALATING: TableDefinition<&str, &str> = TableDefinition::new("escalating");
/// primary IRI → the tombstone of an escalated item, as JSON (`Tombstone`;
/// routing spec §3.3 step 3). Created by the first tombstone. The item's
/// row, id, handle and aliases stay: its id still chooses this store, and
/// its handle still resolves (§2.3).
const TOMBSTONES: TableDefinition<&str, &str> = TableDefinition::new("tombstones");
```

After `fn alias_primary` (its closing brace), add:

```rust
/// The primary id and kind behind `id`, read from `ids` — `IDS`, open in
/// any transaction — following one alias hop through the `ALIASES` table
/// `aliases` opens in the same one. `"alias"` is an index marker in `IDS`,
/// not a `Kind`, so it is handled here before `Kind::from_wire` sees it.
fn locate_in<A: ReadableTable<&'static str, &'static str>>(
    label: &str,
    id: &Iri,
    ids: &impl ReadableTable<&'static str, &'static str>,
    aliases: impl FnOnce() -> Result<A, StoreError>,
) -> Result<(Iri, Kind), StoreError> {
    let Some(v) = ids.get(id.as_str()).map_err(backend)? else {
        return Err(StoreError::NotOwned {
            id: id.clone(),
            searched: vec![label.to_string()],
        });
    };
    let (primary, wire) = if v.value() == "alias" {
        alias_primary(id, ids, &aliases()?)?
    } else {
        (id.clone(), v.value().to_string())
    };
    let kind = Kind::from_wire(&wire)
        .ok_or_else(|| decode(format!("unknown kind `{wire}` for {primary}")))?;
    Ok((primary, kind))
}

/// The JSON row `key` names in `table`, if it has one.
fn json_at<T: serde::de::DeserializeOwned>(
    table: &impl ReadableTable<&'static str, &'static str>,
    key: &Iri,
) -> Result<Option<T>, StoreError> {
    match table.get(key.as_str()).map_err(backend)? {
        Some(v) => Ok(Some(serde_json::from_str(v.value()).map_err(decode)?)),
        None => Ok(None),
    }
}

/// The JSON row `key` names in an additive `table`, read inside `tx`. A
/// table no write has created yet holds nothing.
fn additive<T: serde::de::DeserializeOwned>(
    tx: &redb::ReadTransaction,
    table: TableDefinition<&str, &str>,
    key: &Iri,
) -> Result<Option<T>, StoreError> {
    match tx.open_table(table) {
        Ok(t) => json_at(&t, key),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(backend(e)),
    }
}

/// [`additive`], inside a write transaction. ⚠ A write transaction's
/// `open_table` creates the table, so this asks whether it exists first: a
/// write that only looks for a mark leaves a store that never marked
/// without the table.
fn additive_in_write<T: serde::de::DeserializeOwned>(
    tx: &redb::WriteTransaction,
    table: TableDefinition<&str, &str>,
    key: &Iri,
) -> Result<Option<T>, StoreError> {
    let exists = tx
        .list_tables()
        .map_err(backend)?
        .any(|t| t.name() == table.name());
    if !exists {
        return Ok(None);
    }
    json_at(&tx.open_table(table).map_err(backend)?, key)
}

/// `Escalated` when `primary` has a tombstone, read inside the write
/// transaction `tx` that is about to write (routing spec §3.6).
fn refuse_escalated(tx: &redb::WriteTransaction, primary: &Iri) -> Result<(), StoreError> {
    if let Some(t) = additive_in_write::<Tombstone>(tx, TOMBSTONES, primary)? {
        return Err(StoreError::Escalated {
            from: primary.clone(),
            to: t.to,
        });
    }
    Ok(())
}

/// Refuse a write to `primary`, a `kind`, inside the write transaction `tx`
/// that would make it: a tombstoned item is `Escalated`, and a marked one is
/// `Escalating` — a write would change the copy the escalation is moving
/// (routing spec §3.3 step 1, §3.6).
fn refuse_unwritable(
    tx: &redb::WriteTransaction,
    primary: &Iri,
    kind: Kind,
) -> Result<(), StoreError> {
    refuse_escalated(tx, primary)?;
    if additive_in_write::<Mark>(tx, ESCALATING, primary)?.is_some() {
        return Err(StoreError::Escalating {
            id: primary.clone(),
            to_finish: escalate_command(kind, primary),
        });
    }
    Ok(())
}

/// A new item's id.
fn mint() -> Result<Iri, StoreError> {
    Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7())).map_err(backend)
}
```

Replace `fn insert_new` with the two functions below (`insert_new` now takes its id from `mint`):

```rust
    fn insert_new<T: serde::Serialize>(
        &self,
        kind: Kind,
        table: TableDefinition<&str, &str>,
        raise: Option<u64>,
        build: impl FnOnce(Iri) -> T,
    ) -> Result<Iri, StoreError> {
        self.insert_new_with_id(mint()?, kind, table, raise, build)
    }

    /// `insert_new`, with `build` given the write transaction: what the new
    /// row depends on is read — and refused — in the write that adds it.
    fn insert_new_in<T: serde::Serialize>(
        &self,
        kind: Kind,
        table: TableDefinition<&str, &str>,
        raise: Option<u64>,
        build: impl FnOnce(&redb::WriteTransaction, Iri) -> Result<T, StoreError>,
    ) -> Result<Iri, StoreError> {
        self.insert_in(mint()?, kind, table, raise, build)
    }
```

Replace `insert_new_with_id`'s signature and body — keep its doc comment, which still describes the one transaction — up to and including the line `index_new(&tx, &id, kind)?;` with the following; the rest of the old body (the row insert, the format raise, the commit) is `insert_in`'s, unchanged:

```rust
    pub(crate) fn insert_new_with_id<T: serde::Serialize>(
        &self,
        id: Iri,
        kind: Kind,
        table: TableDefinition<&str, &str>,
        raise: Option<u64>,
        build: impl FnOnce(Iri) -> T,
    ) -> Result<Iri, StoreError> {
        self.insert_in(id, kind, table, raise, |_, id| Ok(build(id)))
    }

    /// `insert_new_with_id`'s one write transaction, with `build` run inside
    /// it: an error from `build` drops the transaction like any other.
    fn insert_in<T: serde::Serialize>(
        &self,
        id: Iri,
        kind: Kind,
        table: TableDefinition<&str, &str>,
        raise: Option<u64>,
        build: impl FnOnce(&redb::WriteTransaction, Iri) -> Result<T, StoreError>,
    ) -> Result<Iri, StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        let json = serde_json::to_string(&build(&tx, id.clone())?).map_err(backend)?;
        index_new(&tx, &id, kind)?;
```

Replace `fn locate` (keep its doc comment) with `locate` and the four helpers after it:

```rust
    fn locate(&self, id: &Iri) -> Result<(Iri, Kind), StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let ids = tx.open_table(IDS).map_err(backend)?;
        locate_in(&self.label, id, &ids, || {
            tx.open_table(ALIASES).map_err(backend)
        })
    }

    /// [`Self::locate`], inside the write transaction `tx` that is about to
    /// write: a check made here holds when the write lands.
    fn locate_for_write(
        &self,
        tx: &redb::WriteTransaction,
        id: &Iri,
    ) -> Result<(Iri, Kind), StoreError> {
        let ids = tx.open_table(IDS).map_err(backend)?;
        locate_in(&self.label, id, &ids, || {
            tx.open_table(ALIASES).map_err(backend)
        })
    }

    /// The primary `id` names, or `None` when this store does not hold it.
    fn held(&self, id: &Iri) -> Result<Option<Iri>, StoreError> {
        match self.locate(id) {
            Ok((primary, _)) => Ok(Some(primary)),
            Err(StoreError::NotOwned { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The primary IRIs this store holds a tombstone for: what every list
    /// leaves out (routing spec §2.4).
    fn tombstoned(&self) -> Result<BTreeSet<String>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(TOMBSTONES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeSet::new()),
            Err(e) => return Err(backend(e)),
        };
        let mut out = BTreeSet::new();
        for entry in table.iter().map_err(backend)? {
            let (k, _) = entry.map_err(backend)?;
            out.insert(k.value().to_string());
        }
        Ok(out)
    }

    /// Change the row `id` names in `table` with `change` — in ONE write
    /// transaction that first refuses an escalated or escalating item
    /// (routing spec §3.3 step 1, §3.6), so nothing lands between the check
    /// and the write. The row is keyed by the primary, whatever name `id`
    /// is. `false`, writing nothing, when `table` holds no row for it: an id
    /// held under another kind.
    fn rewrite<T: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        table: TableDefinition<&str, &str>,
        id: &Iri,
        change: impl FnOnce(T) -> T,
    ) -> Result<bool, StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        let (primary, kind) = self.locate_for_write(&tx, id)?;
        refuse_unwritable(&tx, &primary, kind)?;
        {
            let mut t = tx.open_table(table).map_err(backend)?;
            let Some(old) = json_at::<T>(&t, &primary)? else {
                return Ok(false);
            };
            let json = serde_json::to_string(&change(old)).map_err(backend)?;
            t.insert(primary.as_str(), json.as_str()).map_err(backend)?;
        }
        tx.commit().map_err(backend)?;
        Ok(true)
    }
```

In `get_json`, after `let tx = self.db.begin_read().map_err(backend)?;`, add:

```rust
        // An escalated item reads as `Escalated`, under every name it has
        // (routing spec §3.6).
        if let Some(t) = additive::<Tombstone>(&tx, TOMBSTONES, &target)? {
            return Err(StoreError::Escalated {
                from: target,
                to: t.to,
            });
        }
```

In `impl Tracker for RedbStore`, replace `list_records` and `set_record_state` with:

```rust
    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let all: Vec<Record> = self.all_json(RECORDS)?;
        let gone = self.tombstoned()?;
        Ok(all
            .into_iter()
            .filter(|r| r.project == *project && !gone.contains(r.id.iri().as_str()))
            .collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        // `rewrite` writes under the primary, not `id`: `id` may be an
        // alias, and writing under an alias key would leave a stray row
        // behind instead of updating the one that exists.
        let changed = self.rewrite(RECORDS, id.iri(), |mut rec: Record| {
            rec.state = state;
            rec
        })?;
        if !changed {
            return Err(StoreError::NoSuchRecord(id.clone()));
        }
        Ok(())
    }
```

Replace `add_finding` with:

```rust
    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        self.check_kind(finding.project.iri(), Kind::Project)?;
        let raise = finding.area.as_ref().map(|_| FORMAT_WITH_ROUTING);
        let id = self.insert_new_in(Kind::Finding, FINDINGS, raise, |tx, id| {
            // `record` may be given as an alias (e.g. the CLI stores
            // whatever the caller typed): resolve to the primary, so two
            // findings raised against the same record always agree on which
            // IRI names it.
            let (record_primary, record_kind) = self.locate_for_write(tx, finding.record.iri())?;
            if record_kind != Kind::Record {
                return Err(StoreError::WrongKind {
                    id: finding.record.iri().clone(),
                    expected: Kind::Record,
                    found: record_kind,
                });
            }
            // A marked record takes findings (routing spec §3.3 step 1); an
            // escalated one is `Escalated`, which the router follows.
            refuse_escalated(tx, &record_primary)?;
            let mut finding = finding;
            finding.id = FindingId(id);
            finding.record = RecordId(record_primary);
            Ok(finding)
        })?;
        Ok(FindingId(id))
    }
```

Replace `update_finding`, `list_findings` and `withdrawals_by` with:

```rust
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        let changed = self.rewrite(FINDINGS, finding.id.iri(), |kept: Finding| {
            // `kept.id` is always the primary: the row is keyed by it. Take
            // every other field from the caller's version, but keep the id
            // pinned to the primary — even if `finding.id` (what the caller
            // passed) is an alias — so an update through an alias still
            // lands on, and stays keyed by, the primary, rather than writing
            // a second row under the alias.
            //
            // The stored `also_known_as` is kept and the caller's ignored
            // (see the trait): only `add_alias` adds a name.
            let mut stored = finding.clone();
            stored.id = kept.id;
            stored.also_known_as = kept.also_known_as;
            // Fixed when the finding is raised (routing spec §1.1; GitHub
            // tracker spec §6): the caller's copy never changes them.
            stored.record = kept.record;
            stored.raised_by = kept.raised_by;
            stored.security = kept.security;
            stored.area = kept.area;
            stored
        })?;
        if !changed {
            return Err(StoreError::NoSuchFinding(finding.id.clone()));
        }
        Ok(())
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let all: Vec<Finding> = self.all_json(FINDINGS)?;
        let gone = self.tombstoned()?;
        Ok(all
            .into_iter()
            .filter(|f| f.project == *project && !gone.contains(f.id.iri().as_str()))
            .collect())
    }

    /// An escalated finding is counted by the tier it lives in now.
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        let all: Vec<Finding> = self.all_json(FINDINGS)?;
        let gone = self.tombstoned()?;
        Ok(all
            .into_iter()
            .filter(|f| f.raised_by == actor && f.state == FindingState::Withdrawn)
            .filter(|f| !gone.contains(f.id.iri().as_str()))
            .count() as u64)
    }
```

In `add_alias`, after the block that computes `let (resolved, kind) = { … };`, add:

```rust
        // An alias is a write to the item it names (routing spec §3.3 step
        // 1, §3.6), checked in this transaction.
        refuse_unwritable(&tx, &resolved, kind)?;
```

After `impl Tracker for RedbStore` (before `impl Ledger for RedbStore`), add:

```rust
/// Each method is one write transaction, or one read: a check made in it
/// holds when its write lands (routing spec §2.7: the store is opened by
/// one process at a time).
impl Escalations for RedbStore {
    fn mark(&self, id: &Iri, mark: &Mark) -> Result<(), StoreError> {
        let json = serde_json::to_string(mark).map_err(backend)?;
        let tx = self.db.begin_write().map_err(backend)?;
        let (primary, kind) = self.locate_for_write(&tx, id)?;
        refuse_escalated(&tx, &primary)?;
        if !matches!(kind, Kind::Record | Kind::Finding) {
            return Err(StoreError::WrongKind {
                id: id.clone(),
                expected: Kind::Record,
                found: kind,
            });
        }
        if additive_in_write::<Mark>(&tx, ESCALATING, &primary)?.is_some() {
            return Err(EscalationFault::AlreadyMarked { id: primary, kind }.into());
        }
        tx.open_table(ESCALATING)
            .map_err(backend)?
            .insert(primary.as_str(), json.as_str())
            .map_err(backend)?;
        raise_format(&tx, FORMAT_WITH_ROUTING)?;
        tx.commit().map_err(backend)
    }

    fn mark_of(&self, id: &Iri) -> Result<Option<Mark>, StoreError> {
        let Some(primary) = self.held(id)? else {
            return Ok(None);
        };
        let tx = self.db.begin_read().map_err(backend)?;
        additive(&tx, ESCALATING, &primary)
    }

    fn unmark(&self, id: &Iri) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        let (primary, _) = self.locate_for_write(&tx, id)?;
        let removed = tx
            .open_table(ESCALATING)
            .map_err(backend)?
            .remove(primary.as_str())
            .map_err(backend)?
            .is_some();
        if !removed {
            return Err(EscalationFault::NotMarked { id: primary }.into());
        }
        tx.commit().map_err(backend)
    }

    fn tombstone(&self, id: &Iri, to: &Iri) -> Result<Tombstone, StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        let (primary, _) = self.locate_for_write(&tx, id)?;
        // The mark is removed and the tombstone written in this one
        // transaction: the item is never both, and never neither.
        let mark: Mark = {
            let mut t = tx.open_table(ESCALATING).map_err(backend)?;
            let Some(v) = t.remove(primary.as_str()).map_err(backend)? else {
                return Err(EscalationFault::NotMarked { id: primary }.into());
            };
            serde_json::from_str(v.value()).map_err(decode)?
        };
        let tomb = Tombstone {
            from: primary.clone(),
            to: to.clone(),
            by: mark.by,
            reason: mark.reason,
            at_ms: mark.at_ms,
        };
        let json = serde_json::to_string(&tomb).map_err(backend)?;
        tx.open_table(TOMBSTONES)
            .map_err(backend)?
            .insert(primary.as_str(), json.as_str())
            .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(tomb)
    }

    fn tombstone_of(&self, id: &Iri) -> Result<Option<Tombstone>, StoreError> {
        let Some(primary) = self.held(id)? else {
            return Ok(None);
        };
        let tx = self.db.begin_read().map_err(backend)?;
        additive(&tx, TOMBSTONES, &primary)
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-store --lib`
Expected: PASS — 77 passed, among them `tests::redb_store_meets_the_escalation_contract`, `tests::a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed`, `tests::marks_and_tombstones_survive_a_close_and_reopen`, `tests::the_first_mark_raises_the_store_to_format_5_and_a_store_that_never_marks_stays` and `tests::an_escalated_items_handle_still_resolves_and_the_store_still_owns_it`.

- [ ] **Step 5: Mutation checks**

"Suite" is `cargo test -p fl-store --lib tests::redb_store_meets_the_escalation_contract`; "marked" is `cargo test -p fl-store --lib tests::a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed`; "format" is `cargo test -p fl-store --lib tests::the_first_mark_raises_the_store_to_format_5_and_a_store_that_never_marks_stays`. The suite stops at its first failing case, which each line names.

1. `get_json`'s tombstone check: `additive::<Tombstone>(&tx, TOMBSTONES, &target)?.filter(|_| false)` → suite red (`a_tombstoned_item_reads_as_escalated_and_refuses_every_write`: `get_record` answers the row).
2. `get_json`'s `Escalated` names the primary: `from: key.clone()` → suite red (`an_alias_reaches_the_primary_in_every_escalation_method`).
3. `refuse_unwritable`'s mark check: `if false && additive_in_write::<Mark>(…)` → marked red.
4. `refuse_unwritable` names the item's kind: `escalate_command(Kind::Record, primary)` → marked red (the finding's command).
5. `refuse_unwritable` asks for the tombstone: delete `refuse_escalated(tx, primary)?;` → suite red (the tombstoned case: `set_record_state` writes through).
6. `rewrite` refuses before it writes: delete `refuse_unwritable(&tx, &primary, kind)?;` → suite red (the tombstoned case); replace it with `refuse_escalated(&tx, &primary)?;` → marked red. `set_record_state` and `update_finding` both write only through `rewrite`.
7. `add_alias` checks its item: delete `refuse_unwritable(&tx, &resolved, kind)?;` → suite red (the tombstoned case); replace it with `refuse_escalated(&tx, &resolved)?;` → marked red.
8. `add_finding` refuses a tombstoned record: delete `refuse_escalated(tx, &record_primary)?;` → suite red (the tombstoned case: a finding about an escalated record is written).
9. `add_finding` allows a marked record: replace that line with `refuse_unwritable(tx, &record_primary, Kind::Record)?;` → marked red.
10. `list_records` leaves tombstones out: `.filter(|r| r.project == *project)` → suite red (`a_tombstoned_item_is_left_out_of_every_list`).
11. `list_findings` the same → suite red (the lists case).
12. `withdrawals_by` the same: delete its `gone` `.filter(…)` → suite red (the lists case).
13. `mark`'s kind check: `if false` → suite red (`a_mark_reads_back_and_a_second_mark_is_refused`: a project is marked).
14. `mark`'s second-mark check: `if false` → suite red (the mark case).
15. `AlreadyMarked` carries the item's kind: `kind: Kind::Record` → suite red (the mark case, on the finding).
16. `mark` refuses a tombstoned item: delete `refuse_escalated(&tx, &primary)?;` → suite red (`a_tombstone_is_made_from_the_mark_and_replaces_it`: a tombstoned item can be marked again).
17. `mark` keys the mark by the primary: `.insert(id.as_str(), json.as_str())` → suite red (the alias case).
18. `mark` raises the format: delete `raise_format(&tx, FORMAT_WITH_ROUTING)?;` → format red.
19. `mark_of` answers `None` for an id not held: `let primary = self.locate(id)?.0;` → suite red (`an_id_this_store_never_held_has_no_mark_and_no_tombstone`); `tombstone_of` the same → suite red (the not-held case).
20. `mark_of` resolves an alias: `additive(&tx, ESCALATING, id)` → suite red (the alias case); `tombstone_of`: `additive(&tx, TOMBSTONES, id)` → suite red (the alias case).
21. `unmark` refuses no mark: `if !removed && false` → suite red (`unmarking_clears_the_mark_and_a_second_unmark_is_refused`).
22. `unmark` removes the mark: `.get(primary.as_str())` for `.remove(…)` → suite red (the unmark case).
23. `unmark` refuses an id not held: `let primary = id.clone();` for the `locate_for_write` line → suite red (the not-held case: `NotMarked`, not `NotOwned`).
24. `tombstone` removes the mark in its own transaction: `t.get(primary.as_str())` for `t.remove(…)` → suite red (the tombstone case: "the mark is gone").
25. `tombstone` refuses no mark: return a `Tombstone` with an empty mark in the `else` arm → suite red (the tombstone case).
26. `tombstone` copies the mark: `by: String::new()` → suite red (the tombstone case); `at_ms: 0` → the same.
27. `tombstone` names the primary: `from: id.clone()` → suite red (the alias case).
28. `tombstone` refuses an id not held: `let primary = id.clone();` for the `locate_for_write` line → suite red (the not-held case).
29. `additive_in_write` leaves a missing table uncreated: `if !exists && false` → format red ("escalating was created").
30. `additive` reads a missing table as empty: delete its `TableDoesNotExist` arm → suite red (the mark case: `mark_of` on a fresh store); `tombstoned`'s → suite red (the marked case: `list_records` on a store with no tombstone).

Not observable:
- That `tombstone` writes the tombstone and removes the mark in ONE transaction rather than two: no input makes a write fail between the two, and redb gives a test no fault to inject. What is observable — a tombstone that leaves the mark behind — is mutation 24.
- That `mark` raises the format in the mark's own transaction rather than a second one, for the same reason; the raise itself is mutation 18.
- `refuse_unwritable` asks for the tombstone before the mark: no item has both, because `tombstone` removes the mark in the same write.
- `mark`'s order (tombstone, kind, second mark) and its `WrongKind` naming the id as given are `MemStore`'s, for the reasons Task 1 gives.
- `get_json`'s tombstone check also runs for `get_project` and `get_gate`: neither ever has a tombstone, since `mark` refuses both.
- `marks_and_tombstones_survive_a_close_and_reopen` and `an_escalated_items_handle_still_resolves_and_the_store_still_owns_it` pin what the store keeps — redb's durability, and the handle tables and `IDS` that no escalation method touches — so they guard no line this task adds.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1107 passed, 18 ignored.

- [ ] **Step 7: Commit**

```bash
git add crates/store/src/lib.rs
git commit -m "feat(store): the local store keeps marks and tombstones

RedbStore gains two additive tables, escalating and tombstones, keyed by
the item's primary IRI and read as empty until their first write; the
first mark raises the store to format 5 in the same transaction. It
implements Escalations exactly as MemStore does and meets the shared
escalation cases: a marked item reads as itself and refuses every write,
naming the command that finishes it, while a finding about a marked
record is allowed; a tombstoned item reads as Escalated, refuses every
write and every new finding, and is left out of every list. Each write
checks the mark and the tombstone inside its own write transaction, and
the tombstone replaces the mark in one. No row, id, handle or alias is
deleted. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

---

### Task 3: An escalated issue's block carries its provenance (`fl_format` 3)

An escalated issue's text names who escalated it, why, and the old IRI (routing spec §3.3 step 2). A finding's issue text is its claim, so a paragraph in the prose would become part of the claim, and `Meta` refuses a field it does not know (spec defect 2). This task gives the block a field of its own, `escalated: { from, by, reason }` (plan ruling 2), raises a block that carries it to `fl_format` 3 so an fl built before it says "upgrade fl" rather than "damaged" (plan ruling 3, routing spec decision 14's reasoning), and shows it as a line rendered from the block — `Escalated from the local tier by <by>: <reason>. Its local IRI was <from>.`, each part escaped — after the prose and after the local-record line, stripped on read exactly as that line is, so it survives every rewrite of either kind and a claim reads back as the claim. Nothing writes the field yet: Task 4's `create_escalated` does.

**Blast radius:** `Meta` gains a field. Every `Meta` in the workspace is built by `Meta::new` (`finding_meta`, `add_record_with_area`, the creates in `tracker.rs`), which sets it `None`; there is no other struct literal. The field is skipped when absent, so a block without it is written byte for byte as before — a test pins a format-1 and a format-2 body both ways. `render_body` and `parse_body` serve every GitHub tracker read and write, the ledger's comment reader (`ledger/comment.rs`) and the tests in `tracker.rs` and `crates/cli/tests/routing.rs` that rewrite a body; for a block without `escalated` their output is unchanged (`render_body`'s join of the non-empty parts reproduces the old `match` exactly). `parse_body` now reads a format-3 block instead of refusing it, and `BodyError::UnknownFormat` says "formats 1 to 3"; no assertion outside `meta.rs` names either. Changed on purpose: in `meta::tests::a_body_that_is_missing_damaged_or_newer_is_named`, the newer-format body is format 4, not 3 (`"\"fl_format\":4"`, `Err(BodyError::UnknownFormat(4))`), and the message assertion is `contains("formats 1 to 3") && contains("upgrade fl")`, where it was `contains("formats 1 to 2")`.

**Files:**
- Modify: `crates/github/src/meta.rs` (`FL_FORMAT_ESCALATED`, `EscalatedFrom`, `Meta.escalated`, `required_format`, `escalation_line`, `render_body`, `BodyError`'s message, `parse_body`; tests)
- Modify: `docs/github-tracker.md` ("What an issue looks like": the escalation's field, its line and format 3)

**Interfaces:**
- Consumes: `crate::ledger::render::escape`; `record_line`, `Meta::sealed`, `RecordRef::is_local` (plan A).
- Produces: `pub const FL_FORMAT_ESCALATED: u64 = 3;`; `#[serde(deny_unknown_fields)] pub struct EscalatedFrom { pub from: Iri, pub by: String, pub reason: String }` (`Debug, Clone, PartialEq, Eq, Serialize, Deserialize`); `Meta` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub escalated: Option<EscalatedFrom>` (on the wire between `also_known_as` and `create_key`); `Meta::required_format` is 3 when `escalated` is set, whatever else the block carries, else as before; `pub fn escalation_line(meta: &Meta) -> Option<String>`; `render_body` writes prose, record line, escalation line, block, a blank line between each part present; `parse_body` reads formats 1 to 3 and strips both lines. Task 4 fills the field from a `Provenance`: `EscalatedFrom { from: p.from.clone(), by: p.by.clone(), reason: p.reason.clone() }`.
- Unique phrases: `Escalated from the local tier by`, `Its local IRI was` (the line), `formats 1 to 3` (`UnknownFormat`).

- [ ] **Step 1: Write the failing tests**

In `crates/github/src/meta.rs`, inside `mod tests`, in `fn a_body_that_is_missing_damaged_or_newer_is_named`, replace the last two assertions (the `UnknownFormat(3)` one and the `"formats 1 to 2"` one) with:

```rust
        assert_eq!(
            parse_body(&good.replace("\"fl_format\":1", "\"fl_format\":4")),
            Err(BodyError::UnknownFormat(4))
        );
        let said = BodyError::UnknownFormat(4).to_string();
        assert!(
            said.contains("formats 1 to 3") && said.contains("upgrade fl"),
            "{said}"
        );
```

In the same `mod tests`, after `fn an_area_label_that_is_missing_or_differs_from_the_block_is_diverged` (before `fn only_a_github_issue_url_parses`), add:

```rust
    fn escalated_from(by: &str, reason: &str) -> EscalatedFrom {
        EscalatedFrom {
            from: Iri::parse("urn:uuid:00000000-0000-7000-8000-000000000007").unwrap(),
            by: by.into(),
            reason: reason.into(),
        }
    }

    const ESCALATED_LINE: &str = "Escalated from the local tier by alice: it needs a person. \
                                  Its local IRI was urn:uuid:00000000-0000-7000-8000-000000000007.";

    // Routing spec §3.3: a block that carries its escalation is format 3,
    // whatever else it carries — an fl that reads formats 1 and 2 refuses it
    // as newer rather than reading it as damaged.
    #[test]
    fn the_block_is_format_3_exactly_when_it_carries_its_escalation() {
        let format = |m: &Meta| parse_body(&render_body("p", m)).unwrap().1.fl_format;
        for area in [None, Some("code")] {
            for record in [None, Some(local_ref("t"))] {
                let mut m = meta(ItemKind::Finding, "raised");
                m.area = area.map(str::to_string);
                m.record = record.clone();
                let before = if area.is_some() || record.is_some() {
                    2
                } else {
                    1
                };
                assert_eq!(format(&m), before, "{area:?} {record:?}");
                m.escalated = Some(escalated_from("alice", "it needs a person"));
                m.fl_format = 2;
                assert_eq!(format(&m), 3, "{area:?} {record:?}");
            }
        }
        let mut m = meta(ItemKind::Record, "needs_human");
        m.escalated = Some(escalated_from("alice", "it needs a person"));
        let body = render_body("", &m);
        assert!(
            body.contains(
                "\"escalated\":{\"from\":\"urn:uuid:00000000-0000-7000-8000-000000000007\",\
                 \"by\":\"alice\",\"reason\":\"it needs a person\"}"
            ),
            "{body}"
        );
        assert!(body.contains("\"fl_format\":3"), "{body}");
        assert!(
            !render_body("", &meta(ItemKind::Record, "todo")).contains("escalated"),
            "skipped when absent"
        );
    }

    // Routing spec §3.3 step 2: an escalated record's issue names who
    // escalated it, why, and its old IRI — after its prose, as a line fl
    // writes from the block and reads back out of the prose.
    #[test]
    fn an_escalated_record_shows_its_provenance_after_its_prose_and_reads_back_unchanged() {
        let mut m = meta(ItemKind::Record, "needs_human");
        m.escalated = Some(escalated_from("alice", "it needs a person"));
        assert_eq!(escalation_line(&m).as_deref(), Some(ESCALATED_LINE));
        assert_eq!(escalation_line(&meta(ItemKind::Record, "todo")), None);
        let body = render_body("the prose\nsecond line", &m);
        assert_eq!(
            shown(&body),
            format!("the prose\nsecond line\n\n{ESCALATED_LINE}\n\n")
        );
        assert_eq!(
            parse_body(&body).unwrap(),
            ("the prose\nsecond line".into(), m.clone().sealed())
        );
        let empty = render_body("", &m);
        assert_eq!(shown(&empty), format!("{ESCALATED_LINE}\n\n"));
        assert_eq!(
            parse_body(&empty).unwrap().0,
            "",
            "no prose reads back empty"
        );
    }

    // A finding's text is its claim: the record line and the provenance
    // line follow it, in that order, and the claim reads back unchanged.
    #[test]
    fn an_escalated_findings_claim_reads_back_as_the_claim() {
        let mut m = meta(ItemKind::Finding, "reproduced");
        m.record = Some(local_ref("t"));
        m.escalated = Some(escalated_from("alice", "it needs a person"));
        let record = record_line(&m).unwrap();
        let body = render_body("the claim", &m);
        assert_eq!(
            shown(&body),
            format!("the claim\n\n{record}\n\n{ESCALATED_LINE}\n\n")
        );
        assert_eq!(
            parse_body(&body).unwrap(),
            ("the claim".into(), m.clone().sealed())
        );
        m.record = Some(RecordRef {
            id: Iri::parse("https://github.com/acme/widgets/issues/3").unwrap(),
            node_id: Some("I_3".into()),
            title: None,
        });
        let body = render_body("the claim", &m);
        assert_eq!(shown(&body), format!("the claim\n\n{ESCALATED_LINE}\n\n"));
        assert_eq!(parse_body(&body).unwrap().0, "the claim");
    }

    // The line is text on GitHub: who and why mention nobody, link nothing,
    // open no tag and break no line — and the block keeps them as written.
    #[test]
    fn an_escalations_who_and_why_are_escaped_and_still_read_back() {
        let mut m = meta(ItemKind::Finding, "raised");
        m.escalated = Some(EscalatedFrom {
            from: Iri::parse("urn:x-local:item_7").unwrap(),
            by: "@someone <b>#12".into(),
            reason: "see #12 <b>now</b>\nask @someone".into(),
        });
        let body = render_body("the claim", &m);
        let line = escalation_line(&m).unwrap();
        assert_eq!(
            shown(&body),
            format!("the claim\n\n{line}\n\n"),
            "one line, after the claim"
        );
        assert!(
            line.starts_with(
                "Escalated from the local tier by @&#8203;someone &lt;b&gt;#&#8203;12: "
            ),
            "{line}"
        );
        assert!(
            line.contains("see #&#8203;12 &lt;b&gt;now&lt;/b&gt;<br>ask @&#8203;someone. "),
            "{line}"
        );
        assert!(
            line.ends_with("Its local IRI was urn:x-local:item\\_7."),
            "{line}"
        );
        for raw in ["@someone", "#12", "<b>", "item_7", "\n"] {
            assert!(!line.contains(raw), "{raw:?} in {line}");
        }
        assert_eq!(
            parse_body(&body).unwrap(),
            ("the claim".into(), m.clone().sealed())
        );
    }

    #[test]
    fn a_block_whose_format_disagrees_with_its_escalation_is_damaged() {
        let plain = render_body("p", &meta(ItemKind::Record, "todo"));
        let lying = plain.replace("\"fl_format\":1", "\"fl_format\":3");
        assert_ne!(lying, plain, "the edit must have landed");
        match parse_body(&lying) {
            Err(BodyError::Damaged(why)) => {
                assert!(why.contains("is 3, but its fields are format 1"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        let mut m = meta(ItemKind::Record, "todo");
        m.area = Some("code".into());
        m.escalated = Some(escalated_from("alice", "it needs a person"));
        let good = render_body("p", &m);
        let lying = good.replace("\"fl_format\":3", "\"fl_format\":2");
        assert_ne!(lying, good, "the edit must have landed");
        match parse_body(&lying) {
            Err(BodyError::Damaged(why)) => {
                assert!(why.contains("is 2, but its fields are format 3"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        let extra = good.replacen("\"escalated\":{", "\"escalated\":{\"extra\":1,", 1);
        assert_ne!(extra, good, "the edit must have landed");
        assert!(matches!(parse_body(&extra), Err(BodyError::Damaged(_))));
    }

    // An issue an fl that knows no escalation wrote reads exactly as it did,
    // and is written back byte for byte.
    #[test]
    fn an_older_format_block_reads_and_writes_exactly_as_before() {
        let one = "the prose\n\n<!-- fl:meta\n{\"fl_format\":1,\"kind\":\"record\",\
                   \"state\":\"todo\",\
                   \"project\":\"urn:uuid:00000000-0000-7000-8000-000000000001\",\
                   \"security\":false,\"also_known_as\":[],\"create_key\":\"urn:uuid:k\"}\n-->\n";
        let (prose, m) = parse_body(one).unwrap();
        let mut want = meta(ItemKind::Record, "todo");
        want.create_key = "urn:uuid:k".into();
        assert_eq!((prose.as_str(), &m), ("the prose", &want));
        assert_eq!(render_body(&prose, &m), one);
        let two = "the claim\n\nRecord: t — urn:uuid:00000000-0000-7000-8000-000000000042, \
                   held in the local tier, not on GitHub.\n\n<!-- fl:meta\n{\"fl_format\":2,\
                   \"kind\":\"finding\",\"state\":\"raised\",\
                   \"project\":\"urn:uuid:00000000-0000-7000-8000-000000000001\",\
                   \"area\":\"code\",\"record\":{\"id\":\
                   \"urn:uuid:00000000-0000-7000-8000-000000000042\",\"title\":\"t\"},\
                   \"raised_by\":\"rev\",\"security\":false,\"also_known_as\":[],\
                   \"create_key\":\"urn:uuid:k\"}\n-->\n";
        let (prose, m) = parse_body(two).unwrap();
        let mut want = meta(ItemKind::Finding, "raised");
        want.fl_format = 2;
        want.area = Some("code".into());
        want.record = Some(local_ref("t"));
        want.raised_by = Some("rev".into());
        want.create_key = "urn:uuid:k".into();
        assert_eq!((prose.as_str(), &m), ("the claim", &want));
        assert_eq!(render_body(&prose, &m), two);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-github --lib meta::tests::`
Expected: FAIL to compile — 12 errors, all of them the missing type, field and function: "cannot find type `EscalatedFrom` in this scope" (E0425, E0422), "no field `escalated` on type `meta::Meta`" (E0609) and "cannot find function `escalation_line` in this scope" (E0425).

- [ ] **Step 3: Implement**

In `crates/github/src/meta.rs`, after `pub const FL_FORMAT_ROUTED: u64 = 2;`, add:

```rust
/// The format of a block that carries its escalation (routing spec §3.3):
/// `Meta` refuses a field it does not know, so an fl that reads formats 1
/// and 2 would read the field as damage; raised, it says "upgrade fl".
pub const FL_FORMAT_ESCALATED: u64 = 3;
```

After `impl RecordRef { … }` (before `/// The fields a label cannot hold`), add:

```rust
/// Where an escalated issue came from (routing spec §3.3 step 2): the
/// item's IRI in the local tier, who escalated it, and why. The issue shows
/// it as a line written from the block, never as prose, so a finding's claim
/// stays its claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EscalatedFrom {
    pub from: Iri,
    pub by: String,
    pub reason: String,
}
```

In `pub struct Meta`, after the `also_known_as` field (before `create_key`'s doc comment), add:

```rust
    /// Set once, when the item is escalated from the local tier; skipped
    /// when absent, so a block without one is written byte for byte as
    /// before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalated: Option<EscalatedFrom>,
```

In `Meta::new`, after `also_known_as: vec![],`, add:

```rust
            escalated: None,
```

Replace `required_format`, with its doc comment, by:

```rust
    /// The format this block is written in: [`FL_FORMAT_ESCALATED`] when it
    /// carries its escalation, whatever else it carries; else
    /// [`FL_FORMAT_ROUTED`] when it carries an area or a reference to a
    /// local record; else [`FL_FORMAT`], which every older fl reads.
    pub fn required_format(&self) -> u64 {
        let local_record = self.record.as_ref().is_some_and(RecordRef::is_local);
        if self.escalated.is_some() {
            FL_FORMAT_ESCALATED
        } else if self.area.is_some() || local_record {
            FL_FORMAT_ROUTED
        } else {
            FL_FORMAT
        }
    }
```

Replace the doc comment above `pub fn render_body` (it begins `/// The prose, the line naming a local record`) with:

```rust
/// The line an escalated issue shows (routing spec §3.3 step 2): who
/// escalated it, why, and its IRI in the local tier, each escaped as
/// [`record_line`] escapes a title. `None` when the block carries no
/// escalation.
pub fn escalation_line(meta: &Meta) -> Option<String> {
    let e = meta.escalated.as_ref()?;
    Some(format!(
        "Escalated from the local tier by {}: {}. Its local IRI was {}.",
        crate::ledger::render::escape(&e.by),
        crate::ledger::render::escape(&e.reason),
        crate::ledger::render::escape(e.from.as_str())
    ))
}

/// The prose, the line naming a local record when there is one, the line
/// naming where an escalated item came from when there is one, then the
/// block, sealed — a blank line between each. ⚠ `<` and `>` are escaped
/// inside the JSON so no field value can end the HTML comment or open a
/// second block. They occur only inside JSON strings, where `<`/`>` are the
/// same text.
```

In `render_body`, replace `let shown = match record_line(&meta) { … };` with:

```rust
    let shown = [
        Some(prose.to_string()),
        record_line(&meta),
        escalation_line(&meta),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
```

In `impl std::fmt::Display for BodyError`, in the `UnknownFormat` arm, replace `{FL_FORMAT_ROUTED}` with `{FL_FORMAT_ESCALATED}`, so the format string reads:

```rust
                "has an fl block of format {n}, and this fl reads formats {FL_FORMAT} to \
                 {FL_FORMAT_ESCALATED}: upgrade fl to read it"
```

In `parse_body`, replace the accepted formats `Some(FL_FORMAT | FL_FORMAT_ROUTED) => {}` with:

```rust
        Some(FL_FORMAT | FL_FORMAT_ROUTED | FL_FORMAT_ESCALATED) => {}
```

and replace everything from `let prose = body[..at].trim_end();` to the function's end with:

```rust
    // The lines fl writes from the block are not part of the prose: the
    // escalation's line last, then the local record's before it.
    let mut prose = body[..at].trim_end();
    for line in [escalation_line(&meta), record_line(&meta)]
        .into_iter()
        .flatten()
    {
        prose = prose
            .strip_suffix(line.as_str())
            .map_or(prose, str::trim_end);
    }
    Ok((prose.to_string(), meta))
}
```

The check that a block's `fl_format` equals `required_format()` is unchanged: a format-3 block without `escalated`, or a format-2 block with it, is `Damaged`.

In `docs/github-tracker.md`, under "What an issue looks like", after the paragraph that ends "and fl warns before it does.", add:

```markdown
A record or finding escalated from the local store to GitHub ([routing.md](routing.md)) keeps
where it came from in its block, as `"escalated": {"from": "urn:uuid:…", "by": "…", "reason":
"…"}` — its IRI in the local store, who escalated it, and why — and the issue shows a line
`Escalated from the local tier by <by>: <reason>. Its local IRI was <from>.` after the body and
after any `Record:` line. Both lines are written from the block on every write, so neither
becomes part of a finding's claim or a record's text, and the names and reasons in them mention
nobody and link nothing. A block that carries `escalated` is written as `fl_format` 3, whatever
else it carries; an fl that reads formats 1 and 2 refuses it as a newer format.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github --lib meta::tests::`
Expected: PASS — 29 tests, among them `the_block_is_format_3_exactly_when_it_carries_its_escalation`, `an_escalated_record_shows_its_provenance_after_its_prose_and_reads_back_unchanged`, `an_escalated_findings_claim_reads_back_as_the_claim`, `an_escalations_who_and_why_are_escaped_and_still_read_back`, `a_block_whose_format_disagrees_with_its_escalation_is_damaged`, `an_older_format_block_reads_and_writes_exactly_as_before` and the changed `a_body_that_is_missing_damaged_or_newer_is_named`.

- [ ] **Step 5: Mutation checks**

Each filter is `cargo test -p fl-github --lib meta::tests::<name>`; "format 3" is `the_block_is_format_3_exactly_when_it_carries_its_escalation`, "record" is `an_escalated_record_shows_its_provenance_after_its_prose_and_reads_back_unchanged`, "finding" is `an_escalated_findings_claim_reads_back_as_the_claim`, "escaped" is `an_escalations_who_and_why_are_escaped_and_still_read_back`, "older" is `an_older_format_block_reads_and_writes_exactly_as_before`.

1. `required_format`'s escalation arm: `if false {` → format 3 red (1, not 3).
2. The arm's value: `FL_FORMAT_ROUTED` → format 3 red (2, not 3).
3. Escalation wins: test `self.area.is_some() || local_record` first, then `self.escalated.is_some()` → format 3 red (an area with an escalation is 2).
4. The area conjunct: `false || local_record` → format 3 red (an area alone is 1).
5. The local-record conjunct: `self.area.is_some() || false` → format 3 red (a local record alone is 1).
6. `FL_FORMAT_ESCALATED = 4` → format 3 red.
7. `parse_body` drops 3 from the accept list → record red (`UnknownFormat(3)`).
8. `parse_body` drops 2 → older red (`UnknownFormat(2)`); drops 1 → older red (`UnknownFormat(1)`).
9. `parse_body` accepts 4 (`| 4`) → `a_body_that_is_missing_damaged_or_newer_is_named` red (`Damaged`, not `UnknownFormat(4)`).
10. `UnknownFormat`'s message names `{FL_FORMAT_ROUTED}` → `a_body_that_is_missing_damaged_or_newer_is_named` red ("formats 1 to 2").
11. The format check against the fields: `if false {` → `a_block_whose_format_disagrees_with_its_escalation_is_damaged` red (a format-3 block without `escalated` reads).
12. The strip of the escalation line: `for line in [record_line(&meta)]` → record red (the line reads back as prose). The strip of the record line: `for line in [escalation_line(&meta)]` → `a_local_record_reference_shows_as_plain_text_and_reads_back_as_the_claim` red.
13. The strip order: `[record_line(&meta), escalation_line(&meta)]` → finding red (the record line stays in the claim).
14. The strip trims what is left: `.map_or(prose, |p| p)` → finding red.
15. The escape of `by`: `e.by.clone()` → escaped red; of `reason`: `e.reason.clone()` → escaped red; of `from`: `e.from.as_str().to_string()` → escaped red (`item_7` unescaped).
16. The line's wording: `"Escalated by {}: {}. Its local IRI was {}."` → record red.
17. The line's position after the record line: `[Some(prose…), escalation_line(&meta), record_line(&meta)]` → finding red. After the prose: `[escalation_line(&meta), Some(prose…), record_line(&meta)]` → record red.
18. Empty parts left out: delete `.filter(|part| !part.is_empty())` → record red (an empty prose leaves a blank line before the line).
19. `EscalatedFrom` refuses unknown fields: delete its `#[serde(deny_unknown_fields)]` → `a_block_whose_format_disagrees_with_its_escalation_is_damaged` red.
20. `escalated` is skipped when absent: `#[serde(default)]` alone → older red (`"escalated":null` is written).

Not observable:
- `#[serde(default)]` on `escalated`: serde reads a missing `Option` field as `None` without it, so older stays green when it is deleted; it is kept to match the block's other optional fields.
- `escalated: None` in `Meta::new` is required by the compiler, not a guard.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1113 passed, 18 ignored.

- [ ] **Step 7: Commit**

```bash
git add crates/github/src/meta.rs docs/github-tracker.md
git commit -m "feat(github): an escalated issue's block carries its provenance (fl_format 3)

Meta gains escalated: {from, by, reason} — the item's IRI in the local
tier, who escalated it, and why — skipped when absent, so every block
without one is written byte for byte as before. A block that carries it
is fl_format 3, whatever else it carries; parse_body reads formats 1 to
3 and still names a block whose format disagrees with its fields as
damaged. The issue shows the provenance as a line written from the
block, after the prose and any record line, each part escaped, and
parse_body strips it as it strips the record line, so a finding's claim
and a record's text read back unchanged. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

---

### Task 4: The GitHub tracker finds, checks and creates an escalated issue

An escalation's step 2 finds or creates the item's issue (routing spec §3.3 step 2): "Before any create, fl searches every issue, labelled or not, newest first, back to the mark's time less the create-search margin, for that key." Today the create-key search is private, keyed by a key `Meta::new` mints at random, and runs only inside one create, back to that attempt's start; no create carries a state other than `todo`/`raised` or any alias (spec defects 1 and 3). This task gives the GitHub tracker the three things the router's step 2 and its pre-checks need. `find_by_key` is that search made public — one pass, no settle and retry (plan ruling 1: the key is the item's old local IRI, so the search is the escalation's own, back to the mark's time). `alias_taken` is `add_alias`'s one-namespace rule read without writing (plan ruling 14), and `add_alias` now asks it. `create_escalated` searches first; an issue an earlier run made is labelled from its own block if a stop between the create and the label call left it bare, and returned — never a second issue (Review Focus 1). Otherwise it creates the issue with the item's own state, area and aliases, the old IRI as its create key and first alias, and the block's `escalated` field from Task 3 (plan rulings 2, 3), so the issue shows where it came from. A finding about a local record names it as text (as `add_finding_checked` does), one about a GitHub record by URL and node id (as `add_finding` does), and a security finding goes only to a private repository. An escalated record's text lists its open findings, escaped, never a security one (plan ruling 4, routing spec decision 18).

**Blast radius:** `create`'s last five lines (label, wait, record the kind, remember, return) move into `finish_create`, which `create` calls with the same arguments, so every record and finding create runs the same calls in the same order; the label list is computed there instead of before the body is rendered (it was used only by those lines). `find_by_create_key` becomes `pub fn find_by_key` with its parameter renamed; its one caller, `search_by_create_key`, is unchanged otherwise. `add_finding`'s record read moves into `record_issue`, unchanged (`a_finding_on_a_finding_is_the_wrong_kind_and_on_nothing_is_no_such_record` keeps it). `add_alias`'s two checks become one call to `alias_taken`, in the same order with the same error (`an_alias_is_found_by_a_full_scan_and_a_second_use_of_it_is_refused` keeps it). No existing test changes.

**Files:**
- Modify: `crates/github/src/tracker.rs` (`find_by_key`, `alias_taken`, `create_escalated`, `finish_found`, `finish_create`, `record_issue`, `escalated_findings`; `create`, `search_by_create_key`, `add_finding`, `add_alias`; tests)

**Interfaces:**
- Consumes: `fl_core::escalation::{Outgoing, Provenance}`, `fl_core::tiered::RecordSeen` (its `tier`) (Task 1); `crate::meta::EscalatedFrom` and `Meta.escalated` (Task 3); `crate::ledger::render::escape`; `finding_meta`, `create`, `label_created`, `ensure_labels`, `require_private`, `owner`, `alias_owner` (existing).
- Produces (exactly as "Interfaces every task shares"): `pub fn find_by_key(&self, key: &str, since_ms: u64) -> Result<Option<IssueView>, StoreError>`; `pub fn alias_taken(&self, alias: &Iri) -> Result<Option<Iri>, StoreError>`; `pub fn create_escalated(&self, item: &Outgoing, from: &Provenance, since_ms: u64) -> Result<Iri, StoreError>`, all on `GithubTracker`. `alias_taken` answers the issue's current URL (`issue_url(n)`), for an issue URL of this repository as for an alias. `create_escalated` writes `create_key = from.from.to_string()`, `also_known_as = [from.from, then the item's own aliases, without repeats]`, `escalated = Some(EscalatedFrom { from, by, reason })`; a record's prose is the findings list (empty with none), a finding's its claim; the found path labels from the found issue's own block. Private: `finish_create`, `finish_found`, `record_issue`, `escalated_findings(findings: &[Finding]) -> String`.
- Unique phrases: `Open findings when this record was escalated:` (the list's heading). The tests also assert `fl creates items open` (only `create`'s state check says it) and `without some or all of fl's labels` (only `unlabelled_issue` after a label call that may have failed says it), both existing, reached here and not changed.

- [ ] **Step 1: Write the failing tests**

In `crates/github/src/tracker.rs`, inside `mod tests`, after `fn a_finding_not_marked_security_may_go_to_a_public_repository` (before `/// The same suites the local stores pass` and `mod contract`), add:

```rust
    /// An item's IRI in the local tier.
    fn local_iri(n: u64) -> Iri {
        Iri::parse(&format!("urn:uuid:00000000-0000-7000-8000-{n:012}")).unwrap()
    }

    fn escalation_of(from: &Iri) -> Provenance {
        Provenance {
            from: from.clone(),
            by: "alice".into(),
            reason: "it needs a person".into(),
        }
    }

    /// A local record in `needs_human`, area `code`.
    fn needs_human(id: &Iri, also_known_as: Vec<Iri>) -> Record {
        Record {
            id: RecordId(id.clone()),
            project: p(),
            title: "the build is flaky".into(),
            state: State::NeedsHuman,
            also_known_as,
            area: Some("code".into()),
        }
    }

    fn escalated_record(id: &Iri) -> Outgoing {
        Outgoing::Record {
            record: needs_human(id, vec![]),
            findings: vec![],
        }
    }

    /// Issue `n`'s text as shown and its block.
    fn shown_and_block(fake: &FakeGithub, n: u64) -> (String, Meta) {
        let body = fake.issue(n).body;
        let shown = body[..body.rfind(meta::META_OPEN).unwrap()].to_string();
        (shown, meta::parse_body(&body).unwrap().1)
    }

    /// Label calls sent to issue `n`.
    fn label_posts(fake: &FakeGithub, n: u64) -> usize {
        let call = format!("POST /repos/acme/widgets/issues/{n}/labels");
        fake.state().requests.iter().filter(|r| **r == call).count()
    }

    const ESCALATED_7: &str = "Escalated from the local tier by alice: it needs a person. \
                               Its local IRI was urn:uuid:00000000-0000-7000-8000-000000000007.";

    // Routing spec decision 18: an escalated record's issue lists its open
    // findings — state, claim and IRI, escaped — and never a security one.
    #[test]
    fn an_escalated_records_text_lists_its_open_findings_escaped_and_never_a_security_one() {
        let record = RecordId(local_iri(7));
        let mut open = Finding::raise(p(), record.clone(), "rev", "it fails for @alice\nin #3");
        open.id = FindingId(Iri::parse("urn:x-local:finding_1").unwrap());
        open.state = FindingState::Reproduced;
        let mut slow = Finding::raise(p(), record.clone(), "rev", "slow <b>always</b>");
        slow.id = FindingId(local_iri(8));
        let mut secret = Finding::raise(p(), record, "rev", "the token leaks");
        secret.id = FindingId(local_iri(9));
        secret.security = true;
        assert_eq!(escalated_findings(&[]), "");
        assert_eq!(escalated_findings(std::slice::from_ref(&secret)), "");
        assert_eq!(
            escalated_findings(&[open, secret, slow]),
            "Open findings when this record was escalated:\n\n\
             - reproduced: it fails for @&#8203;alice<br>in #&#8203;3 — urn:x-local:finding\\_1\n\
             - raised: slow &lt;b&gt;always&lt;/b&gt; — \
             urn:uuid:00000000-0000-7000-8000-000000000008"
        );
    }

    // Routing spec §3.3 step 2: the issue has the record's own state, area
    // and aliases, its local IRI as the create key and an alias, and names
    // where it came from; the old IRI still finds it.
    #[test]
    fn an_escalated_record_keeps_its_state_area_and_aliases_and_lists_its_findings() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        let old = local_iri(7);
        let alias = Iri::parse("https://github.com/elsewhere/old/issues/4").unwrap();
        let mut open_finding = Finding::raise(p(), RecordId(old.clone()), "rev", "it is slow");
        open_finding.id = FindingId(local_iri(8));
        let mut secret = Finding::raise(p(), RecordId(old.clone()), "rev", "the token leaks");
        secret.id = FindingId(local_iri(9));
        secret.security = true;
        let item = Outgoing::Record {
            // An alias that repeats the local IRI is written once.
            record: needs_human(&old, vec![alias.clone(), old.clone()]),
            findings: vec![open_finding, secret],
        };
        let url = t
            .create_escalated(&item, &escalation_of(&old), now_millis())
            .unwrap();
        assert_eq!(url, t.issue_url(1));
        let issue = fake.issue(1);
        assert_eq!(issue.state, "open");
        assert_eq!(issue.title, "the build is flaky");
        assert_eq!(
            issue.labels,
            vec!["fl:record", "fl:record/needs_human", "fl:area/code"]
        );
        let (shown, block) = shown_and_block(&fake, 1);
        assert_eq!(
            shown,
            format!(
                "Open findings when this record was escalated:\n\n\
                 - raised: it is slow — urn:uuid:00000000-0000-7000-8000-000000000008\n\n\
                 {ESCALATED_7}\n\n"
            )
        );
        assert!(!shown.contains("token"), "{shown}");
        assert_eq!(block.create_key, old.as_str());
        assert_eq!(block.also_known_as, vec![old.clone(), alias.clone()]);
        assert_eq!(
            block.escalated,
            Some(EscalatedFrom {
                from: old.clone(),
                by: "alice".into(),
                reason: "it needs a person".into(),
            })
        );
        assert_eq!(block.fl_format, 3);
        assert_eq!(
            (block.state.as_str(), block.area.as_deref()),
            ("needs_human", Some("code"))
        );
        let back = open(&fake)
            .get_record(&RecordId(old.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(back.id, RecordId(url));
        assert_eq!(
            (back.title.as_str(), back.state, back.also_known_as),
            ("the build is flaky", State::NeedsHuman, vec![old, alias])
        );
    }

    // Routing spec §2.5, §3.3 step 2: an escalated finding's issue is its
    // claim; its record is named as text when it is local, and by URL and
    // node id when it is on GitHub.
    #[test]
    fn an_escalated_finding_names_a_local_record_as_text_and_links_a_github_one() {
        use fl_core::tiered::RecordSeen;
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        let local_record = RecordId(local_iri(42));
        let old = local_iri(7);
        let alias = Iri::parse("https://github.com/elsewhere/old/issues/4").unwrap();
        let mut f = Finding::raise(p(), local_record.clone(), "rev", "the claim\nin two lines");
        f.id = FindingId(old.clone());
        f.area = Some("design".into());
        f.also_known_as = vec![alias.clone()];
        let item = Outgoing::Finding {
            finding: f,
            record: RecordSeen {
                id: local_record.clone(),
                title: "the build".into(),
                tier: Tier::Local,
            },
        };
        let url = t
            .create_escalated(&item, &escalation_of(&old), now_millis())
            .unwrap();
        assert_eq!(url, t.issue_url(1));
        let issue = fake.issue(1);
        assert_eq!(issue.title, "the claim");
        assert_eq!(
            issue.labels,
            vec!["fl:finding", "fl:finding/raised", "fl:area/design"]
        );
        let (shown, block) = shown_and_block(&fake, 1);
        let record_line = meta::record_line(&block).unwrap();
        assert!(
            record_line.contains("held in the local tier, not on GitHub"),
            "{record_line}"
        );
        assert_eq!(
            shown,
            format!("the claim\nin two lines\n\n{record_line}\n\n{ESCALATED_7}\n\n")
        );
        assert_eq!(
            block.record,
            Some(RecordRef {
                id: local_record.0.clone(),
                node_id: None,
                title: Some("the build".into()),
            })
        );
        assert_eq!(
            (block.create_key.as_str(), block.also_known_as.clone()),
            (old.as_str(), vec![old.clone(), alias])
        );
        assert_eq!(block.fl_format, 3);
        let back = t.get_finding(&FindingId(url)).unwrap().unwrap();
        assert_eq!(
            (back.record, back.claim.as_str()),
            (local_record, "the claim\nin two lines")
        );

        let on_github = t.add_record(&p(), "the build").unwrap();
        let old = local_iri(8);
        let mut f = Finding::raise(p(), RecordId(local_iri(42)), "rev", "another claim");
        f.id = FindingId(old.clone());
        f.state = FindingState::Assigned;
        f.assigned_to = Some("bob".into());
        let item = Outgoing::Finding {
            finding: f,
            record: RecordSeen {
                id: on_github.clone(),
                title: "the build".into(),
                tier: Tier::Github,
            },
        };
        let url = t
            .create_escalated(&item, &escalation_of(&old), now_millis())
            .unwrap();
        assert_eq!(url, t.issue_url(3));
        assert_eq!(
            fake.issue(3).labels,
            vec!["fl:finding", "fl:finding/assigned"]
        );
        let (shown, block) = shown_and_block(&fake, 3);
        assert!(!shown.contains("Record:"), "{shown}");
        assert_eq!(
            block.record,
            Some(RecordRef {
                id: on_github.0.clone(),
                node_id: Some("I_2".into()),
                title: None,
            })
        );
        assert_eq!(block.assigned_to.as_deref(), Some("bob"));
        let back = t.get_finding(&FindingId(url)).unwrap().unwrap();
        assert_eq!(back.record, on_github);
    }

    // GitHub tracker spec §6: a security finding is escalated only to a
    // private repository; anywhere else nothing is created.
    #[test]
    fn an_escalated_security_finding_goes_only_to_a_private_repository() {
        use fl_core::tiered::RecordSeen;
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        let old = local_iri(7);
        let mut f = Finding::raise(p(), RecordId(local_iri(42)), "rev", "the token leaks");
        f.id = FindingId(old.clone());
        f.security = true;
        let item = Outgoing::Finding {
            finding: f,
            record: RecordSeen {
                id: RecordId(local_iri(42)),
                title: "t".into(),
                tier: Tier::Local,
            },
        };
        fake.state().repos[0].visibility = "public".into();
        let err = t
            .create_escalated(&item, &escalation_of(&old), now_millis())
            .unwrap_err();
        let StoreError::SecurityNotPrivate { visibility, .. } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(visibility, "public");
        assert_eq!((fake.issue_count(), issue_posts(&fake)), (0, 0));
        fake.state().repos[0].visibility = "private".into();
        t.create_escalated(&item, &escalation_of(&old), now_millis())
            .unwrap();
        assert!(shown_and_block(&fake, 1).1.security);
    }

    // GitHub tracker spec §3.2: an issue is created open, so a closed item
    // is refused before anything is sent — the router checks first (routing
    // spec §3.2), and the create's own check stays.
    #[test]
    fn a_closed_item_is_never_created_by_an_escalation() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        let old = local_iri(7);
        let mut record = needs_human(&old, vec![]);
        record.state = State::Done;
        let item = Outgoing::Record {
            record,
            findings: vec![],
        };
        let err = t
            .create_escalated(&item, &escalation_of(&old), now_millis())
            .unwrap_err()
            .to_string();
        assert!(err.contains("fl creates items open"), "{err}");
        assert_eq!((fake.issue_count(), issue_posts(&fake)), (0, 0));
    }

    // Routing spec §3.3: running the escalation again resumes — the search
    // finds the issue the first run made, and nothing is created or
    // labelled again.
    #[test]
    fn a_rerun_finds_the_escalated_issue_and_creates_no_second() {
        let fake = FakeGithub::start("acme/widgets");
        let since = now_millis();
        let old = local_iri(7);
        let first = open(&fake)
            .without_settle()
            .create_escalated(&escalated_record(&old), &escalation_of(&old), since)
            .unwrap();
        let labelled = label_posts(&fake, 1);
        let again = open(&fake)
            .without_settle()
            .create_escalated(&escalated_record(&old), &escalation_of(&old), since)
            .unwrap();
        assert_eq!(again, first);
        assert_eq!((fake.issue_count(), issue_posts(&fake)), (1, 1));
        assert_eq!(label_posts(&fake, 1), labelled, "no second label call");
    }

    // Routing spec §3.3 step 2: a run stopped between the create and its
    // label call left an issue with no fl label. A rerun more than the
    // create-search margin later still finds it — it searches back from the
    // mark's time, not its own — labels it as the create would have, and
    // makes no second issue.
    #[test]
    fn an_escalation_stopped_before_its_labels_is_found_and_finished_after_the_margin() {
        let fake = FakeGithub::start("acme/widgets");
        let marked = now_millis();
        let old = local_iri(7);
        fake.state().fail_label_add_next = true;
        let err = open(&fake)
            .without_settle()
            .create_escalated(&escalated_record(&old), &escalation_of(&old), marked)
            .unwrap_err()
            .to_string();
        assert!(err.contains("without some or all of fl's labels"), "{err}");
        assert!(fake.issue(1).labels.is_empty());
        let eleven_minutes = 11 * 60 * 1000;
        for i in fake.state().issues.values_mut() {
            i.created_ms -= eleven_minutes;
        }
        let marked = marked - eleven_minutes;
        // The label is gone too: it is created before it is added.
        fake.state().labels.remove("fl:area/code");
        let t = open(&fake).without_settle();
        assert_eq!(
            t.find_by_key(old.as_str(), now_millis()).unwrap(),
            None,
            "searched from now, the issue is past the margin"
        );
        let created = label_creates(&fake);
        let url = t
            .create_escalated(&escalated_record(&old), &escalation_of(&old), marked)
            .unwrap();
        assert_eq!(url, t.issue_url(1));
        assert_eq!(
            fake.issue(1).labels,
            vec!["fl:record", "fl:record/needs_human", "fl:area/code"]
        );
        assert_eq!(label_creates(&fake), created + 1, "created explicitly");
        assert_eq!((fake.issue_count(), issue_posts(&fake)), (1, 1));
        let records = open(&fake).list_records(&p()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, State::NeedsHuman);
    }

    // GitHub tracker spec §3.3: a create whose answer failed after it
    // landed is found by its create key — the local IRI — and not sent
    // again.
    #[test]
    fn an_escalation_whose_create_failed_after_landing_is_found_not_duplicated() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        let old = local_iri(7);
        fake.state().fail_after_create = true;
        let url = t
            .create_escalated(&escalated_record(&old), &escalation_of(&old), now_millis())
            .unwrap();
        assert_eq!(url, t.issue_url(1));
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
        assert_eq!(
            fake.issue(1).labels,
            vec!["fl:record", "fl:record/needs_human", "fl:area/code"]
        );
    }

    // Routing spec §3.3 step 2: one pass, every issue newest first, back
    // to the margin before the given time and no further.
    #[test]
    fn find_by_key_reads_back_to_the_margin_before_its_time_and_no_further() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        let since = now_millis();
        let old = local_iri(7);
        t.create_escalated(&escalated_record(&old), &escalation_of(&old), since)
            .unwrap();
        t.add_record(&p(), "newer").unwrap();
        let found = |at: u64| t.find_by_key(old.as_str(), at).unwrap().map(|i| i.number);
        assert_eq!(found(since), Some(1));
        assert_eq!(t.find_by_key("urn:uuid:another", since).unwrap(), None);
        let eleven_minutes = 11 * 60 * 1000;
        fake.state().issues.get_mut(&1).unwrap().created_ms -= eleven_minutes;
        assert_eq!(found(since), None, "older than the margin");
        assert_eq!(found(since - eleven_minutes), Some(1));
    }

    // Routing spec §3.2, one id namespace: an alias is taken when it is an
    // issue URL of this repository or another issue's alias.
    #[test]
    fn an_alias_is_taken_by_an_issue_here_or_by_another_issues_alias() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let a = t.add_record(&p(), "a").unwrap();
        let elsewhere = Iri::parse("https://github.com/elsewhere/old/issues/7").unwrap();
        t.add_alias(a.iri(), elsewhere.clone()).unwrap();
        assert_eq!(t.alias_taken(a.iri()).unwrap(), Some(t.issue_url(1)));
        assert_eq!(t.alias_taken(&elsewhere).unwrap(), Some(t.issue_url(1)));
        assert_eq!(t.alias_taken(&local_iri(8)).unwrap(), None);
        let free = Iri::parse("https://github.com/elsewhere/old/issues/8").unwrap();
        assert_eq!(t.alias_taken(&free).unwrap(), None);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-github --lib tracker::tests::`
Expected: FAIL to compile — 32 errors, all of them the missing items: "cannot find type `Outgoing`" / "`Provenance`" (E0425, E0433, E0422), "cannot find struct, variant or union type `EscalatedFrom`" (E0422), "cannot find function `escalated_findings`" (E0425), and "no method named `create_escalated`" / "`find_by_key`" / "`alias_taken`" found for struct `tracker::GithubTracker` (E0599).

- [ ] **Step 3: Implement**

In `crates/github/src/tracker.rs`, replace the import `use crate::meta::{self, IssueView, ItemKind, Meta, Read, RecordRef, TITLE_MAX};` with:

```rust
use crate::meta::{self, EscalatedFrom, IssueView, ItemKind, Meta, Read, RecordRef, TITLE_MAX};
```

and after `use fl_core::at::At;` add:

```rust
use fl_core::escalation::{Outgoing, Provenance};
```

In `fn create`, delete the line `let labels = meta::labels_after(&[], kind, &meta.state, meta.area.as_deref());` (after `self.ensure_labels(meta.area.as_deref())?;`), and replace the function's last five lines — from `let issue = self.label_created(issue, &labels)?;` to `Ok(issue)` — with:

```rust
        self.finish_create(issue, kind, meta, prose, title)
```

After `fn create` (before `/// Add fl's \`labels\` to an issue fl just created without them`), add:

```rust
    /// The end of a create (spec §3.3), once the issue exists: fl's labels
    /// added by a call of their own, a wait until they show, and the issue
    /// remembered as just read.
    fn finish_create(
        &self,
        issue: IssueView,
        kind: ItemKind,
        meta: &Meta,
        prose: &str,
        title: &str,
    ) -> Result<IssueView, StoreError> {
        let labels = meta::labels_after(&[], kind, &meta.state, meta.area.as_deref());
        let issue = self.label_created(issue, &labels)?;
        self.await_create_events(issue.number, &labels);
        self.kinds.borrow_mut().insert(issue.number, kind);
        self.remember(issue.number, meta, prose, title);
        Ok(issue)
    }
```

In `fn search_by_create_key`, replace `self.find_by_create_key(key, started)?` with `self.find_by_key(key, started)?`.

Replace the doc comment, signature and first statement of `fn find_by_create_key` (from `/// The issue whose block carries create key` to the `let since = …;` statement) with the following; the rest of its body is unchanged:

```rust
    /// The issue whose block carries create key `key`, among the issues
    /// created since `CREATE_SEARCH_MARGIN` before `since_ms` (unix
    /// milliseconds, by this machine's clock): when a create attempt began,
    /// or when an escalation marked its item (routing spec §3.3 step 2).
    /// One pass; `Ok(None)` when it misses.
    /// ⚠ Every issue, not only fl's labelled ones: a create sends no labels
    /// and adds them afterward (`label_created`), so an issue this attempt
    /// made may carry none — after a stop between the two calls, or a
    /// create whose answer was lost. Newest first, and it
    /// stops at the first issue older than the margin, so its cost does not
    /// grow with the repository's history.
    pub fn find_by_key(&self, key: &str, since_ms: u64) -> Result<Option<IssueView>, StoreError> {
        let since =
            At::from_unix_millis(since_ms.saturating_sub(CREATE_SEARCH_MARGIN.as_millis() as u64));
```

In `impl GithubTracker`, after `pub fn items_in_area` (its last method), add:

```rust
    /// The issue `alias` already names here, if any — the one id namespace
    /// `add_alias` keeps (spec §2.5), read without writing: an issue URL of
    /// this repository names that issue, and another issue's alias names
    /// that issue.
    /// ⚠ The alias scan reads fl's labelled issues (`alias_owner`): an issue
    /// with no fl label is not seen.
    pub fn alias_taken(&self, alias: &Iri) -> Result<Option<Iri>, StoreError> {
        if let Owner::Ours(n) = self.owner(alias)? {
            return Ok(Some(self.issue_url(n)));
        }
        Ok(self.alias_owner(alias)?.map(|n| self.issue_url(n)))
    }

    /// The issue an escalated item becomes (routing spec §3.3 step 2). Its
    /// create key is the item's local IRI, so the issue an earlier run made
    /// is found first — labelled or not, back to `since_ms` (the mark's
    /// time) less the margin — given its labels if that run stopped before
    /// them, and returned: never a second issue. Otherwise the issue is
    /// made with the item's own state, area and aliases, the local IRI
    /// first among them, and the block names where it came from.
    pub fn create_escalated(
        &self,
        item: &Outgoing,
        from: &Provenance,
        since_ms: u64,
    ) -> Result<Iri, StoreError> {
        if let Some(found) = self.find_by_key(from.from.as_str(), since_ms)? {
            return Ok(self.finish_found(found)?.url);
        }
        let (kind, title, prose, mut meta, aliases) = match item {
            Outgoing::Record { record, findings } => {
                let mut meta = Meta::new(
                    ItemKind::Record,
                    record.state.as_wire(),
                    record.project.clone(),
                );
                meta.area = record.area.clone();
                (
                    ItemKind::Record,
                    record.title.clone(),
                    escalated_findings(findings),
                    meta,
                    record.also_known_as.as_slice(),
                )
            }
            Outgoing::Finding { finding, record } => {
                // GitHub tracker spec §6, as `add_finding`.
                if finding.security {
                    self.require_private()?;
                }
                let record = match record.tier {
                    // Routing spec §2.5: as `add_finding_checked`.
                    Tier::Local => RecordRef {
                        id: record.id.iri().clone(),
                        node_id: None,
                        title: Some(record.title.clone()),
                    },
                    Tier::Github => {
                        let issue = self.record_issue(&record.id)?;
                        RecordRef {
                            id: issue.url,
                            node_id: Some(issue.node_id),
                            title: None,
                        }
                    }
                };
                (
                    ItemKind::Finding,
                    meta::title_of(&finding.claim),
                    finding.claim.clone(),
                    finding_meta(finding, record),
                    finding.also_known_as.as_slice(),
                )
            }
        };
        // The local IRI is the create key the search above looks for, and
        // an alias, so the old id still names the item.
        meta.create_key = from.from.to_string();
        meta.also_known_as = vec![from.from.clone()];
        for alias in aliases {
            if !meta.also_known_as.contains(alias) {
                meta.also_known_as.push(alias.clone());
            }
        }
        meta.escalated = Some(EscalatedFrom {
            from: from.from.clone(),
            by: from.by.clone(),
            reason: from.reason.clone(),
        });
        Ok(self.create(kind, &title, &prose, &meta)?.url)
    }

    /// An issue an earlier escalation made, found by its create key: given
    /// the labels its own block names — kind, state, area — exactly as a
    /// create gives them, when a stop between the create and the label
    /// call left it without them. One that carries them is left as it is.
    fn finish_found(&self, issue: IssueView) -> Result<IssueView, StoreError> {
        let (prose, block) = meta::parse_body(&issue.body).map_err(|e| StoreError::Diverged {
            id: issue.url.clone(),
            detail: format!("its body {e}"),
        })?;
        self.ensure_labels(block.area.as_deref())?;
        let title = issue.title.clone();
        self.finish_create(issue, block.kind, &block, &prose, &title)
    }

    /// The issue of the fl record `id` names here (spec §3.1): another kind
    /// is `WrongKind`, no issue is `NoSuchRecord`.
    /// ⚠ `remember: false`: a validity check, not a read the caller
    /// receives the record from.
    fn record_issue(&self, id: &RecordId) -> Result<IssueView, StoreError> {
        match self.item(id.iri(), ItemKind::Record, false)? {
            Found::Item(issue, _, _) => Ok(issue),
            Found::OtherKind(k) => Err(StoreError::WrongKind {
                id: id.iri().clone(),
                expected: Kind::Record,
                found: k.as_kind(),
            }),
            Found::Absent => Err(StoreError::NoSuchRecord(id.clone())),
        }
    }
```

After the closing brace of `impl GithubTracker` (before `/// The block of a new finding about \`record\` (spec §3.1).` and `fn finding_meta`), add:

```rust
/// An escalated record's text (routing spec decision 18): its open
/// findings, one line each — state, claim and IRI, each escaped so none
/// mentions anyone, links anything or breaks the line — under a heading;
/// empty when there are none. The list is as of the escalation, and is not
/// kept current.
fn escalated_findings(findings: &[Finding]) -> String {
    let escape = crate::ledger::render::escape;
    let lines: Vec<String> = findings
        .iter()
        // Defence in depth: the router leaves security findings out too
        // (routing spec decision 18), and a public issue must never list one.
        // Sensitivity is the router's to filter: it holds the routing map,
        // and leaves out a finding in a sensitive or undeclared area
        // (routing spec decision 21).
        .filter(|f| !f.security)
        .map(|f| {
            format!(
                "- {}: {} — {}",
                escape(f.state.as_wire()),
                escape(&f.claim),
                escape(f.id.iri().as_str())
            )
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "Open findings when this record was escalated:\n\n{}",
        lines.join("\n")
    )
}
```

In `impl Tracker for GithubTracker`, in `fn add_finding`, replace everything from the two `// ⚠ \`remember: false\`` comment lines through the `let record = match self.item(…)? { … };` statement with:

```rust
        let record = self.record_issue(&finding.record)?;
```

(the comment line `// The record must be an fl record of this repository (spec §3.1).` above it stays.)

In `fn add_alias`, replace the two checks — `if let Owner::Ours(_) = self.owner(&alias)? { … }` and `if self.alias_owner(&alias)?.is_some() { … }` — with:

```rust
        if self.alias_taken(&alias)?.is_some() {
            return Err(StoreError::AlreadyExists(alias));
        }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github --lib tracker::tests::`
Expected: PASS — 125 tests, among them `an_escalated_records_text_lists_its_open_findings_escaped_and_never_a_security_one`, `an_escalated_record_keeps_its_state_area_and_aliases_and_lists_its_findings`, `an_escalated_finding_names_a_local_record_as_text_and_links_a_github_one`, `an_escalated_security_finding_goes_only_to_a_private_repository`, `a_closed_item_is_never_created_by_an_escalation`, `a_rerun_finds_the_escalated_issue_and_creates_no_second`, `an_escalation_stopped_before_its_labels_is_found_and_finished_after_the_margin`, `an_escalation_whose_create_failed_after_landing_is_found_not_duplicated`, `find_by_key_reads_back_to_the_margin_before_its_time_and_no_further` and `an_alias_is_taken_by_an_issue_here_or_by_another_issues_alias`.

- [ ] **Step 5: Mutation checks**

Each filter is `cargo test -p fl-github --lib tracker::tests::<name>`. "record" is `an_escalated_record_keeps_its_state_area_and_aliases_and_lists_its_findings`, "finding" is `an_escalated_finding_names_a_local_record_as_text_and_links_a_github_one`, "list" is `an_escalated_records_text_lists_its_open_findings_escaped_and_never_a_security_one`, "rerun" is `a_rerun_finds_the_escalated_issue_and_creates_no_second`, "margin" is `an_escalation_stopped_before_its_labels_is_found_and_finished_after_the_margin`, "alias" is `an_alias_is_taken_by_an_issue_here_or_by_another_issues_alias`.

1. Search first: `if let Some(found) = None::<IssueView> {` in `create_escalated` → rerun red (two issues) and margin red.
2. The search before the create: delete the `if let Some(found) = self.find_by_key(…)? { … }` block → rerun red.
3. The search reads back from the mark: `self.find_by_key(from.from.as_str(), now_millis())?` → margin red (the aged issue is past the margin; a second issue is made).
4. The found issue is labelled: in `finish_found`, replace `self.finish_create(issue, block.kind, &block, &prose, &title)` by `Ok(issue)` → margin red (no labels).
5. Its labels are created first: delete `self.ensure_labels(block.area.as_deref())?;` in `finish_found` → margin red (no label create).
6. `create_key`: delete `meta.create_key = from.from.to_string();` → record red (a random key) and rerun red (a second issue).
7. The old IRI an alias: `meta.also_known_as = vec![];` → record red.
8. The record's own aliases: `for alias in &aliases[..0] {` → record red.
9. The finding's own aliases: `&finding.also_known_as[..0],` → finding red.
10. No repeats: `if true {` for `if !meta.also_known_as.contains(alias) {` → record red (the old IRI twice).
11. `escalated`: delete `meta.escalated = Some(…);` → record red; `by: from.reason.clone()` → record red; `reason: from.by.clone()` → record red.
12. The area: delete `meta.area = record.area.clone();` → record red (no `fl:area/code`).
13. The state: `State::Todo.as_wire()` for `record.state.as_wire()` → record red.
14. The record's title: `"x".to_string()` → record red. Its prose: `String::new()` for `escalated_findings(findings)` → record red.
15. The security check: delete `if finding.security { self.require_private()?; }` → `an_escalated_security_finding_goes_only_to_a_private_repository` red (an issue on a public repository).
16. The record's tier: `match Tier::Local {` → finding red (a GitHub record named as text). The local reference's title: `title: None` → finding red. The GitHub reference's node id: `node_id: None` → finding red.
17. A finding's title: `finding.claim.clone()` for `meta::title_of(&finding.claim)` → finding red (a two-line title). Its prose: `String::new()` for `finding.claim.clone()` → finding red.
18. The security filter in the list: delete `.filter(|f| !f.security)` → list red and record red.
19. The escape of the claim: `f.claim.clone()` → list red; of the IRI: `f.id.iri().to_string()` → list red (`finding_1` unescaped).
20. No findings, no text: delete `if lines.is_empty() { return String::new(); }` → list red (a bare heading).
21. `alias_taken`'s issue arm: delete `if let Owner::Ours(n) = self.owner(alias)? { … }` → alias red; its alias arm: `Ok(None)` → alias red.
22. `add_alias` asks `alias_taken`: `if false {` → `an_alias_is_found_by_a_full_scan_and_a_second_use_of_it_is_refused` red.
23. `find_by_key`'s margin: `if false {` for `if created_at(node)? < since {` → `find_by_key_reads_back_to_the_margin_before_its_time_and_no_further` red.
24. `create`'s state check, reached through the escalation: `if false {` for `if state != "open" {` → `a_closed_item_is_never_created_by_an_escalation` red.
25. `record_issue`'s arms (moved from `add_finding`): `Found::Absent => Err(StoreError::Deleted(id.iri().clone()))` → `a_finding_on_a_finding_is_the_wrong_kind_and_on_nothing_is_no_such_record` red; `found: Kind::Record` for `found: k.as_kind()` → the same test red.

Not observable:
- The escape of the state in the list: every finding state's wire name (`raised`, `reproduced`, `assigned`, `fixed`, `withdrawn`) is plain lowercase letters, which `escape` leaves as they are, so list stays green when `escape(f.state.as_wire())` becomes `f.state.as_wire().to_string()`. It is kept so every part of the line is escaped alike.
- `finish_found`'s `Diverged` on a body that does not parse: `find_by_key` returns only an issue whose body parsed with that key, so no input reaches it; it keeps the error typed rather than a panic.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1123 passed, 18 ignored.

- [ ] **Step 7: Commit**

```bash
git add crates/github/src/tracker.rs
git commit -m "feat(github): the GitHub tracker finds, checks and creates an escalated issue

find_by_key is the create-key search made public: one pass over every
issue, labelled or not, newest first, back to the given time less the
create-search margin. alias_taken answers add_alias's one-namespace
rule without writing, and add_alias now asks it. create_escalated
searches for the item's local IRI as a create key first: an issue an
earlier run made is labelled from its own block if a stop left it
without labels, and returned — never a second issue. Otherwise it
creates the issue with the item's own state, area and aliases, the
local IRI as its create key and first alias, and the block's escalated
field; a finding about a local record names it as text, one about a
GitHub record by URL and node id, and a security finding goes only to
a private repository. An escalated record's text lists its open
findings, escaped, never a security one. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

---

### Task 5: What the router asks of the GitHub tier for an escalation

An escalation's pre-checks and its step 2 ask GitHub three things (routing spec §3.2, §3.3 step 2): whether an alias the item carries already names an issue there ("one id names one item across both tiers"), whether the issue an earlier run made exists ("Before any create, fl searches every issue, labelled or not, newest first, back to the mark's time less the create-search margin, for that key"), and to find or create the issue. Task 4 gave the GitHub tracker those three as inherent methods; the router reaches GitHub only through `GithubTier` (routing spec §2.6), so this task adds them to the trait, exactly as "Interfaces every task shares" writes them, and gives every implementor its answer. `GithubTracker` implements `GithubTier` itself — the live test drives the router over `MemStore` and the real tracker with no CLI in between (plan ruling 22) — answering each question with its own method; the CLI's `LazyGithub` opens the tracker on the first of them, as it does for `require_private`; and `MemIssues` gains an in-memory model the router's escalation tests (Task 7) can stop after each step: create keys searched first, the old IRI and the item's aliases recorded as aliases (so the old id still names the item, plan ruling 1), a security finding refused when public, a count of creates, and two one-shot knobs — a create that fails before it lands, and one that lands and loses its answer (GitHub tracker spec §3.3's ambiguous create; Review Focus 1).

**Blast radius:** `GithubTier` gains three required methods. Its implementors are `MemIssues` (fl-core, tests and the `conformance` feature only), the CLI's `LazyGithub`, and — new — `GithubTracker`; all three implement them here, so the workspace compiles. No caller exists yet (Task 7 is the first). `GithubTracker`'s inherent methods keep their names: inside `impl GithubTier for GithubTracker` and through `LazyGithub::open()`, a call by the shared name resolves to the inherent method first, so nothing recurses and no existing call changes. `MemIssues::up` now builds its error through a new private `unreachable()`, with the same store label and cause. No existing behaviour changes and no existing test changes.

**Files:**
- Modify: `crates/core/src/routing.rs` (`GithubTier::{find_escalated, alias_taken, create_escalated}`; the `escalation` import)
- Modify: `crates/core/src/mem_issues.rs` (`MemIssues.{fail_next_create, lose_next_create_answer, creates}`, `Issues.keys`; `set_fail_next_create`, `set_lose_next_create_answer`, `creates`, private `unreachable`; the three methods; tests)
- Modify: `crates/github/src/tracker.rs` (`impl GithubTier for GithubTracker`; tests)
- Modify: `crates/cli/src/tiers.rs` (`LazyGithub`'s three methods; a test)

**Interfaces:**
- Consumes: `fl_core::escalation::{Outgoing, Provenance}`, `fl_core::tiered::RecordSeen` (Task 1); `GithubTracker::{find_by_key, alias_taken, create_escalated}` (Task 4), `require_private`, `items_in_area`, `repo`'s `full_name`, `meta::{parse_issue_url, is_issue_url}` (existing); `MemIssues`' `up`, `mint`, `public`, `ISSUES`, `LABEL` (existing).
- Produces (exactly as "Interfaces every task shares"): on `GithubTier`, `fn find_escalated(&self, key: &Iri, since_ms: u64) -> Result<Option<Iri>, StoreError>`, `fn alias_taken(&self, alias: &Iri) -> Result<Option<Iri>, StoreError>`, `fn create_escalated(&self, item: &Outgoing, from: &Provenance, since_ms: u64) -> Result<Iri, StoreError>`; `impl GithubTier for GithubTracker` (`available` true; `claims` an issue URL of this repository under its name now, any case, no request; `issue_form` any issue URL; `tracker` itself; the rest its own methods, `find_escalated` being `find_by_key`'s issue URL); the three on `LazyGithub` (each through `self.open()?`).
- Produces, beyond the shared block, on `MemIssues` (test support): `pub fn set_fail_next_create(&self, fail: bool)` — the next `create_escalated` fails `Unreachable` before it reads or writes anything, once; `pub fn set_lose_next_create_answer(&self, lose: bool)` — the next `create_escalated` that creates lands its issue (key, aliases, count) and then fails `Unreachable`, once; `pub fn creates(&self) -> u32` — issues `create_escalated` made (a found one is not counted). `MemIssues::create_escalated` searches its keys first, then refuses a security finding when public (`SecurityNotPrivate`), then creates: a record or finding with the item's own fields, the new issue URL as its id, `also_known_as = [from.from, then the item's own aliases]`, each of them an alias, and a finding's `record` set to the `RecordSeen`'s id. `find_escalated` answers from the keys and `since_ms` is not read (this tier has no clock); `alias_taken` answers any `ISSUES` URL with that issue, held here or not (as the GitHub tracker does, routing spec §3.2), else an alias's issue.
- Unique phrases: none. The tests match on variants (`Unreachable`, `SecurityNotPrivate`, `Routing(TierUnavailable { tier: Github, .. })`) and on IRIs, not on message text.

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/mem_issues.rs`, inside `mod tests`, after `use crate::ids::seq_iri;` add:

```rust
    use crate::tiered::RecordSeen;
```

and at the end of `mod tests` (after `fn an_id_this_tier_never_held_is_not_owned_and_a_public_one_refuses_security`), add:

```rust
    /// An item's IRI in the local tier.
    fn local(n: u64) -> Iri {
        Iri::parse(&format!("urn:uuid:00000000-0000-7000-8000-{n:012}")).unwrap()
    }

    fn escalation_of(from: &Iri) -> Provenance {
        Provenance {
            from: from.clone(),
            by: "alice".into(),
            reason: "it needs a person".into(),
        }
    }

    /// A local record in `needs_human`, area `code`, with one alias.
    fn escalated_record(id: &Iri, alias: &Iri) -> Outgoing {
        Outgoing::Record {
            record: Record {
                id: RecordId(id.clone()),
                project: ProjectId(seq_iri(1)),
                title: "the build is flaky".into(),
                state: State::NeedsHuman,
                also_known_as: vec![alias.clone()],
                area: Some("code".into()),
            },
            findings: vec![],
        }
    }

    // Routing spec §3.3 step 2: the issue carries the item's own state, area
    // and aliases, and its old IRI is an alias of it, so the old id still
    // names the item; the old IRI is the create key the search finds.
    #[test]
    fn an_escalated_record_keeps_its_state_area_and_aliases_and_its_old_iri_finds_it() {
        let t = MemIssues::default();
        let (old, alias) = (local(7), local(8));
        let url = t
            .create_escalated(&escalated_record(&old, &alias), &escalation_of(&old), 0)
            .unwrap();
        assert_eq!(url, MemIssues::issue(1));
        let expected = Record {
            id: RecordId(url.clone()),
            project: ProjectId(seq_iri(1)),
            title: "the build is flaky".into(),
            state: State::NeedsHuman,
            also_known_as: vec![old.clone(), alias.clone()],
            area: Some("code".into()),
        };
        assert_eq!(
            t.get_record(&RecordId(old.clone())).unwrap(),
            Some(expected.clone())
        );
        assert_eq!(t.get_record(&RecordId(alias)).unwrap(), Some(expected));
        assert_eq!(t.find_escalated(&old, 0).unwrap(), Some(url));
        assert_eq!(t.find_escalated(&local(9), 0).unwrap(), None);
        assert_eq!(t.creates(), 1);
    }

    // Routing spec §3.3: a rerun searches first and finds the issue the
    // first run made — never a second one.
    #[test]
    fn a_second_escalation_of_an_item_finds_its_issue_and_creates_none() {
        let t = MemIssues::default();
        let (old, alias) = (local(7), local(8));
        let item = escalated_record(&old, &alias);
        let first = t.create_escalated(&item, &escalation_of(&old), 0).unwrap();
        let again = t.create_escalated(&item, &escalation_of(&old), 0).unwrap();
        assert_eq!(again, first);
        assert_eq!(t.creates(), 1);
        assert_eq!(t.list_records(&ProjectId(seq_iri(1))).unwrap().len(), 1);
    }

    // Routing spec §3.3 step 2: a finding keeps its own state, area and
    // security mark, and names the record as the router read it — here, a
    // local record that was escalated before it. A security finding goes
    // to a private repository only; a public one takes any other finding.
    #[test]
    fn an_escalated_finding_keeps_its_state_and_names_its_record_and_security_needs_private() {
        let t = MemIssues::default();
        let p = ProjectId(seq_iri(1));
        let record = t.add_record(&p, "the record, escalated").unwrap();
        let about = |old: &Iri, security: bool, also_known_as: Vec<Iri>| {
            let mut finding = Finding::raise(p.clone(), RecordId(local(3)), "rev", "it fails");
            finding.id = FindingId(old.clone());
            finding.state = FindingState::Assigned;
            finding.assigned_to = Some("bob".into());
            finding.area = Some("code".into());
            finding.security = security;
            finding.also_known_as = also_known_as;
            Outgoing::Finding {
                finding,
                record: RecordSeen {
                    id: record.clone(),
                    title: "t".into(),
                    tier: Tier::Github,
                },
            }
        };
        let old = local(7);
        let item = about(&old, true, vec![local(8)]);
        let url = t.create_escalated(&item, &escalation_of(&old), 0).unwrap();
        assert_eq!(url, MemIssues::issue(2));
        let back = t.get_finding(&FindingId(old.clone())).unwrap().unwrap();
        let Outgoing::Finding {
            finding: mut expected,
            ..
        } = item
        else {
            unreachable!()
        };
        expected.id = FindingId(url);
        expected.record = record.clone();
        expected.also_known_as = vec![old.clone(), local(8)];
        assert_eq!(back, expected);
        let by_alias = t.get_finding(&FindingId(local(8))).unwrap();
        assert_eq!(by_alias, Some(expected));

        t.set_public(true);
        // A rerun finds the issue it made before any visibility check, as
        // the GitHub tracker does: the issue passed it when it was made.
        let again = t.create_escalated(&about(&old, true, vec![local(8)]), &escalation_of(&old), 0);
        assert_eq!(again.unwrap(), MemIssues::issue(2));
        let open = local(9);
        let url = t.create_escalated(&about(&open, false, vec![]), &escalation_of(&open), 0);
        assert_eq!(url.unwrap(), MemIssues::issue(3));
        let secret = local(11);
        let err = t
            .create_escalated(&about(&secret, true, vec![]), &escalation_of(&secret), 0)
            .unwrap_err();
        assert!(
            matches!(err, StoreError::SecurityNotPrivate { .. }),
            "{err:?}"
        );
        assert_eq!(t.find_escalated(&secret, 0).unwrap(), None);
        assert_eq!(t.creates(), 2);
    }

    // Routing spec §3.2, one id namespace: an alias is taken when it is an
    // issue URL of this repository, held here or not, or another issue's
    // alias.
    #[test]
    fn an_alias_is_taken_by_any_issue_url_here_or_by_an_alias_of_one() {
        let t = MemIssues::default();
        let p = ProjectId(seq_iri(1));
        let r = t.add_record(&p, "t").unwrap();
        let elsewhere = Iri::parse("https://github.com/elsewhere/old/issues/7").unwrap();
        t.add_alias(r.iri(), elsewhere.clone()).unwrap();
        assert_eq!(t.alias_taken(r.iri()).unwrap(), Some(MemIssues::issue(1)));
        assert_eq!(
            t.alias_taken(&elsewhere).unwrap(),
            Some(MemIssues::issue(1))
        );
        assert_eq!(
            t.alias_taken(&MemIssues::issue(2)).unwrap(),
            Some(MemIssues::issue(2)),
            "an issue URL of this repository, though no issue 2 is held here"
        );
        assert_eq!(t.alias_taken(&local(8)).unwrap(), None);
    }

    // A stop at step 2, for the router's tests: a create that fails before
    // it lands leaves nothing, and the next one runs.
    #[test]
    fn a_failed_create_lands_nothing_and_fails_once() {
        let t = MemIssues::default();
        let (old, alias) = (local(7), local(8));
        let item = escalated_record(&old, &alias);
        t.set_fail_next_create(true);
        let err = t
            .create_escalated(&item, &escalation_of(&old), 0)
            .unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert_eq!(t.find_escalated(&old, 0).unwrap(), None);
        assert_eq!(t.alias_taken(&old).unwrap(), None);
        assert_eq!(t.creates(), 0);
        assert_eq!(
            t.create_escalated(&item, &escalation_of(&old), 0).unwrap(),
            MemIssues::issue(1)
        );
        assert_eq!(t.creates(), 1);
        // It fails before the search too: GitHub was not reached at all.
        t.set_fail_next_create(true);
        let err = t
            .create_escalated(&item, &escalation_of(&old), 0)
            .unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    // An ambiguous create (GitHub tracker spec §3.3): the issue lands and
    // the answer is lost. A rerun finds it by its create key, once.
    #[test]
    fn a_create_whose_answer_was_lost_lands_and_a_rerun_finds_it() {
        let t = MemIssues::default();
        let (old, alias) = (local(7), local(8));
        let item = escalated_record(&old, &alias);
        t.set_lose_next_create_answer(true);
        let err = t
            .create_escalated(&item, &escalation_of(&old), 0)
            .unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert_eq!(
            t.find_escalated(&old, 0).unwrap(),
            Some(MemIssues::issue(1))
        );
        assert_eq!(t.creates(), 1);
        assert_eq!(
            t.create_escalated(&item, &escalation_of(&old), 0).unwrap(),
            MemIssues::issue(1)
        );
        assert_eq!(t.creates(), 1, "found, not made again");
        let other = local(9);
        assert_eq!(
            t.create_escalated(
                &escalated_record(&other, &local(10)),
                &escalation_of(&other),
                0
            )
            .unwrap(),
            MemIssues::issue(2),
            "the knob fires once"
        );
    }

    #[test]
    fn the_escalation_calls_fail_as_unreachable_when_the_tier_is_down() {
        let t = MemIssues::default();
        let old = local(7);
        t.set_down(true);
        let unreachable = |e: StoreError| matches!(e, StoreError::Unreachable { .. });
        assert!(unreachable(t.find_escalated(&old, 0).unwrap_err()));
        assert!(unreachable(t.alias_taken(&old).unwrap_err()));
        let item = escalated_record(&old, &local(8));
        assert!(unreachable(
            t.create_escalated(&item, &escalation_of(&old), 0)
                .unwrap_err()
        ));
        assert_eq!(t.creates(), 0);
    }
```

In `crates/github/src/tracker.rs`, inside `mod tests`, after `fn an_alias_is_taken_by_an_issue_here_or_by_another_issues_alias` (before `/// The same suites the local stores pass` and `mod contract`), add:

```rust
    // Routing spec §2.6: the router reaches the GitHub tracker through
    // `GithubTier`. It claims an issue URL of this repository under its
    // name now — in any case, with no request — and no other repository's;
    // any issue URL has the form.
    #[test]
    fn the_tracker_as_a_tier_claims_its_own_issue_urls_under_its_name_now() {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        let url = |name: &str| Iri::parse(&format!("https://github.com/{name}/issues/4")).unwrap();
        let (t, _) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        let tier: &dyn GithubTier = &t;
        let asked = fake.state().requests.len();
        assert!(tier.available());
        assert!(tier.claims(&url("acme/widgets")) && tier.claims(&url("Acme/Widgets")));
        assert!(!tier.claims(&url("acme/gadgets")) && tier.issue_form(&url("acme/gadgets")));
        assert!(!tier.claims(&local_iri(4)) && !tier.issue_form(&local_iri(4)));
        assert_eq!(fake.state().requests.len(), asked, "read by form alone");
        fake.rename("acme/gadgets");
        let (t, _) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        let tier: &dyn GithubTier = &t;
        assert!(tier.claims(&url("acme/gadgets")));
        assert!(
            !tier.claims(&url("acme/widgets")),
            "the old name is not claimed by form"
        );
    }

    // Routing spec §3.2, §3.3 step 2, as the router asks them: through the
    // tier, an escalation's issue is made, found by its key, and its old
    // IRI is then a taken alias; the rest is the tracker's own.
    #[test]
    fn the_tracker_as_a_tier_creates_finds_and_checks_an_escalated_issue() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        let tier: &dyn GithubTier = &t;
        let since = now_millis();
        let old = local_iri(7);
        assert_eq!(tier.find_escalated(&old, since).unwrap(), None);
        assert_eq!(tier.alias_taken(&old).unwrap(), None);
        let url = tier
            .create_escalated(&escalated_record(&old), &escalation_of(&old), since)
            .unwrap();
        assert_eq!(url, t.issue_url(1));
        assert_eq!(tier.find_escalated(&old, since).unwrap(), Some(url.clone()));
        assert_eq!(tier.alias_taken(&old).unwrap(), Some(url.clone()));
        assert_eq!(
            tier.items_in_area(&p(), "code").unwrap(),
            vec![(Kind::Record, url.clone())]
        );
        let back = tier.tracker().unwrap().get_record(&RecordId(old)).unwrap();
        assert_eq!(back.map(|r| r.id.0), Some(url));
        assert!(tier.require_private().is_ok());
        fake.state().repos[0].visibility = "public".into();
        assert!(matches!(
            tier.require_private(),
            Err(StoreError::SecurityNotPrivate { .. })
        ));
    }
```

In `crates/cli/src/tiers.rs`, inside `mod tests`, after `fn with_no_binding_the_tier_is_unavailable_naming_the_config_entry` (before `fn an_open_that_fails_with_a_store_error_keeps_it_and_one_that_succeeds_is_kept`), add:

```rust
    // Routing spec §1.3: what an escalation asks of GitHub, with no binding,
    // is the missing tier — never "no issue" or "not taken" — and opens
    // nothing.
    #[test]
    fn with_no_binding_an_escalations_questions_are_the_missing_tier() {
        let lazy = LazyGithub::new(
            None,
            "the config".into(),
            Box::new(|_: &TrackerBinding| unreachable!("no binding, nothing to open")),
        );
        let old = fl_core::ids::seq_iri(7);
        let unavailable = |e: StoreError| {
            matches!(
                e,
                StoreError::Routing(RoutingFault::TierUnavailable {
                    tier: Tier::Github,
                    ..
                })
            )
        };
        assert!(unavailable(lazy.find_escalated(&old, 0).unwrap_err()));
        assert!(unavailable(lazy.alias_taken(&old).unwrap_err()));
        let record = fl_core::model::Record {
            id: fl_core::ids::RecordId(old.clone()),
            project: ProjectId(fl_core::ids::seq_iri(1)),
            title: "t".into(),
            state: fl_core::model::State::NeedsHuman,
            also_known_as: vec![],
            area: None,
        };
        let item = Outgoing::Record {
            record,
            findings: vec![],
        };
        let from = Provenance {
            from: old,
            by: "alice".into(),
            reason: "r".into(),
        };
        assert!(unavailable(
            lazy.create_escalated(&item, &from, 0).unwrap_err()
        ));
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-core --lib mem_issues::tests::`
Expected: FAIL to compile — 43 errors, all of them the missing items: "cannot find type `Outgoing`" / "`Provenance`" (E0425, E0433, E0422; the test module's `use super::*` brings them only once Step 3 imports them), and "no method named `create_escalated`" / "`find_escalated`" / "`alias_taken`" / "`creates`" / "`set_fail_next_create`" / "`set_lose_next_create_answer`" found for struct `mem_issues::MemIssues` (E0599).

Run: `cargo test -p fl-github --lib tracker::tests::the_tracker_as_a_tier`
Expected: FAIL to compile — 3 errors, "cannot find trait `GithubTier` in this scope" (E0405).

Run: `cargo test -p fl-cli --bin fl tiers::tests::`
Expected: FAIL to compile — 5 errors: "cannot find type `Outgoing`" (E0433), "cannot find struct, variant or union type `Provenance`" (E0422), and "no method named `find_escalated`" / "`alias_taken`" / "`create_escalated`" found for struct `tiers::LazyGithub<'a>` (E0599).

- [ ] **Step 3: Implement**

In `crates/core/src/routing.rs`, before `use crate::ids::{Kind, ProjectId, RecordId};` add:

```rust
use crate::escalation::{Outgoing, Provenance};
```

and in `pub trait GithubTier`, after `fn items_in_area(…) -> Result<Vec<(Kind, Iri)>, StoreError>;` (the trait's last method), add:

```rust
    /// The issue an escalation made for the item whose old IRI is `key`
    /// (routing spec §3.3 step 2): every issue, labelled or not, newest
    /// first, back to `since_ms` — the mark's time — less the create-search
    /// margin. `None` when there is none.
    fn find_escalated(&self, key: &Iri, since_ms: u64) -> Result<Option<Iri>, StoreError>;
    /// The issue `alias` already names on GitHub, if any (routing spec §3.2:
    /// one id names one item across both tiers).
    fn alias_taken(&self, alias: &Iri) -> Result<Option<Iri>, StoreError>;
    /// Step 2 of an escalation (routing spec §3.3): searches first, as
    /// `find_escalated`, and answers the issue an earlier run made — given
    /// its labels if that run stopped before them — or creates the issue
    /// with the item's own state, area and aliases, `from.from` as its
    /// create key and first alias, and `from` as where it came from.
    fn create_escalated(
        &self,
        item: &Outgoing,
        from: &Provenance,
        since_ms: u64,
    ) -> Result<Iri, StoreError>;
```

In `crates/github/src/tracker.rs`, replace the top-level import `use fl_core::routing::{ForeignRecord, Tier};` with:

```rust
use fl_core::routing::{ForeignRecord, GithubTier, Tier};
```

and after `impl Handles for GithubTracker { … }` (before `/// The events and body edits fl's own write accounts for.` and `struct Own`), add:

```rust
/// The router's view of this tracker (routing spec §2.6), for a caller that
/// holds it open already — the CLI opens it lazily instead. Each question is
/// answered by the tracker's own method of the same name, except
/// `find_escalated`, which is `find_by_key`'s issue URL.
impl GithubTier for GithubTracker {
    fn available(&self) -> bool {
        true
    }

    /// An issue URL of this repository under its name now, in any case. No
    /// request: a URL under an old name is not claimed, so the router asks
    /// the local tier first and then this tracker, which resolves it.
    fn claims(&self, id: &Iri) -> bool {
        meta::parse_issue_url(id)
            .is_some_and(|(name, _)| name.eq_ignore_ascii_case(&self.repo.full_name))
    }

    fn issue_form(&self, id: &Iri) -> bool {
        meta::is_issue_url(id)
    }

    fn tracker(&self) -> Result<&dyn Tracker, StoreError> {
        Ok(self)
    }

    fn require_private(&self) -> Result<(), StoreError> {
        GithubTracker::require_private(self)
    }

    fn items_in_area(
        &self,
        project: &ProjectId,
        area: &str,
    ) -> Result<Vec<(Kind, Iri)>, StoreError> {
        GithubTracker::items_in_area(self, project, area)
    }

    fn find_escalated(&self, key: &Iri, since_ms: u64) -> Result<Option<Iri>, StoreError> {
        Ok(self.find_by_key(key.as_str(), since_ms)?.map(|i| i.url))
    }

    fn alias_taken(&self, alias: &Iri) -> Result<Option<Iri>, StoreError> {
        GithubTracker::alias_taken(self, alias)
    }

    fn create_escalated(
        &self,
        item: &Outgoing,
        from: &Provenance,
        since_ms: u64,
    ) -> Result<Iri, StoreError> {
        GithubTracker::create_escalated(self, item, from, since_ms)
    }
}
```

In `crates/cli/src/tiers.rs`, after `use fl_core::TieredTracker;` add:

```rust
use fl_core::escalation::{Outgoing, Provenance};
```

and in `impl GithubTier for LazyGithub<'_>`, after `fn items_in_area` (the impl's last method), add:

```rust
    fn find_escalated(&self, key: &Iri, since_ms: u64) -> Result<Option<Iri>, StoreError> {
        self.open()?.find_escalated(key, since_ms)
    }

    fn alias_taken(&self, alias: &Iri) -> Result<Option<Iri>, StoreError> {
        self.open()?.alias_taken(alias)
    }

    fn create_escalated(
        &self,
        item: &Outgoing,
        from: &Provenance,
        since_ms: u64,
    ) -> Result<Iri, StoreError> {
        self.open()?.create_escalated(item, from, since_ms)
    }
```

`find_escalated` calls `GithubTracker`'s `GithubTier` method (it has no inherent one of that name), so the URL mapping lives in one place; `alias_taken` and `create_escalated` call the inherent methods.

In `crates/core/src/mem_issues.rs`, before `use crate::finding::{Finding, FindingState};` add:

```rust
use crate::escalation::{Outgoing, Provenance};
```

In `pub struct MemIssues`, after `asked: Cell<u32>,` add:

```rust
    fail_next_create: Cell<bool>,
    lose_next_create_answer: Cell<bool>,
    creates: Cell<u32>,
```

In `struct Issues`, after `aliases: BTreeMap<Iri, u64>,` add:

```rust
    /// An escalated issue's create key — the item's old IRI — and its
    /// number.
    keys: BTreeMap<Iri, u64>,
```

In `impl MemIssues`, after `pub fn asked(&self) -> u32 { … }` (before `pub fn issue(n: u64) -> Iri`), add:

```rust
    /// The next `create_escalated` fails as unreachable before it reads or
    /// writes anything, so nothing lands: a stop before the create. Once.
    pub fn set_fail_next_create(&self, fail: bool) {
        self.fail_next_create.set(fail);
    }

    /// The next `create_escalated` that creates lands its issue, then fails
    /// as unreachable: a create whose answer was lost. Once.
    pub fn set_lose_next_create_answer(&self, lose: bool) {
        self.lose_next_create_answer.set(lose);
    }

    /// How many issues `create_escalated` made — a found one is not made.
    pub fn creates(&self) -> u32 {
        self.creates.get()
    }

    fn unreachable() -> StoreError {
        StoreError::Unreachable {
            store: LABEL.into(),
            cause: "connection refused".into(),
        }
    }
```

Replace `fn up` with:

```rust
    fn up(&self) -> Result<(), StoreError> {
        if self.down.get() {
            return Err(Self::unreachable());
        }
        Ok(())
    }
```

In `impl GithubTier for MemIssues`, after `fn items_in_area` (the impl's last method), add:

```rust
    /// ⚠ This tier has no clock, so `since_ms` is not read: every issue an
    /// escalation made is found. The GitHub tracker's own tests cover the
    /// search's margin.
    fn find_escalated(&self, key: &Iri, _since_ms: u64) -> Result<Option<Iri>, StoreError> {
        self.up()?;
        Ok(self.inner.borrow().keys.get(key).map(|n| Self::issue(*n)))
    }

    /// The one id namespace, as the GitHub tracker keeps it (routing spec
    /// §3.2): any issue URL of this repository is taken — it names that
    /// issue, held here or not — and so is another issue's alias.
    fn alias_taken(&self, alias: &Iri) -> Result<Option<Iri>, StoreError> {
        self.up()?;
        if let Some(n) = alias
            .as_str()
            .strip_prefix(ISSUES)
            .and_then(|n| n.parse::<u64>().ok())
        {
            return Ok(Some(Self::issue(n)));
        }
        Ok(self
            .inner
            .borrow()
            .aliases
            .get(alias)
            .map(|n| Self::issue(*n)))
    }

    /// As the GitHub tracker: the search first — by the create key, which
    /// is the old IRI — then the create, with the item's own state, area
    /// and project, the old IRI and the item's aliases as its aliases, and
    /// a finding's record as the router read it. ⚠ `since_ms` is not read,
    /// as in `find_escalated`.
    fn create_escalated(
        &self,
        item: &Outgoing,
        from: &Provenance,
        _since_ms: u64,
    ) -> Result<Iri, StoreError> {
        self.up()?;
        if self.fail_next_create.replace(false) {
            return Err(Self::unreachable());
        }
        if let Some(n) = self.inner.borrow().keys.get(&from.from) {
            return Ok(Self::issue(*n));
        }
        if let Outgoing::Finding { finding, .. } = item
            && finding.security
            && self.public.get()
        {
            return Err(StoreError::SecurityNotPrivate {
                repo: "acme/widgets".into(),
                visibility: "public".into(),
            });
        }
        let n = self.mint();
        let url = Self::issue(n);
        let mut s = self.inner.borrow_mut();
        let mut aliases = vec![from.from.clone()];
        match item {
            Outgoing::Record { record, .. } => {
                aliases.extend(record.also_known_as.iter().cloned());
                let mut record = record.clone();
                record.id = RecordId(url.clone());
                record.also_known_as = aliases.clone();
                s.records.insert(n, record);
            }
            Outgoing::Finding { finding, record } => {
                aliases.extend(finding.also_known_as.iter().cloned());
                let mut finding = finding.clone();
                finding.id = FindingId(url.clone());
                finding.record = record.id.clone();
                finding.also_known_as = aliases.clone();
                s.findings.insert(n, finding);
            }
        }
        for alias in aliases {
            s.aliases.insert(alias, n);
        }
        s.keys.insert(from.from.clone(), n);
        self.creates.set(self.creates.get() + 1);
        if self.lose_next_create_answer.replace(false) {
            return Err(Self::unreachable());
        }
        Ok(url)
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-core --lib mem_issues::tests::`
Expected: PASS — 11 tests, among them `an_escalated_record_keeps_its_state_area_and_aliases_and_its_old_iri_finds_it`, `a_second_escalation_of_an_item_finds_its_issue_and_creates_none`, `an_escalated_finding_keeps_its_state_and_names_its_record_and_security_needs_private`, `an_alias_is_taken_by_any_issue_url_here_or_by_an_alias_of_one`, `a_failed_create_lands_nothing_and_fails_once`, `a_create_whose_answer_was_lost_lands_and_a_rerun_finds_it` and `the_escalation_calls_fail_as_unreachable_when_the_tier_is_down`.

Run: `cargo test -p fl-github --lib tracker::tests::`
Expected: PASS — 127 tests, among them `the_tracker_as_a_tier_claims_its_own_issue_urls_under_its_name_now` and `the_tracker_as_a_tier_creates_finds_and_checks_an_escalated_issue`.

Run: `cargo test -p fl-cli --bin fl tiers::tests::`
Expected: PASS — 4 tests, among them `with_no_binding_an_escalations_questions_are_the_missing_tier`.

- [ ] **Step 5: Mutation checks**

The GitHub tracker's filter is `cargo test -p fl-github --lib tracker::tests::<name>`: "claims" is `the_tracker_as_a_tier_claims_its_own_issue_urls_under_its_name_now`, "tier" is `the_tracker_as_a_tier_creates_finds_and_checks_an_escalated_issue`. The CLI's is `cargo test -p fl-cli --bin fl tiers::tests::with_no_binding_an_escalations_questions_are_the_missing_tier` ("lazy"). `MemIssues`' is `cargo test -p fl-core --lib mem_issues::tests::<name>`: "record" is `an_escalated_record_keeps_its_state_area_and_aliases_and_its_old_iri_finds_it`, "rerun" is `a_second_escalation_of_an_item_finds_its_issue_and_creates_none`, "finding" is `an_escalated_finding_keeps_its_state_and_names_its_record_and_security_needs_private`, "alias" is `an_alias_is_taken_by_any_issue_url_here_or_by_an_alias_of_one`, "failed" is `a_failed_create_lands_nothing_and_fails_once`, "lost" is `a_create_whose_answer_was_lost_lands_and_a_rerun_finds_it`, "down" is `the_escalation_calls_fail_as_unreachable_when_the_tier_is_down`.

1. `GithubTracker`'s `available`: `false` → claims red.
2. `claims`' name check: `.is_some_and(|(name, _)| true)` → claims red (`acme/gadgets` claimed). Its case: `name == self.repo.full_name` → claims red (`Acme/Widgets` not claimed). By form alone: `matches!(self.owner(id), Ok(Owner::Ours(_)))` → claims red (a request is sent for another name).
3. `issue_form`: `false` → claims red.
4. `tracker`: `Err(StoreError::Backend("x".into()))` → tier red.
5. Each delegation of `GithubTracker`'s impl: `require_private` → `Ok(())`, `items_in_area` → `Ok(vec![])`, `find_escalated` → `Ok(None)`, `alias_taken` → `Ok(None)`, `create_escalated` → `Ok(from.from.clone())` → tier red, each.
6. Each delegation of `LazyGithub`'s: `find_escalated` → `Ok(None)`, `alias_taken` → `Ok(None)`, `create_escalated` → `Ok(from.from.clone())` → lazy red, each (an answer with no binding, not the missing tier).
7. `MemIssues`' search first: delete the `if let Some(n) = self.inner.borrow().keys.get(&from.from) { … }` block → rerun red (two creates) and lost red.
8. The search before the security check: move the security `if let Outgoing::Finding { .. } … { return Err(…) }` above the search → finding red (the rerun of a security finding's escalation, now public, is refused instead of found).
9. The old IRI recorded: `let mut aliases = vec![];` → record red (`get_record` of the old IRI is `NotOwned`).
10. The aliases recorded: delete the `for alias in aliases { s.aliases.insert(alias, n); }` loop → record red. The record's own aliases: delete `aliases.extend(record.also_known_as.iter().cloned());` → record red. The finding's own: delete `aliases.extend(finding.also_known_as.iter().cloned());` → finding red.
11. The key recorded: delete `s.keys.insert(from.from.clone(), n);` → record red (`find_escalated` answers `None`).
12. The item's own state: add `record.state = State::Todo;` after `record.id = RecordId(url.clone());` → record red.
13. A finding's record from `RecordSeen`: delete `finding.record = record.id.clone();` → finding red.
14. The security refusal's two conjuncts: delete `&& finding.security` → finding red (a plain finding refused on a public repository); delete `&& self.public.get()` → finding red (a security finding refused on a private one).
15. The fail knob: delete the `if self.fail_next_create.replace(false) { … }` block → failed red. One-shot: `.get()` for `.replace(false)` → failed red. Before the search: move the block after the search → failed red (an existing issue is answered instead of the failure).
16. The lost-answer knob: delete the `if self.lose_next_create_answer.replace(false) { … }` block → lost red. One-shot: `.get()` for `.replace(false)` → lost red ("the knob fires once").
17. The count: delete `self.creates.set(self.creates.get() + 1);` → rerun red.
18. `alias_taken`'s issue-URL arm: delete the `if let Some(n) = … { return Ok(Some(Self::issue(n))); }` block → alias red (issue 1's own URL is free); a held check put back, `&& self.inner.borrow().records.contains_key(&n)` after the `parse` → alias red (issue 2's URL, which this tier does not hold, is free). Its alias arm: `Ok(None)` → alias red.
19. Each of the three new `self.up()?;` lines deleted → down red, each.

Not observable: none — every guard above went red.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1133 passed, 18 ignored.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/routing.rs crates/core/src/mem_issues.rs crates/github/src/tracker.rs crates/cli/src/tiers.rs
git commit -m "feat(core): what the router asks of the GitHub tier for an escalation

GithubTier gains find_escalated, alias_taken and create_escalated
(routing spec §3.2, §3.3 step 2). The GitHub tracker implements
GithubTier itself, delegating to its own methods, so a caller holding it
open can drive the router; the CLI's lazy tier delegates through open().
MemIssues keeps create keys, records the old IRI and the item's aliases
as aliases, refuses a security finding when public, counts its creates,
and has two one-shot knobs: a create that fails before it lands, and one
whose answer is lost after it lands.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

---

### Task 6: The router follows a tombstone and sees a mark

Once an item is escalated the local store answers its old IRI with `StoreError::Escalated { from, to }`, "which the router follows and the CLI never shows when it can follow it" (routing spec §3.6); a `urn:uuid:` IRI whose tombstone the local store holds means "the tombstone's target" (§2.2). This task makes the router follow it, in `route` only and once (plan ruling 8): `route`'s action now takes the id it should act on, so the hop asks GitHub for the tombstone's target by that target's own IRI, and GitHub's answer for it is the answer — never "held elsewhere", and a GitHub that cannot be reached is that tier's error (§2.5). A finding's record is shown as the record now is (plan ruling 9; §2.5 "Evidence", §3.5): `get_finding` and `findings` replace a record IRI the GitHub tier does not claim by its tombstone's target, so new evidence and `finding list --record` name the issue, while the stored reference stays as raised — stores ignore the record on `update_finding`. The router also gains `escalations`, the local tier's marks and tombstones (plan ruling 5), and `escalating`, which reads a local item's mark for the lists (§2.4) and for the CLI's checks in later tasks without asking GitHub. "An item marked 'escalating' and the GitHub issue whose alias is its IRI are one item, not two: the lookup returns the GitHub issue once it exists" (§2.2; plan ruling 23): for an id the local tier holds marked, `route` asks GitHub by the escalation's own search — the item's primary IRI, back to the mark's time — and acts on the issue when it exists and GitHub reads it as an fl item. Otherwise — no issue, an issue fl cannot read (a stop between the create and its labels leaves one), or a GitHub that cannot be opened or reached — the local item answers, as it last was, and the local store refuses every write to it (§3.3 step 1): the redirect is best effort, and no write lands in two places. A merged list leaves out a marked local item when a GitHub item in the same list names it as an alias, with no further request; a list of the local tier alone keeps it. `items_naming_area` needs nothing new: the local lists leave tombstones out already (Task 1, Task 2), and a test pins it.

**Blast radius:** `TieredTracker` gains a public field, so every struct literal of it changes: in `crates/core/src/tiered.rs` the tests' `W::router()`, the literal in `a_github_create_checks_its_project_in_the_catalog`, and the conformance fixture `Over::with`; `crates/cli/src/main.rs` (`escalations: &store` — `RedbStore` implements `Escalations` since Task 2); `crates/exec/tests/tiered_evidence.rs` (`escalations: &local`; its behaviour is Task 10's). `grep -rn "TieredTracker {" crates/` finds no other. `route` is `pub(crate)`; its six callers, all in `tiered.rs` (`record_of`, `get_record`, `set_record_state`, `get_finding`, `update_finding`, `add_alias`), now act on the id they are given; `update_finding` clones the finding to give it that id. Behaviour changes only for an item with a tombstone or a mark, which nothing outside tests creates until Task 7: every other id routes exactly as before, and a finding whose record has no tombstone reads unchanged. `get_finding` and `findings` make one local `tombstone_of` read per finding whose record IRI GitHub does not claim — a local read, never a GitHub call. Every `route` call whose id GitHub does not claim makes one local `mark_of` read; only a marked item adds GitHub calls (the tier's `tracker()`, one `find_escalated` and, when it finds an issue, one read of it). `records` and `findings` — and so `list_records`, `list_findings` and every caller of the merged lists, `prepare_escalation`'s open findings among them (Task 7) — drop a marked local item whose issue the same list holds, after one local `mark_of` read per local item a GitHub item names as an alias.

**Files:**
- Modify: `crates/core/src/tiered.rs` (`TieredTracker.escalations`; `route` passes the id, follows a tombstone and reaches a marked item's issue; its callers; `escalating`; `issue_of_marked`; `as_now`, used by `get_finding` and `findings`; `one_copy`, used by `records` and `findings`; tests and the fixtures' literals)
- Modify: `crates/cli/src/main.rs` (`escalations: &store`)
- Modify: `crates/exec/tests/tiered_evidence.rs` (`escalations: &local`)

**Interfaces:**
- Consumes: `Escalations`, `Mark`, `Outgoing`, `Provenance`, `StoreError::{Escalated, Escalating}`, `impl Escalations for MemStore` (Task 1); `impl Escalations for RedbStore` (Task 2); `GithubTier::{find_escalated, create_escalated}` and `impl GithubTier for MemIssues`' versions (Task 5); `RecordSeen`, `MemIssues::{issue, asked, set_unbound, set_down}`, `Catalog::kind_of` (existing).
- Produces (exactly as "Interfaces every task shares"): `TieredTracker` gains `pub escalations: &'a dyn Escalations`; `pub(crate) fn route<T>(&self, id: &Iri, act: impl Fn(&dyn Tracker, &Iri) -> Result<T, StoreError>) -> Result<(Tier, T), StoreError>`; `pub fn escalating(&self, id: &Iri) -> Result<Option<Mark>, StoreError>` (`None` for an id GitHub claims, with no GitHub call; otherwise the local `mark_of`).
- Produces, beyond the shared block: nothing public. `fn as_now(&self, finding: Finding) -> Result<Finding, StoreError>`, `fn issue_of_marked(&self, id: &Iri) -> Result<Option<Iri>, StoreError>` and `fn one_copy<T>(&self, items: Vec<(Tier, T)>, names: impl Fn(&T) -> (&Iri, &Vec<Iri>)) -> Result<Vec<(Tier, T)>, StoreError>` are private. `TieredTracker::findings` and `Tracker::get_finding`/`list_findings` on the router now return each finding's record as the record now is. Every routed read and write of a marked item's id (any of its names) reaches its issue once the issue exists and GitHub reads it as an fl item — `Tier::Github`; in every other case, GitHub unbound or unreachable among them, it reaches the local item, which refuses a write with `Escalating`; `records`/`findings` with `only` = `None` list a marked item whose issue they hold once, as the issue.
- Unique phrases: none new. The tests assert `so this store refuses to change it` (Task 1's `Escalating`), and otherwise match on error variants.

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/tiered.rs`, inside `mod tests`, after `use crate::MemStore;`, add:

```rust
    use crate::escalation::{Escalations, Mark, Outgoing, Provenance};
```

In `impl W`, in `fn router`, after `github: &self.issues,`, add `escalations: &self.local,`; after `fn record`, add:

```rust
        /// `old`, a local item, marked and then replaced by a tombstone that
        /// points to `to` (routing spec §3.3 steps 1 and 3).
        fn tombstone(&self, old: &Iri, to: &Iri) {
            self.local.mark(old, &mark()).unwrap();
            self.local.tombstone(old, to).unwrap();
        }

        /// The local record `old` escalated by hand: its issue made in the
        /// GitHub tier, then the mark and the tombstone.
        fn escalated(&self, old: &RecordId, title: &str) -> RecordId {
            let issue = self
                .issues
                .add_record_with_area(&self.p, title, Some("code"))
                .unwrap();
            self.tombstone(old.iri(), issue.iri());
            issue
        }

        /// The issue an escalation of `item` makes (routing spec §3.3 step
        /// 2): keyed by the item's IRI, which is its first alias, and with no
        /// tombstone yet — a stop before step 3.
        fn issued(&self, item: Outgoing) -> Iri {
            let from = Provenance {
                from: item.id().clone(),
                by: mark().by,
                reason: mark().reason,
            };
            self.issues
                .create_escalated(&item, &from, mark().at_ms)
                .unwrap()
        }
```

and after `impl W`'s closing brace, before `/// Every project routed by one map`, add:

```rust
    fn mark() -> Mark {
        Mark {
            by: "alice".into(),
            reason: "a person decides".into(),
            at_ms: 1,
        }
    }
```

In `a_github_create_checks_its_project_in_the_catalog`, in the `TieredTracker { … }` literal, after `github: &w.issues,`, add `escalations: &w.local,`. In `impl crate::conformance::Fixture for Over`, in the `TieredTracker { … }` literal, after `github: &self.issues,`, add `escalations: &self.local,`.

After `a_finding_is_written_about_the_record_its_placement_checked`, before `/// The tracker suites make items without an area`, add:

```rust
    // Routing spec §2.2, §3.6: an escalated item's old IRI reaches its
    // issue — a read, a write, and a finding raised about it, which is
    // written about the issue and crosses tiers through the router's proof.
    #[test]
    fn a_tombstoned_id_is_followed_to_its_issue() {
        let w = world();
        let t = w.router();
        let old = w.record("code", "fix");
        let issue = w.escalated(&old, "fix");
        assert_eq!(t.get_record(&old).unwrap().unwrap().id, issue);
        t.set_record_state(&old, State::Doing).unwrap();
        assert_eq!(
            w.issues.get_record(&issue).unwrap().unwrap().state,
            State::Doing
        );
        let mut stays = Finding::raise(w.p.clone(), old.clone(), "rev", "stays");
        stays.area = Some("code".into());
        let at = t.place_finding(&stays, None).unwrap();
        assert_eq!(
            (at.record().id.clone(), at.record().tier, at.at().tier()),
            (issue.clone(), Tier::Github, Tier::Local)
        );
        let id = t.add_finding(stays).unwrap();
        assert_eq!(t.tier_of(id.iri()), Tier::Local);
        assert_eq!(w.local.get_finding(&id).unwrap().unwrap().record, issue);
        let mut moves = Finding::raise(w.p.clone(), old.clone(), "rev", "moves");
        moves.area = Some("design".into());
        let id = t.add_finding(moves).unwrap();
        assert_eq!(w.issues.get_finding(&id).unwrap().unwrap().record, issue);
        // A finding's old IRI: read, updated — from a copy read before the
        // escalation — and given an alias, on GitHub.
        let mut f = Finding::raise(w.p.clone(), old.clone(), "rev", "c");
        f.area = Some("code".into());
        let old_f = t.add_finding(f).unwrap();
        let mut before = t.get_finding(&old_f).unwrap().unwrap();
        let gh_f = w
            .issues
            .add_finding(Finding::raise(w.p.clone(), issue.clone(), "rev", "c"))
            .unwrap();
        w.tombstone(old_f.iri(), gh_f.iri());
        assert_eq!(t.get_finding(&old_f).unwrap().unwrap().id, gh_f);
        before.withdraw("no").unwrap();
        t.update_finding(&before).unwrap();
        assert_eq!(
            w.issues.get_finding(&gh_f).unwrap().unwrap().state,
            FindingState::Withdrawn
        );
        let alias = crate::ids::seq_iri(88);
        t.add_alias(old_f.iri(), alias.clone()).unwrap();
        assert_eq!(t.get_finding(&FindingId(alias)).unwrap().unwrap().id, gh_f);
    }

    // Routing spec §2.5: GitHub that cannot be reached while a tombstone is
    // followed is that tier's error, never "held elsewhere" or "no such
    // record".
    #[test]
    fn following_a_tombstone_to_an_unbound_or_unreachable_github_is_that_tiers_error() {
        let w = world();
        let t = w.router();
        let old = w.record("code", "fix");
        w.escalated(&old, "fix");
        w.issues.set_unbound(true);
        let err = t.get_record(&old).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "{err:?}"
        );
        let err = t.set_record_state(&old, State::Doing).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "{err:?}"
        );
        w.issues.set_unbound(false);
        w.issues.set_down(true);
        let err = t.get_record(&old).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    // A tombstone is followed once, to GitHub: its target is GitHub's answer
    // for that id, even when GitHub does not hold it — never "held
    // elsewhere", and never a second hop through the local tier.
    #[test]
    fn a_tombstone_is_followed_once_and_its_target_is_githubs_answer() {
        let w = world();
        let t = w.router();
        let old = w.record("code", "gone");
        let gone = MemIssues::issue(42);
        w.tombstone(old.iri(), &gone);
        assert_eq!(t.get_record(&old).unwrap(), None, "GitHub's answer");
        let err = t.set_record_state(&old, State::Doing).unwrap_err();
        assert!(
            matches!(&err, StoreError::NoSuchRecord(r) if r.iri() == &gone),
            "{err:?}"
        );
        let first = w.record("code", "first");
        let second = w.record("code", "second");
        w.escalated(&second, "second");
        w.tombstone(first.iri(), second.iri());
        let err = t.get_record(&first).unwrap_err();
        assert!(
            matches!(&err, StoreError::NotOwned { id, .. } if id == second.iri()),
            "GitHub's answer for {second}: {err:?}"
        );
    }

    // Routing spec §2.5 ("Evidence"), §3.5: a finding's record is shown as
    // the record now is — its issue, once escalated — in either tier, while
    // the stored reference stays as it was raised.
    #[test]
    fn a_findings_record_reads_as_its_issue_once_escalated() {
        let w = world();
        let t = w.router();
        let old = w.record("code", "fix");
        let mut f = Finding::raise(w.p.clone(), old.clone(), "rev", "here");
        f.area = Some("code".into());
        let local_f = t.add_finding(f).unwrap();
        let mut g = Finding::raise(w.p.clone(), old.clone(), "rev", "there");
        g.area = Some("design".into());
        let gh_f = t.add_finding(g).unwrap();
        let issue = w.escalated(&old, "fix");
        assert_eq!(t.get_finding(&local_f).unwrap().unwrap().record, issue);
        assert_eq!(t.get_finding(&gh_f).unwrap().unwrap().record, issue);
        let listed: Vec<RecordId> = t
            .findings(&w.p, None)
            .unwrap()
            .into_iter()
            .map(|(_, f)| f.record)
            .collect();
        assert_eq!(listed, vec![issue.clone(), issue.clone()]);
        assert_eq!(t.list_findings(&w.p).unwrap()[0].record, issue);
        let mut back = t.get_finding(&local_f).unwrap().unwrap();
        back.withdraw("no").unwrap();
        t.update_finding(&back).unwrap();
        let stored = w.local.get_finding(&local_f).unwrap().unwrap();
        assert_eq!(
            (stored.state, stored.record),
            (FindingState::Withdrawn, old),
            "stores ignore the record on an update"
        );
    }

    // Routing spec §2.2: an issue of the bound repository is GitHub's, even
    // when the local tier holds its URL as an alias of an escalating or
    // escalated item — no mark is read for it, and no tombstone rewrites it.
    #[test]
    fn an_issue_url_is_never_read_through_a_local_mark_or_tombstone() {
        let w = world();
        let t = w.router();
        let gh = w.record("design", "on github");
        let local = w.record("code", "local");
        let unmarked = w.record("code", "unmarked");
        w.local.add_alias(local.iri(), gh.iri().clone()).unwrap();
        w.local.mark(local.iri(), &mark()).unwrap();
        let asked = w.issues.asked();
        assert_eq!(t.escalating(local.iri()).unwrap(), Some(mark()));
        assert_eq!(t.escalating(unmarked.iri()).unwrap(), None);
        assert_eq!(t.escalating(gh.iri()).unwrap(), None);
        assert_eq!(w.issues.asked(), asked, "GitHub is not asked for a mark");
        let issue = w
            .issues
            .add_record_with_area(&w.p, "local", Some("code"))
            .unwrap();
        w.local.tombstone(local.iri(), issue.iri()).unwrap();
        let mut f = Finding::raise(w.p.clone(), gh.clone(), "rev", "c");
        f.area = Some("design".into());
        let id = t.add_finding(f).unwrap();
        assert_eq!(t.get_finding(&id).unwrap().unwrap().record, gh);
        assert_eq!(t.findings(&w.p, None).unwrap()[0].1.record, gh);
    }

    // Routing spec §2.4, §3.6: a marked item reads as itself and refuses a
    // write through the router; a tombstoned item is not listed.
    #[test]
    fn a_marked_item_refuses_a_write_and_a_tombstoned_one_is_not_listed() {
        let w = world();
        let t = w.router();
        let marked = w.record("code", "marked");
        let mut f = Finding::raise(w.p.clone(), marked.clone(), "rev", "c");
        f.area = Some("code".into());
        let kept = t.add_finding(f.clone()).unwrap();
        let gone_f = t.add_finding(f).unwrap();
        w.local.mark(marked.iri(), &mark()).unwrap();
        let err = t.set_record_state(&marked, State::Doing).unwrap_err();
        assert!(
            matches!(&err, StoreError::Escalating { id, .. } if id == marked.iri()),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains("so this store refuses to change it"),
            "{err}"
        );
        assert_eq!(t.get_record(&marked).unwrap().unwrap().id, marked);
        let gone = w.record("code", "gone");
        let issue = w.escalated(&gone, "gone");
        w.tombstone(gone_f.iri(), &MemIssues::issue(50));
        let records: Vec<(Tier, RecordId)> = t
            .records(&w.p, None)
            .unwrap()
            .into_iter()
            .map(|(tier, r)| (tier, r.id))
            .collect();
        assert_eq!(records, vec![(Tier::Local, marked), (Tier::Github, issue)]);
        let findings: Vec<(Tier, FindingId)> = t
            .findings(&w.p, None)
            .unwrap()
            .into_iter()
            .map(|(tier, f)| (tier, f.id))
            .collect();
        assert_eq!(findings, vec![(Tier::Local, kept)]);
    }

    // Routing spec §1.2: an escalated item is named once, by the tier it
    // lives in now.
    #[test]
    fn an_escalated_item_names_its_area_only_from_its_issue() {
        let w = world();
        let t = w.router();
        let old = w.record("code", "fix");
        let issue = w.escalated(&old, "fix");
        assert_eq!(
            t.items_naming_area(&w.p, "code").unwrap(),
            vec![(Tier::Github, Kind::Record, issue.0)]
        );
    }

    // Routing spec §2.2: a marked item and the issue whose alias is its IRI
    // are one item. Until the issue exists the old IRI reads the local item;
    // once it does, any name of the item reads, writes and places a finding
    // about the issue. An unmarked item asks nothing of GitHub.
    #[test]
    fn a_marked_item_is_its_issue_once_the_issue_exists() {
        let w = world();
        let t = w.router();
        let r = w.record("code", "fix");
        let alias = crate::ids::seq_iri(77);
        w.local.add_alias(r.iri(), alias.clone()).unwrap();
        let mut f = Finding::raise(w.p.clone(), r.clone(), "rev", "c");
        f.area = Some("code".into());
        let f = t.add_finding(f).unwrap();
        let asked = w.issues.asked();
        assert_eq!(t.get_record(&r).unwrap().unwrap().id, r);
        assert_eq!(w.issues.asked(), asked, "an unmarked item asks nothing");
        w.local.mark(r.iri(), &mark()).unwrap();
        w.local.mark(f.iri(), &mark()).unwrap();
        assert_eq!(t.get_record(&r).unwrap().unwrap().id, r, "no issue yet");
        assert_eq!(t.get_finding(&f).unwrap().unwrap().id, f, "no issue yet");
        let record = w.local.get_record(&r).unwrap().unwrap();
        let issue = RecordId(w.issued(Outgoing::Record {
            record,
            findings: vec![],
        }));
        let finding = w.local.get_finding(&f).unwrap().unwrap();
        let seen = RecordSeen {
            id: issue.clone(),
            title: "fix".into(),
            tier: Tier::Github,
        };
        let f_issue = FindingId(w.issued(Outgoing::Finding {
            finding,
            record: seen,
        }));
        assert_eq!(t.get_record(&r).unwrap().unwrap().id, issue);
        assert_eq!(t.get_record(&RecordId(alias)).unwrap().unwrap().id, issue);
        assert_eq!(t.get_finding(&f).unwrap().unwrap().id, f_issue);
        let mut about = Finding::raise(w.p.clone(), r.clone(), "rev", "about");
        about.area = Some("code".into());
        let at = t.place_finding(&about, None).unwrap();
        assert_eq!(
            (at.record().id.clone(), at.record().tier),
            (issue.clone(), Tier::Github)
        );
        t.set_record_state(&r, State::Doing).unwrap();
        assert_eq!(
            w.issues.get_record(&issue).unwrap().unwrap().state,
            State::Doing
        );
        assert_eq!(
            w.local.get_record(&r).unwrap().unwrap().state,
            State::Todo,
            "the local item is not written"
        );
    }

    // Routing spec §2.2, §3.3 step 1: a marked item's issue takes over only
    // when GitHub answers for it. A GitHub that cannot be opened or reached
    // leaves the local item — even when the issue exists — which reads as
    // itself and refuses a write.
    #[test]
    fn a_marked_item_with_github_unbound_or_unreachable_is_the_local_item() {
        let w = world();
        let t = w.router();
        let r = w.record("code", "fix");
        w.local.mark(r.iri(), &mark()).unwrap();
        let record = w.local.get_record(&r).unwrap().unwrap();
        w.issued(Outgoing::Record {
            record,
            findings: vec![],
        });
        for (unbound, down) in [(true, false), (false, true)] {
            w.issues.set_unbound(unbound);
            w.issues.set_down(down);
            let read = t.get_record(&r).unwrap().unwrap();
            assert_eq!(read.id, r, "unbound {unbound}, down {down}");
            let err = t.set_record_state(&r, State::Doing).unwrap_err();
            assert!(
                matches!(&err, StoreError::Escalating { id, .. } if id == r.iri()),
                "unbound {unbound}, down {down}: {err:?}"
            );
        }
    }

    // Routing spec §2.2, §2.4: a merged list shows a marked item whose issue
    // exists once — as the issue; a list of the local tier alone still shows
    // the local item. An unmarked local item stays listed even when an issue
    // names it among its aliases.
    #[test]
    fn a_merged_list_shows_a_marked_item_with_an_issue_once() {
        let w = world();
        let t = w.router();
        let r = w.record("code", "fix");
        let other = w.record("code", "other");
        let mut f = Finding::raise(w.p.clone(), r.clone(), "rev", "c");
        f.area = Some("code".into());
        let f = t.add_finding(f).unwrap();
        w.local.mark(r.iri(), &mark()).unwrap();
        w.local.mark(f.iri(), &mark()).unwrap();
        let record = w.local.get_record(&r).unwrap().unwrap();
        let issue = RecordId(w.issued(Outgoing::Record {
            record,
            findings: vec![],
        }));
        let finding = w.local.get_finding(&f).unwrap().unwrap();
        let seen = RecordSeen {
            id: issue.clone(),
            title: "fix".into(),
            tier: Tier::Github,
        };
        let f_issue = FindingId(w.issued(Outgoing::Finding {
            finding,
            record: seen,
        }));
        let stray = w
            .issues
            .add_record_with_area(&w.p, "stray", Some("code"))
            .unwrap();
        w.issues.add_alias(stray.iri(), other.0.clone()).unwrap();
        let records = |only: Option<Tier>| -> Vec<(Tier, RecordId)> {
            let got = t.records(&w.p, only).unwrap();
            got.into_iter().map(|(tier, r)| (tier, r.id)).collect()
        };
        assert_eq!(
            records(None),
            vec![
                (Tier::Local, other.clone()),
                (Tier::Github, issue),
                (Tier::Github, stray)
            ]
        );
        assert_eq!(
            records(Some(Tier::Local)),
            vec![(Tier::Local, r), (Tier::Local, other)]
        );
        let findings = |only: Option<Tier>| -> Vec<(Tier, FindingId)> {
            let got = t.findings(&w.p, only).unwrap();
            got.into_iter().map(|(tier, f)| (tier, f.id)).collect()
        };
        assert_eq!(findings(None), vec![(Tier::Github, f_issue)]);
        assert_eq!(findings(Some(Tier::Local)), vec![(Tier::Local, f)]);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-core --lib tiered::tests::`
Expected: FAIL to compile — `error[E0560]`: `TieredTracker` has no field named `escalations` (the three literals), and `error[E0599]`: no method named `escalating` found for `TieredTracker`.

- [ ] **Step 3: Implement**

In `crates/core/src/tiered.rs`, before `use crate::finding::Finding;`, add:

```rust
use crate::escalation::{Escalations, Mark};
```

In `pub struct TieredTracker`, after the `github` field, add:

```rust
    /// The local tier's marks and tombstones (§3.6): the local store.
    pub escalations: &'a dyn Escalations,
```

Replace `route`, with its doc comment, by:

```rust
    /// `act` in the tier that owns `id` (§2.2), given the id to act on: an
    /// issue of the bound repository in GitHub; any other id in the local
    /// tier, then — if the local tier never held it — in GitHub, whose alias
    /// scan finds an item another machine moved there. With no binding, an
    /// issue URL the local tier does not hold is the missing tier's (§1.3).
    /// An id the local tier holds as a tombstone is GitHub's, asked once
    /// for the tombstone's target (§3.6): GitHub's answer for that issue is
    /// the answer. An id the local tier holds marked escalating is its
    /// issue once the issue exists and GitHub reads it (§2.2); otherwise,
    /// best effort, the local item, which refuses every write.
    /// ⚠ An id neither tier holds is `Elsewhere`, never `NotOwned`; outside
    /// that best effort, a tier that cannot be reached is its own error,
    /// never "not held".
    pub(crate) fn route<T>(
        &self,
        id: &Iri,
        act: impl Fn(&dyn Tracker, &Iri) -> Result<T, StoreError>,
    ) -> Result<(Tier, T), StoreError> {
        if self.github.claims(id) {
            return Ok((Tier::Github, act(self.github.tracker()?, id)?));
        }
        if let Some(issue) = self.issue_of_marked(id)? {
            return Ok((Tier::Github, act(self.github.tracker()?, &issue)?));
        }
        let mut searched = match act(self.local, id) {
            Err(StoreError::NotOwned { searched, .. }) => searched,
            Err(StoreError::Escalated { to, .. }) => {
                return Ok((Tier::Github, act(self.github.tracker()?, &to)?));
            }
            other => return Ok((Tier::Local, other?)),
        };
        if !self.github.available() {
            // An issue URL is GitHub's all the same: with no binding it is
            // refused as the missing tier, never as held elsewhere (§1.3).
            if self.github.issue_form(id) {
                self.github.tracker()?;
            }
            return Err(RoutingFault::Elsewhere {
                id: id.clone(),
                searched,
            }
            .into());
        }
        match act(self.github.tracker()?, id) {
            Err(StoreError::NotOwned {
                searched: theirs, ..
            }) => {
                searched.extend(theirs);
                Err(RoutingFault::Elsewhere {
                    id: id.clone(),
                    searched,
                }
                .into())
            }
            other => Ok((Tier::Github, other?)),
        }
    }
```

In `record_of`, replace `self.route(id.iri(), |t| t.get_record(id))?` with:

```rust
self.route(id.iri(), |t, id| t.get_record(&RecordId(id.clone())))?
```

After `record_of`, before `fn check_project`, add:

```rust
    /// The mark on `id` when the local tier holds it marked escalating
    /// (§2.4, §3.3 step 1). An id GitHub claims is GitHub's, so it has none,
    /// and GitHub is not asked.
    pub fn escalating(&self, id: &Iri) -> Result<Option<Mark>, StoreError> {
        if self.tier_of(id) == Tier::Github {
            return Ok(None);
        }
        self.escalations.mark_of(id)
    }

    /// The issue of an item the local tier holds marked escalating, when
    /// its escalation made one that GitHub reads as an fl item (§2.2: a
    /// marked item and the GitHub issue whose alias is its IRI are one
    /// item). The search is the escalation's own — by the item's primary
    /// IRI, back to the mark's time — and only a marked item pays for it.
    /// ⚠ Best effort: no issue found, an issue fl cannot read (a stop
    /// between the create and its labels leaves one), and a GitHub that
    /// cannot be opened or reached all answer `None`, so the local item
    /// answers — as it last was, and refusing every write (§3.3 step 1). No
    /// write lands in two places.
    fn issue_of_marked(&self, id: &Iri) -> Result<Option<Iri>, StoreError> {
        let Some(mark) = self.escalations.mark_of(id)? else {
            return Ok(None);
        };
        let Ok(github) = self.github.tracker() else {
            return Ok(None);
        };
        let kind = self.catalog.kind_of(id)?;
        let primary = match kind {
            Kind::Finding => self
                .local
                .get_finding(&FindingId(id.clone()))?
                .map(|f| f.id.0),
            _ => self
                .local
                .get_record(&RecordId(id.clone()))?
                .map(|r| r.id.0),
        };
        let Some(primary) = primary else {
            return Ok(None);
        };
        let Ok(Some(issue)) = self.github.find_escalated(&primary, mark.at_ms) else {
            return Ok(None);
        };
        let readable = match kind {
            Kind::Finding => github
                .get_finding(&FindingId(issue.clone()))
                .is_ok_and(|f| f.is_some()),
            _ => github
                .get_record(&RecordId(issue.clone()))
                .is_ok_and(|r| r.is_some()),
        };
        Ok(readable.then_some(issue))
    }

    /// `finding` with its record as the record now is (§2.5 "Evidence",
    /// §3.5): a local record escalated since the finding was raised is named
    /// by its issue. The stored reference is not rewritten — stores ignore
    /// the record on an update. An id GitHub claims is never looked up
    /// locally.
    fn as_now(&self, mut finding: Finding) -> Result<Finding, StoreError> {
        if self.tier_of(finding.record.iri()) == Tier::Local
            && let Some(t) = self.escalations.tombstone_of(finding.record.iri())?
        {
            finding.record = RecordId(t.to);
        }
        Ok(finding)
    }
```

Replace `records` and `findings`, with their doc comments, by these, and add `one_copy` after them:

```rust
    /// The project's records in both tiers, or in `only`, each with its
    /// tier (§2.4).
    pub fn records(
        &self,
        project: &ProjectId,
        only: Option<Tier>,
    ) -> Result<Vec<(Tier, Record)>, StoreError> {
        let mut out = Vec::new();
        for tier in tiers(only) {
            let got = self.read(tier, only, |t| t.list_records(project))?;
            out.extend(got.into_iter().map(|r| (tier, r)));
        }
        self.one_copy(out, |r| (r.id.iri(), &r.also_known_as))
    }

    /// The project's findings in both tiers, or in `only`, each with its
    /// tier (§2.4) and its record as the record now is (§2.5).
    pub fn findings(
        &self,
        project: &ProjectId,
        only: Option<Tier>,
    ) -> Result<Vec<(Tier, Finding)>, StoreError> {
        let mut out = Vec::new();
        for tier in tiers(only) {
            for f in self.read(tier, only, |t| t.list_findings(project))? {
                out.push((tier, self.as_now(f)?));
            }
        }
        self.one_copy(out, |f| (f.id.iri(), &f.also_known_as))
    }

    /// `items` without the local copy of a marked item whose issue is in
    /// the same list (§2.2: they are one item, and the issue is the one
    /// shown). The issue names the item's IRI among its aliases, so nothing
    /// more is asked of GitHub. A list of the local tier alone keeps the
    /// copy, listed with its mark (§2.4).
    fn one_copy<T>(
        &self,
        items: Vec<(Tier, T)>,
        names: impl Fn(&T) -> (&Iri, &Vec<Iri>),
    ) -> Result<Vec<(Tier, T)>, StoreError> {
        let issued: std::collections::BTreeSet<Iri> = items
            .iter()
            .filter(|(tier, _)| *tier == Tier::Github)
            .flat_map(|(_, item)| names(item).1.iter().cloned())
            .collect();
        let mut out = Vec::with_capacity(items.len());
        for (tier, item) in items {
            let id = names(&item).0;
            let its_issue_listed = tier == Tier::Local && issued.contains(id);
            if its_issue_listed && self.escalations.mark_of(id)?.is_some() {
                continue;
            }
            out.push((tier, item));
        }
        Ok(out)
    }
```

In `impl Tracker for TieredTracker<'_>`, replace the functions `get_record`, `set_record_state`, `get_finding`, `update_finding` and `add_alias` with, in order:

```rust
    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.route(id.iri(), |t, id| t.get_record(&RecordId(id.clone())))
            .map(|(_, r)| r)
    }
```

```rust
    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        self.route(id.iri(), |t, id| {
            t.set_record_state(&RecordId(id.clone()), state)
        })
        .map(drop)
    }
```

```rust
    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        match self.route(id.iri(), |t, id| t.get_finding(&FindingId(id.clone())))? {
            (_, Some(f)) => Ok(Some(self.as_now(f)?)),
            (_, None) => Ok(None),
        }
    }
```

```rust
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        self.route(finding.id.iri(), |t, id| {
            let mut finding = finding.clone();
            finding.id = FindingId(id.clone());
            t.update_finding(&finding)
        })
        .map(drop)
    }
```

```rust
    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        self.route(primary, |t, id| t.add_alias(id, alias.clone()))
            .map(drop)
    }
```

In `crates/cli/src/main.rs`, in the `fl_core::TieredTracker { … }` literal, after `github: l,`, add:

```rust
        escalations: &store,
```

In `crates/exec/tests/tiered_evidence.rs`, in the `TieredTracker { … }` literal, after `github: &issues,`, add:

```rust
        escalations: &local,
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-core --lib tiered::tests::` then `cargo test --workspace`
Expected: PASS — 38 tests in `tiered::tests`, the two conformance runs among them.

- [ ] **Step 5: Mutation checks**

Each: change → run → red → restore → `cmp` against the saved copy. Every filter is `cargo test -p fl-core --lib tiered::tests::<name>`.

1. The hop: delete the `Err(StoreError::Escalated { to, .. }) => { … }` arm of `route` → `a_tombstoned_id_is_followed_to_its_issue` red (the old IRI reads as `Escalated`).
2. Once: replace the hop's `return Ok((Tier::Github, act(self.github.tracker()?, &to)?));` with `return self.route(&to, &act as &dyn Fn(&dyn Tracker, &Iri) -> Result<T, StoreError>);` (route the target again) → `a_tombstone_is_followed_once_and_its_target_is_githubs_answer` red (a second tombstone is followed to its issue instead of GitHub's answer). A plain `self.route(&to, &act)` does not compile (the recursion limit), so the cast is the mutation.
3. The tier on a hop: return `Tier::Local` → `a_tombstoned_id_is_followed_to_its_issue` red (the finding's placement sees a local record; `add_finding` writes it uncrossed and the local store refuses the issue URL).
4. The hop acts on the target: pass `id` for `&to` → `a_tombstoned_id_is_followed_to_its_issue` red (GitHub does not hold the old urn).
5. The hop opens GitHub with its own error: replace `self.github.tracker()?` in the hop with `self.github.tracker().unwrap_or(self.local)` → `following_a_tombstone_to_an_unbound_or_unreachable_github_is_that_tiers_error` red.
6. `get_finding` resolves the record: return `f` for `self.as_now(f)?` → `a_findings_record_reads_as_its_issue_once_escalated` red.
7. `findings` resolves the record: push `(tier, f)` for `(tier, self.as_now(f)?)` → `a_findings_record_reads_as_its_issue_once_escalated` red (at the merged list's assertion; `list_findings` goes through it).
8. `as_now`'s guard: delete `self.tier_of(finding.record.iri()) == Tier::Local &&` → `an_issue_url_is_never_read_through_a_local_mark_or_tombstone` red (a GitHub record whose URL the local tier holds as an alias of a tombstoned item is rewritten). `tombstone_of` never asks GitHub, so `asked()` cannot show this guard; the alias does.
9. `escalating`'s guard: delete the `if self.tier_of(id) == Tier::Github { return Ok(None); }` → `an_issue_url_is_never_read_through_a_local_mark_or_tombstone` red (the issue's URL reads the local alias's mark).
10. Each caller acts on the id it is given — replace the closure by one that ignores its id and uses the caller's own, each → `a_tombstoned_id_is_followed_to_its_issue` red: `record_of` (`|t, _| t.get_record(id)`), `get_record` (same), `set_record_state` (`|t, _| t.set_record_state(id, state)`), `get_finding` (`|t, _| t.get_finding(id)`), `add_alias` (`|t, _| t.add_alias(primary, alias.clone())`), and `update_finding` (delete `finding.id = FindingId(id.clone());` — the copy read before the escalation carries the old id).

11. The marked item's issue: delete `if let Some(issue) = self.issue_of_marked(id)? { … }` in `route` → `a_marked_item_is_its_issue_once_the_issue_exists` red (the old IRI reads the local item).
12. Its tier: `Tier::Local` for `Tier::Github` in that return → `a_marked_item_is_its_issue_once_the_issue_exists` red (the finding's placement sees a local record).
13. Only a marked item asks GitHub: move `let Ok(github) = self.github.tracker() else { return Ok(None); };` above the `let Some(mark) = … else` in `issue_of_marked` → `a_marked_item_is_its_issue_once_the_issue_exists` red (`asked()` moves for an unmarked item).
14. A GitHub that cannot be opened or reached leaves the local item: `let github = self.github.tracker()?;` for the `let Ok(github) = self.github.tracker() else { return Ok(None); };` → `a_marked_item_with_github_unbound_or_unreachable_is_the_local_item` red (`TierUnavailable`, then `Unreachable`, instead of the local item). Only a readable issue takes over: delete the `Kind::Finding => github.get_finding(…)…` arm of `readable` → `a_marked_item_is_its_issue_once_the_issue_exists` red (a finding's issue is read as a record, found unreadable, and the old id reads the local finding). The two other fall-backs — a search that fails, and an issue fl cannot read — need GitHub's own answers (`MemIssues` has no labels and fails a search only when down, which `tracker()` already reports): Task 8's black-box checks 21 and 22 pin them.
15. The primary IRI is the key: `Some(_) => self.github.find_escalated(id, mark.at_ms),` → `a_marked_item_is_its_issue_once_the_issue_exists` red (the item's alias reads the local item). A finding's primary: delete the `Kind::Finding => …` arm → the same test red.
16. `records` drops the copy: `Ok(out)` for `self.one_copy(out, |r| …)` → `a_merged_list_shows_a_marked_item_with_an_issue_once` red; the same in `findings` → the same test red.
17. Only when its issue is listed: `let its_issue_listed = tier == Tier::Local;` → `a_merged_list_shows_a_marked_item_with_an_issue_once` red (the local-tier list loses the marked record). Only a marked item: `if its_issue_listed {` → the same test red (the unmarked record an issue names as an alias is dropped).

Not observable: the conformance fixtures' and the exec test's `escalations` field — the compiler requires it, and no other value type-checks there. Acting on the issue's own IRI rather than the id asked (`id` for `&issue` in `route`'s marked arm): the issue holds every name of the item as an alias, so GitHub answers the old IRI with the issue too; the issue's IRI spares the real tracker an alias scan. The search's time (`0` for `mark.at_ms`): `MemIssues` reads no time, and Task 4's tracker tests pin the margin. `let Some(primary) = primary else { return Ok(None); };` in `issue_of_marked`: a marked item is always held, so its kind and its read always answer. `.is_ok_and(|r| r.is_some())` against `.is_ok()`: a found issue the tracker reads as absent is one deleted between the search and the read, which no fake can stage. In `one_copy`, the `.filter(|(tier, _)| *tier == Tier::Github)` and `tier == Tier::Local` conjuncts: a local item's alias never names another local item, and no issue holds an issue URL of its own repository as an alias (the one-namespace rule), so neither can be told apart by any input.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1143 passed, 18 ignored.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/tiered.rs crates/cli/src/main.rs crates/exec/tests/tiered_evidence.rs
git commit -m "feat(core): the router follows a tombstone and sees a mark

TieredTracker gains escalations, the local tier's marks and tombstones.
route's action now takes the id it acts on, so when the local tier
answers Escalated the router asks GitHub for the tombstone's target —
once, by the target's own IRI: GitHub's answer for that issue is the
answer, a GitHub that cannot be reached is that tier's error, and no
tombstone is followed a second time. A lookup, a write, an alias and a
finding raised about an escalated item's old IRI reach its issue. A
finding read through the router names its record as the record now is:
get_finding and findings replace a local record's IRI by its tombstone's
target, and never look up an IRI GitHub claims; stores ignore the record
on an update, so the rewrite is never written back. escalating reads a
local item's mark without asking GitHub.

A marked item and the issue whose alias is its IRI are one item (routing
spec §2.2): route asks GitHub, by the escalation's own search back to the
mark's time, for the issue of an id the local tier holds marked, and acts
on the issue once it exists and GitHub reads it as an fl item. Otherwise
- no issue, an issue fl cannot read, or a GitHub that cannot be opened or
reached - the local item answers and refuses every write: no write lands
in two places. A merged list leaves out a marked local item whose issue
it holds; a list of the local tier alone keeps it.
Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

---

### Task 7: The router escalates: pre-checks, mark, find or create, tombstone, abandon

This is where the escalation runs (routing spec §3.1–§3.3, §3.6). It is split like placement (plan ruling 10): `prepare_escalation` makes every check and writes nothing, so "an item is never marked and then stranded" (§3.2); `escalate` runs the three steps of §3.3 — "Mark the local item "escalating", with who, why and the time", find or create the issue by a search of its own, then "Replace the local item with a tombstone"; `abandon_escalation` removes the mark "only after the step-2 search proves no issue exists". The checks, in order: an id the GitHub tier claims, a tombstone (naming where it went, §3.1), an id the local tier does not hold, and the wrong kind; the project's map (`Unrouted`, plan ruling 21) and an open GitHub tier (`TierUnavailable`, or the tier's own error); then, on an item an earlier run marked, the search for its issue by the old IRI (plan rulings 1, 13) — a found issue passed every check when it was made, and its aliases now include the item's own IRI, so the rerun only finishes it — `create_escalated` searches first, finds it, makes no second issue and gives it the labels a stop before the label call left off (plan ruling 13), then the tombstone; else every check again: a closed state, a record title GitHub refuses — an empty one among them (spec defect 4) — anything sensitive on a repository that is not private (decisions 21 and 22, plan ruling 20 — refused, never warned, Review Focus 4), and the one-namespace rule for the item's IRI and each alias (plan ruling 14). What goes out is a record with its open findings from both tiers (decision 18, plan ruling 4) — never a security finding, nor one in a sensitive area or an area the map no longer declares, by the same rule the finding pre-check uses (decisions 21 and 22), since the list is published with the record — or a finding with its record as the router reads it now — an escalated record's issue (plan ruling 9). A rerun resumes with the mark's own who, why and time (plan ruling 12), and the search reaches back to the mark's time (§3.3 step 2). "The item is never live in both tiers" (§3.3): the mark blocks local writes before the create; once the issue exists and GitHub reads it as an fl item the router reads and writes the old id as the issue, and until then the local copy answers and refuses writes (§2.2, plan ruling 23), and after the tombstone every read and write of the old id reaches the issue (Task 6).

**Blast radius:** a new module, `crates/core/src/escalate.rs`, and `pub use escalate::Prepared` in `lib.rs`. In `crates/core/src/tiered.rs`, `map_of` and `private_or_refuse` become `pub(crate)` so the new module calls them rather than copying them; neither changes, and every existing caller is in `tiered.rs`. Nothing outside the new module calls the three new methods until the CLI does (Task 8), so every existing path behaves exactly as before. The tests' GitHub tier is `MemIssues` behind a test-only wrapper that records each `create_escalated` call, and one test's local tier is `MemStore` behind a wrapper whose `tombstone` fails once; both live in the tests.

**Files:**
- Create: `crates/core/src/escalate.rs` (`Prepared`, `ISSUE_TITLE_MAX`, the router's `prepare_escalation`, `escalate`, `abandon_escalation`, and their tests)
- Modify: `crates/core/src/lib.rs` (`pub mod escalate;`, `pub use escalate::Prepared;`)
- Modify: `crates/core/src/tiered.rs` (`map_of` and `private_or_refuse` become `pub(crate)`)

**Interfaces:**
- Consumes: `Escalations`, `Mark`, `Tombstone`, `Outgoing`, `Provenance`, `EscalationFault`, `StoreError::{Escalating, Escalated, Escalation}`, `impl Escalations for MemStore` (Task 1); `GithubTier::{find_escalated, alias_taken, create_escalated}` and `MemIssues::{set_fail_next_create, set_lose_next_create_answer, creates}` (Task 5); `TieredTracker.escalations`, `route`'s hop through a tombstone, and `findings`/`get_finding` naming a record as it now is (Task 6); `TieredTracker::{record_of, findings, map_of, private_or_refuse}`, `Catalog::kind_of`, `MemIssues::{set_public, set_unbound, set_down, set_visibility_unread}`, `VISIBILITY_UNREAD`, `ForeignRecord::for_tests` (existing).
- Produces (exactly as "Interfaces every task shares"): `pub struct Prepared` (private fields) with `id(&self) -> &Iri` (the item's primary local IRI), `kind(&self) -> Kind`, `outgoing(&self) -> &Outgoing`, `resumes(&self) -> Option<&Mark>`, `found(&self) -> Option<&Iri>`; on `TieredTracker`: `pub fn prepare_escalation(&self, id: &Iri, kind: Kind) -> Result<Prepared, StoreError>`, `pub fn escalate(&self, at: &Prepared, by: &str, reason: &str, now_ms: u64) -> Result<Iri, StoreError>`, `pub fn abandon_escalation(&self, id: &Iri, kind: Kind) -> Result<(), StoreError>`.
- Produces, beyond the shared block: `pub const ISSUE_TITLE_MAX: usize = 256` in `escalate.rs`; `pub use escalate::Prepared` in `lib.rs`; `TieredTracker::{map_of, private_or_refuse}` are `pub(crate)`. The refusals, for Task 8: the wrong kind is the store's `StoreError::WrongKind { id, expected, found }`, `expected` being `Finding` for the finding form and `Record` for any other kind asked; an id GitHub claims (even one the local tier holds as an alias) and an id the local catalog does not hold are both `EscalationFault::NotLocal`; sensitivity is `RoutingFault::SensitiveNamedPublic` with `what` = `this record`, `this finding`, or `this finding, about a record in a sensitive area,` (the phrase `place_finding` uses); an unreadable visibility is `require_private`'s own error. `abandon_escalation` checks the item as `prepare_escalation`'s first step does (`NotLocal`, `AlreadyEscalated`, `WrongKind`), then `NotMarked`, then the GitHub tier, then the search. A `Prepared` whose `found()` is `Some` still carries the outgoing item (read, not checked), and `escalate` still passes it to `create_escalated`, which finds the issue and labels it if it has no fl labels: a found issue is never tombstoned by itself.
- Unique phrases: `an issue needs a title` (the empty title's reason). The tests also assert Task 1's `is in a closed state`, `cannot be an issue's title`, `already names` and `its issue exists`, and otherwise match on variants and IRIs.

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/escalate.rs` with the module's header, its `use` lines, and the tests:

```rust
//! The router's escalation (routing spec §3): every check the GitHub create
//! would make, before anything is written; then the three steps — mark the
//! local item, find or create its issue, replace the local item with a
//! tombstone — each of which a rerun resumes.

use crate::escalation::{EscalationFault, Mark, Outgoing, Provenance};
use crate::finding::{Finding, FindingState};
use crate::ids::{FindingId, Kind, RecordId};
use crate::iri::Iri;
use crate::model::{Record, State};
use crate::routing::{RoutingMap, Tier};
use crate::store::StoreError;
use crate::tiered::{RecordSeen, TieredTracker};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::escalation::{Escalations, Tombstone};
    use crate::ids::{ProjectId, seq_iri};
    use crate::mem_issues::{MemIssues, VISIBILITY_UNREAD};
    use crate::routing::{ForeignRecord, GithubTier, RoutingFault};
    use crate::store::{Catalog, Tracker};
    use std::cell::{Cell, RefCell};

    /// The in-memory GitHub tier, and every `create_escalated` it was asked:
    /// the provenance, and the time its search reached back to.
    #[derive(Default)]
    struct Watched {
        issues: MemIssues,
        creates_asked: RefCell<Vec<(Provenance, u64)>>,
    }

    impl std::ops::Deref for Watched {
        type Target = MemIssues;

        fn deref(&self) -> &MemIssues {
            &self.issues
        }
    }

    impl GithubTier for Watched {
        fn available(&self) -> bool {
            self.issues.available()
        }

        fn claims(&self, id: &Iri) -> bool {
            self.issues.claims(id)
        }

        fn issue_form(&self, id: &Iri) -> bool {
            self.issues.issue_form(id)
        }

        fn tracker(&self) -> Result<&dyn Tracker, StoreError> {
            self.issues.tracker()
        }

        fn require_private(&self) -> Result<(), StoreError> {
            self.issues.require_private()
        }

        fn items_in_area(
            &self,
            project: &ProjectId,
            area: &str,
        ) -> Result<Vec<(Kind, Iri)>, StoreError> {
            self.issues.items_in_area(project, area)
        }

        fn find_escalated(&self, key: &Iri, since_ms: u64) -> Result<Option<Iri>, StoreError> {
            self.issues.find_escalated(key, since_ms)
        }

        fn alias_taken(&self, alias: &Iri) -> Result<Option<Iri>, StoreError> {
            self.issues.alias_taken(alias)
        }

        fn create_escalated(
            &self,
            item: &Outgoing,
            from: &Provenance,
            since_ms: u64,
        ) -> Result<Iri, StoreError> {
            self.creates_asked
                .borrow_mut()
                .push((from.clone(), since_ms));
            self.issues.create_escalated(item, from, since_ms)
        }
    }

    /// The local store, whose next `tombstone` fails before it writes
    /// anything: a stop after the issue exists and before step 3.
    struct TombstoneFailsOnce<'a> {
        local: &'a MemStore,
        fail: Cell<bool>,
    }

    impl Escalations for TombstoneFailsOnce<'_> {
        fn mark(&self, id: &Iri, mark: &Mark) -> Result<(), StoreError> {
            self.local.mark(id, mark)
        }

        fn mark_of(&self, id: &Iri) -> Result<Option<Mark>, StoreError> {
            self.local.mark_of(id)
        }

        fn unmark(&self, id: &Iri) -> Result<(), StoreError> {
            self.local.unmark(id)
        }

        fn tombstone(&self, id: &Iri, to: &Iri) -> Result<Tombstone, StoreError> {
            if self.fail.replace(false) {
                return Err(StoreError::Backend("the disk is full".into()));
            }
            self.local.tombstone(id, to)
        }

        fn tombstone_of(&self, id: &Iri) -> Result<Option<Tombstone>, StoreError> {
            self.local.tombstone_of(id)
        }
    }

    /// A project with the starting map in a local `MemStore`, and an
    /// in-memory GitHub tier.
    struct W {
        local: MemStore,
        issues: Watched,
        p: ProjectId,
    }

    fn world() -> W {
        let local = MemStore::default();
        let p = local.add_project("/p").unwrap();
        local.set_routes(&p, &RoutingMap::starting()).unwrap();
        W {
            local,
            issues: Watched::default(),
            p,
        }
    }

    const BY: &str = "alice";
    const WHY: &str = "a person decides";
    const NOW: u64 = 10;

    impl W {
        fn router(&self) -> TieredTracker<'_> {
            self.over(&self.local)
        }

        /// The router, with `escalations` as the local tier's marks and
        /// tombstones.
        fn over<'a>(&'a self, escalations: &'a dyn Escalations) -> TieredTracker<'a> {
            TieredTracker {
                catalog: &self.local,
                local: &self.local,
                routes: &self.local,
                github: &self.issues,
                escalations,
            }
        }

        /// A local record, written straight to the local store: in any
        /// area, or none.
        fn record(&self, area: Option<&str>, title: &str) -> RecordId {
            self.local
                .add_record_with_area(&self.p, title, area)
                .unwrap()
        }

        /// A local finding about `record`, written straight to the local
        /// store.
        fn finding(&self, record: &RecordId, area: &str, security: bool) -> FindingId {
            let mut f = Finding::raise(self.p.clone(), record.clone(), "bob", "the claim");
            f.area = Some(area.into());
            f.security = security;
            self.local.add_finding(f).unwrap()
        }

        fn set_finding_state(&self, id: &FindingId, state: FindingState) {
            let mut f = self.local.get_finding(id).unwrap().unwrap();
            f.state = state;
            self.local.update_finding(&f).unwrap();
        }

        /// The whole command, as the CLI runs it.
        fn run(&self, id: &Iri, kind: Kind) -> Result<Iri, StoreError> {
            self.run_as(id, kind, BY, WHY, NOW)
        }

        fn run_as(
            &self,
            id: &Iri,
            kind: Kind,
            by: &str,
            why: &str,
            now: u64,
        ) -> Result<Iri, StoreError> {
            let t = self.router();
            let at = t.prepare_escalation(id, kind)?;
            t.escalate(&at, by, why, now)
        }

        /// The command refused, and nothing written: no mark, no issue.
        fn refused(&self, id: &Iri, kind: Kind) -> StoreError {
            let err = self.run(id, kind).unwrap_err();
            assert_eq!(self.local.mark_of(id).unwrap(), None, "marked: {err}");
            assert_eq!(self.issues.creates(), 0, "an issue made: {err}");
            err
        }
    }

    fn escalation(e: &StoreError) -> Option<&EscalationFault> {
        match e {
            StoreError::Escalation(f) => Some(f),
            _ => None,
        }
    }

    fn fault(e: &StoreError) -> Option<&RoutingFault> {
        match e {
            StoreError::Routing(f) => Some(f),
            _ => None,
        }
    }

    // Routing spec §3.1: an item that is not local, or was escalated
    // already, is refused; so is an id given as the wrong kind.
    #[test]
    fn an_item_that_is_not_local_or_was_escalated_already_is_refused_before_the_mark() {
        let w = world();
        let gh = w
            .issues
            .add_record_with_area(&w.p, "on github", Some("design"))
            .unwrap();
        let err = w.refused(gh.iri(), Kind::Record);
        assert!(
            matches!(escalation(&err), Some(EscalationFault::NotLocal { id }) if id == gh.iri()),
            "{err:?}"
        );
        // An issue URL is GitHub's even when a local item holds it as an
        // alias (the one-namespace rule): it never escalates that item.
        let r = w.record(Some("code"), "t");
        let url = MemIssues::issue(9);
        w.local.add_alias(r.iri(), url.clone()).unwrap();
        let err = w.refused(&url, Kind::Record);
        assert!(
            matches!(escalation(&err), Some(EscalationFault::NotLocal { id }) if *id == url),
            "{err:?}"
        );
        let err = w.refused(&seq_iri(999), Kind::Record);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::NotLocal { id }) if *id == seq_iri(999)
            ),
            "{err:?}"
        );
        // Escalated already: the refusal names where it went.
        let old = w.record(Some("code"), "gone");
        let to = w
            .issues
            .add_record_with_area(&w.p, "gone", Some("code"))
            .unwrap();
        w.local
            .mark(
                old.iri(),
                &Mark {
                    by: BY.into(),
                    reason: WHY.into(),
                    at_ms: 1,
                },
            )
            .unwrap();
        w.local.tombstone(old.iri(), to.iri()).unwrap();
        let err = w.refused(old.iri(), Kind::Record);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::AlreadyEscalated { id, to: t })
                    if id == old.iri() && t == to.iri()
            ),
            "{err:?}"
        );
        // The wrong kind: a finding's id to the record form, a record's to
        // the finding form, a project's to either.
        let f = w.finding(&r, "code", false);
        for (id, kind, found) in [
            (f.iri(), Kind::Record, Kind::Finding),
            (r.iri(), Kind::Finding, Kind::Record),
            (w.p.iri(), Kind::Record, Kind::Project),
        ] {
            let err = w.refused(id, kind);
            assert!(
                matches!(
                    err,
                    StoreError::WrongKind { expected, found: got, .. }
                        if expected == kind && got == found
                ),
                "{err:?}"
            );
        }
        // Only a record or a finding escalates.
        let err = w.refused(w.p.iri(), Kind::Project);
        assert!(
            matches!(
                err,
                StoreError::WrongKind {
                    expected: Kind::Record,
                    found: Kind::Project,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    // Routing spec §3.2: a closed state — a record `done`, a finding
    // `fixed` or `withdrawn` — is refused; GitHub takes open items only.
    #[test]
    fn a_closed_item_is_refused_before_the_mark() {
        let w = world();
        let r = w.record(Some("code"), "t");
        w.local.set_record_state(&r, State::Done).unwrap();
        let err = w.refused(r.iri(), Kind::Record);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::Closed { state, .. }) if state == "done"
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("is in a closed state"), "{err}");
        let open = w.record(Some("code"), "open");
        for (state, wire) in [
            (FindingState::Fixed, "fixed"),
            (FindingState::Withdrawn, "withdrawn"),
        ] {
            let f = w.finding(&open, "code", false);
            w.set_finding_state(&f, state);
            let err = w.refused(f.iri(), Kind::Finding);
            assert!(
                matches!(
                    escalation(&err),
                    Some(EscalationFault::Closed { id, state }) if id == f.iri() && state == wire
                ),
                "{err:?}"
            );
        }
    }

    // Routing spec §3.2: a record's title is its issue's title, so one the
    // GitHub tracker refuses — over 256 characters, or with whitespace at
    // either end — is refused. A finding's title is cut from its claim, so
    // a long claim is not.
    #[test]
    fn a_title_github_would_refuse_is_refused_before_the_mark() {
        let w = world();
        for title in [
            "é".repeat(257),
            " leading".to_string(),
            "trailing\n".to_string(),
            String::new(),
        ] {
            let r = w.record(Some("code"), &title);
            let err = w.refused(r.iri(), Kind::Record);
            assert!(
                matches!(
                    escalation(&err),
                    Some(EscalationFault::Title { id, .. }) if id == r.iri()
                ),
                "{title:?}: {err:?}"
            );
            assert!(
                err.to_string().contains("cannot be an issue's title"),
                "{err}"
            );
            assert_eq!(
                err.to_string().contains("an issue needs a title"),
                title.is_empty(),
                "{err}"
            );
        }
        // 256 characters, each two bytes: the limit counts characters.
        let r = w.record(Some("code"), &"é".repeat(256));
        w.router()
            .prepare_escalation(r.iri(), Kind::Record)
            .unwrap();
        let mut f = Finding::raise(w.p.clone(), r, "bob", &format!(" {}", "x".repeat(300)));
        f.area = Some("code".into());
        let f = w.local.add_finding(f).unwrap();
        w.router()
            .prepare_escalation(f.iri(), Kind::Finding)
            .unwrap();
    }

    // Routing spec §3.1: a project that is not routed, and a GitHub tier
    // this machine cannot open, are refused.
    #[test]
    fn an_unrouted_or_unbound_project_is_refused_before_the_mark() {
        let w = world();
        let q = w.local.add_project("/q").unwrap();
        let unrouted = w.local.add_record_with_area(&q, "t", None).unwrap();
        // The map is the first cause: GitHub is not the remedy.
        w.issues.set_unbound(true);
        let err = w.refused(unrouted.iri(), Kind::Record);
        assert!(
            matches!(fault(&err), Some(RoutingFault::Unrouted { project }) if *project == q),
            "{err:?}"
        );
        let r = w.record(Some("code"), "t");
        let err = w.refused(r.iri(), Kind::Record);
        assert!(
            matches!(
                fault(&err),
                Some(RoutingFault::TierUnavailable {
                    tier: Tier::Github,
                    ..
                })
            ),
            "{err:?}"
        );
        w.issues.set_unbound(false);
        w.issues.set_down(true);
        let err = w.refused(r.iri(), Kind::Record);
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    // Routing spec §3.2, the one-namespace rule: the item's IRI and each of
    // its aliases must name nothing on GitHub — an alias of another issue,
    // or an issue's own URL.
    #[test]
    fn an_alias_github_already_uses_is_refused_before_the_mark() {
        let w = world();
        let other = w
            .issues
            .add_record_with_area(&w.p, "other", Some("design"))
            .unwrap();
        // The item's own IRI, taken on GitHub.
        let r = w.record(Some("code"), "t");
        w.issues.add_alias(other.iri(), r.iri().clone()).unwrap();
        let err = w.refused(r.iri(), Kind::Record);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::AliasTaken { alias, issue })
                    if alias == r.iri() && issue == other.iri()
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("already names"), "{err}");
        // One of its aliases, taken by another issue's alias.
        let alias = Iri::parse("urn:x-acme:widget-7").unwrap();
        w.issues.add_alias(other.iri(), alias.clone()).unwrap();
        let s = w.record(Some("code"), "t");
        w.local
            .add_alias(s.iri(), Iri::parse("urn:x-acme:widget-6").unwrap())
            .unwrap();
        w.local.add_alias(s.iri(), alias.clone()).unwrap();
        let err = w.refused(s.iri(), Kind::Record);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::AliasTaken { alias: a, issue })
                    if *a == alias && issue == other.iri()
            ),
            "{err:?}"
        );
        // A local alias that is an issue URL of the bound repository, on a
        // finding.
        let f = w.finding(&w.record(Some("code"), "t"), "code", false);
        w.local.add_alias(f.iri(), other.iri().clone()).unwrap();
        let err = w.refused(f.iri(), Kind::Finding);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::AliasTaken { alias, issue })
                    if alias == other.iri() && issue == other.iri()
            ),
            "{err:?}"
        );
    }

    // Routing spec decisions 21 and 22: nothing about an item in a
    // sensitive area, and no security finding, reaches a repository that is
    // not private — refused before the mark. An area the map no longer
    // declares counts as sensitive; a record with no area is not.
    #[test]
    fn nothing_sensitive_is_escalated_to_a_public_repository() {
        let w = world();
        let code = w.record(Some("code"), "fine");
        let secret = w.record(Some("security"), "the key leaks");
        let gone = w.record(Some("ops"), "undeclared");
        let refused = [
            (secret.iri().clone(), Kind::Record, "this record"),
            (gone.iri().clone(), Kind::Record, "this record"),
            (
                w.finding(&code, "code", true).0,
                Kind::Finding,
                "this finding",
            ),
            (
                w.finding(&code, "security", false).0,
                Kind::Finding,
                "this finding",
            ),
            (
                w.finding(&code, "ops", false).0,
                Kind::Finding,
                "this finding",
            ),
            (
                w.finding(&secret, "code", false).0,
                Kind::Finding,
                "this finding, about a record in a sensitive area,",
            ),
            (
                w.finding(&gone, "code", false).0,
                Kind::Finding,
                "this finding, about a record in a sensitive area,",
            ),
        ];
        // Private: every one of them may go.
        for (id, kind, _) in &refused {
            w.router().prepare_escalation(id, *kind).unwrap();
        }
        w.issues.set_public(true);
        for (id, kind, what) in &refused {
            let err = w.refused(id, *kind);
            assert!(
                matches!(
                    fault(&err),
                    Some(RoutingFault::SensitiveNamedPublic { what: w, .. }) if w == what
                ),
                "{id}: {err:?}"
            );
        }
        // Not sensitive: refused nowhere here (the CLI warns before it
        // publishes). A record with no area; a finding about a record that
        // GitHub holds already, whose title is published.
        let none = w.record(None, "no area");
        let on_github = w
            .issues
            .add_record_with_area(&w.p, "public", Some("security"))
            .unwrap();
        let mut about = Finding::raise(w.p.clone(), on_github.clone(), "bob", "c");
        about.area = Some("code".into());
        let about = w
            .local
            .add_finding_checked(
                about,
                ForeignRecord::for_tests(on_github, "public", Tier::Github),
            )
            .unwrap();
        for (id, kind) in [
            (code.iri(), Kind::Record),
            (none.iri(), Kind::Record),
            (w.finding(&code, "code", false).iri(), Kind::Finding),
            (w.finding(&none, "code", false).iri(), Kind::Finding),
            (about.iri(), Kind::Finding),
        ] {
            w.router().prepare_escalation(id, kind).unwrap();
        }
        // A visibility that cannot be read is that error, never a pass.
        w.issues.set_public(false);
        w.issues.set_visibility_unread(true);
        let err = w.refused(secret.iri(), Kind::Record);
        assert!(
            matches!(&err, StoreError::Backend(why) if why == VISIBILITY_UNREAD),
            "{err:?}"
        );
    }

    // Routing spec §3.3, decision 18: a record goes out with its own state,
    // area and aliases, and its open findings from both tiers — never a
    // security finding, nor one in a sensitive or undeclared area
    // (decisions 21, 22); then the tombstone replaces it, and the old id is
    // the issue.
    #[test]
    fn a_record_escalates_with_its_state_aliases_and_open_findings() {
        let w = world();
        let t = w.router();
        let r = w.record(Some("code"), "needs a person");
        let alias = Iri::parse("urn:x-acme:widget-7").unwrap();
        w.local.add_alias(r.iri(), alias.clone()).unwrap();
        w.local.set_record_state(&r, State::NeedsHuman).unwrap();
        let open = w.finding(&r, "code", false);
        let fixed = w.finding(&r, "code", false);
        w.set_finding_state(&fixed, FindingState::Fixed);
        let withdrawn = w.finding(&r, "code", false);
        w.set_finding_state(&withdrawn, FindingState::Withdrawn);
        let assigned = w.finding(&r, "tests", false);
        w.set_finding_state(&assigned, FindingState::Assigned);
        let security = w.finding(&r, "code", true);
        let sensitive = w.finding(&r, "security", false);
        let undeclared = w.finding(&r, "gone", false);
        let elsewhere = w.finding(&w.record(Some("code"), "other"), "code", false);
        // On GitHub: one raised through the router, one about the alias.
        let mut f = Finding::raise(w.p.clone(), r.clone(), "bob", "looks off");
        f.area = Some("design".into());
        let on_github = t.add_finding(f).unwrap();
        assert_eq!(t.tier_of(on_github.iri()), Tier::Github);
        let by_alias = w
            .issues
            .add_finding_checked(
                Finding::raise(w.p.clone(), RecordId(alias.clone()), "bob", "by alias"),
                ForeignRecord::for_tests(RecordId(alias.clone()), "needs a person", Tier::Local),
            )
            .unwrap();

        let at = t.prepare_escalation(r.iri(), Kind::Record).unwrap();
        assert_eq!(
            (at.id(), at.kind(), at.resumes(), at.found()),
            (r.iri(), Kind::Record, None, None)
        );
        let Outgoing::Record { record, findings } = at.outgoing() else {
            panic!("{:?}", at.outgoing());
        };
        assert_eq!(*record, w.local.get_record(&r).unwrap().unwrap());
        let ids: Vec<&FindingId> = findings.iter().map(|f| &f.id).collect();
        assert_eq!(ids, vec![&open, &assigned, &on_github, &by_alias]);
        assert!(!ids.contains(&&fixed) && !ids.contains(&&withdrawn));
        assert!(!ids.contains(&&security) && !ids.contains(&&elsewhere));
        assert!(!ids.contains(&&sensitive) && !ids.contains(&&undeclared));

        let issue = t.escalate(&at, BY, WHY, NOW).unwrap();
        assert_eq!(issue, MemIssues::issue(3));
        let made = w
            .issues
            .get_record(&RecordId(issue.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(
            (made.state, made.area.as_deref(), made.title.as_str()),
            (State::NeedsHuman, Some("code"), "needs a person")
        );
        assert_eq!(made.also_known_as, vec![r.iri().clone(), alias.clone()]);
        assert_eq!(
            w.issues.get_record(&r).unwrap().unwrap().id.iri(),
            &issue,
            "the old IRI names the issue"
        );
        assert_eq!(
            w.local.tombstone_of(r.iri()).unwrap(),
            Some(Tombstone {
                from: r.iri().clone(),
                to: issue.clone(),
                by: BY.into(),
                reason: WHY.into(),
                at_ms: NOW,
            })
        );
        assert_eq!(w.local.mark_of(r.iri()).unwrap(), None);
        assert_eq!(
            *w.issues.creates_asked.borrow(),
            vec![(
                Provenance {
                    from: r.iri().clone(),
                    by: BY.into(),
                    reason: WHY.into(),
                },
                NOW
            )]
        );
        // The router reads the old id, and its alias, as the issue.
        for old in [r.clone(), RecordId(alias)] {
            assert_eq!(t.get_record(&old).unwrap().unwrap().id.iri(), &issue);
        }
        // A finding raised about the old id afterwards names the issue.
        let mut later = Finding::raise(w.p.clone(), r.clone(), "bob", "later");
        later.area = Some("code".into());
        let later = t.add_finding(later).unwrap();
        assert_eq!(
            w.local.get_finding(&later).unwrap().unwrap().record.iri(),
            &issue
        );
    }

    // Routing spec §3.1, decision 5: a finding escalates while its record
    // stays local; about a record escalated already, its record is the
    // issue.
    #[test]
    fn a_finding_escalates_about_a_local_record_and_about_an_escalated_one() {
        let w = world();
        let t = w.router();
        let r = w.record(Some("code"), "the record");
        let f = w.finding(&r, "code", false);
        w.set_finding_state(&f, FindingState::Reproduced);
        let at = t.prepare_escalation(f.iri(), Kind::Finding).unwrap();
        let seen = RecordSeen {
            id: r.clone(),
            title: "the record".into(),
            tier: Tier::Local,
        };
        assert!(
            matches!(at.outgoing(), Outgoing::Finding { record, .. } if *record == seen),
            "{:?}",
            at.outgoing()
        );
        let issue = t.escalate(&at, BY, WHY, NOW).unwrap();
        let made = w
            .issues
            .get_finding(&FindingId(issue.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(
            (&made.record, made.state, &made.also_known_as),
            (&r, FindingState::Reproduced, &vec![f.iri().clone()])
        );
        assert_eq!(t.get_finding(&f).unwrap().unwrap().id.iri(), &issue);
        assert!(w.local.tombstone_of(f.iri()).unwrap().is_some());
        assert_eq!(
            w.local.get_record(&r).unwrap().unwrap().state,
            State::Todo,
            "the record stays local"
        );

        // A finding raised before its record was escalated.
        let g = w.finding(&r, "code", false);
        let to = w.run(r.iri(), Kind::Record).unwrap();
        let at = t.prepare_escalation(g.iri(), Kind::Finding).unwrap();
        assert!(
            matches!(
                at.outgoing(),
                Outgoing::Finding { record, .. }
                    if record.id.iri() == &to && record.tier == Tier::Github
            ),
            "{:?}",
            at.outgoing()
        );
        let issue = t.escalate(&at, BY, WHY, NOW).unwrap();
        let made = w.issues.get_finding(&FindingId(issue)).unwrap().unwrap();
        assert_eq!(made.record.iri(), &to);
    }

    /// The item is never live in both tiers (routing spec §2.2, §3.3):
    /// while it is marked the local store refuses a write; the router
    /// refuses one too until the issue exists, and from then on reaches the
    /// issue.
    fn assert_marked_and_unwritable(
        w: &W,
        t: &TieredTracker<'_>,
        r: &RecordId,
        issue: Option<&Iri>,
    ) {
        assert!(w.local.mark_of(r.iri()).unwrap().is_some());
        assert_eq!(w.local.tombstone_of(r.iri()).unwrap(), None);
        let err = w.local.set_record_state(r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Escalating { .. }), "{err:?}");
        match issue {
            None => {
                let err = t.set_record_state(r, State::Doing).unwrap_err();
                assert!(matches!(err, StoreError::Escalating { .. }), "{err:?}");
            }
            Some(issue) => {
                assert_eq!(t.get_record(r).unwrap().unwrap().id.iri(), issue);
            }
        }
    }

    /// The escalation finished: one issue, the tombstone, and every read and
    /// write of the old id reaching the issue, never the dead local row.
    fn assert_finished(w: &W, t: &TieredTracker<'_>, r: &RecordId, issue: &Iri) {
        assert_eq!(w.local.mark_of(r.iri()).unwrap(), None);
        assert_eq!(w.local.tombstone_of(r.iri()).unwrap().unwrap().to, *issue);
        t.set_record_state(r, State::Review).unwrap();
        let read = t.get_record(r).unwrap().unwrap();
        assert_eq!((read.id.iri(), read.state), (issue, State::Review));
        let local = w.local.get_record(r).unwrap_err();
        assert!(matches!(local, StoreError::Escalated { .. }), "{local:?}");
    }

    // Routing spec §3.3: "Each step can be run again, and running the
    // command again resumes from where it stopped." A stop before the
    // create, after the create landed with its answer lost, and after the
    // create before the tombstone: each rerun finishes with one issue.
    #[test]
    fn a_rerun_after_each_step_finishes_with_one_issue_and_one_live_copy() {
        let w = world();
        let t = w.router();

        // Stopped before the create: marked, no issue.
        let r1 = w.record(Some("code"), "one");
        w.issues.set_fail_next_create(true);
        let err = w.run(r1.iri(), Kind::Record).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert_eq!(w.issues.creates(), 0);
        assert_marked_and_unwritable(&w, &t, &r1, None);
        let at = t.prepare_escalation(r1.iri(), Kind::Record).unwrap();
        assert_eq!((at.resumes().is_some(), at.found()), (true, None));
        let issue = t.escalate(&at, BY, WHY, NOW).unwrap();
        assert_eq!(w.issues.creates(), 1);
        assert_finished(&w, &t, &r1, &issue);

        // Stopped after the create landed, its answer lost: the rerun finds
        // the issue — its own IRI is that issue's alias now, and the search
        // comes before that check — and finishes it through the create,
        // which finds it and makes none.
        let r2 = w.record(Some("code"), "two");
        w.issues.set_lose_next_create_answer(true);
        let err = w.run(r2.iri(), Kind::Record).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert_eq!(w.issues.creates(), 2);
        let landed = MemIssues::issue(2);
        assert_marked_and_unwritable(&w, &t, &r2, Some(&landed));
        let at = t.prepare_escalation(r2.iri(), Kind::Record).unwrap();
        assert_eq!(at.found(), Some(&landed));
        let asked = w.issues.creates_asked.borrow().len();
        assert_eq!(t.escalate(&at, BY, WHY, NOW).unwrap(), landed);
        assert_eq!(w.issues.creates(), 2);
        assert_eq!(
            w.issues.creates_asked.borrow().len(),
            asked + 1,
            "a found issue is finished through the create"
        );
        assert_finished(&w, &t, &r2, &landed);

        // Stopped after the create, before the tombstone.
        let r3 = w.record(Some("code"), "three");
        let failing = TombstoneFailsOnce {
            local: &w.local,
            fail: Cell::new(true),
        };
        let over = w.over(&failing);
        let at = over.prepare_escalation(r3.iri(), Kind::Record).unwrap();
        let err = over.escalate(&at, BY, WHY, NOW).unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{err:?}");
        assert_eq!(w.issues.creates(), 3);
        let landed = MemIssues::issue(3);
        assert_marked_and_unwritable(&w, &t, &r3, Some(&landed));
        let at = over.prepare_escalation(r3.iri(), Kind::Record).unwrap();
        assert_eq!(at.found(), Some(&landed));
        assert_eq!(over.escalate(&at, BY, WHY, NOW).unwrap(), landed);
        assert_eq!(w.issues.creates(), 3);
        assert_finished(&w, &t, &r3, &landed);
    }

    // Routing spec §3.2, §3.3: a rerun that finds no issue makes every
    // check again — the repository may have turned public since the mark.
    #[test]
    fn a_rerun_that_finds_no_issue_makes_every_check_again() {
        let w = world();
        let r = w.record(Some("security"), "the key leaks");
        w.issues.set_fail_next_create(true);
        w.run(r.iri(), Kind::Record).unwrap_err();
        let marked = w.local.mark_of(r.iri()).unwrap();
        assert!(marked.is_some());
        w.issues.set_public(true);
        let err = w.run(r.iri(), Kind::Record).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveNamedPublic { .. })),
            "{err:?}"
        );
        assert_eq!(w.local.mark_of(r.iri()).unwrap(), marked, "the mark stays");
        assert_eq!(w.issues.creates(), 0);
    }

    // Routing spec §3.3: a rerun resumes the mark — its who, why and time —
    // whatever it is given.
    #[test]
    fn a_rerun_keeps_the_marks_who_and_why() {
        let w = world();
        let r = w.record(Some("code"), "t");
        w.issues.set_fail_next_create(true);
        w.run_as(r.iri(), Kind::Record, "alice", "first", 10)
            .unwrap_err();
        let at = w
            .router()
            .prepare_escalation(r.iri(), Kind::Record)
            .unwrap();
        let mark = Mark {
            by: "alice".into(),
            reason: "first".into(),
            at_ms: 10,
        };
        assert_eq!(at.resumes(), Some(&mark));
        w.run_as(r.iri(), Kind::Record, "bob", "second", 99)
            .unwrap();
        let from = Provenance {
            from: r.iri().clone(),
            by: "alice".into(),
            reason: "first".into(),
        };
        assert_eq!(
            *w.issues.creates_asked.borrow(),
            vec![(from.clone(), 10), (from, 10)],
            "the search reaches back to the mark's time"
        );
        let tomb = w.local.tombstone_of(r.iri()).unwrap().unwrap();
        assert_eq!(
            (tomb.by.as_str(), tomb.reason.as_str(), tomb.at_ms),
            ("alice", "first", 10)
        );
    }

    // Routing spec §3.3: `--abandon` removes the mark only after the search
    // proves no issue exists; once it exists, the only way on is step 3.
    #[test]
    fn an_escalation_is_abandoned_only_before_its_issue_exists() {
        let w = world();
        let t = w.router();
        let before = w.record(Some("code"), "before");
        w.issues.set_fail_next_create(true);
        w.run(before.iri(), Kind::Record).unwrap_err();
        // GitHub must answer the search: unbound, the mark stays.
        w.issues.set_unbound(true);
        let err = t
            .abandon_escalation(before.iri(), Kind::Record)
            .unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "{err:?}"
        );
        assert!(w.local.mark_of(before.iri()).unwrap().is_some());
        w.issues.set_unbound(false);
        t.abandon_escalation(before.iri(), Kind::Record).unwrap();
        assert_eq!(w.local.mark_of(before.iri()).unwrap(), None);
        t.set_record_state(&before, State::Doing).unwrap();

        let after = w.record(Some("code"), "after");
        w.issues.set_lose_next_create_answer(true);
        w.run(after.iri(), Kind::Record).unwrap_err();
        let err = t.abandon_escalation(after.iri(), Kind::Record).unwrap_err();
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::IssueExists { id, kind: Kind::Record, issue })
                    if id == after.iri() && *issue == MemIssues::issue(1)
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("its issue exists"), "{err}");
        assert!(w.local.mark_of(after.iri()).unwrap().is_some());

        // No mark: refused before GitHub is asked.
        let never = w.record(Some("code"), "never");
        w.issues.set_unbound(true);
        let err = t.abandon_escalation(never.iri(), Kind::Record).unwrap_err();
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::NotMarked { id }) if id == never.iri()
            ),
            "{err:?}"
        );
    }

    // Routing spec §3.3 step 1: "A finding raised against a marked record is
    // not a write to it and is allowed."
    #[test]
    fn a_finding_raised_about_a_marked_record_is_allowed() {
        let w = world();
        let t = w.router();
        let r = w.record(Some("code"), "t");
        w.issues.set_fail_next_create(true);
        w.run(r.iri(), Kind::Record).unwrap_err();
        let mut f = Finding::raise(w.p.clone(), r.clone(), "bob", "c");
        f.area = Some("code".into());
        let f = t.add_finding(f).unwrap();
        assert_eq!(w.local.get_finding(&f).unwrap().unwrap().record, r);
        // And it goes out with the record when the rerun finishes.
        let at = t.prepare_escalation(r.iri(), Kind::Record).unwrap();
        assert!(
            matches!(at.outgoing(), Outgoing::Record { findings, .. } if findings[0].id == f),
            "{:?}",
            at.outgoing()
        );
    }
}
```

In `crates/core/src/lib.rs`, after `pub mod decision;`, add:

```rust
pub mod escalate;
```

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test -p fl-core --lib escalate::tests::
```

Expected: FAIL to compile — `error[E0599]: no method named `prepare_escalation` found for struct `tiered::TieredTracker<'a>` in the current scope` (14), the same for `escalate` (8) and `abandon_escalation` (4), and two `E0614` errors that follow from them (28 errors; a warning, `unused import: `Record``).

- [ ] **Step 3: Implement**

In `crates/core/src/tiered.rs`, make two helpers visible to the crate — replace `    fn map_of(&self, project: &ProjectId) -> Result<RoutingMap, StoreError> {` with:

```rust
    pub(crate) fn map_of(&self, project: &ProjectId) -> Result<RoutingMap, StoreError> {
```

and `    fn private_or_refuse(&self, by_map: bool, what: &str) -> Result<(), StoreError> {` with:

```rust
    pub(crate) fn private_or_refuse(&self, by_map: bool, what: &str) -> Result<(), StoreError> {
```

In `crates/core/src/escalate.rs`, after the `use` lines and before `#[cfg(test)]`, add:

```rust
/// The longest issue title the GitHub tracker writes: GitHub's limit, in
/// characters (routing spec §3.2). fl-core cannot see `fl_github`'s own
/// `TITLE_MAX`, so it keeps this one for the check before the mark.
pub const ISSUE_TITLE_MAX: usize = 256;

/// An escalation checked and ready to run (routing spec §3.2): what goes
/// out, and where an earlier run stopped. Only the router builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    id: Iri,
    kind: Kind,
    outgoing: Outgoing,
    resumes: Option<Mark>,
    found: Option<Iri>,
}

impl Prepared {
    /// The item's primary local IRI: its issue's create key and first alias.
    pub fn id(&self) -> &Iri {
        &self.id
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// What the escalation writes to GitHub, for the warning the CLI prints
    /// before it publishes to a repository that is not private.
    pub fn outgoing(&self) -> &Outgoing {
        &self.outgoing
    }

    /// The mark an earlier run left: this run resumes it, with its own who,
    /// why and time.
    pub fn resumes(&self) -> Option<&Mark> {
        self.resumes.as_ref()
    }

    /// The issue an earlier run made: `escalate` finishes it — its labels,
    /// if a stop left them off, then the tombstone.
    pub fn found(&self) -> Option<&Iri> {
        self.found.as_ref()
    }
}

/// A local item, read for its escalation.
enum Local {
    Record(Record),
    Finding(Finding),
}

impl Local {
    /// The primary IRI.
    fn id(&self) -> &Iri {
        match self {
            Local::Record(r) => r.id.iri(),
            Local::Finding(f) => f.id.iri(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            Local::Record(_) => Kind::Record,
            Local::Finding(_) => Kind::Finding,
        }
    }

    /// The closed state it is in, if any (routing spec §3.2): a record
    /// `done`, a finding `fixed` or `withdrawn`.
    fn closed(&self) -> Option<&'static str> {
        match self {
            Local::Record(r) => (r.state == State::Done).then(|| r.state.as_wire()),
            Local::Finding(f) => matches!(f.state, FindingState::Fixed | FindingState::Withdrawn)
                .then(|| f.state.as_wire()),
        }
    }
}

/// Whether an item in `area` is sensitive (decisions 21, 22): an area the
/// map marks so, or one it no longer declares, which may have been. An item
/// with no area — made before the project was routed — is not.
fn sensitive(map: &RoutingMap, area: Option<&str>) -> bool {
    area.is_some_and(|a| map.route(a).is_none_or(|r| r.sensitive))
}

/// Why the GitHub tracker would refuse `title` for an issue, if it would.
fn title_refused(title: &str) -> Option<String> {
    if title.is_empty() {
        return Some("it is empty, and an issue needs a title".into());
    }
    let n = title.chars().count();
    if n > ISSUE_TITLE_MAX {
        return Some(format!(
            "it is {n} characters, and GitHub's limit is {ISSUE_TITLE_MAX}"
        ));
    }
    if title.trim() != title {
        return Some("it starts or ends with whitespace, which GitHub may trim".into());
    }
    None
}

impl TieredTracker<'_> {
    /// The local item `id` names, as the `kind` asked for (routing spec
    /// §3.1): an id GitHub claims is not local, even when the local tier
    /// holds it as an alias; a tombstoned one names where it went.
    fn local_item(&self, id: &Iri, kind: Kind) -> Result<Local, StoreError> {
        let not_local = || StoreError::from(EscalationFault::NotLocal { id: id.clone() });
        if self.github.claims(id) {
            return Err(not_local());
        }
        if let Some(t) = self.escalations.tombstone_of(id)? {
            return Err(EscalationFault::AlreadyEscalated {
                id: t.from,
                to: t.to,
            }
            .into());
        }
        let found = match self.catalog.kind_of(id) {
            Err(StoreError::NotOwned { .. }) => return Err(not_local()),
            other => other?,
        };
        let expected = match kind {
            Kind::Finding => Kind::Finding,
            _ => Kind::Record,
        };
        if found != expected {
            return Err(StoreError::WrongKind {
                id: id.clone(),
                expected,
                found,
            });
        }
        if expected == Kind::Record {
            let id = RecordId(id.clone());
            let r = self.local.get_record(&id)?;
            r.map(Local::Record).ok_or(StoreError::NoSuchRecord(id))
        } else {
            let id = FindingId(id.clone());
            let f = self.local.get_finding(&id)?;
            f.map(Local::Finding).ok_or(StoreError::NoSuchFinding(id))
        }
    }

    /// The one-namespace rule (routing spec §3.2): no name of the item may
    /// already name something on GitHub.
    fn names_free(&self, id: &Iri, aliases: &[Iri]) -> Result<(), StoreError> {
        for alias in std::iter::once(id).chain(aliases) {
            if let Some(issue) = self.github.alias_taken(alias)? {
                return Err(EscalationFault::AliasTaken {
                    alias: alias.clone(),
                    issue,
                }
                .into());
            }
        }
        Ok(())
    }

    /// The record's open findings in both tiers, about it by its IRI or any
    /// of its aliases (decision 18), as the record's issue lists them: never
    /// a security finding, nor one in a sensitive area or an area `map` no
    /// longer declares (decisions 21, 22) — the list is published.
    fn open_findings(&self, record: &Record, map: &RoutingMap) -> Result<Vec<Finding>, StoreError> {
        let about =
            |f: &Finding| f.record == record.id || record.also_known_as.contains(f.record.iri());
        Ok(self
            .findings(&record.project, None)?
            .into_iter()
            .map(|(_, f)| f)
            .filter(|f| !matches!(f.state, FindingState::Fixed | FindingState::Withdrawn))
            .filter(|f| !f.security && !sensitive(map, f.area.as_deref()))
            .filter(about)
            .collect())
    }

    /// Every check the GitHub create would make, before anything is written
    /// (routing spec §3.2), and what the escalation would write. Writes
    /// nothing.
    ///
    /// ⚠ On an item an earlier run marked, the search comes first: an issue
    /// it finds passed these checks when it was made — and its aliases now
    /// include the item's own IRI — so only finishing it is left. With
    /// none found, every check runs again: the repository may have turned
    /// public since.
    pub fn prepare_escalation(&self, id: &Iri, kind: Kind) -> Result<Prepared, StoreError> {
        let item = self.local_item(id, kind)?;
        let primary = item.id().clone();
        let project = match &item {
            Local::Record(r) => &r.project,
            Local::Finding(f) => &f.project,
        };
        let map = self.map_of(project)?;
        self.github.tracker()?;
        let resumes = self.escalations.mark_of(&primary)?;
        let found = match &resumes {
            Some(mark) => self.github.find_escalated(&primary, mark.at_ms)?,
            None => None,
        };
        let check = found.is_none();
        if check && let Some(state) = item.closed() {
            return Err(EscalationFault::Closed {
                id: primary,
                state: state.to_string(),
            }
            .into());
        }
        let outgoing = match item {
            Local::Record(record) => {
                if check {
                    if let Some(why) = title_refused(&record.title) {
                        return Err(EscalationFault::Title { id: primary, why }.into());
                    }
                    if sensitive(&map, record.area.as_deref()) {
                        self.private_or_refuse(false, "this record")?;
                    }
                    self.names_free(&primary, &record.also_known_as)?;
                }
                let findings = self.open_findings(&record, &map)?;
                Outgoing::Record { record, findings }
            }
            Local::Finding(finding) => {
                let (tier, record) = self.record_of(&finding.record)?;
                if check {
                    // Routing spec decision 21: a finding about a local record in a
                    // sensitive area would publish that record's title.
                    let own = finding.security || sensitive(&map, finding.area.as_deref());
                    let about = tier == Tier::Local && sensitive(&map, record.area.as_deref());
                    if own || about {
                        self.private_or_refuse(
                            false,
                            if own {
                                "this finding"
                            } else {
                                "this finding, about a record in a sensitive area,"
                            },
                        )?;
                    }
                    self.names_free(&primary, &finding.also_known_as)?;
                }
                let record = RecordSeen {
                    id: record.id,
                    title: record.title,
                    tier,
                };
                Outgoing::Finding { finding, record }
            }
        };
        Ok(Prepared {
            id: primary,
            kind: outgoing.kind(),
            outgoing,
            resumes,
            found,
        })
    }

    /// Run the escalation `at` (routing spec §3.3): mark the local item,
    /// find or create its issue, replace the local item with a tombstone.
    /// Answers the issue.
    ///
    /// ⚠ A run that resumes a mark keeps the mark's who, why and time, and
    /// ignores `by`, `reason` and `now_ms`. Any error after the mark leaves
    /// it: the item stays unwritable, and a rerun resumes.
    ///
    /// ⚠ An issue an earlier run made goes through `create_escalated` too:
    /// it searches first and makes no second issue, and it gives a found
    /// issue the labels a stop between the create and the label call left
    /// off (routing spec §3.3) — a tombstone alone would point at an issue
    /// fl cannot read.
    pub fn escalate(
        &self,
        at: &Prepared,
        by: &str,
        reason: &str,
        now_ms: u64,
    ) -> Result<Iri, StoreError> {
        let mark = match &at.resumes {
            Some(mark) => mark.clone(),
            None => {
                let mark = Mark {
                    by: by.to_string(),
                    reason: reason.to_string(),
                    at_ms: now_ms,
                };
                self.escalations.mark(&at.id, &mark)?;
                mark
            }
        };
        let from = Provenance {
            from: at.id.clone(),
            by: mark.by,
            reason: mark.reason,
        };
        let issue = self
            .github
            .create_escalated(&at.outgoing, &from, mark.at_ms)?;
        self.escalations.tombstone(&at.id, &issue)?;
        Ok(issue)
    }

    /// `--abandon` (routing spec §3.3): remove the mark, only after the
    /// search proves no issue exists. ⚠ Once the issue exists, the only way
    /// on is the tombstone: `escalate` finishes it.
    pub fn abandon_escalation(&self, id: &Iri, kind: Kind) -> Result<(), StoreError> {
        let item = self.local_item(id, kind)?;
        let primary = item.id().clone();
        let Some(mark) = self.escalations.mark_of(&primary)? else {
            return Err(EscalationFault::NotMarked { id: primary }.into());
        };
        self.github.tracker()?;
        if let Some(issue) = self.github.find_escalated(&primary, mark.at_ms)? {
            return Err(EscalationFault::IssueExists {
                id: primary,
                kind: item.kind(),
                issue,
            }
            .into());
        }
        self.escalations.unmark(&primary)
    }
}
```

In `crates/core/src/lib.rs`, after `pub use decision::{Decision, DecisionKind, Flushed, LeftLocal, Outcome, TransitionOutcome};`, add:

```rust
pub use escalate::Prepared;
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p fl-core --lib escalate::tests::
```

Expected: PASS (13 tests).

- [ ] **Step 5: Mutation checks**

Each: change → run → red → restore → `cmp` against the saved copy. Every filter is `cargo test -p fl-core --lib escalate::tests::<name>`; `not_local` below is `an_item_that_is_not_local_or_was_escalated_already_is_refused_before_the_mark`, `rerun` is `a_rerun_after_each_step_finishes_with_one_issue_and_one_live_copy`, `sensitive` is `nothing_sensitive_is_escalated_to_a_public_repository`, `record_out` is `a_record_escalates_with_its_state_aliases_and_open_findings`, `keeps` is `a_rerun_keeps_the_marks_who_and_why`, `abandon` is `an_escalation_is_abandoned_only_before_its_issue_exists`.

1. The claim: delete `if self.github.claims(id) { return Err(not_local()); }` in `local_item` → `not_local` red (an issue URL the local tier holds as an alias escalates that item).
2. The tombstone: make it `if let Some(t) = None::<crate::escalation::Tombstone> {` → `not_local` red (the tombstoned record reads as `Escalated`, not `AlreadyEscalated`).
3. Not held: delete the `Err(StoreError::NotOwned { .. }) => return Err(not_local()),` arm → `not_local` red (`NotOwned`).
4. The kind: make the guard `if false && found != expected {` → `not_local` red (`NoSuchRecord` for a finding's id).
5. The kind asked: make `_ => Kind::Record` read `other => other` → `not_local` red (a project asked as a project passes the kind check).
6. The map first: swap `let map = self.map_of(project)?;` and `self.github.tracker()?;` → `an_unrouted_or_unbound_project_is_refused_before_the_mark` red (an unrouted project on an unbound machine reads `TierUnavailable`).
7. GitHub open: delete `self.github.tracker()?;` in `prepare_escalation` → `an_unrouted_or_unbound_project_is_refused_before_the_mark` red (`TierUnreadable` from the findings list, not `TierUnavailable`).
8. The search before the checks — `let check = true;` → `rerun` red (the rerun after a lost answer is refused `AliasTaken`: its own IRI is its issue's alias now).
9. Every check again when nothing is found — `let check = resumes.is_none();` → `a_rerun_that_finds_no_issue_makes_every_check_again` red (a marked security record on a repository now public is not refused).
10. The search runs on a mark — `Some(_) => None,` for `Some(mark) => self.github.find_escalated(&primary, mark.at_ms)?,` → `rerun` red.
11. A found issue goes through the create: put back a short-cut at the top of `escalate`, `if let Some(issue) = &at.found { self.escalations.tombstone(&at.id, issue)?; return Ok(issue.clone()); }` → `rerun` red ("a found issue is finished through the create": `create_escalated` is not asked, so an issue left unlabelled by a stop before its label call would stay unlabelled). `MemIssues` has no labels, so the test watches the call; the labels themselves are the GitHub tracker's `create_escalated` (Task 4) and Task 8's black-box test.
12. A record `done`: `(r.state == State::Done && false)` → `a_closed_item_is_refused_before_the_mark` red.
13. A finding `fixed`: drop `FindingState::Fixed |` from `closed` → `a_closed_item_is_refused_before_the_mark` red.
14. A finding `withdrawn`: drop `| FindingState::Withdrawn` from `closed` → `a_closed_item_is_refused_before_the_mark` red.
15. An empty title: delete `if title.is_empty() { return Some("it is empty, and an issue needs a title".into()); }` → `a_title_github_would_refuse_is_refused_before_the_mark` red (an empty record title prepares). The title's length: `if false && n > ISSUE_TITLE_MAX {` → `a_title_github_would_refuse_is_refused_before_the_mark` red.
16. Characters, not bytes: `let n = title.len();` → `a_title_github_would_refuse_is_refused_before_the_mark` red (256 two-byte characters refused).
17. Whitespace: `if false {` for `if title.trim() != title {` → `a_title_github_would_refuse_is_refused_before_the_mark` red.
18. Records only: `if let Some(why) = None::<String> {` for `if let Some(why) = title_refused(&record.title) {` → `a_title_github_would_refuse_is_refused_before_the_mark` red; a finding's claim is never checked as a title (the test's long, space-led claim prepares).
19. A sensitive record: `if false && sensitive(&map, record.area.as_deref()) {` → `sensitive` red.
20. Only a sensitive record: `if true {` for the record's `if sensitive(…) {` → `sensitive` red (a `code` record is refused).
21. An undeclared area is sensitive: `map.route(a).is_some_and(|r| r.sensitive)` in `sensitive` → `sensitive` red.
22. No area is not sensitive: `area.is_none_or(|a| …)` in `sensitive` → `sensitive` red.
23. A security finding: `let own = sensitive(&map, finding.area.as_deref());` → `sensitive` red (the mark is written, then GitHub refuses: the stranding this check prevents).
24. A finding's own area: `let own = finding.security;` → `sensitive` red.
25. A finding about a local sensitive record: `let about = false;` → `sensitive` red.
26. Only a LOCAL record: drop `tier == Tier::Local &&` → `sensitive` red (a finding about a record GitHub holds in `security` is refused; its title is published already).
27. The wording: `if !own {` → `sensitive` red. And `self.private_or_refuse(true, "this record")` → `sensitive` red (`SensitiveToPublic`, which claims the map sent it).
28. The item's own IRI: `for alias in aliases {` → `an_alias_github_already_uses_is_refused_before_the_mark` red.
29. Every alias: `.chain(aliases.iter().take(1))` → `an_alias_github_already_uses_is_refused_before_the_mark` red (the second alias is taken).
30. Names checked for a record — delete `self.names_free(&primary, &record.also_known_as)?;` → `an_alias_github_already_uses_is_refused_before_the_mark` red; for a finding — delete `self.names_free(&primary, &finding.also_known_as)?;` → the same test red.
31. Open findings: delete `.filter(|f| !matches!(f.state, FindingState::Fixed | FindingState::Withdrawn))` → `record_out` red.
32. Non-security: `.filter(|f| !sensitive(map, f.area.as_deref()))` → `record_out` red. Not sensitive: `.filter(|f| !f.security)` → `record_out` red (the findings in `security` and in the undeclared `gone` go out).
33. About this record: delete `.filter(about)` → `record_out` red. By an alias too: drop `|| record.also_known_as.contains(f.record.iri())` → `record_out` red.
34. Both tiers: `.findings(&record.project, Some(Tier::Local))?` → `record_out` red.
35. The mark: delete `self.escalations.mark(&at.id, &mark)?;` → `rerun` red (not marked after a stop before the create).
36. Mark before create: build the mark without writing it, call `create_escalated`, then `if at.resumes.is_none() { self.escalations.mark(&at.id, &mark)?; }` → `rerun` red (a create that fails leaves no mark: the item stays writable).
37. A resume does not mark again: `Some(mark) => { self.escalations.mark(&at.id, mark)?; mark.clone() }` → `rerun` red (`AlreadyMarked`).
38. A resume keeps the mark: `Some(_) => Mark { by: by.to_string(), reason: reason.to_string(), at_ms: now_ms },` → `keeps` red. The provenance from the mark: `by: by.to_string(), reason: reason.to_string(),` in `Provenance` → `keeps` red. The search from the mark's time: `create_escalated(&at.outgoing, &from, now_ms)` → `keeps` red.
39. The mark's time: `at_ms: 0,` for `at_ms: now_ms,` → `record_out` red (the tombstone's time).
40. Abandon asks GitHub: delete `self.github.tracker()?;` in `abandon_escalation` → `abandon` red (an unbound tier's search finds nothing, and the mark is removed).
41. Abandon's found guard: `if let Some(issue) = None::<Iri> {` → `abandon` red (the mark is removed though the issue exists).
42. No mark, before GitHub: move `self.github.tracker()?;` above the `let Some(mark) = … else` → `abandon` red (`TierUnavailable`, not `NotMarked`).

Not observable: create before tombstone — `tombstone` takes the issue `create_escalated` answers, so no other order compiles.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1156 passed, 18 ignored.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/escalate.rs crates/core/src/lib.rs crates/core/src/tiered.rs
git commit -m "feat(core): the router escalates: pre-checks, mark, find or create, tombstone, abandon

TieredTracker gains prepare_escalation, escalate and abandon_escalation.
prepare_escalation writes nothing: it refuses an id GitHub claims, a
tombstoned item (naming where it went), an id the local tier does not
hold, the wrong kind, a project with no map and a GitHub tier that cannot
be opened; then, for an item an earlier run marked, it searches for the
issue by the old IRI first — a found issue passed every check when it
was made, so only finishing it is left. Otherwise it refuses a closed
state, a record title GitHub would refuse (an empty one too), anything
sensitive on a repository that is not private (a sensitive or undeclared
area, a security finding, a finding about a local record in such an
area), and any name of the item GitHub already uses; then it reads what
goes out: a record with its open findings from both tiers — none a
security finding or in a sensitive or undeclared area — or a finding
with its record as the router reads it now. escalate marks the
item (a resume keeps the mark's who, why and time), creates or finds the
issue, and writes the tombstone; any error after the mark leaves it, and
a rerun resumes. abandon_escalation removes the mark only after the
search proves no issue exists. map_of and private_or_refuse become
pub(crate). Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

---

### Task 8: `fl record escalate` and `fl finding escalate`

A person escalates a local item with a command of its kind (routing spec §3.1): `fl record escalate <id> --by <who> --reason <why>`, or `fl finding escalate …`, and `--abandon` to stop one that stopped (§3.3). The router makes every check the GitHub create would make and runs the three steps (Task 7); this task is the CLI around it. A store with no routing map has no local tier, so it is refused naming `fl routing set` (plan ruling 21). The checks that need the working tree run here, after the router's and still before anything is written (plan ruling 11): the routing map the manifest carries (§1.2, as for a new item), and, for a finding with a reproduction gate, the gate in the committed manifest — the issue names it where another machine reads it (GitHub tracker spec §4.3). An issue an earlier run made passed those checks when it was made, so a rerun that finds one skips them (plan ruling 13). On a repository that is not private, "anything else is warned about before it is published, naming what will be published" (§3.2, decision 18): one `warning:` line with the repository, its visibility, the title or claim, the item's local IRI (the issue's text names it and it becomes one of the issue's aliases), the reason, who escalated it, and a record's number of open findings; a visibility that cannot be read refuses, since an unknown visibility is not private. A rerun keeps the mark's who and why and says so when it was given others (plan ruling 12). The command prints `<old handle>\tescalated\t#<n>` or `<handle>\tabandoned` (plan ruling 19), the old handle read before the escalation. A failure that leaves the item marked names the command that finishes it, or `--abandon` — "running the command again resumes from where it stopped" (§3.3).

**Blast radius:** `fl record` and `fl finding` gain an `escalate` subcommand each; no existing subcommand changes, and `iris()`/`has_handle()` pick up its id through `refs()`. `cmd::routing::not_routed` is not reused: its wording is about `--area`/`--tier`; `escalate.rs` has a sibling of its own. `docs/routing.md` gains a section. No other behaviour changes.

**Files:**
- Create: `crates/cli/src/cmd/escalate.rs` (`run`; private `not_routed`, `escalate`, `check_tree`, `warn_disclosure`)
- Create: `crates/cli/tests/escalation.rs` (black-box escalation tests)
- Modify: `crates/cli/src/cmd/mod.rs` (`pub mod escalate;`)
- Modify: `crates/cli/src/cmd/record.rs` (`Cmd::Escalate`; `refs()`; `run`'s arm)
- Modify: `crates/cli/src/cmd/finding.rs` (`Cmd::Escalate`; `refs()`; `run`'s arm)
- Modify: `docs/routing.md` (section "Escalating an item")

**Interfaces:**
- Consumes: `Outgoing` (Task 1); `fl_github::meta::{parse_body, Meta}` with `Meta.escalated` and `fl_format` 3 (Task 3); `GithubTier::create_escalated` (Task 5); `TieredTracker::escalating` (Task 6); `Prepared::{outgoing, resumes, found}`, `TieredTracker::{prepare_escalation, escalate, abandon_escalation}` and their refusals (Task 7); a rerun's found issue is finished through `create_escalated`, which labels an issue a stop left without fl's labels (Task 7's `escalate`); `Ctx::{resolve_item, show_item}`, `Ctx.tiers`, `LazyGithub::open`, `GithubTracker::{visibility, repo}`, `cmd::manifest::{ensure_routing_current, ensure_publishable}`, and `FakeGithub`'s `fail_label_add_next`, `rate_limited_next_create`, `fail_repo_read_after`, `fail_issues_query_after`, `issue`, `issue_count` (existing). The router's best-effort redirect for a marked item (Task 6, plan ruling 23), which this task pins end to end where `MemIssues` cannot: an issue with no fl labels, and a search that fails.
- Produces: `pub fn run(ctx: &Ctx<'_>, kind: Kind, id: &Ref, by: Option<&str>, reason: Option<&str>, abandon: bool) -> Result<i32>` in `crates/cli/src/cmd/escalate.rs` — `--abandon` takes neither, so both are `Option`, and `run` reads them only when `abandon` is false (clap requires both then). `record::Cmd::Escalate` and `finding::Cmd::Escalate`, each `{ id: Ref, by: Option<String>, reason: Option<String>, abandon: bool }` (`--by`/`--reason` `required_unless_present = "abandon"`; `--abandon` `conflicts_with_all = ["by", "reason"]`). Task 9 adds `after_landed_move` to this module.
- Produces, beyond the shared block: the context on a failure is added when `t.router.escalating(&iri)` reads `Ok(Some(_))` — a mark that cannot be read leaves the error as it is — and to any error once the item is marked, a rerun's included; the warning on a rerun names the mark's who and why, which is what the issue publishes.
- Unique phrases: `has no local tier to escalate from` (the unrouted refusal), `the escalation publishes` (the warning), `its local IRI, <IRI>,` (the item's IRI in the warning), `the mark's who and why are kept` (the note), `stopped after its mark was written` (the context). The tests also assert Task 1's `is not a local item`, `is in a closed state`, `its issue exists`, `is not marked escalating` and `so this store refuses to change it`, Task 3's `Escalated from the local tier by`, Task 4's `Open findings when this record was escalated:`, Task 7's sensitivity wording through `RoutingFault::SensitiveNamedPublic`'s `Use the local tier for it`, the manifest checks' `changed since the manifest at` and `committed manifest: commit it`, and the tracker's `could not read the visibility of`.

- [ ] **Step 1: Write the failing tests**

Create `crates/cli/tests/escalation.rs`:

```rust
//! Escalation (routing spec §3): `fl record escalate` and `fl finding
//! escalate` move a local item to the fake GitHub's `acme/widgets`, after
//! every check, and a rerun finishes what a stop left.

use assert_cmd::Command;
use fl_github::fake::FakeGithub;
use predicates::str::contains;
use std::fs;
use std::path::Path;
use std::process::Command as Sys;

fn git(dir: &Path, args: &[&str]) {
    let out = Sys::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The config line binding the project to the fake's repository.
const BOUND: &str = "tracker = { github = \"acme/widgets\", credential = \"env\" }\n";

/// The disclosure warning's own words (routing spec §3.2).
const WARNED: &str = "the escalation publishes";

/// The context a failure after the mark carries (routing spec §3.3).
const STOPPED: &str = "stopped after its mark was written";

/// The note a rerun given another who or why prints.
const KEPT: &str = "the mark's who and why are kept";

struct R {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    fake: FakeGithub,
}

/// A git working tree with `src/a.rs`, whose project's config entry
/// carries `tracker` — [`BOUND`], or "" for none.
fn world(tracker: &str) -> R {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/a.rs"), "fn a() {}").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    let r = R {
        home,
        repo,
        fake: FakeGithub::start("acme/widgets"),
    };
    let cfg = format!(
        "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n{tracker}",
        r.repo.path().canonicalize().unwrap().display(),
        r.home.path().join("fl.redb").display()
    );
    fs::create_dir_all(r.home.path().join("config/fl")).unwrap();
    fs::write(r.home.path().join("config/fl/config.toml"), cfg).unwrap();
    r
}

impl R {
    fn fl(&self) -> Command {
        let home = self.home.path();
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("FL_GITHUB_TOKEN", "t")
            .env("FL_GITHUB_API_URL", self.fake.url())
            .env_remove("GITHUB_TOKEN")
            .env_remove("FL_DB")
            .current_dir(self.repo.path());
        c
    }

    /// `fl args`, which must succeed; its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.fl().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "fl {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// `fl args`, which must succeed; its stdout and stderr.
    fn ok_said(&self, args: &[&str]) -> (String, String) {
        let out = self.fl().args(args).output().unwrap();
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(out.status.success(), "fl {args:?}: {err}");
        (String::from_utf8(out.stdout).unwrap(), err)
    }

    /// `fl args`, which must fail with exit 2; its stderr.
    fn refused(&self, args: &[&str]) -> String {
        let out = self.fl().args(args).output().unwrap();
        let err = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.status.code(), Some(2), "fl {args:?}: {err}");
        err
    }

    /// The project, routed with the starting set (`code` and `tests` local).
    fn routed(&self) {
        self.ok(&["project", "add", "."]);
        self.ok(&["routing", "set", "--project", "1", "code", "local"]);
    }

    /// A local record in `area`, with `--tier local`.
    fn local_record(&self, title: &str, area: &str) {
        self.ok(&[
            "record",
            "add",
            "--project",
            "1",
            "--title",
            title,
            "--area",
            area,
            "--tier",
            "local",
        ]);
    }

    /// Issue `n`'s block.
    fn block(&self, n: u64) -> fl_github::meta::Meta {
        fl_github::meta::parse_body(&self.fake.issue(n).body)
            .unwrap()
            .1
    }
}

// Routing spec §3.3: a local record becomes an issue with its own state,
// area and open findings, its old IRI as the create key and an alias, and
// a line naming who escalated it and why; the old handle then names the
// issue (§2.3).
#[test]
fn a_record_escalates_to_an_issue_with_its_state_area_findings_and_provenance() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&["record", "move", "1", "--to", "doing"]);
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "1",
        "--claim",
        "it drops a token",
        "--by",
        "rev",
    ]);
    let (out, err) = g.ok_said(&[
        "record",
        "escalate",
        "1",
        "--by",
        "alice",
        "--reason",
        "needs a design call",
    ]);
    assert_eq!(out, "1\tescalated\t#1\n");
    assert!(!err.contains(WARNED), "a private repository: {err}");
    assert_eq!(g.fake.issue_count(), 1);
    let issue = g.fake.issue(1);
    assert_eq!(issue.title, "fix the parser");
    assert_eq!(
        issue.labels,
        vec!["fl:record", "fl:record/doing", "fl:area/code"]
    );
    let block = g.block(1);
    assert_eq!(block.fl_format, 3);
    let from = block.escalated.as_ref().unwrap();
    assert_eq!(
        (from.by.as_str(), from.reason.as_str()),
        ("alice", "needs a design call")
    );
    assert_eq!(block.create_key, from.from.as_str());
    assert_eq!(block.also_known_as, vec![from.from.clone()]);
    assert!(
        issue.body.contains(&format!(
            "Escalated from the local tier by alice: needs a design call. Its local IRI was {}.",
            from.from
        )),
        "{}",
        issue.body
    );
    assert!(
        issue
            .body
            .contains("Open findings when this record was escalated:")
            && issue.body.contains("- raised: it drops a token — "),
        "{}",
        issue.body
    );
    // The tombstoned row is left out; the issue is listed in its place.
    assert_eq!(
        g.ok(&["record", "list", "--project", "1"]),
        "#1\tgithub\tdoing\tfix the parser\n"
    );
    // The old handle follows the tombstone to the issue.
    let moved = g.ok(&["record", "move", "1", "--to", "review"]);
    assert!(moved.starts_with("#1\treview\t"), "{moved}");
    assert!(
        g.fake
            .issue(1)
            .labels
            .contains(&"fl:record/review".to_string())
    );
}

// Routing spec §2.5, §3.3: a finding about a local record escalates, and
// its issue names the record, which stays local.
#[test]
fn a_finding_about_a_local_record_escalates_and_its_issue_names_the_record() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "1",
        "--claim",
        "it drops a token",
        "--by",
        "rev",
    ]);
    assert_eq!(
        g.ok(&[
            "finding",
            "escalate",
            "1",
            "--by",
            "alice",
            "--reason",
            "a person decides",
        ]),
        "1\tescalated\t#1\n"
    );
    let issue = g.fake.issue(1);
    assert_eq!(
        issue.labels,
        vec!["fl:finding", "fl:finding/raised", "fl:area/code"]
    );
    assert!(
        issue.body.contains("Record: fix the parser — urn:uuid:")
            && issue
                .body
                .contains("held in the local tier, not on GitHub."),
        "{}",
        issue.body
    );
    assert!(
        issue
            .body
            .contains("Escalated from the local tier by alice: a person decides."),
        "{}",
        issue.body
    );
    assert_eq!(
        g.ok(&["record", "list", "--project", "1"]),
        "1\tlocal\ttodo\tfix the parser\n"
    );
}

// Routing spec §3.2, decisions 18 and 21: to a public repository a
// sensitive item is refused before the mark, and anything else is published
// after a warning naming what goes out.
#[test]
fn an_escalation_to_a_public_repository_warns_what_it_publishes() {
    let g = world(BOUND);
    g.routed();
    g.fake.state().repos[0].visibility = "public".into();
    g.local_record("rotate the keys", "security");
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("this record is security-sensitive")
            && err.contains("Use the local tier for it")
            && !err.contains(WARNED)
            && !err.contains(STOPPED),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    // Not marked: the record still moves.
    g.ok(&["record", "move", "1", "--to", "doing"]);

    g.local_record("fix the parser", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "2",
        "--claim",
        "it drops a token",
        "--by",
        "rev",
    ]);
    let (out, err) = g.ok_said(&[
        "record",
        "escalate",
        "2",
        "--by",
        "alice",
        "--reason",
        "needs a design call",
    ]);
    assert_eq!(out, "2\tescalated\t#1\n");
    let iri = g.block(1).escalated.unwrap().from;
    assert!(
        err.contains("warning: acme/widgets is public")
            && err.contains(WARNED)
            && err.contains("\"fix the parser\"")
            && err.contains(&format!("its local IRI, {iri},"))
            && err.contains("and its 1 open findings"),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 1);

    // A finding about a record on GitHub publishes its claim; one about a
    // local record publishes that record's title and IRI too.
    let (out, err) = g.ok_said(&["finding", "escalate", "1", "--by", "bob", "--reason", "why"]);
    assert_eq!(out, "1\tescalated\t#2\n");
    let iri = g.block(2).escalated.unwrap().from;
    assert!(
        err.contains(WARNED)
            && err.contains("this finding's claim, \"it drops a token\"")
            && err.contains(&format!("its local IRI, {iri},"))
            && !err.contains("its local record's title"),
        "{err}"
    );
    g.local_record("tidy the lexer", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "3",
        "--claim",
        "it is slow",
        "--by",
        "rev",
    ]);
    let (_, err) = g.ok_said(&["finding", "escalate", "2", "--by", "bob", "--reason", "why"]);
    assert!(
        err.contains(WARNED) && err.contains("and its local record's title, \"tidy the lexer\""),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 3);
}

// Routing spec §3.2, decisions 18, 21 and 22: a record's issue lists its
// open findings, but never one in an area that is sensitive now — here
// marked so after the finding was raised, so it carries no security flag.
#[test]
fn a_records_issue_never_lists_a_finding_in_a_sensitive_area() {
    let g = world(BOUND);
    g.routed();
    g.fake.state().repos[0].visibility = "public".into();
    g.local_record("fix the parser", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "1",
        "--claim",
        "the key is in the log",
        "--area",
        "tests",
        "--by",
        "rev",
    ]);
    g.ok(&[
        "routing",
        "set",
        "--project",
        "1",
        "tests",
        "local",
        "--sensitive",
    ]);
    let (out, err) = g.ok_said(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert_eq!(out, "1\tescalated\t#1\n");
    let body = g.fake.issue(1).body;
    assert!(!body.contains("the key is in the log"), "{body}");
    assert!(
        err.contains(WARNED) && err.contains("and its 0 open findings"),
        "{err}"
    );
}

// Routing spec §3.2: a closed item is refused before the mark — the GitHub
// tracker creates open issues only.
#[test]
fn a_done_record_is_refused_before_the_mark() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&["record", "move", "1", "--to", "done"]);
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("is in a closed state") && !err.contains(STOPPED),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    // Not marked: the record still moves.
    g.ok(&["record", "move", "1", "--to", "doing"]);
}

// Routing spec §3.2: a visibility that cannot be read is not private, so
// the escalation is refused before the mark.
#[test]
fn an_unreadable_visibility_refuses_before_the_mark() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.fake.state().fail_repo_read_after = Some(1);
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("could not read the visibility of acme/widgets")
            && !err.contains(STOPPED)
            && !err.contains(WARNED),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    g.ok(&["record", "move", "1", "--to", "doing"]);
}

// Routing spec §3.3: a stop between the create and its labels leaves the
// item marked and the issue unlabelled. The command says how to finish;
// `--abandon` is refused, naming the issue; a rerun finds the issue —
// skipping the checks the issue already passed — and labels it.
#[test]
fn a_stop_between_the_create_and_its_labels_is_finished_by_a_rerun_with_one_issue() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    g.local_record("fix the parser", "code");
    g.fake.state().fail_label_add_next = true;
    let args = ["record", "escalate", "1", "--by", "alice", "--reason", "r"];
    let err = g.refused(&args);
    assert!(
        err.contains(STOPPED)
            && err.contains("`fl record escalate 1 --by <who> --reason <why>`")
            && err.contains("`fl record escalate 1 --abandon`"),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 1);
    let err = g.refused(&["record", "escalate", "1", "--abandon"]);
    assert!(
        err.contains("its issue exists")
            && err.contains("https://github.com/acme/widgets/issues/1"),
        "{err}"
    );
    // An issue fl cannot read — it has no labels yet — is not the record:
    // the local copy answers, refusing a move and allowing a finding about
    // it (routing spec §2.2, §3.3 step 1). So it does when the search fails,
    // and offline.
    let err = g.refused(&["record", "move", "1", "--to", "doing"]);
    assert!(
        err.contains("so this store refuses to change it") && err.contains("fl record escalate"),
        "{err}"
    );
    let raise = |claim: &'static str| {
        [
            "finding", "raise", "--record", "1", "--claim", claim, "--by", "rev",
        ]
    };
    g.ok(&raise("one"));
    g.fake.state().fail_issues_query_after = Some(0);
    g.ok(&raise("two"));
    let offline = g
        .fl()
        .env("FL_GITHUB_API_URL", "http://127.0.0.1:9")
        .args(raise("three"))
        .output()
        .unwrap();
    assert!(
        offline.status.success(),
        "{}",
        String::from_utf8_lossy(&offline.stderr)
    );
    // The routing map changes after the export: a first run would now be
    // refused, and the rerun is not.
    g.ok(&["routing", "set", "--project", "1", "product", "local"]);
    let (out, err) = g.ok_said(&args);
    assert_eq!(out, "1\tescalated\t#1\n");
    assert!(!err.contains(KEPT), "the same who and why: {err}");
    assert_eq!(g.fake.issue_count(), 1);
    assert_eq!(
        g.fake.issue(1).labels,
        vec!["fl:record", "fl:record/todo", "fl:area/code"]
    );
}

// Routing spec §3.3: a stop before the create leaves a mark and no issue;
// `--abandon` removes the mark, and the record is writable again.
#[test]
fn an_abandon_before_any_issue_clears_the_mark() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.fake.state().rate_limited_next_create = true;
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(err.contains(STOPPED), "{err}");
    let err = g.refused(&["record", "move", "1", "--to", "doing"]);
    assert!(err.contains("fl record escalate"), "{err}");
    assert_eq!(
        g.ok(&["record", "escalate", "1", "--abandon"]),
        "1\tabandoned\n"
    );
    g.ok(&["record", "move", "1", "--to", "doing"]);
    assert_eq!(g.fake.issue_count(), 0);
    let err = g.refused(&["record", "escalate", "1", "--abandon"]);
    assert!(err.contains("is not marked escalating"), "{err}");
}

// Routing spec §3.3: a rerun resumes with the mark's who and why, and says
// so when it was given others.
#[test]
fn a_rerun_with_another_reason_keeps_the_marks_and_says_so() {
    let g = world(BOUND);
    g.routed();
    // Public, so the warning says what goes out: the mark's reason.
    g.fake.state().repos[0].visibility = "public".into();
    g.local_record("fix the parser", "code");
    g.fake.state().rate_limited_next_create = true;
    let first = g.refused(&[
        "record", "escalate", "1", "--by", "alice", "--reason", "first",
    ]);
    assert!(!first.contains(KEPT), "{first}");
    let (out, err) = g.ok_said(&[
        "record", "escalate", "1", "--by", "alice", "--reason", "second",
    ]);
    assert_eq!(out, "1\tescalated\t#1\n");
    assert!(
        err.contains(KEPT)
            && err.contains("by alice for \"first\"")
            && err.contains("the reason, \"first\"")
            && !err.contains("the reason, \"second\""),
        "{err}"
    );
    assert!(
        g.fake
            .issue(1)
            .body
            .contains("Escalated from the local tier by alice: first."),
        "{}",
        g.fake.issue(1).body
    );
}

// GitHub tracker spec §4.3: an escalated finding names its reproduction
// gate where another machine reads it, so the gate must be in the committed
// manifest — checked before the mark.
#[test]
fn a_finding_whose_gate_is_not_in_the_committed_manifest_is_refused_before_the_mark() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "1",
        "--claim",
        "it drops a token",
        "--by",
        "rev",
    ]);
    g.ok(&[
        "gate",
        "add",
        "--project",
        "1",
        "--name",
        "g",
        "--glob",
        "src/**/*.rs",
        "--program",
        "false",
    ]);
    g.ok(&["finding", "reproduce", "1", "--gate", "1"]);
    g.ok(&["manifest", "export", "--project", "1"]);
    let err = g.refused(&["finding", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("is not committed") && err.contains("committed manifest: commit it"),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    // Not marked: the finding is still writable.
    g.ok(&["finding", "assign", "1", "--to", "bob"]);
}

// Routing spec §1.2: a routing-map change the manifest does not carry
// refuses the escalation before the mark, as it refuses a new item.
#[test]
fn a_routing_map_the_manifest_does_not_carry_is_refused_before_the_mark() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    g.local_record("fix the parser", "code");
    g.ok(&["routing", "set", "--project", "1", "product", "local"]);
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("changed since the manifest at") && !err.contains(STOPPED),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    g.ok(&["record", "move", "1", "--to", "doing"]);
}

// Routing spec §3.1: only a routed store has a local tier to escalate from.
#[test]
fn an_unrouted_store_is_refused_naming_fl_routing_set() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.ok(&["record", "add", "--project", "1", "--title", "t"]);
    let err = g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains("has no local tier to escalate from") && err.contains("fl routing set"),
        "{err}"
    );
    let err = g.refused(&["finding", "escalate", "1", "--abandon"]);
    assert!(err.contains("has no local tier to escalate from"), "{err}");
}

// Routing spec §3.1: a GitHub issue is not a local item.
#[test]
fn a_github_issue_is_refused_as_not_local() {
    let g = world(BOUND);
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "look",
        "--area",
        "design",
    ]);
    let err = g.refused(&["record", "escalate", "#1", "--by", "alice", "--reason", "r"]);
    assert!(err.contains("is not a local item"), "{err}");
}

// Routing spec §3.3 step 1: while a finding is marked, a write to it is
// refused naming the command that finishes the escalation.
#[test]
fn a_write_to_a_marked_finding_names_fl_finding_escalate() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "1",
        "--claim",
        "it drops a token",
        "--by",
        "rev",
    ]);
    g.fake.state().rate_limited_next_create = true;
    let err = g.refused(&["finding", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert!(
        err.contains(STOPPED) && err.contains("`fl finding escalate 1 --abandon`"),
        "{err}"
    );
    let err = g.refused(&["finding", "withdraw", "1", "--reason", "wrong"]);
    assert!(
        err.contains("so this store refuses to change it") && err.contains("fl finding escalate"),
        "{err}"
    );
}

// `--by` and `--reason` go together, and `--abandon` takes neither.
#[test]
fn an_escalation_takes_by_and_reason_or_abandon() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    for (args, said) in [
        (vec!["record", "escalate", "1"], "--by <BY>"),
        (
            vec!["record", "escalate", "1", "--by", "alice"],
            "--reason <REASON>",
        ),
        (
            vec!["record", "escalate", "1", "--abandon", "--by", "alice"],
            "cannot be used with",
        ),
        (
            vec!["finding", "escalate", "1", "--abandon", "--reason", "r"],
            "cannot be used with",
        ),
    ] {
        g.fl().args(&args).assert().code(2).stderr(contains(said));
    }
    assert_eq!(g.fake.issue_count(), 0);
}
```

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test -p fl-cli --test escalation
```

Expected: FAIL. All 15 tests fail: every `fl record escalate` / `fl finding escalate` exits 2 with `error: unrecognized subcommand 'escalate'`.

- [ ] **Step 3: Implement**

In `crates/cli/src/cmd/mod.rs`, after `pub mod check;`, add:

```rust
pub mod escalate;
```

Create `crates/cli/src/cmd/escalate.rs`:

```rust
//! `fl record escalate` and `fl finding escalate` (routing spec §3): the
//! checks that need the working tree, the warning before a repository that
//! is not private, and what a person reads back. The router makes every
//! other check and runs the three steps.

use crate::ctx::Ctx;
use crate::refs::Ref;
use crate::tiers::Tiers;
use anyhow::{Context, Result};
use fl_core::escalation::Outgoing;
use fl_core::ids::Kind;
use fl_core::routing::Tier;
use fl_core::{Iri, Prepared};

/// The refusal in a store with no routing map: it has no tiers, so nothing
/// in it is local in the sense an escalation moves from (routing spec §3.1).
fn not_routed(kind: Kind) -> anyhow::Error {
    anyhow::anyhow!(
        "`fl {} escalate` moves an item from the local tier to GitHub, and this store has no \
         local tier to escalate from: its project declares no routing map. `fl routing set \
         --project <project> <area> <tier>` declares one",
        kind.as_wire()
    )
}

pub fn run(
    ctx: &Ctx<'_>,
    kind: Kind,
    id: &Ref,
    by: Option<&str>,
    reason: Option<&str>,
    abandon: bool,
) -> Result<i32> {
    let Some(t) = ctx.tiers else {
        return Err(not_routed(kind));
    };
    let iri = ctx.resolve_item(kind, id)?;
    // The handle the person knows the item by, read before the escalation
    // turns it into a tombstone's.
    let shown = ctx.show_item(kind, &iri)?;
    if abandon {
        t.router.abandon_escalation(&iri, kind)?;
        println!("{shown}\tabandoned");
        return Ok(0);
    }
    let (Some(by), Some(reason)) = (by, reason) else {
        unreachable!("clap requires --by and --reason unless --abandon is given")
    };
    match escalate(ctx, t, kind, &iri, &shown, by, reason) {
        Ok(issue) => {
            println!("{shown}\tescalated\t{}", ctx.show_item(kind, &issue)?);
            Ok(0)
        }
        // ⚠ Marked: the item refuses every write until the escalation is
        // finished or abandoned (routing spec §3.3). A read of the mark that
        // fails leaves the error as it is.
        Err(e) if matches!(t.router.escalating(&iri), Ok(Some(_))) => {
            let what = kind.as_wire();
            Err(e.context(format!(
                "the escalation of {what} {shown} stopped after its mark was written, and the \
                 local item refuses every write until it is finished. Run `fl {what} escalate \
                 {shown} --by <who> --reason <why>` to finish it, or `fl {what} escalate \
                 {shown} --abandon` to stop it"
            )))
        }
        Err(e) => Err(e),
    }
}

/// The escalation of `iri`, checked, warned about and run: the issue.
fn escalate(
    ctx: &Ctx<'_>,
    t: &Tiers<'_>,
    kind: Kind,
    iri: &Iri,
    shown: &str,
    by: &str,
    reason: &str,
) -> Result<Iri> {
    let prepared = t.router.prepare_escalation(iri, kind)?;
    // An issue an earlier run made passed every check when it was made:
    // only finishing it is left (routing spec §3.3).
    if prepared.found().is_none() {
        check_tree(ctx, &prepared)?;
        warn_disclosure(t, &prepared, by, reason)?;
    }
    if let Some(mark) = prepared.resumes()
        && (mark.by.as_str(), mark.reason.as_str()) != (by, reason)
    {
        eprintln!(
            "note: {shown} resumes the escalation marked by {} for {:?}: the mark's who and why \
             are kept, not the ones given here",
            mark.by, mark.reason
        );
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("the system clock is before 1970")?
        .as_millis();
    let now_ms = u64::try_from(now_ms).unwrap_or(u64::MAX);
    Ok(t.router.escalate(&prepared, by, reason, now_ms)?)
}

/// The checks that need the working tree, before anything is written: the
/// routing map the manifest carries (routing spec §1.2), and a finding's
/// reproduction gate in the committed manifest — the issue names it where
/// another machine reads it (GitHub tracker spec §4.3).
fn check_tree(ctx: &Ctx<'_>, prepared: &Prepared) -> Result<()> {
    let (project, gate) = match prepared.outgoing() {
        Outgoing::Record { record, .. } => (&record.project, None),
        Outgoing::Finding { finding, .. } => (&finding.project, finding.reproduction.as_ref()),
    };
    crate::cmd::manifest::ensure_routing_current(ctx.store, project)?;
    if let Some(gate) = gate {
        crate::cmd::manifest::ensure_publishable(ctx.store, project, Some(gate))?;
    }
    Ok(())
}

/// Routing spec §3.2: on a repository that is not private, say what the
/// escalation publishes before it is written. (Anything sensitive the router
/// has refused already.) A visibility that cannot be read refuses: an
/// unknown visibility is not private.
fn warn_disclosure(t: &Tiers<'_>, prepared: &Prepared, by: &str, reason: &str) -> Result<()> {
    let gh = t.github.open()?;
    let visibility = gh.visibility()?;
    if visibility == "private" {
        return Ok(());
    }
    // A rerun publishes the mark's who and why.
    let (by, reason) = match prepared.resumes() {
        Some(mark) => (mark.by.as_str(), mark.reason.as_str()),
        None => (by, reason),
    };
    // The local IRI is published too: the issue's text names it, and it
    // becomes one of the issue's aliases.
    let iri = prepared.id();
    let what = match prepared.outgoing() {
        Outgoing::Record { record, findings } => format!(
            "this record's title, {:?}, its local IRI, {iri}, the reason, {reason:?}, who \
             escalated it, {by}, and its {} open findings",
            record.title,
            findings.len()
        ),
        Outgoing::Finding { finding, record } => {
            let about = match record.tier {
                Tier::Local => format!(
                    ", and its local record's title, {:?}, and IRI",
                    record.title
                ),
                Tier::Github => String::new(),
            };
            format!(
                "this finding's claim, {:?}, its local IRI, {iri}, the reason, {reason:?}, who \
                 escalated it, {by}{about}",
                finding.claim
            )
        }
    };
    eprintln!(
        "warning: {} is {visibility}: the escalation publishes {what}, and anyone who can read \
         the repository will see them",
        gh.repo().full_name
    );
    Ok(())
}
```

In `crates/cli/src/cmd/record.rs`, in `pub enum Cmd`, after the `Move { … }` variant, add:

```rust
    /// Move a local record to GitHub (routing spec §3): every check first,
    /// then the mark, the issue, and the tombstone. A rerun finishes an
    /// escalation that stopped.
    Escalate {
        id: Ref,
        /// Who escalates it, named on the issue.
        #[arg(long, required_unless_present = "abandon")]
        by: Option<String>,
        /// Why, named on the issue.
        #[arg(long, required_unless_present = "abandon")]
        reason: Option<String>,
        /// Remove the mark of an escalation that stopped before its issue
        /// was made.
        #[arg(long, conflicts_with_all = ["by", "reason"])]
        abandon: bool,
    },
```

in `Cmd::refs`, after `            Cmd::Move { id, .. } => vec![id],`, add:

```rust
            Cmd::Escalate { id, .. } => vec![id],
```

and in `run`, after the `Cmd::Move { id, to } => { … }` arm (the last), add:

```rust
        Cmd::Escalate {
            id,
            by,
            reason,
            abandon,
        } => {
            return crate::cmd::escalate::run(
                ctx,
                Kind::Record,
                &id,
                by.as_deref(),
                reason.as_deref(),
                abandon,
            );
        }
```

In `crates/cli/src/cmd/finding.rs`, in `pub enum Cmd`, after the `List { … }` variant (the last), add:

```rust
    /// Move a local finding to GitHub (routing spec §3): every check first,
    /// then the mark, the issue, and the tombstone. A rerun finishes an
    /// escalation that stopped.
    Escalate {
        id: Ref,
        /// Who escalates it, named on the issue.
        #[arg(long, required_unless_present = "abandon")]
        by: Option<String>,
        /// Why, named on the issue.
        #[arg(long, required_unless_present = "abandon")]
        reason: Option<String>,
        /// Remove the mark of an escalation that stopped before its issue
        /// was made.
        #[arg(long, conflicts_with_all = ["by", "reason"])]
        abandon: bool,
    },
```

in `Cmd::refs`, after `            Cmd::Withdraw { finding, .. } => vec![finding],`, add:

```rust
            Cmd::Escalate { id, .. } => vec![id],
```

and in `run`, after the `Cmd::List { … } => { … }` arm (the last), add:

```rust
        Cmd::Escalate {
            id,
            by,
            reason,
            abandon,
        } => {
            return crate::cmd::escalate::run(
                ctx,
                Kind::Finding,
                &id,
                by.as_deref(),
                reason.as_deref(),
                abandon,
            );
        }
```

In `docs/routing.md`, before `## Limits`, add:

```markdown
## Escalating an item

`fl record escalate <id> --by <who> --reason <why>` moves a local record to GitHub, and `fl
finding escalate <id> --by <who> --reason <why>` a local finding. The item becomes an issue with
its own title, state, area and aliases; the issue names who escalated it, why, and the item's
old IRI. A record's issue also lists the record's open findings in both tiers — claim, state and
IRI — leaving out security findings and findings in a sensitive area or one the map no longer
declares; the list is the state at the escalation and is not kept current. A finding about a
local record names that record's title and IRI, as any GitHub finding about a local record does.
The command prints the old handle and the issue: `1\tescalated\t#4`.

Everything that would make the GitHub create refuse is checked first, and a refusal writes
nothing: the project must be routed and this machine must bind its repository; the item must be
a local item of the kind named, not closed (a record `done`, a finding `fixed` or `withdrawn`),
and, for a record, have a title GitHub takes; no name of the item may already name something on
GitHub; the routing map must be current, as for a new item; and a finding's reproduction gate
must be in the committed manifest. Nothing sensitive reaches a repository that is not private: a
record in a sensitive area, a security finding, a finding in a sensitive area or about a local
record in one — or in an area the map no longer declares — is refused, and stays local. Anything
else escalated to a repository that is not private is published after a `warning:` that names the
repository, its visibility, and what goes out: the title or claim, the item's local IRI, the
reason, who escalated it, and a record's number of open findings.

The escalation then marks the local item "escalating", with who, why and the time. While it is
marked, the local store refuses every write to it, naming the command that finishes it; a finding
raised about a marked record is allowed. Once the item's issue exists and GitHub reads it as an fl
item, fl's lookups and writes of the item go to the issue; otherwise — no issue yet, an issue a stop
left without fl's labels, or GitHub out of reach — the local copy answers, as it last was, and
refuses writes. If the command stops after the mark — GitHub could not be reached, or it stopped
between the create and the labels — it says so: run the same command again to finish. A rerun
resumes with the mark's who and why, and a `note:` says so if it was given others; it searches for
the issue the stopped run may have made, by the old IRI, and never makes a second one. `--abandon`
(`fl record escalate <id> --abandon`) removes the mark instead, and only once the search proves no
issue exists: once the issue exists it is refused, naming the issue, and only finishing is left.
`--abandon` takes neither `--by` nor `--reason`, and prints `1\tabandoned`.

Last, the local item is replaced by a tombstone: the old IRI, the issue, who, when and why. The
old handle and the old IRI then name the issue — a lookup, a move, `fl finding list --record` —
and the local row is left out of lists.
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p fl-cli --test escalation
```

Expected: PASS (15 tests).

- [ ] **Step 5: Mutation checks**

Each: make the change, run the named test (`cargo test -p fl-cli --test escalation -- <name>`), watch it go red, restore, `cmp` against the saved copy.

1. The unrouted refusal: `return Ok(0);` for `return Err(not_routed(kind));` → `an_unrouted_store_is_refused_naming_fl_routing_set` red.
2. The routing check: delete `crate::cmd::manifest::ensure_routing_current(ctx.store, project)?;` in `check_tree` → `a_routing_map_the_manifest_does_not_carry_is_refused_before_the_mark` red.
3. The gate check: delete `crate::cmd::manifest::ensure_publishable(ctx.store, project, Some(gate))?;` → `a_finding_whose_gate_is_not_in_the_committed_manifest_is_refused_before_the_mark` red.
4. Only a gate is checked: replace the `if let Some(gate) = gate { … }` block with `crate::cmd::manifest::ensure_publishable(ctx.store, project, gate)?;` → `a_record_escalates_to_an_issue_with_its_state_area_findings_and_provenance` red (`there is no manifest at …`).
5. Both checks skipped when found: `if true {` for `if prepared.found().is_none() {` → `a_stop_between_the_create_and_its_labels_is_finished_by_a_rerun_with_one_issue` red (the rerun is refused by the changed routing map).
6. No warning on a private repository: `if false {` for `if visibility == "private" {` → `a_record_escalates_to_an_issue_with_its_state_area_findings_and_provenance` red.
7. A warning otherwise: `if true {` for `if visibility == "private" {` → `an_escalation_to_a_public_repository_warns_what_it_publishes` red.
8. An unreadable visibility refuses: `let visibility = gh.visibility().unwrap_or_else(|_| "private".into());` → `an_unreadable_visibility_refuses_before_the_mark` red.
9. A rerun's warning names the mark's who and why: `Some(_) => (by, reason),` for `Some(mark) => (mark.by.as_str(), mark.reason.as_str()),` in `warn_disclosure` → `a_rerun_with_another_reason_keeps_the_marks_and_says_so` red.
10. A finding about a local record names that record: make the `Tier::Local` arm in `warn_disclosure` answer `String::new()` → `an_escalation_to_a_public_repository_warns_what_it_publishes` red.
11. The note when they differ: `==` for `!=` in `(mark.by.as_str(), mark.reason.as_str()) != (by, reason)` → `a_rerun_with_another_reason_keeps_the_marks_and_says_so` red.
12. No note when they match: `&& true` for that comparison → `a_stop_between_the_create_and_its_labels_is_finished_by_a_rerun_with_one_issue` red.
13. The reason too, not only who: `&& mark.by.as_str() != by` → `a_rerun_with_another_reason_keeps_the_marks_and_says_so` red.
14. Context only on a marked item: `Err(e) if true => {` → `an_escalation_to_a_public_repository_warns_what_it_publishes` red (the sensitivity refusal, before the mark, names the command to finish).
15. Context on a marked item: `Err(e) if false => {` → `a_stop_between_the_create_and_its_labels_is_finished_by_a_rerun_with_one_issue` red.
16. The abandon branch: `if false {` for `if abandon {` → `an_abandon_before_any_issue_clears_the_mark` red (the `unreachable!` panics).
17. The kind each command passes: `Kind::Finding` in `record.rs`'s arm → `a_record_escalates_to_an_issue_with_its_state_area_findings_and_provenance` red; `Kind::Record` in `finding.rs`'s arm → `a_finding_about_a_local_record_escalates_and_its_issue_names_the_record` red.
18. The local IRI in the warning: drop `its local IRI, {iri}, ` from the record's text → `an_escalation_to_a_public_repository_warns_what_it_publishes` red; from the finding's → the same test red.
19. The router's sensitivity filter, end to end: `.filter(|f| !f.security)` for `.filter(|f| !f.security && !sensitive(map, f.area.as_deref()))` in `open_findings` (`crates/core/src/escalate.rs`) → `a_records_issue_never_lists_a_finding_in_a_sensitive_area` red (the claim is in the issue's text).
20. The closed state, end to end: `(r.state == State::Done && false)` in `Local::closed` (`crates/core/src/escalate.rs`) → `a_done_record_is_refused_before_the_mark` red (the record is marked and escalated).
21. The router's fall-back for an issue fl cannot read — in `crates/core/src/tiered.rs` `issue_of_marked`, `Ok(Some(issue))` for `Ok(readable.then_some(issue))` (or `github.get_record(&RecordId(issue.clone()))?.is_some()` for the record arm of `readable`) → `a_stop_between_the_create_and_its_labels_is_finished_by_a_rerun_with_one_issue` red (the move is refused as `is a GitHub issue with no fl label`, not by the mark).
22. Its fall-back for a search that fails: `let Some(issue) = self.github.find_escalated(&primary, mark.at_ms)? else { return Ok(None); };` → the same test red (`GitHub answered 502 to a GraphQL query` for the second finding).
23. Its fall-back for a GitHub that cannot be opened: `let github = self.github.tracker()?;` → the same test red (the offline raise fails: `could not be opened`).

Not observable: reading the old handle before the escalation rather than after — the local store keeps a tombstoned item's handle (plan ruling 5), so both read `1`. The clock — the mark's time is never printed, and the fake's search with any time reaches the issue a stop left.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1171 passed, 18 ignored.

- [ ] **Step 7: Commit**

```bash
git add crates/cli/src/cmd/escalate.rs crates/cli/src/cmd/mod.rs crates/cli/src/cmd/record.rs crates/cli/src/cmd/finding.rs crates/cli/tests/escalation.rs docs/routing.md
git commit -m "feat(cli): fl record escalate and fl finding escalate

Both take --by and --reason, or --abandon alone. An unrouted store is
refused naming fl routing set. The router's prepare_escalation makes
its checks first; then, unless an earlier run's issue was found, the CLI
checks that the manifest carries the routing map and, for a finding with
a reproduction gate, that the gate is in the committed manifest, and on
a repository that is not private warns what the escalation publishes:
the title or claim, the item's local IRI, the reason, who escalated it,
and a record's number of open findings (a visibility that cannot be read
refuses). A rerun
given another who or why says the mark's are kept. Stdout reads
<old handle>\tescalated\t#<n>, or <handle>\tabandoned; a failure that
leaves the item marked names the command that finishes it, or
--abandon. While a stop between the create and its labels leaves an
issue fl cannot read, the record's move is refused by its mark and a
finding about it is allowed, online, when the search fails, and offline.
docs/routing.md gains a section on escalation. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

---

### Task 9: A landed move to `needs_human` escalates the record

In a routed project, `fl record move <id> --to needs_human` on a local record runs the transition as today, and "**If the move landed**, it then runs the escalation; a refused move escalates nothing. If the escalation fails, the record stays local, in `needs_human`, marked "escalating" if step 1 ran, and the command exits with the move's own code and a `warning:` naming `fl record escalate <id>`" (routing spec §3.4, decision 16). `fl record move` takes no actor, so the escalation names `fl` and the reason `the record was moved to needs_human` (plan ruling 15). It is the same path as `fl record escalate` — the router's checks, the working-tree checks, the disclosure warning, the three steps and the `<shown>\tescalated\t#<n>` line — so `run`'s output moves into a private `escalate_and_say` both callers share. "The ledger entry written" is the move's own evidence, its gate runs (plan ruling 16): a gated move that lands has them in the local ledger before the escalation starts; a refused move escalates nothing. A marked record refuses every write (§3.3 step 1), and `move_record` would refuse it only after its gates had run and been appended, as an untyped error; so the CLI reads the mark first and refuses with the store's own `StoreError::Escalating`, before the import check and the gates (plan ruling 17, Review Focus 2). `fl finding reproduce` and `fl finding verify` run a gate before their write (`attach_reproduction` and `verify_finding` reach `update_finding` after it), so a marked finding is refused the same way, through one helper, `refuse_marked`, that all three commands call. Only a local record in a routed store escalates: a GitHub record is where the escalation would put it, and an unrouted store has no local tier (plan ruling 21).

**Blast radius:** `fl record move` changes in a routed store only — a marked record is refused before its import check and gates (the refusal it got before came after the gates, from `set_record_state`); a local record whose move lands in `needs_human` is escalated after the move's line. `fl finding reproduce` and `fl finding verify` change in a routed store only, for a marked finding: refused before the import check and the gate, where before they were refused after the gate ran. In an unrouted store, and for a move to any other state, nothing changes. `fl record escalate`'s output is unchanged; it now prints through `escalate_and_say`. The test file's `world` writes its config through a new `R::configure`, and `R::fl` calls a new `R::fl_at`; both produce what they did before.

**Files:**
- Modify: `crates/cli/src/cmd/escalate.rs` (`refuse_marked`, `after_landed_move`; private `escalate_and_say`, which `run` now calls)
- Modify: `crates/cli/src/cmd/record.rs` (`Move`: the marked refusal first, `after_landed` on both landed paths; private `after_landed`)
- Modify: `crates/cli/src/cmd/finding.rs` (`Reproduce`, `Verify`: the marked refusal first)
- Modify: `crates/cli/tests/escalation.rs` (helpers `configure`, `fl_at`, `gated`, `runs`, `only_warning`; ten tests)
- Modify: `docs/routing.md` ("Escalating an item": the `needs_human` trigger)

**Interfaces:**
- Consumes: `fl_core::escalation::escalate_command` (Task 1); `StoreError::Escalating` (Task 1) and its wording `so this store refuses to change it`; `TieredTracker::escalating` (Task 6), which answers `None` for an id GitHub claims; Task 8's private `escalate` (prepare → working-tree checks → disclosure warning → escalate) and `run`; Task 3's escalation line (`Escalated from the local tier by`), as the issue's Markdown escapes it (`needs\_human`); `MoveOutcome::{Ungated, Moved, Refused}` and `move_record` (existing); `FakeGithub`'s `rate_limited_next_create`, `requests`, `repos[0].visibility` (existing).
- Produces: `pub fn after_landed_move(ctx: &Ctx<'_>, record: &Record, shown: &str)` in `crates/cli/src/cmd/escalate.rs`, as the shared block names it; and, beyond it, `pub fn refuse_marked(ctx: &Ctx<'_>, kind: Kind, id: &Iri) -> Result<()>` there — `Ok` in an unrouted store or for an unmarked item, else `StoreError::Escalating { id, to_finish: escalate_command(kind, id) }`. It returns nothing, so no caller can turn a failed escalation into the move's exit. It reads `record.id` only; the router reads the record again.
- Produces, beyond the shared block: private `fn escalate_and_say(ctx, t, kind, iri, shown, by, reason) -> Result<()>` in `escalate.rs` (the escalation and its stdout line); private `fn after_landed(ctx, record, to: State, shown)` in `record.rs`, which holds the `needs_human` condition once for both landed paths. The marked refusal is `StoreError::Escalating { id: <primary IRI>, to_finish: escalate_command(<kind>, <primary IRI>) }` — the store's own text — for a record's move and a finding's reproduction or verification alike.
- Unique phrases: `--by <name> --reason <text>` (the warning's command; `fl record escalate`'s own context says `--by <who> --reason <why>`). The tests also assert Task 1's `so this store refuses to change it`, Task 3's escalation line, Task 7's `this record is security-sensitive`, `ensure_import_current`'s `changed since this store imported it` (as absent), and Task 8's `stopped after its mark was written` (as absent) and `the mark's who and why are kept`.

- [ ] **Step 1: Write the failing tests**

In `crates/cli/tests/escalation.rs`, after `const KEPT: &str = "the mark's who and why are kept";`, add:

```rust
/// What the warning after a landed move to `needs_human` tells a person to
/// run (routing spec §3.4).
const TO_FINISH: &str = "`fl record escalate 1 --by <name> --reason <text>`";

/// The escalation line of a record the move to `needs_human` escalated, as
/// the issue's Markdown escapes it.
const BY_FL: &str = "Escalated from the local tier by fl: the record was moved to needs\\_human.";

/// The ungated move of record 1 from `todo` to `needs_human`, as printed.
const UNGATED: &str =
    "1\tneeds_human\tungated: project 1 declares no transition from `todo` to `needs_human`\n";
```

In `fn world`, replace the three statements from `    let cfg = format!(` to `    fs::write(r.home.path().join("config/fl/config.toml"), cfg).unwrap();` with:

```rust
    r.configure(r.home.path(), tracker);
```

In `impl R`, replace `fn fl` (its whole body) with:

```rust
    /// The config entry of the machine whose home is `home`: the working
    /// tree's project, a store of its own, and `tracker`.
    fn configure(&self, home: &Path, tracker: &str) {
        let cfg = format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n{tracker}",
            self.repo.path().canonicalize().unwrap().display(),
            home.join("fl.redb").display()
        );
        fs::create_dir_all(home.join("config/fl")).unwrap();
        fs::write(home.join("config/fl/config.toml"), cfg).unwrap();
    }

    fn fl(&self) -> Command {
        self.fl_at(self.home.path())
    }

    /// `fl` on the machine whose home is `home`.
    fn fl_at(&self, home: &Path) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("FL_GITHUB_TOKEN", "t")
            .env("FL_GITHUB_API_URL", self.fake.url())
            .env_remove("GITHUB_TOKEN")
            .env_remove("FL_DB")
            .current_dir(self.repo.path());
        c
    }
```

In `impl R`, after `fn block`, add:

```rust
    /// A gate running `program` over `src/**/*.rs`, and the transition from
    /// `from` to `to` it gates.
    fn gated(&self, from: &str, to: &str, program: &str) {
        self.ok(&[
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "g",
            "--glob",
            "src/**/*.rs",
            "--program",
            program,
        ]);
        self.ok(&[
            "transition",
            "add",
            "--project",
            "1",
            "--name",
            "t",
            "--from",
            from,
            "--to",
            to,
            "--regret",
            "low",
            "--gate",
            "1",
        ]);
    }

    /// How many gate runs the store holds, over every gate: a routed
    /// store's ledger is local (routing spec §3.5).
    fn runs(&self) -> usize {
        use fl_core::store::{Catalog, Ledger};
        let store = fl_store::RedbStore::open(&self.home.path().join("fl.redb")).unwrap();
        store
            .list_projects()
            .unwrap()
            .iter()
            .flat_map(|p| store.list_gates(&p.id).unwrap())
            .map(|g| store.gate_runs(&g.id).unwrap().len())
            .sum()
    }
```

At the end of the file, add:

```rust
/// The one `warning:` line in `err`, which must hold exactly one.
fn only_warning(err: &str) -> &str {
    let warned: Vec<&str> = err.lines().filter(|l| l.starts_with("warning: ")).collect();
    assert_eq!(warned.len(), 1, "{err}");
    warned[0]
}

// Routing spec §3.4: a local record whose move to `needs_human` lands is
// escalated, by `fl`, for the move — after the move's own line.
#[test]
fn an_ungated_move_of_a_local_record_to_needs_human_escalates_it() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    let (out, err) = g.ok_said(&["record", "move", "1", "--to", "needs_human"]);
    assert_eq!(out, format!("{UNGATED}1\tescalated\t#1\n"));
    assert!(!err.contains("warning:"), "{err}");
    let issue = g.fake.issue(1);
    assert_eq!(
        issue.labels,
        vec!["fl:record", "fl:record/needs_human", "fl:area/code"]
    );
    assert!(issue.body.contains(BY_FL), "{}", issue.body);
    let from = g.block(1).escalated.unwrap();
    assert_eq!(
        (from.by.as_str(), from.reason.as_str()),
        ("fl", "the record was moved to needs_human")
    );
    assert_eq!(
        g.ok(&["record", "list", "--project", "1"]),
        "#1\tgithub\tneeds_human\tfix the parser\n"
    );
}

// Routing spec §3.4, §3.5: a gated move that lands keeps its evidence — its
// gate run, in the local ledger — and then escalates the record.
#[test]
fn a_gated_move_to_needs_human_that_lands_escalates_it_and_keeps_its_gate_run() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.gated("todo", "needs_human", "true");
    let out = g.ok(&["record", "move", "1", "--to", "needs_human"]);
    assert!(
        out.starts_with("PASS\tt\tg\t") && out.ends_with("\n1\tneeds_human\n1\tescalated\t#1\n"),
        "{out}"
    );
    assert_eq!(g.runs(), 1);
    assert_eq!(g.fake.issue_count(), 1);
    assert!(g.fake.issue(1).body.contains(BY_FL));
    assert!(
        g.fake
            .issue(1)
            .labels
            .contains(&"fl:record/needs_human".to_string())
    );
}

// Routing spec §3.4: only a move that lands escalates. A refused move exits
// with its gate's code, keeps its evidence, and leaves the record local,
// unmarked and unescalated.
#[test]
fn a_refused_move_to_needs_human_escalates_nothing() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.gated("todo", "needs_human", "false");
    let out = g
        .fl()
        .args(["record", "move", "1", "--to", "needs_human"])
        .output()
        .unwrap();
    let said = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(
        said.contains("REFUSED\t1\tstays `todo`") && !said.contains("escalated"),
        "{said}"
    );
    assert_eq!(g.runs(), 1);
    assert_eq!(g.fake.issue_count(), 0);
    // Not marked: the record still moves.
    g.ok(&["record", "move", "1", "--to", "doing"]);
    assert_eq!(
        g.ok(&["record", "list", "--project", "1"]),
        "1\tlocal\tdoing\tfix the parser\n"
    );
}

// Routing spec §3.4, decision 16: an escalation that stops after its mark
// leaves the move standing — exit 0, the record local in `needs_human` and
// marked — and a warning names the command that finishes it, which then
// resumes with the mark's who and why.
#[test]
fn a_landed_move_whose_escalation_fails_warns_and_keeps_the_moves_code() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.fake.state().rate_limited_next_create = true;
    let (out, err) = g.ok_said(&["record", "move", "1", "--to", "needs_human"]);
    assert_eq!(out, UNGATED);
    let warned = only_warning(&err);
    assert!(
        warned.contains(TO_FINISH) && warned.contains("rate limit") && !err.contains(STOPPED),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    assert_eq!(
        g.ok(&["record", "list", "--project", "1"]),
        "1\tlocal\tneeds_human\tfix the parser\n"
    );
    // Marked: a further move is refused, naming the command.
    let refused = g.refused(&["record", "move", "1", "--to", "doing"]);
    assert!(
        refused.contains("so this store refuses to change it")
            && refused.contains("fl record escalate"),
        "{refused}"
    );
    let (out, err) = g.ok_said(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert_eq!(out, "1\tescalated\t#1\n");
    assert!(err.contains(KEPT), "{err}");
    assert!(g.fake.issue(1).body.contains(BY_FL));
}

// Routing spec §3.2, §3.4: an escalation refused before its mark — a
// sensitive record bound for a public repository — leaves the move standing
// and the record unmarked, and the warning names why.
#[test]
fn a_landed_move_whose_escalation_is_refused_before_the_mark_leaves_it_unmarked() {
    let g = world(BOUND);
    g.routed();
    g.fake.state().repos[0].visibility = "public".into();
    g.local_record("rotate the keys", "security");
    let (out, err) = g.ok_said(&["record", "move", "1", "--to", "needs_human"]);
    assert_eq!(out, UNGATED);
    let warned = only_warning(&err);
    assert!(
        warned.contains("this record is security-sensitive") && warned.contains(TO_FINISH),
        "{err}"
    );
    assert_eq!(g.fake.issue_count(), 0);
    g.ok(&["record", "move", "1", "--to", "doing"]);
}

// Routing spec §3.3 step 1: a marked record's move is refused before any
// gate runs, so no evidence is written for a move that cannot land.
#[test]
fn a_move_of_a_marked_record_is_refused_before_its_gates_run() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.gated("todo", "doing", "true");
    g.fake.state().rate_limited_next_create = true;
    g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    let out = g
        .fl()
        .args(["record", "move", "1", "--to", "doing"])
        .output()
        .unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains("so this store refuses to change it") && err.contains("fl record escalate"),
        "{err}"
    );
    assert!(out.stdout.is_empty(), "no gate line");
    assert_eq!(g.runs(), 0, "no gate ran");
}

// Routing spec §3.3 step 1: a marked finding's reproduction and its
// verification are refused before their gate runs, as a marked record's
// move is: no evidence is written for a write that cannot land.
#[test]
fn a_reproduction_or_verification_of_a_marked_finding_is_refused_before_its_gate_runs() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    for claim in ["it drops a token", "it is slow"] {
        g.ok(&[
            "finding", "raise", "--record", "1", "--claim", claim, "--by", "rev",
        ]);
    }
    g.ok(&[
        "gate",
        "add",
        "--project",
        "1",
        "--name",
        "g",
        "--glob",
        "src/**/*.rs",
        "--program",
        "false",
    ]);
    // Finding 2 is reproduced and assigned, so it can be verified; its
    // escalation needs its gate in the committed manifest.
    g.ok(&["finding", "reproduce", "2", "--gate", "1"]);
    g.ok(&["finding", "assign", "2", "--to", "bob"]);
    g.ok(&["manifest", "export", "--project", "1"]);
    git(g.repo.path(), &["add", "-A"]);
    git(g.repo.path(), &["commit", "-qm", "manifest"]);
    for n in ["1", "2"] {
        g.fake.state().rate_limited_next_create = true;
        let err = g.refused(&["finding", "escalate", n, "--by", "alice", "--reason", "r"]);
        assert!(err.contains(STOPPED), "{err}");
    }
    let runs = g.runs();
    for args in [
        &["finding", "reproduce", "1", "--gate", "1"][..],
        &["finding", "verify", "2"][..],
    ] {
        let out = g.fl().args(args).output().unwrap();
        let err = String::from_utf8(out.stderr).unwrap();
        assert_eq!(g.runs(), runs, "{args:?}: a gate ran: {err}");
        assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
        assert!(
            err.contains("so this store refuses to change it")
                && err.contains("fl finding escalate"),
            "{args:?}: {err}"
        );
    }
}

// Routing spec §3.3 step 1: on a machine that imported the manifest, a
// marked record's gated move is refused for the mark, before the import is
// checked — the import is not what stops it.
#[test]
fn a_marked_records_move_is_refused_before_the_import_check() {
    let g = world(BOUND);
    g.routed();
    g.gated("todo", "doing", "true");
    g.ok(&["manifest", "export", "--project", "1"]);
    let other = tempfile::tempdir().unwrap();
    g.configure(other.path(), BOUND);
    let at = |args: &[&str]| g.fl_at(other.path()).args(args).output().unwrap();
    assert!(at(&["manifest", "import"]).status.success());
    assert!(
        at(&[
            "record",
            "add",
            "--project",
            "1",
            "--title",
            "t",
            "--area",
            "code",
        ])
        .status
        .success()
    );
    g.fake.state().rate_limited_next_create = true;
    assert_eq!(
        at(&["record", "escalate", "1", "--by", "alice", "--reason", "r"])
            .status
            .code(),
        Some(2)
    );
    // The import goes stale.
    g.ok(&["routing", "set", "--project", "1", "product", "local"]);
    g.ok(&["manifest", "export", "--project", "1"]);
    let out = at(&["record", "move", "1", "--to", "doing"]);
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains("so this store refuses to change it")
            && !err.contains("changed since this store imported it"),
        "{err}"
    );
}

// Routing spec §3.4: only a local record escalates. A GitHub record moved to
// `needs_human` is moved, and no second issue is made.
#[test]
fn a_github_record_moved_to_needs_human_is_not_escalated() {
    let g = world(BOUND);
    g.routed();
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "look",
        "--area",
        "design",
    ]);
    let (out, err) = g.ok_said(&["record", "move", "#1", "--to", "needs_human"]);
    assert!(
        out.starts_with("#1\tneeds_human\t") && !out.contains("escalated"),
        "{out}"
    );
    assert!(!err.contains("warning:"), "{err}");
    assert_eq!(g.fake.issue_count(), 1);
}

// Routing spec §3.1: a store with no routing map has no local tier, so its
// move to `needs_human` is as it was — no escalation, nothing asked of
// GitHub.
#[test]
fn an_unrouted_stores_move_to_needs_human_is_unchanged() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.ok(&["record", "add", "--project", "1", "--title", "t"]);
    let (out, err) = g.ok_said(&["record", "move", "1", "--to", "needs_human"]);
    assert_eq!(out, UNGATED);
    assert!(err.is_empty(), "{err}");
    assert!(g.fake.state().requests.is_empty());
}
```

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test -p fl-cli --test escalation
```

Expected: FAIL. Seven of the ten new tests fail; the fifteen earlier ones, and three new ones that hold already (`a_refused_move_to_needs_human_escalates_nothing`, `a_github_record_moved_to_needs_human_is_not_escalated`, `an_unrouted_stores_move_to_needs_human_is_unchanged`), pass:
- `an_ungated_move_of_a_local_record_to_needs_human_escalates_it`: ``left: "1\tneeds_human\tungated: … `needs_human`\n"``, `right: "…\n1\tescalated\t#1\n"` — no escalation.
- `a_gated_move_to_needs_human_that_lands_escalates_it_and_keeps_its_gate_run`: the stdout ends `1\tneeds_human\n`.
- `a_landed_move_whose_escalation_fails_warns_and_keeps_the_moves_code` and `a_landed_move_whose_escalation_is_refused_before_the_mark_leaves_it_unmarked`: no `warning:` line (`left: 0`, `right: 1`).
- `a_move_of_a_marked_record_is_refused_before_its_gates_run`: `left: 1`, `right: 0` — the gate ran before `set_record_state` refused.
- `a_marked_records_move_is_refused_before_the_import_check`: the refusal is `the manifest at … changed since this store imported it`.
- `a_reproduction_or_verification_of_a_marked_finding_is_refused_before_its_gate_runs`: `["finding", "reproduce", "1", "--gate", "1"]: a gate ran` — the gate ran before `update_finding` refused.

- [ ] **Step 3: Implement**

In `crates/cli/src/cmd/escalate.rs`, replace the imports from `use fl_core::escalation::Outgoing;` to `use fl_core::routing::Tier;` so the block reads:

```rust
use fl_core::escalation::{Outgoing, escalate_command};
use fl_core::ids::Kind;
use fl_core::model::{Record, State};
use fl_core::routing::Tier;
use fl_core::store::StoreError;
```

In `run`, replace

```rust
    match escalate(ctx, t, kind, &iri, &shown, by, reason) {
        Ok(issue) => {
            println!("{shown}\tescalated\t{}", ctx.show_item(kind, &issue)?);
            Ok(0)
        }
```

with

```rust
    match escalate_and_say(ctx, t, kind, &iri, &shown, by, reason) {
        Ok(()) => Ok(0),
```

(the two `Err` arms stay as they are). Before `fn escalate` and its doc comment (``/// The escalation of `iri`, checked, warned about and run: the issue.``), add:

```rust
/// ⚠ Routing spec §3.3 step 1: a marked item refuses every write. A
/// command whose gates run before its write — `fl record move`, `fl finding
/// reproduce`, `fl finding verify` — calls this first, before the import
/// check and the gates, and is refused as the store would refuse it: nothing
/// runs and no evidence is written for a write that cannot land. `id` is
/// the item's primary IRI, as the router read it.
pub fn refuse_marked(ctx: &Ctx<'_>, kind: Kind, id: &Iri) -> Result<()> {
    if let Some(t) = ctx.tiers
        && t.router.escalating(id)?.is_some()
    {
        return Err(StoreError::Escalating {
            id: id.clone(),
            to_finish: escalate_command(kind, id),
        }
        .into());
    }
    Ok(())
}

/// Routing spec §3.4: a local record whose move to `needs_human` landed is
/// escalated, by `fl`, as `fl record escalate` would. The move stands
/// whatever happens here: a failure is a warning naming the command that
/// finishes the escalation, never an error, so the move's exit code is the
/// command's.
pub fn after_landed_move(ctx: &Ctx<'_>, record: &Record, shown: &str) {
    let Some(t) = ctx.tiers else {
        return;
    };
    let iri = record.id.iri();
    if t.router.tier_of(iri) != Tier::Local {
        return;
    }
    let reason = format!("the record was moved to {}", State::NeedsHuman.as_wire());
    if let Err(e) = escalate_and_say(ctx, t, Kind::Record, iri, shown, "fl", &reason) {
        eprintln!(
            "warning: the move stands, but the escalation of record {shown} to GitHub stopped \
             (run `fl record escalate {shown} --by <name> --reason <text>` to finish it): {e:#}"
        );
    }
}

/// The escalation of `iri`, run, and its line for a person:
/// `<shown>\tescalated\t<issue>`.
fn escalate_and_say(
    ctx: &Ctx<'_>,
    t: &Tiers<'_>,
    kind: Kind,
    iri: &Iri,
    shown: &str,
    by: &str,
    reason: &str,
) -> Result<()> {
    let issue = escalate(ctx, t, kind, iri, shown, by, reason)?;
    println!("{shown}\tescalated\t{}", ctx.show_item(kind, &issue)?);
    Ok(())
}
```

In `crates/cli/src/cmd/record.rs`, replace the import `use fl_core::model::State;` with:

```rust
use fl_core::model::{Record, State};
```

In `run`'s `Cmd::Move` arm, after the `let Some(record) = ctx.tracker.get_record(&r)? else { … };` statement and before `let gated = store`, add:

```rust
            // Routing spec §3.3 step 1: a marked record's move is refused
            // before the import check and the gates.
            crate::cmd::escalate::refuse_marked(ctx, Kind::Record, record.id.iri())?;

```

In the `if let MoveOutcome::Ungated = report.outcome { … }` block, between its `println!(…);` and `return Ok(0);`, add:

```rust
                after_landed(ctx, &record, state, &shown);
```

In the `match report.outcome` arm `MoveOutcome::Moved => { … }`, after `println!("{shown}\t{}", state.as_wire());`, add:

```rust
                    after_landed(ctx, &record, state, &shown);
```

(The `MoveOutcome::Refused { code }` arm is unchanged: a refused move escalates nothing.) At the end of the file, after `pub fn run`, add:

```rust
/// Routing spec §3.4: a move that landed in `needs_human` escalates a local
/// record. It returns nothing: the move's own code is the command's.
fn after_landed(ctx: &Ctx<'_>, record: &Record, to: State, shown: &str) {
    if to == State::NeedsHuman {
        crate::cmd::escalate::after_landed_move(ctx, record, shown);
    }
}
```

In `crates/cli/src/cmd/finding.rs`, in `run`'s `Cmd::Reproduce { finding, gate }` arm, after `let f = self::finding(ctx, &finding)?;` and before `crate::cmd::manifest::ensure_import_current(store, &f.project)?;`, add:

```rust
            // Routing spec §3.3 step 1: a marked finding's reproduction is
            // refused before the import check and the gate.
            crate::cmd::escalate::refuse_marked(ctx, Kind::Finding, f.id.iri())?;
```

and in the `Cmd::Verify { finding }` arm, at the same place, add:

```rust
            // Routing spec §3.3 step 1: a marked finding's verification is
            // refused before the import check and the gate.
            crate::cmd::escalate::refuse_marked(ctx, Kind::Finding, f.id.iri())?;
```

In `docs/routing.md`, in "## Escalating an item", replace the last line of the last paragraph, `and the local row is left out of lists.`, with:

```markdown
and the local row is left out of lists. A local record moved to `needs_human` is escalated the
same way once the move lands, by `fl`; if that escalation fails, the move stands and a
`warning:` names the command that finishes it.
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p fl-cli --test escalation
```

Expected: PASS (25 tests).

- [ ] **Step 5: Mutation checks**

Each: make the change, run the named test (`cargo test -p fl-cli --test escalation -- <name>`), watch it go red, restore, `cmp` against the saved copy.

1. The ungated path's call: delete `after_landed(ctx, &record, state, &shown);` before `return Ok(0);` → `an_ungated_move_of_a_local_record_to_needs_human_escalates_it` red.
2. The gated path's call: delete `after_landed(ctx, &record, state, &shown);` in the `MoveOutcome::Moved` arm → `a_gated_move_to_needs_human_that_lands_escalates_it_and_keeps_its_gate_run` red.
3. Only `needs_human`: `if true {` for `if to == State::NeedsHuman {` → `a_refused_move_to_needs_human_escalates_nothing` red (its later move to `doing` escalates the record).
4. A refused move escalates nothing: add `after_landed(ctx, &record, state, &shown);` after the `REFUSED` line in the `MoveOutcome::Refused { code }` arm → `a_refused_move_to_needs_human_escalates_nothing` red.
5. Only a local record: `if false {` for `if t.router.tier_of(iri) != Tier::Local {` → `a_github_record_moved_to_needs_human_is_not_escalated` red (the router refuses the GitHub record as not local, and the warning prints).
6. The marked refusal: delete `crate::cmd::escalate::refuse_marked(ctx, Kind::Record, record.id.iri())?;` in the `Move` arm → `a_move_of_a_marked_record_is_refused_before_its_gates_run` red (`left: 1`, `right: 0`: the gate ran).
7. Its position before the import check: move that call from before `let gated = store` to after the `if gated { ensure_import_current(…)?; }` block (still before the pre-flight and the gates) → `a_marked_records_move_is_refused_before_the_import_check` red (`changed since this store imported it`). `a_move_of_a_marked_record_is_refused_before_its_gates_run` stays green under this one — it is the gates' position, which check 6 covers.
8. The mark read: `.is_none()` for `.is_some()` in `refuse_marked` → `a_move_of_a_marked_record_is_refused_before_its_gates_run` red.
9. A warning, not an error — the move's code kept: add `std::process::exit(2);` after the `eprintln!` in `after_landed_move` → `a_landed_move_whose_escalation_fails_warns_and_keeps_the_moves_code` red.
10. The warning: replace the `if let Err(e) = escalate_and_say(…) { eprintln!(…); }` with `let _ = escalate_and_say(ctx, t, Kind::Record, iri, shown, "fl", &reason);` → `a_landed_move_whose_escalation_is_refused_before_the_mark_leaves_it_unmarked` red.
11. Who: `"alice"` for `"fl"` in that call → `an_ungated_move_of_a_local_record_to_needs_human_escalates_it` red.
12. Why: `"the record moved to {}"` for `"the record was moved to {}"` → `an_ungated_move_of_a_local_record_to_needs_human_escalates_it` red.
13. A marked finding's reproduction: delete the `refuse_marked(ctx, Kind::Finding, f.id.iri())?;` call in the `Reproduce` arm → `a_reproduction_or_verification_of_a_marked_finding_is_refused_before_its_gate_runs` red (`a gate ran`). Its verification: the same call in the `Verify` arm → the same test red (`["finding", "verify", "2"]: a gate ran`). Its kind: `Kind::Record` in the `Reproduce` arm's call → the same test red (the refusal names `fl record escalate`).

Not observable: the routed condition, in both places — `let Some(t) = ctx.tiers else { return; }` in `after_landed_move` and `if let Some(t) = ctx.tiers` in `refuse_marked`. Without the tiers there is no router to read a mark from or escalate through, so no mutation that compiles drops either; `an_unrouted_stores_move_to_needs_human_is_unchanged` pins the unrouted behaviour (the move's line only, no stderr, no request to the fake). The finding calls' place before `ensure_import_current` is not pinned by a test of its own: no test imports a manifest for a marked finding; each call stands where `record move`'s does, whose order check 7 pins.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1181 passed, 18 ignored.

- [ ] **Step 7: Commit**

```bash
git add crates/cli/src/cmd/escalate.rs crates/cli/src/cmd/record.rs crates/cli/src/cmd/finding.rs crates/cli/tests/escalation.rs docs/routing.md
git commit -m "feat(cli): a landed move to needs_human escalates the record

In a routed store, a local record whose move to needs_human lands -
ungated, or with its gates passed - is escalated by fl, for the reason
\"the record was moved to needs_human\", through the same checks, warning
and output as fl record escalate. The move stands whatever happens: a
failed escalation is one warning naming the failure and
\`fl record escalate <id> --by <name> --reason <text>\`, and the command
exits with the move's own code. A refused move, a move to any other
state, a GitHub record and an unrouted store escalate nothing.

A move of a marked record, and a reproduction or a verification of a
marked finding, is refused naming the escalate command before the import
check and the gates, so no gate runs and no evidence is written for a
write that cannot land. docs/routing.md says what the trigger does.
Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

---

### Task 10: Lists, handles and evidence after an escalation

Once an item is escalated, every way of naming it reaches the issue, and every list shows where it is. "An item marked "escalating" is listed with that mark. Tombstones are not listed." (routing spec §2.4): in a routed store's lists the tier column of a marked local item reads `escalating` (plan ruling 18), read with `TieredTracker::escalating` per local row through one helper both lists call; a tombstoned item is already left out by the local store (Task 1, Task 2), and its issue is listed as GitHub's under `#n`; a marked item whose issue already exists is listed once, as the issue, in a list of both tiers (Task 6, plan ruling 23), and `--tier local` still shows it as `escalating`. "A local handle of an escalated item resolves through its tombstone" (§2.3): the router's `route` already follows it (Task 6, plan ruling 8), so the old bare handle and the old IRI move the issue, a finding raised about either is about the issue (`add_finding_at` takes the record the router read), and `fl attempt` records its attempt against the issue — this task pins all three end to end. `fl finding list --record <old>` lists the record's findings from both tiers: the local ones, whose stored record is the old IRI, shown as the issue (plan ruling 9, §3.5), and one raised on GitHub about the issue. "The router resolves that IRI through any tombstone first, so new evidence names the record where it now lives" (§2.5 "Evidence"): an `fl-exec` test reproduces a local finding after its record's escalation, through the router, and its gate run names the issue (Review Focus 3). Unrouted output is unchanged.

**Blast radius:** `fl record list` and `fl finding list` change in a routed store only, and only for a local item marked escalating, whose tier column now reads `escalating` (a script that reads the column sees a third value — plan ruling 18). Each local row makes one local `mark_of` read; a GitHub row makes none, and nothing new asks GitHub. The unrouted arms are not touched (`an_unrouted_list_has_no_tier_column_and_refuses_tier` and `finding_list_record_works_in_an_unrouted_project_too` in `crates/cli/tests/routing.rs` pin their bytes). One existing assertion changes on purpose: in `a_landed_move_whose_escalation_fails_warns_and_keeps_the_moves_code` the marked record lists as `1\tescalating\tneeds_human\tfix the parser`, not `1\tlocal\t…`. The test file's `R::runs` opens the store through a new `R::store`; it counts what it did before.

**Files:**
- Modify: `crates/cli/src/tiers.rs` (`impl Tiers<'_>`: `column`)
- Modify: `crates/cli/src/cmd/record.rs` (`List`: the tier column through `column`)
- Modify: `crates/cli/src/cmd/finding.rs` (`List`: each routed row carries its tier column)
- Modify: `crates/cli/tests/escalation.rs` (helpers `store`, `issue_iri`; three tests; one assertion changed)
- Create: `crates/exec/tests/escalated_evidence.rs`
- Modify: `docs/routing.md` ("Lists", with the marked item whose issue exists; "Escalating an item", last paragraph, with a GitHub finding's old local reference to an escalated record)

**Interfaces:**
- Consumes: `TieredTracker::escalating` (Task 6), which answers `None` for an id GitHub claims with no read, and otherwise the local `mark_of`; `route`'s tombstone hop and `as_now` in `get_finding`/`findings` (Task 6); the local lists' tombstone exclusion (Task 1, Task 2); `TieredTracker::{prepare_escalation, escalate}` (Task 7); `fl record escalate`/`fl finding escalate` and their `<shown>\tescalated\t#<n>` line (Task 8); the test file's `world`, `R::{ok, refused, routed, local_record, block, runs}` (Task 8, Task 9); `Meta.escalated` (Task 3); `FakeGithub::rate_limited_next_create` (existing); `MemIssues`, `ISSUES`, `attach_reproduction` (existing).
- Produces: `pub fn column(&self, in_tier: Tier, id: &Iri) -> Result<&'static str, StoreError>` on `crate::tiers::Tiers` (fl-cli, crate-internal use): `"escalating"` for a local row whose item is marked, else `in_tier.as_wire()`. Nothing in the shared block changes.
- Unique phrases: none new. The tests assert whole list lines (`1\tescalating\t…`), whole `escalated` lines, and stored IRIs.

- [ ] **Step 1: Write the failing tests**

In `crates/cli/tests/escalation.rs`, in `impl R`, replace `fn runs`'s doc comment and its first two lines

```rust
    /// How many gate runs the store holds, over every gate: a routed
    /// store's ledger is local (routing spec §3.5).
    fn runs(&self) -> usize {
        use fl_core::store::{Catalog, Ledger};
        let store = fl_store::RedbStore::open(&self.home.path().join("fl.redb")).unwrap();
```

with

```rust
    /// The local store, opened between commands.
    fn store(&self) -> fl_store::RedbStore {
        fl_store::RedbStore::open(&self.home.path().join("fl.redb")).unwrap()
    }

    /// How many gate runs the store holds, over every gate: a routed
    /// store's ledger is local (routing spec §3.5).
    fn runs(&self) -> usize {
        use fl_core::store::{Catalog, Ledger};
        let store = self.store();
```

(the rest of `runs` is unchanged). In `a_landed_move_whose_escalation_fails_warns_and_keeps_the_moves_code`, replace

```rust
        "1\tlocal\tneeds_human\tfix the parser\n"
```

with

```rust
        "1\tescalating\tneeds_human\tfix the parser\n"
```

At the end of the file, add:

```rust
/// The fake's issue `n`, as an IRI's text.
fn issue_iri(n: u64) -> String {
    format!("https://github.com/acme/widgets/issues/{n}")
}

// Routing spec §2.4: an item marked escalating is listed with that mark — in
// the tier column, where a GitHub row never shows it — and once the
// escalation finishes, its issue is listed as GitHub's and the local row is
// gone.
#[test]
fn a_marked_record_and_finding_list_as_escalating_until_finished() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.local_record("tidy the lexer", "code");
    for (record, claim) in [("1", "it drops a token"), ("2", "it is slow")] {
        g.ok(&[
            "finding", "raise", "--record", record, "--claim", claim, "--by", "rev",
        ]);
    }
    g.ok(&[
        "record",
        "add",
        "--project",
        "1",
        "--title",
        "look",
        "--area",
        "design",
    ]);
    g.fake.state().rate_limited_next_create = true;
    g.refused(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    g.fake.state().rate_limited_next_create = true;
    g.refused(&["finding", "escalate", "1", "--by", "alice", "--reason", "r"]);
    assert_eq!(
        g.ok(&["record", "list", "--project", "1"]),
        "1\tescalating\ttodo\tfix the parser\n2\tlocal\ttodo\ttidy the lexer\n\
         #1\tgithub\ttodo\tlook\n"
    );
    assert_eq!(
        g.ok(&["record", "list", "--project", "1", "--tier", "local"]),
        "1\tescalating\ttodo\tfix the parser\n2\tlocal\ttodo\ttidy the lexer\n"
    );
    assert_eq!(
        g.ok(&["finding", "list", "--project", "1"]),
        "1\tescalating\traised\trev\tit drops a token\n2\tlocal\traised\trev\tit is slow\n"
    );

    assert_eq!(
        g.ok(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]),
        "1\tescalated\t#2\n"
    );
    assert_eq!(
        g.ok(&["finding", "escalate", "1", "--by", "alice", "--reason", "r"]),
        "1\tescalated\t#3\n"
    );
    assert_eq!(
        g.ok(&["record", "list", "--project", "1"]),
        "2\tlocal\ttodo\ttidy the lexer\n#1\tgithub\ttodo\tlook\n\
         #2\tgithub\ttodo\tfix the parser\n"
    );
    assert_eq!(
        g.ok(&["finding", "list", "--project", "1"]),
        "2\tlocal\traised\trev\tit is slow\n#3\tgithub\traised\trev\tit drops a token\n"
    );
}

// Routing spec §2.3, §2.5: an escalated record's old handle and its old IRI
// both reach its issue — a move moves the issue, a finding raised about it
// is about the issue, and an attempt is recorded against the issue. (The
// bare handle's move is pinned by
// `a_record_escalates_to_an_issue_with_its_state_area_findings_and_provenance`.)
#[test]
fn an_escalated_records_old_handle_moves_its_issue() {
    use fl_core::store::{Catalog, Ledger, Tracker};
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    let old = g.block(1).escalated.unwrap().from.to_string();

    let moved = g.ok(&["record", "move", &old, "--to", "doing"]);
    assert!(moved.starts_with("#1\tdoing\t"), "{moved}");
    assert!(
        g.fake
            .issue(1)
            .labels
            .contains(&"fl:record/doing".to_string())
    );

    for (by, record) in [("by handle", "1"), ("by iri", old.as_str())] {
        g.ok(&[
            "finding", "raise", "--record", record, "--claim", by, "--by", "rev",
        ]);
    }
    let store = g.store();
    let p = store.list_projects().unwrap()[0].id.clone();
    let about: Vec<(String, String)> = store
        .list_findings(&p)
        .unwrap()
        .into_iter()
        .map(|f| (f.claim, f.record.iri().to_string()))
        .collect();
    assert_eq!(
        about,
        vec![
            ("by handle".to_string(), issue_iri(1)),
            ("by iri".to_string(), issue_iri(1)),
        ]
    );
    drop(store);

    // A zero budget is refused before anything is spawned, and the attempt
    // is still recorded — against the issue.
    for record in ["1", old.as_str()] {
        g.fl()
            .args(["attempt", record, "--budget-usd-micros", "0"])
            .assert()
            .code(1);
    }
    let attempts: Vec<String> = g
        .store()
        .attempts(&p)
        .unwrap()
        .into_iter()
        .map(|a| a.record.iri().to_string())
        .collect();
    assert_eq!(attempts, vec![issue_iri(1), issue_iri(1)]);
}

// Routing spec §2.4, §3.5: after a record's escalation its findings stay
// where they are, and `finding list --record` by its old handle, its old
// IRI or its issue lists them from both tiers — the local ones, whose stored
// record is the old IRI, and one raised on GitHub about the issue.
#[test]
fn finding_list_by_an_escalated_records_old_handle_lists_both_tiers() {
    let g = world(BOUND);
    g.routed();
    g.local_record("fix the parser", "code");
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "1",
        "--claim",
        "it drops a token",
        "--by",
        "rev",
    ]);
    g.ok(&["record", "escalate", "1", "--by", "alice", "--reason", "r"]);
    g.ok(&[
        "finding",
        "raise",
        "--record",
        "#1",
        "--claim",
        "it is slow",
        "--by",
        "rev",
        "--area",
        "design",
    ]);
    let old = g.block(1).escalated.unwrap().from.to_string();
    let both = "1\tlocal\traised\trev\tit drops a token\n#2\tgithub\traised\trev\tit is slow\n";
    for record in ["1", old.as_str(), "#1"] {
        assert_eq!(
            g.ok(&["finding", "list", "--record", record]),
            both,
            "--record {record}"
        );
    }
}
```

Create `crates/exec/tests/escalated_evidence.rs`:

```rust
//! Evidence names an escalated record where it now lives (routing spec
//! §2.5 "Evidence", §3.5): a local finding about a local record keeps its
//! stored reference when the record is escalated, and a reproduction run
//! through the routing tracker afterwards is tagged with the record's issue,
//! never the old local IRI.

use fl_core::mem_issues::{ISSUES, MemIssues};
use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
use fl_core::store::{Catalog, Ledger, Roles, Tracker};
use fl_core::{Finding, Kind, MemStore, RecordId, RoutingMap, TieredTracker};
use fl_exec::finding::attach_reproduction;
use std::process::Command;

fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(d.path())
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::create_dir_all(d.path().join("src")).unwrap();
    std::fs::write(d.path().join("src/a.rs"), "fn a() {}").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "first"]);
    d
}

#[test]
fn a_reproduction_after_its_records_escalation_tags_its_run_with_the_issue() {
    let d = repo();
    let local = MemStore::default();
    let issues = MemIssues::default();
    let p = local.add_project(&d.path().display().to_string()).unwrap();
    local.set_routes(&p, &RoutingMap::starting()).unwrap();
    let router = TieredTracker {
        catalog: &local,
        local: &local,
        routes: &local,
        github: &issues,
        escalations: &local,
    };
    let r = router
        .add_record_with_area(&p, "fix the parser", Some("code"))
        .unwrap();
    let fid = router
        .add_finding(Finding::raise(p.clone(), r.clone(), "rev", "it breaks"))
        .unwrap();
    assert!(
        !fid.iri().as_str().starts_with(ISSUES),
        "the finding is local"
    );

    let at = router.prepare_escalation(r.iri(), Kind::Record).unwrap();
    let issue = RecordId(
        router
            .escalate(&at, "alice", "a person decides", 1)
            .unwrap(),
    );
    assert!(
        issue.iri().as_str().starts_with(ISSUES),
        "the record is on GitHub"
    );

    let head = fl_exec::Git::head(d.path()).unwrap();
    let kind = GateKind::Command(CommandSpec {
        program: "false".into(),
        args: vec![],
        delivery: PopulationDelivery::Args,
        timeout_secs: 30,
        pass_codes: vec![0],
    });
    let sel = Selector::Glob {
        pattern: "src/**/*.rs".into(),
    };
    let gate = local
        .add_gate(&p, "fails", kind, sel, 1, &head, "o")
        .unwrap();
    let roles = Roles {
        catalog: &local,
        tracker: &router,
        ledger: &local,
    };
    attach_reproduction(roles, &fid, &gate).unwrap();
    let runs = local.gate_runs(&gate).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].record.as_ref(),
        Some(&issue),
        "the run names the record where it lives now"
    );
    // The finding stays local, and its stored reference is not rewritten
    // (routing spec §3.5): it resolves through the tombstone.
    let stored = local.get_finding(&fid).unwrap().unwrap();
    assert_eq!(stored.record, r);
    assert_eq!(stored.reproduction, Some(gate));
}
```

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test -p fl-cli --test escalation
cargo test -p fl-exec --test escalated_evidence
```

Expected: the first FAILS, two of its 28 tests:
- `a_marked_record_and_finding_list_as_escalating_until_finished`: ``left: "1\tlocal\ttodo\tfix the parser\n2\tlocal\ttodo\ttidy the lexer\n#1\tgithub\ttodo\tlook\n"``, ``right: "1\tescalating\ttodo\tfix the parser\n…"`` — the marked record lists as `local`.
- `a_landed_move_whose_escalation_fails_warns_and_keeps_the_moves_code`: ``left: "1\tlocal\tneeds_human\tfix the parser\n"``, ``right: "1\tescalating\tneeds_human\tfix the parser\n"`` — the assertion this task changes.

`an_escalated_records_old_handle_moves_its_issue` and `finding_list_by_an_escalated_records_old_handle_lists_both_tiers` PASS already, and so does the second command (`a_reproduction_after_its_records_escalation_tags_its_run_with_the_issue`): the router follows the tombstone and shows a finding's record as it now is since Task 6. They pin that behaviour end to end through the binary and through `fl-exec`; the mutation checks below show each goes red without it.

- [ ] **Step 3: Implement**

In `crates/cli/src/tiers.rs`, after `pub struct Tiers<'a> { … }`, add:

```rust
impl Tiers<'_> {
    /// A listed item's tier column (routing spec §2.4): the tier it was
    /// listed from, or `escalating` for a local item marked escalating —
    /// between the tiers. Only a local row reads a mark: a GitHub row is
    /// where an escalation ends, and asks nothing.
    pub fn column(&self, in_tier: Tier, id: &Iri) -> Result<&'static str, StoreError> {
        if in_tier == Tier::Local && self.router.escalating(id)?.is_some() {
            return Ok("escalating");
        }
        Ok(in_tier.as_wire())
    }
}
```

(`Tier`, `Iri` and `StoreError` are imported there already.)

In `crates/cli/src/cmd/record.rs`, in `run`'s `Cmd::List` arm, in the `Some(t) =>` branch's `println!`, replace

```rust
                            in_tier.as_wire(),
```

with

```rust
                            t.column(in_tier, r.id.iri())?,
```

In `crates/cli/src/cmd/finding.rs`, after `use fl_core::routing::Tier;`, add:

```rust
use fl_core::store::StoreError;
```

In `run`'s `Cmd::List` arm, replace the comment and statement from `// ⚠ The whole list or an error: `findings` refuses when a tier` to the end of `let listed … };` with:

```rust
            // ⚠ The whole list or an error: `findings` refuses when a tier
            // it must read cannot be read (routing spec §2.4). Each routed
            // row carries its tier column.
            let listed: Vec<(Option<&str>, Finding)> = match ctx.tiers {
                None => ctx
                    .tracker
                    .list_findings(&p)?
                    .into_iter()
                    .map(|f| (None, f))
                    .collect(),
                Some(t) => t
                    .router
                    .findings(&p, tier)?
                    .into_iter()
                    .map(|(in_tier, f)| Ok((Some(t.column(in_tier, f.id.iri())?), f)))
                    .collect::<Result<_, StoreError>>()?,
            };
```

and in the `for` loop below it, replace

```rust
            for (in_tier, f) in listed
                .iter()
                .filter(|(_, f)| want.is_none_or(|w| f.state == w) && named(f))
            {
                let shown = ctx.show_item(Kind::Finding, f.id.iri())?;
                match in_tier {
                    Some(t) => println!(
                        "{shown}\t{}\t{}\t{}\t{}",
                        t.as_wire(),
                        f.state.as_wire(),
```

with

```rust
            for (column, f) in listed
                .iter()
                .filter(|(_, f)| want.is_none_or(|w| f.state == w) && named(f))
            {
                let shown = ctx.show_item(Kind::Finding, f.id.iri())?;
                match column {
                    Some(column) => println!(
                        "{shown}\t{column}\t{}\t{}\t{}",
                        f.state.as_wire(),
```

(the rest of the arm, the `None =>` arm and the withdrawal counts are unchanged). The `named` filter is unchanged: a local finding's record reads as the issue (`as_now`), and the issue's aliases hold the old IRI, so `--record` by the old handle, the old IRI or `#n` matches it.

In `docs/routing.md`, replace the paragraph under `## Lists` with:

```markdown
In a routed project `fl record list` and `fl finding list` read both tiers and show each item's tier
in a second column (`1\tlocal\ttodo\t…`, `#1\tgithub\ttodo\t…`); `--tier local` or `--tier github`
reads one. A local item marked "escalating" — an escalation that has not finished — shows
`escalating` in that column (`1\tescalating\ttodo\t…`); once its issue exists, a list of both tiers
shows the issue alone, and `--tier local` still shows the marked item. An escalated item is listed
as its issue. When a tier cannot be read — GitHub is down, or this machine binds no repository — the
list is refused rather than shown in part, and the refusal suggests `--tier local`. `fl finding list
--record <id>` lists one record's findings, from both tiers; it works in any project. The withdrawal
counts under a finding list sum both tiers; with `--tier`, they count that tier, and say so.
```

and, under `## Escalating an item`, the last paragraph (from `Last, the local item is replaced by a tombstone`) with:

```markdown
Last, the local item is replaced by a tombstone: the old IRI, the issue, who, when and why. The old
handle and the old IRI then name the issue — a lookup, a move, a finding raised about it, an
attempt, `fl finding list --record` — and the local row is left out of lists. The record's local
findings stay local, and the evidence recorded about them from then on names the issue. A GitHub
finding about a record that was escalated later still shows the record's old local reference in its
issue text; fl resolves it through the tombstone. A local record moved to `needs_human` is escalated
the same way once the move lands, by `fl`; if that escalation fails, the move stands and a
`warning:` names the command that finishes it.
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p fl-cli --test escalation
cargo test -p fl-exec --test escalated_evidence
```

Expected: PASS (28 tests, and 1).

- [ ] **Step 5: Mutation checks**

Each: make the change, run the named test (`cargo test -p fl-cli --test escalation -- <name>`, or `cargo test -p fl-exec --test escalated_evidence`), watch it go red, restore, `cmp` against the saved copy.

1. The mark read: `.is_none()` for `.is_some()` in `Tiers::column` → `a_marked_record_and_finding_list_as_escalating_until_finished` red (the marked record lists as `local`).
2. The mark decides, not the tier: `if in_tier == Tier::Local {` for `if in_tier == Tier::Local && self.router.escalating(id)?.is_some() {` → `a_marked_record_and_finding_list_as_escalating_until_finished` red (the unmarked `2\tlocal\t…` reads `escalating`).
3. A GitHub row never shows the mark: `if true {` for that condition → `a_marked_record_and_finding_list_as_escalating_until_finished` red (`#1\tescalating\ttodo\tlook`).
4. `fl record list` uses the column: `in_tier.as_wire(),` for `t.column(in_tier, r.id.iri())?,` → `a_marked_record_and_finding_list_as_escalating_until_finished` red.
5. `fl finding list` uses the column: `Ok((Some(in_tier.as_wire()), f))` for `Ok((Some(t.column(in_tier, f.id.iri())?), f))` → `a_marked_record_and_finding_list_as_escalating_until_finished` red (`1\tlocal\traised\t…`).
6. The local store's tombstone exclusion, end to end (not this task's code; pinned here): in `crates/store/src/lib.rs` `list_records`, `.filter(|r| r.project == *project)` for `.filter(|r| r.project == *project && !gone.contains(r.id.iri().as_str()))` → `a_marked_record_and_finding_list_as_escalating_until_finished` red (the old `1\tlocal\ttodo\tfix the parser` row is back).
7. The same in `list_findings`: `.filter(|f| f.project == *project)` for `.filter(|f| f.project == *project && !gone.contains(f.id.iri().as_str()))` → `a_marked_record_and_finding_list_as_escalating_until_finished` red.
8. The evidence resolution — this pins Task 6's behaviour end to end: in `crates/core/src/tiered.rs` `get_finding`, `(_, Some(f)) => Ok(Some(f)),` for `(_, Some(f)) => Ok(Some(self.as_now(f)?)),` → `a_reproduction_after_its_records_escalation_tags_its_run_with_the_issue` red (`left: Some(RecordId(Iri("urn:uuid:…")))`, `right: Some(RecordId(Iri("https://github.com/acme/widgets/issues/1")))`).
9. The old handle and IRI reach the issue — Task 6's hop, end to end: delete the `Err(StoreError::Escalated { to, .. }) => { return Ok((Tier::Github, act(self.github.tracker()?, &to)?)); }` arm of `route` → `an_escalated_records_old_handle_moves_its_issue` red, and `finding_list_by_an_escalated_records_old_handle_lists_both_tiers` red (the move by the old IRI fails, and so does `finding list --record 1`: the local store's `Escalated` is no longer followed).

Not observable:
- The `in_tier == Tier::Local` conjunct alone (`if self.router.escalating(id)?.is_some() {`): `escalating` answers `None` for an id GitHub claims, without a read (Task 6), and every GitHub row is an issue URL under the repository's name, which GitHub claims. The conjunct saves a call; check 3 pins that a GitHub row never reads `escalating`.
- `as_now` in `findings`, through `fl finding list --record`: the local finding's stored record is the old IRI, which the issue's `also_known_as` holds, so `named` matches it with or without the resolution. Task 6's `tiered::tests::a_findings_record_reads_as_its_issue_once_escalated` pins it (`cargo test -p fl-core --lib tiered::tests::` red under that mutation).

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1185 passed, 18 ignored.

- [ ] **Step 7: Commit**

```bash
git add crates/cli/src/tiers.rs crates/cli/src/cmd/record.rs crates/cli/src/cmd/finding.rs crates/cli/tests/escalation.rs crates/exec/tests/escalated_evidence.rs docs/routing.md
git commit -m "feat(cli): lists, handles and evidence after an escalation

In a routed store's lists the tier column of a local item marked
escalating reads \`escalating\`, through one helper, \`Tiers::column\`, that
both lists call; only a local row reads a mark. An escalated item is
listed as its issue and its local row is gone. Unrouted output is
unchanged. docs/routing.md says how a marked item whose issue exists is
listed, and that a GitHub finding about a record escalated later still
shows the record's old local reference, which fl resolves.

Tests pin, end to end, what the router already does once an item is
escalated: the old handle and the old IRI move the issue, a finding
raised about either is about the issue, an attempt is recorded against
it, \`fl finding list --record\` by either lists the record's findings in
both tiers, and a reproduction of a local finding about the record tags
its gate run with the issue's IRI. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

---

### Task 11: The escalation live test

Routing spec §5's one live test: "escalate a local record; the issue exists, carries the old IRI, its area label and its findings list, and the local item is a tombstone". It runs over `MemStore` and the router, because the live tests drive the GitHub tracker directly and fl-github has no redb store (plan ruling 22, spec defect 7); the shared conformance cases hold `MemStore` and `RedbStore` to one escalation contract. The test builds a `TieredTracker` whose local tier, catalog, routes and escalations are one `MemStore` holding a project with the starting map, and whose GitHub tier is the live repository's `GithubTracker` (`impl GithubTier for GithubTracker`). A record made in `code` lands locally; it is moved to `doing`, so the issue must carry "the item's title, state, area" (§3.3 step 2) rather than a fresh `todo`. It gets an open finding and a security finding. Then `prepare_escalation` and `escalate`, with the time from the clock, and the test reads GitHub — again within the file's lag allowance (`eventually`) where it reads GitHub directly — for: one open issue in the record's state, labelled `fl:record`, `fl:record/doing` and `fl:area/code`, the only record issue of its project; its block at `fl_format` 3 with the old IRI as the create key and first alias and `escalated` naming who and why (plan rulings 1–3); the escalation line in its text, and the open finding's claim and IRI listed, the security finding's neither (decision 18, plan ruling 4). Locally: the tombstone from the mark's who, why and time pointing at the issue, the mark gone, the local store answering the old id `Escalated`, and the router reading the old id as the issue (§2.3, plan ruling 8).

⚠ An escalated record's IRI stays an alias of its issue for good, and a default `MemStore` mints `seq_iri(1)`, `seq_iri(2)`, … in every run: a second run against the same repository would make a record with the first run's IRI, and its escalation would be refused at the one-namespace check (`already names`). So `MemStore` gains a test-support constructor, `starting_at(first)`, whose first minted id is `seq_iri(first)` — still deterministic for a given start — and the live test starts its store at the clock's unix milliseconds. Each run then has ids, and a project, of its own, which "the only record issue of its project" relies on. Unix milliseconds fit `seq_iri`'s 48 bits.

**Blast radius:** one new test-only constructor, `MemStore::starting_at`, behind `#[cfg(any(test, feature = "conformance"))]` and `#[doc(hidden)]` like `ForeignRecord::for_tests`; nothing calls it but its unit test and the live test, and `MemStore::default()` is unchanged. In `crates/github/tests/live.rs`: one ignored test, and `now()` now reads the clock through the new `now_ms()`, with the same result for its callers. The module doc and `docs/github-tracker.md` name the new test. No test that runs by default changes.

**Files:**
- Modify: `crates/core/src/mem.rs` (`MemStore::starting_at`; its unit test)
- Modify: `crates/github/tests/live.rs` (the module doc; `use` lines; `now_ms`; the test `a_local_record_escalates_to_an_issue_and_leaves_a_tombstone`)
- Modify: `docs/github-tracker.md` ("The live tests": the fourth test, and its status)

**Interfaces:**
- Consumes: `TieredTracker { catalog, local, routes, github, escalations }` (Task 6), `TieredTracker::{prepare_escalation, escalate}`, `Prepared::{resumes, found}` (Task 7); `impl GithubTier for GithubTracker` (Task 5); `Escalations::{tombstone_of, mark_of}`, `Tombstone`, `StoreError::Escalated` (Task 1); `meta::{EscalatedFrom, FL_FORMAT_ESCALATED, escalation_line}` (Task 3); `MemStore::set_routes`, `RoutingMap::starting`, `seq_iri`, `meta::{parse_issue_url, parse_body}`, `ledger::render::escape`, the `conformance` feature (fl-github's dev-dependency on fl-core already enables it), and the file's `tracker`, `client`, `repo`, `eventually` (existing).
- Produces, test support only: `#[cfg(any(test, feature = "conformance"))] #[doc(hidden)] pub fn MemStore::starting_at(first: u64) -> MemStore` — the first id minted is `seq_iri(first)`, the next `seq_iri(first + 1)`; handles still start at 1. In the live test file: `fn now_ms() -> u64`.
- Unique phrases: none new. The live test asserts Task 4's `Open findings when this record was escalated:`, and otherwise labels, IRIs, block fields and variants.

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/mem.rs`, inside `mod tests`, before `fn mem_store_meets_the_escalation_contract`, add:

```rust
    #[test]
    fn a_store_started_at_n_mints_n_first() {
        let s = MemStore::starting_at(1_000);
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        assert_eq!((p.0, r.0), (seq_iri(1_000), seq_iri(1_001)));
        assert_eq!(
            MemStore::default().add_project("/p").unwrap().0,
            seq_iri(1),
            "the default store still starts at 1"
        );
    }
```

In `crates/github/tests/live.rs`, replace the module doc's first paragraph (from `//! Against GitHub itself` to `//! again on what earlier runs left.`) with:

```rust
//! Against GitHub itself (GitHub tracker spec §8.3; GitHub ledger spec
//! §8.4; routing spec §5, the escalation). Ignored by default.
//!
//! Run only against THROWAWAY repositories. The tracker's tests and the
//! escalation's create issues and never delete them. The ledger's append
//! to `fl/ledger`, leave a branch `fl-live/root` at its first commit, and
//! delete nothing: a ledger under a ruleset cannot be deleted, so every
//! test is safe to run again on what earlier runs left.
```

Replace its `FL_GITHUB_LIVE_REPO` bullet (three lines) with:

```rust
//! - `FL_GITHUB_LIVE_REPO`: a private repository (the tracker's tests, the
//!   escalation's, and most of the ledger's) holding at least one commit:
//!   `init` refuses an empty repository.
```

Replace its paragraph beginning `//! A ledger test whose variable is unset skips` with:

```rust
//! A ledger test whose variable is unset skips, saying which; the tracker's
//! tests and the escalation's still fail without `FL_GITHUB_LIVE_REPO`.
```

Replace `use fl_core::store::{StoreError, Tracker};` with:

```rust
use fl_core::store::{Catalog, StoreError, Tracker};
```

After `use fl_core::verdict::Verdict;`, add:

```rust
use fl_core::{Escalations, Kind, RoutingMap, TieredTracker, Tombstone};
```

After `use fl_github::ledger::{Visibility, ruleset_command};`, add:

```rust
use fl_github::meta::{self, EscalatedFrom};
```

Replace `fn now() -> At` (the whole function) with:

```rust
/// This machine's clock, in unix milliseconds.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_millis() as u64
}

fn now() -> At {
    At::from_unix_millis(now_ms())
}
```

After `fn the_edit_history_and_timeline_counts_match_fls_model` (before the doc comment that begins ``/// ⚠ Spec §6.1: `init`'s first commit holds``), add:

```rust
/// Routing spec §5's one live test, through the routing tracker over a
/// local `MemStore` and the live repository: a local record in `code`, with
/// an open finding and a security finding, escalates to one open issue in
/// the record's state, labelled with its area, whose block names the old
/// IRI as its create key, an alias and where it came from, and whose text
/// lists the open finding and not the security one. The local item is a
/// tombstone, and the router reads the old id as the issue.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_local_record_escalates_to_an_issue_and_leaves_a_tombstone() {
    const BY: &str = "fl live test";
    const WHY: &str = "the escalation live test";
    const OPEN: &str = "fl live test: an open finding";
    const SECRET: &str = "fl live test: a security finding";
    let github = tracker();
    let raw = client();
    let repo = repo();
    // ⚠ An escalated record's IRI stays an alias of its issue for good, so
    // each run's ids must be its own: a run that reused an earlier run's
    // record IRI would be refused, the IRI already naming an issue.
    let local = MemStore::starting_at(now_ms());
    let p = local.add_project("/live").unwrap();
    local.set_routes(&p, &RoutingMap::starting()).unwrap();
    let router = TieredTracker {
        catalog: &local,
        local: &local,
        routes: &local,
        github: &github,
        escalations: &local,
    };

    // The starting map routes `code` to the local tier. A state other than
    // `todo` shows the issue carries the record's own.
    let old = router
        .add_record_with_area(&p, "fl live test: an escalated record", Some("code"))
        .unwrap();
    router.set_record_state(&old, State::Doing).unwrap();
    let open = router
        .add_finding(Finding::raise(p.clone(), old.clone(), "live", OPEN))
        .unwrap();
    let mut secret = Finding::raise(p.clone(), old.clone(), "live", SECRET);
    secret.security = true;
    let secret = router.add_finding(secret).unwrap();
    assert!(
        local.get_record(&old).unwrap().is_some(),
        "the record is local"
    );
    assert!(
        local.get_finding(&open).unwrap().is_some(),
        "the finding is local"
    );
    assert!(local.get_finding(&secret).unwrap().unwrap().security);

    let at = router.prepare_escalation(old.iri(), Kind::Record).unwrap();
    assert_eq!((at.resumes(), at.found()), (None, None), "a first run");
    let now = now_ms();
    let issue = router.escalate(&at, BY, WHY, now).unwrap();

    // The issue: one, open, in this repository, labelled and in the record's
    // state.
    let (name, n) = meta::parse_issue_url(&issue).expect("an issue URL");
    assert!(
        name.eq_ignore_ascii_case(&repo),
        "{issue} is not in `{repo}`"
    );
    let labels = |v: &Value| -> Vec<String> {
        v["labels"]
            .as_array()
            .map(|ls| {
                ls.iter()
                    .filter_map(|l| l["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let got = eventually(
        || {
            raw.send(Method::Get, &format!("/repos/{repo}/issues/{n}"), None)
                .unwrap()
                .body
        },
        |v| labels(v).iter().any(|l| l == "fl:area/code"),
    );
    assert_eq!(got["state"].as_str(), Some("open"), "{got}");
    let on = labels(&got);
    for want in ["fl:record", "fl:record/doing", "fl:area/code"] {
        assert!(on.iter().any(|l| l == want), "no `{want}` among {on:?}");
    }
    let listed = eventually(|| github.list_records(&p).unwrap(), |rs| !rs.is_empty());
    let ids: Vec<&Iri> = listed.iter().map(|r| r.id.iri()).collect();
    assert_eq!(ids, vec![&issue], "one issue for the record, never two");

    // Its block: the old IRI as create key, first alias and provenance.
    let body = got["body"].as_str().unwrap().replace("\r\n", "\n");
    let (prose, block) = meta::parse_body(&body).expect("the issue's block");
    assert_eq!(block.fl_format, meta::FL_FORMAT_ESCALATED);
    assert_eq!(block.state, "doing");
    assert_eq!(block.area.as_deref(), Some("code"));
    assert_eq!(block.create_key, old.iri().as_str());
    assert_eq!(block.also_known_as.first(), Some(old.iri()));
    assert_eq!(
        block.escalated,
        Some(EscalatedFrom {
            from: old.iri().clone(),
            by: BY.into(),
            reason: WHY.into(),
        })
    );

    // Its text: the escalation line, and the open finding — not the
    // security one.
    let line = meta::escalation_line(&block).expect("an escalation line");
    assert!(body.contains(&line), "no `{line}` in:\n{body}");
    assert!(
        prose.contains("Open findings when this record was escalated:"),
        "no findings list in:\n{prose}"
    );
    for shown in [OPEN, open.iri().as_str()] {
        assert!(
            prose.contains(&render::escape(shown)),
            "`{shown}` is not listed in:\n{prose}"
        );
    }
    for hidden in [SECRET, secret.iri().as_str()] {
        assert!(
            !body.contains(hidden) && !body.contains(&render::escape(hidden)),
            "the security finding's `{hidden}` is published in:\n{body}"
        );
    }

    // The local item is a tombstone, its mark gone, and the router reads
    // the old id as the issue.
    assert_eq!(
        local.tombstone_of(old.iri()).unwrap(),
        Some(Tombstone {
            from: old.iri().clone(),
            to: issue.clone(),
            by: BY.into(),
            reason: WHY.into(),
            at_ms: now,
        })
    );
    assert_eq!(local.mark_of(old.iri()).unwrap(), None);
    let read = local.get_record(&old);
    assert!(
        matches!(&read, Err(StoreError::Escalated { to, .. }) if *to == issue),
        "the local store still answers the old id: {read:?}"
    );
    let seen = router
        .get_record(&old)
        .unwrap()
        .expect("the router reads the old id");
    assert_eq!(seen.id.iri(), &issue);
    assert_eq!(seen.state, State::Doing);
}
```

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test -p fl-core --lib mem::tests::a_store_started_at_n_mints_n_first
cargo test -p fl-github --test live --no-run
```

Expected: both FAIL to compile — ``error[E0599]: no associated function or constant named `starting_at` found for struct `mem::MemStore` in the current scope`` (the live test's error names `MemStore` the same way). Beyond that, a live test has no offline red run: everything else it exercises exists (Tasks 1–7), and it reads GitHub, which no test run by default may contact.

- [ ] **Step 3: Implement**

In `crates/core/src/mem.rs`, at the top of the `impl MemStore` block that holds `set_routes` (before ``/// Write `project`'s routing map.``), add:

```rust
    /// A store whose first minted id is `seq_iri(first)`, the next
    /// `seq_iri(first + 1)`, and so on: still deterministic for a given
    /// `first`. For a test against a real service that keeps what earlier
    /// runs wrote, so each run's ids are its own. Handles still start at 1.
    /// Not in the binary: the `conformance` feature is a dev-dependency only.
    #[cfg(any(test, feature = "conformance"))]
    #[doc(hidden)]
    pub fn starting_at(first: u64) -> Self {
        let store = Self::default();
        store.inner.borrow_mut().next_id = first.saturating_sub(1);
        store
    }
```

In `docs/github-tracker.md`, under "## The live tests", replace the status paragraph and the paragraph after it (from `**Status: passed on 2026-09-29` to `if it is not set.`) with:

```markdown
**Status: the first three passed on 2026-09-29, writing with a fine-grained token; the
escalation test has not yet been run.** The concurrency test counted 1 clean round, 9 conflicts
caught and 0 updates lost. None has yet been run writing as the App.

The tests that run in CI use an in-process fake GitHub. It proves the structure, not how
GitHub behaves, so four more tests in `crates/github/tests/live.rs` run against GitHub
itself: a round trip of a record and a finding; ten rounds of two writers adding to the same
finding at once, which require no update ever to be lost silently; exact counts of the edit
history and the timeline against the model conflict detection rests on, as
[Conflicts](#conflicts) lists; and an escalation through the routing tracker
([routing.md](routing.md)). The escalation test escalates a local record that has an open
finding and a security finding, and checks that it becomes one open issue in the record's
state with its area label; that the issue's block carries the old IRI as its create key, as an
alias and as where it came from; that its text lists the open finding and not the security
one; and that the local item is a tombstone the router follows. They are ignored by default,
and each fails at once, naming `FL_GITHUB_LIVE_REPO`, if it is not set.
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p fl-core --lib mem::tests::a_store_started_at_n_mints_n_first
cargo test -p fl-github --test live --no-run
cargo test -p fl-github --test live
```

Expected: the unit test passes (1 passed); the live test compiles (`Executable tests/live.rs`); the plain run prints `test a_local_record_escalates_to_an_issue_and_leaves_a_tombstone ... ignored, live: needs FL_GITHUB_LIVE_REPO and a credential` and `test result: ok. 0 passed; 0 failed; 18 ignored`.

The live run itself is the owner's: credentials for it belong to the owner, and the implementer does not run it. The owner exports `FL_GITHUB_TOKEN` (or both `FL_GITHUB_APP_ID` and `FL_GITHUB_APP_KEY`) from a secret store, then runs the module doc's command, filtered to this test:

```bash
FL_GITHUB_LIVE_REPO=owner/repo \
  cargo test -p fl-github --test live -- --ignored --nocapture --test-threads=1 \
  a_local_record_escalates_to_an_issue_and_leaves_a_tombstone
```

Expected: `test result: ok. 1 passed`. It leaves one open issue in the throwaway repository, titled `fl live test: an escalated record`; nothing deletes it, and a rerun makes another.

- [ ] **Step 5: Mutation checks**

The constructor (save `crates/core/src/mem.rs` first; restore and `cmp` after each):

1. Delete the line `store.inner.borrow_mut().next_id = first.saturating_sub(1);` → `cargo test -p fl-core --lib mem::tests::a_store_started_at_n_mints_n_first` red (`left: (…000000000001, …000000000002)`).
2. `first.saturating_sub(1)` → `first` (off by one) → the same test red (`left: (…0000000003e9, …0000000003ea)`).
3. Not observable: `saturating_sub(1)` against `- 1` differs only at `first = 0` — there the constructor starts at 1, as `default` does, rather than overflowing — and no caller passes 0.

The live test: mutation checks do not apply. Its assertions check GitHub and the code from Tasks 1–7, and reverting a guard in that code is those tasks' checks, run offline. Instead, the assertion that catches each failure:

4. A second issue (a create after a search that missed, or a create twice) → `assert_eq!(ids, vec![&issue], "one issue for the record, never two")` over `github.list_records(&p)`, whose project is this run's alone.
5. A missing label (the label call skipped or lost) → `eventually` spends its allowance waiting for `fl:area/code`, then ``no `fl:area/code` among …``; a missing state or kind label → the same loop's ``no `fl:record/doing` among …`` or ``no `fl:record` among …``.
6. A wrong state (the issue created `todo`, or closed) → `got["state"]` is `open` and `block.state == "doing"`, and `seen.state == State::Doing` through the router.
7. A leaked security finding → ``the security finding's `…` is published in:`` — its claim and its IRI, raw or escaped, anywhere in the body.
8. The open finding not listed → `` `…` is not listed in: `` (its claim and IRI, escaped as the tracker writes them) and `no findings list in:`.
9. The block without its provenance → `block.create_key`, `block.also_known_as.first()`, `block.escalated` and `block.fl_format` against the old IRI, `BY`, `WHY` and `FL_FORMAT_ESCALATED`; the line not shown → ``no `…` in:`` for `escalation_line(&block)`.
10. A missing tombstone → `assert_eq!(local.tombstone_of(old.iri()).unwrap(), Some(Tombstone { .. }))` (`left: None`); a tombstone without the mark's who, why or time → the same assertion's fields; a mark left behind → `local.mark_of(old.iri())` is `None`; a local copy still live → `the local store still answers the old id:`; the router not following → `seen.id.iri() == &issue`.

Checked while drafting: the test body, run against `fl_github::fake::FakeGithub` in a scratch copy that is not committed, passed with `starting_at`. Run earlier with the same assertions, it went red at the named assertion for each of four faults injected: a second issue added for the project (4), the issue's labels rewritten without `fl:area/code` (5), the security finding raised without `security` (7), and an `Escalations` whose `tombstone` writes nothing (10).

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1186 passed, 19 ignored (the new live test among them).

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/mem.rs crates/github/tests/live.rs docs/github-tracker.md
git commit -F - <<'EOF'
test(github): a live test escalates a local record to an issue

One ignored live test, routing spec §5: the routing tracker over a
MemStore and the live repository's GithubTracker escalates a local record
in `code`, in state doing, with an open finding and a security finding.
It checks one open issue in this repository, labelled fl:record,
fl:record/doing and fl:area/code, and the only record issue of its
project; a block at fl_format 3 carrying the old IRI as its create key,
its first alias and `escalated` with who and why; the escalation line in
the text and the open finding's claim and IRI, and nothing of the
security finding; and the local item a tombstone from the mark's who, why
and time, with the mark gone, the local store answering Escalated and
the router reading the old id as the issue. GitHub is read again within
the file's lag allowance.

An escalated record's IRI stays an alias of its issue for good, so each
run's ids must be its own. MemStore gains a test-support constructor,
starting_at(first), whose first minted id is seq_iri(first) — still
deterministic for a given start, behind the conformance feature — and
the live test starts its store at the clock's unix milliseconds. The
module doc and the tracker guide name the new test; it has not been run
against GitHub.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>
EOF
git status --porcelain
```

---

---

## After the last task

1. The whole-branch review (subagent-driven development's final review) on the most capable model, against this plan and the spec's rev 2.4.
2. The owner runs the live test once, by hand, against the private throwaway repository (Task 11's Step 4 gives the command; the credential comes from the environment, never from an argument). Its result goes in the PR.
3. A PR against `main`; the owner merges. Then sub-project 4 is complete.
