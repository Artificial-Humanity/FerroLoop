# GitHub ledger, plan B2a — the binding, the pre-flight and the hand-run commands

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish everything that must precede and accompany binding the GitHub ledger in the CLI — pinned line bytes, one cache write, reads that fail closed, a bounded `verify`, the message sweep — then add the `ledger = "github"` key, `fl github ledger init | verify | quarantine`, the mode in `fl github whoami`, the `SplitLedger` binding in `Ctx`, the pre-flight before every decision, decisions 14–16 at the command line, and the `fl stats` rule.

**Architecture:** `fl-github` gets the last library fixes the B1 final review asked for (golden bytes, `remember` as the only cache write, `tree_files`/`compare`/`mode` hardening, `verify_with` with a cap, progress and a cross-directory id check, `init` refusing a branch under `fl/ledger/`). `fl-cli` reads `ledger = "github"` from the tracker binding; when it is set, `main` builds one `GithubLedger` over `GithubTracker::client` and the local store, puts it in `Ctx::github_ledger`, and binds `Ctx::ledger` to `SplitLedger { local: store, github }`. A new `preflight` module runs §2.4's checks before any gate or adapter. A new `cmd/ledger.rs` holds `fl github ledger …`. `fl stats` takes a `Source` that says whether it reads GitHub, and why not.

**Tech Stack:** Rust 2024 (`rust-version = "1.98"`), redb 4.3, serde/serde_json, clap 4, ureq 3, assert_cmd/predicates for black-box tests. No new crates.

**Spec:** `docs/superpowers/specs/2026-09-30-github-ledger-design.md` at `39eb098` — especially decisions 2, 8, 9, 12, 14, 15, 16; §1.5, §2.2, §2.4, §2.5, §2.6, §3.5, §3.6, §6.1, §6.2, §7, §8.3. Plan B1 (`2026-10-01-github-ledger-b1-storage.md`, merged) defines `GithubLedger`, its `init`/`mode`/`verify`/`quarantine`, `guidance`, `LedgerCache` and `Outbox`; its section "B2 requirements from the final review" lists the twelve items this plan and B2b carry.

**Branch:** `ferris/github-ledger-b2a`, off `ferris/github-ledger-plan-b2` (main + spec decisions 15–16 + this plan), or off `main` once that is merged.

**Plan B2b (written after B2a merges) — scope:**
- Decision comments (§4): rendered from the ledger — the `Decision` line and the entries it names, a move's or check's runs found through the local catalog's gates for its transitions (§4.2) — with the header, the table, `<details>` excerpts on a private repository only, escaping of `|`, newlines, backticks, `@`, `#`, `<!--`, the 60,000-byte cap that truncates excerpts first, the `<!-- fl:decision {"id":"…"} -->` marker, and the line saying whether the state change completed; posted after the state change for a move, `check --record`, `finding reproduce`/`verify` (on the finding's issue) and an attempt; a transferred issue gets it at its current location; a comment that fails to post leaves the state change and names the recovery command (§7).
- `fl github ledger comment <record>` (§4.3): lists the issue's comments, every page, and posts only the decisions with no marker; the fake's paginated issue-comment listing (§8.1).
- The §8.3 comment battery: escaping, the 60,000-byte cap, recovery and marker de-duplication across pages, a scan of every comment for absolute paths, `$HOME` and the host name.
- The live tests (`crates/github/tests/live.rs`, ignored by default), on the private throwaway `Artificial-Humanity/fl-live-test` (detection-only) and a public throwaway for the ruleset checks — §8.4, plus every name a *Modelled* marker in the code cites: `init_sets_up_a_ledger_on_a_private_repository`, `create_commit_on_branch_is_refused_when_the_head_moved`, `create_commit_on_branch_without_contents_write_is_refused`, `a_hand_edit_is_detected_and_named`, `rules_on_the_ledger_branch_are_readable`, `a_private_repository_without_a_ruleset_is_detection_only`, `tree_entry_modes_are_integers`, and this plan's `a_branch_under_the_ledger_branch_is_found`; plus B1 requirement 7: the shape of a GraphQL timeout (a 200 with an error must judge as "unknown"), an unrelated-history compare (404 "No common ancestor"?), an empty repository answering 409 to `git/ref` (and what `init` then says), `rules/branches/fl/ledger` on the private Free-plan repository answering `200 []` (measured by hand on 2026-10-02 — pin it in `a_private_repository_without_a_ruleset_is_detection_only`, so the fake's default stays true), a near-full segment through `createCommitOnBranch`, and on the public throwaway GitHub refusing a force update and a deletion of `fl/ledger`.
- `docs/github-ledger.md` (new) — setup, the two modes, permissions, disclosure and its known limit, every error of §7 and its remedy, quarantine, verify, and the limit of ruling 13 (an entry published with the same id as a local one but different content is caught only by a read that merges it — runs and attempts — never for a decision, and never by the append's de-duplication) — linked from `docs/README.md` and `docs/github-tracker.md`; the identity spec's §0.1 mode B row points to it; `docs/sharing-gates.md`'s manifest-format note names format 2 and `ledger_root` (§9).

---

## Global Constraints

- Verification trio, all green before every commit: `cargo fmt --all --check`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`. Each task runs `cargo fmt --all` first, so code blocks here need not be in rustfmt's exact layout; lines stay within 100 columns.
- Unit tests live in `#[cfg(test)] mod tests` inside the module they test; black-box CLI tests live in `crates/cli/tests/`.
- **No test contacts the network.** GitHub is the in-process fake (`fl_github::fake::FakeGithub` on `127.0.0.1`, reached by the binary through `FL_GITHUB_API_URL`).
- `fl-core` stays pure: "No IO, no async, no clock, no network" (`crates/core/src/lib.rs:1`).
- Spec values, verbatim: `ledger` "is optional; its only accepted value is `"github"`, and any other value is a config error that names it" (§1.5); "A plain `fl check` stays local and needs no network" (decision 6); "Before any gate or adapter runs, every decision in mode B checks, cheapest first: visibility (live, every decision); the ruleset's presence, which sets the mode (§6); the branch; the head descends from the anchor and from the last head seen (§3.5); and `ensure_publishable(project, None)`" (§2.4); "A failure refuses the decision before anything runs or is spent" (§2.4); "Every error exits 2 with `error: …` and says what to do — except the attempt row below, which is a warning (decision 14)" (§7); decision 15: "fl prints the attempt's outcome first, reports the failed save as an error message, and exits 0 or 1 as the attempt did"; decision 16: "`--by` and `--reason` are text the person writes to publish; the command says they are published permanently, then appends".
- Spec invariants, verbatim: "the local store keeps every run"; "`gate_runs(gate)`: if GitHub cannot be read, an ERROR. Unreachable is not empty"; "The flush — the ledger commit — comes before the state change it supports"; "Every core feature must work on GitHub Free" (decision 12).
- Every existing configuration keeps working: a project with no `ledger` key behaves exactly as before — mode A, ledger in the local store — and so does a project with no tracker binding. Every existing test passes, except the assertions a task changes on purpose (named in that task).
- `snake_case` on every wire (`crates/core/src/wire.rs`).
- **Every guard gets a mutation check**: revert it, watch the named test go red, restore. Each task's mutation step lists EVERY guard the task adds or moves — a guard missing from that list is a plan defect, so a reviewer checks the list against the diff.
- **A test never asserts with a substring another code path also produces.** Where two messages share words (`fl github ledger init` appears in `NotSetUp`, `NoAnchor` and the cut-over refusal), assert a phrase only the intended path writes; each task names it.
- **No plan names, task numbers or review labels in code comments** ("Task 9", "B2", "review finding 4", "Ruling:"). Cite the spec by section, or say the reason.
- **A *Modelled* marker always names its live test**: "*Modelled* — confirmed by live test `<name>`". Every such name is listed in B2b's scope above.
- **A change to shared plumbing states its blast radius** — the readers and writers it reaches — in the task that makes it.
- Line numbers cite `39eb098`. An earlier task's edits shift the lines a later task names: find the named item, not the number.
- Commits are authored by the machine account (`WORKFLOW.md`) and end with `Co-authored-by: Ferris <Ferris@artificialhumanity.io>`. Stage explicit paths — never `git add -A` — and after each commit run `git status --porcelain`, which must print nothing: the trio runs on the working tree, so a file a task changed but did not stage stays green locally and leaves the commit unbuildable.
- The repository is public: no machine paths, host names or lab names in code, tests or messages. Test repositories are `acme/widgets` (and `acme/other` for a record another repository owns).

## Review Focus

1. **A machine that never imported the manifest runs `init` on a repository whose ledger was deleted.** It must refuse as deleted — never create a second ledger that hides the deletion. Task 8 (`a_machine_that_never_imported_the_manifest_does_not_create_a_second_ledger_where_one_was_deleted`).
2. **A paid attempt whose publish, or whose local save, fails.** The exit code is the attempt's own (0 or 1), never 2, which scripts retry on; the outcome is printed first. Task 12 (`an_attempt_whose_publish_fails_warns_keeps_its_exit_code_and_rides_with_the_next`, `an_attempt_that_cannot_be_saved_keeps_its_own_exit_code_and_is_not_published`).
3. **A second machine that imported the manifest but never ran `init`.** Every decision there is refused, naming `init`, before anything runs — never a decision that quietly publishes nothing. Task 11 (`a_machine_with_no_cut_over_is_refused_naming_init`).
4. **`fl stats` while GitHub is down, under `--db`, or after the `ledger` key is removed.** The count says it covers the local store only and why — never a short count that reads as the total. Task 13 (`stats_when_github_cannot_be_reached_covers_the_local_store_and_says_so`, `stats_under_db_says_it_covers_the_local_store_only`, `stats_without_the_ledger_key_on_a_store_that_records_one_says_so`).
5. **Quarantine on a repository that is not private.** `--by` and `--reason` are published for good; the command warns before appending, and says they are permanent on a private one too. Task 10 (`quarantine_on_a_repository_that_is_not_private_warns_then_appends`, `quarantine_on_a_private_repository_says_the_text_is_permanent_without_a_warning`).

## Rulings this plan makes

The spec is silent or ambiguous on these. Each says why, and what it costs if wrong.

1. **B2 splits into B2a (this plan) and B2b (comments, recovery, live tests, docs).** B2a binds `SplitLedger` with no decision comments: the ledger commit is the evidence and a comment is its view (§4.1, §4.3). *If wrong:* a release cut from B2a alone shows nothing on issues.
2. **`init`'s confirmation is a flag, `--confirm <commit>`, not an interactive prompt** (§6.1 step 6). The flag binds exactly the commit the person was shown, so a "yes" typed later cannot adopt a different one, and a script or CI never hangs on a read. *If wrong:* the person types one more line.
3. **A `Confirm` outcome exits 1**: nothing was recorded and the ledger is not set up on this machine, but nothing was refused either. *If wrong:* a script reading 1 as failure retries `init`, which asks again.
4. **`init` imports the manifest only where the store does not author the project.** An authoring store wrote the manifest's `ledger_root` itself, from its own store, and `import_manifest` refuses an authoring store. No manifest file: nothing to learn, `init` proceeds. *If wrong:* an authoring store that lost its root table could create a second ledger after a deletion.
5. **`init` never writes the manifest.** It tells the person to run `fl manifest export` and commit, as §6.1 step 4 says the person commits (spec defect 8). *If wrong:* one more command for the person.
6. **`init`, `verify` and `quarantine` all require `ledger = "github"`**, with one refusal naming the key. *If wrong:* `verify` on a repository whose binding lost the key is refused when it could have run.
7. **The pre-flight runs once per decision command, in §2.4's order, with no cache of its own.** Each command makes one decision; `GithubLedger` already reads visibility once (B1 ruling 21); the append re-reads the head as §3.2 step 1 requires. *If wrong:* a future command that makes two decisions repeats the pre-flight's requests.
8. **The pre-flight adds one check §2.4 does not list: this machine's cut-over** (§6.1 step 5 says "until then the pre-flight refuses, naming `init`"), after the branch checks and before `ensure_publishable`. *If wrong:* none; it is the spec's own refusal placed in order.
9. **Every move in mode B pre-flights, an ungated one included**: it flushes a decision (§2.2). *If wrong:* an ungated move needs GitHub and a committed manifest when it could have done without.
10. **A rules answer the pre-flight cannot read refuses the decision** (§2.4: "A failure refuses"); detection-only never does (decision 12). Measured on 2026-10-02: a private repository on GitHub Free answers `rules/branches/fl/ledger` with `200 []`, which `mode()` reads as detection-only — so Free refuses nothing, and a failed read is a real failure, not Free's normal answer. The 403 "Upgrade to GitHub" arm stays only as a defensive path for a plan that refuses outright; no live answer has shown it. *If wrong:* a plan that answers some third shape is refused until `mode()` learns it.
11. **`fl stats` reads GitHub only when no `--db`/`$FL_DB` is given, the store it runs on is this directory's, and the binding names the GitHub ledger.** It falls back to the local store, saying why, when GitHub cannot be reached (transient, as B1 ruling 5), under `--db`, on another project's store, or when the store records a ledger root the binding no longer names. A ledger that is not set up is an error naming `init`, not a fallback: it is lasting, not "cannot be read". *If wrong:* `fl stats` on a project whose ledger was never set up exits 2 instead of counting locally.
12. **Decision 15: an attempt whose local save failed is not flushed.** A decision cannot rest on an entry the store does not hold (`RestsOnLocalEntry`); publishing it from memory would put on GitHub what this machine cannot show. The command prints the outcome, then `error: …`, and exits with the attempt's code. Tested through a ledger double: redb cannot be made to fail an append from outside the process without failing the open first. *If wrong:* an attempt lost locally is also missing from GitHub.
13. **Requirement 6: `verify` checks one id on two different lines across every directory at the head** (skipping quarantined and unreadable lines, as readers do). The local-versus-GitHub half of §3.5 check 5 is left where it is, and its limit is documented (B1 requirement 6 allows that; B2b's `docs/github-ledger.md` states it): a merged read (`SplitLedger::merge` — runs and attempts) catches a GitHub copy whose content differs from the local entry, but decisions are never merge-read, and the append's de-duplication (`append.rs:157-166`) treats any id already present as published whatever its content. *If wrong:* `verify` downloads every segment at the head once — about one request per segment more; and a differing GitHub copy of a decision goes unreported until someone compares it.
14. **Requirement 8: `verify` stops after 100,000 commits (`--max-commits`) with a refusal that names the flag, reports progress every 100 commits on stderr; `compare` asks `?per_page=1`; `mode()` reads every page with `?per_page=100`.** *If wrong:* a real ledger past 100,000 commits needs the flag.
15. **Requirement 2: `LedgerCache::set_last_head` and `cache` are removed, not made test-only.** Tests write through `remember`, so no code path — test or production — can move the head without its segments. *If wrong:* tests read a little longer.
16. **The `ledger` value is part of the tracker binding's identity.** Two config entries naming one store and one repository, one with `ledger = "github"` and one without, are refused as two trackers, and the refusal names which has the GitHub ledger. *If wrong:* such a config, today accepted only by accident, now needs one entry fixed.
17. **A cause is wrapped as a clause** (`fl_core::as_clause` strips one final period), so a message around it ends each sentence once, and none says "above" for a cause on the same line (requirement 11). *If wrong:* message text.
18. **`init` refuses a branch under `fl/ledger/`** (requirement 3), found through `git/matching-refs/heads/fl/ledger/`, before it creates anything. *Modelled* — confirmed by live test `a_branch_under_the_ledger_branch_is_found`. *If wrong:* `init` fails later with GitHub's 422 and leaves an orphan tree and commit.
19. **Notes a read leaves (`GithubLedger::take_notes`) are printed on stderr as `note: …` after the command, whether it succeeded or not.** *If wrong:* a refused command also shows what its reads skipped — which is still true.
20. **`fl github whoami` prints `ledger\tlocal`, or `ledger\tgithub` and `mode\t<name>` (with what is missing after a tab for detection-only).** A mode that cannot be read is an error, exit 2.
21. **`init` reads the mode before it creates or records anything.** Once the root is recorded, nothing left in `init` can fail, so a failed rules read leaves no branch and a re-run starts over — the "export and commit the manifest" instruction is never skipped. *If wrong:* a `Confirm` outcome also costs one rules read.
22. **The pre-flight refuses a committed manifest that does not carry the ledger's first commit this store records** (spec §7 row "`ledger_root` missing from the manifest"), on every store. Without it a machine decides freely while every other machine fails with `NoAnchor`; with it, the person is told at the first decision even if they missed `init`'s instruction. The refusal names what can be done where it runs: on the store that authors the project, `fl manifest export` there and a commit; on any other store — whose root came from an earlier manifest or from `init --confirm` — the export and commit on the authoring machine, then a pull and `fl manifest import` here (`export_manifest` refuses a store that does not author the project). *If wrong:* one more manifest read per decision.
23. **On a ledger already set up, `init` says so, records this machine's cut-over if it had none, prints the mode, and stops** (spec §6.1 step 6): the guidance (steps 7–9) prints only when this run created or adopted the root. *If wrong:* a person on a second machine sees no guidance; the first machine's run printed it.
24. **A missing manifest is refused naming the command that writes it**: `fl manifest export` on the store that authors the project, and on any other a restore from git and `fl manifest import`. The two store-aware callers of the manifest read (`ensure_publishable` for an authoring store, `ensure_import_current` for an importing one) each pass which they are, so the wording needs no extra lookup. *If wrong:* message text.

## Spec defects this plan found

1. **§2.4 versus decision 12 — closed.** The pre-flight reads the ruleset, and "A failure refuses the decision", while decision 12 makes the mode non-critical on Free. Measured on 2026-10-02 with a read-only GET as the machine account: the private Free-plan repository `fl-live-test` answers `rules/branches/fl/ledger` (and `…/main`) with `HTTP 200` and `[]`, which `mode()` reads as detection-only. Nothing is refused on Free; ruling 10.
2. **§2.4 "cached for the life of one command"** reads as a cache the pre-flight keeps, but each command makes one decision and §3.2 step 1 re-reads the head on every append anyway; only visibility (§5) needs reading once. Ruling 7.
3. **§6.1 step 6 "asks the person to confirm"** names no mechanism. Ruling 2.
4. **§6.1 step 1** says only `init` refuses without `ledger = "github"`; §3.5/§3.6 are silent for `verify` and `quarantine`. Ruling 6.
5. **§2.5 and §2.6 on `fl stats`** cover "GitHub cannot be read, or under `--db`", but not a project reached through another store's IRI, a store whose binding lost the key, or a ledger never set up. Ruling 11.
6. **§6.1 step 2** names only a branch called `fl`; git equally cannot create `fl/ledger` while `fl/ledger/<x>` exists (requirement 3). Ruling 18.
7. **Decision 15** says nothing of the flush after a failed save; one cannot rest on the unsaved attempt. Ruling 12.
8. **§7's row "`ledger_root` missing from the manifest — commit the manifest from `init`"** implies `init` writes the manifest; §6.1 step 4 says the person commits it and nothing says `init` exports. Ruling 5.
9. **§3.5 says fl checks all seven on every read and append**, but check 5 across directories needs every directory read; readers cannot, so only `verify` does (ruling 13).

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `Cargo.toml` | modify | a comment: `serde_json` must never gain `preserve_order` |
| `crates/github/src/ledger/layout.rs` | modify (tests) | golden bytes of every line kind; the key-order guard |
| `crates/core/src/split.rs` | modify | `LedgerCache`: `remember` is the only write |
| `crates/core/src/mem.rs`, `crates/store/src/lib.rs` | modify | drop the two removed writes |
| `crates/core/src/conformance.rs` | modify | the `ledger-cache` suite writes through `remember` |
| `crates/github/src/ledger/read.rs`, `init.rs` | modify (tests); `read.rs` doc | tests write through `remember`; the pre-flight doc |
| `crates/github/src/ledger/git.rs` | modify | `tree_files` fails closed; `compare` asks one commit per page; `branches_under` |
| `crates/github/src/ledger/init.rs` | modify | `mode()` reads every page; `init` refuses a branch under `fl/ledger/` |
| `crates/github/src/client.rs` | modify (tests) | `graphql_write`'s 401 is a credential error |
| `crates/github/src/fake.rs` | modify | `page` shared with `fake_git`; `fail_repo_read_after` |
| `crates/github/src/fake_git.rs` | modify | rules paged; `git/matching-refs` |
| `crates/github/src/ledger/verify.rs`, `mod.rs` | modify | `verify_with`: a cap, progress, one id on two lines |
| `crates/core/src/store.rs`, `lib.rs` | modify | `as_clause` |
| `crates/exec/src/population.rs` | modify | refusals around a clause |
| `crates/cli/src/config.rs` | modify | `ledger = "github"` |
| `crates/cli/src/main.rs` | modify | the ledger binding, notes, `stats` source; comment fixes |
| `crates/cli/src/ctx.rs` | modify | `Ctx::github_ledger` |
| `crates/cli/src/preflight.rs` | create | §2.4 before every decision |
| `crates/cli/src/cmd/ledger.rs` | create | `fl github ledger init | verify | quarantine` |
| `crates/cli/src/cmd/mod.rs`, `github.rs` | modify | the `ledger` subcommand; `whoami` states the ledger and mode |
| `crates/cli/src/cmd/manifest.rs` | modify | `import_before_ledger_init`, `print_import` |
| `crates/cli/src/cmd/record.rs`, `check.rs`, `finding.rs`, `attempt.rs` | modify | pre-flight call sites; wording; decisions 14–15 |
| `crates/cli/src/cmd/stats.rs` | modify | `Source` |
| `crates/cli/src/testing.rs` | modify | `Flushes::failing_append` |
| `crates/cli/tests/ledger.rs` | create | the black-box battery for mode B |
| `docs/github-tracker.md` | modify | what `whoami` prints |

---

### Task 1: A published line's bytes are pinned

B1 requirement 1. `decode` accepts a line only when re-encoding it gives the same bytes (B1 ruling 11), and `encode` goes through `serde_json::Value`, whose object is a `BTreeMap` — keys sorted — only while `serde_json`'s `preserve_order` feature is off. Cargo unifies features across the workspace, so one dependency turning it on would make every line already published unreadable.

**Files:**
- Modify: `crates/github/src/ledger/layout.rs` (tests module, after `each_entry_is_filed_under_its_gate_project_finding_or_record`, line 416)
- Modify: `Cargo.toml:15` (comment above `serde_json`)

**Interfaces:**
- Consumes: `Line`, `Line::encode`, `decode`, `Area`, `QuarantineLine` (`layout.rs`); `disclose::attempt`, `Visibility` (`disclose.rs`).
- Produces: nothing new; two pinning tests.

- [ ] **Step 1: Write the tests**

In `crates/github/src/ledger/layout.rs`, inside `mod tests`, after `each_entry_is_filed_under_its_gate_project_finding_or_record`, add:

```rust
    // ⚠ The bytes every machine reads back. `decode` accepts a line only
    // when it is byte-for-byte what `encode` writes, so a change to any of
    // these strings makes every line already published unreadable: it is a
    // new format (spec §3.1), never a fix.
    #[test]
    fn each_line_kind_is_written_as_exactly_these_bytes() {
        use crate::ledger::disclose::{self, Visibility};
        use fl_core::decision::TransitionOutcome;
        use fl_core::log::AttemptStatus;
        use fl_core::verdict::Verdict;
        let at = |s: &str| At::parse(s).unwrap();

        let run = GateRun {
            id: Some(seq_iri(9)),
            at: Some(at("2026-10-02T00:00:00.000Z")),
            gate: GateId(seq_iri(1)),
            record: Some(record()),
            commit: "abc".into(),
            verdict: Verdict::from_predicate(true, 3),
            population: 3,
            output_excerpt: Some("ok".into()),
            duration_ms: 5,
            cost_usd_micros: 0,
        };
        let run_line = concat!(
            r#"{"at":"2026-10-02T00:00:00.000Z","by":"fake-user","commit":"abc","#,
            r#""cost_usd_micros":0,"duration_ms":5,"#,
            r#""gate":"urn:uuid:00000000-0000-7000-8000-000000000001","#,
            r#""id":"urn:uuid:00000000-0000-7000-8000-000000000009","output_excerpt":"ok","#,
            r#""population":3,"record":"https://github.com/acme/widgets/issues/1","#,
            r#""verdict":{"pass":{"population":3}}}"#,
        );
        assert_eq!(Line::Run(run.clone()).encode("fake-user"), run_line);
        assert_eq!(
            decode(Area::Runs, run_line).unwrap(),
            (Line::Run(run), "fake-user".to_string())
        );

        let attempt = Attempt {
            id: Some(seq_iri(10)),
            at: Some(at("2026-10-02T00:00:01.000Z")),
            project: ProjectId(seq_iri(2)),
            record: record(),
            adapter: "claude".into(),
            status: AttemptStatus::Completed,
            duration_ms: 7,
            tokens_in: 11,
            tokens_out: 13,
            cost_usd_micros: 17,
            paths_touched: PathsTouched::Listed(vec!["src/a.rs".into()]),
            output_excerpt: Some("done".into()),
        };
        let attempt_line = concat!(
            r#"{"adapter":"claude","at":"2026-10-02T00:00:01.000Z","by":"fake-user","#,
            r#""cost_usd_micros":17,"duration_ms":7,"#,
            r#""id":"urn:uuid:00000000-0000-7000-8000-00000000000a","#,
            r#""output_excerpt":"done","paths_touched":["src/a.rs"],"#,
            r#""project":"urn:uuid:00000000-0000-7000-8000-000000000002","#,
            r#""record":"https://github.com/acme/widgets/issues/1","status":"completed","#,
            r#""tokens_in":11,"tokens_out":13}"#,
        );
        assert_eq!(
            Line::Attempt(attempt.clone()).encode("fake-user"),
            attempt_line
        );
        assert_eq!(
            decode(Area::Attempts, attempt_line).unwrap(),
            (Line::Attempt(attempt.clone()), "fake-user".to_string())
        );

        // Decision 2's projection: `null` for the excerpt, a count for the
        // paths.
        let withheld = disclose::attempt(&attempt, Visibility::NotPrivate);
        let withheld_line = concat!(
            r#"{"adapter":"claude","at":"2026-10-02T00:00:01.000Z","by":"fake-user","#,
            r#""cost_usd_micros":17,"duration_ms":7,"#,
            r#""id":"urn:uuid:00000000-0000-7000-8000-00000000000a","#,
            r#""output_excerpt":null,"paths_touched":1,"#,
            r#""project":"urn:uuid:00000000-0000-7000-8000-000000000002","#,
            r#""record":"https://github.com/acme/widgets/issues/1","status":"completed","#,
            r#""tokens_in":11,"tokens_out":13}"#,
        );
        assert_eq!(
            Line::Attempt(withheld.clone()).encode("fake-user"),
            withheld_line
        );
        assert_eq!(
            decode(Area::Attempts, withheld_line).unwrap(),
            (Line::Attempt(withheld), "fake-user".to_string())
        );

        let decision = Decision {
            id: seq_iri(11),
            at: at("2026-10-02T00:00:02.000Z"),
            record: record(),
            finding: None,
            outcome: Outcome::Move {
                from: State::Review,
                to: State::Done,
                transitions: vec![TransitionOutcome {
                    transition: "launch".into(),
                    passed: true,
                }],
                allowed: true,
            },
            rests_on: vec![seq_iri(9)],
        };
        let decision_line = concat!(
            r#"{"at":"2026-10-02T00:00:02.000Z","by":"fake-user","finding":null,"#,
            r#""id":"urn:uuid:00000000-0000-7000-8000-00000000000b","#,
            r#""outcome":{"move":{"allowed":true,"from":"review","to":"done","#,
            r#""transitions":[{"passed":true,"transition":"launch"}]}},"#,
            r#""record":"https://github.com/acme/widgets/issues/1","#,
            r#""rests_on":["urn:uuid:00000000-0000-7000-8000-000000000009"]}"#,
        );
        assert_eq!(
            Line::Decision(decision.clone()).encode("fake-user"),
            decision_line
        );
        assert_eq!(
            decode(Area::Decisions, decision_line).unwrap(),
            (Line::Decision(decision), "fake-user".to_string())
        );

        // A quarantine line is a struct, encoded in field order.
        let q = QuarantineLine {
            id: seq_iri(12),
            at: at("2026-10-02T00:00:03.000Z"),
            file: "runs/0123456789abcdef0123456789abcdef/1.jsonl".into(),
            line: 2,
            quarantined_by: "maintainer".into(),
            reason: "a hand edit".into(),
            by: "fake-user".into(),
        };
        let q_line = concat!(
            r#"{"id":"urn:uuid:00000000-0000-7000-8000-00000000000c","#,
            r#""at":"2026-10-02T00:00:03.000Z","#,
            r#""file":"runs/0123456789abcdef0123456789abcdef/1.jsonl","line":2,"#,
            r#""quarantined_by":"maintainer","reason":"a hand edit","by":"fake-user"}"#,
        );
        assert_eq!(q.encode(), q_line);
        assert_eq!(QuarantineLine::decode(q_line).unwrap(), q);
    }

    // ⚠ `serde_json`'s `preserve_order` feature must stay off. With it, a
    // `Value` keeps keys in insertion order: every line would encode in
    // field order instead of sorted order, and every line already published
    // would fail `decode`'s byte-for-byte check. Cargo unifies features
    // across the workspace, so any dependency that turns it on turns this
    // test red.
    #[test]
    fn json_objects_encode_with_their_keys_sorted() {
        let v: Value = serde_json::from_str(r#"{"b":1,"a":2}"#).unwrap();
        assert_eq!(v.to_string(), r#"{"a":2,"b":1}"#);
    }
```

`record()` is the module's helper (`https://github.com/acme/widgets/issues/1`); `GateRun`, `Attempt`, `Decision`, `At`, `Value` come from the module's own imports through `use super::*`; `GateId`, `ProjectId`, `seq_iri`, `Outcome`, `PathsTouched`, `State` are already imported by the tests module.

- [ ] **Step 2: Run them — they pin today's bytes**

Run: `cargo test -p fl-github --lib -- each_line_kind_is_written_as_exactly_these_bytes json_objects_encode_with_their_keys_sorted`
Expected: PASS. These tests pin bytes the encoder writes today, so they pass at once. The literals were derived by hand from the types (keys of a `Value` sorted; a `QuarantineLine` in field order). If one fails, do NOT change the encoder: run with `-- --nocapture`, compare the two strings key by key, and correct the literal only where it misstates what the encoder writes at `39eb098`.

- [ ] **Step 3: Document the constraint where the dependency is declared**

In `Cargo.toml`, replace line 15 (`serde_json = "1.0.151"`) with:

```toml
# ⚠ Never enable `preserve_order`: the GitHub ledger reads a line only when
# re-encoding it gives the same bytes, which needs a `Value`'s keys sorted.
# `ledger::layout::tests::json_objects_encode_with_their_keys_sorted` guards it.
serde_json = "1.0.151"
```

- [ ] **Step 4: Mutation checks**

Guards in this task, each reverted, run, seen red, restored:

1. The key order: change line 15 of `Cargo.toml` to `serde_json = { version = "1.0.151", features = ["preserve_order"] }`, run `cargo test -p fl-github --lib ledger::layout` → `json_objects_encode_with_their_keys_sorted` and `each_line_kind_is_written_as_exactly_these_bytes` red. Restore with `git checkout -- Cargo.toml Cargo.lock`, then re-apply Step 3's comment.
2. `Line::encode` writes `by`: change `"by"` to `"writer"` in `Line::encode` → `each_line_kind_is_written_as_exactly_these_bytes` red.
3. Decision 2's projection reaches the bytes: in `disclose::attempt`, drop `out.output_excerpt = None;` → the `withheld_line` assertion red.

- [ ] **Step 5: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add Cargo.toml crates/github/src/ledger/layout.rs
git commit -m "test(github): pin the bytes of every ledger line kind, and sorted JSON keys

Golden strings for a run, an attempt (whole and as decision 2 publishes
it), a decision and a quarantine line, each decoded back; a guard that
serde_json's preserve_order stays off, since a feature turned on anywhere
in the workspace would make every published line unreadable. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 2: The ledger cache has one write

B1 requirement 2. `LedgerCache::set_last_head` and `LedgerCache::cache` let a caller move the last head without the segments that head was checked with, or cache a segment no recorded head confirmed — the non-atomic pattern `remember` replaced. No production code calls either today (`grep -rn "set_last_head\|\.cache(" crates`: only the trait, its two implementations, one forwarding double and tests). Remove both; tests write through `remember`.

**Blast radius:** writers — `GithubLedger::check_format`, `snapshot` and `append` already write only through `remember` (`read.rs:146`, `:202`; `append.rs:210`); readers (`last_head`, `cached`, `cached_under`) are unchanged. Every implementation of `LedgerCache` (`MemStore`, `RedbStore`, the `NoCutover` double in `init.rs` tests) loses two methods; the stored tables are unchanged.

**Files:**
- Modify: `crates/core/src/split.rs:137-168` (`LedgerCache`), `:190-192` (a "(Task 9)" label in `owns_record`'s doc)
- Modify: `crates/core/src/mem.rs:489-495`, `:522-528`
- Modify: `crates/store/src/lib.rs:1318-1325`, `:1368-1376`, test `the_ledger_cache_survives_a_reopen` (`:2112-2131`)
- Modify: `crates/core/src/conformance.rs:268-274`, `:283-291`, `:330-331`, `:365-369`, `:455-467`
- Modify: `crates/github/src/ledger/init.rs:751-753`, `:764-766` (the `NoCutover` double)
- Modify: `crates/github/src/ledger/read.rs` tests at `:754`, `:855`, `:864`, `:1099`, `:1747`

**Interfaces:**
- Consumes: `LedgerCache::remember` (B1).
- Produces:

```rust
// fl_core::split
pub trait LedgerCache {
    fn last_head(&self, repo: &str) -> Result<Option<String>, StoreError>;
    fn cached(&self, repo: &str, path: &str) -> Result<Option<CachedSegment>, StoreError>;
    fn cached_under(&self, repo: &str, dir: &str)
        -> Result<Vec<(String, CachedSegment)>, StoreError>;
    fn remember(&self, repo: &str, head: &str, segments: &[(String, CachedSegment)])
        -> Result<(), StoreError>;   // the ONLY write
}
```

- [ ] **Step 1: Rewrite the tests that used the two writes**

In `crates/core/src/conformance.rs`, replace `the_last_head_is_kept_per_repository_and_replaced` (lines 268-274) with:

```rust
fn the_last_head_is_kept_per_repository_and_replaced<S: LedgerCache>(s: &S) {
    assert_eq!(s.last_head("R_1").unwrap(), None);
    s.remember("R_1", "c1", &[]).unwrap();
    s.remember("R_1", "c2", &[]).unwrap();
    assert_eq!(s.last_head("R_1").unwrap().as_deref(), Some("c2"));
    assert_eq!(s.last_head("R_2").unwrap(), None);
}

/// Caches `segment` at `path` in `repo`, as a read that validated it at a
/// head `h` does: through `remember`, the only write.
fn put<S: LedgerCache>(s: &S, repo: &str, path: &str, segment: CachedSegment) {
    s.remember(repo, "h", &[(path.to_string(), segment)])
        .unwrap();
}
```

In `a_cached_file_reads_back_by_path_and_by_its_own_directory_only`, replace lines 284-291 (the five `s.cache(…)` calls) with:

```rust
    put(s, "R_1", "runs/aa/1.jsonl", seg("o1", true));
    put(s, "R_1", "runs/aa/2.jsonl", seg("o2", false));
    put(s, "R_1", "runs/aab/1.jsonl", seg("o3", false));
    put(s, "R_2", "runs/aa/1.jsonl", seg("o4", false));
    put(s, "R_1", "runs/aa/2.jsonl", seg("o5", true));
```

In `a_segment_at_the_same_path_in_another_repository_is_never_returned`, replace lines 330-331 with:

```rust
    put(s, "R_1", "runs/aa/1.jsonl", seg("o1"));
    put(s, "R_2", "runs/aa/1.jsonl", seg("o2"));
```

In `cached_under_scans_from_the_directory_not_from_the_start_of_the_repository`, replace lines 365-369 with:

```rust
    put(s, "R_1", "format", seg("meta"));
    put(s, "R_1", "runs/a9/1.jsonl", seg("a9"));
    put(s, "R_1", "runs/aa/1.jsonl", seg("r1a"));
    put(s, "R_2", "runs/aa/1.jsonl", seg("r2a"));
    put(s, "R_2", "runs/aa/2.jsonl", seg("r2b"));
```

In `a_cached_file_keeps_bytes_that_are_not_utf8_exactly`, replace the doc's first two lines (`/// ⚠ A file is cached as the bytes GitHub sent, exactly — through \`cache\`` / `/// and through \`remember\` alike. …`) with:

```rust
/// ⚠ A file is cached as the bytes GitHub sent, exactly — through every
/// `remember`, whatever head it names. A store that keeps text would have to
```

and replace `s.cache("R_1", "runs/aa/1.jsonl", &one).unwrap();` (line 467) with:

```rust
    put(s, "R_1", "runs/aa/1.jsonl", one.clone());
```

In `crates/store/src/lib.rs`, test `the_ledger_cache_survives_a_reopen`, replace

```rust
            s.set_last_head("R_1", "c1").unwrap();
            s.cache("R_1", "runs/aa/1.jsonl", &seg).unwrap();
```

with

```rust
            s.remember("R_1", "c1", &[("runs/aa/1.jsonl".to_string(), seg.clone())])
                .unwrap();
```

In `crates/github/src/ledger/read.rs` tests, replace each of the five lines `local.set_last_head("R_1", &mine).unwrap();` (lines 754, 855, 1099, 1747) and `local.set_last_head("R_1", &theirs).unwrap();` (line 864) with the same call through `remember`:

```rust
        local.remember("R_1", &mine, &[]).unwrap();
```

```rust
        local.remember("R_1", &theirs, &[]).unwrap();
```

- [ ] **Step 2: Remove the two writes**

In `crates/core/src/split.rs`, replace the `LedgerCache` trait (lines 137-168) with:

```rust
/// What this machine remembers of each repository's GitHub ledger, keyed
/// by the repository's `node_id` (spec §3.2 step 6, §3.3): the last head it
/// checked, and every file it read — so a closed segment is downloaded
/// once, and an altered one is caught.
///
/// ⚠ `remember` is the ONLY write: the last head and the segments checked
/// at it always move together, so no code can record a head without the
/// files it was checked with, or cache a file no recorded head confirmed.
pub trait LedgerCache {
    fn last_head(&self, repo: &str) -> Result<Option<String>, StoreError>;
    fn cached(&self, repo: &str, path: &str) -> Result<Option<CachedSegment>, StoreError>;
    /// Every file cached under the directory `dir` (such as `runs/<key>`),
    /// with its path, in path order. Not a directory whose name merely
    /// starts with `dir`.
    fn cached_under(
        &self,
        repo: &str,
        dir: &str,
    ) -> Result<Vec<(String, CachedSegment)>, StoreError>;
    /// Commits `head` as the last seen, and every `(path, segment)` a read
    /// validated, together in ONE write (GitHub ledger spec §3.5 checks 3
    /// and 4); a segment already cached at a path is replaced. A read that
    /// fails partway must leave neither applied: a segment cached from a
    /// read that never finished, while the last head stayed behind it,
    /// would hold a position (open or closed) or a content a later read AT
    /// THAT SAME, unmoved head never itself confirmed — raising a false
    /// alarm the next time that head is read.
    fn remember(
        &self,
        repo: &str,
        head: &str,
        segments: &[(String, CachedSegment)],
    ) -> Result<(), StoreError>;
}
```

In the same file, in `RemoteLedger::owns_record`'s doc (line 192), replace `/// with \`fl_github::owner::issue_of_repository\` (Task 9).` with:

```rust
    /// with `fl_github::owner::issue_of_repository`.
```

In `crates/core/src/mem.rs`, delete `fn set_last_head` (lines 489-495) and `fn cache` (lines 522-528) from `impl LedgerCache for MemStore`.

In `crates/store/src/lib.rs`, delete `fn set_last_head` (lines 1318-1325) and `fn cache` (lines 1368-1376) from `impl LedgerCache for RedbStore`.

In `crates/github/src/ledger/init.rs`, in `impl LedgerCache for NoCutover<'_>`, delete the forwarding `fn set_last_head` (lines 751-753) and `fn cache` (lines 764-766).

- [ ] **Step 3: Run the tests**

Run: `cargo test -p fl-core -p fl-store -p fl-github`
Expected: PASS — the `ledger-cache` conformance suite over both stores, and every read test.

- [ ] **Step 4: The guard is the type system**

There is no runtime guard to mutate: the two writes no longer exist. Confirm: `grep -rn "set_last_head\|\.cache(" crates --include='*.rs'` prints nothing. Then add `local.set_last_head("R_1", "x").unwrap();` to any read.rs test and run `cargo test -p fl-github --no-run` → compile error `no method named set_last_head`; remove it.

- [ ] **Step 5: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/split.rs crates/core/src/mem.rs crates/core/src/conformance.rs crates/store/src/lib.rs crates/github/src/ledger/init.rs crates/github/src/ledger/read.rs
git commit -m "refactor(core): remember is the ledger cache's only write

set_last_head and cache let a caller move the last head without the
segments it was checked with, or cache a segment no recorded head
confirmed. No production code called them; tests now write through
remember, so no path can split the two again.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 3: Ledger reads fail closed, and ask GitHub for less

B1 requirements 8 (compare with `?per_page=1`; paginate rules in `mode()`), 9 (`tree_files` fails closed) and 10 (`graphql_write`'s 401 is `StoreError::Credential`).

**Blast radius:** `compare` has one caller, `check_head`, which every read, every append, `init` on an existing ledger and (Task 11) the pre-flight go through — only the query string changes; `status` is read as before. `mode()` is called by `init`'s guidance (Task 8), `whoami` (Task 8) and the pre-flight (Task 11); a page that fails is still an error, a private repository on Free (`200 []`, measured) is detection-only, and the defensive 403 still is. The detection-only `why` names the plan as one possible cause. `tree_files` is read only by `verify`. The fake's `rules` route now pages like every other list; its existing tests ask for no `per_page` and get every rule on one page (30 by default).

**Files:**
- Modify: `crates/github/src/ledger/git.rs:144-173` (`compare`), `:330-341` (`tree_files`'s loop), tests
- Modify: `crates/github/src/ledger/init.rs:251-315` (`mode`), tests
- Modify: `crates/github/src/fake.rs:655` (`page` becomes `pub(crate)`), `:173-175` (`rules_need_upgrade`'s doc)
- Modify: `crates/github/src/fake_git.rs:294` (route), `:488-524` (`rules`), tests at `:1068-1070`
- Modify: `crates/github/src/client.rs` tests (after `a_graphql_write_hands_every_refusal_to_the_caller`)

**Interfaces:**
- Consumes: `Client::get_all` (`client.rs:162`), `State::page` (`fake.rs:655`).
- Produces: no new names. `pub(crate) fn page(&self, path: &str, q: &BTreeMap<String, String>, items: Vec<Value>) -> Answer` on the fake's `State`.

- [ ] **Step 1: Make the fake page its rules and state the measured answer, then write the failing tests**

The fake must page `rules/branches` first, or the pagination test below passes against today's single-read `mode()` (the fake would hand it every rule at once).

In `crates/github/src/fake.rs`, line 655, change `fn page(` to `pub(crate) fn page(`, and replace the doc of `rules_need_upgrade` (lines 173-175) with:

```rust
    /// `rules/branches` answers 403 with an "Upgrade to GitHub" message.
    /// ⚠ Defensive only: no live answer has shown it. Measured on
    /// 2026-10-02, a private repository on GitHub Free answers `200 []` —
    /// the fake's default, with no ruleset — confirmed by live test
    /// `a_private_repository_without_a_ruleset_is_detection_only`.
```

In `crates/github/src/fake_git.rs`, change the route at line 294 to

```rust
        ("GET", ["rules", "branches", name @ ..]) => rules(s, &full, &name.join("/"), q),
```

replace the doc and signature of `rules` (lines 488-493) with

```rust
/// ⚠ Modelled: `GET /rules/branches/{branch}` lists the rules IN FORCE on
/// the branch — a disabled or evaluate-only ruleset contributes none —
/// paged like every list. Confirmed by live test
/// `rules_on_the_ledger_branch_are_readable`. With no ruleset it answers
/// `200 []`: measured on 2026-10-02 for a private repository on GitHub
/// Free, and confirmed by live test
/// `a_private_repository_without_a_ruleset_is_detection_only`. The 403
/// behind `rules_need_upgrade` is defensive and unmeasured.
fn rules(s: &mut State, full: &str, branch: &str, q: &BTreeMap<String, String>) -> Answer {
```

and its final `answer(200, Value::Array(items))` with

```rust
    s.page(&format!("/repos/{full}/rules/branches/{branch}"), q, items)
```

In the same file's tests, replace the comment above `the_rules_on_a_branch_are_those_of_active_rulesets` (lines 1068-1070) with

```rust
    // Spec §6.2 and §8.1: rules in force only; the 403 is defensive.
    // Modelled — confirmed by live test `rules_on_the_ledger_branch_are_readable`.
```

and add after that test:

```rust
    // Measured on 2026-10-02: a private repository on GitHub Free, with no
    // ruleset, answers `200 []`. Confirmed by live test
    // `a_private_repository_without_a_ruleset_is_detection_only`.
    #[test]
    fn a_branch_with_no_ruleset_answers_an_empty_list() {
        let fake = FakeGithub::start("acme/widgets");
        let r = client(&fake)
            .send(Method::Get, &format!("{REPO}/rules/branches/fl/ledger"), None)
            .unwrap();
        assert_eq!((r.status, r.body), (200, json!([])));
    }
```

Then the tests. In `crates/github/src/ledger/git.rs` tests, after `tree_files_refuses_a_200_with_no_entries_field`, add:

```rust
    // ⚠ Fail closed, as `parse_object` does: a file entry with no path or
    // no id is refused, never skipped — a verify that skipped it would pass
    // a file it never saw.
    #[test]
    fn tree_files_refuses_an_entry_with_no_path_or_no_id() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let tree_sha = fake.state().git.commits[&root].tree.clone();
        let local = MemStore::default();
        for entry in [
            json!({"mode": "100644", "type": "blob", "sha": "a".repeat(40)}),
            json!({"path": "format", "mode": "100644", "type": "blob"}),
        ] {
            let c = client(&fake);
            body_next(
                &fake,
                &format!("/git/trees/{tree_sha}"),
                200,
                json!({"sha": tree_sha, "truncated": false, "tree": [entry.clone()]}),
            );
            let err = open(&c, &local).tree_files(&tree_sha).unwrap_err();
            assert!(err.to_string().contains("no path or no id"), "{entry}: {err}");
        }
    }

    // A compare otherwise answers with up to 250 commits and every file
    // they changed; fl reads only `status`.
    #[test]
    fn a_compare_asks_for_one_commit_per_page() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).compare(&root, &root).unwrap().as_deref(),
            Some("identical")
        );
        let requests = fake.state().requests.clone();
        let asked = format!("GET /repos/acme/widgets/compare/{root}...{root}?per_page=1");
        assert!(requests.contains(&asked), "{requests:?}");
    }
```

In `crates/github/src/ledger/init.rs` tests, after `a_mode_whose_rules_are_not_a_list_is_an_error_not_detection_only`, add:

```rust
    // `rules/branches` is a list GitHub pages: a rule on a later page is in
    // force all the same.
    #[test]
    fn the_mode_reads_every_page_of_the_rules() {
        let fake = FakeGithub::start("acme/widgets");
        fake.seed_ledger();
        {
            let mut s = fake.state();
            s.rulesets = vec![Ruleset::on_ledger(
                "active",
                &["non_fast_forward", "deletion"],
            )];
            s.max_per_page = 1;
        }
        let local = MemStore::default();
        let c = client(&fake);
        assert_eq!(open(&c, &local).mode().unwrap(), Mode::Protected);
    }
```

In `crates/github/src/client.rs` tests, after `a_graphql_write_hands_every_refusal_to_the_caller`, add:

```rust
    // A refused credential is not a refusal the ledger's commit can judge:
    // it is the credential's, whatever the caller asked to judge.
    #[test]
    fn a_graphql_write_refused_for_its_credential_is_a_credential_error() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().body_next.push((
            "/graphql".into(),
            401,
            serde_json::json!({"message": "Bad credentials"}),
        ));
        let err = client(&fake)
            .graphql_write("mutation { x }", serde_json::json!({}))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Credential(ref m) if m.contains("Bad credentials")),
            "{err:?}"
        );
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-github --lib -- tree_files_refuses_an_entry_with_no_path_or_no_id a_compare_asks_for_one_commit_per_page the_mode_reads_every_page_of_the_rules a_graphql_write_refused_for_its_credential_is_a_credential_error a_branch_with_no_ruleset_answers_an_empty_list`
Expected: the first three FAIL (the entry is skipped and the call succeeds; no `?per_page=1` is asked; the fake now pages, so today's single read sees only the first rule and answers detection-only). `a_branch_with_no_ruleset_answers_an_empty_list` and the fourth PASS: it pins the `401` arm of `exchange` (`client.rs:432`) that already holds, and Step 5 mutates it.

- [ ] **Step 3: Implement**

In `crates/github/src/ledger/git.rs`, replace `compare` (its doc and body, lines 144-173) with:

```rust
    /// How `head` relates to `base` as GitHub's compare names it
    /// (`identical`, `ahead`, `behind`, `diverged`); `None` when GitHub
    /// knows one of the two commits not at all.
    ///
    /// ⚠ One commit per page: fl reads only `status`, and a compare
    /// otherwise answers with up to 250 commits and every file they changed,
    /// however far the head has moved since `base`. *Modelled* — `status`
    /// describes the whole comparison whatever the page; confirmed by live
    /// test `a_hand_edit_is_detected_and_named`.
    pub(crate) fn compare(&self, base: &str, head: &str) -> Result<Option<String>, StoreError> {
        let r = self.client.send(
            Method::Get,
            &self.path(&format!("/compare/{base}...{head}?per_page=1")),
            None,
        )?;
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
            s => Err(self.read_refused(s, "compared two commits of the ledger")),
        }
    }
```

In `tree_files`, replace

```rust
            let (Some(path), Some(oid)) = (e["path"].as_str(), e["sha"].as_str()) else {
                continue;
            };
```

with

```rust
            // ⚠ Fail closed, as `parse_object` does: a file with no path or
            // no id is refused, never skipped — a verify that skipped it
            // would pass a file it never saw.
            let (Some(path), Some(oid)) = (e["path"].as_str(), e["sha"].as_str()) else {
                return Err(backend(format!(
                    "GitHub listed an entry of ledger tree {sha} with no path or no id, so fl \
                     cannot check that commit; nothing past it was verified"
                )));
            };
```

In `crates/github/src/ledger/init.rs`, replace `mode` (its doc and body, lines 251-315) with:

```rust
    /// The mode in force (spec §6.2), from the rules GitHub applies to
    /// `fl/ledger`.
    ///
    /// ⚠ Modelled: `rules/branches` lists only the rules in force, so a
    /// disabled or evaluate-only ruleset shows as the rules missing, and it
    /// is paged like every list GitHub answers. Confirmed by live test
    /// `rules_on_the_ledger_branch_are_readable`. Measured on 2026-10-02: a
    /// private repository on GitHub Free answers `200 []` — detection-only,
    /// never a refusal (decision 12); confirmed by live test
    /// `a_private_repository_without_a_ruleset_is_detection_only`.
    ///
    /// The "Upgrade to GitHub" 403 arm is defensive: no live answer has
    /// shown it, and it is kept so a plan that refuses outright still reads
    /// as detection-only.
    pub fn mode(&self) -> Result<Mode, StoreError> {
        // ⚠ Every page (`get_all`): a rule on a later page is in force all
        // the same. A page that fails, or is not a list, is an error —
        // never read as "no rules": an unreadable answer and an absence of
        // rules are different facts, and conflating them would turn "fl
        // cannot tell what rules apply" into "no rules apply".
        let rules = match self
            .client
            .get_all(&self.path(&format!("/rules/branches/{BRANCH}?per_page=100")))
        {
            Ok(rules) => rules,
            Err(StoreError::Backend(m)) if m.contains("Upgrade to GitHub") => {
                return Ok(Mode::DetectionOnly {
                    why: "rulesets are not available on this repository's plan, so nothing \
                          stops a rewrite or a deletion; fl detects them"
                        .into(),
                });
            }
            Err(e) => return Err(e),
        };
        let in_force: BTreeSet<&str> = rules
            .iter()
            .filter_map(|x| x.get("type").and_then(Value::as_str))
            .collect();
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
                "no ruleset in force on `fl/ledger` has {} — none exists, it is disabled or \
                 only evaluating, or the plan offers none for this repository — so nothing \
                 stops a rewrite or a deletion; fl detects them",
                missing.join(" or ")
            ),
        })
    }
```

`init.rs` no longer uses `Method` for `mode`; it still does for `create_branch`, so the import stays.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github`
Expected: PASS, including `a_mode_that_cannot_be_read_is_an_error_not_detection_only` (its message now reads "GitHub answered 500 to a page of …", which still contains `answered 500`) and `a_mode_whose_rules_are_not_a_list_is_an_error_not_detection_only` (`get_all`'s "with something that is not a list").

- [ ] **Step 5: Mutation checks**

Guards in this task:

1. `tree_files` refusal: restore `continue` → `tree_files_refuses_an_entry_with_no_path_or_no_id` red.
2. `compare`'s `?per_page=1`: drop it → `a_compare_asks_for_one_commit_per_page` red.
3. `mode()` reads every page: replace `get_all` with one `send` reading only `r.body.as_array()` → `the_mode_reads_every_page_of_the_rules` red.
4. `mode()`'s defensive plan refusal: drop the `"Upgrade to GitHub"` arm → `the_mode_is_protected_only_when_both_rules_are_in_force` red (its last block).
5. `exchange`'s `401` arm before `CallerJudges`: move `401 => …` below the `_ if judging == Judging::CallerJudges` arm in `client.rs` → `a_graphql_write_refused_for_its_credential_is_a_credential_error` red.
6. The fake's Free answer: make `rules` answer `answer(404, json!({}))` when no ruleset matches → `a_branch_with_no_ruleset_answers_an_empty_list` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/ledger/git.rs crates/github/src/ledger/init.rs crates/github/src/fake.rs crates/github/src/fake_git.rs crates/github/src/client.rs
git commit -m "fix(github): ledger reads fail closed and ask GitHub for less

A tree entry with no path or id is refused, never skipped; a compare asks
for one commit per page; the mode reads every page of the rules; a 401 on
the ledger's commit is a credential error. The fake's rules route pages.
Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 4: `verify` is bounded, reports progress, and finds one id on two lines

B1 requirements 6 and 8 (ruling 13, ruling 14). `verify` walks first parents from the head with no bound: a branch name reused for a long, unrelated history walks it to its root one request at a time, silently. And readers check §3.5 check 5 within one directory only; one id carried by two different lines in two directories is seen by no one.

**Blast radius:** `Verified` gains a field; its only constructor is `verify`, and no test builds one literally (`grep -rn "Verified {" crates`). `verify()` keeps its signature and calls `verify_with`; its only caller outside the module is Task 10's command.

**Files:**
- Modify: `crates/github/src/ledger/verify.rs:8-11` (imports), `:17-24` (`Verified`), `:55-130` (`verify`), new `same_id_at` after `fls_first_commit`, tests
- Modify: `crates/github/src/ledger/mod.rs:31`

**Interfaces:**
- Consumes: `commit_object`, `tree_files`, `blob_bytes` (B1, through `files_of`/`bytes_of`), `layout::{lines, decode, parse_segment_path, Line, QuarantineLine}`.
- Produces:

```rust
// fl_github::ledger (re-exported from verify)
pub const VERIFY_LIMIT: usize = 100_000;
pub struct Verified {                  // Debug, Clone, PartialEq, Eq
    pub commits: usize,
    pub first_bad: Option<BadCommit>,
    pub same_id: Option<SameId>,
}
pub struct SameId {                    // Debug, Clone, PartialEq, Eq
    pub id: Iri,
    pub first: (String, u64),          // segment path, line from 1
    pub second: (String, u64),
}
impl GithubLedger<'_> {
    pub fn verify(&self) -> Result<Verified, StoreError>;   // verify_with(VERIFY_LIMIT, no-op)
    pub fn verify_with(&self, limit: usize, progress: &mut dyn FnMut(usize))
        -> Result<Verified, StoreError>;
}
```

- [ ] **Step 1: Write the failing tests**

In `crates/github/src/ledger/verify.rs` tests, after `a_ledger_fl_wrote_verifies_clean`, add:

```rust
    /// A line carrying `run(1)`'s id but about another gate, filed by hand
    /// in that gate's own directory. Returns its segment.
    fn same_id_elsewhere(fake: &FakeGithub) -> String {
        let mut other = run(1);
        other.gate = GateId(seq_iri(77));
        let path = layout::segment_path(&layout::dir(layout::Area::Runs, other.gate.iri()), 1);
        fake.hand_commit(&[(path.as_str(), Some(file(&[line(&other)]).as_str()))]);
        path
    }

    // ⚠ A history that never reaches the anchor within the limit is
    // refused, naming the flag — never walked to its end one request at a
    // time.
    #[test]
    fn a_history_longer_than_the_limit_is_refused_naming_the_flag() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        for n in 1..=3 {
            publish(&l, n);
        }
        let err = l.verify_with(2, &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("walked back 2 commits"), "{err}");
        assert!(err.to_string().contains("--max-commits"), "{err}");
        assert_eq!(l.verify_with(4, &mut |_| {}).unwrap().commits, 4);
    }

    // A long history must never look like a hang.
    #[test]
    fn progress_is_told_each_commit_walked() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        publish(&l, 1);
        publish(&l, 2);
        let mut seen = Vec::new();
        let v = l.verify_with(VERIFY_LIMIT, &mut |n| seen.push(n)).unwrap();
        assert_eq!(seen, (1..=v.commits).collect::<Vec<_>>());
    }

    // Spec §3.5 check 5 across directories: a reader sees one directory at
    // a time, so only `verify` can see one id carried by two different
    // lines.
    #[test]
    fn one_id_on_two_different_lines_in_two_directories_is_reported() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        publish(&l, 1);
        let other = same_id_elsewhere(&fake);
        let v = l.verify().unwrap();
        assert_eq!(v.first_bad, None, "adding a directory only adds");
        let same = v.same_id.expect("the shared id is reported");
        assert_eq!(same.id, run(1).id.unwrap());
        assert_eq!(
            BTreeSet::from([same.first, same.second]),
            BTreeSet::from([(seg(1), 1), (other, 1)])
        );
    }

    // A quarantined copy is skipped, as readers skip it.
    #[test]
    fn a_quarantined_copy_of_a_shared_id_is_not_reported() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        publish(&l, 1);
        let other = same_id_elsewhere(&fake);
        l.quarantine(
            &seq_iri(500),
            &At::from_unix_millis(9),
            &other,
            1,
            "maintainer",
            "a copy",
        )
        .unwrap();
        assert_eq!(l.verify().unwrap().same_id, None);
    }

    // The same line twice is one entry, as readers read it once.
    #[test]
    fn an_identical_copy_of_a_line_is_not_reported() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        publish(&l, 1);
        let text = fake.ledger_files()[&seg(1)].clone();
        let first = text.lines().next().unwrap().to_string();
        fake.hand_commit(&[(seg(2).as_str(), Some(format!("{first}\n").as_str()))]);
        let v = l.verify().unwrap();
        assert_eq!(v.first_bad, None);
        assert_eq!(v.same_id, None);
    }

    // A symlink is a departure, never read as a segment: its lines are not
    // compared, even when shaped like one.
    #[test]
    fn a_symlinked_segment_is_not_read_for_ids() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        publish(&l, 1);
        let other = same_id_elsewhere(&fake);
        let head = fake.ledger_head().unwrap();
        make_a_symlink(&fake, &head, &other);
        let v = l.verify().unwrap();
        assert_eq!(v.first_bad.map(|b| b.commit), Some(head));
        assert_eq!(v.same_id, None);
    }
```

`make_a_symlink` is the module's existing helper (it answers the next listing of that commit's tree with `path` as mode `120000`); `verify` lists each tree once, so the same listing reaches the id check.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-github --lib ledger::verify`
Expected: compile errors — `verify_with`, `VERIFY_LIMIT` and `same_id` do not exist.

- [ ] **Step 3: Implement**

In `crates/github/src/ledger/verify.rs`, change the `layout` import (lines 9-11) to also bring `Line`:

```rust
use super::layout::{
    self, BRANCH, FORMAT, FORMAT_FILE, Line, QUARANTINE_FILE, QuarantineLine, README_FILE,
};
```

Replace `Verified` (lines 17-24) with:

```rust
/// What `verify` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// How many commits it walked.
    pub commits: usize,
    /// The oldest commit that does anything but add, and what it does.
    pub first_bad: Option<BadCommit>,
    /// One id carried by two different lines at the head (spec §3.5 check
    /// 5, across directories).
    pub same_id: Option<SameId>,
}

/// One id on two different lines: where each is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SameId {
    pub id: Iri,
    /// The segment and the line, from 1, of the copy met first in path
    /// order.
    pub first: (String, u64),
    pub second: (String, u64),
}

/// How many commits `verify` walks before it stops and says so — far more
/// than a ledger gathers in years of decisions, so reaching it means the
/// head does not lead back to the ledger's first commit along a history fl
/// wrote.
pub const VERIFY_LIMIT: usize = 100_000;
```

Replace `pub fn verify` (its doc and the function head through `let mut at = head;` and `loop {` with the loop's first three statements, lines 55-77) so that the function reads:

```rust
    /// `fl github ledger verify` (spec §3.5): every commit from the anchor
    /// to the head, along first parents, each checked to only add lines or
    /// segments. About one request per commit, plus the files it compares.
    pub fn verify(&self) -> Result<Verified, StoreError> {
        self.verify_with(VERIFY_LIMIT, &mut |_| {})
    }

    /// [`Self::verify`], walking at most `limit` commits and telling
    /// `progress` how many it has walked after each one, so a long history
    /// never looks like a hang. At the head it also reads every segment
    /// once, for one id on two lines (§3.5 check 5): about one request per
    /// segment more.
    pub fn verify_with(
        &self,
        limit: usize,
        progress: &mut dyn FnMut(usize),
    ) -> Result<Verified, StoreError> {
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
            // ⚠ Bounded: a history that does not reach the anchor (one the
            // branch name was reused for) is refused here, never walked to
            // its end one request at a time.
            if chain.len() >= limit {
                return Err(StoreError::Backend(format!(
                    "fl walked back {limit} commits from the head of {repo}'s `fl/ledger` \
                     without reaching the ledger's first commit {anchor}, and stopped. If the \
                     ledger really is that long, run `fl github ledger verify --max-commits \
                     <n>` with a larger number"
                )));
            }
            let c = self.commit_object(&at)?;
            let parents = c.parents.clone();
            chain.push((at.clone(), c));
            progress(chain.len());
```

The rest of the loop (from `if at == anchor {` on) is unchanged. At the end of the function, replace

```rust
        Ok(Verified {
            commits: chain.len(),
            first_bad,
        })
```

with

```rust
        let same_id = match chain.last() {
            Some((_, head)) => self.same_id_at(head, &mut seen)?,
            None => None,
        };
        Ok(Verified {
            commits: chain.len(),
            first_bad,
            same_id,
        })
```

(`chain` was reversed, so its last element is the head.) After `fls_first_commit`, add:

```rust
    /// Two different lines at `head` carrying one id (spec §3.5 check 5).
    /// A reader checks this within the one directory it reads; this looks
    /// across every directory.
    ///
    /// ⚠ A line a reader cannot decode is not compared here — reading it
    /// reports it, naming the quarantine command — and a quarantined line is
    /// skipped, as readers skip it. Two identical lines are one entry, as
    /// readers read them once.
    fn same_id_at(
        &self,
        head: &CommitObject,
        seen: &mut Seen,
    ) -> Result<Option<SameId>, StoreError> {
        let files = self.files_of(&head.tree, seen)?;
        let mut skipped: BTreeSet<(String, u64)> = BTreeSet::new();
        if let Some(q) = files.get(QUARANTINE_FILE) {
            let bytes = self.bytes_of(&q.oid, seen)?;
            for (_, text) in layout::lines(&bytes) {
                if let Ok(q) = text.map_err(str::to_string).and_then(QuarantineLine::decode) {
                    skipped.insert((q.file, q.line));
                }
            }
        }
        let mut first: BTreeMap<Iri, (String, u64, Line)> = BTreeMap::new();
        for (path, file) in &files {
            let Some((area, _, _)) = layout::parse_segment_path(path) else {
                continue;
            };
            if file.irregular.is_some() {
                continue;
            }
            let bytes = self.bytes_of(&file.oid, seen)?;
            for (n, text) in layout::lines(&bytes) {
                if skipped.contains(&(path.clone(), n)) {
                    continue;
                }
                let Ok((line, _by)) = text
                    .map_err(str::to_string)
                    .and_then(|t| layout::decode(area, t))
                else {
                    continue;
                };
                let Some(id) = line.id().cloned() else {
                    continue;
                };
                match first.get(&id) {
                    Some((at, m, held)) if *held != line => {
                        return Ok(Some(SameId {
                            id,
                            first: (at.clone(), *m),
                            second: (path.clone(), n),
                        }));
                    }
                    Some(_) => {}
                    None => {
                        first.insert(id, (path.clone(), n, line));
                    }
                }
            }
        }
        Ok(None)
    }
```

In `crates/github/src/ledger/mod.rs`, replace line 31 with:

```rust
pub use verify::{BadCommit, SameId, VERIFY_LIMIT, Verified};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github --lib ledger::verify`
Expected: PASS, the existing `verify` tests included.

- [ ] **Step 5: Mutation checks**

Guards in this task:

1. The cap: delete the `if chain.len() >= limit` block → `a_history_longer_than_the_limit_is_refused_naming_the_flag` red.
2. Progress: delete `progress(chain.len());` → `progress_is_told_each_commit_walked` red.
3. The cross-directory pass: replace `self.same_id_at(head, &mut seen)?` with `None` → `one_id_on_two_different_lines_in_two_directories_is_reported` red.
4. Quarantined lines skipped: delete the `if skipped.contains(…) { continue; }` → `a_quarantined_copy_of_a_shared_id_is_not_reported` red.
5. Identical copies are one entry: change `Some((at, m, held)) if *held != line` to `Some((at, m, _))` → `an_identical_copy_of_a_line_is_not_reported` red.
6. Irregular files are not read as segments: delete `if file.irregular.is_some() { continue; }` → `a_symlinked_segment_is_not_read_for_ids` red (the symlink's copy is compared and reported).

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/ledger/verify.rs crates/github/src/ledger/mod.rs
git commit -m "feat(github): verify is bounded, reports progress, and finds one id on two lines

verify_with walks at most a limit of commits (100,000 by default) and says
how to go further, tells a callback each commit walked, and at the head
reports one id carried by two different lines across directories, which no
reader can see. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 5: `init` refuses a branch under `fl/ledger/`

B1 requirement 3, second half (ruling 18). Git keeps a branch as a file, so `fl/ledger` cannot be created while `fl/ledger/<x>` exists, as it cannot while `fl` does. Today `init` creates a tree and a commit first and fails at the ref with GitHub's 422.

**Blast radius:** one more request in `init`'s creating path only (`(None, None)`); every other path is unchanged. The fake gains one read-only route.

**Files:**
- Modify: `crates/github/src/ledger/git.rs` (new `branches_under` after `branch_head`, line 145)
- Modify: `crates/github/src/ledger/init.rs:93-97` (the `(None, None)` arm), tests
- Modify: `crates/github/src/fake_git.rs:244` (new route before `("GET", ["git", "ref", …])`), tests

**Interfaces:**
- Consumes: `Client::get_all`.
- Produces: `pub(crate) fn branches_under(&self, branch: &str) -> Result<Vec<String>, StoreError>` on `GithubLedger` — each branch whose name starts with `"{branch}/"`, without `refs/heads/`.

- [ ] **Step 1: Write the failing tests**

In `crates/github/src/ledger/init.rs` tests, after `init_creates_the_branch_and_records_its_root_and_this_machines_cut_over`, add:

```rust
    // ⚠ Spec §6.1 step 2, the other way round: git keeps a branch as a
    // file, so `fl/ledger` cannot be created while a branch under
    // `fl/ledger/` exists. Refused before anything is created.
    #[test]
    fn init_refuses_a_branch_under_fl_ledger_before_creating_anything() {
        let fake = FakeGithub::start("acme/widgets");
        {
            let mut s = fake.state();
            let tree = s.git.put_tree(&BTreeMap::new());
            let c = s.git.put_commit(&tree, vec![], "someone's branch");
            s.git.refs.insert("heads/fl/ledger/old".into(), c);
        }
        let local = MemStore::default();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(1), None).unwrap_err();
        assert!(
            err.to_string().contains("has a branch named `fl/ledger/old`"),
            "{err}"
        );
        assert_eq!(fake.state().git.commits.len(), 1, "no commit was created");
        assert_eq!(fake.ledger_head(), None);
        assert_eq!(local.cutover("R_1").unwrap(), None);

        // A listed branch with no name is refused, never skipped.
        let fake = FakeGithub::start("acme/widgets");
        fake.state()
            .body_next
            .push(("/git/matching-refs/".into(), 200, json!([{}])));
        let local = MemStore::default();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(1), None).unwrap_err();
        assert!(err.to_string().contains("with no name"), "{err}");
        assert_eq!(fake.ledger_head(), None);
    }
```

(`json!` reaches the tests through `use super::*`, which brings `serde_json::{Value, json}`.)

In `crates/github/src/fake_git.rs` tests, after `the_rules_on_a_branch_are_those_of_active_rulesets`, add:

```rust
    // ⚠ Modelled: `git/matching-refs/<prefix>` lists every ref whose name
    // starts with the prefix, and an empty list when none does. Confirmed by
    // live test `a_branch_under_the_ledger_branch_is_found`.
    #[test]
    fn matching_refs_lists_the_refs_under_a_prefix() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let c = client(&fake);
        let under = |prefix: &str| -> Vec<String> {
            c.send(Method::Get, &format!("{REPO}/git/matching-refs/{prefix}"), None)
                .unwrap()
                .body
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["ref"].as_str().unwrap().to_string())
                .collect()
        };
        assert!(under("heads/fl/ledger/").is_empty());
        fake.state()
            .git
            .refs
            .insert("heads/fl/ledger/old".into(), root);
        assert_eq!(
            under("heads/fl/ledger/"),
            vec!["refs/heads/fl/ledger/old".to_string()]
        );
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-github --lib -- init_refuses_a_branch_under_fl_ledger_before_creating_anything matching_refs_lists_the_refs_under_a_prefix`
Expected: FAIL — `init` creates a commit and errors with GitHub's 422 (`commits.len()` is 2); the fake answers no route for `matching-refs` (its 404 answer fails `get_all`).

- [ ] **Step 3: Implement**

In `crates/github/src/fake_git.rs`, in `rest`, add before the `("GET", ["git", "ref", name @ ..])` arm (line 244):

```rust
        ("GET", ["git", "matching-refs", prefix @ ..]) => {
            let prefix = prefix.join("/");
            let items: Vec<Value> = s
                .git
                .refs
                .iter()
                .filter(|(name, _)| name.starts_with(&prefix))
                .map(|(name, sha)| {
                    json!({"ref": format!("refs/{name}"), "object": {"sha": sha, "type": "commit"}})
                })
                .collect();
            answer(200, Value::Array(items))
        }
```

In `crates/github/src/ledger/git.rs`, after `branch_head`, add:

```rust
    /// Every branch under `branch/`, such as `fl/ledger/old` under
    /// `fl/ledger`, by name.
    ///
    /// ⚠ Modelled: `git/matching-refs/heads/<branch>/` lists every ref whose
    /// name starts with it, and an empty list when none does. Confirmed by
    /// live test `a_branch_under_the_ledger_branch_is_found`.
    pub(crate) fn branches_under(&self, branch: &str) -> Result<Vec<String>, StoreError> {
        self.client
            .get_all(&self.path(&format!("/git/matching-refs/heads/{branch}/")))?
            .iter()
            .map(|r| {
                r.get("ref")
                    .and_then(Value::as_str)
                    .map(|s| s.trim_start_matches("refs/heads/").to_string())
                    .ok_or_else(|| {
                        backend(format!(
                            "GitHub listed a branch under `{branch}/` with no name"
                        ))
                    })
            })
            .collect()
    }
```

In `crates/github/src/ledger/init.rs`, replace the `(None, None)` arm (lines 93-97) with:

```rust
            (None, None) => {
                // ⚠ Step 2, the other way round: git cannot hold
                // `fl/ledger` while a branch under `fl/ledger/` exists.
                // Refused before anything is created.
                if let Some(first) = self.branches_under(BRANCH)?.first() {
                    return Err(StoreError::Backend(format!(
                        "the repository {repo} has a branch named `{first}`, and git cannot hold \
                         both it and `fl/ledger`. Rename that branch, then run `fl github ledger \
                         init` again"
                    )));
                }
                let root = self.create_branch()?;
                self.record(cutover, &root)?;
                Ok(InitOutcome::Created { root })
            }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Guards in this task:

1. The refusal: delete the `if let Some(first) = …` block → `init_refuses_a_branch_under_fl_ledger_before_creating_anything` red (GitHub's 422 names `refs/heads/fl/ledger`, not `fl/ledger/old`, and a commit was created).
2. The fake's prefix filter: change `name.starts_with(&prefix)` to `true` → `matching_refs_lists_the_refs_under_a_prefix` red (the first assertion lists `heads/fl/ledger`).
3. `branches_under` fails closed: in its `map`, replace `.ok_or_else(…)` with `.or(Some(String::new())).ok_or_else(…)` → `init_refuses_a_branch_under_fl_ledger_before_creating_anything` red (its second case: an unnamed branch counts as one named `` ``, refused with the wrong message).

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/ledger/git.rs crates/github/src/ledger/init.rs crates/github/src/fake_git.rs
git commit -m "fix(github): init refuses a branch under fl/ledger/ before creating anything

git cannot hold fl/ledger while fl/ledger/<x> exists. init now finds such
a branch through git/matching-refs and refuses, naming it, before it
creates a tree or a commit. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 6: A refusal reads as one sentence around its cause

B1 requirement 11 (ruling 17). Four messages wrap a `StoreError`: `ExecError::Unpublished` writes "(…)." around it, `ExecError::PublishRefused` writes ": …. The runs" — and `RestsOnLocalEntry` ends "Nothing was published.", so the result reads "published.. The runs" — and `check` and `fl attempt` write "(…). Fix what is named above", though the cause is on the same line.

**Blast radius:** message text only, on every refused decision: `record move`, `finding reproduce`, `finding verify` (through `refused_publish`), `check --record`, `fl attempt`'s warning. No test asserts the changed phrases (`grep -rn "named above\|cause above" crates` lists only the two sources); tests asserting `nothing changed`, `refused`, `kept in the local store`, `next decision`, `recorded in the local store` keep passing.

**Files:**
- Modify: `crates/core/src/store.rs` (new `as_clause` after `impl StoreError`, line 260; test), `crates/core/src/lib.rs:38-41`
- Modify: `crates/exec/src/population.rs:19-38` (two variants), `:162-171` (`refused_publish`), tests
- Modify: `crates/cli/src/cmd/check.rs:121-136`, tests
- Modify: `crates/cli/src/cmd/attempt.rs:149-160`, tests

**Interfaces:**
- Produces: `pub fn as_clause(e: &dyn std::fmt::Display) -> String` in `fl_core::store`, re-exported as `fl_core::as_clause`.

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/store.rs` tests, add:

```rust
    #[test]
    fn a_message_as_a_clause_drops_its_one_final_period() {
        let e = StoreError::RestsOnLocalEntry {
            decision: seq_iri(1),
            entry: seq_iri(2),
        };
        assert!(as_clause(&e).ends_with("Nothing was published"), "{}", as_clause(&e));
        assert_eq!(as_clause(&"no period"), "no period");
    }
```

In `crates/exec/src/population.rs` tests, add:

```rust
    // One period after the cause, however its own message ends; no "above"
    // for a cause on the same line.
    #[test]
    fn a_refused_publish_reads_as_one_sentence_around_its_cause() {
        for e in [
            fl_core::StoreError::RestsOnLocalEntry {
                decision: fl_core::ids::seq_iri(1),
                entry: fl_core::ids::seq_iri(2),
            },
            fl_core::StoreError::Unreachable {
                store: "the GitHub ledger of acme/widgets".into(),
                cause: "connection refused".into(),
            },
        ] {
            let msg = refused_publish(e).to_string();
            assert!(!msg.contains(".."), "{msg}");
            assert!(!msg.contains(".)"), "{msg}");
            assert!(!msg.contains("above"), "{msg}");
        }
    }
```

In `crates/cli/src/cmd/check.rs` tests, add:

```rust
    #[test]
    fn a_refused_checks_message_has_one_period_after_its_cause_and_no_above() {
        let ledger = Flushes::refusing_with(|| fl_core::store::StoreError::RestsOnLocalEntry {
            decision: seq_iri(1),
            entry: seq_iri(2),
        });
        let err = publish(&ledger, Some(&RecordId(seq_iri(3))), &report(true)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(!msg.contains(".."), "{msg}");
        assert!(!msg.contains(".)"), "{msg}");
        assert!(!msg.contains("above"), "{msg}");
    }
```

In `crates/cli/src/cmd/attempt.rs` tests, add:

```rust
    #[test]
    fn a_publish_warning_has_one_period_after_its_cause_and_no_above() {
        let w = conclude(
            &Flushes::refusing_with(|| StoreError::RestsOnLocalEntry {
                decision: seq_iri(1),
                entry: seq_iri(50),
            }),
            &seq_iri(50),
            &attempt(AttemptStatus::Timeout),
        )
        .expect("a warning");
        assert!(!w.contains(".."), "{w}");
        assert!(!w.contains(".)"), "{w}");
        assert!(!w.contains("above"), "{w}");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-core -p fl-exec -p fl-cli -- one_period a_message_as_a_clause a_refused_publish_reads_as_one_sentence`
Expected: compile error in `fl-core` (`as_clause` missing); once it exists, the three message tests FAIL on `..`/`.)`/`above`.

- [ ] **Step 3: Implement**

In `crates/core/src/store.rs`, after `impl StoreError { … }` (ends line 260), add:

```rust
/// `e`'s message as a clause inside a longer sentence: without the one
/// final period some messages end with, so the sentence around it ends
/// with exactly one.
pub fn as_clause(e: &dyn std::fmt::Display) -> String {
    let text = e.to_string();
    match text.strip_suffix('.') {
        Some(clause) => clause.to_string(),
        None => text,
    }
}
```

In `crates/core/src/lib.rs`, add `as_clause` to the `pub use store::{…}` list (lines 38-41):

```rust
pub use store::{
    Bindings, Catalog, CatalogChecked, Handles, KindRouted, Ledger, Roles, StoreError, Tracker,
    as_clause, follow, ledger_root_shape, node_id_shape,
};
```

In `crates/exec/src/population.rs`, replace the `Unpublished` and `PublishRefused` variants (lines 19-38) with:

```rust
    /// ⚠ The decision's evidence could not be published (GitHub ledger spec
    /// §2.2), so the decision is refused and nothing changed. The runs are
    /// in the local store, and the next decision that reaches the ledger
    /// publishes them. The cause is a clause (`fl_core::as_clause`).
    #[error(
        "refused: the evidence for this decision could not be published to the shared ledger, \
         so nothing changed: {0}. The runs are kept in the local store, and the next decision \
         that reaches the ledger publishes them"
    )]
    Unpublished(String),
    /// ⚠ The decision's evidence was refused for a reason waiting will not
    /// cure (GitHub ledger spec §7): the cause names what to fix. The runs
    /// are kept in the local store.
    #[error(
        "refused: the evidence for this decision could not be published to the shared ledger, \
         so nothing changed: {0}. The runs are kept in the local store; fix that cause first, \
         then decide again"
    )]
    PublishRefused(String),
```

and `refused_publish` (lines 165-171) with:

```rust
pub fn refused_publish(e: fl_core::StoreError) -> ExecError {
    let cause = fl_core::as_clause(&e);
    if e.is_transient() {
        ExecError::Unpublished(cause)
    } else {
        ExecError::PublishRefused(cause)
    }
}
```

In `crates/cli/src/cmd/check.rs`, replace the `map_err` closure of `publish` (lines 123-136) with:

```rust
        .map_err(|e| {
            // ⚠ Only a transient refusal promises a later publish (spec §7).
            let after = if e.is_transient() {
                "Its runs are kept in the local store, and the next decision that reaches the \
                 ledger publishes them."
            } else {
                "Its runs are kept in the local store. Fix that cause first, then decide again."
            };
            anyhow::anyhow!(
                "refused: the check ran, but its decision could not be published to the shared \
                 ledger: {}. {after}",
                fl_core::as_clause(&e)
            )
        })
```

In `crates/cli/src/cmd/attempt.rs`, replace the `Err(e) => { … }` arm of `conclude` (lines 149-160) with:

```rust
        Err(e) => {
            let after = if e.is_transient() {
                "Nothing is lost: the next decision that reaches the ledger publishes it."
            } else {
                "It stays there until that cause is fixed; the first decision that reaches the \
                 ledger afterwards publishes it."
            };
            Some(format!(
                "the attempt ran and is recorded in the local store, but it could not be \
                 published to the shared ledger: {}. {after}",
                fl_core::as_clause(&e)
            ))
        }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS — the new tests, and `the_warning_promises_a_later_publish_only_when_one_can_work` (the non-transient text still lacks "next decision").

- [ ] **Step 5: Mutation checks**

Guards in this task:

1. `as_clause` strips the period: make it return `e.to_string()` → `a_message_as_a_clause_drops_its_one_final_period`, `a_refused_publish_reads_as_one_sentence_around_its_cause`, `a_refused_checks_message_has_one_period_after_its_cause_and_no_above`, `a_publish_warning_has_one_period_after_its_cause_and_no_above` red.
2. `refused_publish` uses the clause: pass `e.to_string()` instead → `a_refused_publish_reads_as_one_sentence_around_its_cause` red.
3. `check` uses the clause: write `{e}` instead → `a_refused_checks_message_has_one_period_after_its_cause_and_no_above` red.
4. `fl attempt` uses the clause: write `{e}` instead → `a_publish_warning_has_one_period_after_its_cause_and_no_above` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/store.rs crates/core/src/lib.rs crates/exec/src/population.rs crates/cli/src/cmd/check.rs crates/cli/src/cmd/attempt.rs
git commit -m "fix: a refused publish reads as one sentence around its cause

as_clause drops a cause's one final period, so a message around it ends
each sentence once (no more 'published.. The runs'), and no message says
'above' for a cause on the same line. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 7: The `ledger = "github"` key

Spec §1.5 (ruling 16). The key is optional; its only value is `"github"`; any other is a config error that names it. `TrackerBinding` is `deny_unknown_fields`, so today the key is refused as unknown. This task only reads it; Task 8 builds the ledger from it and Task 9 binds it.

**Blast radius:** `TrackerBinding`'s derived `PartialEq` now includes `ledger`, which `store_tracker`, `tracker_for` and `config::bound_entry` compare: two entries naming one store and one repository that differ only in `ledger` are refused as two trackers. Their refusal messages (`tracker_name` in `main.rs`, the list in `bound_entry`) now say which has the GitHub ledger. Every struct literal of `TrackerBinding` gains `ledger: None` (`main.rs:668`, `config.rs:258`, `:289`, `:307`).

**Files:**
- Modify: `crates/cli/src/config.rs:24-31` (`TrackerBinding`), new `LedgerChoice`, `:203-206` (`bound_entry`'s names), tests and literals
- Modify: `crates/cli/src/main.rs:357-363` (`tracker_name`), test helper `entry` (`:664-673`), tests

**Interfaces:**
- Produces:

```rust
// crates/cli/src/config.rs
pub enum LedgerChoice { Github }          // Debug, Clone, Copy, PartialEq, Eq, Deserialize (try_from String)
pub struct TrackerBinding {
    pub github: String,
    pub credential: Credential,
    pub ledger: Option<LedgerChoice>,     // #[serde(default)]
}
impl TrackerBinding { pub fn github_ledger(&self) -> bool; }
```

- [ ] **Step 1: Write the failing tests**

In `crates/cli/src/config.rs` tests, add:

```rust
    // Spec §1.5: `ledger` is optional, and its only value is "github"; any
    // other is a config error naming it.
    #[test]
    fn the_only_ledger_a_binding_names_is_github() {
        let with = |ledger: &str| {
            format!(
                "[[project]]\nroot = \"/r\"\nstore = \"/s.redb\"\n\
                 tracker = {{ github = \"acme/widgets\", credential = \"env\"{ledger} }}\n"
            )
        };
        let binding = |text: &str| load_text(text).unwrap().projects[0].tracker.clone().unwrap();
        assert!(!binding(&with("")).github_ledger());
        assert!(binding(&with(", ledger = \"github\"")).github_ledger());
        for bad in ["local", "GitHub", ""] {
            let err = load_text(&with(&format!(", ledger = \"{bad}\""))).expect_err(bad);
            assert!(
                format!("{err:#}").contains(&format!("`ledger = \"{bad}\"` is not a ledger fl knows")),
                "{bad}: {err:#}"
            );
        }
    }

    // Two entries on one root and one store that differ only in `ledger`
    // are two trackers: refused, naming which has the GitHub ledger.
    #[test]
    fn two_entries_differing_only_by_ledger_are_refused_naming_which_has_it() {
        let root = tempfile::tempdir().unwrap();
        let binding = |ledger| TrackerBinding {
            github: "acme/widgets".into(),
            credential: Credential::Env,
            ledger,
        };
        let entries = vec![
            Entry {
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-same.redb"),
                tracker: Some(binding(None)),
            },
            Entry {
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-same.redb"),
                tracker: Some(binding(Some(LedgerChoice::Github))),
            },
        ];
        let msg = format!("{:#}", bound_entry(&entries, root.path()).unwrap_err());
        assert!(msg.contains("-> github:acme/widgets (ledger github)"), "{msg}");
        assert!(msg.contains("-> github:acme/widgets,"), "{msg}");
    }
```

In `crates/cli/src/main.rs` tests, add:

```rust
    // The ledger is part of a store's one tracker: two entries naming one
    // store and one repository, one with the GitHub ledger and one
    // without, are refused, naming which is which.
    #[test]
    fn entries_sharing_a_store_but_not_a_ledger_are_refused_naming_each() {
        let mut a = entry("/a", "/s/shared.redb", Some("acme/widgets"));
        a.tracker.as_mut().unwrap().ledger = Some(config::LedgerChoice::Github);
        let b = entry("/b", "/s/shared.redb", Some("acme/widgets"));
        let entries = [a.clone(), b];
        let msg = format!(
            "{:#}",
            tracker_for(Path::new("/s/shared.redb"), Some(&a), &entries, false).unwrap_err()
        );
        assert!(msg.contains("more than one tracker"), "{msg}");
        assert!(
            msg.contains("/a -> GitHub `acme/widgets` with its GitHub ledger"),
            "{msg}"
        );
        assert!(msg.contains("/b -> GitHub `acme/widgets`"), "{msg}");
        assert!(!msg.contains("/b -> GitHub `acme/widgets` with"), "{msg}");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --bin fl -- the_only_ledger_a_binding_names_is_github entries_sharing_a_store_but_not_a_ledger`
Expected: compile errors — `github_ledger`, `ledger` and `LedgerChoice` do not exist.

- [ ] **Step 3: Implement**

In `crates/cli/src/config.rs`, replace `TrackerBinding` (lines 24-31) with:

```rust
/// A project's tracker when it is not the local store (GitHub tracker spec
/// §1.4): `tracker = { github = "owner/repo", credential = "env" }`, and,
/// for mode B, `ledger = "github"` (GitHub ledger spec §1.5).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerBinding {
    pub github: String,
    pub credential: Credential,
    /// `Some(Github)`: decisions publish their evidence to the repository's
    /// `fl/ledger` branch. `None`: the ledger is the local store.
    ///
    /// ⚠ Part of the binding's identity: two entries for one store that
    /// differ here are two trackers, and refused.
    #[serde(default)]
    pub ledger: Option<LedgerChoice>,
}

impl TrackerBinding {
    /// Whether this binding names the GitHub ledger.
    pub fn github_ledger(&self) -> bool {
        self.ledger == Some(LedgerChoice::Github)
    }
}

/// The ledger a tracker binding names. One value: the GitHub ledger, which
/// always lives in the tracker's repository (spec §1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum LedgerChoice {
    Github,
}

impl TryFrom<String> for LedgerChoice {
    type Error = String;

    /// ⚠ Exactly `github`: any other value is refused by name, never read
    /// as the local ledger — a person who wrote it meant something.
    fn try_from(value: String) -> Result<Self, String> {
        match value.as_str() {
            "github" => Ok(LedgerChoice::Github),
            other => Err(format!(
                "`ledger = \"{other}\"` is not a ledger fl knows. Its only value is \"github\", \
                 the GitHub ledger in the tracker's repository; leave `ledger` out to keep every \
                 run and decision in the local store"
            )),
        }
    }
}
```

In `bound_entry`, replace the `tracker` naming (lines 203-206) with:

```rust
                let tracker = match &e.tracker {
                    Some(t) if t.github_ledger() => format!("github:{} (ledger github)", t.github),
                    Some(t) => format!("github:{}", t.github),
                    None => "the store's own tracker".to_string(),
                };
```

Add `ledger: None,` to the three `TrackerBinding { … }` literals in `config.rs` tests (lines 258, 289, 307).

In `crates/cli/src/main.rs`, replace `tracker_name` (lines 357-363) with:

```rust
/// How a config entry's tracker reads in a refusal.
fn tracker_name(t: Option<&config::TrackerBinding>) -> String {
    match t {
        Some(t) if t.github_ledger() => format!("GitHub `{}` with its GitHub ledger", t.github),
        Some(t) => format!("GitHub `{}`", t.github),
        None => "the store's own tracker".to_string(),
    }
}
```

and add `ledger: None,` to the `config::TrackerBinding { … }` literal in the test helper `entry` (line 668).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS, including `an_unknown_tracker_key_is_refused_not_ignored` (`extra` is still unknown).

- [ ] **Step 5: Mutation checks**

Guards in this task:

1. Only `github` is accepted: make `try_from` accept any value as `Github` → `the_only_ledger_a_binding_names_is_github` red.
2. Exact spelling: compare with `eq_ignore_ascii_case("github")` → red on `"GitHub"`.
3. `github_ledger` reads the field: return `false` → `the_only_ledger_a_binding_names_is_github` red.
4. The refusal names which entry has the ledger: drop the `Some(t) if t.github_ledger()` arm of `tracker_name` → `entries_sharing_a_store_but_not_a_ledger_are_refused_naming_each` red.
5. The binding's identity includes `ledger`: in `store_tracker`, compare trackers with `t.github` only (`trackers.iter().any(|x| x.map(|x| &x.github) == e.tracker.as_ref().map(|t| &t.github))`) → `entries_sharing_a_store_but_not_a_ledger_are_refused_naming_each` red (no refusal).
6. `bound_entry` names which entry has the ledger: drop its `Some(t) if t.github_ledger()` arm → `two_entries_differing_only_by_ledger_are_refused_naming_which_has_it` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/config.rs crates/cli/src/main.rs
git commit -m "feat(cli): the tracker binding's ledger = \"github\" key

Optional; its only value is github, and any other is a config error that
names it. The ledger is part of the binding's identity, so two entries for
one store that differ in it are refused as two trackers, naming which has
the GitHub ledger. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 8: `fl github ledger init`, and the mode in `whoami`

Spec §6.1 (steps 1–9), §6.2, B1 requirement 3 first half (rulings 2–6, 20, 21, 23). `main` builds one `GithubLedger` when the binding names it, over the tracker's client and the local store, and hands it to commands through `Ctx::github_ledger`. `init` imports the project's manifest first on a machine that does not author the project — so a machine that never imported it learns the ledger's first commit before it touches GitHub — then reads the mode, runs B1's `GithubLedger::init` with a fresh cut-over, and prints the outcome: for a root this run recorded, the guidance and what to do next; for a ledger already set up, the mode, and it stops.

**Blast radius:** `Ctx` gains a field — every `Ctx` literal (`main.rs:587`, `:596`, `ctx.rs:74`). `cmd::github::run` takes the project root. `manifest import`'s printing moves into `print_import`, unchanged in output. `fl github whoami` prints two or three more lines; `docs/github-tracker.md` says so. Commands other than `fl github` do not use the new field yet.

**Files:**
- Modify: `crates/cli/src/ctx.rs:9-22` (`Ctx`), test literal (`:74-81`)
- Modify: `crates/cli/src/main.rs:572-604` (build the ledger; `Ctx`), `:639` (dispatch)
- Modify: `crates/cli/src/cmd/github.rs` (whole file)
- Create: `crates/cli/src/cmd/ledger.rs`
- Modify: `crates/cli/src/cmd/mod.rs` (`pub mod ledger;`)
- Modify: `crates/cli/src/cmd/manifest.rs:288-320` (`print_import`), new `import_before_ledger_init`
- Create: `crates/cli/tests/ledger.rs`
- Modify: `docs/github-tracker.md:88-91`

**Interfaces:**
- Consumes: `config::TrackerBinding::github_ledger` (Task 7); `GithubLedger::{new, init, mode, repo}`, `InitOutcome`, `Mode`, `guidance` (B1); `GithubTracker::client` (B1); `fl_exec::stamp::entry_id`; `RedbStore::{owns, imported_hash, import_manifest}`.
- Produces:

```rust
// crates/cli/src/ctx.rs
pub struct Ctx<'a> {
    pub store: &'a RedbStore,
    pub tracker: &'a dyn Tracker,
    pub ledger: &'a dyn Ledger,
    pub handles: &'a dyn Handles,
    pub github: Option<&'a fl_github::GithubTracker>,
    /// The GitHub ledger, when the binding names it.
    pub github_ledger: Option<&'a fl_github::GithubLedger<'a>>,
    pub tracker_label: String,
}
// crates/cli/src/cmd/github.rs
pub fn run(ctx: &Ctx<'_>, cmd: Cmd, root: Option<&Path>) -> Result<i32>;   // Cmd::Ledger(ledger::Cmd)
// crates/cli/src/cmd/ledger.rs
pub enum Cmd { Init { confirm: Option<String> } }    // Task 10 adds Verify, Quarantine
pub fn run(ctx: &Ctx<'_>, cmd: Cmd, root: Option<&Path>) -> Result<i32>;
// crates/cli/src/cmd/manifest.rs
pub fn import_before_ledger_init(store: &RedbStore, root: &Path) -> Result<()>;
// crates/cli/tests/ledger.rs — the fixture later tasks extend
struct Machine { home: TempDir }  fn store(&self) -> PathBuf
struct World { repo: TempDir, fake: FakeGithub, one: Machine }
impl World { fn new(); fn bound(ledger: bool); fn configure(&self, &Machine, bool);
             fn machine(&self) -> Machine; fn fl(&self) -> Command; fn fl_on(&self, &Machine) -> Command;
             fn init(&self); fn export(&self); fn project_and_record(&self); }
```

- [ ] **Step 1: Write the failing tests**

Create `crates/cli/tests/ledger.rs`:

```rust
//! The CLI with a project whose ledger is the GitHub ledger
//! (`ledger = "github"`, GitHub ledger spec), against the in-process fake.
//! The `fl` binary reaches the fake through `FL_GITHUB_API_URL`.

use assert_cmd::Command;
use fl_github::fake::FakeGithub;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
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

/// One machine: a config home and a store of its own.
struct Machine {
    home: tempfile::TempDir,
}

impl Machine {
    fn store(&self) -> PathBuf {
        self.home.path().join("fl.redb")
    }
}

/// A git working tree whose `check.sh` fails while a file named `bug`
/// exists, the fake GitHub holding `acme/widgets`, and the first machine.
struct World {
    repo: tempfile::TempDir,
    fake: FakeGithub,
    one: Machine,
}

impl World {
    /// Bound to `acme/widgets` with the GitHub ledger.
    fn new() -> World {
        World::bound(true)
    }

    /// Bound to `acme/widgets`; with the GitHub ledger only when `ledger`.
    fn bound(ledger: bool) -> World {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.email", "t@example.com"]);
        git(repo.path(), &["config", "user.name", "t"]);
        fs::create_dir_all(repo.path().join("src")).unwrap();
        fs::write(repo.path().join("src/a.rs"), "fn a() {}").unwrap();
        let check = repo.path().join("check.sh");
        fs::write(&check, "#!/bin/sh\n[ ! -e bug ]\n").unwrap();
        fs::set_permissions(&check, fs::Permissions::from_mode(0o755)).unwrap();
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-qm", "first"]);
        let w = World {
            repo,
            fake: FakeGithub::start("acme/widgets"),
            one: Machine {
                home: tempfile::tempdir().unwrap(),
            },
        };
        w.configure(&w.one, ledger);
        w
    }

    /// Writes `m`'s config: the shared working tree, `m`'s own store, and
    /// the binding.
    fn configure(&self, m: &Machine, ledger: bool) {
        let ledger = if ledger { ", ledger = \"github\"" } else { "" };
        let cfg = format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n\
             tracker = {{ github = \"acme/widgets\", credential = \"env\"{ledger} }}\n",
            self.repo.path().canonicalize().unwrap().display(),
            m.store().display()
        );
        fs::create_dir_all(m.home.path().join("config/fl")).unwrap();
        fs::write(m.home.path().join("config/fl/config.toml"), cfg).unwrap();
    }

    /// A second machine on the same working tree and repository, with the
    /// GitHub ledger.
    fn machine(&self) -> Machine {
        let m = Machine {
            home: tempfile::tempdir().unwrap(),
        };
        self.configure(&m, true);
        m
    }

    fn fl(&self) -> Command {
        self.fl_on(&self.one)
    }

    fn fl_on(&self, m: &Machine) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", m.home.path().join("config"))
            .env("XDG_DATA_HOME", m.home.path().join("data"))
            .env("FL_GITHUB_TOKEN", "t")
            .env("FL_GITHUB_API_URL", self.fake.url())
            .env_remove("GITHUB_TOKEN")
            .env_remove("FL_DB")
            .current_dir(self.repo.path());
        c
    }

    fn init(&self) {
        self.fl()
            .args(["github", "ledger", "init"])
            .assert()
            .success();
    }

    /// Exports the manifest and commits it, as the person does after `init`.
    fn export(&self) {
        self.fl()
            .args(["manifest", "export", "--project", "1"])
            .assert()
            .success();
        git(self.repo.path(), &["add", ".fl"]);
        git(self.repo.path(), &["commit", "-qm", "manifest"]);
    }

    /// A project and one record (#1), which binds the repository's node.
    fn project_and_record(&self) {
        self.fl().args(["project", "add", "."]).assert().success();
        self.fl()
            .args(["record", "add", "--project", "1", "--title", "work"])
            .assert()
            .success();
    }
}

// Spec §6.1 step 1.
#[test]
fn init_refuses_a_binding_that_does_not_name_the_github_ledger() {
    let w = World::bound(false);
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .code(2)
        .stderr(contains("does not name the GitHub ledger"));
    assert_eq!(w.fake.ledger_head(), None);
}

// Spec §6.1 steps 3, 4 and 7-9; a second run changes nothing.
#[test]
fn init_creates_the_ledger_and_says_what_the_person_does_next() {
    let w = World::new();
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .success()
        .stdout(
            contains("created\tfl/ledger\t")
                .and(contains("mode: detection-only"))
                .and(contains("Next: run `fl manifest export")),
        );
    let head = w.fake.ledger_head().expect("the branch");
    // Spec §6.1 step 6: set up, and it stops — the mode, not the guidance.
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .success()
        .stdout(
            contains("set up\tfl/ledger\t")
                .and(contains("mode\tdetection-only\n"))
                .and(contains("Protect the default branch").not())
                .and(contains("Next:").not()),
        );
    assert_eq!(w.fake.ledger_head(), Some(head), "a second run changes nothing");
}

// ⚠ The mode is read before anything is created: a rules read that fails
// leaves no branch, so the run that does create the ledger is the one that
// says to export and commit the manifest.
#[test]
fn init_whose_rules_cannot_be_read_creates_nothing_and_a_rerun_says_what_to_do() {
    let w = World::new();
    w.fake.state().fail_rules_next = true;
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .code(2)
        .stderr(contains("rules/branches/fl/ledger"));
    assert_eq!(w.fake.ledger_head(), None, "nothing was created");
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .success()
        .stdout(contains("created\tfl/ledger\t").and(contains("Next: run `fl manifest export")));
}

// ⚠ The manifest first: a machine that never imported it learns the
// ledger's first commit before `init` touches GitHub, and records its own
// cut-over (spec §6.1 steps 4 and 5).
#[test]
fn a_second_machine_learns_the_ledger_from_the_manifest_and_records_its_own_cut_over() {
    let w = World::new();
    w.project_and_record();
    w.init();
    w.export();
    let head = w.fake.ledger_head();
    let two = w.machine();
    w.fl_on(&two)
        .args(["github", "ledger", "init"])
        .assert()
        .success()
        .stdout(
            contains("imported\t")
                .and(contains("set up\tfl/ledger\t"))
                .and(contains("this machine's cut-over is recorded"))
                .and(contains("confirm\t").not()),
        );
    assert_eq!(w.fake.ledger_head(), head, "nothing changed on GitHub");
}

// ⚠ Without the manifest's root, this machine would create a second
// ledger and hide the deletion (spec §6.1 step 6).
#[test]
fn a_machine_that_never_imported_the_manifest_does_not_create_a_second_ledger_where_one_was_deleted()
{
    let w = World::new();
    w.project_and_record();
    w.init();
    w.export();
    w.fake.delete_ledger();
    let two = w.machine();
    w.fl_on(&two)
        .args(["github", "ledger", "init"])
        .assert()
        .code(2)
        .stderr(contains("fl will not start a new ledger"));
    assert_eq!(w.fake.ledger_head(), None);
}

// The authoring store wrote the manifest's root itself: `init` imports
// nothing there, and runs again cleanly.
#[test]
fn init_on_the_machine_that_authors_the_project_imports_nothing() {
    let w = World::new();
    w.project_and_record();
    w.init();
    w.export();
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .success()
        .stdout(contains("set up\tfl/ledger\t").and(contains("imported\t").not()));
}

// Spec §6.1 step 6: a branch no machine records and no manifest names —
// the person confirms its first commit, by name.
#[test]
fn init_adopts_an_existing_branch_only_once_its_first_commit_is_confirmed() {
    let w = World::new();
    let root = w.fake.seed_ledger();
    w.fl()
        .args(["github", "ledger", "init"])
        .assert()
        .code(1)
        .stdout(contains(format!("confirm\tfl/ledger\t{root}")));
    w.fl()
        .args([
            "github",
            "ledger",
            "init",
            "--confirm",
            "0123456789abcdef0123456789abcdef01234567",
        ])
        .assert()
        .code(2)
        .stderr(contains("is not the first commit"));
    w.fl()
        .args(["github", "ledger", "init", "--confirm", &root])
        .assert()
        .success()
        .stdout(contains(format!("adopted\tfl/ledger\t{root}")));
}

// Spec §6.2: `whoami` states the ledger, and the mode in force.
#[test]
fn whoami_states_the_ledger_and_the_mode_in_force() {
    let local = World::bound(false);
    local
        .fl()
        .args(["github", "whoami"])
        .assert()
        .success()
        .stdout(contains("ledger\tlocal\n").and(contains("mode\t").not()));
    let w = World::new();
    w.fl()
        .args(["github", "whoami"])
        .assert()
        .success()
        .stdout(contains("ledger\tgithub\n").and(contains("mode\tdetection-only\t")));
    w.fake
        .state()
        .rulesets
        .push(fl_github::fake_git::Ruleset::on_ledger(
            "active",
            &["non_fast_forward", "deletion"],
        ));
    w.fl()
        .args(["github", "whoami"])
        .assert()
        .success()
        .stdout(contains("mode\tprotected\n"));
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --test ledger`
Expected: FAIL — `fl github ledger` is not a subcommand (clap exits 2 with "unrecognized subcommand"), and `whoami` prints no `ledger` line.

- [ ] **Step 3: Implement the context and the binding**

In `crates/cli/src/ctx.rs`, add the field to `Ctx` after `github` (line 19):

```rust
    /// The GitHub ledger, when the binding names it (GitHub ledger spec
    /// §1.5): over the tracker's client and the local store.
    pub github_ledger: Option<&'a fl_github::GithubLedger<'a>>,
```

and `github_ledger: None,` to the literal in `the_roles_bind_the_ledger_the_command_was_given` (after `github: None,`).

In `crates/cli/src/main.rs`, after the `let github = match (&binding, needs_tracker) { … };` block (lines 572-575), add:

```rust
    // The GitHub ledger, when the binding names it (GitHub ledger spec
    // §1.5): over the tracker's client — one credential, one origin guard
    // (§1.1) — and the local store, which keeps its anchor, cut-over and
    // cache.
    let github_ledger = match (&github, &binding) {
        (Some(gh), Some(b)) if b.github_ledger() => Some(fl_github::GithubLedger::new(
            gh.client(),
            gh.repo().clone(),
            &store,
        )),
        _ => None,
    };
```

In the two `Ctx { … }` literals (lines 587-594, 596-603) add, after `github: …,`, respectively `github_ledger: github_ledger.as_ref(),` and `github_ledger: None,`. Replace the dispatch line 639 with:

```rust
        Command::Github(c) => cmd::github::run(&ctx, c, entry.as_ref().map(|e| e.root.as_path())),
```

- [ ] **Step 4: Implement the manifest step**

In `crates/cli/src/cmd/manifest.rs`, replace the body of the `Cmd::Import { root }` arm after `let m = read(&root)?;` (lines 296-319) with:

```rust
            let report = store.import_manifest(&m, &root.display().to_string())?;
            print_import(store, &m, &report)?;
```

and add, after `ensure_publishable`:

```rust
/// What an import did, for a person: the project, its gates and
/// transitions, and each gate's handle in this store.
fn print_import(store: &RedbStore, m: &Manifest, report: &fl_store::ImportReport) -> Result<()> {
    if let Some((old, new)) = &report.root_moved {
        println!("moved\tthe project's gates now run over {new}, not {old}");
    }
    println!(
        "imported\t{}\tgates: {} added, {} changed, {} unchanged\ttransitions: {}",
        refs::show(store, Kind::Project, report.project.iri())?,
        report.gates_added,
        report.gates_changed,
        report.gates_unchanged,
        report.transitions
    );
    // ⚠ This store numbers handles on its own: they can differ from the
    // authoring machine's, so the person needs to see them here.
    for g in &m.body.gates {
        println!(
            "gate\t{}\t{}",
            refs::show(store, Kind::Gate, g.id.iri())?,
            g.name
        );
    }
    for name in &report.transitions_removed {
        println!("removed\ttransition\t{name}");
    }
    Ok(())
}

/// What `fl github ledger init` learns from the project's committed
/// manifest before it touches GitHub (GitHub ledger spec §6.1 step 4): on a
/// machine that does not author the project, the manifest is imported as
/// `fl manifest import` imports it — which records the ledger's first
/// commit when the manifest carries one. The authoring store wrote that
/// root into the manifest itself; a project with no manifest yet has
/// nothing to teach.
///
/// ⚠ Without it, a machine that never imported the manifest knows no first
/// commit: where the ledger was deleted it would create a second one and
/// hide the deletion, and it would offer an existing ledger for
/// confirmation as if no machine knew it.
pub fn import_before_ledger_init(store: &RedbStore, root: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("`{}` could not be resolved", root.display()))?;
    let path = root.join(MANIFEST_PATH);
    if !path
        .try_exists()
        .with_context(|| format!("could not look for {}", path.display()))?
    {
        return Ok(());
    }
    let m = read(&root)?;
    let project = &m.body.project;
    if store.owns(project.iri())? && store.imported_hash(project)?.is_none() {
        return Ok(());
    }
    Git::head(&root)
        .map_err(|e| anyhow::anyhow!("`{}` is not a git working tree: {e}", root.display()))?;
    let report = store.import_manifest(&m, &root.display().to_string())?;
    print_import(store, &m, &report)
}
```

- [ ] **Step 5: Implement the commands**

Create `crates/cli/src/cmd/ledger.rs`:

```rust
//! `fl github ledger` (GitHub ledger spec §3.5, §3.6, §6.1): set up the
//! GitHub ledger, walk its history, and quarantine a line. Each is run by a
//! person, by hand.

use crate::ctx::Ctx;
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_github::GithubLedger;
use fl_github::ledger::{InitOutcome, guidance};
use std::path::Path;

#[derive(Subcommand)]
pub enum Cmd {
    /// Set up the GitHub ledger on the bound repository, or record this
    /// machine's cut-over on one already set up. Safe to run again.
    Init {
        /// The ledger's first commit, as an earlier `init` asked you to
        /// confirm.
        #[arg(long)]
        confirm: Option<String>,
    },
}

/// The GitHub ledger the binding names, or the refusal that says how to
/// name it (spec §6.1 step 1).
fn bound<'a>(ctx: &Ctx<'a>) -> Result<&'a GithubLedger<'a>> {
    match ctx.github_ledger {
        Some(gl) => Ok(gl),
        None => bail!(
            "this project's tracker binding does not name the GitHub ledger. Add \
             `ledger = \"github\"` to its `tracker = {{ … }}` in the config, then run this again"
        ),
    }
}

pub fn run(ctx: &Ctx<'_>, cmd: Cmd, root: Option<&Path>) -> Result<i32> {
    let gl = bound(ctx)?;
    match cmd {
        Cmd::Init { confirm } => init(ctx, gl, root, confirm.as_deref()),
    }
}

/// `fl github ledger init` (spec §6.1).
fn init(
    ctx: &Ctx<'_>,
    gl: &GithubLedger<'_>,
    root: Option<&Path>,
    confirm: Option<&str>,
) -> Result<i32> {
    // ⚠ The manifest first: a machine that lacks the ledger's first commit
    // learns it there, before anything is created on GitHub.
    if let Some(root) = root {
        crate::cmd::manifest::import_before_ledger_init(ctx.store, root)?;
    }
    let repo = gl.repo().full_name.clone();
    // ⚠ The mode before anything is created or recorded: once the root is
    // recorded nothing below can fail, so a rules read that fails never
    // leaves a ledger whose "export and commit the manifest" was not said.
    let mode = gl.mode()?;
    let outcome = gl.init(&fl_exec::stamp::entry_id(), confirm)?;
    match &outcome {
        InitOutcome::Created { root } => println!("created\tfl/ledger\t{root}"),
        InitOutcome::Adopted { root } => println!("adopted\tfl/ledger\t{root}"),
        // ⚠ Spec §6.1 step 6: the ledger is set up, so `init` says so and
        // stops — after this machine's cut-over (step 5) and the mode in
        // force, a statement of state; the guidance is not printed again.
        InitOutcome::AlreadySetUp {
            root,
            cutover_recorded,
        } => {
            println!("set up\tfl/ledger\t{root}");
            if *cutover_recorded {
                println!(
                    "this machine's cut-over is recorded: from now on, its decisions publish \
                     their evidence"
                );
            }
            println!("mode\t{}", mode.name());
            return Ok(0);
        }
        // ⚠ Nothing was recorded, and nothing was refused: exit 1.
        InitOutcome::Confirm { root } => {
            println!("confirm\tfl/ledger\t{root}");
            println!(
                "`fl/ledger` exists, but neither this machine nor the project's manifest records \
                 its first commit. If {root} is the commit you created with `fl github ledger \
                 init`, run `fl github ledger init --confirm {root}`. Nothing was recorded."
            );
            return Ok(1);
        }
    }
    // Created or adopted: this run recorded the root (steps 7-9).
    for paragraph in guidance(&repo, &mode) {
        println!("\n{paragraph}");
    }
    println!(
        "\nNext: run `fl manifest export --project <project>` and commit \
         `.fl/manifest.json`. Every other machine learns the ledger's first commit from it, \
         and refuses to publish until it has."
    );
    Ok(0)
}
```

In `crates/cli/src/cmd/mod.rs`, add `pub mod ledger;` after `pub mod github;`.

Replace `crates/cli/src/cmd/github.rs` with:

```rust
//! `fl github` (GitHub tracker spec §3.4, §5.4; GitHub ledger spec §6).

use crate::ctx::Ctx;
use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::Iri;
use fl_github::ledger::Mode;
use std::path::Path;

#[derive(Subcommand)]
pub enum Cmd {
    /// Print who fl writes to GitHub as, the repository it binds, and the
    /// ledger with its mode.
    Whoami,
    /// Rewrite a diverged issue's fl labels and open/closed status from
    /// fl's own record in its body, and leave a comment naming who ran it.
    Repair {
        /// The issue: a handle (`41` or `#41`) or its URL.
        id: Ref,
        /// Who is repairing it; recorded in the comment.
        #[arg(long)]
        by: String,
    },
    /// The GitHub ledger: set it up, walk its history, quarantine a line.
    #[command(subcommand)]
    Ledger(crate::cmd::ledger::Cmd),
}

impl Cmd {
    fn refs(&self) -> Vec<&Ref> {
        match self {
            Cmd::Whoami | Cmd::Ledger(_) => vec![],
            Cmd::Repair { id, .. } => vec![id],
        }
    }
    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }
    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }
}

/// `root` is the project's root from its config entry: `ledger init` reads
/// the committed manifest there.
pub fn run(ctx: &Ctx<'_>, cmd: Cmd, root: Option<&Path>) -> Result<i32> {
    let Some(gh) = ctx.github else {
        bail!("`fl github` needs the project to be bound to a GitHub repository");
    };
    match cmd {
        Cmd::Whoami => {
            println!("writes as\t{}", gh.identity()?);
            println!("credential\t{}", gh.describe());
            println!("repository\t{}", gh.repo().full_name);
            match ctx.github_ledger {
                None => println!("ledger\tlocal"),
                Some(gl) => {
                    println!("ledger\tgithub");
                    // Spec §6.2: the mode in force, and what is missing when
                    // it is not protected.
                    match gl.mode()? {
                        Mode::Protected => println!("mode\tprotected"),
                        Mode::DetectionOnly { why } => println!("mode\tdetection-only\t{why}"),
                    }
                }
            }
        }
        Cmd::Repair { id, by } => {
            let iri = match &id {
                Ref::Handle(n) => gh.issue_url(*n),
                Ref::Iri(i) => i.clone(),
            };
            let done = gh.repair(&iri, &by)?;
            let word = if done.changed {
                "repaired"
            } else {
                "consistent"
            };
            println!("{word}\t{}\t{}", done.number, done.state);
        }
        Cmd::Ledger(c) => return crate::cmd::ledger::run(ctx, c, root),
    }
    Ok(0)
}
```

In `docs/github-tracker.md`, replace lines 88-91 (the `fl github whoami` paragraph) with:

```markdown
`fl github whoami`, run inside the project's checkout, prints the identity GitHub reports for
the credential (a user's login, or an App's `<slug>[bot]`), where the credential came from (the
environment variable, or the App's id), the repository fl binds, and the ledger: `local`, or
`github` followed by the mode in force — `protected`, or `detection-only` with what is missing.
It asks GitHub; it does not repeat the config back.
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS — the new file, and every existing test (`whoami_names_the_credential_and_the_repository` still finds its three values).

- [ ] **Step 7: Mutation checks**

Guards in this task:

1. Step 1's refusal: in `main`, drop `if b.github_ledger()` from the `github_ledger` match, so every GitHub binding gets a ledger → `init_refuses_a_binding_that_does_not_name_the_github_ledger` red (a ledger is created); and `whoami_states_the_ledger_and_the_mode_in_force` red (`ledger\tgithub` for the binding without the key).
2. The manifest first: delete the `import_before_ledger_init` call in `init` → `a_second_machine_learns_the_ledger_from_the_manifest_and_records_its_own_cut_over` red (it prints `confirm\t`) and `a_machine_that_never_imported_the_manifest_does_not_create_a_second_ledger_where_one_was_deleted` red (a branch is created).
3. The authoring store imports nothing: delete the `if store.owns(…) && … { return Ok(()); }` → `init_on_the_machine_that_authors_the_project_imports_nothing` red (`import_manifest` refuses an authoring store).
4. No manifest, nothing to learn: delete the `try_exists` check → `init_creates_the_ledger_and_says_what_the_person_does_next` red ("could not read the manifest").
5. `Confirm` exits 1: return `Ok(0)` → `init_adopts_an_existing_branch_only_once_its_first_commit_is_confirmed` red.
6. An existing ledger stops after the mode: delete `return Ok(0);` in the `AlreadySetUp` arm → `init_creates_the_ledger_and_says_what_the_person_does_next` red (the second run prints the guidance and `Next:`).
7. The mode on an existing ledger: delete `println!("mode\t{}", …)` in that arm → `init_creates_the_ledger_and_says_what_the_person_does_next` red.
8. The mode before anything is created: move `let mode = gl.mode()?;` below `gl.init(…)?` → `init_whose_rules_cannot_be_read_creates_nothing_and_a_rerun_says_what_to_do` red (a branch is left, and the re-run says `set up`, not `Next:`).
9. `whoami`'s ledger line: print `ledger\tlocal` always → `whoami_states_the_ledger_and_the_mode_in_force` red.
10. `whoami`'s mode: print `mode\tprotected` always → `whoami_states_the_ledger_and_the_mode_in_force` red.

- [ ] **Step 8: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/ctx.rs crates/cli/src/main.rs crates/cli/src/cmd/github.rs crates/cli/src/cmd/ledger.rs crates/cli/src/cmd/mod.rs crates/cli/src/cmd/manifest.rs crates/cli/tests/ledger.rs docs/github-tracker.md
git commit -m "feat(cli): fl github ledger init, and the ledger and mode in whoami

main builds the GitHub ledger when the binding names it, over the
tracker's client. init imports the project's manifest first on a machine
that does not author it, so a deleted ledger is refused as deleted and an
existing one is not offered for confirmation; it confirms a found first
commit only by --confirm <commit>, reads the mode before creating
anything, prints the guidance and says to export and commit the manifest
for a new root, and on an existing ledger states it and the mode and stops. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 9: Decisions publish through `SplitLedger`

Spec §1.2, §2.1, §2.6 (plan A ruling 11). When the binding names the GitHub ledger, `Ctx::ledger` — and so `Ctx::roles().ledger`, through which `record move`, `check --record`, `finding reproduce`/`verify` and `fl attempt` append and flush — is `SplitLedger { local: store, github }`. Every entry still goes to the local store at once; each flush publishes. A plain `fl check` never opens the tracker (`needs_tracker` is false for it), so it stays local. This task also fixes the comments that described this binding before it existed, and the review labels in `main.rs`.

**Blast radius:** every command that needs the tracker on a binding with `ledger = "github"`. Commands that make no decision (`record add`, `record list`, `finding raise/assign/withdraw/list`) never flush and read no ledger, so they send no ledger request. A binding without the key is untouched: `Ctx::ledger` stays the store.

**Files:**
- Modify: `crates/cli/src/main.rs` (after the `github_ledger` block from Task 8; the two `Ctx` literals; comments at `:73-76`, `:219-220`, `:258-263`, `:368-374`, `:612-620`)
- Modify: `crates/cli/src/ctx.rs:13-14`
- Modify: `crates/cli/tests/ledger.rs` (fixture helpers; tests)

**Interfaces:**
- Consumes: `fl_core::SplitLedger` (plan A); `Ctx::github_ledger` (Task 8).
- Produces: in `crates/cli/tests/ledger.rs`, `World::{gated, ready, ledger_files_in}`:

```rust
fn gated(&self);                     // project, gate `no-bug`, transition `launch` (todo -> doing), record #1
fn ready(&self);                     // gated + init + export
fn ledger_files_in(&self, area: &str) -> Vec<(String, String)>;   // path, text under `area/`
```

- [ ] **Step 1: Write the failing tests**

In `crates/cli/tests/ledger.rs`, add to `impl World`:

```rust
    /// A project with one gate over `src/**/*.rs` (`check.sh`), a
    /// transition `launch` from `todo` to `doing` over it, and one record
    /// (#1).
    fn gated(&self) {
        self.fl().args(["project", "add", "."]).assert().success();
        self.fl()
            .args([
                "gate",
                "add",
                "--project",
                "1",
                "--name",
                "no-bug",
                "--glob",
                "src/**/*.rs",
                "--program",
                "./check.sh",
            ])
            .assert()
            .success();
        self.fl()
            .args([
                "transition",
                "add",
                "--project",
                "1",
                "--name",
                "launch",
                "--from",
                "todo",
                "--to",
                "doing",
                "--regret",
                "low",
                "--gate",
                "1",
            ])
            .assert()
            .success();
        self.fl()
            .args(["record", "add", "--project", "1", "--title", "work"])
            .assert()
            .success();
    }

    /// `gated`, the ledger set up, and the manifest committed: every
    /// decision can publish.
    fn ready(&self) {
        self.gated();
        self.init();
        self.export();
    }

    /// The ledger's files under `area/`, path and text, in path order.
    fn ledger_files_in(&self, area: &str) -> Vec<(String, String)> {
        let prefix = format!("{area}/");
        self.fake
            .ledger_files()
            .into_iter()
            .filter(|(p, _)| p.starts_with(&prefix))
            .collect()
    }
```

and the tests:

```rust
// Spec §2.2 and §2.3: a move flushes its run and its decision to
// `fl/ledger`, and moves the record. That the flush comes first is pinned
// in `fl-exec` (`move_record`'s tests) and the conformance suites.
#[test]
fn a_move_publishes_its_run_and_its_decision_and_moves_the_record() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    let runs = w.ledger_files_in("runs");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].1.lines().count(), 1, "{runs:?}");
    let decisions = w.ledger_files_in("decisions");
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert!(decisions[0].1.contains(r#""allowed":true"#), "{decisions:?}");
    assert!(
        w.fake
            .issue(1)
            .labels
            .contains(&"fl:record/doing".to_string())
    );
}

// Spec §2.2: `check --record` is a decision and publishes; a plain check
// decides nothing and stays local (decision 6).
#[test]
fn a_check_with_a_record_publishes_and_a_plain_check_does_not() {
    let w = World::new();
    w.ready();
    let before = w.fake.ledger_commits();
    w.fl()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .success();
    assert_eq!(w.fake.ledger_commits(), before, "a plain check publishes nothing");
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    assert_eq!(w.fake.ledger_commits(), before + 1);
    assert!(w.ledger_files_in("decisions")[0].1.contains(r#"{"check":"#));
}

// Spec §1.5: without `ledger = "github"`, a decision stays in the local
// store, exactly as in mode A — no flush is even attempted.
#[test]
fn without_the_ledger_key_a_move_publishes_nothing() {
    let w = World::bound(false);
    w.gated();
    w.fake.seed_ledger();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success()
        .stderr(contains("never switched on").not());
    assert_eq!(w.fake.ledger_commits(), 1);
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --test ledger -- publishes`
Expected: the first two FAIL (`Ctx::ledger` is still the store: nothing reaches `fl/ledger`); the third PASSES and is pinned by Step 5's mutation 2.

- [ ] **Step 3: Implement the binding**

In `crates/cli/src/main.rs`, after the `github_ledger` block, add:

```rust
    // Mode B (GitHub ledger spec §1.2, §2.6): every entry goes to the local
    // store at once; each decision's flush publishes through `github_ledger`.
    let split = github_ledger.as_ref().map(|gl| fl_core::SplitLedger {
        local: &store,
        github: gl,
    });
    let ledger: &dyn fl_core::Ledger = match &split {
        Some(s) => s,
        None => &store,
    };
```

and in both `Ctx { … }` literals replace `ledger: &store,` with `ledger,`.

In `crates/cli/src/ctx.rs`, replace lines 13-14 with:

```rust
    /// Where runs, attempts and decisions are recorded: the local store,
    /// or — when the binding names the GitHub ledger — a `SplitLedger` over
    /// the local store and `github_ledger`.
```

- [ ] **Step 4: Fix the comments that carried review labels**

In `crates/cli/src/main.rs`:

- Lines 73-76: replace `/// so the store bound to \`<path>\` is the one it belongs in (Final` / `/// review, item 2). Every other command works on the current project.` with

```rust
    /// so the store bound to `<path>` is the one it belongs in. Every other
    /// command works on the current project.
```

- Line 220: replace `/// ⚠ Ruling: \`--db\` and \`$FL_DB\` CONFINE the command` with

```rust
/// ⚠ `--db` and `$FL_DB` CONFINE the command
```

- Lines 260-261: replace `/// uses the bound store. \`confined\` is true only for \`--db\`/\`$FL_DB\` (Fix` / `/// round 1, item 0, ruling): an explicit store CONFINES the search to` with

```rust
/// uses the bound store. `confined` is true only for `--db`/`$FL_DB`: an
/// explicit store CONFINES the search to
```

- Lines 370-371: replace `/// tracker commands, and \`manifest export\`'s ledger-root binding (whole-` / `/// branch review finding 4) — a store bound to more than one tracker in` with

```rust
/// tracker commands, and `manifest export`'s ledger-root binding — a store
/// bound to more than one tracker in
```

- Line 612: replace `// ⚠⚠ Whole-branch review finding 4: \`manifest\` never asks for a` with

```rust
        // ⚠⚠ `manifest` never asks for a
```

Run `cargo fmt --all` to re-flow the shortened comment lines.

- [ ] **Step 5: Mutation checks**

Run `cargo test -p fl-cli --test ledger` after each. Guards in this task:

1. The binding: set `let ledger: &dyn fl_core::Ledger = &store;` → `a_move_publishes_its_run_and_its_decision_and_moves_the_record`, `a_check_with_a_record_publishes_and_a_plain_check_does_not` red.
2. Only with the key: in the `github_ledger` match, drop `if b.github_ledger()` → `without_the_ledger_key_a_move_publishes_nothing` red (the store has no cut-over, so the flush prints "never switched on").

A plain `check` staying local is not this task's guard: it never opens the tracker (`needs_tracker`, `main.rs:110`), which `a_command_that_needs_no_tracker_never_contacts_github` already pins.

Confirm with `grep -n "review\|Ruling:\|Fix round" crates/cli/src/main.rs crates/cli/src/ctx.rs` that no label remains.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/main.rs crates/cli/src/ctx.rs crates/cli/tests/ledger.rs
git commit -m "feat(cli): decisions publish through SplitLedger when the binding names it

Ctx::ledger is SplitLedger { local: store, github } for a binding with
ledger = \"github\": every entry is stored locally at once and each
decision's flush publishes it. A plain check, and every binding without
the key, stays local. Comments no longer carry review labels. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 10: `fl github ledger verify` and `quarantine`; what a read noted

Spec §3.5 (verify), §3.6 (quarantine), decision 16, B1 requirement 5 (rulings 6, 14, 19). `verify` prints what `verify_with` found and exits 1 when it found a departure or one id on two lines. `quarantine` says every time that `--by` and `--reason` are written to the ledger for good, warns before appending when the repository is not private, then appends. After every command, `main` prints what the GitHub ledger's reads noted — a quarantined line skipped — on stderr.

**Blast radius:** `main`'s return path: the notes are printed after the command whatever its result; a command with no GitHub ledger prints nothing more.

**Files:**
- Modify: `crates/cli/src/cmd/ledger.rs` (two variants; `verify`, `progress_line`, `quarantine`; a unit test)
- Modify: `crates/cli/src/main.rs:629-641` (the dispatch `match`)
- Modify: `crates/cli/tests/ledger.rs` (tests)

**Interfaces:**
- Consumes: `GithubLedger::{verify_with, visibility, quarantine, take_notes}`, `VERIFY_LIMIT`, `Visibility` (B1, Task 4); `fl_exec::stamp::{entry_id, now}`.
- Produces: `ledger::Cmd::Verify { max_commits: usize }`, `ledger::Cmd::Quarantine { file: String, line: u64, by: String, reason: String }`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/cli/tests/ledger.rs`:

```rust
// Spec §3.5: `verify` passes a ledger fl wrote, and names the commit of a
// hand edit.
#[test]
fn verify_passes_a_ledger_fl_wrote_and_names_the_commit_of_a_hand_edit() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    w.fl()
        .args(["github", "ledger", "verify"])
        .assert()
        .success()
        .stdout(contains("verified\t2 commits"));
    let (path, _) = w.ledger_files_in("runs").remove(0);
    let bad = w.fake.hand_commit(&[(path.as_str(), Some("edited\n"))]);
    w.fl()
        .args(["github", "ledger", "verify"])
        .assert()
        .code(1)
        .stdout(contains(format!("BAD\t{bad}\trewrites lines of `{path}`")));
}

#[test]
fn verify_stops_at_its_limit_and_names_the_flag() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    w.fl()
        .args(["github", "ledger", "verify", "--max-commits", "1"])
        .assert()
        .code(2)
        .stderr(contains("walked back 1 commits").and(contains("--max-commits <n>")));
}

// Spec §3.5 check 5: one id on two different lines is not a clean ledger.
#[test]
fn verify_reports_one_id_on_two_lines_and_exits_1() {
    use fl_github::ledger::layout;
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let (_, text) = w.ledger_files_in("runs").remove(0);
    // The published run, about another gate, filed in that gate's own
    // directory: the same id on a different line. Keys stay sorted, so the
    // line is byte-for-byte what fl would write.
    let mut line: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let other = "urn:uuid:00000000-0000-7000-8000-000000000077";
    line["gate"] = serde_json::Value::String(other.into());
    let other_iri = fl_core::Iri::parse(other).unwrap();
    let seg = layout::segment_path(&layout::dir(layout::Area::Runs, &other_iri), 1);
    w.fake
        .hand_commit(&[(seg.as_str(), Some(format!("{line}\n").as_str()))]);
    w.fl()
        .args(["github", "ledger", "verify"])
        .assert()
        .code(1)
        .stdout(contains("SAME ID\t").and(contains("verified\t").not()));
}

#[test]
fn verify_refuses_a_limit_of_no_commits() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["github", "ledger", "verify", "--max-commits", "0"])
        .assert()
        .code(2)
        .stderr(contains("`--max-commits` must be at least 1"));
}

// ⚠ Decision 16: on a repository that is not private,
// `--by` and `--reason` are public for good — warned, then appended.
#[test]
fn quarantine_on_a_repository_that_is_not_private_warns_then_appends() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let (path, _) = w.ledger_files_in("runs").remove(0);
    w.fake.state().repos[0].visibility = "public".into();
    w.fl()
        .args([
            "github",
            "ledger",
            "quarantine",
            &path,
            "1",
            "--by",
            "maintainer",
            "--reason",
            "a test",
        ])
        .assert()
        .success()
        .stderr(contains("warning: acme/widgets is not private: once appended"))
        .stdout(contains(format!("quarantined\t{path}\tline 1\t")));
    assert!(
        w.fake.ledger_files()["quarantine.jsonl"].contains(r#""quarantined_by":"maintainer""#)
    );
}

#[test]
fn quarantine_on_a_private_repository_says_the_text_is_permanent_without_a_warning() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let (path, _) = w.ledger_files_in("runs").remove(0);
    w.fl()
        .args([
            "github",
            "ledger",
            "quarantine",
            &path,
            "1",
            "--by",
            "maintainer",
            "--reason",
            "a test",
        ])
        .assert()
        .success()
        .stderr(
            contains("note: once appended, `--by` and `--reason` are written to the ledger of acme/widgets")
                .and(contains("warning:").not()),
        );
}

// Spec §3.3, §3.6: a quarantined line is skipped and noted by the command
// whose read skipped it.
#[test]
fn a_decision_that_reads_past_a_quarantined_line_notes_it() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let (path, _) = w.ledger_files_in("runs").remove(0);
    w.fl()
        .args([
            "github",
            "ledger",
            "quarantine",
            &path,
            "1",
            "--by",
            "maintainer",
            "--reason",
            "a test",
        ])
        .assert()
        .success();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success()
        .stderr(contains(format!(
            "note: `{path}` line 1 of the GitHub ledger is quarantined (a test)"
        )));
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --test ledger -- verify quarantine`
Expected: FAIL — `verify` and `quarantine` are not subcommands of `fl github ledger`.

- [ ] **Step 3: Implement the commands**

In `crates/cli/src/cmd/ledger.rs`, change the imports to:

```rust
use crate::ctx::Ctx;
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_exec::stamp;
use fl_github::GithubLedger;
use fl_github::ledger::{InitOutcome, VERIFY_LIMIT, Visibility, guidance};
use std::path::Path;
```

add to `Cmd`, after `Init`:

```rust
    /// Walk every commit of `fl/ledger` from its first and check that each
    /// only adds lines or segments; then check that no id is on two
    /// different lines. About one request per commit, plus one per segment.
    Verify {
        /// Stop after walking back this many commits.
        #[arg(long, default_value_t = VERIFY_LIMIT)]
        max_commits: usize,
    },
    /// Mark one line of the ledger for readers to skip. Nothing is removed.
    Quarantine {
        /// The segment, as a path on the branch (`runs/<key>/<n>.jsonl`).
        file: String,
        /// The line, from 1.
        line: u64,
        /// Who decided. Written to the ledger permanently.
        #[arg(long)]
        by: String,
        /// Why. Written to the ledger permanently.
        #[arg(long)]
        reason: String,
    },
```

replace `run`'s `match` with:

```rust
    match cmd {
        Cmd::Init { confirm } => init(ctx, gl, root, confirm.as_deref()),
        Cmd::Verify { max_commits } => verify(gl, max_commits),
        Cmd::Quarantine {
            file,
            line,
            by,
            reason,
        } => quarantine(gl, &file, line, &by, &reason),
    }
```

and add:

```rust
/// How often `verify` says how far it has walked.
const PROGRESS_EVERY: usize = 100;

/// What `verify` says after walking back `n` commits: a line every
/// [`PROGRESS_EVERY`], so a long history never looks like a hang.
fn progress_line(n: usize) -> Option<String> {
    (n > 0 && n % PROGRESS_EVERY == 0).then(|| format!("verify: walked back {n} commits so far"))
}

/// `fl github ledger verify` (spec §3.5). Exit 1 when it found anything.
fn verify(gl: &GithubLedger<'_>, max_commits: usize) -> Result<i32> {
    // A limit of 0 could never verify anything.
    if max_commits == 0 {
        bail!("`--max-commits` must be at least 1: a walk of no commits verifies nothing");
    }
    let v = gl.verify_with(max_commits, &mut |n| {
        if let Some(line) = progress_line(n) {
            eprintln!("{line}");
        }
    })?;
    if let Some(bad) = &v.first_bad {
        println!("BAD\t{}\t{}", bad.commit, bad.what);
    }
    if let Some(same) = &v.same_id {
        println!(
            "SAME ID\t{}\t`{}` line {}\t`{}` line {}",
            same.id, same.first.0, same.first.1, same.second.0, same.second.1
        );
    }
    if v.first_bad.is_none() && v.same_id.is_none() {
        println!(
            "verified\t{} commits\teach only adds, and no id is on two lines",
            v.commits
        );
        return Ok(0);
    }
    println!("walked\t{} commits", v.commits);
    Ok(1)
}

/// `fl github ledger quarantine` (spec §3.6).
///
/// ⚠ Decision 16: decision 2's projection covers the entries, not the
/// text a person writes to publish. Said every time; warned before the
/// append where the repository is not private.
fn quarantine(
    gl: &GithubLedger<'_>,
    file: &str,
    line: u64,
    by: &str,
    reason: &str,
) -> Result<i32> {
    let repo = &gl.repo().full_name;
    match gl.visibility()? {
        // Said before the library checks the arguments, so worded for the
        // append that may not happen.
        Visibility::Private => eprintln!(
            "note: once appended, `--by` and `--reason` are written to the ledger of {repo} \
             permanently; nothing is ever removed from it"
        ),
        Visibility::NotPrivate => eprintln!(
            "warning: {repo} is not private: once appended, `--by` and `--reason` are \
             published permanently — anyone who can read {repo} can read them, and nothing \
             is ever removed from its ledger"
        ),
    }
    let commit = gl.quarantine(&stamp::entry_id(), &stamp::now(), file, line, by, reason)?;
    // A fresh id always adds a line, so `quarantine` names a commit; `None`
    // would mean nothing was added, which the library allows only for an id
    // already present.
    let commit = commit.unwrap_or_else(|| "no commit: the line was already there".into());
    println!("quarantined\t{file}\tline {line}\t{commit}");
    Ok(0)
}
```

and, at the end of `crates/cli/src/cmd/ledger.rs`, its unit test:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // A long walk reports every hundred commits, and only then.
    #[test]
    fn progress_is_reported_every_hundred_commits() {
        for n in [0, 1, 99, 101, 199] {
            assert_eq!(progress_line(n), None, "{n}");
        }
        assert_eq!(
            progress_line(100).as_deref(),
            Some("verify: walked back 100 commits so far")
        );
        assert!(progress_line(200).is_some());
    }
}
```

- [ ] **Step 4: Print what the reads noted**

In `crates/cli/src/main.rs`, replace the final `match cli.command { … }` of `run` (lines 629-640, as Task 8 left it) with:

```rust
    let result = match cli.command {
        Command::Project(c) => cmd::project::run(&store, c),
        Command::Gate(c) => cmd::gate::run(&store, c),
        Command::Transition(c) => cmd::transition::run(&store, c),
        Command::Record(c) => cmd::record::run(&ctx, c),
        Command::Check(c) => cmd::check::run(&ctx, c),
        Command::Finding(c) => cmd::finding::run(&ctx, c),
        Command::Attempt(c) => cmd::attempt::run(&ctx, c),
        Command::Stats(c) => cmd::stats::run(&store, c),
        Command::Manifest(c) => cmd::manifest::run(&store, c, &manifest_binding),
        Command::Github(c) => cmd::github::run(&ctx, c, entry.as_ref().map(|e| e.root.as_path())),
    };
    // What the GitHub ledger's reads noted without refusing — a quarantined
    // line skipped (spec §3.3, §3.6) — once each, whatever became of the
    // command.
    if let Some(gl) = &github_ledger {
        for note in gl.take_notes() {
            eprintln!("note: {note}");
        }
    }
    result
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS.

- [ ] **Step 6: Mutation checks**

Guards in this task:

1. `verify` exits 1 on a departure: return `Ok(0)` at the end → `verify_passes_a_ledger_fl_wrote_and_names_the_commit_of_a_hand_edit` red.
2. `verify` passes `--max-commits` on: call `gl.verify()` instead → `verify_stops_at_its_limit_and_names_the_flag` red.
3. The warning on a repository that is not private: print the `Private` note for both arms → `quarantine_on_a_repository_that_is_not_private_warns_then_appends` red.
4. The note on a private repository: print nothing in the `Private` arm → `quarantine_on_a_private_repository_says_the_text_is_permanent_without_a_warning` red.
5. The notes: delete the `take_notes` loop → `a_decision_that_reads_past_a_quarantined_line_notes_it` red.
6. A shared id is not clean: drop `&& v.same_id.is_none()` from the `verified` condition → `verify_reports_one_id_on_two_lines_and_exits_1` red (it prints `verified` and exits 0).
7. The `SAME ID` line: delete its `println!` → `verify_reports_one_id_on_two_lines_and_exits_1` red.
8. Progress every hundred: change `n % PROGRESS_EVERY == 0` to `n % 10 == 0`, or drop `n > 0 &&` → `progress_is_reported_every_hundred_commits` red.
9. Progress is printed: replace the closure body with `{}` → no test sees stderr at 100 commits (a 100-commit ledger is too slow for a black-box test); the line's content is pinned by mutation 8, and the closure is three lines a reviewer reads.
10. A limit of at least 1: delete the `max_commits == 0` check → `verify_refuses_a_limit_of_no_commits` red (`walked back 0 commits` instead).

The `quarantine` command's no-commit wording is not a guard: a fresh id always adds a line, so the library always names a commit here.

- [ ] **Step 7: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/cmd/ledger.rs crates/cli/src/main.rs crates/cli/tests/ledger.rs
git commit -m "feat(cli): fl github ledger verify and quarantine; print what reads noted

verify exits 1 on a departure or one id on two lines, reports progress
every 100 commits, and stops at --max-commits. quarantine says --by and
--reason are written permanently and warns first on a repository that is
not private (decision 16). Every command prints the lines its ledger
reads skipped. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 11: The pre-flight

Spec §2.4, decision 8, §6.1 step 5, §7 "unreachable in the pre-flight" (rulings 7–10, 22, 24). Before any gate or adapter runs, every decision on a binding with the GitHub ledger checks, in order: visibility (read live — `GithubLedger` keeps the answer for the flush), the mode, the branch, the head's descent from the anchor and the last head seen, and the format (`check_format`), this machine's cut-over, `ensure_publishable(project, None)`, and that the committed manifest carries the ledger's first commit. A failure refuses with `error: refused before any gate or adapter ran: <cause>` and exit 2. The call sites: `record move` (gated or not), `check --record`, `finding reproduce`, `finding verify`, `fl attempt`.

**Blast radius:** those five commands, on a binding with `ledger = "github"` only (`check` only with `--record`); a project without the key, and a plain `check`, are untouched. A decision there now needs GitHub reachable and the manifest committed and current before anything runs — `finding reproduce` already needed the latter for its gate. The fake gains one knob.

**Files:**
- Create: `crates/cli/src/preflight.rs`
- Modify: `crates/cli/src/main.rs:1-6` (`mod preflight;`)
- Modify: `crates/cli/src/cmd/record.rs:117-121`, `crates/cli/src/cmd/check.rs:73`, `crates/cli/src/cmd/finding.rs:164-170` and `:184-186`, `crates/cli/src/cmd/attempt.rs:76-78`
- Modify: `crates/github/src/ledger/read.rs:138-141` (`check_format`'s doc)
- Modify: `crates/cli/src/cmd/manifest.rs` (new `ensure_manifest_carries_root` after `ensure_publishable`; new `manifest_of` after `read`, called at `:80` and `:119`)
- Modify: `crates/github/src/fake.rs` (`State`: `fail_repo_read_after`; route `("GET", ["repos", o, r])` at `:794`)
- Modify: `crates/cli/tests/ledger.rs` (helpers, tests)

**Interfaces:**
- Consumes: `Ctx::github_ledger` (Task 8); `GithubLedger::{visibility, mode, check_format, repo}`; `Outbox::cutover`; `Bindings::ledger_root`; `cmd::manifest::ensure_publishable`.
- Produces:

```rust
// crates/cli/src/preflight.rs
pub fn check(ctx: &Ctx<'_>, project: &ProjectId) -> anyhow::Result<()>;
// crates/cli/src/cmd/manifest.rs
pub fn ensure_manifest_carries_root(store: &RedbStore, project: &ProjectId, node_id: &str, held: &str)
    -> anyhow::Result<()>;
// crates/github/src/fake.rs, State
pub fail_repo_read_after: Option<u32>,
// crates/cli/tests/ledger.rs
const PREFLIGHT: &str;
fn runs(&self, m: &Machine) -> usize;       // gate runs in m's store
fn attempts(&self, m: &Machine) -> usize;   // attempts in m's store
```

- [ ] **Step 1: Add the fake's knob**

In `crates/github/src/fake.rs`, add to `State` after `truncate_trees` (line 254):

```rust
    /// The repository read after this many more answers 500, once. The
    /// tracker's own open reads the repository first, so `Some(1)` fails
    /// the read after it.
    pub fail_repo_read_after: Option<u32>,
```

and at the top of the `("GET", ["repos", o, r]) => {` arm (line 794), before `if s.fail_repo_read {`:

```rust
            if let Some(n) = s.fail_repo_read_after {
                if n == 0 {
                    s.fail_repo_read_after = None;
                    return answer(500, json!({"message": "fake repository failure"}));
                }
                s.fail_repo_read_after = Some(n - 1);
            }
```

- [ ] **Step 2: Write the failing tests**

In `crates/cli/tests/ledger.rs`, add the import

```rust
use fl_core::store::{Catalog, Ledger};
```

and, after the imports:

```rust
/// What every pre-flight refusal starts with (GitHub ledger spec §2.4).
const PREFLIGHT: &str = "refused before any gate or adapter ran: ";
```

Add to `impl World`:

```rust
    /// How many gate runs `m`'s store holds, over every gate.
    fn runs(&self, m: &Machine) -> usize {
        let store = fl_store::RedbStore::open(&m.store()).unwrap();
        store
            .list_projects()
            .unwrap()
            .iter()
            .flat_map(|p| store.list_gates(&p.id).unwrap())
            .map(|g| store.gate_runs(&g.id).unwrap().len())
            .sum()
    }

    /// How many attempts `m`'s store holds.
    fn attempts(&self, m: &Machine) -> usize {
        let store = fl_store::RedbStore::open(&m.store()).unwrap();
        store
            .list_projects()
            .unwrap()
            .iter()
            .map(|p| store.attempts(&p.id).unwrap().len())
            .sum()
    }
```

and the tests (each asserts a phrase only its own check writes):

```rust
// Spec §2.4, §7: no ledger yet — refused, naming `init`, before the move's
// gate runs; the record stays.
#[test]
fn a_move_on_a_repository_with_no_ledger_is_refused_before_its_gate_runs() {
    let w = World::new();
    w.gated();
    w.export();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("has no GitHub ledger yet")));
    assert_eq!(w.runs(&w.one), 0, "no gate ran");
    assert!(
        w.fake
            .issue(1)
            .labels
            .contains(&"fl:record/todo".to_string())
    );
}

// Spec §2.2: an ungated move is flushed too, so it pre-flights like any
// other.
#[test]
fn an_ungated_move_on_a_repository_with_no_ledger_is_refused() {
    let w = World::new();
    w.gated();
    w.export();
    w.fl()
        .args(["record", "move", "1", "--to", "done"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("has no GitHub ledger yet")));
    assert!(
        w.fake
            .issue(1)
            .labels
            .contains(&"fl:record/todo".to_string())
    );
}

// Spec §7, "`ledger_root` missing from the manifest": on this machine a
// manifest committed without the ledger's first commit is refused at the
// first decision, naming the export — so `init`'s instruction cannot be
// lost.
#[test]
fn a_decision_on_a_manifest_without_the_ledgers_first_commit_is_refused_naming_export() {
    let w = World::new();
    w.gated();
    w.export();
    w.init();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(
            contains(PREFLIGHT)
                .and(contains("does not carry the GitHub ledger's first commit"))
                .and(contains("here, then commit it")),
        );
    assert_eq!(w.runs(&w.one), 0);
    w.export();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
}

// The same refusal on a machine that does not author the project: its
// root came from `init --confirm`, because the manifest it imported was
// exported before the ledger existed. It cannot export, so the refusal
// names the authoring machine's export and an import here.
#[test]
fn a_manifest_without_the_first_commit_on_an_importing_machine_names_the_import() {
    let w = World::new();
    w.gated();
    w.export();
    w.init();
    let root = w.fake.ledger_head().expect("the branch");
    let two = w.machine();
    w.fl_on(&two)
        .args(["manifest", "import"])
        .assert()
        .success();
    w.fl_on(&two)
        .args(["github", "ledger", "init"])
        .assert()
        .code(1)
        .stdout(contains(format!("confirm\tfl/ledger\t{root}")));
    w.fl_on(&two)
        .args(["github", "ledger", "init", "--confirm", &root])
        .assert()
        .success();
    w.fl_on(&two)
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("then pull here and run `fl manifest import`")));
    assert_eq!(w.runs(&two), 0);
}

// Spec §2.4, §6.1 step 4: no manifest at all is refused naming the export
// on the store that authors the project.
#[test]
fn a_decision_on_a_project_with_no_manifest_names_the_export() {
    let w = World::new();
    w.gated();
    w.init();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(
            contains(PREFLIGHT)
                .and(contains("there is no manifest at"))
                .and(contains("and commit the file it writes")),
        );
    assert_eq!(w.runs(&w.one), 0);
}

// On a machine that imported the project, a missing manifest names where
// it comes from and the import.
#[test]
fn a_missing_manifest_on_an_importing_machine_names_where_it_comes_from() {
    let w = World::new();
    w.ready();
    let two = w.machine();
    w.fl_on(&two)
        .args(["manifest", "import"])
        .assert()
        .success();
    fs::remove_file(w.repo.path().join(".fl/manifest.json")).unwrap();
    w.fl_on(&two)
        .args(["check", "launch", "--project", "1"])
        .assert()
        .code(2)
        .stderr(contains("It comes from the machine that authors the project"));
}

// Spec §2.4: the manifest must list every gate before a run can reach the
// shared ledger.
#[test]
fn a_check_on_a_project_whose_manifest_lacks_a_gate_is_refused_before_it_runs() {
    let w = World::new();
    w.ready();
    w.fl()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "late",
            "--glob",
            "src/**/*.rs",
            "--program",
            "true",
        ])
        .assert()
        .success();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("gate `late` is not in the manifest")));
    assert_eq!(w.runs(&w.one), 0);
}

// ⚠ Spec §6.1 step 5: a machine that knows the ledger only
// from the manifest has no cut-over until it runs `init` — refused, naming
// it, before anything runs.
#[test]
fn a_machine_with_no_cut_over_is_refused_naming_init() {
    let w = World::new();
    w.ready();
    let two = w.machine();
    w.fl_on(&two)
        .args(["manifest", "import"])
        .assert()
        .success();
    w.fl_on(&two)
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("has no cut-over")));
    assert_eq!(w.runs(&two), 0);
    w.fl_on(&two)
        .args(["github", "ledger", "init"])
        .assert()
        .success();
    w.fl_on(&two)
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
}

// Spec §2.4, §5: visibility is read live, first.
#[test]
fn a_visibility_that_cannot_be_read_is_refused_before_the_gate_runs() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_repo_read_after = Some(1);
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("read the repository's visibility")));
    assert_eq!(w.runs(&w.one), 0);
}

// Spec §2.4: the mode is read before anything runs; rules fl cannot read
// refuse.
#[test]
fn rules_that_cannot_be_read_are_refused_before_the_gate_runs() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_rules_next = true;
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("rules/branches/fl/ledger")));
    assert_eq!(w.runs(&w.one), 0);
}

#[test]
fn a_reproduction_on_a_repository_with_no_ledger_is_refused_before_its_gate_runs() {
    let w = World::new();
    w.gated();
    w.export();
    fs::write(w.repo.path().join("bug"), "").unwrap();
    w.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "a bug", "--by", "reviewer",
        ])
        .assert()
        .success();
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("has no GitHub ledger yet")));
    assert_eq!(w.runs(&w.one), 0);
}

#[test]
fn a_verification_on_a_deleted_ledger_is_refused_before_its_gates_run() {
    let w = World::new();
    w.ready();
    fs::write(w.repo.path().join("bug"), "").unwrap();
    w.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "a bug", "--by", "reviewer",
        ])
        .assert()
        .success();
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .success();
    w.fl()
        .args(["finding", "assign", "2", "--to", "fixer"])
        .assert()
        .success();
    fs::remove_file(w.repo.path().join("bug")).unwrap();
    let before = w.runs(&w.one);
    w.fake.delete_ledger();
    w.fl()
        .args(["finding", "verify", "2"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("fl will not start a new ledger")));
    assert_eq!(w.runs(&w.one), before, "no gate ran");
}

// Decision 8: GitHub is checked before the adapter spends anything.
#[test]
fn an_attempt_on_a_repository_with_no_ledger_is_refused_before_the_adapter_runs() {
    let w = World::new();
    w.gated();
    w.export();
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(2)
        .stderr(contains(PREFLIGHT).and(contains("has no GitHub ledger yet")));
    assert_eq!(w.attempts(&w.one), 0);
}
```

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test -p fl-cli --test ledger`
Expected: every test this step adds FAILS (tests from earlier tasks still pass) — no pre-flight: the gate runs (or the adapter is asked) first, and the flush either publishes or says only that nothing was switched on.

- [ ] **Step 4: Implement the pre-flight**

Create `crates/cli/src/preflight.rs`:

```rust
//! The pre-flight (GitHub ledger spec §2.4): before any gate or adapter
//! runs, a decision whose ledger is the GitHub ledger checks that its
//! evidence can be published — and refuses before anything runs or is
//! spent when it cannot.

use crate::ctx::Ctx;
use anyhow::{Context, Result, bail};
use fl_core::ids::ProjectId;
use fl_core::split::Outbox;
use fl_core::store::Bindings;
use fl_github::GithubLedger;

/// ⚠ Called once by each decision command — `record move`, `check
/// --record`, `finding reproduce`, `finding verify`, `fl attempt` — before
/// its first gate or adapter. Without the GitHub ledger it checks nothing:
/// a local ledger needs no network.
pub fn check(ctx: &Ctx<'_>, project: &ProjectId) -> Result<()> {
    let Some(gl) = ctx.github_ledger else {
        return Ok(());
    };
    checks(ctx, gl, project).context("refused before any gate or adapter ran")
}

/// In the spec's order, cheapest first.
fn checks(ctx: &Ctx<'_>, gl: &GithubLedger<'_>, project: &ProjectId) -> Result<()> {
    // Read live, every decision; the flush uses this answer (spec §5). A
    // visibility that cannot be read is not private, and refuses.
    gl.visibility()?;
    // The mode is never a refusal — detection-only works (decision 12) —
    // but rules fl cannot read are.
    gl.mode()?;
    // The branch, its descent from the anchor and from the last head this
    // machine saw, and its format (§3.5 checks 1, 2 and 7).
    gl.check_format()?;
    // ⚠ §6.1 step 5: a machine whose root came from the manifest has no
    // cut-over until it runs `init`; without one, its decisions would
    // publish nothing and say so only quietly.
    let repo = gl.repo();
    if ctx.store.cutover(&repo.node_id)?.is_none() {
        bail!(
            "this machine has no cut-over for the GitHub ledger of {}: it knows the ledger's \
             first commit, but `fl github ledger init` never ran here, so no decision made on \
             this machine would be published. Run `fl github ledger init` here; it records this \
             machine's cut-over and changes nothing on GitHub",
            repo.full_name
        );
    }
    // No gate IRI the committed manifest does not list reaches the shared
    // ledger.
    crate::cmd::manifest::ensure_publishable(ctx.store, project, None)?;
    // ⚠ §6.1 step 4, §7: the committed manifest carries the ledger's first
    // commit — every other machine's only anchor. Refused here, at the first
    // decision, so the "export and commit" `init` asked for cannot be lost.
    let held = ctx.store.ledger_root(&repo.node_id)?.with_context(|| {
        format!(
            "this machine records no first commit for the GitHub ledger of {}",
            repo.full_name
        )
    })?;
    crate::cmd::manifest::ensure_manifest_carries_root(ctx.store, project, &repo.node_id, &held)
}
```

In `crates/cli/src/cmd/manifest.rs`, after `ensure_publishable`, add:

```rust
/// Spec §6.1 step 4, §7 ("`ledger_root` missing from the manifest"): the
/// project's committed manifest carries the GitHub ledger's first commit
/// `held`, which this store records for the repository `node_id` — the one
/// way every other machine learns the ledger's anchor.
pub fn ensure_manifest_carries_root(
    store: &RedbStore,
    project: &ProjectId,
    node_id: &str,
    held: &str,
) -> Result<()> {
    let root = root_of(store, project)?;
    let m = read(&root)?;
    let carried = m
        .body
        .ledger_root
        .as_ref()
        .map(|r| (r.repository_node_id.as_str(), r.commit.as_str()));
    if carried == Some((node_id, held)) {
        return Ok(());
    }
    let path = root.join(MANIFEST_PATH);
    // ⚠ Only the store that authors the project can export its manifest
    // (`export_manifest` refuses any other): elsewhere the fix is the
    // authoring machine's export and commit, then a pull and an import here.
    if store.imported_hash(project)?.is_some() {
        bail!(
            "{} does not carry the GitHub ledger's first commit {held}, which this machine \
             checks the ledger against. On the machine that authors the project, run `fl \
             manifest export --project {project}` and commit it; then pull here and run `fl \
             manifest import`",
            path.display()
        );
    }
    bail!(
        "{} does not carry the GitHub ledger's first commit {held}, so no other machine can \
         check the ledger against it. Run `fl manifest export --project {project}` here, then \
         commit it",
        path.display()
    )
}
```

This runs on every store. On a machine that imported the manifest, it passes when the imported manifest carried the root this machine holds; it refuses when this machine's root came from `init --confirm` because the manifest it imported carried none — the authoring machine's export has not reached it yet.

Make a missing manifest name the command that writes it (ruling 24). In the same file, add after `read`:

```rust
/// The manifest at `root`, or — when there is none — a refusal naming what
/// writes it: `fl manifest export` on the store that authors `project`
/// (`authoring`), and on any other a restore from git and an import.
fn manifest_of(root: &Path, project: &ProjectId, authoring: bool) -> Result<Manifest> {
    let path = root.join(MANIFEST_PATH);
    if !path
        .try_exists()
        .with_context(|| format!("could not look for {}", path.display()))?
    {
        if authoring {
            bail!(
                "there is no manifest at {}. Run `fl manifest export --project {project}` and \
                 commit the file it writes",
                path.display()
            );
        }
        bail!(
            "there is no manifest at {}. It comes from the machine that authors the project: \
             restore it from git (pull, or check it out), then run `fl manifest import`",
            path.display()
        );
    }
    read(root)
}
```

In `ensure_import_current` (which acts only on an importing store), replace `let m = read(&root)?;` (line 80) with

```rust
    let m = manifest_of(&root, project, false)?;
```

and in `ensure_publishable`'s authoring branch (the `else`), replace `let m = read(&root)?;` (line 119) with

```rust
        let m = manifest_of(&root, project, true)?;
```

**Blast radius of `manifest_of`:** the wording of a missing manifest in every command that checks it — `check`, a gated `record move`, `finding reproduce`/`verify`, `gate run`, `manifest check` — in every mode, and the pre-flight. A manifest that exists reads exactly as before; `tests/manifest.rs`'s `a_missing_manifest_on_an_importing_machine_is_refused_by_path` still finds `.fl/manifest.json` in the new message.

In `crates/cli/src/main.rs`, add `mod preflight;` after `mod ctx;` (line 3).

Call sites:

`crates/cli/src/cmd/record.rs`, after the `if gated { … ensure_import_current … }` block (lines 117-119):

```rust
            // Every move is a decision, gated or not — it is flushed (GitHub
            // ledger spec §2.2) — so its evidence must be publishable before
            // the first gate runs.
            crate::preflight::check(ctx, &record.project)?;
```

`crates/cli/src/cmd/check.rs`, after `crate::cmd::manifest::ensure_import_current(store, &project)?;` (line 73):

```rust
    // A check tied to a record is a decision (GitHub ledger spec §2.2): its
    // evidence must be publishable before the first gate runs. A plain
    // check opens no tracker, so it has no GitHub ledger and the pre-flight
    // checks nothing.
    crate::preflight::check(ctx, &project)?;
```

`crates/cli/src/cmd/finding.rs`, in `Cmd::Reproduce`, after the `if ctx.github.is_some() { … ensure_publishable(…) }` block (lines 168-170):

```rust
            crate::preflight::check(ctx, &f.project)?;
```

and in `Cmd::Verify`, after `crate::cmd::manifest::ensure_import_current(store, &f.project)?;` (line 185):

```rust
            crate::preflight::check(ctx, &f.project)?;
```

`crates/cli/src/cmd/attempt.rs`, after the `let Some(project) = store.get_project(…) else { … };` block (ends line 76):

```rust
    // ⚠ Decision 8: GitHub is checked before the adapter spends anything.
    crate::preflight::check(ctx, &record.project)?;
```

In `crates/github/src/ledger/read.rs`, replace `check_format`'s doc (lines 138-141) with:

```rust
    /// The checked head (checks 1 and 2) whose `format` this fl reads
    /// (check 7), recorded as the last head seen. The
    /// pre-flight asks it before any gate or adapter runs (spec §2.4).
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS — the new tests, and every test of Tasks 8–10 (their worlds are `ready`).

- [ ] **Step 6: Mutation checks**

Run `cargo test -p fl-cli --test ledger` after each. Guards in this task:

1. Visibility first: delete `gl.visibility()?;` → `a_visibility_that_cannot_be_read_is_refused_before_the_gate_runs` red (the flush reads it after the gate ran: runs is 1, and the message lacks `PREFLIGHT`).
2. The mode: delete `gl.mode()?;` → `rules_that_cannot_be_read_are_refused_before_the_gate_runs` red.
3. The branch, anchor and format: delete `gl.check_format()?;` → `a_move_on_a_repository_with_no_ledger_is_refused_before_its_gate_runs` red (the next check refuses instead, with `has no cut-over`, not `has no GitHub ledger yet`).
4. The cut-over: delete the `if ctx.store.cutover(…)` block → `a_machine_with_no_cut_over_is_refused_naming_init` red.
5. The manifest: delete the `ensure_publishable` call (end with `Ok(())`) → `a_check_on_a_project_whose_manifest_lacks_a_gate_is_refused_before_it_runs` red.
6. Nothing without the GitHub ledger: make `check` run `checks` whenever `ctx.github` is set — not compilable as is; instead make the `let Some(gl) = ctx.github_ledger else` arm `bail!("x")` → the mode-A tests in `tests/github.rs` (`records_live_in_github_issues`) red.
7. Call site, `record move`: delete it → `a_move_on_a_repository_with_no_ledger_is_refused_before_its_gate_runs` red. Move it inside `if gated { … }` → `an_ungated_move_on_a_repository_with_no_ledger_is_refused` red (ruling 9).
8. Call site, `check --record`: delete it → `a_check_on_a_project_whose_manifest_lacks_a_gate_is_refused_before_it_runs` red.
9. The manifest carries the root: delete the `ensure_manifest_carries_root` call (end with `Ok(())`) → `a_decision_on_a_manifest_without_the_ledgers_first_commit_is_refused_naming_export` red. Inside it, compare only the commit (drop the node from the tuple) → no test goes red: no manifest names another repository's node with this commit; the node is compared because a root is keyed by it, and the reviewer reads that line. The `with_context` on `ledger_root` is defensive: `check_format` already refused a machine with no first commit (`NoAnchor`).
10. Call site, `finding reproduce`: delete it → `a_reproduction_on_a_repository_with_no_ledger_is_refused_before_its_gate_runs` red.
11. Call site, `finding verify`: delete it → `a_verification_on_a_deleted_ledger_is_refused_before_its_gates_run` red (the gates run, then the flush refuses without `PREFLIGHT`).
12. Call site, `fl attempt`: delete it → `an_attempt_on_a_repository_with_no_ledger_is_refused_before_the_adapter_runs` red.
13. The context line: drop `.context(…)` → every test above red on `PREFLIGHT`.
14. The refusal names what can be done where it runs: invert `if store.imported_hash(project)?.is_some()` (or swap the two `bail!` texts) → `a_decision_on_a_manifest_without_the_ledgers_first_commit_is_refused_naming_export` and `a_manifest_without_the_first_commit_on_an_importing_machine_names_the_import` red.
15. A missing manifest names the export on an authoring store: pass `false` in `ensure_publishable`'s `manifest_of` call → `a_decision_on_a_project_with_no_manifest_names_the_export` red.
16. … and the import on an importing store: pass `true` in `ensure_import_current`'s call → `a_missing_manifest_on_an_importing_machine_names_where_it_comes_from` red.
17. The check for a missing manifest: delete the `try_exists` block in `manifest_of` → both tests above red (`could not read the manifest`).

- [ ] **Step 7: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/preflight.rs crates/cli/src/main.rs crates/cli/src/cmd/record.rs crates/cli/src/cmd/check.rs crates/cli/src/cmd/finding.rs crates/cli/src/cmd/attempt.rs crates/cli/src/cmd/manifest.rs crates/github/src/ledger/read.rs crates/github/src/fake.rs crates/cli/tests/ledger.rs
git commit -m "feat(cli): the pre-flight before every decision on the GitHub ledger

record move, check --record, finding reproduce and verify, and fl attempt
check visibility, the mode, the branch, its descent and format, this
machine's cut-over, the committed manifest and the ledger's first commit
in it before any gate or adapter runs, and refuse naming what to do.
Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain   # after the commit, expect nothing: every file this task changed is in it
```

---

### Task 12: `fl attempt` keeps its own exit code (decisions 14 and 15)

B1 requirement 4 (ruling 12). Decision 14 holds today, but only a unit test pins it. Decision 15 does not: `ctx.ledger.append_attempt(attempt.clone())?` (`attempt.rs:111`) exits 2, before printing the outcome, when the local save fails after the adapter was paid.

**Blast radius:** `fl attempt` only: the outcome is now composed by `settle` from the `Attempt` and printed by `run`, in order, (the same status, duration and excerpt the command printed from the adapter's outcome). `Flushes` gains a constructor; its other users are unchanged.

**Files:**
- Modify: `crates/cli/src/cmd/attempt.rs:94-137` (tail of `run`, `finish`), tests
- Modify: `crates/cli/src/testing.rs` (`Flushes`)
- Modify: `crates/cli/tests/ledger.rs` (test)

**Interfaces:**
- Consumes: `conclude` (Task 6's wording); `fl_core::as_clause`.
- Produces:

```rust
// crates/cli/src/cmd/attempt.rs (private)
enum Said { Stdout(String), Stderr(String) }                           // Debug, Clone, PartialEq, Eq
fn settle(ledger: &dyn Ledger, id: &Iri, attempt: &Attempt) -> (i32, Vec<Said>);   // replaces `finish`
fn unsaved(e: &StoreError) -> String;
// crates/cli/src/testing.rs
impl Flushes { pub fn failing_append() -> Self; }   // every append_attempt fails
```

- [ ] **Step 1: Write the failing tests**

In `crates/cli/src/testing.rs`, add the field and constructor now (the tests need them to compile). Replace the `Flushes` struct with:

```rust
/// A ledger that keeps every decision it is asked to flush — or, built with
/// [`Flushes::refusing`] or [`Flushes::refusing_with`], refuses every one;
/// built with [`Flushes::failing_append`], fails every attempt's save.
#[derive(Default)]
pub struct Flushes {
    refuse: Option<fn() -> StoreError>,
    fail_append: bool,
    pub decisions: RefCell<Vec<Decision>>,
}
```

add to `impl Flushes`:

```rust
    /// Every attempt's local save fails, as a full disk would.
    pub fn failing_append() -> Self {
        Self {
            fail_append: true,
            ..Self::default()
        }
    }
```

and replace `append_attempt` in `impl Ledger for Flushes` with:

```rust
    fn append_attempt(&self, _: Attempt) -> Result<(), StoreError> {
        if self.fail_append {
            return Err(StoreError::Backend("the disk is full.".into()));
        }
        Ok(())
    }
```

In `crates/cli/src/cmd/attempt.rs` tests, replace `an_attempt_that_cannot_be_published_keeps_its_own_exit_code` with:

```rust
    // ⚠ Decision 14: an attempt that ran but could not be published exits
    // with the attempt's own code — never 2, which a script retries on —
    // and the publish failure is a warning.
    #[test]
    fn an_attempt_that_cannot_be_published_keeps_its_own_exit_code() {
        for (status, code) in [(AttemptStatus::Completed, 0), (AttemptStatus::Timeout, 1)] {
            assert_eq!(
                settle(&Flushes::refusing(), &seq_iri(50), &attempt(status)).0,
                code,
                "{status:?}"
            );
        }
    }

    // ⚠ Decision 15: an attempt whose local save fails keeps its own exit
    // code too, prints its outcome first and the failed save after it as
    // an error, and is not flushed — a decision cannot rest on an attempt
    // the local store does not hold.
    #[test]
    fn an_attempt_that_cannot_be_saved_keeps_its_own_exit_code_and_is_not_published() {
        for (status, code) in [(AttemptStatus::Completed, 0), (AttemptStatus::Timeout, 1)] {
            let ledger = Flushes::failing_append();
            let (got, said) = settle(&ledger, &seq_iri(50), &attempt(status));
            assert_eq!(got, code, "{status:?}");
            assert_eq!(
                said.first(),
                Some(&Said::Stdout(format!("{}\t1ms", status.as_wire()))),
                "the outcome comes first: {said:?}"
            );
            assert!(
                matches!(said.last(), Some(Said::Stderr(l)) if l.starts_with("error: the attempt ran")),
                "the failed save comes last, as an error: {said:?}"
            );
            assert!(ledger.decisions.borrow().is_empty(), "{status:?}: nothing flushed");
        }
    }

    #[test]
    fn a_failed_save_says_the_attempt_ran_and_names_its_cause_once() {
        let msg = unsaved(&StoreError::Backend("the disk is full.".into()));
        assert!(
            msg.starts_with(
                "the attempt ran, but it could not be saved to the local store: backend \
                 failure: the disk is full. It is recorded nowhere"
            ),
            "{msg}"
        );
    }
```

In `crates/cli/tests/ledger.rs`, add:

```rust
// ⚠ Decisions 8 and 14: an attempt that ran but could not
// be published keeps its own exit code, warns, and rides with the next
// flush that lands.
#[test]
fn an_attempt_whose_publish_fails_warns_keeps_its_exit_code_and_rides_with_the_next() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_commits = fl_github::ledger::TRIES;
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1)
        .stdout(contains("refused\t"))
        .stderr(
            contains("warning: the attempt ran and is recorded in the local store")
                .and(contains("error:").not()),
        );
    assert_eq!(w.attempts(&w.one), 1);
    assert!(w.ledger_files_in("attempts").is_empty(), "nothing landed");
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1);
    let attempts = w.ledger_files_in("attempts");
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert_eq!(
        attempts[0].1.lines().count(),
        2,
        "the first attempt rode with the second"
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli -- attempt`
Expected: compile errors — `settle`, `Said` and `unsaved` do not exist. (The black-box test already passes against today's `finish`; it pins decision 14 at the command line.)

- [ ] **Step 3: Implement**

In `crates/cli/src/cmd/attempt.rs`, add `use fl_core::store::StoreError;` to the imports (beside `use fl_core::store::{Catalog, Ledger};` — merge into `use fl_core::store::{Catalog, Ledger, StoreError};`), and remove `StoreError` from the tests module's own `use` (it now comes through `use super::*`).

Replace the tail of `run` from `let entry = stamp::entry_id();` (line 96) to the end of `run` with:

```rust
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
    let (code, said) = settle(ctx.ledger, &entry, &attempt);
    for line in said {
        match line {
            Said::Stdout(l) => println!("{l}"),
            Said::Stderr(l) => eprintln!("{l}"),
        }
    }
    Ok(code)
}
```

(The `⚠ Recorded whatever the outcome` comment above `let entry` stays.) Replace `finish` (lines 123-137) with:

```rust
/// One line the command prints, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Said {
    Stdout(String),
    Stderr(String),
}

/// Save the attempt, publish it, and give its exit code and what to print,
/// in order.
///
/// ⚠⚠ Decisions 14 and 15: the attempt already ran and cost what it cost,
/// so the code is the attempt's own — 0 when it completed, 1 otherwise —
/// whatever became of its save or its publish. Exit 2 means "refused"
/// everywhere else, and a script that retries on 2 must never run a paid
/// attempt again. The outcome comes first, whatever became of the save,
/// so a failed save never hides what the attempt did.
fn settle(ledger: &dyn Ledger, id: &Iri, attempt: &Attempt) -> (i32, Vec<Said>) {
    let saved = ledger.append_attempt(attempt.clone());
    let mut said = vec![Said::Stdout(format!(
        "{}\t{}ms",
        attempt.status.as_wire(),
        attempt.duration_ms
    ))];
    if let Some(excerpt) = attempt.output_excerpt.as_deref() {
        said.extend(
            excerpt
                .lines()
                .take(40)
                .map(|l| Said::Stdout(format!("\t| {l}"))),
        );
    }
    match saved {
        // ⚠ Published only once saved: a decision cannot rest on an
        // attempt the local store does not hold.
        Ok(()) => {
            if let Some(warning) = conclude(ledger, id, attempt) {
                said.push(Said::Stderr(format!("warning: {warning}")));
            }
        }
        Err(e) => said.push(Said::Stderr(format!("error: {}", unsaved(&e)))),
    }
    let code = match attempt.status {
        AttemptStatus::Completed => 0,
        _ => 1,
    };
    (code, said)
}

/// What a failed local save says (decision 15).
fn unsaved(e: &StoreError) -> String {
    format!(
        "the attempt ran, but it could not be saved to the local store: {}. It is recorded \
         nowhere, so it was not published either; what it did is printed above, and what it \
         cost was spent",
        fl_core::as_clause(e)
    )
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS, including `a_refused_attempt_is_still_recorded_and_shows_up_in_stats` in `tests/attempt.rs`.

- [ ] **Step 5: Mutation checks**

Guards in this task:

1. Decision 15's code: return `(2, said)` from the `Err(e)` arm → `an_attempt_that_cannot_be_saved_keeps_its_own_exit_code_and_is_not_published` red.
2. The outcome whatever the save: build the outcome lines only in the `Ok(())` arm → `an_attempt_that_cannot_be_saved_…` red (`said.first()` is the error).
3. The outcome before the error: push the `error:` line before the outcome lines → `an_attempt_that_cannot_be_saved_…` red.
4. No flush after a failed save: call `conclude` in both arms → `an_attempt_that_cannot_be_saved_…` red (`decisions` not empty).
5. Decision 14's code: return 2 when `conclude` gives a warning → `an_attempt_that_cannot_be_published_keeps_its_own_exit_code` and `an_attempt_whose_publish_fails_warns_keeps_its_exit_code_and_rides_with_the_next` red.
6. The warning, not an error: print the publish failure as `error:` → `an_attempt_whose_publish_fails_…` red.
7. The cause as a clause in `unsaved`: write `{e}` → `a_failed_save_says_the_attempt_ran_and_names_its_cause_once` red (`full.. It is`).

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/cmd/attempt.rs crates/cli/src/testing.rs crates/cli/tests/ledger.rs
git commit -m "fix(cli): fl attempt keeps its own exit code when its save fails too

Decision 15: the outcome is printed first, a failed local save is an
error message, nothing is flushed (no decision can rest on an unsaved
attempt), and the exit code is the attempt's own. A black-box test pins
decision 14: a failed publish warns, exits with the attempt's code, and
the attempt rides with the next flush. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 13: The `fl stats` rule

Spec §2.5, §2.6 (ruling 11). `fl stats` reads the GitHub ledger, merged with the local store, when no `--db`/`$FL_DB` is given, the store it runs on is this directory's, and the binding names the GitHub ledger. Otherwise — or when GitHub cannot be reached — it counts the local store and, when that store records a GitHub ledger root, says why the count is local only.

**Blast radius:** `fl stats` only. `cmd::stats::run` takes a `Source`. `main` may now open the GitHub tracker for `fl stats` (it did not before): only on a binding with `ledger = "github"`, and a transient failure to open is a fallback, never an error. A project with no GitHub ledger root in its store is untouched: no note, no request.

**Files:**
- Modify: `crates/cli/src/cmd/stats.rs:1-11` (imports), `:30-45` (`run`), new `Source`
- Modify: `crates/cli/src/main.rs` (`run`: `is_stats`, `binding`, `github`, the source, the dispatch; new `local_only_reason`)
- Modify: `crates/cli/tests/ledger.rs` (tests)

**Interfaces:**
- Consumes: `SplitLedger::attempts_for_stats`, `Coverage` (B1); `RedbStore::holds_a_ledger_root`.
- Produces:

```rust
// crates/cli/src/cmd/stats.rs
pub enum Source<'a> {
    Local,                          // the local store is the whole ledger
    LocalOnly(String),              // a GitHub ledger exists but is not read, and why
    Split(&'a SplitLedger<'a>),     // the local store and the GitHub ledger, merged
}
pub fn run(store: &RedbStore, cmd: Cmd, source: Source<'_>) -> Result<i32>;
// crates/cli/src/main.rs
fn local_only_reason(explicit: bool, elsewhere: bool) -> String;
```

- [ ] **Step 1: Write the failing tests**

Add to `crates/cli/tests/ledger.rs`:

```rust
// Spec §2.5: the report merges the local store and GitHub by id — one
// attempt published is counted once — and adds no note.
#[test]
fn stats_in_mode_b_counts_a_published_attempt_once_and_adds_no_note() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1);
    w.fl()
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("attempts: 1\n").and(contains("note: this covers").not()));
}

// ⚠ Spec §2.5: unreachable is not a total.
#[test]
fn stats_when_github_cannot_be_reached_covers_the_local_store_and_says_so() {
    let w = World::new();
    w.ready();
    w.fake.state().down = true;
    w.fl()
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains(
            "note: this covers the local store only, because GitHub could not be read",
        ));
}

#[test]
fn stats_under_db_says_it_covers_the_local_store_only() {
    let w = World::new();
    w.ready();
    w.fl()
        .arg("--db")
        .arg(w.one.store())
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("because --db (or $FL_DB) names the store"));
}

#[test]
fn stats_without_the_ledger_key_on_a_store_that_records_one_says_so() {
    let w = World::new();
    w.ready();
    w.configure(&w.one, false);
    w.fl()
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("names no `ledger = \"github\"`, though this store records"));
}

// A project named by IRI from a directory bound to another store: the
// command runs on the project's store, which is not this directory's, so
// it does not read the GitHub ledger from here — even though this
// directory's binding names one.
#[test]
fn stats_on_another_projects_store_says_it_covers_the_local_store_only() {
    let w = World::new();
    w.ready();
    let iri = {
        let store = fl_store::RedbStore::open(&w.one.store()).unwrap();
        store.list_projects().unwrap()[0].id.iri().as_str().to_string()
    };
    let elsewhere = tempfile::tempdir().unwrap();
    let path = w.one.home.path().join("config/fl/config.toml");
    let mut cfg = fs::read_to_string(&path).unwrap();
    cfg.push_str(&format!(
        "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n\
         tracker = {{ github = \"acme/widgets\", credential = \"env\", ledger = \"github\" }}\n",
        elsewhere.path().canonicalize().unwrap().display(),
        w.one.home.path().join("elsewhere.redb").display()
    ));
    fs::write(&path, cfg).unwrap();
    w.fl()
        .current_dir(elsewhere.path())
        .args(["stats", "--project", &iri])
        .assert()
        .success()
        .stdout(contains("because the project is held by another project's store"));
}

#[test]
fn stats_on_a_project_without_a_github_ledger_adds_no_note_and_asks_nothing() {
    let w = World::bound(false);
    w.gated();
    let before = w.fake.state().requests.len();
    w.fl()
        .args(["stats", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("attempts: 0\n").and(contains("note: this covers").not()));
    assert_eq!(w.fake.state().requests.len(), before, "no request");
}

// A ledger never set up is lasting, not "cannot be read" (spec §2.5): an
// error naming `init`, never a local count.
#[test]
fn stats_on_a_ledger_never_set_up_is_refused_naming_init() {
    let w = World::new();
    w.gated();
    w.fl()
        .args(["stats", "--project", "1"])
        .assert()
        .code(2)
        .stderr(contains("has no GitHub ledger yet"));
}

// Only GitHub that cannot be reached falls back: a missing credential is
// the person's to fix, and refused as it is for every tracker command.
#[test]
fn stats_with_no_credential_is_refused_not_counted_locally() {
    let w = World::new();
    w.ready();
    w.fl()
        .env_remove("FL_GITHUB_TOKEN")
        .args(["stats", "--project", "1"])
        .assert()
        .code(2)
        .stderr(contains("FL_GITHUB_TOKEN or GITHUB_TOKEN"));
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --test ledger -- stats`
Expected: `stats_when_github_cannot_be_reached_…`, `stats_under_db_…`, `stats_without_the_ledger_key_…`, `stats_on_a_ledger_never_set_up_…`, `stats_with_no_credential_…` and `stats_on_another_projects_store_…` FAIL (stats never opens GitHub and never notes); `stats_in_mode_b_counts_a_published_attempt_once_and_adds_no_note` and `stats_on_a_project_without_a_github_ledger_adds_no_note_and_asks_nothing` pass and are pinned by Step 6.

- [ ] **Step 3: Implement `Source`**

In `crates/cli/src/cmd/stats.rs`, replace the imports (lines 1-11) with:

```rust
use crate::refs::{self, Ref};
use anyhow::Result;
use clap::Args;
use fl_core::ids::ProjectId;
use fl_core::log::Attempt;
use fl_core::split::{Coverage, SplitLedger};
use fl_core::store::Ledger;
use fl_core::{Iri, Kind};
use fl_store::RedbStore;
use std::collections::BTreeMap;

/// Where `fl stats` reads attempts from (GitHub ledger spec §2.5, §2.6).
pub enum Source<'a> {
    /// No GitHub ledger is involved: the local store is the whole ledger.
    Local,
    /// The store records a GitHub ledger this command does not read, and
    /// why: the count covers the local store only, and says so.
    LocalOnly(String),
    /// The local store and the project's GitHub ledger, merged.
    Split(&'a SplitLedger<'a>),
}
```

and `run` (lines 30-45) with:

```rust
pub fn run(store: &RedbStore, cmd: Cmd, source: Source<'_>) -> Result<i32> {
    let project = ProjectId(refs::resolve(
        store,
        store.label(),
        Kind::Project,
        &cmd.project,
    )?);
    let (attempts, coverage) = match source {
        Source::Local => (store.attempts(&project)?, Coverage::Complete),
        Source::LocalOnly(reason) => (store.attempts(&project)?, Coverage::LocalOnly { reason }),
        // ⚠ Falls back to the local store only when GitHub could not be
        // read; a damaged or missing ledger is an error.
        Source::Split(split) => split.attempts_for_stats(&project)?,
    };
    for line in report(&attempts, &coverage) {
        println!("{line}");
    }
    Ok(0)
}
```

- [ ] **Step 4: Implement the rule in `main`**

In `crates/cli/src/main.rs`, in `run`:

After `let needs_tracker = cli.command.needs_tracker();` add:

```rust
    let is_stats = matches!(cli.command, Command::Stats(_));
```

Replace the `let binding = if needs_tracker { … } else { None };` block (lines 551-555) with:

```rust
    // `fl stats` reads the GitHub ledger when the store it runs on is this
    // directory's and the binding names the GitHub ledger (GitHub ledger
    // spec §2.5). Under `--db`/`$FL_DB` the entry is not read for `stats`,
    // so `here_binding` is `None` and this is false already.
    let stats_reads_github = is_stats
        && path == bound
        && here_binding
            .as_ref()
            .is_some_and(config::TrackerBinding::github_ledger);
    let binding = if needs_tracker {
        tracker_for(&path, entry.as_ref(), entries, explicit_given)?
    } else if stats_reads_github {
        store_tracker(&path, entries)?
    } else {
        None
    };
```

Replace the `let github = match (&binding, needs_tracker) { … };` block with:

```rust
    // Why `fl stats` could not read GitHub, when it could not reach it.
    let mut unread: Option<String> = None;
    let github = match &binding {
        Some(b) if needs_tracker => Some(open_github(b, cfg.github.as_ref(), &store)?),
        // ⚠ A report falls back to the local store, and says so, when
        // GitHub cannot be reached (§2.5); any other failure is an error.
        Some(b) => match open_github(b, cfg.github.as_ref(), &store) {
            Ok(gh) => Some(gh),
            Err(e)
                if e.downcast_ref::<StoreError>()
                    .is_some_and(StoreError::is_transient) =>
            {
                unread = Some(format!("GitHub could not be read: {e:#}"));
                None
            }
            Err(e) => return Err(e),
        },
        None => None,
    };
```

After the `manifest_binding` block and before the dispatch, add:

```rust
    let stats_source = if !is_stats {
        cmd::stats::Source::Local
    } else if let Some(s) = &split {
        cmd::stats::Source::Split(s)
    } else if let Some(reason) = unread {
        cmd::stats::Source::LocalOnly(reason)
    } else if store.holds_a_ledger_root()? {
        cmd::stats::Source::LocalOnly(local_only_reason(explicit_given, path != bound))
    } else {
        cmd::stats::Source::Local
    };
```

and change the dispatch line for stats to:

```rust
        Command::Stats(c) => cmd::stats::run(&store, c, stats_source),
```

After `store_tracker`, add:

```rust
/// Why `fl stats` reads only the local store of a store that records a
/// GitHub ledger root (GitHub ledger spec §2.5): a count that silently
/// omitted GitHub would read as the total.
fn local_only_reason(explicit: bool, elsewhere: bool) -> String {
    if explicit {
        "--db (or $FL_DB) names the store, so fl reads neither the project's config entry nor \
         its GitHub ledger"
            .into()
    } else if elsewhere {
        "the project is held by another project's store, so fl does not read its GitHub \
         ledger from here"
            .into()
    } else {
        "this project's tracker binding names no `ledger = \"github\"`, though this store \
         records a GitHub ledger"
            .into()
    }
}
```

In `crates/cli/src/cmd/stats.rs`, confirm the old comment ("The local store is the whole ledger here. Once a project binds the GitHub ledger, this reads through a `SplitLedger` …", former lines 42-44) is gone with the old body.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS, including `tests/attempt.rs`'s `stats_over_a_project_with_no_attempts_says_so_rather_than_printing_nothing` (`--db` on a store with no ledger root: `Source::Local`).

- [ ] **Step 6: Mutation checks**

Guards in this task:

1. Stats reads GitHub: set `stats_reads_github = false` → `stats_when_github_cannot_be_reached_covers_the_local_store_and_says_so` red (the note gives the third reason instead) and `stats_on_a_ledger_never_set_up_is_refused_naming_init` red.
2. A transient open falls back: replace the `Err(e) if … is_transient` arm with `Err(e) => return Err(e)` only → `stats_when_github_cannot_be_reached_…` red (exit 2).
3. Only a transient open falls back: drop the `if … is_transient` guard (every error falls back) → `stats_with_no_credential_is_refused_not_counted_locally` red (exit 0 with a note).
4. The note when a root is not read: drop the `holds_a_ledger_root` branch → `stats_under_db_…` and `stats_without_the_ledger_key_…` red.
5. Each reason: swap the `explicit` and `elsewhere` texts → `stats_under_db_…` and `stats_on_another_projects_store_says_it_covers_the_local_store_only` red; return the `--db` text for the last branch → `stats_without_the_ledger_key_…` red.
6. Only this directory's store: drop `&& path == bound` → `stats_on_another_projects_store_says_it_covers_the_local_store_only` red (the `elsewhere` directory's binding names the ledger, so stats reads GitHub through the project's store and adds no note).
7. No request without a GitHub ledger: make `stats_reads_github` ignore `github_ledger` (true for any binding) → `stats_on_a_project_without_a_github_ledger_adds_no_note_and_asks_nothing` red (requests rise).
8. `Source::Split` reads through the merge: replace it with `(store.attempts(&project)?, Coverage::Complete)` → `stats_on_a_ledger_never_set_up_is_refused_naming_init` red.

- [ ] **Step 7: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/cmd/stats.rs crates/cli/src/main.rs crates/cli/tests/ledger.rs
git commit -m "feat(cli): fl stats reads the GitHub ledger, or says why it does not

With no --db, on this directory's store, and a binding that names the
GitHub ledger, stats merges the local store and GitHub. When GitHub
cannot be reached, under --db, on another project's store, or when the
binding no longer names a ledger the store records, it counts the local
store and says why. A ledger never set up is an error naming init. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

## After the last task

- [ ] `git status --porcelain` prints nothing: every change on the branch is committed.
- [ ] Run the trio once more on the whole branch: `cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace`.
- [ ] Confirm no test reached the network: `grep -rn 'api.github.com' crates --include='*.rs'` lists only `DEFAULT_API` and the ignored live tests.
- [ ] Confirm every *Modelled* marker names its live test, and that every name is in B2b's scope: `grep -rn -A3 'Modelled' crates --include='*.rs' | grep -o '\`[a-z_]*\`' | sort -u`.
- [ ] Confirm no label crept into a comment this branch added — only added lines, so labels already in files the plan does not otherwise change are left alone: `git diff origin/main...HEAD -U0 -- '*.rs' | grep '^+' | grep -iE 'task [0-9]|\bB[12]\b|review (finding|focus)|fix round|fix-wave|ruling'` prints nothing.
- [ ] Follow `WORKFLOW.md`: `superpowers:requesting-code-review` on the whole branch, then a pull request against `main`. The pull request names this plan's rulings 1–24 and spec defects 2–9 for the owner (defect 1 is closed by the 2026-10-02 measurement: Free answers `200 []`, detection-only), and says plainly that decision comments, their recovery, the live tests and `docs/github-ledger.md` are B2b's.

## Spec coverage (plan B2a's share)

| spec | where |
|---|---|
| decision 8 (attempts: GitHub checked before the adapter; kept and published next time) | Task 11 (pre-flight in `fl attempt`), Task 12 (the black-box ride-along) |
| decision 12 (detection-only works; the mode never refuses) | Task 3 (`mode()` reads Free's measured `200 []` as detection-only), Task 11 (the pre-flight reads it and passes) |
| decision 14 (an unpublished attempt keeps its own exit code, warning) | Task 12 |
| decision 15 (an unsaved attempt keeps its own exit code; outcome first; error message) | Task 12 |
| decision 16 (quarantine says the text is permanent; warns when not private) | Task 10 |
| §1.5 the `ledger` key; any other value names it | Task 7 |
| §1.2, §2.1, §2.6 `SplitLedger` bound in `Ctx`; a plain check stays local | Tasks 8–9 |
| §2.4 pre-flight before any gate or adapter, in order; refuses before anything runs | Task 11 |
| §2.5, §2.6 the `fl stats` rule | Task 13 |
| §3.3, §3.6 a skipped quarantined line is reported | Task 10 (notes) |
| §3.5 `verify` by hand; check 5 across directories | Task 4, Task 10 |
| §3.6 quarantine command | Task 10 |
| §6.1 steps 1–9 (`init`: the key, the `fl` and `fl/ledger/…` branches, creation, the root, every machine's cut-over, recovery and confirmation, guidance) | Task 5, Task 8 (B1 for the library) |
| §6.2 the mode stated by `init` and `whoami` | Task 8 |
| §7 rows: pre-flight unreachable; `ledger_root` missing from the manifest; unknown `ledger` value; attempt flush warning | Tasks 7, 11 (ruling 22), 12 |
| §8.3 battery: attempt pre-flight refuses before the adapter; an attempt whose flush fails is published by the next | Tasks 11–12 |
| B1 requirements 1, 2, 3, 4, 5, 6, 8, 9, 10, 11 | Tasks 1, 2, 5+8, 12, 10, 4, 3+4, 3, 3, 6 |
| §4 comments and recovery, §8.4 live tests (B1 requirement 7), §9 docs | plan B2b |
