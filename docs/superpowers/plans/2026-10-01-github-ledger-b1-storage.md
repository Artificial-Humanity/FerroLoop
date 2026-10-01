# GitHub ledger, plan B1 — the storage side

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land everything plan A's final review said must precede binding `SplitLedger` (a bounded flush scan, typed refusals with an honest retry promise, decision 14, the batch a flush sends made visible, `LedgerRoot` validation), and build `fl_github::ledger::GithubLedger` — the `fl/ledger` branch in format 1, its reads with the seven tamper checks, its append protocol, decision 2's projection, quarantine, `verify`, `init` and the mode — as a library proven against the in-process fake GitHub, with no CLI binding yet.

**Architecture:** `fl-core` gains `LedgerFault` (one variant per remedy) inside `StoreError::Ledger`, `StoreError::{RestsOnLocalEntry, Contended}`, `StoreError::is_transient`, `Outbox::set_aside`, and `LedgerCache`/`LedgerMemory` — what a machine remembers of a branch (last head, segment cache). `fl-store` drops candidate rows when an entry is published or set aside, and keeps the cache in two additive tables. `fl-github` gains `ledger/` — `layout` (format 1, pure), `disclose` (decision 2, pure), `git` (the requests), `read` (head, snapshot, lines, checks 1–7), `append` (publish), `init` (setup, mode, guidance) and `verify` (history walk, quarantine) — and the fake gains git objects, the ledger's REST endpoints and its three GraphQL operations. `GithubLedger` implements plan A's `RemoteLedger`; the conformance suites run `SplitLedger` over it and the fake.

**Tech Stack:** Rust 2024 (`rust-version = "1.98"`), redb 4.3, serde/serde_json, ureq 3, base64 0.22, sha2 0.10 (a workspace dependency already; this plan adds it to `fl-github`). No new crates.

**Spec:** `docs/superpowers/specs/2026-09-30-github-ledger-design.md` at `ee2663e` — especially decisions 2, 8, 9, 10, 12 and 14, §1.1, §2.1, §2.5, §3 (all of it), §5, §6.1 steps 2–5, §6.2, §6.3, §7, §8.1, §8.3. Plan A (`2026-09-30-github-ledger-a-core.md`, merged) defines `SplitLedger`, `RemoteLedger`, `Outbox`, `Batch`, `Decision` and the rulings this plan builds on.

**Branch:** `ferris/github-ledger-b1`, off `ferris/github-ledger-plan-b` (main + spec decision 14), or off `main` once decision 14 is merged.

**Plan B2 (written after B1 merges) — intended scope:** the `ledger = "github"` config key (§1.5) and the binding of `SplitLedger { local: store, github: GithubLedger }` in `Ctx` (plan A ruling 11), sharing the tracker's `Client` (`GithubTracker::client`, added here); the pre-flight (§2.4) before any gate or adapter — visibility, mode, `GithubLedger::check_head`, `ensure_publishable(project, None)`, and a refusal naming `init` on a machine with no cut-over — cached per command; the commands `fl github ledger init | verify | quarantine | comment` over this plan's library calls (with init's confirmation prompt and `init::guidance`); decision comments (§4: rendered from the ledger, escaping, the 60,000-byte cap, the marker, recovery, posted after the state change; the fake's paginated comment listing); the `fl stats` binding rule; the mode in `fl github whoami`; printing `GithubLedger::take_notes`; the live tests of §8.4, which also confirm every shape this plan marks *Modelled* — by the names this plan's comments give them: `init_sets_up_a_ledger_on_a_private_repository`, `create_commit_on_branch_is_refused_when_the_head_moved`, `a_hand_edit_is_detected_and_named`, `rules_on_the_ledger_branch_are_readable`, `a_private_repository_without_a_ruleset_is_detection_only` (with §8.4's refused force update and deletion on a public throwaway); `docs/github-ledger.md` and its links (§9).

## Global Constraints

- Verification trio, all green before every commit: `cargo fmt --all --check`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`. CI runs the same as the `test, clippy, fmt` check.
- Unit tests live in `#[cfg(test)] mod tests` inside the module they test; black-box CLI tests live in `crates/cli/tests/`.
- **No test contacts the network.** GitHub is the in-process fake (`fl_github::fake::FakeGithub`, on `127.0.0.1`) or `conformance::MemRemote`.
- `fl-core` stays pure: "No IO, no async, no clock, no network" (`crates/core/src/lib.rs:1`).
- Spec values, verbatim: the branch is `fl/ledger`, "an orphan branch … and holds no `.github/`"; `format` holds "the text 1"; "A key is the first 32 hex digits of the SHA-256 of the IRI"; "Segments roll over at 256 KB" (262,144 bytes here); "Only the last segment of a directory grows; a closed segment never changes"; each line is "the entry with `id`, `at` and `by` (the GitHub identity that wrote it). No machine names"; "up to five tries"; "`internal` counts as not private".
- Spec invariants, verbatim: "Two machines appending at once cannot both land a commit on the same head; the one refused reads again and adds only what is missing. No entry is lost or duplicated."; "A reader that finds a `format` other than `1` refuses and says to upgrade fl. A new field in any entry means a new format."; "nothing is ever removed" (§3.6); "`gate_runs(gate)`: if GitHub cannot be read, an ERROR. Unreachable is not empty"; "the local store keeps every run"; "Every core feature must work on GitHub Free" (decision 12).
- Every existing configuration keeps working: B1 binds nothing in the CLI, so a local project and a mode-A project behave exactly as before. Every existing test passes, except the assertions this plan changes on purpose (named in each task).
- `snake_case` on every wire (`crates/core/src/wire.rs`).
- Every guard gets a mutation check: revert it, watch its test go red, restore. Each task lists its guards explicitly; say so in the commit message.
- Line numbers cite `ee2663e`. An earlier task's edits shift the lines a later task names: find the named item, not the number.
- An answer shape fl relies on that no live test has confirmed is marked in a comment: *Modelled — confirmed by live test `<name>`*. Every test so named is plan B2's to write; code comments carry no plan names.
- Commits are authored by the machine account (WORKFLOW.md) and end with `Co-authored-by: Ferris <Ferris@artificialhumanity.io>`. Stage explicit paths — never `git add -A`.
- The repository is public: no machine paths, host names or lab names in code, tests or messages. Test repositories are `acme/widgets` (and `acme/other` for a record another repository owns).

## Review Focus

1. **Another machine appends to the same directory between this machine's read and its commit.** GitHub refuses the stale commit; fl reads again and adds only what is missing — nothing lost, nothing twice. Task 10 (`two_flushes_racing_both_land_and_none_is_lost`, `two_machines_appending_at_once_all_land_once`).
2. **A commit whose answer was lost after it rolled a segment over.** The retry must find the lines in the closed segment as well as the open one, add nothing twice, and make no empty commit. Task 10 (`a_lost_answer_after_a_rollover_adds_nothing_twice`).
3. **GitHub's replica answering `behind` right after this machine's own append.** fl reads again and raises no alarm; a `behind` that persists is a rewrite. Task 9 (`a_head_behind_the_last_one_seen_is_read_again_before_it_counts`, `a_head_that_stays_behind_is_a_rewrite`).
4. **A batch published while the repository was private, sent again after it became public.** The ids are already there, so nothing is added and no commit is made. Task 10 (`a_batch_published_while_private_is_not_added_again_once_public`).
5. **An entry another repository owns, and a store that has published thousands of entries.** The first is reported by one flush only; the second scans only what is still waiting. Task 3 (`a_skipped_entry_is_reported_by_one_flush_only`, `a_published_or_set_aside_entry_leaves_the_candidate_index`).

## Rulings this plan makes

The spec is silent or ambiguous on these. Each says why, and what it costs if wrong.

1. **B1 is the library; every new CLI surface is B2's.** The `ledger = "github"` key with no binding behind it would let a person believe decisions publish when they stay local. *If wrong:* B2 holds one more small CLI task.
2. **Plan A's final-review checklist lands here, whole** (Tasks 1–4), so B2 binds `SplitLedger` onto finished foundations. Decision 14 rides with the retry wording (Task 2) because both are about what a failed publish says. *If wrong:* none; it is ordering.
3. **The waiting set is the store's, not a repository's.** `mark_published` and the new `set_aside` drop the entry's candidate row in the same write, so a flush scans only what waits (spec §2.1). `is_published` stays per repository. A store binds one tracker — the CLI already refuses a store two trackers share — so no second repository can be waiting for the same entry. *If wrong:* a store rebound to another repository would not re-offer entries the first already took; they predate the new repository's cut-over anyway.
4. **An entry skipped for another repository is set aside after the publish lands,** so the flush that reports it is the one whose report reaches the command. *If wrong:* a crash between publish and set-aside reports it once more.
5. **`LedgerFault` is one enum inside `StoreError::Ledger`, one variant per remedy,** and `StoreError::is_transient` is exactly `Unreachable | RateLimited | Contended`. Only a transient refusal promises that "the next decision … publishes them". *If wrong:* message text.
6. **Decision 14 supersedes plan A ruling 16:** `fl attempt` exits with the attempt's own code and prints the publish failure as `warning: …`.
7. **A decision is filed under its finding when it has one, else its record** (§3.1 "one directory per record or finding"; §4.1 comments a finding decision on the finding's issue). Check 6 compares against the same subject. *If wrong:* one function, `layout::decision_subject`.
8. **De-duplication reads the whole directory, not only "the file"** (§3.2 step 3). A lost answer whose commit rolled a segment over would otherwise duplicate the lines that landed in the closed segment. Closed segments are cached, so this costs no request. See spec defect 1.
9. **One compare per read.** The head is compared with the last head this machine saw when there is one, else with the anchor. Every recorded head was itself checked to descend from the anchor, so descent from it implies descent from the anchor (checks 1 and 2 together). *If wrong:* one more request per read.
10. **A read records its checked head as the last seen,** not only an append (§3.2 step 6 names the append). *If wrong:* a later read compares against an older head, which still descends.
11. **A line is encoded through `serde_json::Value` and read by round trip.** The parsed entry, serialized again, must equal the line without `by`; anything else — a field fl does not write, a value spelled differently — is unreadable (§3.1 "A new field in any entry means a new format"). *If wrong:* a looser reader accepts lines a newer format wrote.
12. **The segment cache lives in the local store** (`ledger_heads`, `ledger_segments`, additive tables keyed by `node_id`), and text is cached only after it passes checks 3–4. *If wrong:* a machine downloads its segments again each command.
13. **A lost answer is not an error to the caller.** `publish` reads again; if every line is there it returns the head that holds them. The conformance case `a_commit_whose_answer_was_lost_is_not_duplicated_by_the_next_flush` accepts either behaviour, since `MemRemote` reports the failure. *If wrong:* one more refused decision per lost answer.
14. **Five tries that each found the head moved is `StoreError::Contended`,** which is transient; five tries whose answers were lost or 5xx is `Unreachable`. §7 has no row for either. *If wrong:* message text.
15. **The mode is read from `rules/branches/fl/ledger` alone.** That endpoint lists only rules in force, so a disabled or evaluate-only ruleset shows as the rules missing, and the message says so; a 403 whose message says to upgrade means rulesets are unavailable on the plan. *Modelled — confirmed by live tests `rules_on_the_ledger_branch_are_readable` and `a_private_repository_without_a_ruleset_is_detection_only` (plan B2).* See spec defect 4.
16. **`init` records this machine's cut-over whenever it has none**, including where the ledger is already set up and the root came from an imported manifest. Recorded before the root, and never replaced. See spec defect 2.
17. **A line in the wrong directory may be quarantined,** as an unreadable one may; both messages name the quarantine command. **`quarantine.jsonl` cannot quarantine itself:** an unreadable quarantine line is `Altered` and names `verify`.
18. **`by` is the credential's identity** (`/user`'s login, or `<slug>[bot]` for the App), read once per `GithubLedger`. A quarantine line also carries `quarantined_by`, the name the person gave with `--by`.
19. **The `format` file holds `1\n`;** a reader accepts exactly `1` or `1\n`.
20. **A ledger root's shape** — the commit 40 or 64 lowercase hex digits, the node id 1 to 128 characters of `A-Za-z0-9_=+/-` starting with a letter — is checked by the manifest's consistency check (so at import, parse and export) and by `init`, through one function, `fl_core::store::ledger_root_shape`. `set_ledger_root` does not check it. *If wrong:* one more call site.
21. **Visibility is read once per `GithubLedger`,** which lives for one command and so one decision; B2's pre-flight reads it first.
22. **The unreadable-line error names the commit through GraphQL `blame`;** when blame cannot answer, the message says the commit is unknown rather than failing to report. *Modelled — confirmed by live test `a_hand_edit_is_detected_and_named` (plan B2).*
23. **`verify` walks first parents with `git/commits` and lists each tree with `git/trees?recursive=1`,** reporting the oldest bad commit; a merge or a second root is itself a bad commit.

## Spec defects found

1. §3.2 step 3 de-duplicates against "the file" only; see ruling 8.
2. §6.1 records the cut-over only on the machine that runs `init`. A second machine that imports the manifest has a root and no cut-over, so under plan A ruling 5 its flushes publish nothing, silently. Ruling 16 has `init` record it there; B2's pre-flight should refuse a machine with no cut-over and name `init`.
3. §3.1 says decisions go in "one directory per record or finding", while §3.5 check 6 compares a line's "record" with its directory. Ruling 7.
4. §6.2 asks fl to report "a ruleset that exists but is inactive", but the only endpoint the credential is expected to read lists active rules only. Ruling 15.
5. §2.3 says "a comment can be rendered from the ledger alone", but a `move` or `check` decision names transitions, not gates, so finding its runs under `runs/<gate-key>/` needs the local catalog's gate list. B2's comment recovery must read that list.
6. §7 has no row for a head that keeps moving (contention). Ruling 14.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/core/src/fault.rs` | create | `LedgerFault`: what is wrong with a shared ledger, one variant per remedy |
| `crates/core/src/store.rs` | modify | `StoreError::{Ledger, RestsOnLocalEntry, Contended}`, `is_transient`; `ledger_root_shape` |
| `crates/core/src/split.rs` | modify | typed refusals in `merge`/`flush`; stats fall back only when transient; `Outbox::set_aside`; `LedgerCache`, `CachedSegment`, `LedgerMemory` |
| `crates/core/src/mem.rs` | modify | `set_aside`, the store-wide waiting set, `LedgerCache` for `MemStore` |
| `crates/core/src/conformance.rs` | modify | `MemRemote::damage`, `RemoteControl::batches`, two split cases, the `ledger_cache` suite, the lost-answer case |
| `crates/core/src/lib.rs` | modify | modules and exports |
| `crates/exec/src/population.rs` | modify | `ExecError::PublishRefused`, `refused_publish` |
| `crates/exec/src/record.rs`, `finding.rs`, `journal.rs` | modify | the three flush sites; `Journal::refusing_with` |
| `crates/cli/src/cmd/check.rs`, `attempt.rs`, `testing.rs` | modify | the retry promise only when transient; decision 14; `Flushes::refusing_with` |
| `crates/store/src/lib.rs` | modify | candidate rows dropped; `set_aside`; `LedgerCache` tables; tests |
| `crates/store/src/manifest.rs` | modify | the root's shape in `check_consistent` |
| `crates/cli/tests/manifest.rs` | modify | well-formed roots |
| `crates/github/Cargo.toml` | modify | `sha2` |
| `crates/github/src/lib.rs` | modify | `ledger`, `fake_git` modules |
| `crates/github/src/client.rs` | modify | `GraphqlAnswer`, `graphql_answer`; a 403 names the permission |
| `crates/github/src/tracker.rs` | modify | `GithubTracker::client` |
| `crates/github/src/fake.rs` | modify | the ledger's knobs, the dispatch to `fake_git`, branch helpers |
| `crates/github/src/fake_git.rs` | create | the fake's git objects, REST endpoints and GraphQL operations |
| `crates/github/src/ledger/mod.rs` | create | `GithubLedger`, `RemoteLedger` for it |
| `crates/github/src/ledger/layout.rs` | create | format 1: keys, paths, lines, segments (pure) |
| `crates/github/src/ledger/disclose.rs` | create | decision 2's projection (pure) |
| `crates/github/src/ledger/git.rs` | create | the requests, each answer judged once |
| `crates/github/src/ledger/read.rs` | create | the checked head, snapshots, lines; checks 1–7 |
| `crates/github/src/ledger/append.rs` | create | the append protocol and `publish` |
| `crates/github/src/ledger/fixture.rs` | create (test-only) | `SplitLedger` over `GithubLedger` and the fake, for the conformance suites |
| `crates/github/src/ledger/init.rs` | create | `init`, the mode, the guidance text |
| `crates/github/src/ledger/verify.rs` | create | `verify` and `quarantine` |

---

### Task 1: Typed ledger refusals

**Files:**
- Create: `crates/core/src/fault.rs`
- Modify: `crates/core/src/store.rs:1-6` (imports), `:8-184` (`StoreError`), after `:224` (`is_transient`), tests at `:476-671`
- Modify: `crates/core/src/split.rs:10-17` (imports), `:162-169` (`merge`), `:195-209` (`attempts_for_stats`), `:293-303` (`flush`), tests `:443-457`, `:539-561`, `:755-775`
- Modify: `crates/core/src/conformance.rs:1371-1380` (`RemoteInner`), `:1380-1426` (`MemRemote`), `:1472-1495` (reads)
- Modify: `crates/core/src/lib.rs:3-37`

**Interfaces:**
- Consumes: plan A's `StoreError`, `SplitLedger`, `MemRemote`.
- Produces:

```rust
// fl_core::fault (re-exported as fl_core::LedgerFault)
pub enum LedgerFault {                       // Debug, Clone, PartialEq, Eq, thiserror::Error
    NotSetUp { repo: String },
    Deleted { repo: String, root: String },
    NoAnchor { repo: String },
    Rewritten { repo: String, head: String, against: &'static str, base: String, how: String },
    Altered { repo: String, file: String, what: String, commit: String },
    Misplaced { repo: String, file: String, line: u64, belongs: String, commit: String },
    Unreadable { repo: String, file: String, line: u64, commit: String, cause: String },
    UnknownFormat { repo: String, found: String },
    Unidentified { detail: String },
}

// fl_core::store::StoreError — three new variants
Ledger(LedgerFault),                          // #[from]
RestsOnLocalEntry { decision: Iri, entry: Iri },
Contended { store: String, tries: u32 },
impl StoreError { pub fn is_transient(&self) -> bool; } // Unreachable | RateLimited | Contended

// fl_core::conformance::MemRemote
pub fn damage(&self, on: bool);              // reads answer LedgerFault::Altered
```

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/fault.rs` with only its tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The variant's name. ⚠ An exhaustive match: a variant added to
    /// `LedgerFault` does not compile here until it is named, and then
    /// `every_fault` must carry a sample of it.
    fn variant(f: &LedgerFault) -> &'static str {
        match f {
            LedgerFault::NotSetUp { .. } => "not_set_up",
            LedgerFault::Deleted { .. } => "deleted",
            LedgerFault::NoAnchor { .. } => "no_anchor",
            LedgerFault::Rewritten { .. } => "rewritten",
            LedgerFault::Altered { .. } => "altered",
            LedgerFault::Misplaced { .. } => "misplaced",
            LedgerFault::Unreadable { .. } => "unreadable",
            LedgerFault::UnknownFormat { .. } => "unknown_format",
            LedgerFault::Unidentified { .. } => "unidentified",
        }
    }

    /// One sample of every variant, with the remedy its message must name.
    fn every_fault() -> Vec<(LedgerFault, &'static str)> {
        let repo = || "acme/widgets".to_string();
        vec![
            (LedgerFault::NotSetUp { repo: repo() }, "fl github ledger init"),
            (
                LedgerFault::Deleted {
                    repo: repo(),
                    root: "c0".into(),
                },
                "Restore the branch",
            ),
            (LedgerFault::NoAnchor { repo: repo() }, "fl manifest import"),
            (
                LedgerFault::Rewritten {
                    repo: repo(),
                    head: "c9".into(),
                    against: "the ledger's first commit",
                    base: "c0".into(),
                    how: "GitHub compares them as `diverged`".into(),
                },
                "fl github ledger verify",
            ),
            (
                LedgerFault::Altered {
                    repo: repo(),
                    file: "runs/k/1.jsonl".into(),
                    what: "changed after it was closed".into(),
                    commit: "c9".into(),
                },
                "fl github ledger verify",
            ),
            (
                LedgerFault::Misplaced {
                    repo: repo(),
                    file: "runs/k/1.jsonl".into(),
                    line: 3,
                    belongs: "urn:uuid:x".into(),
                    commit: "c9".into(),
                },
                "fl github ledger quarantine runs/k/1.jsonl 3",
            ),
            (
                LedgerFault::Unreadable {
                    repo: repo(),
                    file: "runs/k/1.jsonl".into(),
                    line: 3,
                    commit: "c9".into(),
                    cause: "not JSON".into(),
                },
                "fl github ledger quarantine runs/k/1.jsonl 3",
            ),
            (
                LedgerFault::UnknownFormat {
                    repo: repo(),
                    found: "2".into(),
                },
                "Upgrade fl",
            ),
            (
                LedgerFault::Unidentified {
                    detail: "a run".into(),
                },
                "fl github ledger verify",
            ),
        ]
    }

    // Spec §7: every error says what to do.
    #[test]
    fn every_fault_names_its_remedy() {
        let faults = every_fault();
        let named: std::collections::BTreeSet<&str> = faults.iter().map(|(f, _)| variant(f)).collect();
        assert_eq!(named.len(), 9, "one sample per variant: {named:?}");
        for (fault, remedy) in faults {
            let msg = fault.to_string();
            assert!(msg.contains(remedy), "{msg}");
        }
    }

    #[test]
    fn a_fault_names_the_file_line_and_commit_it_is_about() {
        let msg = LedgerFault::Unreadable {
            repo: "acme/widgets".into(),
            file: "runs/k/2.jsonl".into(),
            line: 7,
            commit: "c42".into(),
            cause: "not JSON".into(),
        }
        .to_string();
        for part in ["runs/k/2.jsonl", "line 7", "c42", "not JSON", "acme/widgets"] {
            assert!(msg.contains(part), "{part}: {msg}");
        }
    }
}
```

Append to the `tests` module of `crates/core/src/store.rs` (after `a_memory_store_records_a_ledger_root_once`, line 670):

```rust
    // Spec §7: only a GitHub that could not be reached, a spent rate limit,
    // or a ledger others kept appending to clears up by waiting. Every
    // other refusal names something to fix first.
    #[test]
    fn only_an_unreachable_a_rate_limited_or_a_contended_ledger_is_transient() {
        use crate::fault::LedgerFault;
        let transient = [
            StoreError::Unreachable {
                store: "s".into(),
                cause: "c".into(),
            },
            StoreError::RateLimited { reset: "r".into() },
            StoreError::Contended {
                store: "s".into(),
                tries: 5,
            },
        ];
        for e in &transient {
            assert!(e.is_transient(), "{e}");
        }
        let lasting = [
            StoreError::Backend("b".into()),
            StoreError::Credential("c".into()),
            StoreError::Tampered {
                id: seq_iri(1),
                detail: "d".into(),
            },
            StoreError::NotOwned {
                id: seq_iri(1),
                searched: vec![],
            },
            StoreError::RestsOnLocalEntry {
                decision: seq_iri(1),
                entry: seq_iri(2),
            },
            StoreError::Ledger(LedgerFault::NotSetUp {
                repo: "acme/widgets".into(),
            }),
        ];
        for e in &lasting {
            assert!(!e.is_transient(), "{e}");
        }
    }

    #[test]
    fn a_ledger_fault_reads_as_itself() {
        use crate::fault::LedgerFault;
        let fault = LedgerFault::NotSetUp {
            repo: "acme/widgets".into(),
        };
        let e: StoreError = fault.clone().into();
        assert_eq!(e.to_string(), fault.to_string());
    }
```

In `crates/core/src/split.rs`, replace the test `a_published_entry_with_no_id_is_a_backend_error` (lines 440-457) with:

```rust
    // Spec §1.3: every published entry carries an id (an entry with none is
    // never published); one that arrives with none anyway is damage, not a
    // legacy entry, and `merge` refuses it rather than silently keeping it.
    #[test]
    fn a_published_entry_with_no_id_is_a_ledger_fault() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let mut headless = sample_record_run(1, &g, Some(&r));
        headless.id = None;
        remote.insert_run(headless);

        let err = l.gate_runs(&g).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Unidentified { .. })),
            "{err:?}"
        );
    }
```

After `stats_fall_back_to_the_local_store_and_say_why` (line 561), add:

```rust
    // ⚠ Spec §2.5: local-only is for a GitHub that cannot be READ. A ledger
    // that was read and found damaged is an error: a report over the local
    // half would hide the damage.
    #[test]
    fn stats_refuse_a_damaged_ledger_rather_than_fall_back() {
        let (s, p, _g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        l.append_attempt(sample_attempt(1, &p, &r)).unwrap();
        remote.damage(true);
        let err = l.attempts_for_stats(&p).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Altered { .. })),
            "{err:?}"
        );
    }

    #[test]
    fn stats_fall_back_when_the_rate_limit_is_spent() {
        // `is_transient` is the rule; this pins that stats use it, not a
        // match on `Unreachable` alone.
        assert!(
            StoreError::RateLimited {
                reset: "soon".into()
            }
            .is_transient()
        );
    }
```

In `a_decision_resting_on_an_entry_that_is_not_being_published_is_refused_before_anything_is_sent` (lines 755-775), insert after the `let err = …;` statement:

```rust
        assert!(
            matches!(err, StoreError::RestsOnLocalEntry { ref entry, .. } if Some(entry) == untied.id.as_ref()),
            "{err:?}"
        );
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-core`
Expected: FAIL to compile — `LedgerFault`, `StoreError::Contended`, `StoreError::RestsOnLocalEntry`, `is_transient` and `MemRemote::damage` do not exist.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/core/src/fault.rs` (above its test module):

```rust
//! What is wrong with a shared ledger, one variant per remedy (GitHub
//! ledger spec §3.3, §3.5, §7). Every message says what to do.
//!
//! ⚠ None of these clears up by waiting: reading again reads the same
//! damage. `StoreError::is_transient` is false for every one.

/// A shared ledger that is not there, or not as fl wrote it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LedgerFault {
    #[error(
        "the repository {repo} has no GitHub ledger yet: it has no `fl/ledger` branch, and this \
         machine records no anchor for one. Run `fl github ledger init` to set it up"
    )]
    NotSetUp { repo: String },
    #[error(
        "the GitHub ledger of {repo} was deleted: its `fl/ledger` branch is gone, but this \
         machine records its first commit, {root}. fl will not start a new ledger, because a new \
         one would hide the deletion. Restore the branch at a commit that descends from {root}, \
         then retry"
    )]
    Deleted { repo: String, root: String },
    #[error(
        "this machine records no anchor for the GitHub ledger of {repo}, so it cannot check that \
         ledger's history. Import the manifest that `fl github ledger init` committed (`fl \
         manifest import`), or run `fl github ledger init` again"
    )]
    NoAnchor { repo: String },
    #[error(
        "the GitHub ledger of {repo} was rewritten: its head {head} does not descend from \
         {against}, {base} ({how}). Run `fl github ledger verify` to see where its history \
         departs, and find out who rewrote it before trusting it"
    )]
    Rewritten {
        repo: String,
        head: String,
        against: &'static str,
        base: String,
        how: String,
    },
    #[error(
        "the GitHub ledger of {repo} was altered: `{file}` {what} (seen at commit {commit}). A \
         ledger file only ever grows. Run `fl github ledger verify` to find the commit that \
         changed it, and find out who made it before trusting the ledger"
    )]
    Altered {
        repo: String,
        file: String,
        what: String,
        commit: String,
    },
    #[error(
        "the GitHub ledger of {repo} holds a line in the wrong place: `{file}` line {line} is \
         about {belongs}, which that directory does not hold (added by commit {commit}). Find out \
         who wrote it, then run `fl github ledger quarantine {file} {line} --by <name> --reason \
         <text>` so readers skip it; nothing is ever removed"
    )]
    Misplaced {
        repo: String,
        file: String,
        line: u64,
        belongs: String,
        commit: String,
    },
    #[error(
        "the GitHub ledger of {repo} holds an unreadable line: `{file}` line {line}, added by \
         commit {commit}: {cause}. Find out who wrote it, then run `fl github ledger quarantine \
         {file} {line} --by <name> --reason <text>` so readers skip it; nothing is ever removed"
    )]
    Unreadable {
        repo: String,
        file: String,
        line: u64,
        commit: String,
        cause: String,
    },
    #[error(
        "the GitHub ledger of {repo} is format `{found}`, and this version of fl reads format \
         1. Upgrade fl to read it"
    )]
    UnknownFormat { repo: String, found: String },
    #[error(
        "the shared ledger answered with an entry that has no id ({detail}). Every published \
         entry carries one, so the ledger was written by hand or is damaged. Run `fl github \
         ledger verify`"
    )]
    Unidentified { detail: String },
}

```

In `crates/core/src/store.rs`, add to the imports (after line 1):

```rust
use crate::fault::LedgerFault;
```

Inside `pub enum StoreError`, after the `SecurityNotPrivate` variant (line 183), add:

```rust
    /// ⚠ The shared ledger is not there, or not as fl wrote it (GitHub
    /// ledger spec §3.5, §7).
    #[error("{0}")]
    Ledger(#[from] LedgerFault),
    /// ⚠ A decision may rest only on entries it publishes or that are
    /// already published (plan A ruling 8).
    #[error(
        "decision {decision} rests on {entry}, which is neither being published nor published. \
         A run tied to no record, one recorded before the GitHub ledger was switched on, or one \
         tied to another repository's record stays local, so a decision cannot rest on it. \
         Nothing was published."
    )]
    RestsOnLocalEntry { decision: Iri, entry: Iri },
    /// ⚠ Others appended to the ledger before each of fl's tries landed
    /// (GitHub ledger spec §3.2 step 5). Nothing was lost.
    #[error(
        "{store} was appended to by someone else before each of fl's {tries} tries could land, \
         so fl's own append did not. Nothing was lost; retry"
    )]
    Contended { store: String, tries: u32 },
```

After `fn format_version_message` (ends line 224), add:

```rust
impl StoreError {
    /// Whether retrying later can succeed with nothing fixed first: GitHub
    /// could not be reached, its rate limit is spent, or others kept
    /// appending to the ledger. Only these may promise that the next
    /// decision publishes what this one could not (GitHub ledger spec §7).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            StoreError::Unreachable { .. }
                | StoreError::RateLimited { .. }
                | StoreError::Contended { .. }
        )
    }
}
```

In `crates/core/src/split.rs`, add to the imports (after line 11):

```rust
use crate::fault::LedgerFault;
```

Replace `merge`'s refusal of an id-less published entry (lines 163-169) with:

```rust
        let Some(id) = e.entry_id().cloned() else {
            return Err(LedgerFault::Unidentified {
                detail: "an entry read back from the shared ledger".into(),
            }
            .into());
        };
```

Replace `attempts_for_stats`'s match (lines 200-208) with:

```rust
        match self.github.attempts(project) {
            Ok(remote) => Ok((merge(local, remote)?, Coverage::Complete)),
            // ⚠ Only a GitHub that could not be READ falls back. A ledger
            // that was read and is damaged is an error: a count over the
            // local half would hide the damage.
            Err(e) if e.is_transient() => Ok((
                local,
                Coverage::LocalOnly {
                    reason: format!("GitHub could not be read: {e}"),
                },
            )),
            Err(e) => Err(e),
        }
```

Replace the `rests_on` refusal in `flush` (lines 293-303) with:

```rust
        for cited in &decision.rests_on {
            if !ids.contains(cited) && !self.local.is_published(&repo, cited)? {
                return Err(StoreError::RestsOnLocalEntry {
                    decision: decision.id.clone(),
                    entry: cited.clone(),
                });
            }
        }
```

In `crates/core/src/conformance.rs`, add to the imports (after line 15):

```rust
use crate::fault::LedgerFault;
```

Add a field to `RemoteInner` (after `fail_publish: bool,`, line 1373):

```rust
    damaged: bool,
```

Add to `impl MemRemote` (after `fail_publish`, line 1397):

```rust
    /// Every read answers that the ledger was altered — a damaged ledger,
    /// not an unreachable one — while publishing still works.
    pub fn damage(&self, on: bool) {
        self.inner.borrow_mut().damaged = on;
    }

    fn refuse_if_damaged(&self) -> Result<(), StoreError> {
        if self.inner.borrow().damaged {
            return Err(LedgerFault::Altered {
                repo: self.node_id.clone(),
                file: "runs/0/1.jsonl".into(),
                what: "was changed by the test".into(),
                commit: "commit-0".into(),
            }
            .into());
        }
        Ok(())
    }
```

In `impl RemoteLedger for MemRemote`, add `self.refuse_if_damaged()?;` right after `self.refuse_if_down()?;` in both `gate_runs` (line 1473) and `attempts` (line 1485).

In `crates/core/src/lib.rs`, add `pub mod fault;` after `pub mod decision;` (line 9), and after line 22 add:

```rust
pub use fault::LedgerFault;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fl-core`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Revert each guard, run `cargo test -p fl-core`, confirm the named test goes red, restore:

1. `is_transient`: drop the `Unreachable` arm → `only_an_unreachable_a_rate_limited_or_a_contended_ledger_is_transient` red (and `stats_fall_back_to_the_local_store_and_say_why`).
2. `is_transient`: drop the `RateLimited` arm → `only_an_unreachable…` and `stats_fall_back_when_the_rate_limit_is_spent` red.
3. `is_transient`: drop the `Contended` arm → `only_an_unreachable…` red.
4. `attempts_for_stats`: remove the `if e.is_transient()` guard so every error falls back → `stats_refuse_a_damaged_ledger_rather_than_fall_back` red.
5. `merge`: return `StoreError::Backend(..)` instead of `LedgerFault::Unidentified` → `a_published_entry_with_no_id_is_a_ledger_fault` red.
6. `flush`: return `StoreError::Backend(..)` instead of `RestsOnLocalEntry` → `a_decision_resting_on_an_entry_that_is_not_being_published…` red.
7. `LedgerFault`: delete "Upgrade fl" from `UnknownFormat`'s message → `every_fault_names_its_remedy` red.
8. `MemRemote::refuse_if_damaged`: return `Ok(())` always → `stats_refuse_a_damaged_ledger…` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/fault.rs crates/core/src/store.rs crates/core/src/split.rs crates/core/src/conformance.rs crates/core/src/lib.rs
git commit -m "feat(core): typed ledger refusals, and which of them clear up by waiting

LedgerFault names one remedy per variant; StoreError gains Ledger,
RestsOnLocalEntry and Contended, and is_transient (Unreachable,
RateLimited, Contended). fl stats falls back to the local store only for a
transient refusal, never for a damaged ledger. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 2: A refusal promises a retry only when one can work; decision 14

**Files:**
- Modify: `crates/exec/src/population.rs:20-29` (`ExecError`), end of file (`refused_publish`)
- Modify: `crates/exec/src/lib.rs:20` (export)
- Modify: `crates/exec/src/record.rs:1-3`, `:89-92`, tests
- Modify: `crates/exec/src/finding.rs:3`, `:216`, `:327`, tests
- Modify: `crates/exec/src/journal.rs:15-85`
- Modify: `crates/cli/src/cmd/check.rs:110-131`, tests
- Modify: `crates/cli/src/cmd/attempt.rs:112-197`
- Modify: `crates/cli/src/testing.rs:9-52`

**Interfaces:**
- Consumes: `StoreError::is_transient` (Task 1).
- Produces:

```rust
// fl_exec::population (re-exported at fl_exec::refused_publish)
ExecError::PublishRefused(String)            // new variant: no retry promise
pub fn refused_publish(e: fl_core::StoreError) -> ExecError; // Unpublished iff transient

// fl_exec::journal (test-only)
impl Journal<'_> { pub fn refusing_with(store: &MemStore, cause: fn() -> StoreError) -> Journal<'_>; }

// fl_cli::testing (test-only)
impl Flushes { pub fn refusing_with(cause: fn() -> StoreError) -> Flushes; }
```

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module of `crates/exec/src/record.rs`:

```rust
    // Spec §7: only an unreachable or rate-limited GitHub may be promised a
    // later publish. A ledger that refused for any other cause names what
    // to fix, and the refusal must not tell the person to wait it out.
    #[test]
    fn a_move_refused_for_a_cause_a_retry_cannot_cure_promises_no_retry() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());
        let j = Journal::refusing_with(&store, || StoreError::Tampered {
            id: fl_core::ids::seq_iri(99),
            detail: "edited".into(),
        });

        let err = match move_record(j.roles(), &record, State::Done) {
            Err(e) => e,
            Ok(_) => panic!("a move whose flush failed must be refused"),
        };

        assert!(matches!(err, ExecError::PublishRefused(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("nothing changed"), "{msg}");
        assert!(msg.contains("edited"), "the cause is named: {msg}");
        assert!(!msg.contains("next decision"), "{msg}");
        assert_eq!(
            store.get_record(&record.id).unwrap().unwrap().state,
            State::Todo
        );
    }
```

Append to the `tests` module of `crates/exec/src/finding.rs`:

```rust
    fn tampered() -> StoreError {
        StoreError::Tampered {
            id: fl_core::ids::seq_iri(99),
            detail: "edited".into(),
        }
    }

    // Spec §7: the reproduce and verify flush sites promise a retry only
    // for a transient refusal, like the move.
    #[test]
    fn a_reproduce_or_verify_refused_for_a_cause_a_retry_cannot_cure_promises_no_retry() {
        let d = repo();
        let s = MemStore::default();
        let p = s.add_project(&d.path().display().to_string()).unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let red = gate(&s, &p, d.path(), "red", "false");
        let f = s
            .add_finding(Finding::raise(p.clone(), r.clone(), "reviewer", "claim"))
            .unwrap();
        let j = Journal::refusing_with(&s, tampered);
        let err = attach_reproduction(j.roles(), &f, &red).unwrap_err();
        assert!(
            matches!(err, FindingExecError::Exec(ExecError::PublishRefused(_))),
            "{err}"
        );

        let rep = gate(&s, &p, d.path(), "reproduction", "false");
        let assigned_one = assigned(&s, &p, &r, &rep);
        set_program(&s, &rep, "true");
        let err = match verify_finding(j.roles(), &assigned_one) {
            Err(e) => e,
            Ok(_) => panic!("a verify whose flush failed must be refused"),
        };
        assert!(
            matches!(err, FindingExecError::Exec(ExecError::PublishRefused(_))),
            "{err}"
        );
        assert!(!err.to_string().contains("next decision"), "{err}");
    }
```

Add `StoreError` to the test module's import at `crates/exec/src/finding.rs:353`:

```rust
    use fl_core::store::{Catalog, Ledger, Roles, StoreError, Tracker};
```

Append to the `tests` module of `crates/cli/src/cmd/check.rs`:

```rust
    // Spec §7: only a transient refusal promises that the next decision
    // publishes the runs.
    #[test]
    fn a_check_refused_for_a_cause_a_retry_cannot_cure_promises_no_retry() {
        let ledger = Flushes::refusing_with(|| fl_core::store::StoreError::Tampered {
            id: seq_iri(99),
            detail: "edited".into(),
        });
        let err = publish(&ledger, Some(&RecordId(seq_iri(3))), &report(true)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("refused"), "{msg}");
        assert!(msg.contains("kept in the local store"), "{msg}");
        assert!(!msg.contains("next decision"), "{msg}");
    }

    #[test]
    fn a_check_that_could_not_reach_github_promises_the_next_decision_publishes_it() {
        let err = publish(
            &Flushes::refusing(),
            Some(&RecordId(seq_iri(3))),
            &report(true),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("next decision"), "{err:#}");
    }
```

In `crates/cli/src/cmd/attempt.rs`, replace the whole `tests` module (lines 146-197) with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Flushes;
    use fl_core::decision::Outcome;
    use fl_core::ids::{ProjectId, seq_iri};
    use fl_core::store::StoreError;

    fn attempt(status: AttemptStatus) -> Attempt {
        Attempt {
            id: Some(seq_iri(50)),
            at: None,
            project: ProjectId(seq_iri(1)),
            record: RecordId(seq_iri(2)),
            adapter: "claude".into(),
            status,
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
        assert_eq!(
            conclude(&ledger, &seq_iri(50), &attempt(AttemptStatus::Timeout)),
            None
        );
        let decisions = ledger.decisions.borrow();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].record, RecordId(seq_iri(2)));
        assert_eq!(decisions[0].rests_on, vec![seq_iri(50)]);
        assert_eq!(
            decisions[0].outcome,
            Outcome::Attempt {
                status: AttemptStatus::Timeout
            }
        );
    }

    // ⚠ Decision 14: an attempt that ran but could not be published exits
    // with the attempt's own code — never 2, which a script retries on —
    // and the publish failure is a warning.
    #[test]
    fn an_attempt_that_cannot_be_published_keeps_its_own_exit_code() {
        for (status, code) in [(AttemptStatus::Completed, 0), (AttemptStatus::Timeout, 1)] {
            assert_eq!(
                finish(&Flushes::refusing(), &seq_iri(50), &attempt(status)),
                code,
                "{status:?}"
            );
        }
    }

    // Spec §7 and decision 8: kept, not refused; and the next decision
    // publishes it only when the failure was transient.
    #[test]
    fn the_warning_promises_a_later_publish_only_when_one_can_work() {
        let w = conclude(&Flushes::refusing(), &seq_iri(50), &attempt(AttemptStatus::Timeout))
            .expect("a warning");
        assert!(w.contains("recorded in the local store"), "{w}");
        assert!(w.contains("next decision"), "{w}");
        assert!(!w.contains("refused:"), "not a refusal: {w}");

        let w = conclude(
            &Flushes::refusing_with(|| StoreError::Tampered {
                id: seq_iri(99),
                detail: "edited".into(),
            }),
            &seq_iri(50),
            &attempt(AttemptStatus::Timeout),
        )
        .expect("a warning");
        assert!(w.contains("recorded in the local store"), "{w}");
        assert!(!w.contains("next decision"), "{w}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-exec -p fl-cli --lib`
Expected: FAIL to compile — `Journal::refusing_with`, `ExecError::PublishRefused`, `Flushes::refusing_with`, `conclude` and `finish` do not exist.

- [ ] **Step 3: Write the implementation**

In `crates/exec/src/population.rs`, add after the `Unpublished` variant (line 29):

```rust
    /// ⚠ The decision's evidence was refused for a reason waiting will not
    /// cure (GitHub ledger spec §7): the cause names what to fix. The runs
    /// are kept in the local store.
    #[error(
        "refused: the evidence for this decision could not be published to the shared ledger, \
         so nothing changed: {0}. The runs are kept in the local store; fix what is named above \
         before deciding again"
    )]
    PublishRefused(String),
```

In `crates/exec/src/population.rs`, just above its `#[cfg(test)]` module (line 152), add:

```rust
/// How a failed flush refuses its decision (GitHub ledger spec §2.2, §7).
/// ⚠ Only a transient refusal — GitHub unreachable, rate limited, or
/// contended — promises that the next decision publishes the runs; any
/// other cause names what to fix first.
pub fn refused_publish(e: fl_core::StoreError) -> ExecError {
    if e.is_transient() {
        ExecError::Unpublished(e.to_string())
    } else {
        ExecError::PublishRefused(e.to_string())
    }
}
```

In `crates/exec/src/lib.rs`, replace line 20 with:

```rust
pub use population::{ChangedPaths, ExecError, refused_publish, resolve};
```

In `crates/exec/src/record.rs`, replace line 3 with:

```rust
use crate::population::{ExecError, refused_publish};
```

and replace lines 89-92 with:

```rust
    let flushed = roles
        .ledger
        .flush(decision::for_move(record, to, &transitions, allowed))
        .map_err(refused_publish)?;
```

In `crates/exec/src/finding.rs`, replace line 3 with:

```rust
use crate::population::{ExecError, refused_publish};
```

and replace both lines 216 and 327 (each reads `.map_err(|e| FindingExecError::Exec(ExecError::Unpublished(e.to_string())))?;`) with:

```rust
        .map_err(|e| FindingExecError::Exec(refused_publish(e)))?;
```

(`FindingExecError::Exec` still names the `ExecError` type, so that import stays.)

In `crates/exec/src/journal.rs`, replace lines 15-38 (the struct and the two constructors) with:

```rust
pub struct Journal<'a> {
    pub store: &'a MemStore,
    events: RefCell<Vec<&'static str>>,
    decisions: RefCell<Vec<Decision>>,
    /// What every flush fails with, when flushes fail.
    refuse_flush: Option<fn() -> StoreError>,
}

fn unreachable() -> StoreError {
    StoreError::Unreachable {
        store: "github:acme/widgets".into(),
        cause: "connection refused".into(),
    }
}

impl<'a> Journal<'a> {
    pub fn new(store: &'a MemStore) -> Self {
        Self {
            store,
            events: RefCell::new(vec![]),
            decisions: RefCell::new(vec![]),
            refuse_flush: None,
        }
    }

    /// Every flush fails as GitHub unreachable.
    pub fn refusing(store: &'a MemStore) -> Self {
        Self::refusing_with(store, unreachable)
    }

    /// Every flush fails with `cause()`.
    pub fn refusing_with(store: &'a MemStore, cause: fn() -> StoreError) -> Self {
        Self {
            refuse_flush: Some(cause),
            ..Self::new(store)
        }
    }
```

and replace `flush` (lines 71-84) with:

```rust
    fn flush(&self, decision: Decision) -> Result<Flushed, StoreError> {
        self.events.borrow_mut().push("flush");
        if let Some(cause) = self.refuse_flush {
            return Err(cause());
        }
        self.decisions.borrow_mut().push(decision);
        Ok(Flushed {
            commit: Some("c1".into()),
            left_local: vec![],
        })
    }
```

In `crates/cli/src/testing.rs`, replace lines 9-24 with:

```rust
/// A ledger that keeps every decision it is asked to flush — or, built with
/// [`Flushes::refusing`] or [`Flushes::refusing_with`], refuses every one.
#[derive(Default)]
pub struct Flushes {
    refuse: Option<fn() -> StoreError>,
    pub decisions: RefCell<Vec<Decision>>,
}

fn unreachable() -> StoreError {
    StoreError::Unreachable {
        store: "github:acme/widgets".into(),
        cause: "connection refused".into(),
    }
}

impl Flushes {
    /// Every flush fails as GitHub unreachable.
    pub fn refusing() -> Self {
        Self::refusing_with(unreachable)
    }

    /// Every flush fails with `cause()`.
    pub fn refusing_with(cause: fn() -> StoreError) -> Self {
        Self {
            refuse: Some(cause),
            ..Self::default()
        }
    }
}
```

and replace the refusal at the top of `flush` (lines 40-45) with:

```rust
        if let Some(cause) = self.refuse {
            return Err(cause());
        }
```

In `crates/cli/src/cmd/check.rs`, replace the `map_err` in `publish` (lines 123-130) with:

```rust
        .map_err(|e| {
            // ⚠ Only a transient refusal promises a later publish (spec §7).
            let after = if e.is_transient() {
                "Its runs are kept in the local store, and the next decision that reaches the \
                 ledger publishes them."
            } else {
                "Its runs are kept in the local store. Fix what is named above before deciding \
                 again."
            };
            anyhow::anyhow!(
                "refused: the check ran, but its decision could not be published to the shared \
                 ledger ({e}). {after}"
            )
        })
```

In `crates/cli/src/cmd/attempt.rs`, replace lines 112-144 (from `ctx.ledger.append_attempt(…)` to the end of `fn publish`) with:

```rust
    ctx.ledger.append_attempt(attempt.clone())?;

    println!("{}\t{}ms", outcome.status.as_wire(), outcome.duration_ms);
    if !outcome.output_excerpt.is_empty() {
        for line in outcome.output_excerpt.lines().take(40) {
            println!("\t| {line}");
        }
    }

    Ok(finish(ctx.ledger, &entry, &attempt))
}

/// Publish the attempt and give its exit code.
///
/// ⚠⚠ Decision 14: the attempt already ran and cost what it cost, so the
/// code is the attempt's own — 0 when it completed, 1 otherwise — whatever
/// became of its publish. Exit 2 means "refused" everywhere else, and a
/// script that retries on 2 must never run a paid attempt again.
fn finish(ledger: &dyn Ledger, id: &Iri, attempt: &Attempt) -> i32 {
    if let Some(warning) = conclude(ledger, id, attempt) {
        eprintln!("warning: {warning}");
    }
    match attempt.status {
        AttemptStatus::Completed => 0,
        _ => 1,
    }
}

/// Publish the attempt after its local append (GitHub ledger spec §2.2) and
/// report what stayed local. A failure refuses nothing (§7): it comes back
/// as the warning to print, and the attempt goes out with the next flush
/// that succeeds (decision 8).
fn conclude(ledger: &dyn Ledger, id: &Iri, attempt: &Attempt) -> Option<String> {
    match ledger.flush(fl_exec::decision::for_attempt(id, attempt)) {
        Ok(flushed) => {
            crate::ctx::report_flush(&flushed);
            None
        }
        Err(e) => {
            let after = if e.is_transient() {
                "Nothing is lost: the next decision that reaches the ledger publishes it."
            } else {
                "It stays there until the cause above is fixed; the first decision that reaches \
                 the ledger after that publishes it."
            };
            Some(format!(
                "the attempt ran and is recorded in the local store, but it could not be \
                 published to the shared ledger ({e}). {after}"
            ))
        }
    }
}
```

`Flushed` is no longer named in `attempt.rs`; remove `use fl_core::decision::Flushed;` (line 5).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fl-exec -p fl-cli`
Expected: PASS. (`a_move_whose_flush_fails_is_refused_and_the_record_stays`, `a_reproduction_whose_flush_fails…` and `a_verify_whose_flush_fails…` keep passing: `Journal::refusing` is still unreachable, so still `Unpublished`.)

- [ ] **Step 5: Mutation checks**

1. `refused_publish`: always return `Unpublished` → `a_move_refused_for_a_cause…`, `a_reproduce_or_verify_refused…` red.
2. `refused_publish`: always return `PublishRefused` → `a_move_whose_flush_fails_is_refused_and_the_record_stays` (asserts `Unpublished`) red.
3. `record.rs` flush site: revert to `.map_err(|e| ExecError::Unpublished(e.to_string()))` → `a_move_refused_for_a_cause…` red.
4. `finding.rs` reproduce site (`:216`): revert → `a_reproduce_or_verify_refused…` red at its first assertion.
5. `finding.rs` verify site (`:327`): revert → `a_reproduce_or_verify_refused…` red at its second assertion.
6. `check.rs`: always use the transient wording → `a_check_refused_for_a_cause…` red; always the other → `a_check_that_could_not_reach_github…` red.
7. `attempt.rs` `finish`: return `2` when `conclude` gave a warning → `an_attempt_that_cannot_be_published_keeps_its_own_exit_code` red.
8. `attempt.rs` `conclude`: always the transient wording → `the_warning_promises_a_later_publish_only_when_one_can_work` red.
9. `Journal::flush` / `Flushes::flush`: ignore `refuse_flush` → every `…refusing…` test red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/exec/src/population.rs crates/exec/src/lib.rs crates/exec/src/record.rs crates/exec/src/finding.rs crates/exec/src/journal.rs crates/cli/src/cmd/check.rs crates/cli/src/cmd/attempt.rs crates/cli/src/testing.rs
git commit -m "fix: promise a later publish only when waiting can work; decision 14

A flush refused for a transient cause (unreachable, rate limited,
contended) still says the next decision publishes the runs; any other
cause is ExecError::PublishRefused and names what to fix. fl attempt whose
publish fails now exits with the attempt's own code and prints a warning
(spec decision 14, superseding plan A ruling 16). Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---
### Task 3: The flush scans only what waits; a skipped entry is reported once

**Files:**
- Modify: `crates/core/src/split.rs:26-43` (`Outbox`), `:244-313` (`SplitLedger::flush`), tests after `:653`
- Modify: `crates/core/src/mem.rs:23-50` (`Inner`), `:411-450` (`Outbox for MemStore`), tests `:571-606`
- Modify: `crates/core/src/conformance.rs:141-150` (case count), `:201-224` (`split_ledger`), after `:874` (two cases), `:1337-1348` (`RemoteControl`), `:1371-1380`, `:1437-1470`, `:1497-1510` (`MemRemote`)
- Modify: `crates/store/src/lib.rs:231-271` (after `waiting`), `:1159-1168` (`mark_published`), after it (`set_aside`), tests `:1280`, `:1844-1870`, new test

**Interfaces:**
- Consumes: Task 1's `SplitLedger::flush`.
- Produces:

```rust
// fl_core::split::Outbox — one new method; mark_published's contract tightened
fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError>; // also leaves the waiting set
fn set_aside(&self, ids: &[Iri]) -> Result<(), StoreError>;                  // leaves it, unmarked

// fl_core::conformance::RemoteControl — one new method
fn batches(&self) -> Vec<Batch>;            // every batch publish was handed, in order
```

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/conformance.rs`, add to `RemoteControl` (after `fn remote`, line 1347):

```rust
    /// Every batch the remote side was handed to publish, in order — what
    /// each flush offered, which GitHub's own de-duplication would hide
    /// from every read.
    fn batches(&self) -> Vec<Batch>;
```

Add two cases after `a_pending_entry_of_another_repository_does_not_block_the_decision` (line 874):

```rust
// ⚠ Spec §2.1, §3.2 step 6: a published entry is marked, so no later flush
// offers it again. Seen in the batches, because GitHub's de-duplication
// would hide a missing mark from every read.
fn a_published_entry_is_never_offered_to_github_again(roles: &Bound<'_>, ctl: &dyn RemoteControl) {
    let (_p, g, r) = record_world(roles);
    let first = sample_record_run(1, &g, Some(&r));
    roles.ledger.append_gate_run(first.clone()).unwrap();
    roles
        .ledger
        .flush(sample_decision(1, &r, vec![first.id.clone().unwrap()]))
        .unwrap();
    let second = sample_record_run(2, &g, Some(&r));
    roles.ledger.append_gate_run(second.clone()).unwrap();
    roles
        .ledger
        .flush(sample_decision(2, &r, vec![second.id.clone().unwrap()]))
        .unwrap();
    let batches = ctl.batches();
    assert_eq!(batches.len(), 2, "one batch per flush");
    assert_eq!(run_ids(batches[0].runs.clone()), vec![first.id]);
    assert_eq!(
        run_ids(batches[1].runs.clone()),
        vec![second.id],
        "the first run was published and is not offered again"
    );
}

// ⚠ Spec §2.1: an entry another repository owns is reported by the flush
// that skips it, and by no later one.
fn a_skipped_entry_is_reported_by_one_flush_only(roles: &Bound<'_>, ctl: &dyn RemoteControl) {
    let (_p, g, r) = record_world(roles);
    let other = sample_record_run(1, &g, Some(&ctl.foreign_record()));
    roles.ledger.append_gate_run(other).unwrap();
    let first = roles.ledger.flush(sample_decision(1, &r, vec![])).unwrap();
    assert_eq!(first.left_local.len(), 1, "{first:?}");
    let second = roles.ledger.flush(sample_decision(2, &r, vec![])).unwrap();
    assert!(second.left_local.is_empty(), "{second:?}");
    assert!(
        ctl.batches().iter().all(|b| b.runs.is_empty()),
        "never published"
    );
}
```

Add both to the `cases` list in `split_ledger` (after line 210) and change the count at line 147:

```rust
const SPLIT_LEDGER_CASES: usize = 10;
```

In `crates/core/src/split.rs`'s `tests`, add after `a_pending_attempt_of_another_repository_is_skipped_and_reported_not_an_error` (line 653):

```rust
    // ⚠ Ruling 4: a skipped entry is set aside only by a flush that landed,
    // whose report reaches the command. A flush that failed reported
    // nothing, so the next one reports it.
    #[test]
    fn a_skipped_entry_whose_flush_failed_is_reported_by_the_next_one() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let theirs = remote.foreign_record();
        let elsewhere = sample_record_run(1, &g, Some(&theirs));
        l.append_gate_run(elsewhere.clone()).unwrap();
        remote.fail_publish(true);
        assert!(l.flush(sample_decision(1, &r, vec![])).is_err());
        remote.fail_publish(false);
        let flushed = l.flush(sample_decision(2, &r, vec![])).unwrap();
        assert_eq!(
            flushed.left_local,
            vec![LeftLocal::OtherRepository {
                entry: elsewhere.id.clone().unwrap(),
                record: theirs,
            }]
        );
    }
```

In `crates/core/src/mem.rs`, replace the last assertion of `unpublished_lists_only_record_tied_entries_after_the_cut_over_with_no_mark` (lines 592-603) with:

```rust
        let pending = s.unpublished("R_1", &entry_iri(3)).unwrap();
        assert_eq!(
            pending.runs,
            vec![waiting.clone(), late.clone()],
            "id order"
        );
        assert_eq!(pending.attempts, vec![attempt]);
        assert_eq!(
            s.unpublished("R_2", &entry_iri(3)).unwrap().runs,
            vec![waiting, late],
            "the waiting set is the store's: an entry published anywhere waits for no repository"
        );
        assert!(
            !s.is_published("R_2", marked.id.as_ref().unwrap()).unwrap(),
            "a mark is per repository"
        );
    }

    // Spec §2.1: an entry set aside waits no more, and is not published.
    #[test]
    fn an_entry_set_aside_waits_no_more_and_is_not_published() {
        use crate::conformance::{entry_iri, sample_record_run};
        use crate::split::Outbox;
        let (s, g, r, _p) = outbox_world();
        let aside = sample_record_run(4, &g, Some(&r));
        s.append_gate_run(aside.clone()).unwrap();
        s.set_aside(&[aside.id.clone().unwrap()]).unwrap();
        s.set_aside(&[aside.id.clone().unwrap()]).unwrap();
        assert!(s.unpublished("R_1", &entry_iri(0)).unwrap().runs.is_empty());
        assert!(!s.is_published("R_1", aside.id.as_ref().unwrap()).unwrap());
```

(The replaced block ended the test with `}`; the new block ends the new test's body — the original closing brace at line 604 now closes `an_entry_set_aside_waits_no_more_and_is_not_published`.)

In `crates/store/src/lib.rs`, change the test import at line 1280 to:

```rust
    use fl_core::conformance::{entry_iri, sample_attempt, sample_record_run};
```

replace line 1869 (`assert_eq!(s.unpublished("R_2", &entry_iri(0)).unwrap().runs.len(), 1);`) with:

```rust
        assert!(
            s.unpublished("R_2", &entry_iri(0)).unwrap().runs.is_empty(),
            "the waiting set is the store's: published anywhere, it waits nowhere"
        );
```

and add after `a_run_with_no_record_never_enters_the_candidate_index` (ends line 1897):

```rust
    // ⚠ Spec §2.1: the set a flush scans stays bounded by what is waiting —
    // a published or set-aside entry leaves the candidate index in the same
    // write that marks it.
    #[test]
    fn a_published_or_set_aside_entry_leaves_the_candidate_index() {
        let (s, _d) = fresh();
        let (p, g, r) = gate_and_record(&s);
        let published = sample_record_run(1, &g, Some(&r));
        let aside = sample_record_run(2, &g, Some(&r));
        let waiting = sample_record_run(3, &g, Some(&r));
        for run in [&published, &aside, &waiting] {
            s.append_gate_run(run.clone()).unwrap();
        }
        let attempt = sample_attempt(4, &p, &r);
        s.append_attempt(attempt.clone()).unwrap();
        s.mark_published(
            "R_1",
            &[published.id.clone().unwrap(), attempt.id.clone().unwrap()],
        )
        .unwrap();
        s.set_aside(&[aside.id.clone().unwrap()]).unwrap();

        let tx = s.db.begin_read().unwrap();
        let runs: Vec<String> = tx
            .open_table(CANDIDATE_RUNS)
            .unwrap()
            .iter()
            .unwrap()
            .map(|e| e.unwrap().0.value().to_string())
            .collect();
        assert_eq!(runs, vec![entry_iri(3).to_string()]);
        let attempts = tx
            .open_table(CANDIDATE_ATTEMPTS)
            .unwrap()
            .iter()
            .unwrap()
            .count();
        assert_eq!(attempts, 0);
        drop(tx);
        assert!(!s.is_published("R_1", aside.id.as_ref().unwrap()).unwrap());
        assert_eq!(
            s.unpublished("R_1", &entry_iri(0)).unwrap().runs,
            vec![waiting]
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-core -p fl-store`
Expected: FAIL to compile — `RemoteControl::batches` has no implementation for `MemRemote`, and `Outbox::set_aside` does not exist.

- [ ] **Step 3: Write the implementation**

In `crates/core/src/split.rs`, replace the `Outbox` trait (lines 26-43) with:

```rust
/// The local store's half of publishing (spec §2.1, §3.2 step 6), keyed by
/// the repository's `node_id`: its cut-over, and what it already has.
///
/// ⚠ The WAITING SET is the store's, not a repository's: an entry
/// published to one repository, or set aside, leaves it for every
/// repository. A store binds one tracker — the CLI refuses a store that two
/// trackers share — so no second repository waits for the same entry. The
/// published MARKS stay per repository (`is_published`).
pub trait Outbox {
    /// Every entry still waiting, with an id greater than `after`, tied to
    /// a record (every attempt is) — in id order. A run with no record is
    /// never listed (spec §2.1), nor is an entry with no id (§1.3).
    fn unpublished(&self, repo: &str, after: &Iri) -> Result<Pending, StoreError>;
    fn is_published(&self, repo: &str, id: &Iri) -> Result<bool, StoreError>;
    /// Idempotent. ⚠ In the same write, each id leaves the waiting set, so
    /// the set a flush scans stays bounded by what is actually waiting
    /// (spec §2.1).
    fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError>;
    /// Entries a flush skipped because another repository owns their record
    /// (spec §2.1): they leave the waiting set unmarked — reported once, by
    /// the flush that skipped them, and never offered again. Idempotent.
    fn set_aside(&self, ids: &[Iri]) -> Result<(), StoreError>;
    /// The id after which entries are publishable to `repo`, if its GitHub
    /// ledger was switched on.
    fn cutover(&self, repo: &str) -> Result<Option<Iri>, StoreError>;
    /// Recorded once, by `fl github ledger init`. ⚠ The same id again is a
    /// no-op; a different one is `CutoverChanged`.
    fn set_cutover(&self, repo: &str, id: &Iri) -> Result<(), StoreError>;
}
```

Replace `SplitLedger::flush` (lines 237-313, the doc comment included) with:

```rust
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
        let mut set_aside = Vec::new();
        let mut runs = Vec::new();
        for run in pending.runs {
            // `Outbox` lists only runs tied to a record, each with an id.
            let (Some(record), Some(id)) = (run.record.clone(), run.id.clone()) else {
                continue;
            };
            if self.github.owns_record(&record)? {
                runs.push(run);
            } else {
                set_aside.push(id.clone());
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
                set_aside.push(id.clone());
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
                return Err(StoreError::RestsOnLocalEntry {
                    decision: decision.id.clone(),
                    entry: cited.clone(),
                });
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
        // ⚠ Set aside only now, by the flush whose report reaches the
        // command: reported once, never offered again (spec §2.1).
        self.local.set_aside(&set_aside)?;
        Ok(Flushed { commit, left_local })
    }
```

In `crates/core/src/mem.rs`, add a field to `Inner` (after `published`, line 45):

```rust
    /// Every entry published to any repository, or set aside: it waits no
    /// more (spec §2.1).
    settled: BTreeSet<Iri>,
```

Replace the `waiting` closure in `unpublished` (lines 414-419) with:

```rust
        let waiting = |id: &Option<Iri>| {
            id.as_ref()
                .is_some_and(|id| id > after && !s.settled.contains(id))
        };
```

The `repo` parameter of `unpublished` is now unused in `MemStore`; name it `_repo` in that signature.

Replace `mark_published` (lines 444-450) with:

```rust
    fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        for id in ids {
            s.published.insert((repo.to_string(), id.clone()));
            s.settled.insert(id.clone());
        }
        Ok(())
    }

    fn set_aside(&self, ids: &[Iri]) -> Result<(), StoreError> {
        self.inner.borrow_mut().settled.extend(ids.iter().cloned());
        Ok(())
    }
```

In `crates/core/src/conformance.rs`, add a field to `RemoteInner` (after `decisions: Vec<Decision>,`, line 1378):

```rust
    batches: Vec<Batch>,
```

In `MemRemote::publish`, right after `let mut s = self.inner.borrow_mut();` (line 1439), add:

```rust
        s.batches.push(batch.clone());
```

and add to `impl RemoteControl for MemRemote` (after `fn remote`, line 1509):

```rust
    fn batches(&self) -> Vec<Batch> {
        self.inner.borrow().batches.clone()
    }
```

In `crates/store/src/lib.rs`, add after `fn waiting` (line 271):

```rust
/// Drop `ids` from the candidate index inside `tx`: published or set aside,
/// they wait no more, and no flush scans them again (spec §2.1).
fn drop_candidates(tx: &redb::WriteTransaction, ids: &[Iri]) -> Result<(), StoreError> {
    for index in [CANDIDATE_RUNS, CANDIDATE_ATTEMPTS] {
        let mut table = tx.open_table(index).map_err(backend)?;
        for id in ids {
            table.remove(id.as_str()).map_err(backend)?;
        }
    }
    Ok(())
}
```

Keep `waiting`'s check of `LEDGER_PUBLISHED`: a row marked before this change was never dropped, and the check still skips it.

Replace `mark_published` (lines 1159-1168) with:

```rust
    fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        {
            let mut table = tx.open_table(LEDGER_PUBLISHED).map_err(backend)?;
            for id in ids {
                table.insert((repo, id.as_str()), true).map_err(backend)?;
            }
        }
        // ⚠ The same write: a published entry waits no more.
        drop_candidates(&tx, ids)?;
        tx.commit().map_err(backend)
    }

    fn set_aside(&self, ids: &[Iri]) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        drop_candidates(&tx, ids)?;
        tx.commit().map_err(backend)
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fl-core -p fl-store -p fl-exec`
Expected: PASS — both stores run the ten split cases (`a_split_ledger_over_a_mem_store_meets_the_ledger_contracts`, `redb_store_meets_every_role_contract`).

- [ ] **Step 5: Mutation checks**

1. `SplitLedger::flush`: delete the `mark_published` call → `a_published_entry_is_never_offered_to_github_again` red (both stores).
2. `SplitLedger::flush`: delete the `set_aside` call → `a_skipped_entry_is_reported_by_one_flush_only` red (both stores).
3. `SplitLedger::flush`: move the `set_aside` call above `publish` → `a_skipped_entry_whose_flush_failed_is_reported_by_the_next_one` red.
4. `RedbStore::mark_published`: delete `drop_candidates` → `a_published_or_set_aside_entry_leaves_the_candidate_index` red.
5. `RedbStore::set_aside`: make it a no-op → the same test red, and `a_skipped_entry…` red for the redb split suite.
6. `MemStore::mark_published`: stop inserting into `settled` → `unpublished_lists_only…` red ("waits for no repository") and `a_published_entry_is_never_offered…` red for the memory suite.
7. `MemStore::set_aside`: make it a no-op → `an_entry_set_aside_waits_no_more…` red.
8. `MemRemote::publish`: drop `s.batches.push` → `a_published_entry_is_never_offered…` red ("one batch per flush").

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/split.rs crates/core/src/mem.rs crates/core/src/conformance.rs crates/store/src/lib.rs
git commit -m "fix(ledger): a flush scans only what waits, and reports a skip once

mark_published drops the entry's candidate row in the same write, and the
new Outbox::set_aside drops an entry another repository owns after the
flush that reported it lands. RemoteControl::batches lets the split suite
see what each flush offered, so a missing mark can no longer hide behind
GitHub's de-duplication. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 4: A ledger root must look like one; format 4 after an import

**Files:**
- Modify: `crates/core/src/store.rs` (after `Bindings`, line 474: `ledger_root_shape`), tests
- Modify: `crates/core/src/lib.rs:34-37` (export)
- Modify: `crates/store/src/manifest.rs:224-246` (`check_consistent`), tests `:445-480`
- Modify: `crates/store/src/lib.rs` tests `:2325-2378`
- Modify: `crates/cli/tests/manifest.rs:523-730`

**Interfaces:**
- Consumes: nothing new.
- Produces:

```rust
// fl_core::store (re-exported as fl_core::ledger_root_shape)
pub fn ledger_root_shape(node_id: &str, commit: &str) -> Result<(), String>;
```

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module of `crates/core/src/store.rs`:

```rust
    // Ruling 20: a root is the anchor every tamper check rests on, so a
    // value that cannot be one is refused where it enters.
    #[test]
    fn a_ledger_root_is_a_full_commit_id_and_a_github_node_id() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        for (node, commit) in [
            ("R_1", sha1),
            ("R_kgDOAbCdEf", sha256),
            ("MDEwOlJlcG9zaXRvcnkxMjk2MjY5", sha1),
            ("MDEwOlJlcG9zaXRvcnkx+/==", sha1),
        ] {
            assert_eq!(ledger_root_shape(node, commit), Ok(()), "{node} {commit}");
        }
        for (node, commit, names) in [
            ("R_1", "abc", "abc"),
            ("R_1", "0123456789ABCDEF0123456789ABCDEF01234567", "0123456789ABCDEF"),
            ("R_1", "0123456789abcdef0123456789abcdef0123456", "0123456789abcdef0123456"),
            ("R_1", "g123456789abcdef0123456789abcdef01234567", "g123"),
            ("", sha1, "``"),
            ("1R", sha1, "1R"),
            ("R 1", sha1, "R 1"),
            ("R_1;", sha1, "R_1;"),
        ] {
            let why = ledger_root_shape(node, commit).expect_err(node);
            assert!(why.contains(names), "{why}");
        }
        let long = format!("R{}", "a".repeat(128));
        assert!(ledger_root_shape(&long, sha1).is_err(), "129 characters");
    }
```

In `crates/store/src/manifest.rs`'s `tests`, replace `fn root()` (lines 452-457) with:

```rust
    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn root() -> LedgerRoot {
        LedgerRoot {
            repository_node_id: "R_1".into(),
            commit: COMMIT.into(),
        }
    }

    // Ruling 20: a root that cannot be one is refused by the consistency
    // check, so by parse, by import, and by export alike.
    #[test]
    fn a_ledger_root_that_cannot_be_one_is_refused_even_with_a_correct_hash() {
        let (s, p, _, _) = store();
        for (node, commit) in [("R_1", "abc123"), ("", COMMIT), ("1R", COMMIT)] {
            let mut m = export(&s, &p, "abc", 7, Some(root())).unwrap();
            m.body.ledger_root = Some(LedgerRoot {
                repository_node_id: node.into(),
                commit: commit.into(),
            });
            let err = Manifest::parse(&rehashed(m).to_json()).unwrap_err();
            assert!(
                matches!(err, ManifestError::Inconsistent(ref m) if m.contains("ledger root")),
                "{node} {commit}: {err}"
            );
            let err = export(
                &s,
                &p,
                "abc",
                7,
                Some(LedgerRoot {
                    repository_node_id: node.into(),
                    commit: commit.into(),
                }),
            )
            .unwrap_err();
            assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
        }
    }
```

In `crates/store/src/lib.rs`'s `tests`, add after `use crate::manifest::{…};` (line 1935):

```rust
    /// A well-formed ledger root (ruling 20).
    const ROOT_A: &str = "0123456789abcdef0123456789abcdef01234567";
```

and in the three tests `an_export_carries_the_root_of_the_repository_it_is_told_and_no_other`, `an_import_records_the_manifests_ledger_root` and `an_import_naming_another_root_for_a_known_repository_is_refused_and_writes_nothing` (lines 2325-2378) replace every `"abc"` that names a ledger root — `set_ledger_root("R_1", "abc")`, `commit: "abc".into()` and `Some("abc")` — with `ROOT_A` (`ROOT_A.into()` in the struct literal). Leave `export_manifest(&p, "c1", …)` alone: `"c1"` is the provenance commit, not a root.

Replace the body of `an_import_records_the_manifests_ledger_root` with:

```rust
        use fl_core::store::Bindings;
        let (a, _ga, p, _, _) = authoring();
        a.set_ledger_root("R_1", ROOT_A).unwrap();
        let (b, _gb) = fresh();
        b.import_manifest(&a.export_manifest(&p, "c1", 7, Some("R_1")).unwrap(), "/x")
            .unwrap();
        assert_eq!(b.ledger_root("R_1").unwrap().as_deref(), Some(ROOT_A));
        // ⚠ Plan A's checklist: an import that records a root makes the
        // store one an older fl refuses, not one it opens and exports
        // without the root.
        let tx = b.db.begin_read().unwrap();
        let meta = tx.open_table(META).unwrap();
        assert_eq!(
            meta.get(FORMAT_KEY).unwrap().map(|v| v.value()),
            Some(FORMAT_WITH_LEDGER_ROOT)
        );
```

In `crates/cli/tests/manifest.rs`, add above `fn with_ledger_root` (line 523):

```rust
/// A well-formed ledger root (plan B1 ruling 20).
const ROOT: &str = "0123456789abcdef0123456789abcdef01234567";
```

and replace each `"abc123"` in the file (lines 554, 560, 580, 597, 633, 679, 722) with `ROOT`, writing line 560 as:

```rust
        .stdout(contains(format!("ledger_root\t{ROOT}")));
```

and line 580 as:

```rust
        Some(ROOT)
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-core -p fl-store`
Expected: FAIL — `ledger_root_shape` does not exist; once it does, `a_ledger_root_that_cannot_be_one_is_refused…` fails until `check_consistent` calls it.

- [ ] **Step 3: Write the implementation**

In `crates/core/src/store.rs`, add after the `Bindings` trait (line 474):

```rust
/// Whether `node_id` and `commit` can be a ledger root (GitHub ledger spec
/// §6.1 step 4). `commit` is a full commit id: 40 lowercase hexadecimal
/// digits (SHA-1), or 64 (SHA-256). `node_id` is a GitHub node id: 1 to 128
/// of `A-Za-z0-9_=+/-`, starting with a letter.
///
/// ⚠ A root is the anchor every tamper check rests on (§3.5), so a value
/// that cannot be one is refused where it enters: the manifest's
/// consistency check and `init`. The `Err` says which part is wrong.
pub fn ledger_root_shape(node_id: &str, commit: &str) -> Result<(), String> {
    let hex = commit
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !(commit.len() == 40 || commit.len() == 64) || !hex {
        return Err(format!(
            "`{commit}` is not a full commit id: 40 or 64 lowercase hexadecimal digits"
        ));
    }
    let first = node_id.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
    let chars = node_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '=' | '+' | '/'));
    if !first || !chars || node_id.len() > 128 {
        return Err(format!(
            "`{node_id}` is not a GitHub node id: 1 to 128 letters, digits and `_-=+/`, \
             starting with a letter"
        ));
    }
    Ok(())
}
```

In `crates/core/src/lib.rs`, add `ledger_root_shape` to the `pub use store::{…}` list (lines 34-37):

```rust
pub use store::{
    Bindings, Catalog, CatalogChecked, Handles, KindRouted, Ledger, Roles, StoreError, Tracker,
    follow, ledger_root_shape,
};
```

In `crates/store/src/manifest.rs`, replace the `match` at the top of `check_consistent` (lines 226-242) with:

```rust
        match (&self.body.ledger_root, f) {
            (None, MANIFEST_FORMAT_WITHOUT_LEDGER) => {}
            (Some(root), MANIFEST_FORMAT) => {
                fl_core::ledger_root_shape(&root.repository_node_id, &root.commit).map_err(
                    |why| ManifestError::Inconsistent(format!("its ledger root cannot be one: {why}")),
                )?;
            }
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

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

1. `check_consistent`: delete the `ledger_root_shape` call → `a_ledger_root_that_cannot_be_one_is_refused…` red.
2. `ledger_root_shape`: accept any length → the `"abc"` and 39-digit cases red.
3. `ledger_root_shape`: accept uppercase (`is_ascii_hexdigit`) → the uppercase case red.
4. `ledger_root_shape`: drop the first-letter rule → the `""` and `"1R"` cases red.
5. `ledger_root_shape`: drop the character rule → the `"R 1"` and `"R_1;"` cases red.
6. `ledger_root_shape`: drop the 128 limit → the 129-character case red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/store.rs crates/core/src/lib.rs crates/store/src/manifest.rs crates/store/src/lib.rs crates/cli/tests/manifest.rs
git commit -m "fix(manifest): a ledger root must look like one; pin format 4 after import

ledger_root_shape refuses a root whose commit is not 40 or 64 lowercase hex
digits, or whose node id is not a GitHub node id; the manifest's
consistency check calls it, so parse, import and export refuse alike. The
import test now asserts the store is format 4. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 5: What a machine remembers of a GitHub ledger

**Files:**
- Modify: `crates/core/src/split.rs:10-17` (imports), after `:47` (`CachedSegment`, `LedgerCache`, `LedgerMemory`)
- Modify: `crates/core/src/mem.rs:6` (import), `:23-50` (`Inner`), after `:471` (`LedgerCache for MemStore`), `:528-536` (test)
- Modify: `crates/core/src/conformance.rs` (imports, `LEDGER_CACHE_CASES`, `ledger_cache`, two cases)
- Modify: `crates/core/src/lib.rs:32`
- Modify: `crates/store/src/lib.rs:8` (import), after `:84` (two tables), after `impl Bindings for RedbStore` (`LedgerCache`), tests

**Interfaces:**
- Consumes: nothing new.
- Produces:

```rust
// fl_core::split (re-exported at the crate root)
pub struct CachedSegment { pub oid: String, pub text: String, pub closed: bool } // Clone, Eq, serde
pub trait LedgerCache {
    fn last_head(&self, repo: &str) -> Result<Option<String>, StoreError>;
    fn set_last_head(&self, repo: &str, head: &str) -> Result<(), StoreError>;
    fn cached(&self, repo: &str, path: &str) -> Result<Option<CachedSegment>, StoreError>;
    fn cached_under(&self, repo: &str, dir: &str) -> Result<Vec<(String, CachedSegment)>, StoreError>;
    fn cache(&self, repo: &str, path: &str, segment: &CachedSegment) -> Result<(), StoreError>;
}
pub trait LedgerMemory: Bindings + LedgerCache + Outbox {}   // blanket impl

// fl_core::conformance
pub fn ledger_cache<S: LedgerCache, G>(make: impl Fn() -> (S, G));
```

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/conformance.rs`, add to the case counts (after line 153):

```rust
/// How many cases [`ledger_cache`] runs. Update deliberately — see [`run_suite`].
const LEDGER_CACHE_CASES: usize = 2;
```

and after `local_handles` (line 247):

```rust
/// What a local store remembers of a GitHub ledger, per repository `node_id`
/// (GitHub ledger spec §3.2 step 6, §3.3, §3.5 checks 2–4).
pub fn ledger_cache<S: LedgerCache, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        the_last_head_is_kept_per_repository_and_replaced::<S>,
        a_cached_file_reads_back_by_path_and_by_its_own_directory_only::<S>,
    ];
    run_suite("ledger-cache", LEDGER_CACHE_CASES, cases, make);
}

fn the_last_head_is_kept_per_repository_and_replaced<S: LedgerCache>(s: &S) {
    assert_eq!(s.last_head("R_1").unwrap(), None);
    s.set_last_head("R_1", "c1").unwrap();
    s.set_last_head("R_1", "c2").unwrap();
    assert_eq!(s.last_head("R_1").unwrap().as_deref(), Some("c2"));
    assert_eq!(s.last_head("R_2").unwrap(), None);
}

fn a_cached_file_reads_back_by_path_and_by_its_own_directory_only<S: LedgerCache>(s: &S) {
    let seg = |oid: &str, closed: bool| CachedSegment {
        oid: oid.into(),
        text: format!("{oid}\n"),
        closed,
    };
    assert_eq!(s.cached("R_1", "runs/aa/1.jsonl").unwrap(), None);
    assert!(s.cached_under("R_1", "runs/aa").unwrap().is_empty());
    s.cache("R_1", "runs/aa/1.jsonl", &seg("o1", true)).unwrap();
    s.cache("R_1", "runs/aa/2.jsonl", &seg("o2", false)).unwrap();
    s.cache("R_1", "runs/aab/1.jsonl", &seg("o3", false)).unwrap();
    s.cache("R_2", "runs/aa/1.jsonl", &seg("o4", false)).unwrap();
    s.cache("R_1", "runs/aa/2.jsonl", &seg("o5", true)).unwrap();
    assert_eq!(
        s.cached("R_1", "runs/aa/1.jsonl").unwrap(),
        Some(seg("o1", true))
    );
    assert_eq!(
        s.cached("R_1", "runs/aa/2.jsonl").unwrap(),
        Some(seg("o5", true)),
        "a second write replaces the first"
    );
    let under: Vec<(String, String)> = s
        .cached_under("R_1", "runs/aa")
        .unwrap()
        .into_iter()
        .map(|(p, c)| (p, c.oid))
        .collect();
    assert_eq!(
        under,
        vec![
            ("runs/aa/1.jsonl".to_string(), "o1".to_string()),
            ("runs/aa/2.jsonl".to_string(), "o5".to_string()),
        ],
        "not `runs/aab`, and not another repository's"
    );
}
```

In `crates/core/src/mem.rs`, add to `mem_store_meets_every_role_contract` (after line 535):

```rust
        crate::conformance::ledger_cache(|| (MemStore::default(), ()));
```

In `crates/store/src/lib.rs`, add to `redb_store_meets_every_role_contract` (after line 1830):

```rust
        fl_core::conformance::ledger_cache(fresh);
```

and add after `a_cut_over_is_recorded_once_and_survives_a_reopen`:

```rust
    #[test]
    fn the_ledger_cache_survives_a_reopen() {
        use fl_core::split::{CachedSegment, LedgerCache};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let seg = CachedSegment {
            oid: "o1".into(),
            text: "{}\n".into(),
            closed: false,
        };
        {
            let s = RedbStore::open(&path).unwrap();
            s.set_last_head("R_1", "c1").unwrap();
            s.cache("R_1", "runs/aa/1.jsonl", &seg).unwrap();
        }
        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.last_head("R_1").unwrap().as_deref(), Some("c1"));
        assert_eq!(s.cached("R_1", "runs/aa/1.jsonl").unwrap(), Some(seg));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-core -p fl-store`
Expected: FAIL to compile — `LedgerCache` and `CachedSegment` do not exist.

- [ ] **Step 3: Write the implementation**

In `crates/core/src/split.rs`, replace line 15 with:

```rust
use crate::store::{Bindings, Ledger, StoreError};
use serde::{Deserialize, Serialize};
```

and add after `impl<T: Ledger + Outbox> LocalLedger for T {}` (line 47):

```rust
/// One file of a GitHub ledger as this machine last read it (GitHub ledger
/// spec §3.3, §3.5 checks 3 and 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedSegment {
    /// The blob's object id when it was read.
    pub oid: String,
    pub text: String,
    /// Whether a later segment of its directory existed when it was read.
    /// ⚠ A closed segment never changes.
    pub closed: bool,
}

/// What this machine remembers of each repository's GitHub ledger, keyed
/// by the repository's `node_id` (spec §3.2 step 6, §3.3): the last head it
/// checked, and every file it read — so a closed segment is downloaded
/// once, and an altered one is caught.
pub trait LedgerCache {
    fn last_head(&self, repo: &str) -> Result<Option<String>, StoreError>;
    fn set_last_head(&self, repo: &str, head: &str) -> Result<(), StoreError>;
    fn cached(&self, repo: &str, path: &str) -> Result<Option<CachedSegment>, StoreError>;
    /// Every file cached under the directory `dir` (such as `runs/<key>`),
    /// with its path, in path order. Not a directory whose name merely
    /// starts with `dir`.
    fn cached_under(
        &self,
        repo: &str,
        dir: &str,
    ) -> Result<Vec<(String, CachedSegment)>, StoreError>;
    /// Replaces what was cached at `path`.
    fn cache(&self, repo: &str, path: &str, segment: &CachedSegment) -> Result<(), StoreError>;
}

/// Everything the GitHub ledger keeps on this machine: the repository
/// bindings and ledger roots, the cut-over and published marks, and the
/// branch cache.
pub trait LedgerMemory: Bindings + LedgerCache + Outbox {}
impl<T: Bindings + LedgerCache + Outbox> LedgerMemory for T {}
```

In `crates/core/src/lib.rs`, replace line 32 with:

```rust
pub use split::{
    Batch, CachedSegment, Coverage, LedgerCache, LedgerMemory, LocalLedger, Outbox, Pending,
    RemoteLedger, SplitLedger,
};
```

In `crates/core/src/conformance.rs`, replace line 22 with:

```rust
use crate::split::{Batch, CachedSegment, LedgerCache, Outbox, RemoteLedger, SplitLedger};
```

In `crates/core/src/mem.rs`, replace line 6 with:

```rust
use crate::split::{CachedSegment, LedgerCache, Outbox, Pending};
```

add to `Inner` (after `ledger_roots`, line 49):

```rust
    /// repository `node_id` → the last head of its ledger this machine checked.
    heads: BTreeMap<String, String>,
    /// (repository `node_id`, path on the branch) → that file as last read.
    segments: BTreeMap<(String, String), CachedSegment>,
```

and add after `impl Outbox for MemStore` (line 471):

```rust
impl LedgerCache for MemStore {
    fn last_head(&self, repo: &str) -> Result<Option<String>, StoreError> {
        Ok(self.inner.borrow().heads.get(repo).cloned())
    }

    fn set_last_head(&self, repo: &str, head: &str) -> Result<(), StoreError> {
        self.inner
            .borrow_mut()
            .heads
            .insert(repo.to_string(), head.to_string());
        Ok(())
    }

    fn cached(&self, repo: &str, path: &str) -> Result<Option<CachedSegment>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .segments
            .get(&(repo.to_string(), path.to_string()))
            .cloned())
    }

    fn cached_under(
        &self,
        repo: &str,
        dir: &str,
    ) -> Result<Vec<(String, CachedSegment)>, StoreError> {
        let prefix = format!("{dir}/");
        Ok(self
            .inner
            .borrow()
            .segments
            .iter()
            .filter(|((r, p), _)| r == repo && p.starts_with(&prefix))
            .map(|((_, p), c)| (p.clone(), c.clone()))
            .collect())
    }

    fn cache(&self, repo: &str, path: &str, segment: &CachedSegment) -> Result<(), StoreError> {
        self.inner
            .borrow_mut()
            .segments
            .insert((repo.to_string(), path.to_string()), segment.clone());
        Ok(())
    }
}
```

In `crates/store/src/lib.rs`, replace line 8 with:

```rust
use fl_core::split::{CachedSegment, LedgerCache, Outbox, Pending};
```

add after `LEDGER_ROOTS` (line 84):

```rust
/// repository `node_id` → the last head of its GitHub ledger this machine
/// checked (GitHub ledger spec §3.2 step 6). Additive.
const LEDGER_HEADS: TableDefinition<&str, &str> = TableDefinition::new("ledger_heads");
/// (repository `node_id`, path on the branch) → that file as last read, as
/// JSON (`CachedSegment`). Additive.
const LEDGER_SEGMENTS: TableDefinition<(&str, &str), &str> =
    TableDefinition::new("ledger_segments");
```

and add after `impl Bindings for RedbStore` (ends line 1275):

```rust
impl LedgerCache for RedbStore {
    fn last_head(&self, repo: &str) -> Result<Option<String>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(LEDGER_HEADS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        let found = table
            .get(repo)
            .map_err(backend)?
            .map(|v| v.value().to_string());
        Ok(found)
    }

    fn set_last_head(&self, repo: &str, head: &str) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        tx.open_table(LEDGER_HEADS)
            .map_err(backend)?
            .insert(repo, head)
            .map_err(backend)?;
        tx.commit().map_err(backend)
    }

    fn cached(&self, repo: &str, path: &str) -> Result<Option<CachedSegment>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(LEDGER_SEGMENTS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        let found = match table.get((repo, path)).map_err(backend)? {
            Some(v) => Some(serde_json::from_str(v.value()).map_err(decode)?),
            None => None,
        };
        Ok(found)
    }

    fn cached_under(
        &self,
        repo: &str,
        dir: &str,
    ) -> Result<Vec<(String, CachedSegment)>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(LEDGER_SEGMENTS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![]),
            Err(e) => return Err(backend(e)),
        };
        let prefix = format!("{dir}/");
        let mut out = Vec::new();
        for entry in table.range((repo, prefix.as_str())..).map_err(backend)? {
            let (k, v) = entry.map_err(backend)?;
            let (r, p) = k.value();
            if r != repo || !p.starts_with(&prefix) {
                break;
            }
            out.push((p.to_string(), serde_json::from_str(v.value()).map_err(decode)?));
        }
        Ok(out)
    }

    fn cache(&self, repo: &str, path: &str, segment: &CachedSegment) -> Result<(), StoreError> {
        let json = serde_json::to_string(segment).map_err(backend)?;
        let tx = self.db.begin_write().map_err(backend)?;
        tx.open_table(LEDGER_SEGMENTS)
            .map_err(backend)?
            .insert((repo, path), json.as_str())
            .map_err(backend)?;
        tx.commit().map_err(backend)
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fl-core -p fl-store`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

1. `MemStore::cached_under` / `RedbStore::cached_under`: match on `starts_with(dir)` instead of `starts_with("{dir}/")` → `a_cached_file_reads_back_by_path_and_by_its_own_directory_only` red (`runs/aab` leaks in), once per store.
2. Both: drop the repository comparison → the same case red (`R_2`'s file leaks in).
3. `RedbStore::last_head`: return `Ok(None)` → `the_last_head_is_kept…` and `the_ledger_cache_survives_a_reopen` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/split.rs crates/core/src/mem.rs crates/core/src/conformance.rs crates/core/src/lib.rs crates/store/src/lib.rs
git commit -m "feat(store): remember the last ledger head and every file read

LedgerCache keeps, per repository node id, the last head of the GitHub
ledger this machine checked and each branch file as it was read (blob id,
text, whether it was closed). LedgerMemory bundles it with Bindings and
Outbox for the GitHub ledger. Two additive redb tables; a shared suite runs
over both stores. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---
### Task 6: Branch format 1 and decision 2's projection

**Files:**
- Modify: `crates/github/Cargo.toml` (`sha2`)
- Modify: `crates/github/src/lib.rs:4-10` (`pub mod ledger;`)
- Create: `crates/github/src/ledger/mod.rs`
- Create: `crates/github/src/ledger/layout.rs`
- Create: `crates/github/src/ledger/disclose.rs`

**Interfaces:**
- Consumes: `fl_core::{GateRun, Attempt, Decision, PathsTouched, WITHHELD_ERROR_DETAIL}`.
- Produces:

```rust
// fl_github::ledger::layout
pub const BRANCH: &str;            // "fl/ledger"
pub const FORMAT_FILE: &str;       // "format"
pub const FORMAT: &str;            // "1"
pub const README_FILE: &str;       // "README.md"
pub const README: &str;
pub const QUARANTINE_FILE: &str;   // "quarantine.jsonl"
pub const SEGMENT_LIMIT: usize;    // 262_144
pub enum Area { Runs, Attempts, Decisions }        // Copy, Eq, Debug
impl Area { pub fn name(self) -> &'static str; }
pub fn key(iri: &Iri) -> String;                    // 32 hex digits
pub fn dir(area: Area, subject: &Iri) -> String;    // "runs/<key>"
pub fn segment_path(dir: &str, n: u64) -> String;   // "<dir>/<n>.jsonl"
pub fn segment_number(name: &str) -> Option<u64>;
pub fn parse_segment_path(path: &str) -> Option<(Area, String, u64)>;
pub fn decision_subject(d: &Decision) -> &Iri;      // finding, else record
pub enum Line { Run(GateRun), Attempt(Attempt), Decision(Decision) }  // Clone, Eq, Debug
impl Line {
    pub fn area(&self) -> Area;
    pub fn subject(&self) -> &Iri;
    pub fn id(&self) -> Option<&Iri>;
    pub fn dir(&self) -> String;
    pub fn encode(&self, by: &str) -> String;       // one line, no newline
}
pub fn decode(area: Area, text: &str) -> Result<(Line, String), String>; // (line, by)
pub fn lines(text: &str) -> Vec<(u64, Result<&str, &'static str>)>;
pub fn plan_append(segments: &[(u64, String)], new: &[String]) -> Vec<(u64, String)>;
pub struct QuarantineLine { pub id: Iri, pub at: At, pub file: String, pub line: u64,
                            pub quarantined_by: String, pub reason: String, pub by: String }
impl QuarantineLine { pub fn encode(&self) -> String; pub fn decode(text: &str) -> Result<QuarantineLine, String>; }

// fl_github::ledger::disclose (Visibility re-exported at fl_github::ledger::Visibility)
pub enum Visibility { Private, NotPrivate }         // Copy, Eq, Debug
impl Visibility { pub fn from_github(v: &str) -> Visibility; }
pub fn run(run: &GateRun, v: Visibility) -> GateRun;
pub fn attempt(a: &Attempt, v: Visibility) -> Attempt;
```

- [ ] **Step 1: Write the failing tests**

Add to `crates/github/Cargo.toml`'s `[dependencies]` (after `base64.workspace = true`):

```toml
sha2.workspace = true
```

Add `pub mod ledger;` to `crates/github/src/lib.rs` after `pub mod fake;` (line 7). Create `crates/github/src/ledger/mod.rs`:

```rust
//! The GitHub ledger (GitHub ledger spec §3): the `fl/ledger` branch of
//! the repository that backs the tracker, where mode B publishes each
//! decision and the evidence it rests on.

pub mod disclose;
pub mod layout;

pub use disclose::Visibility;
```

Create `crates/github/src/ledger/layout.rs` with only its tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::conformance::{sample_attempt, sample_decision, sample_record_run};
    use fl_core::decision::Outcome;
    use fl_core::ids::{FindingId, GateId, ProjectId, RecordId, seq_iri};
    use fl_core::log::PathsTouched;
    use fl_core::model::State;

    fn record() -> RecordId {
        RecordId(Iri::parse("https://github.com/acme/widgets/issues/1").unwrap())
    }

    fn seg(lines: &[&str]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    #[test]
    fn a_key_is_the_first_32_hex_digits_of_the_sha_256_of_the_iri() {
        let iri = seq_iri(1);
        let full: String = Sha256::digest(iri.as_str().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(key(&iri), &full[..32]);
        assert_ne!(key(&iri), key(&seq_iri(2)));
    }

    // Spec §3.1 and ruling 7.
    #[test]
    fn each_entry_is_filed_under_its_gate_project_finding_or_record() {
        let g = GateId(seq_iri(7));
        let p = ProjectId(seq_iri(8));
        let run = sample_record_run(1, &g, Some(&record()));
        assert_eq!(Line::Run(run).dir(), format!("runs/{}", key(g.iri())));
        let a = sample_attempt(2, &p, &record());
        assert_eq!(
            Line::Attempt(a).dir(),
            format!("attempts/{}", key(p.iri()))
        );
        let mut d = sample_decision(3, &record(), vec![]);
        assert_eq!(
            Line::Decision(d.clone()).dir(),
            format!("decisions/{}", key(record().iri()))
        );
        let f = FindingId(Iri::parse("https://github.com/acme/widgets/issues/2").unwrap());
        d.finding = Some(f.clone());
        assert_eq!(
            Line::Decision(d).dir(),
            format!("decisions/{}", key(f.iri())),
            "a finding's decision is filed under the finding"
        );
    }

    #[test]
    fn segment_names_are_canonical_numbers_from_one() {
        assert_eq!(segment_number("1.jsonl"), Some(1));
        assert_eq!(segment_number("12.jsonl"), Some(12));
        for bad in ["0.jsonl", "01.jsonl", "1.json", "a.jsonl", ".jsonl", "-1.jsonl", "1.jsonl.bak"] {
            assert_eq!(segment_number(bad), None, "{bad}");
        }
        assert_eq!(segment_path("runs/k", 3), "runs/k/3.jsonl");
    }

    #[test]
    fn a_segment_path_is_an_area_a_key_and_a_number() {
        let k = key(&seq_iri(1));
        assert_eq!(
            parse_segment_path(&format!("runs/{k}/3.jsonl")),
            Some((Area::Runs, format!("runs/{k}"), 3))
        );
        assert_eq!(
            parse_segment_path(&format!("decisions/{k}/1.jsonl")),
            Some((Area::Decisions, format!("decisions/{k}"), 1))
        );
        for bad in [
            format!("runs/{k}"),
            format!("runs/{k}/0.jsonl"),
            format!("other/{k}/1.jsonl"),
            "runs/abc/1.jsonl".to_string(),
            format!("runs/{}/1.jsonl", k.to_uppercase()),
            format!("runs/{k}/x/1.jsonl"),
            QUARANTINE_FILE.to_string(),
            FORMAT_FILE.to_string(),
        ] {
            assert_eq!(parse_segment_path(&bad), None, "{bad}");
        }
    }

    #[test]
    fn every_kind_of_line_round_trips_with_its_writer() {
        let g = GateId(seq_iri(7));
        let p = ProjectId(seq_iri(8));
        let mut withheld = sample_attempt(2, &p, &record());
        withheld.output_excerpt = None;
        withheld.paths_touched = PathsTouched::Counted(4);
        let mut refused = sample_decision(3, &record(), vec![]);
        refused.outcome = Outcome::Move {
            from: State::Review,
            to: State::Done,
            transitions: vec![],
            allowed: false,
        };
        for (area, line) in [
            (Area::Runs, Line::Run(sample_record_run(1, &g, Some(&record())))),
            (Area::Attempts, Line::Attempt(sample_attempt(2, &p, &record()))),
            (Area::Attempts, Line::Attempt(withheld)),
            (Area::Decisions, Line::Decision(refused)),
        ] {
            let text = line.encode("fake-user");
            assert!(!text.contains('\n'), "{text}");
            assert_eq!(decode(area, &text), Ok((line, "fake-user".to_string())));
        }
    }

    // Ruling 11 and spec §3.1: a new field means a new format, so a line
    // with anything fl does not write is unreadable.
    #[test]
    fn a_line_is_read_strictly() {
        let g = GateId(seq_iri(7));
        let good = Line::Run(sample_record_run(1, &g, Some(&record()))).encode("fake-user");
        let edit = |f: &dyn Fn(&mut Value)| {
            let mut v: Value = serde_json::from_str(&good).unwrap();
            f(&mut v);
            v.to_string()
        };
        for (case, text) in [
            ("an unknown field", edit(&|v| v["host"] = Value::String("somewhere".into()))),
            ("no writer", edit(&|v| { v.as_object_mut().unwrap().remove("by"); })),
            ("an empty writer", edit(&|v| v["by"] = Value::String(" ".into()))),
            ("no id", edit(&|v| v["id"] = Value::Null)),
            ("no time", edit(&|v| v["at"] = Value::Null)),
            ("a count spelled as a float", edit(&|v| v["population"] = serde_json::json!(1.0))),
            ("not JSON", "{".to_string()),
            ("not an object", "[]".to_string()),
        ] {
            assert!(decode(Area::Runs, &text).is_err(), "{case}: {text}");
        }
        assert!(decode(Area::Attempts, &good).is_err(), "a run is not an attempt");
        assert!(decode(Area::Runs, &good).is_ok());
    }

    #[test]
    fn lines_are_numbered_from_one_and_a_cut_or_empty_line_is_flagged_in_place() {
        assert!(lines("").is_empty());
        assert_eq!(lines("a\nb\n"), vec![(1, Ok("a")), (2, Ok("b"))]);
        assert_eq!(
            lines("a\n\nb\n"),
            vec![(1, Ok("a")), (2, Err("it is empty")), (3, Ok("b"))]
        );
        let cut = lines("a\nb");
        assert_eq!(cut[0], (1, Ok("a")));
        assert_eq!(cut[1].0, 2);
        assert!(cut[1].1.is_err(), "{cut:?}");
    }

    #[test]
    fn new_lines_grow_the_last_segment_and_a_closed_one_is_never_rewritten() {
        let segments = vec![(1, seg(&["a"])), (2, seg(&["b"]))];
        assert_eq!(
            plan_append(&segments, &["c".into(), "d".into()]),
            vec![(2, seg(&["b", "c", "d"]))]
        );
        assert_eq!(
            plan_append(&[], &["a".into()]),
            vec![(1, seg(&["a"]))],
            "an empty directory starts at 1"
        );
        assert!(plan_append(&segments, &[]).is_empty(), "nothing to add writes nothing");
    }

    // Spec §3.1: segments roll over at 256 KB.
    #[test]
    fn a_segment_closes_when_the_next_line_would_pass_the_limit() {
        // The segment holds SEGMENT_LIMIT - 9 bytes: a line of 8 and its
        // newline fill it exactly; a line of 9 does not fit.
        let fill = "x".repeat(SEGMENT_LIMIT - 10);
        let segments = vec![(1, format!("{fill}\n"))];
        let eight = "y".repeat(8);
        assert_eq!(
            plan_append(&segments, std::slice::from_ref(&eight)),
            vec![(1, format!("{fill}\n{eight}\n"))]
        );
        let nine = "z".repeat(9);
        assert_eq!(
            plan_append(&segments, std::slice::from_ref(&nine)),
            vec![(2, format!("{nine}\n"))]
        );
        let out = plan_append(&segments, &[eight.clone(), nine.clone()]);
        assert_eq!(
            out,
            vec![(1, format!("{fill}\n{eight}\n")), (2, format!("{nine}\n"))]
        );
        assert!(out.iter().all(|(_, t)| t.len() <= SEGMENT_LIMIT));
    }

    #[test]
    fn a_line_longer_than_a_segment_gets_a_segment_of_its_own() {
        let huge = "h".repeat(SEGMENT_LIMIT + 1);
        assert_eq!(
            plan_append(&[(1, seg(&["a"]))], &[huge.clone(), "b".into()]),
            vec![(2, format!("{huge}\n")), (3, seg(&["b"]))]
        );
        assert_eq!(
            plan_append(&[], std::slice::from_ref(&huge)),
            vec![(1, format!("{huge}\n"))],
            "an empty segment takes it"
        );
    }

    #[test]
    fn a_quarantine_line_round_trips_and_is_read_strictly() {
        let q = QuarantineLine {
            id: seq_iri(5),
            at: At::from_unix_millis(5),
            file: "runs/k/1.jsonl".into(),
            line: 3,
            quarantined_by: "Ada".into(),
            reason: "a hand edit".into(),
            by: "fake-user".into(),
        };
        assert_eq!(QuarantineLine::decode(&q.encode()), Ok(q.clone()));
        let mut v: Value = serde_json::from_str(&q.encode()).unwrap();
        v["extra"] = Value::Bool(true);
        assert!(QuarantineLine::decode(&v.to_string()).is_err());
        let mut nobody = q.clone();
        nobody.quarantined_by = " ".into();
        assert!(QuarantineLine::decode(&nobody.encode()).is_err());
    }
}
```

Create `crates/github/src/ledger/disclose.rs` with only its tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::conformance::{sample_attempt, sample_record_run};
    use fl_core::ids::{GateId, ProjectId, RecordId, seq_iri};

    // Decision 2: `internal` counts as not private, as anything but
    // `private` does.
    #[test]
    fn only_private_is_private() {
        assert_eq!(Visibility::from_github("private"), Visibility::Private);
        for v in ["public", "internal", "", "Private"] {
            assert_eq!(Visibility::from_github(v), Visibility::NotPrivate, "{v}");
        }
    }

    #[test]
    fn a_private_repository_gets_every_field() {
        let mut r = sample_record_run(1, &GateId(seq_iri(1)), None);
        r.verdict = Verdict::error("spawn failed at /home/someone/bin/lint");
        assert_eq!(run(&r, Visibility::Private), r);
        let a = sample_attempt(2, &ProjectId(seq_iri(2)), &RecordId(seq_iri(3)));
        assert_eq!(attempt(&a, Visibility::Private), a);
    }

    // Decision 2, field by field.
    #[test]
    fn a_repository_that_is_not_private_gets_no_excerpt_no_error_detail_and_a_path_count() {
        let mut r = sample_record_run(1, &GateId(seq_iri(1)), None);
        r.verdict = Verdict::error("spawn failed at /home/someone/bin/lint");
        let p = run(&r, Visibility::NotPrivate);
        assert_eq!(p.output_excerpt, None);
        assert_eq!(p.verdict, Verdict::error(WITHHELD_ERROR_DETAIL));
        assert_eq!(
            (p.id.clone(), p.population, p.commit.clone()),
            (r.id.clone(), r.population, r.commit.clone()),
            "nothing else changes"
        );
        let pass = sample_record_run(2, &GateId(seq_iri(1)), None);
        assert_eq!(
            run(&pass, Visibility::NotPrivate).verdict,
            pass.verdict,
            "a pass has no detail to withhold"
        );
        let a = sample_attempt(3, &ProjectId(seq_iri(2)), &RecordId(seq_iri(3)));
        let q = attempt(&a, Visibility::NotPrivate);
        assert_eq!(q.output_excerpt, None);
        assert_eq!(q.paths_touched, PathsTouched::Counted(1));
        assert_eq!(q.tokens_in, a.tokens_in);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-github --lib ledger`
Expected: FAIL to compile — nothing in `layout` or `disclose` exists yet.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/github/src/ledger/layout.rs`:

```rust
//! The `fl/ledger` branch, format 1 (GitHub ledger spec §3.1): where each
//! entry goes, how a line is written and read, and when a segment closes.
//! Pure: no network, no clock.

use fl_core::at::At;
use fl_core::decision::Decision;
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The branch: an orphan, with no `.github/`, so an append never starts a
/// workflow.
pub const BRANCH: &str = "fl/ledger";
/// The file that names the layout's version.
pub const FORMAT_FILE: &str = "format";
/// The one version this fl reads (spec §3.1: "a new field in any entry
/// means a new format").
pub const FORMAT: &str = "1";
pub const README_FILE: &str = "README.md";
pub const QUARANTINE_FILE: &str = "quarantine.jsonl";
/// A segment closes when the next line would take it past this many
/// bytes; a longer line gets a segment of its own (spec §3.1: 256 KB).
pub const SEGMENT_LIMIT: usize = 256 * 1024;

/// What the branch says about itself. No machine names.
pub const README: &str = "# fl ledger\n\n\
This branch is the shared ledger of fl (FerroLoop): the gate runs, attempts and decisions \
recorded against this repository's issues. fl only ever appends to it. Do not edit it by hand: \
every machine that reads it checks that each file only grows, and refuses a ledger that \
changed.\n\n\
- `format`: the layout's version.\n\
- `runs/<key>/<n>.jsonl`: gate runs, one directory per gate.\n\
- `attempts/<key>/<n>.jsonl`: attempts, one directory per project.\n\
- `decisions/<key>/<n>.jsonl`: decisions, one directory per record or finding.\n\
- `quarantine.jsonl`: lines readers skip, and why.\n\n\
A key is the first 32 hexadecimal digits of the SHA-256 of the item's IRI.\n";

/// The three kinds of directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Runs,
    Attempts,
    Decisions,
}

impl Area {
    pub fn name(self) -> &'static str {
        match self {
            Area::Runs => "runs",
            Area::Attempts => "attempts",
            Area::Decisions => "decisions",
        }
    }

    fn from_name(name: &str) -> Option<Area> {
        [Area::Runs, Area::Attempts, Area::Decisions]
            .into_iter()
            .find(|a| a.name() == name)
    }
}

/// The first 32 hexadecimal digits of the SHA-256 of `iri`.
pub fn key(iri: &Iri) -> String {
    Sha256::digest(iri.as_str().as_bytes())
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `subject`'s directory in `area`: `runs/<key>`.
pub fn dir(area: Area, subject: &Iri) -> String {
    format!("{}/{}", area.name(), key(subject))
}

pub fn segment_path(dir: &str, n: u64) -> String {
    format!("{dir}/{n}.jsonl")
}

/// `n` from a segment's file name `<n>.jsonl`: from 1, with no leading
/// zero, so each segment has one spelling.
pub fn segment_number(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(".jsonl")?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

/// A segment's area, directory and number, from its path; `None` for any
/// path fl does not write as a segment.
pub fn parse_segment_path(path: &str) -> Option<(Area, String, u64)> {
    let mut parts = path.split('/');
    let (area, k, file) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let area = Area::from_name(area)?;
    if k.len() != 32 || !k.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    Some((area, format!("{}/{k}", area.name()), segment_number(file)?))
}

/// What a decision is filed under: its finding when it has one, else its
/// record (spec §3.1; ruling 7).
pub fn decision_subject(d: &Decision) -> &Iri {
    match &d.finding {
        Some(f) => f.iri(),
        None => d.record.iri(),
    }
}

/// One line of a segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Run(GateRun),
    Attempt(Attempt),
    Decision(Decision),
}

impl Line {
    pub fn area(&self) -> Area {
        match self {
            Line::Run(_) => Area::Runs,
            Line::Attempt(_) => Area::Attempts,
            Line::Decision(_) => Area::Decisions,
        }
    }

    /// What its directory is keyed by: the run's gate, the attempt's
    /// project, the decision's finding or record.
    pub fn subject(&self) -> &Iri {
        match self {
            Line::Run(r) => r.gate.iri(),
            Line::Attempt(a) => a.project.iri(),
            Line::Decision(d) => decision_subject(d),
        }
    }

    pub fn id(&self) -> Option<&Iri> {
        match self {
            Line::Run(r) => r.id.as_ref(),
            Line::Attempt(a) => a.id.as_ref(),
            Line::Decision(d) => Some(&d.id),
        }
    }

    pub fn dir(&self) -> String {
        dir(self.area(), self.subject())
    }

    fn value(&self) -> Value {
        match self {
            Line::Run(r) => serde_json::to_value(r),
            Line::Attempt(a) => serde_json::to_value(a),
            Line::Decision(d) => serde_json::to_value(d),
        }
        .expect("an entry always serializes")
    }

    /// The entry and `by`, as one line of JSON with no newline.
    pub fn encode(&self, by: &str) -> String {
        let mut v = self.value();
        if let Value::Object(map) = &mut v {
            map.insert("by".into(), Value::String(by.to_string()));
        }
        v.to_string()
    }
}

/// One line of an `area` segment, and who wrote it.
///
/// ⚠ Strict (ruling 11): the entry, serialized again, must equal the line
/// without `by`. A field fl does not write, or a value spelled as fl never
/// spells it, is unreadable — a new field means a new format (spec §3.1).
/// A published run or attempt always carries `id` and `at`.
pub fn decode(area: Area, text: &str) -> Result<(Line, String), String> {
    let mut v: Value = serde_json::from_str(text).map_err(|e| format!("it is not JSON ({e})"))?;
    let Some(map) = v.as_object_mut() else {
        return Err("it is not a JSON object".into());
    };
    let by = match map.remove("by") {
        Some(Value::String(s)) if !s.trim().is_empty() => s,
        Some(_) => return Err("its `by` is not a name".into()),
        None => return Err("it names no writer (`by`)".into()),
    };
    let line = match area {
        Area::Runs => serde_json::from_value(v.clone())
            .map(Line::Run)
            .map_err(|e| format!("it is not a gate run ({e})"))?,
        Area::Attempts => serde_json::from_value(v.clone())
            .map(Line::Attempt)
            .map_err(|e| format!("it is not an attempt ({e})"))?,
        Area::Decisions => serde_json::from_value(v.clone())
            .map(Line::Decision)
            .map_err(|e| format!("it is not a decision ({e})"))?,
    };
    if line.value() != v {
        return Err(
            "it holds a field fl does not write, or a value spelled as fl never spells it".into(),
        );
    }
    let stamped = match &line {
        Line::Run(r) => r.id.is_some() && r.at.is_some(),
        Line::Attempt(a) => a.id.is_some() && a.at.is_some(),
        Line::Decision(_) => true,
    };
    if !stamped {
        return Err("it has no `id` or no `at`, which every published entry carries".into());
    }
    Ok((line, by))
}

/// The lines of a segment, numbered from 1.
///
/// ⚠ Every line fl writes ends with a newline: an empty line, or text after
/// the last newline (a line cut short), comes back as `Err` in its place,
/// with its number, for the reader to report.
pub fn lines(text: &str) -> Vec<(u64, Result<&str, &'static str>)> {
    let mut out = Vec::new();
    if text.is_empty() {
        return out;
    }
    let mut pieces: Vec<&str> = text.split('\n').collect();
    let tail = pieces.pop().unwrap_or("");
    for (i, p) in pieces.iter().enumerate() {
        let line = if p.is_empty() { Err("it is empty") } else { Ok(*p) };
        out.push(((i + 1) as u64, line));
    }
    if !tail.is_empty() {
        out.push((
            (out.len() + 1) as u64,
            Err("it does not end with a newline: it was cut short"),
        ));
    }
    out
}

/// The files to write so `new` lines join a directory whose segments are
/// `segments` — (number, text), oldest first. Only the last segment grows;
/// it closes when the next line would take it past [`SEGMENT_LIMIT`].
/// Returns (number, its whole new text) for each segment written; nothing
/// to add writes nothing.
pub fn plan_append(segments: &[(u64, String)], new: &[String]) -> Vec<(u64, String)> {
    let (mut n, mut text) = match segments.last() {
        Some((n, t)) => (*n, t.clone()),
        None => (1, String::new()),
    };
    let mut changed = false;
    let mut out = Vec::new();
    for line in new {
        if !text.is_empty() && text.len() + line.len() + 1 > SEGMENT_LIMIT {
            if changed {
                out.push((n, std::mem::take(&mut text)));
            } else {
                text.clear();
            }
            n += 1;
            changed = false;
        }
        text.push_str(line);
        text.push('\n');
        changed = true;
    }
    if changed {
        out.push((n, text));
    }
    out
}

/// One line of `quarantine.jsonl` (spec §3.6): a segment's line readers
/// skip, who decided, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineLine {
    /// Minted by the caller, so a retried quarantine adds one line.
    pub id: Iri,
    pub at: At,
    /// The segment, as a path on the branch.
    pub file: String,
    /// From 1.
    pub line: u64,
    /// The person who decided, as given with `--by`.
    pub quarantined_by: String,
    pub reason: String,
    /// The GitHub identity that wrote this line.
    pub by: String,
}

impl QuarantineLine {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("a quarantine line always serializes")
    }

    pub fn decode(text: &str) -> Result<QuarantineLine, String> {
        let q: QuarantineLine = serde_json::from_str(text)
            .map_err(|e| format!("it is not a quarantine line ({e})"))?;
        if q.by.trim().is_empty() || q.quarantined_by.trim().is_empty() {
            return Err("it names no one".into());
        }
        Ok(q)
    }
}

```

Prepend to `crates/github/src/ledger/disclose.rs`:

```rust
//! Decision 2 (GitHub ledger spec §5): on a repository that is not
//! private, nothing machine-specific is published.

use fl_core::log::{Attempt, GateRun, PathsTouched, WITHHELD_ERROR_DETAIL};
use fl_core::verdict::Verdict;

/// Who can read the repository, as decision 2 needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Private,
    NotPrivate,
}

impl Visibility {
    /// GitHub's `visibility`. ⚠ Only `private` is private: `internal`,
    /// `public`, and anything this fl does not know are not.
    pub fn from_github(v: &str) -> Visibility {
        if v == "private" {
            Visibility::Private
        } else {
            Visibility::NotPrivate
        }
    }
}

/// The copy of `run` a repository of visibility `v` may hold: not private,
/// no excerpt, and an error's detail replaced by the one withheld text.
pub fn run(run: &GateRun, v: Visibility) -> GateRun {
    let mut out = run.clone();
    if v == Visibility::NotPrivate {
        out.output_excerpt = None;
        if let Verdict::Error { .. } = out.verdict {
            out.verdict = Verdict::error(WITHHELD_ERROR_DETAIL);
        }
    }
    out
}

/// The copy of `a` a repository of visibility `v` may hold: not private,
/// no excerpt, and only how many paths it touched.
pub fn attempt(a: &Attempt, v: Visibility) -> Attempt {
    let mut out = a.clone();
    if v == Visibility::NotPrivate {
        out.output_excerpt = None;
        out.paths_touched = PathsTouched::Counted(a.paths_touched.count());
    }
    out
}

```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fl-github --lib ledger`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

1. `key`: take 17 bytes → `a_key_is_the_first_32_hex_digits…` red.
2. `decision_subject`: always the record → `each_entry_is_filed_under…` red.
3. `segment_number`: allow a leading zero → `segment_names_are_canonical…` red; allow `0` (drop the leading-zero rule only for `"0"`) → same.
4. `parse_segment_path`: accept a key of any length, or uppercase hex → `a_segment_path_is_an_area_a_key_and_a_number` red; accept a fourth path part → same.
5. `decode`: delete the round-trip comparison → `a_line_is_read_strictly` red ("an unknown field").
6. `decode`: accept a missing or blank `by` → `a_line_is_read_strictly` red.
7. `decode`: delete the `stamped` check → `a_line_is_read_strictly` red ("no id", "no time").
8. `lines`: drop the empty-line check, or the cut-short tail → `lines_are_numbered_from_one…` red.
9. `plan_append`: `>=` instead of `>` → `a_segment_closes_when_the_next_line_would_pass_the_limit` red; drop the `!text.is_empty()` guard → `a_line_longer_than_a_segment…` red ("an empty segment takes it").
10. `QuarantineLine::decode`: drop the "names no one" check → `a_quarantine_line_round_trips…` red; drop `deny_unknown_fields` → same.
11. `Visibility::from_github`: compare ignoring case → `only_private_is_private` red.
12. `disclose::run`: keep the excerpt, or the error detail → `a_repository_that_is_not_private…` red. `disclose::attempt`: keep the list → same.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/Cargo.toml crates/github/src/lib.rs crates/github/src/ledger/mod.rs crates/github/src/ledger/layout.rs crates/github/src/ledger/disclose.rs Cargo.lock
git commit -m "feat(github): the ledger branch's format 1, and decision 2's projection

layout says where each entry goes (a 32-hex key per gate, project, and
finding or record), writes a line as the entry plus by, reads one strictly
by round trip, numbers a segment's lines, and plans an append that grows
only the last segment and rolls over at 256 KB. disclose withholds the
excerpts, an error's detail and the path list on a repository that is not
private. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 7: The client's ledger answers, and the fake's git objects

**Files:**
- Modify: `crates/github/src/client.rs:116-155` (`graphql`, new `graphql_answer`), `:226-312` (`send`: the permission header), tests
- Modify: `crates/github/src/lib.rs:4-14`
- Create: `crates/github/src/fake_git.rs`
- Modify: `crates/github/src/fake.rs:47-165` (`State`), `:370-391` (`Answer`, `answer`), `:434` (`is_bound`), `:530-578` (`route`), after `:354` (branch helpers)

**Interfaces:**
- Consumes: `layout::{BRANCH, FORMAT_FILE, README, README_FILE}` (Task 6).
- Produces:

```rust
// fl_github::client (GraphqlAnswer re-exported at fl_github::GraphqlAnswer)
pub struct GraphqlAnswer { pub status: u16, pub data: Option<Value>, pub errors: Vec<Value> }
impl Client { pub fn graphql_answer(&self, query: &str, variables: Value) -> Result<GraphqlAnswer, StoreError>; }

// fl_github::fake_git (cfg test or feature "fake")
pub struct FakeCommit { pub tree: String, pub parents: Vec<String>, pub message: String }
pub struct Ruleset { pub enforcement: String, pub branch: String, pub rules: Vec<String> }
impl Ruleset { pub fn on_ledger(enforcement: &str, rules: &[&str]) -> Ruleset; }
pub struct Git { pub blobs, pub trees, pub commits, pub refs /* "heads/<branch>" → commit */ }
impl Git {
    pub fn put_blob(&mut self, text: &str) -> String;
    pub fn put_tree(&mut self, files: &BTreeMap<String, String>) -> String;
    pub fn put_commit(&mut self, tree: &str, parents: Vec<String>, message: &str) -> String;
    pub fn head(&self, branch: &str) -> Option<String>;
    pub fn files_at(&self, commit: &str) -> Option<BTreeMap<String, String>>;
    pub fn is_ancestor(&self, a: &str, b: &str) -> bool;
    pub fn commit_on(&mut self, branch: &str, changes: &[(String, Option<String>)], message: &str) -> String;
    pub fn first_parents(&self, branch: &str) -> Vec<String>;
}
pub fn seed(git: &mut Git, files: &[(&str, &str)]) -> String; // a root commit on fl/ledger

// fl_github::fake::State — new knobs
pub git: Git, pub rulesets: Vec<Ruleset>, pub rules_need_upgrade: bool, pub down: bool,
pub compare_behind_next: u32, pub fail_next_ref_create: bool, pub race_next_ref_create: bool,
pub permission_refused_next: Option<String>,

// fl_github::fake::FakeGithub — branch helpers
pub fn seed_ledger(&self) -> String;
pub fn seed_ledger_with(&self, files: &[(&str, &str)]) -> String;
pub fn ledger_head(&self) -> Option<String>;
pub fn ledger_files(&self) -> BTreeMap<String, String>;
pub fn hand_commit(&self, changes: &[(&str, Option<&str>)]) -> String;
pub fn hand_merge(&self) -> String;
pub fn rewrite_ledger(&self, files: &[(&str, &str)]) -> String;
pub fn delete_ledger(&self);
pub fn ledger_commits(&self) -> usize;
```

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module of `crates/github/src/client.rs`:

```rust
    // The ledger judges a GraphQL answer itself: a stale commit is an
    // error it acts on, not one to report. Only a spent rate limit is
    // still refused here.
    #[test]
    fn a_graphql_answer_hands_its_errors_and_status_to_the_caller() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let a = c
            .graphql_answer(
                "query($id: ID!) { node(id: $id) { ... on Issue { url } } }",
                serde_json::json!({"id": "I_404"}),
            )
            .unwrap();
        assert_eq!(a.status, 200);
        assert_eq!(a.errors.len(), 1);
        assert_eq!(a.data, Some(serde_json::json!({"node": null})));
        fake.state().graphql_rate_limited = true;
        let err = c
            .graphql_answer("query { viewer { login } }", serde_json::json!({}))
            .unwrap_err();
        assert!(matches!(err, StoreError::RateLimited { .. }), "{err:?}");
    }

    // `graphql` keeps refusing what it refused before `graphql_answer`
    // existed: a 5xx is not data.
    #[test]
    fn a_graphql_query_answered_with_a_5xx_is_an_error_not_data() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().html_502_next = true;
        let err = client(&fake)
            .graphql("query { viewer { login } }", serde_json::json!({}))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(ref m) if m.contains("502")),
            "{err:?}"
        );
    }

    // Spec §6.3: a missing permission is found by the first write, and the
    // error names it from GitHub's `x-accepted-github-permissions`.
    // Modelled from GitHub's documentation; no live test provokes it.
    #[test]
    fn a_refusal_for_want_of_a_permission_names_the_permission() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().permission_refused_next = Some("contents=write".into());
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains("contents=write"), "{err}");
    }
```

Create `crates/github/src/fake_git.rs` with only its tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::Ruleset;
    use crate::client::{Client, Method};
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use fl_core::StoreError;
    use serde_json::json;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    const REPO: &str = "/repos/acme/widgets";

    #[test]
    fn a_branch_is_built_from_a_tree_a_commit_and_a_ref_and_read_back() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let tree = c
            .send(
                Method::Post,
                &format!("{REPO}/git/trees"),
                Some(&json!({"tree": [{"path": "format", "mode": "100644", "type": "blob", "content": "1\n"}]})),
            )
            .unwrap();
        assert_eq!(tree.status, 201);
        let commit = c
            .send(
                Method::Post,
                &format!("{REPO}/git/commits"),
                Some(&json!({"message": "m", "tree": tree.body["sha"], "parents": []})),
            )
            .unwrap();
        let sha = commit.body["sha"].as_str().unwrap().to_string();
        assert_eq!(sha.len(), 40, "shaped like a git id");
        let made = c
            .send(
                Method::Post,
                &format!("{REPO}/git/refs"),
                Some(&json!({"ref": "refs/heads/fl/ledger", "sha": sha})),
            )
            .unwrap();
        assert_eq!(made.status, 201);
        let read = c
            .send(Method::Get, &format!("{REPO}/git/ref/heads/fl/ledger"), None)
            .unwrap();
        assert_eq!(read.body["object"]["sha"], json!(sha));
        assert_eq!(fake.ledger_files().get("format").map(String::as_str), Some("1\n"));
        let got = c
            .send(Method::Get, &format!("{REPO}/git/commits/{sha}"), None)
            .unwrap();
        assert_eq!(got.body["parents"], json!([]));
        assert_eq!(
            c.send(Method::Get, &format!("{REPO}/git/ref/heads/fl"), None)
                .unwrap()
                .status,
            404,
            "an exact name, not a prefix"
        );
    }

    #[test]
    fn a_ref_that_exists_or_collides_with_another_is_refused() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let c = client(&fake);
        let again = c
            .send(
                Method::Post,
                &format!("{REPO}/git/refs"),
                Some(&json!({"ref": "refs/heads/fl/ledger", "sha": root})),
            )
            .unwrap_err();
        assert!(again.to_string().contains("already exists"), "{again}");
        let parent = c
            .send(
                Method::Post,
                &format!("{REPO}/git/refs"),
                Some(&json!({"ref": "refs/heads/fl", "sha": root})),
            )
            .unwrap_err();
        assert!(parent.to_string().contains("conflicts"), "{parent}");
    }

    #[test]
    fn a_blob_comes_back_as_wrapped_base64() {
        use base64::Engine;
        let fake = FakeGithub::start("acme/widgets");
        let text = "x".repeat(200);
        let oid = fake.state().git.put_blob(&text);
        let b = client(&fake)
            .send(Method::Get, &format!("{REPO}/git/blobs/{oid}"), None)
            .unwrap();
        let content = b.body["content"].as_str().unwrap();
        assert!(content.contains('\n'), "wrapped, as GitHub sends it");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content.replace('\n', ""))
            .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), text);
    }

    #[test]
    fn a_compare_says_how_two_commits_relate_and_can_lag() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let next = fake.hand_commit(&[("runs/a/1.jsonl", Some("x\n"))]);
        let other = fake.rewrite_ledger(&[("format", "1\n")]);
        let c = client(&fake);
        let status = |base: &str, head: &str| {
            let r = c
                .send(Method::Get, &format!("{REPO}/compare/{base}...{head}"), None)
                .unwrap();
            (r.status, r.body["status"].as_str().unwrap_or("").to_string())
        };
        assert_eq!(status(&root, &root), (200, "identical".into()));
        assert_eq!(status(&root, &next), (200, "ahead".into()));
        assert_eq!(status(&next, &root), (200, "behind".into()));
        assert_eq!(status(&next, &other), (200, "diverged".into()));
        assert_eq!(status(&root, "0000000000000000000000000000000000000000").0, 404);
        fake.state().compare_behind_next = 1;
        assert_eq!(status(&root, &next), (200, "behind".into()), "a lagging replica");
        assert_eq!(status(&root, &next), (200, "ahead".into()), "and then not");
    }

    // Spec §6.2 and §8.1: rules in force only, and the plan's refusal.
    // Modelled — confirmed by live tests `rules_on_the_ledger_branch_are_readable`
    // and `a_private_repository_without_a_ruleset_is_detection_only`.
    #[test]
    fn the_rules_on_a_branch_are_those_of_active_rulesets() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let types = || -> Vec<String> {
            let r = c
                .send(Method::Get, &format!("{REPO}/rules/branches/fl/ledger"), None)
                .unwrap();
            r.body
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["type"].as_str().unwrap().to_string())
                .collect()
        };
        assert!(types().is_empty());
        fake.state().rulesets = vec![
            Ruleset::on_ledger("active", &["non_fast_forward"]),
            Ruleset::on_ledger("disabled", &["deletion"]),
            Ruleset::on_ledger("evaluate", &["deletion"]),
        ];
        assert_eq!(types(), vec!["non_fast_forward".to_string()]);
        fake.state().rules_need_upgrade = true;
        let err = c
            .send(Method::Get, &format!("{REPO}/rules/branches/fl/ledger"), None)
            .unwrap_err();
        assert!(err.to_string().contains("Upgrade to GitHub"), "{err}");
    }

    #[test]
    fn a_fake_that_is_down_is_unreachable() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().down = true;
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        fake.state().down = false;
        assert_eq!(
            client(&fake)
                .send(Method::Get, "/repos/acme/widgets", None)
                .unwrap()
                .status,
            200
        );
    }

    #[test]
    fn the_branch_helpers_commit_merge_rewrite_and_delete() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        assert_eq!(fake.ledger_commits(), 1);
        fake.hand_commit(&[("runs/a/1.jsonl", Some("x\n"))]);
        assert_eq!(fake.ledger_commits(), 2);
        let merge = fake.hand_merge();
        assert_eq!(fake.state().git.commits[&merge].parents.len(), 2);
        fake.hand_commit(&[("runs/a/1.jsonl", None)]);
        assert!(!fake.ledger_files().contains_key("runs/a/1.jsonl"));
        let new_root = fake.rewrite_ledger(&[("format", "1\n")]);
        assert_ne!(new_root, root);
        assert_eq!(fake.ledger_commits(), 1);
        fake.delete_ledger();
        assert_eq!(fake.ledger_head(), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-github --lib`
Expected: FAIL to compile — `graphql_answer`, `fake_git`, the knobs and the helpers do not exist.

- [ ] **Step 3: Write the implementation**

In `crates/github/src/client.rs`, add after `pub struct Reply { … }` (line 26):

```rust
/// A GraphQL answer for a caller that judges it itself (the ledger's
/// commit, GitHub ledger spec §3.2): the HTTP status (200, or a 5xx a write
/// may hide behind), `data` when present, and every error.
#[derive(Debug, Clone)]
pub struct GraphqlAnswer {
    pub status: u16,
    pub data: Option<Value>,
    pub errors: Vec<Value>,
}
```

Replace `graphql` (lines 115-155) with:

```rust
    /// One GraphQL request, judged only for a spent rate limit: the caller
    /// reads the status, the data and the errors.
    pub fn graphql_answer(&self, query: &str, variables: Value) -> Result<GraphqlAnswer, StoreError> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let reply = self.send(Method::Post, "/graphql", Some(&body))?;
        let errors: Vec<Value> = reply
            .body
            .get("errors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // A spent GraphQL rate limit is taken to be a 200 with an error of
        // type RATE_LIMITED: a reading of GitHub's docs, unmeasured.
        if errors
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("RATE_LIMITED"))
        {
            return Err(StoreError::RateLimited {
                reset: "GitHub's GraphQL limit resets (it did not say when)".into(),
            });
        }
        Ok(GraphqlAnswer {
            status: reply.status,
            data: reply.body.get("data").cloned(),
            errors,
        })
    }

    /// One GraphQL query. An `errors` member is an error, never partial data.
    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value, StoreError> {
        let answer = self.graphql_answer(query, variables)?;
        if answer.status != 200 {
            return Err(StoreError::Backend(format!(
                "GitHub answered {} to a GraphQL query; retry",
                answer.status
            )));
        }
        // GitHub is taken to answer a lookup of a missing node with `null`
        // data AND a NOT_FOUND error — an answer, not a failure. A reading
        // of GitHub's docs: unmeasured; no live test checks it yet.
        if !answer.errors.is_empty()
            && !answer
                .errors
                .iter()
                .all(|e| e.get("type").and_then(Value::as_str) == Some("NOT_FOUND"))
        {
            return Err(StoreError::Backend(format!(
                "GitHub refused a GraphQL query: {}",
                Value::Array(answer.errors)
            )));
        }
        answer.data.ok_or_else(|| {
            StoreError::Backend("GitHub answered a GraphQL query with no `data`".into())
        })
    }
```

In `send` (after `let retry_after = header("retry-after");`, line 236) add:

```rust
    // ⚠ Spec §6.3: GitHub names the permission a refused request needed.
    // Modelled from GitHub's documentation; no live test provokes it.
    let accepted = header("x-accepted-github-permissions");
```

and replace the last arm of the final `match` (lines 309-311) with:

```rust
        403 if accepted.is_some() => Err(StoreError::Backend(format!(
            "GitHub answered 403 to {method:?} {url}: {message}. The credential lacks a \
             permission this needs; GitHub names `{}`. Grant it to the token or the App \
             (Contents: read and write, Issues: read and write, Metadata: read)",
            accepted.unwrap_or_default()
        ))),
        _ => Err(StoreError::Backend(format!(
            "GitHub answered {status} to {method:?} {url}: {message}"
        ))),
```

In `crates/github/src/lib.rs`, add after `pub mod fake;` (line 7):

```rust
#[cfg(any(test, feature = "fake"))]
pub mod fake_git;
```

and replace line 12 with:

```rust
pub use client::{Client, DEFAULT_API, GraphqlAnswer, Method, Reply};
```

In `crates/github/src/fake.rs`:

1. Add to `State` (before its closing brace, line 165):

```rust
    /// The repository's git objects and refs (GitHub ledger spec §8.1).
    pub git: crate::fake_git::Git,
    /// Rulesets; only `active` ones apply. A setting, not one-shot.
    pub rulesets: Vec<crate::fake_git::Ruleset>,
    /// `rules/branches` answers 403 with GitHub's upgrade message — rulesets
    /// unavailable on the plan. Modelled — confirmed by live test
    /// `a_private_repository_without_a_ruleset_is_detection_only`.
    pub rules_need_upgrade: bool,
    /// Every request breaks off before an answer: GitHub unreachable. A
    /// setting, not one-shot.
    pub down: bool,
    /// The next this-many compares answer `behind` whatever the commits —
    /// a replica lagging a write (spec §3.5 check 2).
    pub compare_behind_next: u32,
    /// The next ref creation answers 500 and creates nothing. One-shot.
    pub fail_next_ref_create: bool,
    /// Right before the next ref creation, someone else creates the same
    /// branch. One-shot.
    pub race_next_ref_create: bool,
    /// The next request answers 403 naming this permission in
    /// `x-accepted-github-permissions`. One-shot.
    pub permission_refused_next: Option<String>,
```

2. Make `Answer`'s fields and two functions crate-visible: change lines 371-379 to

```rust
pub(crate) struct Answer {
    pub(crate) status: u16,
    pub(crate) body: Value,
    /// When set, this exact text is sent instead of `body.to_string()` — for
    /// answers that are not JSON at all (an HTML 502 from a load balancer).
    pub(crate) raw_body: Option<String>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) hang_up: bool,
    /// Send the status line and headers, then a body that cannot be read.
    pub(crate) break_body: bool,
}
```

change `fn answer(` (line 382) to `pub(crate) fn answer(`, and `fn is_bound(` (line 434) to `pub(crate) fn is_bound(`.

3. In `route`, right after `s.requests.push(format!("{method} {url}"));` (line 531), add:

```rust
    if s.down {
        let mut a = answer(503, Value::Null);
        a.hang_up = true;
        return a;
    }
    if let Some(needs) = s.permission_refused_next.take() {
        let mut a = answer(
            403,
            json!({"message": "Resource not accessible by personal access token"}),
        );
        a.headers.push(("x-accepted-github-permissions".into(), needs));
        return a;
    }
```

and right after `let parts: Vec<&str> = …;` (line 577), add:

```rust
    if let Some(a) = crate::fake_git::rest(s, method, &parts, body) {
        return a;
    }
```

4. Add to `impl FakeGithub` (after `plain_issue`, line 354):

```rust
    /// `fl/ledger` started as `fl github ledger init` starts it — `format`
    /// and `README.md` in a commit with no parent. Returns that commit.
    pub fn seed_ledger(&self) -> String {
        use crate::ledger::layout::{FORMAT_FILE, README, README_FILE};
        self.seed_ledger_with(&[(FORMAT_FILE, "1\n"), (README_FILE, README)])
    }

    /// `fl/ledger` started with `files` in its first commit.
    pub fn seed_ledger_with(&self, files: &[(&str, &str)]) -> String {
        crate::fake_git::seed(&mut self.state().git, files)
    }

    pub fn ledger_head(&self) -> Option<String> {
        self.state().git.head(crate::ledger::layout::BRANCH)
    }

    /// Every file on `fl/ledger` at its head; empty when there is no branch.
    pub fn ledger_files(&self) -> BTreeMap<String, String> {
        let s = self.state();
        s.git
            .head(crate::ledger::layout::BRANCH)
            .and_then(|h| s.git.files_at(&h))
            .unwrap_or_default()
    }

    /// A person with write access committing to `fl/ledger` by hand: each
    /// path written (`Some`) or removed (`None`).
    pub fn hand_commit(&self, changes: &[(&str, Option<&str>)]) -> String {
        let changes: Vec<(String, Option<String>)> = changes
            .iter()
            .map(|(p, t)| (p.to_string(), t.map(str::to_string)))
            .collect();
        self.state()
            .git
            .commit_on(crate::ledger::layout::BRANCH, &changes, "a hand edit")
    }

    /// A person merging a side commit into `fl/ledger`: two parents.
    pub fn hand_merge(&self) -> String {
        let branch = crate::ledger::layout::BRANCH;
        let mut s = self.state();
        let git = &mut s.git;
        let head = git.head(branch).expect("a ledger to merge into");
        let tree = git.commits[&head].tree.clone();
        let side = git.put_commit(&tree, vec![head.clone()], "a side commit");
        let merge = git.put_commit(&tree, vec![head, side], "a merge");
        git.refs.insert(format!("heads/{branch}"), merge.clone());
        merge
    }

    /// A person rewriting `fl/ledger`: a new first commit holding `files`,
    /// forced onto the branch.
    pub fn rewrite_ledger(&self, files: &[(&str, &str)]) -> String {
        crate::fake_git::seed(&mut self.state().git, files)
    }

    pub fn delete_ledger(&self) {
        let branch = crate::ledger::layout::BRANCH;
        self.state().git.refs.remove(&format!("heads/{branch}"));
    }

    /// How many commits `fl/ledger` holds along first parents.
    pub fn ledger_commits(&self) -> usize {
        self.state()
            .git
            .first_parents(crate::ledger::layout::BRANCH)
            .len()
    }
```

Prepend to `crates/github/src/fake_git.rs` (above its tests):

```rust
//! The fake's git objects, and the REST endpoints the GitHub ledger uses
//! (GitHub ledger spec §8.1): refs, trees, commits, blobs, compare, and the
//! rules on a branch.
//!
//! ⚠ It proves structure, not integration: object ids are SHA-256 based,
//! not git's, and each answer's shape is fl's reading of GitHub's
//! documentation. A shape no live test confirms yet says so.

use crate::fake::{Answer, State, answer};
use crate::ledger::layout::BRANCH;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// One commit: its tree, its parents (first parent first), its message.
#[derive(Debug, Clone)]
pub struct FakeCommit {
    pub tree: String,
    pub parents: Vec<String>,
    pub message: String,
}

/// A ruleset on one branch. Only an `active` one applies.
#[derive(Debug, Clone)]
pub struct Ruleset {
    pub enforcement: String,
    pub branch: String,
    pub rules: Vec<String>,
}

impl Ruleset {
    /// A ruleset on `fl/ledger`, enforced as `enforcement` (`active`,
    /// `disabled` or `evaluate`), with `rules`.
    pub fn on_ledger(enforcement: &str, rules: &[&str]) -> Ruleset {
        Ruleset {
            enforcement: enforcement.into(),
            branch: BRANCH.into(),
            rules: rules.iter().map(|r| r.to_string()).collect(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Git {
    /// blob id → text.
    pub blobs: BTreeMap<String, String>,
    /// tree id → every file of the tree, path → blob id.
    pub trees: BTreeMap<String, BTreeMap<String, String>>,
    pub commits: BTreeMap<String, FakeCommit>,
    /// `heads/<branch>` → commit id.
    pub refs: BTreeMap<String, String>,
    clock: u64,
}

/// A 40-hex id, shaped like git's, from `parts`.
fn object_id(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    h.finalize()
        .iter()
        .take(20)
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Git {
    pub fn put_blob(&mut self, text: &str) -> String {
        let id = object_id(&["blob", text]);
        self.blobs.insert(id.clone(), text.to_string());
        id
    }

    /// A tree holding `files` (path → text).
    pub fn put_tree(&mut self, files: &BTreeMap<String, String>) -> String {
        let entries: BTreeMap<String, String> = files
            .iter()
            .map(|(p, t)| (p.clone(), self.put_blob(t)))
            .collect();
        let listing: Vec<String> = entries.iter().map(|(p, b)| format!("{p} {b}")).collect();
        let id = object_id(&["tree", &listing.join("\n")]);
        self.trees.insert(id.clone(), entries);
        id
    }

    pub fn put_commit(&mut self, tree: &str, parents: Vec<String>, message: &str) -> String {
        self.clock += 1;
        let id = object_id(&[
            "commit",
            tree,
            &parents.join(" "),
            message,
            &self.clock.to_string(),
        ]);
        self.commits.insert(
            id.clone(),
            FakeCommit {
                tree: tree.to_string(),
                parents,
                message: message.to_string(),
            },
        );
        id
    }

    pub fn head(&self, branch: &str) -> Option<String> {
        self.refs.get(&format!("heads/{branch}")).cloned()
    }

    /// Every file of `commit`, path → text.
    pub fn files_at(&self, commit: &str) -> Option<BTreeMap<String, String>> {
        let tree = self.trees.get(&self.commits.get(commit)?.tree)?;
        Some(
            tree.iter()
                .map(|(p, b)| (p.clone(), self.blobs[b].clone()))
                .collect(),
        )
    }

    /// Whether `a` is `b` or one of its ancestors.
    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        let mut todo = vec![b.to_string()];
        let mut seen = BTreeSet::new();
        while let Some(c) = todo.pop() {
            if c == a {
                return true;
            }
            if seen.insert(c.clone())
                && let Some(commit) = self.commits.get(&c)
            {
                todo.extend(commit.parents.iter().cloned());
            }
        }
        false
    }

    /// A commit on top of `branch`'s head writing (`Some`) or removing
    /// (`None`) each path; moves the branch, creating it if absent.
    pub fn commit_on(
        &mut self,
        branch: &str,
        changes: &[(String, Option<String>)],
        message: &str,
    ) -> String {
        let head = self.head(branch);
        let mut files = head
            .as_deref()
            .and_then(|h| self.files_at(h))
            .unwrap_or_default();
        for (path, text) in changes {
            match text {
                Some(t) => {
                    files.insert(path.clone(), t.clone());
                }
                None => {
                    files.remove(path);
                }
            }
        }
        let tree = self.put_tree(&files);
        let id = self.put_commit(&tree, head.into_iter().collect(), message);
        self.refs.insert(format!("heads/{branch}"), id.clone());
        id
    }

    /// The commits reachable from `branch`'s head along first parents,
    /// newest first.
    pub fn first_parents(&self, branch: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut at = self.head(branch);
        while let Some(c) = at {
            at = self
                .commits
                .get(&c)
                .and_then(|k| k.parents.first().cloned());
            out.push(c);
        }
        out
    }
}

/// A first commit holding `files`, with no parent, forced onto `fl/ledger`.
pub fn seed(git: &mut Git, files: &[(&str, &str)]) -> String {
    let files: BTreeMap<String, String> = files
        .iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect();
    let tree = git.put_tree(&files);
    let id = git.put_commit(&tree, vec![], "fl: start the ledger");
    git.refs.insert(format!("heads/{BRANCH}"), id.clone());
    id
}

fn not_found() -> Answer {
    answer(404, json!({"message": "Not Found"}))
}

fn unprocessable(message: &str) -> Answer {
    answer(422, json!({"message": message}))
}

fn commit_json(sha: &str, c: &FakeCommit) -> Value {
    json!({
        "sha": sha,
        "tree": {"sha": c.tree},
        "parents": c.parents.iter().map(|p| json!({"sha": p})).collect::<Vec<_>>(),
        "message": c.message,
    })
}

/// Base64 in lines of 60, as GitHub's blob API sends it.
fn wrapped(text: &str) -> String {
    let b64 = STANDARD.encode(text.as_bytes());
    let mut out = String::new();
    for chunk in b64.as_bytes().chunks(60) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push('\n');
    }
    out
}

/// The ledger's REST endpoints; `None` for every other route.
pub(crate) fn rest(s: &mut State, method: &str, parts: &[&str], body: &str) -> Option<Answer> {
    let ["repos", o, r, rest @ ..] = parts else {
        return None;
    };
    if !s.is_bound(o, r) {
        return None;
    }
    let full = format!("{o}/{r}");
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    Some(match (method, rest) {
        ("GET", ["git", "ref", name @ ..]) => {
            let name = name.join("/");
            match s.git.refs.get(&name) {
                Some(sha) => answer(
                    200,
                    json!({"ref": format!("refs/{name}"), "object": {"sha": sha, "type": "commit"}}),
                ),
                None => not_found(),
            }
        }
        ("POST", ["git", "refs"]) => create_ref(s, &v),
        ("POST", ["git", "trees"]) => create_tree(s, &v),
        ("POST", ["git", "commits"]) => create_commit(s, &v),
        ("GET", ["git", "commits", sha]) => match s.git.commits.get(*sha) {
            Some(c) => answer(200, commit_json(sha, c)),
            None => not_found(),
        },
        ("GET", ["git", "trees", sha]) => get_tree(s, sha),
        ("GET", ["git", "blobs", sha]) => match s.git.blobs.get(*sha) {
            Some(t) => answer(
                200,
                json!({"sha": sha, "size": t.len(), "encoding": "base64", "content": wrapped(t)}),
            ),
            None => not_found(),
        },
        ("GET", ["compare", spec]) => compare(s, spec),
        ("GET", ["rules", "branches", name @ ..]) => rules(s, &full, &name.join("/")),
        _ => return None,
    })
}

/// ⚠ Modelled: git keeps a branch as a file, so `fl` and `fl/ledger`
/// cannot both exist, and GitHub refuses the second with a 422. Confirmed by
/// live test `init_sets_up_a_ledger_on_a_private_repository`.
fn create_ref(s: &mut State, v: &Value) -> Answer {
    if std::mem::take(&mut s.fail_next_ref_create) {
        return answer(500, json!({"message": "fake failure before the ref was created"}));
    }
    let (Some(full), Some(sha)) = (v["ref"].as_str(), v["sha"].as_str()) else {
        return unprocessable("ref and sha are required");
    };
    let Some(name) = full.strip_prefix("refs/") else {
        return unprocessable("Reference name must start with 'refs/'");
    };
    if std::mem::take(&mut s.race_next_ref_create) {
        // Someone else creates the same branch a moment earlier.
        let tree = s.git.put_tree(&BTreeMap::new());
        let theirs = s.git.put_commit(&tree, vec![], "someone else's start");
        s.git.refs.insert(name.to_string(), theirs);
    }
    if s.git.refs.contains_key(name) {
        return unprocessable("Reference already exists");
    }
    let collides = s
        .git
        .refs
        .keys()
        .any(|r| r.starts_with(&format!("{name}/")) || name.starts_with(&format!("{r}/")));
    if collides {
        return unprocessable(&format!("'{full}' conflicts with an existing ref"));
    }
    if !s.git.commits.contains_key(sha) {
        return unprocessable("Object does not exist");
    }
    s.git.refs.insert(name.to_string(), sha.to_string());
    answer(201, json!({"ref": full, "object": {"sha": sha, "type": "commit"}}))
}

fn create_tree(s: &mut State, v: &Value) -> Answer {
    if !v["base_tree"].is_null() {
        return unprocessable("the fake builds trees from scratch only");
    }
    let Some(entries) = v["tree"].as_array() else {
        return unprocessable("tree is required");
    };
    let mut files = BTreeMap::new();
    for e in entries {
        match (e["path"].as_str(), e["type"].as_str(), e["content"].as_str()) {
            (Some(p), Some("blob"), Some(c)) => {
                files.insert(p.to_string(), c.to_string());
            }
            _ => return unprocessable("the fake takes only blobs given by content"),
        }
    }
    let sha = s.git.put_tree(&files);
    answer(201, json!({"sha": sha}))
}

fn create_commit(s: &mut State, v: &Value) -> Answer {
    let (Some(tree), Some(message)) = (v["tree"].as_str(), v["message"].as_str()) else {
        return unprocessable("tree and message are required");
    };
    if !s.git.trees.contains_key(tree) {
        return unprocessable("Tree SHA does not exist");
    }
    let parents: Vec<String> = v["parents"]
        .as_array()
        .map(|a| a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if parents.iter().any(|p| !s.git.commits.contains_key(p)) {
        return unprocessable("Parent SHA does not exist");
    }
    let sha = s.git.put_commit(tree, parents, message);
    answer(201, commit_json(&sha, &s.git.commits[&sha]))
}

fn get_tree(s: &State, sha: &str) -> Answer {
    let Some(files) = s.git.trees.get(sha) else {
        return not_found();
    };
    let mut dirs = BTreeSet::new();
    let mut items = Vec::new();
    for (path, blob) in files {
        let mut at = path.as_str();
        while let Some((parent, _)) = at.rsplit_once('/') {
            dirs.insert(parent.to_string());
            at = parent;
        }
        items.push(json!({
            "path": path, "mode": "100644", "type": "blob", "sha": blob,
            "size": s.git.blobs[blob].len(),
        }));
    }
    for d in dirs {
        items.push(json!({
            "path": d, "mode": "040000", "type": "tree",
            "sha": object_id(&["subtree", sha, &d]),
        }));
    }
    answer(200, json!({"sha": sha, "tree": items, "truncated": false}))
}

/// ⚠ Modelled: `GET /compare/{base}...{head}` names `status` as
/// `identical`, `ahead`, `behind` or `diverged`, and answers 404 for a
/// commit it does not hold. Confirmed by live test
/// `a_hand_edit_is_detected_and_named`.
fn compare(s: &mut State, spec: &str) -> Answer {
    let Some((base, head)) = spec.split_once("...") else {
        return not_found();
    };
    if !s.git.commits.contains_key(base) || !s.git.commits.contains_key(head) {
        return not_found();
    }
    let status = if s.compare_behind_next > 0 {
        s.compare_behind_next -= 1;
        "behind"
    } else if base == head {
        "identical"
    } else if s.git.is_ancestor(base, head) {
        "ahead"
    } else if s.git.is_ancestor(head, base) {
        "behind"
    } else {
        "diverged"
    };
    answer(200, json!({"status": status}))
}

/// ⚠ Modelled: `GET /rules/branches/{branch}` lists the rules IN FORCE on
/// the branch — a disabled or evaluate-only ruleset contributes none — and
/// a plan without rulesets answers 403 with an upgrade message. Confirmed
/// by live tests `rules_on_the_ledger_branch_are_readable` and
/// `a_private_repository_without_a_ruleset_is_detection_only`.
fn rules(s: &State, full: &str, branch: &str) -> Answer {
    if s.rules_need_upgrade {
        return answer(
            403,
            json!({"message": "Upgrade to GitHub Pro or make this repository public to enable this feature."}),
        );
    }
    let items: Vec<Value> = s
        .rulesets
        .iter()
        .enumerate()
        .filter(|(_, r)| r.enforcement == "active" && r.branch == branch)
        .flat_map(|(i, r)| {
            r.rules.iter().map(move |rule| {
                json!({
                    "type": rule, "ruleset_source_type": "Repository",
                    "ruleset_source": full, "ruleset_id": i + 1,
                })
            })
        })
        .collect();
    answer(200, Value::Array(items))
}

```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fl-github`
Expected: PASS — the new tests, and every existing client, tracker and creds test.

- [ ] **Step 5: Mutation checks**

1. `Client::graphql_answer`: drop the RATE_LIMITED check → `a_graphql_answer_hands_its_errors…` red (and `a_graphql_not_found_is_an_answer_and_a_graphql_rate_limit_is_an_error`).
2. `Client::graphql`: drop the status check → `a_graphql_query_answered_with_a_5xx_is_an_error_not_data` red.
3. `send`: drop the `403 if accepted.is_some()` arm → `a_refusal_for_want_of_a_permission_names_the_permission` red.
4. fake `create_ref`: drop the collision check → `a_ref_that_exists_or_collides…` red; drop the exists check → same.
5. fake `compare`: drop the `compare_behind_next` branch → `a_compare_says_how_two_commits_relate_and_can_lag` red.
6. fake `rules`: drop the `enforcement == "active"` filter → `the_rules_on_a_branch_are_those_of_active_rulesets` red.
7. fake `route`: drop the `down` check → `a_fake_that_is_down_is_unreachable` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/client.rs crates/github/src/lib.rs crates/github/src/fake.rs crates/github/src/fake_git.rs
git commit -m "feat(github): GraphQL answers the caller judges; the fake's git objects

Client::graphql_answer hands back the status, data and errors (a spent
rate limit is still refused); a 403 that names its missing permission says
so. The fake gains git blobs, trees, commits and refs, and the REST
endpoints the ledger uses: refs, trees, commits, blobs, compare and the
rules on a branch, with knobs for lag, down, a racing or failing ref, and
the plan's upgrade refusal. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---
### Task 8: The fake's ledger GraphQL — listing, `createCommitOnBranch`, blame

**Files:**
- Modify: `crates/github/src/fake_git.rs` (three operations, tests)
- Modify: `crates/github/src/fake.rs:47-165` (`State`: four knobs), `:920-921` (the GraphQL arm)

**Interfaces:**
- Consumes: `Git`, `State`, `answer` (Task 7).
- Produces: the operations `query ledgerObjects(…)`, `query ledgerBlame(…)` and `mutation ledgerAppend(…)` on the fake, told apart by operation name; and

```rust
// fl_github::fake::State — new knobs
pub foreign_appends: Vec<(String, String)>,  // before each commit: someone appends (path, line)
pub fail_commits: u32,                       // the next N commits answer 502, nothing lands
pub hang_up_after_next_commit: bool,         // lands, then the answer is lost; one-shot
pub refuse_next_commit_for: Option<String>,  // 403 naming this permission; one-shot
```

The three operations, exactly as `GithubLedger` sends them (Tasks 9 and 10):

```graphql
query ledgerObjects($owner: String!, $name: String!, $e0: String!, …) {
  repository(owner: $owner, name: $name) {
    e0: object(expression: $e0) { __typename ... on Tree { entries { name oid type } } ... on Blob { oid } }
    …
  }
}
query ledgerBlame($owner: String!, $name: String!, $commit: GitObjectID!, $path: String!) {
  repository(owner: $owner, name: $name) {
    object(oid: $commit) { ... on Commit { blame(path: $path) { ranges { startingLine endingLine commit { oid } } } } }
  }
}
mutation ledgerAppend($input: CreateCommitOnBranchInput!) {
  createCommitOnBranch(input: $input) { commit { oid } }
}
```

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module of `crates/github/src/fake_git.rs`:

```rust
    const OBJECTS: &str = "query ledgerObjects($owner: String!, $name: String!, $e0: String!, $e1: String!, $e2: String!, $e3: String!) { repository(owner: $owner, name: $name) { e0: object(expression: $e0) { __typename } } }";
    const APPEND: &str = "mutation ledgerAppend($input: CreateCommitOnBranchInput!) { createCommitOnBranch(input: $input) { commit { oid } } }";
    const BLAME: &str = "query ledgerBlame($owner: String!, $name: String!, $commit: GitObjectID!, $path: String!) { repository(owner: $owner, name: $name) { object(oid: $commit) { __typename } } }";

    fn append_input(head: &str, path: &str, text: &str) -> serde_json::Value {
        use base64::Engine;
        json!({"input": {
            "branch": {"repositoryNameWithOwner": "acme/widgets", "branchName": "fl/ledger"},
            "message": {"headline": "fl: a test append"},
            "expectedHeadOid": head,
            "fileChanges": {"additions": [{
                "path": path,
                "contents": base64::engine::general_purpose::STANDARD.encode(text),
            }]},
        }})
    }

    // ⚠ Modelled: `object(expression: "<commit>:<path>")` answers a tree's
    // entries, a blob's id, or `null` for a path the commit does not hold.
    #[test]
    fn the_objects_at_a_commit_are_listed_by_expression() {
        let fake = FakeGithub::start("acme/widgets");
        fake.seed_ledger();
        let head = fake.hand_commit(&[
            ("runs/k/1.jsonl", Some("a\n")),
            ("runs/k/2.jsonl", Some("b\n")),
        ]);
        let a = client(&fake)
            .graphql_answer(
                OBJECTS,
                json!({
                    "owner": "acme", "name": "widgets",
                    "e0": format!("{head}:format"), "e1": format!("{head}:runs/k"),
                    "e2": format!("{head}:runs"), "e3": format!("{head}:nothing"),
                }),
            )
            .unwrap();
        let repo = &a.data.unwrap()["repository"];
        assert_eq!(repo["e0"]["__typename"], "Blob");
        assert_eq!(repo["e0"]["oid"].as_str().map(str::len), Some(40));
        let names: Vec<&str> = repo["e1"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["1.jsonl", "2.jsonl"]);
        assert_eq!(repo["e2"]["entries"][0]["type"], "tree");
        assert!(repo["e3"].is_null());
    }

    #[test]
    fn an_append_lands_on_the_head_it_expects_and_is_refused_on_any_other() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let c = client(&fake);
        let a = c
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap();
        let oid = a.data.unwrap()["createCommitOnBranch"]["commit"]["oid"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(fake.ledger_head(), Some(oid));
        assert_eq!(fake.ledger_files()["runs/k/1.jsonl"], "a\n");
        // ⚠ Modelled: a stale expectedHeadOid is refused with STALE_DATA.
        // Confirmed by live test `create_commit_on_branch_is_refused_when_the_head_moved`.
        let stale = c
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\nb\n"))
            .unwrap();
        assert_eq!(stale.errors[0]["type"], "STALE_DATA");
        assert_eq!(fake.ledger_files()["runs/k/1.jsonl"], "a\n", "nothing landed");
    }

    #[test]
    fn another_machines_append_can_land_first() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.state()
            .foreign_appends
            .push(("runs/k/1.jsonl".into(), "theirs".into()));
        let a = client(&fake)
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "mine\n"))
            .unwrap();
        assert_eq!(a.errors[0]["type"], "STALE_DATA");
        assert_eq!(fake.ledger_files()["runs/k/1.jsonl"], "theirs\n");
    }

    #[test]
    fn a_commit_that_did_not_land_answers_502_and_one_whose_answer_was_lost_landed() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let c = client(&fake);
        fake.state().fail_commits = 1;
        let a = c
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap();
        assert_eq!(a.status, 502);
        assert_eq!(fake.ledger_commits(), 1, "nothing landed");
        fake.state().hang_up_after_next_commit = true;
        let err = c
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert_eq!(fake.ledger_commits(), 2, "it landed; only the answer was lost");
    }

    #[test]
    fn a_commit_refused_for_want_of_a_permission_names_it() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.state().refuse_next_commit_for = Some("contents=write".into());
        let err = client(&fake)
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap_err();
        assert!(err.to_string().contains("contents=write"), "{err}");
    }

    #[test]
    fn an_append_to_a_branch_that_is_not_there_is_not_found() {
        let fake = FakeGithub::start("acme/widgets");
        let a = client(&fake)
            .graphql_answer(APPEND, append_input("0000", "runs/k/1.jsonl", "a\n"))
            .unwrap();
        assert_eq!(a.errors[0]["type"], "NOT_FOUND");
    }

    // ⚠ Modelled: blame names, for each line, the commit that last changed
    // it. Confirmed by live test `a_hand_edit_is_detected_and_named`.
    #[test]
    fn blame_names_the_commit_that_last_changed_each_line() {
        let fake = FakeGithub::start("acme/widgets");
        fake.seed_ledger();
        let first = fake.hand_commit(&[("f.jsonl", Some("a\nb\n"))]);
        let second = fake.hand_commit(&[("f.jsonl", Some("a\nB\nc\n"))]);
        let a = client(&fake)
            .graphql_answer(
                BLAME,
                json!({"owner": "acme", "name": "widgets", "commit": second, "path": "f.jsonl"}),
            )
            .unwrap();
        let ranges = a.data.unwrap()["repository"]["object"]["blame"]["ranges"].clone();
        assert_eq!(
            ranges,
            json!([
                {"startingLine": 1, "endingLine": 1, "commit": {"oid": first}},
                {"startingLine": 2, "endingLine": 3, "commit": {"oid": second}},
            ])
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-github --lib fake_git`
Expected: FAIL to compile — the four knobs do not exist; once they do, the GraphQL tests fail because the fake answers the operations as node lookups.

- [ ] **Step 3: Write the implementation**

Add to `State` in `crates/github/src/fake.rs` (after the knobs Task 7 added):

```rust
    /// Before each ledger commit is judged, someone else appends this line
    /// to this path — another machine landing first. Consumed one per
    /// commit, oldest first.
    pub foreign_appends: Vec<(String, String)>,
    /// The next this-many ledger commits answer 502 and nothing lands.
    pub fail_commits: u32,
    /// The next ledger commit lands, then its answer breaks off. One-shot.
    pub hang_up_after_next_commit: bool,
    /// The next ledger commit answers 403 naming this permission. One-shot.
    pub refuse_next_commit_for: Option<String>,
```

In `route`'s GraphQL arm, right after `let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);` (line 921), add:

```rust
            if let Some(a) = crate::fake_git::graphql(s, &v) {
                return a;
            }
```

Add to `crates/github/src/fake_git.rs` (above its tests):

```rust
/// The ledger's GraphQL operations, told apart by operation name; `None`
/// for every other query.
pub(crate) fn graphql(s: &mut State, v: &Value) -> Option<Answer> {
    let query = v["query"].as_str().unwrap_or("");
    let vars = &v["variables"];
    if query.starts_with("query ledgerObjects(") {
        return Some(objects(s, vars));
    }
    if query.starts_with("query ledgerBlame(") {
        return Some(blame(s, vars));
    }
    if query.starts_with("mutation ledgerAppend(") {
        return Some(append(s, vars));
    }
    None
}

fn repository_known(s: &State, vars: &Value) -> bool {
    match (vars["owner"].as_str(), vars["name"].as_str()) {
        (Some(o), Some(n)) => s.is_bound(o, n),
        _ => false,
    }
}

/// ⚠ Modelled: `object(expression: "<commit>:<path>")` answers a tree's
/// entries, a blob's id, or `null` — no error — for a path the commit does
/// not hold. Confirmed by live test `init_sets_up_a_ledger_on_a_private_repository`.
fn objects(s: &State, vars: &Value) -> Answer {
    if !repository_known(s, vars) {
        return answer(
            200,
            json!({"data": {"repository": null}, "errors": [{"type": "NOT_FOUND"}]}),
        );
    }
    let mut repo = serde_json::Map::new();
    if let Some(all) = vars.as_object() {
        for (k, e) in all {
            if k.starts_with('e') && k[1..].parse::<u32>().is_ok() {
                repo.insert(k.clone(), object_at(&s.git, e.as_str().unwrap_or("")));
            }
        }
    }
    answer(200, json!({"data": {"repository": repo}}))
}

fn object_at(git: &Git, expression: &str) -> Value {
    let Some((commit, path)) = expression.split_once(':') else {
        return Value::Null;
    };
    let Some(files) = git.commits.get(commit).and_then(|c| git.trees.get(&c.tree)) else {
        return Value::Null;
    };
    if let Some(blob) = files.get(path) {
        return json!({"__typename": "Blob", "oid": blob, "byteSize": git.blobs[blob].len()});
    }
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    };
    let mut entries: BTreeMap<String, Value> = BTreeMap::new();
    for (p, blob) in files {
        let Some(rest) = p.strip_prefix(prefix.as_str()) else {
            continue;
        };
        match rest.split_once('/') {
            Some((dir, _)) => {
                entries.entry(dir.to_string()).or_insert_with(|| {
                    json!({
                        "name": dir, "type": "tree",
                        "oid": object_id(&["subtree", commit, &format!("{prefix}{dir}")]),
                    })
                });
            }
            None => {
                entries.insert(
                    rest.to_string(),
                    json!({"name": rest, "oid": blob, "type": "blob"}),
                );
            }
        }
    }
    if entries.is_empty() {
        return Value::Null;
    }
    json!({"__typename": "Tree", "entries": entries.into_values().collect::<Vec<_>>()})
}

/// ⚠ Modelled: `Commit.blame(path:)` answers ranges of lines, each with the
/// commit that last changed them. The fake follows first parents and
/// compares line by line. Confirmed by live test
/// `a_hand_edit_is_detected_and_named`.
fn blame(s: &State, vars: &Value) -> Answer {
    let nothing = || answer(200, json!({"data": {"repository": {"object": null}}}));
    let (Some(commit), Some(path)) = (vars["commit"].as_str(), vars["path"].as_str()) else {
        return nothing();
    };
    if !repository_known(s, vars) || !s.git.commits.contains_key(commit) {
        return nothing();
    }
    let mut chain = Vec::new();
    let mut at = Some(commit.to_string());
    while let Some(c) = at {
        at = s.git.commits[&c].parents.first().cloned();
        chain.push(c);
    }
    chain.reverse();
    let mut owners: Vec<String> = Vec::new();
    let mut before: Vec<String> = Vec::new();
    for c in &chain {
        let text = s
            .git
            .files_at(c)
            .and_then(|f| f.get(path).cloned())
            .unwrap_or_default();
        let now: Vec<String> = text.lines().map(str::to_string).collect();
        owners.truncate(now.len());
        for (i, line) in now.iter().enumerate() {
            if before.get(i) != Some(line) {
                if i < owners.len() {
                    owners[i] = c.clone();
                } else {
                    owners.push(c.clone());
                }
            }
        }
        before = now;
    }
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for i in 1..=owners.len() {
        if i == owners.len() || owners[i] != owners[start] {
            ranges.push(json!({
                "startingLine": start + 1, "endingLine": i,
                "commit": {"oid": owners[start]},
            }));
            start = i;
        }
    }
    answer(
        200,
        json!({"data": {"repository": {"object": {"blame": {"ranges": ranges}}}}}),
    )
}

/// ⚠ Modelled from GitHub's documentation: `createCommitOnBranch` refuses a
/// stale `expectedHeadOid` with an error of type `STALE_DATA` whose message
/// says where the branch was expected to point, and lands nothing.
/// Confirmed by live test `create_commit_on_branch_is_refused_when_the_head_moved`.
fn append(s: &mut State, vars: &Value) -> Answer {
    let input = &vars["input"];
    let branch = input
        .pointer("/branch/branchName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let named = input
        .pointer("/branch/repositoryNameWithOwner")
        .and_then(Value::as_str)
        .unwrap_or("");
    let bound = named.split_once('/').is_some_and(|(o, r)| s.is_bound(o, r));
    if !bound || s.git.head(&branch).is_none() {
        return answer(
            200,
            json!({
                "data": {"createCommitOnBranch": null},
                "errors": [{"type": "NOT_FOUND", "message": format!("Could not resolve to a ref named `{branch}`")}],
            }),
        );
    }
    if let Some(needs) = s.refuse_next_commit_for.take() {
        let mut a = answer(403, json!({"message": "Resource not accessible by integration"}));
        a.headers
            .push(("x-accepted-github-permissions".into(), needs));
        return a;
    }
    if !s.foreign_appends.is_empty() {
        let (path, line) = s.foreign_appends.remove(0);
        let head = s.git.head(&branch).expect("checked above");
        let mut text = s
            .git
            .files_at(&head)
            .and_then(|f| f.get(&path).cloned())
            .unwrap_or_default();
        text.push_str(&line);
        text.push('\n');
        s.git
            .commit_on(&branch, &[(path, Some(text))], "another machine's append");
    }
    let head = s.git.head(&branch).expect("checked above");
    let expected = input["expectedHeadOid"].as_str().unwrap_or("");
    if expected != head {
        return answer(
            200,
            json!({
                "data": {"createCommitOnBranch": null},
                "errors": [{
                    "type": "STALE_DATA", "path": ["createCommitOnBranch"],
                    "message": format!("Expected branch to point to \"{expected}\" but it did not. Pull and try again."),
                }],
            }),
        );
    }
    if s.fail_commits > 0 {
        s.fail_commits -= 1;
        return answer(502, json!({"message": "fake: the commit did not land"}));
    }
    let mut changes = Vec::new();
    let additions = input
        .pointer("/fileChanges/additions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for add in additions {
        let text = add["contents"]
            .as_str()
            .and_then(|c| STANDARD.decode(c).ok())
            .and_then(|b| String::from_utf8(b).ok());
        let (Some(path), Some(text)) = (add["path"].as_str(), text) else {
            return answer(
                200,
                json!({"errors": [{"type": "UNPROCESSABLE", "message": "an addition needs a path and base64 contents"}]}),
            );
        };
        changes.push((path.to_string(), Some(text)));
    }
    let headline = input
        .pointer("/message/headline")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let oid = s.git.commit_on(&branch, &changes, &headline);
    if std::mem::take(&mut s.hang_up_after_next_commit) {
        let mut a = answer(200, Value::Null);
        a.hang_up = true;
        return a;
    }
    answer(
        200,
        json!({"data": {"createCommitOnBranch": {"commit": {"oid": oid}}}}),
    )
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fl-github`
Expected: PASS — the new tests, and every existing tracker test (whose node lookups and edit-history queries do not start with a ledger operation name).

- [ ] **Step 5: Mutation checks**

1. `append`: skip the `expected != head` check → `an_append_lands_on_the_head_it_expects…` red ("nothing landed").
2. `append`: apply `foreign_appends` after the expectation check → `another_machines_append_can_land_first` red.
3. `append`: land nothing under `hang_up_after_next_commit` → `a_commit_that_did_not_land_answers_502…` red ("it landed").
4. `append`: land the commit while `fail_commits` is above zero → the same test red ("nothing landed").
5. `object_at`: answer a directory's whole subtree instead of its immediate entries → `the_objects_at_a_commit_are_listed_by_expression` red (`e2` would list files).
6. `blame`: attribute every line to the newest commit → `blame_names_the_commit_that_last_changed_each_line` red.
7. `graphql`: answer every query as `ledgerObjects` (return `Some(objects(s, vars))` first) → the tracker's node-lookup tests in `tracker.rs` (every test that reads a record back by its node id) red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/fake.rs crates/github/src/fake_git.rs
git commit -m "test(github): the fake answers the ledger's GraphQL

ledgerObjects lists a commit's trees and blobs by expression,
ledgerAppend is createCommitOnBranch with expectedHeadOid (stale data,
another machine landing first, a 502 that lands nothing, a lost answer
that landed, a refused permission), and ledgerBlame names the commit that
last changed each line. Each shape is marked Modelled until a live test
confirms it. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 9: `GithubLedger` reads the branch — the seven checks

**Files:**
- Modify: `crates/github/src/ledger/mod.rs` (the struct)
- Create: `crates/github/src/ledger/git.rs`
- Create: `crates/github/src/ledger/read.rs`
- Modify: `crates/github/src/lib.rs:14` (export)
- Modify: `crates/github/src/tracker.rs` (after `repo()`, line 276: `client()`)

**Interfaces:**
- Consumes: `LedgerMemory`, `CachedSegment` (Task 5); `layout` (Task 6); `GraphqlAnswer` (Task 7); the fake (Tasks 7–8); `owner::issue_of_repository` (plan A).
- Produces:

```rust
// fl_github::ledger (GithubLedger re-exported at fl_github::GithubLedger)
pub struct GithubLedger<'a> { /* client, repo, local, lag */ }
impl<'a> GithubLedger<'a> {
    pub fn new(client: &'a Client, repo: Repo, local: &'a dyn LedgerMemory) -> Self;
    #[doc(hidden)] pub fn with_lag(self, reads: u32, pause: Duration) -> Self; // default 3 reads, 500 ms
    pub fn repo(&self) -> &Repo;
    pub fn check_head(&self) -> Result<String, StoreError>;     // checks 1 and 2
    pub fn runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError>;
    pub fn attempts_of(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError>;
    pub fn decisions(&self, subject: &Iri) -> Result<Vec<Decision>, StoreError>;
    pub fn owns(&self, record: &RecordId) -> Result<bool, StoreError>;   // local, no request
    pub fn take_notes(&self) -> Vec<Note>;
}
pub enum Note { Quarantined { file: String, line: u64, reason: String } } // Display, Ord

// crate-internal, used by Tasks 10–12
pub(crate) struct Segment { pub path: String, pub text: String }
pub(crate) struct Snapshot { pub head: String, pub dirs: BTreeMap<String, Vec<Segment>>, pub quarantine: String }
impl GithubLedger<'_> {
    pub(crate) fn snapshot(&self, dirs: &[String]) -> Result<Snapshot, StoreError>; // checks 1–4, 7
    pub(crate) fn lines(&self, snap: &Snapshot, area: Area, dir: &str) -> Result<Vec<Line>, StoreError>; // 5, 6
    pub(crate) fn quarantine_lines(&self, snap: &Snapshot) -> Result<Vec<QuarantineLine>, StoreError>;
    pub(crate) fn altered(&self, file: &str, what: impl Into<String>, commit: &str) -> StoreError;
    pub(crate) fn path(&self, rest: &str) -> String;                       // "/repos/{full_name}{rest}"
    pub(crate) fn branch_head(&self, branch: &str) -> Result<Option<String>, StoreError>;
    pub(crate) fn compare(&self, base: &str, head: &str) -> Result<Option<String>, StoreError>;
    pub(crate) fn blob_text(&self, oid: &str) -> Result<String, StoreError>;
}

// fl_github::GithubTracker
pub fn client(&self) -> &Client;              // for B2, which builds the ledger over the tracker's client
```

- [ ] **Step 1: Write the failing tests**

Replace `crates/github/src/ledger/mod.rs` with:

```rust
//! The GitHub ledger (GitHub ledger spec §1.1, §3): the `fl/ledger` branch
//! of the repository that backs the tracker, where mode B publishes each
//! decision and the evidence it rests on.

pub mod disclose;
mod git;
pub mod layout;
mod read;

use crate::client::Client;
use crate::tracker::Repo;
use fl_core::split::LedgerMemory;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::time::Duration;

pub use disclose::Visibility;
pub use read::Note;

/// The GitHub side of mode B, for one repository and one command.
///
/// ⚠ It borrows the tracker's `Client` — one credential, one origin guard,
/// one rate-limit handling (spec §1.1) — and the local store, which keeps
/// the anchor, the last head this machine checked, and every file it read.
pub struct GithubLedger<'a> {
    pub(crate) client: &'a Client,
    pub(crate) repo: Repo,
    pub(crate) local: &'a dyn LedgerMemory,
    /// How many times a `behind` answer is read again before it counts
    /// (spec §3.5 check 2), and the pause between reads.
    pub(crate) lag_reads: u32,
    pub(crate) lag_pause: Duration,
    /// What reads noted without refusing, once each.
    pub(crate) notes: RefCell<BTreeSet<Note>>,
}

impl<'a> GithubLedger<'a> {
    pub fn new(client: &'a Client, repo: Repo, local: &'a dyn LedgerMemory) -> Self {
        Self {
            client,
            repo,
            local,
            lag_reads: 3,
            lag_pause: Duration::from_millis(500),
            notes: RefCell::new(BTreeSet::new()),
        }
    }

    /// Tests only: how often a lagging `behind` is read again, and the
    /// pause between reads.
    #[doc(hidden)]
    pub fn with_lag(mut self, reads: u32, pause: Duration) -> Self {
        self.lag_reads = reads;
        self.lag_pause = pause;
        self
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// What reads noted since the last call (a quarantined line skipped),
    /// once each, for the command to print.
    pub fn take_notes(&self) -> Vec<Note> {
        std::mem::take(&mut *self.notes.borrow_mut())
            .into_iter()
            .collect()
    }
}
```

Create `crates/github/src/ledger/read.rs` with only its tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use crate::tracker::Repo;
    use fl_core::MemStore;
    use fl_core::at::At;
    use fl_core::conformance::{sample_attempt, sample_decision, sample_record_run};
    use fl_core::ids::seq_iri;
    use fl_core::split::LedgerCache;
    use fl_core::store::Bindings;
    use std::time::Duration;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    fn repo() -> Repo {
        Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        }
    }

    /// A ledger started on the fake, and a machine that records its anchor.
    fn world() -> (FakeGithub, MemStore, String) {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        (fake, local, root)
    }

    fn open<'a>(c: &'a Client, local: &'a MemStore) -> GithubLedger<'a> {
        GithubLedger::new(c, repo(), local).with_lag(2, Duration::ZERO)
    }

    fn gate() -> GateId {
        GateId(seq_iri(7))
    }

    fn record() -> RecordId {
        RecordId(Iri::parse("https://github.com/acme/widgets/issues/1").unwrap())
    }

    fn run(n: u64) -> GateRun {
        sample_record_run(n, &gate(), Some(&record()))
    }

    fn runs_dir() -> String {
        layout::dir(Area::Runs, gate().iri())
    }

    fn seg(n: u64) -> String {
        layout::segment_path(&runs_dir(), n)
    }

    /// A segment's text: each line and its newline.
    fn file(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    fn line(r: &GateRun) -> String {
        Line::Run(r.clone()).encode("someone")
    }

    fn blob_reads(fake: &FakeGithub) -> usize {
        fake.state()
            .requests
            .iter()
            .filter(|r| r.contains("/git/blobs/"))
            .count()
    }

    #[test]
    fn a_fresh_ledger_reads_empty_and_remembers_the_head_it_checked() {
        let (fake, local, root) = world();
        let c = client(&fake);
        assert!(open(&c, &local).runs(&gate()).unwrap().is_empty());
        assert_eq!(local.last_head("R_1").unwrap(), Some(root));
    }

    // Spec §7: a ledger that is not there says which way, and what to do.
    #[test]
    fn a_ledger_that_is_not_there_says_why() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let local = MemStore::default();
        let err = open(&c, &local).check_head().unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::NotSetUp { .. })),
            "{err:?}"
        );
        fake.seed_ledger();
        let err = open(&c, &local).check_head().unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::NoAnchor { .. })),
            "{err:?}"
        );
        local
            .set_ledger_root("R_1", &fake.ledger_head().unwrap())
            .unwrap();
        fake.delete_ledger();
        let err = open(&c, &local).check_head().unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Deleted { .. })),
            "{err:?}"
        );
    }

    #[test]
    fn reading_while_github_is_down_is_an_error_not_empty() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        fake.state().down = true;
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    // ⚠ Spec §3.5 check 1.
    #[test]
    fn a_head_that_does_not_descend_from_the_anchor_is_a_rewrite() {
        let (fake, local, root) = world();
        fake.rewrite_ledger(&[("format", "1\n")]);
        let c = client(&fake);
        match open(&c, &local).check_head().unwrap_err() {
            StoreError::Ledger(LedgerFault::Rewritten { base, against, .. }) => {
                assert_eq!(base, root);
                assert!(against.contains("first commit"), "{against}");
            }
            other => panic!("{other:?}"),
        }
    }

    // ⚠ Spec §3.5 check 2: GitHub's replicas can briefly lag, so a `behind`
    // answer is read again before it counts.
    #[test]
    fn a_head_behind_the_last_one_seen_is_read_again_before_it_counts() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        open(&c, &local).runs(&gate()).unwrap();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        fake.state().compare_behind_next = 2;
        open(&c, &local)
            .check_head()
            .expect("two lagging answers, then `ahead`: no alarm");
    }

    #[test]
    fn a_head_that_stays_behind_is_a_rewrite() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let first = fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        open(&c, &local).runs(&gate()).unwrap();
        assert_eq!(local.last_head("R_1").unwrap(), Some(first.clone()));
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(2))]).as_str()),
        )]);
        fake.state().compare_behind_next = 3;
        match open(&c, &local).check_head().unwrap_err() {
            StoreError::Ledger(LedgerFault::Rewritten {
                base, against, how, ..
            }) => {
                assert_eq!(base, first);
                assert!(against.contains("last head"), "{against}");
                assert!(how.contains("behind"), "{how}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_branch_reset_to_an_older_commit_is_a_rewrite() {
        let (fake, local, root) = world();
        let c = client(&fake);
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        open(&c, &local).runs(&gate()).unwrap();
        fake.state()
            .git
            .refs
            .insert(format!("heads/{BRANCH}"), root);
        let err = open(&c, &local).check_head().unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Rewritten { .. })),
            "{err:?}"
        );
    }

    // ⚠ Spec §3.5 check 7.
    #[test]
    fn a_format_other_than_1_is_refused_naming_an_upgrade() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger_with(&[("format", "2\n"), ("README.md", "x")]);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::UnknownFormat { ref found, .. }) if found == "2"),
            "{err:?}"
        );
        assert!(err.to_string().contains("Upgrade fl"), "{err}");
    }

    #[test]
    fn a_ledger_with_no_format_file_is_altered() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger_with(&[("README.md", "x")]);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Altered { ref file, .. }) if file == FORMAT_FILE),
            "{err:?}"
        );
    }

    #[test]
    fn runs_read_back_in_the_order_they_were_written_across_segments() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[
            (seg(1).as_str(), Some(file(&[line(&run(1)), line(&run(2))]).as_str())),
            (seg(2).as_str(), Some(file(&[line(&run(3))]).as_str())),
        ]);
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).runs(&gate()).unwrap(),
            vec![run(1), run(2), run(3)]
        );
    }

    // ⚠ Spec §3.5 check 3: a closed segment never changes — not even by
    // growing, which an open one may.
    #[test]
    fn a_closed_segment_that_changes_is_altered() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[
            (seg(1).as_str(), Some(file(&[line(&run(1))]).as_str())),
            (seg(2).as_str(), Some(file(&[line(&run(2))]).as_str())),
        ]);
        let c = client(&fake);
        open(&c, &local).runs(&gate()).unwrap();
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(9))]).as_str()),
        )]);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Altered { ref file, ref what, .. }) if *file == seg(1) && what.contains("closed")),
            "{err:?}"
        );
    }

    // ⚠ Spec §3.5 check 4.
    #[test]
    fn an_open_segment_that_no_longer_starts_with_the_copy_read_before_is_altered() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(2))]).as_str()),
        )]);
        let c = client(&fake);
        open(&c, &local).runs(&gate()).unwrap();
        let mut edited = run(1);
        edited.population = 9;
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&edited), line(&run(2))]).as_str()),
        )]);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Altered { ref what, .. }) if what.contains("no longer starts")),
            "{err:?}"
        );
    }

    #[test]
    fn an_open_segment_that_grew_is_read_again_and_cached() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let c = client(&fake);
        open(&c, &local).runs(&gate()).unwrap();
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(2))]).as_str()),
        )]);
        assert_eq!(open(&c, &local).runs(&gate()).unwrap(), vec![run(1), run(2)]);
        let cached = local.cached("R_1", &seg(1)).unwrap().unwrap();
        assert!(cached.text.contains(&line(&run(2))));
    }

    #[test]
    fn a_segment_that_disappears_or_leaves_a_gap_is_altered() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(2).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Altered { ref what, .. }) if what.contains("missing a segment")),
            "{err:?}"
        );

        let (fake2, local2, _root2) = world();
        fake2.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let c2 = client(&fake2);
        open(&c2, &local2).runs(&gate()).unwrap();
        fake2.hand_commit(&[(seg(1).as_str(), None)]);
        let err = open(&c2, &local2).runs(&gate()).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Altered { ref file, ref what, .. }) if *file == seg(1) && what.contains("deleted")),
            "{err:?}"
        );
    }

    #[test]
    fn a_file_that_is_not_a_segment_or_a_file_where_a_directory_belongs_is_altered() {
        let (fake, local, _root) = world();
        let stray = format!("{}/notes.txt", runs_dir());
        fake.hand_commit(&[(stray.as_str(), Some("x"))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Altered { ref file, .. }) if *file == stray),
            "{err:?}"
        );

        let (fake2, local2, _root2) = world();
        fake2.hand_commit(&[(runs_dir().as_str(), Some("x"))]);
        let c2 = client(&fake2);
        let err = open(&c2, &local2).runs(&gate()).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Altered { ref what, .. }) if what.contains("directory")),
            "{err:?}"
        );
    }

    // Spec §3.3: a segment read before is not downloaded again.
    #[test]
    fn a_segment_read_before_is_not_downloaded_again() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[
            (seg(1).as_str(), Some(file(&[line(&run(1))]).as_str())),
            (seg(2).as_str(), Some(file(&[line(&run(2))]).as_str())),
        ]);
        let c = client(&fake);
        open(&c, &local).runs(&gate()).unwrap();
        let before = blob_reads(&fake);
        assert_eq!(open(&c, &local).runs(&gate()).unwrap(), vec![run(1), run(2)]);
        assert_eq!(blob_reads(&fake), before, "nothing downloaded twice");
    }

    // ⚠ Spec §3.3: an unreadable line names the file, the line, the commit
    // that added it, and the quarantine command.
    #[test]
    fn an_unreadable_line_names_its_file_line_commit_and_the_quarantine_command() {
        let (fake, local, _root) = world();
        let first = fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let damaged = format!("{}not json\n", file(&[line(&run(1))]));
        let bad = fake.hand_commit(&[(seg(1).as_str(), Some(damaged.as_str()))]);
        // A later commit that reads do not look at: the head is not `bad`.
        fake.hand_commit(&[("notes.txt", Some("x"))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        match &err {
            StoreError::Ledger(LedgerFault::Unreadable {
                file: f,
                line: n,
                commit,
                ..
            }) => {
                assert_eq!(f, &seg(1));
                assert_eq!(*n, 2);
                assert_eq!(commit, &bad);
                assert_ne!(commit, &first);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            err.to_string()
                .contains(&format!("fl github ledger quarantine {} 2", seg(1))),
            "{err}"
        );
    }

    // Spec §3.6: a quarantined line is skipped and reported; nothing is
    // removed.
    #[test]
    fn a_quarantined_line_is_skipped_and_noted_once() {
        let (fake, local, _root) = world();
        let q = QuarantineLine {
            id: seq_iri(50),
            at: At::from_unix_millis(50),
            file: seg(1),
            line: 2,
            quarantined_by: "Ada".into(),
            reason: "a hand edit".into(),
            by: "fake-user".into(),
        };
        let damaged = format!("{}not json\n{}", file(&[line(&run(1))]), file(&[line(&run(3))]));
        fake.hand_commit(&[
            (seg(1).as_str(), Some(damaged.as_str())),
            (QUARANTINE_FILE, Some(file(&[q.encode()]).as_str())),
        ]);
        let c = client(&fake);
        let l = open(&c, &local);
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1), run(3)]);
        l.runs(&gate()).unwrap();
        assert_eq!(
            l.take_notes(),
            vec![Note::Quarantined {
                file: seg(1),
                line: 2,
                reason: "a hand edit".into(),
            }],
            "once, however often it is read"
        );
        assert!(
            fake.ledger_files()[&seg(1)].contains("not json"),
            "nothing was removed"
        );
    }

    // ⚠ Spec §3.5 check 6.
    #[test]
    fn a_line_in_the_wrong_directory_is_misplaced() {
        let (fake, local, _root) = world();
        let elsewhere = sample_record_run(1, &GateId(seq_iri(8)), Some(&record()));
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&elsewhere)]).as_str()))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Misplaced { line: 1, .. })),
            "{err:?}"
        );
        assert!(err.to_string().contains("fl github ledger quarantine"), "{err}");
    }

    // ⚠ Spec §3.5 check 5: one id, one content.
    #[test]
    fn one_id_with_two_contents_is_tampered_and_an_identical_copy_is_read_once() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(1)), line(&run(2))]).as_str()),
        )]);
        let c = client(&fake);
        assert_eq!(open(&c, &local).runs(&gate()).unwrap(), vec![run(1), run(2)]);
        let mut other = run(1);
        other.commit = "def".into();
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(1)), line(&run(2)), line(&other)]).as_str()),
        )]);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        let want = run(1).id.unwrap();
        assert!(
            matches!(err, StoreError::Tampered { ref id, .. } if *id == want),
            "{err:?}"
        );
    }

    #[test]
    fn an_unreadable_or_vanished_quarantine_file_is_altered() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(QUARANTINE_FILE, Some("not json\n"))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Altered { ref file, .. }) if file == QUARANTINE_FILE),
            "{err:?}"
        );

        let (fake2, local2, _root2) = world();
        let q = QuarantineLine {
            id: seq_iri(50),
            at: At::from_unix_millis(50),
            file: seg(1),
            line: 1,
            quarantined_by: "Ada".into(),
            reason: "r".into(),
            by: "fake-user".into(),
        };
        fake2.hand_commit(&[(QUARANTINE_FILE, Some(file(&[q.encode()]).as_str()))]);
        let c2 = client(&fake2);
        open(&c2, &local2).runs(&gate()).unwrap();
        fake2.hand_commit(&[(QUARANTINE_FILE, None)]);
        let err = open(&c2, &local2).runs(&gate()).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Altered { ref what, .. }) if what.contains("deleted")),
            "{err:?}"
        );
    }

    #[test]
    fn attempts_and_decisions_read_from_their_own_directories() {
        let (fake, local, _root) = world();
        let p = ProjectId(seq_iri(8));
        let a = sample_attempt(2, &p, &record());
        let d = sample_decision(3, &record(), vec![]);
        let adir = layout::dir(Area::Attempts, p.iri());
        let ddir = layout::dir(Area::Decisions, record().iri());
        fake.hand_commit(&[
            (
                layout::segment_path(&adir, 1).as_str(),
                Some(file(&[Line::Attempt(a.clone()).encode("x")]).as_str()),
            ),
            (
                layout::segment_path(&ddir, 1).as_str(),
                Some(file(&[Line::Decision(d.clone()).encode("x")]).as_str()),
            ),
        ]);
        let c = client(&fake);
        let l = open(&c, &local);
        assert_eq!(l.attempts_of(&p).unwrap(), vec![a]);
        assert_eq!(l.decisions(record().iri()).unwrap(), vec![d]);
    }

    // Spec §2.1: ownership is a local answer, with no request.
    #[test]
    fn a_record_this_repository_holds_is_owned_and_another_is_not() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        let before = fake.state().requests.len();
        assert!(l.owns(&record()).unwrap());
        let theirs = RecordId(Iri::parse("https://github.com/acme/other/issues/1").unwrap());
        assert!(!l.owns(&theirs).unwrap());
        assert_eq!(fake.state().requests.len(), before, "no request");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-github --lib ledger::read`
Expected: FAIL to compile — `git.rs` and the read methods do not exist.

- [ ] **Step 3: Write the implementation**

Add to `crates/github/src/tracker.rs`, after `pub fn repo(&self)` (line 276):

```rust
    /// The client this tracker writes through, so the GitHub ledger can
    /// share its credential and origin guard (GitHub ledger spec §1.1).
    pub fn client(&self) -> &Client {
        &self.client
    }
```

In `crates/github/src/lib.rs`, add after line 14:

```rust
pub use ledger::GithubLedger;
```

Create `crates/github/src/ledger/git.rs`:

```rust
//! The requests the GitHub ledger makes. Each answer is judged here, once,
//! so no caller reads a failure as data.

use super::GithubLedger;
use crate::client::Method;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fl_core::StoreError;
use serde_json::{Map, Value, json};

/// What a path on the branch is at one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Object {
    Tree(Vec<Entry>),
    Blob { oid: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub name: String,
    pub oid: String,
    pub is_blob: bool,
}

const OBJECT_FIELDS: &str =
    "__typename ... on Tree { entries { name oid type } } ... on Blob { oid }";

/// ⚠ Modelled: `Commit.blame(path:)` names the commit that last changed
/// each range of lines. Confirmed by live test
/// `a_hand_edit_is_detected_and_named`.
const BLAME: &str = "query ledgerBlame($owner: String!, $name: String!, $commit: GitObjectID!, $path: String!) { repository(owner: $owner, name: $name) { object(oid: $commit) { ... on Commit { blame(path: $path) { ranges { startingLine endingLine commit { oid } } } } } } }";

fn backend(msg: String) -> StoreError {
    StoreError::Backend(msg)
}

impl GithubLedger<'_> {
    pub(crate) fn path(&self, rest: &str) -> String {
        format!("/repos/{}{rest}", self.repo.full_name)
    }

    fn owner_and_name(&self) -> (&str, &str) {
        self.repo
            .full_name
            .split_once('/')
            .unwrap_or((self.repo.full_name.as_str(), ""))
    }

    /// The commit `branch` points at; `None` when there is no such branch.
    /// ⚠ An exact name: `fl` is not `fl/ledger`.
    pub(crate) fn branch_head(&self, branch: &str) -> Result<Option<String>, StoreError> {
        let r = self.client.send(
            Method::Get,
            &self.path(&format!("/git/ref/heads/{branch}")),
            None,
        )?;
        match r.status {
            200 => r
                .body
                .pointer("/object/sha")
                .and_then(Value::as_str)
                .map(|s| Some(s.to_string()))
                .ok_or_else(|| {
                    backend(format!(
                        "GitHub answered a read of the branch `{branch}` with no commit"
                    ))
                }),
            404 => Ok(None),
            s => Err(backend(format!(
                "GitHub answered {s} when fl read the branch `{branch}`; retry"
            ))),
        }
    }

    /// How `head` relates to `base` as GitHub's compare names it
    /// (`identical`, `ahead`, `behind`, `diverged`); `None` when GitHub
    /// knows one of the two commits not at all.
    pub(crate) fn compare(&self, base: &str, head: &str) -> Result<Option<String>, StoreError> {
        let r = self
            .client
            .send(Method::Get, &self.path(&format!("/compare/{base}...{head}")), None)?;
        match r.status {
            200 => r
                .body
                .get("status")
                .and_then(Value::as_str)
                .map(|s| Some(s.to_string()))
                .ok_or_else(|| {
                    backend("GitHub compared two ledger commits but named no status".into())
                }),
            404 => Ok(None),
            s => Err(backend(format!(
                "GitHub answered {s} when fl compared two commits of the ledger; retry"
            ))),
        }
    }

    /// What each of `paths` is at commit `head`, in one request.
    pub(crate) fn objects(
        &self,
        head: &str,
        paths: &[String],
    ) -> Result<Vec<Option<Object>>, StoreError> {
        let (owner, name) = self.owner_and_name();
        let mut vars = Map::new();
        vars.insert("owner".into(), json!(owner));
        vars.insert("name".into(), json!(name));
        let mut declared = String::from("$owner: String!, $name: String!");
        let mut fields = String::new();
        for (i, p) in paths.iter().enumerate() {
            declared.push_str(&format!(", $e{i}: String!"));
            fields.push_str(&format!(
                " e{i}: object(expression: $e{i}) {{ {OBJECT_FIELDS} }}"
            ));
            vars.insert(format!("e{i}"), json!(format!("{head}:{p}")));
        }
        let query = format!(
            "query ledgerObjects({declared}) {{ repository(owner: $owner, name: $name) {{{fields} }} }}"
        );
        let data = self.client.graphql(&query, Value::Object(vars))?;
        let repo = data
            .get("repository")
            .filter(|r| !r.is_null())
            .ok_or_else(|| {
                backend(format!(
                    "GitHub did not find the repository {} when fl read its ledger",
                    self.repo.full_name
                ))
            })?;
        paths
            .iter()
            .enumerate()
            .map(|(i, p)| parse_object(&repo[format!("e{i}").as_str()], p))
            .collect()
    }

    /// A blob's text, downloaded.
    ///
    /// ⚠ Lossy on purpose: bytes that are not UTF-8 become U+FFFD, so a
    /// damaged file fails the line and growth checks by name instead of
    /// failing here without one.
    pub(crate) fn blob_text(&self, oid: &str) -> Result<String, StoreError> {
        let r = self
            .client
            .send(Method::Get, &self.path(&format!("/git/blobs/{oid}")), None)?;
        if r.status != 200 {
            return Err(backend(format!(
                "GitHub answered {} when fl read ledger blob {oid}; retry",
                r.status
            )));
        }
        // GitHub wraps the base64 in lines.
        let content: String = r
            .body
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| backend(format!("GitHub answered ledger blob {oid} with no content")))?
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let bytes = STANDARD.decode(content).map_err(|e| {
            backend(format!(
                "GitHub answered ledger blob {oid} with content that is not base64 ({e})"
            ))
        })?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The commit that last changed `line` of `path` at `commit`, for a
    /// message. ⚠ Never an error: a message that cannot name the commit
    /// says so, and still names the file and the line.
    pub(crate) fn blame(&self, commit: &str, path: &str, line: u64) -> String {
        let (owner, name) = self.owner_and_name();
        let found = self
            .client
            .graphql(
                BLAME,
                json!({"owner": owner, "name": name, "commit": commit, "path": path}),
            )
            .ok()
            .and_then(|d| {
                d.pointer("/repository/object/blame/ranges")?
                    .as_array()?
                    .iter()
                    .find(|r| {
                        let from = r["startingLine"].as_u64().unwrap_or(0);
                        let to = r["endingLine"].as_u64().unwrap_or(0);
                        from <= line && line <= to
                    })?
                    .pointer("/commit/oid")?
                    .as_str()
                    .map(str::to_string)
            });
        found.unwrap_or_else(|| format!("unknown (GitHub's blame of `{path}` did not name it)"))
    }
}

fn parse_object(v: &Value, path: &str) -> Result<Option<Object>, StoreError> {
    if v.is_null() {
        return Ok(None);
    }
    match v.get("__typename").and_then(Value::as_str) {
        Some("Blob") => {
            let oid = v.get("oid").and_then(Value::as_str).ok_or_else(|| {
                backend(format!("GitHub answered `{path}` on the ledger branch with no id"))
            })?;
            Ok(Some(Object::Blob { oid: oid.to_string() }))
        }
        Some("Tree") => {
            let entries = v
                .get("entries")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    backend(format!("GitHub answered the directory `{path}` with no entries"))
                })?;
            let mut out = Vec::new();
            for e in entries {
                let (Some(name), Some(oid)) = (e["name"].as_str(), e["oid"].as_str()) else {
                    return Err(backend(format!(
                        "GitHub answered an entry of `{path}` with no name or no id"
                    )));
                };
                out.push(Entry {
                    name: name.to_string(),
                    oid: oid.to_string(),
                    is_blob: e["type"].as_str() == Some("blob"),
                });
            }
            Ok(Some(Object::Tree(out)))
        }
        other => Err(backend(format!(
            "GitHub answered `{path}` on the ledger branch as {other:?}, which is neither a \
             file nor a directory"
        ))),
    }
}
```

Prepend to `crates/github/src/ledger/read.rs` (above its tests):

```rust
//! Reading the `fl/ledger` branch (GitHub ledger spec §3.3, §3.5): the
//! checked head, a snapshot of the directories one read or append needs,
//! and their lines, parsed strictly. Each of the seven checks is named
//! where it is made.

use super::GithubLedger;
use super::git::Object;
use super::layout::{
    self, Area, BRANCH, FORMAT, FORMAT_FILE, Line, QUARANTINE_FILE, QuarantineLine,
};
use fl_core::decision::Decision;
use fl_core::ids::{GateId, ProjectId, RecordId};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use fl_core::split::CachedSegment;
use fl_core::{LedgerFault, StoreError};
use std::collections::BTreeMap;

/// What a read reports without refusing (spec §3.3, §3.6).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Note {
    /// A quarantined line, skipped.
    Quarantined {
        file: String,
        line: u64,
        reason: String,
    },
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::Quarantined { file, line, reason } => write!(
                f,
                "`{file}` line {line} of the GitHub ledger is quarantined ({reason}), so fl \
                 skipped it"
            ),
        }
    }
}

/// One segment of a directory, read and checked. Segments are numbered
/// from 1 with no gap, so a directory's `n`th is at index `n - 1`.
pub(crate) struct Segment {
    pub path: String,
    pub text: String,
}

/// The directories one read or append needs, at one checked head.
pub(crate) struct Snapshot {
    pub head: String,
    pub dirs: BTreeMap<String, Vec<Segment>>,
    /// `quarantine.jsonl`'s text; empty when there is none.
    pub quarantine: String,
}

impl GithubLedger<'_> {
    pub(crate) fn altered(&self, file: &str, what: impl Into<String>, commit: &str) -> StoreError {
        LedgerFault::Altered {
            repo: self.repo.full_name.clone(),
            file: file.to_string(),
            what: what.into(),
            commit: commit.to_string(),
        }
        .into()
    }

    /// The head of `fl/ledger`, checked (spec §3.5 checks 1 and 2).
    ///
    /// ⚠ One compare (ruling 9): against the last head this machine saw
    /// when there is one, else against the anchor. Every head recorded as
    /// seen was itself checked to descend from the anchor, so descending
    /// from it is descending from the anchor. A `behind` answer is read
    /// again `lag_reads` times before it counts: GitHub's replicas can
    /// briefly lag a write.
    pub fn check_head(&self) -> Result<String, StoreError> {
        let repo = self.repo.full_name.clone();
        let node = self.repo.node_id.as_str();
        let anchor = self.local.ledger_root(node)?;
        let mut reads = 0u32;
        loop {
            let (head, anchor) = match (self.branch_head(BRANCH)?, anchor.clone()) {
                (Some(h), Some(a)) => (h, a),
                (None, None) => return Err(LedgerFault::NotSetUp { repo }.into()),
                (None, Some(root)) => return Err(LedgerFault::Deleted { repo, root }.into()),
                (Some(_), None) => return Err(LedgerFault::NoAnchor { repo }.into()),
            };
            let (against, base) = match self.local.last_head(node)? {
                Some(seen) => ("the last head this machine saw", seen),
                None => ("the ledger's first commit", anchor),
            };
            if head == base {
                return Ok(head);
            }
            match self.compare(&base, &head)?.as_deref() {
                Some("ahead" | "identical") => return Ok(head),
                Some("behind") if reads < self.lag_reads => {
                    reads += 1;
                    std::thread::sleep(self.lag_pause);
                }
                other => {
                    let how = match other {
                        Some("behind") => {
                            format!("it is behind it, still after {reads} reads again")
                        }
                        Some(s) => format!("GitHub compares them as `{s}`"),
                        None => "GitHub does not know one of the two commits".to_string(),
                    };
                    return Err(LedgerFault::Rewritten {
                        repo,
                        head,
                        against,
                        base,
                        how,
                    }
                    .into());
                }
            }
        }
    }

    /// `dirs` at the checked head (spec §3.5 checks 1–4 and 7), with
    /// `quarantine.jsonl`. Records the head as the last seen (ruling 10).
    pub(crate) fn snapshot(&self, dirs: &[String]) -> Result<Snapshot, StoreError> {
        let head = self.check_head()?;
        let mut paths = vec![FORMAT_FILE.to_string(), QUARANTINE_FILE.to_string()];
        paths.extend(dirs.iter().cloned());
        let found = self.objects(&head, &paths)?;
        self.format(&head, found[0].as_ref())?;
        let quarantine = match &found[1] {
            None => {
                if self
                    .local
                    .cached(&self.repo.node_id, QUARANTINE_FILE)?
                    .is_some()
                {
                    return Err(self.altered(QUARANTINE_FILE, "was deleted", &head));
                }
                String::new()
            }
            Some(Object::Blob { oid }) => self.grown(&head, QUARANTINE_FILE, oid, false)?,
            Some(Object::Tree(_)) => {
                return Err(self.altered(
                    QUARANTINE_FILE,
                    "is a directory where a file belongs",
                    &head,
                ));
            }
        };
        let mut out = BTreeMap::new();
        for (dir, f) in dirs.iter().zip(&found[2..]) {
            out.insert(dir.clone(), self.directory(&head, dir, f.as_ref())?);
        }
        self.local.set_last_head(&self.repo.node_id, &head)?;
        Ok(Snapshot {
            head,
            dirs: out,
            quarantine,
        })
    }

    /// `path`'s text at blob `oid` — from this machine's cache when it
    /// holds that blob, else downloaded — and what the cache held before.
    fn text_at(&self, path: &str, oid: &str) -> Result<(String, Option<CachedSegment>), StoreError> {
        let before = self.local.cached(&self.repo.node_id, path)?;
        let text = match &before {
            Some(c) if c.oid == oid => c.text.clone(),
            _ => self.blob_text(oid)?,
        };
        Ok((text, before))
    }

    /// ⚠ Check 7: the format is one this fl knows.
    fn format(&self, head: &str, found: Option<&Object>) -> Result<(), StoreError> {
        let Some(Object::Blob { oid }) = found else {
            return Err(self.altered(FORMAT_FILE, "is missing", head));
        };
        let (text, _) = self.text_at(FORMAT_FILE, oid)?;
        if text.strip_suffix('\n').unwrap_or(&text) != FORMAT {
            return Err(LedgerFault::UnknownFormat {
                repo: self.repo.full_name.clone(),
                found: text.trim().to_string(),
            }
            .into());
        }
        self.local.cache(
            &self.repo.node_id,
            FORMAT_FILE,
            &CachedSegment {
                oid: oid.clone(),
                text,
                closed: true,
            },
        )
    }

    /// A file that may only grow, checked against the copy read before, and
    /// cached only once it passed.
    fn grown(&self, head: &str, path: &str, oid: &str, closed: bool) -> Result<String, StoreError> {
        let (text, before) = self.text_at(path, oid)?;
        if let Some(c) = &before
            && c.oid != oid
        {
            // ⚠ Check 3: a closed segment never changes.
            if c.closed {
                return Err(self.altered(path, "changed after it was closed", head));
            }
            // ⚠ Check 4: the open segment starts with the copy read before.
            if !text.starts_with(&c.text) {
                return Err(self.altered(
                    path,
                    "no longer starts with the copy this machine read before",
                    head,
                ));
            }
        }
        self.local.cache(
            &self.repo.node_id,
            path,
            &CachedSegment {
                oid: oid.to_string(),
                text: text.clone(),
                closed,
            },
        )?;
        Ok(text)
    }

    /// One directory's segments: named `1.jsonl` up with no gap, none of
    /// those read before gone, each checked by [`Self::grown`].
    fn directory(
        &self,
        head: &str,
        dir: &str,
        found: Option<&Object>,
    ) -> Result<Vec<Segment>, StoreError> {
        let entries = match found {
            None => Vec::new(),
            Some(Object::Tree(entries)) => entries.clone(),
            Some(Object::Blob { .. }) => {
                return Err(self.altered(dir, "is a file where a directory belongs", head));
            }
        };
        let mut numbered: BTreeMap<u64, (String, String)> = BTreeMap::new();
        for e in &entries {
            let n = if e.is_blob {
                layout::segment_number(&e.name)
            } else {
                None
            };
            let Some(n) = n else {
                return Err(self.altered(
                    &format!("{dir}/{}", e.name),
                    "is not a segment fl writes",
                    head,
                ));
            };
            numbered.insert(n, (layout::segment_path(dir, n), e.oid.clone()));
        }
        let last = numbered.len() as u64;
        if numbered.keys().copied().ne(1..=last) {
            return Err(self.altered(dir, "is missing a segment", head));
        }
        for (path, _) in self.local.cached_under(&self.repo.node_id, dir)? {
            if !numbered.values().any(|(p, _)| *p == path) {
                return Err(self.altered(&path, "was deleted", head));
            }
        }
        let mut out = Vec::new();
        for (n, (path, oid)) in numbered {
            let text = self.grown(head, &path, &oid, n < last)?;
            out.push(Segment { path, text });
        }
        Ok(out)
    }

    /// `quarantine.jsonl`'s lines. ⚠ One fl cannot read is `Altered`: the
    /// quarantine file cannot quarantine itself (ruling 17).
    pub(crate) fn quarantine_lines(&self, snap: &Snapshot) -> Result<Vec<QuarantineLine>, StoreError> {
        let mut out = Vec::new();
        for (n, text) in layout::lines(&snap.quarantine) {
            match text.map_err(str::to_string).and_then(QuarantineLine::decode) {
                Ok(q) => out.push(q),
                Err(cause) => {
                    return Err(self.altered(
                        QUARANTINE_FILE,
                        format!(
                            "has an unreadable line {n} ({cause}), and the quarantine file \
                             cannot quarantine its own lines"
                        ),
                        &snap.head,
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Every line of `dir` in `snap`, parsed strictly (spec §3.3), checked
    /// (§3.5 checks 5 and 6), each id once. A quarantined line is skipped
    /// and noted (§3.6).
    pub(crate) fn lines(
        &self,
        snap: &Snapshot,
        area: Area,
        dir: &str,
    ) -> Result<Vec<Line>, StoreError> {
        let skipped: BTreeMap<(String, u64), String> = self
            .quarantine_lines(snap)?
            .into_iter()
            .map(|q| ((q.file, q.line), q.reason))
            .collect();
        let mut out: Vec<Line> = Vec::new();
        let mut seen: BTreeMap<Iri, (String, u64, usize)> = BTreeMap::new();
        for seg in snap.dirs.get(dir).map(Vec::as_slice).unwrap_or_default() {
            for (n, text) in layout::lines(&seg.text) {
                if let Some(reason) = skipped.get(&(seg.path.clone(), n)) {
                    self.notes.borrow_mut().insert(Note::Quarantined {
                        file: seg.path.clone(),
                        line: n,
                        reason: reason.clone(),
                    });
                    continue;
                }
                let line = match text
                    .map_err(str::to_string)
                    .and_then(|t| layout::decode(area, t))
                {
                    Ok((line, _by)) => line,
                    Err(cause) => {
                        return Err(LedgerFault::Unreadable {
                            repo: self.repo.full_name.clone(),
                            file: seg.path.clone(),
                            line: n,
                            commit: self.blame(&snap.head, &seg.path, n),
                            cause,
                        }
                        .into());
                    }
                };
                // ⚠ Check 6: a line's gate, project or record matches its
                // directory.
                if line.dir() != dir {
                    return Err(LedgerFault::Misplaced {
                        repo: self.repo.full_name.clone(),
                        file: seg.path.clone(),
                        line: n,
                        belongs: line.subject().to_string(),
                        commit: self.blame(&snap.head, &seg.path, n),
                    }
                    .into());
                }
                let id = line.id().cloned().expect("decode refuses a line with no id");
                match seen.get(&id) {
                    // ⚠ Check 5: the same id never appears with different
                    // content. An identical copy is read once.
                    Some((file, at, i)) => {
                        if out[*i] != line {
                            return Err(StoreError::Tampered {
                                id,
                                detail: format!(
                                    "the GitHub ledger holds it twice with different content: \
                                     `{file}` line {at} and `{}` line {n}",
                                    seg.path
                                ),
                            });
                        }
                    }
                    None => {
                        seen.insert(id, (seg.path.clone(), n, out.len()));
                        out.push(line);
                    }
                }
            }
        }
        Ok(out)
    }

    fn read(&self, area: Area, subject: &Iri) -> Result<Vec<Line>, StoreError> {
        let dir = layout::dir(area, subject);
        let snap = self.snapshot(std::slice::from_ref(&dir))?;
        self.lines(&snap, area, &dir)
    }

    /// Every run of `gate` the ledger holds. ⚠ Unreachable is an error,
    /// never an empty list (spec §2.5).
    pub fn runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        Ok(self
            .read(Area::Runs, gate.iri())?
            .into_iter()
            .filter_map(|l| match l {
                Line::Run(r) => Some(r),
                _ => None,
            })
            .collect())
    }

    pub fn attempts_of(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        Ok(self
            .read(Area::Attempts, project.iri())?
            .into_iter()
            .filter_map(|l| match l {
                Line::Attempt(a) => Some(a),
                _ => None,
            })
            .collect())
    }

    /// The decisions filed under `subject`: a finding, or a record.
    pub fn decisions(&self, subject: &Iri) -> Result<Vec<Decision>, StoreError> {
        Ok(self
            .read(Area::Decisions, subject)?
            .into_iter()
            .filter_map(|l| match l {
                Line::Decision(d) => Some(d),
                _ => None,
            })
            .collect())
    }

    /// Whether `record` is an issue of this repository. ⚠ A local answer,
    /// never a request (spec §2.1).
    pub fn owns(&self, record: &RecordId) -> Result<bool, StoreError> {
        crate::owner::issue_of_repository(
            record.iri(),
            &self.repo.full_name,
            &self.repo.node_id,
            self.local,
        )
    }
}

```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fl-github`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

1. `check_head`: swap the `(None, Some(root))` and `(Some(_), None)` arms' faults → `a_ledger_that_is_not_there_says_why` red.
2. `check_head`: accept `diverged` with `ahead` → `a_head_that_does_not_descend_from_the_anchor_is_a_rewrite` red.
3. `check_head`: delete the `Some("behind") if reads < self.lag_reads` arm, so a first `behind` is a rewrite → `a_head_behind_the_last_one_seen_is_read_again_before_it_counts` red.
4. `check_head`: accept `behind` outright → `a_head_that_stays_behind_is_a_rewrite` and `a_branch_reset_to_an_older_commit_is_a_rewrite` red.
5. `check_head`: always compare against the anchor, ignoring the last head → `a_head_that_stays_behind_is_a_rewrite` red (its `base`) and `a_branch_reset…` red.
6. `format`: accept any text → `a_format_other_than_1…` red; accept a missing file → `a_ledger_with_no_format_file_is_altered` red.
7. `grown`: delete the `closed` check → `a_closed_segment_that_changes_is_altered` red (the change only grows it, so check 4 alone passes it).
8. `grown`: delete the `starts_with` check → `an_open_segment_that_no_longer_starts…` red.
9. `directory`: delete the gap check → `a_segment_that_disappears_or_leaves_a_gap…` red (first half); delete the deleted-file check → the same test red (second half); delete the not-a-segment check, or the blob-where-a-directory-belongs check → `a_file_that_is_not_a_segment…` red.
10. `snapshot`: delete the deleted-quarantine check → `an_unreadable_or_vanished_quarantine_file_is_altered` red; delete `set_last_head` → `a_fresh_ledger_reads_empty_and_remembers…` and `a_head_that_stays_behind…` red.
11. `quarantine_lines`: skip an unreadable line instead → `an_unreadable_or_vanished_quarantine_file…` red.
12. `lines`: delete the quarantine skip → `a_quarantined_line_is_skipped_and_noted_once` red; delete the note → the same; delete the misplaced check → `a_line_in_the_wrong_directory_is_misplaced` red; delete the different-content check → `one_id_with_two_contents…` red; push identical copies too → the same test's first assertion red.
13. `text_at`: always download → `a_segment_read_before_is_not_downloaded_again` red.
14. `lines`: name the head instead of `self.blame(…)` as the unreadable line's commit → `an_unreadable_line_names_its_file_line_commit_and_the_quarantine_command` red (a later, unrelated commit is the head there).

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/lib.rs crates/github/src/tracker.rs crates/github/src/ledger/mod.rs crates/github/src/ledger/git.rs crates/github/src/ledger/read.rs
git commit -m "feat(github): GithubLedger reads the branch, with the seven checks

The head must exist, have an anchor, and descend from the last head seen
or the anchor (a behind answer is read again for replica lag); the format
must be 1; a closed segment never changes, the open one only grows, no
segment goes missing; every line is read strictly, sits in its own
directory, and carries one content per id. A quarantined line is skipped
and noted; an unreadable one names its file, line, commit and the
quarantine command. Ownership stays local. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---
### Task 10: `GithubLedger` publishes — the append protocol, decision 2, and the split suites over GitHub

**Files:**
- Create: `crates/github/src/ledger/append.rs`
- Create: `crates/github/src/ledger/fixture.rs` (test-only)
- Modify: `crates/github/src/ledger/mod.rs` (modules, two fields, `visibility`, `identity`, `RemoteLedger`)
- Modify: `crates/core/src/conformance.rs:814-846` (the lost-answer case)
- Modify: `crates/core/src/split.rs` tests (the `MemRemote` lost answer, kept)

**Interfaces:**
- Consumes: `snapshot`, `lines`, `quarantine_lines`, `runs`, `attempts_of`, `owns` (Task 9); `layout`, `disclose` (Task 6); `GraphqlAnswer` (Task 7); `StoreError::Contended` (Task 1); `RemoteControl::batches` (Task 3).
- Produces:

```rust
// fl_github::ledger::append
pub const TRIES: u32;                                       // 5
pub(crate) struct NewLine { pub id: Iri, pub text: String }
impl GithubLedger<'_> {
    /// One commit adding the missing lines of each directory and of quarantine.jsonl.
    pub(crate) fn append(&self, dirs: &[(Area, String, Vec<NewLine>)], quarantine: &[NewLine],
                         headline: &str) -> Result<Option<String>, StoreError>;
}

// fl_github::ledger (mod.rs)
impl GithubLedger<'_> {
    pub fn visibility(&self) -> Result<Visibility, StoreError>;   // once per GithubLedger
    pub(crate) fn identity(&self) -> Result<String, StoreError>;  // `by`, once
}
impl RemoteLedger for GithubLedger<'_> { /* repo_node_id, owns_record, publish, gate_runs, attempts */ }
```

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/conformance.rs`, replace the first `flush` assertion of `a_commit_whose_answer_was_lost_is_not_duplicated_by_the_next_flush` (lines 824-831) with:

```rust
    ctl.lose_next_answer();
    // A remote that cannot tell refuses the flush, and the next one carries
    // the run; one that reads the ledger again finds the commit landed and
    // answers (GitHub ledger spec §3.2 step 5; ruling 13). Either way, the
    // run is never published twice.
    let _ = roles.ledger.flush(sample_decision(1, &r, vec![id.clone()]));
```

In `crates/core/src/split.rs`'s `tests`, add — so `MemRemote`'s own answer to a lost answer stays pinned now that the shared case accepts either:

```rust
    // ⚠ Spec §3.2 step 5: `MemRemote` reports a lost answer as a failure;
    // the next flush carries the run, and adds no second copy.
    #[test]
    fn a_lost_answer_refuses_the_flush_and_the_next_one_adds_nothing_twice() {
        let (s, _p, g, r) = world();
        let remote = MemRemote::new("R_1");
        let l = SplitLedger {
            local: &s,
            github: &remote,
        };
        let run = sample_record_run(1, &g, Some(&r));
        l.append_gate_run(run.clone()).unwrap();
        remote.lose_next_answer();
        assert!(
            l.flush(sample_decision(1, &r, vec![run.id.clone().unwrap()]))
                .is_err()
        );
        l.flush(sample_decision(2, &r, vec![run.id.clone().unwrap()]))
            .unwrap();
        assert_eq!(published_ids(&remote, &g), vec![run.id]);
    }
```

Create `crates/github/src/ledger/fixture.rs`:

```rust
//! `SplitLedger` over `GithubLedger` and the fake GitHub, for the shared
//! ledger suites (GitHub ledger spec §8.2): the split ledger meets the
//! same contract over the GitHub side as over `MemRemote`.

use super::GithubLedger;
use crate::client::Client;
use crate::creds::EnvToken;
use crate::fake::FakeGithub;
use crate::tracker::GithubTracker;
use fl_core::conformance::{Bound, Fixture, RemoteControl, SplitFixture, entry_iri};
use fl_core::ids::{GateId, ProjectId, RecordId};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use fl_core::split::{Batch, Outbox, RemoteLedger, SplitLedger};
use fl_core::store::{Bindings, StoreError};
use fl_core::{CatalogChecked, KindRouted, MemStore};
use std::cell::RefCell;
use std::time::Duration;

fn client(fake: &FakeGithub) -> Client {
    Client::new(
        &fake.url(),
        Box::new(EnvToken::from_lookup(|_| Some("t".into())).expect("a token")),
    )
}

/// The GitHub side, keeping each batch it is handed, so the suite sees
/// what a flush offered.
struct Recorded<'a> {
    inner: &'a dyn RemoteLedger,
    batches: RefCell<Vec<Batch>>,
}

impl RemoteLedger for Recorded<'_> {
    fn repo_node_id(&self) -> &str {
        self.inner.repo_node_id()
    }
    fn owns_record(&self, record: &RecordId) -> Result<bool, StoreError> {
        self.inner.owns_record(record)
    }
    fn publish(&self, batch: &Batch) -> Result<Option<String>, StoreError> {
        self.batches.borrow_mut().push(batch.clone());
        self.inner.publish(batch)
    }
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        self.inner.gate_runs(gate)
    }
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        self.inner.attempts(project)
    }
}

struct Controls<'a> {
    fake: &'a FakeGithub,
    remote: &'a Recorded<'a>,
}

impl RemoteControl for Controls<'_> {
    fn set_down(&self, down: bool) {
        self.fake.state().down = down;
    }
    fn lose_next_answer(&self) {
        self.fake.state().hang_up_after_next_commit = true;
    }
    fn foreign_record(&self) -> RecordId {
        RecordId(
            Iri::parse("https://github.com/acme/other/issues/1").expect("an issue URL is an IRI"),
        )
    }
    fn remote(&self) -> &dyn RemoteLedger {
        self.remote
    }
    fn batches(&self) -> Vec<Batch> {
        self.remote.batches.borrow().clone()
    }
}

/// A split ledger over a `MemStore`, a `GithubTracker` and a `GithubLedger`
/// on a fresh fake whose ledger is set up, with this machine's cut-over
/// before every sample entry.
struct OverFake;

impl SplitFixture for OverFake {
    fn with_split(&self, f: &mut dyn FnMut(&Bound<'_>, &dyn RemoteControl)) {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        local
            .set_ledger_root("R_1", &root)
            .expect("a fresh store records a root");
        local
            .set_cutover("R_1", &entry_iri(0))
            .expect("a fresh store records a cut-over");
        let tracker = GithubTracker::open(client(&fake), "acme/widgets", &local)
            .expect("the fake's repository opens")
            .0
            .with_visibility(Duration::from_secs(10), Duration::ZERO);
        let ledger_client = client(&fake);
        let ledger = GithubLedger::new(&ledger_client, tracker.repo().clone(), &local)
            .with_lag(0, Duration::ZERO);
        let recorded = Recorded {
            inner: &ledger,
            batches: RefCell::new(Vec::new()),
        };
        let split = SplitLedger {
            local: &local,
            github: &recorded,
        };
        let checked = CatalogChecked {
            catalog: &local,
            tracker: &tracker,
        };
        let routed = KindRouted {
            catalog: &local,
            tracker: &tracker,
        };
        f(
            &Bound {
                catalog: &local,
                tracker: &checked,
                ledger: &split,
                handles: &routed,
            },
            &Controls {
                fake: &fake,
                remote: &recorded,
            },
        );
    }
}

impl Fixture for OverFake {
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
        self.with_split(&mut |b, _| f(b));
    }
}

// ⚠ Spec §8.2: the split ledger meets the shared contracts over the GitHub
// side itself, not only over `MemRemote`.
#[test]
fn a_split_ledger_over_github_meets_the_ledger_contracts() {
    fl_core::conformance::ledger(|| OverFake);
    fl_core::conformance::split_ledger(|| OverFake);
}
```

Create `crates/github/src/ledger/append.rs` with only its tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, GraphqlAnswer};
    use crate::creds::EnvToken;
    use crate::fake::{FakeGithub, USER_LOGIN};
    use crate::ledger::layout::{SEGMENT_LIMIT, decode};
    use crate::tracker::Repo;
    use fl_core::MemStore;
    use fl_core::conformance::{sample_attempt, sample_decision, sample_record_run};
    use fl_core::decision::Outcome;
    use fl_core::ids::{GateId, ProjectId, RecordId, seq_iri};
    use fl_core::log::{Attempt, GateRun, PathsTouched, WITHHELD_ERROR_DETAIL};
    use fl_core::model::State;
    use fl_core::split::{LedgerCache, RemoteLedger};
    use fl_core::store::Bindings;
    use fl_core::verdict::Verdict;
    use std::time::Duration;

    fn client_at(url: &str) -> Client {
        Client::new(
            url,
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    fn client(fake: &FakeGithub) -> Client {
        client_at(&fake.url())
    }

    fn repo() -> Repo {
        Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        }
    }

    /// A ledger started on the fake, and a machine that records its anchor.
    fn world() -> (FakeGithub, MemStore, String) {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        (fake, local, root)
    }

    fn open<'a>(c: &'a Client, local: &'a MemStore) -> GithubLedger<'a> {
        GithubLedger::new(c, repo(), local).with_lag(2, Duration::ZERO)
    }

    fn gate() -> GateId {
        GateId(seq_iri(7))
    }

    fn project() -> ProjectId {
        ProjectId(seq_iri(8))
    }

    fn record() -> RecordId {
        RecordId(Iri::parse("https://github.com/acme/widgets/issues/1").unwrap())
    }

    fn run(n: u64) -> GateRun {
        sample_record_run(n, &gate(), Some(&record()))
    }

    fn attempt(n: u64) -> Attempt {
        sample_attempt(n, &project(), &record())
    }

    /// A `check` decision `n`, resting on `runs` and `attempts`.
    fn batch(n: u64, runs: Vec<GateRun>, attempts: Vec<Attempt>) -> Batch {
        let rests_on = runs
            .iter()
            .filter_map(|r| r.id.clone())
            .chain(attempts.iter().filter_map(|a| a.id.clone()))
            .collect();
        Batch {
            decision: sample_decision(n, &record(), rests_on),
            runs,
            attempts,
        }
    }

    fn runs_dir() -> String {
        layout::dir(Area::Runs, gate().iri())
    }

    fn files_under(fake: &FakeGithub, dir: &str) -> Vec<(String, String)> {
        let prefix = format!("{dir}/");
        fake.ledger_files()
            .into_iter()
            .filter(|(p, _)| p.starts_with(&prefix))
            .collect()
    }

    #[test]
    fn a_publish_files_each_entry_in_its_own_directory_and_names_its_writer() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        let b = batch(1, vec![run(1)], vec![attempt(2)]);
        let commit = l.publish(&b).unwrap();
        assert_eq!(commit, fake.ledger_head());
        let files = fake.ledger_files();
        for (area, subject, line) in [
            (Area::Runs, gate().iri().clone(), Line::Run(run(1))),
            (Area::Attempts, project().iri().clone(), Line::Attempt(attempt(2))),
            (Area::Decisions, record().iri().clone(), Line::Decision(b.decision.clone())),
        ] {
            let path = layout::segment_path(&layout::dir(area, &subject), 1);
            let text = files
                .get(&path)
                .unwrap_or_else(|| panic!("{path} in {:?}", files.keys()));
            assert_eq!(
                decode(area, text.trim_end()),
                Ok((line, USER_LOGIN.to_string()))
            );
        }
        assert_eq!(
            local.last_head("R_1").unwrap(),
            commit,
            "the new head is the last seen"
        );
    }

    // Spec §3.2 step 5: nothing left to add, no commit.
    #[test]
    fn publishing_what_is_already_there_makes_no_commit() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let b = batch(1, vec![run(1)], vec![]);
        open(&c, &local).publish(&b).unwrap();
        let commits = fake.ledger_commits();
        assert_eq!(open(&c, &local).publish(&b).unwrap(), None);
        assert_eq!(fake.ledger_commits(), commits);
    }

    // ⚠ Spec §3.2 (Invariant): two machines cannot both land a commit on one
    // head; the one refused reads again and adds only what is missing.
    #[test]
    fn two_flushes_racing_both_land_and_none_is_lost() {
        let (fake, local, _root) = world();
        let theirs = Line::Run(run(5)).encode("another-machine");
        fake.state()
            .foreign_appends
            .push((layout::segment_path(&runs_dir(), 1), theirs));
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![])).unwrap();
        assert_eq!(
            l.runs(&gate()).unwrap(),
            vec![run(5), run(1)],
            "theirs, then mine; each once"
        );
        assert_eq!(fake.ledger_commits(), 3, "the start, theirs, mine");
    }

    // ⚠ The same invariant with two real machines on two threads.
    #[test]
    fn two_machines_appending_at_once_all_land_once() {
        const EACH: u64 = 4;
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let url = fake.url();
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|s| {
            for machine in 1..=2u64 {
                let (url, root, start) = (url.clone(), root.clone(), &start);
                s.spawn(move || {
                    let local = MemStore::default();
                    local.set_ledger_root("R_1", &root).unwrap();
                    let c = client_at(&url);
                    let l = GithubLedger::new(&c, repo(), &local).with_lag(3, Duration::ZERO);
                    start.wait();
                    for i in 0..EACH {
                        let b = batch(100 * machine + i, vec![], vec![]);
                        // `Contended` means someone else landed each time and
                        // nothing was lost: this machine tries again.
                        loop {
                            match l.publish(&b) {
                                Ok(_) => break,
                                Err(StoreError::Contended { .. }) => continue,
                                Err(e) => panic!("machine {machine}: {e}"),
                            }
                        }
                    }
                });
            }
        });
        let c = client(&fake);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let mut got: Vec<Iri> = open(&c, &local)
            .decisions(record().iri())
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        got.sort();
        let mut want: Vec<Iri> = (1..=2u64)
            .flat_map(|m| (0..EACH).map(move |i| sample_decision(100 * m + i, &record(), vec![]).id))
            .collect();
        want.sort();
        assert_eq!(got, want, "every decision once, none lost");
        assert_eq!(fake.ledger_commits(), 1 + 2 * EACH as usize, "no empty commit");
    }

    // ⚠ Spec §3.2 step 5 and §8.3: a timeout, then a retry — no duplicate
    // and no empty commit; the head that holds the lines comes back.
    #[test]
    fn a_lost_answer_is_read_again_and_nothing_is_added_twice_or_committed_empty() {
        let (fake, local, _root) = world();
        fake.state().hang_up_after_next_commit = true;
        let c = client(&fake);
        let l = open(&c, &local);
        let b = batch(1, vec![run(1)], vec![]);
        let got = l.publish(&b).unwrap();
        assert_eq!(got, fake.ledger_head(), "the head that holds the lines");
        assert_eq!(fake.ledger_commits(), 2, "one commit, and no empty one after it");
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
        assert_eq!(l.decisions(record().iri()).unwrap(), vec![b.decision]);
    }

    // ⚠ Ruling 8: a lost answer whose commit rolled a segment over — the
    // retry finds the lines in the closed segment as well as the open one.
    #[test]
    fn a_lost_answer_after_a_rollover_adds_nothing_twice() {
        let (fake, local, _root) = world();
        let (a, b) = (run(1), run(2));
        let a_len = Line::Run(a.clone()).encode(USER_LOGIN).len() + 1;
        // A filler run whose line leaves room for `a`'s and no more.
        let mut filler = run(9);
        filler.output_excerpt = Some(String::new());
        let base = Line::Run(filler.clone()).encode("x").len() + 1;
        filler.output_excerpt = Some("f".repeat(SEGMENT_LIMIT - a_len - base));
        let first = format!("{}\n", Line::Run(filler.clone()).encode("x"));
        assert_eq!(first.len(), SEGMENT_LIMIT - a_len);
        let seg1 = layout::segment_path(&runs_dir(), 1);
        fake.hand_commit(&[(seg1.as_str(), Some(first.as_str()))]);
        fake.state().hang_up_after_next_commit = true;
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![a.clone(), b.clone()], vec![]))
            .unwrap();
        let segs = files_under(&fake, &runs_dir());
        assert_eq!(segs.len(), 2, "`b` rolled into a second segment");
        let lines: usize = segs.iter().map(|(_, t)| layout::lines(t).len()).sum();
        assert_eq!(lines, 3, "the filler, `a` and `b`, each once");
        assert_eq!(fake.ledger_commits(), 3, "the start, the filler, one append");
        assert_eq!(l.runs(&gate()).unwrap(), vec![filler, a, b]);
    }

    #[test]
    fn a_commit_that_did_not_land_is_sent_again() {
        let (fake, local, _root) = world();
        fake.state().fail_commits = 1;
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![])).unwrap();
        assert_eq!(fake.ledger_commits(), 2);
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
    }

    // Ruling 14: five tries that each found the head moved is `Contended` —
    // transient, and nothing is lost.
    #[test]
    fn a_head_that_keeps_moving_is_contended_and_nothing_is_lost() {
        let (fake, local, _root) = world();
        for n in 0..u64::from(TRIES) {
            fake.state().foreign_appends.push((
                layout::segment_path(&runs_dir(), 1),
                Line::Run(run(10 + n)).encode("another-machine"),
            ));
        }
        let c = client(&fake);
        let l = open(&c, &local);
        let b = batch(1, vec![run(1)], vec![]);
        let err = l.publish(&b).unwrap_err();
        assert!(matches!(err, StoreError::Contended { tries: TRIES, .. }), "{err:?}");
        assert!(err.is_transient());
        assert_eq!(l.runs(&gate()).unwrap().len(), TRIES as usize, "all of theirs");
        l.publish(&b).unwrap();
        assert_eq!(l.runs(&gate()).unwrap().len(), TRIES as usize + 1);
    }

    // Ruling 14: five answers that never said whether the append landed is
    // `Unreachable` — transient, and nothing landed twice.
    #[test]
    fn answers_that_never_say_whether_the_append_landed_are_unreachable() {
        let (fake, local, _root) = world();
        fake.state().fail_commits = TRIES;
        let c = client(&fake);
        let l = open(&c, &local);
        let err = l.publish(&batch(1, vec![run(1)], vec![])).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert!(err.is_transient());
        assert_eq!(fake.ledger_commits(), 1, "nothing landed");
    }

    // Spec §6.3: a credential without Contents: write is found by the first
    // flush, whose error names it — not a retry.
    #[test]
    fn a_commit_refused_for_want_of_a_permission_refuses_and_names_it() {
        let (fake, local, _root) = world();
        fake.state().refuse_next_commit_for = Some("contents=write".into());
        let c = client(&fake);
        let err = open(&c, &local)
            .publish(&batch(1, vec![], vec![]))
            .unwrap_err();
        assert!(err.to_string().contains("contents=write"), "{err}");
        assert!(!err.is_transient());
        assert_eq!(fake.ledger_commits(), 1);
    }

    // Spec §3.1: segments roll over, and a directory of many reads back
    // whole, even on a machine that never read it.
    #[test]
    fn segments_roll_over_and_a_directory_of_many_reads_back_whole() {
        let (fake, local, root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        let mut published = Vec::new();
        for n in 1..=12u64 {
            let mut r = run(n);
            r.output_excerpt = Some("e".repeat(60 * 1024));
            l.publish(&batch(n, vec![r.clone()], vec![])).unwrap();
            published.push(r);
        }
        let segs = files_under(&fake, &runs_dir());
        assert!(segs.len() >= 3, "{} segments", segs.len());
        assert!(segs.iter().all(|(_, t)| t.len() <= SEGMENT_LIMIT));
        let fresh = MemStore::default();
        fresh.set_ledger_root("R_1", &root).unwrap();
        assert_eq!(open(&c, &fresh).runs(&gate()).unwrap(), published);
    }

    // ⚠ Decision 2 and spec §8.3: on a repository that is not private, no
    // published line holds anything machine-specific.
    #[test]
    fn a_repository_that_is_not_private_publishes_nothing_machine_specific() {
        let secret_path = "/home/someone/work/app/src/a.rs";
        let secret_host = "build-host-7";
        let home = std::env::var("HOME").ok().filter(|h| h.len() > 1);
        for visibility in ["public", "internal"] {
            let (fake, local, _root) = world();
            fake.state().repos[0].visibility = visibility.into();
            let mut r = run(1);
            r.output_excerpt = Some(format!("{secret_path} on {secret_host}"));
            let mut errored = run(2);
            errored.verdict = Verdict::error(format!("could not spawn {secret_path} on {secret_host}"));
            let mut a = attempt(3);
            a.output_excerpt = Some(format!("{secret_path} on {secret_host}"));
            a.paths_touched = PathsTouched::Listed(vec![secret_path.into()]);
            let c = client(&fake);
            let l = open(&c, &local);
            l.publish(&batch(1, vec![r, errored], vec![a])).unwrap();
            let secrets: Vec<String> = [Some(secret_path.to_string()), Some(secret_host.to_string()), home.clone()]
                .into_iter()
                .flatten()
                .collect();
            for (path, text) in fake.ledger_files() {
                for secret in &secrets {
                    assert!(!text.contains(secret.as_str()), "{visibility}: `{path}` holds `{secret}`");
                }
            }
            let runs = l.runs(&gate()).unwrap();
            assert_eq!(runs[0].output_excerpt, None);
            assert_eq!(runs[1].verdict, Verdict::error(WITHHELD_ERROR_DETAIL));
            let attempts = l.attempts_of(&project()).unwrap();
            assert_eq!(attempts[0].output_excerpt, None);
            assert_eq!(attempts[0].paths_touched, PathsTouched::Counted(1));
        }
    }

    #[test]
    fn a_private_repository_publishes_the_excerpts() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![attempt(2)])).unwrap();
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
        assert_eq!(l.attempts_of(&project()).unwrap(), vec![attempt(2)]);
    }

    #[test]
    fn a_batch_published_while_private_is_not_added_again_once_public() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let b = batch(1, vec![run(1)], vec![attempt(2)]);
        open(&c, &local).publish(&b).unwrap();
        let commits = fake.ledger_commits();
        fake.state().repos[0].visibility = "public".into();
        assert_eq!(open(&c, &local).publish(&b).unwrap(), None);
        assert_eq!(fake.ledger_commits(), commits);
    }

    // Decision 2: a visibility that cannot be read is an error, never
    // "private"; nothing is published.
    #[test]
    fn a_visibility_that_cannot_be_read_refuses_and_publishes_nothing() {
        let (fake, local, _root) = world();
        fake.state().fail_repo_read = true;
        let c = client(&fake);
        let err = open(&c, &local)
            .publish(&batch(1, vec![run(1)], vec![]))
            .unwrap_err();
        assert!(err.to_string().contains("visibility"), "{err}");
        assert_eq!(fake.ledger_commits(), 1);
    }

    // Spec §5, ruling 21: read once per ledger, which lives for one command.
    #[test]
    fn the_visibility_is_read_once_per_ledger() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![])).unwrap();
        l.publish(&batch(2, vec![run(2)], vec![])).unwrap();
        let reads = fake
            .state()
            .requests
            .iter()
            .filter(|r| r.as_str() == "GET /repos/acme/widgets")
            .count();
        assert_eq!(reads, 1);
    }

    // Spec §1.3: an entry with no id is never published.
    #[test]
    fn an_entry_without_an_id_is_refused_before_anything_is_appended() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let mut headless = run(1);
        headless.id = None;
        let err = open(&c, &local)
            .publish(&batch(1, vec![headless], vec![]))
            .unwrap_err();
        assert!(err.to_string().contains("no id"), "{err}");
        let mut headless = attempt(2);
        headless.id = None;
        let err = open(&c, &local)
            .publish(&batch(2, vec![], vec![headless]))
            .unwrap_err();
        assert!(err.to_string().contains("no id"), "{err}");
        assert_eq!(fake.ledger_commits(), 1);
    }

    // Decision 11: a refused decision is published like any other.
    #[test]
    fn a_refused_decision_is_published_and_reads_back() {
        let (fake, local, _root) = world();
        let mut b = batch(1, vec![run(1)], vec![]);
        b.decision.outcome = Outcome::Move {
            from: State::Review,
            to: State::Done,
            transitions: vec![],
            allowed: false,
        };
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&b).unwrap();
        assert_eq!(l.decisions(record().iri()).unwrap(), vec![b.decision]);
    }

    fn answer(status: u16, data: Option<serde_json::Value>, errors: Vec<serde_json::Value>) -> GraphqlAnswer {
        GraphqlAnswer {
            status,
            data,
            errors,
        }
    }

    // ⚠ Modelled: what each answer to `createCommitOnBranch` means.
    // Confirmed by live test `create_commit_on_branch_is_refused_when_the_head_moved`.
    #[test]
    fn each_answer_to_a_commit_is_judged_once() {
        let landed = judge(answer(
            200,
            Some(json!({"createCommitOnBranch": {"commit": {"oid": "c1"}}})),
            vec![],
        ));
        assert_eq!(landed.unwrap(), Landed::Commit("c1".into()));
        for e in [
            json!({"type": "STALE_DATA"}),
            json!({"message": "Expected branch to point to \"c0\" but it did not. Pull and try again."}),
        ] {
            assert_eq!(judge(answer(200, None, vec![e])).unwrap(), Landed::HeadMoved);
        }
        assert!(matches!(
            judge(answer(502, None, vec![])).unwrap(),
            Landed::Unknown(_)
        ));
        assert!(matches!(
            judge(answer(200, Some(json!({})), vec![])).unwrap(),
            Landed::Unknown(_)
        ));
        let refused = judge(answer(200, None, vec![json!({"type": "NOT_FOUND"})])).unwrap_err();
        assert!(matches!(refused, StoreError::Backend(_)), "{refused:?}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-github --lib ledger`
Expected: FAIL to compile — `append`, `judge`, `Landed`, `visibility` and `RemoteLedger for GithubLedger` do not exist.

- [ ] **Step 3: Write the implementation**

In `crates/github/src/ledger/mod.rs`, replace the module list and imports (from `pub mod disclose;` through `use std::time::Duration;`) with:

```rust
mod append;
pub mod disclose;
#[cfg(test)]
mod fixture;
mod git;
pub mod layout;
mod read;

use crate::client::{Client, Method};
use crate::tracker::Repo;
use fl_core::ids::{GateId, ProjectId, RecordId};
use fl_core::log::{Attempt, GateRun};
use fl_core::split::{Batch, LedgerMemory, RemoteLedger};
use fl_core::StoreError;
use serde_json::Value;
use std::cell::{OnceCell, RefCell};
use std::collections::BTreeSet;
use std::time::Duration;

pub use append::TRIES;
```

Add two fields to `GithubLedger` (after `notes`):

```rust
    /// Read once per `GithubLedger` (spec §5; ruling 21).
    pub(crate) visibility: OnceCell<Visibility>,
    /// `by` on every line it writes (spec §3.1; ruling 18).
    pub(crate) identity: OnceCell<String>,
```

initialise them in `new` (after `notes: …,`):

```rust
            visibility: OnceCell::new(),
            identity: OnceCell::new(),
```

and add to `impl<'a> GithubLedger<'a>`:

```rust
    /// The repository's visibility, read live, once per `GithubLedger` —
    /// one command, one decision (spec §5).
    ///
    /// ⚠ A failed read is an error: an unknown visibility is not private.
    /// An answer that names none is taken as not private, which withholds.
    pub fn visibility(&self) -> Result<Visibility, StoreError> {
        if let Some(v) = self.visibility.get() {
            return Ok(*v);
        }
        let r = self.client.send(Method::Get, &self.path(""), None)?;
        if r.status != 200 {
            return Err(StoreError::Backend(format!(
                "fl could not read the visibility of {} (GitHub answered {}), so it publishes \
                 nothing to its ledger: an unknown visibility is not private. Retry",
                self.repo.full_name, r.status
            )));
        }
        let v = Visibility::from_github(
            r.body
                .get("visibility")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        let _ = self.visibility.set(v);
        Ok(v)
    }

    /// Who the lines this ledger writes say wrote them, read once.
    pub(crate) fn identity(&self) -> Result<String, StoreError> {
        if let Some(by) = self.identity.get() {
            return Ok(by.clone());
        }
        let by = self.client.identity()?;
        let _ = self.identity.set(by.clone());
        Ok(by)
    }
```

and at the end of `mod.rs`:

```rust
impl RemoteLedger for GithubLedger<'_> {
    fn repo_node_id(&self) -> &str {
        &self.repo.node_id
    }

    fn owns_record(&self, record: &RecordId) -> Result<bool, StoreError> {
        self.owns(record)
    }

    fn publish(&self, batch: &Batch) -> Result<Option<String>, StoreError> {
        self.publish_batch(batch)
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        self.runs(gate)
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        self.attempts_of(project)
    }
}
```

Prepend to `crates/github/src/ledger/append.rs` (above its tests):

```rust
//! Appending to the `fl/ledger` branch (GitHub ledger spec §3.2): read and
//! check, add only what is missing, commit on the head that was read, and
//! read again when the head moved or an answer was lost.

use super::GithubLedger;
use super::disclose;
use super::layout::{self, Area, BRANCH, Line, QUARANTINE_FILE};
use crate::client::GraphqlAnswer;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fl_core::StoreError;
use fl_core::iri::Iri;
use fl_core::split::Batch;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// How many times an append reads the ledger and tries to land (spec §3.2
/// step 5: "up to five tries").
pub const TRIES: u32 = 5;

/// ⚠ Modelled from GitHub's documentation: `createCommitOnBranch` lands a
/// signed commit only on `expectedHeadOid`. Confirmed by live test
/// `create_commit_on_branch_is_refused_when_the_head_moved`.
const APPEND: &str = "mutation ledgerAppend($input: CreateCommitOnBranchInput!) { createCommitOnBranch(input: $input) { commit { oid } } }";

const HEAD_MOVED: &str = "someone else appended first";

/// One line to append, and the id that makes a retry safe.
pub(crate) struct NewLine {
    pub id: Iri,
    pub text: String,
}

/// What became of one commit.
#[derive(Debug, PartialEq, Eq)]
enum Landed {
    Commit(String),
    HeadMoved,
    /// The answer does not say: only a fresh read can tell.
    Unknown(String),
}

/// What one answer to the commit means.
///
/// ⚠ Modelled: a stale `expectedHeadOid` is refused with an error of type
/// `STALE_DATA`, or one whose message says where the branch was expected
/// to point; a 5xx or an answer with no commit may hide a commit that
/// landed. Confirmed by live test
/// `create_commit_on_branch_is_refused_when_the_head_moved`.
fn judge(answer: GraphqlAnswer) -> Result<Landed, StoreError> {
    if answer.status != 200 {
        return Ok(Landed::Unknown(format!("GitHub answered {}", answer.status)));
    }
    if !answer.errors.is_empty() {
        let moved = answer.errors.iter().any(|e| {
            e.get("type").and_then(Value::as_str) == Some("STALE_DATA")
                || e.get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.contains("Expected branch to point to"))
        });
        if moved {
            return Ok(Landed::HeadMoved);
        }
        return Err(StoreError::Backend(format!(
            "GitHub refused the ledger commit: {}",
            Value::Array(answer.errors)
        )));
    }
    match answer
        .data
        .as_ref()
        .and_then(|d| d.pointer("/createCommitOnBranch/commit/oid"))
        .and_then(Value::as_str)
    {
        Some(oid) => Ok(Landed::Commit(oid.to_string())),
        None => Ok(Landed::Unknown(
            "GitHub answered the commit with no commit id".into(),
        )),
    }
}

impl GithubLedger<'_> {
    /// Append the lines of each `(area, directory, lines)` and of
    /// `quarantine` in one commit, and return only once it landed (spec
    /// §3.2).
    ///
    /// Each try reads a fresh snapshot (checks 1–7), drops every line whose
    /// id the directory already holds — in ANY of its segments, not only the
    /// last (ruling 8) — or that `quarantine.jsonl` already holds, and
    /// commits the rest on the head it read. Nothing left: no commit, and
    /// the answer is `None` — or, after a try whose answer was lost, the
    /// head that holds the lines (ruling 13).
    pub(crate) fn append(
        &self,
        dirs: &[(Area, String, Vec<NewLine>)],
        quarantine: &[NewLine],
        headline: &str,
    ) -> Result<Option<String>, StoreError> {
        let wanted: Vec<String> = dirs.iter().map(|(_, d, _)| d.clone()).collect();
        let mut sent = false;
        let mut why: Vec<String> = Vec::new();
        for _ in 0..TRIES {
            let snap = self.snapshot(&wanted)?;
            let mut writes: Vec<(String, String)> = Vec::new();
            for (area, dir, lines) in dirs {
                let present: BTreeSet<Iri> = self
                    .lines(&snap, *area, dir)?
                    .iter()
                    .filter_map(|l| l.id().cloned())
                    .collect();
                let new: Vec<String> = lines
                    .iter()
                    .filter(|l| !present.contains(&l.id))
                    .map(|l| l.text.clone())
                    .collect();
                let segments: Vec<(u64, String)> = snap
                    .dirs
                    .get(dir)
                    .map(|segs| {
                        segs.iter()
                            .enumerate()
                            .map(|(i, s)| ((i + 1) as u64, s.text.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                for (n, text) in layout::plan_append(&segments, &new) {
                    writes.push((layout::segment_path(dir, n), text));
                }
            }
            if !quarantine.is_empty() {
                let present: BTreeSet<Iri> = self
                    .quarantine_lines(&snap)?
                    .into_iter()
                    .map(|q| q.id)
                    .collect();
                let mut text = snap.quarantine.clone();
                let before = text.len();
                for l in quarantine.iter().filter(|l| !present.contains(&l.id)) {
                    text.push_str(&l.text);
                    text.push('\n');
                }
                if text.len() != before {
                    writes.push((QUARANTINE_FILE.to_string(), text));
                }
            }
            if writes.is_empty() {
                return Ok(sent.then_some(snap.head));
            }
            sent = true;
            match self.commit(&snap.head, &writes, headline)? {
                Landed::Commit(oid) => {
                    self.local.set_last_head(&self.repo.node_id, &oid)?;
                    return Ok(Some(oid));
                }
                Landed::HeadMoved => why.push(HEAD_MOVED.to_string()),
                Landed::Unknown(cause) => why.push(cause),
            }
        }
        let store = format!("the GitHub ledger of {}", self.repo.full_name);
        if why.iter().all(|w| w == HEAD_MOVED) {
            Err(StoreError::Contended {
                store,
                tries: TRIES,
            })
        } else {
            Err(StoreError::Unreachable {
                store,
                cause: format!(
                    "{TRIES} tries ended without an answer that said whether the append landed \
                     ({}); the next flush reads the ledger and adds only what is missing",
                    why.join("; ")
                ),
            })
        }
    }

    fn commit(
        &self,
        head: &str,
        writes: &[(String, String)],
        headline: &str,
    ) -> Result<Landed, StoreError> {
        let additions: Vec<Value> = writes
            .iter()
            .map(|(path, text)| json!({"path": path, "contents": STANDARD.encode(text.as_bytes())}))
            .collect();
        let input = json!({
            "branch": {"repositoryNameWithOwner": self.repo.full_name, "branchName": BRANCH},
            "message": {"headline": headline},
            "expectedHeadOid": head,
            "fileChanges": {"additions": additions},
        });
        match self.client.graphql_answer(APPEND, json!({"input": input})) {
            Ok(answer) => judge(answer),
            // ⚠ The request may have reached GitHub before the connection
            // broke (spec §3.2 step 5): only a fresh read can tell.
            Err(StoreError::Unreachable { cause, .. }) => Ok(Landed::Unknown(cause)),
            Err(e) => Err(e),
        }
    }

    /// `RemoteLedger::publish`: the batch's entries, projected for the
    /// repository's visibility (decision 2), each in its own directory, in
    /// one commit.
    pub(crate) fn publish_batch(&self, batch: &Batch) -> Result<Option<String>, StoreError> {
        // ⚠ An entry with no id cannot be de-duplicated, so it is never
        // published (spec §1.3). Refused before any request.
        if batch.runs.iter().any(|r| r.id.is_none())
            || batch.attempts.iter().any(|a| a.id.is_none())
        {
            return Err(StoreError::Backend(
                "fl was about to publish an entry with no id, which it never does; nothing was \
                 published. This is a defect in fl"
                    .into(),
            ));
        }
        let visibility = self.visibility()?;
        let by = self.identity()?;
        let lines = batch
            .runs
            .iter()
            .map(|r| Line::Run(disclose::run(r, visibility)))
            .chain(
                batch
                    .attempts
                    .iter()
                    .map(|a| Line::Attempt(disclose::attempt(a, visibility))),
            )
            .chain(std::iter::once(Line::Decision(batch.decision.clone())));
        let mut grouped: BTreeMap<String, (Area, Vec<NewLine>)> = BTreeMap::new();
        for line in lines {
            let id = line.id().cloned().expect("checked above: every entry has an id");
            let text = line.encode(&by);
            grouped
                .entry(line.dir())
                .or_insert_with(|| (line.area(), Vec::new()))
                .1
                .push(NewLine { id, text });
        }
        let dirs: Vec<(Area, String, Vec<NewLine>)> = grouped
            .into_iter()
            .map(|(dir, (area, lines))| (area, dir, lines))
            .collect();
        let d = &batch.decision;
        self.append(&dirs, &[], &format!("fl: {} {}", d.kind().as_wire(), d.id))
    }
}

```

Note for the test module: `use super::*` brings `json!`, `Line`, `layout`, `Area`, `Batch`, `Iri`, `StoreError`, `judge`, `Landed` and `TRIES`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fl-github && cargo test -p fl-core -p fl-store`
Expected: PASS — including `a_split_ledger_over_github_meets_the_ledger_contracts`, which runs the four ledger cases and ten split cases over the fake.

- [ ] **Step 5: Mutation checks**

1. `append`: de-duplicate against the last segment only (`snap.dirs[dir].last()`'s ids) → `a_lost_answer_after_a_rollover_adds_nothing_twice` red.
2. `append`: drop de-duplication → `publishing_what_is_already_there_makes_no_commit`, `a_lost_answer_is_read_again…`, `two_flushes_racing…` red.
3. `append`: commit even when `writes` is empty → `publishing_what_is_already_there_makes_no_commit` red.
4. `append`: answer `None` after a lost answer → `a_lost_answer_is_read_again…` red.
5. `append`: one try only (`TRIES` used as 1 in the loop) → `two_flushes_racing…`, `a_commit_that_did_not_land_is_sent_again` red.
6. `append`: swap `Contended` and `Unreachable` → `a_head_that_keeps_moving_is_contended…` and `answers_that_never_say…` red.
7. `append`: drop `set_last_head` after a commit → `a_publish_files_each_entry…` red.
8. `commit`: propagate `Unreachable` instead of `Landed::Unknown` → `a_lost_answer_is_read_again…` red, and the conformance case `a_commit_whose_answer_was_lost…` red over the fake.
9. `judge`: treat any error as `HeadMoved` → `each_answer_to_a_commit_is_judged_once` red (NOT_FOUND); drop the message test → the same test red; treat a 5xx as an error → the same test red and `a_commit_that_did_not_land_is_sent_again` red.
10. `publish_batch`: skip the projection → `a_repository_that_is_not_private_publishes_nothing_machine_specific` red.
11. `publish_batch`: drop the id check for runs, or for attempts → `an_entry_without_an_id_is_refused…` red (it panics on `expect`).
12. `visibility`: do not cache → `the_visibility_is_read_once_per_ledger` red; treat a failed read as private → `a_visibility_that_cannot_be_read…` red.
13. `SplitLedger` over GitHub: in `fixture.rs`, make `Recorded::publish` skip the push → `a_published_entry_is_never_offered_to_github_again` red (proves the suite sees the batches).

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/ledger/mod.rs crates/github/src/ledger/append.rs crates/github/src/ledger/fixture.rs crates/core/src/conformance.rs crates/core/src/split.rs
git commit -m "feat(github): GithubLedger publishes, and the split suites run over it

publish projects each entry for the repository's visibility (decision 2),
groups the lines by directory, and appends them in one createCommitOnBranch
on the head it read: it de-duplicates against every segment of a
directory, makes no empty commit, reads again when the head moved or an
answer was lost, and gives up after five tries as Contended or
Unreachable. The shared ledger and split-ledger suites now run over the
fake GitHub too. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---
### Task 11: `init`, the mode, and what `init` tells the person

**Files:**
- Create: `crates/github/src/ledger/init.rs`
- Modify: `crates/github/src/ledger/git.rs` (`parents_of`)
- Modify: `crates/github/src/ledger/mod.rs` (module, exports)

**Interfaces:**
- Consumes: `branch_head`, `check_head`, `path` (Task 9); `LedgerMemory` (`ledger_root`, `set_ledger_root`, `cutover`, `set_cutover`); `ledger_root_shape` (Task 4); the fake's refs, trees, commits and rules (Task 7).
- Produces:

```rust
// fl_github::ledger (re-exported from init)
pub enum InitOutcome {                         // Debug, Clone, Eq
    Created { root: String },
    AlreadySetUp { root: String, cutover_recorded: bool },
    Confirm { root: String },                  // run again with `confirmed = Some(root)`
    Adopted { root: String },
}
pub enum Mode { Protected, DetectionOnly { why: String } }  // Debug, Clone, Eq
impl Mode { pub fn name(&self) -> &'static str; }           // "protected" | "detection-only"
impl GithubLedger<'_> {
    pub fn init(&self, cutover: &Iri, confirmed: Option<&str>) -> Result<InitOutcome, StoreError>;
    pub fn mode(&self) -> Result<Mode, StoreError>;
}
pub fn guidance(repo: &str, mode: &Mode) -> Vec<String>;   // spec §6.1 steps 6-8, §6.3
pub fn ruleset_command(repo: &str) -> String;              // a ready `gh api` command

// crate-internal (git.rs)
pub(crate) fn parents_of(&self, sha: &str) -> Result<Vec<String>, StoreError>;
```

B2's `fl github ledger init` mints `cutover` with `fl_exec::stamp::entry_id()`, prints `guidance`, and on `Confirm` asks the person before calling `init` again with the commit.

- [ ] **Step 1: Write the failing tests**

Create `crates/github/src/ledger/init.rs` with only its tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use crate::fake_git::Ruleset;
    use crate::tracker::Repo;
    use fl_core::conformance::entry_iri;
    use fl_core::ids::{GateId, seq_iri};
    use fl_core::split::{CachedSegment, LedgerCache, Outbox, Pending};
    use fl_core::store::Bindings;
    use fl_core::MemStore;
    use std::time::Duration;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    fn repo() -> Repo {
        Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        }
    }

    fn open<'a>(c: &'a Client, local: &'a dyn fl_core::split::LedgerMemory) -> GithubLedger<'a> {
        GithubLedger::new(c, repo(), local).with_lag(0, Duration::ZERO)
    }

    // Spec §6.1 steps 3 and 4.
    #[test]
    fn init_creates_the_branch_and_records_its_root_and_this_machines_cut_over() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        let l = open(&c, &local);
        let InitOutcome::Created { root } = l.init(&entry_iri(0), None).unwrap() else {
            panic!("a fresh repository gets a new ledger");
        };
        let files = fake.ledger_files();
        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![README_FILE, FORMAT_FILE],
            "README.md and format (in path order), and no `.github/`"
        );
        assert_eq!(files[FORMAT_FILE], "1\n");
        assert!(fake.state().git.commits[&root].parents.is_empty(), "an orphan");
        assert_eq!(fake.ledger_head(), Some(root.clone()));
        assert_eq!(local.ledger_root("R_1").unwrap(), Some(root));
        assert_eq!(local.cutover("R_1").unwrap(), Some(entry_iri(0)));
        assert!(l.runs(&GateId(seq_iri(1))).unwrap().is_empty(), "and it reads");
    }

    #[test]
    fn init_run_again_says_the_ledger_is_set_up_and_moves_nothing() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        let InitOutcome::Created { root } = open(&c, &local).init(&entry_iri(0), None).unwrap() else {
            panic!("created");
        };
        assert_eq!(
            open(&c, &local).init(&entry_iri(7), None).unwrap(),
            InitOutcome::AlreadySetUp {
                root,
                cutover_recorded: false,
            }
        );
        assert_eq!(local.cutover("R_1").unwrap(), Some(entry_iri(0)), "never moved");
        assert_eq!(fake.ledger_commits(), 1);
    }

    // ⚠ Ruling 16 (spec defect 2): a machine that imported the root has no
    // cut-over; init records one, or its flushes would publish nothing.
    #[test]
    fn a_machine_that_imported_the_root_records_its_own_cut_over() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).init(&entry_iri(5), None).unwrap(),
            InitOutcome::AlreadySetUp {
                root,
                cutover_recorded: true,
            }
        );
        assert_eq!(local.cutover("R_1").unwrap(), Some(entry_iri(5)));
    }

    // Spec §6.1 step 2.
    #[test]
    fn init_refuses_when_a_branch_named_fl_exists() {
        let fake = FakeGithub::start("acme/widgets");
        let commit = fake.seed_ledger();
        fake.delete_ledger();
        fake.state().git.refs.insert("heads/fl".into(), commit);
        let local = MemStore::default();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(err.to_string().contains("named `fl`"), "{err}");
        assert_eq!(fake.ledger_head(), None, "nothing created");
        assert_eq!(local.ledger_root("R_1").unwrap(), None);
    }

    // Spec §6.1 step 5: init stopped after creating the branch.
    #[test]
    fn init_that_stopped_after_the_branch_asks_to_confirm_its_first_commit() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.hand_commit(&[("runs/k/1.jsonl", Some("x\n"))]);
        let local = MemStore::default();
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).init(&entry_iri(0), None).unwrap(),
            InitOutcome::Confirm { root: root.clone() }
        );
        assert_eq!(local.ledger_root("R_1").unwrap(), None, "nothing until confirmed");
        let err = open(&c, &local)
            .init(&entry_iri(0), Some("0123456789abcdef0123456789abcdef01234567"))
            .unwrap_err();
        assert!(err.to_string().contains(&root), "names the real first commit: {err}");
        assert_eq!(local.ledger_root("R_1").unwrap(), None);
        assert_eq!(
            open(&c, &local).init(&entry_iri(0), Some(&root)).unwrap(),
            InitOutcome::Adopted { root: root.clone() }
        );
        assert_eq!(local.ledger_root("R_1").unwrap(), Some(root));
        assert_eq!(local.cutover("R_1").unwrap(), Some(entry_iri(0)));
    }

    // Ruling 16: a cut-over recorded by a run that stopped before the root
    // is kept, never replaced.
    #[test]
    fn init_that_stopped_between_the_cut_over_and_the_root_keeps_the_cut_over() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        local.set_cutover("R_1", &entry_iri(3)).unwrap();
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).init(&entry_iri(9), Some(&root)).unwrap(),
            InitOutcome::Adopted { root }
        );
        assert_eq!(local.cutover("R_1").unwrap(), Some(entry_iri(3)));
    }

    #[test]
    fn init_whose_branch_could_not_be_created_records_nothing_and_a_rerun_creates_it() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().fail_next_ref_create = true;
        let local = MemStore::default();
        let c = client(&fake);
        assert!(open(&c, &local).init(&entry_iri(0), None).is_err());
        assert_eq!(local.ledger_root("R_1").unwrap(), None);
        assert_eq!(local.cutover("R_1").unwrap(), None);
        assert!(matches!(
            open(&c, &local).init(&entry_iri(0), None).unwrap(),
            InitOutcome::Created { .. }
        ));
    }

    #[test]
    fn someone_creating_the_branch_while_init_runs_leaves_this_machine_unrecorded() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().race_next_ref_create = true;
        let local = MemStore::default();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(err.to_string().contains("while init ran"), "{err}");
        assert_eq!(local.ledger_root("R_1").unwrap(), None);
        let theirs = fake.ledger_head().unwrap();
        assert_eq!(
            open(&c, &local).init(&entry_iri(0), None).unwrap(),
            InitOutcome::Confirm { root: theirs }
        );
    }

    // Spec §6.1 step 5 and §7: a deleted ledger is refused, never replaced.
    #[test]
    fn init_refuses_a_ledger_that_was_deleted() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.delete_ledger();
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Deleted { .. })),
            "{err:?}"
        );
        assert_eq!(fake.ledger_head(), None, "no new ledger hides the deletion");
    }

    #[test]
    fn init_refuses_a_rewritten_ledger() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.rewrite_ledger(&[("format", "1\n")]);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Rewritten { .. })),
            "{err:?}"
        );
    }

    // Ruling 20: init checks the root's shape before recording it.
    #[test]
    fn init_refuses_a_root_it_cannot_record() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        let bad = GithubLedger::new(
            &c,
            Repo {
                full_name: "acme/widgets".into(),
                node_id: "1 not a node".into(),
            },
            &local,
        );
        let err = bad.init(&entry_iri(0), None).unwrap_err();
        assert!(err.to_string().contains("cannot be one"), "{err}");
        assert_eq!(local.cutover("1 not a node").unwrap(), None);
    }

    /// A local store whose cut-over cannot be written.
    struct NoCutover<'a>(&'a MemStore);
    impl Bindings for NoCutover<'_> {
        fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError> {
            self.0.bound_node_id(repo)
        }
        fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError> {
            self.0.bind_node_id(repo, node_id)
        }
        fn ledger_root(&self, node_id: &str) -> Result<Option<String>, StoreError> {
            self.0.ledger_root(node_id)
        }
        fn set_ledger_root(&self, node_id: &str, commit: &str) -> Result<(), StoreError> {
            self.0.set_ledger_root(node_id, commit)
        }
    }
    impl LedgerCache for NoCutover<'_> {
        fn last_head(&self, repo: &str) -> Result<Option<String>, StoreError> {
            self.0.last_head(repo)
        }
        fn set_last_head(&self, repo: &str, head: &str) -> Result<(), StoreError> {
            self.0.set_last_head(repo, head)
        }
        fn cached(&self, repo: &str, path: &str) -> Result<Option<CachedSegment>, StoreError> {
            self.0.cached(repo, path)
        }
        fn cached_under(
            &self,
            repo: &str,
            dir: &str,
        ) -> Result<Vec<(String, CachedSegment)>, StoreError> {
            self.0.cached_under(repo, dir)
        }
        fn cache(&self, repo: &str, path: &str, s: &CachedSegment) -> Result<(), StoreError> {
            self.0.cache(repo, path, s)
        }
    }
    impl Outbox for NoCutover<'_> {
        fn unpublished(&self, repo: &str, after: &Iri) -> Result<Pending, StoreError> {
            self.0.unpublished(repo, after)
        }
        fn is_published(&self, repo: &str, id: &Iri) -> Result<bool, StoreError> {
            self.0.is_published(repo, id)
        }
        fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError> {
            self.0.mark_published(repo, ids)
        }
        fn set_aside(&self, ids: &[Iri]) -> Result<(), StoreError> {
            self.0.set_aside(ids)
        }
        fn cutover(&self, repo: &str) -> Result<Option<Iri>, StoreError> {
            self.0.cutover(repo)
        }
        fn set_cutover(&self, _: &str, _: &Iri) -> Result<(), StoreError> {
            Err(StoreError::Backend("the disk is full".into()))
        }
    }

    // ⚠ Ruling 16: the cut-over is recorded before the root, so a root on
    // record means a cut-over is too — a failed cut-over leaves no root.
    #[test]
    fn a_cut_over_that_cannot_be_recorded_leaves_no_root() {
        let fake = FakeGithub::start("acme/widgets");
        let store = MemStore::default();
        let local = NoCutover(&store);
        let c = client(&fake);
        assert!(open(&c, &local).init(&entry_iri(0), None).is_err());
        assert_eq!(store.ledger_root("R_1").unwrap(), None);
    }

    // Spec §6.2: protected only with both rules in force.
    #[test]
    fn the_mode_is_protected_only_when_both_rules_are_in_force() {
        let fake = FakeGithub::start("acme/widgets");
        fake.seed_ledger();
        let local = MemStore::default();
        let c = client(&fake);
        let mode = |rulesets: Vec<Ruleset>| {
            fake.state().rulesets = rulesets;
            open(&c, &local).mode().unwrap()
        };
        let both = ["non_fast_forward", "deletion"];
        assert_eq!(mode(vec![Ruleset::on_ledger("active", &both)]), Mode::Protected);
        assert_eq!(
            mode(vec![
                Ruleset::on_ledger("active", &["non_fast_forward"]),
                Ruleset::on_ledger("active", &["deletion"]),
            ]),
            Mode::Protected,
            "two rulesets may share the two rules"
        );
        for (rulesets, names) in [
            (vec![], vec!["`non_fast_forward`", "`deletion`"]),
            (vec![Ruleset::on_ledger("active", &["non_fast_forward"])], vec!["`deletion`"]),
            (vec![Ruleset::on_ledger("disabled", &both)], vec!["`non_fast_forward`", "`deletion`"]),
            (vec![Ruleset::on_ledger("evaluate", &both)], vec!["`non_fast_forward`", "`deletion`"]),
        ] {
            match mode(rulesets) {
                Mode::DetectionOnly { why } => {
                    for n in &names {
                        assert!(why.contains(n), "{why}");
                    }
                    if names.len() == 1 {
                        assert!(!why.contains("`non_fast_forward`"), "names only what is missing: {why}");
                    }
                }
                Mode::Protected => panic!("{names:?} missing, yet protected"),
            }
        }
        fake.state().rulesets = vec![];
        fake.state().rules_need_upgrade = true;
        match open(&c, &local).mode().unwrap() {
            Mode::DetectionOnly { why } => assert!(why.contains("plan"), "{why}"),
            Mode::Protected => panic!("no rulesets on the plan, yet protected"),
        }
        assert_eq!(Mode::Protected.name(), "protected");
    }

    // Spec §6.1 steps 6-8 and §6.3.
    #[test]
    fn the_guidance_names_the_mode_the_ruleset_the_default_branch_the_permissions_and_the_limit() {
        let detection = Mode::DetectionOnly {
            why: "no active ruleset".into(),
        };
        let all = guidance("acme/widgets", &detection).join("\n");
        for part in [
            "detection-only",
            "gh api --method POST repos/acme/widgets/rulesets",
            "default branch",
            "Contents: read and write",
            "Issues: read and write",
            "Metadata: read",
            "made public",
        ] {
            assert!(all.contains(part), "{part}: {all}");
        }
        let protected = guidance("acme/widgets", &Mode::Protected).join("\n");
        assert!(protected.contains("protected"), "{protected}");
        assert!(!protected.contains("gh api"), "no ruleset to add: {protected}");

        let command = ruleset_command("acme/widgets");
        let body = command
            .split_once('\n')
            .and_then(|(_, rest)| rest.rsplit_once("\nJSON"))
            .map(|(json, _)| json)
            .expect("a heredoc body");
        let v: Value = serde_json::from_str(body).expect("the ruleset is JSON");
        assert_eq!(v["conditions"]["ref_name"]["include"][0], "refs/heads/fl/ledger");
        assert_eq!(v["enforcement"], "active");
        let rules: Vec<&str> = v["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["type"].as_str().unwrap())
            .collect();
        assert_eq!(rules, vec!["non_fast_forward", "deletion"]);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-github --lib ledger::init`
Expected: FAIL to compile — `init`, `mode`, `guidance` and `ruleset_command` do not exist.

- [ ] **Step 3: Write the implementation**

Add to `crates/github/src/ledger/git.rs`, inside `impl GithubLedger<'_>`:

```rust
    /// A ledger commit's parents, first parent first.
    pub(crate) fn parents_of(&self, sha: &str) -> Result<Vec<String>, StoreError> {
        let r = self
            .client
            .send(Method::Get, &self.path(&format!("/git/commits/{sha}")), None)?;
        if r.status != 200 {
            return Err(backend(format!(
                "GitHub answered {} when fl read ledger commit {sha}; retry",
                r.status
            )));
        }
        let parents = r
            .body
            .get("parents")
            .and_then(Value::as_array)
            .ok_or_else(|| backend(format!("GitHub answered ledger commit {sha} with no parents")))?;
        Ok(parents
            .iter()
            .filter_map(|p| p.get("sha").and_then(Value::as_str).map(str::to_string))
            .collect())
    }
```

In `crates/github/src/ledger/mod.rs`, add `mod init;` after `mod git;`, and after `pub use append::TRIES;`:

```rust
pub use init::{InitOutcome, Mode, guidance, ruleset_command};
```

Prepend to `crates/github/src/ledger/init.rs` (above its tests):

```rust
//! Setting up the GitHub ledger (GitHub ledger spec §6.1, §6.2): `init`,
//! recovering from a run that stopped partway; the mode in force; and what
//! `init` tells the person.

use super::GithubLedger;
use super::layout::{BRANCH, FORMAT_FILE, README, README_FILE};
use crate::client::Method;
use fl_core::iri::Iri;
use fl_core::{LedgerFault, StoreError, ledger_root_shape};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// What `init` found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitOutcome {
    /// The branch was created at `root`; this machine recorded it and its
    /// cut-over.
    Created { root: String },
    /// This machine already knew the ledger's root. `cutover_recorded` says
    /// whether it had no cut-over and now has one (ruling 16).
    AlreadySetUp { root: String, cutover_recorded: bool },
    /// The branch exists and this machine knows no root: the person confirms
    /// that `root` is the first commit they made, by running `init` again
    /// with it (spec §6.1 step 5). Nothing was recorded.
    Confirm { root: String },
    /// The person confirmed `root`; this machine recorded it and its
    /// cut-over.
    Adopted { root: String },
}

/// The guarantee in force on `fl/ledger` (spec §6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// A ruleset refuses a rewrite and a deletion; fl detects an edit.
    Protected,
    /// Nothing refuses them; fl detects all three. `why` names what is
    /// missing.
    DetectionOnly { why: String },
}

impl Mode {
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Protected => "protected",
            Mode::DetectionOnly { .. } => "detection-only",
        }
    }
}

const RULES: [&str; 2] = ["non_fast_forward", "deletion"];

impl GithubLedger<'_> {
    /// `fl github ledger init` (spec §6.1 steps 2–5). Run once per
    /// repository by a person, and safe to run again: it recovers from a
    /// run that stopped after any step.
    ///
    /// `cutover` is this machine's cut-over, minted by the caller; it is
    /// recorded only when the machine has none. `confirmed` is the first
    /// commit the person confirmed after an earlier `Confirm`.
    pub fn init(&self, cutover: &Iri, confirmed: Option<&str>) -> Result<InitOutcome, StoreError> {
        let repo = self.repo.full_name.clone();
        // ⚠ Step 2: git cannot hold both `fl` and `fl/ledger`.
        if self.branch_head("fl")?.is_some() {
            return Err(StoreError::Backend(format!(
                "the repository {repo} has a branch named `fl`, and git cannot hold both `fl` \
                 and `fl/ledger`. Rename that branch, then run `fl github ledger init` again"
            )));
        }
        match (
            self.branch_head(BRANCH)?,
            self.local.ledger_root(&self.repo.node_id)?,
        ) {
            // ⚠ Step 5: the ledger was deleted. A new one would hide that.
            (None, Some(root)) => Err(LedgerFault::Deleted { repo, root }.into()),
            (None, None) => {
                let root = self.create_branch()?;
                self.record(cutover, &root)?;
                Ok(InitOutcome::Created { root })
            }
            (Some(_), Some(root)) => {
                self.check_head()?;
                let cutover_recorded = self.record_cutover(cutover)?;
                Ok(InitOutcome::AlreadySetUp {
                    root,
                    cutover_recorded,
                })
            }
            (Some(head), None) => {
                let first = self.first_commit(&head)?;
                match confirmed {
                    None => Ok(InitOutcome::Confirm { root: first }),
                    Some(c) if c == first => {
                        self.record(cutover, &first)?;
                        Ok(InitOutcome::Adopted { root: first })
                    }
                    Some(c) => Err(StoreError::Backend(format!(
                        "`{c}` is not the first commit of {repo}'s `fl/ledger` branch; {first} \
                         is. Confirm that commit, or find out who started the ledger"
                    ))),
                }
            }
        }
    }

    /// Step 3: `format` and `README.md` in a commit with no parent — through
    /// the REST Git Data API, because `createCommitOnBranch` needs a branch
    /// that exists — and the branch at it.
    fn create_branch(&self) -> Result<String, StoreError> {
        let tree = self.created(
            "/git/trees",
            json!({"tree": [
                {"path": FORMAT_FILE, "mode": "100644", "type": "blob", "content": "1\n"},
                {"path": README_FILE, "mode": "100644", "type": "blob", "content": README},
            ]}),
            "the ledger's first tree",
        )?;
        let commit = self.created(
            "/git/commits",
            json!({"message": "fl: start the ledger", "tree": tree, "parents": []}),
            "the ledger's first commit",
        )?;
        let made = self.client.send(
            Method::Post,
            &self.path("/git/refs"),
            Some(&json!({"ref": format!("refs/heads/{BRANCH}"), "sha": commit})),
        );
        match made {
            Ok(r) if r.status == 201 => Ok(commit),
            Ok(r) => Err(StoreError::Backend(format!(
                "GitHub answered {} when fl created the branch `fl/ledger`; nothing was \
                 recorded. Run `fl github ledger init` again",
                r.status
            ))),
            // ⚠ Modelled: a ref that exists is refused with 422 "Reference
            // already exists". Confirmed by live test
            // `init_sets_up_a_ledger_on_a_private_repository`.
            Err(StoreError::Backend(m)) if m.contains("already exists") => {
                Err(StoreError::Backend(format!(
                    "someone created `fl/ledger` in {} while init ran, so this machine \
                     recorded nothing. Run `fl github ledger init` again: it asks you to \
                     confirm that branch's first commit",
                    self.repo.full_name
                )))
            }
            Err(e) => Err(e),
        }
    }

    /// The id of what a POST to `rest` created.
    fn created(&self, rest: &str, body: Value, what: &str) -> Result<String, StoreError> {
        let r = self.client.send(Method::Post, &self.path(rest), Some(&body))?;
        if r.status != 201 {
            return Err(StoreError::Backend(format!(
                "GitHub answered {} when fl created {what}; nothing was recorded. Run `fl \
                 github ledger init` again",
                r.status
            )));
        }
        r.body
            .get("sha")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| StoreError::Backend(format!("GitHub created {what} but named no id")))
    }

    /// Step 4: this machine's cut-over if it has none, then the root.
    ///
    /// ⚠ In that order (ruling 16): a root on record means the cut-over is
    /// too, and a run that stopped between the two keeps its cut-over.
    fn record(&self, cutover: &Iri, root: &str) -> Result<(), StoreError> {
        ledger_root_shape(&self.repo.node_id, root).map_err(|why| {
            StoreError::Backend(format!(
                "fl cannot record {root} as the ledger's root for repository node {}: it cannot \
                 be one ({why})",
                self.repo.node_id
            ))
        })?;
        self.record_cutover(cutover)?;
        self.local.set_ledger_root(&self.repo.node_id, root)
    }

    /// Whether this machine had no cut-over and now has `cutover`. ⚠ An
    /// existing cut-over never moves.
    fn record_cutover(&self, cutover: &Iri) -> Result<bool, StoreError> {
        if self.local.cutover(&self.repo.node_id)?.is_some() {
            return Ok(false);
        }
        self.local.set_cutover(&self.repo.node_id, cutover)?;
        Ok(true)
    }

    /// The branch's first commit: first parents from `head` back to the
    /// commit with none.
    fn first_commit(&self, head: &str) -> Result<String, StoreError> {
        let mut at = head.to_string();
        loop {
            match self.parents_of(&at)?.into_iter().next() {
                Some(p) => at = p,
                None => return Ok(at),
            }
        }
    }

    /// The mode in force (spec §6.2), from the rules GitHub applies to
    /// `fl/ledger`.
    ///
    /// ⚠ Modelled: `rules/branches` lists only the rules in force, so a
    /// disabled or evaluate-only ruleset shows as the rules missing; a plan
    /// without rulesets answers 403 with an upgrade message. Confirmed by
    /// live tests `rules_on_the_ledger_branch_are_readable` and
    /// `a_private_repository_without_a_ruleset_is_detection_only`.
    pub fn mode(&self) -> Result<Mode, StoreError> {
        let r = match self.client.send(
            Method::Get,
            &self.path(&format!("/rules/branches/{BRANCH}")),
            None,
        ) {
            Ok(r) => r,
            Err(StoreError::Backend(m)) if m.contains("Upgrade to GitHub") => {
                return Ok(Mode::DetectionOnly {
                    why: "rulesets are not available on this repository's plan, so nothing \
                          stops a rewrite or a deletion; fl detects them"
                        .into(),
                });
            }
            Err(e) => return Err(e),
        };
        if r.status != 200 {
            return Err(StoreError::Backend(format!(
                "GitHub answered {} when fl read the rules on `fl/ledger`; retry",
                r.status
            )));
        }
        let in_force: BTreeSet<&str> = r
            .body
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("type").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        let missing: Vec<String> = RULES
            .iter()
            .filter(|t| !in_force.contains(**t))
            .map(|t| format!("`{t}`"))
            .collect();
        if missing.is_empty() {
            return Ok(Mode::Protected);
        }
        Ok(Mode::DetectionOnly {
            why: format!(
                "no active ruleset on `fl/ledger` has {} (a ruleset that is disabled or only \
                 evaluating applies none), so nothing stops a rewrite or a deletion; fl \
                 detects them",
                missing.join(" or ")
            ),
        })
    }
}

/// What `init` tells the person once the ledger is set up (spec §6.1 steps
/// 6–8, §6.2, §6.3), one paragraph each, for the command to print.
pub fn guidance(repo: &str, mode: &Mode) -> Vec<String> {
    let mut out = vec![match mode {
        Mode::Protected => "mode: protected. A ruleset on `fl/ledger` refuses a rewrite or a \
                            deletion, and fl detects an edit."
            .to_string(),
        Mode::DetectionOnly { why } => format!("mode: detection-only. {why}."),
    }];
    if *mode != Mode::Protected {
        out.push(format!(
            "Optional, for an administrator, where the plan offers rulesets (on GitHub Free a \
             private repository has none, and the ledger stays detection-only): protect \
             `fl/ledger` with a ruleset. fl's credential must not hold Administration \
             permission, so fl cannot add it itself:\n\n{}",
            ruleset_command(repo)
        ));
    }
    out.push(format!(
        "Protect the default branch of {repo} as well: Contents: write lets fl's credential \
         push to any branch."
    ));
    out.push(
        "fl's credential needs Contents: read and write, Issues: read and write, and Metadata: \
         read. A missing write permission shows at the first flush or comment, whose error \
         names it; until it is granted, every move, check and finding decision is refused, and \
         each attempt is kept locally with a warning."
            .to_string(),
    );
    out.push(format!(
        "Disclosure: on a private repository the ledger keeps output excerpts. If {repo} is \
         ever made public, every excerpt in the ledger's history becomes public, and removing \
         one would take the history rewrite the ledger exists to forbid."
    ));
    out
}

/// The administrator's ruleset for `fl/ledger`, as a ready `gh api`
/// command (spec §6.1 step 6).
pub fn ruleset_command(repo: &str) -> String {
    let body = json!({
        "name": "fl ledger",
        "target": "branch",
        "enforcement": "active",
        "conditions": {"ref_name": {"include": [format!("refs/heads/{BRANCH}")], "exclude": []}},
        "rules": RULES.iter().map(|r| json!({"type": r})).collect::<Vec<_>>(),
    });
    format!("gh api --method POST repos/{repo}/rulesets --input - <<'JSON'\n{body}\nJSON")
}

```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fl-github`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

1. `init`: delete the step-2 check → `init_refuses_when_a_branch_named_fl_exists` red.
2. `init`: create a new branch when the root is known and the branch is gone → `init_refuses_a_ledger_that_was_deleted` red.
3. `init`: skip `check_head` when set up → `init_refuses_a_rewritten_ledger` red.
4. `init`: accept any confirmed commit → `init_that_stopped_after_the_branch_asks…` red.
5. `record_cutover`: always write the new cut-over → `init_run_again_says…` and `init_that_stopped_between…` red (`CutoverChanged`).
6. `init`'s set-up arm: skip `record_cutover` → `a_machine_that_imported_the_root_records_its_own_cut_over` red.
7. `record`: record the root before the cut-over → `a_cut_over_that_cannot_be_recorded_leaves_no_root` red.
8. `record`: drop the shape check → `init_refuses_a_root_it_cannot_record` red.
9. `create_branch`: treat "already exists" as success → `someone_creating_the_branch_while_init_runs…` red.
10. `mode`: require only `non_fast_forward` → `the_mode_is_protected_only_when_both_rules_are_in_force` red; drop the upgrade arm → the same test red (an error, not a mode).
11. `guidance`: print the ruleset command when protected → `the_guidance_names…` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/ledger/mod.rs crates/github/src/ledger/git.rs crates/github/src/ledger/init.rs
git commit -m "feat(github): ledger init, the mode in force, and init's guidance

init refuses a branch named fl, creates the orphan first commit through
the Git Data API, and records this machine's cut-over, then the root; run
again it recovers from every step (asking to confirm the first commit of
a branch it did not record), refuses a deleted or rewritten ledger, and
records a missing cut-over on a machine that imported the root. mode reads
the rules in force on fl/ledger; guidance carries the ruleset command, the
default-branch advice, the permissions and the disclosure limit. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 12: `verify` and `quarantine`

**Files:**
- Create: `crates/github/src/ledger/verify.rs`
- Modify: `crates/github/src/ledger/git.rs` (`CommitObject`, `commit_object`, `tree_files`; `parents_of` over `commit_object`)
- Modify: `crates/github/src/ledger/mod.rs` (module, exports)
- Modify: `crates/github/src/fake.rs` (`State`: `truncate_trees`), `crates/github/src/fake_git.rs` (`get_tree`)

**Interfaces:**
- Consumes: `snapshot`, `identity`, `append`, `NewLine` (Tasks 9–10); `layout::{parse_segment_path, lines, QuarantineLine}` (Task 6).
- Produces:

```rust
// fl_github::ledger (re-exported from verify)
pub struct Verified { pub commits: usize, pub first_bad: Option<BadCommit> }   // Debug, Eq
pub struct BadCommit { pub commit: String, pub what: String }                 // Debug, Eq
impl GithubLedger<'_> {
    pub fn verify(&self) -> Result<Verified, StoreError>;
    pub fn quarantine(&self, id: &Iri, at: &At, file: &str, line: u64, by_name: &str,
                      reason: &str) -> Result<Option<String>, StoreError>;
}

// crate-internal (git.rs)
pub(crate) struct CommitObject { pub tree: String, pub parents: Vec<String> }
pub(crate) fn commit_object(&self, sha: &str) -> Result<CommitObject, StoreError>;
pub(crate) fn tree_files(&self, tree: &str) -> Result<BTreeMap<String, String>, StoreError>; // path → blob

// fl_github::fake::State
pub truncate_trees: bool,
```

- [ ] **Step 1: Write the failing tests**

Create `crates/github/src/ledger/verify.rs` with only its tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::creds::EnvToken;
    use crate::fake::{FakeGithub, USER_LOGIN};
    use crate::tracker::Repo;
    use fl_core::MemStore;
    use fl_core::conformance::{sample_decision, sample_record_run};
    use fl_core::ids::{GateId, RecordId, seq_iri};
    use fl_core::log::GateRun;
    use fl_core::split::{Batch, RemoteLedger};
    use fl_core::store::Bindings;
    use std::time::Duration;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    fn repo() -> Repo {
        Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        }
    }

    fn world() -> (FakeGithub, MemStore, String) {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        (fake, local, root)
    }

    fn open<'a>(c: &'a Client, local: &'a MemStore) -> GithubLedger<'a> {
        GithubLedger::new(c, repo(), local).with_lag(0, Duration::ZERO)
    }

    fn gate() -> GateId {
        GateId(seq_iri(7))
    }

    fn record() -> RecordId {
        RecordId(Iri::parse("https://github.com/acme/widgets/issues/1").unwrap())
    }

    fn run(n: u64) -> GateRun {
        sample_record_run(n, &gate(), Some(&record()))
    }

    fn seg(n: u64) -> String {
        layout::segment_path(&layout::dir(layout::Area::Runs, gate().iri()), n)
    }

    fn file(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    fn line(r: &GateRun) -> String {
        layout::Line::Run(r.clone()).encode("someone")
    }

    fn publish(l: &GithubLedger<'_>, n: u64) {
        l.publish(&Batch {
            decision: sample_decision(n, &record(), vec![run(n).id.unwrap()]),
            runs: vec![run(n)],
            attempts: vec![],
        })
        .unwrap();
    }

    // Spec §3.5: a ledger fl wrote only ever adds.
    #[test]
    fn a_ledger_fl_wrote_verifies_clean() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        for n in 1..=3 {
            publish(&l, n);
        }
        let v = l.verify().unwrap();
        assert_eq!(v.first_bad, None, "{v:?}");
        assert_eq!(v.commits, fake.ledger_commits());
    }

    // Spec §3.5: verify reports the first commit that does anything but
    // add lines or segments.
    #[test]
    fn each_departure_is_reported_at_the_commit_that_made_it() {
        let cases: Vec<(&str, Vec<(String, Option<String>)>, &str)> = vec![
            ("a deletion", vec![(seg(1), None)], "deletes"),
            ("the README", vec![(README_FILE.into(), Some("edited".into()))], "changes `README.md`"),
            ("the format", vec![(FORMAT_FILE.into(), Some("1\n\n".into()))], "changes `format`"),
            (
                "a rewritten line",
                vec![(seg(1), Some(file(&[line(&run(9))])))],
                "rewrites lines",
            ),
            ("a stray file", vec![("notes.txt".into(), Some("x".into()))], "adds `notes.txt`"),
            ("a gap", vec![(seg(3), Some(file(&[line(&run(3))])))], "gap"),
        ];
        for (case, changes, what) in cases {
            let (fake, local, _root) = world();
            fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
            let changes: Vec<(&str, Option<&str>)> = changes
                .iter()
                .map(|(p, t)| (p.as_str(), t.as_deref()))
                .collect();
            let bad = fake.hand_commit(&changes);
            fake.hand_commit(&[(seg(1).as_str(), None), ("other.txt", Some("y"))]);
            let c = client(&fake);
            let v = open(&c, &local).verify().unwrap();
            let first = v.first_bad.unwrap_or_else(|| panic!("{case}: nothing reported"));
            assert_eq!(first.commit, bad, "{case}: {first:?}");
            assert!(first.what.contains(what), "{case}: {}", first.what);
        }
    }

    #[test]
    fn a_closed_segment_that_changes_is_reported() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        fake.hand_commit(&[(seg(2).as_str(), Some(file(&[line(&run(2))]).as_str()))]);
        let bad = fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(3))]).as_str()),
        )]);
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, bad);
        assert!(first.what.contains("closed"), "{}", first.what);
    }

    #[test]
    fn a_second_history_or_a_merge_is_reported() {
        let (fake, local, root) = world();
        let new_root = fake.rewrite_ledger(&[("format", "1\n")]);
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, new_root);
        assert!(first.what.contains(&root), "names the real first commit: {}", first.what);

        let (fake2, local2, _root2) = world();
        let merge = fake2.hand_merge();
        let c2 = client(&fake2);
        let first = open(&c2, &local2).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, merge);
        assert!(first.what.contains("merges"), "{}", first.what);
    }

    #[test]
    fn a_first_commit_holding_more_than_fl_writes_is_reported() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger_with(&[
            ("format", "1\n"),
            ("README.md", "x"),
            (".github/workflows/ci.yml", "x"),
        ]);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, root);
    }

    #[test]
    fn a_tree_listing_cut_short_is_an_error_not_a_pass() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        fake.state().truncate_trees = true;
        let c = client(&fake);
        let err = open(&c, &local).verify().unwrap_err();
        assert!(err.to_string().contains("short"), "{err}");
    }

    #[test]
    fn verify_says_why_when_there_is_no_ledger_to_walk() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        assert!(matches!(
            open(&c, &local).verify().unwrap_err(),
            StoreError::Ledger(LedgerFault::NotSetUp { .. })
        ));
        fake.seed_ledger();
        assert!(matches!(
            open(&c, &local).verify().unwrap_err(),
            StoreError::Ledger(LedgerFault::NoAnchor { .. })
        ));
    }

    // Spec §3.6: quarantine appends; readers skip the line; nothing is
    // removed.
    #[test]
    fn an_unreadable_line_once_quarantined_is_skipped_and_its_damage_stays() {
        let (fake, local, _root) = world();
        let damaged = format!("{}not json\n", file(&[line(&run(1))]));
        fake.hand_commit(&[(seg(1).as_str(), Some(damaged.as_str()))]);
        let c = client(&fake);
        let l = open(&c, &local);
        assert!(l.runs(&gate()).is_err());
        let committed = l
            .quarantine(&seq_iri(60), &At::from_unix_millis(60), &seg(1), 2, "Ada", "a hand edit")
            .unwrap();
        assert_eq!(committed, fake.ledger_head());
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
        assert_eq!(l.take_notes().len(), 1);
        let files = fake.ledger_files();
        assert!(files[&seg(1)].contains("not json"), "nothing was removed");
        let q = QuarantineLine::decode(files[QUARANTINE_FILE].trim_end()).unwrap();
        assert_eq!((q.file.as_str(), q.line), (seg(1).as_str(), 2));
        assert_eq!((q.quarantined_by.as_str(), q.by.as_str()), ("Ada", USER_LOGIN));
        assert_eq!(l.verify().unwrap().first_bad, None, "a quarantine only adds");
    }

    // Ruling 17: a line in the wrong directory can be quarantined too.
    #[test]
    fn a_misplaced_line_can_be_quarantined() {
        let (fake, local, _root) = world();
        let elsewhere = sample_record_run(1, &GateId(seq_iri(8)), Some(&record()));
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&elsewhere)]).as_str()))]);
        let c = client(&fake);
        let l = open(&c, &local);
        l.quarantine(&seq_iri(61), &At::from_unix_millis(61), &seg(1), 1, "Ada", "misfiled")
            .unwrap();
        assert!(l.runs(&gate()).unwrap().is_empty());
    }

    #[test]
    fn a_quarantine_retried_after_a_lost_answer_adds_one_line() {
        let (fake, local, _root) = world();
        let damaged = format!("{}not json\n", file(&[line(&run(1))]));
        fake.hand_commit(&[(seg(1).as_str(), Some(damaged.as_str()))]);
        fake.state().hang_up_after_next_commit = true;
        let c = client(&fake);
        let l = open(&c, &local);
        l.quarantine(&seq_iri(60), &At::from_unix_millis(60), &seg(1), 2, "Ada", "r")
            .unwrap();
        l.quarantine(&seq_iri(60), &At::from_unix_millis(60), &seg(1), 2, "Ada", "r")
            .unwrap();
        assert_eq!(layout::lines(&fake.ledger_files()[QUARANTINE_FILE]).len(), 1);
    }

    #[test]
    fn a_quarantine_must_name_a_line_of_a_segment_and_who_decided_why() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let c = client(&fake);
        let l = open(&c, &local);
        let at = At::from_unix_millis(60);
        for (file, line, by, reason, says) in [
            (QUARANTINE_FILE.to_string(), 1, "Ada", "r", "its own lines"),
            (FORMAT_FILE.to_string(), 1, "Ada", "r", "not a segment"),
            (seg(2), 1, "Ada", "r", "no such file"),
            (seg(1), 0, "Ada", "r", "1 lines"),
            (seg(1), 2, "Ada", "r", "1 lines"),
            (seg(1), 1, " ", "r", "--by"),
            (seg(1), 1, "Ada", "", "--reason"),
        ] {
            let err = l
                .quarantine(&seq_iri(60), &at, &file, line, by, reason)
                .unwrap_err();
            assert!(err.to_string().contains(says), "{file} {line}: {err}");
        }
        assert!(!fake.ledger_files().contains_key(QUARANTINE_FILE), "nothing written");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fl-github --lib ledger::verify`
Expected: FAIL to compile — `verify`, `quarantine`, `commit_object`, `tree_files` and `truncate_trees` do not exist.

- [ ] **Step 3: Write the implementation**

In `crates/github/src/fake.rs`, add to `State`:

```rust
    /// Every recursive tree listing says GitHub cut it short. A setting.
    pub truncate_trees: bool,
```

and in `crates/github/src/fake_git.rs`'s `get_tree`, replace the final `answer(…)` with:

```rust
    answer(
        200,
        json!({"sha": sha, "tree": items, "truncated": s.truncate_trees}),
    )
```

In `crates/github/src/ledger/git.rs`, add `use std::collections::BTreeMap;` to the imports, add after `struct Entry`:

```rust
/// A commit as the ledger walks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitObject {
    pub tree: String,
    /// First parent first.
    pub parents: Vec<String>,
}
```

and replace `parents_of` (Task 11) with:

```rust
    /// A ledger commit: its tree and its parents.
    pub(crate) fn commit_object(&self, sha: &str) -> Result<CommitObject, StoreError> {
        let r = self
            .client
            .send(Method::Get, &self.path(&format!("/git/commits/{sha}")), None)?;
        if r.status != 200 {
            return Err(backend(format!(
                "GitHub answered {} when fl read ledger commit {sha}; retry",
                r.status
            )));
        }
        let tree = r
            .body
            .pointer("/tree/sha")
            .and_then(Value::as_str)
            .ok_or_else(|| backend(format!("GitHub answered ledger commit {sha} with no tree")))?
            .to_string();
        let parents = r
            .body
            .get("parents")
            .and_then(Value::as_array)
            .ok_or_else(|| backend(format!("GitHub answered ledger commit {sha} with no parents")))?
            .iter()
            .filter_map(|p| p.get("sha").and_then(Value::as_str).map(str::to_string))
            .collect();
        Ok(CommitObject { tree, parents })
    }

    /// A ledger commit's parents, first parent first.
    pub(crate) fn parents_of(&self, sha: &str) -> Result<Vec<String>, StoreError> {
        Ok(self.commit_object(sha)?.parents)
    }

    /// Every file of tree `sha`, path → blob id.
    ///
    /// ⚠ A listing GitHub cut short is an error, never read as the whole
    /// tree: a verify over part of a tree would pass what it never saw.
    pub(crate) fn tree_files(&self, sha: &str) -> Result<BTreeMap<String, String>, StoreError> {
        let r = self.client.send(
            Method::Get,
            &self.path(&format!("/git/trees/{sha}?recursive=1")),
            None,
        )?;
        if r.status != 200 {
            return Err(backend(format!(
                "GitHub answered {} when fl listed ledger tree {sha}; retry",
                r.status
            )));
        }
        if r.body.get("truncated").and_then(Value::as_bool) == Some(true) {
            return Err(backend(format!(
                "GitHub cut its listing of ledger tree {sha} short, so fl cannot check that \
                 commit; nothing past it was verified"
            )));
        }
        let items = r
            .body
            .get("tree")
            .and_then(Value::as_array)
            .ok_or_else(|| backend(format!("GitHub listed ledger tree {sha} with no entries")))?;
        Ok(items
            .iter()
            .filter(|e| e["type"].as_str() == Some("blob"))
            .filter_map(|e| Some((e["path"].as_str()?.to_string(), e["sha"].as_str()?.to_string())))
            .collect())
    }
```

In `crates/github/src/ledger/mod.rs`, add `mod verify;` after `mod read;`, and after the `pub use init::…;` line:

```rust
pub use verify::{BadCommit, Verified};
```

Prepend to `crates/github/src/ledger/verify.rs` (above its tests):

```rust
//! What a person runs by hand (GitHub ledger spec §3.5, §3.6): `verify`,
//! which walks every commit from the anchor and checks that each only adds
//! lines or segments, and `quarantine`, which marks a line readers skip
//! without removing it.

use super::GithubLedger;
use super::append::NewLine;
use super::git::CommitObject;
use super::layout::{
    self, BRANCH, FORMAT, FORMAT_FILE, QUARANTINE_FILE, QuarantineLine, README_FILE,
};
use fl_core::at::At;
use fl_core::iri::Iri;
use fl_core::{LedgerFault, StoreError};
use std::collections::{BTreeMap, BTreeSet};

/// What `verify` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// How many commits it walked.
    pub commits: usize,
    /// The oldest commit that does anything but add, and what it does.
    pub first_bad: Option<BadCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadCommit {
    pub commit: String,
    pub what: String,
}

impl GithubLedger<'_> {
    /// `fl github ledger verify` (spec §3.5): every commit from the anchor
    /// to the head, along first parents, each checked to only add lines or
    /// segments. About one request per commit, plus the files it compares.
    pub fn verify(&self) -> Result<Verified, StoreError> {
        let repo = self.repo.full_name.clone();
        let (head, anchor) = match (
            self.branch_head(BRANCH)?,
            self.local.ledger_root(&self.repo.node_id)?,
        ) {
            (Some(h), Some(a)) => (h, a),
            (None, None) => return Err(LedgerFault::NotSetUp { repo }.into()),
            (None, Some(root)) => return Err(LedgerFault::Deleted { repo, root }.into()),
            (Some(_), None) => return Err(LedgerFault::NoAnchor { repo }.into()),
        };
        // Newest first: from the head back to the anchor, or to a commit
        // with no parent that is not the anchor.
        let mut chain: Vec<(String, CommitObject)> = Vec::new();
        let mut bad: Vec<(String, String)> = Vec::new();
        let mut at = head;
        loop {
            let c = self.commit_object(&at)?;
            let parents = c.parents.clone();
            chain.push((at.clone(), c));
            if at == anchor {
                break;
            }
            match parents.as_slice() {
                [] => {
                    bad.push((
                        at.clone(),
                        format!(
                            "starts a history of its own: it has no parent, and it is not the \
                             ledger's first commit {anchor}"
                        ),
                    ));
                    break;
                }
                [p] => at = p.clone(),
                [p, ..] => {
                    bad.push((
                        at.clone(),
                        "merges two histories, and fl only ever adds one commit on top of the \
                         last"
                            .into(),
                    ));
                    at = p.clone();
                }
            }
        }
        chain.reverse();
        let mut blobs: BTreeMap<String, String> = BTreeMap::new();
        if chain.first().is_some_and(|(c, _)| *c == anchor) {
            if let Some(what) = self.fls_first_commit(&chain[0].1, &mut blobs)? {
                bad.push((anchor.clone(), what));
            }
            for pair in chain.windows(2) {
                if let Some(what) = self.only_adds(&pair[0].1, &pair[1].1, &mut blobs)? {
                    bad.push((pair[1].0.clone(), what));
                }
            }
        }
        let age: BTreeMap<&str, usize> = chain
            .iter()
            .enumerate()
            .map(|(i, (c, _))| (c.as_str(), i))
            .collect();
        let first_bad = bad
            .into_iter()
            .min_by_key(|(c, _)| age.get(c.as_str()).copied().unwrap_or(0))
            .map(|(commit, what)| BadCommit { commit, what });
        Ok(Verified {
            commits: chain.len(),
            first_bad,
        })
    }

    fn text_of(&self, oid: &str, blobs: &mut BTreeMap<String, String>) -> Result<String, StoreError> {
        if let Some(t) = blobs.get(oid) {
            return Ok(t.clone());
        }
        let t = self.blob_text(oid)?;
        blobs.insert(oid.to_string(), t.clone());
        Ok(t)
    }

    /// What is wrong with the anchor, if anything: it holds exactly
    /// `format` (reading 1) and `README.md`.
    fn fls_first_commit(
        &self,
        anchor: &CommitObject,
        blobs: &mut BTreeMap<String, String>,
    ) -> Result<Option<String>, StoreError> {
        let files = self.tree_files(&anchor.tree)?;
        let names: BTreeSet<&str> = files.keys().map(String::as_str).collect();
        if names != BTreeSet::from([FORMAT_FILE, README_FILE]) {
            return Ok(Some(format!(
                "starts the ledger with {names:?}, where fl writes only `format` and `README.md`"
            )));
        }
        let format = self.text_of(&files[FORMAT_FILE], blobs)?;
        if format.strip_suffix('\n').unwrap_or(&format) != FORMAT {
            return Ok(Some(format!("starts the ledger at format `{}`", format.trim())));
        }
        Ok(None)
    }

    /// What `after` does besides add, compared with its parent `before`.
    fn only_adds(
        &self,
        before: &CommitObject,
        after: &CommitObject,
        blobs: &mut BTreeMap<String, String>,
    ) -> Result<Option<String>, StoreError> {
        let old = self.tree_files(&before.tree)?;
        let new = self.tree_files(&after.tree)?;
        for (path, oid) in &old {
            let Some(now) = new.get(path) else {
                return Ok(Some(format!("deletes `{path}`")));
            };
            if now == oid {
                continue;
            }
            if path == FORMAT_FILE || path == README_FILE {
                return Ok(Some(format!("changes `{path}`")));
            }
            match layout::parse_segment_path(path) {
                Some((_, dir, n)) => {
                    let closed = old.keys().any(|p| {
                        layout::parse_segment_path(p).is_some_and(|(_, d, m)| d == dir && m > n)
                    });
                    if closed {
                        return Ok(Some(format!("changes `{path}` after it was closed")));
                    }
                }
                None if path == QUARANTINE_FILE => {}
                None => return Ok(Some(format!("changes `{path}`, which fl never writes"))),
            }
            let (was, is) = (self.text_of(oid, blobs)?, self.text_of(now, blobs)?);
            if !is.starts_with(&was) {
                return Ok(Some(format!("rewrites lines of `{path}`")));
            }
        }
        let mut numbers: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for path in new.keys() {
            match layout::parse_segment_path(path) {
                Some((_, dir, n)) => numbers.entry(dir).or_default().push(n),
                None if path == QUARANTINE_FILE || old.contains_key(path) => {}
                None => return Ok(Some(format!("adds `{path}`, which fl never writes"))),
            }
        }
        for (dir, mut ns) in numbers {
            ns.sort_unstable();
            if ns.iter().copied().ne(1..=ns.len() as u64) {
                return Ok(Some(format!("leaves a gap in the segments of `{dir}`")));
            }
        }
        Ok(None)
    }

    /// `fl github ledger quarantine <file> <line> --by <name> --reason
    /// <text>` (spec §3.6): readers skip that line from now on.
    ///
    /// ⚠ Nothing is removed: the damage and its repair both stay in the
    /// history. `id` is minted by the caller, so a retry adds one line.
    pub fn quarantine(
        &self,
        id: &Iri,
        at: &At,
        file: &str,
        line: u64,
        by_name: &str,
        reason: &str,
    ) -> Result<Option<String>, StoreError> {
        let refuse =
            |why: String| StoreError::Backend(format!("fl will not quarantine `{file}` line {line}: {why}"));
        if file == QUARANTINE_FILE {
            return Err(refuse(
                "the quarantine file cannot quarantine its own lines; run `fl github ledger \
                 verify` to find the commit that damaged it"
                    .into(),
            ));
        }
        let Some((_, dir, _)) = layout::parse_segment_path(file) else {
            return Err(refuse("it is not a segment of the ledger".into()));
        };
        if by_name.trim().is_empty() {
            return Err(refuse("name who decided, with --by".into()));
        }
        if reason.trim().is_empty() {
            return Err(refuse("say why, with --reason".into()));
        }
        let snap = self.snapshot(std::slice::from_ref(&dir))?;
        let held = snap
            .dirs
            .get(&dir)
            .and_then(|segs| segs.iter().find(|s| s.path == file))
            .map(|s| layout::lines(&s.text).len() as u64);
        match held {
            None => {
                return Err(refuse(format!(
                    "the ledger holds no such file at {}",
                    snap.head
                )));
            }
            Some(n) if line == 0 || line > n => {
                return Err(refuse(format!("it has {n} lines, numbered from 1")));
            }
            Some(_) => {}
        }
        let q = QuarantineLine {
            id: id.clone(),
            at: at.clone(),
            file: file.to_string(),
            line,
            quarantined_by: by_name.to_string(),
            reason: reason.to_string(),
            by: self.identity()?,
        };
        self.append(
            &[],
            &[NewLine {
                id: id.clone(),
                text: q.encode(),
            }],
            &format!("fl: quarantine {file} line {line}"),
        )
    }
}

```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fl-github`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

1. `verify`: stop at a second root without reporting it → `a_second_history_or_a_merge_is_reported` red; follow a merge without reporting it → the same test red.
2. `verify`: report the newest departure instead of the oldest (`max_by_key`) → `each_departure_is_reported_at_the_commit_that_made_it` red (each case adds a second departure after `bad`).
3. `fls_first_commit`: accept any file list → `a_first_commit_holding_more_than_fl_writes_is_reported` red.
4. `only_adds`: drop the deletion check → "a deletion" case red; drop the `format`/`README.md` check → those cases red; drop the closed-segment check → `a_closed_segment_that_changes_is_reported` red; drop the `starts_with` check → "a rewritten line" case red; drop the stray-file check → "a stray file" case red; drop the gap check → "a gap" case red.
5. `tree_files`: ignore `truncated` → `a_tree_listing_cut_short_is_an_error_not_a_pass` red.
6. `verify`: return `Verified` for a missing anchor → `verify_says_why_when_there_is_no_ledger_to_walk` red.
7. `quarantine`: each refusal — the quarantine file, a non-segment, a missing file, line 0, a line past the end, an empty `--by`, an empty `--reason` — removed one at a time → `a_quarantine_must_name_a_line_of_a_segment_and_who_decided_why` red at that row.
8. `append`'s quarantine branch (Task 10): add every line without the id check → `a_quarantine_retried_after_a_lost_answer_adds_one_line` red.
9. `lines` (Task 9) still skips a quarantined misplaced line: move the quarantine skip below the misplaced check → `a_misplaced_line_can_be_quarantined` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/fake.rs crates/github/src/fake_git.rs crates/github/src/ledger/mod.rs crates/github/src/ledger/git.rs crates/github/src/ledger/verify.rs
git commit -m "feat(github): verify the ledger's history, and quarantine a line

verify walks every commit from the anchor along first parents and reports
the oldest that deletes, changes format or README.md, changes a closed
segment, rewrites lines, adds a file fl never writes, leaves a gap in a
directory's segments, merges, or starts a second history. quarantine
appends to quarantine.jsonl, naming the segment, the line, who decided and
why; readers skip the line and nothing is removed. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

## After the last task

- [ ] Run the trio once more on the whole branch: `cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace`.
- [ ] Confirm no test reached the network: `grep -rn 'api.github.com' crates --include='*.rs'` lists only `DEFAULT_API` and the ignored live tests.
- [ ] Confirm every *Modelled* shape names the live test that will confirm it: `grep -rn 'Modelled' crates/github/src` — each hit names a test for plan B2 to write.
- [ ] Follow WORKFLOW.md: `superpowers:requesting-code-review` on the whole branch, then a pull request against `main`. The pull request names this plan's rulings 1–23 and spec defects 1–6 for the owner, and says plainly that B1 binds nothing in the CLI: `ledger = "github"` is still refused as an unknown key until B2.

## Spec coverage (plan B1's share)

| spec | where |
|---|---|
| decision 2 (projection on a repository that is not private; `internal` is not private; a failed visibility read is an error) | Task 6 (`disclose`), Task 10 (`publish_batch`, `visibility`) |
| decision 8 (an attempt whose flush fails is kept and published next time) | plan A; Task 2 (decision 14's warning) |
| decision 9 / §6.2 (modes; inactive or partial ruleset is detection-only, naming what is missing) | Task 11 (`mode`, ruling 15) |
| decision 10 / §3.6 (quarantine by an append, never a removal) | Task 9 (readers skip and note), Task 12 (`quarantine`) |
| decision 12 (Free is the baseline) | Task 11 (detection-only works; the plan's refusal is a mode, not an error) |
| decision 14 (an unpublished attempt exits with its own code, warning) | Task 2 |
| §1.1 `GithubLedger`, sharing the tracker's client | Tasks 9–10 (`GithubTracker::client`) |
| §2.1 bounded scan; a skipped entry reported | Task 3 |
| §2.5 `attempts` local-only only when GitHub cannot be read | Task 1 |
| §3.1 layout, keys, segments, lines with `by`, strict format | Task 6 |
| §3.2 append protocol: read, check, add what is missing, `expectedHeadOid`, five tries, timeout, no empty commit, last seen | Task 10 |
| §3.3 reads: listing with ids, download only what is not cached, strict lines, quarantine skipped, unreadable line names file, line, commit and command | Task 9 |
| §3.5 checks 1–7 | Task 9 (each named in the code), Task 10 (on every append) |
| §3.5 `verify` | Task 12 |
| §6.1 steps 2–5 (`init`, its recovery) and steps 6–8 (guidance) | Task 11 (the CLI prints it in B2) |
| §6.3 permissions named from `x-accepted-github-permissions` | Task 7, Task 10 |
| §7 rows: branch missing; deleted; no root; rewritten; tampering; unknown format; unreadable line; rate limited; unreachable | Task 1 (`LedgerFault`), Task 9, Task 10 |
| §8.1 fake: Git Data API, `createCommitOnBranch` with `expectedHeadOid` and its knobs, listing with ids, `rules/branches` knobs | Tasks 7–8 (paginated issue comments: B2) |
| §8.2 conformance over `SplitLedger` and the fake | Task 10 (`fixture.rs`) |
| §8.3 battery: racing flushes; timeout then retry; each tamper check and a `behind` that catches up; quarantine; unknown ledger format; rollover and many segments; refused decision flushed; `init` stopped after each step; disclosure by field and a scan for paths, `$HOME` and a host name; both modes | Tasks 9–12 (the manifest-format refusal is plan A's; comment battery items: B2) |
| plan A checklist: unbounded scan; typed refusals and retry wording; decision 14; batches visible; `LedgerRoot` shape; stats fall back only when transient; format 4 after import | Tasks 1–4 |
| §1.5 config key, §2.4 pre-flight, §4 comments, §2.6 stats rule, whoami mode, §8.4 live tests, §9 docs | plan B2 |
