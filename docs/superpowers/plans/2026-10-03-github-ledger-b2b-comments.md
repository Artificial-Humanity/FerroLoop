# GitHub ledger, plan B2b — decision comments, their recovery, the live tests and the docs

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Post one comment per decision on the issue it concerns, rendered from what the ledger holds and safe to show (§4); recover missing comments with `fl github ledger comment <item>`; close the B1 requirement-7 shapes the live tests confirm; write the live tests; and write `docs/github-ledger.md` with the cost, setup and limits it owes.

**Architecture:** `fl-github` gains `ledger/render.rs` (pure: escaping, the marker, the header, the table, excerpts in `<details>` on a private repository, the 60,000-byte cap, and the view of a decision over the entries found for it), `ledger/comment.rs` (posting a comment, following a transferred issue, listing every page of an issue's comments for markers), and reads that keep who wrote a line and where (`published_decisions`, `commit_of`, `runs_of`). The fake serves issue comments, paged, and transferred issues. `fl-cli` wraps the bound ledger in a `Witness` that remembers each decision whose flush landed; each decision command posts that decision's comment after its state change — saying whether it completed — and a failure to post is a warning naming the recovery command. `fl github ledger comment` renders every missing comment from the ledger. Two small library fixes the live tests pin: an empty repository's 409 and a GraphQL timeout on the commit.

**Tech Stack:** Rust 2024 (`rust-version = "1.98"`), redb 4.3, serde/serde_json, clap 4, ureq 3, base64 0.22, assert_cmd/predicates for black-box tests. No new crates.

**Spec:** `docs/superpowers/specs/2026-09-30-github-ledger-design.md` at `2a279d4` — especially decisions 2, 11, 12, 14; §3.4, §4 (all of it), §5, §6.3, §7, §8.1, §8.3, §8.4, §9. Plan B2a (`2026-10-02-github-ledger-b2a-binding.md`, merged as PR #22) defines `Ctx::github_ledger`, the pre-flight, `cmd/ledger.rs` and the black-box world in `crates/cli/tests/ledger.rs`; its header's "Plan B2b — scope" is this plan's scope. Plan B1's "B2 requirements from the final review" item 7, and the comments/recovery/docs half of item 12, are this plan's.

**Branch:** `ferris/github-ledger-b2b`, off `ferris/github-ledger-plan-b2b` (main at `2a279d4` + this plan), or off `main` once that is merged.

---

## Global Constraints

- Verification trio, all green before every commit: `cargo fmt --all --check`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`. Each task runs `cargo fmt --all` first, so code blocks here need not be in rustfmt's exact layout; lines stay within 100 columns.
- Unit tests live in `#[cfg(test)] mod tests` inside the module they test; black-box CLI tests live in `crates/cli/tests/`.
- **No test contacts the network**, except the live tests in `crates/github/tests/live.rs`, which are `#[ignore]`d and run by hand. GitHub is otherwise the in-process fake (`fl_github::fake::FakeGithub` on `127.0.0.1`, reached by the binary through `FL_GITHUB_API_URL`).
- `fl-core` stays pure: "No IO, no async, no clock, no network" (`crates/core/src/lib.rs:1`). This plan does not touch `fl-core` or `fl-exec`.
- Spec values, verbatim: "Order: the ledger commit, then the state change, then the comment. The evidence is the ledger commit, not the comment" (§4.1); "A finding decision comments on the finding's issue; a move, a `check --record` and an attempt on the record's issue. A transferred issue gets it at its current location. One move is one comment" (§4.1); "Cells are escaped for `|`, newlines and backticks" (§4.2); "Excerpts only on a private repository, in a `<details>` block inside a fenced code block whose fence is longer than any backtick run in the text" (§4.2); "Every other user-supplied string has markdown and HTML escaped, and `@` and `#` neutralised" (§4.2); "The body is at most 60,000 bytes of UTF-8 … truncating excerpts first and saying so, so the render always fits" (§4.2); "Each comment carries `<!-- fl:decision {"id":"<decision id>"} -->`; the command lists the issue's comments, every page, and posts only decisions with no marker" (§4.3); "a comment someone edits still counts as posted" (§4.3); "Decisions made before the GitHub ledger was switched on get no retroactive comments" (§4.3); "Refused decisions are flushed too, and their comment says they were refused" (decision 11); decision 17: "A decision comment that fails to post is a warning, not a refusal … fl prints a `warning:` naming `fl github ledger comment <item>` and exits with the command's own code"; "A secondary rate limit is not provoked live" (§8.4).
- Spec invariants, verbatim: "The flush — the ledger commit — comes before the state change it supports"; "the local store keeps every run"; decision 14: "Exit 2 means 'refused' everywhere else, and a script that retries on it must never re-run an attempt that was already paid for"; "Every core feature must work on GitHub Free" (decision 12).
- Every existing configuration keeps working: a project with no `ledger` key behaves exactly as before — no flush, no comment. Every existing test passes, except the assertions a task changes on purpose (named in that task).
- `snake_case` on every wire (`crates/core/src/wire.rs`).
- **Every guard gets a mutation check**: revert it, watch the named test go red, restore. Each task's mutation step lists EVERY guard the task adds or moves — including **ordering** (what must happen before what), **each conjunct** of a compound condition, **every match arm** that must honour an input, and **each phase** of a loop. A guard missing from that list is a plan defect, so a reviewer checks the list against the diff. Where a line looks like a guard but is not (no input can tell it apart), the mutation step says so and why.
- **A unit-test filter in a mutation step names the full module path** (`cargo test -p fl-github --lib ledger::render::tests::`), or it matches nothing and proves nothing. A black-box filter names the test file (`cargo test -p fl-cli --test ledger -- <name>`). **At most one filter goes before `--`**: cargo refuses a second; several filters go after `--`, where the test harness takes them all.
- **A test never asserts with a substring another code path also produces.** Each task names the unique phrase it asserts. The warning a failed comment gives starts `warning: the decision's comment was not posted on `, which no other path writes.
- **No plan names, task numbers or review labels in code comments** ("Task 9", "B2b", "review finding 4", "Ruling:"). Cite the spec by section, or say the reason.
- **A *Modelled* marker this plan adds names the live test that confirms it**, with exactly one exception the owner allowed: the GraphQL timeout in `judge`, whose marker says it is modelled from GitHub's documented error shape and has no live test. A fake behaviour no live test measures is marked "Unmeasured", never "Modelled". Task 12's check enforces both.
- **A change to shared plumbing states its blast radius** — the readers and writers it reaches — in the task that makes it.
- Line numbers cite `2a279d4`, and an earlier task's edits shift them. Find the named item, not the number.
- Commits are authored by the machine account (`WORKFLOW.md`) and end with `Co-authored-by: Ferris <Ferris@artificialhumanity.io>`. Stage explicit paths — never `git add -A` — and after each commit run `git status --porcelain`, which must print nothing.
- The repository is public: no machine paths, host names, user names or lab names in code, tests, messages or this plan. A leak scan reads the machine's names at run time (`hostname`, `$USER`, `$HOME`) and never writes a literal one down. Fake repositories are `acme/widgets` (and `acme/other` for one another repository owns).
- **Live tests:** the token is read from `FL_GITHUB_TOKEN`, then `GITHUB_TOKEN` (or the App), and never appears in argv or in output — no test prints a credential, a header or a client. Every live test is safe to re-run against a ledger earlier runs left behind: a ledger branch under a ruleset cannot be deleted, so no live test depends on starting from nothing. A ledger live test whose resource variable is unset skips, printing which variable is missing; it does not fail. Leftover `fl-live/root` branches and hand-edit commits on the throwaways are accepted.

## Review Focus

1. **A comment that fails to post after a passing `check --record`, or after a paid attempt.** The command keeps its own exit code (0 or 1) and warns, naming `fl github ledger comment <n>` — never exit 2, which CI reads as a failed check and a script retries. Task 7 (`a_check_whose_comment_fails_keeps_the_checks_own_exit_code`), Task 8 (`an_attempts_comment_that_fails_keeps_the_attempts_exit_code`).
2. **An excerpt or a name holding a marker, `</details>`, a run of backticks, `@` or `#`.** It must not close its block, forge a marker that recovery would trust, notify anyone or link an issue. Task 3 (`excerpts_show_only_on_a_private_repository_each_in_a_fence_longer_than_its_backticks`, `a_hostile_decision_id_gets_no_marker_and_nothing_closes_a_comment`), Task 6 (`a_marker_in_a_comment_fl_did_not_write_does_not_count`, `posted_reads_every_page_and_the_first_marker_of_fls_own_comments`), Task 10 (`a_gate_named_with_markup_is_escaped_in_its_comment`).
3. **Recovery on an issue whose comments run to several pages, or whose page fails.** A marker on page 2 still counts — no duplicate; a page that fails is an error, never "none posted, so post them all". Task 9 (`comment_finds_its_markers_on_every_page`, `a_comment_page_that_cannot_be_read_posts_nothing`).
4. **A repository that is not private.** No comment and no published line carries an excerpt, an error's detail, an absolute path, `$HOME` or the host name — and the scan is shown to see a leak on a private repository, so it cannot pass by looking at nothing. Task 10 (`comments_and_lines_on_a_repository_that_is_not_private_hold_no_path_home_or_host_name`).
5. **A move whose state change fails after its flush.** The decision is on the ledger; the comment says the state change did not complete; the command still exits 2 with the tracker's error, as today. Task 7 (`a_move_whose_state_change_fails_after_its_flush_says_so_in_its_comment`).

## Rulings this plan makes

The spec is silent or ambiguous on these. Each says why, and what it costs if wrong.

1. **A live comment is rendered from the flushed `Decision` and the local copies of the entries it rests on, projected by decision 2 — the very lines the flush published — not read back from GitHub.** Recovery reads them from the ledger. Both go through one renderer, and a test pins that they render the same body. Reading back would cost a head read, a listing and a download per grown segment after every state change, each one a new way for the comment to fail. *If wrong:* one case shows: when an entry's id is already on the ledger with different content, the append takes it as published (B2a ruling 13's limit), so the live comment shows the local copy and a recovered one shows the ledger's. Every other published copy is the projection of the local one.
2. **Decisions reach the comment step through `Witness`, a decorator over the bound ledger that remembers each decision whose flush landed** — not through new fields on `fl-exec`'s reports. A refused reproduction and a state change that fails after its flush return `Err` from `fl-exec` after flushing; the witness still holds their decision, so their comment can be posted (decision 11, §4.2). No `fl-exec` signature changes. *If wrong:* a future command that flushes twice posts two comments, which §4.1 allows.
3. **A comment that fails to post is a `warning:`, and every decision command keeps its exit code** — spec decision 17 (owner, 2026-10-03), with §7's lead-in and comment row amended to match. The decision and any state change stood: exit 2 means "refused", CI reads it as a failed check, and a script that retries would repeat a decision — and for an attempt, re-spend (decision 14's reason). The warning names the issue and `fl github ledger comment <n>`, and says what stands: "The decision and its state change stand" only when a state change was made and completed, else "The decision stands" (ruling 26). `fl github ledger comment` itself, the recovery tool, still exits 2 when a read or a post fails. *If wrong:* a script that reads only exit codes does not notice a missing comment; the ledger still holds the decision.
4. **"Whether the state change completed" (§4.2) is whether the command's `fl-exec` call returned `Ok` after its flush.** When it did not, the comment is still posted, saying the state change did not complete, and the command then exits 2 with the tracker's error as today. A recovered comment carries no such line: the ledger does not record it. *If wrong:* message text.
5. **The live comment posts on the bound repository's issue directly; only recovery resolves where the issue is now**, following GitHub's redirect for a transferred issue. The tracker refuses a moved item before any decision about it is made, so the live path meets a transfer only in the seconds between the state change and the comment — the post fails, and the warning names recovery. *If wrong:* such an issue gets its comment only when recovery runs.
6. **Recovery takes the item's project from fl's block in the issue at its current location**, not from the tracker, which refuses a moved item. The block also proves the issue is an fl item. *If wrong:* a transferred issue whose block was removed cannot be recovered; its body has no fl block to read.
7. **Recovery finds a decision's runs through the local catalog** (§4.2): a move's or check's transitions' gates, a reproduction's gate, and for a verify every gate of the project — its passing neighbours are named nowhere else in its outcome. All candidate directories are read at one head, in one listing. A run not found is named under the table. *If wrong:* a verify on a project of many gates reads one directory per gate; a gate removed from the catalog since shows its run as not found.
8. **Escaping:** `&`, `<`, `>` become entities; `\`, `` ` ``, `*`, `_`, `[`, `]`, `(`, `)`, `~`, `|`, `$` are backslash-escaped (`$` because GitHub renders `$…$` as math). `!` is not: it is markdown only before `[`, which is escaped, so `![` cannot form an image, and `<!--` already reads `&lt;!--`. A newline is `<br>`. `@` and `#`, and the `-` of `GH-` (any case), are each followed by a zero-width space written as `&#8203;`, so `@name` no longer mentions, and `#1` and `GH-1` no longer link. Inside an excerpt's `<summary>` — an HTML block, where markdown is not read — a name gets the entities and the zero-width spaces only, and a newline becomes a space. Each cell, each name in the header and each summary is capped at 200 bytes escaped, ending in `…`. *If wrong:* if GitHub ignored the zero-width space, a name could mention or link — the live round trip reads GitHub's rendered HTML of such a comment to check (ruling 22).
9. **A comment marks a decision only when its author is trusted for that item: the login fl posts as now** (`GithubLedger::by`, read once per command, as `fl github whoami` reads it) **or the `by` of any decision line filed under the item** (`Published::by`, which `published_decisions` already returns, so the set costs no request). Several developers usually hold a token each: a comment machine A posted as `alice` counts when machine B recovers, because `alice` wrote a decision line there. Within a trusted comment, the first well-formed marker line outside a fenced block counts, wherever it is in the body — so a maintainer's edit, which keeps the author, never makes recovery post again (§4.3's "a comment someone edits still counts as posted" holds even when the edit pushes the marker down), and a marker quoted in an excerpt marks nothing. A listed comment with no author is no one's. An account that never wrote a decision under the item — a passer-by, or a person who only ran recovery under another credential — cannot mark one. Only recovery reads markers: the live path posts once, after its flush, and lists nothing, so there is no second set to keep in step. **Known limit:** a collaborator who has written a decision under an item can mark any other decision there as posted and so stop its recovery; anyone a ledger line names held Contents: write and could alter the ledger itself, so this grants no new power. *If wrong:* a comment posted under a credential that wrote no decision under that item (the App after a token, before the App's first decision there) is not recognised, and recovery posts that decision again — a harmless duplicate.
10. **Over 60,000 bytes, excerpts are cut first, each to the largest equal share that fits; then left out; then the table's last rows dropped** — each step saying so and pointing at the ledger commit. Entries not found are named up to 20, then counted. *If wrong:* a decision of many long excerpts shows each one short rather than some whole.
11. **The link to the ledger commit comes from `Flushed::commit` when posted live, and from GitHub's blame of the decision's line when recovered** (one GraphQL request per comment recovered). A commit id that is not 40 hex digits gets no link: the header says the ledger holds it. *If wrong:* a blame that fails leaves a recovered comment without its link.
12. **A comment is posted only for a flush that named a commit.** A flush that published nothing has nothing for the comment to point at. *If wrong:* none — the pre-flight already refuses the one case that publishes nothing (no cut-over).
13. **An empty repository's 409 on a ref read is a refusal that says to push a first commit**, not "retry": GitHub keeps no branch until a repository has a first commit. Only a 409 whose message says "Git Repository is empty" is read this way; any other 409 stays the error it was. *Modelled* — confirmed by live test `an_empty_repository_is_refused_naming_a_first_commit`. *If wrong:* message text.
14. **A GraphQL error on the commit whose message says "timeout" is an unknown landing**, read again (§3.2 step 5) — unless an error also says `FORBIDDEN`, which stays a refusal. Modelled from GitHub's documented error shape ("This may be the result of a timeout"), with no live test: provoking it takes a request built to run past GitHub's limit. The owner allowed it as the single exception to "every *Modelled* marker names a live test". *If wrong:* a timeout worded otherwise stays an error that marks nothing published, as today.
15. **The live tests keep their place across re-runs with a branch `fl-live/root` at the ledger's first commit**, made by the first run on a repository. Later runs record that root on a fresh machine and run `init` through its "already set up" path — never `--confirm`, whose walk to the first commit grows with every run and stops at 1,000 commits. *If wrong:* `init`'s create path is exercised live only by the first run against a repository; the fake covers it on every run.
16. **`a_branch_under_the_ledger_branch_is_found` probes `git/matching-refs` with `fl-live/`**, which holds `fl-live/root`: git cannot hold a branch under `fl/ledger/` beside `fl/ledger`, so the live test checks the listing's prefix rule on another prefix, and that `fl/ledger/` answers `200 []`. *If wrong:* none; the prefix rule is the same.
17. **The live test of a refused deletion sends `DELETE` through `ureq` in the test itself.** fl's client has no `DELETE` and production needs none. *If wrong:* the test carries four headers fl's client also sends.
18. **`fl github ledger comment <item>`** takes a handle (`41`, `#41`), `owner/repo#41` or an issue URL; the item must be an issue of this repository — an alias IRI is refused, because a decision is filed under the IRI it recorded. It requires `ledger = "github"`, like every `fl github ledger` command (B2a ruling 6). It prints `moved\t<url>` when the issue was transferred, `posted\t<decision id>` per comment, `skipped\t<id>\t…` on stderr for a decision whose id fl does not write (ruling 21), and `comments\t<n> posted, <m> already there`; it exits 0, 1 when it skipped one, or 2 when a read or a post fails (a re-run posts the rest).
19. **The private-repository line of `quarantine` is prefixed `permanent:`**, a prefix nothing else uses, so it is not taken for a read's `note:`.
20. **The cost of a decision is documented from a test that counts the fake's requests by kind** for a steady-state `check --record`; the spec's §3.4 is corrected to the same breakdown. *If wrong:* the test fails first, and the doc moves with it.
21. **A decision id is written into a comment only when it is one fl writes**: `urn:uuid:` followed by a lowercase, hyphenated UUID (8-4-4-4-12 hex) — the only form `fl_exec::stamp::entry_id` mints. No `fl:` form is accepted: no code writes one. Anything else — `x:a--><b>`, say, which `Iri::parse` accepts and a hand-written ledger line can carry — gets no marker and no comment: `render` writes no marker for it, the live path refuses to post it (a warning of its own, naming `fl github ledger verify` and quarantine, never the recovery command, which would skip it too), and recovery skips it, saying so on stderr, and exits 1. Such an id can then never put `--`, `<` or `>` inside the comment's HTML comment, and the comment's header stays bounded, so "always fits" holds. Rendering it without a marker and posting anyway was rejected: recovery could never see it posted and would post it on every run. *If wrong:* a decision with an id fl never mints gets no comment until someone quarantines its line.
22. **The live round trip reads GitHub's rendered HTML** of the comment it posts (GraphQL `IssueComment.bodyHTML`, which needs no Accept header fl's client does not send) and asserts no mention, no issue link and no link to the record's issue was made for its `@`, `#` and `GH-` text. A control comment posted raw (`#<n>` and `GH-<n>` for the record's own issue) must render as links, so the detector cannot pass by recognising nothing. No raw mention is posted: that could notify a real account. *If wrong:* GitHub renames its link classes; the control then fails loudly.
23. **The live force-update test refuses to try unless the credential cannot bypass the ruleset**: it reads every ruleset `rules/branches/fl/ledger` names (`GET /repos/{repo}/rulesets/{id}`) and requires `current_user_can_bypass == "never"` — absent or anything else panics before any write. Then GitHub must refuse both the force update and the deletion, and the head must be unchanged. A credential that could bypass would make the test destroy the throwaway's ledger or pass vacuously; this makes it do neither. *If wrong:* a plan that hides `current_user_can_bypass` from a reader stops the test before it writes — a failure, never a destroyed ledger.
24. **A ledger live test whose resource is unset skips, printing the variable** (owner, 2026-10-03). The tracker's three live tests keep failing on an unset `FL_GITHUB_LIVE_REPO`, as they do today.
25. **Each live `Live` files its entries under a record of its own** (`issues/<n>` with `n` drawn from a fresh UUIDv7; the issue need not exist), so a re-run never re-reads the growing decisions directory of the runs before it. *If wrong:* none; the ledger never resolves the issue.
26. **A failed comment's warning says what stands from the outcome and whether its state change completed**: "The decision and its state change stand" when a move was allowed, a reproduction accepted or a verify closed and the state change completed; "The decision stands" otherwise — a refused move, a check, an attempt, or a state change that failed. *If wrong:* message text.

## Spec defects this plan found

1. **§7's header against decision 14 — closed by the owner (2026-10-03).** "Every error exits 2 … except the two attempt rows" covered "a comment fails to post". The owner added decision 17, and §7's lead-in and comment row now match it (edited with this plan). Ruling 3.
2. **§3.4 "about three requests".** Counted against the code: four for the pre-flight (visibility, rules, head, `format`), four for the flush (head, listing, identity, commit), one per segment downloaded, one for the comment — about ten. Corrected in §3.4 by Task 13, pinned by a test.
3. **§4.2 "whether the state change completed" against §2.2's error paths.** `fl-exec` returns an error when a state change fails after its flush, so the command has no report to read the completion from. Ruling 4.
4. **§4.1 "A transferred issue gets it at its current location".** The live path cannot meet one: the tracker refuses a moved item before the decision. Only recovery can. Ruling 5.
5. **§4.2 "finding its runs on the branch uses the local catalog's gates for those transitions".** A verify's outcome names its reproduction and its regressions, not its passing neighbours, and names no transition. Ruling 7.
6. **§4.2 neutralises `@` and `#` only.** GitHub also links `GH-<n>` (neutralised here too, ruling 8) and a full issue URL. A URL in a gate name is still auto-linked, and a link from a comment adds a cross-reference to that issue's timeline. Stated as a limit in `docs/github-ledger.md`.
7. **§3.1/§4.3: decisions are filed under the IRI they recorded.** After a repository rename, an item's current URL keys a different directory; recovery by the current number finds nothing older. Stated as a limit; the old URL still works.
8. **§8.4 is silent on re-running.** A ledger under a ruleset cannot be deleted, and `--confirm`'s walk grows with every run. Ruling 15.
9. **B1 requirement 7's "a branch under the ledger branch is found"** names a state git forbids beside `fl/ledger`. Ruling 16.
10. **B1 requirement 7's "the shape of a GraphQL timeout"** cannot be measured live without a request built to exceed GitHub's limit. Ruling 14; the owner allowed it as the one *Modelled* marker with no live test (2026-10-03).
11. **§4.3 does not say whose comment carries a marker.** Read literally, anyone's would count, and anyone who can comment could suppress recovery with a decision id read from a public ledger. Ruling 9 counts only comments by fl's login or by an account that wrote a decision under the item.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/github/src/ledger/render.rs` | create | a decision comment: escaping, marker, header, table, excerpts, cap; the view over the entries found |
| `crates/github/src/ledger/comment.rs` | create | post a comment; where an item's issue is now; the decisions its comments mark |
| `crates/github/src/ledger/read.rs` | modify | `located` keeps who wrote a line and where; `published_decisions`, `commit_of`, `runs_of` |
| `crates/github/src/ledger/mod.rs` | modify | the two new modules; re-exports |
| `crates/github/src/ledger/git.rs` | modify | `branch_head`: an empty repository's 409 |
| `crates/github/src/ledger/append.rs` | modify | `judge`: a timeout is an unknown landing |
| `crates/github/src/ledger/verify.rs` | modify | "1 commit", not "1 commits" |
| `crates/github/src/ledger/init.rs` | modify (tests) | a sibling `fl/ledgers`; no tree after a refusal; an empty repository |
| `crates/github/src/fake.rs`, `fake_git.rs` | modify | paged issue comments; transferred issues; an empty repository; a timeout after a commit |
| `crates/github/tests/live.rs` | modify | the ledger's live tests |
| `crates/cli/src/ctx.rs` | modify | `Flush`, `Witness`, `Ctx::witness` |
| `crates/cli/src/comment.rs` | create | the comment after a decision; the catalog's view of a project |
| `crates/cli/src/main.rs` | modify | `mod comment`; the witness over the split ledger |
| `crates/cli/src/cmd/record.rs`, `check.rs`, `finding.rs`, `attempt.rs` | modify | post each decision's comment after its state change |
| `crates/cli/src/cmd/ledger.rs`, `github.rs` | modify | `fl github ledger comment`; `permanent:` |
| `crates/cli/src/config.rs` | modify (tests) | `ledger = true` |
| `crates/cli/src/testing.rs` | modify | `Flushes::publishing_nothing` |
| `crates/cli/tests/ledger.rs` | modify | comments, recovery, the disclosure scan, the cost |
| `docs/github-ledger.md` | create | the GitHub ledger, for a person |
| `docs/github-tracker.md`, `docs/README.md`, `docs/sharing-gates.md` | modify | links; what stays local; Contents permission; `ledger_root` |
| `docs/superpowers/specs/2026-09-30-github-ledger-design.md` | modify | §3.4's cost (decision 17 and §7 were edited with this plan) |
| `docs/superpowers/specs/2026-09-23-identity-and-store-roles-design.md` | modify | §0.1's mode B row points to the doc |

---

### Task 1: Wording, and the test gaps PR #22 left

Items parked from PR #22. No behaviour changes but two message texts; the rest closes test gaps around code B2a already shipped.

**Blast radius:** `verify_with`'s limit refusal (read by `fl github ledger verify` only) and `quarantine`'s private-repository line (stderr of `fl github ledger quarantine` only). `World::bound`'s helpers in `crates/cli/tests/ledger.rs` gain `only_file_in`.

**Files:**
- Modify: `crates/github/src/ledger/verify.rs` (the `if chain.len() >= limit` refusal in `verify_with`; tests)
- Modify: `crates/github/src/ledger/init.rs` (tests only)
- Modify: `crates/cli/src/cmd/ledger.rs` (the `Visibility::Private` arm in `quarantine`)
- Modify: `crates/cli/src/cmd/attempt.rs` (`EXCERPT_LINES`; `settle`; tests)
- Modify: `crates/cli/src/config.rs` (tests only)
- Modify: `crates/cli/tests/ledger.rs`

**Interfaces:**
- Consumes: `GithubLedger::verify_with`, `GithubLedger::init`, `settle` (`cmd/attempt.rs`), `load_text` (`config.rs` tests).
- Produces: `const EXCERPT_LINES: usize = 40` in `cmd/attempt.rs`; `World::only_file_in(&self, area: &str) -> (String, String)` in `crates/cli/tests/ledger.rs`.

- [ ] **Step 1: Write the failing tests**

In `crates/github/src/ledger/verify.rs`, inside `mod tests`, after `a_history_longer_than_the_limit_is_refused_naming_the_flag`, add:

```rust
    // One commit is "1 commit", not "1 commits".
    #[test]
    fn a_limit_of_one_commit_is_named_in_the_singular() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        publish(&l, 1);
        let err = l.verify_with(1, &mut |_, _| {}).unwrap_err().to_string();
        assert!(err.contains("walked back 1 commit from the head"), "{err}");
    }
```

In `crates/github/src/ledger/init.rs`, inside `mod tests`, in `init_refuses_a_branch_under_fl_ledger_before_creating_anything`, after `assert_eq!(fake.state().git.commits.len(), 1, "no commit was created");`, add:

```rust
        assert_eq!(fake.state().git.trees.len(), 1, "no tree was created");
```

and after that test, add:

```rust
    // `fl/ledgers` sits beside `fl/ledger`, not under it: git holds both,
    // so it is no reason to refuse.
    #[test]
    fn init_does_not_refuse_a_sibling_branch_named_like_the_ledger() {
        let fake = FakeGithub::start("acme/widgets");
        {
            let mut s = fake.state();
            let tree = s.git.put_tree(&BTreeMap::new());
            let c = s.git.put_commit(&tree, vec![], "someone's branch");
            s.git.refs.insert("heads/fl/ledgers".into(), c);
        }
        let local = MemStore::default();
        let c = client(&fake);
        let outcome = open(&c, &local).init(&entry_iri(1), None).unwrap();
        assert!(matches!(outcome, InitOutcome::Created { .. }), "{outcome:?}");
        assert!(fake.ledger_head().is_some());
    }
```

In `crates/cli/src/cmd/attempt.rs`, inside `mod tests`, after `an_attempts_decision_rests_on_the_attempt`, add:

```rust
    // What a person reads back of a long excerpt: its first lines, and no
    // more. The local store keeps all of it.
    #[test]
    fn an_attempts_excerpt_is_printed_up_to_forty_lines() {
        let mut a = attempt(AttemptStatus::Completed);
        a.output_excerpt = Some(
            (1..=50)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let (_, said) = settle(&Flushes::default(), &seq_iri(50), &a);
        let excerpt: Vec<&Said> = said
            .iter()
            .filter(|s| matches!(s, Said::Stdout(l) if l.starts_with("\t| ")))
            .collect();
        assert_eq!(excerpt.len(), EXCERPT_LINES, "{said:?}");
        assert_eq!(EXCERPT_LINES, 40);
        assert_eq!(excerpt.last(), Some(&&Said::Stdout("\t| line 40".into())));
    }
```

In `crates/cli/src/config.rs`, inside `mod tests`, after `the_only_ledger_a_binding_names_is_github`, add:

```rust
    // `ledger = true` is not the string "github": refused when the config
    // is read, never taken as the local ledger.
    #[test]
    fn a_ledger_key_that_is_not_a_string_is_refused() {
        let text = "[[project]]\nroot = \"/r\"\nstore = \"/s.redb\"\n\
                    tracker = { github = \"acme/widgets\", credential = \"env\", ledger = true }\n";
        let msg = format!("{:#}", load_text(text).expect_err("`ledger = true` is refused"));
        assert!(msg.contains("invalid type: boolean `true`"), "{msg}");
        assert!(msg.contains("expected a string"), "{msg}");
    }
```

In `crates/cli/tests/ledger.rs`:

(a) In `impl World`, after `ledger_files_in`, add:

```rust
    /// The one ledger file under `area/`, path and text — refused unless
    /// there is exactly one.
    fn only_file_in(&self, area: &str) -> (String, String) {
        let mut files = self.ledger_files_in(area);
        assert_eq!(files.len(), 1, "one file under {area}/: {files:?}");
        files.remove(0)
    }
```

(b) Replace every `w.ledger_files_in("runs").remove(0)` (in `verify_passes_a_ledger_fl_wrote_and_names_the_commit_of_a_hand_edit`, `verify_reports_one_id_on_two_lines_and_exits_1`, the three `quarantine_…` tests and `a_decision_that_reads_past_a_quarantined_line_notes_it`) with `w.only_file_in("runs")`, and in `a_check_with_a_record_publishes_and_a_plain_check_does_not` replace

```rust
    assert!(w.ledger_files_in("decisions")[0].1.contains(r#"{"check":"#));
```

with

```rust
    assert!(w.only_file_in("decisions").1.contains(r#"{"check":"#));
```

(c) In `stats_on_another_projects_store_says_it_covers_the_local_store_only`, replace the block that reads `store.list_projects().unwrap()[0]` with:

```rust
    let iri = {
        let store = fl_store::RedbStore::open(&w.one.store()).unwrap();
        let projects = store.list_projects().unwrap();
        assert_eq!(projects.len(), 1, "{projects:?}");
        projects[0].id.iri().as_str().to_string()
    };
```

(d) In `verify_stops_at_its_limit_and_names_the_flag`, replace `contains("walked back 1 commits")` with `contains("walked back 1 commit from the head")`.

(e) In `quarantine_on_a_private_repository_says_the_text_is_permanent_without_a_warning`, replace the `.stderr(…)` predicate with:

```rust
        .stderr(
            contains(concat!(
                "permanent: once appended, `--by` and `--reason` are written to the ledger of ",
                "acme/widgets permanently"
            ))
            .and(contains("warning:").not())
            .and(contains("note:").not()),
        );
```

(f) Replace `without_the_ledger_key_a_move_publishes_nothing` with:

```rust
// Spec §1.5: without `ledger = "github"`, a decision stays in the local
// store, exactly as in mode A — no request touches the ledger at all.
#[test]
fn without_the_ledger_key_a_move_publishes_nothing() {
    let w = World::bound(false);
    w.gated();
    w.fake.seed_ledger();
    w.fake.state().requests.clear();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    let requests = w.fake.state().requests.clone();
    assert!(
        requests
            .iter()
            .any(|r| r == "PATCH /repos/acme/widgets/issues/1"),
        "the move reached GitHub: {requests:#?}"
    );
    let ledger: Vec<&String> = requests
        .iter()
        .filter(|r| r.contains("/git/") || r.contains("/rules/") || r.contains("/compare/"))
        .collect();
    assert!(ledger.is_empty(), "no ledger request: {ledger:#?}");
    assert_eq!(w.fake.ledger_commits(), 1);
}
```

- [ ] **Step 2: Run them to verify which fail**

Run: `cargo test -p fl-github --lib -- ledger::verify::tests::a_limit_of_one_commit_is_named_in_the_singular ledger::init::tests::init_`
Expected: `a_limit_of_one_commit_is_named_in_the_singular` FAILS ("walked back 1 commits"); the two `init_` tests above pass (they pin shipped behaviour).

Run: `cargo test -p fl-cli --bin fl -- an_attempts_excerpt_is_printed_up_to_forty_lines a_ledger_key_that_is_not_a_string_is_refused`
Expected: FAIL to compile — `EXCERPT_LINES` does not exist.

Run: `cargo test -p fl-cli --test ledger -- verify_stops quarantine_on_a_private without_the_ledger_key`
Expected: `verify_stops_at_its_limit_and_names_the_flag` and `quarantine_on_a_private_repository_says_the_text_is_permanent_without_a_warning` FAIL; `without_the_ledger_key_a_move_publishes_nothing` passes.

- [ ] **Step 3: Implement**

In `crates/github/src/ledger/verify.rs`, in `verify_with`, replace the refusal inside `if chain.len() >= limit` with:

```rust
            if chain.len() >= limit {
                let commits = if limit == 1 { "commit" } else { "commits" };
                return Err(StoreError::Backend(format!(
                    "fl walked back {limit} {commits} from the head of {repo}'s `fl/ledger` \
                     without reaching the ledger's first commit {anchor}, and stopped. If the \
                     ledger really is that long, run `fl github ledger verify --max-commits \
                     <n>` with a larger number"
                )));
            }
```

In `crates/cli/src/cmd/ledger.rs`, in `quarantine`, replace the `Visibility::Private` arm with:

```rust
        // Said before the library checks the arguments, so worded for the
        // append that may not happen. ⚠ Its own prefix: a read's notes are
        // `note:`.
        Visibility::Private => eprintln!(
            "permanent: once appended, `--by` and `--reason` are written to the ledger of \
             {repo} permanently; nothing is ever removed from it"
        ),
```

In `crates/cli/src/cmd/attempt.rs`, after `const KNOWN_ADAPTERS: &str = "claude";`, add:

```rust
/// How many lines of an attempt's excerpt the command prints. The local
/// store keeps the whole excerpt.
const EXCERPT_LINES: usize = 40;
```

and in `settle` replace `.take(40)` with `.take(EXCERPT_LINES)`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github --lib -- ledger::verify::tests:: ledger::init::tests::` then `cargo test -p fl-cli`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Guards in this task, each reverted, run, seen red, restored:

1. The singular: replace `if limit == 1 { "commit" } else { "commits" }` with `"commits"` → `cargo test -p fl-github --lib ledger::verify::tests::a_limit_of_one_commit_is_named_in_the_singular` red, and `cargo test -p fl-cli --test ledger -- verify_stops_at_its_limit_and_names_the_flag` red.
2. The plural (the other arm): replace it with `"commit"` → `cargo test -p fl-github --lib ledger::verify::tests::a_history_longer_than_the_limit_is_refused_naming_the_flag` red ("walked back 2 commits").
3. The prefix: change `permanent:` back to `note:` → `cargo test -p fl-cli --test ledger -- quarantine_on_a_private_repository_says_the_text_is_permanent_without_a_warning` red.
4. `branches_under` asks for names under `fl/ledger/`, slash included: in `crates/github/src/ledger/git.rs`, change `"/git/matching-refs/heads/{branch}/"` to `"/git/matching-refs/heads/{branch}"` → `cargo test -p fl-github --lib ledger::init::tests::init_does_not_refuse_a_sibling_branch_named_like_the_ledger` red.
5. The refusal comes before anything is created: in `init`'s `(None, None)` arm, move the `branches_under` check below `let root = self.create_branch()?;` → `cargo test -p fl-github --lib ledger::init::tests::init_refuses_a_branch_under_fl_ledger_before_creating_anything` red (a tree was created).
6. `EXCERPT_LINES`: change it to `41` → `cargo test -p fl-cli --bin fl -- cmd::attempt::tests::an_attempts_excerpt_is_printed_up_to_forty_lines` red.
7. Mode A asks nothing of the ledger: in `crates/cli/src/main.rs`, change the `github_ledger` arm's guard `if b.github_ledger()` to `if true` → `cargo test -p fl-cli --test ledger -- without_the_ledger_key_a_move_publishes_nothing` red (the pre-flight reads `rules/` and `git/`).

Not a guard: `a_ledger_key_that_is_not_a_string_is_refused` pins serde's existing refusal; there is no line to revert.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/ledger/verify.rs crates/github/src/ledger/init.rs crates/cli/src/cmd/ledger.rs crates/cli/src/cmd/attempt.rs crates/cli/src/config.rs crates/cli/tests/ledger.rs
git commit -m "fix(cli,github): say 1 commit, give quarantine's line its own prefix, close test gaps

verify's limit refusal says \"1 commit\". quarantine's private-repository
line is prefixed permanent:, not the note: a read's notes use. New tests:
a sibling fl/ledgers is not refused by init, a refused init creates no
tree, ledger = true is refused, an attempt prints 40 excerpt lines, and
mode A makes no ledger request at all. Tests assert a length before they
index. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 2: An empty repository, and a commit answered with a timeout

B1 requirement 7: two answers fl had not modelled. On a repository with no commit, GitHub answers a ref read `409 Git Repository is empty`; today `init` says "GitHub answered 409 … retry", which no retry cures. And a GraphQL request that runs past GitHub's limit is answered 200 with an error saying it "may be the result of a timeout" — the commit may have landed, and §3.2 step 5 says to read again; today `judge` makes it an error that stops the append.

**Blast radius:** `branch_head` is read by `init` (twice), `check_head` (every read, every append, the pre-flight) and `verify`; a 409 was an error at each and is now a named one. `judge` is read by `commit` only. The fake gains two knobs, both off by default.

**Files:**
- Modify: `crates/github/src/ledger/git.rs` (`branch_head`)
- Modify: `crates/github/src/ledger/append.rs` (`judge`; tests)
- Modify: `crates/github/src/ledger/init.rs` (tests)
- Modify: `crates/github/src/fake.rs` (`State`: `empty_repository`, `timeout_after_next_commit`)
- Modify: `crates/github/src/fake_git.rs` (`rest`; `append`)

**Interfaces:**
- Consumes: `Client::send`, `GraphqlAnswer`.
- Produces: `State::empty_repository: bool` and `State::timeout_after_next_commit: bool` on the fake. The refusal's unique phrase: `is empty: GitHub keeps no branch until`.

- [ ] **Step 1: Write the failing tests**

In `crates/github/src/ledger/init.rs`, inside `mod tests`, after `init_refuses_when_a_branch_named_fl_exists`, add:

```rust
    // ⚠ A repository with no commit keeps no branch: `init` says to push
    // a first commit, and creates nothing. A decision there reads the same
    // refusal.
    #[test]
    fn init_on_an_empty_repository_says_to_push_a_first_commit_and_creates_nothing() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().empty_repository = true;
        let local = MemStore::default();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(1), None).unwrap_err().to_string();
        assert!(err.contains("is empty: GitHub keeps no branch until"), "{err}");
        assert!(err.contains("then run `fl github ledger init` again"), "{err}");
        assert!(fake.state().git.trees.is_empty(), "no tree was created");
        assert_eq!(local.cutover("R_1").unwrap(), None);

        let known = MemStore::default();
        known.set_ledger_root("R_1", "0123456789abcdef0123456789abcdef01234567").unwrap();
        let err = open(&c, &known).check_head().unwrap_err().to_string();
        assert!(err.contains("is empty: GitHub keeps no branch until"), "{err}");
    }

    // A 409 that does not say the repository is empty stays the error it
    // was: no "push a first commit" for a conflict of another kind.
    #[test]
    fn a_409_for_another_reason_is_not_read_as_an_empty_repository() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state()
            .body_next
            .push(("/git/ref/heads/fl".into(), 409, json!({"message": "Conflict"})));
        let local = MemStore::default();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(1), None).unwrap_err().to_string();
        assert!(err.contains("GitHub answered 409"), "{err}");
        assert!(!err.contains("is empty"), "{err}");
    }
```

In `crates/github/src/ledger/append.rs`, inside `mod tests`, after `a_commit_answered_with_a_body_that_breaks_off_is_read_again_and_lands_once`, add:

```rust
    // ⚠ Spec §3.2 step 5: a timeout is read again. The commit landed, so
    // nothing is added twice and no empty commit follows.
    #[test]
    fn a_commit_answered_with_a_timeout_is_read_again_and_lands_once() {
        let (fake, local, _root) = world();
        fake.state().timeout_after_next_commit = true;
        let c = client(&fake);
        let l = open(&c, &local);
        let b = batch(1, vec![run(1)], vec![]);
        assert_eq!(l.publish(&b).unwrap(), fake.ledger_head());
        assert_eq!(fake.ledger_commits(), 2, "one commit, none empty, none twice");
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
    }
```

and after `each_answer_to_a_commit_is_judged_once`, add:

```rust
    // A timeout says nothing about whether the commit landed: unknown,
    // whatever its case. A refusal for want of a permission that rides
    // along with one is still a refusal.
    #[test]
    fn a_timeout_is_an_unknown_landing_and_a_forbidden_answer_stays_refused() {
        let timeout = json!({"message": "Something went wrong while executing your query. \
            This may be the result of a timeout, or it could be a GitHub bug."});
        assert!(matches!(
            judge(answer(200, None, vec![timeout.clone()])).unwrap(),
            Landed::Unknown(_)
        ));
        assert!(matches!(
            judge(answer(200, None, vec![json!({"message": "Request TIMEOUT"})])).unwrap(),
            Landed::Unknown(_)
        ));
        let refused = judge(answer(
            200,
            None,
            vec![json!({"type": "FORBIDDEN", "message": "Resource not accessible"}), timeout],
        ))
        .unwrap_err();
        assert!(refused.to_string().contains("Contents: write"), "{refused}");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-github --lib -- ledger::init::tests::init_on_an_empty_repository ledger::append::tests::a_commit_answered_with_a_timeout ledger::append::tests::a_timeout_is_an_unknown_landing`
Expected: FAIL to compile — `empty_repository` and `timeout_after_next_commit` are not fields of the fake's `State`.

- [ ] **Step 3: Implement**

In `crates/github/src/fake.rs`, in `pub struct State`, after `pub truncate_trees: bool,`, add:

```rust
    /// Every git data request answers 409, as for a repository with no
    /// commit. A setting.
    pub empty_repository: bool,
    /// The next commit lands, and its answer is GitHub's timeout error: a
    /// 200 with no data and an error saying it may be a timeout. One-shot.
    pub timeout_after_next_commit: bool,
```

In `crates/github/src/fake_git.rs`, in `rest`, after the `if !s.is_bound(o, r) { return None; }` check, add:

```rust
    // ⚠ Modelled: a repository with no commit answers a ref read 409 "Git
    // Repository is empty". Confirmed by live test
    // `an_empty_repository_is_refused_naming_a_first_commit`. Unmeasured:
    // the fake answers every other git data request the same way; fl reads
    // a ref before it sends any other.
    if s.empty_repository && rest.first() == Some(&"git") {
        return Some(answer(409, json!({"message": "Git Repository is empty."})));
    }
```

In `append`, after `let oid = s.git.commit_on(&branch, &changes, &headline);`, add:

```rust
    if std::mem::take(&mut s.timeout_after_next_commit) {
        return answer(
            200,
            json!({"data": null, "errors": [{"message": "Something went wrong while executing \
                your query. This may be the result of a timeout, or it could be a GitHub bug. \
                Please include `0000:0000:0000000:0000000:00000000` when reporting this \
                issue."}]}),
        );
    }
```

In `crates/github/src/ledger/git.rs`, in `branch_head`, replace

```rust
        let r = self.client.send(
            Method::Get,
            &self.path(&format!("/git/ref/heads/{branch}")),
            None,
        )?;
```

with

```rust
        let r = match self.client.send(
            Method::Get,
            &self.path(&format!("/git/ref/heads/{branch}")),
            None,
        ) {
            Ok(r) => r,
            // ⚠ Modelled: GitHub answers a ref read on a repository with no
            // commit 409 "Git Repository is empty". No retry cures it.
            // Confirmed by live test
            // `an_empty_repository_is_refused_naming_a_first_commit`.
            Err(StoreError::Backend(m))
                if m.starts_with("GitHub answered 409 ") && m.contains("Git Repository is empty") =>
            {
                let repo = &self.repo.full_name;
                return Err(backend(format!(
                    "the repository {repo} is empty: GitHub keeps no branch until a repository \
                     has a first commit, so fl cannot keep a ledger there yet. Push a first \
                     commit to {repo} (a README will do), then run `fl github ledger init` again"
                )));
            }
            Err(e) => return Err(e),
        };
```

In `crates/github/src/ledger/append.rs`, in `judge`, replace the body of `if !answer.errors.is_empty() { … }` after the `if moved { return Ok(Landed::HeadMoved); }` line with:

```rust
        let forbidden = answer
            .errors
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("FORBIDDEN"));
        // ⚠ Modelled from GitHub's documented error shape, with no live
        // test: a request that runs past GitHub's time limit is answered 200
        // with an error saying it "may be the result of a timeout", and the
        // commit may have landed; only a fresh read can tell (spec §3.2
        // step 5). Provoking it takes a request built to run past GitHub's
        // limit, which would abuse the API. A refusal for want of a
        // permission stays a refusal.
        let timed_out = answer.errors.iter().any(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .is_some_and(|m| m.to_ascii_lowercase().contains("timeout"))
        });
        if timed_out && !forbidden {
            return Ok(Landed::Unknown(format!(
                "GitHub answered fl's commit with a timeout ({})",
                Value::Array(answer.errors)
            )));
        }
        let errors = Value::Array(answer.errors);
        return Err(StoreError::Backend(if forbidden {
            format!(
                "GitHub refused fl's commit to the ledger for want of a permission ({errors}), \
                 so nothing was published. {NEEDS}: grant it, then retry"
            )
        } else {
            // ⚠ An error fl does not know may come with a commit that
            // landed: fl cannot tell, so it claims neither. Nothing is
            // marked published, and the next flush reads the ledger and
            // adds only what is missing.
            format!(
                "GitHub answered fl's commit to the ledger with an error ({errors}), so fl \
                 cannot tell whether it was published. Nothing is marked published: the next \
                 flush reads the ledger and adds only what is missing"
            )
        }));
```

(The `moved` computation and its `if` stay first, unchanged.)

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

1. The 409 arm: delete it → `cargo test -p fl-github --lib ledger::init::tests::init_on_an_empty_repository_says_to_push_a_first_commit_and_creates_nothing` red ("GitHub answered 409 …").
2. The arm's status conjunct: change `"GitHub answered 409 "` to `"GitHub answered 410 "` → the same test red.
3. The arm's message conjunct: drop `&& m.contains("Git Repository is empty")` → `cargo test -p fl-github --lib ledger::init::tests::a_409_for_another_reason_is_not_read_as_an_empty_repository` red.
4. The arm serves every caller of `branch_head`, decisions as well as `init`: add `&& branch == "fl"` to the guard → `ledger::init::tests::init_on_an_empty_repository_says_to_push_a_first_commit_and_creates_nothing` red (its `check_head` half).
5. The timeout arm: delete `if timed_out && !forbidden { … }` → `cargo test -p fl-github --lib ledger::append::tests::a_commit_answered_with_a_timeout_is_read_again_and_lands_once` and `cargo test -p fl-github --lib ledger::append::tests::a_timeout_is_an_unknown_landing_and_a_forbidden_answer_stays_refused` red.
6. The `!forbidden` conjunct: drop it → `cargo test -p fl-github --lib ledger::append::tests::a_timeout_is_an_unknown_landing_and_a_forbidden_answer_stays_refused` red.
7. Any case: drop `.to_ascii_lowercase()` → the same test red ("Request TIMEOUT").

Not product guards: the fake's `rest.first() == Some(&"git")` (fl reads a ref before any other git data request, so no test can tell); and `moved` before `timed_out` — a head-moved error and a timeout both send the append back to read again, so no answer tells the two orders apart.


- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/ledger/git.rs crates/github/src/ledger/append.rs crates/github/src/ledger/init.rs crates/github/src/fake.rs crates/github/src/fake_git.rs
git commit -m "fix(github): an empty repository says to push a first commit; a timeout is read again

A ref read on a repository with no commit answers 409; init and every
decision now say to push a first commit instead of retry. A commit answered
with GitHub's timeout error may have landed, so the append reads again
(spec 3.2 step 5); a FORBIDDEN error alongside it stays a refusal. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 3: A decision comment, rendered

Spec §4.2, pure: no request and no clock. This task renders a comment whole; Task 4 adds the 60,000-byte cap.

**Files:**
- Create: `crates/github/src/ledger/render.rs`
- Modify: `crates/github/src/ledger/mod.rs` (`pub mod render;` after `pub mod layout;`)

**Interfaces:**
- Consumes: `Visibility` (`ledger::disclose`); `Decision`, `Outcome` (`fl_core::decision`); `GateRun`, `Attempt` (`fl_core::log`); `Verdict`; `Iri`.
- Produces, in `fl_github::ledger::render`:
  - `pub const COMMENT_LIMIT: usize = 60_000;`, `pub const CELL_LIMIT: usize = 200;`, `pub const MISSING_SHOWN: usize = 20;`
  - `pub struct RunRow { pub role: String, pub gate: String, pub run: GateRun }`
  - `pub struct DecisionView { pub decision: Decision, pub by: String, pub commit: Option<String>, pub rows: Vec<RunRow>, pub attempt: Option<Attempt>, pub missing: Vec<Iri> }`
  - `pub fn markable(id: &Iri) -> bool`, `pub fn marker(id: &Iri) -> Option<String>`, `pub fn marked(body: &str) -> Option<Iri>`
  - `pub fn is_sha(s: &str) -> bool`
  - `pub fn escape(s: &str) -> String`, `pub fn escape_capped(s: &str, limit: usize) -> String` (markdown text); private `fn escape_html_capped(s: &str, limit: usize) -> String` (inside an HTML element)
  - `pub fn state_line(outcome: &Outcome, completed: bool) -> String`
  - `pub fn render(view: &DecisionView, repo: &str, visibility: Visibility, state: Option<&str>) -> String`
  - private, replaced or reused by Task 4: `struct Block`, `fn blocks`, `fn block(title: &str, text: &str) -> String`, `fn fence_for`, `fn head`, `fn table(view, shown: usize)`, `fn missing`, `fn assemble`.

- [ ] **Step 1: Write the module with its failing tests**

Create `crates/github/src/ledger/render.rs` holding the module doc, the `use` lines, the constants and the types below, then the tests. Step 3 adds the functions.

```rust
//! A decision comment (GitHub ledger spec §4.2): what an issue shows of a
//! decision the ledger holds. Pure — no request and no clock. A comment
//! posted as the decision is made and one recovered later are rendered
//! here alike; only the first says whether the state change completed.

use super::disclose::Visibility;
use fl_core::decision::{Decision, Outcome};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use fl_core::verdict::Verdict;
use serde_json::{Value, json};

/// The most a comment's body may hold, in bytes of UTF-8 — under GitHub's
/// 65,536-character limit however it counts (spec §4.2).
pub const COMMENT_LIMIT: usize = 60_000;

/// The most one escaped table cell, or one name in the header, may hold.
/// A longer one is cut, and ends in `…`.
pub const CELL_LIMIT: usize = 200;

/// How many entries not found a comment names; the rest it counts.
pub const MISSING_SHOWN: usize = 20;

const MARKER_OPEN: &str = "<!-- fl:decision ";
const MARKER_CLOSE: &str = " -->";

/// One run a decision rests on, as its comment shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    /// What the run was for: its transition, or `reproduction`,
    /// `regression` or `neighbour`.
    pub role: String,
    /// The gate's name in the local catalog, or its IRI when the catalog
    /// does not hold it.
    pub gate: String,
    pub run: GateRun,
}

/// Everything a comment shows of one decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionView {
    pub decision: Decision,
    /// Who wrote the decision's line (spec §3.1).
    pub by: String,
    /// The ledger commit that holds it, when known.
    pub commit: Option<String>,
    /// The runs it rests on, in the order it names them.
    pub rows: Vec<RunRow>,
    /// The attempt it rests on, for an attempt.
    pub attempt: Option<Attempt>,
    /// Entries it rests on that were not found.
    pub missing: Vec<Iri>,
}

/// One output excerpt a private repository's comment shows, folded.
struct Block {
    title: String,
    text: String,
}

/// Where a name is written: in markdown text (a table cell, the header),
/// or inside an HTML element (an excerpt's `<summary>`), where markdown is
/// not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    Markdown,
    Html,
}
```

Then the tests, at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::at::At;
    use fl_core::decision::TransitionOutcome;
    use fl_core::ids::{GateId, ProjectId, RecordId, seq_iri};
    use fl_core::log::{AttemptStatus, PathsTouched};
    use fl_core::model::State;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn record() -> RecordId {
        RecordId(Iri::parse("https://github.com/acme/widgets/issues/1").unwrap())
    }

    fn decision(outcome: Outcome, rests_on: Vec<Iri>) -> Decision {
        Decision {
            id: seq_iri(90),
            at: At::from_unix_millis(1),
            record: record(),
            finding: None,
            outcome,
            rests_on,
        }
    }

    fn moved(allowed: bool) -> Outcome {
        Outcome::Move {
            from: State::Review,
            to: State::Done,
            transitions: vec![TransitionOutcome {
                transition: "launch".into(),
                passed: allowed,
            }],
            allowed,
        }
    }

    fn check(passed: bool) -> Outcome {
        Outcome::Check {
            transition: TransitionOutcome {
                transition: "launch".into(),
                passed,
            },
        }
    }

    fn reproduce(accepted: bool) -> Outcome {
        Outcome::Reproduce {
            gate: GateId(seq_iri(7)),
            accepted,
        }
    }

    fn verify(closed: bool) -> Outcome {
        Outcome::Verify {
            reproduction: GateId(seq_iri(7)),
            reproduction_passed: closed,
            regressions: vec![],
            closed,
        }
    }

    fn run(n: u64, verdict: Verdict, excerpt: Option<&str>) -> GateRun {
        GateRun {
            id: Some(seq_iri(n)),
            at: Some(At::from_unix_millis(n)),
            gate: GateId(seq_iri(7)),
            record: Some(record()),
            commit: "abcdef0123".into(),
            verdict,
            population: 3,
            output_excerpt: excerpt.map(str::to_string),
            duration_ms: 5,
            cost_usd_micros: 0,
        }
    }

    fn row(role: &str, gate: &str, run: GateRun) -> RunRow {
        RunRow {
            role: role.into(),
            gate: gate.into(),
            run,
        }
    }

    fn attempt(excerpt: Option<&str>) -> Attempt {
        Attempt {
            id: Some(seq_iri(50)),
            at: Some(At::from_unix_millis(2)),
            project: ProjectId(seq_iri(2)),
            record: record(),
            adapter: "claude".into(),
            status: AttemptStatus::Completed,
            duration_ms: 7,
            tokens_in: 11,
            tokens_out: 13,
            cost_usd_micros: 1_234_567,
            paths_touched: PathsTouched::Counted(2),
            output_excerpt: excerpt.map(str::to_string),
        }
    }

    fn view_of(outcome: Outcome, rows: Vec<RunRow>) -> DecisionView {
        DecisionView {
            decision: decision(outcome, rows.iter().filter_map(|r| r.run.id.clone()).collect()),
            by: "fake-user".into(),
            commit: Some(SHA.into()),
            rows,
            attempt: None,
            missing: vec![],
        }
    }

    #[test]
    fn escape_neutralises_markup_mentions_and_references() {
        assert_eq!(
            escape("a|b `c` @x #1 <!-- y\r\nz & *w* _u_ [l](t) !i ~s~ $m$ GH-1 gh-2 \\"),
            r"a\|b \`c\` @&#8203;x #&#8203;1 &lt;!-- y<br>z &amp; \*w\* \_u\_ \[l\]\(t\) !i \~s\~ \$m\$ GH-&#8203;1 gh-&#8203;2 \\"
        );
        assert_eq!(escape("a > b"), "a &gt; b");
        assert_eq!(
            escape("plain-text.v1: ok/fine-G-H-"),
            "plain-text.v1: ok/fine-G-H-",
            "nothing else changes, and a `-` not after `GH` stays"
        );
    }

    // Inside `<summary>` markdown is not read: entities and the zero-width
    // spaces only, and a newline becomes a space.
    #[test]
    fn a_name_inside_an_html_element_gets_no_markdown_escapes() {
        assert_eq!(
            escape_html_capped("a|b_c @x #1 <i> GH-1 & \\\nz", CELL_LIMIT),
            r"a|b_c @&#8203;x #&#8203;1 &lt;i&gt; GH-&#8203;1 &amp; \ z"
        );
    }

    #[test]
    fn a_long_name_is_cut_on_a_whole_escape_and_says_so() {
        assert_eq!(escape_capped("short", CELL_LIMIT), "short");
        let cut = escape_capped(&"é".repeat(500), CELL_LIMIT);
        assert!(cut.len() <= CELL_LIMIT && cut.ends_with('…'), "{cut}");
        // Never half an escape: four `&` in ten bytes keep one whole `&amp;`.
        assert_eq!(escape_capped("&&&&", 10), "&amp;…");
        // Exactly at the limit: whole.
        assert_eq!(escape_capped(&"a".repeat(CELL_LIMIT), CELL_LIMIT), "a".repeat(CELL_LIMIT));
        // A cut whose kept text and ellipsis fill the limit exactly.
        assert_eq!(escape_capped("abcdef", 4), "a…");
        assert_eq!(escape_html_capped("abcdef", 4), "a…");
    }

    // ⚠ The first marker line counts wherever it is: an edit that pushes it
    // down keeps the comment posted. Only the first: a marker quoted
    // further down marks nothing.
    #[test]
    fn the_first_marker_line_counts_wherever_it_is() {
        let id = seq_iri(90);
        let m = marker(&id).expect("an id fl writes");
        assert_eq!(m, format!("<!-- fl:decision {{\"id\":\"{id}\"}} -->"));
        assert_eq!(marked(&format!("{m}\n\nbody")), Some(id.clone()));
        assert_eq!(marked(&format!("{m}\r\nbody")), Some(id.clone()), "GitHub may send CRLF");
        assert_eq!(
            marked(&format!("A maintainer's note.\n\n{m}\n\nbody")),
            Some(id.clone()),
            "moved down by an edit"
        );
        let other = marker(&seq_iri(91)).unwrap();
        assert_eq!(marked(&format!("{m}\n````\n{other}\n````")), Some(id.clone()));
        assert_eq!(marked(&format!("{m}\n{other}")), Some(id.clone()), "only the first");
        // A broken marker-like line above the real one is passed over.
        assert_eq!(
            marked(&format!("<!-- fl:decision broken -->\n{m}")),
            Some(id.clone())
        );
        // fl's marker deleted: a marker quoted inside an excerpt's fence
        // marks nothing, however long the fence.
        assert_eq!(marked(&format!("A note.\n\n````\n{other}\n````\n")), None);
        assert_eq!(marked(&format!("````\n```\n{other}\n````\n")), None, "a shorter run does not close");
        assert_eq!(marked(&format!("```\n``` x\n{other}\n```\n")), None, "nor a run with text after it");
        // After a fence closes, a marker counts again.
        assert_eq!(marked(&format!("```\nx\n```\n{m}")), Some(id.clone()));
        for body in [
            "",
            "no marker at all",
            "<!-- fl:decision {} -->",
            "<!-- fl:decision {\"id\":\"not an iri\"} -->",
            "<!-- fl:decision {\"id\":1} -->",
            "<!-- fl:decision not json -->",
            "<!-- fl:decision {\"id\":\"urn:uuid:00000000-0000-7000-8000-00000000005a\"}",
            "<!-- fl:decision {\"id\":\"urn:x:a\"} -->",
        ] {
            assert_eq!(marked(body), None, "{body}");
        }
    }

    // ⚠ An id fl does not write never reaches the comment: no marker, and
    // nothing in the body opens or closes an HTML comment.
    #[test]
    fn a_hostile_decision_id_gets_no_marker_and_nothing_closes_a_comment() {
        let hostile = Iri::parse("urn:x:a--><b>").unwrap();
        assert!(!markable(&hostile));
        assert_eq!(marker(&hostile), None);
        let mut v = view_of(moved(true), vec![]);
        v.decision.id = hostile;
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(!body.contains("<!--") && !body.contains("-->"), "{body}");
        assert!(!body.contains("<b>"), "{body}");
        assert_eq!(marked(&body), None);
        assert!(markable(&seq_iri(90)));
        for bad in [
            "urn:uuid:not-a-uuid",
            "urn:uuid:0000000-0000-7000-8000-00000000005a0",
            "urn:uuid:gggggggg-0000-7000-8000-000000000000",
            "fl:gate/no-bug.v1",
            "https://example.com/x",
        ] {
            assert!(!markable(&Iri::parse(bad).unwrap()), "{bad}");
        }
    }

    // A run of backticks sets the fence; separate runs do not add up.
    #[test]
    fn the_fence_outruns_the_longest_run_of_backticks_not_their_sum() {
        assert_eq!(fence_for(""), "```");
        assert_eq!(fence_for("a `` b"), "```");
        assert_eq!(fence_for("a ``` b ```` c ``` d"), "`````");
        assert_eq!(fence_for("````"), "`````");
    }

    // ⚠ What a ledger line carries is escaped like any name: who decided,
    // the attempt's adapter, the run's commit, and an excerpt's title.
    #[test]
    fn who_decided_the_adapter_the_commit_and_a_title_are_escaped() {
        let hostile = "@x #1 <!-- |";
        let mut r = run(1, Verdict::from_predicate(false, 1), Some("out"));
        r.commit = "@x#<|`ab".into();
        let mut v = view_of(moved(true), vec![row("launch", hostile, r)]);
        v.by = hostile.into();
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(body.contains(r"Decided by @&#8203;x #&#8203;1 &lt;!-- \| at "), "{body}");
        assert!(body.contains(r"| @&#8203;x#&#8203;&lt;\|\`a |"), "the commit's first seven: {body}");
        assert!(
            body.contains("<summary>launch / @&#8203;x #&#8203;1 &lt;!-- |: FAIL</summary>"),
            "{body}"
        );
        assert_eq!(body.matches("<!--").count(), 1, "only the marker: {body}");
        let mut v = view_of(Outcome::Attempt { status: AttemptStatus::Completed }, vec![]);
        let mut a = attempt(Some("x"));
        a.adapter = hostile.into();
        v.attempt = Some(a);
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(body.contains(r"| @&#8203;x #&#8203;1 &lt;!-- \| | completed |"), "{body}");
        assert!(body.contains("<summary>@&#8203;x #&#8203;1 &lt;!-- |: completed</summary>"), "{body}");
        assert_eq!(body.matches("<!--").count(), 1, "only the marker: {body}");
    }

    #[test]
    fn only_a_full_commit_id_is_linked() {
        assert!(is_sha(SHA));
        assert!(!is_sha("0123456"), "too short");
        assert!(!is_sha(&"g".repeat(40)), "not hex");
        let mut v = view_of(moved(true), vec![]);
        v.commit = Some("unknown (GitHub's blame of `x` did not name it)".into());
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(
            body.contains("Evidence: the ledger holds it; this comment does not name its commit."),
            "{body}"
        );
        assert!(!body.contains("/commit/"), "{body}");
        v.commit = None;
        assert!(
            render(&v, "acme/widgets", Visibility::Private, None).contains("does not name its commit")
        );
    }

    // Spec §4.2: the header, the move's states, the state line and the
    // table, exactly.
    #[test]
    fn a_moves_comment_says_what_was_decided_by_whom_and_where_its_evidence_is() {
        let v = view_of(
            moved(true),
            vec![row("launch", "no-bug", run(1, Verdict::from_predicate(true, 3), Some("ok")))],
        );
        let body = render(
            &v,
            "acme/widgets",
            Visibility::NotPrivate,
            Some("The state change completed: the record is now `done`."),
        );
        let expected = format!(
            concat!(
                "<!-- fl:decision {{\"id\":\"{id}\"}} -->\n\n",
                "### fl move: allowed\n\n",
                "Decided by fake-user at 1970-01-01T00:00:00.001Z. Evidence: ledger commit ",
                "[0123456](https://github.com/acme/widgets/commit/{sha}).\n\n",
                "From `review` to `done`.\n\n",
                "The state change completed: the record is now `done`.\n\n",
                "| for | gate | verdict | population | commit | duration |\n",
                "|---|---|---|---|---|---|\n",
                "| launch | no-bug | PASS | 3 | abcdef0 | 5 ms |\n",
            ),
            id = seq_iri(90),
            sha = SHA
        );
        assert_eq!(body, expected);
    }

    // Decision 11: a refused decision's comment says it was refused.
    #[test]
    fn the_heading_names_the_kind_and_the_outcome_refused_ones_included() {
        let cases = vec![
            (moved(true), "### fl move: allowed"),
            (moved(false), "### fl move: refused"),
            (check(true), "### fl check: passed"),
            (check(false), "### fl check: failed"),
            (reproduce(true), "### fl reproduce: accepted"),
            (reproduce(false), "### fl reproduce: refused"),
            (verify(true), "### fl verify: closed"),
            (verify(false), "### fl verify: not closed"),
            (
                Outcome::Attempt {
                    status: AttemptStatus::Timeout,
                },
                "### fl attempt: timeout",
            ),
        ];
        for (o, heading) in cases {
            let body = render(&view_of(o.clone(), vec![]), "acme/widgets", Visibility::Private, None);
            assert!(body.contains(&format!("\n\n{heading}\n\n")), "{o:?}: {body}");
        }
        let body = render(&view_of(moved(false), vec![]), "acme/widgets", Visibility::Private, None);
        assert!(body.contains("No gate ran for this decision."), "{body}");
        assert!(!body.contains("The state change"), "no state line unless given: {body}");
    }

    #[test]
    fn the_state_line_says_whether_the_state_change_completed() {
        let cases: Vec<(Outcome, bool, &str)> = vec![
            (moved(true), true, "The state change completed: the record is now `done`."),
            (
                moved(true),
                false,
                "The move was allowed, but its state change did not complete: the record may \
                 still be `review`.",
            ),
            (moved(false), true, "The move was refused: the record stays `review`."),
            (moved(false), false, "The move was refused: the record stays `review`."),
            (check(true), true, "A check changes no state."),
            (check(false), false, "A check changes no state."),
            (
                reproduce(true),
                true,
                "The state change completed: the finding records this gate as its reproduction.",
            ),
            (
                reproduce(true),
                false,
                "The reproduction was accepted, but its state change did not complete.",
            ),
            (reproduce(false), false, "The reproduction was refused: the finding is unchanged."),
            (verify(true), true, "The state change completed: the finding is closed."),
            (
                verify(true),
                false,
                "The finding passed its verification, but closing it did not complete.",
            ),
            (verify(false), true, "The finding stays open: the repair is not done."),
            (
                Outcome::Attempt {
                    status: AttemptStatus::Completed,
                },
                true,
                "An attempt changes no state.",
            ),
        ];
        for (outcome, completed, line) in cases {
            assert_eq!(state_line(&outcome, completed), line, "{outcome:?} {completed}");
        }
    }

    #[test]
    fn an_attempts_comment_shows_its_adapter_status_duration_tokens_and_cost() {
        let mut v = view_of(
            Outcome::Attempt {
                status: AttemptStatus::Completed,
            },
            vec![],
        );
        v.decision.rests_on = vec![seq_iri(50)];
        v.attempt = Some(attempt(None));
        let body = render(&v, "acme/widgets", Visibility::NotPrivate, None);
        assert!(
            body.contains(concat!(
                "| adapter | status | duration | tokens in | tokens out | cost |\n",
                "|---|---|---|---|---|---|\n",
                "| claude | completed | 7 ms | 11 | 13 | 1.234567 USD |\n"
            )),
            "{body}"
        );
        assert!(!body.contains("No gate ran"), "{body}");
    }

    #[test]
    fn a_name_in_the_table_is_escaped_and_cut_and_an_error_has_no_population() {
        let long = format!("a|b @x {}", "z".repeat(10_000));
        let v = view_of(moved(true), vec![row(&long, &long, run(1, Verdict::error("broke"), None))]);
        let body = render(&v, "acme/widgets", Visibility::NotPrivate, None);
        let line = body.lines().find(|l| l.starts_with(r"| a\|b")).expect("the row");
        assert!(line.contains("@&#8203;x"), "{line}");
        assert!(line.len() < 3 * CELL_LIMIT, "{} bytes", line.len());
        assert!(line.contains("| ERROR | — |"), "{line}");
    }

    #[test]
    fn a_table_shown_in_part_and_entries_not_found_say_how_many_are_left_out() {
        let rows: Vec<RunRow> = (1..=3)
            .map(|n| row("launch", "g", run(n, Verdict::from_predicate(true, 1), None)))
            .collect();
        let mut v = view_of(moved(true), rows);
        let shown = table(&v, 1);
        assert_eq!(shown.matches("| launch | g |").count(), 1, "{shown}");
        assert!(shown.contains("… and 2 more runs: the ledger commit holds every one."), "{shown}");
        assert!(!table(&v, 3).contains("more runs"));
        v.missing = (100..100 + MISSING_SHOWN as u64 + 2).map(seq_iri).collect();
        let named = missing(&v);
        assert_eq!(named.matches("Not found in the ledger: ").count(), MISSING_SHOWN, "{named}");
        assert!(named.contains("… and 2 more entries not found."), "{named}");
        v.missing.truncate(1);
        assert!(!missing(&v).contains("more entries"));
    }

    // ⚠ Spec §4.2: excerpts only on a private repository, each in a fence
    // longer than any run of backticks in it — so nothing in it closes the
    // block, opens a tag, or forges a marker.
    #[test]
    fn excerpts_show_only_on_a_private_repository_each_in_a_fence_longer_than_its_backticks() {
        let text = "line ```` four\n</details>\n<!-- fl:decision \
                    {\"id\":\"urn:uuid:00000000-0000-7000-8000-000000000001\"} -->\n@someone #1";
        let v = view_of(
            moved(true),
            vec![row("launch", "no-bug", run(1, Verdict::from_predicate(false, 3), Some(text)))],
        );
        let private = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(
            private.contains(&format!(
                "<details><summary>launch / no-bug: FAIL</summary>\n\n`````\n{text}\n`````\n\n\
                 </details>\n"
            )),
            "{private}"
        );
        assert_eq!(marked(&private), Some(seq_iri(90)), "only fl's own marker, the first, marks it");
        let public = render(&v, "acme/widgets", Visibility::NotPrivate, None);
        assert!(!public.contains("<details>"), "{public}");
        assert!(!public.contains("four"), "{public}");
    }

    #[test]
    fn an_errors_detail_and_an_attempts_excerpt_show_and_an_empty_one_does_not() {
        let v = view_of(
            moved(true),
            vec![
                row("launch", "a", run(1, Verdict::error("spawn failed"), Some(""))),
                row("launch", "b", run(2, Verdict::from_predicate(true, 1), Some(""))),
                row("launch", "c", run(3, Verdict::from_predicate(true, 1), None)),
            ],
        );
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(
            body.contains("<summary>launch / a: ERROR</summary>\n\n```\nerror: spawn failed\n```"),
            "{body}"
        );
        assert_eq!(body.matches("<details>").count(), 1, "{body}");

        let mut v = view_of(
            Outcome::Attempt {
                status: AttemptStatus::Completed,
            },
            vec![],
        );
        v.attempt = Some(attempt(Some("did it")));
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(
            body.contains("<summary>claude: completed</summary>\n\n```\ndid it\n```"),
            "{body}"
        );
        v.attempt = Some(attempt(Some("")));
        assert!(!render(&v, "acme/widgets", Visibility::Private, None).contains("<details>"));
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Add `pub mod render;` after `pub mod layout;` in `crates/github/src/ledger/mod.rs`.

Run: `cargo test -p fl-github --lib ledger::render::tests::`
Expected: FAIL to compile — `escape`, `escape_capped`, `escape_html_capped`, `markable`, `marker`, `marked`, `is_sha`, `fence_for`, `state_line`, `render`, `table` and `missing` do not exist.

- [ ] **Step 3: Implement**

In `render.rs`, after `struct Block`, add:

```rust
/// Whether `id` is one fl writes, and so may stand inside a marker — an
/// HTML comment: `urn:uuid:` and a lowercase, hyphenated UUID, the only
/// form fl mints. ⚠ Nothing else: `Iri::parse` accepts `urn:x:a--><b>`,
/// and a hand-written ledger line can carry it; written into a marker it
/// would close the comment early.
pub fn markable(id: &Iri) -> bool {
    let Some(uuid) = id.as_str().strip_prefix("urn:uuid:") else {
        return false;
    };
    let widths: Vec<usize> = uuid.split('-').map(str::len).collect();
    widths == [8, 4, 4, 4, 12]
        && uuid
            .bytes()
            .all(|b| b == b'-' || b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The marker every decision comment starts with (spec §4.3); `None` for
/// an id fl does not write ([`markable`]).
pub fn marker(id: &Iri) -> Option<String> {
    markable(id).then(|| format!("{MARKER_OPEN}{}{MARKER_CLOSE}", json!({ "id": id.as_str() })))
}

/// The decision one line marks, if it is a well-formed marker.
fn marker_line(line: &str) -> Option<Iri> {
    let inner = line.strip_prefix(MARKER_OPEN)?.strip_suffix(MARKER_CLOSE)?;
    let v: Value = serde_json::from_str(inner).ok()?;
    Iri::parse(v.get("id")?.as_str()?).ok().filter(markable)
}

/// The decision a comment marks: its first well-formed marker line outside
/// a fenced block, wherever it is in the body — an edit that pushes it
/// down keeps it, and a broken marker-like line above it is passed over.
///
/// ⚠ Never a line inside a fence: an excerpt is fenced, and the gate output
/// it holds is the project's, so a marker quoted there marks nothing — not
/// even when someone deletes fl's own marker line. Only the first: a
/// second marker further down marks nothing. Whose comment may mark a
/// decision at all is the caller's to judge (`GithubLedger::posted`).
///
/// A fence here is any line of three or more backticks, whatever follows
/// them or however far it is indented; tildes open none. fl renders no
/// other kind, so an edit that adds one above the marker costs at most a
/// second comment, never a lost one.
pub fn marked(body: &str) -> Option<Iri> {
    // The open fence's length, while inside one.
    let mut fence: Option<usize> = None;
    for line in body.lines().map(str::trim) {
        let ticks = line.len() - line.trim_start_matches('`').len();
        if ticks >= 3 {
            match fence {
                None => {
                    fence = Some(ticks);
                    continue;
                }
                Some(open) if ticks >= open && line[ticks..].trim().is_empty() => {
                    fence = None;
                    continue;
                }
                Some(_) => {}
            }
        }
        if fence.is_some() {
            continue;
        }
        if let Some(id) = marker_line(line) {
            return Some(id);
        }
    }
    None
}

/// A full commit id: forty hex digits.
pub fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether the text before ends in `GH`, any case: a `-` there would make
/// `GH-<n>` a reference to issue `n`.
fn after_gh(before: &[char]) -> bool {
    matches!(before, [.., 'g' | 'G', 'h' | 'H'])
}

/// `c`, which follows `before`, as it is written in `ctx`.
fn push_escaped(out: &mut String, c: char, before: &[char], ctx: Context) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        // A zero-width space after each: `@name` no longer mentions anyone,
        // and `#1` and `GH-1` no longer link an issue.
        '@' => out.push_str("@&#8203;"),
        '#' => out.push_str("#&#8203;"),
        '-' if after_gh(before) => out.push_str("-&#8203;"),
        '\r' => {}
        '\n' if ctx == Context::Markdown => out.push_str("<br>"),
        '\n' => out.push(' '),
        // `$` too: GitHub renders `$…$` as math. Not `!`: it matters only
        // before `[`, which is escaped. Not inside an HTML element, where
        // markdown is not read and a backslash would show.
        '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '~' | '|' | '$'
            if ctx == Context::Markdown =>
        {
            out.push('\\');
            out.push(c);
        }
        c => out.push(c),
    }
}

/// `s` escaped for `ctx`, holding at most `limit` bytes: cut after a whole
/// escaped character, and ending in `…` when cut.
fn escape_in(s: &str, limit: usize, ctx: Context) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut full = String::with_capacity(s.len());
    // How long the escaped text is after each character.
    let mut ends = Vec::with_capacity(chars.len());
    for (i, c) in chars.iter().enumerate() {
        push_escaped(&mut full, *c, &chars[..i], ctx);
        ends.push(full.len());
    }
    if full.len() <= limit {
        return full;
    }
    let room = limit.saturating_sub('…'.len_utf8());
    let keep = ends.iter().copied().take_while(|e| *e <= room).last().unwrap_or(0);
    let mut out = full[..keep].to_string();
    out.push('…');
    out
}

/// `s` as markdown text in a comment (spec §4.2): HTML and markdown
/// escaped, `@`, `#` and `GH-` neutralised, a newline a `<br>` — so a
/// name never notifies anyone, links an issue, opens a tag or breaks a
/// table.
pub fn escape(s: &str) -> String {
    escape_in(s, usize::MAX, Context::Markdown)
}

/// [`escape`], holding at most `limit` bytes, ending in `…` when cut.
pub fn escape_capped(s: &str, limit: usize) -> String {
    escape_in(s, limit, Context::Markdown)
}

/// `s` inside an HTML element: entities and the neutralised `@`, `#` and
/// `GH-` only, a newline a space; at most `limit` bytes.
fn escape_html_capped(s: &str, limit: usize) -> String {
    escape_in(s, limit, Context::Html)
}

/// The line a comment posted as the decision is made adds (spec §4.2):
/// whether its state change completed. `completed`: the command's state
/// change returned without error.
pub fn state_line(outcome: &Outcome, completed: bool) -> String {
    match outcome {
        Outcome::Move {
            allowed: false,
            from,
            ..
        } => format!("The move was refused: the record stays `{}`.", from.as_wire()),
        Outcome::Move { to, .. } if completed => {
            format!("The state change completed: the record is now `{}`.", to.as_wire())
        }
        Outcome::Move { from, .. } => format!(
            "The move was allowed, but its state change did not complete: the record may still \
             be `{}`.",
            from.as_wire()
        ),
        Outcome::Check { .. } => "A check changes no state.".into(),
        Outcome::Reproduce {
            accepted: false, ..
        } => "The reproduction was refused: the finding is unchanged.".into(),
        Outcome::Reproduce { .. } if completed => {
            "The state change completed: the finding records this gate as its reproduction."
                .into()
        }
        Outcome::Reproduce { .. } => {
            "The reproduction was accepted, but its state change did not complete.".into()
        }
        Outcome::Verify { closed: false, .. } => {
            "The finding stays open: the repair is not done.".into()
        }
        Outcome::Verify { .. } if completed => {
            "The state change completed: the finding is closed.".into()
        }
        Outcome::Verify { .. } => {
            "The finding passed its verification, but closing it did not complete.".into()
        }
        Outcome::Attempt { .. } => "An attempt changes no state.".into(),
    }
}

/// What was decided, in a word or two — refused ones included (decision
/// 11).
fn verdict_word(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Move { allowed: true, .. } => "allowed".into(),
        Outcome::Move { .. } => "refused".into(),
        Outcome::Check { transition } if transition.passed => "passed".into(),
        Outcome::Check { .. } => "failed".into(),
        Outcome::Reproduce { accepted: true, .. } => "accepted".into(),
        Outcome::Reproduce { .. } => "refused".into(),
        Outcome::Verify { closed: true, .. } => "closed".into(),
        Outcome::Verify { .. } => "not closed".into(),
        Outcome::Attempt { status } => status.as_wire().into(),
    }
}

/// The marker, what was decided, by whom, where its evidence is, a move's
/// states, and the state line when there is one.
fn head(view: &DecisionView, repo: &str, state: Option<&str>) -> String {
    let d = &view.decision;
    let mut out = String::new();
    // ⚠ An id fl does not write gets no marker (`markable`); no caller
    // posts such a comment.
    if let Some(m) = marker(&d.id) {
        out.push_str(&m);
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "### fl {}: {}\n\n",
        d.kind().as_wire(),
        verdict_word(&d.outcome)
    ));
    out.push_str(&format!(
        "Decided by {} at {}. ",
        escape_capped(&view.by, CELL_LIMIT),
        d.at.as_str()
    ));
    // ⚠ Only a full commit id is linked: anything else is not a commit.
    match view.commit.as_deref().filter(|c| is_sha(c)) {
        Some(c) => out.push_str(&format!(
            "Evidence: ledger commit [{}](https://github.com/{repo}/commit/{c}).\n\n",
            &c[..7]
        )),
        None => out.push_str(
            "Evidence: the ledger holds it; this comment does not name its commit.\n\n",
        ),
    }
    if let Outcome::Move { from, to, .. } = &d.outcome {
        out.push_str(&format!("From `{}` to `{}`.\n\n", from.as_wire(), to.as_wire()));
    }
    if let Some(line) = state {
        out.push_str(line);
        out.push_str("\n\n");
    }
    out
}

/// The attempt, or the first `shown` runs, as a table (spec §4.2), saying
/// how many runs it leaves out.
fn table(view: &DecisionView, shown: usize) -> String {
    if let Some(a) = &view.attempt {
        return format!(
            "| adapter | status | duration | tokens in | tokens out | cost |\n\
             |---|---|---|---|---|---|\n\
             | {} | {} | {} ms | {} | {} | {}.{:06} USD |\n",
            escape_capped(&a.adapter, CELL_LIMIT),
            a.status.as_wire(),
            a.duration_ms,
            a.tokens_in,
            a.tokens_out,
            a.cost_usd_micros / 1_000_000,
            a.cost_usd_micros % 1_000_000
        );
    }
    if view.rows.is_empty() {
        return "No gate ran for this decision.\n".into();
    }
    let mut out =
        String::from("| for | gate | verdict | population | commit | duration |\n|---|---|---|---|---|---|\n");
    for row in view.rows.iter().take(shown) {
        let r = &row.run;
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} ms |\n",
            escape_capped(&row.role, CELL_LIMIT),
            escape_capped(&row.gate, CELL_LIMIT),
            r.verdict.describe().0,
            r.verdict
                .population()
                .map_or_else(|| "—".to_string(), |p| p.to_string()),
            escape_capped(r.commit.get(..7).unwrap_or(&r.commit), CELL_LIMIT),
            r.duration_ms
        ));
    }
    let hidden = view.rows.len().saturating_sub(shown);
    if hidden > 0 {
        out.push_str(&format!(
            "\n… and {hidden} more runs: the ledger commit holds every one.\n"
        ));
    }
    out
}

/// The entries the decision names that were not found: the first
/// [`MISSING_SHOWN`], then how many more.
fn missing(view: &DecisionView) -> String {
    let mut out = String::new();
    for id in view.missing.iter().take(MISSING_SHOWN) {
        out.push_str(&format!(
            "\nNot found in the ledger: {}.\n",
            escape_capped(id.as_str(), CELL_LIMIT)
        ));
    }
    let more = view.missing.len().saturating_sub(MISSING_SHOWN);
    if more > 0 {
        out.push_str(&format!("\n… and {more} more entries not found.\n"));
    }
    out
}

/// Each excerpt `view` holds, with what it is about: a run's error detail
/// and output, an attempt's output. ⚠ Asked only for a private repository
/// (decision 2).
fn blocks(view: &DecisionView) -> Vec<Block> {
    let mut out = Vec::new();
    for row in &view.rows {
        let (label, detail) = row.run.verdict.describe();
        let mut text = String::new();
        if matches!(row.run.verdict, Verdict::Error { .. }) {
            text.push_str(&format!("error: {detail}\n"));
        }
        if let Some(e) = row.run.output_excerpt.as_deref() {
            text.push_str(e);
        }
        if !text.is_empty() {
            out.push(Block {
                title: escape_html_capped(
                    &format!("{} / {}: {label}", row.role, row.gate),
                    CELL_LIMIT,
                ),
                text,
            });
        }
    }
    if let Some(a) = &view.attempt
        && let Some(e) = a.output_excerpt.as_deref()
        && !e.is_empty()
    {
        out.push(Block {
            title: escape_html_capped(
                &format!("{}: {}", a.adapter, a.status.as_wire()),
                CELL_LIMIT,
            ),
            text: e.to_string(),
        });
    }
    out
}

/// A fence longer than any run of backticks in `text`, and never shorter
/// than three.
fn fence_for(text: &str) -> String {
    let (mut longest, mut run) = (0usize, 0usize);
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

/// One excerpt, folded (spec §4.2). Inside the fence nothing is markup:
/// mentions and references neither notify nor link.
fn block(title: &str, text: &str) -> String {
    let fence = fence_for(text);
    let end = if text.ends_with('\n') { "" } else { "\n" };
    format!("<details><summary>{title}</summary>\n\n{fence}\n{text}{end}{fence}\n\n</details>\n")
}

fn assemble(view: &DecisionView, repo: &str, state: Option<&str>, blocks: &[Block]) -> String {
    let mut out = head(view, repo, state);
    out.push_str(&table(view, view.rows.len()));
    out.push_str(&missing(view));
    for b in blocks {
        out.push('\n');
        out.push_str(&block(&b.title, &b.text));
    }
    out
}

/// The comment for `view` on `repo` (spec §4.2). `state`: the line a
/// comment posted as the decision is made adds; `None` for one recovered
/// later.
pub fn render(view: &DecisionView, repo: &str, visibility: Visibility, state: Option<&str>) -> String {
    // ⚠ Decision 2: excerpts only on a private repository.
    let blocks = match visibility {
        Visibility::Private => blocks(view),
        Visibility::NotPrivate => Vec::new(),
    };
    assemble(view, repo, state, &blocks)
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github --lib ledger::render::tests::`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Each run with `cargo test -p fl-github --lib ledger::render::tests::`:

1. Each arm of `push_escaped`, one at a time — `&`, `<`, `>`, `@`, `#`, the `GH-` arm, `\r`, the markdown `\n` arm, the markdown backslash set: delete it → `escape_neutralises_markup_mentions_and_references` red (for `>`, its second assertion).
2. The `GH-` arm's lookbehind: make `after_gh` return `true` → the same test red (its third assertion: `-` not after `GH` stays).
3. The context conjunct on the backslash set: drop `if ctx == Context::Markdown` → `a_name_inside_an_html_element_gets_no_markdown_escapes` red.
4. The newline in an HTML element: delete the `'\n' => out.push(' ')` arm → the same test red (the newline passes through).
5. `escape_in`'s early return: delete it → `a_long_name_is_cut_on_a_whole_escape_and_says_so` red (the exact-limit case gains `…`).
6. Its bound counts the ellipsis: replace `limit.saturating_sub('…'.len_utf8())` with `limit` → the same test red (`&&&&` keeps two escapes).
7. Its boundary: change `*e <= room` to `*e < room` → the same test red (`"abcdef"` at 4 gives `…`, not `a…`).
8. `markable`'s widths conjunct: drop `widths == [8, 4, 4, 4, 12] &&` → `a_hostile_decision_id_gets_no_marker_and_nothing_closes_a_comment` red (`urn:uuid:0000000-0000-7000-8000-00000000005a0`: hex, wrong widths).
9. `markable`'s hex conjunct: drop the `.all(…)` → the same test red (`urn:uuid:gggggggg-…`: right widths, not hex).
10. `markable`'s prefix: return `true` from the `else` → the same test red (`urn:x:…`, `fl:…`, `https://…`).
11. `marker` only for a markable id: drop `markable(id).then(…)` for `Some(…)` → the same test red (a marker, and `-->`, in the body).
12. `head` writes the marker only when there is one: replace `if let Some(m) = marker(&d.id)` with an unconditional marker built from the id → the same test red.
13. `marked` finds the marker wherever it is: make the loop look at the first line only (`.take(1)`) → `the_first_marker_line_counts_wherever_it_is` red ("moved down by an edit").
14. …only the first: keep scanning after a match and return the last one found → the same test red ("only the first").
15. …passing over a broken marker-like line: return `marker_line(line)` for the first line that starts with `MARKER_OPEN`, well-formed or not → the same test red (the broken line above).
16. Never inside a fence: delete the `if fence.is_some() { continue; }` → the same test red (the quoted marker counts).
17. The fence opens on three or more backticks: change `ticks >= 3` to `ticks >= 5` → the same test red (a four-backtick fence is not seen).
18. A fence closes only on a run at least as long: drop `ticks >= open &&` → the same test red ("a shorter run does not close").
18a. …and only on a bare run: drop `&& line[ticks..].trim().is_empty()` → the same test red ("nor a run with text after it").
19. After a fence closes, markers count again: never reset `fence` to `None` → the same test red (the last case).
20. `marked`'s close: drop `.strip_suffix(MARKER_CLOSE)?` and parse `inner` as is → the same test red (the unclosed marker).
21. `marked` reads only an id fl writes: drop `.filter(markable)` → the same test red (`urn:x:a`).
22. `is_sha`'s two conjuncts: drop `s.len() == 40 &&` → `only_a_full_commit_id_is_linked` red ("0123456"); drop the hex check → the same test red.
23. `head` links only a full id: drop `.filter(|c| is_sha(c))` → `only_a_full_commit_id_is_linked` red.
24. `head` escapes who decided: write `view.by.clone()` → `who_decided_the_adapter_the_commit_and_a_title_are_escaped` red.
25. `table` escapes the attempt's adapter: write `a.adapter.clone()` → the same test red.
26. `table` escapes the run's commit: write `r.commit.get(..7).unwrap_or(&r.commit).to_string()` → the same test red.
27. `blocks` escapes a title for an HTML element: use `escape_capped` (markdown) for the run's title → the same test red (`\|` in the summary); write the title unescaped → the same test red (a second `<!--`). Do both for the attempt's title too.
28. `head`'s move line: delete the `if let Outcome::Move { from, to, .. }` block → `a_moves_comment_says_what_was_decided_by_whom_and_where_its_evidence_is` red. (Only a move has states to name.)
29. `head`'s state line, only when given: replace `if let Some(line) = state { … }` with `let line = state.unwrap_or("The state change completed."); out.push_str(line); out.push_str("\n\n");` → `the_heading_names_the_kind_and_the_outcome_refused_ones_included` red (its last assertion); delete the block instead → `a_moves_comment…` red.
30. Each arm of `verdict_word` (nine): swap one arm's word at a time → `the_heading_names_the_kind_and_the_outcome_refused_ones_included` red.
31. Each arm of `state_line`, and each `if completed` guard (three): drop one guard at a time → `the_state_line_says_whether_the_state_change_completed` red.
32. `table`: the attempt first — move the `if let Some(a)` block below `view.rows.is_empty()` → `an_attempts_comment_shows_its_adapter_status_duration_tokens_and_cost` red ("No gate ran"); the empty-rows line — delete it → `the_heading_names…` red; `take(shown)` → `take(usize::MAX)` → `a_table_shown_in_part…` red; the hidden note's `if hidden > 0` → `if true` → the same test red; the population dash → replace with `"0"` → `a_name_in_the_table_is_escaped_and_cut_and_an_error_has_no_population` red; the short commit → drop `.get(..7)` → `a_moves_comment…` red.
33. `missing`: `take(MISSING_SHOWN)` → `take(usize::MAX)` → `a_table_shown_in_part…` red; `if more > 0` → `if true` → the same test red (its last assertion).
34. `render` shows excerpts on a private repository only: make the `NotPrivate` arm call `blocks(view)` → `excerpts_show_only_on_a_private_repository_each_in_a_fence_longer_than_its_backticks` red.
35. `blocks`: the error prefix — delete it → `an_errors_detail_and_an_attempts_excerpt_show_and_an_empty_one_does_not` red; `!text.is_empty()` → `true` → the same test red (three `<details>`); the attempt's `!e.is_empty()` conjunct — drop it → the same test red (its last assertion).
36. `fence_for`'s floor and margin: `longest.max(2) + 1` → `longest.max(2)` → `the_fence_outruns_the_longest_run_of_backticks_not_their_sum` red.
37. `fence_for`'s run reset (each run on its own): delete `run = 0;` → the same test red (`a ``` b ```` c ``` d` gives eleven, not five).
38. `block` ends the text with a newline before the fence: make `end` always `""` → `an_errors_detail…` red (`did it```` on one line).

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/ledger/render.rs crates/github/src/ledger/mod.rs
git commit -m "feat(github): render a decision comment

The marker, what was decided and by whom, a link to the ledger commit, a
move's states, the line saying whether the state change completed, and a
table of the runs or the attempt (spec 4.2). Names are escaped, @, # and
GH- neutralised, cells capped; a marker only for an id fl writes, read
from the first marker line. Excerpts only on a private repository, each in
a fence longer than its backticks. Refused decisions say so (decision
11). Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 4: A comment always fits — excerpts cut first

Spec §4.2: "The body is at most 60,000 bytes of UTF-8 … truncating excerpts first and saying so, so the render always fits." Ruling 10: each excerpt is cut to the largest equal share that fits; if even one byte of each does not fit, excerpts are left out; if the rest still does not fit, the table's last rows go. Each step says so. Cells are already capped (Task 3), so the header and one row are a few hundred bytes, and dropping rows always ends inside the limit.

**Files:**
- Modify: `crates/github/src/ledger/render.rs` (`assemble` and `render` replaced; `Share` and `cut_at` added; tests)

**Interfaces:**
- Consumes: Task 3's `head`, `table`, `missing`, `blocks`, `block`, `Block`, `COMMENT_LIMIT`.
- Produces: no new public item. `render` now guarantees `render(..).len() <= COMMENT_LIMIT`.

- [ ] **Step 1: Write the failing tests**

In `crates/github/src/ledger/render.rs`, inside `mod tests`, add:

```rust
    /// `n` runs of gate `gate`, each with `excerpt`, in one move.
    fn many(n: u64, gate: &str, excerpt: Option<&str>) -> DecisionView {
        let rows = (1..=n)
            .map(|i| row("launch", gate, run(i, Verdict::from_predicate(false, 1), excerpt)))
            .collect();
        view_of(moved(false), rows)
    }

    #[test]
    fn a_body_that_fits_is_left_whole() {
        let excerpt = "x".repeat(1_000);
        let body = render(&many(1, "g", Some(&excerpt)), "acme/widgets", Visibility::Private, None);
        assert!(body.contains(&format!("\n{excerpt}\n")), "the whole excerpt");
        assert!(!body.contains("to fit in one comment"), "{body}");
    }

    // ⚠ Spec §4.2: excerpts first, each to the largest equal share that
    // fits; every row stays, a short excerpt stays whole, and the comment
    // says what was cut.
    #[test]
    fn a_body_over_the_limit_cuts_excerpts_first_using_the_room_left_and_says_so() {
        let mut v = many(20, "g", Some(&"x".repeat(10_000)));
        v.rows.push(row("launch", "g", run(21, Verdict::from_predicate(false, 1), Some("short"))));
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(body.len() <= COMMENT_LIMIT, "{} bytes", body.len());
        assert!(body.len() > COMMENT_LIMIT - 100, "the room is used: {} bytes", body.len());
        assert_eq!(body.matches("| launch | g |").count(), 21, "every row stays");
        assert_eq!(body.matches("<details>").count(), 21, "every excerpt stays, cut");
        assert!(body.contains("\nshort\n"), "a short excerpt stays whole");
        assert!(
            body.contains("Excerpts were cut to fit in one comment. The ledger commit holds them \
                           in full."),
            "{body}"
        );
        assert_eq!(marked(&body), Some(seq_iri(90)));
    }

    // An excerpt is cut on a whole character: `render` returns a `String`,
    // so a cut inside one would panic.
    #[test]
    fn an_excerpt_is_cut_on_a_whole_character() {
        let body = render(
            &many(1, "g", Some(&"é".repeat(40_000))),
            "acme/widgets",
            Visibility::Private,
            None,
        );
        assert!(body.len() <= COMMENT_LIMIT, "{} bytes", body.len());
        assert!(body.contains("é\n```\n\n</details>"), "the fence closes after a whole é");
    }

    // When even a byte of each excerpt does not fit, excerpts are left out,
    // the table stays whole, and the comment says so.
    #[test]
    fn excerpts_that_do_not_fit_even_cut_are_left_out_and_the_comment_says_so() {
        let body = render(
            &many(200, &"g".repeat(190), Some(&"y".repeat(500))),
            "acme/widgets",
            Visibility::Private,
            None,
        );
        assert!(body.len() <= COMMENT_LIMIT, "{} bytes", body.len());
        assert!(!body.contains("<details>"), "no excerpt");
        assert!(
            body.contains("Output excerpts are not shown: they do not fit in one comment. The \
                           ledger commit holds them."),
            "{body}"
        );
        assert_eq!(body.matches("| launch | ggg").count(), 200, "every row stays");
    }

    // Then the table's last rows, as many as fit, saying how many went —
    // and nothing about excerpts where there are none to show: none were
    // kept (`None`), or the repository is not private.
    #[test]
    fn a_table_too_long_for_one_comment_drops_its_last_rows_and_says_so() {
        let cases = [
            (many(1_000, &"g".repeat(190), None), Visibility::Private),
            (many(1_000, &"g".repeat(190), Some("y")), Visibility::NotPrivate),
        ];
        for (v, visibility) in cases {
            let body = render(&v, "acme/widgets", visibility, None);
            assert!(body.len() <= COMMENT_LIMIT, "{} bytes", body.len());
            assert!(body.len() > COMMENT_LIMIT - 1_000, "as many rows as fit: {} bytes", body.len());
            assert!(body.contains("more runs: the ledger commit holds every one."), "{body}");
            assert!(!body.contains("Output excerpts are not shown"), "{body}");
            assert!(!body.contains("<details>"), "{body}");
            assert_eq!(marked(&body), Some(seq_iri(90)));
        }
    }

    // ⚠ Whatever the excerpt, the comment fits and is marked.
    #[test]
    fn every_comment_fits_and_is_marked() {
        for size in [0, 1, 59_000, 60_000, 61_000, 500_000] {
            let body = render(
                &many(1, "g", Some(&"z".repeat(size))),
                "acme/widgets",
                Visibility::Private,
                Some("A check changes no state."),
            );
            assert!(body.len() <= COMMENT_LIMIT, "{size}: {} bytes", body.len());
            assert_eq!(marked(&body), Some(seq_iri(90)), "{size}");
        }
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-github --lib ledger::render::tests::`
Expected: `a_body_over_the_limit…`, `an_excerpt_is_cut…`, `excerpts_that_do_not_fit…`, `a_table_too_long…` and `every_comment_fits_and_is_marked` FAIL (bodies over 60,000 bytes); `a_body_that_fits_is_left_whole` passes.

- [ ] **Step 3: Implement**

In `render.rs`, replace `assemble` and `render` with:

```rust
/// How much of each excerpt a comment keeps.
#[derive(Debug, Clone, Copy)]
enum Share {
    All,
    /// At most this many bytes of each, cut on a whole character.
    Bytes(usize),
    None,
}

/// `s` cut to at most `n` bytes, on a whole character.
fn cut_at(s: &str, n: usize) -> &str {
    let mut i = n.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

/// The comment with the first `rows` runs and each excerpt as `share` says,
/// saying what it left out.
fn assemble(
    view: &DecisionView,
    repo: &str,
    state: Option<&str>,
    rows: usize,
    blocks: &[Block],
    share: Share,
) -> String {
    let mut out = head(view, repo, state);
    out.push_str(&table(view, rows));
    out.push_str(&missing(view));
    if blocks.is_empty() {
        return out;
    }
    if let Share::None = share {
        out.push_str(
            "\nOutput excerpts are not shown: they do not fit in one comment. The ledger commit \
             holds them.\n",
        );
        return out;
    }
    let mut cut = false;
    for b in blocks {
        let text = match share {
            Share::Bytes(n) => cut_at(&b.text, n),
            Share::All | Share::None => b.text.as_str(),
        };
        cut |= text.len() < b.text.len();
        out.push('\n');
        out.push_str(&block(&b.title, text));
    }
    if cut {
        out.push_str(
            "\nExcerpts were cut to fit in one comment. The ledger commit holds them in full.\n",
        );
    }
    out
}

/// The largest `n` in `lo..=hi` for which `fits(n)`, given `fits(lo)`.
fn largest(mut lo: usize, mut hi: usize, fits: impl Fn(usize) -> bool) -> usize {
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// The comment for `view` on `repo` (spec §4.2), at most
/// [`COMMENT_LIMIT`] bytes. `state`: the line a comment posted as the
/// decision is made adds; `None` for one recovered later.
///
/// ⚠ Over the limit, excerpts go first — each cut to the largest equal
/// share that fits, then left out — and only then the table's last rows.
/// Each step says so.
pub fn render(view: &DecisionView, repo: &str, visibility: Visibility, state: Option<&str>) -> String {
    // ⚠ Decision 2: excerpts only on a private repository.
    let blocks = match visibility {
        Visibility::Private => blocks(view),
        Visibility::NotPrivate => Vec::new(),
    };
    let all = view.rows.len();
    let fits = |rows: usize, share: Share| {
        assemble(view, repo, state, rows, &blocks, share).len() <= COMMENT_LIMIT
    };
    if fits(all, Share::All) {
        return assemble(view, repo, state, all, &blocks, Share::All);
    }
    if fits(all, Share::None) {
        if !fits(all, Share::Bytes(1)) {
            return assemble(view, repo, state, all, &blocks, Share::None);
        }
        let longest = blocks.iter().map(|b| b.text.len()).max().unwrap_or(1);
        let n = largest(1, longest, |n| fits(all, Share::Bytes(n)));
        return assemble(view, repo, state, all, &blocks, Share::Bytes(n));
    }
    let rows = largest(0, all, |r| fits(r, Share::None));
    assemble(view, repo, state, rows, &blocks, Share::None)
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github --lib ledger::render::tests::`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Each run with `cargo test -p fl-github --lib ledger::render::tests::`:

1. Excerpts before rows (ordering): replace `if fits(all, Share::None) {` with `if false {` → `a_body_over_the_limit_cuts_excerpts_first_using_the_room_left_and_says_so` red (rows dropped, excerpts gone).
2. Left out when even a byte does not fit: delete the `if !fits(all, Share::Bytes(1)) { … }` block → `excerpts_that_do_not_fit_even_cut_are_left_out_and_the_comment_says_so` red (over the limit).
3. The largest share: replace `largest(1, longest, …)` with `1` → `a_body_over_the_limit…` red (`> COMMENT_LIMIT - 100`).
4. `largest`'s step keeps the fitting half: change `lo = mid` to `hi = mid - 1` in the `if fits(mid)` arm → the same test red.
5. The largest number of rows: replace `largest(0, all, …)` with `0` → `a_table_too_long_for_one_comment_drops_its_last_rows_and_says_so` red.
6. Rows dropped at all: replace it with `all` → the same test and `every_comment_fits_and_is_marked` red.
7. `cut_at` on a whole character: delete the `while` loop → `an_excerpt_is_cut_on_a_whole_character` panics (red).
8. The cut note: delete `cut |= …` → `a_body_over_the_limit…` red.
9. The left-out note: delete its `push_str` → `excerpts_that_do_not_fit…` red.
10. No note where there is no excerpt (a later phase's silence): delete `if blocks.is_empty() { return out; }` → `a_table_too_long_for_one_comment_drops_its_last_rows_and_says_so` red (both cases say excerpts do not fit).

Run mutation 4's sibling — `div_ceil(2)` → `/ 2` in `largest` — under `timeout 120 cargo test …`: it does not fail, it loops forever (`mid` never passes `lo`); the timeout ending it counts as red.

Not a guard: the first `if fits(all, Share::All)` return. Without it the search below reaches the same body (`Share::Bytes(longest)` cuts nothing and adds no note); it only saves the searches.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/ledger/render.rs
git commit -m "feat(github): a decision comment always fits in 60,000 bytes

Over the limit, each excerpt is cut to the largest equal share that fits,
then excerpts are left out, then the table's last rows are dropped; each
step says so and points at the ledger commit (spec 4.2). Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 5: A decision read back from the ledger, and its view

What recovery renders from (spec §4.2, §4.3): each decision under an item, with who wrote it and where its line is; the commit that added that line; the runs of many gates read at one head; and the view of a decision over the entries found for it, through the local catalog.

**Blast radius:** `lines` (read by `read`, `append`'s de-duplication and `quarantine`) keeps its signature and result; its body moves into `located`, which also keeps who wrote each line and where. Every existing read test pins that nothing else changed.

**Files:**
- Modify: `crates/github/src/ledger/read.rs` (`Located`, `located`; `lines` over it; `Published`, `published_decisions`, `commit_of`, `runs_of`; tests)
- Modify: `crates/github/src/ledger/render.rs` (`Catalogued`, `candidate_gates`, `view`, `role`; tests)
- Modify: `crates/github/src/ledger/mod.rs` (`pub use read::{Note, Published};` replaces `pub use read::Note;`)

**Interfaces:**
- Consumes: `snapshot`, `blame` (`read.rs`, `git.rs`); `render::{DecisionView, RunRow, is_sha}` (Task 3).
- Produces:
  - in `read.rs`: `pub struct Published { pub decision: Decision, pub by: String, pub file: String, pub line: u64 }`; on `GithubLedger`: `pub fn published_decisions(&self, subject: &Iri) -> Result<(String, Vec<Published>), StoreError>` (the head read, and the decisions in ledger order), `pub fn commit_of(&self, head: &str, p: &Published) -> Option<String>`, `pub fn runs_of(&self, gates: &[GateId]) -> Result<Vec<GateRun>, StoreError>`; `pub(crate) struct Located { pub line: Line, pub by: String, pub file: String, pub n: u64 }`, `pub(crate) fn located(&self, snap: &Snapshot, area: Area, dir: &str) -> Result<Vec<Located>, StoreError>`.
  - in `render.rs`: `pub struct Catalogued { pub names: BTreeMap<GateId, String>, pub transitions: BTreeMap<String, Vec<GateId>> }` (`Default`), `pub fn candidate_gates(outcome: &Outcome, cat: &Catalogued) -> Vec<GateId>`, `pub fn view(decision: Decision, by: String, commit: Option<String>, runs: &[GateRun], attempts: &[Attempt], cat: &Catalogued) -> DecisionView`.

- [ ] **Step 1: Write the failing tests**

In `crates/github/src/ledger/read.rs`, inside `mod tests`, add `use crate::fake::USER_LOGIN;`, `use crate::ledger::{disclose, render};`, `use fl_core::split::{Batch, RemoteLedger};` and `use fl_core::verdict::Verdict;` to the imports (keep the existing ones), then add:

```rust
    fn decision_batch(n: u64, runs: Vec<GateRun>) -> Batch {
        Batch {
            decision: sample_decision(n, &record(), runs.iter().filter_map(|r| r.id.clone()).collect()),
            runs,
            attempts: vec![],
        }
    }

    // Spec §4.2: who wrote each decision, and where its line is.
    #[test]
    fn published_decisions_name_who_wrote_each_and_where() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        let (b1, b2) = (decision_batch(1, vec![]), decision_batch(2, vec![]));
        l.publish(&b1).unwrap();
        l.publish(&b2).unwrap();
        let (head, published) = l.published_decisions(record().iri()).unwrap();
        assert_eq!(Some(head), fake.ledger_head());
        let seg = layout::segment_path(&layout::dir(Area::Decisions, record().iri()), 1);
        assert_eq!(
            published,
            vec![
                Published {
                    decision: b1.decision,
                    by: USER_LOGIN.into(),
                    file: seg.clone(),
                    line: 1,
                },
                Published {
                    decision: b2.decision,
                    by: USER_LOGIN.into(),
                    file: seg,
                    line: 2,
                },
            ]
        );
    }

    #[test]
    fn commit_of_names_the_commit_that_added_the_line_or_nothing() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        let c1 = l.publish(&decision_batch(1, vec![])).unwrap();
        let c2 = l.publish(&decision_batch(2, vec![])).unwrap();
        let (head, published) = l.published_decisions(record().iri()).unwrap();
        assert_eq!(l.commit_of(&head, &published[0]), c1);
        assert_eq!(l.commit_of(&head, &published[1]), c2);
        // A blame that names no commit gives none — never its message as one.
        body_next(&fake, "/graphql", 200, json!({"data": {"repository": {"object": null}}}));
        assert_eq!(l.commit_of(&head, &published[0]), None);
    }

    #[test]
    fn runs_of_reads_every_gate_at_one_head_in_one_listing() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        let other = GateId(seq_iri(8));
        let (a, b) = (run(1), sample_record_run(2, &other, Some(&record())));
        l.publish(&decision_batch(1, vec![a.clone(), b.clone()])).unwrap();
        fake.state().requests.clear();
        assert_eq!(l.runs_of(&[gate(), other]).unwrap(), vec![a, b]);
        let listings = fake.state().requests.iter().filter(|r| *r == "POST /graphql").count();
        assert_eq!(listings, 1, "one listing for every gate");
        fake.state().requests.clear();
        assert!(l.runs_of(&[]).unwrap().is_empty());
        assert!(fake.state().requests.is_empty(), "no gate, no request");
    }

    // ⚠ Spec §4.2: a comment recovered from the ledger renders exactly as
    // the one posted live did, from the local copies projected as
    // published — on either visibility.
    #[test]
    fn a_recovered_decision_renders_as_the_live_one_did() {
        for visibility in ["private", "public"] {
            let (fake, local, _root) = world();
            fake.state().repos[0].visibility = visibility.into();
            let c = client(&fake);
            let l = open(&c, &local);
            let mut r = run(1);
            r.verdict = Verdict::error("spawn failed at /home/someone/bin/lint");
            let b = decision_batch(1, vec![r.clone()]);
            let commit = l.publish(&b).unwrap();
            let seen = l.visibility().unwrap();
            let mut cat = render::Catalogued::default();
            cat.names.insert(gate(), "no-bug".into());
            cat.transitions.insert("launch".into(), vec![gate()]);
            let live = render::view(
                b.decision.clone(),
                USER_LOGIN.into(),
                commit,
                &[disclose::run(&r, seen)],
                &[],
                &cat,
            );
            let (head, published) = l.published_decisions(record().iri()).unwrap();
            let p = &published[0];
            let gates = render::candidate_gates(&p.decision.outcome, &cat);
            let recovered = render::view(
                p.decision.clone(),
                p.by.clone(),
                l.commit_of(&head, p),
                &l.runs_of(&gates).unwrap(),
                &[],
                &cat,
            );
            assert_eq!(
                render::render(&recovered, "acme/widgets", seen, None),
                render::render(&live, "acme/widgets", seen, None),
                "{visibility}"
            );
        }
    }
```

In `crates/github/src/ledger/render.rs`, inside `mod tests`, add:

```rust
    fn t(name: &str) -> TransitionOutcome {
        TransitionOutcome {
            transition: name.into(),
            passed: true,
        }
    }

    fn verify_of(reproduction: &GateId, regressions: &[&GateId]) -> Outcome {
        Outcome::Verify {
            reproduction: reproduction.clone(),
            reproduction_passed: true,
            regressions: regressions.iter().map(|g| (*g).clone()).collect(),
            closed: false,
        }
    }

    fn gates3() -> (GateId, GateId, GateId) {
        (GateId(seq_iri(1)), GateId(seq_iri(2)), GateId(seq_iri(3)))
    }

    // Spec §4.2: a decision names transitions, not gates; its runs are
    // found through the local catalog. A verify's passing neighbours are
    // named nowhere but the catalog.
    #[test]
    fn a_decisions_gates_are_found_through_the_local_catalog() {
        let (g1, g2, g3) = gates3();
        let mut cat = Catalogued::default();
        for (n, g) in [(1, &g1), (2, &g2), (3, &g3)] {
            cat.names.insert(g.clone(), format!("g{n}"));
        }
        cat.transitions.insert("launch".into(), vec![g1.clone(), g2.clone()]);
        cat.transitions.insert("ship".into(), vec![g2.clone(), g3.clone()]);
        let mv = Outcome::Move {
            from: State::Review,
            to: State::Done,
            transitions: vec![t("launch"), t("ship")],
            allowed: true,
        };
        assert_eq!(candidate_gates(&mv, &cat), vec![g1.clone(), g2.clone(), g3.clone()], "each once");
        assert_eq!(
            candidate_gates(&Outcome::Check { transition: t("ship") }, &cat),
            vec![g2.clone(), g3.clone()]
        );
        assert!(
            candidate_gates(&Outcome::Check { transition: t("gone") }, &cat).is_empty(),
            "a transition the catalog no longer holds"
        );
        assert_eq!(
            candidate_gates(&Outcome::Reproduce { gate: g3.clone(), accepted: true }, &cat),
            vec![g3.clone()]
        );
        assert_eq!(
            candidate_gates(&verify_of(&g2, &[&g3]), &cat),
            vec![g2.clone(), g3.clone(), g1.clone()]
        );
        assert!(
            candidate_gates(&Outcome::Attempt { status: AttemptStatus::Completed }, &cat).is_empty()
        );
    }

    #[test]
    fn a_view_shows_each_entry_in_the_order_the_decision_names_it() {
        let (g1, g2, g3) = gates3();
        let mut cat = Catalogued::default();
        cat.names.insert(g1.clone(), "one".into());
        cat.names.insert(g2.clone(), "two".into());
        cat.transitions.insert("launch".into(), vec![g1.clone()]);
        cat.transitions.insert("ship".into(), vec![g2.clone()]);
        let on = |n: u64, g: &GateId| {
            let mut r = run(n, Verdict::from_predicate(true, 1), None);
            r.gate = g.clone();
            r
        };
        let runs = vec![on(11, &g1), on(12, &g2), on(13, &g3), on(14, &g1)];
        let mv = Outcome::Move {
            from: State::Review,
            to: State::Done,
            transitions: vec![t("launch"), t("ship")],
            allowed: true,
        };
        let d = decision(mv, vec![seq_iri(13), seq_iri(11), seq_iri(99), seq_iri(12)]);
        let v = view(d.clone(), "fake-user".into(), None, &runs, &[], &cat);
        let shown: Vec<(&str, &str)> =
            v.rows.iter().map(|r| (r.role.as_str(), r.gate.as_str())).collect();
        assert_eq!(shown, vec![("", g3.iri().as_str()), ("launch", "one"), ("ship", "two")]);
        assert_eq!(v.missing, vec![seq_iri(99)]);
        assert_eq!((v.decision, v.by, v.commit), (d, "fake-user".to_string(), None));

        let d = decision(verify_of(&g1, &[&g2]), vec![seq_iri(11), seq_iri(12), seq_iri(13)]);
        let roles: Vec<String> =
            view(d, String::new(), None, &runs, &[], &cat).rows.into_iter().map(|r| r.role).collect();
        assert_eq!(roles, vec!["reproduction", "regression", "neighbour"]);

        let d = decision(Outcome::Check { transition: t("launch") }, vec![seq_iri(11)]);
        assert_eq!(view(d, String::new(), None, &runs, &[], &cat).rows[0].role, "launch");

        let d = decision(reproduce(true), vec![seq_iri(13)]);
        assert_eq!(view(d, String::new(), None, &runs, &[], &cat).rows[0].role, "reproduction");

        let a = attempt(Some("x"));
        let d = decision(Outcome::Attempt { status: AttemptStatus::Completed }, vec![seq_iri(50)]);
        let v = view(d, String::new(), None, &[], std::slice::from_ref(&a), &cat);
        assert_eq!((v.attempt, v.missing.len()), (Some(a), 0));
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-github --lib -- ledger::read::tests:: ledger::render::tests::`
Expected: FAIL to compile — `Published`, `published_decisions`, `commit_of`, `runs_of`, `Catalogued`, `candidate_gates` and `view` do not exist.

- [ ] **Step 3: Implement**

In `crates/github/src/ledger/read.rs`, after `pub(crate) struct Snapshot { … }`, add:

```rust
/// One line of a directory, who wrote it, and where it is.
pub(crate) struct Located {
    pub line: Line,
    pub by: String,
    pub file: String,
    /// The line, from 1.
    pub n: u64,
}

/// A decision as the ledger holds it: who wrote its line, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub decision: Decision,
    pub by: String,
    pub file: String,
    /// The line, from 1.
    pub line: u64,
}
```

Rename `pub(crate) fn lines(` to `pub(crate) fn located(`, changing its return type to `Result<Vec<Located>, StoreError>` and its doc's first line to "Every line of `dir` in `snap`, with who wrote it and where, parsed strictly (spec §3.3), …". Inside it make exactly these changes:

- `let mut out: Vec<Line> = Vec::new();` → `let mut out: Vec<Located> = Vec::new();`
- `Ok((line, _by)) => line,` → `Ok((line, by)) => (line, by),` and bind the match as `let (line, by) = match …`
- in the check-5 arm, `if out[*i] != line {` → `if out[*i].line != line {`
- `out.push(line);` → `out.push(Located { line, by, file: seg.path.clone(), n });`

Then add, after it:

```rust
    /// Every line of `dir` in `snap`, each id once ([`Self::located`]
    /// without who wrote it or where).
    pub(crate) fn lines(
        &self,
        snap: &Snapshot,
        area: Area,
        dir: &str,
    ) -> Result<Vec<Line>, StoreError> {
        Ok(self
            .located(snap, area, dir)?
            .into_iter()
            .map(|l| l.line)
            .collect())
    }
```

After `decisions`, add:

```rust
    /// The decisions filed under `subject`, in ledger order, each with who
    /// wrote it and where — and the checked head they were read at, for
    /// [`Self::commit_of`] (spec §4.3).
    pub fn published_decisions(
        &self,
        subject: &Iri,
    ) -> Result<(String, Vec<Published>), StoreError> {
        let dir = layout::dir(Area::Decisions, subject);
        let snap = self.snapshot(std::slice::from_ref(&dir))?;
        let out = self
            .located(&snap, Area::Decisions, &dir)?
            .into_iter()
            .filter_map(|l| match l.line {
                Line::Decision(decision) => Some(Published {
                    decision,
                    by: l.by,
                    file: l.file,
                    line: l.n,
                }),
                _ => None,
            })
            .collect();
        Ok((snap.head, out))
    }

    /// The commit that added `p`'s line, as GitHub's blame names it at
    /// `head`. ⚠ `None` when the blame names no commit: its message is
    /// never taken for one.
    pub fn commit_of(&self, head: &str, p: &Published) -> Option<String> {
        let c = self.blame(head, &p.file, p.line);
        super::render::is_sha(&c).then_some(c)
    }

    /// Every run of `gates`, read at one checked head in one listing — the
    /// directories a recovered comment's decisions may rest on (spec
    /// §4.2). No gate, no request.
    pub fn runs_of(&self, gates: &[GateId]) -> Result<Vec<GateRun>, StoreError> {
        if gates.is_empty() {
            return Ok(Vec::new());
        }
        let dirs: Vec<String> = gates
            .iter()
            .map(|g| layout::dir(Area::Runs, g.iri()))
            .collect();
        let snap = self.snapshot(&dirs)?;
        let mut out = Vec::new();
        for dir in &dirs {
            for l in self.lines(&snap, Area::Runs, dir)? {
                if let Line::Run(r) = l {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }
```

In `crates/github/src/ledger/mod.rs`, replace `pub use read::Note;` with `pub use read::{Note, Published};`.

In `crates/github/src/ledger/render.rs`, add `use fl_core::ids::GateId;` and `use std::collections::BTreeMap;` to the imports, and after `struct Block`, add:

```rust
/// What the local catalog says of a project, for a comment (spec §4.2):
/// each gate's name, and each transition's gates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalogued {
    pub names: BTreeMap<GateId, String>,
    pub transitions: BTreeMap<String, Vec<GateId>>,
}

/// The gates whose runs a decision may rest on, found through the local
/// catalog (spec §4.2), each once: a move's or a check's transitions'
/// gates; a reproduction's gate; for a verify its reproduction, its
/// regressions and every other gate of the project — its passing
/// neighbours are named nowhere else. None for an attempt.
pub fn candidate_gates(outcome: &Outcome, cat: &Catalogued) -> Vec<GateId> {
    let mut out: Vec<GateId> = Vec::new();
    let mut add = |g: &GateId| {
        if !out.contains(g) {
            out.push(g.clone());
        }
    };
    let of = |name: &str| cat.transitions.get(name).into_iter().flatten();
    match outcome {
        Outcome::Move { transitions, .. } => {
            for t in transitions {
                of(&t.transition).for_each(&mut add);
            }
        }
        Outcome::Check { transition } => of(&transition.transition).for_each(&mut add),
        Outcome::Reproduce { gate, .. } => add(gate),
        Outcome::Verify {
            reproduction,
            regressions,
            ..
        } => {
            add(reproduction);
            regressions.iter().for_each(&mut add);
            cat.names.keys().for_each(&mut add);
        }
        Outcome::Attempt { .. } => {}
    }
    out
}

/// What a run was for, in a decision with `outcome`.
fn role(outcome: &Outcome, gate: &GateId, cat: &Catalogued) -> String {
    match outcome {
        Outcome::Move { transitions, .. } => transitions
            .iter()
            .find(|t| {
                cat.transitions
                    .get(&t.transition)
                    .is_some_and(|gs| gs.contains(gate))
            })
            .map(|t| t.transition.clone())
            .unwrap_or_default(),
        Outcome::Check { transition } => transition.transition.clone(),
        Outcome::Reproduce { .. } => "reproduction".into(),
        Outcome::Verify {
            reproduction,
            regressions,
            ..
        } => {
            let what = if gate == reproduction {
                "reproduction"
            } else if regressions.contains(gate) {
                "regression"
            } else {
                "neighbour"
            };
            what.into()
        }
        Outcome::Attempt { .. } => String::new(),
    }
}

/// The view of `decision` over the entries found (spec §4.2): each run it
/// rests on, in the order it names them, with what it was for and its
/// gate's name; its attempt; and each entry it names that was not found.
pub fn view(
    decision: Decision,
    by: String,
    commit: Option<String>,
    runs: &[GateRun],
    attempts: &[Attempt],
    cat: &Catalogued,
) -> DecisionView {
    let mut rows = Vec::new();
    let mut attempt = None;
    let mut missing = Vec::new();
    for id in &decision.rests_on {
        if let Some(run) = runs.iter().find(|r| r.id.as_ref() == Some(id)) {
            rows.push(RunRow {
                role: role(&decision.outcome, &run.gate, cat),
                gate: cat
                    .names
                    .get(&run.gate)
                    .cloned()
                    .unwrap_or_else(|| run.gate.iri().to_string()),
                run: run.clone(),
            });
        } else if let Some(a) = attempts.iter().find(|a| a.id.as_ref() == Some(id)) {
            attempt = Some(a.clone());
        } else {
            missing.push(id.clone());
        }
    }
    DecisionView {
        decision,
        by,
        commit,
        rows,
        attempt,
        missing,
    }
}
```

(`candidate_gates`'s `add` closure borrows `out` mutably; `out` is returned after its last use, so the borrow ends in time. If the borrow checker objects to `of` borrowing `cat` alongside, inline `cat.transitions.get(…).into_iter().flatten()` at each use.)

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github`
Expected: PASS — every existing read, append and verify test included.

- [ ] **Step 5: Mutation checks**

1. `located` keeps who wrote the line: push `by: String::new()` → `cargo test -p fl-github --lib ledger::read::tests::published_decisions_name_who_wrote_each_and_where` red.
2. …and where, the line: push `n: 1` → the same test red (line 2).
2a. …and where, the file: push `file: String::new()` → the same test red.
3. `commit_of` refuses a blame's message: drop `is_sha(&c).then_some(c)` for `Some(c)` → `ledger::read::tests::commit_of_names_the_commit_that_added_the_line_or_nothing` red.
4. `runs_of` asks nothing for no gate: delete the `if gates.is_empty()` return → `ledger::read::tests::runs_of_reads_every_gate_at_one_head_in_one_listing` red.
5. `runs_of` reads every gate at one head: replace its body with `gates.iter().map(|g| self.runs(g)).collect::<Result<Vec<_>, _>>().map(|v| v.concat())` → the same test red (two listings).
6. `candidate_gates` gives each gate once: drop `if !out.contains(g)` → `ledger::render::tests::a_decisions_gates_are_found_through_the_local_catalog` red.
7. `candidate_gates`'s verify arm reads the catalog: delete `cat.names.keys().for_each(&mut add);` → the same test red. Each other arm (move, check, reproduce, attempt): empty it in turn → the same test red.
8. `role`'s arms (move, check, reproduce, verify's three branches): change one at a time → `ledger::render::tests::a_view_shows_each_entry_in_the_order_the_decision_names_it` red.
9. `view`'s gate name falls back to the IRI: use `unwrap_or_default()` → the same test red.
10. `view` names what it did not find: drop the `else { missing.push(…) }` → the same test red.
10a. `view` finds the attempt among the attempts: delete the `else if let Some(a) = attempts…` branch → the same test red (its last assertion: no attempt, one missing).
11. `view` follows the decision's order, not the runs': iterate `runs` and keep those in `rests_on` instead → the same test red (`("", g3)` first).

The refactor of `lines` is not a guard: the existing read, append and verify tests pin that `lines` returns what it did.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/ledger/read.rs crates/github/src/ledger/render.rs crates/github/src/ledger/mod.rs
git commit -m "feat(github): read a decision back with who wrote it, where, and its runs

published_decisions keeps each decision's writer and line; commit_of names
the commit that added it through blame, or nothing; runs_of reads many
gates' runs at one head in one listing. A decision's view is built
through the local catalog's gates (spec 4.2), and a recovered decision
renders exactly as the live one did. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 6: Posting a comment, finding the issue where it is now, and the markers it carries

Spec §4.1 and §4.3 against GitHub, and §8.1's "paginated issue comments" in the fake: `post_comment` for the live path; `issue_at`, which follows a transferred issue to where it is now and reads fl's block there for the item's project (ruling 6); `posted`, every page of an issue's comments, each comment by a trusted author — fl's login, or a writer the caller names — read for its first marker line (ruling 9); `post_at`.

**Blast radius:** the fake's `POST …/issues/{n}/comments` now answers 410 for a deleted issue and 301 for a moved one; the tracker's `repair`, the only other poster, refuses both before it posts. The fake's `Issue` gains `comment_authors`; a comment it does not list was written by `USER_LOGIN`, so every existing test that pushes or reads `comments` is unchanged. New routes only otherwise.

**Files:**
- Create: `crates/github/src/ledger/comment.rs`
- Modify: `crates/github/src/ledger/mod.rs` (`mod comment;` after `mod append;`; `pub use comment::IssueAt;`)
- Modify: `crates/github/src/fake.rs` (`TRANSFERRED_REPO`, `Issue::comment_authors`, `State::transferred`, `FakeGithub::transfer`, `redirect`, `comment_items`, `author`, the comment routes)

**Interfaces:**
- Consumes: `GithubLedger::{owns, identity, path, read_refused}`; `Client::{send, get_all}`; `meta::{parse_issue_url, parse_body, IssueView}`; `render::marked` (Task 3).
- Produces, in `fl_github::ledger`:
  - `pub struct IssueAt { pub comments: String, pub project: ProjectId, pub moved_to: Option<Iri> }`
  - on `GithubLedger`: `pub fn by(&self) -> Result<String, StoreError>`, `pub fn post_comment(&self, item: &Iri, body: &str) -> Result<(), StoreError>`, `pub fn issue_at(&self, item: &Iri) -> Result<IssueAt, StoreError>`, `pub fn posted(&self, at: &IssueAt, writers: &BTreeSet<String>) -> Result<BTreeSet<Iri>, StoreError>` (`writers`: the `by` of every decision line under the item; fl's own login is always added), `pub fn post_at(&self, at: &IssueAt, body: &str) -> Result<(), StoreError>`.
  - in `fl_github::fake`: `pub const TRANSFERRED_REPO: u64 = 99;`, `Issue::comment_authors: Vec<String>`, `State::transferred: BTreeMap<u64, Issue>`, `FakeGithub::transfer(&self, n: u64) -> String` (the address the issue is at now).

- [ ] **Step 1: Write the failing tests**

Create `crates/github/src/ledger/comment.rs` with only its doc line (`//! Decision comments on GitHub (spec §4.1, §4.3).`) and this test module; add `mod comment;` after `mod append;` in `ledger/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use crate::fake::USER_LOGIN;
    use crate::ledger::{GithubLedger, render};
    use crate::tracker::{GithubTracker, Repo};
    use fl_core::MemStore;
    use fl_core::StoreError;
    use fl_core::ids::{ProjectId, seq_iri};
    use fl_core::iri::Iri;
    use fl_core::store::Tracker;
    use serde_json::json;
    use std::collections::BTreeSet;

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

    fn issue(n: u64) -> Iri {
        Iri::parse(&format!("https://github.com/acme/widgets/issues/{n}")).unwrap()
    }

    fn project() -> ProjectId {
        ProjectId(seq_iri(2))
    }

    /// A fake holding one fl record, #1, made by the tracker as fl makes
    /// one.
    fn with_record() -> FakeGithub {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        let (t, _) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        t.add_record(&project(), "work").unwrap();
        fake
    }

    #[test]
    fn a_live_comment_is_posted_on_the_items_issue() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        l.post_comment(&issue(1), "hello").unwrap();
        assert_eq!(fake.issue(1).comments, vec!["hello".to_string()]);
    }

    // ⚠ Ownership is local: an item another repository holds, or an IRI
    // that is no issue's URL, is refused before any request.
    #[test]
    fn an_item_this_repository_does_not_hold_is_refused_before_any_request() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        fake.state().requests.clear();
        let other = Iri::parse("https://github.com/acme/other/issues/1").unwrap();
        assert!(matches!(l.post_comment(&other, "x"), Err(StoreError::NotOwned { .. })));
        assert!(matches!(l.issue_at(&seq_iri(5)), Err(StoreError::NotOwned { .. })));
        assert!(fake.state().requests.is_empty());
    }

    #[test]
    fn a_comment_github_does_not_take_is_an_error() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        fake.state().fail_comment_next = true;
        let err = l.post_comment(&issue(1), "x").unwrap_err().to_string();
        assert!(err.contains("when fl posted a decision comment"), "{err}");
        assert!(fake.issue(1).comments.is_empty());
    }

    #[test]
    fn issue_at_reads_where_the_issue_is_and_what_its_block_says() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        assert_eq!(
            l.issue_at(&issue(1)).unwrap(),
            IssueAt {
                comments: "/repos/acme/widgets/issues/1/comments".into(),
                project: project(),
                moved_to: None,
            }
        );
    }

    // Spec §4.1: a transferred issue gets its comment where it is now.
    #[test]
    fn a_transferred_issue_is_followed_to_where_it_is_now() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        let before = l.issue_at(&issue(1)).unwrap();
        let to = fake.transfer(1);
        let at = l.issue_at(&issue(1)).unwrap();
        assert_eq!(at.comments, format!("{to}/comments"));
        assert_eq!(
            at.moved_to,
            Some(Iri::parse("https://github.com/elsewhere/transferred/issues/1").unwrap())
        );
        assert_eq!(at.project, project());
        l.post_at(&at, "where it is").unwrap();
        assert_eq!(fake.state().transferred[&1].comments, vec!["where it is".to_string()]);
        assert!(fake.issue(1).comments.is_empty(), "nothing at the old address");
        assert!(l.posted(&at, &none()).unwrap().is_empty(), "no marker in a plain comment");
        // The old address answers with where the issue went, never a list.
        assert!(l.posted(&before, &none()).is_err());
        assert!(l.post_comment(&issue(1), "late").is_err());
        // Gone from where it moved: said, naming the move.
        fake.state().transferred.remove(&1);
        let err = l.issue_at(&issue(1)).unwrap_err().to_string();
        assert!(err.contains("when fl read issue 1 where it moved"), "{err}");
    }

    #[test]
    fn issue_at_refuses_what_is_not_an_fl_issue_where_it_is() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        let pull = fake.plain_issue(&["fl:record"], true);
        let plain = fake.plain_issue(&[], false);
        let what = |n: u64| match l.issue_at(&issue(n)) {
            Err(StoreError::NotAnFlItem { what, .. }) => what,
            other => panic!("expected not an fl item, got {other:?}"),
        };
        assert_eq!(what(pull), "a pull request");
        assert!(what(plain).starts_with("an issue without fl's block"), "{}", what(plain));
        assert_eq!(what(99), "an issue that does not exist");
        // ⚠ A move is followed only under the API: the credential goes
        // along.
        fake.state().issues.get_mut(&plain).unwrap().moved_to =
            Some("https://elsewhere.example/repositories/1/issues/1".into());
        let err = l.issue_at(&issue(plain)).unwrap_err().to_string();
        assert!(err.contains("refused to send the GitHub credential"), "{err}");
        let here = l.issue_at(&issue(1)).unwrap();
        fake.state().issues.get_mut(&1).unwrap().gone = true;
        assert!(matches!(l.issue_at(&issue(1)), Err(StoreError::Deleted(_))));
        assert!(l.post_comment(&issue(1), "x").is_err(), "nothing posted on a deleted issue");
        assert!(l.posted(&here, &none()).is_err(), "a deleted issue lists no comments");
    }

    // A server error reading the issue says nothing lasting: transient.
    #[test]
    fn a_server_error_reading_the_issue_is_transient() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        fake.state()
            .body_next
            .push(("/repos/acme/widgets/issues/1".into(), 502, serde_json::Value::Null));
        let err = l.issue_at(&issue(1)).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    fn mark(n: u64) -> String {
        render::marker(&seq_iri(n)).expect("an id fl writes")
    }

    /// No decision writer named: only fl's own login is trusted.
    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    // ⚠ Spec §4.3: every page; each comment fl wrote counts by its first
    // marker line, even one a maintainer's edit pushed down; anyone
    // else's marks nothing. fl's login is read once.
    #[test]
    fn posted_reads_every_page_and_the_first_marker_of_fls_own_comments() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        {
            let mut s = fake.state();
            let i = s.issues.get_mut(&1).unwrap();
            i.comments = vec![
                format!("{}\n\nfirst", mark(10)),
                "thanks!".into(),
                format!("{}\n\nforged", mark(12)),
                format!("A maintainer's note.\n\n{}\n\nsecond", mark(11)),
            ];
            i.comment_authors = vec![
                USER_LOGIN.into(),
                "a-reviewer".into(),
                "a-reviewer".into(),
                USER_LOGIN.into(),
            ];
            s.max_per_page = 1;
        }
        let at = l.issue_at(&issue(1)).unwrap();
        fake.state().requests.clear();
        assert_eq!(l.posted(&at, &none()).unwrap(), BTreeSet::from([seq_iri(10), seq_iri(11)]));
        assert_eq!(l.posted(&at, &none()).unwrap().len(), 2);
        let requests = fake.state().requests.clone();
        let pages = requests.iter().filter(|r| r.contains("/issues/1/comments")).count();
        assert_eq!(pages, 8, "every page, twice: {requests:#?}");
        assert_eq!(
            requests.iter().filter(|r| *r == "GET /user").count(),
            1,
            "fl's login, read once: {requests:#?}"
        );
    }

    // What fl posts, fl wrote: its marker counts on the next read.
    #[test]
    fn a_comment_fl_posts_is_one_it_wrote() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        l.post_comment(&issue(1), &format!("{}\n\nbody", mark(10))).unwrap();
        let at = l.issue_at(&issue(1)).unwrap();
        assert_eq!(l.posted(&at, &none()).unwrap(), BTreeSet::from([seq_iri(10)]));
    }

    // ⚠ A comment by another account that wrote a decision under the item
    // counts — a colleague's machine posted it — so recovery does not post
    // it again; the same comment by an account that wrote none does not.
    #[test]
    fn a_marker_by_an_account_that_wrote_a_decision_under_the_item_counts() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        {
            let mut s = fake.state();
            let i = s.issues.get_mut(&1).unwrap();
            i.comments = vec![mark(10)];
            i.comment_authors = vec!["colleague".into()];
        }
        let at = l.issue_at(&issue(1)).unwrap();
        let writers = BTreeSet::from(["colleague".to_string()]);
        assert_eq!(l.posted(&at, &writers).unwrap(), BTreeSet::from([seq_iri(10)]));
        let others = BTreeSet::from(["someone-else".to_string()]);
        assert!(l.posted(&at, &others).unwrap().is_empty(), "wrote nothing here");
    }

    // ⚠ A comment someone else wrote cannot suppress recovery, whatever it
    // carries; a listed comment with no author is no one's.
    #[test]
    fn a_marker_in_a_comment_fl_did_not_write_does_not_count() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        {
            let mut s = fake.state();
            let i = s.issues.get_mut(&1).unwrap();
            i.comments = vec![mark(10)];
            i.comment_authors = vec!["someone-else".into()];
        }
        let at = l.issue_at(&issue(1)).unwrap();
        assert!(l.posted(&at, &none()).unwrap().is_empty());
        fake.state().issues.get_mut(&1).unwrap().comment_authors = vec![USER_LOGIN.into()];
        assert_eq!(l.posted(&at, &none()).unwrap(), BTreeSet::from([seq_iri(10)]), "fl's own counts");
        fake.state()
            .body_next
            .push(("/issues/1/comments".into(), 200, json!([{"id": 1, "body": mark(10)}])));
        assert!(l.posted(&at, &none()).unwrap().is_empty(), "no author, no one's");
    }

    #[test]
    fn a_page_that_cannot_be_read_fails_the_read() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        {
            let mut s = fake.state();
            s.issues.get_mut(&1).unwrap().comments = vec!["a".into(), "b".into(), "c".into()];
            s.max_per_page = 1;
            s.fail_page = Some(("/repos/acme/widgets/issues/1/comments".into(), 2));
        }
        let at = l.issue_at(&issue(1)).unwrap();
        assert!(l.posted(&at, &none()).is_err(), "a missing page is not an empty one");
    }

    #[test]
    fn a_listed_comment_without_a_body_is_an_error_and_an_empty_one_marks_nothing() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        let at = l.issue_at(&issue(1)).unwrap();
        fake.state().body_next.push(("/issues/1/comments".into(), 200, json!([{"id": 1}])));
        assert!(l.posted(&at, &none()).is_err());
        fake.state()
            .body_next
            .push(("/issues/1/comments".into(), 200, json!([{"id": 1, "body": null}])));
        assert!(l.posted(&at, &none()).unwrap().is_empty());
        fake.state().body_next.push((
            "/issues/1/comments".into(),
            200,
            json!([{"id": 1, "body": 5, "user": {"login": USER_LOGIN}}]),
        ));
        assert!(l.posted(&at, &none()).is_err(), "a body that is not text");
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-github --lib ledger::comment::tests::`
Expected: FAIL to compile — `IssueAt`, `post_comment`, `issue_at`, `posted`, `post_at`, `Issue::comment_authors` and `FakeGithub::transfer` do not exist.

- [ ] **Step 3: Implement**

In `crates/github/src/fake.rs`, after `pub const APP_SLUG: &str = "fake-app";`, add:

```rust
/// The id of the repository a transferred issue moves to.
pub const TRANSFERRED_REPO: u64 = 99;
```

In `pub struct Issue`, after `pub comments: Vec<String>,`, add:

```rust
    /// Who wrote each comment, by index. A comment this does not list was
    /// written by [`USER_LOGIN`].
    pub comment_authors: Vec<String>,
```

In `pub struct State`, after `pub transferred_nodes: BTreeSet<String>,`, add:

```rust
    /// Issues transferred out of the bound repository, by number, as GitHub
    /// serves them where they are now:
    /// `/repositories/{TRANSFERRED_REPO}/issues/{n}`.
    pub transferred: BTreeMap<u64, Issue>,
```

In `impl FakeGithub`, after `web_edit`, add:

```rust
    /// Transfers issue `n` to another repository, as GitHub does: the old
    /// address answers `301` to where the issue is now, and the issue —
    /// its body and its comments so far — is served there. Returns that
    /// address.
    pub fn transfer(&self, n: u64) -> String {
        let mut s = self.state();
        let to = format!("{}/repositories/{TRANSFERRED_REPO}/issues/{n}", s.base);
        let issue = s.issues.get_mut(&n).expect("an issue to transfer");
        let moved = issue.clone();
        issue.moved_to = Some(to.clone());
        s.transferred.insert(n, moved);
        to
    }
```

After `fn installation(…)`, add:

```rust
/// A `301` to `to`, as GitHub answers for an issue that moved.
fn redirect(to: &str) -> Answer {
    let mut a = answer(301, json!({"message": "Moved Permanently"}));
    a.headers.push(("Location".into(), to.to_string()));
    a
}

/// An issue's comments as GitHub lists them, oldest first, each with its
/// author.
fn comment_items(i: &Issue) -> Vec<Value> {
    i.comments
        .iter()
        .enumerate()
        .map(|(k, body)| {
            let by = i.comment_authors.get(k).map_or(USER_LOGIN, String::as_str);
            json!({"id": k as u64 + 1, "body": body, "user": {"login": by}})
        })
        .collect()
}

/// Who a request's credential writes as: the App's bot for its
/// installation token, else the token's user.
fn author(auth: &str) -> String {
    if auth == format!("Bearer {INSTALLATION_TOKEN}") {
        format!("{APP_SLUG}[bot]")
    } else {
        USER_LOGIN.to_string()
    }
}
```

In `route`, replace the `("POST", ["repos", o, r, "issues", n, "comments"])` arm with:

```rust
        ("POST", ["repos", o, r, "issues", n, "comments"]) if s.is_bound(o, r) => {
            if std::mem::take(&mut s.fail_comment_next) {
                return answer(500, json!({"message": "fake comment failure"}));
            }
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let by = author(auth);
            match n.parse::<u64>().ok().and_then(|n| s.issues.get_mut(&n)) {
                None => answer(404, json!({"message": "Not Found"})),
                // Unmeasured: GitHub's answers for a deleted or transferred
                // issue's comments, as its documentation describes them; no
                // live test deletes or transfers an issue.
                Some(i) if i.gone => answer(410, json!({"message": "This issue was deleted"})),
                Some(i) if i.moved_to.is_some() => redirect(i.moved_to.as_deref().unwrap_or("")),
                Some(i) => {
                    let k = i.comments.len();
                    i.comment_authors.resize(k, USER_LOGIN.to_string());
                    i.comment_authors.push(by);
                    i.comments
                        .push(v["body"].as_str().unwrap_or("").to_string());
                    answer(201, json!({"id": i.comments.len()}))
                }
            }
        }
        // ⚠ Modelled: an issue's comments are listed with each one's `body`
        // and its author's `user.login`. Confirmed by live test
        // `a_decision_comment_round_trips_with_its_marker`, which reads one
        // page; paging is the `Link` header every list GitHub answers uses.
        ("GET", ["repos", o, r, "issues", n, "comments"]) if s.is_bound(o, r) => {
            let items = match n.parse::<u64>().ok().and_then(|n| s.issues.get(&n)) {
                None => return answer(404, json!({"message": "Not Found"})),
                Some(i) if i.gone => {
                    return answer(410, json!({"message": "This issue was deleted"}));
                }
                Some(i) if i.moved_to.is_some() => {
                    return redirect(i.moved_to.as_deref().unwrap_or(""));
                }
                Some(i) => comment_items(i),
            };
            s.page(&path, &q, items)
        }
        // Unmeasured: a transferred issue, its comments and a post there,
        // as GitHub's documentation describes them; no live test transfers
        // an issue.
        ("GET", ["repositories", id, "issues", n])
            if id.parse::<u64>().ok() == Some(TRANSFERRED_REPO) =>
        {
            match n.parse::<u64>().ok().and_then(|n| s.transferred.get(&n)) {
                None => answer(404, json!({"message": "Not Found"})),
                Some(i) => {
                    let mut v = s.issue_json(i);
                    v["html_url"] = json!(format!(
                        "https://github.com/elsewhere/transferred/issues/{}",
                        i.number
                    ));
                    answer(200, v)
                }
            }
        }
        ("GET", ["repositories", id, "issues", n, "comments"])
            if id.parse::<u64>().ok() == Some(TRANSFERRED_REPO) =>
        {
            let items = match n.parse::<u64>().ok().and_then(|n| s.transferred.get(&n)) {
                None => return answer(404, json!({"message": "Not Found"})),
                Some(i) => comment_items(i),
            };
            s.page(&path, &q, items)
        }
        ("POST", ["repositories", id, "issues", n, "comments"])
            if id.parse::<u64>().ok() == Some(TRANSFERRED_REPO) =>
        {
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let by = author(auth);
            match n.parse::<u64>().ok().and_then(|n| s.transferred.get_mut(&n)) {
                None => answer(404, json!({"message": "Not Found"})),
                Some(i) => {
                    let k = i.comments.len();
                    i.comment_authors.resize(k, USER_LOGIN.to_string());
                    i.comment_authors.push(by);
                    i.comments
                        .push(v["body"].as_str().unwrap_or("").to_string());
                    answer(201, json!({"id": i.comments.len()}))
                }
            }
        }
```

Above `mod tests` in `crates/github/src/ledger/comment.rs`, add:

```rust
use super::GithubLedger;
use super::render;
use crate::client::Method;
use crate::meta::{self, IssueView};
use fl_core::StoreError;
use fl_core::ids::{ProjectId, RecordId};
use fl_core::iri::Iri;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Where an item's issue is now, and the project fl's block there names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueAt {
    /// Its comments: a path under the API, or under the address GitHub
    /// gave for an issue that moved.
    pub comments: String,
    pub project: ProjectId,
    /// Where the issue is now, when it was transferred.
    pub moved_to: Option<Iri>,
}

fn backend(msg: String) -> StoreError {
    StoreError::Backend(msg)
}

impl GithubLedger<'_> {
    /// Who the ledger's lines say wrote them: the credential's identity,
    /// read once per command.
    pub fn by(&self) -> Result<String, StoreError> {
        self.identity()
    }

    /// The number of `item`'s issue in this repository. ⚠ Local: an item
    /// another repository holds, or an IRI that is no issue's URL, is
    /// refused before any request.
    fn number_of(&self, item: &Iri) -> Result<u64, StoreError> {
        let elsewhere = || StoreError::NotOwned {
            id: item.clone(),
            searched: vec![format!("the issues of {}", self.repo.full_name)],
        };
        if !self.owns(&RecordId(item.clone()))? {
            return Err(elsewhere());
        }
        meta::parse_issue_url(item)
            .map(|(_, n)| n)
            .ok_or_else(elsewhere)
    }

    /// Post `body` on `item`'s issue in this repository: the comment a
    /// decision posts once its state change is done (spec §4.1).
    pub fn post_comment(&self, item: &Iri, body: &str) -> Result<(), StoreError> {
        let n = self.number_of(item)?;
        self.post(&self.path(&format!("/issues/{n}/comments")), body)
    }

    /// Where `item`'s issue is now (spec §4.1: "A transferred issue gets it
    /// at its current location"), and the project fl's block there names —
    /// which also proves the issue is an fl item.
    ///
    /// ⚠ A moved issue is followed once, to the address GitHub gives,
    /// which the client refuses unless it is under the API: the credential
    /// goes along. An answer that names no address leaves an empty one,
    /// refused the same way.
    pub fn issue_at(&self, item: &Iri) -> Result<IssueAt, StoreError> {
        let n = self.number_of(item)?;
        let here = self.path(&format!("/issues/{n}"));
        let r = self.client.send(Method::Get, &here, None)?;
        let (r, base, moved) = match r.status {
            200 => (r, here, false),
            301 | 302 | 307 | 308 => {
                let to = r.location.clone().unwrap_or_default();
                let again = self.client.send(Method::Get, &to, None)?;
                if again.status != 200 {
                    return Err(backend(format!(
                        "GitHub answered {} when fl read issue {n} where it moved, {to}",
                        again.status
                    )));
                }
                (again, to, true)
            }
            404 => {
                return Err(StoreError::NotAnFlItem {
                    id: item.clone(),
                    what: "an issue that does not exist".into(),
                });
            }
            410 => return Err(StoreError::Deleted(item.clone())),
            s => return Err(self.read_refused(s, &format!("read issue {n}"))),
        };
        let issue = IssueView::from_json(&r.body)?;
        if issue.is_pull_request {
            return Err(StoreError::NotAnFlItem {
                id: item.clone(),
                what: "a pull request".into(),
            });
        }
        let (_, block) = meta::parse_body(&issue.body).map_err(|e| StoreError::NotAnFlItem {
            id: item.clone(),
            what: format!("an issue without fl's block ({e})"),
        })?;
        Ok(IssueAt {
            comments: format!("{base}/comments"),
            project: block.project,
            moved_to: moved.then_some(issue.url),
        })
    }

    /// Every decision the comments at `at` mark, over every page (spec
    /// §4.3). ⚠ A page that fails fails the read — never "none posted",
    /// which would post every decision again.
    ///
    /// ⚠ Only a trusted author's comment marks a decision: the login fl
    /// posts as (read once per command), or one of `writers` — the `by` of
    /// every decision line filed under the item, each of whom held
    /// Contents: write. Anyone else who can comment could otherwise copy a
    /// decision id from the ledger and stop its comment being recovered.
    /// An edit keeps the author, so a comment someone edits still counts;
    /// its first marker line counts wherever it is.
    pub fn posted(
        &self,
        at: &IssueAt,
        writers: &BTreeSet<String>,
    ) -> Result<BTreeSet<Iri>, StoreError> {
        let mut trusted = writers.clone();
        trusted.insert(self.by()?);
        let mut out = BTreeSet::new();
        for c in self
            .client
            .get_all(&format!("{}?per_page=100", at.comments))?
        {
            let text = match c.get("body") {
                None => {
                    return Err(backend("GitHub listed a comment without a body".into()));
                }
                // An empty comment marks nothing.
                Some(Value::Null) => continue,
                Some(body) => body.as_str().ok_or_else(|| {
                    backend("GitHub listed a comment whose body is not text".into())
                })?,
            };
            // A comment with no author is no one's, and never trusted.
            let author = c.pointer("/user/login").and_then(Value::as_str);
            if !author.is_some_and(|a| trusted.contains(a)) {
                continue;
            }
            if let Some(id) = render::marked(text) {
                out.insert(id);
            }
        }
        Ok(out)
    }

    /// Post `body` where the issue is now.
    pub fn post_at(&self, at: &IssueAt, body: &str) -> Result<(), StoreError> {
        self.post(&at.comments, body)
    }

    fn post(&self, comments: &str, body: &str) -> Result<(), StoreError> {
        let r = self
            .client
            .send(Method::Post, comments, Some(&json!({ "body": body })))?;
        match r.status {
            201 => Ok(()),
            s => Err(backend(format!(
                "GitHub answered {s} when fl posted a decision comment"
            ))),
        }
    }
}
```

In `crates/github/src/ledger/mod.rs`, after `pub use append::TRIES;`, add `pub use comment::IssueAt;`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Each run with `cargo test -p fl-github --lib ledger::comment::tests::`:

1. Ownership before any request: delete `number_of`'s `if !self.owns(…)` block → `an_item_this_repository_does_not_hold_is_refused_before_any_request` red (a request for `acme/other`'s number).
2. Not a guard: `number_of`'s `.ok_or_else(elsewhere)` after `parse_issue_url`. `owns` already answers `false` for an IRI that is no issue's URL, so no input reaches it; it stays so the function never panics.
3. `post` takes only `201`: make it `_ => Ok(())` → `a_comment_github_does_not_take_is_an_error` and `a_transferred_issue_is_followed_to_where_it_is_now` (its last assertion) red.
4. `issue_at` follows a move: delete the `301 | 302 | 307 | 308` arm → `a_transferred_issue_is_followed_to_where_it_is_now` red.
5. …and records where to: set `moved_to: None` always → the same test red; set it `Some(issue.url)` always → `issue_at_reads_where_the_issue_is_and_what_its_block_says` red.
6. The second answer must be `200`: drop the `if again.status != 200` check → `a_transferred_issue_is_followed_to_where_it_is_now` red (its last assertion: the 404 is read as an issue and fails as "without `html_url`").
7. `404` and `410` arms: delete each → `issue_at_refuses_what_is_not_an_fl_issue_where_it_is` red.
8. A pull request: delete its check → the same test red.
9. The block: replace `parse_body(…)?` with a default `Meta` → the same test red.
10. `posted` reads every page: replace `get_all` with one `send` reading `body.as_array()` → `posted_reads_every_page_and_the_first_marker_of_fls_own_comments` red (the fourth comment is on page 4); `a_page_that_cannot_be_read_fails_the_read` red.
11. A comment without a body: make the `None` arm `continue` → `a_listed_comment_without_a_body_is_an_error_and_an_empty_one_marks_nothing` red; the `Null` arm an error → the same test red.
12. A body that is not text: replace `.ok_or_else(…)?` with `.unwrap_or("")` → the same test red (its last assertion).
13. Only a trusted author's comment marks (the author guard): delete the `if !author.is_some_and(…) { continue; }` block → `a_marker_in_a_comment_fl_did_not_write_does_not_count` and `posted_reads_every_page_and_the_first_marker_of_fls_own_comments` red.
14. A comment with no author is never trusted: replace `!author.is_some_and(|a| trusted.contains(a))` with `author.is_some_and(|a| !trusted.contains(a))` → `a_marker_in_a_comment_fl_did_not_write_does_not_count` red (its last assertion).
14a. The writers' half of the set: replace `writers.clone()` with `BTreeSet::new()` → `a_marker_by_an_account_that_wrote_a_decision_under_the_item_counts` red.
14b. fl's own half of the set: delete `trusted.insert(self.by()?);` (keep a `self.by()?` call so the read stays) → `a_comment_fl_posts_is_one_it_wrote` red.
15. fl's login is read once (ordering: before the loop, through the cache): replace `trusted.insert(self.by()?)` with a `self.client.identity()?` read inside the loop → `posted_reads_every_page_and_the_first_marker_of_fls_own_comments` red (`GET /user` more than once).
16. `issue_at`'s other statuses: replace the `s => … read_refused(…)` arm with `s => return Err(backend(format!("{s}")))` → `a_server_error_reading_the_issue_is_transient` red.
17. The fake: `POST` on a moved issue answers 301 — delete that arm → `a_transferred_issue_is_followed_to_where_it_is_now` red (the late post lands); on a deleted issue 410 — delete it → `issue_at_refuses…` red (its `post_comment` assertion).
18. The fake: `GET` comments of a moved issue answers 301 — delete that arm → `a_transferred_issue_is_followed_to_where_it_is_now` red (`posted(&before)` lists); of a deleted issue 410 — delete it → `issue_at_refuses…` red (its last assertion).
19. The fake lists each comment's author: drop `"user": {"login": by}` from `comment_items` → `a_marker_in_a_comment_fl_did_not_write_does_not_count` red (fl's own no longer counts).
20. The fake records who posted: push `"someone".into()` instead of `by` in the `POST` route → `a_comment_fl_posts_is_one_it_wrote` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/ledger/comment.rs crates/github/src/ledger/mod.rs crates/github/src/fake.rs
git commit -m "feat(github): post a decision comment, find its issue where it is, read its markers

post_comment posts on this repository's issue; issue_at follows a
transferred issue to where it is and reads fl's block there; posted reads
every page of the issue's comments, counting a marker only in a comment
fl wrote, wherever its first marker line is, and a failed
page fails the read (spec 4.1, 4.3). The fake lists issue comments, paged,
and serves transferred issues. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 7: A move's and a check's comment, after the state change

Spec §4.1–§4.2 at the command line, for `record move` and `check --record`. Ruling 2: `Witness` wraps the bound ledger and remembers each decision whose flush landed. After the `fl-exec` call returns — `Ok` or not — the command posts that decision's comment, saying whether the state change completed (ruling 4), and only then turns an error into its exit. A comment that fails is a warning (ruling 3).

**Blast radius:** in mode B every decision command now runs through `Witness`, which forwards every `Ledger` call unchanged (its unit test pins the flush; the reads are one-line forwards). Mode A builds no witness and posts nothing. `Ctx` gains a field, so every `Ctx { … }` literal changes: `main.rs` (two), and the tests in `ctx.rs` and `cmd/ledger.rs`. `World`'s `check.sh` now prints `checked`, so every run in `crates/cli/tests/ledger.rs` has an excerpt; no existing assertion reads one.

**Files:**
- Modify: `crates/cli/src/ctx.rs` (`Flush`, `Witness`, `Ctx::witness`; tests)
- Create: `crates/cli/src/comment.rs`
- Modify: `crates/cli/src/main.rs` (`mod comment;`; the witness; both `Ctx` literals)
- Modify: `crates/cli/src/cmd/record.rs` (the `Move` arm), `crates/cli/src/cmd/check.rs` (`run`)
- Modify: `crates/cli/src/cmd/ledger.rs` (its test's `Ctx` literal)
- Modify: `crates/cli/src/testing.rs` (`Flushes::publishing_nothing`)
- Modify: `crates/cli/tests/ledger.rs`

**Interfaces:**
- Consumes: `render::{Catalogued, candidate_gates, view, render, state_line}` (Tasks 3–5); `GithubLedger::{visibility, by, post_comment, repo}` (Task 6); `disclose::{run, attempt}`; `layout::decision_subject`; `fl_github::meta::parse_issue_url`.
- Produces:
  - in `ctx.rs`: `pub struct Flush { pub decision: Decision, pub flushed: Flushed }`; `pub struct Witness<'a>` with `pub fn new(inner: &'a dyn Ledger) -> Self` and `pub fn take(&self) -> Vec<Flush>`, implementing `Ledger`; `Ctx::witness: Option<&'a Witness<'a>>`.
  - in `comment.rs`: `pub fn catalogued(store: &RedbStore, project: &ProjectId) -> Result<Catalogued, StoreError>`, `pub fn recovery(item: &Iri) -> String`, `pub fn post_after(ctx: &Ctx<'_>, project: &ProjectId, completed: bool) -> Vec<String>`, `pub fn after(ctx: &Ctx<'_>, project: &ProjectId, completed: bool)`.
  - in `testing.rs`: `Flushes::publishing_nothing() -> Flushes`.
  - in `crates/cli/tests/ledger.rs`: `World::gated_with(&self, gate: &str, program: &str)`, `World::gated_as(&self, gate: &str)`; `gated()` keeps its meaning.
  - The warning's unique phrase: `warning: the decision's comment was not posted on issue `; what stands follows it (ruling 26).

- [ ] **Step 1: Write the failing tests**

In `crates/cli/src/ctx.rs`, inside `mod tests`, add:

```rust
    fn check_decision() -> fl_core::decision::Decision {
        fl_core::decision::Decision {
            id: fl_core::ids::seq_iri(9),
            at: fl_core::at::At::from_unix_millis(1),
            record: RecordId(Iri::parse("https://github.com/acme/widgets/issues/1").unwrap()),
            finding: None,
            outcome: fl_core::decision::Outcome::Check {
                transition: fl_core::decision::TransitionOutcome {
                    transition: "launch".into(),
                    passed: true,
                },
            },
            rests_on: vec![],
        }
    }

    // Spec §4.1: only a decision whose flush landed has a comment to post;
    // the flush itself goes through unchanged.
    #[test]
    fn the_witness_remembers_only_a_flush_that_landed_and_forwards_it() {
        let landed = Flushes::default();
        let w = Witness::new(&landed);
        assert_eq!(w.flush(check_decision()).unwrap().commit.as_deref(), Some("c1"));
        assert_eq!(landed.decisions.borrow().len(), 1, "forwarded");
        let seen = w.take();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].decision, check_decision());
        assert!(w.take().is_empty(), "taken once");
        let refused = Flushes::refusing();
        let w = Witness::new(&refused);
        assert!(w.flush(check_decision()).is_err());
        assert!(w.take().is_empty());
    }
```

Create `crates/cli/src/comment.rs` holding only its module doc (`//! Decision comments (GitHub ledger spec §4).`) and this test module, and add `mod comment;` after `mod cmd;` in `main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::Witness;
    use crate::testing::Flushes;
    use fl_core::at::At;
    use fl_core::decision::{Decision, TransitionOutcome};
    use fl_core::ids::{RecordId, seq_iri};
    use fl_github::fake::FakeGithub;
    use fl_github::{Client, EnvToken, Repo};

    fn issue(n: u64) -> Iri {
        Iri::parse(&format!("https://github.com/acme/widgets/issues/{n}")).unwrap()
    }

    // A state change is said to stand only when there was one and it
    // completed.
    #[test]
    fn a_failed_comment_says_what_stands() {
        let moved = |allowed| Outcome::Move {
            from: fl_core::model::State::Todo,
            to: fl_core::model::State::Doing,
            transitions: vec![],
            allowed,
        };
        let state = "The decision and its state change stand";
        let only = "The decision stands";
        assert_eq!(what_stands(&moved(true), true), state);
        assert_eq!(what_stands(&moved(true), false), only);
        assert_eq!(what_stands(&moved(false), true), only);
        let gate = fl_core::ids::GateId(seq_iri(1));
        assert_eq!(what_stands(&Outcome::Reproduce { gate: gate.clone(), accepted: true }, true), state);
        assert_eq!(what_stands(&Outcome::Reproduce { gate: gate.clone(), accepted: false }, true), only);
        let verify = |closed| Outcome::Verify {
            reproduction: gate.clone(),
            reproduction_passed: closed,
            regressions: vec![],
            closed,
        };
        assert_eq!(what_stands(&verify(true), true), state);
        assert_eq!(what_stands(&verify(false), true), only);
        let check = Outcome::Check {
            transition: TransitionOutcome {
                transition: "launch".into(),
                passed: true,
            },
        };
        assert_eq!(what_stands(&check, true), only);
        let attempt = Outcome::Attempt {
            status: fl_core::log::AttemptStatus::Completed,
        };
        assert_eq!(what_stands(&attempt, true), only);
    }

    #[test]
    fn the_recovery_command_names_the_issue_by_its_number() {
        assert_eq!(recovery(&issue(3)), "fl github ledger comment 3");
        assert_eq!(recovery(&seq_iri(5)), format!("fl github ledger comment {}", seq_iri(5)));
    }

    // A comment points at the commit that holds its decision: a flush that
    // published nothing gets no comment. One that did gets one — so this
    // test sees a comment when there should be one.
    #[test]
    fn a_flush_that_published_nothing_gets_no_comment() {
        let fake = FakeGithub::start("acme/widgets");
        fake.plain_issue(&[], false);
        let client = Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        );
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("s.redb")).unwrap();
        let project = store.add_project("/r").unwrap();
        let repo = Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        };
        let gl = GithubLedger::new(&client, repo, &store);
        for (flushes, comments) in [(Flushes::publishing_nothing(), 0), (Flushes::default(), 1)] {
            let witness = Witness::new(&flushes);
            let ctx = Ctx {
                store: &store,
                tracker: &store,
                ledger: &witness,
                handles: &store,
                github: None,
                github_ledger: Some(&gl),
                witness: Some(&witness),
                tracker_label: String::new(),
            };
            witness
                .flush(Decision {
                    id: seq_iri(9),
                    at: At::from_unix_millis(1),
                    record: RecordId(issue(1)),
                    finding: None,
                    outcome: Outcome::Check {
                        transition: TransitionOutcome {
                            transition: "launch".into(),
                            passed: true,
                        },
                    },
                    rests_on: vec![],
                })
                .unwrap();
            let warnings = post_after(&ctx, &project, true);
            assert!(warnings.is_empty(), "{warnings:?}");
            assert_eq!(fake.issue(1).comments.len(), comments);
        }
    }

    // ⚠ A decision whose id fl does not write gets no comment — a marker
    // carrying it could close the HTML comment early — and a warning.
    #[test]
    fn a_decision_whose_id_fl_does_not_write_gets_a_warning_and_no_comment() {
        let fake = FakeGithub::start("acme/widgets");
        fake.plain_issue(&[], false);
        let client = Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        );
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("s.redb")).unwrap();
        let project = store.add_project("/r").unwrap();
        let repo = Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        };
        let gl = GithubLedger::new(&client, repo, &store);
        let flushes = Flushes::default();
        let witness = Witness::new(&flushes);
        let ctx = Ctx {
            store: &store,
            tracker: &store,
            ledger: &witness,
            handles: &store,
            github: None,
            github_ledger: Some(&gl),
            witness: Some(&witness),
            tracker_label: String::new(),
        };
        witness
            .flush(Decision {
                id: Iri::parse("urn:x:a--><b>").unwrap(),
                at: At::from_unix_millis(1),
                record: RecordId(issue(1)),
                finding: None,
                outcome: Outcome::Check {
                    transition: TransitionOutcome {
                        transition: "launch".into(),
                        passed: true,
                    },
                },
                rests_on: vec![],
            })
            .unwrap();
        let warnings = post_after(&ctx, &project, true);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("has an id fl does not write"), "{warnings:?}");
        assert!(warnings[0].contains("fl github ledger verify"), "{warnings:?}");
        assert!(!warnings[0].contains("fl github ledger comment"), "recovery skips it too");
        assert!(fake.issue(1).comments.is_empty());
    }
}
```

In `crates/cli/tests/ledger.rs`:

(a) In `World::bound`, replace `fs::write(&check, "#!/bin/sh\n[ ! -e bug ]\n").unwrap();` with:

```rust
        // `checked` is every run's excerpt.
        fs::write(&check, "#!/bin/sh\necho checked\n[ ! -e bug ]\n").unwrap();
```

(b) After `const PREFLIGHT`, add:

```rust
/// What a comment that could not be posted warns. No other path writes it.
const NOT_POSTED: &str = "warning: the decision's comment was not posted on issue ";
```

(c) In `impl World`, after `only_file_in`, add:

```rust
    /// Every decision id the ledger holds, in path order.
    fn decision_ids(&self) -> Vec<String> {
        self.ledger_files_in("decisions")
            .iter()
            .flat_map(|(_, text)| {
                text.lines()
                    .map(|l| {
                        serde_json::from_str::<serde_json::Value>(l).unwrap()["id"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
```

(d) In `without_the_ledger_key_a_move_publishes_nothing`, before its last line, add:

```rust
    assert!(w.fake.issue(1).comments.is_empty(), "no comment in mode A");
```

(d2) Rename `fn gated(&self)` to `fn gated_with(&self, gate: &str, program: &str)`, change its doc to "A project with one gate named `gate` over `src/**/*.rs`, running `program`, …", replace `"no-bug",` and `"./check.sh",` in its `gate add` arguments with `gate,` and `program,`, and add after it:

```rust
    /// `gated_with(gate, "./check.sh")`.
    fn gated_as(&self, gate: &str) {
        self.gated_with(gate, "./check.sh");
    }

    /// `gated_as("no-bug")`.
    fn gated(&self) {
        self.gated_as("no-bug");
    }
```

(e) After `a_check_with_a_record_publishes_and_a_plain_check_does_not`, add:

```rust
// Spec §4.1: the ledger commit, then the state change, then the comment —
// one, on the record's issue.
#[test]
fn a_move_posts_its_comment_on_the_record_after_the_state_change() {
    let w = World::new();
    w.ready();
    w.fake.state().requests.clear();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success()
        .stderr(contains(NOT_POSTED).not());
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let ids = w.decision_ids();
    assert_eq!(ids.len(), 1, "{ids:?}");
    let c = &comments[0];
    assert!(
        c.starts_with(&format!("<!-- fl:decision {{\"id\":\"{}\"}} -->\n", ids[0])),
        "{c}"
    );
    assert!(c.contains("### fl move: allowed"), "{c}");
    assert!(c.contains("From `todo` to `doing`."), "{c}");
    assert!(
        c.contains("The state change completed: the record is now `doing`."),
        "{c}"
    );
    let head = w.fake.ledger_head().expect("the ledger");
    assert!(
        c.contains(&format!("(https://github.com/acme/widgets/commit/{head})")),
        "{c}"
    );
    assert!(c.contains("| launch | no-bug | PASS | 1 |"), "{c}");
    assert!(
        c.contains("<details><summary>launch / no-bug: PASS</summary>"),
        "a private repository shows the excerpt: {c}"
    );
    let requests = w.fake.state().requests.clone();
    let moved = requests
        .iter()
        .position(|r| r == "PATCH /repos/acme/widgets/issues/1")
        .expect("the state change");
    let commented = requests
        .iter()
        .position(|r| r == "POST /repos/acme/widgets/issues/1/comments")
        .expect("the comment");
    assert!(moved < commented, "{requests:#?}");
    assert!(
        requests[commented..]
            .iter()
            .all(|r| !r.contains("/git/") && r != "POST /graphql" && !r.starts_with("PATCH")),
        "nothing is published or changed after the comment: {requests:#?}"
    );
}

// Decision 11: a refused move is commented, saying it was refused.
#[test]
fn a_refused_move_is_commented_as_refused() {
    let w = World::new();
    w.ready();
    fs::write(w.repo.path().join("bug"), "").unwrap();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .code(1);
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(comments[0].contains("### fl move: refused"), "{}", comments[0]);
    assert!(
        comments[0].contains("The move was refused: the record stays `todo`."),
        "{}",
        comments[0]
    );
}

// Decision 2: a comment on a repository that is not private shows no
// excerpt.
#[test]
fn a_moves_comment_on_a_repository_that_is_not_private_shows_no_excerpt() {
    let w = World::new();
    w.ready();
    w.fake.state().repos[0].visibility = "public".into();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(!comments[0].contains("<details>"), "{}", comments[0]);
    assert!(!comments[0].contains("checked"), "{}", comments[0]);
}

// ⚠ Decision 2 at the command line: a gate that cannot start names its
// program's path in its detail. On a repository that is not private the
// comment shows neither that detail nor the text that stands in for it;
// on a private one it shows the detail, so the test sees what it checks.
#[test]
fn a_comment_on_a_repository_that_is_not_private_shows_no_error_detail() {
    for private in [true, false] {
        let w = World::new();
        let program = w.repo.path().join("no-such-gate");
        w.gated_with("no-bug", program.to_str().unwrap());
        w.init();
        w.export();
        if !private {
            w.fake.state().repos[0].visibility = "public".into();
        }
        w.fl()
            .args(["record", "move", "1", "--to", "doing"])
            .assert()
            .code(2);
        let comments = w.fake.issue(1).comments;
        assert_eq!(comments.len(), 1, "{comments:?}");
        let c = &comments[0];
        assert!(c.contains("| ERROR |"), "{c}");
        assert_eq!(c.contains("no-such-gate"), private, "the detail, private only: {c}");
        assert_eq!(c.contains("<details>"), private, "{c}");
        assert!(!c.contains("withheld"), "{c}");
    }
}

#[test]
fn a_check_with_a_record_posts_its_comment_and_a_plain_check_posts_none() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1"])
        .assert()
        .success();
    assert!(w.fake.issue(1).comments.is_empty(), "a plain check decides nothing");
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(comments[0].contains("### fl check: passed"), "{}", comments[0]);
    assert!(comments[0].contains("A check changes no state."), "{}", comments[0]);
}

// ⚠ A comment that cannot be posted leaves the decision and its state
// change standing: a warning naming the recovery command, and the
// command's own exit code.
#[test]
fn a_move_whose_comment_fails_keeps_its_exit_code_and_names_the_recovery() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success()
        .stderr(
            contains(format!("{NOT_POSTED}1: "))
                .and(contains("run `fl github ledger comment 1` to post it")),
        );
    assert!(w.fake.issue(1).comments.is_empty());
    assert!(
        w.fake
            .issue(1)
            .labels
            .contains(&"fl:record/doing".to_string()),
        "the move stands"
    );
}

// ⚠ CI reads `check`'s exit code: a comment that cannot be posted never
// turns a pass or a fail into a refusal.
#[test]
fn a_check_whose_comment_fails_keeps_the_checks_own_exit_code() {
    let w = World::new();
    w.ready();
    for (bug, code) in [(false, 0), (true, 1)] {
        if bug {
            fs::write(w.repo.path().join("bug"), "").unwrap();
        }
        w.fake.state().fail_comment_next = true;
        w.fl()
            .args(["check", "launch", "--project", "1", "--record", "1"])
            .assert()
            .code(code)
            .stderr(contains(NOT_POSTED));
    }
}

// ⚠ Spec §4.2: the comment says whether the state change completed. One
// that fails after the flush is commented too; the command still exits 2
// with the tracker's error.
#[test]
fn a_move_whose_state_change_fails_after_its_flush_says_so_in_its_comment() {
    let w = World::new();
    w.ready();
    w.fake.state().foreign_label_on_next_patch = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .code(2);
    assert_eq!(w.decision_ids().len(), 1, "the decision was published");
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].contains("The move was allowed, but its state change did not complete"),
        "{}",
        comments[0]
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli`
Expected: FAIL to compile — `Witness`, `Flushes::publishing_nothing`, `post_after`, `recovery` and `Ctx::witness` do not exist.

- [ ] **Step 3: Implement**

In `crates/cli/src/testing.rs`, add a field `no_commit: bool` to `Flushes` (after `fail_append`), extend its doc with "built with [`Flushes::publishing_nothing`], lands every flush and names no commit", add

```rust
    /// Every flush lands and names no commit: it published nothing.
    pub fn publishing_nothing() -> Self {
        Self {
            no_commit: true,
            ..Self::default()
        }
    }
```

and in its `flush`, replace `commit: Some("c1".into()),` with `commit: (!self.no_commit).then(|| "c1".into()),`.

In `crates/cli/src/ctx.rs`, extend the imports to:

```rust
use fl_core::decision::{Decision, Flushed, LeftLocal};
use fl_core::ids::{GateId, ProjectId};
use fl_core::log::{Attempt, GateRun};
use fl_core::store::{Handles, Ledger, Roles, StoreError, Tracker};
use fl_store::RedbStore;
use std::cell::RefCell;
```

add to `pub struct Ctx`, after `github_ledger`:

```rust
    /// The bound ledger as decisions see it, remembering each decision
    /// whose flush landed, for its comment (GitHub ledger spec §4.1). Set
    /// with the GitHub ledger, and only then.
    pub witness: Option<&'a Witness<'a>>,
```

and after `impl Ctx<'_> { … }`:

```rust
/// One decision this command flushed, and what its flush did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flush {
    pub decision: Decision,
    pub flushed: Flushed,
}

/// The bound ledger, remembering each decision whose flush landed, so the
/// command can post its comment once the state change is done (GitHub
/// ledger spec §4.1). Every call goes through to the ledger it wraps.
pub struct Witness<'a> {
    inner: &'a dyn Ledger,
    seen: RefCell<Vec<Flush>>,
}

impl<'a> Witness<'a> {
    pub fn new(inner: &'a dyn Ledger) -> Self {
        Self {
            inner,
            seen: RefCell::new(Vec::new()),
        }
    }

    /// The decisions flushed since the last call, oldest first.
    pub fn take(&self) -> Vec<Flush> {
        std::mem::take(&mut *self.seen.borrow_mut())
    }
}

impl Ledger for Witness<'_> {
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError> {
        self.inner.append_gate_run(run)
    }

    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError> {
        self.inner.append_attempt(attempt)
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        self.inner.gate_runs(gate)
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        self.inner.attempts(project)
    }

    /// ⚠ Remembered only once the flush returned: a refused flush
    /// published nothing for a comment to show.
    fn flush(&self, decision: Decision) -> Result<Flushed, StoreError> {
        let kept = decision.clone();
        let flushed = self.inner.flush(decision)?;
        self.seen.borrow_mut().push(Flush {
            decision: kept,
            flushed: flushed.clone(),
        });
        Ok(flushed)
    }
}
```

In `ctx.rs`'s test `the_roles_bind_the_ledger_the_command_was_given` and `cmd/ledger.rs`'s test `init_without_the_projects_root_refuses_and_creates_nothing`, add `witness: None,` to the `Ctx { … }` literal.

In `crates/cli/src/comment.rs`, above `mod tests`, add:

```rust
use crate::ctx::{Ctx, Flush};
use fl_core::decision::Outcome;
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::store::{Catalog, Ledger, StoreError};
use fl_github::GithubLedger;
use fl_github::ledger::disclose::{self, Visibility};
use fl_github::ledger::layout::decision_subject;
use fl_github::ledger::render::{self, Catalogued, DecisionView};
use fl_store::RedbStore;

/// What the local catalog says of `project`: each gate's name, and each
/// transition's gates. A comment names gates by it (spec §4.2).
pub fn catalogued(store: &RedbStore, project: &ProjectId) -> Result<Catalogued, StoreError> {
    let mut cat = Catalogued::default();
    for g in store.list_gates(project)? {
        cat.names.insert(g.id, g.name);
    }
    for t in store.list_transitions(project)? {
        cat.transitions.insert(t.name, t.gates);
    }
    Ok(cat)
}

/// How a person names `item`: its issue number, or its IRI.
fn named(item: &Iri) -> String {
    fl_github::meta::parse_issue_url(item)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| item.to_string())
}

/// The command that posts `item`'s missing comments (spec §4.3).
pub fn recovery(item: &Iri) -> String {
    format!("fl github ledger comment {}", named(item))
}

/// Post the comment of each decision this command flushed (spec §4.1),
/// once its state change is done. `completed`: the command's state change
/// returned without error (§4.2).
///
/// ⚠ A comment that cannot be posted is a warning, never an error: the
/// decision and its state change stand, and the command keeps its own exit
/// code. Each warning names the command that posts the comment later.
pub fn post_after(ctx: &Ctx<'_>, project: &ProjectId, completed: bool) -> Vec<String> {
    let Some(witness) = ctx.witness else {
        return Vec::new();
    };
    let Some(gl) = ctx.github_ledger else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    for flush in witness.take() {
        // A flush that published nothing has nothing to show.
        if flush.flushed.commit.is_none() {
            continue;
        }
        // ⚠ An id fl does not write gets no comment, now or from recovery
        // (`render::markable`): its marker could close the HTML comment
        // early. Someone wrote that line by hand; say what to do about it.
        if !render::markable(&flush.decision.id) {
            warnings.push(format!(
                "warning: decision {} has an id fl does not write, so it gets no comment; \
                 someone wrote its ledger line by hand. Run `fl github ledger verify`, then \
                 `fl github ledger quarantine` on that line",
                flush.decision.id.as_str().escape_debug()
            ));
            continue;
        }
        if let Err(e) = post_one(ctx.store, gl, project, &flush, completed) {
            let item = decision_subject(&flush.decision);
            warnings.push(format!(
                "warning: the decision's comment was not posted on issue {}: {}. {}; run `{}` \
                 to post it",
                named(item),
                fl_core::as_clause(&e),
                what_stands(&flush.decision.outcome, completed),
                recovery(item)
            ));
        }
    }
    warnings
}

/// What a failed comment's warning says stands: a state change only when
/// the decision made one and it completed.
fn what_stands(outcome: &Outcome, completed: bool) -> &'static str {
    let changes_state = matches!(
        outcome,
        Outcome::Move { allowed: true, .. }
            | Outcome::Reproduce { accepted: true, .. }
            | Outcome::Verify { closed: true, .. }
    );
    if changes_state && completed {
        "The decision and its state change stand"
    } else {
        "The decision stands"
    }
}

/// [`post_after`], each warning on stderr.
pub fn after(ctx: &Ctx<'_>, project: &ProjectId, completed: bool) {
    for w in post_after(ctx, project, completed) {
        eprintln!("{w}");
    }
}

fn post_one(
    store: &RedbStore,
    gl: &GithubLedger<'_>,
    project: &ProjectId,
    flush: &Flush,
    completed: bool,
) -> Result<(), StoreError> {
    let visibility = gl.visibility()?;
    let view = local_view(store, gl, project, flush, visibility)?;
    let state = render::state_line(&flush.decision.outcome, completed);
    let body = render::render(&view, &gl.repo().full_name, visibility, Some(&state));
    gl.post_comment(decision_subject(&flush.decision), &body)
}

/// The view of a decision this command flushed: the entries it rests on,
/// read from the local store rather than back from GitHub, projected for
/// the repository's visibility exactly as the flush published them
/// (decision 2).
fn local_view(
    store: &RedbStore,
    gl: &GithubLedger<'_>,
    project: &ProjectId,
    flush: &Flush,
    visibility: Visibility,
) -> Result<DecisionView, StoreError> {
    let cat = catalogued(store, project)?;
    let mut runs = Vec::new();
    for g in render::candidate_gates(&flush.decision.outcome, &cat) {
        runs.extend(
            store
                .gate_runs(&g)?
                .iter()
                .map(|r| disclose::run(r, visibility)),
        );
    }
    let attempts = match flush.decision.outcome {
        Outcome::Attempt { .. } => store
            .attempts(project)?
            .iter()
            .map(|a| disclose::attempt(a, visibility))
            .collect(),
        _ => Vec::new(),
    };
    Ok(render::view(
        flush.decision.clone(),
        gl.by()?,
        flush.flushed.commit.clone(),
        &runs,
        &attempts,
        &cat,
    ))
}
```

In `crates/cli/src/main.rs`, replace

```rust
    let ledger: &dyn fl_core::Ledger = match &split {
        Some(s) => s,
        None => &store,
    };
```

with

```rust
    // Each decision this command flushes, remembered so its comment can be
    // posted once the state change is done (GitHub ledger spec §4.1).
    let witness = split.as_ref().map(|s| ctx::Witness::new(s));
    let ledger: &dyn fl_core::Ledger = match &witness {
        Some(w) => w,
        None => &store,
    };
```

and add `witness: witness.as_ref(),` after `github_ledger: github_ledger.as_ref(),` in the GitHub `Ctx` literal, and `witness: None,` after `github_ledger: None,` in the local one.

In `crates/cli/src/cmd/record.rs`, in the `Move` arm, replace

```rust
            let report =
                move_record(ctx.roles(), &record, state).map_err(|e| anyhow::anyhow!("{e}"))?;
```

with

```rust
            let moved = move_record(ctx.roles(), &record, state);
            // Spec §4.1: the ledger commit, then the state change, then the
            // comment — posted whether the state change completed or not,
            // saying which (§4.2), before an error becomes the exit.
            crate::comment::after(ctx, &record.project, moved.is_ok());
            let report = moved.map_err(|e| anyhow::anyhow!("{e}"))?;
```

In `crates/cli/src/cmd/check.rs`, in `run`, after `crate::ctx::report_flush(&flushed);`, add:

```rust
    // Spec §4.1: a check tied to a record changes no state; its comment
    // follows its flush. A plain check flushed nothing.
    crate::comment::after(ctx, &project, true);
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS — every existing test included.

- [ ] **Step 5: Mutation checks**

1. The witness remembers only a flush that landed: push before calling `self.inner.flush(decision)` (on a clone), then `?` → `cargo test -p fl-cli --bin fl -- ctx::tests::the_witness_remembers_only_a_flush_that_landed_and_forwards_it` red.
2. …and forwards it: return `Ok(Flushed::NOTHING)` without calling `inner` → the same test red ("forwarded").
3. No comment for a flush that published nothing: delete the `continue` → `cargo test -p fl-cli --bin fl -- comment::tests::a_flush_that_published_nothing_gets_no_comment` red.
4. A warning, never an error: in `record.rs`, replace `crate::comment::after(…)` with `if !crate::comment::post_after(ctx, &record.project, moved.is_ok()).is_empty() { anyhow::bail!("comment"); }` → `cargo test -p fl-cli --test ledger -- a_move_whose_comment_fails_keeps_its_exit_code_and_names_the_recovery` red.
5. Ordering — after the state change: move the `after` call up to just below `crate::preflight::check(…)?;` → `a_move_posts_its_comment_on_the_record_after_the_state_change` red (nothing witnessed yet, no comment).
6. Ordering — before the error becomes the exit: move it below `let report = moved…?;` → `a_move_whose_state_change_fails_after_its_flush_says_so_in_its_comment` red.
7. `completed` is the move's own result: pass `true` → the same test red.
8. `check`: delete its `after` call → `a_check_with_a_record_posts_its_comment_and_a_plain_check_posts_none` red; move it above `let flushed = publish(…)?;` → the same test red.
9. `check`'s exit code is the check's: replace its `after` with the bail of mutation 4 → `a_check_whose_comment_fails_keeps_the_checks_own_exit_code` red.
10. `main.rs` decides through the witness: bind `ledger` to the split ledger directly (`match &split { Some(s) => s, None => &store }`), keeping `witness: witness.as_ref()` in `Ctx` → `a_move_posts_its_comment…` red.
11. The repository's visibility chooses the excerpts: in `post_one`, pass `Visibility::Private` to `render` → `cargo test -p fl-cli --test ledger -- a_comment_on_a_repository_that_is_not_private_shows_no_error_detail` red (the projected run's withheld text shows in a block).
12. The projection and the render arm together (defence in depth): in `local_view`, replace `disclose::run(r, visibility)` with `r.clone()` **and** pass `Visibility::Private` in `post_one` → the same test red (the path shows). Alone, the projection's removal stays green: `render` shows no block on a repository that is not private, and the table shows a verdict's label only. Each layer is a second guard for the other.
13. An id fl does not write gets no comment, and its own warning: delete `post_after`'s `if !render::markable(…)` block → `cargo test -p fl-cli --bin fl -- comment::tests::a_decision_whose_id_fl_does_not_write_gets_a_warning_and_no_comment` red (a comment is posted, with no marker, and no warning).
14. What stands, each conjunct and arm: drop `&& completed` → `cargo test -p fl-cli --bin fl -- comment::tests::a_failed_comment_says_what_stands` red; delete each of the three `matches!` alternatives in turn (an allowed move, an accepted reproduction, a closed verify) → the same test red; drop `allowed: true` (any move) → the same test red.

Not guards: `post_after`'s two `let … else { return }` — the types force them (no ledger to post through, no witness to read); `main.rs` sets `witness` exactly when it sets `github_ledger`.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/ctx.rs crates/cli/src/comment.rs crates/cli/src/main.rs crates/cli/src/cmd/record.rs crates/cli/src/cmd/check.rs crates/cli/src/cmd/ledger.rs crates/cli/src/testing.rs crates/cli/tests/ledger.rs
git commit -m "feat(cli): a move and a check --record post their comment after the state change

A Witness over the bound ledger remembers each decision whose flush
landed. Once the move's state change returns, completed or not, its
comment is posted on the record's issue saying which (spec 4.1, 4.2); a
check --record's after its flush. A comment that cannot be posted is a
warning naming fl github ledger comment, and the command keeps its exit
code. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 8: A finding's and an attempt's comment

Spec §4.1 for the other three flush sites: `finding reproduce` and `finding verify` comment on the finding's issue — a refused reproduction too (decision 11) — and `fl attempt` on the record's issue. An attempt keeps its own exit code whatever its comment does (decision 14's reason, ruling 3).

**Blast radius:** the three call sites only; Task 7's `comment::after` is unchanged.

**Files:**
- Modify: `crates/cli/src/cmd/finding.rs` (the `Reproduce` and `Verify` arms)
- Modify: `crates/cli/src/cmd/attempt.rs` (`run`)
- Modify: `crates/cli/tests/ledger.rs`

**Interfaces:**
- Consumes: `crate::comment::after(ctx, project, completed)` (Task 7); `NOT_POSTED`, `World` (Task 7).
- Produces: nothing new.

- [ ] **Step 1: Write the failing tests**

In `crates/cli/tests/ledger.rs`, in `impl World`, add:

```rust
    /// `ready`, a file named `bug`, and a finding (#2) on record #1.
    fn finding_raised(&self) {
        self.ready();
        fs::write(self.repo.path().join("bug"), "").unwrap();
        self.fl()
            .args([
                "finding", "raise", "--record", "1", "--claim", "a bug", "--by", "reviewer",
            ])
            .assert()
            .success();
    }
```

and after `a_move_whose_state_change_fails_after_its_flush_says_so_in_its_comment`, add:

```rust
// Spec §4.1: a finding's decisions comment on the finding's issue.
#[test]
fn a_reproduction_posts_its_comment_on_the_findings_issue() {
    let w = World::new();
    w.finding_raised();
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .success();
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let c = &comments[0];
    assert!(c.contains("### fl reproduce: accepted"), "{c}");
    assert!(
        c.contains("The state change completed: the finding records this gate as its reproduction."),
        "{c}"
    );
    assert!(c.contains("| reproduction | no-bug | FAIL |"), "{c}");
    assert!(w.fake.issue(1).comments.is_empty(), "nothing on the record's issue");
}

// Decision 11: a refused reproduction is flushed and commented, and the
// command is refused as before.
#[test]
fn a_refused_reproduction_is_commented_as_refused() {
    let w = World::new();
    w.finding_raised();
    fs::remove_file(w.repo.path().join("bug")).unwrap();
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .code(2)
        .stderr(contains("currently PASSES"));
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(comments[0].contains("### fl reproduce: refused"), "{}", comments[0]);
    assert!(
        comments[0].contains("The reproduction was refused: the finding is unchanged."),
        "{}",
        comments[0]
    );
}

/// `finding_raised`, reproduced, assigned, and the bug fixed.
fn ready_to_verify(w: &World) {
    w.finding_raised();
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .success();
    w.fl()
        .args(["finding", "assign", "2", "--to", "fixer"])
        .assert()
        .success();
    fs::remove_file(w.repo.path().join("bug")).unwrap();
}

#[test]
fn a_verification_that_closes_its_finding_posts_its_comment() {
    let w = World::new();
    ready_to_verify(&w);
    w.fl().args(["finding", "verify", "2"]).assert().success();
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(comments[1].contains("### fl verify: closed"), "{}", comments[1]);
    assert!(
        comments[1].contains("The state change completed: the finding is closed."),
        "{}",
        comments[1]
    );
}

// A verification that ran but did not close is commented, saying the
// finding stays open.
#[test]
fn a_verification_that_does_not_close_its_finding_posts_its_comment() {
    let w = World::new();
    ready_to_verify(&w);
    fs::write(w.repo.path().join("bug"), "").unwrap();
    w.fl().args(["finding", "verify", "2"]).assert().code(1);
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(comments[1].contains("### fl verify: not closed"), "{}", comments[1]);
    assert!(
        comments[1].contains("The finding stays open: the repair is not done."),
        "{}",
        comments[1]
    );
}

// ⚠ Spec §4.2: a verification whose closing fails after its flush is
// commented too, saying so; the command exits 2 as before.
#[test]
fn a_verification_whose_closing_fails_says_so_in_its_comment() {
    let w = World::new();
    ready_to_verify(&w);
    w.fake.state().foreign_label_on_next_patch = true;
    w.fl().args(["finding", "verify", "2"]).assert().code(2);
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(
        comments[1].contains("The finding passed its verification, but closing it did not complete."),
        "{}",
        comments[1]
    );
}

// Spec §4.1 and decision 14: an attempt's comment, on the record's issue;
// one that cannot be posted is a warning, and the exit code stays the
// attempt's own.
#[test]
fn an_attempts_comment_that_fails_keeps_the_attempts_exit_code() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1)
        .stderr(contains(NOT_POSTED).not());
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let c = &comments[0];
    assert!(c.contains("### fl attempt: refused"), "{c}");
    assert!(c.contains("An attempt changes no state."), "{c}");
    assert!(c.contains("| claude | refused |"), "{c}");
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1)
        .stderr(contains(NOT_POSTED).and(contains("fl github ledger comment 1")));
    assert_eq!(w.fake.issue(1).comments.len(), 1);
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --test ledger -- reproduction verification attempts_comment`
Expected: the six new tests FAIL — no comment is posted.

- [ ] **Step 3: Implement**

In `crates/cli/src/cmd/finding.rs`, in the `Reproduce` arm, replace

```rust
            let (report, flushed) = attach_reproduction(ctx.roles(), &fid, &gid)
                .map_err(|e| explain(e, &finding, Some(&gate)))?;
```

with

```rust
            let reproduced = attach_reproduction(ctx.roles(), &fid, &gid);
            // Spec §4.1: on the finding's issue, once the state change is
            // done — a refused reproduction's too (decision 11) — before an
            // error becomes the exit.
            crate::comment::after(ctx, &f.project, reproduced.is_ok());
            let (report, flushed) = reproduced.map_err(|e| explain(e, &finding, Some(&gate)))?;
```

In the `Verify` arm, replace

```rust
            let report =
                verify_finding(ctx.roles(), &id).map_err(|e| explain(e, &finding, None))?;
```

with

```rust
            let verified = verify_finding(ctx.roles(), &id);
            // Spec §4.1–§4.2: on the finding's issue, saying whether the
            // finding closed, before an error becomes the exit.
            crate::comment::after(ctx, &f.project, verified.is_ok());
            let report = verified.map_err(|e| explain(e, &finding, None))?;
```

In `crates/cli/src/cmd/attempt.rs`, in `run`, just before `let entry = stamp::entry_id();`, add:

```rust
    let project_id = record.project.clone();
```

and replace the end of `run`

```rust
    Ok(code)
}
```

with

```rust
    // Spec §4.1: the attempt's comment, on its record's issue. ⚠ One that
    // cannot be posted is a warning: the exit code stays the attempt's own
    // (decision 14).
    crate::comment::after(ctx, &project_id, true);
    Ok(code)
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Each with `cargo test -p fl-cli --test ledger -- <name>`:

1. `reproduce` comments before the error becomes the exit (ordering): move its `after` below `let (report, flushed) = reproduced…?;` → `a_refused_reproduction_is_commented_as_refused` red.
1a. `reproduce`'s call: delete it → `a_reproduction_posts_its_comment_on_the_findings_issue` and `a_refused_reproduction_is_commented_as_refused` red.
2. `reproduce`'s `completed`: pass `false` → `a_reproduction_posts_its_comment_on_the_findings_issue` red ("did not complete").
3. `verify` before the error (ordering): move its `after` below `let report = verified…?;` → `a_verification_whose_closing_fails_says_so_in_its_comment` red.
4. `verify`'s `completed`: pass `true` → the same test red.
5. `verify`'s call: delete it → `a_verification_that_closes_its_finding_posts_its_comment` and `a_verification_that_does_not_close_its_finding_posts_its_comment` red.
6. `attempt`'s call: delete it → `an_attempts_comment_that_fails_keeps_the_attempts_exit_code` red.
7. `attempt`'s exit code is its own: replace its `after` with `if !crate::comment::post_after(ctx, &project_id, true).is_empty() { return Ok(2); }` → the same test red.

Not a guard: the attempt's comment after its printed outcome. stdout and stderr are separate streams, so no test can order a stderr warning against a stdout line; decision 15's "outcome first" is about the outcome and the save error, both settled in `settle` before the comment.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/cmd/finding.rs crates/cli/src/cmd/attempt.rs crates/cli/tests/ledger.rs
git commit -m "feat(cli): a finding's and an attempt's decisions post their comment

finding reproduce and finding verify comment on the finding's issue, a
refused reproduction included (decision 11), saying whether the state
change completed; fl attempt comments on the record's issue and keeps its
own exit code whatever the comment does (decision 14). Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 9: `fl github ledger comment <item>`

Spec §4.3: every decision filed under a record or finding whose comment no comment on its issue marks — every page read — rendered from the ledger and posted, oldest first, where the issue is now. Ruling 18 for its shape.

**Blast radius:** a new subcommand; `github::Cmd::refs` now includes its item, so `iris()` and `has_handle()` see it as they see every command's items.

**Files:**
- Modify: `crates/cli/src/cmd/ledger.rs` (`Cmd::Comment`, `Cmd::refs`, `comment`; tests)
- Modify: `crates/cli/src/cmd/github.rs` (`Cmd::refs`)
- Modify: `crates/cli/tests/ledger.rs`

**Interfaces:**
- Consumes: `GithubLedger::{issue_at, posted, post_at, published_decisions, commit_of, runs_of, attempts_of, visibility, repo}` (Tasks 5, 6); `render::{candidate_gates, view, render}`; `crate::comment::catalogued` (Task 7); `GithubTracker::issue_url`.
- Produces: `Cmd::Comment { item: Ref }`; `pub fn refs(&self) -> Vec<&Ref>` on `cmd::ledger::Cmd`. Output: on stdout `moved\t<url>`, `posted\t<decision id>`, `comments\t<n> posted, <m> already there`; on stderr `skipped\t<id>\t…` for each decision whose id fl does not write. Exit 0; 1 when it skipped one; 2 when a read or a post fails.

- [ ] **Step 1: Write the failing tests**

In `crates/cli/src/cmd/ledger.rs`, inside `mod tests`, add:

```rust
    // The item `comment` names reaches the store's choice and the handle
    // check, as every command's items do.
    #[test]
    fn the_item_comment_names_is_one_of_its_refs() {
        use crate::cmd::github::Cmd as Github;
        assert!(Github::Ledger(Cmd::Comment { item: Ref::Handle(3) }).has_handle());
        let iri = fl_core::Iri::parse("https://github.com/acme/widgets/issues/3").unwrap();
        assert_eq!(
            Github::Ledger(Cmd::Comment { item: Ref::Iri(iri.clone()) }).iris(),
            vec![iri]
        );
        assert!(Github::Ledger(Cmd::Verify { max_commits: 1 }).iris().is_empty());
    }
```

In `crates/cli/tests/ledger.rs`, after `an_attempts_comment_that_fails_keeps_the_attempts_exit_code`, add:

```rust
/// How many of the fake's requests since the last clear are `line`.
fn requests_equal(w: &World, line: &str) -> usize {
    w.fake.state().requests.iter().filter(|r| *r == line).count()
}

// ⚠ Spec §4.3: only what no comment marks, rendered from the ledger with
// no state line — the ledger does not record whether the state change
// completed. A second run posts nothing, and reads nothing past the
// listing.
#[test]
fn comment_posts_only_what_is_missing_and_a_second_run_posts_nothing() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success()
        .stderr(contains(NOT_POSTED));
    assert_eq!(w.fake.issue(1).comments.len(), 1);
    w.fake.state().requests.clear();
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 1 already there"));
    assert_eq!(
        requests_equal(&w, "POST /graphql"),
        3,
        "the decisions' listing, the runs' listing, and one blame"
    );
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(comments[1].contains("### fl check: passed"), "{}", comments[1]);
    assert!(comments[1].contains("| launch | no-bug | PASS |"), "{}", comments[1]);
    assert!(!comments[1].contains("A check changes no state."), "{}", comments[1]);
    w.fake.state().requests.clear();
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t0 posted, 2 already there").and(contains("posted\turn").not()));
    assert_eq!(w.fake.issue(1).comments.len(), 2);
    assert_eq!(requests_equal(&w, "POST /graphql"), 1, "only the decisions' listing");
    assert_eq!(
        requests_equal(&w, "GET /repos/acme/widgets"),
        1,
        "the tracker's own read; no visibility read when nothing is missing"
    );
}

// Oldest first; a post that fails is refused and names nothing posted; a
// re-run posts what is left.
#[test]
fn comment_posts_oldest_first_and_a_rerun_after_a_failed_post_posts_the_rest() {
    let w = World::new();
    w.ready();
    for args in [
        &["record", "move", "1", "--to", "doing"][..],
        &["check", "launch", "--project", "1", "--record", "1"][..],
    ] {
        w.fake.state().fail_comment_next = true;
        w.fl().args(args).assert().success().stderr(contains(NOT_POSTED));
    }
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .code(2)
        .stdout(contains("posted\t").not());
    assert!(w.fake.issue(1).comments.is_empty());
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t2 posted, 0 already there"));
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(comments[0].contains("### fl move: allowed"), "{}", comments[0]);
    assert!(comments[1].contains("### fl check: passed"), "{}", comments[1]);
}

// ⚠ Spec §4.3: a marker on a later page still counts.
#[test]
fn comment_finds_its_markers_on_every_page() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    {
        let mut s = w.fake.state();
        s.issues.get_mut(&1).unwrap().comments.push("thanks!".into());
        s.max_per_page = 1;
    }
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t0 posted, 2 already there"));
    assert_eq!(w.fake.issue(1).comments.len(), 3);
}

// ⚠ A page that fails is an error, never "none posted, so post them all".
#[test]
fn a_comment_page_that_cannot_be_read_posts_nothing() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    {
        let mut s = w.fake.state();
        s.issues.get_mut(&1).unwrap().comments.push("thanks!".into());
        s.max_per_page = 1;
        s.fail_page = Some(("/repos/acme/widgets/issues/1/comments".into(), 2));
    }
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .code(2)
        .stdout(contains("posted\t").not());
    assert_eq!(w.fake.issue(1).comments.len(), 2, "nothing posted");
}

// Spec §4.1: a transferred issue gets its comment where it is now.
#[test]
fn comment_on_a_transferred_issue_posts_where_it_is_now() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    w.fake.transfer(1);
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(
            contains("moved\thttps://github.com/elsewhere/transferred/issues/1\n")
                .and(contains("comments\t1 posted, 0 already there")),
        );
    let s = w.fake.state();
    assert_eq!(s.transferred[&1].comments.len(), 1);
    assert!(s.transferred[&1].comments[0].contains("### fl move: allowed"));
    assert!(s.issues[&1].comments.is_empty(), "nothing at the old address");
}

#[test]
fn comment_on_a_finding_posts_its_decisions_on_the_findings_issue() {
    let w = World::new();
    w.finding_raised();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .success();
    w.fl()
        .args(["github", "ledger", "comment", "2"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 0 already there"));
    let comments = w.fake.issue(2).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(comments[0].contains("### fl reproduce: accepted"), "{}", comments[0]);
    assert!(comments[0].contains("| reproduction | no-bug | FAIL |"), "{}", comments[0]);
    assert!(w.fake.issue(1).comments.is_empty());
}

// Spec §4.2: a recovered comment links the commit that holds its
// decision, not the ledger's head.
#[test]
fn a_recovered_comment_links_the_commit_that_holds_its_decision() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    let decided = w.fake.ledger_head().expect("the move's commit");
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    assert_ne!(w.fake.ledger_head(), Some(decided.clone()));
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 1 already there"));
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(comments[1].contains("### fl move: allowed"), "{}", comments[1]);
    assert!(
        comments[1].contains(&format!("/commit/{decided})")),
        "{}",
        comments[1]
    );
}

#[test]
fn comment_recovers_an_attempts_comment_from_the_ledger() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["attempt", "1", "--budget-usd-micros", "0"])
        .assert()
        .code(1);
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 0 already there"));
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(comments[0].contains("### fl attempt: refused"), "{}", comments[0]);
    assert!(comments[0].contains("| claude | refused |"), "{}", comments[0]);
}

// ⚠ A marker in a comment someone else wrote does not stop recovery.
#[test]
fn a_marker_someone_else_posted_does_not_stop_recovery() {
    let w = World::new();
    w.ready();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let ids = w.decision_ids();
    assert_eq!(ids.len(), 1, "{ids:?}");
    {
        let mut s = w.fake.state();
        let i = s.issues.get_mut(&1).unwrap();
        i.comments
            .push(format!("<!-- fl:decision {{\"id\":\"{}\"}} -->\n\nnot fl", ids[0]));
        i.comment_authors = vec!["someone-else".into()];
    }
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 0 already there"));
    assert_eq!(w.fake.issue(1).comments.len(), 2);
}

// ⚠ Several developers hold a token each: a comment a colleague's machine
// posted, under a login that wrote a decision under the item, counts —
// recovery run here does not post that decision again.
#[test]
fn a_comment_by_another_account_that_published_under_the_item_counts() {
    use fl_github::ledger::layout;
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let id = "urn:uuid:00000000-0000-7000-8000-0000000000c1";
    let theirs = fl_core::Decision {
        id: fl_core::Iri::parse(id).unwrap(),
        at: fl_core::At::from_unix_millis(1),
        record: fl_core::RecordId(
            fl_core::Iri::parse("https://github.com/acme/widgets/issues/1").unwrap(),
        ),
        finding: None,
        outcome: fl_core::Outcome::Check {
            transition: fl_core::TransitionOutcome {
                transition: "launch".into(),
                passed: true,
            },
        },
        rests_on: vec![],
    };
    let (path, text) = w.only_file_in("decisions");
    let line = layout::Line::Decision(theirs).encode("colleague");
    w.fake
        .hand_commit(&[(path.as_str(), Some(format!("{text}{line}\n").as_str()))]);
    {
        let mut s = w.fake.state();
        let i = s.issues.get_mut(&1).unwrap();
        assert_eq!(i.comments.len(), 1, "the check's own comment");
        i.comments
            .push(format!("<!-- fl:decision {{\"id\":\"{id}\"}} -->\n\ntheirs"));
        i.comment_authors = vec!["fake-user".into(), "colleague".into()];
    }
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t0 posted, 2 already there"));
    assert_eq!(w.fake.issue(1).comments.len(), 2, "nothing posted twice");
}

// ⚠ A decision line whose id fl does not write is skipped, said on
// stderr, and the command exits 1; nothing is posted for it.
#[test]
fn comment_skips_a_decision_whose_id_fl_does_not_write() {
    use fl_github::ledger::layout;
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let hostile = fl_core::Decision {
        id: fl_core::Iri::parse("urn:x:a--><b>").unwrap(),
        at: fl_core::At::from_unix_millis(1),
        record: fl_core::RecordId(
            fl_core::Iri::parse("https://github.com/acme/widgets/issues/1").unwrap(),
        ),
        finding: None,
        outcome: fl_core::Outcome::Check {
            transition: fl_core::TransitionOutcome {
                transition: "launch".into(),
                passed: true,
            },
        },
        rests_on: vec![],
    };
    let (path, text) = w.only_file_in("decisions");
    let line = layout::Line::Decision(hostile).encode("fake-user");
    w.fake
        .hand_commit(&[(path.as_str(), Some(format!("{text}{line}\n").as_str()))]);
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .code(1)
        .stderr(contains("skipped\t").and(contains("is not one fl writes")))
        .stdout(contains("comments\t0 posted, 1 already there"));
    assert_eq!(w.fake.issue(1).comments.len(), 1, "only the check's own comment");
}

// Decision 2: a comment recovered on a repository that is not private
// shows no error detail, and no stand-in for one.
#[test]
fn comment_on_a_repository_that_is_not_private_recovers_no_error_detail() {
    let w = World::new();
    let program = w.repo.path().join("no-such-gate");
    w.gated_with("no-bug", program.to_str().unwrap());
    w.init();
    w.export();
    w.fake.state().repos[0].visibility = "public".into();
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .code(2);
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted, 0 already there"));
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let c = &comments[0];
    assert!(c.contains("| ERROR |"), "{c}");
    assert!(!c.contains("<details>"), "{c}");
    assert!(!c.contains("withheld"), "{c}");
    assert!(!c.contains("no-such-gate"), "{c}");
}

#[test]
fn comment_without_the_ledger_key_is_refused() {
    let w = World::bound(false);
    w.gated();
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .code(2)
        .stderr(contains("does not name the GitHub ledger"));
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli`
Expected: FAIL to compile — `Cmd::Comment` does not exist.

- [ ] **Step 3: Implement**

In `crates/cli/src/cmd/ledger.rs`, extend the imports with:

```rust
use crate::refs::Ref;
use fl_core::decision::Outcome;
use fl_github::ledger::{Published, render};
use std::collections::BTreeSet;
```

add the variant at the end of `pub enum Cmd`:

```rust
    /// Post every decision comment missing from a record's or a finding's
    /// issue, rendered from the ledger. Safe to run again: a decision whose
    /// comment is there is skipped.
    Comment {
        /// The record or finding: its issue number (`41` or `#41`),
        /// `owner/repo#41`, or its URL.
        item: Ref,
    },
```

after the enum:

```rust
impl Cmd {
    /// Every item this command names, by `Ref`.
    pub fn refs(&self) -> Vec<&Ref> {
        match self {
            Cmd::Comment { item } => vec![item],
            Cmd::Init { .. } | Cmd::Verify { .. } | Cmd::Quarantine { .. } => vec![],
        }
    }
}
```

in `run`'s `match cmd`, add `Cmd::Comment { item } => comment(ctx, gl, &item),`, and after `quarantine`:

```rust
/// `fl github ledger comment <item>` (spec §4.3): every decision filed
/// under the item whose comment no comment on its issue marks — all pages
/// read — rendered from the ledger and posted where the issue is now,
/// oldest first.
fn comment(ctx: &Ctx<'_>, gl: &GithubLedger<'_>, item: &Ref) -> Result<i32> {
    let Some(gh) = ctx.github else {
        bail!("`fl github ledger comment` needs the project bound to a GitHub repository");
    };
    let iri = match item {
        Ref::Handle(n) => gh.issue_url(*n),
        Ref::Iri(i) => i.clone(),
    };
    let at = gl.issue_at(&iri)?;
    if let Some(to) = &at.moved_to {
        println!("moved\t{to}");
    }
    let (head, published) = gl.published_decisions(&iri)?;
    // Whose comments may mark a decision here: fl's own login, which
    // `posted` adds, and everyone who wrote a decision under the item (spec
    // §4.3; a colleague's machine posts under its own login).
    let writers: BTreeSet<String> = published.iter().map(|p| p.by.clone()).collect();
    let posted = gl.posted(&at, &writers)?;
    let missing: Vec<&Published> = published
        .iter()
        .filter(|p| !posted.contains(&p.decision.id))
        .collect();
    let already = published.len() - missing.len();
    // ⚠ A decision whose id fl does not write gets no comment: its marker
    // could close the HTML comment early (`render::markable`). Said, and
    // never posted.
    let (missing, unmarkable): (Vec<&Published>, Vec<&Published>) = missing
        .into_iter()
        .partition(|p| render::markable(&p.decision.id));
    for p in &unmarkable {
        eprintln!(
            "skipped\t{}\tits id is not one fl writes, so it gets no comment; quarantine its \
             line (`fl github ledger quarantine {} {} --by <name> --reason <text>`)",
            p.decision.id.as_str().escape_debug(),
            p.file,
            p.line
        );
    }
    // Nothing missing: nothing more to read.
    if !missing.is_empty() {
        let cat = crate::comment::catalogued(ctx.store, &at.project)?;
        let mut gates = Vec::new();
        for p in &missing {
            for g in render::candidate_gates(&p.decision.outcome, &cat) {
                if !gates.contains(&g) {
                    gates.push(g);
                }
            }
        }
        let runs = gl.runs_of(&gates)?;
        // The project's attempts are read only when an attempt's comment is
        // missing.
        let attempts = if missing
            .iter()
            .any(|p| matches!(p.decision.outcome, Outcome::Attempt { .. }))
        {
            gl.attempts_of(&at.project)?
        } else {
            Vec::new()
        };
        let visibility = gl.visibility()?;
        let repo = &gl.repo().full_name;
        for p in &missing {
            let view = render::view(
                p.decision.clone(),
                p.by.clone(),
                gl.commit_of(&head, p),
                &runs,
                &attempts,
                &cat,
            );
            // No state line: the ledger does not record whether the state
            // change completed.
            gl.post_at(&at, &render::render(&view, repo, visibility, None))?;
            println!("posted\t{}", p.decision.id);
        }
    }
    println!(
        "comments\t{} posted, {already} already there",
        missing.len()
    );
    Ok(if unmarkable.is_empty() { 0 } else { 1 })
}
```

In `crates/cli/src/cmd/github.rs`, in `Cmd::refs`, replace `Cmd::Whoami | Cmd::Ledger(_) => vec![],` with:

```rust
            Cmd::Whoami => vec![],
            Cmd::Ledger(c) => c.refs(),
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

1. The item is one of the command's refs: revert `Cmd::Ledger(c) => c.refs()` to `vec![]` → `cargo test -p fl-cli --bin fl -- cmd::ledger::tests::the_item_comment_names_is_one_of_its_refs` red.
2. Only what no comment marks: replace the filter with `|_| true` → `cargo test -p fl-cli --test ledger -- comment_posts_only_what_is_missing_and_a_second_run_posts_nothing` red.
3. Nothing more read when nothing is missing: replace `if !missing.is_empty() {` with `if true {` → the same test red (a second `GET /repos/acme/widgets`, the visibility read).
4. The project's attempts only for a missing attempt's comment: replace the `any(…)` condition with `true` → the same test red (four GraphQL requests in its first run).
5. Post, then say so (each step of the loop): print `posted` before `post_at` → `comment_posts_oldest_first_and_a_rerun_after_a_failed_post_posts_the_rest` red.
6. Oldest first: iterate `missing.iter().rev()` → the same test red.
7. No state line: pass `Some("A check changes no state.")` → `comment_posts_only_what_is_missing…` red.
8. The move is said: delete the `moved` line → `comment_on_a_transferred_issue_posts_where_it_is_now` red.
9. The link names the decision's own commit: pass `None` for the commit → `a_recovered_comment_links_the_commit_that_holds_its_decision` red; pass `Some(head.clone())` → the same test red.
10. The directory read is the item's own: replace `gl.published_decisions(&iri)` with `gl.published_decisions(&gh.issue_url(1))` → `comment_on_a_finding_posts_its_decisions_on_the_findings_issue` red (record #1's directory holds no reproduction).
11. A failed post is the command's failure: replace `gl.post_at(…)?;` with `let _ = gl.post_at(…);` → `comment_posts_oldest_first_and_a_rerun_after_a_failed_post_posts_the_rest` red (exit 0, `posted` printed).
12. An id fl does not write is never posted: delete the `partition` (keep every missing decision) → `comment_skips_a_decision_whose_id_fl_does_not_write` red (a comment with no marker is posted).
13. …and the command says it skipped one: return `Ok(0)` always → the same test red (exit 0).
13a. Recovery trusts the item's decision writers: pass `&BTreeSet::new()` to `gl.posted` → `a_comment_by_another_account_that_published_under_the_item_counts` red (the colleague's decision is posted again). The outsider half is `a_marker_someone_else_posted_does_not_stop_recovery`, which stays green under this mutation and goes red if `writers` holds every comment author.
14. Recovery renders for the repository's visibility: pass `Visibility::Private` to `render::render` → `comment_on_a_repository_that_is_not_private_recovers_no_error_detail` red (the withheld text shows in a block).

Not a guard: `comment`'s `if !gates.contains(&g)` — a gate named twice only lists its directory twice in one snapshot, and `lines` reads it the same each time; `view` picks runs by id either way.

`posted` reading every page and failing on a failed page is Task 6's guard; `comment_finds_its_markers_on_every_page` and `a_comment_page_that_cannot_be_read_posts_nothing` pin it through the command.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/cmd/ledger.rs crates/cli/src/cmd/github.rs crates/cli/tests/ledger.rs
git commit -m "feat(cli): fl github ledger comment posts every missing decision comment

It reads the item's issue where it is now, lists its comments on every
page, and posts each decision filed under the item that no comment marks,
oldest first, rendered from the ledger with a link to the commit that
holds it (spec 4.3). A second run posts nothing. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 10: The comment battery — escaping end to end, and the disclosure scan

Spec §8.3's comment items that need the whole command: a gate named with every character §4.2 names, and "a scan of every published line and comment for absolute paths, `$HOME` and the hostname". The scan covers every kind of comment — a live move and check, a recovered one, an attempt, a refused and an accepted reproduction, a gate that errors with its path in its detail — and every published line and commit message. It first proves it can see a leak: the same decisions on a private repository do show what names the machine, in comments and in ledger lines. The machine's names are read at run time (`hostname`, `$USER`, `$HOME`), never written down.

**Blast radius:** tests only.

**Files:**
- Modify: `crates/cli/tests/ledger.rs`

**Interfaces:**
- Consumes: `World`, `World::{gated, gated_as}`, `NOT_POSTED` (Task 7).
- Produces: `machine_names`, `holds_word`, `telling` (test helpers).

- [ ] **Step 1: Write the tests**

In `crates/cli/tests/ledger.rs`, after `comment_without_the_ledger_key_is_refused`, add:

```rust
// ⚠ Spec §4.2, §8.3: a gate named with a pipe, backticks, a mention, a
// reference (`#1` and `GH-1`), a comment opener and a newline is escaped
// in its comment — it notifies no one, links nothing, opens nothing,
// breaks no table.
#[test]
fn a_gate_named_with_markup_is_escaped_in_its_comment() {
    let w = World::new();
    w.gated_as("a|b `c` @someone #1 GH-1 <!-- x\ny");
    w.init();
    w.export();
    w.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    let comments = w.fake.issue(1).comments;
    assert_eq!(comments.len(), 1, "{comments:?}");
    let c = &comments[0];
    assert!(
        c.contains(
            r"| launch | a\|b \`c\` @&#8203;someone #&#8203;1 GH-&#8203;1 &lt;!-- x<br>y | PASS |"
        ),
        "{c}"
    );
    assert!(!c.contains("@someone"), "{c}");
    assert!(!c.contains("#1 "), "{c}");
    assert!(!c.contains("GH-1"), "{c}");
    assert_eq!(c.matches("<!--").count(), 1, "only the marker opens a comment: {c}");
}

/// What names this machine, read now and never written down: paths (the
/// working tree, `$HOME`), matched anywhere; and words (`$USER`, the host
/// name), matched whole and only from four letters on — a two-letter host
/// name would match ordinary text.
fn machine_names(w: &World) -> (Vec<String>, Vec<String>) {
    let home = std::env::var("HOME").expect("HOME is set");
    assert!(home.len() > 1, "a home to look for");
    let paths = vec![
        w.repo.path().display().to_string(),
        w.repo.path().canonicalize().unwrap().display().to_string(),
        home,
    ];
    let host = Sys::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let user = std::env::var("USER").unwrap_or_default();
    let words = [host, user].into_iter().filter(|n| n.len() >= 4).collect();
    (paths, words)
}

/// Whether `text` holds `word` with no letter, digit, `-` or `_` on
/// either side.
fn holds_word(text: &str, word: &str) -> bool {
    let edge = |c: Option<char>| !c.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_');
    text.match_indices(word).any(|(i, _)| {
        edge(text[..i].chars().next_back()) && edge(text[i + word.len()..].chars().next())
    })
}

/// A world whose gate prints where it runs and fails, with a second gate
/// whose program does not exist (its detail names its path), and every
/// kind of comment: a refused move and a failed check posted live; a check
/// whose comment failed, recovered from the ledger; an attempt; a
/// reproduction by the gate that errors (refused) and by the one that
/// fails (accepted).
fn telling(private: bool) -> World {
    let w = World::new();
    fs::write(
        w.repo.path().join("check.sh"),
        "#!/bin/sh\npwd\necho \"$HOME\"\necho \"$USER\"\nhostname\n[ ! -e bug ]\n",
    )
    .unwrap();
    w.gated();
    let broken = w.repo.path().join("no-such-gate");
    w.fl()
        .args([
            "gate",
            "add",
            "--project",
            "1",
            "--name",
            "broken",
            "--glob",
            "src/**/*.rs",
            "--program",
            broken.to_str().unwrap(),
        ])
        .assert()
        .success();
    w.init();
    w.export();
    if !private {
        w.fake.state().repos[0].visibility = "public".into();
    }
    fs::write(w.repo.path().join("bug"), "").unwrap();
    let decide = |args: &[&str], code: i32| {
        w.fl().args(args).assert().code(code).stderr(contains(NOT_POSTED).not());
    };
    decide(&["record", "move", "1", "--to", "doing"], 1);
    decide(&["check", "launch", "--project", "1", "--record", "1"], 1);
    w.fake.state().fail_comment_next = true;
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .code(1)
        .stderr(contains(NOT_POSTED));
    w.fl()
        .args(["github", "ledger", "comment", "1"])
        .assert()
        .success()
        .stdout(contains("comments\t1 posted"));
    decide(&["attempt", "1", "--budget-usd-micros", "0"], 1);
    w.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "a bug", "--by", "reviewer",
        ])
        .assert()
        .success();
    decide(&["finding", "reproduce", "2", "--gate", "2"], 2);
    decide(&["finding", "reproduce", "2", "--gate", "1"], 0);
    w
}

// ⚠ Decision 2, spec §8.3: on a repository that is not private, no
// comment, published line or commit message names this machine. The same
// decisions on a private repository do — in comments and in ledger lines
// — so the scan cannot pass by looking at nothing.
#[test]
fn comments_and_lines_on_a_repository_that_is_not_private_hold_no_path_home_or_host_name() {
    let private = telling(true);
    let (paths, _) = machine_names(&private);
    let tree = &paths[..2];
    let shown = [private.fake.issue(1).comments, private.fake.issue(2).comments].concat();
    assert_eq!(shown.len(), 6, "every kind of comment: {shown:?}");
    let shown = shown.join("\n");
    assert!(tree.iter().any(|t| shown.contains(t.as_str())), "a private comment shows it: {shown}");
    let lines: String = private.fake.ledger_files().into_values().collect();
    assert!(tree.iter().any(|t| lines.contains(t.as_str())), "a private line holds it");

    let public = telling(false);
    let mut published = [public.fake.issue(1).comments, public.fake.issue(2).comments].concat();
    assert_eq!(published.len(), 6, "{published:?}");
    published.extend(public.fake.ledger_files().into_values());
    published.extend(public.fake.ledger_commit_messages());
    let (paths, words) = machine_names(&public);
    for text in &published {
        for p in &paths {
            assert!(!text.contains(p.as_str()), "a path is published: {text}");
        }
        for word in &words {
            assert!(!holds_word(text, word), "a machine's name is published: {text}");
        }
    }
}
```

- [ ] **Step 2: Run them**

Run: `cargo test -p fl-cli --test ledger -- markup not_private`
Expected: PASS — Tasks 3, 7 and 9 built what they check. This task's deliverable is the battery; Step 3 shows each test can fail.

- [ ] **Step 3: Show each test sees what it guards**

Each reverted after it is seen red.

1. Decision 2's projection and the render arm together: in `crates/github/src/ledger/disclose.rs` drop `out.output_excerpt = None;` from `run` **and** make `render`'s `Visibility::NotPrivate` arm call `blocks(view)` → `cargo test -p fl-cli --test ledger -- comments_and_lines_on_a_repository_that_is_not_private_hold_no_path_home_or_host_name` red. The render arm alone is caught by Task 7's `a_comment_on_a_repository_that_is_not_private_shows_no_error_detail`, not by this scan: the projection leaves nothing machine-specific for a block to show.
2. The projection alone: drop `out.output_excerpt = None;` from `disclose::run` → the scan red (a published ledger line names the working tree).
3. The error detail's projection: drop the `Verdict::Error` replacement in `disclose::run` → the scan red (the broken gate's path is published).
4. In `push_escaped`, delete the `'@'` arm → `cargo test -p fl-cli --test ledger -- a_gate_named_with_markup_is_escaped_in_its_comment` red. Repeat for `'#'`, the `GH-` arm, `'<'`, the markdown `'\n'` arm and the backslash set — each red.
5. The excerpt title's escape: in `blocks`, write the run's title unescaped → the same markup test red (the summary carries `@someone`).

- [ ] **Step 4: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/tests/ledger.rs
git commit -m "test(cli): the comment battery: escaping end to end, and the disclosure scan

A gate named with a pipe, backticks, a mention, #1, GH-1, a comment
opener and a newline is escaped in its comment. On a repository that is
not private no comment of any kind, ledger line or commit message names
the working tree, \$HOME, \$USER or the host name; the same decisions on a
private repository do, in comments and in lines, so the scan sees a leak
when there is one (spec 8.3).

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 11: The ledger's live tests on the private repository

Spec §8.4's private-repository half, every *Modelled* name the ledger's code cites, and B1 requirement 7's shapes that a private repository can show. Ignored by default; run by hand against the private throwaway `FL_GITHUB_LIVE_REPO` names. Each is safe to re-run (ruling 15): a fresh memory per test, fresh UUIDv7 ids per entry, and a branch `fl-live/root` that a later run reads instead of walking the history.

What each test leaves behind, for good: appended commits on `fl/ledger`; the branch `fl-live/root`; an unreferenced commit (`an_unrelated_history_is_read_as_a_rewrite`); and two hand commits — an edited line and an unreadable one, each in a directory of its own fresh gate — from `a_hand_edit_is_detected_and_named`. Readers never meet those directories again; `fl github ledger verify` on this repository reports the first hand commit, as it should.

**Blast radius:** `crates/github/tests/live.rs` only. `client()` becomes `client_for(&repo())` over a shared `credentials_for`; the tracker's three live tests are unchanged.

**Files:**
- Modify: `crates/github/tests/live.rs` (module doc; `credentials_for`, `client_for`; `Live`; helpers; nine tests)

**Interfaces:**
- Consumes: `GithubLedger::{new, with_lag, init, mode, check_head, check_format, runs, publish}`; `InitOutcome`, `Mode`; `layout::{dir, segment_path, Area, BRANCH, Line, SEGMENT_LIMIT}`; `Client::{send, get_all, graphql, graphql_answer, identity}`.
- Produces, in `live.rs` (Task 12 uses them): `fn credentials_for(repo: &str) -> Box<dyn Credentials>`, `fn client_for(repo: &str) -> Client`, `fn fresh() -> Iri`, `fn now() -> At`, `const APPEND: &str`, `const ROOT_BRANCH: &str = "fl-live/root"`, `struct Live { client, repo, local }` with `on`, `private`, `ledger`, `path`, `head_of`, `head`, `record`, `by`, `set_up`, `hand_commit`, `created`; `fn run_on(gate, record, excerpt) -> GateRun`; `fn batch(record, runs) -> Batch`. (`Live::public` comes with its tests in Task 12: a helper no test calls fails clippy.)

- [ ] **Step 1: Write the module doc, the helpers and the tests**

In `crates/github/tests/live.rs`, replace the module doc with:

```rust
//! Against GitHub itself (GitHub tracker spec §8.3; GitHub ledger spec
//! §8.4). Ignored by default.
//!
//! Run only against THROWAWAY repositories. The tracker's tests create
//! issues and never delete them. The ledger's append to `fl/ledger`, leave
//! a branch `fl-live/root` at its first commit, and delete nothing: a
//! ledger under a ruleset cannot be deleted, so every test is safe to run
//! again on what earlier runs left.
//!
//! - `FL_GITHUB_LIVE_REPO`: a private repository (the tracker's tests, and
//!   most of the ledger's).
//! - `FL_GITHUB_LIVE_PUBLIC_REPO`: a public repository holding only test
//!   data and one commit, with an active ruleset on `refs/heads/fl/ledger`
//!   holding `non_fast_forward` and `deletion` (`fl github ledger init`
//!   prints the command that adds it), which the credential cannot bypass:
//!   the force-update test reads the ruleset and refuses to write unless
//!   GitHub says the credential's bypass is `never`.
//! - `FL_GITHUB_LIVE_EMPTY_REPO`: a private repository with no commit at
//!   all.
//! - `FL_GITHUB_LIVE_READ_ONLY_TOKEN`: a fine-grained token on
//!   `FL_GITHUB_LIVE_REPO` only, with Contents: read and Metadata: read.
//!
//! A ledger test whose variable is unset skips, saying which; the tracker's
//! tests still fail without `FL_GITHUB_LIVE_REPO`.
//!
//! Export each token in your shell first, from a secret store (never typed
//! inline, where shell history keeps it), then:
//!
//!   FL_GITHUB_LIVE_REPO=owner/repo \
//!     cargo test -p fl-github --test live -- --ignored --nocapture --test-threads=1
//!
//! The token is read from FL_GITHUB_TOKEN, then GITHUB_TOKEN. For the App
//! instead, set BOTH FL_GITHUB_APP_ID and FL_GITHUB_APP_KEY (the path of its
//! private key file); one without the other is refused, never a fallback to
//! the token. No test prints a token, a header or a client.
```

Replace `fn client() -> Client { … }` with:

```rust
/// The credential for `repo`: the App when both of its variables are set,
/// else the token.
fn credentials_for(repo: &str) -> Box<dyn Credentials> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    match (var("FL_GITHUB_APP_ID"), var("FL_GITHUB_APP_KEY")) {
        (Some(id), Some(key)) => Box::new(
            AppCredentials::from_file(
                DEFAULT_API,
                id.parse()
                    .expect("FL_GITHUB_APP_ID must be the App's numeric id"),
                key.as_ref(),
                repo,
            )
            .expect("the App credential"),
        ),
        // ⚠ Half an App is refused, never a silent fallback to the token:
        // the run would write as someone other than the one meant.
        (Some(_), None) => panic!(
            "FL_GITHUB_APP_ID is set but FL_GITHUB_APP_KEY is not: set both to write as the \
             App, or neither to use the token"
        ),
        (None, Some(_)) => panic!(
            "FL_GITHUB_APP_KEY is set but FL_GITHUB_APP_ID is not: set both to write as the \
             App, or neither to use the token"
        ),
        (None, None) => Box::new(EnvToken::from_env().expect("FL_GITHUB_TOKEN or GITHUB_TOKEN")),
    }
}

fn client_for(repo: &str) -> Client {
    Client::new(DEFAULT_API, credentials_for(repo))
}

fn client() -> Client {
    client_for(&repo())
}
```

Extend the imports with:

```rust
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fl_core::LedgerFault;
use fl_core::at::At;
use fl_core::decision::{Decision, Outcome, TransitionOutcome};
use fl_core::ids::{GateId, RecordId};
use fl_core::log::GateRun;
use fl_core::split::{Batch, RemoteLedger};
use fl_core::store::Bindings;
use fl_core::verdict::Verdict;
use fl_github::ledger::layout::{self, Area, BRANCH, Line, SEGMENT_LIMIT};
use fl_github::ledger::{InitOutcome, Mode};
use fl_github::{GithubLedger, Repo};
use std::sync::Barrier;
use std::time::{SystemTime, UNIX_EPOCH};
```

After `fn project()`, add:

```rust
/// A fresh id: UUIDv7, so a re-run never meets its own earlier entries.
fn fresh() -> Iri {
    Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7())).unwrap()
}

fn now() -> At {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_millis();
    At::from_unix_millis(ms as u64)
}

/// `createCommitOnBranch`, as fl sends it.
const APPEND: &str = "mutation ledgerAppend($input: CreateCommitOnBranchInput!) { \
    createCommitOnBranch(input: $input) { commit { oid } } }";

/// Where the first run on a repository leaves the ledger's first commit.
const ROOT_BRANCH: &str = "fl-live/root";

/// Whether `var` is unset: then the test skips, saying which variable it
/// needs. A resource the owner has not provided is no failure of fl's.
fn unset(var: &str) -> bool {
    let missing = std::env::var(var).map_or(true, |v| v.trim().is_empty());
    if missing {
        eprintln!("skipped: set {var} to run this live test");
    }
    missing
}

/// One live repository, a fresh machine's memory of its ledger, and a
/// record of its own for entries to be tied to.
struct Live {
    client: Client,
    repo: Repo,
    local: MemStore,
    record: RecordId,
}

impl Live {
    /// The repository `var` names, checked to be private, or not, as
    /// `private` says.
    fn on(var: &str, private: bool) -> Live {
        let name = std::env::var(var)
            .unwrap_or_else(|_| panic!("set {var}=owner/repo to run this live test"));
        let client = client_for(&name);
        let r = client
            .send(Method::Get, &format!("/repos/{name}"), None)
            .expect("read the live repository");
        assert_eq!(
            r.status, 200,
            "GitHub answered {} when the live test read `{name}` ({var})",
            r.status
        );
        let is_private = r.body["visibility"].as_str() == Some("private");
        let want = if private { "private" } else { "public" };
        assert_eq!(is_private, private, "`{name}` ({var}) must be {want}");
        let text = |k: &str| {
            r.body[k]
                .as_str()
                .unwrap_or_else(|| panic!("`{name}` has no `{k}`"))
                .to_string()
        };
        let repo = Repo {
            full_name: text("full_name"),
            node_id: text("node_id"),
        };
        // A record of its own, so a re-run never reads the decisions the
        // runs before it filed; the issue need not exist for the ledger.
        let n = uuid::Uuid::now_v7().as_u128() as u64 % 1_000_000_000 + 1_000_000;
        let record = RecordId(
            Iri::parse(&format!("https://github.com/{}/issues/{n}", repo.full_name)).unwrap(),
        );
        Live {
            client,
            repo,
            local: MemStore::default(),
            record,
        }
    }

    /// The private throwaway, `FL_GITHUB_LIVE_REPO`.
    fn private() -> Live {
        Live::on("FL_GITHUB_LIVE_REPO", true)
    }

    /// GitHub's replicas can lag a write: read again five times, a second
    /// apart, before a lag counts.
    fn ledger(&self) -> GithubLedger<'_> {
        GithubLedger::new(&self.client, self.repo.clone(), &self.local)
            .with_lag(5, Duration::from_secs(1))
    }

    fn path(&self, rest: &str) -> String {
        format!("/repos/{}{rest}", self.repo.full_name)
    }

    /// The commit `branch` points at, if the branch exists.
    fn head_of(&self, branch: &str) -> Option<String> {
        let r = self
            .client
            .send(Method::Get, &self.path(&format!("/git/ref/heads/{branch}")), None)
            .expect("read a branch");
        match r.status {
            200 => Some(r.body["object"]["sha"].as_str().expect("a commit").to_string()),
            404 => None,
            s => panic!("GitHub answered {s} for the branch `{branch}`"),
        }
    }

    fn head(&self) -> String {
        self.head_of(BRANCH).expect("the ledger's branch")
    }

    /// This `Live`'s record, for entries to be tied to.
    fn record(&self) -> RecordId {
        self.record.clone()
    }

    fn by(&self) -> String {
        self.client.identity().expect("who the credential is")
    }

    /// The ledger, set up and recorded on this machine with a cut-over.
    ///
    /// ⚠ Safe to run again, and never a walk of the history: the first run
    /// on a repository leaves `fl-live/root` at the ledger's first commit,
    /// and every later run records that commit, as an imported manifest
    /// would, and runs `init` through its "already set up" path.
    fn set_up(&self) -> String {
        let l = self.ledger();
        if let Some(root) = self.head_of(ROOT_BRANCH) {
            self.local.set_ledger_root(&self.repo.node_id, &root).unwrap();
            match l.init(&fresh(), None).expect("init over the recorded root") {
                InitOutcome::AlreadySetUp { root: r, .. } => assert_eq!(r, root),
                other => panic!("expected the ledger set up, got {other:?}"),
            }
            return root;
        }
        let root = match l.init(&fresh(), None).expect("init") {
            InitOutcome::Created { root } => root,
            InitOutcome::Confirm { root } => {
                match l.init(&fresh(), Some(&root)).expect("confirm") {
                    InitOutcome::Adopted { root } => root,
                    other => panic!("expected the ledger adopted, got {other:?}"),
                }
            }
            other => panic!("a machine with no root got {other:?}"),
        };
        let made = self
            .client
            .send(
                Method::Post,
                &self.path("/git/refs"),
                Some(&json!({"ref": format!("refs/heads/{ROOT_BRANCH}"), "sha": root})),
            )
            .expect("create the root's branch");
        assert_eq!(made.status, 201, "{:?}", made.body);
        root
    }

    /// One commit on the ledger writing `text` at `path`, as anyone with
    /// write access can. Returns it.
    fn hand_commit(&self, path: &str, text: &str) -> String {
        let answer = self
            .client
            .graphql_answer(
                APPEND,
                json!({"input": {
                    "branch": {
                        "repositoryNameWithOwner": self.repo.full_name,
                        "branchName": BRANCH,
                    },
                    "message": {"headline": "fl live test: a hand edit"},
                    "expectedHeadOid": self.head(),
                    "fileChanges": {"additions": [
                        {"path": path, "contents": STANDARD.encode(text)},
                    ]},
                }}),
            )
            .expect("an answer");
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        answer
            .data
            .as_ref()
            .and_then(|d| d.pointer("/createCommitOnBranch/commit/oid"))
            .and_then(Value::as_str)
            .expect("the commit")
            .to_string()
    }

    /// The id of what a POST to `rest` created.
    fn created(&self, rest: &str, body: Value) -> String {
        let r = self
            .client
            .send(Method::Post, &self.path(rest), Some(&body))
            .expect("an answer");
        assert_eq!(r.status, 201, "{:?}", r.body);
        r.body["sha"].as_str().expect("an id").to_string()
    }
}

/// A run of `gate` tied to `record`, stamped now, with `excerpt`.
fn run_on(gate: &GateId, record: &RecordId, excerpt: &str) -> GateRun {
    GateRun {
        id: Some(fresh()),
        at: Some(now()),
        gate: gate.clone(),
        record: Some(record.clone()),
        commit: "live".into(),
        verdict: Verdict::from_predicate(true, 1),
        population: 1,
        output_excerpt: Some(excerpt.into()),
        duration_ms: 1,
        cost_usd_micros: 0,
    }
}

/// A `check` about `record`, resting on `runs`, published with them.
fn batch(record: &RecordId, runs: Vec<GateRun>) -> Batch {
    let rests_on = runs.iter().filter_map(|r| r.id.clone()).collect();
    Batch {
        decision: Decision {
            id: fresh(),
            at: now(),
            record: record.clone(),
            finding: None,
            outcome: Outcome::Check {
                transition: TransitionOutcome {
                    transition: "live".into(),
                    passed: true,
                },
            },
            rests_on,
        },
        runs,
        attempts: vec![],
    }
}
```

Then the tests, at the end of the file:

```rust
/// ⚠ Spec §6.1: `init`'s first commit holds `format` and `README.md` and
/// no parent; a second `fl/ledger` is refused with 422 "Reference already
/// exists" (modelled in `create_branch`); and a machine that records the
/// root records its own cut-over once.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn init_sets_up_a_ledger_on_a_private_repository() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    let root = live.set_up();
    let commit = live
        .client
        .send(Method::Get, &live.path(&format!("/git/commits/{root}")), None)
        .unwrap();
    assert_eq!(commit.body["parents"], json!([]), "an orphan");
    let tree = live
        .client
        .send(Method::Get, &live.path(&format!("/git/trees/{root}")), None)
        .unwrap();
    let paths: Vec<&str> = tree.body["tree"]
        .as_array()
        .expect("the tree")
        .iter()
        .filter_map(|e| e["path"].as_str())
        .collect();
    assert_eq!(paths, vec!["README.md", "format"], "and no `.github/`");
    let again = live.client.send(
        Method::Post,
        &live.path("/git/refs"),
        Some(&json!({"ref": format!("refs/heads/{BRANCH}"), "sha": root})),
    );
    let err = again.expect_err("a second fl/ledger is refused").to_string();
    assert!(err.contains("already exists"), "{err}");
    let other = Live::private();
    other.local.set_ledger_root(&other.repo.node_id, &root).unwrap();
    assert!(matches!(
        other.ledger().init(&fresh(), None).unwrap(),
        InitOutcome::AlreadySetUp { cutover_recorded: true, .. }
    ));
    assert!(matches!(
        other.ledger().init(&fresh(), None).unwrap(),
        InitOutcome::AlreadySetUp { cutover_recorded: false, .. }
    ));
}

/// ⚠ Spec §3.2, §8.4: two machines appending at once both land; neither
/// entry is lost or written twice.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn two_flushes_racing_both_land() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    Live::private().set_up();
    let gate = GateId(fresh());
    let barrier = Barrier::new(2);
    let ids: Vec<Iri> = std::thread::scope(|s| {
        let racers: Vec<_> = (0..2)
            .map(|i| {
                let (gate, barrier) = (&gate, &barrier);
                s.spawn(move || {
                    let live = Live::private();
                    live.set_up();
                    let run = run_on(gate, &live.record(), &format!("racer {i}"));
                    let id = run.id.clone().unwrap();
                    barrier.wait();
                    live.ledger()
                        .publish(&batch(&live.record(), vec![run]))
                        .expect("each flush lands");
                    id
                })
            })
            .collect();
        racers.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let reader = Live::private();
    reader.set_up();
    let back: Vec<Option<Iri>> = reader
        .ledger()
        .runs(&gate)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(back.len(), 2, "{back:?}");
    for id in &ids {
        assert_eq!(back.iter().filter(|b| b.as_ref() == Some(id)).count(), 1, "{id}");
    }
}

/// ⚠ Confirms what `judge` and the fake take: a stale `expectedHeadOid` is refused
/// with `STALE_DATA`, or a message saying where the branch was expected to
/// point, and nothing lands.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn create_commit_on_branch_is_refused_when_the_head_moved() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let l = live.ledger();
    let before = l.check_format().unwrap();
    let record = live.record();
    l.publish(&batch(&record, vec![run_on(&GateId(fresh()), &record, "moves the head")]))
        .unwrap();
    let after = live.head();
    assert_ne!(before, after);
    let probe = run_on(&GateId(fresh()), &record, "a stale append");
    let path = layout::segment_path(&layout::dir(Area::Runs, probe.gate.iri()), 1);
    let text = format!("{}\n", Line::Run(probe).encode(&live.by()));
    let answer = live
        .client
        .graphql_answer(
            APPEND,
            json!({"input": {
                "branch": {
                    "repositoryNameWithOwner": live.repo.full_name,
                    "branchName": BRANCH,
                },
                "message": {"headline": "fl live test: a stale append"},
                "expectedHeadOid": before,
                "fileChanges": {"additions": [{"path": path, "contents": STANDARD.encode(text)}]},
            }}),
        )
        .expect("an answer");
    println!("a stale append: status {}, errors {:?}", answer.status, answer.errors);
    assert_eq!(answer.status, 200);
    let moved = answer.errors.iter().any(|e| {
        e["type"] == "STALE_DATA"
            || e["message"]
                .as_str()
                .is_some_and(|m| m.contains("Expected branch to point to"))
    });
    assert!(moved, "refused as a moved head: {:?}", answer.errors);
    assert_eq!(live.head(), after, "nothing landed");
}

/// ⚠ Spec §3.5: an edit of a line this machine read is caught (check 4),
/// naming the file and the commit; a line fl cannot read is named with the
/// commit that added it — GitHub's blame (modelled in `git.rs`); and a
/// compare asked one commit per page still says `ahead` across many
/// (modelled in `compare`).
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_hand_edit_is_detected_and_named() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    let root = live.set_up();
    let l = live.ledger();
    let record = live.record();

    let gate = GateId(fresh());
    let run = run_on(&gate, &record, "as published");
    l.publish(&batch(&record, vec![run.clone()])).expect("published");
    assert_eq!(l.runs(&gate).expect("read, and cached").len(), 1);
    let seg = layout::segment_path(&layout::dir(Area::Runs, gate.iri()), 1);
    let mut edited = run;
    edited.output_excerpt = Some("edited by hand".into());
    let edit = live.hand_commit(&seg, &format!("{}\n", Line::Run(edited).encode(&live.by())));
    let err = l.runs(&gate).expect_err("an edited line is caught").to_string();
    assert!(err.contains(&format!("`{seg}`")) && err.contains(&edit), "{err}");

    let other = GateId(fresh());
    let seg2 = layout::segment_path(&layout::dir(Area::Runs, other.iri()), 1);
    let added = live.hand_commit(&seg2, "not a line fl wrote\n");
    match l.runs(&other) {
        Err(StoreError::Ledger(LedgerFault::Unreadable {
            file, line, commit, ..
        })) => assert_eq!((file, line, commit), (seg2, 1, added)),
        got => panic!("expected an unreadable line named by its commit, got {got:?}"),
    }

    let head = live.head();
    let r = live
        .client
        .send(
            Method::Get,
            &live.path(&format!("/compare/{root}...{head}?per_page=1")),
            None,
        )
        .expect("a compare");
    assert_eq!(r.body["status"], "ahead", "{}", r.body["status"]);
    assert!(r.body["total_commits"].as_u64().unwrap_or(0) > 1, "{}", r.body["total_commits"]);
    assert!(r.body["commits"].as_array().map_or(0, Vec::len) <= 1);
    let machine = Live::private();
    machine.set_up();
    assert_eq!(machine.ledger().check_head().expect("checked from the root"), head);
}

/// ⚠ Confirms what `git.rs` takes: GraphQL sends `TreeEntry.mode` as an Int whose
/// value is the octal mode; REST sends it as a string.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn tree_entry_modes_are_integers() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    let root = live.set_up();
    let (owner, name) = live.repo.full_name.split_once('/').unwrap();
    let data = live
        .client
        .graphql(
            "query($owner: String!, $name: String!, $e: String!) { repository(owner: $owner, \
             name: $name) { object(expression: $e) { ... on Tree { entries { name mode } } } } }",
            json!({"owner": owner, "name": name, "e": format!("{root}:")}),
        )
        .unwrap();
    let entries = data["repository"]["object"]["entries"]
        .as_array()
        .expect("the first commit's tree");
    let format = entries.iter().find(|e| e["name"] == "format").expect("`format`");
    assert_eq!(format["mode"], json!(0o100644), "{format}");
    let rest = live
        .client
        .send(Method::Get, &live.path(&format!("/git/trees/{root}")), None)
        .unwrap();
    let listed = rest.body["tree"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "format")
        .cloned()
        .expect("`format`");
    assert_eq!(listed["mode"], json!("100644"), "{listed}");
}

/// ⚠ Measured on 2026-10-02 and pinned here: a private repository on
/// GitHub Free answers `rules/branches/fl/ledger` with `200 []`, which
/// `mode()` reads as detection-only (decision 12).
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_private_repository_without_a_ruleset_is_detection_only() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    let r = live
        .client
        .send(Method::Get, &live.path(&format!("/rules/branches/{BRANCH}?per_page=100")), None)
        .expect("the rules are readable");
    assert_eq!((r.status, &r.body), (200, &json!([])), "{:?}", r.body);
    let mode = live.ledger().mode().unwrap();
    assert!(matches!(mode, Mode::DetectionOnly { .. }), "{mode:?}");
}

/// ⚠ Confirms what `branches_under` takes: `git/matching-refs/heads/<prefix>`
/// lists every branch whose name starts with the prefix, and `200 []`
/// when none does. A branch under `fl/ledger/` cannot sit beside
/// `fl/ledger`, so the listing is checked on `fl-live/`.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_branch_under_the_ledger_branch_is_found() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let under = |prefix: &str| -> Vec<String> {
        live.client
            .get_all(&live.path(&format!("/git/matching-refs/heads/{prefix}")))
            .expect("matching-refs")
            .iter()
            .map(|r| r["ref"].as_str().expect("a ref").to_string())
            .collect()
    };
    let found = under("fl-live/");
    assert!(found.contains(&format!("refs/heads/{ROOT_BRANCH}")), "{found:?}");
    assert!(found.iter().all(|r| r.starts_with("refs/heads/fl-live/")), "{found:?}");
    assert_eq!(under("fl/ledger/"), Vec::<String>::new());
}

/// Spec §3.5 check 1: a head whose history is not the anchor's is read
/// as a rewrite, whatever GitHub answers a compare of unrelated histories
/// (printed).
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn an_unrelated_history_is_read_as_a_rewrite() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let tree = live.created(
        "/git/trees",
        json!({"tree": [{"path": "probe", "mode": "100644", "type": "blob", "content": "unrelated\n"}]}),
    );
    let stray = live.created(
        "/git/commits",
        json!({"message": "fl live test: an unrelated history", "tree": tree, "parents": []}),
    );
    let head = live.head();
    let raw = live.client.send(
        Method::Get,
        &live.path(&format!("/compare/{stray}...{head}?per_page=1")),
        None,
    );
    match &raw {
        Ok(r) => println!("a compare of unrelated histories: {} {}", r.status, r.body),
        Err(e) => println!("a compare of unrelated histories: {e}"),
    }
    let other = Live::private();
    other.local.set_ledger_root(&other.repo.node_id, &stray).unwrap();
    let err = other.ledger().check_head().unwrap_err();
    assert!(
        matches!(err, StoreError::Ledger(LedgerFault::Rewritten { .. })),
        "{err:?}"
    );
}

/// Spec §3.1: a segment filled near its 256 KB limit lands through
/// `createCommitOnBranch`, and the next lines roll over to a second.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_near_full_segment_lands_through_create_commit_on_branch() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let l = live.ledger();
    let by = live.by();
    let record = live.record();
    let gate = GateId(fresh());
    let excerpt = "x".repeat(4_000);
    let (mut first, mut bytes) = (Vec::new(), 0usize);
    loop {
        let r = run_on(&gate, &record, &excerpt);
        let len = Line::Run(r.clone()).encode(&by).len() + 1;
        if bytes + len > SEGMENT_LIMIT - 8 * 1024 {
            break;
        }
        bytes += len;
        first.push(r);
    }
    assert!(bytes > SEGMENT_LIMIT - 16 * 1024, "near full: {bytes}");
    l.publish(&batch(&record, first.clone())).expect("a near-full segment lands");
    let second: Vec<GateRun> = (0..4).map(|_| run_on(&gate, &record, &excerpt)).collect();
    l.publish(&batch(&record, second.clone())).expect("the rollover lands");
    assert_eq!(l.runs(&gate).unwrap().len(), first.len() + second.len());
    let dir = layout::dir(Area::Runs, gate.iri());
    let listing = live
        .client
        .send(Method::Get, &live.path(&format!("/contents/{dir}?ref=fl%2Fledger")), None)
        .unwrap();
    let names: Vec<&str> = listing
        .body
        .as_array()
        .expect("the directory")
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert_eq!(names, vec!["1.jsonl", "2.jsonl"]);
}
```

- [ ] **Step 2: Compile and list them**

Run: `cargo test -p fl-github --test live -- --list`
Expected: the tracker's three tests and these nine, each `: test`.

Run: `cargo test -p fl-github --test live`
Expected: `0 passed; 0 failed; 12 ignored`. Nothing reaches the network.

- [ ] **Step 3: Check every *Modelled* name has its test**

Run (it reads comments joined across lines, so a name wrapped onto the next comment line is found, and it floors the result at the eleven names this plan relies on):

```bash
python3 - <<'EOF'
import pathlib, re, subprocess

def flat(text):
    # Comments joined across lines, so a name wrapped onto the next
    # comment line is found.
    return re.sub(r"\s+", " ", re.sub(r"\n\s*//[/!]?", " ", text))

cited = set()
for p in sorted(pathlib.Path("crates").rglob("*.rs")):
    cited |= set(re.findall(r"live test `([a-z0-9_]+)`", flat(p.read_text())))
live = pathlib.Path("crates/github/tests/live.rs").read_text()
missing = sorted(n for n in cited if f"fn {n}()" not in live)
expected = {
    "init_sets_up_a_ledger_on_a_private_repository",
    "create_commit_on_branch_is_refused_when_the_head_moved",
    "create_commit_on_branch_without_contents_write_is_refused",
    "a_hand_edit_is_detected_and_named",
    "rules_on_the_ledger_branch_are_readable",
    "a_private_repository_without_a_ruleset_is_detection_only",
    "tree_entry_modes_are_integers",
    "a_branch_under_the_ledger_branch_is_found",
    "an_empty_repository_is_refused_naming_a_first_commit",
    "a_decision_comment_round_trips_with_its_marker",
    "the_edit_history_and_timeline_counts_match_fls_model",
}
print(f"{len(cited)} names cited:", *sorted(cited), sep="\n  ")
print("cited with no live test:", missing)
# The floor: the check must find every name this plan relies on.
assert expected <= cited, f"not found by the check: {sorted(expected - cited)}"

# Every Modelled marker this branch adds names a live test — but one: the
# GraphQL timeout in `judge`, modelled from GitHub's documented error shape.
diff = subprocess.run(["git", "diff", "origin/main...HEAD", "-U0", "--", "*.rs"],
                      capture_output=True, text=True, check=True).stdout
added = " ".join(l[1:] for l in diff.splitlines() if l.startswith("+") and not l.startswith("+++"))
added = re.sub(r"\s+", " ", re.sub(r"\s*//[/!]?\s*", " ", added))
bare = [m[:160] for m in re.split(r"(?=Modelled)", added)[1:] if "live test `" not in m[:600]]
print("Modelled markers added with no live test:", bare)
assert len(bare) == 1 and "documented error shape" in bare[0], bare
assert not missing, f"cited with no live test: {missing}"
EOF
```

Expected here, before Task 12: every assertion passes but the last, which fails naming exactly the four names Task 12 adds — `an_empty_repository_is_refused_naming_a_first_commit`, `a_decision_comment_round_trips_with_its_marker`, `create_commit_on_branch_without_contents_write_is_refused` and `rules_on_the_ledger_branch_are_readable`. Any other name, or a floor or *Modelled* failure, is a defect to fix now. The printed list shows the wrapped names (`create_commit_on_branch_without_contents_write_is_refused` in `fake.rs` and `fake_git.rs`, `the_edit_history_and_timeline_counts_match_fls_model` in `fake.rs`, and the wrapped markers this plan adds), which a one-line `grep` missed.

- [ ] **Step 4: Run the trio and commit**

No product code changes, so no mutation step: these tests run only by hand.

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/tests/live.rs
git commit -m "test(github): the ledger's live tests on the private repository

init, two racing flushes, a stale createCommitOnBranch, a hand edit and an
unreadable line named by blame, a compare one commit per page, TreeEntry
modes, Free's 200 [] for rules, matching-refs, an unrelated history read
as a rewrite, and a near-full segment (spec 8.4, B1 requirement 7). Each
is ignored by default and safe to re-run.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 12: The live tests of comments, a read-only token, an empty repository and the public ruleset

Spec §8.4's comment round trip and its public-repository half, plus B1 requirement 7's read-only token and empty repository. Each needs a resource the owner provides (owner, 2026-10-03): `FL_GITHUB_LIVE_PUBLIC_REPO` (public, test data, one commit, an active ruleset on `refs/heads/fl/ledger` with `non_fast_forward` and `deletion`), `FL_GITHUB_LIVE_EMPTY_REPO` (private, no commit), `FL_GITHUB_LIVE_READ_ONLY_TOKEN` (fine-grained, the private throwaway only, Contents: read and Metadata: read). A test whose variable is unset skips, printing which variable is missing (ruling 24).

**Blast radius:** `crates/github/tests/live.rs` only.

**Files:**
- Modify: `crates/github/tests/live.rs`

**Interfaces:**
- Consumes: Task 11's `Live`, `unset`, `client_for`, `credentials_for`, `fresh`, `now`, `run_on`, `batch`; the tracker tests' `tracker()` and `project()`; `GithubLedger::{post_comment, issue_at, posted, mode, init, publish}`; `render::{render, marked, DecisionView, RunRow}`; `meta::parse_issue_url`; `ruleset_command`; `Visibility`.
- Produces: `Live::public`; five tests.

- [ ] **Step 1: Write the tests**

Extend the imports of `crates/github/tests/live.rs` with:

```rust
use fl_github::ledger::render::{self, DecisionView, RunRow};
use fl_github::ledger::{Visibility, ruleset_command};
use std::collections::BTreeSet;
```

and add at the end of the file:

```rust
impl Live {
    /// The public throwaway with a ruleset on `fl/ledger`,
    /// `FL_GITHUB_LIVE_PUBLIC_REPO`.
    fn public() -> Live {
        Live::on("FL_GITHUB_LIVE_PUBLIC_REPO", false)
    }
}

/// ⚠ Spec §4.2, §4.3, §8.4: a decision comment posted on an fl record
/// keeps its marker through GitHub's storage and is found among the
/// issue's comments as fl's own; and GitHub's rendering of it makes no
/// mention and no issue link of its `@`, `#` and `GH-` text. A control
/// comment posted raw must render its `#<n>` as a link, so the check
/// cannot pass by recognising nothing.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_decision_comment_round_trips_with_its_marker() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let record = tracker()
        .add_record(&project(), "fl live test: a decision comment")
        .unwrap();
    let decision = Decision {
        id: fresh(),
        at: now(),
        record: record.clone(),
        finding: None,
        outcome: Outcome::Check {
            transition: TransitionOutcome {
                transition: "live".into(),
                passed: true,
            },
        },
        rests_on: vec![],
    };
    let (_, n) = fl_github::meta::parse_issue_url(record.iri()).expect("an issue URL");
    let view = DecisionView {
        decision: decision.clone(),
        by: live.by(),
        commit: None,
        rows: vec![RunRow {
            role: "live".into(),
            gate: format!("@fl-live-test-nobody #{n} GH-{n}"),
            run: run_on(&GateId(fresh()), &record, "an excerpt"),
        }],
        attempt: None,
        missing: vec![],
    };
    let body = render::render(
        &view,
        &live.repo.full_name,
        Visibility::Private,
        Some("A check changes no state."),
    );
    let l = live.ledger();
    l.post_comment(record.iri(), &body).expect("posted");
    let at = l.issue_at(record.iri()).expect("the issue");
    assert_eq!(at.moved_to, None);
    let posted = l.posted(&at, &BTreeSet::new()).expect("listed");
    assert!(posted.contains(&decision.id), "{posted:?}");
    assert!(!posted.contains(&fresh()));
    let listed = live
        .client
        .get_all(&format!("{}?per_page=100", at.comments))
        .expect("listed");
    let back = listed
        .iter()
        .filter_map(|c| c["body"].as_str())
        .find(|b| render::marked(b).as_ref() == Some(&decision.id))
        .expect("the comment");
    println!(
        "GitHub kept the comment {}",
        if back == body { "byte for byte" } else { "with changes" }
    );
    assert!(back.contains("@&#8203;fl-live-test-nobody"), "{back}");

    // What GitHub renders: GraphQL's `bodyHTML` (REST's `body_html` needs
    // an Accept header fl's client does not send).
    l.post_comment(record.iri(), &format!("fl live test control: #{n} GH-{n}"))
        .expect("the control posted");
    let (owner, name) = live.repo.full_name.split_once('/').unwrap();
    let data = live
        .client
        .graphql(
            "query($owner: String!, $name: String!, $n: Int!) { repository(owner: $owner, \
             name: $name) { issue(number: $n) { comments(last: 20) { nodes { body bodyHTML } \
             } } } }",
            json!({"owner": owner, "name": name, "n": n}),
        )
        .expect("the rendered comments");
    let nodes = data["repository"]["issue"]["comments"]["nodes"]
        .as_array()
        .expect("the comments")
        .clone();
    let html_of = |part: &str| -> String {
        nodes
            .iter()
            .find(|c| c["body"].as_str().is_some_and(|b| b.contains(part)))
            .and_then(|c| c["bodyHTML"].as_str())
            .unwrap_or_else(|| panic!("no rendered comment holds {part}"))
            .to_string()
    };
    let control = html_of("fl live test control");
    assert!(control.contains("issue-link"), "the control links: {control}");
    let ours = html_of(decision.id.as_str());
    println!("rendered: {ours}");
    // No raw mention is posted as a control: it could notify a real
    // account. Instead the cell must have rendered its `@` text, with the
    // zero-width space after it, so the check below looked at something;
    // if GitHub renamed `user-mention`, this test's printout shows it.
    assert!(
        ours.contains("@\u{200b}fl-live-test-nobody") || ours.contains("@&#8203;fl-live-test-nobody"),
        "the gate's cell rendered: {ours}"
    );
    assert!(!ours.contains("user-mention"), "a mention: {ours}");
    assert!(!ours.contains("issue-link"), "an issue link: {ours}");
    assert!(!ours.contains(&format!("/issues/{n}\"")), "a link to #{n}: {ours}");
}

/// ⚠ Confirms what `judge` takes: a credential without Contents: write is refused
/// — a 403 naming the permission, or a 200 with `FORBIDDEN` — and nothing
/// lands.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO, FL_GITHUB_LIVE_READ_ONLY_TOKEN and a credential"]
fn create_commit_on_branch_without_contents_write_is_refused() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    if unset("FL_GITHUB_LIVE_READ_ONLY_TOKEN") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let token = std::env::var("FL_GITHUB_LIVE_READ_ONLY_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .expect(
            "set FL_GITHUB_LIVE_READ_ONLY_TOKEN to a fine-grained token on FL_GITHUB_LIVE_REPO \
             with Contents: read only",
        );
    let reader = Client::new(
        DEFAULT_API,
        Box::new(
            EnvToken::from_lookup(move |k| (k == "FL_GITHUB_TOKEN").then(|| token.clone()))
                .unwrap(),
        ),
    );
    let l = GithubLedger::new(&reader, live.repo.clone(), &live.local)
        .with_lag(5, Duration::from_secs(1));
    let head = live.head();
    let record = live.record();
    let err = l
        .publish(&batch(&record, vec![run_on(&GateId(fresh()), &record, "refused")]))
        .expect_err("a read-only credential cannot append")
        .to_string();
    assert!(err.contains("Contents: write"), "{err}");
    assert_eq!(live.head(), head, "nothing landed");
}

/// ⚠ Confirms what `branch_head` and the fake take: a repository with no commit
/// answers a ref read 409, and `init` says to push a first commit.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_EMPTY_REPO and a credential"]
fn an_empty_repository_is_refused_naming_a_first_commit() {
    if unset("FL_GITHUB_LIVE_EMPTY_REPO") {
        return;
    }
    let name = std::env::var("FL_GITHUB_LIVE_EMPTY_REPO")
        .expect("set FL_GITHUB_LIVE_EMPTY_REPO=owner/repo: a private repository with no commit");
    let client = client_for(&name);
    // ⚠ First: the repository really has no commit, so `init` below cannot
    // create anything.
    let raw = client.send(Method::Get, &format!("/repos/{name}/git/ref/heads/fl"), None);
    let err = raw
        .expect_err("a ref read on a repository with no commit is refused")
        .to_string();
    assert!(err.contains("GitHub answered 409 ") && err.contains("Git Repository is empty"), "{err}");
    let r = client
        .send(Method::Get, &format!("/repos/{name}"), None)
        .expect("the repository");
    let repo = Repo {
        full_name: r.body["full_name"].as_str().expect("a name").into(),
        node_id: r.body["node_id"].as_str().expect("a node").into(),
    };
    let local = MemStore::default();
    let l = GithubLedger::new(&client, repo, &local);
    // `fl github ledger init` reads the mode before it calls `init`: on a
    // repository with no commit, the rules must still read, or the person
    // would see a rules error instead of "push a first commit".
    let mode = l.mode();
    println!("the mode of a repository with no commit: {mode:?}");
    assert!(mode.is_ok(), "{mode:?}");
    let err = l
        .init(&fresh(), None)
        .expect_err("init is refused")
        .to_string();
    assert!(err.contains("is empty: GitHub keeps no branch until"), "{err}");
}

/// ⚠ Confirms what `mode()` takes: the token reads `rules/branches/fl/ledger`, and
/// an active ruleset with both rules reads as protected.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_PUBLIC_REPO and a credential"]
fn rules_on_the_ledger_branch_are_readable() {
    if unset("FL_GITHUB_LIVE_PUBLIC_REPO") {
        return;
    }
    let live = Live::public();
    let mode = live.ledger().mode().expect("the rules are readable by the token");
    assert_eq!(
        mode,
        Mode::Protected,
        "`{}` needs an active ruleset on `fl/ledger` with `non_fast_forward` and `deletion`. \
         An administrator adds it with:\n{}",
        live.repo.full_name,
        ruleset_command(&live.repo.full_name)
    );
}

/// ⚠ Spec §6.2, §8.4: under the ruleset GitHub refuses a force update and a
/// deletion of `fl/ledger`, and the ledger stays as it was.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_PUBLIC_REPO and a credential"]
fn a_force_update_and_a_deletion_of_the_ledger_are_refused() {
    if unset("FL_GITHUB_LIVE_PUBLIC_REPO") {
        return;
    }
    let live = Live::public();
    // ⚠ First: without the ruleset in force, or with a credential that may
    // bypass it, what follows would succeed and destroy this repository's
    // ledger — or pass by never being refused. Nothing is written until
    // both are known.
    assert_eq!(
        live.ledger().mode().expect("the rules"),
        Mode::Protected,
        "refusing to try a force update without the ruleset in force"
    );
    let rules = live
        .client
        .get_all(&live.path(&format!("/rules/branches/{BRANCH}?per_page=100")))
        .expect("the rules");
    let mut ids: Vec<u64> = rules
        .iter()
        .map(|r| r["ruleset_id"].as_u64().expect("a ruleset id"))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert!(!ids.is_empty(), "no ruleset names fl/ledger");
    for id in ids {
        let set = live
            .client
            .send(Method::Get, &live.path(&format!("/rulesets/{id}")), None)
            .expect("the ruleset");
        let can = set.body["current_user_can_bypass"].as_str();
        assert_eq!(
            can,
            Some("never"),
            "refusing to try: the credential's bypass of ruleset {id} is {can:?} (bypass list: {})",
            set.body["bypass_actors"]
        );
    }
    let root = live.set_up();
    let record = live.record();
    live.ledger()
        .publish(&batch(&record, vec![run_on(&GateId(fresh()), &record, "public test data")]))
        .expect("an append lands under the ruleset");
    let head = live.head();
    assert_ne!(head, root);

    let forced = live.client.send(
        Method::Patch,
        &live.path(&format!("/git/refs/heads/{BRANCH}")),
        Some(&json!({"sha": root, "force": true})),
    );
    match &forced {
        Ok(r) => println!("a force update: {} {}", r.status, r.body),
        Err(e) => println!("a force update: {e}"),
    }
    assert!(
        !matches!(&forced, Ok(r) if (200..300).contains(&r.status)),
        "GitHub took a force update of fl/ledger"
    );

    // fl's client sends no DELETE; this one request goes straight through
    // ureq, with the same credential.
    let token = credentials_for(&live.repo.full_name)
        .token()
        .expect("a token");
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .build(),
    );
    let deleted = agent
        .delete(&format!(
            "{DEFAULT_API}/repos/{}/git/refs/heads/{BRANCH}",
            live.repo.full_name
        ))
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "fl-live-test")
        .call()
        .expect("an answer");
    let status = deleted.status().as_u16();
    println!("a deletion: {status}");
    assert!(!(200..300).contains(&status), "GitHub deleted fl/ledger");
    assert_eq!(live.head(), head, "the ledger is as it was");
}
```

- [ ] **Step 2: Compile and list them**

Run: `cargo test -p fl-github --test live -- --list`
Expected: the three tracker tests and fourteen ledger tests.

Run: `cargo test -p fl-github --test live`
Expected: `0 passed; 0 failed; 17 ignored`.

- [ ] **Step 3: Every *Modelled* name has its test**

Run:

```bash
python3 - <<'EOF'
import pathlib, re, subprocess

def flat(text):
    # Comments joined across lines, so a name wrapped onto the next
    # comment line is found.
    return re.sub(r"\s+", " ", re.sub(r"\n\s*//[/!]?", " ", text))

cited = set()
for p in sorted(pathlib.Path("crates").rglob("*.rs")):
    cited |= set(re.findall(r"live test `([a-z0-9_]+)`", flat(p.read_text())))
live = pathlib.Path("crates/github/tests/live.rs").read_text()
missing = sorted(n for n in cited if f"fn {n}()" not in live)
expected = {
    "init_sets_up_a_ledger_on_a_private_repository",
    "create_commit_on_branch_is_refused_when_the_head_moved",
    "create_commit_on_branch_without_contents_write_is_refused",
    "a_hand_edit_is_detected_and_named",
    "rules_on_the_ledger_branch_are_readable",
    "a_private_repository_without_a_ruleset_is_detection_only",
    "tree_entry_modes_are_integers",
    "a_branch_under_the_ledger_branch_is_found",
    "an_empty_repository_is_refused_naming_a_first_commit",
    "a_decision_comment_round_trips_with_its_marker",
    "the_edit_history_and_timeline_counts_match_fls_model",
}
print(f"{len(cited)} names cited:", *sorted(cited), sep="\n  ")
print("cited with no live test:", missing)
# The floor: the check must find every name this plan relies on.
assert expected <= cited, f"not found by the check: {sorted(expected - cited)}"

# Every Modelled marker this branch adds names a live test — but one: the
# GraphQL timeout in `judge`, modelled from GitHub's documented error shape.
diff = subprocess.run(["git", "diff", "origin/main...HEAD", "-U0", "--", "*.rs"],
                      capture_output=True, text=True, check=True).stdout
added = " ".join(l[1:] for l in diff.splitlines() if l.startswith("+") and not l.startswith("+++"))
added = re.sub(r"\s+", " ", re.sub(r"\s*//[/!]?\s*", " ", added))
bare = [m[:160] for m in re.split(r"(?=Modelled)", added)[1:] if "live test `" not in m[:600]]
print("Modelled markers added with no live test:", bare)
assert len(bare) == 1 and "documented error shape" in bare[0], bare
assert not missing, f"cited with no live test: {missing}"
EOF
```

Expected: every assertion passes; the cited list holds at least the eleven names, and the one *Modelled* marker with no live test is the timeout's.

- [ ] **Step 4: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/tests/live.rs
git commit -m "test(github): live tests of a comment round trip, a read-only token, an empty
repository and the public ruleset

A decision comment keeps its marker through GitHub, is found on its
issue as fl's own, and renders with no mention or issue link; a token
without Contents: write is refused; a repository with no commit answers
409, its rules still read, and init says to push one; on the public
throwaway, with a credential that cannot bypass the ruleset, GitHub
refuses a force update and a deletion of fl/ledger (spec 8.4). Each skips
when its variable is unset, saying which.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 13: `docs/github-ledger.md`, the cost it states, and the links (§9)

Spec §9, the docs half of B1 requirement 12, and the items parked from PR #22: `docs/github-tracker.md` says the ledger stays local (untrue in mode B) and lists no Contents permission; the real cost of a decision, pinned by a test, replaces §3.4's "about three"; the doc says `init` re-imports the manifest on an importing machine, and states ruling 13's limit from B2a. The doc states spec decision 17 — a comment that fails to post is a warning and the command keeps its exit code — which the owner added with this plan, together with §7's matching lead-in and comment row (already in the spec; this task does not edit them).

**Blast radius:** documentation, the spec's §3.4 text, and one black-box test.

**Files:**
- Create: `docs/github-ledger.md`
- Modify: `docs/README.md`, `docs/github-tracker.md`, `docs/sharing-gates.md`
- Modify: `docs/superpowers/specs/2026-09-30-github-ledger-design.md` (§3.4 only; decision 17 and §7 are already edited)
- Modify: `docs/superpowers/specs/2026-09-23-identity-and-store-roles-design.md` (§0.1's table)
- Modify: `crates/cli/tests/ledger.rs`

**Interfaces:**
- Consumes: `World`, `requests_equal` (Task 9).
- Produces: nothing in code.

- [ ] **Step 1: Write the test that pins the cost**

In `crates/cli/tests/ledger.rs`, after `comment_without_the_ledger_key_is_refused`, add:

```rust
// docs/github-ledger.md "What a decision costs", and spec §3.4: the
// requests a steady-state `check --record` makes, by kind. A count that
// changes changes the doc and §3.4 in the same commit.
#[test]
fn a_steady_state_decision_costs_what_the_docs_say() {
    let w = World::new();
    w.ready();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    w.fake.state().requests.clear();
    w.fl()
        .args(["check", "launch", "--project", "1", "--record", "1"])
        .assert()
        .success();
    let requests = w.fake.state().requests.clone();
    let containing = |part: &str| requests.iter().filter(|r| r.contains(part)).count();
    assert_eq!(
        [
            requests_equal(&w, "GET /repos/acme/widgets"),
            containing("/rules/branches/fl/ledger"),
            containing("/git/ref/heads/fl/ledger"),
            containing("/compare/"),
            requests_equal(&w, "POST /graphql"),
            containing("/git/blobs/"),
            requests_equal(&w, "GET /user"),
            requests_equal(&w, "POST /repos/acme/widgets/issues/1/comments"),
        ],
        // The tracker's own read and the visibility; the rules; the head
        // twice; no compare; the format's listing, the directories'
        // listing and the commit; the two segments the last decision grew;
        // who fl writes as; the comment.
        [2, 1, 2, 0, 3, 2, 1, 1],
        "{requests:#?}"
    );
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p fl-cli --test ledger -- a_steady_state_decision_costs_what_the_docs_say`
Expected: PASS. ⚠ If a count differs, do not change the number to make it pass: find which request the reading above missed (print `requests`), and correct this test, the doc's "What a decision costs" and §3.4 together, from what the code does. Then tell the reviewer.

Show the test can fail: in `crates/github/src/ledger/mod.rs`, drop `identity`'s `if let Some(by) = self.identity.get() { … }` early return → red (`GET /user` twice: the flush and the comment). Restore.

- [ ] **Step 3: Write `docs/github-ledger.md`**

Create `docs/github-ledger.md`:

````markdown
# Keeping the ledger in GitHub

By default every gate run and every attempt is kept in the project's local store, and only
there. A project whose records and findings live in GitHub Issues
([github-tracker.md](github-tracker.md)) can also publish the evidence of each *decision* to
its repository: an append-only branch, `fl/ledger`, that every machine working on the project
reads, and one comment per decision on the issue it concerns.

## What goes where

| what | the local store | `fl/ledger` |
|---|---|---|
| a run tied to no record: a plain `fl check`, `fl gate run` | at once | never |
| a run tied to a record: `fl record move`, `fl check --record`, `fl finding reproduce` and `verify` | at once | with its decision |
| an attempt | at once | with its decision |
| the decision itself | — | at its flush |

The local store keeps every run, whatever happens on GitHub, and a plain `fl check` stays
local and needs no network. A *decision* — a move, a check tied to a record, a reproduction, a
verification, an attempt — publishes itself and the entries it rests on in one commit on
`fl/ledger` **before** the state change it supports. If that commit cannot be made, the
decision is refused and nothing changes; the runs stay in the local store, and the next
decision that reaches the ledger publishes them. An attempt is the exception: it has already
run and cost what it cost, so a failed publish is a warning, fl exits as the attempt did, and
the next decision publishes it.

A decision publishes only entries recorded after this machine's *cut-over* — the moment
`fl github ledger init` ran here — and only those tied to issues of this repository. Earlier
entries stay local.

## Setting it up

1. Name the GitHub ledger in the project's binding:

   ```toml
   tracker = { github = "acme/widgets", credential = "env", ledger = "github" }
   ```

   `ledger` is optional, its only value is `"github"`, and any other value is refused when the
   config is read. The ledger always lives in the tracker's repository.
2. In the project's checkout, run `fl github ledger init`. It refuses a repository that holds
   a branch named `fl`, or a branch under `fl/ledger/`, which git cannot hold beside
   `fl/ledger`, and a repository with no commit at all — push a first commit, then run it
   again. Otherwise it creates `fl/ledger`, a branch of its own that shares no history with
   the code and holds only `format` and `README.md`; records the branch's first commit and
   this machine's cut-over in the local store; and prints the mode in force and what to do
   next.
3. Run `fl manifest export --project <project>` and commit `.fl/manifest.json`. The manifest
   is then format 2: it carries `ledger_root`, the ledger's first commit, which every other
   machine checks the ledger's history against. Until it is committed, every decision is
   refused, saying so.
4. On every other machine, pull, then run `fl github ledger init` there. On a machine that
   imports the manifest rather than authoring the project, `init` first imports the committed
   manifest again — as `fl manifest import` does — so it learns the ledger's first commit
   before it touches GitHub; then it records this machine's own cut-over and changes nothing
   on GitHub. Until it has run, every decision on that machine is refused, naming `init`.

`init` can be run again at any time. On a ledger already set up it says so, records this
machine's cut-over if it had none, and prints the mode. If an earlier run stopped after
creating the branch but before recording its first commit, the next run shows that commit and
asks you to confirm it: `fl github ledger init --confirm <commit>`. If this machine or the
manifest records a first commit but the branch is gone, `init` refuses: the ledger was
deleted, and a new one would hide that.

## The two modes

| ruleset on `fl/ledger` | mode | what holds |
|---|---|---|
| active, with `non_fast_forward` and `deletion` | protected | GitHub refuses a rewrite or a deletion; fl detects an edit |
| none, disabled, evaluate-only, missing a rule, or not offered by the plan | detection-only | fl detects a rewrite, a deletion or an edit; nothing prevents them |

fl's credential must not hold Administration permission, so fl cannot add the ruleset; `init`
prints the `gh api` command an administrator runs. On GitHub Free a private repository has no
rulesets, and the ledger runs detection-only — every feature works in either mode. `init` and
`fl github whoami` state the mode in force, and what is missing. Protect the default branch as
well: Contents: write lets fl's credential push to any branch.

## Permissions

The credential — the token or the App — needs **Contents: read and write**, **Issues: read and
write** and **Metadata: read**. Fine-grained tokens do not list their permissions, so a missing
write permission shows at the first publish or comment, whose error names it. Until it is
granted, every move, check and finding decision is refused, and each attempt is kept locally
with a warning.

## What is published, and the disclosure limit

Each line of the ledger is one run, attempt or decision as JSON, with who wrote it (`by`, the
GitHub identity) and no machine name. On a repository that is not private — `public` or
`internal` — nothing machine-specific is published: an output excerpt becomes `null`, an
error's detail becomes a fixed text saying only that the gate errored, and the paths an
attempt touched become a count. fl reads the repository's visibility live before every
decision, and refuses the decision if it cannot.

**Known limit.** If a private repository is later made public, every excerpt already in the
ledger's history becomes public with it, and removing one would take the history rewrite the
ledger exists to forbid. Comments can be edited; the ledger cannot.

The text you give `fl github ledger quarantine` — `--by` and `--reason` — is published as you
wrote it, permanently, whatever the visibility; on a repository that is not private the
command warns before it appends.

## Decision comments

Once its state change is done, each decision posts one comment on the issue it concerns: a
move, a `check --record` and an attempt on the record's issue; a reproduction and a
verification on the finding's issue. A refused decision gets one too, saying it was refused.
The comment shows what was decided, the outcome, who decided, a link to the ledger commit, a
table of the runs it rests on — or the attempt's adapter, status, duration, tokens and cost —
and one line saying whether the state change completed. Output excerpts appear only on a
private repository, folded in a `<details>` block. Every name is escaped, and `@`, `#` and the
`-` of `GH-` are followed by a zero-width space, so a comment does not mention anyone or link
an issue by accident. A comment holds at most 60,000 bytes: past that, excerpts are cut first,
then left out, then the table's last rows, and the comment says so — the ledger commit holds
everything. Decisions made before the GitHub ledger was switched on get no comment.

The ledger is the record and a comment is its view: a comment fl posted still counts as posted
when someone edits it, wherever the edit leaves its marker outside a code block. A comment counts only when its
author is the account fl posts as, or an account that wrote a decision under that item — so a
colleague's machine, posting under its own token, is not duplicated, and a passer-by's comment
cannot stand in for one.

If a comment cannot be posted, the decision stands, and so does any state change it made. fl
prints a warning naming the issue and the command that posts it later, and exits as the
decision did — a passing check still exits 0, and an attempt exits with its own code.

`fl github ledger comment <item>` — the record's or finding's issue number (`41` or `#41`),
`owner/repo#41`, or its URL — lists every comment on the issue, every page, and posts each
decision filed under that item whose comment is missing, oldest first, rendered from the
ledger. It prints `posted` and the decision's id for each, then how many it posted and how many
were already there. A decision whose id is not one fl writes gets no comment: the command names
it on stderr, with the quarantine command for its line, and exits 1. Run it again at any time:
a decision whose comment is there is skipped. A comment posted seconds earlier may not be
listed yet, so at worst it posts a harmless duplicate. If the issue was transferred, it prints
where to and posts there. A recovered comment has no line about the state change: the ledger
does not record whether it completed. Run it where the project's catalog is: a comment names
gates by the local catalog's names.

## What a decision costs

A decision that follows another made on this machine makes about ten requests to GitHub's
ledger, besides the tracker's own:

- four for the pre-flight, before any gate runs: the repository's visibility, the rules on
  `fl/ledger`, the branch's head, and its `format` file;
- four for the flush: the head again, one listing of the directories it appends to, who fl
  writes as (once per command), and the commit;
- one download for each segment that grew since this machine last read it — in steady state,
  each directory the previous decision appended to;
- one for the comment.

A `check --record` that follows another on the same record and gate makes eleven: four, four,
two downloads and its comment. A move adds the tracker's write and its checks
([github-tracker.md](github-tracker.md#limits-and-costs)). A head another machine moved adds a
compare. A rate limit, primary or secondary, refuses the decision, naming when it resets if
GitHub says. `fl github ledger verify` costs about one request per commit, plus one per
segment.

## Errors and what to do

Every error exits 2 with `error: …`, except where this table says otherwise.

| what fl says | what to do |
|---|---|
| the repository has no GitHub ledger yet | `fl github ledger init` |
| the repository is empty | push a first commit, then `fl github ledger init` |
| the ledger was deleted | restore `fl/ledger` at a commit that descends from the first commit fl names; `init` will not start a new one |
| this machine records no anchor | `fl manifest import` the committed manifest, or `fl github ledger init` again |
| the committed manifest lacks the ledger's first commit | on the machine that authors the project, `fl manifest export` and commit; elsewhere, pull and `fl manifest import` |
| this machine has no cut-over | `fl github ledger init` here |
| the ledger was rewritten | `fl github ledger verify`, then find out who rewrote it |
| a file changed, a line sits in the wrong directory, or one id has two contents | `fl github ledger verify`; the message names the file and the commit |
| an unreadable line | upgrade fl if a newer one wrote it; otherwise `fl github ledger quarantine <file> <line> --by <name> --reason <text>` |
| an unknown ledger or manifest format | upgrade fl |
| rate limited | decide again after the reset time it names |
| the head moved on every try | decide again: the runs stayed local, and the next decision publishes them |
| GitHub unreachable before anything ran | nothing ran; retry |
| GitHub unreachable at a move's, check's or finding's publish | refused, no state change; the runs stayed local |
| GitHub unreachable at an attempt's publish | a warning, not an error: the attempt is kept locally and published by the next decision; fl exits as the attempt did |
| the attempt could not be saved locally | an error after the attempt's outcome; it is not published; fl exits as the attempt did |
| a comment was not posted | a warning, not an error: `fl github ledger comment <item>`; fl exits as the decision did |
| `skipped`: a decision's id is not one fl writes | someone wrote that line by hand; `fl github ledger verify`, then quarantine it |
| `ledger = "…"` with any value but `"github"` | fix the config |

## Quarantine

A line fl cannot read stops every read of its directory. `fl github ledger quarantine <file>
<line> --by <name> --reason <text>` appends a line to `quarantine.jsonl` naming it; readers
then skip it and say so. Nothing is removed: the damage and its repair both stay in the
history.

## Verify

`fl github ledger verify` walks every commit from the ledger's first to its head and checks
that each only adds lines or segments, reporting the first that does anything else; then it
checks that no id is on two different lines. It prints progress every hundred steps, stops
after `--max-commits` commits (100,000 unless you say), and exits 1 when it found anything.

## Limits

- **One id, two contents.** Reads refuse an id that appears twice with different content in
  one directory, and `verify` checks every directory. But an entry the ledger holds under the
  same id as a local entry, with different content, is caught only by a read that merges the
  two — runs and attempts. Decisions are never read that way, and publishing takes any id
  already on the ledger as published, whatever its content.
- **A renamed repository.** Decisions are filed under the record's or finding's URL as it was
  when the decision was made. After a rename, `fl github ledger comment` given the issue's
  number finds only decisions filed under the new URL; give it the old URL for the older
  ones.
- **Links.** `@`, `#` and `GH-` are neutralised, but a full issue URL in a gate's name is
  still linked by GitHub, and that link adds a reference to the linked issue's timeline.
- **Whose comments count.** A comment counts as posted when the account fl posts as wrote it,
  or an account that wrote a decision under that item. A collaborator who has written one can
  therefore mark another decision there as posted, and stop its recovery; anyone a ledger line
  names could alter the ledger itself. A comment by an account that wrote no decision under the
  item — after a switch from a token to the App, say — is not recognised, and recovery posts
  that decision again: a harmless duplicate.
- **Retention.** A ledger directory grows without limit; segments bound each file, not the
  whole.
- **Another machine's pass.** A transition runs its own gates. Runs other machines published
  are shown, never taken in place of a local run.

## The live tests

The tests in CI use an in-process fake GitHub, which proves structure, not how GitHub behaves.
`crates/github/tests/live.rs` checks the fake's reading of GitHub against GitHub itself. The
tests are ignored by default; a ledger test whose variable is unset skips, saying which.

- `FL_GITHUB_LIVE_REPO` — a private, throwaway repository: `init`, two racing publishes, a
  stale commit refused, a hand edit and an unreadable line named, tree modes, the rules on
  Free (`200 []`, detection-only), branch listing by prefix, an unrelated history, a near-full
  segment, and a decision comment's round trip — including GitHub's rendering of it, which must
  hold no mention and no issue link.
- `FL_GITHUB_LIVE_READ_ONLY_TOKEN` — a fine-grained token on that repository only, with
  Contents: read and Metadata: read: an append is refused, naming the permission.
- `FL_GITHUB_LIVE_EMPTY_REPO` — a private repository with no commit: the rules still read, and
  `init` says to push a first commit.
- `FL_GITHUB_LIVE_PUBLIC_REPO` — a public repository holding only test data and one commit,
  with the ruleset `init` prints, active: the mode reads as protected, and GitHub refuses a
  force update and a deletion of `fl/ledger`. The test first reads the ruleset and refuses to
  write anything unless GitHub says the credential can never bypass it.

They append to `fl/ledger` and never delete it — a ledger under a ruleset cannot be deleted —
and the first run leaves a branch `fl-live/root` at the ledger's first commit; every later run
reads it there, so each test is safe to run again. The hand-edit test leaves two hand commits
for good, so `fl github ledger verify` on the private throwaway reports them. Set the token as
for the tracker's live tests ([github-tracker.md](github-tracker.md#the-live-tests)), then:

```text
FL_GITHUB_LIVE_REPO=acme/fl-live FL_GITHUB_LIVE_PUBLIC_REPO=acme/fl-live-public \
FL_GITHUB_LIVE_EMPTY_REPO=acme/fl-live-empty \
  cargo test -p fl-github --test live -- --ignored --nocapture --test-threads=1
```

A secondary rate limit, and a GraphQL request that runs past GitHub's time limit, are modelled
from GitHub's documentation and not provoked: doing so would abuse the API.
````

- [ ] **Step 4: Update the other documents**

In `docs/README.md`, after the `github-tracker.md` bullet, add:

```markdown
* [github-ledger.md](github-ledger.md) — publish each decision's evidence to the tracker's
  repository and comment it on its issue: setup, modes, disclosure, comments and their
  recovery, cost, errors and limits.
```

In `docs/github-tracker.md` — each text below is wrapped across lines there, so match it across line breaks — replace the paragraph that starts "Only records and findings move." with:

```markdown
Only records and findings move. The catalog — the project, its gates and its transitions —
stays in the local store, and so does every gate run and attempt. With `ledger = "github"` in
the binding, each decision also publishes its evidence to the repository's `fl/ledger` branch
and posts a comment on its issue; [github-ledger.md](github-ledger.md) says how. Other machines
get the gates from the committed manifest, as [sharing-gates.md](sharing-gates.md) describes;
they never read another machine's store. Because a finding's reproduction names a gate that
other machines will read, `fl finding reproduce` on a project bound to GitHub first checks that
the committed manifest carries that gate as this store has it, and refuses until it does.
```

replace "The catalog commands — `fl project`, `fl gate`, `fl transition`, `fl stats` and `fl manifest` — never contact GitHub." with:

```markdown
The catalog commands — `fl project`, `fl gate`, `fl transition` and `fl manifest` — never
contact GitHub, and `fl stats` does only to count a project's attempts on its GitHub ledger
([github-ledger.md](github-ledger.md)).
```

replace "The token needs to read the repository and to read and write its issues." with:

```markdown
The token needs to read the repository and to read and write its issues; with the GitHub
ledger, also to read and write its contents.
```

replace "registers the App with two repository permissions, **Issues: read and write** and **Metadata: read**, installs it on" with:

```markdown
registers the App with the repository permissions **Issues: read and write** and **Metadata:
read** — and, when the binding names the GitHub ledger, **Contents: read and write** — installs it on
```

(re-wrap the paragraph to 95 columns), and at the end of "## The live tests", add:

```markdown
The GitHub ledger has live tests of its own in the same file;
[github-ledger.md](github-ledger.md#the-live-tests) says what they need.
```

In `docs/sharing-gates.md`, replace "(manifest format 2, written for a project whose GitHub ledger is switched on)" with "(manifest format 2: it carries `ledger_root`, the first commit of the project's GitHub ledger — see [github-ledger.md](github-ledger.md))".

In `docs/superpowers/specs/2026-09-30-github-ledger-design.md`, replace §3.4's first sentence ("A decision costs about three requests, plus one per new segment, plus its comment.") with:

```markdown
A decision costs about ten requests: four for the pre-flight — the repository's visibility,
the rules on `fl/ledger`, the branch's head and its `format` file; four for the flush — the
head again, one listing of the directories it appends to, who fl writes as (once per command)
and the commit; one download for each segment that grew since this machine last read it — in
steady state, each directory the previous decision appended to; and one for its comment. A
head another machine moved adds a compare.
```

In `docs/superpowers/specs/2026-09-23-identity-and-store-roles-design.md`, §0.1, replace the row `| GitHub mode B | GitHub | GitHub | where we start |` with:

```markdown
| GitHub mode B | GitHub | GitHub: the local store keeps every run; each decision publishes to `fl/ledger` ([github-ledger.md](../../github-ledger.md)) | where we start |
```

- [ ] **Step 5: Check the links and the words**

Run: `grep -n "github-ledger.md" docs/README.md docs/github-tracker.md docs/sharing-gates.md docs/superpowers/specs/2026-09-23-identity-and-store-roles-design.md`
Expected: at least one hit in each file.

Run: `grep -n "ledger of gate runs and attempts stay in the local store\|about three requests" docs -r`
Expected: no hit.

Run: `grep -nF "$HOME" docs/github-ledger.md; grep -nwF "$USER" docs/github-ledger.md; grep -nwF "$(hostname)" docs/github-ledger.md; grep -nE '/home/[a-z]|/Users/' docs/github-ledger.md`
Expected: no hit (if `$USER` is an ordinary word such as `root`, read the hit: `fl-live/root` is not a leak). The machine's names are read here, at run time; none is written into the repository.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add docs/github-ledger.md docs/README.md docs/github-tracker.md docs/sharing-gates.md docs/superpowers/specs/2026-09-30-github-ledger-design.md docs/superpowers/specs/2026-09-23-identity-and-store-roles-design.md crates/cli/tests/ledger.rs
git commit -m "docs: the GitHub ledger, its cost, and the links to it

docs/github-ledger.md: what goes where, setup (init re-imports the
manifest on an importing machine), the two modes, permissions,
disclosure and its limit, decision comments and their recovery, the cost
of a decision, every error and its remedy, quarantine, verify, limits,
and the live tests. The tracker doc no longer says the ledger stays
local, and lists Contents. Spec 3.4 states the counted cost; a test pins
it.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

## After the last task

- [ ] `git status --porcelain` prints nothing: every change on the branch is committed.
- [ ] Run the trio once more on the whole branch: `cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace`.
- [ ] Confirm no test reached the network: `grep -rln 'api.github.com\|DEFAULT_API' crates --include='*.rs'` lists exactly four files: `crates/github/src/client.rs` (the constant, and the `next_link` tests' URLs, which are parsed, never fetched), `crates/github/src/lib.rs` (its re-export), `crates/cli/src/main.rs` (the default) and `crates/github/tests/live.rs` (ignored by default).
- [ ] Confirm every *Modelled* name has its live test, and the timeout's is the only *Modelled* marker without one: Task 12 Step 3's script passes.
- [ ] Confirm no label crept into a comment this branch added: `git diff origin/main...HEAD -U0 -- '*.rs' | grep '^+' | grep -iE 'task [0-9]|\bB2[ab]?\b|review (finding|focus)|fix round|fix-wave|ruling'` prints nothing.
- [ ] Confirm no token can reach output in the live tests: `grep -n 'println!\|eprintln!\|dbg!' crates/github/tests/live.rs` — none prints a token, a header, a `Client` or a credential.
- [ ] Follow `WORKFLOW.md`: `superpowers:requesting-code-review` on the whole branch, then a pull request against `main`. The pull request names rulings 1–26 and spec defects 1–11, says the owner settled defect 1 with spec decision 17 and allowed the timeout as the one *Modelled* marker with no live test, lists the three resources the live tests use (`FL_GITHUB_LIVE_PUBLIC_REPO` with its ruleset, `FL_GITHUB_LIVE_EMPTY_REPO`, `FL_GITHUB_LIVE_READ_ONLY_TOKEN`), and says whether the live tests have been run.

## Spec coverage (plan B2b's share)

| spec | where |
|---|---|
| decision 2 (nothing machine-specific on a repository that is not private) in comments | Task 3 (excerpts private only), Task 7 (`a_moves_comment_on_a_repository_that_is_not_private_shows_no_excerpt`), Task 10 (the scan) |
| decision 11 (a refused decision's comment says so) | Task 3 (headings), Task 7 (refused move), Task 8 (refused reproduction) |
| decisions 14 and 17 (a failed comment is a warning; every decision command keeps its exit code) | Tasks 7, 8 |
| §3.4 cost | Task 13 (test, doc, §3.4) |
| §4.1 order, where, transferred issue, one move one comment | Tasks 6, 7, 8, 9 |
| §4.2 rendered from the ledger, transitions → gates through the catalog, header, table, escaping, excerpts, fence, 60,000-byte cap, the state line | Tasks 3, 4, 5, 7 |
| §4.3 recovery, the marker, every page, posting only what is missing, an edited comment still counts (only fl's own comments count), no retroactive comments | Tasks 3 (marker, ids fl writes), 6 (pages, authors), 9 (the command) |
| §4.4 the tracker unaffected | unchanged: comments are not state events (`STATE_EVENTS` in `tracker.rs`) |
| §5 visibility read once per decision, used for the comment | Task 7 (`post_one` reads the cached visibility) |
| §6.3 permissions named in the docs | Task 13 |
| §7 "a comment fails to post" (amended with decision 17) | Task 7 (ruling 3) |
| §8.1 paginated issue comments in the fake | Task 6 |
| §8.3 comment recovery and marker de-duplication across pages; escaping; the cap; the scan for paths, `$HOME`, the host name | Tasks 4, 6, 9, 10 |
| §8.4 live tests, private and public | Tasks 11, 12 |
| §9 docs and links | Task 13 |
| B1 requirement 7 (live tests; GraphQL timeout; unrelated-history compare; empty repository; `rules/branches` on Free; near-full segment; `TreeEntry.mode`) | Tasks 2, 11, 12 |
| B1 requirement 12 (comments, recovery, docs) | Tasks 3–9, 13 |
| PR #22 parked items (plural, `note:` prefix, test gaps, tracker doc, Contents, cost, re-import, ruling 13's limit) | Tasks 1, 13 |
