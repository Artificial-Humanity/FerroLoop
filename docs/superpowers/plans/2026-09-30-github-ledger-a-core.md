# GitHub ledger, plan A — the core

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every ledger entry an identity, add the `Decision` entry and `Ledger::flush`, build `SplitLedger` with the interface its GitHub side implements (cut-over, local ownership, merged reads), flush at every decision site before its state change, write `ledger_root` into a format-2 manifest, fix the rule that says which records a repository owns, and classify GitHub's secondary rate limit — all testable without the `fl/ledger` branch, with every existing configuration (local, mode A) behaving as it does today.

**Architecture:** `fl-core` gains `At` (one fixed-width RFC 3339 spelling), `id`/`at` on `GateRun` and `Attempt`, the `Decision` entry, the `Flushed` report, a do-nothing `Ledger::flush`, and `split.rs`: `SplitLedger { local, github }` over a `LocalLedger` (`Ledger + Outbox`) and a `RemoteLedger` — the trait plan B's `GithubLedger` implements. The local side keeps, per repository `node_id`, a cut-over and published marks. `conformance.rs` gains `MemRemote`, the in-memory remote, and two `Bound`-based suites. `fl-exec` mints ids and times (`stamp.rs`), composes decisions (`decision.rs`) and flushes inside `move_record`, `attach_reproduction` and `verify_finding` before the state changes. The CLI routes `check --record` and `fl attempt` through `Ctx.ledger`. `RedbStore` gains additive tables (candidate index, published marks, cut-overs, ledger roots) and store format 4. `fl-github` gains the local ownership rule. Plan A binds no `SplitLedger` in the CLI: every flush in the product is still the local store's no-op.

**Tech Stack:** Rust 2024 (`rust-version = "1.98"`), redb 4.3, serde/serde_json, uuid 1.26 (`v7`; already a workspace dependency — plan A adds it to `fl-exec` only). No new crates.

**Spec:** `docs/superpowers/specs/2026-09-30-github-ledger-design.md` at `a691cf0` — especially decision 2, §1.2–§1.4 and §1.6 (components), §2.1 (routing, ownership, cut-over), §2.2–§2.6 (flush, `Decision`, reads, call sites), §3.2 step 6 (published marks), §6.1 step 4 (cut-over, `ledger_root`, manifest format 2, store format 4), §7 (errors), §8.2–§8.3 (conformance, evidence before state). **Plan B** (`2026-09-30-github-ledger-b-branch.md`, not yet written) builds `GithubLedger`, the branch protocol, tamper checks, quarantine, `verify`, `init` (which records the cut-over and the root through the methods this plan adds), pre-flight, the disclosure projection, decision comments, the fake's Git Data and GraphQL endpoints, the live tests, `docs/github-ledger.md`, the `ledger = "github"` config key, and the CLI binding of `SplitLedger`.

**Branch:** `ferris/github-ledger-a`, off `ferris/github-ledger-spec` (stacked, as sub-project 2 did), or off `main` once the spec's pull request is merged.

## Global Constraints

- Verification trio, all green before every commit: `cargo fmt --all --check`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`. CI runs the same as the `test, clippy, fmt` check.
- Unit tests live in `#[cfg(test)] mod tests` inside the module they test; black-box CLI tests live in `crates/cli/tests/`.
- **No test contacts the network.** GitHub is the in-process fake (`fl-github`, feature `fake`) or `conformance::MemRemote`.
- `fl-core` stays pure: "No IO, no async, no clock, no network" (`crates/core/src/lib.rs:1`). Ids and times are minted in `fl_exec::stamp`.
- Spec invariants, verbatim: "the local store keeps every run"; "The flush — the ledger commit — comes before the state change it supports"; "A flush failure refuses the decision: no state change"; "`SplitLedger::flush` returns only after the GitHub commit has landed"; "`gate_runs(gate)`: if GitHub cannot be read, an ERROR. Unreachable is not empty"; "Entries written before this change have neither and are never published"; "A refused decision is flushed too (decision 11)"; "Ownership is a local check, with no network … An entry whose record another binding owns is skipped and reported, never an error that blocks the decision"; "Entries recorded before [the cut-over] stay local"; "Runs with no record are never candidates".
- Every existing configuration keeps working: a local project and a mode-A project (GitHub tracker, local ledger) behave exactly as before, apart from the stricter checks this plan names (Review Focus 8; rulings 15–16). Every existing test passes.
- A store written by the current `fl` opens and reads in this build; a store this build writes stays readable by the current `fl` unless it records a ledger root — then it is format 4, which an older `fl` refuses by design (Tasks 8a–8b).
- `snake_case` on every wire (`crates/core/src/wire.rs`); a new enum that crosses the boundary gets `wire_names!` or `wire_tags!`.
- Every guard gets a mutation check: revert it, watch its test go red, restore. Say so in the commit message.
- Commits are authored by the machine account (WORKFLOW.md) and end with `Co-authored-by: Ferris <Ferris@artificialhumanity.io>`. Stage explicit paths — never `git add -A`.
- The repository is public: no machine paths, host names or lab names in code, tests or messages. Test repositories are `acme/widgets`.

## Review Focus

1. **A store written by the current `fl`** — runs and attempts with no `id` or `at`, a string excerpt, a path list — must open, read unchanged, and never offer those entries for publishing. Task 1 (`a_run_and_an_attempt_stored_before_entry_ids_still_read`), Task 4 (`an_entry_stored_before_ids_is_never_waiting_to_be_published`).
2. **GitHub going down in the middle of a decision** must refuse the decision with no state change, keep every run locally, and let the next successful flush publish what was left behind — and a commit that landed before its answer was lost must not be duplicated by the retry. Task 3 (`a_flush_that_fails_publishes_and_marks_nothing_and_the_next_one_carries_it`), Task 4 (split cases 4 and 7), Tasks 5–6 (`…whose_flush_fails…`).
3. **A published copy that differs from the local copy only in what decision 2 withholds** — a `null` excerpt, a path count, the fixed withheld error text — must merge to the local copy; any other difference (population, commit, verdict, a changed excerpt, another error text) is an ERROR naming the entry. Task 3 (`a_copy_that_withholds…`, `a_copy_that_differs_in_anything_else…`).
4. **A format-1 manifest committed before this change** must still import, and a manifest exported now for a project with no ledger must be byte-for-byte what an older `fl` reads and hashes. Task 8a (`a_manifest_with_no_ledger_root_is_format_1_and_an_older_fl_reads_it`).
5. **A 403 that is a permission refusal, not a rate limit,** must stay a refusal that quotes GitHub's message. Task 10 (`a_403_that_is_not_a_rate_limit_stays_a_refusal_naming_its_message`).
6. **Entries recorded before the cut-over, and a repository with no cut-over at all,** must stay local without blocking a decision: the first are never scanned, the second makes the flush publish nothing and say so. Task 3 (`entries_before_the_cut_over_stay_local…`, `with_no_cut_over_a_flush_publishes_nothing_and_says_so`), Task 4 (store cut-over tests).
7. **A pending entry whose record another repository owns** must be skipped and reported, never an error that blocks the decision; a run with no record is never even scanned. Task 3 (`a_pending_entry_of_another_repository_is_skipped_and_reported_not_an_error`), Task 4 (`a_run_with_no_record_never_enters_the_candidate_index`), Task 9 (the ownership rule).
8. **`check --record` in mode A now reads the record from the tracker** (`get_record` through the GitHub tracker — at most one more request) to file its runs under the record's primary id; a record GitHub deleted must still be refused before any gate runs. Task 7 (`check_with_a_record_in_github_mode_ties_its_runs_to_the_issue`, `check_with_a_record_github_deleted_is_refused_before_any_gate_runs`, both in `crates/cli/tests/github.rs` against the fake).

## Rulings the amended spec now states (`1e93d56`, `a691cf0`)

These were this plan's rulings; the spec has since adopted them, so they are not open: `id`/`at` optional with a serde default and one fixed-width `at` spelling (§1.3); `Attempt.output_excerpt` nullable and `paths_touched` a list or a count (§1.3); the merge ignores current visibility (§2.5); `by` exists only on branch lines (§3.1), so `Decision` has none; `SplitLedger` writes the published marks (§3.2 step 6); manifest format 2 exactly when it carries a root that names its `node_id`, import records it and refuses a different one, store format 4, and an export that cannot tell its repository refuses (§6.1 step 4); the finding flush point includes a refused verdict (§2.2); cut-over and local ownership (§2.1).

## Rulings this plan makes

The spec is silent or ambiguous on these. Each says why, and what it costs if wrong.

1. **`PathsTouched` is an untagged enum, `Listed(Vec<String>) | Counted(u64)`.** A stored list reads unchanged, and a count can only be a number. *If wrong:* a tagged form would change every stored attempt's bytes.
2. **The withheld error detail is one fixed text, `fl_core::log::WITHHELD_ERROR_DETAIL`.** Decision 2 says the published copy "says only that the gate errored"; the merge accepts exactly that text on a published `error` verdict and nothing else. *If wrong:* one constant's value.
3. **`Flushed` is a struct, `{ commit: Option<String>, left_local: Vec<LeftLocal> }`, with `Flushed::NOTHING`.** §1.4 names `Flushed::Nothing`; but §2.1 says skipped entries and a missing cut-over are *reported*, and the report needs a channel back to the command. `Flushed::NOTHING` is what the default returns. *If wrong:* cosmetic — an enum with the same data.
4. **The cut-over is set once per repository;** the same id again is a no-op, a different one is `StoreError::CutoverChanged`. Moving it later would strand every entry between the two. **"After the cut-over" is strictly greater** (ids are UUIDv7, so string order is time order). *If wrong:* one comparison.
5. **No cut-over recorded → the flush publishes nothing, not even the decision,** and reports `LeftLocal::NoCutover`; it is not a refusal. The GitHub ledger was never switched on, and plan B's pre-flight refuses before this point anyway ("branch missing … names `fl github ledger init`"). *If wrong:* a mis-set-up binding would proceed silently in plan A's tests only.
6. **The decision's own record must be owned** (else `NotOwned`, before anything is sent): a decision cannot be filed under another repository's record. Only *pending* entries of other repositories are skipped and reported. *If wrong:* one check.
7. **Ownership (Task 9):** a record belongs to the bound repository when its IRI is an issue URL `https://github.com/{owner}/{repo}/issues/{n}` whose `owner/repo` is, without regard to case, the repository's current full name or a name this store has bound to the same `node_id` (`github_bindings` — the configured name kept after a rename). Anything else, including a `urn:uuid:` IRI, is not owned. No network. *If wrong:* a record from before a rename-and-rebind could be skipped — reported, never lost.
8. **A decision may rest only on entries being published now or already published;** otherwise the flush is refused before anything is sent. *If wrong:* a decision could cite runs the shared ledger never holds.
9. **`RedbStore` keeps a candidate index** — additive tables `ledger_candidate_runs` and `ledger_candidate_attempts`, entry id → row key — written in the append's own transaction, only for an entry with an id (and, for a run, a record). `unpublished` ranges over ids after the cut-over, so a plain `fl check` run never enters the scan, and neither does an entry from before ids. *If wrong:* a scan of the whole log per flush.
10. **`attach_reproduction` returns `(GateReport, Flushed)`, `MoveReport` and `FixReport` gain `flushed`,** so the command can print what stayed local. On a refused reproduction the report is dropped with the refusal; the same entries are reported again at the next flush. *If wrong:* one more field.
11. **The `ledger = "github"` config key and the `fl stats` binding rule are left to plan B.** The key is meaningless until `GithubLedger` exists; `TrackerBinding` is `deny_unknown_fields` (`crates/cli/src/config.rs:26-31`), so today it is refused as unknown, by name. Plan A builds the stats local-only note (Task 7) so plan B only wires it. *If wrong:* plan B has one more small task.
12. **`StoreError::FormatVersion` becomes `{ found, oldest, newest }`:** a store newer than this build says "upgrade fl"; an older or unversioned one keeps "start a new store". This is the message a user meets after importing a format-2 manifest and going back to an older build — from the *next* release on; an already-shipped `fl` keeps its own wording. *If wrong:* message text.
13. **An ungated move flushes too** (a decision with no transitions) — "one flush per move, however many transitions". *If wrong:* one `if`.
14. **A clock before 1970 stamps the epoch;** `at` only orders, the id identifies. *If wrong:* cosmetic.
15. **`finding reproduce` checks the finding can take a reproduction before running the gate,** so every refusal after the run is a verdict, which is flushed. *If wrong:* the old order ran the gate first.
16. **`fl attempt` whose publish fails prints its outcome, then exits 2** saying the attempt is kept (§7: "not refused"; "every error exits 2"). *If wrong:* exit code.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/core/src/at.rs` | create | `At`: the one RFC 3339 spelling, pure date arithmetic, serde |
| `crates/core/src/log.rs` | modify | `id`, `at` on `GateRun`/`Attempt`; nullable excerpts; `PathsTouched`; `WITHHELD_ERROR_DETAIL` |
| `crates/core/src/decision.rs` | create | `Decision`, `Outcome`, `TransitionOutcome`, `DecisionKind`, `Flushed`, `LeftLocal` |
| `crates/core/src/split.rs` | create | `SplitLedger`, `Outbox`, `LocalLedger`, `RemoteLedger`, `Batch`, `Pending`, `Coverage`, the merge |
| `crates/core/src/store.rs` | modify | `Ledger::flush`; `StoreError::{Tampered, CutoverChanged, LedgerRootChanged}`; `FormatVersion` range; `Bindings` ledger roots |
| `crates/core/src/mem.rs` | modify | `Outbox` and ledger roots for `MemStore` |
| `crates/core/src/conformance.rs` | modify | `MemRemote`, `RemoteControl`, sample entries; `ledger` becomes `Bound`-based; `split_ledger`; `SplitOver` |
| `crates/core/src/lib.rs` | modify | modules and exports |
| `crates/core/tests/wire_refs.rs` | modify | floors for the new IRIs; a `Decision` sample |
| `crates/exec/Cargo.toml` | modify | `uuid` |
| `crates/exec/src/stamp.rs` | create | `entry_id()`, `now()` |
| `crates/exec/src/decision.rs` | create | the five `Decision` builders |
| `crates/exec/src/journal.rs` | create (test-only) | a tracker+ledger double recording the order of flushes and state changes |
| `crates/exec/src/evaluate.rs` | modify | runs stamped; `GateReport.run`; `run_single_gate` takes a record |
| `crates/exec/src/record.rs` | modify | the flush inside `move_record`; `MoveReport.flushed` |
| `crates/exec/src/finding.rs` | modify | runs tagged with the finding's record; flush before `update_finding`; `FixReport.flushed` |
| `crates/exec/src/population.rs` | modify | `ExecError::Unpublished` |
| `crates/exec/src/lib.rs` | modify | modules |
| `crates/store/src/lib.rs` | modify | `Outbox` (candidate index, marks, cut-overs); ledger roots; format 4; `raise_format` |
| `crates/store/src/manifest.rs` | modify | format 2, `LedgerRoot`, "upgrade fl" |
| `crates/cli/src/ctx.rs` | modify | `Ctx.ledger`; `roles()`; `flush_notes` |
| `crates/cli/src/testing.rs` | create (test-only) | `Flushes`, a ledger double |
| `crates/cli/src/main.rs` | modify | `Ctx.ledger`; the manifest binding |
| `crates/cli/src/cmd/check.rs` | modify | primary record id; the flush of `check --record` |
| `crates/cli/src/cmd/attempt.rs` | modify | stamped attempt through `Ctx.ledger`; publish after the append |
| `crates/cli/src/cmd/record.rs` | modify | print what stayed local |
| `crates/cli/src/cmd/finding.rs` | modify | `attach_reproduction`'s pair; print what stayed local |
| `crates/cli/src/cmd/stats.rs` | modify | the report and its local-only note |
| `crates/cli/src/cmd/gate.rs` | modify | `run_single_gate(…, None)` |
| `crates/cli/src/cmd/manifest.rs` | modify | `Binding`; export writes the root or refuses |
| `crates/cli/tests/manifest.rs` | modify | the root through export and import; the refusals |
| `docs/sharing-gates.md` | modify | store format 4 and "upgrade fl" |
| `crates/github/src/owner.rs` | create | `issue_of_repository`, the local ownership rule |
| `crates/github/src/lib.rs` | modify | module |
| `crates/cli/tests/github.rs` | modify | `check --record` in mode A |
| `crates/github/src/client.rs` | modify | the secondary rate limit |
| `crates/github/src/fake.rs` | modify | two knobs |

---

### Task 1: Entry identity — `At`, ids, and the fields a published copy withholds

**Files:**
- Create: `crates/core/src/at.rs`
- Modify: `crates/core/src/log.rs:1-50` (the whole file)
- Modify: `crates/core/src/lib.rs:3-31` (modules, exports)
- Modify: `crates/core/src/conformance.rs:927-938` (`sample_run`)
- Modify: `crates/core/tests/wire_refs.rs:130-158`
- Create: `crates/exec/src/stamp.rs`
- Modify: `crates/exec/Cargo.toml` (`uuid`)
- Modify: `crates/exec/src/lib.rs:1-17`
- Modify: `crates/exec/src/evaluate.rs:13-20` (`GateReport`), `:189-200` and `:217-224` (the append and the report in `run_gate`), tests
- Modify: `crates/cli/src/cmd/attempt.rs:6`, `:95-106`
- Modify: `crates/store/src/lib.rs:1194-1203` (a test's `GateRun` literal), tests

**Interfaces:**
- Consumes: nothing new.
- Produces:

```rust
// fl_core::log
pub const WITHHELD_ERROR_DETAIL: &str;       // decision 2's text for a published error's detail

// fl_core::at (re-exported as fl_core::At, fl_core::AtError)
pub struct At(String);                       // Clone, Eq, Ord, Hash, Debug, Serialize, Deserialize, Display
impl At {
    pub fn from_unix_millis(ms: u64) -> Self;
    pub fn parse(s: &str) -> Result<Self, AtError>;
    pub fn as_str(&self) -> &str;
}
pub struct AtError(String);                  // thiserror, PartialEq

// fl_core::log (PathsTouched re-exported at the crate root)
pub struct GateRun {
    pub id: Option<Iri>, pub at: Option<At>,
    pub gate: GateId, pub record: Option<RecordId>, pub commit: String,
    pub verdict: Verdict, pub population: u64,
    pub output_excerpt: Option<String>,      // was String
    pub duration_ms: u64, pub cost_usd_micros: u64,
}
pub struct Attempt {
    pub id: Option<Iri>, pub at: Option<At>,
    pub project: ProjectId, pub record: RecordId, pub adapter: String,
    pub status: AttemptStatus, pub duration_ms: u64, pub tokens_in: u64,
    pub tokens_out: u64, pub cost_usd_micros: u64,
    pub paths_touched: PathsTouched,         // was Vec<String>
    pub output_excerpt: Option<String>,      // was String
}
pub enum PathsTouched { Listed(Vec<String>), Counted(u64) }   // #[serde(untagged)]
impl PathsTouched { pub fn count(&self) -> u64; }

// fl_exec::stamp
pub fn entry_id() -> Iri;                    // urn:uuid, version 7
pub fn now() -> At;

// fl_exec::evaluate
pub struct GateReport { /* existing fields */ pub run: Iri }   // the GateRun's id
```

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/at.rs` with only its test module for now (the implementation comes in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants_spell_as_rfc_3339_utc_to_the_millisecond() {
        for (ms, spelled) in [
            (0, "1970-01-01T00:00:00.000Z"),
            (951_782_400_000, "2000-02-29T00:00:00.000Z"),
            (1_727_658_123_004, "2024-09-30T01:02:03.004Z"),
            (MAX_MILLIS, "9999-12-31T23:59:59.999Z"),
        ] {
            assert_eq!(At::from_unix_millis(ms).as_str(), spelled, "{ms}");
        }
        assert_eq!(
            At::from_unix_millis(u64::MAX).as_str(),
            "9999-12-31T23:59:59.999Z",
            "past the last four-digit year it holds at that year's last millisecond"
        );
    }

    #[test]
    fn every_spelling_parses_back_to_itself() {
        for ms in [
            0,
            999,
            86_399_999,
            86_400_000,
            951_868_799_999,
            1_727_658_123_004,
            MAX_MILLIS,
        ] {
            let at = At::from_unix_millis(ms);
            assert_eq!(At::parse(at.as_str()), Ok(at.clone()), "{at}");
        }
    }

    // ⚠ Entries are ordered by `at` (spec §2.5) through the derived `Ord`,
    // which compares the strings. This pins that string order is time order.
    #[test]
    fn string_order_is_time_order() {
        let times: Vec<At> = [0, 999, 1000, 86_400_000, 951_782_400_000, MAX_MILLIS]
            .iter()
            .map(|ms| At::from_unix_millis(*ms))
            .collect();
        for pair in times.windows(2) {
            assert!(pair[0] < pair[1], "{} is not before {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn anything_but_the_one_spelling_is_refused_by_name() {
        for bad in [
            "",
            "2026-09-30T12:34:56Z",
            "2026-09-30T12:34:56.789+00:00",
            "2026-09-30 12:34:56.789Z",
            "2026-09-30T12:34:56.789z",
            "2026-02-30T00:00:00.000Z",
            "2026-13-01T00:00:00.000Z",
            "2026-09-30T24:00:00.000Z",
            "2026-09-30T23:59:60.000Z",
            "1969-12-31T23:59:59.999Z",
        ] {
            let err = At::parse(bad).expect_err(bad);
            assert!(err.to_string().contains(&format!("`{bad}`")), "{err}");
        }
    }

    #[test]
    fn the_wire_form_is_a_plain_string_checked_on_the_way_in() {
        let at: At = serde_json::from_str("\"2026-09-30T12:34:56.789Z\"").unwrap();
        assert_eq!(
            serde_json::to_string(&at).unwrap(),
            "\"2026-09-30T12:34:56.789Z\""
        );
        assert!(serde_json::from_str::<At>("\"2026-09-30T12:34:56Z\"").is_err());
        assert!(serde_json::from_str::<At>("1727658123004").is_err());
    }
}
```

Replace the whole of `crates/core/src/log.rs` with the version in Step 3, but first append these tests to it (they fail to compile until Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::seq_iri;

    #[test]
    fn a_run_stored_before_entry_ids_still_reads_and_has_neither() {
        // Exactly what every store held before this change: no `id`, no
        // `at`, and the excerpt a plain string.
        let json = format!(
            r#"{{"gate":"{}","record":null,"commit":"abc","verdict":{{"pass":{{"population":1}}}},"population":1,"output_excerpt":"ok","duration_ms":1,"cost_usd_micros":0}}"#,
            seq_iri(1)
        );
        let run: GateRun = serde_json::from_str(&json).unwrap();
        assert_eq!((run.id, run.at), (None, None));
        assert_eq!(run.output_excerpt.as_deref(), Some("ok"));
    }

    #[test]
    fn an_attempt_stored_before_entry_ids_still_reads_its_path_list() {
        let json = format!(
            r#"{{"project":"{}","record":"{}","adapter":"claude","status":"completed","duration_ms":1,"tokens_in":0,"tokens_out":0,"cost_usd_micros":0,"paths_touched":["a.rs"],"output_excerpt":""}}"#,
            seq_iri(1),
            seq_iri(2)
        );
        let a: Attempt = serde_json::from_str(&json).unwrap();
        assert_eq!((a.id, a.at), (None, None));
        assert_eq!(a.paths_touched, PathsTouched::Listed(vec!["a.rs".into()]));
        assert_eq!(a.paths_touched.count(), 1);
        assert_eq!(a.output_excerpt.as_deref(), Some(""));
    }

    // Decision 2: on a repository that is not private, a published copy
    // carries `null` for the excerpt and a count for the paths.
    #[test]
    fn a_published_copy_can_withhold_the_excerpt_and_count_the_paths() {
        let json = format!(
            r#"{{"id":"{}","at":"2026-09-30T00:00:00.000Z","project":"{}","record":"{}","adapter":"claude","status":"completed","duration_ms":1,"tokens_in":0,"tokens_out":0,"cost_usd_micros":0,"paths_touched":3,"output_excerpt":null}}"#,
            seq_iri(9),
            seq_iri(1),
            seq_iri(2)
        );
        let a: Attempt = serde_json::from_str(&json).unwrap();
        assert_eq!(a.paths_touched, PathsTouched::Counted(3));
        assert_eq!(a.paths_touched.count(), 3);
        assert_eq!(a.output_excerpt, None);
        assert_eq!(
            a.at.as_ref().map(At::as_str),
            Some("2026-09-30T00:00:00.000Z")
        );
    }

    /// `GateRun` and `Attempt` as the current fl declares them, frozen here:
    /// no `deny_unknown_fields`, a string excerpt, a path list. An older fl
    /// must still read what this one writes into the same store.
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct OlderRun {
        gate: GateId,
        record: Option<RecordId>,
        commit: String,
        verdict: Verdict,
        population: u64,
        output_excerpt: String,
        duration_ms: u64,
        cost_usd_micros: u64,
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct OlderAttempt {
        project: ProjectId,
        record: RecordId,
        adapter: String,
        status: AttemptStatus,
        duration_ms: u64,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd_micros: u64,
        paths_touched: Vec<String>,
        output_excerpt: String,
    }

    #[test]
    fn an_older_fl_still_reads_the_entries_this_one_stores() {
        let run = GateRun {
            id: Some(seq_iri(9)),
            at: Some(At::from_unix_millis(1)),
            gate: GateId(seq_iri(1)),
            record: Some(RecordId(seq_iri(2))),
            commit: "abc".into(),
            verdict: Verdict::from_predicate(true, 1),
            population: 1,
            output_excerpt: Some("ok".into()),
            duration_ms: 1,
            cost_usd_micros: 0,
        };
        let old: OlderRun = serde_json::from_str(&serde_json::to_string(&run).unwrap())
            .expect("an older fl reads a run this one stored");
        assert_eq!(old.output_excerpt, "ok");

        let attempt = Attempt {
            id: Some(seq_iri(10)),
            at: Some(At::from_unix_millis(2)),
            project: ProjectId(seq_iri(3)),
            record: RecordId(seq_iri(2)),
            adapter: "claude".into(),
            status: AttemptStatus::Completed,
            duration_ms: 1,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd_micros: 0,
            paths_touched: PathsTouched::Listed(vec!["a.rs".into()]),
            output_excerpt: Some(String::new()),
        };
        let old: OlderAttempt = serde_json::from_str(&serde_json::to_string(&attempt).unwrap())
            .expect("an older fl reads an attempt this one stored");
        assert_eq!(old.paths_touched, vec!["a.rs".to_string()]);
    }
}
```

Append to `crates/exec/src/evaluate.rs`'s `mod tests` (after `every_run_is_recorded_with_its_population_whatever_the_verdict`):

```rust
    // Spec §1.3: every run is minted an id and a time when it is recorded,
    // and the report names the run it describes, so a decision can cite it.
    #[test]
    fn every_run_is_recorded_under_its_own_id_and_time_and_the_report_names_it() {
        let d = repo_with(&[("src/a.rs", "fn a() {}")]);
        let s = MemStore::default();
        let p = setup(&s, d.path(), "true", "src/**/*.rs", Regret::Low);
        let first = evaluate_transition(&s, &s, &p, "launch", None).unwrap();
        let second = evaluate_transition(&s, &s, &p, "launch", None).unwrap();
        let gate = s.list_gates(&p).unwrap()[0].id.clone();
        let runs = s.gate_runs(&gate).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id.as_ref(), Some(&first.gates[0].run));
        assert_eq!(runs[1].id.as_ref(), Some(&second.gates[0].run));
        assert_ne!(first.gates[0].run, second.gates[0].run);
        assert!(first.gates[0].run.as_str().starts_with("urn:uuid:"));
        // Stamped, not ordered: two runs a moment apart may share a
        // millisecond, and the wall clock is not this test's to assert.
        assert!(runs.iter().all(|r| r.at.is_some()));
        assert_eq!(
            runs[0].output_excerpt.as_deref(),
            Some(first.gates[0].output_excerpt.as_str()),
            "the local copy always keeps the excerpt"
        );
    }
```

Add to `crates/store/src/lib.rs`'s `mod tests`, after `fresh()` (the helper is reused by Task 4):

```rust
    /// A store holding one run and one attempt in the shape every store held
    /// before entry ids: raw rows, as the current fl writes them. The row
    /// counters move with them, so a later append cannot overwrite them.
    fn with_legacy_entries() -> (RedbStore, tempfile::TempDir, ProjectId, GateId) {
        let (s, d) = fresh();
        let p = s.add_project("/p").unwrap();
        let g = s.add_gate(&p, "fmt", kind(), selector(), 1, "abc", "o").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let run = format!(
            r#"{{"gate":"{}","record":"{}","commit":"abc","verdict":{{"pass":{{"population":1}}}},"population":1,"output_excerpt":"ok","duration_ms":1,"cost_usd_micros":0}}"#,
            g.iri(),
            r.iri()
        );
        let attempt = format!(
            r#"{{"project":"{}","record":"{}","adapter":"claude","status":"completed","duration_ms":1,"tokens_in":0,"tokens_out":0,"cost_usd_micros":0,"paths_touched":["a.rs"],"output_excerpt":""}}"#,
            p.iri(),
            r.iri()
        );
        {
            let tx = s.db.begin_write().unwrap();
            tx.open_table(GATE_RUNS)
                .unwrap()
                .insert(1u64, run.as_str())
                .unwrap();
            tx.open_table(ATTEMPTS)
                .unwrap()
                .insert(1u64, attempt.as_str())
                .unwrap();
            {
                let mut meta = tx.open_table(META).unwrap();
                meta.insert(NEXT_RUN, 1u64).unwrap();
                meta.insert(NEXT_ATTEMPT, 1u64).unwrap();
            }
            tx.commit().unwrap();
        }
        (s, d, p, g)
    }

    #[test]
    fn a_run_and_an_attempt_stored_before_entry_ids_still_read() {
        let (s, _d, p, g) = with_legacy_entries();
        let runs = s.gate_runs(&g).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].id.as_ref(), runs[0].at.as_ref()), (None, None));
        assert_eq!(runs[0].output_excerpt.as_deref(), Some("ok"));
        let attempts = s.attempts(&p).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].id, None);
        assert_eq!(
            attempts[0].paths_touched,
            fl_core::log::PathsTouched::Listed(vec!["a.rs".into()])
        );
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p fl-core --lib 2>&1 | tail -20`
Expected: FAIL to compile — `At`, `PathsTouched`, `GateRun::id` and `MAX_MILLIS` do not exist (`at.rs` is not yet a module, so add `pub mod at;` to `lib.rs` first to see the at-tests fail on the missing items).

- [ ] **Step 3: `At`, the entry fields, and every construction site**

Put this above the test module in `crates/core/src/at.rs`:

```rust
//! When a ledger entry was made (GitHub ledger spec §1.3): RFC 3339, UTC, to
//! the millisecond, in exactly one spelling — `2026-09-30T12:34:56.789Z`.
//!
//! ⚠ One fixed-width spelling on purpose. Entries are ordered by `at` and
//! then by id (spec §2.5). With one width and one zone, string order IS time
//! order, so the derived `Ord` is correct and no reader parses an offset to
//! sort. `parse` accepts exactly what `from_unix_millis` writes.
//!
//! No clock here: `fl-core` has none. The caller reads the time
//! (`fl_exec::stamp::now`) and hands in the milliseconds.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// 9999-12-31T23:59:59.999Z, the last instant a four-digit year spells.
const MAX_MILLIS: u64 = 253_402_300_799_999;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct At(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "`{0}` is not a time fl writes: it must be RFC 3339 in UTC, to the millisecond, such as \
     `2026-09-30T12:34:56.789Z`"
)]
pub struct AtError(String);

impl At {
    /// `ms` milliseconds after the Unix epoch. Past the year 9999 it is held
    /// at that year's last millisecond, which is all RFC 3339 can spell.
    pub fn from_unix_millis(ms: u64) -> Self {
        let ms = ms.min(MAX_MILLIS);
        let secs = ms / 1000;
        let (y, m, d) = civil_from_days((secs / 86_400) as i64);
        let rem = secs % 86_400;
        At(format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60,
            ms % 1000
        ))
    }

    /// The one spelling, and nothing else: no other offset, no other
    /// precision, no date that does not exist.
    pub fn parse(s: &str) -> Result<Self, AtError> {
        let refuse = || AtError(s.to_string());
        let b = s.as_bytes();
        if b.len() != 24 {
            return Err(refuse());
        }
        for (i, c) in b.iter().enumerate() {
            let fits = match i {
                4 | 7 => *c == b'-',
                10 => *c == b'T',
                13 | 16 => *c == b':',
                19 => *c == b'.',
                23 => *c == b'Z',
                _ => c.is_ascii_digit(),
            };
            if !fits {
                return Err(refuse());
            }
        }
        let n = |from: usize, to: usize| -> i64 {
            s[from..to].parse().expect("checked above: ASCII digits")
        };
        let days = days_from_civil(n(0, 4), n(5, 7), n(8, 10));
        let ms = days * 86_400_000
            + n(11, 13) * 3_600_000
            + n(14, 16) * 60_000
            + n(17, 19) * 1000
            + n(20, 23);
        // An impossible date (February 30th), hour 24 or second 60 lands on
        // another instant, and that instant spells differently. A date
        // before 1970 is negative here, wraps to a huge `u64`, is held at
        // the year 9999, and spells differently too.
        let at = At::from_unix_millis(ms as u64);
        if at.0 != s {
            return Err(refuse());
        }
        Ok(at)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`). Any month or day is accepted and lands somewhere;
/// `parse`'s round trip refuses the ones that do not exist.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `z` days after 1970-01-01 (Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

impl fmt::Display for At {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for At {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for At {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        At::parse(&raw).map_err(serde::de::Error::custom)
    }
}
```

Replace `crates/core/src/log.rs` above its new test module with:

```rust
use crate::at::At;
use crate::ids::{GateId, ProjectId, RecordId};
use crate::iri::Iri;
use crate::verdict::Verdict;
use serde::{Deserialize, Serialize};

/// One execution of one gate. Append-only.
///
/// ⚠ `population` is not optional on a recorded run. A verdict without a
/// count cannot be written, which is why the field is not an `Option`.
///
/// ⚠ `id` and `at` (GitHub ledger spec §1.3) are `None` only on a run
/// recorded before entries carried them. Such a run still reads, and is
/// never published: a shared ledger de-duplicates by id, and an entry with
/// none cannot be told from a second copy of itself. Every run recorded now
/// has both — `fl_exec::stamp` mints them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRun {
    #[serde(default)]
    pub id: Option<Iri>,
    #[serde(default)]
    pub at: Option<At>,
    pub gate: GateId,
    pub record: Option<RecordId>,
    pub commit: String,
    pub verdict: Verdict,
    pub population: u64,
    /// `Some` in the local store, always. `None` only on a copy published to
    /// a repository that is not private, which withholds it (decision 2).
    pub output_excerpt: Option<String>,
    pub duration_ms: u64,
    pub cost_usd_micros: u64,
}

/// What an `error` verdict's detail reads on a copy published to a
/// repository that is not private (decision 2): the detail can name paths
/// and hosts, and a verdict carries no error class to publish instead, so
/// the copy says only that the gate errored. The merge (`split.rs`) accepts
/// exactly this text in place of the local detail, and nothing else.
pub const WITHHELD_ERROR_DETAIL: &str =
    "the gate errored; its detail is withheld because the repository is not private";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Completed,
    Timeout,
    Crashed,
    Refused,
}

crate::wire::wire_names!(AttemptStatus as attempt_status_wire {
    Completed => "completed",
    Timeout => "timeout",
    Crashed => "crashed",
    Refused => "refused",
});

/// The paths an attempt changed: the list, in the local store; only how
/// many, on a copy published to a repository that is not private
/// (decision 2).
///
/// Untagged on purpose: a list is what every store has always held, so a
/// stored attempt reads unchanged, and a count can only be a number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PathsTouched {
    Listed(Vec<String>),
    Counted(u64),
}

impl PathsTouched {
    pub fn count(&self) -> u64 {
        match self {
            PathsTouched::Listed(paths) => paths.len() as u64,
            PathsTouched::Counted(n) => *n,
        }
    }
}

/// One runner invocation. Append-only. `id`, `at` and the two withheld
/// fields follow [`GateRun`]'s rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    #[serde(default)]
    pub id: Option<Iri>,
    #[serde(default)]
    pub at: Option<At>,
    pub project: ProjectId,
    pub record: RecordId,
    pub adapter: String,
    pub status: AttemptStatus,
    pub duration_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd_micros: u64,
    pub paths_touched: PathsTouched,
    pub output_excerpt: Option<String>,
}
```

In `crates/core/src/lib.rs`, add the module and the exports:

```rust
mod wire;

pub mod at;
#[cfg(any(test, feature = "conformance"))]
#[doc(hidden)]
pub mod conformance;
pub mod finding;
pub mod ids;
pub mod iri;
pub mod log;
pub mod mem;
pub mod model;
pub mod stale;
pub mod store;
pub mod verdict;

pub use at::{At, AtError};
pub use finding::{Finding, FindingError, FindingState};
pub use ids::{FindingId, GateId, Kind, ProjectId, RecordId};
pub use iri::{Iri, IriError};
pub use log::{Attempt, AttemptStatus, GateRun, PathsTouched};
```

(The `pub use mem::…`, `model::…`, `stale::…`, `store::…`, `verdict::…` lines and `MANIFEST_PATH` stay as they are.)

In `crates/core/src/conformance.rs`, `sample_run` (`:927-938`) becomes a run recorded before ids — the shared suites keep exercising that shape:

```rust
/// A run as recorded before entry ids (spec §1.3): no `id`, no `at`.
fn sample_run(gate: GateId, commit: &str, population: u64) -> GateRun {
    GateRun {
        id: None,
        at: None,
        gate,
        record: None,
        commit: commit.into(),
        verdict: Verdict::from_predicate(true, population),
        population,
        output_excerpt: Some(String::new()),
        duration_ms: 1,
        cost_usd_micros: 0,
    }
}
```

In `crates/core/tests/wire_refs.rs`, the `gate_run` and `attempt` samples (`:130-151`) and their floors (`:157-158`) become:

```rust
    let gate_run = GateRun {
        id: Some(iri(7)),
        at: Some(At::from_unix_millis(1)),
        gate: g.clone(),
        record: Some(r.clone()),
        commit: "abc".into(),
        verdict: Verdict::from_predicate(true, 1),
        population: 1,
        output_excerpt: Some(String::new()),
        duration_ms: 1,
        cost_usd_micros: 0,
    };
    let attempt = Attempt {
        id: Some(iri(8)),
        at: Some(At::from_unix_millis(2)),
        project: p.clone(),
        record: r.clone(),
        adapter: "claude".into(),
        status: AttemptStatus::Completed,
        duration_ms: 1,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd_micros: 0,
        paths_touched: PathsTouched::Listed(vec!["a.rs".into()]),
        output_excerpt: Some(String::new()),
    };
```

```rust
    assert_every_reference_refuses_a_handle("GateRun", &gate_run, 3);
    assert_every_reference_refuses_a_handle("Attempt", &attempt, 3);
```

Add `uuid` to `crates/exec/Cargo.toml`'s `[dependencies]`:

```toml
uuid.workspace = true
```

Create `crates/exec/src/stamp.rs`:

```rust
//! The id and the time a new ledger entry is stamped with (GitHub ledger
//! spec §1.3). Here rather than in `fl-core`, which has no clock and no
//! randomness.

use fl_core::{At, Iri};
use std::time::{SystemTime, UNIX_EPOCH};

/// A fresh `urn:uuid:` of version 7: ordered by time, and unique without
/// asking anyone.
pub fn entry_id() -> Iri {
    Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7()))
        .expect("a formatted urn:uuid is a valid IRI")
}

/// Now. A clock set before 1970 stamps the epoch itself: a visibly wrong
/// time on an entry, never a refusal of evidence already gathered. `at` only
/// orders entries; the id alone identifies one.
pub fn now() -> At {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    At::from_unix_millis(ms)
}

#[cfg(test)]
mod tests {
    #[test]
    fn two_ids_are_never_the_same_and_both_are_version_7() {
        let (a, b) = (super::entry_id(), super::entry_id());
        assert_ne!(a, b);
        for id in [&a, &b] {
            let hex = id.as_str().strip_prefix("urn:uuid:").expect("a urn:uuid");
            assert_eq!(hex.as_bytes()[14], b'7', "not version 7: {id}");
        }
    }

    #[test]
    fn now_is_after_this_code_was_written() {
        assert!(super::now().as_str() > "2026-09-30T00:00:00.000Z");
    }
}
```

In `crates/exec/src/lib.rs`, add `pub mod stamp;` after `pub mod runner;`.

In `crates/exec/src/evaluate.rs`, add `use crate::stamp;` and `use fl_core::iri::Iri;` to the imports, and give `GateReport` (`:13-20`) the run's id:

```rust
#[derive(Debug, Clone)]
pub struct GateReport {
    pub gate: GateId,
    pub name: String,
    pub verdict: Verdict,
    pub staleness: Staleness,
    pub output_excerpt: String,
    pub duration_ms: u64,
    /// The id the run was recorded under, so a decision can name the runs
    /// it rests on (GitHub ledger spec §2.3).
    pub run: Iri,
}
```

In `run_gate`, replace the append (`:189-200`) and the report (`:217-224`) with:

```rust
    let run = stamp::entry_id();
    ledger
        .append_gate_run(GateRun {
            id: Some(run.clone()),
            at: Some(stamp::now()),
            gate: def.id.clone(),
            record: record.cloned(),
            commit: head.to_string(),
            verdict: verdict.clone(),
            population: verdict.population().unwrap_or(0),
            output_excerpt: Some(excerpt.clone()),
            duration_ms,
            cost_usd_micros: 0,
        })
        .map_err(|e| ExecError::Store(e.to_string()))?;
```

```rust
    Ok(GateReport {
        gate: def.id.clone(),
        name: def.name.clone(),
        verdict,
        staleness,
        output_excerpt: excerpt,
        duration_ms,
        run,
    })
```

In `crates/cli/src/cmd/attempt.rs`, import `PathsTouched` (`:6` becomes `use fl_core::log::{Attempt, AttemptStatus, PathsTouched};`) and stamp the attempt (`:95-106`):

```rust
    store.append_attempt(Attempt {
        id: Some(fl_exec::stamp::entry_id()),
        at: Some(fl_exec::stamp::now()),
        project: record.project,
        record: record.id,
        adapter: "claude".into(),
        status: outcome.status,
        duration_ms: outcome.duration_ms,
        tokens_in: outcome.tokens_in,
        tokens_out: outcome.tokens_out,
        cost_usd_micros: outcome.cost_usd_micros,
        paths_touched: PathsTouched::Listed(outcome.paths_touched.clone()),
        output_excerpt: Some(outcome.output_excerpt.clone()),
    })?;
```

In `crates/store/src/lib.rs`, the `GateRun` literal of `a_gate_run_verdict_survives_a_close_and_reopen` (`:1194-1203`) gains `id: None, at: None,` and `output_excerpt: Some(String::new()),`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS, including the 5 `at` tests, the 4 `log` tests, `two_ids_are_never_the_same…`, `now_is_after…`, `every_run_is_recorded_under_its_own_id…` and `a_run_and_an_attempt_stored_before_entry_ids_still_read`.

- [ ] **Step 5: Mutation checks**

One at a time, confirm red, restore:
- Remove `#[serde(untagged)]` from `PathsTouched` → `an_attempt_stored_before_entry_ids…` and `a_run_and_an_attempt_stored_before…` FAIL.
- Delete the round-trip comparison in `At::parse` (return `Ok(at)` unconditionally) → `anything_but_the_one_spelling…` FAILS on `2026-02-30…`.
- In `run_gate`, mint the id once for the process (`static` or a constant `seq_iri(1)`) → `every_run_is_recorded_under_its_own_id…` FAILS.
- Add `#[serde(deny_unknown_fields)]` to `OlderRun` → `an_older_fl_still_reads…` FAILS (proves the test would catch an older reader that refused new fields).

- [ ] **Step 6: Trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/at.rs crates/core/src/log.rs crates/core/src/lib.rs \
  crates/core/src/conformance.rs crates/core/tests/wire_refs.rs \
  crates/exec/Cargo.toml crates/exec/src/stamp.rs crates/exec/src/lib.rs \
  crates/exec/src/evaluate.rs crates/cli/src/cmd/attempt.rs crates/store/src/lib.rs
git commit -m "feat(core): every ledger entry gets an id and a time

GateRun and Attempt gain id (urn:uuid v7) and at (RFC 3339 UTC, one
fixed-width spelling, so string order is time order). Both are Option:
an entry stored before this change reads with neither and is never
published. The excerpts become nullable and an attempt's paths a list or
a count, so a copy published to a public repository reads back. fl-exec
mints ids and times; a GateReport names its run. An older fl still reads
every row this one writes. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 2: The `Decision` entry and `Ledger::flush`

**Files:**
- Create: `crates/core/src/decision.rs`
- Modify: `crates/core/src/store.rs:1-5` (imports), `:256-262` (`Ledger`), tests
- Modify: `crates/core/src/lib.rs` (module, exports)
- Modify: `crates/core/tests/wire_refs.rs` (a `Decision` sample)

**Interfaces:**
- Consumes: `At` (Task 1).
- Produces:

```rust
// fl_core::decision (all re-exported at the crate root)
pub struct TransitionOutcome { pub transition: String, pub passed: bool }
pub enum Outcome {
    Move { from: State, to: State, transitions: Vec<TransitionOutcome>, allowed: bool },
    Check { transition: TransitionOutcome },
    Reproduce { gate: GateId, accepted: bool },
    Verify { reproduction: GateId, reproduction_passed: bool, regressions: Vec<GateId>, closed: bool },
    Attempt { status: AttemptStatus },
}
pub enum DecisionKind { Move, Check, Reproduce, Verify, Attempt }   // wire_names!
pub struct Decision {
    pub id: Iri, pub at: At,
    pub record: RecordId,               // for a finding decision, the finding's record
    pub finding: Option<FindingId>,     // Some for reproduce and verify
    pub outcome: Outcome,
    pub rests_on: Vec<Iri>,             // ids of the runs or the attempt, in the order they ran
}
impl Decision { pub fn kind(&self) -> DecisionKind; }
pub struct Flushed { pub commit: Option<String>, pub left_local: Vec<LeftLocal> }
impl Flushed { pub const NOTHING: Flushed; }  // no commit, nothing reported
pub enum LeftLocal {                          // reported, never a refusal (spec §2.1)
    NoCutover,                                // the binding's GitHub ledger was never switched on
    OtherRepository { entry: Iri, record: RecordId },
}

// fl_core::store::Ledger
fn flush(&self, _decision: Decision) -> Result<Flushed, StoreError> { Ok(Flushed::NOTHING) }
```

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/decision.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::seq_iri;

    fn decision(outcome: Outcome) -> Decision {
        Decision {
            id: seq_iri(90),
            at: At::from_unix_millis(1),
            record: RecordId(seq_iri(1)),
            finding: None,
            outcome,
            rests_on: vec![seq_iri(91)],
        }
    }

    fn one_of_each() -> Vec<Outcome> {
        vec![
            Outcome::Move {
                from: State::Review,
                to: State::Done,
                transitions: vec![TransitionOutcome {
                    transition: "launch".into(),
                    passed: false,
                }],
                allowed: false,
            },
            Outcome::Check {
                transition: TransitionOutcome {
                    transition: "launch".into(),
                    passed: true,
                },
            },
            Outcome::Reproduce {
                gate: GateId(seq_iri(2)),
                accepted: true,
            },
            Outcome::Verify {
                reproduction: GateId(seq_iri(2)),
                reproduction_passed: true,
                regressions: vec![GateId(seq_iri(3))],
                closed: false,
            },
            Outcome::Attempt {
                status: AttemptStatus::Timeout,
            },
        ]
    }

    // The kind is derived, never stored beside the outcome, so the two
    // cannot disagree; and every kind has a sample here, so a kind added
    // without one fails rather than passing over nothing.
    #[test]
    fn a_decisions_kind_is_its_outcomes_tag_and_every_kind_is_sampled() {
        let mut seen = Vec::new();
        for outcome in one_of_each() {
            let d = decision(outcome);
            let json = serde_json::to_value(&d.outcome).unwrap();
            let tag = json
                .as_object()
                .and_then(|o| o.keys().next().cloned())
                .expect("an externally tagged enum");
            assert_eq!(tag, d.kind().as_wire());
            seen.push(d.kind());
        }
        for k in DecisionKind::ALL {
            assert!(seen.contains(k), "no sample of `{}`", k.as_wire());
        }
    }

    #[test]
    fn a_decision_round_trips_through_its_wire_form() {
        for outcome in one_of_each() {
            let d = decision(outcome);
            let back: Decision =
                serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
            assert_eq!(back, d);
        }
    }
}
```

Append to `crates/core/src/store.rs`'s `mod tests`:

```rust
    // Spec §1.4: a ledger with nowhere to publish — every local store —
    // answers a flush with `Nothing`, and changes nothing.
    #[test]
    fn a_local_store_has_nowhere_to_publish_so_its_flush_does_nothing() {
        use crate::at::At;
        use crate::decision::{Decision, Flushed, Outcome};
        use crate::log::AttemptStatus;
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let d = Decision {
            id: seq_iri(90),
            at: At::from_unix_millis(1),
            record: r,
            finding: None,
            outcome: Outcome::Attempt {
                status: AttemptStatus::Completed,
            },
            rests_on: vec![],
        };
        assert_eq!(s.flush(d).unwrap(), Flushed::NOTHING);
    }
```

In `crates/core/tests/wire_refs.rs`, after the `attempt` sample, add a `Decision` whose every reference is set, and its floor:

```rust
    let decision = Decision {
        id: iri(9),
        at: At::from_unix_millis(3),
        record: r.clone(),
        finding: Some(f.clone()),
        outcome: Outcome::Verify {
            reproduction: g.clone(),
            reproduction_passed: true,
            regressions: vec![g.clone()],
            closed: false,
        },
        rests_on: vec![iri(10)],
    };
```

```rust
    assert_every_reference_refuses_a_handle("Decision", &decision, 6);
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p fl-core 2>&1 | tail -20`
Expected: FAIL to compile — `Decision`, `Outcome`, `Flushed` and `Ledger::flush` do not exist.

- [ ] **Step 3: The entry, the trait method, the exports**

Put this above the test module in `crates/core/src/decision.rs`:

```rust
//! What fl decided, as the shared ledger records it (GitHub ledger spec
//! §2.3). The audit trail keeps verdicts as well as runs, and a decision
//! comment is rendered from the ledger alone (§4.2).

use crate::at::At;
use crate::ids::{FindingId, GateId, RecordId};
use crate::iri::Iri;
use crate::log::AttemptStatus;
use crate::model::State;
use serde::{Deserialize, Serialize};

/// One transition a decision evaluated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionOutcome {
    pub transition: String,
    /// `TransitionReport::passed`: false for a transition with no gates,
    /// which verified nothing.
    pub passed: bool,
}

/// What was decided, composed by the caller from the reports it already
/// holds.
///
/// ⚠ It records the decision, not whether a state change that followed it
/// completed: the flush comes BEFORE the state change (spec §2.2), so it
/// cannot know. A comment adds that line when it is posted live (§4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// `record move`. `transitions` is empty for an ungated move; `allowed`
    /// is false when any covering transition refused.
    Move {
        from: State,
        to: State,
        transitions: Vec<TransitionOutcome>,
        allowed: bool,
    },
    /// `check --record`.
    Check { transition: TransitionOutcome },
    /// `finding reproduce`: whether the gate was accepted as the
    /// reproduction.
    Reproduce { gate: GateId, accepted: bool },
    /// `finding verify`.
    Verify {
        reproduction: GateId,
        reproduction_passed: bool,
        regressions: Vec<GateId>,
        closed: bool,
    },
    /// `fl attempt`.
    Attempt { status: AttemptStatus },
}

crate::wire::wire_tags!(Outcome as outcome_wire {
    Outcome::Move { .. } => "move", Outcome::Move {
        from: State::Review,
        to: State::Done,
        transitions: vec![],
        allowed: true,
    };
    Outcome::Check { .. } => "check", Outcome::Check {
        transition: TransitionOutcome { transition: "launch".into(), passed: true },
    };
    Outcome::Reproduce { .. } => "reproduce", Outcome::Reproduce {
        gate: GateId(crate::ids::seq_iri(1)),
        accepted: true,
    };
    Outcome::Verify { .. } => "verify", Outcome::Verify {
        reproduction: GateId(crate::ids::seq_iri(1)),
        reproduction_passed: true,
        regressions: vec![],
        closed: true,
    };
    Outcome::Attempt { .. } => "attempt", Outcome::Attempt { status: AttemptStatus::Completed };
});

/// The kind of a decision (spec §2.3): `move`, `check`, `reproduce`,
/// `verify` or `attempt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Move,
    Check,
    Reproduce,
    Verify,
    Attempt,
}

crate::wire::wire_names!(DecisionKind as decision_kind_wire {
    Move => "move",
    Check => "check",
    Reproduce => "reproduce",
    Verify => "verify",
    Attempt => "attempt",
});

/// One decision, as its flush publishes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: Iri,
    pub at: At,
    /// The record the decision concerns. For `reproduce` and `verify`, the
    /// finding's record: the ledger files every decision under a record.
    pub record: RecordId,
    /// The finding, for `reproduce` and `verify`.
    pub finding: Option<FindingId>,
    pub outcome: Outcome,
    /// The ids of the runs, or of the attempt, it rests on, in the order
    /// they ran.
    pub rests_on: Vec<Iri>,
}

impl Decision {
    /// Derived from the outcome and never stored beside it, so the two
    /// cannot disagree.
    pub fn kind(&self) -> DecisionKind {
        match self.outcome {
            Outcome::Move { .. } => DecisionKind::Move,
            Outcome::Check { .. } => DecisionKind::Check,
            Outcome::Reproduce { .. } => DecisionKind::Reproduce,
            Outcome::Verify { .. } => DecisionKind::Verify,
            Outcome::Attempt { .. } => DecisionKind::Attempt,
        }
    }
}

/// What a flush did (spec §1.4), and what it left local and why (§2.1).
///
/// ⚠ A struct, not the `Nothing | Commit` pair §1.4 sketches: §2.1 says a
/// skipped entry and a missing cut-over are REPORTED, and the report has to
/// reach the command that prints it. `Flushed::NOTHING` is §1.4's
/// `Flushed::Nothing`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Flushed {
    /// The ledger commit that holds the decision, when one was made.
    pub commit: Option<String>,
    /// Entries that stayed local, and why. Reported, never a refusal.
    pub left_local: Vec<LeftLocal>,
}

impl Flushed {
    /// Nothing published and nothing to report: every local store's flush.
    pub const NOTHING: Flushed = Flushed {
        commit: None,
        left_local: Vec::new(),
    };
}

/// Why a flush left something local (GitHub ledger spec §2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeftLocal {
    /// The binding has no cut-over: its GitHub ledger was never switched on
    /// (`fl github ledger init`), so the flush published nothing at all.
    NoCutover,
    /// A pending entry tied to a record another repository owns.
    OtherRepository { entry: Iri, record: RecordId },
}
```

In `crates/core/src/store.rs`, import the new types (`:1-5`):

```rust
use crate::decision::{Decision, Flushed};
```

and give `Ledger` (`:256-262`) its flush:

```rust
/// Append-only evidence: gate runs and attempts.
pub trait Ledger {
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError>;
    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError>;
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError>;
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError>;

    /// Publish a decision and the evidence it rests on (GitHub ledger spec
    /// §1.4, §2.2).
    ///
    /// ⚠⚠ Call it BEFORE the state change the decision supports, and treat
    /// an error as a refusal of the decision: no state change. A refused
    /// decision is flushed too (decision 11).
    ///
    /// A ledger with nowhere to publish — every local store — does nothing,
    /// which is this default.
    fn flush(&self, _decision: Decision) -> Result<Flushed, StoreError> {
        Ok(Flushed::NOTHING)
    }
}
```

In `crates/core/src/lib.rs`, add `pub mod decision;` after `pub mod conformance;` and export:

```rust
pub use decision::{Decision, DecisionKind, Flushed, LeftLocal, Outcome, TransitionOutcome};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS, including the 2 `decision` tests, the generated `outcome_wire` and `decision_kind_wire` tests, `a_local_store_has_nowhere_to_publish…` and the widened `every_reference_field_on_the_wire_is_a_full_iri`.

- [ ] **Step 5: Mutation checks**

One at a time, confirm red, restore:
- Map `Outcome::Verify { .. }` to `DecisionKind::Check` in `kind()` → `a_decisions_kind_is_its_outcomes_tag…` FAILS.
- Make the default `flush` answer `Ok(Flushed { commit: Some(String::new()), left_local: vec![] })` → `a_local_store_has_nowhere_to_publish…` FAILS.
- Remove `#[serde(rename_all = "snake_case")]` from `Outcome` → the generated `outcome_wire` test FAILS (the tag would read `Move`).

- [ ] **Step 6: Trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/decision.rs crates/core/src/store.rs crates/core/src/lib.rs \
  crates/core/tests/wire_refs.rs
git commit -m "feat(core): the Decision entry and Ledger::flush

A decision records what was decided — a move, a check, a reproduction, a
verify or an attempt — with the ids of the runs or attempt it rests on;
its kind is derived from its outcome. Ledger::flush publishes it before
the state change and reports what stayed local; its default does
nothing, which is every local store.
Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 3: `SplitLedger` — cut-over, ownership, the flush, and merged reads

**Files:**
- Create: `crates/core/src/split.rs`
- Modify: `crates/core/src/store.rs:7-149` (`StoreError::Tampered`, `StoreError::CutoverChanged`)
- Modify: `crates/core/src/mem.rs:1-8` (imports), `:22-43` (`Inner`), new `Outbox` impl after `:402`, tests
- Modify: `crates/core/src/conformance.rs:14-22` (imports), new items at the end (`MemRemote`, `RemoteControl`, sample entries)
- Modify: `crates/core/src/lib.rs` (module, exports)

**Interfaces:**
- Consumes: `At`, `GateRun`/`Attempt` with ids, `PathsTouched`, `WITHHELD_ERROR_DETAIL` (Task 1); `Decision`, `Flushed`, `LeftLocal`, `Ledger::flush` (Task 2).
- Produces:

```rust
// fl_core::split (all re-exported at the crate root)
#[derive(Default)] pub struct Pending { pub runs: Vec<GateRun>, pub attempts: Vec<Attempt> }
pub trait Outbox {
    /// Entries with an id greater than `after`, tied to a record (every attempt is),
    /// not marked published to `repo`; in id order. A run with no record is never listed.
    fn unpublished(&self, repo: &str, after: &Iri) -> Result<Pending, StoreError>;
    fn is_published(&self, repo: &str, id: &Iri) -> Result<bool, StoreError>;
    fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError>;   // idempotent
    fn cutover(&self, repo: &str) -> Result<Option<Iri>, StoreError>;
    fn set_cutover(&self, repo: &str, id: &Iri) -> Result<(), StoreError>;       // once; plan B's init calls it
}
pub trait LocalLedger: Ledger + Outbox {}          // blanket impl for every Ledger + Outbox
pub struct Batch { pub decision: Decision, pub runs: Vec<GateRun>, pub attempts: Vec<Attempt> }
pub trait RemoteLedger {                             // plan B: fl_github::GithubLedger implements this
    fn repo_node_id(&self) -> &str;
    fn owns_record(&self, record: &RecordId) -> Result<bool, StoreError>;   // LOCAL: no network (Task 9's rule)
    fn publish(&self, batch: &Batch) -> Result<Option<String>, StoreError>; // the commit; returns once landed
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError>;
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError>;
}
pub enum Coverage { Complete, LocalOnly { reason: String } }
pub struct SplitLedger<'a> { pub local: &'a dyn LocalLedger, pub github: &'a dyn RemoteLedger }
impl Ledger for SplitLedger<'_> { /* all five methods */ }
impl SplitLedger<'_> {
    pub fn attempts_for_stats(&self, project: &ProjectId) -> Result<(Vec<Attempt>, Coverage), StoreError>;
}

// fl_core::store
StoreError::Tampered { id: Iri, detail: String }
StoreError::CutoverChanged { node_id: String, held: Iri, found: Iri }

// fl_core::conformance (test/`conformance` feature only)
pub struct MemRemote;                                // owns every record but its foreign ones; no tracker read
impl MemRemote {
    pub fn new(node_id: &str) -> Self;
    pub fn fail_publish(&self, on: bool);            // publish fails and nothing lands
    pub fn insert_run(&self, run: GateRun);          // a copy as GitHub holds it, no de-duplication
    pub fn insert_attempt(&self, attempt: Attempt);
    pub fn decisions(&self) -> Vec<Decision>;
}
pub trait RemoteControl {
    fn set_down(&self, down: bool);                  // every remote READ or WRITE fails; ownership is local
    fn lose_next_answer(&self);                      // the next publish lands, then its answer fails
    fn foreign_record(&self) -> RecordId;            // a record another repository owns
    fn remote(&self) -> &dyn RemoteLedger;
}
pub fn entry_iri(n: u64) -> Iri;
pub fn sample_record_run(n: u64, gate: &GateId, record: Option<&RecordId>) -> GateRun;
pub fn sample_attempt(n: u64, project: &ProjectId, record: &RecordId) -> Attempt;
pub fn sample_decision(n: u64, record: &RecordId, rests_on: Vec<Iri>) -> Decision;
```

- [ ] **Step 1: The test double and the sample entries**

These are test support, not the code under test, so they come first; they compile once Step 4's traits exist. In `crates/core/src/conformance.rs`, replace `use crate::log::GateRun;` (`:17`) with:

```rust
use crate::at::At;
use crate::decision::{Decision, Outcome, TransitionOutcome};
use crate::log::{Attempt, AttemptStatus, GateRun, PathsTouched};
use crate::split::{Batch, RemoteLedger};
use std::cell::RefCell;
use std::collections::BTreeSet;
```

and append at the end of the file:

```rust
/// An entry id no store mints: the `9` variant nibble keeps it apart from
/// [`seq_iri`]'s ids. Ordered by `n`, as UUIDv7 ids are ordered by time.
pub fn entry_iri(n: u64) -> Iri {
    Iri::parse(&format!("urn:uuid:00000000-0000-7000-9000-{n:012x}"))
        .expect("a formatted urn:uuid is a valid IRI")
}

/// A run with an id, stamped `n` milliseconds after the epoch.
pub fn sample_record_run(n: u64, gate: &GateId, record: Option<&RecordId>) -> GateRun {
    GateRun {
        id: Some(entry_iri(n)),
        at: Some(At::from_unix_millis(n)),
        gate: gate.clone(),
        record: record.cloned(),
        commit: "abc".into(),
        verdict: Verdict::from_predicate(true, 1),
        population: 1,
        output_excerpt: Some(format!("run {n}")),
        duration_ms: 1,
        cost_usd_micros: 0,
    }
}

/// An attempt with an id, stamped `n` milliseconds after the epoch.
pub fn sample_attempt(n: u64, project: &ProjectId, record: &RecordId) -> Attempt {
    Attempt {
        id: Some(entry_iri(n)),
        at: Some(At::from_unix_millis(n)),
        project: project.clone(),
        record: record.clone(),
        adapter: "claude".into(),
        status: AttemptStatus::Completed,
        duration_ms: 1,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd_micros: 0,
        paths_touched: PathsTouched::Listed(vec!["src/a.rs".into()]),
        output_excerpt: Some(format!("attempt {n}")),
    }
}

/// A `check` decision about `record`, resting on `rests_on`.
pub fn sample_decision(n: u64, record: &RecordId, rests_on: Vec<Iri>) -> Decision {
    Decision {
        id: entry_iri(1_000_000 + n),
        at: At::from_unix_millis(1_000_000 + n),
        record: record.clone(),
        finding: None,
        outcome: Outcome::Check {
            transition: TransitionOutcome {
                transition: "launch".into(),
                passed: true,
            },
        },
        rests_on,
    }
}

/// Drives a split ledger's GitHub side. `MemRemote` implements it here;
/// plan B's fixture implements it over `GithubLedger` and the fake GitHub.
pub trait RemoteControl {
    /// While down, every remote read and write fails as unreachable.
    /// Ownership is a local check (spec §2.1) and keeps answering.
    fn set_down(&self, down: bool);
    /// The next publish lands, and then its answer is lost — a timeout
    /// after the commit (spec §3.2 step 5, §8.3).
    fn lose_next_answer(&self);
    /// A record another repository owns.
    fn foreign_record(&self) -> RecordId;
    /// The remote side itself, read without the local store.
    fn remote(&self) -> &dyn RemoteLedger;
}

/// The one record [`MemRemote`] does not own.
fn foreign() -> RecordId {
    RecordId(
        Iri::parse("urn:uuid:00000000-0000-7000-f000-000000000001")
            .expect("a formatted urn:uuid is a valid IRI"),
    )
}

/// An in-memory GitHub side for [`crate::split::SplitLedger`] (GitHub
/// ledger spec §8.2). Ownership is a pure, local answer, as the spec
/// requires: it owns every record except [`RemoteControl::foreign_record`],
/// and never reads a tracker. `publish` adds only ids it does not hold, and
/// makes no commit when nothing is left.
pub struct MemRemote {
    node_id: String,
    inner: RefCell<RemoteInner>,
}

#[derive(Default)]
struct RemoteInner {
    down: bool,
    fail_publish: bool,
    lose_next_answer: bool,
    foreign: BTreeSet<RecordId>,
    runs: Vec<GateRun>,
    attempts: Vec<Attempt>,
    decisions: Vec<Decision>,
    commits: u64,
}

impl MemRemote {
    pub fn new(node_id: &str) -> Self {
        let inner = RemoteInner {
            foreign: BTreeSet::from([foreign()]),
            ..RemoteInner::default()
        };
        Self {
            node_id: node_id.to_string(),
            inner: RefCell::new(inner),
        }
    }

    /// Every `publish` fails and nothing lands, while everything else
    /// answers.
    pub fn fail_publish(&self, on: bool) {
        self.inner.borrow_mut().fail_publish = on;
    }

    /// A run as GitHub holds it — another machine's, or an altered copy —
    /// added without de-duplication.
    pub fn insert_run(&self, run: GateRun) {
        self.inner.borrow_mut().runs.push(run);
    }

    /// An attempt as GitHub holds it, added without de-duplication.
    pub fn insert_attempt(&self, attempt: Attempt) {
        self.inner.borrow_mut().attempts.push(attempt);
    }

    pub fn decisions(&self) -> Vec<Decision> {
        self.inner.borrow().decisions.clone()
    }

    fn unreachable(&self, cause: &str) -> StoreError {
        StoreError::Unreachable {
            store: format!("github ledger {}", self.node_id),
            cause: cause.to_string(),
        }
    }

    fn refuse_if_down(&self) -> Result<(), StoreError> {
        if self.inner.borrow().down {
            return Err(self.unreachable("taken down by the test"));
        }
        Ok(())
    }
}

impl RemoteLedger for MemRemote {
    fn repo_node_id(&self) -> &str {
        &self.node_id
    }

    fn owns_record(&self, record: &RecordId) -> Result<bool, StoreError> {
        Ok(!self.inner.borrow().foreign.contains(record))
    }

    fn publish(&self, batch: &Batch) -> Result<Option<String>, StoreError> {
        self.refuse_if_down()?;
        let mut s = self.inner.borrow_mut();
        if s.fail_publish {
            return Err(self.unreachable("the commit did not land"));
        }
        let mut added = 0;
        for run in &batch.runs {
            if !s.runs.iter().any(|held| held.id == run.id) {
                s.runs.push(run.clone());
                added += 1;
            }
        }
        for attempt in &batch.attempts {
            if !s.attempts.iter().any(|held| held.id == attempt.id) {
                s.attempts.push(attempt.clone());
                added += 1;
            }
        }
        if !s.decisions.iter().any(|d| d.id == batch.decision.id) {
            s.decisions.push(batch.decision.clone());
            added += 1;
        }
        let commit = if added == 0 {
            None
        } else {
            s.commits += 1;
            Some(format!("commit-{}", s.commits))
        };
        if std::mem::take(&mut s.lose_next_answer) {
            return Err(self.unreachable("the commit landed, but its answer was lost"));
        }
        Ok(commit)
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        self.refuse_if_down()?;
        Ok(self
            .inner
            .borrow()
            .runs
            .iter()
            .filter(|r| r.gate == *gate)
            .cloned()
            .collect())
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        self.refuse_if_down()?;
        Ok(self
            .inner
            .borrow()
            .attempts
            .iter()
            .filter(|a| a.project == *project)
            .cloned()
            .collect())
    }
}

impl RemoteControl for MemRemote {
    fn set_down(&self, down: bool) {
        self.inner.borrow_mut().down = down;
    }
    fn lose_next_answer(&self) {
        self.inner.borrow_mut().lose_next_answer = true;
    }
    fn foreign_record(&self) -> RecordId {
        foreign()
    }
    fn remote(&self) -> &dyn RemoteLedger {
        self
    }
}
```

- [ ] **Step 2: Write the failing tests**

Append to `crates/core/src/mem.rs`'s `mod tests` (the store side of the `Outbox` contract; `RedbStore` gets the same checks in Task 4):

```rust
    /// A project with one gate and one record.
    fn outbox_world() -> (MemStore, crate::ids::GateId, crate::ids::RecordId, ProjectId) {
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let kind = crate::model::GateKind::Command(crate::model::CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: crate::model::PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        });
        let sel = crate::model::Selector::Glob {
            pattern: "**/*".into(),
        };
        let g = s.add_gate(&p, "g", kind, sel, 1, "c", "o").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        (s, g, r, p)
    }

    // Spec §1.3, §2.1, §3.2 step 6: only an entry with an id, tied to a
    // record, after the cut-over and not yet marked can be waiting; a mark
    // is per repository; the list is in id order.
    #[test]
    fn unpublished_lists_only_record_tied_entries_after_the_cut_over_with_no_mark() {
        use crate::conformance::{entry_iri, sample_attempt, sample_record_run};
        use crate::split::Outbox;
        let (s, g, r, p) = outbox_world();
        let before = sample_record_run(2, &g, Some(&r));
        let marked = sample_record_run(4, &g, Some(&r));
        let no_record = sample_record_run(5, &g, None);
        let mut no_id = sample_record_run(6, &g, Some(&r));
        no_id.id = None;
        let late = sample_record_run(8, &g, Some(&r));
        let waiting = sample_record_run(7, &g, Some(&r));
        for run in [&before, &marked, &no_record, &no_id, &late, &waiting] {
            s.append_gate_run(run.clone()).unwrap();
        }
        let attempt = sample_attempt(9, &p, &r);
        s.append_attempt(attempt.clone()).unwrap();
        s.mark_published("R_1", &[marked.id.clone().unwrap()]).unwrap();

        let pending = s.unpublished("R_1", &entry_iri(3)).unwrap();
        assert_eq!(pending.runs, vec![waiting.clone(), late.clone()], "id order");
        assert_eq!(pending.attempts, vec![attempt]);
        assert_eq!(
            s.unpublished("R_2", &entry_iri(3)).unwrap().runs,
            vec![marked, waiting, late],
            "a mark is per repository"
        );
    }

    // Spec §2.1: the cut-over is recorded once, when the GitHub ledger is
    // switched on; moving it would strand every entry in between.
    #[test]
    fn a_cut_over_is_recorded_once_per_repository() {
        use crate::conformance::entry_iri;
        use crate::split::Outbox;
        let s = MemStore::default();
        assert_eq!(s.cutover("R_1").unwrap(), None);
        s.set_cutover("R_1", &entry_iri(3)).unwrap();
        s.set_cutover("R_1", &entry_iri(3)).unwrap();
        let err = s.set_cutover("R_1", &entry_iri(4)).unwrap_err();
        assert!(
            matches!(err, StoreError::CutoverChanged { ref held, .. } if *held == entry_iri(3)),
            "{err:?}"
        );
        assert_eq!(s.cutover("R_1").unwrap(), Some(entry_iri(3)));
        assert_eq!(s.cutover("R_2").unwrap(), None);
    }
```

Create `crates/core/src/split.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::conformance::{
        MemRemote, RemoteControl, entry_iri, sample_attempt, sample_decision, sample_record_run,
    };
    use crate::ids::seq_iri;
    use crate::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
    use crate::store::{Catalog, Tracker};

    /// A project with one gate and one record, with no cut-over recorded.
    fn bare_world() -> (MemStore, ProjectId, GateId, RecordId) {
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let kind = GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        });
        let sel = Selector::Glob {
            pattern: "**/*".into(),
        };
        let g = s.add_gate(&p, "g", kind, sel, 1, "c", "o").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        (s, p, g, r)
    }

    /// As [`bare_world`], with `R_1`'s GitHub ledger switched on before
    /// every sample entry.
    fn world() -> (MemStore, ProjectId, GateId, RecordId) {
        let w = bare_world();
        w.0.set_cutover("R_1", &entry_iri(0)).unwrap();
        w
    }

    fn published_ids(remote: &MemRemote, g: &GateId) -> Vec<Option<Iri>> {
        remote
            .gate_runs(g)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect()
    }

    // ⚠ Spec §2.5: the fields decision 2 withholds are supplied by the local
    // copy — and those are the only fields that may differ.
    #[test]
    fn a_copy_that_withholds_only_what_a_public_repository_withholds_merges_to_the_local_copy()
    {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let mut run = sample_record_run(1, &g, Some(&r));
        run.verdict = Verdict::error("could not spawn the lint program");
        l.append_gate_run(run.clone()).unwrap();
        let mut published = run.clone();
        published.output_excerpt = None;
        published.verdict = Verdict::error(WITHHELD_ERROR_DETAIL);
        remote.insert_run(published);

        assert_eq!(
            l.gate_runs(&g).unwrap(),
            vec![run],
            "one entry, and the local copy supplies what was withheld"
        );
    }

    #[test]
    fn a_copy_that_differs_in_anything_else_is_an_error_naming_the_entry() {
        let edits: [fn(&mut GateRun); 5] = [
            |r| r.population = 2,
            |r| r.commit = "def".into(),
            |r| r.verdict = Verdict::from_predicate(false, 1),
            |r| r.verdict = Verdict::error(WITHHELD_ERROR_DETAIL),
            |r| r.output_excerpt = Some("something else".into()),
        ];
        for edit in edits {
            let (s, _p, g, r) = world();
            let remote = MemRemote::new("R_1");
            let l = SplitLedger {
                local: &s,
                github: &remote,
            };
            let run = sample_record_run(1, &g, Some(&r));
            l.append_gate_run(run.clone()).unwrap();
            let mut copy = run.clone();
            edit(&mut copy);
            remote.insert_run(copy);

            let err = l.gate_runs(&g).unwrap_err();
            let id = run.id.clone().unwrap();
            assert!(
                matches!(err, StoreError::Tampered { id: ref got, .. } if *got == id),
                "{err:?}"
            );
            assert!(err.to_string().contains(id.as_str()), "{err}");
        }
    }

    // Decision 2 withholds an error's detail with ONE text. Any other text
    // on a published error is an altered copy, not a withheld one.
    #[test]
    fn a_published_error_with_any_text_but_the_withheld_one_is_an_error() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let mut run = sample_record_run(1, &g, Some(&r));
        run.verdict = Verdict::error("could not spawn the lint program");
        l.append_gate_run(run.clone()).unwrap();
        let mut copy = run.clone();
        copy.verdict = Verdict::error("spawn");
        remote.insert_run(copy);
        let err = l.gate_runs(&g).unwrap_err();
        assert!(matches!(err, StoreError::Tampered { .. }), "{err:?}");
    }

    #[test]
    fn an_attempts_path_count_merges_only_when_it_counts_the_same_paths() {
        for (count, merges) in [(1, true), (2, false)] {
            let (s, p, _g, r) = world();
            let remote = MemRemote::new("R_1");
            let l = SplitLedger {
                local: &s,
                github: &remote,
            };
            let a = sample_attempt(1, &p, &r);
            l.append_attempt(a.clone()).unwrap();
            let mut copy = a.clone();
            copy.paths_touched = PathsTouched::Counted(count);
            copy.output_excerpt = None;
            remote.insert_attempt(copy);

            match (l.attempts(&p), merges) {
                (Ok(got), true) => assert_eq!(got, vec![a]),
                (Err(StoreError::Tampered { .. }), false) => {}
                (other, _) => panic!("a count of {count} answered {other:?}"),
            }
        }
    }

    // Spec §2.5: ordered by `at`, then `id`. An entry from before ids has
    // neither, and is older than every entry that has them.
    #[test]
    fn merged_entries_are_ordered_by_time_then_id_and_entries_without_ids_come_first() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let mut legacy = sample_record_run(0, &g, Some(&r));
        legacy.id = None;
        legacy.at = None;
        let late = sample_record_run(5, &g, Some(&r));
        l.append_gate_run(legacy).unwrap();
        l.append_gate_run(late.clone()).unwrap();
        let elsewhere = sample_record_run(3, &g, Some(&r));
        let mut same_time = sample_record_run(4, &g, Some(&r));
        same_time.at = late.at.clone();
        remote.insert_run(same_time.clone());
        remote.insert_run(elsewhere.clone());

        let ids: Vec<Option<Iri>> = l.gate_runs(&g).unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![None, elsewhere.id, same_time.id, late.id]);
    }

    // ⚠ Spec §2.5 (Invariant): unreachable is not empty.
    #[test]
    fn reading_while_github_is_down_is_an_error_not_the_local_half() {
        let (s, p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        l.append_gate_run(sample_record_run(1, &g, Some(&r))).unwrap();
        l.append_attempt(sample_attempt(2, &p, &r)).unwrap();
        remote.set_down(true);
        let err = l.gate_runs(&g).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        let err = l.attempts(&p).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    #[test]
    fn stats_fall_back_to_the_local_store_and_say_why() {
        let (s, p, _g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        l.append_attempt(sample_attempt(1, &p, &r)).unwrap();
        remote.set_down(true);
        let (attempts, coverage) = l.attempts_for_stats(&p).unwrap();
        assert_eq!(attempts.len(), 1);
        assert!(
            matches!(coverage, Coverage::LocalOnly { ref reason } if reason.contains("could not be read")),
            "{coverage:?}"
        );
        // Local-only is for an unreadable GitHub, never for a project the
        // local catalog never held.
        let err = l.attempts_for_stats(&ProjectId(seq_iri(999))).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        remote.set_down(false);
        assert_eq!(l.attempts_for_stats(&p).unwrap().1, Coverage::Complete);
    }

    // Spec §2.1: a flush publishes the pending entries tied to a record this
    // binding owns, and marks them.
    #[test]
    fn a_flush_publishes_the_entries_tied_to_records_this_repository_holds_and_marks_them() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let tied = sample_record_run(1, &g, Some(&r));
        let untied = sample_record_run(2, &g, None);
        l.append_gate_run(tied.clone()).unwrap();
        l.append_gate_run(untied.clone()).unwrap();
        let flushed = l
            .flush(sample_decision(1, &r, vec![tied.id.clone().unwrap()]))
            .unwrap();
        assert!(flushed.commit.is_some(), "{flushed:?}");
        assert!(flushed.left_local.is_empty(), "{flushed:?}");
        assert_eq!(published_ids(&remote, &g), vec![tied.id.clone()]);
        assert_eq!(remote.decisions().len(), 1);
        assert!(s.is_published("R_1", tied.id.as_ref().unwrap()).unwrap());
        assert!(!s.is_published("R_1", untied.id.as_ref().unwrap()).unwrap());
    }

    // ⚠ Spec §2.1: an entry whose record another binding owns is skipped
    // and reported — never an error that blocks the decision.
    #[test]
    fn a_pending_entry_of_another_repository_is_skipped_and_reported_not_an_error() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let theirs = remote.foreign_record();
        let tied = sample_record_run(1, &g, Some(&r));
        let elsewhere = sample_record_run(2, &g, Some(&theirs));
        l.append_gate_run(tied.clone()).unwrap();
        l.append_gate_run(elsewhere.clone()).unwrap();

        let flushed = l
            .flush(sample_decision(1, &r, vec![tied.id.clone().unwrap()]))
            .unwrap();

        assert_eq!(published_ids(&remote, &g), vec![tied.id.clone()]);
        assert_eq!(
            flushed.left_local,
            vec![LeftLocal::OtherRepository {
                entry: elsewhere.id.clone().unwrap(),
                record: theirs,
            }]
        );
        assert!(!s.is_published("R_1", elsewhere.id.as_ref().unwrap()).unwrap());
    }

    // ⚠ Spec §2.1: entries recorded before the cut-over stay local; they
    // are not even scanned, so nothing reports them.
    #[test]
    fn entries_before_the_cut_over_stay_local_and_those_after_are_published() {
        let (s, _p, g, r) = bare_world();
        s.set_cutover("R_1", &entry_iri(5)).unwrap();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let before = sample_record_run(3, &g, Some(&r));
        let after = sample_record_run(7, &g, Some(&r));
        l.append_gate_run(before.clone()).unwrap();
        l.append_gate_run(after.clone()).unwrap();

        let flushed = l
            .flush(sample_decision(1, &r, vec![after.id.clone().unwrap()]))
            .unwrap();

        assert_eq!(published_ids(&remote, &g), vec![after.id.clone()]);
        assert!(flushed.left_local.is_empty(), "{flushed:?}");
        assert!(!s.is_published("R_1", before.id.as_ref().unwrap()).unwrap());
    }

    // Spec §2.1 and ruling 5: no cut-over, no GitHub ledger — the flush
    // publishes nothing, not even the decision, and says so. Not a refusal.
    #[test]
    fn with_no_cut_over_a_flush_publishes_nothing_and_says_so() {
        let (s, p, _g, r) = bare_world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        l.append_attempt(sample_attempt(1, &p, &r)).unwrap();

        let flushed = l
            .flush(sample_decision(1, &r, vec![entry_iri(1)]))
            .unwrap();

        assert_eq!(
            flushed,
            Flushed {
                commit: None,
                left_local: vec![LeftLocal::NoCutover],
            }
        );
        assert!(remote.decisions().is_empty());
        assert!(remote.attempts(&p).unwrap().is_empty());
        assert!(!s.is_published("R_1", &entry_iri(1)).unwrap());
    }

    // Spec §1.3: an entry from before ids is never published.
    #[test]
    fn an_entry_without_an_id_is_never_published() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let mut legacy = sample_record_run(1, &g, Some(&r));
        legacy.id = None;
        l.append_gate_run(legacy).unwrap();
        l.flush(sample_decision(1, &r, vec![])).unwrap();
        assert!(remote.gate_runs(&g).unwrap().is_empty());
    }

    // ⚠ Spec §2.2 and decision 8: a flush that fails publishes nothing and
    // marks nothing, and the next flush that succeeds carries what was left.
    #[test]
    fn a_flush_that_fails_publishes_and_marks_nothing_and_the_next_one_carries_it() {
        let (s, p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let run = sample_record_run(1, &g, Some(&r));
        let attempt = sample_attempt(2, &p, &r);
        l.append_gate_run(run.clone()).unwrap();
        l.append_attempt(attempt.clone()).unwrap();

        remote.fail_publish(true);
        let refused = l.flush(sample_decision(1, &r, vec![attempt.id.clone().unwrap()]));
        assert!(refused.is_err(), "{refused:?}");
        assert!(!s.is_published("R_1", run.id.as_ref().unwrap()).unwrap());
        assert!(!s.is_published("R_1", attempt.id.as_ref().unwrap()).unwrap());

        remote.fail_publish(false);
        l.flush(sample_decision(2, &r, vec![run.id.clone().unwrap()]))
            .unwrap();
        assert_eq!(
            remote.attempts(&p).unwrap().len(),
            1,
            "the attempt left behind is published by the next flush"
        );
        assert!(s.is_published("R_1", run.id.as_ref().unwrap()).unwrap());
        assert!(s.is_published("R_1", attempt.id.as_ref().unwrap()).unwrap());
    }

    #[test]
    fn a_decision_resting_on_an_entry_that_is_not_being_published_is_refused_before_anything_is_sent()
    {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let untied = sample_record_run(1, &g, None);
        l.append_gate_run(untied.clone()).unwrap();
        let err = l
            .flush(sample_decision(1, &r, vec![untied.id.clone().unwrap()]))
            .unwrap_err();
        assert!(
            err.to_string().contains(untied.id.as_ref().unwrap().as_str()),
            "{err}"
        );
        assert!(remote.decisions().is_empty(), "nothing was sent");
    }

    // Ruling 6: the decision's OWN record must be this repository's.
    #[test]
    fn a_decision_about_a_record_this_repository_does_not_hold_is_refused() {
        let (s, _p, _g, _r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let err = l
            .flush(sample_decision(1, &remote.foreign_record(), vec![]))
            .unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        assert!(remote.decisions().is_empty());
    }
}
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test -p fl-core 2>&1 | tail -20`
Expected: FAIL to compile — `SplitLedger`, `Coverage`, `Outbox`, `StoreError::Tampered`/`CutoverChanged` and `crate::split` do not exist. (Add `pub mod split;` to `lib.rs` first, so the failure is the missing items and not a missing module.)

- [ ] **Step 4: The errors, `Outbox` for `MemStore`, and `split.rs`**

In `crates/core/src/store.rs`, add to `StoreError` after `Conflict` (`:112`):

```rust
    /// ⚠ One entry id, two contents: this machine's copy and the shared
    /// ledger's (GitHub ledger spec §2.5). An entry never changes once it is
    /// written, so one of the two was altered afterwards.
    #[error(
        "{id} is recorded differently on this machine and in the shared ledger: {detail}. An \
         entry never changes once it is written, so one copy was altered afterwards, and fl \
         uses neither. Find out who changed it before trusting either copy."
    )]
    Tampered { id: Iri, detail: String },
    /// ⚠ The cut-over is recorded once, when a repository's GitHub ledger
    /// is switched on (spec §2.1): moving it would strand every entry
    /// recorded between the two.
    #[error(
        "the GitHub ledger of repository node {node_id} was switched on at {held}, and this \
         names {found}. The cut-over never moves: entries between the two would never be \
         published. Keep the recorded one."
    )]
    CutoverChanged {
        node_id: String,
        held: Iri,
        found: Iri,
    },
```

In `crates/core/src/mem.rs`, import `use crate::split::{Outbox, Pending};` and change `use std::collections::BTreeMap;` to `use std::collections::{BTreeMap, BTreeSet};`. Add to `Inner` (`:22-43`):

```rust
    /// (repository `node_id`, entry id) for every entry marked published.
    published: BTreeSet<(String, Iri)>,
    /// repository `node_id` → the id after which entries are publishable.
    cutovers: BTreeMap<String, Iri>,
```

and after `impl Ledger for MemStore` (`:374-402`):

```rust
impl Outbox for MemStore {
    fn unpublished(&self, repo: &str, after: &Iri) -> Result<Pending, StoreError> {
        let s = self.inner.borrow();
        let waiting = |id: &Option<Iri>| {
            id.as_ref().is_some_and(|id| {
                id > after && !s.published.contains(&(repo.to_string(), id.clone()))
            })
        };
        let mut runs: Vec<GateRun> = s
            .runs
            .iter()
            .filter(|r| r.record.is_some() && waiting(&r.id))
            .cloned()
            .collect();
        let mut attempts: Vec<Attempt> = s
            .attempts
            .iter()
            .filter(|a| waiting(&a.id))
            .cloned()
            .collect();
        runs.sort_by(|a, b| a.id.cmp(&b.id));
        attempts.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Pending { runs, attempts })
    }

    fn is_published(&self, repo: &str, id: &Iri) -> Result<bool, StoreError> {
        Ok(self
            .inner
            .borrow()
            .published
            .contains(&(repo.to_string(), id.clone())))
    }

    fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        for id in ids {
            s.published.insert((repo.to_string(), id.clone()));
        }
        Ok(())
    }

    fn cutover(&self, repo: &str) -> Result<Option<Iri>, StoreError> {
        Ok(self.inner.borrow().cutovers.get(repo).cloned())
    }

    fn set_cutover(&self, repo: &str, id: &Iri) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        match s.cutovers.get(repo) {
            Some(held) if held == id => Ok(()),
            Some(held) => Err(StoreError::CutoverChanged {
                node_id: repo.to_string(),
                held: held.clone(),
                found: id.clone(),
            }),
            None => {
                s.cutovers.insert(repo.to_string(), id.clone());
                Ok(())
            }
        }
    }
}
```

Put this above the test module in `crates/core/src/split.rs`:

```rust
//! Mode B's ledger (GitHub ledger spec §1.2, §2): every entry goes to the
//! local store at once, and at each decision's flush the entries tied to a
//! record this binding owns, recorded after its cut-over, go to GitHub with
//! the decision.
//!
//! The GitHub side is a [`RemoteLedger`]: `fl_github::GithubLedger`
//! implements it (plan B), and `conformance::MemRemote` is the in-memory
//! double every test here uses.

use crate::at::At;
use crate::decision::{Decision, Flushed, LeftLocal};
use crate::ids::{GateId, ProjectId, RecordId};
use crate::iri::Iri;
use crate::log::{Attempt, GateRun, PathsTouched, WITHHELD_ERROR_DETAIL};
use crate::store::{Ledger, StoreError};
use crate::verdict::Verdict;
use std::collections::BTreeMap;

/// Local entries waiting to be published to one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pending {
    pub runs: Vec<GateRun>,
    pub attempts: Vec<Attempt>,
}

/// The local store's half of publishing (spec §2.1, §3.2 step 6), keyed by
/// the repository's `node_id`: its cut-over, and what it already has.
pub trait Outbox {
    /// Every entry with an id greater than `after`, tied to a record (every
    /// attempt is), and not marked published to `repo` — in id order. A
    /// run with no record is never listed (spec §2.1), nor is an entry with
    /// no id (§1.3).
    fn unpublished(&self, repo: &str, after: &Iri) -> Result<Pending, StoreError>;
    fn is_published(&self, repo: &str, id: &Iri) -> Result<bool, StoreError>;
    /// Idempotent: marking an id twice is not an error.
    fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError>;
    /// The id after which entries are publishable to `repo`, if its GitHub
    /// ledger was switched on.
    fn cutover(&self, repo: &str) -> Result<Option<Iri>, StoreError>;
    /// Recorded once, by `fl github ledger init` (plan B). ⚠ The same id
    /// again is a no-op; a different one is `CutoverChanged`.
    fn set_cutover(&self, repo: &str, id: &Iri) -> Result<(), StoreError>;
}

/// What a split ledger's local side must be.
pub trait LocalLedger: Ledger + Outbox {}
impl<T: Ledger + Outbox> LocalLedger for T {}

/// One flush: the decision and every entry it publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub decision: Decision,
    pub runs: Vec<GateRun>,
    pub attempts: Vec<Attempt>,
}

/// The GitHub side of mode B.
pub trait RemoteLedger {
    /// The repository's `node_id`: the key of the local cut-over and marks.
    fn repo_node_id(&self) -> &str;
    /// Whether `record` belongs to this binding's repository.
    ///
    /// ⚠ A LOCAL answer, from the record's IRI and what the local store
    /// remembers — never a network call (spec §2.1). `GithubLedger` answers
    /// with `fl_github::owner::issue_of_repository` (Task 9).
    fn owns_record(&self, record: &RecordId) -> Result<bool, StoreError>;
    /// Append `batch` in one commit, and return only once it has landed:
    /// the commit, or `None` when nothing was left to add. An entry whose
    /// id is already there is not added twice.
    fn publish(&self, batch: &Batch) -> Result<Option<String>, StoreError>;
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError>;
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError>;
}

/// How much of the ledger a report covers (spec §2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    /// The local store and GitHub, merged.
    Complete,
    /// The local store only, and why.
    LocalOnly { reason: String },
}

/// Mode B's `Ledger`: `local` keeps every entry, `github` holds what the
/// decisions published.
pub struct SplitLedger<'a> {
    pub local: &'a dyn LocalLedger,
    pub github: &'a dyn RemoteLedger,
}

/// What the merge needs of an entry.
trait Entry: Clone + PartialEq {
    fn entry_id(&self) -> Option<&Iri>;
    fn entry_at(&self) -> Option<&At>;
    /// `self` with each field decision 2 withholds taken from `published`
    /// wherever `published` holds the withheld form. Equal to `published`
    /// exactly when the two copies agree.
    fn as_published(&self, published: &Self) -> Self;
}

impl Entry for GateRun {
    fn entry_id(&self) -> Option<&Iri> {
        self.id.as_ref()
    }
    fn entry_at(&self) -> Option<&At> {
        self.at.as_ref()
    }
    fn as_published(&self, published: &Self) -> Self {
        let mut mine = self.clone();
        if published.output_excerpt.is_none() {
            mine.output_excerpt = None;
        }
        // An error's detail is withheld with ONE text (decision 2); any
        // other text on a published error is an altered copy.
        if let (Verdict::Error { .. }, Verdict::Error { detail, .. }) =
            (&mine.verdict, &published.verdict)
            && detail == WITHHELD_ERROR_DETAIL
        {
            mine.verdict = published.verdict.clone();
        }
        mine
    }
}

impl Entry for Attempt {
    fn entry_id(&self) -> Option<&Iri> {
        self.id.as_ref()
    }
    fn entry_at(&self) -> Option<&At> {
        self.at.as_ref()
    }
    fn as_published(&self, published: &Self) -> Self {
        let mut mine = self.clone();
        if published.output_excerpt.is_none() {
            mine.output_excerpt = None;
        }
        if let PathsTouched::Counted(n) = published.paths_touched
            && mine.paths_touched.count() == n
        {
            mine.paths_touched = PathsTouched::Counted(n);
        }
        mine
    }
}

/// Local and published entries, de-duplicated by id and ordered by `at`,
/// then id (spec §2.5). An entry with no id is local, from before ids, and
/// comes first.
///
/// ⚠ The same id with different content is `Tampered`, except in the
/// fields decision 2 withholds, which the local copy supplies. A published
/// entry with no id is damage, never a legacy entry.
fn merge<T: Entry>(local: Vec<T>, remote: Vec<T>) -> Result<Vec<T>, StoreError> {
    let mut out: Vec<T> = Vec::with_capacity(local.len() + remote.len());
    let mut seen: BTreeMap<Iri, usize> = BTreeMap::new();
    for e in local {
        if let Some(id) = e.entry_id() {
            seen.insert(id.clone(), out.len());
        }
        out.push(e);
    }
    for e in remote {
        let Some(id) = e.entry_id().cloned() else {
            return Err(StoreError::Backend(
                "the shared ledger holds an entry with no id, and every published entry carries \
                 one"
                .into(),
            ));
        };
        match seen.get(&id) {
            Some(&i) => {
                if out[i].as_published(&e) != e {
                    return Err(StoreError::Tampered {
                        id,
                        detail: "the two copies differ in more than the fields a repository \
                                 that is not private withholds"
                            .into(),
                    });
                }
            }
            None => {
                seen.insert(id, out.len());
                out.push(e);
            }
        }
    }
    out.sort_by(|a, b| (a.entry_at(), a.entry_id()).cmp(&(b.entry_at(), b.entry_id())));
    Ok(out)
}

impl SplitLedger<'_> {
    /// `attempts` for `fl stats` (spec §2.5): when GitHub cannot be read,
    /// the local store's attempts and a coverage that says so — never an
    /// error, and never a short count that reads as the total.
    pub fn attempts_for_stats(
        &self,
        project: &ProjectId,
    ) -> Result<(Vec<Attempt>, Coverage), StoreError> {
        let local = self.local.attempts(project)?;
        match self.github.attempts(project) {
            Ok(remote) => Ok((merge(local, remote)?, Coverage::Complete)),
            Err(e) => Ok((
                local,
                Coverage::LocalOnly {
                    reason: format!("GitHub could not be read: {e}"),
                },
            )),
        }
    }
}

impl Ledger for SplitLedger<'_> {
    /// Local, at once (spec §2.1): the local store keeps every run.
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError> {
        self.local.append_gate_run(run)
    }

    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError> {
        self.local.append_attempt(attempt)
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        // Local first: a gate the local catalog never held is `NotOwned`
        // (spec §8.2), whatever GitHub holds.
        let local = self.local.gate_runs(gate)?;
        // ⚠ Unreachable is not empty (spec §2.5, Invariant).
        let remote = self.github.gate_runs(gate)?;
        merge(local, remote)
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        let local = self.local.attempts(project)?;
        let remote = self.github.attempts(project)?;
        merge(local, remote)
    }

    /// ⚠⚠ Returns only after the commit has landed (spec §2.2).
    ///
    /// Before anything is sent: a decision about a record this repository
    /// does not hold is `NotOwned`; with no cut-over, nothing is published
    /// and `LeftLocal::NoCutover` says so; a pending entry of another
    /// repository is skipped and reported; a decision resting on an entry
    /// that is neither being published nor published is refused.
    fn flush(&self, decision: Decision) -> Result<Flushed, StoreError> {
        let repo = self.github.repo_node_id().to_string();
        if !self.github.owns_record(&decision.record)? {
            return Err(StoreError::NotOwned {
                id: decision.record.iri().clone(),
                searched: vec![format!(
                    "the repository this ledger publishes to (node {repo})"
                )],
            });
        }
        let Some(cutover) = self.local.cutover(&repo)? else {
            return Ok(Flushed {
                commit: None,
                left_local: vec![LeftLocal::NoCutover],
            });
        };
        let pending = self.local.unpublished(&repo, &cutover)?;
        let mut left_local = Vec::new();
        let mut runs = Vec::new();
        for run in pending.runs {
            // `Outbox` lists only runs tied to a record, each with an id.
            let (Some(record), Some(id)) = (run.record.clone(), run.id.clone()) else {
                continue;
            };
            if self.github.owns_record(&record)? {
                runs.push(run);
            } else {
                left_local.push(LeftLocal::OtherRepository { entry: id, record });
            }
        }
        let mut attempts = Vec::new();
        for a in pending.attempts {
            let Some(id) = a.id.clone() else {
                continue;
            };
            if self.github.owns_record(&a.record)? {
                attempts.push(a);
            } else {
                left_local.push(LeftLocal::OtherRepository {
                    entry: id,
                    record: a.record.clone(),
                });
            }
        }
        let ids: Vec<Iri> = runs
            .iter()
            .filter_map(|r| r.id.clone())
            .chain(attempts.iter().filter_map(|a| a.id.clone()))
            .collect();
        for cited in &decision.rests_on {
            if !ids.contains(cited) && !self.local.is_published(&repo, cited)? {
                return Err(StoreError::Backend(format!(
                    "decision {} rests on {cited}, which is neither being published nor \
                     published. A run tied to no record, one recorded before the GitHub ledger \
                     was switched on, or one tied to another repository's record stays local, \
                     so a decision cannot rest on it. Nothing was published.",
                    decision.id
                )));
            }
        }
        let commit = self.github.publish(&Batch {
            decision,
            runs,
            attempts,
        })?;
        // Only after the commit landed: a mark written first would hide an
        // entry GitHub never received.
        self.local.mark_published(&repo, &ids)?;
        Ok(Flushed { commit, left_local })
    }
}
```

In `crates/core/src/lib.rs`, add `pub mod split;` after `pub mod stale;` and export:

```rust
pub use split::{
    Batch, Coverage, LocalLedger, Outbox, Pending, RemoteLedger, SplitLedger,
};
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p fl-core`
Expected: PASS, including the 15 `split` tests and the two new `mem` tests. Then `cargo test --workspace`: PASS.

- [ ] **Step 6: Mutation checks**

One at a time, confirm red, restore:
- In `GateRun::as_published`, delete the `output_excerpt` line → `a_copy_that_withholds_only…` FAILS (`Tampered`).
- In `GateRun::as_published`, drop `&& detail == WITHHELD_ERROR_DETAIL` → `a_published_error_with_any_text_but_the_withheld_one…` FAILS.
- In `GateRun::as_published`, take the published verdict whatever the variants → `a_copy_that_differs_in_anything_else…` FAILS on the `from_predicate(false, 1)` and the withheld-error edits.
- In `Attempt::as_published`, drop `&& mine.paths_touched.count() == n` → `an_attempts_path_count_merges_only…` FAILS at count 2.
- Delete the `sort_by` in `merge` → `merged_entries_are_ordered…` FAILS.
- In `gate_runs`, use `self.github.gate_runs(gate).unwrap_or_default()` → `reading_while_github_is_down…` FAILS.
- In `attempts_for_stats`, propagate the remote error with `?` → `stats_fall_back…` FAILS.
- In `flush`, push a foreign run into `runs` instead of `left_local` → `a_pending_entry_of_another_repository…` FAILS.
- In `flush`, return `Err(…)` for a foreign pending entry → `a_pending_entry_of_another_repository…` FAILS.
- In `flush`, treat a missing cut-over as `entry_iri`-zero (publish everything) → `with_no_cut_over_a_flush_publishes_nothing…` FAILS.
- In `MemStore::unpublished`, drop `id > after` → `entries_before_the_cut_over_stay_local…` and `unpublished_lists_only…` FAIL.
- In `MemStore::unpublished`, drop `r.record.is_some()` → `unpublished_lists_only…` FAILS.
- In `MemStore::unpublished`, drop the `published` test → `unpublished_lists_only…` FAILS.
- In `MemStore::set_cutover`, overwrite a held cut-over → `a_cut_over_is_recorded_once…` FAILS.
- Move `mark_published` above `publish` → `a_flush_that_fails_publishes_and_marks_nothing…` FAILS.
- Delete the `rests_on` loop → `a_decision_resting_on_an_entry_that_is_not_being_published…` FAILS.
- Delete the `owns_record(&decision.record)` check → `a_decision_about_a_record…` FAILS.

- [ ] **Step 7: Trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/split.rs crates/core/src/store.rs crates/core/src/mem.rs \
  crates/core/src/conformance.rs crates/core/src/lib.rs
git commit -m "feat(core): SplitLedger — local always, GitHub at the flush

SplitLedger routes every append to the local store and, at a decision's
flush, publishes the entries recorded after the repository's cut-over
and tied to a record it owns — a local check — then marks them, only
after the commit landed. An entry of another repository is skipped and
reported; with no cut-over nothing is published and the flush says so.
Reads merge both sides by id, ordered by time then id; the same id with
different content is Tampered except in what decision 2 withholds; an
unreadable GitHub is an error, and only fl stats falls back to the local
store, saying so. MemRemote is RemoteLedger's in-memory double. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 4: The ledger contract, shared by every ledger, and `Outbox` for `RedbStore`

**Files:**
- Modify: `crates/core/src/conformance.rs:100-110` (case counts), `:138-141` (`ledger`), `:518-537` (the one ledger case), new suite, fixture and cases
- Modify: `crates/core/src/mem.rs:440-448` (the contract test)
- Modify: `crates/store/src/lib.rs:8` (imports), `:36-47` (new tables), `:318-341` (`append_json`), `:945-955` (`append_*`), new `Outbox` impl, tests `:1493-1504` and new tests

**Interfaces:**
- Consumes: `SplitLedger`, `Outbox`, `RemoteLedger`, `MemRemote`, `RemoteControl`, the sample entries (Task 3).
- Produces:

```rust
// fl_core::conformance
pub fn ledger<F: Fixture>(make: impl Fn() -> F);           // was (S, G)-based; now Bound-based, 4 cases
pub trait SplitFixture {
    fn with_split(&self, f: &mut dyn FnMut(&Bound<'_>, &dyn RemoteControl));
}
pub fn split_ledger<F: SplitFixture>(make: impl Fn() -> F); // 8 cases
pub struct SplitOver<S, G>(pub S, pub G);                   // SplitLedger over S + MemRemote("R_1"), cut-over recorded
// plan B writes its own SplitFixture over GithubLedger and the fake GitHub

// fl_store
impl fl_core::split::Outbox for RedbStore { /* candidate index, marks, cut-overs: additive tables */ }
```

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/conformance.rs`, change the count and the suite (`:105`, `:138-141`):

```rust
/// How many cases [`ledger`] runs. Update deliberately — see [`run_suite`].
const LEDGER_CASES: usize = 4;
/// How many cases [`split_ledger`] runs. Update deliberately — see [`run_suite`].
const SPLIT_LEDGER_CASES: usize = 8;
```

```rust
/// The contract every `Ledger` meets: the local stores, and a split ledger
/// over them. Bound-based, so a split binding runs it too.
pub fn ledger<F: Fixture>(make: impl Fn() -> F) {
    let cases: &[fn(&Bound<'_>)] = &[
        the_logs_are_append_only_and_read_back_in_order,
        attempts_are_read_back_per_project,
        a_flush_with_nothing_waiting_still_answers,
        a_run_tied_to_a_record_reads_back_once_after_its_flush,
    ];
    run_bound("ledger", LEDGER_CASES, cases, make);
}

/// What only a split ledger has (GitHub ledger spec §8.2, §8.3): runs tied
/// to a record, the flush, merge by id, the local store succeeding while
/// GitHub fails, a lost answer retried without a duplicate, another
/// repository's entry skipped without blocking, and `NotOwned` from the
/// local catalog.
pub fn split_ledger<F: SplitFixture>(make: impl Fn() -> F) {
    let cases: &[fn(&Bound<'_>, &dyn RemoteControl)] = &[
        runs_tied_to_a_record_reach_github_at_the_flush_and_not_before,
        a_run_tied_to_no_record_is_never_published,
        a_second_flush_publishes_nothing_twice_and_reads_see_each_run_once,
        while_github_is_down_appends_succeed_and_flushes_and_reads_are_refused_until_it_returns,
        another_machines_run_is_read_back_beside_this_ones,
        a_gate_the_local_catalog_never_held_is_not_owned_whatever_github_holds,
        a_commit_whose_answer_was_lost_is_not_duplicated_by_the_next_flush,
        a_pending_entry_of_another_repository_does_not_block_the_decision,
    ];
    assert_eq!(
        cases.len(),
        SPLIT_LEDGER_CASES,
        "the split-ledger suite lists {} cases but declares {SPLIT_LEDGER_CASES}. A case was \
         added or removed: if that was deliberate, update the count beside the list; if not, \
         restore the case",
        cases.len()
    );
    for case in cases {
        let fixture = make();
        fixture.with_split(&mut |b, c| case(b, c));
    }
}
```

Rewrite the one existing ledger case (`:518-537`) for a `Bound`, and add the new cases after it:

```rust
fn the_logs_are_append_only_and_read_back_in_order(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/tmp/p").unwrap();
    let g = roles
        .catalog
        .add_gate(
            &p,
            "fmt",
            sample_kind(),
            sample_selector(),
            1,
            "abc",
            "owner",
        )
        .unwrap();
    roles
        .ledger
        .append_gate_run(sample_run(g.clone(), "abc", 3))
        .unwrap();
    roles
        .ledger
        .append_gate_run(sample_run(g.clone(), "def", 5))
        .unwrap();
    let runs = roles.ledger.gate_runs(&g).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].commit, "abc");
    assert_eq!(runs[1].population, 5);
}

fn attempts_are_read_back_per_project(roles: &Bound<'_>) {
    let p1 = roles.catalog.add_project("/p1").unwrap();
    let p2 = roles.catalog.add_project("/p2").unwrap();
    let r1 = roles.tracker.add_record(&p1, "a").unwrap();
    let r2 = roles.tracker.add_record(&p2, "b").unwrap();
    for a in [
        sample_attempt(1, &p1, &r1),
        sample_attempt(2, &p2, &r2),
        sample_attempt(3, &p1, &r1),
    ] {
        roles.ledger.append_attempt(a).unwrap();
    }
    let ids = |p: &ProjectId| -> Vec<Option<Iri>> {
        roles
            .ledger
            .attempts(p)
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect()
    };
    assert_eq!(ids(&p1), vec![Some(entry_iri(1)), Some(entry_iri(3))]);
    assert_eq!(ids(&p2), vec![Some(entry_iri(2))]);
}

fn a_flush_with_nothing_waiting_still_answers(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![]))
        .expect("a flush with nothing waiting is not an error");
}

/// A project with one gate and one record.
fn record_world(roles: &Bound<'_>) -> (ProjectId, GateId, RecordId) {
    let p = roles.catalog.add_project("/p").unwrap();
    let g = roles
        .catalog
        .add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o")
        .unwrap();
    let r = roles.tracker.add_record(&p, "t").unwrap();
    (p, g, r)
}

fn run_ids(runs: Vec<GateRun>) -> Vec<Option<Iri>> {
    runs.into_iter().map(|r| r.id).collect()
}

fn a_run_tied_to_a_record_reads_back_once_after_its_flush(roles: &Bound<'_>) {
    let (_p, g, r) = record_world(roles);
    let run = sample_record_run(1, &g, Some(&r));
    let id = run.id.clone().unwrap();
    roles.ledger.append_gate_run(run).unwrap();
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![id.clone()]))
        .unwrap();
    assert_eq!(run_ids(roles.ledger.gate_runs(&g).unwrap()), vec![Some(id)]);
}

fn runs_tied_to_a_record_reach_github_at_the_flush_and_not_before(
    roles: &Bound<'_>,
    ctl: &dyn RemoteControl,
) {
    let (_p, g, r) = record_world(roles);
    let run = sample_record_run(1, &g, Some(&r));
    roles.ledger.append_gate_run(run.clone()).unwrap();
    assert!(
        ctl.remote().gate_runs(&g).unwrap().is_empty(),
        "an append publishes nothing"
    );
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![run.id.clone().unwrap()]))
        .unwrap();
    assert_eq!(run_ids(ctl.remote().gate_runs(&g).unwrap()), vec![run.id]);
}

fn a_run_tied_to_no_record_is_never_published(roles: &Bound<'_>, ctl: &dyn RemoteControl) {
    let (_p, g, r) = record_world(roles);
    let untied = sample_record_run(1, &g, None);
    let tied = sample_record_run(2, &g, Some(&r));
    roles.ledger.append_gate_run(untied).unwrap();
    roles.ledger.append_gate_run(tied.clone()).unwrap();
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![tied.id.clone().unwrap()]))
        .unwrap();
    assert_eq!(run_ids(ctl.remote().gate_runs(&g).unwrap()), vec![tied.id]);
}

fn a_second_flush_publishes_nothing_twice_and_reads_see_each_run_once(
    roles: &Bound<'_>,
    ctl: &dyn RemoteControl,
) {
    let (_p, g, r) = record_world(roles);
    let run = sample_record_run(1, &g, Some(&r));
    let id = run.id.clone().unwrap();
    roles.ledger.append_gate_run(run).unwrap();
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![id.clone()]))
        .unwrap();
    // The second decision rests on a run that is already published.
    roles
        .ledger
        .flush(sample_decision(2, &r, vec![id.clone()]))
        .unwrap();
    assert_eq!(
        run_ids(ctl.remote().gate_runs(&g).unwrap()),
        vec![Some(id.clone())]
    );
    assert_eq!(run_ids(roles.ledger.gate_runs(&g).unwrap()), vec![Some(id)]);
}

// ⚠ Spec §2.2 and decision 8: the local store keeps every entry while
// GitHub is down; the decision is refused; reads are errors, not the local
// half; and the next flush publishes what was left behind.
fn while_github_is_down_appends_succeed_and_flushes_and_reads_are_refused_until_it_returns(
    roles: &Bound<'_>,
    ctl: &dyn RemoteControl,
) {
    let (p, g, r) = record_world(roles);
    let run = sample_record_run(1, &g, Some(&r));
    let attempt = sample_attempt(2, &p, &r);
    ctl.set_down(true);
    roles.ledger.append_gate_run(run.clone()).unwrap();
    roles.ledger.append_attempt(attempt.clone()).unwrap();
    assert!(
        roles
            .ledger
            .flush(sample_decision(1, &r, vec![attempt.id.clone().unwrap()]))
            .is_err(),
        "a flush GitHub cannot take refuses the decision"
    );
    assert!(
        roles.ledger.gate_runs(&g).is_err(),
        "unreachable is not empty"
    );
    ctl.set_down(false);
    assert!(ctl.remote().gate_runs(&g).unwrap().is_empty());
    roles
        .ledger
        .flush(sample_decision(2, &r, vec![run.id.clone().unwrap()]))
        .unwrap();
    assert_eq!(run_ids(ctl.remote().gate_runs(&g).unwrap()), vec![run.id]);
    assert_eq!(
        ctl.remote()
            .attempts(&p)
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect::<Vec<_>>(),
        vec![attempt.id],
        "the attempt left behind goes out with the next flush"
    );
}

fn another_machines_run_is_read_back_beside_this_ones(roles: &Bound<'_>, ctl: &dyn RemoteControl) {
    let (_p, g, r) = record_world(roles);
    let mine = sample_record_run(1, &g, Some(&r));
    roles.ledger.append_gate_run(mine.clone()).unwrap();
    let theirs = sample_record_run(2, &g, Some(&r));
    ctl.remote()
        .publish(&Batch {
            decision: sample_decision(9, &r, vec![theirs.id.clone().unwrap()]),
            runs: vec![theirs.clone()],
            attempts: vec![],
        })
        .unwrap();
    assert_eq!(
        run_ids(roles.ledger.gate_runs(&g).unwrap()),
        vec![mine.id, theirs.id]
    );
}

fn a_gate_the_local_catalog_never_held_is_not_owned_whatever_github_holds(
    roles: &Bound<'_>,
    _ctl: &dyn RemoteControl,
) {
    let err = roles.ledger.gate_runs(&GateId(stranger())).unwrap_err();
    assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
}

// ⚠ Spec §3.2 step 5 and §8.3: a commit that landed before its answer was
// lost reads, to fl, as a failure; the retry must not add a second copy.
fn a_commit_whose_answer_was_lost_is_not_duplicated_by_the_next_flush(
    roles: &Bound<'_>,
    ctl: &dyn RemoteControl,
) {
    let (_p, g, r) = record_world(roles);
    let run = sample_record_run(1, &g, Some(&r));
    let id = run.id.clone().unwrap();
    roles.ledger.append_gate_run(run).unwrap();
    ctl.lose_next_answer();
    assert!(
        roles
            .ledger
            .flush(sample_decision(1, &r, vec![id.clone()]))
            .is_err(),
        "an answer that never came is not a landed commit, as far as fl can tell"
    );
    let flushed = roles
        .ledger
        .flush(sample_decision(2, &r, vec![id.clone()]))
        .unwrap();
    assert!(flushed.commit.is_some(), "the new decision is new: a commit");
    assert_eq!(
        run_ids(ctl.remote().gate_runs(&g).unwrap()),
        vec![Some(id.clone())],
        "the retry added no second copy"
    );
    assert_eq!(run_ids(roles.ledger.gate_runs(&g).unwrap()), vec![Some(id)]);
}

// ⚠ Spec §2.1: skipped and reported, never an error that blocks.
fn a_pending_entry_of_another_repository_does_not_block_the_decision(
    roles: &Bound<'_>,
    ctl: &dyn RemoteControl,
) {
    let (_p, g, r) = record_world(roles);
    let theirs = ctl.foreign_record();
    let mine = sample_record_run(1, &g, Some(&r));
    let other = sample_record_run(2, &g, Some(&theirs));
    roles.ledger.append_gate_run(mine.clone()).unwrap();
    roles.ledger.append_gate_run(other.clone()).unwrap();
    let flushed = roles
        .ledger
        .flush(sample_decision(1, &r, vec![mine.id.clone().unwrap()]))
        .unwrap();
    assert_eq!(
        flushed.left_local,
        vec![LeftLocal::OtherRepository {
            entry: other.id.clone().unwrap(),
            record: theirs,
        }]
    );
    assert_eq!(run_ids(ctl.remote().gate_runs(&g).unwrap()), vec![mine.id]);
}
```

In `crates/core/src/mem.rs`, the contract test (`:440-448`) runs the ledger suite over a `MemStore`, and a new test runs both suites over a split ledger over one:

```rust
    #[test]
    fn mem_store_meets_every_role_contract() {
        use crate::conformance::Single;
        crate::conformance::catalog(|| (MemStore::default(), ()));
        crate::conformance::tracker(|| Single(MemStore::default(), ()));
        crate::conformance::ledger(|| Single(MemStore::default(), ()));
        crate::conformance::all_roles(|| Single(MemStore::default(), ()));
        crate::conformance::local_handles(|| (MemStore::default(), ()));
    }

    #[test]
    fn a_split_ledger_over_a_mem_store_meets_the_ledger_contracts() {
        use crate::conformance::SplitOver;
        crate::conformance::ledger(|| SplitOver(MemStore::default(), ()));
        crate::conformance::split_ledger(|| SplitOver(MemStore::default(), ()));
    }
```

In `crates/store/src/lib.rs`, the contract test (`:1493-1504`) and new tests (the tests module gains `use fl_core::conformance::{entry_iri, sample_record_run};`; `Outbox` reaches it through `use super::*`):

```rust
    #[test]
    fn redb_store_meets_every_role_contract() {
        use fl_core::conformance::{Single, SplitOver};
        let single = || {
            let (s, g) = fresh();
            Single(s, g)
        };
        let split = || {
            let (s, g) = fresh();
            SplitOver(s, g)
        };
        fl_core::conformance::catalog(fresh);
        fl_core::conformance::tracker(single);
        fl_core::conformance::ledger(single);
        fl_core::conformance::ledger(split);
        fl_core::conformance::split_ledger(split);
        fl_core::conformance::all_roles(single);
        fl_core::conformance::local_handles(fresh);
    }

    /// A project with one gate and one record, in `s`.
    fn gate_and_record(s: &RedbStore) -> (ProjectId, GateId, RecordId) {
        let p = s.add_project("/p").unwrap();
        let g = s.add_gate(&p, "fmt", kind(), selector(), 1, "abc", "o").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        (p, g, r)
    }

    // Spec §3.2 step 6: marks are keyed by the repository's node_id, and
    // survive a reopen.
    #[test]
    fn published_marks_are_per_repository_and_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let id = {
            let s = RedbStore::open(&path).unwrap();
            let (_p, g, r) = gate_and_record(&s);
            let run = sample_record_run(1, &g, Some(&r));
            let id = run.id.clone().unwrap();
            s.append_gate_run(run).unwrap();
            // No mark exists yet, and no table: "nothing marked", not a failure.
            assert!(!s.is_published("R_1", &id).unwrap());
            assert_eq!(s.unpublished("R_1", &entry_iri(0)).unwrap().runs.len(), 1);
            s.mark_published("R_1", std::slice::from_ref(&id)).unwrap();
            s.mark_published("R_1", std::slice::from_ref(&id)).unwrap();
            id
        };
        let s = RedbStore::open(&path).unwrap();
        assert!(s.is_published("R_1", &id).unwrap());
        assert!(!s.is_published("R_2", &id).unwrap(), "a mark is per repository");
        assert!(s.unpublished("R_1", &entry_iri(0)).unwrap().runs.is_empty());
        assert_eq!(s.unpublished("R_2", &entry_iri(0)).unwrap().runs.len(), 1);
    }

    #[test]
    fn an_entry_stored_before_ids_is_never_waiting_to_be_published() {
        let (s, _d, _p, _g) = with_legacy_entries();
        let pending = s.unpublished("R_1", &entry_iri(0)).unwrap();
        assert!(pending.runs.is_empty(), "{:?}", pending.runs);
        assert!(pending.attempts.is_empty(), "{:?}", pending.attempts);
    }

    // ⚠ Spec §2.1: runs with no record are never candidates, so a plain
    // `fl check` never lengthens the scan a flush makes.
    #[test]
    fn a_run_with_no_record_never_enters_the_candidate_index() {
        let (s, _d) = fresh();
        let (_p, g, r) = gate_and_record(&s);
        s.append_gate_run(sample_record_run(1, &g, None)).unwrap();
        s.append_gate_run(sample_record_run(2, &g, Some(&r))).unwrap();
        let tx = s.db.begin_read().unwrap();
        let index = tx.open_table(CANDIDATE_RUNS).unwrap();
        let ids: Vec<String> = index
            .iter()
            .unwrap()
            .map(|e| e.unwrap().0.value().to_string())
            .collect();
        assert_eq!(ids, vec![entry_iri(2).to_string()]);
    }

    // Spec §2.1: entries recorded before the cut-over stay local.
    #[test]
    fn only_entries_after_the_cut_over_are_waiting() {
        let (s, _d) = fresh();
        let (_p, g, r) = gate_and_record(&s);
        for n in [2, 3, 4] {
            s.append_gate_run(sample_record_run(n, &g, Some(&r))).unwrap();
        }
        let ids: Vec<Option<fl_core::Iri>> = s
            .unpublished("R_1", &entry_iri(3))
            .unwrap()
            .runs
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec![Some(entry_iri(4))]);
    }

    #[test]
    fn a_cut_over_is_recorded_once_and_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        {
            let s = RedbStore::open(&path).unwrap();
            assert_eq!(s.cutover("R_1").unwrap(), None);
            s.set_cutover("R_1", &entry_iri(3)).unwrap();
            s.set_cutover("R_1", &entry_iri(3)).unwrap();
            let err = s.set_cutover("R_1", &entry_iri(4)).unwrap_err();
            assert!(matches!(err, StoreError::CutoverChanged { .. }), "{err:?}");
        }
        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.cutover("R_1").unwrap(), Some(entry_iri(3)));
        assert_eq!(s.cutover("R_2").unwrap(), None);
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: FAIL to compile — `SplitOver`, `SplitFixture`, `CANDIDATE_RUNS` and `RedbStore: Outbox` do not exist.

- [ ] **Step 3: The fixture, and `Outbox` over redb**

In `crates/core/src/conformance.rs`, extend Task 3's imports to `use crate::decision::{Decision, LeftLocal, Outcome, TransitionOutcome};` and `use crate::split::{Batch, Outbox, RemoteLedger, SplitLedger};`, and add after `Single`'s `Fixture` impl (`:77`):

```rust
/// A split ledger to test, with the means to drive its GitHub side.
pub trait SplitFixture {
    fn with_split(&self, f: &mut dyn FnMut(&Bound<'_>, &dyn RemoteControl));
}

/// `SplitLedger` over a local store `S` and a [`MemRemote`] for `R_1`,
/// whose ledger was switched on (a cut-over before every sample entry). A
/// [`Fixture`] too, so the shared ledger suite runs over it. Plan B's
/// fixture swaps `MemRemote` for `GithubLedger` over the fake GitHub.
pub struct SplitOver<S, G>(pub S, pub G);

impl<S: Catalog + Tracker + Ledger + Handles + Outbox, G> SplitFixture for SplitOver<S, G> {
    fn with_split(&self, f: &mut dyn FnMut(&Bound<'_>, &dyn RemoteControl)) {
        self.0
            .set_cutover("R_1", &entry_iri(0))
            .expect("a fresh store records a cut-over");
        let remote = MemRemote::new("R_1");
        let split = SplitLedger {
            local: &self.0,
            github: &remote,
        };
        f(
            &Bound {
                catalog: &self.0,
                tracker: &self.0,
                ledger: &split,
                handles: &self.0,
            },
            &remote,
        );
    }
}

impl<S: Catalog + Tracker + Ledger + Handles + Outbox, G> Fixture for SplitOver<S, G> {
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
        self.with_split(&mut |b, _| f(b));
    }
}
```

In `crates/store/src/lib.rs`, import `use fl_core::split::{Outbox, Pending};` (`:8`), and add after `GITHUB_BINDINGS` (`:47`):

```rust
/// (repository `node_id`, entry id) → published (GitHub ledger spec §3.2
/// step 6). Additive, like every table below: created by the first write,
/// and a store without it has none. An older fl ignores them all: it
/// publishes nothing.
const LEDGER_PUBLISHED: TableDefinition<(&str, &str), bool> =
    TableDefinition::new("ledger_published");
/// repository `node_id` → the id after which entries are publishable (spec
/// §2.1).
const LEDGER_CUTOVERS: TableDefinition<&str, &str> = TableDefinition::new("ledger_cutovers");
/// Entry id → row key in `gate_runs`, for every run that carries an id AND
/// a record — the only runs a flush may publish (spec §1.3, §2.1). Written
/// in the append's own transaction. A plain `fl check` run, and every run
/// from before ids, never enters it, so a flush never scans them.
const CANDIDATE_RUNS: TableDefinition<&str, u64> = TableDefinition::new("ledger_candidate_runs");
/// Entry id → row key in `attempts`, for every attempt that carries an id.
const CANDIDATE_ATTEMPTS: TableDefinition<&str, u64> =
    TableDefinition::new("ledger_candidate_attempts");
```

`append_json` (`:318-341`) indexes a candidate in the same transaction as its row:

```rust
    /// Log rows are keyed by an internal sequence, not by an id: they are
    /// never addressed from outside the store. A `candidate` — an index and
    /// the entry's id — is written in the same transaction as the row.
    fn append_json<T: serde::Serialize>(
        &self,
        counter: &str,
        table: TableDefinition<u64, &str>,
        value: &T,
        candidate: Option<(TableDefinition<&str, u64>, &Iri)>,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(value).map_err(backend)?;
        let tx = self.db.begin_write().map_err(backend)?;
        {
            let mut meta = tx.open_table(META).map_err(backend)?;
            let seq = meta
                .get(counter)
                .map_err(backend)?
                .map(|v| v.value())
                .unwrap_or(0)
                + 1;
            meta.insert(counter, seq).map_err(backend)?;
            tx.open_table(table)
                .map_err(backend)?
                .insert(seq, json.as_str())
                .map_err(backend)?;
            if let Some((index, id)) = candidate {
                tx.open_table(index)
                    .map_err(backend)?
                    .insert(id.as_str(), seq)
                    .map_err(backend)?;
            }
        }
        tx.commit().map_err(backend)
    }
```

The two appends (`:948-954`):

```rust
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError> {
        let candidate = match (&run.id, &run.record) {
            (Some(id), Some(_)) => Some((CANDIDATE_RUNS, id)),
            _ => None,
        };
        self.append_json(NEXT_RUN, GATE_RUNS, &run, candidate)
    }

    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError> {
        let candidate = attempt.id.as_ref().map(|id| (CANDIDATE_ATTEMPTS, id));
        self.append_json(NEXT_ATTEMPT, ATTEMPTS, &attempt, candidate)
    }
```

Next to `index_new` (after `:152`):

```rust
/// The entries `index` lists with an id after `after` that `repo` has not
/// marked, read from `log` by their row key, in id order.
fn waiting<T: serde::de::DeserializeOwned>(
    tx: &redb::ReadTransaction,
    index: TableDefinition<&str, u64>,
    log: TableDefinition<u64, &str>,
    repo: &str,
    after: &Iri,
) -> Result<Vec<T>, StoreError> {
    let candidates = match tx.open_table(index) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![]),
        Err(e) => return Err(backend(e)),
    };
    let marks = match tx.open_table(LEDGER_PUBLISHED) {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(backend(e)),
    };
    let rows = tx.open_table(log).map_err(backend)?;
    let mut out = Vec::new();
    for entry in candidates.range(after.as_str()..).map_err(backend)? {
        let (id, seq) = entry.map_err(backend)?;
        let id = id.value();
        if id == after.as_str() {
            continue;
        }
        if let Some(m) = &marks
            && m.get((repo, id)).map_err(backend)?.is_some()
        {
            continue;
        }
        let Some(row) = rows.get(seq.value()).map_err(backend)? else {
            return Err(decode(format!(
                "the ledger's candidate index names {id}, whose row is missing"
            )));
        };
        out.push(serde_json::from_str(row.value()).map_err(decode)?);
    }
    Ok(out)
}
```

and after `impl Ledger for RedbStore` (`:945-967`):

```rust
impl Outbox for RedbStore {
    fn unpublished(&self, repo: &str, after: &Iri) -> Result<Pending, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        Ok(Pending {
            runs: waiting(&tx, CANDIDATE_RUNS, GATE_RUNS, repo, after)?,
            attempts: waiting(&tx, CANDIDATE_ATTEMPTS, ATTEMPTS, repo, after)?,
        })
    }

    fn is_published(&self, repo: &str, id: &Iri) -> Result<bool, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(LEDGER_PUBLISHED) {
            Ok(t) => t,
            // The first mark creates the table: "nothing marked".
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(e) => return Err(backend(e)),
        };
        let found = table
            .get((repo, id.as_str()))
            .map_err(backend)?
            .is_some();
        Ok(found)
    }

    fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        {
            let mut table = tx.open_table(LEDGER_PUBLISHED).map_err(backend)?;
            for id in ids {
                table.insert((repo, id.as_str()), true).map_err(backend)?;
            }
        }
        tx.commit().map_err(backend)
    }

    fn cutover(&self, repo: &str) -> Result<Option<Iri>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(LEDGER_CUTOVERS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        let Some(v) = table.get(repo).map_err(backend)? else {
            return Ok(None);
        };
        Iri::parse(v.value()).map(Some).map_err(decode)
    }

    fn set_cutover(&self, repo: &str, id: &Iri) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        {
            let mut table = tx.open_table(LEDGER_CUTOVERS).map_err(backend)?;
            let held = table
                .get(repo)
                .map_err(backend)?
                .map(|v| v.value().to_string());
            match held {
                // Dropping `tx` uncommitted aborts it: nothing was written.
                Some(h) if h == id.as_str() => return Ok(()),
                Some(h) => {
                    return Err(StoreError::CutoverChanged {
                        node_id: repo.to_string(),
                        held: Iri::parse(&h).map_err(decode)?,
                        found: id.clone(),
                    });
                }
                None => {
                    table.insert(repo, id.as_str()).map_err(backend)?;
                }
            }
        }
        tx.commit().map_err(backend)
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS — the 4 ledger cases over `MemStore`, `RedbStore` and a split ledger over each; the 8 split cases over both; the five new store tests. `fl-github`'s contract tests still pass: they do not call `ledger`.

- [ ] **Step 5: Mutation checks**

One at a time, confirm red, restore:
- In `append_gate_run`, index every run with an id, record or not → `a_run_with_no_record_never_enters_the_candidate_index` FAILS.
- In `waiting`, drop the marks test → `published_marks_are_per_repository…` FAILS.
- In `waiting`, range from the start (`candidates.iter()`) and drop the `after` skip → `only_entries_after_the_cut_over_are_waiting` FAILS.
- In `is_published`, return `Ok(false)` → `published_marks_are_per_repository…` FAILS.
- In `set_cutover`, overwrite a held cut-over → `a_cut_over_is_recorded_once_and_survives_a_reopen` FAILS.
- In `SplitLedger::gate_runs`, replace the local read with `let local: Vec<GateRun> = Vec::new();` → `a_gate_the_local_catalog_never_held…` FAILS (the remote answers an empty list for any gate, so only the local catalog can say `NotOwned`).
- In `MemRemote::publish`, drop the de-duplication of runs → `a_commit_whose_answer_was_lost_is_not_duplicated…` FAILS.
- Remove one case from `split_ledger`'s list without changing `SPLIT_LEDGER_CASES` → the count assertion FAILS.

- [ ] **Step 6: Trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/conformance.rs crates/core/src/mem.rs crates/store/src/lib.rs
git commit -m "test(core): one ledger contract for every ledger; Outbox over redb

The ledger suite is Bound-based and runs over MemStore, RedbStore and a
SplitLedger over each; a split-ledger suite pins the flush, merge by id,
GitHub down and back, a lost answer retried without a duplicate, another
machine's runs, another repository's entry skipped without blocking,
and NotOwned from the local catalog. RedbStore indexes only runs tied to
a record (and attempts) as candidates, ranges them after the cut-over,
and keeps marks and cut-overs per repository node_id, all in additive
tables. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 5: Decisions from reports, and the flush inside `move_record`

**Files:**
- Create: `crates/exec/src/decision.rs`
- Create: `crates/exec/src/journal.rs` (test-only)
- Modify: `crates/exec/src/population.rs:7-37` (`ExecError::Unpublished`)
- Modify: `crates/exec/src/record.rs:1-99` (imports, `MoveReport`, `move_record`), tests
- Modify: `crates/exec/src/lib.rs` (modules)

**Interfaces:**
- Consumes: `Decision`, `Outcome`, `TransitionOutcome`, `Flushed`, `Ledger::flush` (Task 2); `GateReport.run`, `stamp` (Task 1).
- Produces:

```rust
// fl_exec::decision
pub fn for_move(record: &Record, to: State, transitions: &[TransitionReport], allowed: bool) -> Decision;
pub fn for_check(record: &RecordId, report: &TransitionReport) -> Decision;
pub fn for_reproduce(finding: &Finding, gate: &GateId, report: &GateReport, accepted: bool) -> Decision;
pub fn for_verify(finding: &Finding, reproduction: &GateReport, neighbours: &[GateReport], closed: bool) -> Decision;
pub fn for_attempt(id: &Iri, attempt: &Attempt) -> Decision;

// fl_exec::population::ExecError
Unpublished(String)          // the flush failed: the decision is refused, nothing changed

// fl_exec::record
pub struct MoveReport { pub transitions: Vec<TransitionReport>, pub outcome: MoveOutcome, pub flushed: Flushed }

// fl_exec::journal (cfg(test) only)
pub struct Journal<'a> { pub store: &'a MemStore, /* … */ }
impl<'a> Journal<'a> {
    pub fn new(store: &'a MemStore) -> Self;
    pub fn refusing(store: &'a MemStore) -> Self;     // every flush fails as unreachable
    pub fn roles(&self) -> Roles<'_>;                 // catalog = store; tracker and ledger = the journal
    pub fn events(&self) -> Vec<&'static str>;        // "flush", "set_record_state", "update_finding"
    pub fn decisions(&self) -> Vec<Decision>;         // every flush that succeeded
}
```

- [ ] **Step 1: The journal**

Test support first. Create `crates/exec/src/journal.rs`:

```rust
//! A tracker and ledger over a `MemStore` that writes down the order of
//! every flush and every state change, and can refuse the flush — for the
//! evidence-before-state tests (GitHub ledger spec §2.2, §8.3).

use fl_core::MemStore;
use fl_core::decision::{Decision, Flushed};
use fl_core::finding::Finding;
use fl_core::ids::{FindingId, GateId, ProjectId, RecordId};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use fl_core::model::{Record, State};
use fl_core::store::{Ledger, Roles, StoreError, Tracker};
use std::cell::RefCell;

pub struct Journal<'a> {
    pub store: &'a MemStore,
    events: RefCell<Vec<&'static str>>,
    decisions: RefCell<Vec<Decision>>,
    refuse_flush: bool,
}

impl<'a> Journal<'a> {
    pub fn new(store: &'a MemStore) -> Self {
        Self {
            store,
            events: RefCell::new(vec![]),
            decisions: RefCell::new(vec![]),
            refuse_flush: false,
        }
    }

    /// Every flush fails as GitHub unreachable.
    pub fn refusing(store: &'a MemStore) -> Self {
        Self {
            refuse_flush: true,
            ..Self::new(store)
        }
    }

    /// The store as the catalog; this journal as the tracker and the ledger.
    pub fn roles(&self) -> Roles<'_> {
        Roles {
            catalog: self.store,
            tracker: self,
            ledger: self,
        }
    }

    pub fn events(&self) -> Vec<&'static str> {
        self.events.borrow().clone()
    }

    pub fn decisions(&self) -> Vec<Decision> {
        self.decisions.borrow().clone()
    }
}

impl Ledger for Journal<'_> {
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError> {
        self.store.append_gate_run(run)
    }
    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError> {
        self.store.append_attempt(attempt)
    }
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        self.store.gate_runs(gate)
    }
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        self.store.attempts(project)
    }
    fn flush(&self, decision: Decision) -> Result<Flushed, StoreError> {
        self.events.borrow_mut().push("flush");
        if self.refuse_flush {
            return Err(StoreError::Unreachable {
                store: "github:acme/widgets".into(),
                cause: "connection refused".into(),
            });
        }
        self.decisions.borrow_mut().push(decision);
        Ok(Flushed {
            commit: Some("c1".into()),
            left_local: vec![],
        })
    }
}

impl Tracker for Journal<'_> {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        self.store.add_record(project, title)
    }
    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.store.get_record(id)
    }
    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        self.store.list_records(project)
    }
    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        self.events.borrow_mut().push("set_record_state");
        self.store.set_record_state(id, state)
    }
    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        self.store.add_finding(finding)
    }
    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        self.store.get_finding(id)
    }
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        self.events.borrow_mut().push("update_finding");
        self.store.update_finding(finding)
    }
    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        self.store.list_findings(project)
    }
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        self.store.withdrawals_by(actor)
    }
    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        self.store.add_alias(primary, alias)
    }
}
```

In `crates/exec/src/lib.rs`, add after `pub mod command;`:

```rust
pub mod decision;
```

and after `pub mod git;`:

```rust
#[cfg(test)]
mod journal;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/exec/src/decision.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ids::{ProjectId, seq_iri};
    use fl_core::log::{AttemptStatus, PathsTouched};
    use fl_core::model::Regret;
    use fl_core::stale::Staleness;
    use fl_core::verdict::Verdict;

    fn gate_report(n: u64, pass: bool) -> GateReport {
        GateReport {
            gate: GateId(seq_iri(n)),
            name: format!("g{n}"),
            verdict: Verdict::from_predicate(pass, 1),
            staleness: Staleness::Fresh,
            output_excerpt: String::new(),
            duration_ms: 1,
            run: seq_iri(100 + n),
        }
    }

    fn finding() -> Finding {
        let mut f = Finding::raise(
            ProjectId(seq_iri(2)),
            RecordId(seq_iri(1)),
            "reviewer",
            "claim",
        );
        f.id = FindingId(seq_iri(5));
        f
    }

    #[test]
    fn a_moves_decision_names_each_transition_and_rests_on_every_run_in_order() {
        let record = Record {
            id: RecordId(seq_iri(1)),
            project: ProjectId(seq_iri(2)),
            title: "t".into(),
            state: State::Review,
            also_known_as: vec![],
        };
        let transitions = vec![
            TransitionReport {
                transition: "launch".into(),
                regret: Regret::Low,
                gates: vec![gate_report(1, true), gate_report(2, true)],
            },
            TransitionReport {
                transition: "ship".into(),
                regret: Regret::High,
                gates: vec![gate_report(3, false)],
            },
        ];
        let d = for_move(&record, State::Done, &transitions, false);
        assert_eq!(d.record, record.id);
        assert_eq!(d.finding, None);
        assert_eq!(
            d.outcome,
            Outcome::Move {
                from: State::Review,
                to: State::Done,
                transitions: vec![
                    TransitionOutcome {
                        transition: "launch".into(),
                        passed: true
                    },
                    TransitionOutcome {
                        transition: "ship".into(),
                        passed: false
                    },
                ],
                allowed: false,
            }
        );
        assert_eq!(d.rests_on, vec![seq_iri(101), seq_iri(102), seq_iri(103)]);
        assert!(d.id.as_str().starts_with("urn:uuid:"));
    }

    // ⚠ The empty-population rule one level up: a transition with no gates
    // verified nothing, and the ledger must not record it as passed.
    #[test]
    fn a_transition_with_no_gates_is_recorded_as_not_passed() {
        let empty = TransitionReport {
            transition: "launch".into(),
            regret: Regret::Low,
            gates: vec![],
        };
        let d = for_check(&RecordId(seq_iri(1)), &empty);
        assert_eq!(
            d.outcome,
            Outcome::Check {
                transition: TransitionOutcome {
                    transition: "launch".into(),
                    passed: false
                }
            }
        );
        assert!(d.rests_on.is_empty());
    }

    #[test]
    fn a_verifys_decision_names_its_regressions_and_rests_on_every_gate_it_ran() {
        let f = finding();
        let d = for_verify(
            &f,
            &gate_report(1, true),
            &[gate_report(2, true), gate_report(3, false)],
            false,
        );
        assert_eq!((d.record.clone(), d.finding.clone()), (f.record, Some(f.id)));
        assert_eq!(
            d.outcome,
            Outcome::Verify {
                reproduction: GateId(seq_iri(1)),
                reproduction_passed: true,
                regressions: vec![GateId(seq_iri(3))],
                closed: false,
            }
        );
        assert_eq!(d.rests_on, vec![seq_iri(101), seq_iri(102), seq_iri(103)]);
    }

    #[test]
    fn a_reproduction_and_an_attempt_each_rest_on_their_one_entry() {
        let f = finding();
        let d = for_reproduce(&f, &GateId(seq_iri(1)), &gate_report(1, false), true);
        assert_eq!(d.finding, Some(f.id.clone()));
        assert_eq!(d.record, f.record);
        assert_eq!(
            d.outcome,
            Outcome::Reproduce {
                gate: GateId(seq_iri(1)),
                accepted: true
            }
        );
        assert_eq!(d.rests_on, vec![seq_iri(101)]);

        let attempt = Attempt {
            id: Some(seq_iri(50)),
            at: None,
            project: ProjectId(seq_iri(2)),
            record: RecordId(seq_iri(1)),
            adapter: "claude".into(),
            status: AttemptStatus::Crashed,
            duration_ms: 1,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd_micros: 0,
            paths_touched: PathsTouched::Listed(vec![]),
            output_excerpt: Some(String::new()),
        };
        let d = for_attempt(&seq_iri(50), &attempt);
        assert_eq!(d.record, attempt.record);
        assert_eq!(
            d.outcome,
            Outcome::Attempt {
                status: AttemptStatus::Crashed
            }
        );
        assert_eq!(d.rests_on, vec![seq_iri(50)]);
    }

    #[test]
    fn two_decisions_never_share_an_id() {
        let empty = TransitionReport {
            transition: "launch".into(),
            regret: Regret::Low,
            gates: vec![],
        };
        let r = RecordId(seq_iri(1));
        assert_ne!(for_check(&r, &empty).id, for_check(&r, &empty).id);
    }
}
```

Append to `crates/exec/src/record.rs`'s `mod tests` (add `use crate::journal::Journal;`, `use fl_core::decision::Outcome;` and `use fl_core::iri::Iri;` to its imports):

```rust
    /// A project whose `todo → done` is covered by one transition per
    /// program, each with its own gate over `*.rs`.
    fn covered_by(store: &MemStore, root: &std::path::Path, programs: &[&str]) -> Record {
        let head = crate::git::Git::head(root).unwrap();
        let p = store.add_project(&root.display().to_string()).unwrap();
        for (i, program) in programs.iter().enumerate() {
            let g = store
                .add_gate(
                    &p,
                    &format!("g{i}"),
                    GateKind::Command(CommandSpec {
                        program: (*program).into(),
                        args: vec![],
                        delivery: PopulationDelivery::Args,
                        timeout_secs: 10,
                        pass_codes: vec![0],
                    }),
                    Selector::Glob {
                        pattern: "*.rs".into(),
                    },
                    1,
                    &head,
                    "tester",
                )
                .unwrap();
            store
                .add_transition(Transition {
                    project: p.clone(),
                    name: format!("t{i}"),
                    from: State::Todo,
                    to: State::Done,
                    regret: Regret::Low,
                    gates: vec![g],
                })
                .unwrap();
        }
        let r = store.add_record(&p, "t").unwrap();
        store.get_record(&r).unwrap().unwrap()
    }

    // ⚠⚠ Spec §2.2 (Invariant): evidence before state. Confirmed by
    // mutation in Step 5 — moving the flush below the state change turns
    // this test red.
    #[test]
    fn the_decision_is_flushed_before_the_record_moves() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());
        let j = Journal::new(&store);

        let report = move_record(j.roles(), &record, State::Done).unwrap();

        assert!(matches!(report.outcome, MoveOutcome::Moved));
        assert_eq!(j.events(), vec!["flush", "set_record_state"]);
        assert_eq!(
            report.flushed.commit.as_deref(),
            Some("c1"),
            "the report carries what the flush did, for the command to print"
        );
    }

    // ⚠ Spec §2.2: a flush failure refuses the decision — no state change —
    // and the runs stay in the local store.
    #[test]
    fn a_move_whose_flush_fails_is_refused_and_the_record_stays() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());
        let j = Journal::refusing(&store);

        let err = match move_record(j.roles(), &record, State::Done) {
            Err(e) => e,
            Ok(_) => panic!("a move whose flush failed must be refused"),
        };

        assert!(matches!(err, ExecError::Unpublished(_)), "{err:?}");
        assert!(err.to_string().contains("nothing changed"), "{err}");
        assert_eq!(
            store.get_record(&record.id).unwrap().unwrap().state,
            State::Todo
        );
        let gate = &store.list_gates(&record.project).unwrap()[0].id;
        assert_eq!(
            store.gate_runs(gate).unwrap().len(),
            1,
            "the run is kept in the local store"
        );
        assert_eq!(j.events(), vec!["flush"]);
    }

    // Decision 11: a refused decision is flushed too.
    #[test]
    fn a_refused_move_is_flushed_too() {
        let d = repo();
        let store = MemStore::default();
        let record = covered_by(&store, d.path(), &["false"]);
        let j = Journal::new(&store);

        let report = move_record(j.roles(), &record, State::Done).unwrap();

        assert!(matches!(report.outcome, MoveOutcome::Refused { code: 1 }));
        assert_eq!(j.events(), vec!["flush"], "flushed, and the state untouched");
        match &j.decisions()[0].outcome {
            Outcome::Move {
                allowed,
                transitions,
                ..
            } => {
                assert!(!allowed);
                assert_eq!(transitions.len(), 1);
                assert!(!transitions[0].passed);
            }
            other => panic!("{other:?}"),
        }
    }

    // Spec §2.2: one flush per move, however many transitions cover it; the
    // runs carry the record, and the decision rests on every one of them.
    #[test]
    fn one_move_is_one_flush_however_many_transitions_cover_it() {
        let d = repo();
        let store = MemStore::default();
        let record = covered_by(&store, d.path(), &["true", "true"]);
        let j = Journal::new(&store);

        move_record(j.roles(), &record, State::Done).unwrap();

        assert_eq!(j.events(), vec!["flush", "set_record_state"]);
        let decisions = j.decisions();
        assert_eq!(decisions.len(), 1);
        let runs: Vec<GateRun> = store
            .list_gates(&record.project)
            .unwrap()
            .iter()
            .flat_map(|g| store.gate_runs(&g.id).unwrap())
            .collect();
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|r| r.record.as_ref() == Some(&record.id)));
        let mut cited = decisions[0].rests_on.clone();
        cited.sort();
        let mut ran: Vec<Iri> = runs.iter().map(|r| r.id.clone().unwrap()).collect();
        ran.sort();
        assert_eq!(cited, ran);
        assert_eq!(decisions[0].record, record.id);
    }

    #[test]
    fn an_ungated_move_is_flushed_with_no_transitions() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());
        let j = Journal::new(&store);

        let report = move_record(j.roles(), &record, State::Doing).unwrap();

        assert!(matches!(report.outcome, MoveOutcome::Ungated));
        assert_eq!(j.events(), vec!["flush", "set_record_state"]);
        assert_eq!(
            j.decisions()[0].outcome,
            Outcome::Move {
                from: State::Todo,
                to: State::Doing,
                transitions: vec![],
                allowed: true,
            }
        );
    }
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test -p fl-exec 2>&1 | tail -20`
Expected: FAIL to compile — `for_move` and the other builders, and `ExecError::Unpublished`, do not exist. (With stub builders the ordering tests fail: `move_record` never flushes, so `events()` has no `"flush"`.)

- [ ] **Step 4: The builders, the error, and the flush**

Put this above the test module in `crates/exec/src/decision.rs`:

```rust
//! The `Decision` each command flushes (GitHub ledger spec §2.3), composed
//! from the reports the command already holds. The id and the time are
//! minted here, as every entry's are.

use crate::evaluate::{GateReport, TransitionReport};
use crate::stamp;
use fl_core::decision::{Decision, Outcome, TransitionOutcome};
use fl_core::finding::Finding;
use fl_core::ids::{FindingId, GateId, RecordId};
use fl_core::iri::Iri;
use fl_core::log::Attempt;
use fl_core::model::{Record, State};

fn outcome_of(t: &TransitionReport) -> TransitionOutcome {
    TransitionOutcome {
        transition: t.transition.clone(),
        passed: t.passed(),
    }
}

fn stamped(
    record: RecordId,
    finding: Option<FindingId>,
    outcome: Outcome,
    rests_on: Vec<Iri>,
) -> Decision {
    Decision {
        id: stamp::entry_id(),
        at: stamp::now(),
        record,
        finding,
        outcome,
        rests_on,
    }
}

/// `record move`: every transition that covered it, and whether the move
/// was allowed. Rests on every run, in the order they ran.
pub fn for_move(
    record: &Record,
    to: State,
    transitions: &[TransitionReport],
    allowed: bool,
) -> Decision {
    stamped(
        record.id.clone(),
        None,
        Outcome::Move {
            from: record.state,
            to,
            transitions: transitions.iter().map(outcome_of).collect(),
            allowed,
        },
        transitions
            .iter()
            .flat_map(|t| t.gates.iter().map(|g| g.run.clone()))
            .collect(),
    )
}

/// `check --record`.
pub fn for_check(record: &RecordId, report: &TransitionReport) -> Decision {
    stamped(
        record.clone(),
        None,
        Outcome::Check {
            transition: outcome_of(report),
        },
        report.gates.iter().map(|g| g.run.clone()).collect(),
    )
}

/// `finding reproduce`: whether the gate was accepted as the reproduction.
pub fn for_reproduce(
    finding: &Finding,
    gate: &GateId,
    report: &GateReport,
    accepted: bool,
) -> Decision {
    stamped(
        finding.record.clone(),
        Some(finding.id.clone()),
        Outcome::Reproduce {
            gate: gate.clone(),
            accepted,
        },
        vec![report.run.clone()],
    )
}

/// `finding verify`: the reproduction, every neighbour that did not pass,
/// and whether the finding closed. Rests on every gate it ran.
pub fn for_verify(
    finding: &Finding,
    reproduction: &GateReport,
    neighbours: &[GateReport],
    closed: bool,
) -> Decision {
    stamped(
        finding.record.clone(),
        Some(finding.id.clone()),
        Outcome::Verify {
            reproduction: reproduction.gate.clone(),
            reproduction_passed: reproduction.verdict.is_pass(),
            regressions: neighbours
                .iter()
                .filter(|n| !n.verdict.is_pass())
                .map(|n| n.gate.clone())
                .collect(),
            closed,
        },
        std::iter::once(reproduction)
            .chain(neighbours)
            .map(|g| g.run.clone())
            .collect(),
    )
}

/// `fl attempt`: its status, resting on the attempt recorded as `id`.
pub fn for_attempt(id: &Iri, attempt: &Attempt) -> Decision {
    stamped(
        attempt.record.clone(),
        None,
        Outcome::Attempt {
            status: attempt.status,
        },
        vec![id.clone()],
    )
}
```

In `crates/exec/src/population.rs`, add to `ExecError` after `Store` (`:19`):

```rust
    /// ⚠ The decision's evidence could not be published (GitHub ledger spec
    /// §2.2), so the decision is refused and nothing changed. The runs are
    /// in the local store, and the next decision that reaches the ledger
    /// publishes them.
    #[error(
        "refused: the evidence for this decision could not be published to the shared ledger, \
         so nothing changed ({0}). The runs are kept in the local store, and the next decision \
         that reaches the ledger publishes them"
    )]
    Unpublished(String),
```

In `crates/exec/src/record.rs`, replace the imports and `MoveReport` (`:1-9`; `MoveOutcome` and `store_err` stay as they are):

```rust
use crate::decision;
use crate::evaluate::{TransitionReport, evaluate_transition};
use crate::population::ExecError;
use fl_core::decision::Flushed;
use fl_core::model::{Record, State, Transition};
use fl_core::store::Roles;

pub struct MoveReport {
    pub transitions: Vec<TransitionReport>,
    pub outcome: MoveOutcome,
    /// What the move's flush published, and what it left local and why
    /// (GitHub ledger spec §2.1) — for the command to report.
    pub flushed: Flushed,
}
```

and `move_record` (`:27-99`, the doc comment and the function):

```rust
/// Move a record, running every transition that covers `(record.state, to)`.
///
/// ⚠ This used to be `set_record_state` and nothing else. `check` would
/// refuse the transition and `record move` would perform the very state
/// change those gates exist to protect — reading nothing, running nothing,
/// exiting 0. A gate that the guarded action does not consult is decoration.
///
/// A transition is addressed by name; a move is addressed by the pair it
/// performs. So the move asks which declarations cover (from, to) and runs
/// every one of them.
///
/// ⚠⚠ Evidence before state (spec §3.5; GitHub ledger spec §2.2,
/// Invariant). The gate runs are appended to the ledger inside
/// `evaluate_transition`; then the move's decision is flushed — one flush
/// per move, however many transitions, a refused or ungated move included;
/// only after both does the tracker state change. A failed flush refuses
/// the move. A crash in between leaves evidence and no move, which is safe
/// to retry. The reverse order would leave a move with no evidence.
pub fn move_record(roles: Roles<'_>, record: &Record, to: State) -> Result<MoveReport, ExecError> {
    let declared: Vec<Transition> = roles
        .catalog
        .list_transitions(&record.project)
        .map_err(store_err)?
        .into_iter()
        .filter(|t| t.from == record.state && t.to == to)
        .collect();

    let mut transitions = Vec::new();
    let mut worst = 0;
    for t in &declared {
        let report = evaluate_transition(
            roles.catalog,
            roles.ledger,
            &record.project,
            &t.name,
            Some(&record.id),
        )?;
        // Same rule `check` applies: a transition that declares no gates
        // verified nothing, so it cannot authorise a move.
        let code = if report.gates.is_empty() {
            1
        } else {
            report.exit_code()
        };
        worst = worst.max(code);
        transitions.push(report);
    }

    let outcome = if declared.is_empty() {
        MoveOutcome::Ungated
    } else if worst != 0 {
        MoveOutcome::Refused { code: worst }
    } else {
        MoveOutcome::Moved
    };
    let allowed = !matches!(outcome, MoveOutcome::Refused { .. });

    let flushed = roles
        .ledger
        .flush(decision::for_move(record, to, &transitions, allowed))
        .map_err(|e| ExecError::Unpublished(e.to_string()))?;

    if allowed {
        roles
            .tracker
            .set_record_state(&record.id, to)
            .map_err(store_err)?;
    }
    Ok(MoveReport {
        transitions,
        outcome,
        flushed,
    })
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p fl-exec`
Expected: PASS, including the 5 `decision` tests and the 5 new `record` tests; the three existing `record` tests still pass. Then `cargo test --workspace`: PASS (the CLI's move tests are unchanged in local mode: the store's flush does nothing).

- [ ] **Step 6: Mutation checks — evidence before state**

One at a time, confirm red, restore:
- ⚠⚠ Move the whole `let flushed = roles.ledger.flush(…)?;` statement below the `if allowed { … }` block → `the_decision_is_flushed_before_the_record_moves` FAILS (`["set_record_state", "flush"]`) and `a_move_whose_flush_fails_is_refused…` FAILS (the record is `done`). **This is the spec §8.3 mutation: record its red output in the commit message.**
- Drop the `?` after the flush's `map_err` (`let flushed = ….unwrap_or_default();`) → `a_move_whose_flush_fails_is_refused…` FAILS.
- Flush only when allowed (`if allowed { flush }`) → `a_refused_move_is_flushed_too` FAILS.
- Return early for an ungated move before the flush (the old shape) → `an_ungated_move_is_flushed_with_no_transitions` FAILS.
- In `outcome_of`, use `passed: true` → `a_transition_with_no_gates_is_recorded_as_not_passed` FAILS.
- In `for_verify`, list every neighbour as a regression → `a_verifys_decision_names_its_regressions…` FAILS.
- In `for_move`, rest on the first transition's runs only → `a_moves_decision_names…` FAILS.
- Return `flushed: Flushed::NOTHING` from `move_record` → `the_decision_is_flushed_before_the_record_moves` FAILS.

- [ ] **Step 7: Trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/exec/src/decision.rs crates/exec/src/journal.rs crates/exec/src/population.rs \
  crates/exec/src/record.rs crates/exec/src/lib.rs
git commit -m "feat(exec): a move flushes its decision before the state changes

fl-exec composes each command's Decision from the reports it holds.
move_record flushes once per move — after every covering transition ran,
before set_record_state — and a refused or ungated move is flushed too.
A failed flush refuses the move: ExecError::Unpublished, the record
stays, the runs stay local. Evidence before state confirmed by mutation:
moving the flush below the state change turns
the_decision_is_flushed_before_the_record_moves red. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 6: Findings are decisions — tagged runs, and the flush before `update_finding`

**Files:**
- Modify: `crates/exec/src/evaluate.rs:241-266` (`run_single_gate` takes a record), tests at `:483` and `:765`
- Modify: `crates/exec/src/finding.rs:1-8` (imports), `:134-289` (`attach_reproduction`, `verify_finding`), tests (`:290-`)
- Modify: `crates/cli/src/cmd/gate.rs:253`
- Modify: `crates/cli/src/cmd/finding.rs:172-173` (the pair `attach_reproduction` returns)

**Interfaces:**
- Consumes: `decision::for_reproduce`, `decision::for_verify`, `ExecError::Unpublished`, `Journal` (Task 5).
- Produces:

```rust
// fl_exec::evaluate — the record parameter is new
pub fn run_single_gate(
    catalog: &dyn Catalog,
    ledger: &dyn Ledger,
    project: &ProjectId,
    gate: &GateId,
    record: Option<&RecordId>,   // None for `gate run`; the finding's record for reproduce/verify
) -> Result<GateReport, ExecError>;
```

```rust
// fl_exec::finding — what the flush did travels back to the command (ruling 10)
pub fn attach_reproduction(roles: Roles<'_>, finding: &FindingId, gate: &GateId)
    -> Result<(GateReport, Flushed), FindingExecError>;          // was Result<GateReport, _>
pub struct FixReport { /* existing fields */ pub flushed: Flushed }
```

`attach_reproduction` now refuses a finding that cannot take a reproduction (`FindingExecError::Finding`) **before** running the gate.

- [ ] **Step 1: Write the failing tests**

Add to `crates/exec/src/finding.rs`'s test imports:

```rust
    use crate::journal::Journal;
    use fl_core::decision::Outcome;
    use fl_core::store::Ledger;
```

and append to its `mod tests`:

```rust
    fn set_program(s: &MemStore, g: &GateId, program: &str) {
        let mut def = s.get_gate(g).unwrap().unwrap();
        def.kind = GateKind::Command(CommandSpec {
            program: program.into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 10,
            pass_codes: vec![0],
        });
        s.update_gate(&def).unwrap();
    }

    // ⚠⚠ Spec §2.2 (Invariant), confirmed by mutation in Step 5.
    #[test]
    fn a_reproduction_is_flushed_before_the_finding_changes() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let g = gate(&s, &p, d.path(), "red", "false");
        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();
        let j = Journal::new(&s);

        let (_, flushed) = attach_reproduction(j.roles(), &f, &g).unwrap();

        assert_eq!(j.events(), vec!["flush", "update_finding"]);
        assert_eq!(flushed.commit.as_deref(), Some("c1"));
        let decisions = j.decisions();
        assert_eq!(
            decisions[0].outcome,
            Outcome::Reproduce {
                gate: g.clone(),
                accepted: true
            }
        );
        assert_eq!(decisions[0].record, r);
        assert_eq!(decisions[0].finding, Some(f));
        let runs = s.gate_runs(&g).unwrap();
        assert_eq!(decisions[0].rests_on, vec![runs[0].id.clone().unwrap()]);
        assert_eq!(
            runs[0].record,
            Some(r),
            "the run carries the finding's record, so it is published with the decision"
        );
    }

    // Decision 11: a refused decision is flushed too.
    #[test]
    fn a_refused_reproduction_is_flushed_too_and_the_finding_stays_raised() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let g = gate(&s, &p, d.path(), "green", "true");
        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();
        let j = Journal::new(&s);

        let err = attach_reproduction(j.roles(), &f, &g).unwrap_err();

        assert!(
            matches!(err, FindingExecError::ReproductionPasses { .. }),
            "{err}"
        );
        assert_eq!(j.events(), vec!["flush"]);
        assert_eq!(
            j.decisions()[0].outcome,
            Outcome::Reproduce {
                gate: g,
                accepted: false
            }
        );
        assert_eq!(
            s.get_finding(&f).unwrap().unwrap().state,
            FindingState::Raised
        );
    }

    #[test]
    fn a_reproduction_whose_flush_fails_is_refused_and_keeps_its_run_locally() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let g = gate(&s, &p, d.path(), "red", "false");
        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();
        let j = Journal::refusing(&s);

        let err = attach_reproduction(j.roles(), &f, &g).unwrap_err();

        assert!(
            matches!(err, FindingExecError::Exec(ExecError::Unpublished(_))),
            "{err}"
        );
        assert_eq!(
            s.get_finding(&f).unwrap().unwrap().state,
            FindingState::Raised
        );
        assert_eq!(s.gate_runs(&g).unwrap().len(), 1, "the run is kept");
    }

    // A finding that cannot take a reproduction is refused before anything
    // runs, so every refusal after a run is a verdict — and is flushed.
    #[test]
    fn a_finding_that_cannot_take_a_reproduction_is_refused_before_its_gate_runs() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let g = gate(&s, &p, d.path(), "red", "false");
        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();
        let mut fin = s.get_finding(&f).unwrap().unwrap();
        fin.withdraw("not concrete").unwrap();
        s.update_finding(&fin).unwrap();
        let j = Journal::new(&s);

        let err = attach_reproduction(j.roles(), &f, &g).unwrap_err();

        assert!(matches!(err, FindingExecError::Finding(_)), "{err}");
        assert!(s.gate_runs(&g).unwrap().is_empty(), "nothing ran");
        assert!(j.events().is_empty(), "nothing was decided or flushed");
    }

    // ⚠⚠ Spec §2.2 (Invariant), confirmed by mutation in Step 5.
    #[test]
    fn a_verify_is_flushed_before_the_finding_closes_and_its_runs_carry_the_record() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let rep = gate(&s, &p, d.path(), "reproduction", "false");
        let f = assigned(&s, &p, &r, &rep);
        set_program(&s, &rep, "true");
        let j = Journal::new(&s);

        let report = verify_finding(j.roles(), &f).unwrap();

        assert!(report.closed);
        assert_eq!(report.flushed.commit.as_deref(), Some("c1"));
        assert_eq!(j.events(), vec!["flush", "update_finding"]);
        assert_eq!(
            j.decisions()[0].outcome,
            Outcome::Verify {
                reproduction: rep.clone(),
                reproduction_passed: true,
                regressions: vec![],
                closed: true,
            }
        );
        assert!(
            s.gate_runs(&rep)
                .unwrap()
                .iter()
                .all(|run| run.record.as_ref() == Some(&r)),
            "the reproduce and the verify both tag their runs with the finding's record"
        );
    }

    #[test]
    fn a_verify_that_does_not_close_is_flushed_too_and_names_the_regression() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let neighbour = gate(&s, &p, d.path(), "neighbour", "true");
        let rep = gate(&s, &p, d.path(), "reproduction", "false");
        run_single_gate(&s, &s, &p, &neighbour, None).unwrap();
        let f = assigned(&s, &p, &r, &rep);
        set_program(&s, &rep, "true");
        set_program(&s, &neighbour, "false");
        let j = Journal::new(&s);

        let report = verify_finding(j.roles(), &f).unwrap();

        assert!(!report.closed);
        assert_eq!(j.events(), vec!["flush"]);
        let decisions = j.decisions();
        assert_eq!(
            decisions[0].outcome,
            Outcome::Verify {
                reproduction: rep,
                reproduction_passed: true,
                regressions: vec![neighbour],
                closed: false,
            }
        );
        assert_eq!(decisions[0].rests_on.len(), 2);
        assert_eq!(
            s.get_finding(&f).unwrap().unwrap().state,
            FindingState::Assigned
        );
    }

    #[test]
    fn a_verify_whose_flush_fails_does_not_close_the_finding() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let rep = gate(&s, &p, d.path(), "reproduction", "false");
        let f = assigned(&s, &p, &r, &rep);
        set_program(&s, &rep, "true");
        let j = Journal::refusing(&s);

        let err = match verify_finding(j.roles(), &f) {
            Err(e) => e,
            Ok(_) => panic!("a verify whose flush failed must be refused"),
        };

        assert!(
            matches!(err, FindingExecError::Exec(ExecError::Unpublished(_))),
            "{err}"
        );
        assert_eq!(
            s.get_finding(&f).unwrap().unwrap().state,
            FindingState::Assigned
        );
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p fl-exec finding 2>&1 | tail -30`
Expected: FAIL to compile — `run_single_gate` takes four arguments. Once Step 3's signature change alone is in, the ordering tests FAIL (`events()` has no `"flush"`), the record-tagging assertions FAIL (`record: None`), and `a_finding_that_cannot_take…` FAILS (`gate_runs` has one run).

- [ ] **Step 3: Tag the runs, check the state first, flush before the change**

In `crates/exec/src/evaluate.rs`, `run_single_gate` and its doc comment (`:241-266`) become:

```rust
/// Run one gate against the live working tree, exactly once, and record the
/// result.
///
/// Applies staleness at [`Regret::Low`] — a bare gate run is not a
/// transition, so it warns and never fails for staleness alone — and tags
/// the appended [`GateRun`] with `record`: `None` for `gate run`, which
/// stays local; the finding's record for `attach_reproduction` and
/// `verify_finding`, so the run is published with their decision (GitHub
/// ledger spec §2.2, decision 7).
pub fn run_single_gate(
    catalog: &dyn Catalog,
    ledger: &dyn Ledger,
    project: &ProjectId,
    gate: &GateId,
    record: Option<&RecordId>,
) -> Result<GateReport, ExecError> {
    let proj = project_of(catalog, project)?;
    let root = Path::new(&proj.root);

    let Some(def) = catalog
        .get_gate(gate)
        .map_err(|e| ExecError::Store(e.to_string()))?
    else {
        return Err(ExecError::BadSelector(format!("no gate with id {gate}")));
    };

    let head = Git::head(root)?;
    run_gate(catalog, ledger, root, &head, &def, Regret::Low, record)
}
```

Its two test callers (`:483`, `:765`) gain a last argument `None`:

```rust
        let err = run_single_gate(&StampRefused(&s), &s, &p, &g, None)
```

```rust
        let err = run_single_gate(
            &store,
            &store,
            &ProjectId(seq_iri(1)),
            &GateId(seq_iri(1)),
            None,
        )
```

In `crates/cli/src/cmd/gate.rs:253`, `gate run` stays local:

```rust
            let report = run_single_gate(store, store, &g.project, &g.id, None)
```

In `crates/exec/src/finding.rs`, the imports (`:1-8`) become:

```rust
use crate::decision;
use crate::evaluate::{GateReport, run_single_gate};
use crate::population::ExecError;
use fl_core::decision::Flushed;
use fl_core::finding::FindingState;
use fl_core::ids::{FindingId, GateId};
use fl_core::iri::Iri;
use fl_core::model::Selector;
use fl_core::store::{Roles, StoreError, follow};
use fl_core::verdict::{FailReason, Verdict};
```

and `attach_reproduction` and `verify_finding` (`:134-289`, from `pub fn attach_reproduction(` to the end of `verify_finding`; their doc comments above stay) become:

```rust
pub fn attach_reproduction(
    roles: Roles<'_>,
    finding: &FindingId,
    gate: &GateId,
) -> Result<(GateReport, Flushed), FindingExecError> {
    let f = roles
        .tracker
        .get_finding(finding)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .ok_or_else(|| FindingExecError::NoSuchFinding(finding.clone()))?;
    let def = roles
        .catalog
        .get_gate(gate)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .ok_or_else(|| FindingExecError::NoSuchGate(gate.clone()))?;

    // The state change a reproduction would make, checked BEFORE the gate
    // runs: a finding that cannot take one is refused with nothing run and
    // nothing decided, so every refusal below is a verdict.
    let mut next = f.clone();
    next.attach_reproduction(gate.clone())?;

    // Tagged with the finding's record, so the run is published with the
    // decision (GitHub ledger spec §2.2, decision 7).
    let report = run_single_gate(
        roles.catalog,
        roles.ledger,
        &f.project,
        gate,
        Some(&f.record),
    )?;
    let refusal = match &report.verdict {
        Verdict::Pass { population, .. } => Some(FindingExecError::ReproductionPasses {
            name: def.name.clone(),
            population: population.get(),
        }),
        Verdict::Error { detail, .. } => Some(FindingExecError::ReproductionErrored {
            name: def.name.clone(),
            detail: detail.clone(),
        }),
        // An empty-population fail examined nothing, so it is refused for
        // the same reason a Pass is: it is not evidence the defect is
        // present. This must be checked BEFORE the catch-all below, which
        // would otherwise accept it as a legitimate reproduction — and
        // because Verdict::Pass requires a non-zero population by
        // construction, a finding reproduced this way could never close
        // through any repair.
        Verdict::Fail {
            reason: FailReason::EmptyPopulation,
            ..
        } => {
            let root = roles
                .catalog
                .get_project(&f.project)
                .map_err(|e| FindingExecError::Store(e.to_string()))?
                .map(|p| p.root)
                .unwrap_or_default();
            Some(FindingExecError::ReproductionEmptyPopulation {
                name: def.name.clone(),
                selector: describe_selector(&def.selector),
                root,
            })
        }
        Verdict::Fail { .. } => None,
    };

    // ⚠⚠ Evidence before state (GitHub ledger spec §2.2, Invariant): the
    // decision — accepted or refused (decision 11) — is flushed before the
    // finding changes. A failed flush refuses it: the finding stays raised.
    let flushed = roles
        .ledger
        .flush(decision::for_reproduce(
            &f,
            gate,
            &report,
            refusal.is_none(),
        ))
        .map_err(|e| FindingExecError::Exec(ExecError::Unpublished(e.to_string())))?;
    if let Some(refused) = refusal {
        return Err(refused);
    }
    roles
        .tracker
        .update_finding(&next)
        .map_err(|e| FindingExecError::Store(e.to_string()))?;
    Ok((report, flushed))
}
```

```rust
pub fn verify_finding(
    roles: Roles<'_>,
    finding: &FindingId,
) -> Result<FixReport, FindingExecError> {
    let f = roles
        .tracker
        .get_finding(finding)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .ok_or_else(|| FindingExecError::NoSuchFinding(finding.clone()))?;
    if f.state != FindingState::Assigned {
        return Err(FindingExecError::NotAssigned(
            finding.clone(),
            f.state.as_wire(),
        ));
    }

    // The finding's own stored references, followed explicitly so a
    // dangling or never-owned project or reproduction is named as such —
    // by the finding, not surfaced as whatever `run_single_gate` happens to
    // say about a bare id it was handed.
    follow_ref(
        format!("finding {finding}'s project"),
        f.project.iri(),
        roles.catalog.get_project(&f.project),
    )?;

    let gate = f
        .reproduction
        .clone()
        .ok_or_else(|| FindingExecError::NoReproduction(finding.clone()))?;

    follow_ref(
        format!("finding {finding}'s reproduction"),
        gate.iri(),
        roles.catalog.get_gate(&gate),
    )?;

    // Every run is tagged with the finding's record, so it is published
    // with the decision (GitHub ledger spec §2.2, decision 7).
    let reproduction = run_single_gate(
        roles.catalog,
        roles.ledger,
        &f.project,
        &gate,
        Some(&f.record),
    )?;

    // The baseline is already on disk: a gate with a last_pass_commit passed
    // at some point, so a failure now is a regression rather than news.
    let neighbours: Vec<_> = roles
        .catalog
        .list_gates(&f.project)
        .map_err(|e| FindingExecError::Store(e.to_string()))?
        .into_iter()
        .filter(|g| g.id != gate && g.last_pass_commit.is_some())
        .map(|g| g.id)
        .collect();

    // Every qualifying neighbour is run here, exactly once, whether it ends
    // up passing or failing. `regressions` is derived from this same pass —
    // not a second one — by filtering out whatever did not pass.
    let mut neighbour_reports = Vec::new();
    for id in &neighbours {
        let r = run_single_gate(
            roles.catalog,
            roles.ledger,
            &f.project,
            id,
            Some(&f.record),
        )?;
        neighbour_reports.push(r);
    }
    let regressions: Vec<GateReport> = neighbour_reports
        .iter()
        .filter(|r| !r.verdict.is_pass())
        .cloned()
        .collect();

    let closed = reproduction.verdict.is_pass() && regressions.is_empty();
    let mut next = f.clone();
    if closed {
        next.mark_fixed()?;
    }

    // ⚠⚠ Evidence before state (GitHub ledger spec §2.2, Invariant): flushed
    // before the finding closes, and flushed when it does not (decision 11).
    // A failed flush refuses the verify: the finding stays assigned.
    let flushed = roles
        .ledger
        .flush(decision::for_verify(
            &f,
            &reproduction,
            &neighbour_reports,
            closed,
        ))
        .map_err(|e| FindingExecError::Exec(ExecError::Unpublished(e.to_string())))?;
    if closed {
        roles
            .tracker
            .update_finding(&next)
            .map_err(|e| FindingExecError::Store(e.to_string()))?;
    }

    Ok(FixReport {
        reproduction,
        neighbours: neighbour_reports,
        regressions,
        closed,
        flushed,
    })
}
```

`FixReport` (`:74-79`) carries the flush's report:

```rust
pub struct FixReport {
    pub reproduction: GateReport,
    pub neighbours: Vec<GateReport>,
    pub regressions: Vec<GateReport>,
    pub closed: bool,
    /// What the verify's flush published, and what it left local and why
    /// (GitHub ledger spec §2.1) — for the command to report.
    pub flushed: Flushed,
}
```

The existing test `a_failing_gate_is_accepted_and_moves_the_finding_to_reproduced` (`:458`) takes the pair:

```rust
        let (report, _) = attach_reproduction(Roles::single(&s), &f, &g).unwrap();
```

In `crates/cli/src/cmd/finding.rs:172-173`, the reproduce arm takes the pair (Task 7 prints the second half):

```rust
            let (report, _flushed) = attach_reproduction(ctx.roles(), &fid, &gid)
                .map_err(|e| explain(e, &finding, Some(&gate)))?;
```

The two existing test calls of `run_single_gate` in `finding.rs` (`:506`, `:566`) gain `None`:

```rust
        let _ = crate::evaluate::run_single_gate(&s, &s, &p, &neighbour, None).unwrap();
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS, including the 7 new `finding` tests; every existing `finding` test and every CLI finding test still passes (`crates/cli/src/cmd/finding.rs` needs no change: `explain` passes `Exec` through, and in local mode the store's flush does nothing).

- [ ] **Step 5: Mutation checks — evidence before state**

One at a time, confirm red, restore:
- ⚠⚠ In `attach_reproduction`, move the flush below `update_finding` (flushing only on acceptance) → `a_reproduction_is_flushed_before_the_finding_changes` FAILS (`["update_finding", "flush"]`) and `a_reproduction_whose_flush_fails…` FAILS (the finding is reproduced).
- ⚠⚠ In `verify_finding`, move the flush below the `if closed { update_finding }` block → `a_verify_is_flushed_before_the_finding_closes…` FAILS and `a_verify_whose_flush_fails…` FAILS (the finding is fixed).
- In `attach_reproduction`, flush only when accepted (`if refusal.is_none()`) → `a_refused_reproduction_is_flushed_too…` FAILS.
- In `verify_finding`, flush only when closed → `a_verify_that_does_not_close_is_flushed_too…` FAILS.
- Pass `None` instead of `Some(&f.record)` in `attach_reproduction` → `a_reproduction_is_flushed_before…` FAILS (`record: None`).
- Pass `None` for the reproduction's run in `verify_finding` → `…its_runs_carry_the_record` FAILS.
- Move `next.attach_reproduction(gate.clone())?;` below the run → `a_finding_that_cannot_take_a_reproduction…` FAILS (a run is recorded).
- Return `Flushed::NOTHING` in place of the flush's answer in either function → `a_reproduction_is_flushed_before…` or `a_verify_is_flushed_before…` FAILS.

- [ ] **Step 6: Trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/exec/src/evaluate.rs crates/exec/src/finding.rs crates/cli/src/cmd/gate.rs \
  crates/cli/src/cmd/finding.rs
git commit -m "feat(exec): reproduce and verify are decisions, flushed before the finding changes

run_single_gate takes the record its run is tied to: none for gate run,
the finding's record for reproduce and verify, so their runs go out with
the decision. Both flush before update_finding, whether the verdict was
accepted or refused; a failed flush refuses the decision and leaves the
finding as it was. A finding that cannot take a reproduction is refused
before its gate runs. Evidence before state confirmed by mutation.
Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 7: The CLI records through the bound ledger — `Ctx.ledger`, `check --record`, `fl attempt`, `fl stats`

**Files:**
- Create: `crates/cli/src/testing.rs` (test-only)
- Modify: `crates/cli/src/ctx.rs:1-28` (the whole file)
- Modify: `crates/cli/src/main.rs:1-4` (module), `:562-576` (`Ctx`)
- Modify: `crates/cli/src/cmd/check.rs:1-7` (imports), `:42-83` (`run`), tests
- Modify: `crates/cli/src/cmd/attempt.rs:1-11` (imports), `:47-119` (`run`), tests
- Modify: `crates/cli/src/cmd/stats.rs:1-8` (imports), `:33-69` (`run`), tests
- Modify: `crates/cli/src/cmd/record.rs:121-122` and `crates/cli/src/cmd/finding.rs:172-173`, `:194-195` (print what stayed local)
- Modify: `crates/cli/tests/github.rs` (two tests: `check --record` in mode A)

**Interfaces:**
- Consumes: `Ledger::flush`, `Flushed`, `Coverage` (Tasks 2–3); `decision::for_check`, `decision::for_attempt`, `stamp` (Tasks 1, 5).
- Produces:

```rust
// crates/cli/src/ctx.rs
pub struct Ctx<'a> {
    pub store: &'a RedbStore,
    pub tracker: &'a dyn Tracker,
    pub ledger: &'a dyn Ledger,       // new: the local store today; plan B binds a SplitLedger here
    pub handles: &'a dyn Handles,
    pub github: Option<&'a fl_github::GithubTracker>,
    pub tracker_label: String,
}
// roles().ledger is `self.ledger`
pub fn flush_notes(flushed: &Flushed) -> Vec<String>;   // one "note:" line per LeftLocal
pub fn report_flush(flushed: &Flushed);                 // prints flush_notes to stderr

// crates/cli/src/testing.rs (cfg(test))
#[derive(Default)] pub struct Flushes { pub decisions: RefCell<Vec<Decision>>, /* refuse */ }
impl Flushes { pub fn refusing() -> Self; }       // impl Ledger

// crates/cli/src/cmd/stats.rs
fn report(attempts: &[Attempt], coverage: &Coverage) -> Vec<String>;   // plan B passes LocalOnly
```

- [ ] **Step 1: The test double**

Create `crates/cli/src/testing.rs`:

```rust
//! Test doubles shared by the command modules' unit tests.

use fl_core::decision::{Decision, Flushed};
use fl_core::ids::{GateId, ProjectId};
use fl_core::log::{Attempt, GateRun};
use fl_core::store::{Ledger, StoreError};
use std::cell::RefCell;

/// A ledger that keeps every decision it is asked to flush — or, built with
/// [`Flushes::refusing`], refuses every one as unreachable.
#[derive(Default)]
pub struct Flushes {
    refuse: bool,
    pub decisions: RefCell<Vec<Decision>>,
}

impl Flushes {
    pub fn refusing() -> Self {
        Self {
            refuse: true,
            ..Self::default()
        }
    }
}

impl Ledger for Flushes {
    fn append_gate_run(&self, _: GateRun) -> Result<(), StoreError> {
        Ok(())
    }
    fn append_attempt(&self, _: Attempt) -> Result<(), StoreError> {
        Ok(())
    }
    fn gate_runs(&self, _: &GateId) -> Result<Vec<GateRun>, StoreError> {
        Ok(vec![])
    }
    fn attempts(&self, _: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        Ok(vec![])
    }
    fn flush(&self, decision: Decision) -> Result<Flushed, StoreError> {
        if self.refuse {
            return Err(StoreError::Unreachable {
                store: "github:acme/widgets".into(),
                cause: "connection refused".into(),
            });
        }
        self.decisions.borrow_mut().push(decision);
        Ok(Flushed {
            commit: Some("c1".into()),
            left_local: vec![],
        })
    }
}
```

In `crates/cli/src/main.rs`, after `mod refs;` (`:4`):

```rust
#[cfg(test)]
mod testing;
```

- [ ] **Step 2: Write the failing tests**

`crates/cli/src/ctx.rs` gains a test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Flushes;
    use fl_core::Iri;
    use fl_core::ids::RecordId;

    // Spec §2.6: `Ctx::roles()` binds the ledger the command was given —
    // never the local store behind its back.
    #[test]
    fn the_roles_bind_the_ledger_the_command_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("s.redb")).unwrap();
        let flushes = Flushes::default();
        let ctx = Ctx {
            store: &store,
            tracker: &store,
            ledger: &flushes,
            handles: &store,
            github: None,
            tracker_label: String::new(),
        };
        assert!(
            std::ptr::addr_eq(ctx.roles().ledger, &flushes as &dyn Ledger),
            "the roles carry the bound ledger"
        );
    }

    #[test]
    fn a_flush_that_left_nothing_local_says_nothing() {
        assert!(flush_notes(&Flushed::NOTHING).is_empty());
    }

    // Spec §2.1: what stays local is reported, one line per cause.
    #[test]
    fn what_a_flush_left_local_is_reported_by_cause() {
        let entry = fl_core::ids::seq_iri(7);
        let record = RecordId(Iri::parse("https://github.com/acme/other/issues/3").unwrap());
        let lines = flush_notes(&Flushed {
            commit: None,
            left_local: vec![
                LeftLocal::NoCutover,
                LeftLocal::OtherRepository {
                    entry: entry.clone(),
                    record: record.clone(),
                },
            ],
        });
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("never switched on"), "{lines:?}");
        assert!(
            lines[1].contains(entry.as_str()) && lines[1].contains(record.iri().as_str()),
            "{lines:?}"
        );
    }
}
```

`crates/cli/src/cmd/check.rs` gains a test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Flushes;
    use fl_core::decision::{Outcome, TransitionOutcome};
    use fl_core::ids::{GateId, seq_iri};
    use fl_core::model::Regret;
    use fl_core::stale::Staleness;
    use fl_core::verdict::Verdict;
    use fl_exec::evaluate::GateReport;

    fn report(pass: bool) -> TransitionReport {
        TransitionReport {
            transition: "launch".into(),
            regret: Regret::Low,
            gates: vec![GateReport {
                gate: GateId(seq_iri(1)),
                name: "g".into(),
                verdict: Verdict::from_predicate(pass, 1),
                staleness: Staleness::Fresh,
                output_excerpt: String::new(),
                duration_ms: 1,
                run: seq_iri(7),
            }],
        }
    }

    // Spec §2.1: a plain `check` stays local and needs no network.
    #[test]
    fn a_plain_check_decides_nothing_and_flushes_nothing() {
        let ledger = Flushes::refusing();
        let flushed = publish(&ledger, None, &report(true)).expect("no record, no flush, no error");
        assert_eq!(flushed, fl_core::decision::Flushed::NOTHING);
    }

    #[test]
    fn a_checks_decision_names_the_record_the_transition_and_its_runs() {
        let ledger = Flushes::default();
        let r = RecordId(seq_iri(3));
        publish(&ledger, Some(&r), &report(true)).unwrap();
        let decisions = ledger.decisions.borrow();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].record, r);
        assert_eq!(
            decisions[0].outcome,
            Outcome::Check {
                transition: TransitionOutcome {
                    transition: "launch".into(),
                    passed: true
                }
            }
        );
        assert_eq!(decisions[0].rests_on, vec![seq_iri(7)]);
    }

    // Decision 11: a failed check is a decision too.
    #[test]
    fn a_failed_check_is_flushed_too() {
        let ledger = Flushes::default();
        publish(&ledger, Some(&RecordId(seq_iri(3))), &report(false)).unwrap();
        assert_eq!(ledger.decisions.borrow().len(), 1);
    }

    // Spec §7: unreachable at a check's flush refuses it; the runs stay.
    #[test]
    fn a_check_whose_decision_cannot_be_published_is_refused_and_says_the_runs_are_kept() {
        let err = publish(
            &Flushes::refusing(),
            Some(&RecordId(seq_iri(3))),
            &report(true),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("refused"), "{msg}");
        assert!(msg.contains("kept in the local store"), "{msg}");
    }
}
```

`crates/cli/src/cmd/attempt.rs` gains a test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Flushes;
    use fl_core::decision::Outcome;
    use fl_core::ids::{ProjectId, seq_iri};

    fn attempt() -> Attempt {
        Attempt {
            id: Some(seq_iri(50)),
            at: None,
            project: ProjectId(seq_iri(1)),
            record: RecordId(seq_iri(2)),
            adapter: "claude".into(),
            status: AttemptStatus::Timeout,
            duration_ms: 1,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd_micros: 0,
            paths_touched: PathsTouched::Listed(vec![]),
            output_excerpt: Some(String::new()),
        }
    }

    #[test]
    fn an_attempts_decision_rests_on_the_attempt() {
        let ledger = Flushes::default();
        publish(&ledger, &seq_iri(50), &attempt()).unwrap();
        let decisions = ledger.decisions.borrow();
        assert_eq!(decisions[0].rests_on, vec![seq_iri(50)]);
        assert_eq!(
            decisions[0].outcome,
            Outcome::Attempt {
                status: AttemptStatus::Timeout
            }
        );
    }

    // Spec §7 and decision 8: the attempt already ran, so a failed publish
    // refuses nothing — it is reported, and the attempt is kept for the next
    // flush.
    #[test]
    fn an_attempt_that_cannot_be_published_is_reported_as_kept_not_refused() {
        let err = publish(&Flushes::refusing(), &seq_iri(50), &attempt()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("recorded in the local store"), "{msg}");
        assert!(msg.contains("next decision"), "{msg}");
        assert!(!msg.contains("refused:"), "not a refusal: {msg}");
    }
}
```

`crates/cli/src/cmd/stats.rs` gains a test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Spec §2.5: a report over the local store alone says so — a count that
    // silently omitted GitHub would read as the total.
    #[test]
    fn a_local_only_report_says_so_even_when_it_found_nothing() {
        let lines = report(
            &[],
            &Coverage::LocalOnly {
                reason: "GitHub could not be read: connection refused".into(),
            },
        );
        assert_eq!(lines[0], "attempts: 0");
        assert!(
            lines.iter().any(|l| l.starts_with("note: this covers the local store only")
                && l.contains("connection refused")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_complete_report_adds_no_note() {
        let lines = report(&[], &Coverage::Complete);
        assert_eq!(lines, vec!["attempts: 0".to_string()]);
    }
}
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test -p fl-cli --bin fl 2>&1 | tail -20`
Expected: FAIL to compile — `Ctx` has no `ledger`, and `check::publish`, `attempt::publish` and `stats::report` do not exist.

- [ ] **Step 4: `Ctx.ledger` and the three commands**

Replace `crates/cli/src/ctx.rs` above its test module with:

```rust
//! What a command works with (GitHub tracker spec §1.3; GitHub ledger spec
//! §2.6): the local store for the catalog, whichever tracker the project is
//! bound to, and whichever ledger it is bound to.

use fl_core::decision::{Flushed, LeftLocal};
use fl_core::store::{Handles, Ledger, Roles, Tracker};
use fl_store::RedbStore;

pub struct Ctx<'a> {
    pub store: &'a RedbStore,
    /// The local store, or the GitHub tracker behind `CatalogChecked`.
    pub tracker: &'a dyn Tracker,
    /// Where runs, attempts and decisions are recorded: the local store,
    /// or — in mode B — a `SplitLedger` over it (plan B binds that).
    pub ledger: &'a dyn Ledger,
    /// The local store's handles, or `KindRouted` over the store and GitHub.
    pub handles: &'a dyn Handles,
    /// The GitHub tracker, for `fl github` and the publish check.
    pub github: Option<&'a fl_github::GithubTracker>,
    /// Where records and findings live, for messages.
    pub tracker_label: String,
}

impl Ctx<'_> {
    pub fn roles(&self) -> Roles<'_> {
        Roles {
            catalog: self.store,
            tracker: self.tracker,
            ledger: self.ledger,
        }
    }
}

/// What a flush left local, one line each, for a person. Reported, never a
/// refusal (GitHub ledger spec §2.1).
pub fn flush_notes(flushed: &Flushed) -> Vec<String> {
    flushed
        .left_local
        .iter()
        .map(|left| match left {
            LeftLocal::NoCutover => "note: this repository's GitHub ledger was never switched                                      on (`fl github ledger init`), so nothing was published;                                      the runs stay in the local store"
                .to_string(),
            LeftLocal::OtherRepository { entry, record } => format!(
                "note: {entry} is tied to {record}, which is not an issue of this repository,                  so it stays in the local store"
            ),
        })
        .collect()
}

/// [`flush_notes`], on stderr.
pub fn report_flush(flushed: &Flushed) {
    for line in flush_notes(flushed) {
        eprintln!("{line}");
    }
}
```

In `crates/cli/src/main.rs`, both `Ctx` literals (`:562-576`) gain the ledger, which in plan A is always the local store:

```rust
            Ctx {
                store: &store,
                tracker: &checked,
                ledger: &store,
                handles: &routed,
                github: Some(gh),
                tracker_label: format!("github:{}", gh.repo().full_name),
            }
```

```rust
        None => Ctx {
            store: &store,
            tracker: &store,
            ledger: &store,
            handles: &store,
            github: None,
            tracker_label: store.label().to_string(),
        },
```

In `crates/cli/src/cmd/check.rs`, the imports (`:1-7`) become:

```rust
use crate::ctx::Ctx;
use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Args;
use fl_core::decision::Flushed;
use fl_core::ids::{ProjectId, RecordId};
use fl_core::store::Ledger;
use fl_core::{Iri, Kind};
use fl_exec::evaluate::{TransitionReport, evaluate_transition};
```

and `run` (`:42-83`) becomes, followed by `publish`:

```rust
pub fn run(ctx: &Ctx<'_>, cmd: Cmd) -> Result<i32> {
    let store = ctx.store;
    let project = ProjectId(refs::resolve(
        ctx.handles,
        store.label(),
        Kind::Project,
        &cmd.project,
    )?);
    let record = match &cmd.record {
        Some(r) => {
            let id = RecordId(refs::resolve(
                ctx.handles,
                &ctx.tracker_label,
                Kind::Record,
                r,
            )?);
            // The record's PRIMARY id, never the alias the person typed:
            // every run tied to one record must name it the same way.
            let Some(rec) = ctx.tracker.get_record(&id)? else {
                bail!(
                    "`{r}` is not a record in the store at {}. Use \
                     `fl record list --project <project>` to see records that exist.",
                    ctx.tracker_label
                );
            };
            Some(rec.id)
        }
        None => None,
    };
    crate::cmd::manifest::ensure_import_current(store, &project)?;
    let roles = ctx.roles();
    let report = evaluate_transition(
        roles.catalog,
        roles.ledger,
        &project,
        &cmd.transition,
        record.as_ref(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    for g in &report.gates {
        let (label, detail) = g.verdict.describe();
        let note = g.staleness.note();
        println!("{label}\t{}\t{detail}\t{}ms{note}", g.name, g.duration_ms);
        if !g.verdict.is_pass() && !g.output_excerpt.is_empty() {
            for line in g.output_excerpt.lines().take(20) {
                println!("\t| {line}");
            }
        }
    }

    let code = if report.gates.is_empty() {
        println!(
            "FAIL\t{}\tthe transition declares no gates, so nothing was verified",
            report.transition
        );
        1
    } else {
        report.exit_code()
    };

    let flushed = publish(roles.ledger, record.as_ref(), &report)?;
    crate::ctx::report_flush(&flushed);
    Ok(code)
}

/// `check --record` is a decision (GitHub ledger spec §2.2): flushed at the
/// end, whatever the verdict. A plain `check` names no record, decides
/// nothing, and stays local.
fn publish(
    ledger: &dyn Ledger,
    record: Option<&RecordId>,
    report: &TransitionReport,
) -> Result<Flushed> {
    let Some(record) = record else {
        return Ok(Flushed::NOTHING);
    };
    ledger
        .flush(fl_exec::decision::for_check(record, report))
        .map_err(|e| {
            anyhow::anyhow!(
                "refused: the check ran, but its decision could not be published to the shared \
                 ledger ({e}). Its runs are kept in the local store, and the next decision that \
                 reaches the ledger publishes them."
            )
        })
}
```

In `crates/cli/src/cmd/attempt.rs`, the imports (`:1-11`) become:

```rust
use crate::ctx::Ctx;
use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Args;
use fl_core::decision::Flushed;
use fl_core::ids::RecordId;
use fl_core::log::{Attempt, AttemptStatus, PathsTouched};
use fl_core::store::{Catalog, Ledger};
use fl_core::{Iri, Kind};
use fl_exec::adapters::ClaudeAdapter;
use fl_exec::runner::{AttemptSpec, Runner};
use fl_exec::stamp;
```

and `run` (`:47-119`) becomes, followed by `publish`:

```rust
pub fn run(ctx: &Ctx<'_>, cmd: Cmd) -> Result<i32> {
    let store = ctx.store;
    if cmd.adapter != "claude" {
        bail!(
            "`{}` is not a known adapter. Milestone 1 ships: {KNOWN_ADAPTERS}.",
            cmd.adapter
        );
    }
    let id = RecordId(refs::resolve(
        ctx.handles,
        &ctx.tracker_label,
        Kind::Record,
        &cmd.record,
    )?);
    let Some(record) = ctx.tracker.get_record(&id)? else {
        bail!(
            "`{}` is not a record in the store at {}. Run `fl record list --project <project>` \
             to see the ones that exist.",
            cmd.record,
            ctx.tracker_label
        );
    };
    let Some(project) = store.get_project(&record.project)? else {
        bail!(
            "record {} belongs to project {}, which no longer exists.",
            cmd.record,
            refs::show(ctx.handles, Kind::Project, record.project.iri())?
        );
    };

    let adapter = ClaudeAdapter::new(cmd.binary);
    let spec = AttemptSpec {
        project_root: std::path::PathBuf::from(&project.root),
        // The record's PRIMARY id, never the alias the person typed: every
        // attempt against one record must name it the same way.
        record: record.id.clone(),
        instruction: cmd.instruction.unwrap_or_else(|| record.title.clone()),
        timeout_secs: cmd.timeout_secs,
        budget_usd_micros: cmd.budget_usd_micros,
    };

    let rt = tokio::runtime::Runtime::new()?;
    let outcome = rt
        .block_on(adapter.attempt(spec))
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // ⚠ Recorded whatever the outcome. A crash, a timeout and a refusal all
    // cost something, even when that something is only the wall clock.
    let entry = stamp::entry_id();
    let attempt = Attempt {
        id: Some(entry.clone()),
        at: Some(stamp::now()),
        project: record.project,
        record: record.id,
        adapter: "claude".into(),
        status: outcome.status,
        duration_ms: outcome.duration_ms,
        tokens_in: outcome.tokens_in,
        tokens_out: outcome.tokens_out,
        cost_usd_micros: outcome.cost_usd_micros,
        paths_touched: PathsTouched::Listed(outcome.paths_touched.clone()),
        output_excerpt: Some(outcome.output_excerpt.clone()),
    };
    ctx.ledger.append_attempt(attempt.clone())?;

    println!("{}\t{}ms", outcome.status.as_wire(), outcome.duration_ms);
    if !outcome.output_excerpt.is_empty() {
        for line in outcome.output_excerpt.lines().take(40) {
            println!("\t| {line}");
        }
    }

    let flushed = publish(ctx.ledger, &entry, &attempt)?;
    crate::ctx::report_flush(&flushed);

    Ok(match outcome.status {
        AttemptStatus::Completed => 0,
        _ => 1,
    })
}

/// Publish the attempt after its local append (GitHub ledger spec §2.2).
/// It already ran and cost what it cost, so a failure here refuses nothing
/// (§7): the outcome is printed, the error is reported, and the attempt
/// goes out with the next flush that succeeds (decision 8).
fn publish(ledger: &dyn Ledger, id: &Iri, attempt: &Attempt) -> Result<Flushed> {
    ledger
        .flush(fl_exec::decision::for_attempt(id, attempt))
        .map_err(|e| {
            anyhow::anyhow!(
                "the attempt ran and is recorded in the local store, but it could not be \
                 published to the shared ledger ({e}). Nothing is lost: the next decision that \
                 reaches the ledger publishes it."
            )
        })
}
```

In `crates/cli/src/cmd/stats.rs`, the imports (`:1-8`) become:

```rust
use crate::refs::{self, Ref};
use anyhow::Result;
use clap::Args;
use fl_core::ids::ProjectId;
use fl_core::log::Attempt;
use fl_core::split::Coverage;
use fl_core::store::Ledger;
use fl_core::{Iri, Kind};
use fl_store::RedbStore;
use std::collections::BTreeMap;
```

and `run` (`:33-69`) becomes, followed by `report`:

```rust
pub fn run(store: &RedbStore, cmd: Cmd) -> Result<i32> {
    let project = ProjectId(refs::resolve(
        store,
        store.label(),
        Kind::Project,
        &cmd.project,
    )?);
    // Plan A: the local store is the whole ledger. Plan B reads through a
    // `SplitLedger` and passes the coverage it answers.
    let attempts = store.attempts(&project)?;
    for line in report(&attempts, &Coverage::Complete) {
        println!("{line}");
    }
    Ok(0)
}

/// The report's lines.
///
/// ⚠ The count is printed even when it is zero: a report that prints
/// nothing when it found nothing is indistinguishable from a report that
/// did not run. And a report over the local store alone says so (GitHub
/// ledger spec §2.5): a count that silently omitted GitHub would read as the
/// total.
fn report(attempts: &[Attempt], coverage: &Coverage) -> Vec<String> {
    let mut out = vec![format!("attempts: {}", attempts.len())];
    if let Coverage::LocalOnly { reason } = coverage {
        out.push(format!(
            "note: this covers the local store only, because {reason}"
        ));
    }
    if attempts.is_empty() {
        return out;
    }

    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    let mut total_cost = 0u64;
    let mut total_ms = 0u64;
    for a in attempts {
        *by_status.entry(a.status.as_wire().to_string()).or_default() += 1;
        total_cost += a.cost_usd_micros;
        total_ms += a.duration_ms;
    }
    for (status, n) in by_status {
        out.push(format!("  {status}: {n}"));
    }
    out.push(format!("wall clock: {total_ms}ms"));
    out.push(format!("cost: {total_cost} micro-USD"));
    if total_cost == 0 {
        out.push(
            "note: no adapter reported a cost, so the figure above is a floor and not a total"
                .to_string(),
        );
    }
    out
}
```

In `crates/cli/src/cmd/record.rs`, the move (`:121-122`) reports what its flush left local:

```rust
            let report =
                move_record(ctx.roles(), &record, state).map_err(|e| anyhow::anyhow!("{e}"))?;
            crate::ctx::report_flush(&report.flushed);
```

In `crates/cli/src/cmd/finding.rs`, the reproduce arm (`:172-173`, which Task 6 left as `(report, _flushed)`) and the verify arm (`:194-195`) do the same:

```rust
            let (report, flushed) = attach_reproduction(ctx.roles(), &fid, &gid)
                .map_err(|e| explain(e, &finding, Some(&gate)))?;
            crate::ctx::report_flush(&flushed);
```

```rust
            let report =
                verify_finding(ctx.roles(), &id).map_err(|e| explain(e, &finding, None))?;
            crate::ctx::report_flush(&report.flushed);
```

Append to `crates/cli/tests/github.rs` two tests that pin `check --record` in mode A (Review Focus 8). They pass before this task too — the handle already resolves to the issue URL — and stay green after it, now that `check` reads the record for its primary id:

```rust
/// A gate over `src/**/*.rs`, a transition `launch` (review → done) over
/// it, the manifest committed, and one record.
fn gated_record(g: &G) {
    g.project();
    g.fl()
        .args([
            "gate", "add", "--project", "1", "--name", "no-bug", "--glob", "src/**/*.rs",
            "--program", "./check.sh",
        ])
        .assert()
        .success();
    g.fl()
        .args([
            "transition", "add", "--project", "1", "--name", "launch", "--from", "review",
            "--to", "done", "--regret", "low", "--gate", "1",
        ])
        .assert()
        .success();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "work"])
        .assert()
        .success();
}

/// The runs the local store holds for the project's only gate.
fn runs_of_the_only_gate(g: &G) -> Vec<fl_core::GateRun> {
    use fl_core::store::{Catalog, Ledger};
    let store = fl_store::RedbStore::open(&g.home.path().join("fl.redb")).unwrap();
    let p = store.list_projects().unwrap()[0].id.clone();
    let gate = store.list_gates(&p).unwrap()[0].id.clone();
    store.gate_runs(&gate).unwrap()
}

// GitHub ledger spec §3.2 step 3: an entry is filed by its own record
// field, so every run of one record names it by the issue's primary URL.
#[test]
fn check_with_a_record_in_github_mode_ties_its_runs_to_the_issue() {
    let g = fixture();
    gated_record(&g);
    g.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let runs = runs_of_the_only_gate(&g);
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].record.as_ref().map(|r| r.iri().as_str().to_string()),
        Some("https://github.com/acme/widgets/issues/1".to_string())
    );
}

#[test]
fn check_with_a_record_github_deleted_is_refused_before_any_gate_runs() {
    let g = fixture();
    gated_record(&g);
    g.fake.state().issues.get_mut(&1).unwrap().gone = true;
    g.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2);
    assert!(runs_of_the_only_gate(&g).is_empty(), "no gate ran");
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace`
Expected: PASS — the 11 new unit tests, the two new `github.rs` tests, and every CLI black-box test (`attempt.rs`, `gates.rs`, `findings.rs`, `github.rs`, …) unchanged: in both configurations the bound ledger is the local store, whose flush does nothing.

- [ ] **Step 6: Mutation checks**

One at a time, confirm red, restore:
- In `Ctx::roles`, bind `ledger: self.store` → `the_roles_bind_the_ledger_the_command_was_given` FAILS.
- In `check::publish`, flush only when `report.passed()` → `a_failed_check_is_flushed_too` FAILS.
- In `check::publish`, flush with no record too (drop the `let … else`, building the decision from a placeholder record) → `a_plain_check_decides_nothing…` FAILS (the refusing ledger errors).
- In `check::publish`, swallow the flush error (`let _ =`) → `a_check_whose_decision_cannot_be_published…` FAILS.
- In `attempt::publish`, swallow the flush error → `an_attempt_that_cannot_be_published…` FAILS.
- In `stats::report`, drop the `LocalOnly` note → `a_local_only_report_says_so…` FAILS.
- In `flush_notes`, return no line for `OtherRepository` → `what_a_flush_left_local_is_reported_by_cause` FAILS.

- [ ] **Step 7: Trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/testing.rs crates/cli/src/ctx.rs crates/cli/src/main.rs \
  crates/cli/src/cmd/check.rs crates/cli/src/cmd/attempt.rs crates/cli/src/cmd/stats.rs \
  crates/cli/src/cmd/record.rs crates/cli/src/cmd/finding.rs crates/cli/tests/github.rs
git commit -m "feat(cli): commands record through the bound ledger

Ctx carries the ledger a project is bound to, and roles() hands it on;
in plan A it is the local store in every configuration. check --record
resolves the record's primary id and flushes its decision at the end,
pass or fail; a flush failure refuses the check with exit 2 and keeps
the runs. fl attempt appends its stamped attempt, prints the outcome,
then publishes; a failure there is reported, not a refusal. fl stats
says when it covers the local store only. Every flush's left-local
report is printed as a note. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 8a: `ledger_root` in the store and in manifest format 2; store format 4

**Files:**
- Modify: `crates/core/src/store.rs:63-69` (`FormatVersion`), `StoreError::LedgerRootChanged`, `Bindings` (`:395-398`), tests
- Modify: `crates/core/src/mem.rs` (`Inner`, `Bindings` `:418-434`)
- Modify: `crates/store/src/manifest.rs:16` (formats), `:19-46` (`LedgerRoot`, `Body`), `:54-58` (`Format`), `:115-153` (`export`), `:165-186` (`parse`), `:202-250` (`check_consistent`), tests
- Modify: `crates/store/src/lib.rs:8-14` (imports), `:43-55` (table, format), `:155-184` (`open`), `:446-457` (`export_manifest`), `:462-467` and `:612-620` (import), `:993-1016` (`Bindings`), tests
- Modify: `crates/cli/src/cmd/manifest.rs:155`, `:203` (the new argument)
- Modify: `docs/sharing-gates.md:47-51` (store format 4)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:

```rust
// fl_core::store
pub trait Bindings {
    fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError>;
    fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError>;
    fn ledger_root(&self, node_id: &str) -> Result<Option<String>, StoreError>;          // new
    fn set_ledger_root(&self, node_id: &str, commit: &str) -> Result<(), StoreError>;    // new; plan B's init calls it
}
StoreError::LedgerRootChanged { node_id: String, held: String, found: String }
StoreError::FormatVersion { found: Option<u64>, oldest: u64, newest: u64 }   // was { found, expected }

// fl_store::manifest
pub const MANIFEST_FORMAT: u64 = 2;
pub const MANIFEST_FORMAT_WITHOUT_LEDGER: u64 = 1;
pub struct LedgerRoot { pub repository_node_id: String, pub commit: String }
pub struct Body { /* existing */ pub ledger_root: Option<LedgerRoot> }   // skipped when None
pub fn export(catalog: &dyn Catalog, project: &ProjectId, commit: &str,
              exported_at_unix: u64, ledger_root: Option<LedgerRoot>) -> Result<Manifest, ManifestError>;

// fl_store
pub const FORMAT_WITH_LEDGER_ROOT: u64 = 4;
impl RedbStore {
    pub fn export_manifest(&self, project: &ProjectId, commit: &str, exported_at_unix: u64,
                           repository_node_id: Option<&str>) -> Result<Manifest, ManifestError>;
    pub fn holds_a_ledger_root(&self) -> Result<bool, StoreError>;
}
```

- [ ] **Step 1: Write the failing tests**

Append to `crates/core/src/store.rs`'s `mod tests`:

```rust
    // Spec §3.5: the anchor never changes. A different root for a
    // repository that has one is refused, the same one again is not.
    #[test]
    fn a_memory_store_records_a_ledger_root_once() {
        let s = MemStore::default();
        assert_eq!(s.ledger_root("R_1").unwrap(), None);
        s.set_ledger_root("R_1", "abc").unwrap();
        s.set_ledger_root("R_1", "abc").unwrap();
        let err = s.set_ledger_root("R_1", "def").unwrap_err();
        assert!(
            matches!(err, StoreError::LedgerRootChanged { ref held, ref found, .. } if held == "abc" && found == "def"),
            "{err:?}"
        );
        assert_eq!(s.ledger_root("R_1").unwrap().as_deref(), Some("abc"));
        assert_eq!(s.ledger_root("R_2").unwrap(), None);
    }
```

In `crates/store/src/manifest.rs`'s `mod tests`, extend `a_future_format_is_named_as_one` (`:356-364`) with

```rust
        assert!(err.to_string().contains("upgrade fl"), "{err}");
```

and append:

```rust
    /// `Body` and `Manifest` as format 1 was declared before format 2
    /// existed, frozen here: an older fl parses and hashes with exactly this.
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct OlderBody {
        format_version: u64,
        provenance: Provenance,
        project: ProjectId,
        gates: Vec<GateDef>,
        transitions: Vec<Transition>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct OlderManifest {
        body: OlderBody,
        content_sha256: String,
    }

    // ⚠ A project with no GitHub ledger must keep exporting exactly what an
    // older fl reads: same fields, same bytes, same hash.
    #[test]
    fn a_manifest_with_no_ledger_root_is_format_1_and_an_older_fl_reads_it() {
        let (s, p, _, _) = store();
        let m = export(&s, &p, "abc", 7, None).unwrap();
        assert_eq!(m.body.format_version, 1);
        let text = m.to_json();
        assert!(!text.contains("ledger_root"), "{text}");
        let old: OlderManifest =
            serde_json::from_str(&text).expect("an older fl parses a format-1 export");
        let bytes = serde_json::to_vec(&old.body).unwrap();
        let hash: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hash, old.content_sha256, "an older fl's hash check passes");
    }

    fn root() -> LedgerRoot {
        LedgerRoot {
            repository_node_id: "R_1".into(),
            commit: "abc123".into(),
        }
    }

    #[test]
    fn a_manifest_with_a_ledger_root_is_format_2_and_round_trips() {
        let (s, p, _, _) = store();
        let m = export(&s, &p, "abc", 7, Some(root())).unwrap();
        assert_eq!(m.body.format_version, 2);
        let back = Manifest::parse(&m.to_json()).unwrap();
        assert_eq!(back.body.ledger_root, Some(root()));
        assert_eq!(back, m);
    }

    #[test]
    fn a_root_on_format_1_or_no_root_on_format_2_is_refused() {
        let (s, p, _, _) = store();
        let mut m = export(&s, &p, "abc", 7, None).unwrap();
        m.body.ledger_root = Some(root());
        let err = rehashed(m).verify().unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");

        let mut m = export(&s, &p, "abc", 7, Some(root())).unwrap();
        m.body.ledger_root = None;
        let err = rehashed(m).verify().unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
    }
```

In `crates/store/src/lib.rs`'s `mod tests`, add `LedgerRoot` to the manifest import (`:1506`: `use crate::manifest::{LedgerRoot, Manifest, ManifestError, content_sha256};`) and append:

```rust
    // Ruling 12: a store a newer fl wrote says to upgrade, not to start over.
    #[test]
    fn a_store_from_a_newer_fl_says_to_upgrade_not_to_start_a_new_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.redb");
        {
            let db = redb::Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            tx.open_table(META)
                .unwrap()
                .insert(FORMAT_KEY, FORMAT_WITH_LEDGER_ROOT + 1)
                .unwrap();
            tx.commit().unwrap();
        }
        let msg = RedbStore::open(&path).err().unwrap().to_string();
        assert!(msg.contains("Upgrade fl"), "{msg}");
        assert!(!msg.contains("start a new store"), "{msg}");
    }

    // ⚠ An older fl would open a store at format 2 or 3 and export its
    // manifest WITHOUT the root; format 4 makes it refuse the store instead.
    #[test]
    fn a_ledger_root_is_recorded_once_and_raises_the_store_to_format_4() {
        use fl_core::store::Bindings;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        {
            let s = RedbStore::open(&path).unwrap();
            assert!(!s.holds_a_ledger_root().unwrap());
            assert_eq!(s.ledger_root("R_1").unwrap(), None);
            s.set_ledger_root("R_1", "abc").unwrap();
            s.set_ledger_root("R_1", "abc").unwrap();
            let err = s.set_ledger_root("R_1", "def").unwrap_err();
            assert!(
                matches!(err, StoreError::LedgerRootChanged { ref held, ref found, .. } if held == "abc" && found == "def"),
                "{err:?}"
            );
            assert!(s.holds_a_ledger_root().unwrap());
        }
        let db = redb::Database::open(&path).unwrap();
        let tx = db.begin_read().unwrap();
        let meta = tx.open_table(META).unwrap();
        assert_eq!(
            meta.get(FORMAT_KEY).unwrap().map(|v| v.value()),
            Some(FORMAT_WITH_LEDGER_ROOT),
            "an older fl must refuse this store rather than export its manifest without the root"
        );
        drop(meta);
        drop(tx);
        drop(db);
        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.ledger_root("R_1").unwrap().as_deref(), Some("abc"));
        assert_eq!(s.ledger_root("R_2").unwrap(), None);
    }

    #[test]
    fn an_import_never_lowers_the_format_of_a_store_that_records_a_ledger_root() {
        use fl_core::store::Bindings;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.redb");
        let (a, _ga, p, _, _) = authoring();
        {
            let b = RedbStore::open(&path).unwrap();
            b.set_ledger_root("R_9", "abc").unwrap();
            b.import_manifest(&a.export_manifest(&p, "c1", 7, None).unwrap(), "/x")
                .unwrap();
        }
        let db = redb::Database::open(&path).unwrap();
        let tx = db.begin_read().unwrap();
        let meta = tx.open_table(META).unwrap();
        assert_eq!(
            meta.get(FORMAT_KEY).unwrap().map(|v| v.value()),
            Some(FORMAT_WITH_LEDGER_ROOT)
        );
    }

    #[test]
    fn an_export_carries_the_root_of_the_repository_it_is_told_and_no_other() {
        use fl_core::store::Bindings;
        let (a, _ga, p, _, _) = authoring();
        a.set_ledger_root("R_1", "abc").unwrap();
        let m = a.export_manifest(&p, "c1", 7, Some("R_1")).unwrap();
        assert_eq!(m.body.format_version, 2);
        assert_eq!(
            m.body.ledger_root,
            Some(LedgerRoot {
                repository_node_id: "R_1".into(),
                commit: "abc".into(),
            })
        );
        for other in [None, Some("R_2")] {
            let m = a.export_manifest(&p, "c1", 7, other).unwrap();
            assert_eq!(
                (m.body.format_version, m.body.ledger_root),
                (1, None),
                "{other:?}"
            );
        }
    }

    #[test]
    fn an_import_records_the_manifests_ledger_root() {
        use fl_core::store::Bindings;
        let (a, _ga, p, _, _) = authoring();
        a.set_ledger_root("R_1", "abc").unwrap();
        let (b, _gb) = fresh();
        b.import_manifest(&a.export_manifest(&p, "c1", 7, Some("R_1")).unwrap(), "/x")
            .unwrap();
        assert_eq!(b.ledger_root("R_1").unwrap().as_deref(), Some("abc"));
    }

    #[test]
    fn an_import_naming_another_root_for_a_known_repository_is_refused_and_writes_nothing() {
        use fl_core::store::Bindings;
        let (a, _ga, p, _, _) = authoring();
        a.set_ledger_root("R_1", "abc").unwrap();
        let (b, _gb) = fresh();
        b.set_ledger_root("R_1", "other").unwrap();
        let err = b
            .import_manifest(&a.export_manifest(&p, "c1", 7, Some("R_1")).unwrap(), "/x")
            .unwrap_err();
        assert!(
            matches!(err, ManifestError::Store(StoreError::LedgerRootChanged { .. })),
            "{err}"
        );
        assert!(!b.owns(p.iri()).unwrap(), "nothing was written");
        assert_eq!(b.ledger_root("R_1").unwrap().as_deref(), Some("other"));
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: FAIL to compile — `ledger_root`, `set_ledger_root`, `LedgerRoot`, `FORMAT_WITH_LEDGER_ROOT`, the five-argument `export` and `FormatVersion`'s new fields do not exist.

- [ ] **Step 3: The core side**

In `crates/core/src/store.rs`, `FormatVersion` (`:63-69`) names the range this build reads, and tells a store from a newer fl — which needs an upgrade — from an older one — which has no migration (ruling 12):

```rust
    #[error("{}", format_version_message(*found, *oldest, *newest))]
    FormatVersion {
        found: Option<u64>,
        oldest: u64,
        newest: u64,
    },
```

with, after `follow` (`:167`):

```rust
/// `FormatVersion`'s message. A store from a NEWER fl is not damaged and
/// needs no new store: importing a manifest that carries a ledger root, for
/// one, raises a store to format 4, and the remedy on an older build is to
/// upgrade.
fn format_version_message(found: Option<u64>, oldest: u64, newest: u64) -> String {
    match found {
        Some(v) if v > newest => format!(
            "the store holds format {v}, which a newer fl wrote, and this version of fl reads \
             formats {oldest} to {newest}. Upgrade fl to open it: nothing is wrong with the store."
        ),
        _ => format!(
            "the store holds format {}, and this version of fl reads formats {oldest} to \
             {newest}. There is no migration: start a new store, or keep using the version of fl \
             that wrote this one.",
            match found {
                Some(v) => v.to_string(),
                None => "none (written before format versioning)".to_string(),
            }
        ),
    }
}
```


In `crates/core/src/store.rs`, add to `StoreError` after `Tampered`:

```rust
    /// ⚠ A ledger's anchor — its branch's first commit — never changes
    /// (GitHub ledger spec §3.5).
    #[error(
        "the GitHub ledger of repository node {node_id} is anchored at commit {held}, and this \
         names {found}. A ledger's first commit never changes: a different one means the ledger \
         was deleted and created again, or the manifest was edited. Find out which before \
         trusting either."
    )]
    LedgerRootChanged {
        node_id: String,
        held: String,
        found: String,
    },
```

and `Bindings` (`:392-398`) becomes:

```rust
/// What a local store remembers about the GitHub repositories a tracker is
/// bound to (GitHub tracker spec §2.4): the repository's `node_id`, keyed by
/// the configured `owner/repo`, compared without regard to case; and the
/// ledger root of each repository whose GitHub ledger it knows (GitHub
/// ledger spec §6.1 step 4), keyed by `node_id`.
pub trait Bindings {
    fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError>;
    fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError>;
    fn ledger_root(&self, node_id: &str) -> Result<Option<String>, StoreError>;
    /// ⚠ An anchor never changes: a different root for a `node_id` that has
    /// one is `LedgerRootChanged`; the same root again is a no-op.
    fn set_ledger_root(&self, node_id: &str, commit: &str) -> Result<(), StoreError>;
}
```

In `crates/core/src/mem.rs`, add to `Inner`:

```rust
    /// repository `node_id` → its GitHub ledger's first commit.
    ledger_roots: BTreeMap<String, String>,
```

and to `impl crate::store::Bindings for MemStore` (`:418-434`):

```rust
    fn ledger_root(&self, node_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self.inner.borrow().ledger_roots.get(node_id).cloned())
    }
    fn set_ledger_root(&self, node_id: &str, commit: &str) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        match s.ledger_roots.get(node_id) {
            Some(held) if held == commit => Ok(()),
            Some(held) => Err(StoreError::LedgerRootChanged {
                node_id: node_id.to_string(),
                held: held.clone(),
                found: commit.to_string(),
            }),
            None => {
                s.ledger_roots
                    .insert(node_id.to_string(), commit.to_string());
                Ok(())
            }
        }
    }
```

- [ ] **Step 4: The manifest**

In `crates/store/src/manifest.rs`, the format constant (`:16`) becomes:

```rust
/// Format 1: gates and transitions. Format 2 adds `ledger_root` (GitHub
/// ledger spec §6.1 step 4). An export writes 2 exactly when it carries a
/// root, so a project with no GitHub ledger still exports a manifest every
/// older fl reads.
pub const MANIFEST_FORMAT: u64 = 2;
pub const MANIFEST_FORMAT_WITHOUT_LEDGER: u64 = 1;
```

Add after `Provenance` (`:24`):

```rust
/// The first commit of the `fl/ledger` branch in the repository whose
/// `node_id` this names: every machine's anchor for the ledger's tamper
/// checks (GitHub ledger spec §3.5). The `node_id` lets an importing machine
/// key it without knowing the exporter's config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerRoot {
    pub repository_node_id: String,
    pub commit: String,
}
```

and a last field on `Body` (after `transitions`, `:38`):

```rust
    /// Format 2 only. ⚠ Skipped when absent, so a format-1 body serializes —
    /// and hashes — byte for byte as it did before format 2 existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_root: Option<LedgerRoot>,
```

`ManifestError::Format` (`:54-58`) says to upgrade:

```rust
    #[error(
        "the manifest is format {found}, and this version of fl reads formats \
         {MANIFEST_FORMAT_WITHOUT_LEDGER} to {MANIFEST_FORMAT}. A newer fl wrote it: upgrade fl \
         to read it"
    )]
    Format { found: u64 },
```

`export` (`:115-153`) takes the root and picks the format from it:

```rust
/// Every gate and transition of `project`, with pass marks cleared, and the
/// ledger root when the project's repository has one.
pub fn export(
    catalog: &dyn Catalog,
    project: &ProjectId,
    commit: &str,
    exported_at_unix: u64,
    ledger_root: Option<LedgerRoot>,
) -> Result<Manifest, ManifestError> {
    if catalog.get_project(project)?.is_none() {
        return Err(ManifestError::Inconsistent(format!(
            "{project} is held by this store, but it is not a project"
        )));
    }
    let mut gates = catalog.list_gates(project)?;
    for g in &mut gates {
        g.last_pass_commit = None;
    }
    gates.sort_by(|a, b| a.id.cmp(&b.id));
    let mut transitions = catalog.list_transitions(project)?;
    transitions.sort_by(|a, b| a.name.cmp(&b.name));
    let body = Body {
        format_version: if ledger_root.is_some() {
            MANIFEST_FORMAT
        } else {
            MANIFEST_FORMAT_WITHOUT_LEDGER
        },
        provenance: Provenance {
            commit: commit.to_string(),
            exported_at_unix,
        },
        project: project.clone(),
        gates,
        transitions,
        ledger_root,
    };
    let content_sha256 = content_sha256(&body)?;
    let m = Manifest {
        body,
        content_sha256,
    };
    // An export never writes a file its own parse would refuse — for
    // example a transition naming a gate of another project.
    m.check_consistent()?;
    Ok(m)
}
```

In `parse` (`:170-181`), accept both formats:

```rust
        match loose
            .pointer("/body/format_version")
            .and_then(|v| v.as_u64())
        {
            Some(v) if (MANIFEST_FORMAT_WITHOUT_LEDGER..=MANIFEST_FORMAT).contains(&v) => {}
            Some(found) => return Err(ManifestError::Format { found }),
            None => {
                return Err(ManifestError::Parse(
                    "it has no `body.format_version`".into(),
                ));
            }
        }
```

At the top of `check_consistent` (`:202`), before `let p = &self.body.project;`:

```rust
        let f = self.body.format_version;
        match (&self.body.ledger_root, f) {
            (None, MANIFEST_FORMAT_WITHOUT_LEDGER) | (Some(_), MANIFEST_FORMAT) => {}
            (None, _) => {
                return Err(ManifestError::Inconsistent(format!(
                    "it is format {f} and carries no ledger root; only format \
                     {MANIFEST_FORMAT} carries one, and every format {MANIFEST_FORMAT} manifest \
                     does"
                )));
            }
            (Some(_), _) => {
                return Err(ManifestError::Inconsistent(format!(
                    "it carries a ledger root but is format {f}; a manifest with a ledger root \
                     is format {MANIFEST_FORMAT}"
                )));
            }
        }
```

The eleven existing `export(&s, &p, "abc", 7)` calls in this file's tests gain a fifth argument:

```bash
sed -i 's/export(&s, &p, "abc", 7)/export(\&s, \&p, "abc", 7, None)/' crates/store/src/manifest.rs
```

- [ ] **Step 5: The store, and the two CLI calls that must still compile**

In `crates/store/src/lib.rs`, the imports (`:8-14`) gain `Bindings` and `LedgerRoot`:

```rust
use fl_core::store::{Bindings, Catalog, Handles, Ledger, StoreError, Tracker};
```

```rust
use crate::manifest::{LedgerRoot, Manifest, ManifestError};
```

(and the `impl fl_core::store::Bindings for RedbStore` line may now read `impl Bindings for RedbStore`). After `FORMAT_WITH_IMPORTS` (`:55`):

```rust
/// ⚠ The format of a store that records a ledger root. An older fl opens a
/// store at 2 or 3 and would export its manifest WITHOUT the root — the
/// anchor every other machine checks the ledger against — so recording one
/// raises the store to 4, which an older fl refuses. This build opens 2, 3
/// and 4. A format is only ever raised (`raise_format`).
pub const FORMAT_WITH_LEDGER_ROOT: u64 = 4;

/// repository `node_id` → the first commit of its `fl/ledger` branch
/// (GitHub ledger spec §6.1 step 4). Created by the first root recorded.
const LEDGER_ROOTS: TableDefinition<&str, &str> = TableDefinition::new("ledger_roots");
```

Next to `index_new` (after `:152`):

```rust
/// Raise the store's format to at least `to`, inside `tx`. Never lowers it:
/// an import into a store that records a ledger root must leave it at 4.
fn raise_format(tx: &redb::WriteTransaction, to: u64) -> Result<(), StoreError> {
    let mut meta = tx.open_table(META).map_err(backend)?;
    let now = meta
        .get(FORMAT_KEY)
        .map_err(backend)?
        .map(|v| v.value())
        .unwrap_or(FORMAT_VERSION);
    if now < to {
        meta.insert(FORMAT_KEY, to).map_err(backend)?;
    }
    Ok(())
}

/// ⚠ An anchor never changes (spec §3.5): a different root for a
/// `node_id` that has one is refused. Asked BEFORE any write, by both
/// callers of [`record_ledger_root`].
fn refuse_a_changed_root(
    node_id: &str,
    held: Option<String>,
    commit: &str,
) -> Result<(), StoreError> {
    match held {
        Some(h) if h != commit => Err(StoreError::LedgerRootChanged {
            node_id: node_id.to_string(),
            held: h,
            found: commit.to_string(),
        }),
        _ => Ok(()),
    }
}

/// Record `commit` as `node_id`'s ledger root inside `tx`, and raise the
/// store to [`FORMAT_WITH_LEDGER_ROOT`]. The caller has already refused a
/// changed root.
fn record_ledger_root(
    tx: &redb::WriteTransaction,
    node_id: &str,
    commit: &str,
) -> Result<(), StoreError> {
    tx.open_table(LEDGER_ROOTS)
        .map_err(backend)?
        .insert(node_id, commit)
        .map_err(backend)?;
    raise_format(tx, FORMAT_WITH_LEDGER_ROOT)
}
```

In `open` (`:173-183`), accept format 4, and name the range it reads:

```rust
        match found {
            // No META table at all: a brand-new file.
            None => Self::create_tables(&db)?,
            Some(Some(v))
                if v == FORMAT_VERSION
                    || v == FORMAT_WITH_IMPORTS
                    || v == FORMAT_WITH_LEDGER_ROOT => {}
            Some(v) => {
                return Err(StoreError::FormatVersion {
                    found: v,
                    oldest: FORMAT_VERSION,
                    newest: FORMAT_WITH_LEDGER_ROOT,
                });
            }
        }
```

The two existing tests that match `FormatVersion` follow the new fields: `a_format_1_store_is_refused_by_format_2` (`:1133-1140`) matches

```rust
                StoreError::FormatVersion {
                    found: Some(1),
                    oldest: 2,
                    newest: 4
                }
```

and `a_store_from_before_format_versioning_is_refused_with_a_remedy` (`:1454-1460`) matches

```rust
                StoreError::FormatVersion {
                    found: None,
                    oldest: FORMAT_VERSION,
                    ..
                }
```

`export_manifest` (`:446-457`):

```rust
    /// Export `project`, which this store must author. `repository_node_id`
    /// is the repository the project's ledger is in, when its binding names
    /// one: the export carries that repository's ledger root, if this store
    /// records one (GitHub ledger spec §6.1 step 4).
    pub fn export_manifest(
        &self,
        project: &ProjectId,
        commit: &str,
        exported_at_unix: u64,
        repository_node_id: Option<&str>,
    ) -> Result<Manifest, ManifestError> {
        self.check_kind(project.iri(), Kind::Project)?;
        if self.imported_hash(project)?.is_some() {
            return Err(ManifestError::NotAuthoring(project.clone()));
        }
        let ledger_root = match repository_node_id {
            None => None,
            Some(node) => self.ledger_root(node)?.map(|commit| LedgerRoot {
                repository_node_id: node.to_string(),
                commit,
            }),
        };
        manifest::export(self, project, commit, exported_at_unix, ledger_root)
    }

    /// Whether this store records any ledger root. An export that cannot
    /// tell which repository its project's ledger is in asks this before it
    /// writes a manifest without one.
    pub fn holds_a_ledger_root(&self) -> Result<bool, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(LEDGER_ROOTS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(e) => return Err(backend(e)),
        };
        let any = table.iter().map_err(backend)?.next().is_some();
        Ok(any)
    }
```

In `import_manifest`, refuse a changed root with everything else that can refuse — before anything is written, as its doc comment promises. After `let project = &body.project;` (`:467`):

```rust
        if let Some(root) = &body.ledger_root {
            refuse_a_changed_root(
                &root.repository_node_id,
                self.ledger_root(&root.repository_node_id)?,
                &root.commit,
            )?;
        }
```

and replace the unconditional format write (`:616-619`) with the root and a raise:

```rust
        if let Some(root) = &body.ledger_root {
            record_ledger_root(&tx, &root.repository_node_id, &root.commit)?;
        }
        raise_format(&tx, FORMAT_WITH_IMPORTS)?;
```

and add to `impl Bindings for RedbStore` (`:993-1016`):

```rust
    fn ledger_root(&self, node_id: &str) -> Result<Option<String>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(LEDGER_ROOTS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        let found = table
            .get(node_id)
            .map_err(backend)?
            .map(|v| v.value().to_string());
        Ok(found)
    }
    fn set_ledger_root(&self, node_id: &str, commit: &str) -> Result<(), StoreError> {
        refuse_a_changed_root(node_id, self.ledger_root(node_id)?, commit)?;
        let tx = self.db.begin_write().map_err(backend)?;
        record_ledger_root(&tx, node_id, commit)?;
        tx.commit().map_err(backend)
    }
```

The fourteen existing three-argument `export_manifest` calls in this file's tests gain `None`:

```bash
sed -i -E 's/export_manifest\(&p, "(c[0-9])", ([0-9])\)/export_manifest(\&p, "\1", \2, None)/' crates/store/src/lib.rs
```

A call the pattern misses is a compile error (the method takes four arguments), so the build is the check.

In `crates/cli/src/cmd/manifest.rs`, the two calls of `export_manifest` take the new argument; Task 8b replaces the second with the binding's node. `ensure_publishable`'s comparison (`:155`) compares gates and transitions only, and stays `None`:

```rust
            let now = store.export_manifest(project, "", 0, None)?;
```

```rust
            let m = store.export_manifest(&p, &head, now, None)?;
```

- [ ] **Step 6: The release note**

In `docs/sharing-gates.md`, after the paragraph on format 3 (`:47-51`), add:

```markdown
A store that records a GitHub ledger root is format 4. `fl github ledger init` records one, and
so does importing a manifest that carries one (manifest format 2, written for a project whose
GitHub ledger is switched on). An older `fl` refuses a format 4 store — it would otherwise export
the manifest without the root every other machine checks the ledger against. The remedy is to
upgrade `fl`: nothing is wrong with the store, and starting a new one would lose its history.
From this release on, `fl` itself says so when it meets a store newer than it reads.
```

- [ ] **Step 7: Run the tests**

Run: `cargo test --workspace`
Expected: PASS — the new core, manifest and store tests, and every existing manifest and CLI test (format-1 manifests export, parse, hash and import exactly as before).

- [ ] **Step 8: Mutation checks**

One at a time, confirm red, restore:
- Remove `skip_serializing_if` from `Body.ledger_root` → `a_manifest_with_no_ledger_root_is_format_1…` FAILS (the text carries `"ledger_root": null`, which `OlderManifest` refuses). No other test sees it: every hash stays self-consistent.
- Delete the format/root match in `check_consistent` → `a_root_on_format_1_or_no_root_on_format_2…` FAILS.
- In `parse`, accept only `MANIFEST_FORMAT` → `an_export_round_trips_through_its_file_form` FAILS (a format-1 export is refused).
- In `raise_format`, insert `to` unconditionally → `an_import_never_lowers_the_format…` FAILS (3, not 4).
- In `set_ledger_root`, drop `refuse_a_changed_root` → `a_ledger_root_is_recorded_once…` FAILS.
- In `import_manifest`, drop the root pre-check → `an_import_naming_another_root…` FAILS (the root is overwritten).
- Remove `FORMAT_WITH_LEDGER_ROOT` from `open`'s accepted formats → `a_ledger_root_is_recorded_once…` FAILS on the reopen.
- In `format_version_message`, drop the `v > newest` arm → `a_store_from_a_newer_fl_says_to_upgrade…` FAILS.
- In `MemStore::set_ledger_root`, overwrite → `a_memory_store_records_a_ledger_root_once` FAILS.

- [ ] **Step 9: Trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/store.rs crates/core/src/mem.rs crates/store/src/manifest.rs \
  crates/store/src/lib.rs crates/cli/src/cmd/manifest.rs docs/sharing-gates.md
git commit -m "feat(store): the ledger root — kept by node id, carried by manifest format 2

Bindings records each repository's ledger root, keyed by node_id, and
refuses a different one, before any write: an anchor never changes. A
store holding a root is format 4, so an older fl refuses it instead of
exporting its manifest without the root; this build says to upgrade
when it meets a newer store. A manifest is format 2 exactly when it
carries ledger_root (repository node_id and commit); a project with no
GitHub ledger still exports byte-for-byte format 1. Import records the
root. An unknown manifest format says to upgrade fl. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 8b: `manifest export` resolves the project's repository, or refuses

**Files:**
- Modify: `crates/cli/src/cmd/manifest.rs` (imports, `Binding`, `ledger_node`, `:188-234` `run`)
- Modify: `crates/cli/src/main.rs:470-482` (`entry_read`), `:578-589` (dispatch)
- Modify: `crates/cli/tests/manifest.rs` (imports, four tests)

**Interfaces:**
- Consumes: `Bindings::{bound_node_id, ledger_root, set_ledger_root}`, `RedbStore::{export_manifest, holds_a_ledger_root}` (Task 8a).
- Produces:

```rust
// crates/cli/src/cmd/manifest.rs
pub enum Binding { Unread, Local, Github(String) }
pub fn run(store: &RedbStore, cmd: Cmd, binding: &Binding) -> Result<i32>;
```

- [ ] **Step 1: Write the failing tests**

In `crates/cli/tests/manifest.rs`, add `use predicates::prelude::PredicateBooleanExt;` to the imports and append:

```rust
/// Put a ledger root for `node` into the store at `db`, bound to `repo`, as
/// `fl github ledger init` will (plan B).
fn with_ledger_root(db: &Path, repo: &str, node: &str, commit: &str) {
    use fl_core::store::Bindings;
    let s = fl_store::RedbStore::open(db).unwrap();
    s.bind_node_id(repo, node).unwrap();
    s.set_ledger_root(node, commit).unwrap();
}

// Spec §6.1 step 4: every export writes `ledger_root` from the store, and an
// import records it — how every machine gets its anchor.
#[test]
fn a_bound_projects_export_carries_its_ledger_root_and_an_import_records_it() {
    let m = Machine::new();
    let r = repo();
    let db = m.data.path().join("bound.redb");
    let root = r.path().canonicalize().unwrap();
    std::fs::create_dir_all(m.config.path().join("fl")).unwrap();
    std::fs::write(
        m.config.path().join("fl/config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\ntracker = {{ github = \"acme/widgets\", credential = \"env\" }}\n",
            root.display(),
            db.display()
        ),
    )
    .unwrap();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    with_ledger_root(&db, "acme/widgets", "R_1", "abc123");

    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("ledger_root\tabc123"));
    let text = std::fs::read_to_string(r.path().join(".fl/manifest.json")).unwrap();
    assert!(
        text.contains("\"format_version\": 2") && text.contains("\"repository_node_id\": \"R_1\""),
        "{text}"
    );
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "manifest"]);

    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success();
    use fl_core::store::Bindings;
    let imported = fl_store::RedbStore::open(&other.data.path().join("fl/fl.redb")).unwrap();
    assert_eq!(
        imported.ledger_root("R_1").unwrap().as_deref(),
        Some("abc123")
    );
}

// Spec §6.1 step 4: with --db the config entry is not read, so the export cannot
// know which repository's root belongs in it — and writing none would drop
// every machine's anchor.
#[test]
fn an_export_under_db_from_a_store_with_a_ledger_root_is_refused_and_writes_nothing() {
    let m = Machine::new();
    let r = repo();
    let db = m.data.path().join("s.redb");
    let dbs = db.display().to_string();
    m.fl(r.path())
        .args(["--db", &dbs, "project", "add", "."])
        .assert()
        .success();
    with_ledger_root(&db, "acme/widgets", "R_1", "abc123");

    m.fl(r.path())
        .args(["--db", &dbs, "manifest", "export", "--project", "1"])
        .assert()
        .code(2)
        .stderr(contains("records a GitHub ledger root").and(contains("without --db")));
    assert!(
        !r.path().join(".fl/manifest.json").exists(),
        "nothing was written"
    );
}

// Spec §6.1 step 4: a store that holds a root but no node for the name the
// config binds cannot tell which root is this project's either.
#[test]
fn an_export_whose_configured_repository_has_no_node_in_a_store_with_a_root_is_refused() {
    let m = Machine::new();
    let r = repo();
    let db = m.data.path().join("bound.redb");
    let root = r.path().canonicalize().unwrap();
    std::fs::create_dir_all(m.config.path().join("fl")).unwrap();
    std::fs::write(
        m.config.path().join("fl/config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\ntracker = {{ github = \"acme/widgets\", credential = \"env\" }}\n",
            root.display(),
            db.display()
        ),
    )
    .unwrap();
    m.fl(r.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    // A root for a repository the store knows under another name only.
    with_ledger_root(&db, "acme/gadgets", "R_1", "abc123");

    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .code(2)
        .stderr(contains("no repository node for `acme/widgets`").and(contains("fl github whoami")));
    assert!(
        !r.path().join(".fl/manifest.json").exists(),
        "nothing was written"
    );
}
```

Add a fourth: the command can land on another project's store when it names that project by IRI, and the current directory's binding is then not that project's:

```rust
// Spec §6.1 step 4: "for a store another IRI selected" the export cannot
// know the repository either — even when that store happens to hold a node
// under the current directory's configured name.
#[test]
fn an_export_of_a_project_in_another_projects_store_is_refused_when_that_store_holds_a_root() {
    use fl_core::store::Catalog;
    let m = Machine::new();
    let bound_repo = repo();
    let other_repo = repo();
    let a = m.data.path().join("a.redb");
    let b = m.data.path().join("b.redb");
    std::fs::create_dir_all(m.config.path().join("fl")).unwrap();
    std::fs::write(
        m.config.path().join("fl/config.toml"),
        format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\ntracker = {{ github = \"acme/widgets\", credential = \"env\" }}\n\n[[project]]\nroot = \"{}\"\nstore = \"{}\"\n",
            bound_repo.path().canonicalize().unwrap().display(),
            a.display(),
            other_repo.path().canonicalize().unwrap().display(),
            b.display()
        ),
    )
    .unwrap();
    m.fl(other_repo.path())
        .args(["project", "add", "."])
        .assert()
        .success();
    let other_project = {
        let s = fl_store::RedbStore::open(&b).unwrap();
        s.list_projects().unwrap()[0].id.iri().to_string()
    };
    with_ledger_root(&b, "acme/widgets", "R_9", "abc123");

    m.fl(bound_repo.path())
        .args(["manifest", "export", "--project", &other_project])
        .assert()
        .code(2)
        .stderr(contains("records a GitHub ledger root"));
    assert!(
        !other_repo.path().join(".fl/manifest.json").exists(),
        "nothing was written"
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p fl-cli --test manifest 2>&1 | tail -20`
Expected: `a_bound_projects_export_carries_its_ledger_root…` FAILS (the export writes format 1 — Task 8a passes `None`); the three refusal tests FAIL (the export succeeds without the root).

- [ ] **Step 3: The binding, the export, and `main.rs`**

In `crates/cli/src/cmd/manifest.rs`, add `use fl_core::store::Bindings;` to the imports, and before `run`:

```rust
/// What `manifest export` knows of the repository its project's ledger is
/// in (GitHub ledger spec §6.1 step 4) — or that it cannot know.
pub enum Binding {
    /// The config entry was not read (`--db`/`$FL_DB`), or the command runs
    /// on another project's store.
    Unread,
    /// The project's tracker is the local store: no GitHub ledger.
    Local,
    /// The project is bound to this `owner/repo`.
    Github(String),
}

/// The `node_id` whose ledger root an export writes.
///
/// ⚠ Every export writes the root from the store, so it cannot be dropped
/// (spec §6.1 step 4). When the store records a root and the export cannot
/// tell which repository is this project's — the entry was not read, or it
/// names a repository this store has no node for — it is refused rather
/// than written without one.
fn ledger_node(store: &RedbStore, binding: &Binding) -> Result<Option<String>> {
    match binding {
        Binding::Github(repo) => match store.bound_node_id(repo)? {
            Some(node) => Ok(Some(node)),
            None if store.holds_a_ledger_root()? => bail!(
                "this store records a GitHub ledger root, but no repository node for `{repo}`, \
                 the repository this project's config entry names, so fl cannot tell which \
                 root belongs in the manifest. Run a command that opens the GitHub tracker from \
                 the project's root — `fl github whoami` — which records the binding, then \
                 export again"
            ),
            None => Ok(None),
        },
        Binding::Local => Ok(None),
        Binding::Unread => {
            if store.holds_a_ledger_root()? {
                bail!(
                    "this store records a GitHub ledger root, and fl cannot tell which \
                     repository this project's ledger is in: with --db (or $FL_DB), or an IRI \
                     held by another project's store, it does not read the project's config \
                     entry. Run `fl manifest export` from the project's root, without --db and \
                     with $FL_DB unset, so the manifest keeps the root every other machine \
                     checks the ledger against"
                );
            }
            Ok(None)
        }
    }
}
```

`run` takes the binding (`:188`), and the export uses it (`:203`) and names the root it wrote (after the `wrote` line, `:230`):

```rust
pub fn run(store: &RedbStore, cmd: Cmd, binding: &Binding) -> Result<i32> {
```

```rust
            let node = ledger_node(store, binding)?;
            let m = store.export_manifest(&p, &head, now, node.as_deref())?;
```

(replacing Task 8a's `store.export_manifest(&p, &head, now, None)?`).

```rust
            println!("wrote\t{}\tsha256:{}", path.display(), m.content_sha256);
            if let Some(ledger_root) = &m.body.ledger_root {
                println!("ledger_root\t{}", ledger_root.commit);
            }
```

In `crates/cli/src/main.rs`, record whether the entry was read (`:470-482`):

```rust
    let needs_tracker = cli.command.needs_tracker();
    let explicit = explicit_db(cli.db);
    let entry_read = explicit.is_none() || needs_tracker;
```

```rust
    let entry = if entry_read {
        config::bound_entry(entries, &locus)?
    } else {
        None
    };
```

and, just before `match cli.command {` (`:578`):

```rust
    // `manifest export` writes the ledger root of the repository the
    // project's ledger is in (GitHub ledger spec §6.1 step 4). Only the
    // config entry says which, so it is known only when the entry was read
    // and the command runs on that entry's store.
    let manifest_binding = if !entry_read || path != bound {
        cmd::manifest::Binding::Unread
    } else {
        match &here_binding {
            Some(t) => cmd::manifest::Binding::Github(t.github.clone()),
            None => cmd::manifest::Binding::Local,
        }
    };
```

with the dispatch arm:

```rust
        Command::Manifest(c) => cmd::manifest::run(&store, c, &manifest_binding),
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS — the four new tests, and every existing manifest test: an unbound project's export is format 1 exactly as before.

- [ ] **Step 5: Mutation checks**

One at a time, confirm red, restore:
- In `ledger_node`, drop the `Unread` refusal → `an_export_under_db_from_a_store_with_a_ledger_root…` FAILS.
- In `ledger_node`, answer `Ok(None)` when `bound_node_id` is `None` whatever the store holds → `an_export_whose_configured_repository_has_no_node…` FAILS.
- In `main.rs`, bind `manifest_binding` to `Local` always → `a_bound_projects_export_carries_its_ledger_root…` FAILS.
- In `main.rs`, drop `|| path != bound` → `an_export_of_a_project_in_another_projects_store…` FAILS (the other store's node for `acme/widgets` puts its root in the wrong manifest).

- [ ] **Step 6: Trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/cmd/manifest.rs crates/cli/src/main.rs crates/cli/tests/manifest.rs
git commit -m "feat(cli): manifest export writes the ledger root, or refuses

The export resolves the project's repository through its config entry
and writes that repository's ledger root from the store. Under --db, for
a store another IRI selected, or when the configured name has no node in
a store that holds a root, it refuses rather than drop the anchor. The
root is named on stdout. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 9: The ownership rule — which records a repository owns, answered locally

**Files:**
- Create: `crates/github/src/owner.rs`
- Modify: `crates/github/src/lib.rs:4-9` (module)

**Interfaces:**
- Consumes: `meta::parse_issue_url` (`crates/github/src/meta.rs:400-411`), `Bindings::bound_node_id`.
- Produces:

```rust
// fl_github::owner — plan B's GithubLedger::owns_record answers with this
pub fn issue_of_repository(
    record: &Iri,
    current: &str,      // the bound repository's current full name, `owner/repo`
    node_id: &str,      // its node id
    memory: &dyn Bindings,
) -> Result<bool, StoreError>;
```

- [ ] **Step 1: Write the module with its tests**

The rule is spec §2.1's: "Ownership is a local check, with no network: the record's IRI names this binding's repository." A record's IRI in a GitHub-bound project is its issue URL, `https://github.com/{owner}/{repo}/issues/{n}` (`meta::parse_issue_url`). That names this repository when `owner/repo` is its current name, or a name this store bound to the same node — `GithubTracker::open` records the configured name (`crates/github/src/tracker.rs:246-255`), which a rename leaves behind. `GithubTracker::owner` (`:348`) also follows a redirect over the network; this rule never does, so an issue URL under a name this store never bound is "not owned" — skipped and reported by the flush, never lost (ruling 7).

Create `crates/github/src/owner.rs`:

```rust
//! Which records a repository owns, for the GitHub ledger (GitHub ledger
//! spec §2.1). Local: the record's IRI and what the local store remembers,
//! never a network call.

use crate::meta::parse_issue_url;
use fl_core::iri::Iri;
use fl_core::store::{Bindings, StoreError};

/// Whether `record` is an issue of the repository whose current full name
/// is `current` and whose node is `node_id`.
///
/// ⚠ The issue URL names its repository by `owner/repo`. That is this
/// repository when the name is its current one, without regard to case, or
/// a name this store has bound to the same node — the configured name kept
/// after a rename (GitHub tracker spec §2.4). Anything else is not owned:
/// another repository's URL, a URL under a name this store never bound (fl
/// claims no history it does not know), and any IRI that is not an issue
/// URL.
pub fn issue_of_repository(
    record: &Iri,
    current: &str,
    node_id: &str,
    memory: &dyn Bindings,
) -> Result<bool, StoreError> {
    let Some((name, _)) = parse_issue_url(record) else {
        return Ok(false);
    };
    if name.eq_ignore_ascii_case(current) {
        return Ok(true);
    }
    Ok(memory.bound_node_id(&name)?.as_deref() == Some(node_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;

    fn url(s: &str) -> Iri {
        Iri::parse(s).unwrap()
    }

    #[test]
    fn an_issue_under_the_current_name_is_owned_whatever_its_case() {
        let memory = MemStore::default();
        for record in [
            "https://github.com/acme/gadgets/issues/1",
            "https://github.com/Acme/Gadgets/issues/2",
        ] {
            assert!(
                issue_of_repository(&url(record), "acme/gadgets", "R_1", &memory).unwrap(),
                "{record}"
            );
        }
    }

    // A record raised before a rename keeps the old name in its URL; the
    // store bound that name to this repository's node when it was
    // configured.
    #[test]
    fn an_issue_under_a_name_this_store_bound_to_the_same_node_is_owned() {
        let memory = MemStore::default();
        memory.bind_node_id("acme/widgets", "R_1").unwrap();
        let old = url("https://github.com/acme/widgets/issues/1");
        assert!(issue_of_repository(&old, "acme/gadgets", "R_1", &memory).unwrap());
    }

    #[test]
    fn anything_else_is_not_owned() {
        let memory = MemStore::default();
        memory.bind_node_id("acme/other", "R_2").unwrap();
        for record in [
            // Another repository this store knows, under its own node.
            "https://github.com/acme/other/issues/1",
            // A name this store never bound: fl claims no history.
            "https://github.com/acme/unknown/issues/1",
            // Not an issue URL at all.
            "https://github.com/acme/gadgets/pull/1",
            "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b",
        ] {
            assert!(
                !issue_of_repository(&url(record), "acme/gadgets", "R_1", &memory).unwrap(),
                "{record}"
            );
        }
    }
}
```

In `crates/github/src/lib.rs`, add after `pub mod meta;`:

```rust
pub mod owner;
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p fl-github owner`
Expected: PASS, the 3 tests.

- [ ] **Step 3: Mutation checks**

One at a time, confirm red, restore:
- Compare the current name case-sensitively (`name == current`) → `an_issue_under_the_current_name…` FAILS on `Acme/Gadgets`.
- Drop the `bound_node_id` branch (`Ok(false)`) → `an_issue_under_a_name_this_store_bound…` FAILS.
- Answer `true` for any bound name, whatever its node (`is_some()`) → `anything_else_is_not_owned` FAILS on `acme/other`.
- Answer `true` for an IRI that is not an issue URL → `anything_else_is_not_owned` FAILS.

- [ ] **Step 4: Trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/owner.rs crates/github/src/lib.rs
git commit -m "feat(github): which records a repository owns, answered locally

A record belongs to the bound repository when its issue URL names the
repository's current name, or a name this store bound to the same node
— the configured name kept after a rename. No network: the GitHub
ledger's flush skips and reports what this rule does not own. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 10: GitHub's secondary rate limit is `RateLimited`

**Files:**
- Modify: `crates/github/src/fake.rs:48-157` (two knobs on `State`), `:522-529` (route)
- Modify: `crates/github/src/client.rs:279-288` (classification), new `reset_time`, tests

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:

```rust
// fl_github::fake::State — two one-shot knobs
pub secondary_rate_limit_next: Option<(u16, Option<u64>)>,   // (403 or 429, retry-after seconds)
pub forbidden_next: bool,                                    // a 403 that is not a rate limit
// client::send: a 403/429 whose message names GitHub's "secondary rate limit"
// is StoreError::RateLimited { reset }, with the reset time when a header gives one.
```

- [ ] **Step 1: The fake's knobs**

In `crates/github/src/fake.rs`, add to `State` after `delete_edits_on_next_patch` (`:156`):

```rust
    /// The next request answers with GitHub's secondary rate limit: this
    /// status (403 or 429), a `retry-after` of this many seconds when set,
    /// and the primary budget untouched (`x-ratelimit-remaining: 4999`).
    /// Modelled from GitHub's documentation; not measured. One-shot.
    pub secondary_rate_limit_next: Option<(u16, Option<u64>)>,
    /// The next request answers 403 for want of a permission — not a rate
    /// limit. One-shot.
    pub forbidden_next: bool,
```

and in `route`, after the GraphQL rate-limit arm (`:529`):

```rust
    if let Some((status, retry_after)) = s.secondary_rate_limit_next.take() {
        let mut a = answer(
            status,
            json!({"message": "You have exceeded a secondary rate limit. Please wait a few \
                   minutes before you try again. If you reach out to GitHub Support for help, \
                   please include the request ID 0000:0000:0000000:0000000:00000000."}),
        );
        a.headers
            .push(("x-ratelimit-remaining".into(), "4999".into()));
        a.headers
            .push(("x-ratelimit-reset".into(), "1700000000".into()));
        if let Some(secs) = retry_after {
            a.headers.push(("retry-after".into(), secs.to_string()));
        }
        return a;
    }
    if std::mem::take(&mut s.forbidden_next) {
        let mut a = answer(
            403,
            json!({"message": "Resource not accessible by personal access token"}),
        );
        a.headers
            .push(("x-ratelimit-remaining".into(), "4999".into()));
        return a;
    }
```

- [ ] **Step 2: Write the failing tests**

Append to `crates/github/src/client.rs`'s `mod tests`:

```rust
    // Spec §1.6: GitHub's secondary limit leaves `x-ratelimit-remaining`
    // above zero and may send no `retry-after`; only its message names it.
    // The primary `x-ratelimit-reset` is not its reset time.
    #[test]
    fn a_secondary_rate_limit_without_a_retry_after_is_rate_limited_not_a_refusal() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().secondary_rate_limit_next = Some((403, None));
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        match err {
            StoreError::RateLimited { ref reset } => {
                assert!(reset.contains("at least a minute"), "{reset}");
                assert!(!reset.contains("1700000000"), "the primary reset: {reset}");
            }
            other => panic!("a secondary rate limit answered {other:?}"),
        }
    }

    #[test]
    fn a_secondary_rate_limit_with_a_retry_after_names_it() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().secondary_rate_limit_next = Some((429, Some(60)));
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(
            matches!(err, StoreError::RateLimited { ref reset } if reset.contains("60 seconds")),
            "{err:?}"
        );
    }

    // The rule reads the message, not the status: a 403 for want of a
    // permission is a refusal, and says why.
    #[test]
    fn a_403_that_is_not_a_rate_limit_stays_a_refusal_naming_its_message() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().forbidden_next = true;
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(ref m) if m.contains("Resource not accessible")),
            "{err:?}"
        );
    }
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test -p fl-github client 2>&1 | tail -20` (the `fake` module is compiled for the crate's own tests)
Expected: `a_secondary_rate_limit_without_a_retry_after…` FAILS (`Backend("GitHub answered 403 …")`). `a_secondary_rate_limit_with_a_retry_after_names_it` FAILS too: today's classification (`client.rs:282-286`) prefers `x-ratelimit-reset` whenever it is present and names `1700000000 (unix seconds)` — the PRIMARY window's reset, which a secondary limit leaves untouched; §1.6 asks for the reset time the response gives, and for a secondary limit that is `retry-after`. `a_403_that_is_not_a_rate_limit…` PASSES already — it pins behaviour the change must keep (Step 5's mutation is its red evidence).

- [ ] **Step 4: Classify it**

In `crates/github/src/client.rs`, replace the classification (`:279-288`, from `let rate_limited =` through the `_ if rate_limited` arm) with:

```rust
    // ⚠ GitHub's SECONDARY rate limit (GitHub ledger spec §1.6): a 403 or
    // 429 whose message says so. It often leaves `x-ratelimit-remaining`
    // above zero and may carry no `retry-after`, so neither header alone
    // finds it. Modelled from GitHub's documentation — "You have exceeded a
    // secondary rate limit" — and not provoked in a live test, because doing
    // so would abuse the API.
    let secondary = matches!(status, 403 | 429)
        && message.to_ascii_lowercase().contains("secondary rate limit");
    let rate_limited = matches!(status, 403 | 429)
        && (secondary || remaining.as_deref() == Some("0") || retry_after.is_some());
    match status {
        _ if rate_limited => Err(StoreError::RateLimited {
            reset: reset_time(
                secondary,
                remaining.as_deref(),
                reset.as_deref(),
                retry_after.as_deref(),
            ),
        }),
```

(the remaining arms, `200..=399 | …`, `401` and `_`, stay as they are), and add after `send`:

```rust
/// When a rate limit lifts, from the headers that say so.
///
/// `x-ratelimit-reset` is the PRIMARY window's reset, and means something
/// only when that window is spent (`remaining` is `0`). A secondary limit
/// is lifted by `retry-after`, or — GitHub's documentation says — after at
/// least a minute when it gives no time.
fn reset_time(
    secondary: bool,
    remaining: Option<&str>,
    reset: Option<&str>,
    retry_after: Option<&str>,
) -> String {
    if remaining == Some("0")
        && let Some(r) = reset
    {
        return format!("{r} (unix seconds)");
    }
    if let Some(s) = retry_after {
        return format!("{s} seconds from now");
    }
    if secondary {
        return "at least a minute from now: GitHub gave no time for its secondary limit, and \
                its documentation says to wait at least one minute"
            .into();
    }
    "an unknown time".into()
}
```

- [ ] **Step 5: Run the tests, mutation checks**

Run: `cargo test -p fl-github`
Expected: PASS, including the 3 new tests and the existing `a_rate_limit_is_an_error_naming_the_reset` (the primary limit: `remaining` 0, reset `1700000000`).

One at a time, confirm red, restore:
- Drop `secondary ||` from `rate_limited` → `a_secondary_rate_limit_without_a_retry_after…` FAILS.
- Treat every 403/429 as secondary (drop the message test) → `a_403_that_is_not_a_rate_limit…` FAILS.
- In `reset_time`, return the `x-ratelimit-reset` whenever present (drop `remaining == Some("0") &&`) → `a_secondary_rate_limit_without_a_retry_after…` FAILS (names `1700000000`).

- [ ] **Step 6: Trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/client.rs crates/github/src/fake.rs
git commit -m "feat(github): the secondary rate limit is RateLimited, not a refusal

A 403 or 429 whose message names GitHub's secondary rate limit is
RateLimited, with retry-after when given and at least a minute when not;
the primary window's reset is named only when that window is spent. A
403 for want of a permission still refuses, quoting GitHub. Modelled
from GitHub's documentation; not provoked live. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

## After the last task

- [ ] Run the trio once more on the whole branch: `cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace`.
- [ ] Confirm no test reached the network: `grep -rn 'api.github.com' crates --include='*.rs'` lists only `DEFAULT_API` and ignored live tests.
- [ ] Follow WORKFLOW.md: `superpowers:requesting-code-review` on the whole branch, then a pull request against `main` (or stacked on the spec's branch). The pull request names this plan's rulings 1–16 for the owner, and the release note of Task 8a (a store that imports a format-2 manifest becomes format 4; an older `fl` then refuses it; the remedy is upgrading `fl`).

## Spec coverage (plan A's share)

| spec | where |
|---|---|
| decision 2 (what a published copy withholds; one withheld error text) | Task 1 (`WITHHELD_ERROR_DETAIL`, nullable fields), Task 3 (merge) |
| §1.2 `SplitLedger { local, github }` | Task 3 |
| §1.3 `id`, `at`; old entries never published; nullable excerpts; paths list or count | Task 1 (types), Tasks 3–4 (never published) |
| §1.4 `Ledger::flush`, `Flushed` | Task 2 (ruling 3) |
| §1.5 `ledger = "github"` | plan B (ruling 11) |
| §1.6 secondary rate limit | Task 10 |
| §2.1 routing: local at once; tied runs and attempts at the flush; untied never, and never scanned | Tasks 3–4 |
| §2.1 ownership: local, skipped and reported | Task 3 (`SplitLedger`), Task 4 (split case 8), Task 9 (the rule) |
| §2.1 cut-over, per repository `node_id` | Task 3 (`Outbox`, `MemStore`), Task 4 (`RedbStore`); plan B's `init` records it |
| §2.1 stranded attempt published by the next flush (decision 8) | Task 3 (`a_flush_that_fails…`), Task 4 (split case 4) |
| §2.2 evidence before state; failure refuses; refused decisions flushed; flush sites | Task 5 (move), Task 6 (findings), Task 7 (`check --record`, attempt) |
| §2.2 / §8.3 evidence before state confirmed by mutation | Task 5 Step 6, Task 6 Step 5 |
| §2.3 `Decision` | Task 2 (entry), Task 5 (composed from reports) |
| §2.4 pre-flight | plan B |
| §2.5 merged reads; same id different content = ERROR; withheld fields; unreachable = ERROR; stats local-only | Task 3; Task 7 (stats note) |
| §2.6 `check --record`, `fl attempt`, `Ctx::roles()` rewired; finding runs tagged | Tasks 6–7 |
| §2.6 `fl stats` binding rule | plan B (ruling 11) |
| §3.2 step 5 timeout then retry: no duplicate | Task 4 (split case 7; `MemRemote::lose_next_answer`) |
| §3.2 step 6 published marks keyed by `node_id`, written by `SplitLedger` | Tasks 3–4 |
| §6.1 step 4 `ledger_root` in store and manifest format 2; store format 4; import records it and refuses a different one; format 1 imports | Task 8a |
| §6.1 step 4 export resolves the repository through the binding, or refuses | Task 8b |
| §7 unknown manifest format names upgrading fl | Task 8a |
| §8.2 conformance: `SplitLedger` over `MemStore` and a remote double | Task 4 (plan B adds the fake GitHub fixture) |
