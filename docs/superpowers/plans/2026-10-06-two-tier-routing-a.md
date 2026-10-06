# Two-tier routing, plan A — the area, the routing map and the routing tracker

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a project bind two trackers — the local store and one GitHub repository — and route every new record and finding to one of them by its **area**, through a `TieredTracker` that implements `Tracker` over both tiers, with handles that never mean two items, merged lists, references across tiers, and GitHub opened only when a GitHub item is needed.

**Architecture:** `fl-core` gains `routing.rs` (the tier, the area, the routing map, `ForeignRecord`, `RoutingFault`, the `Routes` and `GithubTier` traits), `tiered.rs` (`TieredTracker`: placement, creates, lookups, merged lists) and a test-only GitHub tier, `mem_issues.rs`. `Record` and `Finding` carry `area`; `Tracker` gains `add_record_with_area` and `add_finding_checked`, and every implementation and wrapper changes with it. `fl-store` keeps the routing map and raises its format to 5; its manifest carries the map as format 3. `fl-github` writes the area into the issue's block (`fl_format` 2) and as an `fl:area/<name>` label every rewrite keeps, writes a reference to a local record as `{id, title}`, and finds items by area by reading blocks. `fl-cli` builds the router for a routed store over a lazily opened GitHub tier, adds `fl routing set|show|remove`, `--area`/`--tier` on creates, `#41` as a GitHub handle, and a tier column in routed lists.

**Tech Stack:** Rust 2024 (`rust-version = "1.98"`), redb 4.3, serde/serde_json, thiserror 2, clap 4, ureq 3, assert_cmd/predicates for black-box tests. No new crates.

**Spec:** `docs/superpowers/specs/2026-10-06-two-tier-routing-design.md` (rev 2.2: rev 2 at `06786fe`, amended with this plan by the owner's decisions 20–22 and the seven defects below) — all of §0.1, §1, §2, §4, §5 and §8's "Plan A". The GitHub tracker spec (`2026-09-26-github-tracker-design.md`) §2.3, §3.1–§3.4 and §6, and the identity spec §3.3–§3.4 and §6.1, are the ground this plan changes.

**Branch:** `ferris/routing-a`, off `ferris/routing-plan-a` (main at `06786fe` + this plan), or off `main` once this plan is merged.

## Plan B — what this plan leaves

Plan B (escalation) is written after plan A merges. Plan A builds none of: `fl record escalate` / `fl finding escalate`, the pre-checks of §3.2, the "escalating" mark, the find-or-create step and its search, tombstones, `--abandon`, the findings list in an escalated issue, the `needs_human` trigger, and the escalation live test. Where plan B plugs in, plan A leaves exactly this:

* **Store format 5** (`FORMAT_WITH_ROUTING`) is defined here and raised by its first two writers — a routing map and an item with an area. Plan B's mark and tombstone tables are "additive under format 5" *only if no release is cut between the two plans*; if one is, plan B raises the store to 6, because an fl built from plan A would open a store holding marks and ignore them (plan ruling 23).
* **Lookups go through one function**, `TieredTracker::route` (Task 7). Plan B adds the tombstone hop there ("a local item the store holds as a tombstone → its target") and nowhere else; the router's lists are `records`/`findings` (Task 8), where plan B adds the "escalating" mark and leaves tombstones out.
* **The error variants** plan B needs do not exist yet. Plan A adds neither `Escalating` nor a tombstone error. ⚠ The spec's `StoreError::Moved { from, to }` (§3.6) cannot be added under that name: `StoreError::Moved { id, to: String }` already exists for a transferred issue (`crates/core/src/store.rs:155-160`). Rev 2.1 of the spec names plan B's variant `Escalated { from, to }` (spec defect 1).
* **Evidence.** Plan A tags a finding's runs with the record's primary IRI as the tier that holds the record answers it (plan ruling 22). Plan B makes `route` follow a tombstone first, which is all §2.5's "resolves that IRI through any tombstone first" needs.

---

## Global Constraints

- Verification trio, all green before every commit: `cargo fmt --all --check`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`. Each task runs `cargo fmt --all` first, so code blocks here need not be in rustfmt's exact layout; lines stay within 100 columns.
- Unit tests live in `#[cfg(test)] mod tests` inside the module they test; black-box CLI tests live in `crates/cli/tests/`; an `fl-exec` test that drives a real store lives in `crates/exec/tests/`.
- **No test contacts the network.** GitHub is the in-process fake (`fl_github::fake::FakeGithub` on `127.0.0.1`, reached by the binary through `FL_GITHUB_API_URL`), or, inside `fl-core`, `mem_issues::MemIssues`. This plan adds no live test (the spec's one live test is plan B's).
- `fl-core` stays pure: "No IO, no async, no clock, no network" (`crates/core/src/lib.rs:1`). The router takes the GitHub tier as a trait object; opening it is the CLI's job.
- Spec values, verbatim: "Area names are lowercase `[a-z0-9-]{1,32}`" (§1.1); "On GitHub the area is also a label, `fl:area/<name>`" (§1.1); the starting set — "`code` and `tests` → `local`; `design` and `product` → `github`; `security` → `github`, sensitive" (§1.2); manifest formats "1 | gates, transitions | no ledger root, no routing; 2 | + `ledger_root` | a ledger root, no routing; 3 | + `routing` (and `ledger_root` if any) | a routing map" (§1.2); "the store raises its format to **5** the first time one of them is written" (§1.4); "An issue whose block carries an area or a local record reference is written with `fl_format` 2" (decision 14); "Exactly two tiers, with fixed names `local` and `github`" (decision 8).
- Spec invariants, verbatim: "one routing rule per project, not per machine" (§1.2); "routing never changes tier silently" (§1.3); "a list that cannot see its whole population fails" (§2.4); "If that tier cannot be reached, the result is an error, never 'no such record'" (§2.5); "When the map sends a security finding to a non-private repository, fl refuses and the error says to use `--tier local`. It never moves the finding by itself" (§2.1).
- **Every existing configuration keeps working:** a store that holds no routing map behaves exactly as before — the same trackers, the same handles (`#41` and `41` alike), the same output columns, the same manifest bytes (formats 1 and 2), the same store format, and GitHub opened at the start as today. Every existing test passes, except the assertions a task changes on purpose (named in that task).
- `snake_case` on every wire (`crates/core/src/wire.rs`); `Tier` spells itself through `wire_names!`/`wire_parse!`.
- **Every guard gets a mutation check**: revert it, watch the named test go red, restore. Each task's mutation step lists every guard the task adds — each conjunct of a compound condition, each match arm that must honour an input, and ordering where order matters. Where a line looks like a guard but no input can tell it apart, the step says so.
- **A unit-test filter in a mutation step names the full module path** (`cargo test -p fl-core --lib tiered::tests::`), or it matches nothing. A black-box filter names the test file (`cargo test -p fl-cli --test routing -- <name>`). At most one filter goes before `--`.
- **A test never asserts with a substring another code path also produces.** Each task names the unique phrase it asserts.
- **No plan names, task numbers or review labels in code comments** ("Task 7", "plan A", "ruling 9"). Cite the spec by section ("routing spec §2.2"), or say the reason.
- **A change to shared plumbing states its blast radius** in the task that makes it.
- Line numbers cite `06786fe`; an earlier task's edits shift them. Find the named item, not the number.
- Commits are authored by the machine account (`WORKFLOW.md`) and end with `Co-authored-by: Ferris <Ferris@artificialhumanity.io>`. Stage explicit paths — never `git add -A` — and after each commit run `git status --porcelain`, which must print nothing.
- The repository is public: no machine paths, host names, user names or lab names in code, tests, messages or docs. Fake repositories are `acme/widgets`.

## Review Focus

1. **A routed project whose GitHub is down, or not bound on this machine.** Work on local items must succeed with no request to GitHub at all, and a merged list must refuse — never print the local items as if they were the whole list — and say to use `--tier local`. Task 8 (`a_merged_list_refuses_when_the_github_tier_cannot_be_read`), Task 10 (`a_local_item_is_made_and_moved_without_a_request_to_github`), Task 13 (`a_merged_list_with_github_down_is_refused_naming_tier_local`).
2. **The same number typed as `41` and as `#41` in a routed project** — the case the migration notice is about. They must name two different items (local 41, issue 41), print back exactly as they can be typed, and a bare number no local item holds must ask "did you mean `#41`?" rather than reach GitHub. Task 11 (`a_bare_number_and_a_hash_number_name_different_items_and_print_back_as_typed`, `a_bare_number_no_local_item_holds_asks_did_you_mean_the_issue`).
3. **An id that neither tier holds** — another machine's local item, or a typo. The answer names "another machine's local tier", never `NotOwned` or "no such record"; and when GitHub cannot be read during the fallback, the answer is that error, never "not existing". Task 8 (`an_id_neither_tier_holds_is_held_elsewhere_never_not_owned`, `a_fallback_to_an_unreachable_github_is_an_error_not_not_found`).
4. **A finding whose record is in the other tier, in both directions, after it is written.** On GitHub the block holds `{id, title}`, is `fl_format` 2, shows the record as escaped plain text, reads back as the claim alone, and keeps all of it through an update; locally the store keeps a GitHub record's URL; the reproduction's run names the record's IRI. Task 1 (`a_local_record_reference_shows_as_plain_text_and_reads_back_as_the_claim`), Task 5 (`a_finding_about_a_local_record_reads_back_and_survives_an_update`), Task 7 (`a_finding_crosses_tiers_only_through_a_checked_foreign_record`), Task 8 (`crates/exec/tests/tiered_evidence.rs`).
5. **A sensitive area, a `--security` finding, or a finding about a local record in a sensitive area, on its way to a public repository.** Refused before anything is written — no issue, no label — naming `--tier local` when the map chose the tier; with `--tier github` given explicitly, it is the tracker's own refusal (spec decision 21). Task 7 (`a_sensitive_area_routed_to_a_public_repository_is_refused_before_anything_is_written`, `a_finding_about_a_record_in_a_sensitive_area_never_reaches_a_public_repository`, `a_record_whose_area_the_map_no_longer_declares_counts_as_sensitive`), Task 9 (`a_tier_change_keeps_the_areas_sensitivity`), Task 14 (`a_record_whose_area_was_removed_elsewhere_stays_protected`), Task 12 (`a_sensitive_area_routed_to_a_public_repository_creates_nothing_and_names_tier_local`, `a_finding_about_a_sensitive_local_record_is_refused_on_a_public_repository`).

## Rulings this plan makes

The spec leaves these open or ambiguous. Each says why, and what it costs if wrong.

1. **Plan ruling: `Tracker::add_record_with_area` is the required method, and `add_record(project, title)` stays as a provided method that passes `None`.** 168 call sites keep compiling, and a wrapper implements the one required method, so no wrapper can drop the area. *If wrong:* none — the router refuses `None` in a routed project.
2. **Plan ruling: the spec's `create_in(tier, …)` is the router's placement with a tier given** — `place_record(project, area, Some(tier))` or `place_finding(&f, Some(tier))`, then `add_record_at` / `add_finding_at`. Placement makes every check before anything is written and lets the CLI warn between the decision and the write; a placement can only be built by the router. *If wrong:* a name.
3. **Plan ruling: area inheritance happens in the router**, so `Tracker::add_finding` inherits too; the CLI prints the note from the placement. *If wrong:* none.
4. **Plan ruling: `ForeignRecord` carries the record's id, title and tier**, has a `pub(crate)` constructor, and a `#[doc(hidden)]` `for_tests` behind the `conformance` feature, which the binary does not enable. *If wrong:* a test can build one; the binary cannot.
5. **Plan ruling: a GitHub block's reference to a local record is `{"id": …, "title": …}`.** The field is `id`, as in every reference (`RecordRef`, `crates/github/src/meta.rs:63-68`), not the spec's `{ iri }`; `title` is added so the line the issue shows is rendered from the block and survives every rewrite (an update re-renders the body from prose and block). *If wrong:* a renamed local record shows its old title on GitHub — fl has no record rename.
6. **Plan ruling: the issue shows `Record: <title> — <IRI>, held in the local tier, not on GitHub.`** between the claim and the block, both parts escaped with the ledger comment's `render::escape`, so a title cannot mention anyone or link an issue. A read strips exactly that line. *If wrong:* wording.
7. **Plan ruling: `fl_format` is computed, never set by hand.** `Meta::sealed` sets it from the fields (`render_body` and the tracker's remembered copy both seal), and a block whose `fl_format` disagrees with its fields is damaged. *If wrong:* none; without it, a remembered format-1 copy of a format-2 block would read as a conflict on the next write.
8. **Plan ruling: an area label is created the first time a process writes an item with that area** (`ensure_labels(Some(area))`), and an update or repair ensures it before it writes. fl never deletes one. *If wrong:* one label list read per new area per process.
9. **Plan ruling, made safe by spec decision 20 (owner, 2026-10-06): a store is routed when its project has a routing map, and a routed store holds exactly one project.** The tracker and the handle rules are chosen per command before the command's project is known (`fl record move 41` names none), and handles are numbered per store, so "routed project" and "routed store" are one thing. `fl routing set` is refused while the store holds another project, naming the remedy (a config entry with its own `store`). `[agent]` extension, so the rule cannot be walked around: once a store is routed, `fl project add` and an import of another project into it are refused, and so is an import of a routed manifest into a store that holds another project (Tasks 3, 4). The router keeps its `Unrouted` refusal for a project without a map, which no routed store can now reach through the CLI. *If wrong:* a person with several projects in the default store must give the routed one its own store.
10. **Plan ruling: every `fl routing` command takes `--project`,** as every project-scoped command does; the spec's syntax omits it. *If wrong:* one flag.
11. **Plan ruling: routing currency on the authoring machine compares the working tree's manifest with the store's map, when a manifest exists;** with none, no other machine can import the project, so the invariant holds and the create proceeds. On an importing machine it is `ensure_import_current`. *If wrong:* a routed project nobody exported routes only on its authoring machine — which is the only machine it has.
12. **Plan ruling: an import that would drop a project's routing map is refused**, as an import that would drop a gate is (`WouldRemoveGate`); an older checked-out manifest would otherwise un-route one machine. *If wrong:* a machine must check out a newer manifest to import.
13. **Plan ruling: merged lists, the withdrawal counts and `fl routing remove` need both tiers.** A routed project with no binding on this machine lists with `--tier local`. With `--tier`, the withdrawal counts are that tier's, and say so. *If wrong:* one flag for an unbound machine; §1.3's "works on its local items" holds for creates, lookups and moves.
14. **Plan ruling: a record in a sensitive area is refused at creation on a non-private repository; `Record` gains no stored security flag.** The flag's only consequence today is that rule (GitHub tracker spec §6); a finding in a sensitive area does get `security: true`. *If wrong:* plan B's escalation pre-check reads the area's sensitivity from the map, as §3.2 already says.
15. **Plan ruling: the router reads a finding's record once, in the tier that holds it, to place the finding**, and writes that record's primary IRI into the finding. A tracker that holds the record then checks it again as it always has (the GitHub tracker's `add_finding` reads the issue); the router adds one read, no more. *If wrong:* one read.
16. **Plan ruling: `fl routing remove` reads every issue's block.** An issue carrying no `fl:` label whose block is unreadable is not fl's and is skipped; one carrying an `fl:` label whose block is unreadable refuses the removal, naming it. *If wrong:* a repository with many issues pays one full listing per removal.
17. **Plan ruling: the tier column is the second column** (`<handle>\t<tier>\t…`), only in a routed store. *If wrong:* column order.
18. **Plan ruling: `--area` and `--tier` in an unrouted store are refused, naming `fl routing set`.** *If wrong:* none.
19. **Plan ruling: `fl finding list --record <id>` works in every project**; exactly one of `--project` and `--record` is given. *If wrong:* none.
20. **Plan ruling: a lazily opened GitHub tier that fails with an error that is not a `StoreError`** (a credential missing, `$FL_GITHUB_API_URL` refused) becomes `StoreError::Backend` carrying the whole message chain. *If wrong:* the message loses its anyhow context markers.
21. **Plan ruling, under spec decision 21 (owner, 2026-10-06): a GitHub finding about a local record in a sensitive area is refused on a repository that is not private, like any security item — naming `--tier local` when the map chose the tier; about a local record in any other area, the disclosure of §2.5 is a `warning:` line on stderr, after placement and before the write.** A visibility that cannot be read refuses the create. A record whose area the map no longer declares counts as sensitive (spec decision 22, failing closed), and every finding the rule covers carries `security: true`, wherever it is placed. *If wrong:* wording.
22. **Plan ruling: in plan A, "the resolved IRI" a finding's evidence is tagged with is the record's primary IRI as the tier holding it answers.** Plan B adds the tombstone hop in `route`. *If wrong:* none until plan B.
23. **Plan ruling: store format 5 is shared with plan B only if no release ships between them**; otherwise plan B raises to 6 (the owner was told, 2026-10-06; spec §1.4 says so). *If wrong:* an fl from plan A ignores plan B's marks.
24. **Plan ruling: an inherited area is reported as `note: area: <name>, from its record` on stderr**, so stdout keeps its columns. *If wrong:* wording.
25. **Plan ruling: the first `fl routing set` prints two `notice:` lines** — the starting set it wrote, rendered from `RoutingMap::starting()` so the two cannot drift, and the handle change for this project's previous mode (a binding in the config: GitHub-only before; none: local-only before). The import that first routes a store prints the same handle-change line (spec §2.3, rev 2.1). *If wrong:* wording.
26. **Plan ruling: `fl github …` in a routed store opens GitHub at the start**, as today: those commands name GitHub items only. *If wrong:* none.
27. **Plan ruling: GitHub's tier claims an issue URL under the configured name, or — once GitHub is open — under the repository's name now**, which differs after a rename and is the name the tracker writes into every URL it returns. Any other issue URL is asked of the local tier first, then of GitHub, whose own `owner` check resolves an old name of the bound repository. With no binding the tier claims nothing; an issue URL the local tier does not hold is then refused as the missing tier (`GithubTier::issue_form`), never as held elsewhere, and one the local tier holds as an alias is found there. *If wrong:* one local lookup before GitHub's.
28. **Plan ruling: an IRI no local store holds reaches the router in a routed store** — `choose_store` falls back to the bound store when it is routed, and a routed store strips issue URLs from the search even with no binding — so the router can say "held on another machine's local tier", find an item another machine moved to GitHub, or name the missing config entry. Whether the bound store is routed is read only when the command names an IRI. *If wrong:* one extra open of the bound store for such a command.
29. **Plan ruling: a local store's `update_finding` keeps the stored area, record, raiser and security mark,** as the GitHub tracker always has; the conformance suite pins it. An item keeps its area for life (§1.1). *If wrong:* none — no caller changes them.
30. **Plan ruling: `fl manifest import` of a routed manifest is refused where the binding names `ledger = "github"`** (decision 12), before anything is imported. *If wrong:* none.
31. **Owner decision 22 (2026-10-06), as this plan carries it: decision 21 fails closed.** `fl routing set` keeps an area's sensitivity unless told: `--sensitive` sets it, `--not-sensitive` clears it, neither keeps it (`after_set`'s `Option<bool>`, Task 3; the flags, Tasks 9 and 14). Clearing it is refused while any item in either tier names the area, by the same check and with the same list as `fl routing remove` (one function, `refuse_while_named`, Task 14). A record whose area the map no longer declares counts as sensitive (Task 7). A finding the rule covers carries `security: true` (Task 7). `[agent]`: a record with no area at all — made before the project was routed — is not sensitive; it never had an area to protect. *If wrong:* such a record's finding is published like any other's.

## Spec defects this plan found

All seven are amended in the spec's rev 2.1 (same file, 2026-10-06), each marked as an owner decision or `[agent]`.

1. **§3.6 `StoreError::Moved { from, to }`** — the name is taken: `StoreError::Moved { id, to: String }` already reports a transferred issue (`crates/core/src/store.rs:155-160`). Rev 2.1 names plan B's variant `Escalated { from, to }`.
2. **§1.2 "the stale check on the authoring machine, as gated moves already do (`cli/src/cmd/manifest.rs:100`)".** Gated moves call only `ensure_import_current` (`crates/cli/src/cmd/record.rs:115-120`), which returns `Ok` on the authoring machine (`crates/cli/src/cmd/manifest.rs:98-101`): no gated move checks staleness there. Plan ruling 11.
3. **Decision 14 / §2.5 "local record ref `{ iri }`".** The block's reference field is `id` (`crates/github/src/meta.rs:63-68`). Plan ruling 5.
4. **Decision 13 "an item made with a sensitive area carries the security flag".** Only `Finding` has the flag (`crates/core/src/finding.rs:80-84`); `Record` has none (`crates/core/src/model.rs:117-127`). Plan ruling 14.
5. **§1.2 command syntax.** `fl routing set <area> <tier>` names no project, while every project-scoped command takes `--project` (`crates/cli/src/cmd/record.rs:13-21`). Plan ruling 10.
6. **§1.3 "A routed project with no binding works on its local items" against §2.4 "If either tier cannot be read, the merged list is an error".** Read together, an unbound machine cannot list without `--tier local`. Plan ruling 13.
7. **§2.2 "an issue URL of the bound repository → GitHub".** "The bound repository" by name cannot include its old names without a request (`GithubTracker::owner`, `crates/github/src/tracker.rs:665-692`). Plan ruling 27.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/core/src/routing.rs` | create | `Tier`, area names, `AreaRoute`, `RoutingMap`, `after_set`, `Routes`, `ForeignRecord`, `RoutingFault`, `GithubTier` |
| `crates/core/src/tiered.rs` | create | `TieredTracker`: placement, creates, lookups, merged lists, the `Tracker` role |
| `crates/core/src/mem_issues.rs` | create (test/conformance only) | `MemIssues`: an in-memory GitHub tier |
| `crates/core/src/model.rs`, `finding.rs` | modify | `Record.area`, `Finding.area` |
| `crates/core/src/store.rs` | modify | `add_record_with_area`, `add_finding_checked`, `StoreError::Routing`; `CatalogChecked` |
| `crates/core/src/mem.rs` | modify | the area, the routing map, `add_finding_checked` |
| `crates/core/src/conformance.rs` | modify | one tracker case: an area reads back |
| `crates/core/src/lib.rs` | modify | modules and re-exports |
| `crates/core/tests/wire_refs.rs` | modify | `area: None` |
| `crates/exec/src/journal.rs`, `evaluate.rs`, `decision.rs` | modify | the trait's new methods; `area: None` |
| `crates/exec/tests/tiered_evidence.rs` | create | a finding's run names its record across tiers |
| `crates/store/src/lib.rs` | modify | format 5, the area, the `routing` table, `add_finding_checked`, import of a map |
| `crates/store/src/manifest.rs` | modify | format 3, `routing` |
| `crates/github/src/meta.rs` | modify | `Meta.area`, local `RecordRef`, `fl_format` 2, the record line, area labels |
| `crates/github/src/tracker.rs` | modify | the area in blocks and labels, `add_finding_checked`, `items_in_area`, `visibility` |
| `crates/cli/src/tiers.rs` | create | `LazyGithub`, `Tiers` |
| `crates/cli/src/ctx.rs` | modify | `Ctx.tiers`, `show_item`, `resolve_item`, `on_github` |
| `crates/cli/src/refs.rs` | modify | `Ref::Issue` |
| `crates/cli/src/main.rs` | modify | the routed store, decision 12, the lazy tier, `Command::Routing` |
| `crates/cli/src/cmd/routing.rs` | create | `fl routing set|show|remove` |
| `crates/cli/src/cmd/record.rs`, `finding.rs`, `attempt.rs`, `check.rs`, `github.rs`, `ledger.rs`, `mod.rs` | modify | `--area`, `--tier`, `--record`, routed lists, handles |
| `crates/cli/src/cmd/manifest.rs` | modify | routing currency; an import that first routes a store; decision 12 on import |
| `crates/cli/tests/routing.rs` | create | the black-box world of a routed project |
| `docs/routing.md` | create | routing, for a person |
| `docs/github-tracker.md`, `docs/sharing-gates.md`, `docs/README.md` | modify | the area label and block, local references, formats, links |
| `docs/superpowers/specs/2026-09-26-github-tracker-design.md` | modify | §2.3's invariant amended for local records |
| `docs/getting-started.md` | modify | the `fl --help` block lists `routing` |
| `docs/superpowers/specs/2026-10-06-two-tier-routing-design.md` | modified with this plan (rev 2.2) | decisions 20–22 and the seven defects; no task edits it |

---

### Task 1: The issue block and labels learn the area and the local record reference

The pure half of GitHub's side (routing spec §1.1, §2.5, decision 14): `Meta` gains `area`; a reference to a local record has no `node_id` and carries its title; `fl_format` is computed — 2 exactly when the block carries an area or a local reference; the issue shows a local record as escaped text; `fl:area/<name>` is one of fl's labels, kept by every label rewrite and checked against the block on every read. The tracker's two label rewrites (update, repair) pass the area through and create its label first.

**Blast radius:** `parse_body` is read by every issue read, every list, the create-key search and the ledger's comment recovery: a block of format 2 now reads, and a block whose `fl_format` disagrees with its fields is damaged (none exists — fl never wrote one). `read_item` is read by every read and list: an `fl:area/…` label was a stray label (diverged) and is now checked against the block. `labels_after` gains a parameter (two callers in `tracker.rs`). `ensure_labels` gains a parameter (three callers). No item is written with an area until Task 2.

**Files:**
- Modify: `crates/github/src/meta.rs` (`FL_FORMAT`, `RecordRef`, `Meta`, `render_body`, `BodyError`'s `Display`, `parse_body`, `labels_after`, `read_item`; tests)
- Modify: `crates/github/src/tracker.rs` (`GithubTracker.labels_ready`, `open`, `ensure_labels`, `create`, `update`, `repair`, `current_ref`, `add_finding`; tests)

**Interfaces:**
- Consumes: `crate::ledger::render::escape(&str) -> String` (`crates/github/src/ledger/render.rs:350`).
- Produces (in `fl_github::meta`): `pub const FL_FORMAT_ROUTED: u64 = 2`; `pub const AREA_LABEL_PREFIX: &str = "fl:area/"`; `pub fn area_label(area: &str) -> String`; `RecordRef { pub id: Iri, pub node_id: Option<String>, pub title: Option<String> }` with `pub fn is_local(&self) -> bool`; `Meta.area: Option<String>`; `Meta::required_format(&self) -> u64`; `Meta::sealed(self) -> Meta`; `pub fn record_line(meta: &Meta) -> Option<String>`; `pub fn labels_after(current: &[String], kind: ItemKind, state: &str, area: Option<&str>) -> Vec<String>`. In `GithubTracker`: `fn ensure_labels(&self, area: Option<&str>) -> Result<(), StoreError>`. Unique phrases: `which its block's area needs`, `do not match its block's area`, `held in the local tier, not on GitHub`, `but its fields are format`.

- [ ] **Step 1: Write the failing tests**

In `crates/github/src/meta.rs`, inside `mod tests`, change `a_body_that_is_missing_damaged_or_newer_is_named`'s last assertion (format 2 is now a format this fl reads) to:

```rust
        assert_eq!(
            parse_body(&good.replace("\"fl_format\":1", "\"fl_format\":3")),
            Err(BodyError::UnknownFormat(3))
        );
        assert!(
            BodyError::UnknownFormat(3).to_string().contains("formats 1 to 2"),
            "{}",
            BodyError::UnknownFormat(3)
        );
```

In `every_state_has_a_label_and_fl_replaces_only_its_own`, give `labels_after` its new last argument, `None`. In `an_unknown_field_inside_record_is_damaged`, build the reference as:

```rust
        m.record = Some(RecordRef {
            id: Iri::parse("urn:uuid:00000000-0000-7000-8000-000000000099").unwrap(),
            node_id: Some("I_9".into()),
            title: None,
        });
```

Then add, after `an_unknown_field_inside_record_is_damaged`:

```rust
    fn local_ref(title: &str) -> RecordRef {
        RecordRef {
            id: Iri::parse("urn:uuid:00000000-0000-7000-8000-000000000042").unwrap(),
            node_id: None,
            title: Some(title.into()),
        }
    }

    /// Everything before the block: what GitHub renders.
    fn shown(body: &str) -> &str {
        &body[..body.rfind(META_OPEN).unwrap()]
    }

    // Routing spec decision 14: format 2 exactly when the block carries an
    // area or a reference to a local record — computed, whatever the field
    // held.
    #[test]
    fn the_block_is_format_2_exactly_when_it_carries_an_area_or_a_local_record() {
        let format = |m: &Meta| parse_body(&render_body("p", m)).unwrap().1.fl_format;
        let mut plain = meta(ItemKind::Finding, "raised");
        plain.fl_format = 7;
        assert_eq!(format(&plain), 1, "computed, never taken from the field");
        let mut with_area = meta(ItemKind::Record, "todo");
        with_area.area = Some("code".into());
        assert_eq!(format(&with_area), 2);
        let mut on_github = meta(ItemKind::Finding, "raised");
        on_github.record = Some(RecordRef {
            id: Iri::parse("https://github.com/acme/widgets/issues/3").unwrap(),
            node_id: Some("I_3".into()),
            title: None,
        });
        assert_eq!(format(&on_github), 1);
        assert!(!shown(&render_body("p", &on_github)).contains("Record: "));
        let mut on_local = meta(ItemKind::Finding, "raised");
        on_local.record = Some(local_ref("t"));
        assert_eq!(format(&on_local), 2);
        assert!(render_body("p", &plain).contains("\"fl_format\":1"));
        assert!(render_body("p", &with_area).contains("\"fl_format\":2"));
        assert!(!render_body("p", &plain).contains("\"area\""), "skipped when absent");
    }

    #[test]
    fn a_block_whose_format_disagrees_with_its_fields_is_damaged() {
        let plain = render_body("p", &meta(ItemKind::Record, "todo"));
        let lying = plain.replace("\"fl_format\":1", "\"fl_format\":2");
        assert_ne!(lying, plain, "the edit must have landed");
        match parse_body(&lying) {
            Err(BodyError::Damaged(why)) => {
                assert!(why.contains("but its fields are format 1"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        let mut m = meta(ItemKind::Record, "todo");
        m.area = Some("code".into());
        let lying = render_body("p", &m).replace("\"fl_format\":2", "\"fl_format\":1");
        assert!(matches!(parse_body(&lying), Err(BodyError::Damaged(_))));
    }

    // Routing spec §2.5: a reader on GitHub cannot open a local record, so
    // the issue names it as text — text that mentions nobody and links
    // nothing — and the claim reads back without it.
    #[test]
    fn a_local_record_reference_shows_as_plain_text_and_reads_back_as_the_claim() {
        let mut m = meta(ItemKind::Finding, "raised");
        m.record = Some(local_ref("@alice: fix #3 <b>"));
        let body = render_body("the claim", &m);
        let text = shown(&body);
        assert!(text.contains("Record: "), "{text}");
        assert!(text.contains("held in the local tier, not on GitHub"), "{text}");
        assert!(text.contains("00000000-0000-7000-8000-000000000042"), "{text}");
        assert!(!text.contains("@alice"), "a title must not mention anyone: {text}");
        assert!(!text.contains("#3"), "a title must not link an issue: {text}");
        assert!(!text.contains("<b>"), "{text}");
        let (prose, back) = parse_body(&body).unwrap();
        assert_eq!(prose, "the claim");
        assert_eq!(back, m.clone().sealed());
        let (prose, _) = parse_body(&render_body("", &m)).unwrap();
        assert_eq!(prose, "", "an empty claim reads back empty");
    }

    // Routing spec §1.1: the area label is one of fl's, rewritten from the
    // block like the other two.
    #[test]
    fn every_rewrite_keeps_the_area_label_from_the_block() {
        let after = labels_after(
            &["bug".into(), "fl:record/todo".into(), "fl:area/old".into()],
            ItemKind::Record,
            "doing",
            Some("code"),
        );
        assert_eq!(after, vec!["bug", "fl:record", "fl:record/doing", "fl:area/code"]);
        let none = labels_after(&["fl:area/code".into()], ItemKind::Record, "todo", None);
        assert_eq!(none, vec!["fl:record", "fl:record/todo"]);
    }

    // Routing spec §1.1: "an issue whose area label is missing or differs
    // from its block reads as diverged, as a wrong state label does today".
    #[test]
    fn an_area_label_that_is_missing_or_differs_from_the_block_is_diverged() {
        let mut m = meta(ItemKind::Record, "todo");
        m.area = Some("code".into());
        let body = render_body("", &m);
        let read = |labels: &[&str]| read_item(&issue(labels, "open", &body));
        assert!(matches!(
            read(&["fl:record", "fl:record/todo", "fl:area/code"]).unwrap(),
            Read::Item { .. }
        ));
        let err = read(&["fl:record", "fl:record/todo"]).unwrap_err().to_string();
        assert!(
            err.contains("which its block's area needs") && err.contains("fl:area/code"),
            "{err}"
        );
        let err = read(&["fl:record", "fl:record/todo", "fl:area/design"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("do not match its block's area") && err.contains("fl:area/design"),
            "{err}"
        );
        let err = read(&["fl:record", "fl:record/todo", "fl:area/code", "fl:area/x"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("do not match its block's area"), "{err}");
        let plain = render_body("", &meta(ItemKind::Record, "todo"));
        let err = read_item(&issue(
            &["fl:record", "fl:record/todo", "fl:area/code"],
            "open",
            &plain,
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("do not match its block's area (none)"), "{err}");
    }
```

In `crates/github/src/tracker.rs`, inside `mod tests`, after `a_record_is_an_issue_with_its_two_labels_and_its_block`, add:

```rust
    /// Gives issue `n` the area `area` in its block, with its label or
    /// without — what a newer fl, or a hand, left there.
    fn give_area(fake: &FakeGithub, n: u64, area: &str, labelled: bool) {
        fake.web_edit(n, |i| {
            let (prose, mut m) = meta::parse_body(&i.body).unwrap();
            m.area = Some(area.into());
            i.body = meta::render_body(&prose, &m);
            if labelled {
                i.labels.push(meta::area_label(area));
            }
        });
    }

    /// How many labels fl has created in the repository.
    fn label_creates(fake: &FakeGithub) -> usize {
        fake.state()
            .requests
            .iter()
            .filter(|r| *r == "POST /repos/acme/widgets/labels")
            .count()
    }

    // Routing spec §1.1: a missing area label reads as diverged, and repair
    // restores it from the block — creating the label first, never as a side
    // effect of the write (GitHub tracker spec §3.3).
    #[test]
    fn a_repair_restores_a_missing_area_label_creating_it_first() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        give_area(&fake, 1, "code", false);
        let fresh = open(&fake);
        let err = fresh.get_record(&r).unwrap_err().to_string();
        assert!(err.contains("which its block's area needs"), "{err}");
        let before = label_creates(&fake);
        assert!(fresh.repair(r.iri(), "owner").unwrap().changed);
        let labels = fake.issue(1).labels;
        assert!(labels.contains(&"fl:area/code".to_string()), "{labels:?}");
        assert_eq!(label_creates(&fake), before + 1, "created explicitly");
        assert!(open(&fake).get_record(&r).unwrap().is_some());
    }

    // Routing spec §1.1: an area label that differs from the block reads
    // as diverged, and repair rewrites it from the block.
    #[test]
    fn a_repair_replaces_a_wrong_area_label_with_the_blocks() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        give_area(&fake, 1, "code", false);
        fake.web_edit(1, |i| i.labels.push(meta::area_label("design")));
        let fresh = open(&fake);
        let err = fresh.get_record(&r).unwrap_err().to_string();
        assert!(err.contains("do not match its block's area"), "{err}");
        assert!(fresh.repair(r.iri(), "owner").unwrap().changed);
        let labels = fake.issue(1).labels;
        assert!(
            labels.contains(&"fl:area/code".to_string())
                && !labels.contains(&"fl:area/design".to_string()),
            "{labels:?}"
        );
        assert!(open(&fake).get_record(&r).unwrap().is_some());
    }

    #[test]
    fn an_update_keeps_the_area_label_and_creates_it_first() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        give_area(&fake, 1, "code", true);
        let before = label_creates(&fake);
        open(&fake).set_record_state(&r, State::Doing).unwrap();
        let labels = fake.issue(1).labels;
        assert!(
            labels.contains(&"fl:area/code".to_string())
                && labels.contains(&"fl:record/doing".to_string()),
            "{labels:?}"
        );
        assert_eq!(label_creates(&fake), before + 1, "created explicitly");
    }

    // Routing spec §2.5: a reference to a local record is resolved in the
    // local tier, never through GitHub.
    #[test]
    fn a_findings_reference_to_a_local_record_reads_back_without_a_lookup() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let f = t.add_finding(Finding::raise(p(), r, "rev", "claim")).unwrap();
        let local = Iri::parse("urn:uuid:00000000-0000-7000-8000-000000000042").unwrap();
        fake.web_edit(2, |i| {
            let (prose, mut m) = meta::parse_body(&i.body).unwrap();
            m.record = Some(RecordRef {
                id: local.clone(),
                node_id: None,
                title: Some("t".into()),
            });
            i.body = meta::render_body(&prose, &m);
        });
        let graphql = |fk: &FakeGithub| {
            fk.state().requests.iter().filter(|r| r.starts_with("POST /graphql")).count()
        };
        let before = graphql(&fake);
        let back = open(&fake).get_finding(&f).unwrap().unwrap();
        assert_eq!(back.record.iri(), &local);
        assert_eq!(back.claim, "claim");
        assert_eq!(graphql(&fake), before, "no node lookup for a local record");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-github --lib -- meta::tests:: tracker::tests::a_repair_re tracker::tests::an_update_keeps tracker::tests::a_findings_reference`
Expected: FAIL to compile — `RecordRef` has no `title`, `Meta` no `area`, `labels_after` takes three arguments, `area_label` and `sealed` do not exist.

- [ ] **Step 3: Implement**

In `crates/github/src/meta.rs`, after `pub const FL_FORMAT: u64 = 1;`, add:

```rust
/// The format of a block that carries an area or a reference to a local
/// record (routing spec decision 14). An older fl reads format 1 only, so
/// it refuses such an issue as a newer format instead of reading half of it.
pub const FL_FORMAT_ROUTED: u64 = 2;
/// The start of an area's label, `fl:area/<name>` (routing spec §1.1).
pub const AREA_LABEL_PREFIX: &str = "fl:area/";
```

Replace `RecordRef` with:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordRef {
    pub id: Iri,
    /// The record's issue node id (spec §2.3). `None` for a record in the
    /// project's local tier (routing spec §2.5), which no issue holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// A local record's title, which the issue shows beside its IRI: a
    /// reader on GitHub cannot open a local item. `None` for a record on
    /// GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl RecordRef {
    /// Whether this names a record in the local tier.
    pub fn is_local(&self) -> bool {
        self.node_id.is_none()
    }
}
```

In `Meta`, after `pub project: ProjectId,`, add:

```rust
    /// The item's area (routing spec §1.1), fixed for its life; its
    /// `fl:area/<name>` label is rewritten from this. Skipped when absent,
    /// so a block without one is written byte for byte as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
```

In `Meta::new`, add `area: None,` after `project,`. After `impl Meta`'s `new`, add:

```rust
    /// The format this block is written in: [`FL_FORMAT_ROUTED`] when it
    /// carries an area or a reference to a local record, else
    /// [`FL_FORMAT`], which every older fl reads.
    pub fn required_format(&self) -> u64 {
        let local_record = self.record.as_ref().is_some_and(RecordRef::is_local);
        if self.area.is_some() || local_record {
            FL_FORMAT_ROUTED
        } else {
            FL_FORMAT
        }
    }

    /// This block with `fl_format` set from its fields. ⚠ Never set the
    /// field by hand: a block whose format disagrees with its fields reads
    /// as damaged, and a remembered copy that disagrees with the issue reads
    /// as a conflict.
    pub fn sealed(mut self) -> Self {
        self.fl_format = self.required_format();
        self
    }
```

After `labels_after`'s neighbour `state_label`, add:

```rust
/// An area's label (routing spec §1.1).
pub fn area_label(area: &str) -> String {
    format!("{AREA_LABEL_PREFIX}{area}")
}
```

Replace `labels_after` with:

```rust
/// An issue's labels after fl writes it: every label that is not fl's, then
/// this kind's two, then the area's — all from the block (spec §3.3; routing
/// spec §1.1: fl replaces only its own labels, and the area label is one).
pub fn labels_after(
    current: &[String],
    kind: ItemKind,
    state: &str,
    area: Option<&str>,
) -> Vec<String> {
    let mut out: Vec<String> = current
        .iter()
        .filter(|l| !l.starts_with("fl:"))
        .cloned()
        .collect();
    out.push(kind_label(kind));
    out.push(state_label(kind, state));
    if let Some(a) = area {
        out.push(area_label(a));
    }
    out
}
```

Before `render_body`, add:

```rust
/// The line an issue shows for a finding whose record is in the local tier
/// (routing spec §2.5): the record's title and IRI as text, escaped as a
/// ledger comment escapes a name, so neither mentions anyone, links an issue
/// or opens a tag. `None` when the block names no local record.
pub fn record_line(meta: &Meta) -> Option<String> {
    let r = meta.record.as_ref().filter(|r| r.is_local())?;
    Some(format!(
        "Record: {} — {}, held in the local tier, not on GitHub.",
        crate::ledger::render::escape(r.title.as_deref().unwrap_or("")),
        crate::ledger::render::escape(r.id.as_str())
    ))
}
```

Replace `render_body` with:

```rust
/// The prose, the line naming a local record when there is one, then the
/// block, sealed. ⚠ `<` and `>` are escaped inside the JSON so no field
/// value can end the HTML comment or open a second block. They occur only
/// inside JSON strings, where `<`/`>` are the same text.
pub fn render_body(prose: &str, meta: &Meta) -> String {
    let meta = meta.clone().sealed();
    let json = serde_json::to_string(&meta)
        .expect("a Meta always serializes")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
    let block = format!("{META_OPEN}\n{json}\n{META_CLOSE}");
    let shown = match record_line(&meta) {
        Some(line) if prose.is_empty() => line,
        Some(line) => format!("{prose}\n\n{line}"),
        None => prose.to_string(),
    };
    if shown.is_empty() {
        format!("{block}\n")
    } else {
        format!("{shown}\n\n{block}\n")
    }
}
```

In `BodyError`'s `Display`, replace the `UnknownFormat` arm with:

```rust
            BodyError::UnknownFormat(n) => write!(
                f,
                "has an fl block of format {n}, and this fl reads formats {FL_FORMAT} to \
                 {FL_FORMAT_ROUTED}: upgrade fl to read it"
            ),
```

In `parse_body`, replace `Some(FL_FORMAT) => {}` with `Some(FL_FORMAT | FL_FORMAT_ROUTED) => {}`, and replace everything from `let meta: Meta =` to the end of the function with:

```rust
    let meta: Meta =
        serde_json::from_value(loose).map_err(|e| BodyError::Damaged(e.to_string()))?;
    if meta.fl_format != meta.required_format() {
        return Err(BodyError::Damaged(format!(
            "its `fl_format` is {}, but its fields are format {}",
            meta.fl_format,
            meta.required_format()
        )));
    }
    if !rest[end + META_CLOSE.len()..].trim().is_empty() {
        return Err(BodyError::Damaged("text follows the block".into()));
    }
    let prose = body[..at].trim_end();
    // The line fl writes for a local record is not part of the prose.
    let prose = match record_line(&meta) {
        Some(line) => prose
            .strip_suffix(line.as_str())
            .map_or(prose, str::trim_end),
        None => prose,
    };
    Ok((prose.to_string(), meta))
}
```

In `read_item`, replace the `stray` computation and its `if` with:

```rust
    let own = kind_label(kind);
    let stray: Vec<&&str> = fl
        .iter()
        .filter(|l| {
            **l != own && !l.starts_with(prefix.as_str()) && !l.starts_with(AREA_LABEL_PREFIX)
        })
        .collect();
    if !stray.is_empty() {
        problems.push(format!("it also carries {stray:?}"));
    }
    // Routing spec §1.1: the block is the truth; an area label that is
    // missing, extra or another area's is diverged.
    let areas: Vec<&str> = fl
        .iter()
        .copied()
        .filter(|l| l.starts_with(AREA_LABEL_PREFIX))
        .collect();
    let want = meta.area.as_deref().map(area_label);
    match (want.as_deref(), areas.as_slice()) {
        (None, []) => {}
        (Some(w), [one]) if *one == w => {}
        (Some(w), []) => {
            problems.push(format!("it has no `{w}` label, which its block's area needs"))
        }
        (w, found) => problems.push(format!(
            "its area labels {found:?} do not match its block's area ({})",
            w.unwrap_or("none")
        )),
    }
```

In `crates/github/src/tracker.rs`:

* In `GithubTracker`, replace the field `labels_ready: Cell<bool>,` with:

```rust
    /// Every fl label this process knows exists in the repository — the
    /// kind and state labels, and each area's (spec §3.3; routing spec
    /// §1.1) — so each is listed and created at most once per process.
    labels_ready: RefCell<BTreeSet<String>>,
```

  and in `open`, `labels_ready: Cell::new(false),` with `labels_ready: RefCell::new(BTreeSet::new()),`. Drop `Cell` from `use std::cell::{Cell, RefCell};` if nothing else uses it.

* Replace `ensure_labels` with:

```rust
    /// Create every fl label that is missing — the kind and state labels,
    /// and `area`'s when given — explicitly, never as a side effect of an
    /// issue write (spec §3.3). A label this process already knows exists is
    /// not looked for again.
    fn ensure_labels(&self, area: Option<&str>) -> Result<(), StoreError> {
        let mut wanted = meta::all_labels();
        if let Some(a) = area {
            wanted.push(meta::area_label(a));
        }
        if wanted.iter().all(|l| self.labels_ready.borrow().contains(l)) {
            return Ok(());
        }
        let have: BTreeSet<String> = self
            .client
            .get_all(&self.path("/labels?per_page=100"))?
            .iter()
            .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
            .collect();
        for name in &wanted {
            if have.contains(name) {
                continue;
            }
            let body = json!({"name": name, "color": "5319e7", "description": "managed by fl"});
            let r = self
                .client
                .send(Method::Post, &self.path("/labels"), Some(&body))?;
            if r.status != 201 {
                return Err(backend(format!(
                    "GitHub answered {} when fl created the label `{name}`; retry",
                    r.status
                )));
            }
        }
        self.labels_ready.borrow_mut().extend(wanted);
        Ok(())
    }
```

* In `create`, `self.ensure_labels()?;` becomes `self.ensure_labels(None)?;` (Task 2 passes the area).
* In `update`, `self.ensure_labels()?;` becomes `self.ensure_labels(None)?;`; after `change(&mut meta, &mut prose, &mut title)?;` add `self.ensure_labels(meta.area.as_deref())?;`; and the labels line becomes `let labels = meta::labels_after(&issue.labels, kind, &meta.state, meta.area.as_deref());`.
* In `repair`, `self.ensure_labels()?;` becomes `self.ensure_labels(None)?;`; after the `if !meta.kind.valid_state(&meta.state) { … }` refusal add `self.ensure_labels(meta.area.as_deref())?;`; and the labels line becomes `let labels = meta::labels_after(&issue.labels, meta.kind, &meta.state, meta.area.as_deref());`.
* In `current_ref`, before the first `if let`, add:

```rust
        // Routing spec §2.5: a record in the local tier is resolved there,
        // by its IRI; GitHub holds no node for it.
        let Some(node_id) = &r.node_id else {
            return Ok(r.id.clone());
        };
```

  and in the GraphQL variables use `json!({ "id": node_id })`.
* In `add_finding`, the reference becomes:

```rust
        meta.record = Some(RecordRef {
            id: record.url.clone(),
            node_id: Some(record.node_id.clone()),
            title: None,
        });
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github --lib -- meta::tests:: tracker::tests::`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Each run with `cargo test -p fl-github --lib -- meta::tests::` unless named:

1. `required_format`'s area conjunct: replace `self.area.is_some() || local_record` with `local_record` → `the_block_is_format_2_exactly_when_it_carries_an_area_or_a_local_record` red.
2. Its local-record conjunct: replace it with `self.area.is_some()` → the same test red.
3. `render_body` seals: serialize `meta` without `.sealed()` (keep the clone) → the same test red (format 7).
4. `parse_body` reads format 2: replace `Some(FL_FORMAT | FL_FORMAT_ROUTED)` with `Some(FL_FORMAT)` → the same test red.
5. The format-consistency check: delete it → `a_block_whose_format_disagrees_with_its_fields_is_damaged` red.
6. `record_line`'s filter: drop `.filter(|r| r.is_local())` → `the_block_is_format_2…` red (a GitHub reference shows a line).
7. The title is escaped: write `r.title.as_deref().unwrap_or("")` unescaped → `a_local_record_reference_shows_as_plain_text_and_reads_back_as_the_claim` red.
8. The read strips the line: replace the `match record_line(&meta)` with `prose` → the same test red.
9. An empty claim: make the `Some(line) if prose.is_empty()` arm `format!("{prose}\n\n{line}")` → the same test red (its last assertion).
10. `labels_after` pushes the area: delete the `if let Some(a)` → `every_rewrite_keeps_the_area_label_from_the_block` red.
11. `read_item`'s stray filter: drop `&& !l.starts_with(AREA_LABEL_PREFIX)` → `an_area_label_that_is_missing_or_differs_from_the_block_is_diverged` red (its first assertion).
12. The matching arm: replace `if *one == w` with `if true` → the same test red (`fl:area/design`).
13. The missing arm: make `(Some(w), []) => {}` → the same test red.
14. The other arm: make `(w, found) => {}` → the same test red.
15. `ensure_labels` adds the area: delete the `if let Some(a)` → `cargo test -p fl-github --lib tracker::tests::a_repair_restores_a_missing_area_label_creating_it_first` red.
16. `repair`'s `ensure_labels(meta.area.as_deref())`: delete it → the same test red (no label created).
17. `repair` passes the area to `labels_after`: pass `None` → the same test red.
17a. `labels_after` drops every fl label it does not write, a wrong area label included: make its filter keep `fl:area/` labels (`!l.starts_with("fl:") || l.starts_with(AREA_LABEL_PREFIX)`) → `cargo test -p fl-github --lib tracker::tests::a_repair_replaces_a_wrong_area_label_with_the_blocks` red, and `meta::tests::every_rewrite_keeps_the_area_label_from_the_block` red.
18. `update`'s `ensure_labels(meta.area.as_deref())`: delete it → `cargo test -p fl-github --lib tracker::tests::an_update_keeps_the_area_label_and_creates_it_first` red.
19. `update` passes the area: pass `None` → the same test red.
20. `current_ref`'s local branch: replace `return Ok(r.id.clone());` with `return Err(StoreError::Deleted(r.id.clone()));` → `cargo test -p fl-github --lib tracker::tests::a_findings_reference_to_a_local_record_reads_back_without_a_lookup` red.
21. `ensure_labels`'s early return: make the `all(…)` check `false` → not observable through results (every label is found and nothing created); `an_area_label_is_created_once_per_process` (Task 2) pins the request count. Not a guard here.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/github/src/meta.rs crates/github/src/tracker.rs
git commit -m "feat(github): the block's area, a local record reference, and fl:area labels

Meta gains area; a reference to a local record has no node_id and carries
its title, which the issue shows as escaped text. fl_format is computed:
2 exactly when the block carries an area or a local reference. The area
label is one of fl's: every rewrite keeps it, a missing or wrong one reads
as diverged, and update and repair create it before they write. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 2: Records and findings carry an area, in every store

`Record` and `Finding` gain `area` (routing spec §1.1). `Tracker` gains the required `add_record_with_area`; `add_record` stays, passing `None` (plan ruling 1). Every implementation and wrapper changes with the trait. The local store keeps the area with the item and raises itself to format 5 in the same write (§1.4); the GitHub tracker writes it into the block and as a label, created on first use.

**Blast radius:** the `Tracker` trait — `MemStore`, `RedbStore`, `GithubTracker`, `CatalogChecked`, `Journal` (`fl-exec`), and the test doubles `NeverAsked` (`core/src/store.rs`) and `BrokenStore` (`exec/src/evaluate.rs`). `add_record`'s 168 callers are unchanged. The local stores' `update_finding` now keeps the stored record, raiser, security mark and area, as the GitHub tracker does (plan ruling 29): no caller changes them today, so no behaviour a test or command sees changes. `RedbStore::open` now accepts format 5. `insert_new` and `insert_new_with_id` gain a parameter (their five callers and two test calls). The conformance tracker suite gains one case, which every fixture runs.

**Files:**
- Modify: `crates/core/src/model.rs` (`Record`), `crates/core/src/finding.rs` (`Finding`, `raise`)
- Modify: `crates/core/src/store.rs` (`Tracker`, `CatalogChecked`, `NeverAsked`)
- Modify: `crates/core/src/mem.rs` (`add_record_with_area`)
- Modify: `crates/core/src/conformance.rs` (`TRACKER_CASES`, `tracker`, a new case)
- Modify: `crates/core/tests/wire_refs.rs`, `crates/exec/src/decision.rs` (tests: `area: None`)
- Modify: `crates/exec/src/journal.rs`, `crates/exec/src/evaluate.rs` (`BrokenStore`)
- Modify: `crates/store/src/lib.rs` (`FORMAT_WITH_ROUTING`, `open`, `insert_new`, `insert_new_with_id`, `add_record_with_area`, `add_finding`; tests)
- Modify: `crates/github/src/tracker.rs` (`create`, `remember`, `add_record_with_area`, `add_finding`, `record_from`, `finding_from`; tests)
- Modify: `docs/github-tracker.md` ("What an issue looks like")

**Interfaces:**
- Consumes: `meta::labels_after(…, area)`, `Meta.area`, `Meta::sealed`, `ensure_labels(area)` (Task 1).
- Produces: `Record.area: Option<String>`, `Finding.area: Option<String>` (both `#[serde(default, skip_serializing_if = "Option::is_none")]`); `Tracker::add_record_with_area(&self, project: &ProjectId, title: &str, area: Option<&str>) -> Result<RecordId, StoreError>` (required); `Tracker::add_record` (provided); `fl_store::FORMAT_WITH_ROUTING: u64 = 5`; conformance cases `an_area_given_at_creation_reads_back` and `an_update_keeps_what_a_finding_was_raised_with` (`TRACKER_CASES = 14`).

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/conformance.rs`, set `const TRACKER_CASES: usize = 14;`, add `an_area_given_at_creation_reads_back,` and `an_update_keeps_what_a_finding_was_raised_with,` as the last two entries of `tracker`'s list, and after `update_finding_keeps_the_stored_aliases_whatever_the_caller_holds` add:

```rust
/// Routing spec §1.1: an area given when an item is made is stored with it
/// and reads back.
fn an_area_given_at_creation_reads_back(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles
        .tracker
        .add_record_with_area(&p, "t", Some("code"))
        .unwrap();
    assert_eq!(
        roles.tracker.get_record(&r).unwrap().unwrap().area.as_deref(),
        Some("code")
    );
    let mut f = Finding::raise(p, r, "reviewer", "claim");
    f.area = Some("code".into());
    let id = roles.tracker.add_finding(f).unwrap();
    assert_eq!(
        roles.tracker.get_finding(&id).unwrap().unwrap().area.as_deref(),
        Some("code")
    );
}

/// Routing spec §1.1: an item keeps its area for its whole life — and a
/// finding its record, its raiser and its security mark: an update never
/// takes them from the caller.
fn an_update_keeps_what_a_finding_was_raised_with(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/p").unwrap();
    let r = roles
        .tracker
        .add_record_with_area(&p, "t", Some("code"))
        .unwrap();
    let other = roles
        .tracker
        .add_record_with_area(&p, "u", Some("code"))
        .unwrap();
    let mut f = Finding::raise(p, r.clone(), "reviewer", "claim");
    f.area = Some("code".into());
    let id = roles.tracker.add_finding(f).unwrap();
    let mut changed = roles.tracker.get_finding(&id).unwrap().unwrap();
    changed.area = Some("tests".into());
    changed.record = other;
    changed.security = true;
    changed.raised_by = "someone else".into();
    changed.withdraw("no").unwrap();
    roles.tracker.update_finding(&changed).unwrap();
    let back = roles.tracker.get_finding(&id).unwrap().unwrap();
    assert_eq!(back.state, FindingState::Withdrawn, "the caller's state is written");
    assert_eq!(
        (back.area.as_deref(), back.record, back.security, back.raised_by.as_str()),
        (Some("code"), r, false, "reviewer")
    );
}
```

In `crates/core/src/finding.rs`, inside `mod tests`, add:

```rust
    // Routing spec §1.1: an item written before areas reads as having none,
    // and an item without one is stored as before.
    #[test]
    fn a_finding_without_an_area_is_stored_as_before_and_reads_as_none() {
        let f = Finding::raise(ProjectId(seq_iri(1)), RecordId(seq_iri(2)), "a", "c");
        assert_eq!(f.area, None, "raise sets no area");
        let v = serde_json::to_value(&f).unwrap();
        assert!(v.get("area").is_none(), "skipped when absent: {v}");
        let back: Finding = serde_json::from_value(v).unwrap();
        assert_eq!(back.area, None);
    }
```

In `crates/core/src/model.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn a_record_without_an_area_is_stored_as_before_and_reads_as_none() {
        let r = Record {
            id: RecordId(seq_iri(1)),
            project: ProjectId(seq_iri(2)),
            title: "t".into(),
            state: State::Todo,
            also_known_as: vec![],
            area: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("area").is_none(), "skipped when absent: {v}");
        let back: Record = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }
```

In `crates/store/src/lib.rs`, inside `mod tests`, change `a_store_from_a_newer_fl_says_to_upgrade_not_to_start_a_new_store` to insert `FORMAT_WITH_ROUTING + 1` instead of `FORMAT_WITH_LEDGER_ROOT + 1`; in `a_format_1_store_is_refused_by_format_2`, the expected `newest: 4` becomes `newest: FORMAT_WITH_ROUTING`; and add:

```rust
    /// The format a store at `path` records.
    fn format_at(path: &std::path::Path) -> Option<u64> {
        let db = redb::Database::open(path).unwrap();
        let tx = db.begin_read().unwrap();
        let meta = tx.open_table(META).unwrap();
        meta.get(FORMAT_KEY).unwrap().map(|v| v.value())
    }

    // Routing spec §1.4: an older fl would read an item and drop its area,
    // so the first item with one raises the store to 5 — in the same write.
    // A store that never routes stays where it was.
    #[test]
    fn an_item_with_an_area_raises_the_store_to_format_5_and_one_without_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.redb");
        let r = {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let plain = s.add_record(&p, "t").unwrap();
            s.add_finding(Finding::raise(p.clone(), plain, "a", "c")).unwrap();
            drop(s);
            assert_eq!(format_at(&path), Some(FORMAT_VERSION));
            let s = RedbStore::open(&path).unwrap();
            s.add_record_with_area(&p, "u", Some("code")).unwrap()
        };
        assert_eq!(format_at(&path), Some(FORMAT_WITH_ROUTING));
        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.get_record(&r).unwrap().unwrap().area.as_deref(), Some("code"));

        let path = dir.path().join("b.redb");
        {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let rec = s.add_record(&p, "t").unwrap();
            let mut f = Finding::raise(p, rec, "a", "c");
            f.area = Some("design".into());
            s.add_finding(f).unwrap();
        }
        assert_eq!(format_at(&path), Some(FORMAT_WITH_ROUTING), "a finding raises it too");
    }
```

In `crates/github/src/tracker.rs`, inside `mod tests`, after `a_findings_reference_to_a_local_record_reads_back_without_a_lookup`, add:

```rust
    // Routing spec §1.1: the area is a field of the block and an fl label,
    // created the first time an item with that area is made here.
    #[test]
    fn an_item_made_with_an_area_carries_it_in_its_block_and_as_a_label() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record_with_area(&p(), "fix it", Some("code")).unwrap();
        let issue = fake.issue(1);
        assert_eq!(issue.labels, vec!["fl:record", "fl:record/todo", "fl:area/code"]);
        assert!(
            issue.body.contains("\"fl_format\":2") && issue.body.contains("\"area\":\"code\""),
            "{}",
            issue.body
        );
        // ⚠ Counted, not looked up: the fake also records a label an issue
        // write applies, so only the create request proves fl made it first.
        assert_eq!(
            label_creates(&fake),
            meta::all_labels().len() + 1,
            "every kind and state label, and the area's"
        );
        assert_eq!(t.get_record(&r).unwrap().unwrap().area.as_deref(), Some("code"));
        // What fl remembers of its create is sealed as the block is, so the
        // next write is not a conflict.
        t.set_record_state(&r, State::Doing).unwrap();
        assert!(fake.issue(1).labels.contains(&"fl:area/code".to_string()));
        let mut f = Finding::raise(p(), r, "rev", "claim");
        f.area = Some("design".into());
        let fid = t.add_finding(f).unwrap();
        assert!(fake.issue(2).labels.contains(&"fl:area/design".to_string()));
        assert_eq!(t.get_finding(&fid).unwrap().unwrap().area.as_deref(), Some("design"));
    }

    #[test]
    fn an_area_label_is_created_once_per_process() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        t.add_record_with_area(&p(), "a", Some("code")).unwrap();
        let created = label_creates(&fake);
        let lists = |f: &FakeGithub| {
            f.state()
                .requests
                .iter()
                .filter(|r| r.starts_with("GET /repos/acme/widgets/labels"))
                .count()
        };
        let listed = lists(&fake);
        t.add_record_with_area(&p(), "b", Some("code")).unwrap();
        assert_eq!(label_creates(&fake), created, "not created twice");
        assert_eq!(lists(&fake), listed, "a label this process knows is not listed again");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-core --lib` and `cargo test -p fl-github --lib tracker::tests::an_item_made_with_an_area`
Expected: FAIL to compile — `Record` and `Finding` have no `area`, `add_record_with_area` does not exist.

- [ ] **Step 3: Implement**

In `crates/core/src/model.rs`, at the end of `Record`, add:

```rust
    /// The record's area (routing spec §1.1), fixed for its whole life.
    /// `None` for a record made before areas, or outside a routed project.
    /// Skipped when absent, so such a record is stored exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
```

In `crates/core/src/finding.rs`, at the end of `Finding`, add the same field with the same doc comment (reading "The finding's area"), and in `Finding::raise` add `area: None,`.

In `crates/core/src/store.rs`, replace `Tracker::add_record`'s declaration with:

```rust
    /// A record with no area: [`Tracker::add_record_with_area`] with
    /// `None`. A routed project refuses it (routing spec §2.1).
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        self.add_record_with_area(project, title, None)
    }
    /// A record in `area` (routing spec §1.1), stored with it for good.
    ///
    /// ⚠ The one method an implementation writes: `add_record` reaches it,
    /// so no wrapper can drop the area.
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError>;
```

In `CatalogChecked`'s `Tracker` impl, replace `add_record` with:

```rust
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError> {
        self.project(project)?;
        self.tracker.add_record_with_area(project, title, area)
    }
```

In `NeverAsked` (tests), rename `add_record` to `add_record_with_area` with the extra `_: Option<&str>` parameter, keeping its `unreachable!`.

In `crates/core/src/mem.rs`, replace `add_record` with `add_record_with_area` (same body), adding the parameter `area: Option<&str>` and the field `area: area.map(str::to_string),` to the `Record` it inserts. In `update_finding`, replace the lines from `// The stored `also_known_as` is kept…` to `s.findings.insert(target, stored);` with:

```rust
        // The stored `also_known_as` is kept and the caller's ignored (see
        // the trait): only `add_alias` adds a name. The record, the raiser,
        // the security mark and the area are fixed when the finding is
        // raised (routing spec §1.1; GitHub tracker spec §6).
        let kept = s.findings[&target].clone();
        let mut stored = finding.clone();
        stored.id = FindingId(target.clone());
        stored.also_known_as = kept.also_known_as;
        stored.record = kept.record;
        stored.raised_by = kept.raised_by;
        stored.security = kept.security;
        stored.area = kept.area;
        s.findings.insert(target, stored);
```

In `crates/exec/src/journal.rs`, replace `add_record` with:

```rust
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError> {
        self.store.add_record_with_area(project, title, area)
    }
```

In `crates/exec/src/evaluate.rs`, in `BrokenStore`'s `Tracker` impl, rename `add_record` to `add_record_with_area` with the extra `_: Option<&str>` parameter, still `Err(broken())`. In `crates/exec/src/decision.rs` (the test's `Record`) and `crates/core/tests/wire_refs.rs` (`record`), add `area: None,`.

In `crates/store/src/lib.rs`, after `FORMAT_WITH_LEDGER_ROOT`, add:

```rust
/// ⚠ The format of a store that holds a routing map or an item with an
/// area (routing spec §1.4). An older fl would read such an item and drop
/// its area — or route nothing — so the first such write raises the store
/// to 5 in the same transaction, and an older fl refuses it. This build
/// opens 2 to 5. A store that never routes stays where it was.
pub const FORMAT_WITH_ROUTING: u64 = 5;
```

In `RedbStore::open`, add `|| v == FORMAT_WITH_ROUTING` to the accepted versions and make `newest: FORMAT_WITH_ROUTING`. Give `insert_new` and `insert_new_with_id` a parameter `raise: Option<u64>` after `table` (`insert_new` passes it on), and in `insert_new_with_id`, after the row is inserted and before `tx.commit()`, add:

```rust
        if let Some(to) = raise {
            raise_format(&tx, to)?;
        }
```

`add_project` and `add_gate` pass `None`, as do the two `insert_new_with_id` calls in the tests. Replace `add_record` with:

```rust
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let raise = area.map(|_| FORMAT_WITH_ROUTING);
        let id = self.insert_new(Kind::Record, RECORDS, raise, |id| Record {
            id: RecordId(id),
            project: project.clone(),
            title: title.to_string(),
            state: State::Todo,
            also_known_as: vec![],
            area: area.map(str::to_string),
        })?;
        Ok(RecordId(id))
    }
```

In `add_finding`, before `let id = self.insert_new(…)`, add `let raise = finding.area.as_ref().map(|_| FORMAT_WITH_ROUTING);` and pass `raise` as `insert_new`'s third argument. In `update_finding`, replace the lines from `let primary = stored.id.clone();` to the `put_json` with:

```rust
        let primary = stored.id.clone();
        let kept = std::mem::replace(&mut stored, finding.clone());
        stored.id = primary.clone();
        stored.also_known_as = kept.also_known_as;
        // Fixed when the finding is raised (routing spec §1.1; GitHub
        // tracker spec §6): the caller's copy never changes them.
        stored.record = kept.record;
        stored.raised_by = kept.raised_by;
        stored.security = kept.security;
        stored.area = kept.area;
        self.put_json(FINDINGS, primary.iri(), &stored)
```

In `crates/github/src/tracker.rs`:

* In `create`, `self.ensure_labels(None)?;` becomes `self.ensure_labels(meta.area.as_deref())?;`, and the labels become `let labels = meta::labels_after(&[], kind, &meta.state, meta.area.as_deref());`.
* `remember` stores `meta.clone().sealed()` instead of `meta.clone()`.
* Replace `add_record` with:

```rust
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError> {
        let mut meta = Meta::new(ItemKind::Record, State::Todo.as_wire(), project.clone());
        meta.area = area.map(str::to_string);
        Ok(RecordId(
            self.create(ItemKind::Record, title, "", &meta)?.url,
        ))
    }
```

* In `add_finding`, after `meta.security = finding.security;`, add `meta.area = finding.area.clone();`.
* In `record_from` and `finding_from`, add `area: meta.area.clone(),`.

In `docs/github-tracker.md`, section "What an issue looks like", replace the paragraph beginning "fl creates the labels it needs" with:

```markdown
fl creates the labels it needs — one kind label and one label per state, for records and for
findings — the first time a command writes, with the description "managed by fl". It never
relies on GitHub creating a label as a side effect of a write.

An item of a project that routes its items between its local store and GitHub carries a third
fl label, `fl:area/<name>`, naming its area. fl creates an area's label the first time it makes
an item with that area, keeps it through every write and repair, and never deletes it. The area
is also a field of the block below; a block that carries one is written as `fl_format` 2, which
an older fl refuses as a newer format rather than reading half of it.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS — the conformance suite now runs 14 tracker cases over `MemStore`, `RedbStore` and the GitHub tracker (which already kept those fields).

- [ ] **Step 5: Mutation checks**

1. `Record.area`'s `skip_serializing_if`: remove it → `cargo test -p fl-core --lib model::tests::a_record_without_an_area_is_stored_as_before_and_reads_as_none` red.
2. `Finding.area`'s: remove it → `cargo test -p fl-core --lib finding::tests::a_finding_without_an_area_is_stored_as_before_and_reads_as_none` red.
3. `MemStore` stores the area: write `area: None` → `cargo test -p fl-core --lib mem::tests::mem_store_meets_every_role_contract` red.
4. `CatalogChecked` forwards the area: pass `None` → `cargo test -p fl-github --lib tracker::tests::contract::the_github_tracker_meets_the_tracker_contract` red.
5. `RedbStore` stores the area: write `area: None` → `cargo test -p fl-store --lib tests::redb_store_meets_every_role_contract` red.
6. The record's raise: make `raise` `None` in `add_record_with_area` → `cargo test -p fl-store --lib tests::an_item_with_an_area_raises_the_store_to_format_5_and_one_without_does_not` red.
7. The finding's raise: make it `None` in `add_finding` → the same test red ("a finding raises it too").
8. `insert_new_with_id` honours `raise`: delete the `if let Some(to)` → the same test red.
9. `open` accepts 5: drop `|| v == FORMAT_WITH_ROUTING` → the same test red (the reopen).
10. `create` labels the area: pass `None` to `labels_after` → `cargo test -p fl-github --lib tracker::tests::an_item_made_with_an_area_carries_it_in_its_block_and_as_a_label` red.
11. `create` creates the area label first: pass `None` to `ensure_labels` → the same test red (one label create short).
12. `remember` seals: store `meta.clone()` → the same test red (`set_record_state` is a conflict).
13. `add_finding` writes the area: delete `meta.area = finding.area.clone();` → the same test red.
14. `record_from` / `finding_from` read the area: write `area: None` in each → the same test red.
15. `ensure_labels`' early return (Task 1): make the `all(…)` check `false` → `cargo test -p fl-github --lib tracker::tests::an_area_label_is_created_once_per_process` red.
16. `update_finding` keeps each fixed field, in each local store: delete `stored.area = kept.area;`, then `stored.record = …`, `stored.raised_by = …`, `stored.security = …`, one at a time, in `MemStore` and in `RedbStore` → `cargo test -p fl-core --lib mem::tests::mem_store_meets_every_role_contract` red, and `cargo test -p fl-store --lib tests::redb_store_meets_every_role_contract` red (`an_update_keeps_what_a_finding_was_raised_with`).

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/model.rs crates/core/src/finding.rs crates/core/src/store.rs crates/core/src/mem.rs crates/core/src/conformance.rs crates/core/tests/wire_refs.rs crates/exec/src/journal.rs crates/exec/src/evaluate.rs crates/exec/src/decision.rs crates/store/src/lib.rs crates/github/src/tracker.rs docs/github-tracker.md
git commit -m "feat: records and findings carry an area, in every store

Record and Finding gain area, skipped when absent. Tracker's required
method is add_record_with_area; add_record passes None. The local store
keeps the area and raises itself to format 5 in the same write; the GitHub
tracker writes it into the block and as fl:area/<name>, created on first
use. Two more conformance cases: an area reads back, and a local store's
update keeps a finding's fixed fields. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 3: The routing map, in the core and in the local store

The map — area → `{tier, sensitive}` — and its rules (routing spec §1.2): area names, the starting set the first `set` writes, a change to one area, and a removal. The local store keeps a project's map, refuses to change an imported project's, and raises itself to format 5 when it writes one. Whether a store is routed never depends on what its map holds (§1.3). A routed store holds exactly one project (spec decision 20, plan ruling 9): the store refuses a map while it holds another project, and another project once it is routed.

**Blast radius:** new module and table. `MemStore` gains a field and an inherent method. `RedbStore::add_project` refuses in a routed store — no store is routed before this task, so no existing caller sees it. `MemStore` keeps no one-project rule: it is a test store, and the router's tests hold several projects in one.

**Files:**
- Create: `crates/core/src/routing.rs`
- Modify: `crates/core/src/lib.rs` (module, re-exports)
- Modify: `crates/core/src/mem.rs` (`Inner.routing`, `set_routes`, `Routes`)
- Modify: `crates/store/src/lib.rs` (`ROUTING`, `Routes`, `set_routes`, `holds_routing`; tests)

**Interfaces:**
- Consumes: `crate::wire::wire_names!`, `wire_parse!`; `RedbStore::refuse_if_imported`, `raise_format`, `FORMAT_WITH_ROUTING` (Task 2).
- Produces (in `fl_core::routing`, re-exported at the crate root): `enum Tier { Local, Github }` (wire `local`, `github`; `Tier::ALL`, `as_wire`, `from_wire`, `wire_values`); `pub fn area_name(name: &str) -> Result<(), String>`; `struct AreaRoute { pub area: String, pub tier: Tier, pub sensitive: bool }`; `struct RoutingMap { pub areas: Vec<AreaRoute> }` (`#[serde(transparent)]`) with `starting() -> RoutingMap`, `route(&self, &str) -> Option<&AreaRoute>`, `declared(&self) -> Vec<String>`, `with(&self, &str, Tier, bool) -> RoutingMap`, `without(&self, &str) -> RoutingMap`, `check(&self) -> Result<(), String>`; `pub fn after_set(current: Option<&RoutingMap>, area: &str, tier: Tier, sensitive: Option<bool>) -> (RoutingMap, bool)` (`None` keeps the area's sensitivity, decision 22); `trait Routes { fn routes(&self, project: &ProjectId) -> Result<Option<RoutingMap>, StoreError>; }`. `MemStore::set_routes(&self, &ProjectId, &RoutingMap) -> Result<(), StoreError>`; `RedbStore::set_routes(&self, &ProjectId, &RoutingMap) -> Result<(), StoreError>`; `RedbStore::holds_routing(&self) -> Result<bool, StoreError>`; crate-private `RedbStore::refuse_a_second_project(&self, &ProjectId) -> Result<(), StoreError>` and `fn routed_store_is_taken() -> StoreError` (Task 4's import uses both). Unique phrases: `is not an area name`, `change the routing map of`, `needs a store of its own`.

- [ ] **Step 1: Write the module with its failing tests**

Create `crates/core/src/routing.rs`:

```rust
//! Two-tier routing (routing spec §1): a project's areas, the tier each one
//! routes a new item to, and whether it is sensitive.

use crate::ids::ProjectId;
use crate::store::StoreError;
use serde::{Deserialize, Serialize};

/// One of a routed project's two trackers (routing spec decision 8): the
/// local store, or the GitHub repository the machine's config binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Local,
    Github,
}

crate::wire::wire_names!(Tier as tier_wire {
    Local => "local",
    Github => "github",
});

// `--tier` and `fl routing set` take a tier by name.
crate::wire::wire_parse!(Tier as tier_parse);

/// The longest area name: GitHub's label limit is 50 characters, and
/// `fl:area/` takes 8 (routing spec §1.1).
pub const AREA_MAX: usize = 32;

/// Whether `name` can be an area: 1 to 32 of lowercase `a-z`, `0-9` and
/// `-` (routing spec §1.1). The `Err` says what an area name is.
pub fn area_name(name: &str) -> Result<(), String> {
    let chars = name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if name.is_empty() || name.len() > AREA_MAX || !chars {
        return Err(format!(
            "`{name}` is not an area name: 1 to {AREA_MAX} of lowercase letters, digits and `-`"
        ));
    }
    Ok(())
}

/// Where one area routes a new item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaRoute {
    pub area: String,
    pub tier: Tier,
    /// An item made in a sensitive area is treated as a security item
    /// (routing spec decision 13).
    pub sensitive: bool,
}

/// A project's routing map (routing spec §1.2), sorted by area, each area
/// once. ⚠ A list, not a map: the manifest hashes its bytes, and a list in
/// a fixed order serializes the same way every time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoutingMap {
    pub areas: Vec<AreaRoute>,
}

impl RoutingMap {
    /// What the project's first `fl routing set` writes before the area it
    /// names (routing spec §1.2): developer-level areas local, human-level
    /// ones on GitHub, `security` on GitHub and sensitive.
    pub fn starting() -> Self {
        let route = |area: &str, tier, sensitive| AreaRoute {
            area: area.to_string(),
            tier,
            sensitive,
        };
        Self {
            areas: vec![
                route("code", Tier::Local, false),
                route("design", Tier::Github, false),
                route("product", Tier::Github, false),
                route("security", Tier::Github, true),
                route("tests", Tier::Local, false),
            ],
        }
    }

    pub fn route(&self, area: &str) -> Option<&AreaRoute> {
        self.areas.iter().find(|a| a.area == area)
    }

    /// Every area this map declares, in order, for a refusal to list.
    pub fn declared(&self) -> Vec<String> {
        self.areas.iter().map(|a| a.area.clone()).collect()
    }

    /// This map with `area` routed to `tier` — added, or changed in place.
    pub fn with(&self, area: &str, tier: Tier, sensitive: bool) -> Self {
        let mut areas: Vec<AreaRoute> =
            self.areas.iter().filter(|a| a.area != area).cloned().collect();
        areas.push(AreaRoute {
            area: area.to_string(),
            tier,
            sensitive,
        });
        areas.sort_by(|a, b| a.area.cmp(&b.area));
        Self { areas }
    }

    /// This map without `area`.
    pub fn without(&self, area: &str) -> Self {
        Self {
            areas: self.areas.iter().filter(|a| a.area != area).cloned().collect(),
        }
    }

    /// Whether this map is one fl writes: every name an area name, sorted,
    /// each once. A map read from a manifest or a store is checked with
    /// this before it routes anything.
    pub fn check(&self) -> Result<(), String> {
        for a in &self.areas {
            area_name(&a.area)?;
        }
        for pair in self.areas.windows(2) {
            if pair[0].area >= pair[1].area {
                return Err(format!(
                    "its areas are not each listed once in order: `{}` then `{}`",
                    pair[0].area, pair[1].area
                ));
            }
        }
        Ok(())
    }
}

/// The map after `fl routing set <area> <tier>` (routing spec §1.2), and
/// whether this was the project's first set — which writes the starting
/// set first.
///
/// `sensitive`: `Some` sets the area's sensitivity; `None` keeps what the
/// map — on the first set, the starting set — says, and `false` for a new
/// area. ⚠ A set that only changes a tier never clears a sensitivity
/// (decision 22).
pub fn after_set(
    current: Option<&RoutingMap>,
    area: &str,
    tier: Tier,
    sensitive: Option<bool>,
) -> (RoutingMap, bool) {
    let (base, first) = match current {
        Some(map) => (map.clone(), false),
        None => (RoutingMap::starting(), true),
    };
    let sensitive =
        sensitive.unwrap_or_else(|| base.route(area).is_some_and(|r| r.sensitive));
    (base.with(area, tier, sensitive), first)
}

/// Where the router reads a project's routing map: the local store, which
/// holds the map it authored or imported (routing spec §1.2). `None`: the
/// project has no map.
pub trait Routes {
    fn routes(&self, project: &ProjectId) -> Result<Option<RoutingMap>, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_area_name_is_up_to_32_lowercase_letters_digits_and_hyphens() {
        for ok in ["code", "a", "x-1", "2fa", &"a".repeat(32)] {
            assert_eq!(area_name(ok), Ok(()), "{ok}");
        }
        for bad in ["", "Code", "a b", "a_b", "a/b", "é", "fl:x", &"a".repeat(33)] {
            let why = area_name(bad).expect_err(bad);
            assert!(why.contains("is not an area name"), "{why}");
        }
    }

    #[test]
    fn the_starting_set_routes_developer_areas_local_and_human_areas_to_github() {
        let s = RoutingMap::starting();
        let got: Vec<(&str, Tier, bool)> = s
            .areas
            .iter()
            .map(|a| (a.area.as_str(), a.tier, a.sensitive))
            .collect();
        assert_eq!(
            got,
            vec![
                ("code", Tier::Local, false),
                ("design", Tier::Github, false),
                ("product", Tier::Github, false),
                ("security", Tier::Github, true),
                ("tests", Tier::Local, false),
            ]
        );
        assert_eq!(s.check(), Ok(()));
    }

    #[test]
    fn the_first_set_writes_the_starting_set_first_and_a_later_one_changes_one_area() {
        let (first, was_first) = after_set(None, "ops", Tier::Github, None);
        assert!(was_first);
        assert_eq!(first.declared().len(), 6);
        assert_eq!(first.route("ops").unwrap().tier, Tier::Github);
        assert_eq!(first.route("code").unwrap().tier, Tier::Local);
        assert_eq!(first.check(), Ok(()), "kept in order");
        let (next, was_first) = after_set(Some(&first), "code", Tier::Github, Some(true));
        assert!(!was_first);
        let code = next.route("code").unwrap();
        assert_eq!((code.tier, code.sensitive), (Tier::Github, true));
        assert_eq!(next.declared(), first.declared(), "one area changed, none added");
        let (named, _) = after_set(None, "code", Tier::Github, None);
        assert_eq!(named.route("code").unwrap().tier, Tier::Github, "the set wins");
    }

    // Routing spec decision 22: a set that names no sensitivity keeps the
    // area's — a tier change never clears it — and only `Some(false)` does.
    #[test]
    fn a_set_that_names_no_sensitivity_keeps_the_areas() {
        let starting = RoutingMap::starting();
        let (moved, _) = after_set(Some(&starting), "security", Tier::Local, None);
        let security = moved.route("security").unwrap();
        assert_eq!((security.tier, security.sensitive), (Tier::Local, true));
        let (first, _) = after_set(None, "security", Tier::Local, None);
        assert!(first.route("security").unwrap().sensitive, "the starting set's, kept");
        let (new, _) = after_set(Some(&starting), "ops", Tier::Local, None);
        assert!(!new.route("ops").unwrap().sensitive, "a new area is not sensitive");
        let (cleared, _) = after_set(Some(&starting), "security", Tier::Github, Some(false));
        assert!(!cleared.route("security").unwrap().sensitive);
    }

    #[test]
    fn without_removes_one_area_only() {
        let m = RoutingMap::starting().without("design");
        assert_eq!(m.declared(), vec!["code", "product", "security", "tests"]);
    }

    #[test]
    fn a_map_out_of_order_with_an_area_twice_or_a_bad_name_is_refused() {
        let mut m = RoutingMap::starting();
        m.areas.swap(0, 1);
        assert!(m.check().unwrap_err().contains("in order"));
        let mut m = RoutingMap::starting();
        m.areas.insert(1, m.areas[0].clone());
        assert!(m.check().unwrap_err().contains("in order"));
        let mut m = RoutingMap::starting();
        m.areas[0].area = "Code".into();
        assert!(m.check().unwrap_err().contains("is not an area name"));
    }

    #[test]
    fn a_map_serializes_as_a_list_in_order() {
        let json = serde_json::to_string(&RoutingMap::starting().without("design")).unwrap();
        assert!(json.starts_with("[{\"area\":\"code\",\"tier\":\"local\",\"sensitive\":false}"), "{json}");
    }
}
```

In `crates/core/src/mem.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn a_memory_store_keeps_one_routing_map_per_project() {
        use crate::routing::{RoutingMap, Routes};
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let q = s.add_project("/q").unwrap();
        assert_eq!(s.routes(&p).unwrap(), None);
        s.set_routes(&p, &RoutingMap::starting()).unwrap();
        assert_eq!(s.routes(&p).unwrap(), Some(RoutingMap::starting()));
        assert_eq!(s.routes(&q).unwrap(), None, "per project");
        let err = s.routes(&ProjectId(seq_iri(99))).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
    }
```

In `crates/store/src/lib.rs`, inside `mod tests`, add:

```rust
    // Routing spec §1.2, §1.4: the map is kept, raising the store to 5 in
    // the same write; a store holds routing once its project has a map.
    #[test]
    fn a_routing_map_is_kept_and_raises_the_store_to_format_5() {
        use fl_core::routing::{RoutingMap, Routes};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.redb");
        let p = {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            assert!(!s.holds_routing().unwrap());
            assert_eq!(s.routes(&p).unwrap(), None, "no table yet: no map");
            s.set_routes(&p, &RoutingMap::starting()).unwrap();
            assert!(s.holds_routing().unwrap());
            p
        };
        assert_eq!(format_at(&path), Some(FORMAT_WITH_ROUTING));
        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.routes(&p).unwrap(), Some(RoutingMap::starting()));
        let err = s.routes(&ProjectId(fl_core::ids::seq_iri(99))).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
    }

    // Routing spec decision 20: handles are numbered per store, so a routed
    // store holds exactly one project — no map while another project is
    // there, and no other project once there is one.
    #[test]
    fn a_routed_store_holds_exactly_one_project() {
        use fl_core::routing::{RoutingMap, Routes};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("two.redb");
        {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let q = s.add_project("/q").unwrap();
            let err = s.set_routes(&p, &RoutingMap::starting()).unwrap_err().to_string();
            assert!(
                err.contains("needs a store of its own")
                    && err.contains(q.iri().as_str())
                    && err.contains("its own `store`"),
                "{err}"
            );
            assert_eq!(s.routes(&p).unwrap(), None, "nothing written");
            assert!(!s.holds_routing().unwrap());
        }
        assert_eq!(format_at(&path), Some(FORMAT_VERSION));
        let (s, _d) = fresh();
        let p = s.add_project("/p").unwrap();
        s.set_routes(&p, &RoutingMap::starting()).unwrap();
        let err = s.add_project("/q").unwrap_err().to_string();
        assert!(err.contains("needs a store of its own"), "{err}");
        assert_eq!(s.list_projects().unwrap().len(), 1);
    }

    #[test]
    fn a_map_that_is_not_valid_is_refused_before_anything_is_written() {
        use fl_core::routing::{RoutingMap, Routes};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.redb");
        {
            let s = RedbStore::open(&path).unwrap();
            let p = s.add_project("/p").unwrap();
            let mut bad = RoutingMap::starting();
            bad.areas[0].area = "Code".into();
            let err = s.set_routes(&p, &bad).unwrap_err().to_string();
            assert!(err.contains("is not an area name"), "{err}");
            assert_eq!(s.routes(&p).unwrap(), None);
        }
        assert_eq!(format_at(&path), Some(FORMAT_VERSION));
    }

    // The map is authored where the project is (routing spec §1.2): an
    // importing store's copy comes from the manifest only.
    #[test]
    fn an_imported_projects_routing_map_cannot_be_changed_here() {
        use fl_core::routing::RoutingMap;
        let (a, _ga, p, _, _) = authoring();
        let (b, _gb) = fresh();
        b.import_manifest(&a.export_manifest(&p, "c1", 7, None).unwrap(), "/x")
            .unwrap();
        let err = b.set_routes(&p, &RoutingMap::starting()).unwrap_err();
        assert!(matches!(err, StoreError::Imported { .. }), "{err:?}");
        assert!(err.to_string().contains("change the routing map of"), "{err}");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-core --lib` and `cargo test -p fl-store --lib`
Expected: FAIL to compile — `routing` is not a module, and `set_routes`, `holds_routing`, `Routes` do not exist.

- [ ] **Step 3: Implement**

In `crates/core/src/lib.rs`, add `pub mod routing;` after `pub mod model;`, and `pub use routing::{AreaRoute, Routes, RoutingMap, Tier};` after the `pub use model::…` line.

In `crates/core/src/mem.rs`: add `use crate::routing::{RoutingMap, Routes};`; add to `Inner`:

```rust
    /// project → its routing map (routing spec §1.2).
    routing: BTreeMap<Iri, RoutingMap>,
```

and after `impl Catalog for MemStore`, add:

```rust
impl MemStore {
    /// Write `project`'s routing map. A map that is not one fl writes is
    /// refused (`RoutingMap::check`).
    pub fn set_routes(&self, project: &ProjectId, map: &RoutingMap) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&project.0, Kind::Project)?;
        map.check()
            .map_err(|why| StoreError::Backend(format!("the routing map is not valid: {why}")))?;
        s.routing.insert(project.0.clone(), map.clone());
        Ok(())
    }
}

impl Routes for MemStore {
    fn routes(&self, project: &ProjectId) -> Result<Option<RoutingMap>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.routing.get(&project.0).cloned())
    }
}
```

In `crates/store/src/lib.rs`: add `use fl_core::routing::{RoutingMap, Routes};`; after `LEDGER_SEGMENTS`, add:

```rust
/// project → its routing map, as JSON (routing spec §1.2). Created by the
/// first map written; writing one raises the store to
/// [`FORMAT_WITH_ROUTING`] in the same transaction.
const ROUTING: TableDefinition<&str, &str> = TableDefinition::new("routing");
```

In `impl RedbStore`, after `holds_a_ledger_root`, add:

```rust
    /// Write `project`'s routing map (routing spec §1.2) — which this store
    /// must author, and which must be its only project (decision 20) — and
    /// raise the store to format 5, in one transaction.
    pub fn set_routes(&self, project: &ProjectId, map: &RoutingMap) -> Result<(), StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        self.refuse_if_imported(project, "change the routing map of")?;
        self.refuse_a_second_project(project)?;
        map.check()
            .map_err(|why| backend(format!("the routing map is not valid: {why}")))?;
        let json = serde_json::to_string(map).map_err(backend)?;
        let tx = self.db.begin_write().map_err(backend)?;
        tx.open_table(ROUTING)
            .map_err(backend)?
            .insert(project.iri().as_str(), json.as_str())
            .map_err(backend)?;
        raise_format(&tx, FORMAT_WITH_ROUTING)?;
        tx.commit().map_err(backend)
    }

    /// Routing spec decision 20: a routed store holds exactly one project,
    /// because handles are numbered per store. `Err` names the other
    /// project and the remedy.
    pub(crate) fn refuse_a_second_project(&self, project: &ProjectId) -> Result<(), StoreError> {
        match self.list_projects()?.into_iter().find(|p| p.id != *project) {
            Some(other) => Err(backend(format!(
                "this store also holds project {} (root {}), and a routed project needs a store \
                 of its own: handles are numbered per store, so one number would name items of \
                 both. Give the project its own store — a config entry with its own `store` — \
                 and route it there",
                other.id, other.root
            ))),
            None => Ok(()),
        }
    }

    /// Whether any project in this store has a routing map: the store is
    /// then routed (routing spec §1.3), whatever the map holds.
    pub fn holds_routing(&self) -> Result<bool, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(ROUTING) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(e) => return Err(backend(e)),
        };
        let any = table.iter().map_err(backend)?.next().is_some();
        Ok(any)
    }
```

After `drop_candidates`, add:

```rust
/// Routing spec decision 20: a store whose project is routed takes no
/// other project.
fn routed_store_is_taken() -> StoreError {
    backend(
        "this store holds a routed project, which needs a store of its own: handles are \
         numbered per store. Give the new project its own store — a config entry with its own \
         `store`",
    )
}
```

In `impl Catalog for RedbStore`, `add_project` starts:

```rust
    fn add_project(&self, root: &str) -> Result<ProjectId, StoreError> {
        // Routing spec decision 20: a routed store holds one project.
        if self.holds_routing()? {
            return Err(routed_store_is_taken());
        }
```

And after `impl Catalog for RedbStore`, add:

```rust
impl Routes for RedbStore {
    fn routes(&self, project: &ProjectId) -> Result<Option<RoutingMap>, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(ROUTING) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        let Some(v) = table.get(project.iri().as_str()).map_err(backend)? else {
            return Ok(None);
        };
        let map: RoutingMap = serde_json::from_str(v.value()).map_err(decode)?;
        // ⚠ A stored map routes nothing until it is one fl writes.
        map.check().map_err(decode)?;
        Ok(Some(map))
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-core --lib` and `cargo test -p fl-store --lib`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Each `fl-core` run with `cargo test -p fl-core --lib routing::tests::`:

1. `area_name`'s empty conjunct: drop `name.is_empty() ||` → `an_area_name_is_up_to_32_lowercase_letters_digits_and_hyphens` red.
2. Its length conjunct: drop `name.len() > AREA_MAX ||` → the same test red (33 characters).
3. Its character conjunct: drop `|| !chars` → the same test red.
4. Each allowed class: drop `b.is_ascii_digit()`, then `b == b'-'` → the same test red (`x-1`, `2fa`).
5. `with` keeps order: delete the `sort_by` → `the_first_set_writes_the_starting_set_first_and_a_later_one_changes_one_area` red (`check` fails on `ops`).
6. `with` replaces an area: drop the `filter` → the same test red (two `code` entries).
7. `after_set`'s first branch writes the starting set: make `None` start from `RoutingMap::default()` → the same test red.
7a. `after_set` keeps a sensitivity it is not told to change (decision 22): replace the `unwrap_or_else(…)` with `unwrap_or(false)` → `a_set_that_names_no_sensitivity_keeps_the_areas` red. Read the kept value from `current` only, not `base` → the same test red (the first set's `security`).
8. `check`'s order test: change `>=` to `>` → `a_map_out_of_order_with_an_area_twice_or_a_bad_name_is_refused` red (an area twice).
9. `check`'s name test: delete the `for a in &self.areas` loop → the same test red.
10. `MemStore::set_routes` checks the map: delete the `map.check()` → `cargo test -p fl-core --lib mem::tests::` stays green — not observable there; `RedbStore`'s twin is pinned below. Not a guard in `MemStore`, which tests build by hand.
11. `RedbStore::set_routes` raises the format: delete `raise_format` → `cargo test -p fl-store --lib tests::a_routing_map_is_kept_and_raises_the_store_to_format_5` red.
12. It checks the map first: move `map.check()` below the commit → `cargo test -p fl-store --lib tests::a_map_that_is_not_valid_is_refused_before_anything_is_written` red.
13. It refuses an imported project: delete `refuse_if_imported` → `cargo test -p fl-store --lib tests::an_imported_projects_routing_map_cannot_be_changed_here` red.
14. `routes` checks the project: delete `check_kind` → `cargo test -p fl-store --lib tests::a_routing_map_is_kept_and_raises_the_store_to_format_5` red (a stranger reads `None`).
15. `holds_routing` reads the table: return `Ok(true)` when the table does not exist → the same test red.
16. `set_routes` refuses a second project: delete `self.refuse_a_second_project(project)?;` → `cargo test -p fl-store --lib tests::a_routed_store_holds_exactly_one_project` red. Move it below the commit → the same test red ("nothing written").
17. `refuse_a_second_project` passes over the project itself: make its `find` take the first project, whoever it is → `tests::a_routing_map_is_kept_and_raises_the_store_to_format_5` red.
18. `add_project` refuses in a routed store: delete the `if self.holds_routing()?` → `tests::a_routed_store_holds_exactly_one_project` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/routing.rs crates/core/src/lib.rs crates/core/src/mem.rs crates/store/src/lib.rs
git commit -m "feat(core,store): the routing map and its starting set

Tier, area names, the map (sorted, each area once), the starting set the
first set writes, and Routes. The local store keeps a project's map,
refuses to change an imported project's, and raises itself to format 5 in
the same write. A routed store holds exactly one project: no map while
another project is there, no other project once there is one. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 4: Manifest format 3 carries the routing map

A machine that imports the manifest imports the map with it (routing spec §1.2: "one routing rule per project, not per machine"). An export is the oldest format that holds what it carries — 1, 2, or 3 with a map — so a project without routing exports what every older fl reads. The hash covers the map. An import writes the map and raises the store to 5; one that would drop a map the store imported is refused (plan ruling 12); and an import keeps a routed store to one project (spec decision 20) — a routed manifest goes only into a store holding no other project, and no other project goes into a routed store.

**Blast radius:** `fl_store::manifest::export` gains a parameter (its one production caller, `RedbStore::export_manifest`, and the tests in `manifest.rs`). `MANIFEST_FORMAT` now means "newest" (3); the format-2 meaning moves to `MANIFEST_FORMAT_WITH_LEDGER`. `check_consistent` is read by every parse, import and export. `ImportReport` gains a field (read by `print_import`).

**Files:**
- Modify: `crates/store/src/manifest.rs` (constants, `Body`, `ManifestError`, `format_for`, `export`, `parse`, `check_consistent`; tests)
- Modify: `crates/store/src/lib.rs` (`ImportReport`, `export_manifest`, `import_manifest`; tests)
- Modify: `crates/cli/src/cmd/manifest.rs` (`print_import`)
- Modify: `docs/sharing-gates.md` (formats)

**Interfaces:**
- Consumes: `RoutingMap`, `Routes`, `RedbStore::refuse_a_second_project`, `routed_store_is_taken` (Task 3).
- Produces: `MANIFEST_FORMAT_WITHOUT_LEDGER = 1`, `MANIFEST_FORMAT_WITH_LEDGER = 2`, `MANIFEST_FORMAT_WITH_ROUTING = 3`, `MANIFEST_FORMAT = 3`; `pub fn format_for(ledger_root: bool, routing: bool) -> u64`; `Body.routing: Option<RoutingMap>`; `pub fn export(catalog, project, commit, exported_at_unix, ledger_root, routing: Option<RoutingMap>)`; `ManifestError::WouldDropRouting(ProjectId)`; `ImportReport.areas: Option<usize>`. Unique phrases: `but what it carries`, `its routing map is not valid`, `would un-route the project`.

- [ ] **Step 1: Write the failing tests**

In `crates/store/src/manifest.rs`, inside `mod tests`, give every existing `export(` call a last argument `None`: `export(&s, &p, "abc", 7, None)` becomes `export(&s, &p, "abc", 7, None, None)`, `export(&s, &p, "abc", 7, Some(root()))` becomes `export(&s, &p, "abc", 7, Some(root()), None)`, and the multi-line call in `a_ledger_root_that_cannot_be_one_is_refused_even_with_a_correct_hash` gains `None,` after its `Some(LedgerRoot { … }),`. Then add:

```rust
    fn routes() -> fl_core::RoutingMap {
        fl_core::RoutingMap::starting()
    }

    // Routing spec §1.2: the oldest format that holds what the manifest
    // carries, so a project without routing exports what every older fl
    // reads.
    #[test]
    fn the_format_is_the_oldest_that_holds_what_the_manifest_carries() {
        let (s, p, _, _) = store();
        for (root, routing, want) in [
            (None, None, 1),
            (Some(root()), None, 2),
            (None, Some(routes()), 3),
            (Some(root()), Some(routes()), 3),
        ] {
            let m = export(&s, &p, "abc", 7, root, routing.clone()).unwrap();
            assert_eq!(m.body.format_version, want);
            let back = Manifest::parse(&m.to_json()).unwrap();
            assert_eq!(back.body.routing, routing);
            assert_eq!(back, m);
        }
        let text = export(&s, &p, "abc", 7, None, None).unwrap().to_json();
        assert!(!text.contains("routing"), "skipped when absent: {text}");
    }

    #[test]
    fn a_map_on_a_format_it_does_not_belong_to_or_not_valid_is_refused() {
        let (s, p, _, _) = store();
        let mut m = export(&s, &p, "abc", 7, None, Some(routes())).unwrap();
        m.body.format_version = 2;
        let err = rehashed(m).verify().unwrap_err();
        assert!(
            matches!(err, ManifestError::Inconsistent(ref w) if w.contains("but what it carries")),
            "{err}"
        );
        let mut m = export(&s, &p, "abc", 7, None, Some(routes())).unwrap();
        m.body.routing = None;
        let err = rehashed(m).verify().unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
        let mut m = export(&s, &p, "abc", 7, None, Some(routes())).unwrap();
        m.body.routing.as_mut().unwrap().areas[0].area = "Code".into();
        let err = rehashed(m).verify().unwrap_err();
        assert!(
            matches!(err, ManifestError::Inconsistent(ref w) if w.contains("its routing map is not valid")),
            "{err}"
        );
        let mut bad = routes();
        bad.areas.swap(0, 1);
        assert!(export(&s, &p, "abc", 7, None, Some(bad)).is_err(), "never exported");
    }

    #[test]
    fn a_hand_edited_routing_map_is_refused() {
        let (s, p, _, _) = store();
        let text = export(&s, &p, "abc", 7, None, Some(routes()))
            .unwrap()
            .to_json();
        let edited = text.replacen("\"tier\": \"local\"", "\"tier\": \"github\"", 1);
        assert_ne!(edited, text, "the edit must have landed");
        let err = Manifest::parse(&edited).unwrap_err();
        assert!(matches!(err, ManifestError::HandEdited { .. }), "{err}");
    }
```

In `crates/store/src/lib.rs`, inside `mod tests`, add:

```rust
    // Routing spec §1.2: the importing machine routes as the authoring one.
    #[test]
    fn an_export_carries_the_map_and_an_import_writes_it_raising_the_store_to_5() {
        use fl_core::routing::{RoutingMap, Routes};
        let (a, _ga, p, _, _) = authoring();
        a.set_routes(&p, &RoutingMap::starting()).unwrap();
        let m = a.export_manifest(&p, "c1", 7, None).unwrap();
        assert_eq!(m.body.format_version, 3);
        assert_eq!(m.body.routing, Some(RoutingMap::starting()));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.redb");
        {
            let b = RedbStore::open(&path).unwrap();
            let report = b.import_manifest(&m, "/x").unwrap();
            assert_eq!(report.areas, Some(5));
            assert_eq!(b.routes(&p).unwrap(), Some(RoutingMap::starting()));
            assert!(b.holds_routing().unwrap());
        }
        assert_eq!(format_at(&path), Some(FORMAT_WITH_ROUTING));
    }

    // Routing spec §1.2: an older checked-out manifest must not un-route one
    // machine.
    #[test]
    fn an_import_that_would_drop_the_routing_map_is_refused() {
        use fl_core::routing::{RoutingMap, Routes};
        let (a, _ga, p, _, _) = authoring();
        let unrouted = a.export_manifest(&p, "c1", 7, None).unwrap();
        a.set_routes(&p, &RoutingMap::starting()).unwrap();
        let routed = a.export_manifest(&p, "c2", 8, None).unwrap();
        let (b, _gb) = fresh();
        b.import_manifest(&routed, "/x").unwrap();
        let err = b.import_manifest(&unrouted, "/x").unwrap_err();
        assert!(matches!(err, ManifestError::WouldDropRouting(_)), "{err}");
        assert!(err.to_string().contains("would un-route the project"), "{err}");
        assert_eq!(b.routes(&p).unwrap(), Some(RoutingMap::starting()), "nothing written");
        // A map that changed is imported: areas come and go (decision 15).
        a.set_routes(&p, &RoutingMap::starting().without("design")).unwrap();
        b.import_manifest(&a.export_manifest(&p, "c3", 9, None).unwrap(), "/x")
            .unwrap();
        assert_eq!(b.routes(&p).unwrap().unwrap().declared().len(), 4);
    }

    // Routing spec decision 20: an import keeps a routed store to one
    // project, both ways round.
    #[test]
    fn an_import_never_puts_a_routed_project_beside_another() {
        use fl_core::routing::RoutingMap;
        let (a, _ga, p, _, _) = authoring();
        a.set_routes(&p, &RoutingMap::starting()).unwrap();
        let routed = a.export_manifest(&p, "c1", 7, None).unwrap();
        let (b, _gb) = fresh();
        b.add_project("/elsewhere").unwrap();
        let err = b.import_manifest(&routed, "/x").unwrap_err().to_string();
        assert!(err.contains("needs a store of its own"), "{err}");
        assert!(!b.holds_routing().unwrap(), "nothing written");
        let (c, _gc) = fresh();
        c.import_manifest(&routed, "/x").unwrap();
        let (o, _go, op, _, _) = authoring();
        let unrouted = o.export_manifest(&op, "c1", 7, None).unwrap();
        let err = c.import_manifest(&unrouted, "/y").unwrap_err().to_string();
        assert!(err.contains("needs a store of its own"), "{err}");
        assert_eq!(c.list_projects().unwrap().len(), 1);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-store --lib`
Expected: FAIL to compile — `export` takes five arguments, `Body` has no `routing`, `ImportReport` no `areas`, `ManifestError` no `WouldDropRouting`.

- [ ] **Step 3: Implement**

In `crates/store/src/manifest.rs`, replace the two format constants and their doc comment with:

```rust
/// Format 1: gates and transitions. Format 2 adds `ledger_root` (GitHub
/// ledger spec §6.1 step 4). Format 3 adds `routing` (routing spec §1.2),
/// with a ledger root or without. An export writes the oldest format that
/// holds what it carries ([`format_for`]), so a project with neither
/// still exports a manifest every older fl reads.
pub const MANIFEST_FORMAT_WITHOUT_LEDGER: u64 = 1;
pub const MANIFEST_FORMAT_WITH_LEDGER: u64 = 2;
pub const MANIFEST_FORMAT_WITH_ROUTING: u64 = 3;
/// The newest format this fl reads.
pub const MANIFEST_FORMAT: u64 = MANIFEST_FORMAT_WITH_ROUTING;
```

Add `use fl_core::routing::RoutingMap;`. In `Body`, after `ledger_root`, add:

```rust
    /// Format 3 only (routing spec §1.2). ⚠ Skipped when absent, so a body
    /// of format 1 or 2 serializes — and hashes — byte for byte as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingMap>,
```

In `ManifestError`, after `WouldRemoveGate`, add:

```rust
    #[error(
        "the manifest has no routing map, and this store imported one for project {0}. \
         Importing it would un-route the project on this machine alone, so it is refused. The \
         checked-out manifest may be older than the one this store imported: check out a \
         commit whose manifest has the routing map"
    )]
    WouldDropRouting(ProjectId),
```

After `content_sha256`, add:

```rust
/// The format of a body carrying a ledger root and a routing map, or not:
/// the oldest that holds both (routing spec §1.2).
pub fn format_for(ledger_root: bool, routing: bool) -> u64 {
    match (ledger_root, routing) {
        (_, true) => MANIFEST_FORMAT_WITH_ROUTING,
        (true, false) => MANIFEST_FORMAT_WITH_LEDGER,
        (false, false) => MANIFEST_FORMAT_WITHOUT_LEDGER,
    }
}
```

Give `export` a last parameter `routing: Option<RoutingMap>`, and build its body with `format_version: format_for(ledger_root.is_some(), routing.is_some()),` and `routing,` after `ledger_root,`. In `parse`, the accepted range stays `(MANIFEST_FORMAT_WITHOUT_LEDGER..=MANIFEST_FORMAT)`. In `check_consistent`, replace the `match (&self.body.ledger_root, f) { … }` with:

```rust
        let want = format_for(self.body.ledger_root.is_some(), self.body.routing.is_some());
        if f != want {
            let carried = match (&self.body.ledger_root, &self.body.routing) {
                (Some(_), Some(_)) => "a ledger root and a routing map",
                (Some(_), None) => "a ledger root",
                (None, Some(_)) => "a routing map",
                (None, None) => "neither a ledger root nor a routing map",
            };
            return Err(ManifestError::Inconsistent(format!(
                "it is format {f}, but what it carries — {carried} — is format {want}"
            )));
        }
        if let Some(root) = &self.body.ledger_root {
            fl_core::ledger_root_shape(&root.repository_node_id, &root.commit).map_err(|why| {
                ManifestError::Inconsistent(format!("its ledger root cannot be one: {why}"))
            })?;
        }
        if let Some(map) = &self.body.routing {
            map.check().map_err(|why| {
                ManifestError::Inconsistent(format!("its routing map is not valid: {why}"))
            })?;
        }
```

In `crates/store/src/lib.rs`: in `ImportReport`, add

```rust
    /// How many areas the imported routing map declares, when it has one.
    pub areas: Option<usize>,
```

In `export_manifest`, the last line becomes `manifest::export(self, project, commit, exported_at_unix, ledger_root, self.routes(project)?)`. In `import_manifest`, after the `if held_project && self.imported_hash(project)?.is_none()` refusal, add:

```rust
        // ⚠ An older checked-out manifest would un-route this machine alone
        // (routing spec §1.2: one routing rule per project, not per machine).
        if held_project && body.routing.is_none() && self.routes(project)?.is_some() {
            return Err(ManifestError::WouldDropRouting(project.clone()));
        }
        // Routing spec decision 20: a routed store holds one project — a
        // routed manifest goes only into a store holding no other, and no
        // other project goes into a routed store.
        if body.routing.is_some() {
            self.refuse_a_second_project(project)?;
        } else if !held_project && self.holds_routing()? {
            return Err(routed_store_is_taken().into());
        }
```

In `report`, add `areas: body.routing.as_ref().map(|m| m.areas.len()),`. Inside the transaction, after the `IMPORTS` insert, add:

```rust
        if let Some(map) = &body.routing {
            let json = serde_json::to_string(map).map_err(backend)?;
            tx.open_table(ROUTING)
                .map_err(backend)?
                .insert(project.iri().as_str(), json.as_str())
                .map_err(backend)?;
            raise_format(&tx, FORMAT_WITH_ROUTING)?;
        }
```

In `crates/cli/src/cmd/manifest.rs`, in `print_import`, after the `imported\t…` line, add:

```rust
    if let Some(n) = report.areas {
        println!("routing\t{n} areas");
    }
```

In `docs/sharing-gates.md`, after the paragraph about format 4, add:

```markdown
A project that routes its items between its local store and GitHub exports its routing map
too: the manifest is then format 3 (with its ledger root, if it has one), and the map is
covered by the hash like everything else in it. Importing such a manifest writes the map and
raises the store to format 5, which an older `fl` refuses — it would route nothing. A
re-import of a manifest that has no routing map, into a store that imported one, is refused:
the checked-out manifest is older than the one this store imported. A project without
routing still exports format 1 or 2, exactly as before.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-store` and `cargo test -p fl-cli --test manifest`
Expected: PASS (the CLI's import output gains a `routing` line only for a routed manifest, which no existing test imports).

- [ ] **Step 5: Mutation checks**

Each `manifest.rs` run with `cargo test -p fl-store --lib manifest::tests::`:

1. `format_for`'s routing arm: make `(_, true)` return `MANIFEST_FORMAT_WITH_LEDGER` → `the_format_is_the_oldest_that_holds_what_the_manifest_carries` red.
2. Its ledger arm: make `(true, false)` return `MANIFEST_FORMAT_WITHOUT_LEDGER` → the same test red.
3. `Body.routing`'s `skip_serializing_if`: remove it → the same test red (`routing` in the text) and `a_manifest_with_no_ledger_root_is_format_1_and_an_older_fl_reads_it` red.
4. `check_consistent`'s format test: delete the `if f != want` block → `a_map_on_a_format_it_does_not_belong_to_or_not_valid_is_refused` red.
5. Its map check: delete the `if let Some(map)` block → the same test red.
6. `export` checks its own output (`m.check_consistent()?`, existing): delete it → the same test red ("never exported").
7. `export_manifest` reads the store's map: pass `None` → `cargo test -p fl-store --lib tests::an_export_carries_the_map_and_an_import_writes_it_raising_the_store_to_5` red.
8. The import writes the map: delete the `if let Some(map)` in the transaction → the same test red.
9. The import's raise: delete `raise_format(&tx, FORMAT_WITH_ROUTING)` → the same test red.
10. The drop refusal, each conjunct: drop `body.routing.is_none() &&` → `cargo test -p fl-store --lib tests::an_import_that_would_drop_the_routing_map_is_refused` red (the changed map is refused); drop `&& self.routes(project)?.is_some()` → `cargo test -p fl-store --lib tests::a_reimport_mirrors_the_manifests_transitions` red (an unrouted re-import is refused); drop `held_project &&` → not observable (an unheld project has no map); not a guard.
11. `areas` in the report: write `None` → `tests::an_export_carries_the_map…` red.
12. The import's one-project rule, each arm: delete `self.refuse_a_second_project(project)?;` → `cargo test -p fl-store --lib tests::an_import_never_puts_a_routed_project_beside_another` red (its first refusal); delete the `else if` arm → the same test red (its second).

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/store/src/manifest.rs crates/store/src/lib.rs crates/cli/src/cmd/manifest.rs docs/sharing-gates.md
git commit -m "feat(store): manifest format 3 carries the routing map

An export is the oldest format that holds what it carries; the hash covers
the map. An import writes it and raises the store to format 5; one that
would drop a map the store imported is refused, and none puts a routed
project beside another in one store. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 5: A finding about a record in the other tier, through a checked reference

References may cross tiers (routing spec decision 5, §2.5). Today each tracker refuses a record it does not hold (`crates/store/src/lib.rs:976-985`, `crates/github/src/tracker.rs:1864-1879`), and that refusal stays. A tracker stores a reference into the other tier only through `add_finding_checked(finding, ForeignRecord)`, where a `ForeignRecord` is proof the router read the record in the tier that owns it. In this task only tests build one (`for_tests`); Task 7 gives the router its constructor. The GitHub tracker writes the reference as `{id, title}` (Task 1's format-2 block and record line); a local store keeps a GitHub record's URL.

**Blast radius:** the `Tracker` trait gains a required method — every implementation and wrapper of Task 2's list. `add_finding`'s GitHub block-building moves into a helper both creates share. No existing behaviour changes.

**Files:**
- Modify: `crates/core/src/routing.rs` (`ForeignRecord`, `check_foreign_for_local`; tests)
- Modify: `crates/core/src/store.rs` (`Tracker::add_finding_checked`; `CatalogChecked`; `NeverAsked`; tests)
- Modify: `crates/core/src/mem.rs`, `crates/core/src/lib.rs`
- Modify: `crates/exec/src/journal.rs`, `crates/exec/src/evaluate.rs`
- Modify: `crates/store/src/lib.rs` (`add_finding_checked`; tests)
- Modify: `crates/github/src/tracker.rs` (`finding_meta`, `add_finding`, `add_finding_checked`; tests)
- Modify: `docs/github-tracker.md`, `docs/superpowers/specs/2026-09-26-github-tracker-design.md` (§2.3)

**Interfaces:**
- Consumes: `Tier` (Task 3); `RecordRef { node_id: None, title }`, `record_line` (Task 1).
- Produces: `pub struct ForeignRecord` with `id(&self) -> &RecordId`, `title(&self) -> &str`, `tier(&self) -> Tier`, and `#[cfg(any(test, feature = "conformance"))] #[doc(hidden)] pub fn for_tests(id: RecordId, title: &str, tier: Tier) -> ForeignRecord`; `pub fn check_foreign_for_local(record: &ForeignRecord, held_here: bool) -> Result<(), StoreError>`; `Tracker::add_finding_checked(&self, finding: Finding, record: ForeignRecord) -> Result<FindingId, StoreError>` (required). Unique phrases: `is a record in the local tier, so a finding about it`, `is held by this store, so a finding about it`, `is a record on GitHub, so a finding about it`.

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/mem.rs`, inside `mod tests`, add:

```rust
    // Routing spec §2.5: a local store keeps a GitHub record's reference
    // only through the router's proof, and never for a record it holds.
    #[test]
    fn a_memory_store_keeps_a_checked_reference_to_a_github_record() {
        use crate::routing::{ForeignRecord, Tier};
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let url = RecordId(Iri::parse("https://github.com/acme/widgets/issues/7").unwrap());
        // Raised against the placeholder: the stored reference must come
        // from the proof.
        let f = Finding::raise(p.clone(), RecordId(seq_iri(0)), "rev", "claim");
        let id = s
            .add_finding_checked(f.clone(), ForeignRecord::for_tests(url.clone(), "t", Tier::Github))
            .unwrap();
        assert_eq!(s.get_finding(&id).unwrap().unwrap().record, url);
        let err = s
            .add_finding_checked(f.clone(), ForeignRecord::for_tests(url, "t", Tier::Local))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is a record in the local tier, so a finding about it"), "{err}");
        let held = s.add_record(&p, "t").unwrap();
        let err = s
            .add_finding_checked(f, ForeignRecord::for_tests(held, "t", Tier::Github))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is held by this store, so a finding about it"), "{err}");
    }
```

In `crates/core/src/store.rs`, inside `mod tests`, add:

```rust
    // A split binding checks the project of a checked reference — and no
    // record: the reference is to the other tier.
    #[test]
    fn a_split_binding_checks_only_the_project_of_a_checked_reference() {
        use crate::routing::{ForeignRecord, Tier};
        let catalog = MemStore::default();
        let url = RecordId(Iri::parse("https://github.com/acme/widgets/issues/7").unwrap());
        let refuses = CatalogChecked { catalog: &catalog, tracker: &NeverAsked };
        let stranger = ProjectId(seq_iri(99));
        let err = refuses
            .add_finding_checked(
                Finding::raise(stranger, url.clone(), "a", "c"),
                ForeignRecord::for_tests(url.clone(), "t", Tier::Github),
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        let p = catalog.add_project("/p").unwrap();
        let passes = CatalogChecked { catalog: &catalog, tracker: &catalog };
        let id = passes
            .add_finding_checked(
                Finding::raise(p, url.clone(), "a", "c"),
                ForeignRecord::for_tests(url.clone(), "t", Tier::Github),
            )
            .unwrap();
        assert_eq!(catalog.get_finding(&id).unwrap().unwrap().record, url);
    }
```

In `crates/store/src/lib.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn the_local_store_keeps_a_checked_reference_to_a_github_record() {
        use fl_core::routing::{ForeignRecord, Tier};
        let (s, _d) = fresh();
        let p = s.add_project("/p").unwrap();
        let url = RecordId(Iri::parse("https://github.com/acme/widgets/issues/7").unwrap());
        let err = s
            .add_finding(Finding::raise(p.clone(), url.clone(), "rev", "claim"))
            .unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "unchecked stays refused: {err:?}");
        // Raised against the placeholder: the stored reference must come
        // from the proof.
        let f = Finding::raise(p.clone(), RecordId(fl_core::ids::seq_iri(0)), "rev", "claim");
        let id = s
            .add_finding_checked(f.clone(), ForeignRecord::for_tests(url.clone(), "t", Tier::Github))
            .unwrap();
        assert_eq!(s.get_finding(&id).unwrap().unwrap().record, url);
        let err = s
            .add_finding_checked(f.clone(), ForeignRecord::for_tests(url, "t", Tier::Local))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is a record in the local tier, so a finding about it"), "{err}");
        let held = s.add_record(&p, "t").unwrap();
        let err = s
            .add_finding_checked(f, ForeignRecord::for_tests(held, "t", Tier::Github))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is held by this store, so a finding about it"), "{err}");
    }
```

In `crates/github/src/tracker.rs`, inside `mod tests`, after `an_area_label_is_created_once_per_process`, add:

```rust
    // Routing spec §2.5, decision 14: a GitHub finding about a local record
    // names it as text and carries `{id, title}`; it reads back as the
    // claim, and keeps all of it through an update.
    #[test]
    fn a_finding_about_a_local_record_reads_back_and_survives_an_update() {
        use fl_core::routing::{ForeignRecord, Tier};
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let local = RecordId(Iri::parse("urn:uuid:00000000-0000-7000-8000-000000000042").unwrap());
        let mut f = Finding::raise(p(), local.clone(), "rev", "the claim");
        f.area = Some("design".into());
        let id = t
            .add_finding_checked(f, ForeignRecord::for_tests(local.clone(), "@alice fix", Tier::Local))
            .unwrap();
        let body = fake.issue(1).body;
        let text = &body[..body.rfind(meta::META_OPEN).unwrap()];
        assert!(text.contains("held in the local tier, not on GitHub"), "{text}");
        assert!(!text.contains("@alice"), "{text}");
        assert!(body.contains("\"fl_format\":2"), "{body}");
        assert!(
            body.contains(
                "\"record\":{\"id\":\"urn:uuid:00000000-0000-7000-8000-000000000042\",\
                 \"title\":\"@alice fix\"}"
            ),
            "{body}"
        );
        let back = t.get_finding(&id).unwrap().unwrap();
        assert_eq!((back.record.clone(), back.claim.as_str()), (local.clone(), "the claim"));
        let mut back = back;
        back.attach_reproduction(fl_core::ids::GateId(seq_iri(5))).unwrap();
        t.update_finding(&back).unwrap();
        let again = open(&fake).get_finding(&id).unwrap().unwrap();
        assert_eq!((again.record, again.claim.as_str()), (local.clone(), "the claim"));
        assert!(fake.issue(1).body.contains("held in the local tier, not on GitHub"));
        // A security finding still goes only to a private repository (spec §6).
        fake.state().repos[0].visibility = "public".into();
        let mut secret = Finding::raise(p(), local.clone(), "rev", "secret");
        secret.security = true;
        let err = t
            .add_finding_checked(secret, ForeignRecord::for_tests(local, "t", Tier::Local))
            .unwrap_err();
        assert!(matches!(err, StoreError::SecurityNotPrivate { .. }), "{err:?}");
        assert_eq!(fake.issue_count(), 1, "nothing created");
    }

    #[test]
    fn a_checked_reference_to_a_github_record_is_refused() {
        use fl_core::routing::{ForeignRecord, Tier};
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let err = t
            .add_finding_checked(
                Finding::raise(p(), r.clone(), "rev", "c"),
                ForeignRecord::for_tests(r, "t", Tier::Github),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("is a record on GitHub, so a finding about it"), "{err}");
        assert_eq!(fake.issue_count(), 1, "nothing created");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-core --lib` and `cargo test -p fl-store --lib` and `cargo test -p fl-github --lib tracker::tests::a_finding_about_a_local`
Expected: FAIL to compile — `ForeignRecord` and `add_finding_checked` do not exist.

- [ ] **Step 3: Implement**

In `crates/core/src/routing.rs`, add `use crate::ids::RecordId;` and, before `mod tests`:

```rust
/// A record in the OTHER tier, which the router read there before it built
/// this (routing spec §2.5). A tracker stores a finding about a record it
/// does not hold only with one of these — never on an id alone, which no
/// one has checked. Only the router builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignRecord {
    id: RecordId,
    title: String,
    tier: Tier,
}

impl ForeignRecord {
    /// For the trackers' own tests. Not in the binary: the `conformance`
    /// feature is a dev-dependency only.
    #[cfg(any(test, feature = "conformance"))]
    #[doc(hidden)]
    pub fn for_tests(id: RecordId, title: &str, tier: Tier) -> Self {
        Self {
            id,
            title: title.to_string(),
            tier,
        }
    }

    /// The record's primary id, as the tier that holds it answers it.
    pub fn id(&self) -> &RecordId {
        &self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// The tier that holds the record.
    pub fn tier(&self) -> Tier {
        self.tier
    }
}

/// A local store's check of the proof for a finding about a record it does
/// not hold: the record is on GitHub, and this store does not hold it (it
/// would check one it holds itself, with `add_finding`).
pub fn check_foreign_for_local(record: &ForeignRecord, held_here: bool) -> Result<(), StoreError> {
    if record.tier() == Tier::Local {
        return Err(StoreError::Backend(format!(
            "{} is a record in the local tier, so a finding about it is raised with \
             `add_finding`, which checks it there",
            record.id()
        )));
    }
    if held_here {
        return Err(StoreError::Backend(format!(
            "{} is held by this store, so a finding about it is raised with `add_finding`, \
             which checks it here",
            record.id()
        )));
    }
    Ok(())
}
```

Add `ForeignRecord` to the `pub use routing::{…}` line in `crates/core/src/lib.rs`.

In `crates/core/src/store.rs`, add `use crate::routing::ForeignRecord;`, and in `Tracker`, after `add_finding`:

```rust
    /// Raise `finding` about a record in the OTHER tier (routing spec
    /// §2.5). The tracker does not look for the record — it does not hold
    /// it — and takes `record`, the router's proof of its check, instead.
    /// ⚠ `add_finding` still refuses a record the tracker does not hold.
    fn add_finding_checked(
        &self,
        finding: Finding,
        record: ForeignRecord,
    ) -> Result<FindingId, StoreError>;
```

In `CatalogChecked`'s impl:

```rust
    /// The project only: the record is in the other tier, which the router
    /// checked (routing spec §2.5).
    fn add_finding_checked(
        &self,
        finding: Finding,
        record: ForeignRecord,
    ) -> Result<FindingId, StoreError> {
        self.project(&finding.project)?;
        self.tracker.add_finding_checked(finding, record)
    }
```

In `NeverAsked` (tests), add the method with `unreachable!("the binding must refuse before the tracker is asked")`.

In `crates/core/src/mem.rs`, in `impl Tracker for MemStore`, after `add_finding`:

```rust
    fn add_finding_checked(
        &self,
        finding: Finding,
        record: ForeignRecord,
    ) -> Result<FindingId, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&finding.project.0, Kind::Project)?;
        check_foreign_for_local(&record, s.check(record.id().iri()).is_ok())?;
        let id = FindingId(s.mint(Kind::Finding));
        let mut finding = finding;
        finding.id = id.clone();
        finding.record = record.id().clone();
        s.findings.insert(id.0.clone(), finding);
        Ok(id)
    }
```

with `use crate::routing::{ForeignRecord, check_foreign_for_local};` at the top.

In `crates/exec/src/journal.rs`, add (importing `fl_core::routing::ForeignRecord`):

```rust
    fn add_finding_checked(
        &self,
        finding: Finding,
        record: ForeignRecord,
    ) -> Result<FindingId, StoreError> {
        self.store.add_finding_checked(finding, record)
    }
```

In `crates/exec/src/evaluate.rs`, `BrokenStore` gains `add_finding_checked(&self, _: Finding, _: fl_core::routing::ForeignRecord)` returning `Err(broken())`.

In `crates/store/src/lib.rs`, add `use fl_core::routing::{ForeignRecord, check_foreign_for_local};` and, in `impl Tracker for RedbStore` after `add_finding`:

```rust
    fn add_finding_checked(
        &self,
        finding: Finding,
        record: ForeignRecord,
    ) -> Result<FindingId, StoreError> {
        self.check_kind(finding.project.iri(), Kind::Project)?;
        check_foreign_for_local(&record, self.owns(record.id().iri())?)?;
        let raise = finding.area.as_ref().map(|_| FORMAT_WITH_ROUTING);
        let id = self.insert_new(Kind::Finding, FINDINGS, raise, |id| {
            let mut finding = finding;
            finding.id = FindingId(id);
            finding.record = record.id().clone();
            finding
        })?;
        Ok(FindingId(id))
    }
```

In `crates/github/src/tracker.rs`, add `use fl_core::routing::{ForeignRecord, Tier};`. Before `impl Tracker for GithubTracker`, add:

```rust
/// The block of a new finding about `record` (spec §3.1).
fn finding_meta(finding: &Finding, record: RecordRef) -> Meta {
    let mut meta = Meta::new(
        ItemKind::Finding,
        finding.state.as_wire(),
        finding.project.clone(),
    );
    meta.record = Some(record);
    meta.area = finding.area.clone();
    meta.reproduction = finding.reproduction.clone();
    meta.raised_by = Some(finding.raised_by.clone());
    meta.assigned_to = finding.assigned_to.clone();
    meta.withdrawn_reason = finding.withdrawn_reason.clone();
    meta.security = finding.security;
    meta
}
```

In `add_finding`, replace everything from `let mut meta = Meta::new(` to the line before `let title = …` with:

```rust
        let meta = finding_meta(
            &finding,
            RecordRef {
                id: record.url.clone(),
                node_id: Some(record.node_id.clone()),
                title: None,
            },
        );
```

and after `add_finding` add:

```rust
    /// A finding about a record in the project's local tier (routing spec
    /// §2.5): its block carries `{id, title}` and no node id (decision 14),
    /// and the issue shows the record as text.
    fn add_finding_checked(
        &self,
        finding: Finding,
        record: ForeignRecord,
    ) -> Result<FindingId, StoreError> {
        if record.tier() != Tier::Local {
            return Err(backend(format!(
                "{} is a record on GitHub, so a finding about it is raised with `add_finding`, \
                 which checks it here",
                record.id()
            )));
        }
        if finding.security {
            self.require_private()?;
        }
        let meta = finding_meta(
            &finding,
            RecordRef {
                id: record.id().iri().clone(),
                node_id: None,
                title: Some(record.title().to_string()),
            },
        );
        let title = meta::title_of(&finding.claim);
        Ok(FindingId(
            self.create(ItemKind::Finding, &title, &finding.claim, &meta)?
                .url,
        ))
    }
```

In `docs/github-tracker.md`, after the block example's paragraph ("The block is fl's record of the protocol…"), add:

```markdown
A finding whose record lives in the project's local store — possible when the project routes
its items between the two — names the record in its block as `{"id": "urn:uuid:…", "title":
"…"}`, with no `node_id`, since no issue holds it, and the issue shows a line `Record: <title> —
<id>, held in the local tier, not on GitHub.` A reader on GitHub sees the title and the id; fl
resolves the id in the local store. On a repository that is not private, raising such a finding
publishes the record's title, and fl warns before it does.
```

In `docs/superpowers/specs/2026-09-26-github-tracker-design.md`, §2.3, after the paragraph ending "never trusts an old name.", add:

```markdown
*Amended by the two-tier routing spec (decision 14, 2026-10-06):* a finding whose record lives
in the project's **local tier** carries a reference with no `node_id` — `{"id": "<local IRI>",
"title": "<the record's title>"}` — because no issue holds that record. fl resolves it in the
local tier, by its IRI, never through GitHub, and the issue shows the title and IRI as text. A
block that carries one is written as `fl_format` 2, so an older fl refuses it as a newer format
rather than reading it as damaged.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

1. `check_foreign_for_local`'s tier test: delete it → `cargo test -p fl-core --lib mem::tests::a_memory_store_keeps_a_checked_reference_to_a_github_record` red, and `cargo test -p fl-store --lib tests::the_local_store_keeps_a_checked_reference_to_a_github_record` red.
2. Its held test: delete it → the same two tests red.
3. `MemStore` asks whether it holds the record: pass `false` → the mem test red.
4. `RedbStore` asks: pass `false` → the store test red.
5. `MemStore` and `RedbStore` take the reference from the proof: drop `finding.record = record.id().clone();` in each → its test red (the finding was raised against the placeholder).
6. `CatalogChecked` checks the project: delete `self.project(…)?` → `cargo test -p fl-core --lib store::tests::a_split_binding_checks_only_the_project_of_a_checked_reference` red (`NeverAsked` panics).
7. The GitHub tracker refuses a GitHub record: delete the `if record.tier() != Tier::Local` → `cargo test -p fl-github --lib tracker::tests::a_checked_reference_to_a_github_record_is_refused` red.
8. It writes no node id: write `node_id: Some(String::new())` → `cargo test -p fl-github --lib tracker::tests::a_finding_about_a_local_record_reads_back_and_survives_an_update` red.
9. It writes the title: write `title: None` → the same test red.
10. `finding_meta` keeps the area: delete `meta.area = …` → `cargo test -p fl-github --lib tracker::tests::an_item_made_with_an_area_carries_it_in_its_block_and_as_a_label` red.
11. The security check before the create: delete `if finding.security { self.require_private()?; }` in `add_finding_checked` → `a_finding_about_a_local_record_reads_back_and_survives_an_update` red (an issue is created on a public repository).

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/routing.rs crates/core/src/store.rs crates/core/src/mem.rs crates/core/src/lib.rs crates/exec/src/journal.rs crates/exec/src/evaluate.rs crates/store/src/lib.rs crates/github/src/tracker.rs docs/github-tracker.md docs/superpowers/specs/2026-09-26-github-tracker-design.md
git commit -m "feat: a finding about a record in the other tier, through a checked reference

Tracker gains add_finding_checked, which takes a ForeignRecord — proof the
router read the record where it lives — instead of looking for a record
it does not hold; add_finding still refuses one. The local store keeps a
GitHub record's URL; the GitHub tracker writes {id, title}, format 2, and
shows the record as text. The tracker spec's 2.3 invariant is amended for
local records. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 6: What the router asks of the GitHub tier, and an in-memory one to test it with

The router is pure (`fl-core`), and GitHub opens lazily (routing spec §2.6), so the router reaches GitHub through a trait, `GithubTier`: whether this machine binds a repository, whether an id is that repository's issue by its form, the tracker (opened on first use), the private-repository rule, and the items in an area by their blocks (§1.2). The router's refusals become one `RoutingFault`, under `StoreError::Routing`, as the ledger's are under `StoreError::Ledger`. `MemIssues` is an in-memory GitHub tier for `fl-core`'s and `fl-exec`'s tests; it passes the tracker conformance suite. The GitHub tracker gains `items_in_area` and a public `visibility`.

**Blast radius:** `StoreError` gains a variant (no `match` over `StoreError` is exhaustive outside `is_transient`'s `matches!`). `GithubTracker::require_private` becomes public and reads through `visibility`; its refusal message for an unreadable visibility changes wording (no test reads it).

**Files:**
- Modify: `crates/core/src/routing.rs` (`RoutingFault`, `GithubTier`; tests)
- Modify: `crates/core/src/store.rs` (`StoreError::Routing`)
- Create: `crates/core/src/mem_issues.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/github/src/tracker.rs` (`visibility`, `require_private`, `items_in_area`; tests)

**Interfaces:**
- Consumes: `ForeignRecord` (Task 5), `Tier` (Task 3), the tracker conformance suite (13 cases).
- Produces: `RoutingFault::{Unrouted { project }, NoArea { declared }, NothingToInherit { record, declared }, Undeclared { area, declared }, TierUnavailable { tier, why }, TierUnreadable { tier, cause }, Elsewhere { id, searched }, SensitiveToPublic { what, repo, visibility }}`; `StoreError::Routing(RoutingFault)`; `trait GithubTier { fn available(&self) -> bool; fn claims(&self, id: &Iri) -> bool; fn issue_form(&self, id: &Iri) -> bool; fn tracker(&self) -> Result<&dyn Tracker, StoreError>; fn require_private(&self) -> Result<(), StoreError>; fn items_in_area(&self, project: &ProjectId, area: &str) -> Result<Vec<(Kind, Iri)>, StoreError>; }`; `fl_core::mem_issues::{MemIssues, ISSUES}` (test/conformance only) with `set_down`, `set_unbound`, `set_public`, `asked`, `issue(n) -> Iri`; `GithubTracker::visibility(&self) -> Result<String, StoreError>`, `pub fn require_private(&self)`, `pub fn items_in_area(&self, project: &ProjectId, area: &str) -> Result<Vec<(Kind, Iri)>, StoreError>`. Unique phrases: `another machine's local tier`, `never puts an item in the other tier`, `rather than show part of it`, `carries an fl label, but its body`.

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/routing.rs`, inside `mod tests`, add:

```rust
    // Routing spec §4: every refusal names its cause and what to do.
    #[test]
    fn every_routing_refusal_names_its_remedy() {
        use crate::ids::seq_iri;
        let declared = vec!["code".to_string(), "design".to_string()];
        for (fault, phrase) in [
            (RoutingFault::Unrouted { project: ProjectId(seq_iri(1)) }, "fl routing set"),
            (RoutingFault::NoArea { declared: declared.clone() }, "code, design"),
            (
                RoutingFault::NothingToInherit {
                    record: RecordId(seq_iri(2)),
                    declared: declared.clone(),
                },
                "`--area`",
            ),
            (
                RoutingFault::Undeclared { area: "ops".into(), declared: declared.clone() },
                "code, design",
            ),
            (
                RoutingFault::TierUnavailable { tier: Tier::Github, why: "w".into() },
                "never puts an item in the other tier",
            ),
            (
                RoutingFault::TierUnreadable { tier: Tier::Github, cause: "c".into() },
                "--tier local",
            ),
            (
                RoutingFault::Elsewhere { id: seq_iri(3), searched: vec!["s".into()] },
                "another machine's local tier",
            ),
            (
                RoutingFault::SensitiveToPublic {
                    what: "this finding".into(),
                    repo: "acme/widgets".into(),
                    visibility: "public".into(),
                },
                "--tier local",
            ),
        ] {
            let msg = StoreError::from(fault).to_string();
            assert!(msg.contains(phrase), "{msg}");
        }
        assert!(RoutingFault::NoArea { declared: vec![] }.to_string().contains("none"));
    }
```

Create `crates/core/src/mem_issues.rs` with its tests (Step 3 gives the module above them):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::conformance::{self, Bound, Fixture};
    use crate::ids::seq_iri;

    /// A `MemStore` catalog with this tier as the tracker, as the GitHub
    /// tracker is bound.
    struct Over {
        catalog: MemStore,
        issues: MemIssues,
    }

    impl Fixture for Over {
        fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
            f(&Bound {
                catalog: &self.catalog,
                tracker: &self.issues,
                ledger: &self.catalog,
                handles: &self.catalog,
            });
        }
    }

    #[test]
    fn the_in_memory_github_tier_meets_the_tracker_contract() {
        conformance::tracker(|| Over {
            catalog: MemStore::default(),
            issues: MemIssues::default(),
        });
    }

    #[test]
    fn a_tier_that_is_down_or_unbound_answers_with_an_error_never_an_answer() {
        let t = MemIssues::default();
        let p = ProjectId(seq_iri(1));
        let r = t.add_record_with_area(&p, "t", Some("code")).unwrap();
        assert_eq!(r.iri(), &MemIssues::issue(1));
        t.set_down(true);
        assert!(matches!(t.get_record(&r), Err(StoreError::Unreachable { .. })));
        assert!(matches!(t.list_records(&p), Err(StoreError::Unreachable { .. })));
        assert!(t.tracker().is_err());
        assert!(matches!(t.items_in_area(&p, "code"), Err(StoreError::Unreachable { .. })));
        t.set_down(false);
        t.set_unbound(true);
        assert!(!t.available());
        assert!(!t.claims(&MemIssues::issue(1)) && t.issue_form(&MemIssues::issue(1)));
        assert!(matches!(
            t.tracker().err(),
            Some(StoreError::Routing(RoutingFault::TierUnavailable { .. }))
        ));
        assert_eq!(t.asked(), 2);
    }

    #[test]
    fn an_id_this_tier_never_held_is_not_owned_and_a_public_one_refuses_security() {
        let t = MemIssues::default();
        let err = t.get_record(&RecordId(seq_iri(9))).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        assert!(t.claims(&MemIssues::issue(5)) && !t.claims(&seq_iri(5)));
        t.set_public(true);
        assert!(matches!(t.require_private(), Err(StoreError::SecurityNotPrivate { .. })));
        let p = ProjectId(seq_iri(1));
        let r = t.add_record(&p, "t").unwrap();
        let mut f = Finding::raise(p.clone(), r, "a", "c");
        f.security = true;
        assert!(matches!(t.add_finding(f), Err(StoreError::SecurityNotPrivate { .. })));
        let area = t.add_record_with_area(&p, "u", Some("design")).unwrap();
        assert_eq!(t.items_in_area(&p, "design").unwrap(), vec![(Kind::Record, area.0)]);
        assert!(t.items_in_area(&ProjectId(seq_iri(2)), "design").unwrap().is_empty());
    }
}
```

In `crates/github/src/tracker.rs`, inside `mod tests`, add:

```rust
    // Routing spec §1.2: GitHub items are found by their blocks, so an item
    // that lost its labels is not missed, and an issue that is not fl's is
    // passed over.
    #[test]
    fn items_in_an_area_are_found_by_their_block_labelled_or_not() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record_with_area(&p(), "a", Some("code")).unwrap();
        t.add_record_with_area(&p(), "b", Some("design")).unwrap();
        let mut f = Finding::raise(p(), r, "rev", "c");
        f.area = Some("code".into());
        t.add_finding(f).unwrap();
        t.add_record_with_area(&ProjectId(seq_iri(2)), "other", Some("code"))
            .unwrap();
        let plain = fake.plain_issue(&[], false);
        fake.web_edit(plain, |i| i.body = "quoting <!-- fl:meta\n{broken".into());
        fake.web_edit(1, |i| i.labels.clear());
        let found = t.items_in_area(&p(), "code").unwrap();
        assert_eq!(
            found,
            vec![(Kind::Record, t.issue_url(1)), (Kind::Finding, t.issue_url(3))]
        );
        assert!(t.items_in_area(&p(), "ops").unwrap().is_empty());
    }

    #[test]
    fn an_fl_labelled_issue_whose_block_cannot_be_read_refuses_the_area_scan() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        t.add_record_with_area(&p(), "a", Some("code")).unwrap();
        fake.web_edit(1, |i| i.body = "<!-- fl:meta\n{broken".into());
        let err = t.items_in_area(&p(), "design").unwrap_err().to_string();
        assert!(err.contains("carries an fl label, but its body"), "{err}");
    }

    #[test]
    fn the_visibility_is_read_live_and_only_private_holds_a_security_item() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        assert_eq!(t.visibility().unwrap(), "private");
        t.require_private().unwrap();
        for v in ["internal", "public"] {
            fake.state().repos[0].visibility = v.into();
            assert_eq!(t.visibility().unwrap(), v);
            assert!(
                matches!(t.require_private(), Err(StoreError::SecurityNotPrivate { .. })),
                "{v}"
            );
        }
        fake.state().omit_visibility = true;
        assert!(t.visibility().is_err(), "an unknown visibility is not private");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-core --lib` and `cargo test -p fl-github --lib tracker::tests::items_in tracker::tests::an_fl_labelled tracker::tests::the_visibility`
Expected: FAIL to compile — `RoutingFault`, `GithubTier`, `MemIssues`, `items_in_area`, `visibility` do not exist.

- [ ] **Step 3: Implement**

In `crates/core/src/routing.rs`, change the imports to `use crate::ids::{Kind, ProjectId, RecordId}; use crate::iri::Iri; use crate::store::{StoreError, Tracker};` and add, before `mod tests`:

```rust
/// Why the router refused (routing spec §4): each names its cause and what
/// to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoutingFault {
    #[error(
        "project {project} declares no areas, so fl cannot route its items. Run `fl routing set \
         --project <project> <area> <tier>` to declare one"
    )]
    Unrouted { project: ProjectId },
    #[error(
        "a routed project needs an area for every new item: name one with `--area`. The \
         declared areas: {}",
        list(declared)
    )]
    NoArea { declared: Vec<String> },
    #[error(
        "the finding's record {record} has no area to inherit, so name the finding's area with \
         `--area`. The declared areas: {}",
        list(declared)
    )]
    NothingToInherit { record: RecordId, declared: Vec<String> },
    #[error(
        "`{area}` is not an area this project declares. The declared areas: {}; `fl routing \
         set` declares a new one",
        list(declared)
    )]
    Undeclared { area: String, declared: Vec<String> },
    #[error(
        "the `{}` tier is not available on this machine: {why}. fl never puts an item in the \
         other tier by itself",
        tier.as_wire()
    )]
    TierUnavailable { tier: Tier, why: String },
    #[error(
        "the `{}` tier could not be read ({cause}), so fl refuses the whole list rather than \
         show part of it. Read one tier with `--tier local`",
        tier.as_wire()
    )]
    TierUnreadable { tier: Tier, cause: String },
    #[error(
        "{id} is not held by any tier this machine can read (searched: {}). A local item lives \
         in one store on one machine: it is held in another machine's local tier, or it does \
         not exist",
        list(searched)
    )]
    Elsewhere { id: Iri, searched: Vec<String> },
    #[error(
        "refused: {what} is security-sensitive, and the routing map sends it to {repo}, whose \
         visibility is `{visibility}`. Raise it with `--tier local`, or bind a private \
         repository; fl never moves it to the local tier by itself"
    )]
    SensitiveToPublic {
        what: String,
        repo: String,
        visibility: String,
    },
}

/// Names for a refusal, or `none`.
fn list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// The GitHub tier as the router sees it (routing spec §2.6): the CLI
/// opens the tracker on the first call that needs it, so work on local
/// items needs no network and no credential.
pub trait GithubTier {
    /// Whether this machine binds a GitHub repository for the project
    /// (routing spec §1.3). No request.
    fn available(&self) -> bool;
    /// Whether `id` names an issue of the bound repository, by its form
    /// alone. No request.
    fn claims(&self, id: &Iri) -> bool;
    /// Whether `id` has the form of a GitHub issue URL, of any repository.
    /// No request. With no binding, such an id is refused as the missing
    /// tier, never as held elsewhere (routing spec §1.3).
    fn issue_form(&self, id: &Iri) -> bool;
    /// The tracker, opened on the first call. ⚠ An error — with no
    /// binding, `TierUnavailable` — never a tracker that answers nothing.
    fn tracker(&self) -> Result<&dyn Tracker, StoreError>;
    /// `Ok` only when the repository is private (GitHub tracker spec §6);
    /// otherwise `SecurityNotPrivate`.
    fn require_private(&self) -> Result<(), StoreError>;
    /// Every item of `project` whose block names `area`, labelled or not
    /// (routing spec §1.2).
    fn items_in_area(&self, project: &ProjectId, area: &str)
    -> Result<Vec<(Kind, Iri)>, StoreError>;
}
```

In `crates/core/src/store.rs`, add `use crate::routing::RoutingFault;` and, after the `Ledger(#[from] LedgerFault)` variant:

```rust
    /// ⚠ The routing tracker refused (routing spec §4).
    #[error("{0}")]
    Routing(#[from] RoutingFault),
```

In `crates/core/src/lib.rs`, add after `pub mod log;`:

```rust
#[cfg(any(test, feature = "conformance"))]
#[doc(hidden)]
pub mod mem_issues;
```

and add `GithubTier, RoutingFault` to the `pub use routing::{…}` line.

Put this above the tests in `crates/core/src/mem_issues.rs`:

```rust
//! An in-memory GitHub tier, for the tests of the router and of what drives
//! it (routing spec §5). Ids are issue URLs of `acme/widgets`; records and
//! findings share one numbering, as issues do; no project is checked — the
//! router checks it, as it does for the real tracker. Never in the binary.

use crate::finding::{Finding, FindingState};
use crate::ids::{FindingId, Kind, ProjectId, RecordId};
use crate::iri::Iri;
use crate::model::{Record, State};
use crate::routing::{ForeignRecord, GithubTier, RoutingFault, Tier};
use crate::store::{StoreError, Tracker};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

/// Every issue URL this tier mints starts with this.
pub const ISSUES: &str = "https://github.com/acme/widgets/issues/";
const LABEL: &str = "github:acme/widgets";

#[derive(Default)]
pub struct MemIssues {
    inner: RefCell<Issues>,
    down: Cell<bool>,
    unbound: Cell<bool>,
    public: Cell<bool>,
    asked: Cell<u32>,
}

#[derive(Default)]
struct Issues {
    last: u64,
    records: BTreeMap<u64, Record>,
    findings: BTreeMap<u64, Finding>,
    aliases: BTreeMap<Iri, u64>,
}

impl MemIssues {
    /// GitHub cannot be reached: every call fails as unreachable.
    pub fn set_down(&self, down: bool) {
        self.down.set(down);
    }

    /// This machine binds no repository: the tier is not available.
    pub fn set_unbound(&self, unbound: bool) {
        self.unbound.set(unbound);
    }

    /// The repository is public.
    pub fn set_public(&self, public: bool) {
        self.public.set(public);
    }

    /// How many times the router asked for this tier's tracker.
    pub fn asked(&self) -> u32 {
        self.asked.get()
    }

    pub fn issue(n: u64) -> Iri {
        Iri::parse(&format!("{ISSUES}{n}")).expect("an issue URL is an IRI")
    }

    fn up(&self) -> Result<(), StoreError> {
        if self.down.get() {
            return Err(StoreError::Unreachable {
                store: LABEL.into(),
                cause: "connection refused".into(),
            });
        }
        Ok(())
    }

    /// The issue `id` names, by its own URL or an alias. ⚠ Any other id is
    /// `NotOwned`, never an answer of `None`.
    fn number(&self, id: &Iri) -> Result<u64, StoreError> {
        if let Some(n) = id.as_str().strip_prefix(ISSUES).and_then(|n| n.parse().ok()) {
            return Ok(n);
        }
        self.inner
            .borrow()
            .aliases
            .get(id)
            .copied()
            .ok_or_else(|| StoreError::NotOwned {
                id: id.clone(),
                searched: vec![LABEL.into()],
            })
    }

    fn mint(&self) -> u64 {
        let mut s = self.inner.borrow_mut();
        s.last += 1;
        s.last
    }

    fn insert_finding(&self, mut finding: Finding, record: RecordId) -> Result<FindingId, StoreError> {
        if finding.security && self.public.get() {
            return Err(StoreError::SecurityNotPrivate {
                repo: "acme/widgets".into(),
                visibility: "public".into(),
            });
        }
        let n = self.mint();
        let id = FindingId(Self::issue(n));
        finding.id = id.clone();
        finding.record = record;
        self.inner.borrow_mut().findings.insert(n, finding);
        Ok(id)
    }
}

impl Tracker for MemIssues {
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError> {
        self.up()?;
        let n = self.mint();
        let id = RecordId(Self::issue(n));
        self.inner.borrow_mut().records.insert(
            n,
            Record {
                id: id.clone(),
                project: project.clone(),
                title: title.to_string(),
                state: State::Todo,
                also_known_as: vec![],
                area: area.map(str::to_string),
            },
        );
        Ok(id)
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.up()?;
        let n = self.number(id.iri())?;
        Ok(self.inner.borrow().records.get(&n).cloned())
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        self.up()?;
        let s = self.inner.borrow();
        Ok(s.records.values().filter(|r| r.project == *project).cloned().collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        self.up()?;
        let n = self.number(id.iri())?;
        let mut s = self.inner.borrow_mut();
        let r = s
            .records
            .get_mut(&n)
            .ok_or_else(|| StoreError::NoSuchRecord(id.clone()))?;
        r.state = state;
        Ok(())
    }

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        self.up()?;
        let n = self.number(finding.record.iri())?;
        let primary = {
            let s = self.inner.borrow();
            if s.findings.contains_key(&n) {
                return Err(StoreError::WrongKind {
                    id: finding.record.iri().clone(),
                    expected: Kind::Record,
                    found: Kind::Finding,
                });
            }
            s.records
                .get(&n)
                .map(|r| r.id.clone())
                .ok_or_else(|| StoreError::NoSuchRecord(finding.record.clone()))?
        };
        self.insert_finding(finding, primary)
    }

    fn add_finding_checked(
        &self,
        finding: Finding,
        record: ForeignRecord,
    ) -> Result<FindingId, StoreError> {
        self.up()?;
        if record.tier() != Tier::Local {
            return Err(StoreError::Backend(format!(
                "{} is a record on GitHub, so a finding about it is raised with `add_finding`, \
                 which checks it here",
                record.id()
            )));
        }
        self.insert_finding(finding, record.id().clone())
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        self.up()?;
        let n = self.number(id.iri())?;
        Ok(self.inner.borrow().findings.get(&n).cloned())
    }

    /// As the GitHub tracker: the stored id, aliases, record, raiser,
    /// security mark and area are kept; everything else is the caller's.
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        self.up()?;
        let n = self.number(finding.id.iri())?;
        let mut s = self.inner.borrow_mut();
        let stored = s
            .findings
            .get_mut(&n)
            .ok_or_else(|| StoreError::NoSuchFinding(finding.id.clone()))?;
        let kept = stored.clone();
        *stored = finding.clone();
        stored.id = kept.id;
        stored.also_known_as = kept.also_known_as;
        stored.record = kept.record;
        stored.raised_by = kept.raised_by;
        stored.security = kept.security;
        stored.area = kept.area;
        Ok(())
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        self.up()?;
        let s = self.inner.borrow();
        Ok(s.findings.values().filter(|f| f.project == *project).cloned().collect())
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        self.up()?;
        let s = self.inner.borrow();
        Ok(s.findings
            .values()
            .filter(|f| f.raised_by == actor && f.state == FindingState::Withdrawn)
            .count() as u64)
    }

    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        self.up()?;
        // One namespace: an issue URL of this repository, or another
        // item's alias, is taken.
        let taken = alias.as_str().starts_with(ISSUES)
            || self.inner.borrow().aliases.contains_key(&alias);
        if taken {
            return Err(StoreError::AlreadyExists(alias));
        }
        let n = self.number(primary)?;
        let mut s = self.inner.borrow_mut();
        if let Some(r) = s.records.get_mut(&n) {
            r.also_known_as.push(alias.clone());
        } else if let Some(f) = s.findings.get_mut(&n) {
            f.also_known_as.push(alias.clone());
        } else {
            return Err(StoreError::NotOwned {
                id: primary.clone(),
                searched: vec![LABEL.into()],
            });
        }
        s.aliases.insert(alias, n);
        Ok(())
    }
}

impl GithubTier for MemIssues {
    fn available(&self) -> bool {
        !self.unbound.get()
    }

    /// Like the CLI's tier: with no binding it claims nothing.
    fn claims(&self, id: &Iri) -> bool {
        !self.unbound.get() && id.as_str().starts_with(ISSUES)
    }

    fn issue_form(&self, id: &Iri) -> bool {
        id.as_str().starts_with("https://github.com/") && id.as_str().contains("/issues/")
    }

    fn tracker(&self) -> Result<&dyn Tracker, StoreError> {
        self.asked.set(self.asked.get() + 1);
        if self.unbound.get() {
            return Err(RoutingFault::TierUnavailable {
                tier: Tier::Github,
                why: "this test binds no repository".into(),
            }
            .into());
        }
        self.up()?;
        Ok(self)
    }

    fn require_private(&self) -> Result<(), StoreError> {
        self.up()?;
        if self.public.get() {
            return Err(StoreError::SecurityNotPrivate {
                repo: "acme/widgets".into(),
                visibility: "public".into(),
            });
        }
        Ok(())
    }

    fn items_in_area(
        &self,
        project: &ProjectId,
        area: &str,
    ) -> Result<Vec<(Kind, Iri)>, StoreError> {
        self.up()?;
        let s = self.inner.borrow();
        let records = s
            .records
            .values()
            .filter(|r| r.project == *project && r.area.as_deref() == Some(area))
            .map(|r| (Kind::Record, r.id.0.clone()));
        let findings = s
            .findings
            .values()
            .filter(|f| f.project == *project && f.area.as_deref() == Some(area))
            .map(|f| (Kind::Finding, f.id.0.clone()));
        Ok(records.chain(findings).collect())
    }
}
```

In `crates/github/src/tracker.rs`, replace `fn require_private` with:

```rust
    /// The repository's visibility — `private`, `internal` or `public` —
    /// read live, every time: visibility can change. ⚠ An answer that
    /// cannot be read is an error: an unknown visibility is not private.
    pub fn visibility(&self) -> Result<String, StoreError> {
        let r = self.client.send(
            Method::Get,
            &format!("/repos/{}", self.repo.full_name),
            None,
        )?;
        let refuse = |why: String| {
            backend(format!(
                "fl could not read the visibility of {} ({why}), and an unknown visibility is \
                 not private. Retry",
                self.repo.full_name
            ))
        };
        if r.status != 200 {
            return Err(refuse(format!("GitHub answered {}", r.status)));
        }
        r.body
            .get("visibility")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| refuse("the answer names no visibility".into()))
    }

    /// Spec §6: only a `private` repository may hold a security finding.
    pub fn require_private(&self) -> Result<(), StoreError> {
        let visibility = self.visibility()?;
        if visibility == "private" {
            Ok(())
        } else {
            Err(StoreError::SecurityNotPrivate {
                repo: self.repo.full_name.clone(),
                visibility,
            })
        }
    }

    /// Every fl item of `project` whose block names `area` (routing spec
    /// §1.2), found by reading every issue's block — labelled or not — so an
    /// item that lost its labels is not missed. An issue without an fl
    /// label whose block cannot be read is not fl's, and is passed over.
    /// ⚠ One WITH an fl label whose block cannot be read is an error: it
    /// may name the area.
    pub fn items_in_area(
        &self,
        project: &ProjectId,
        area: &str,
    ) -> Result<Vec<(Kind, Iri)>, StoreError> {
        let mut out = Vec::new();
        let mut unreadable = None;
        self.each_issue(None, Order::OldestFirst, LIST_PAGE, |node| {
            let issue = IssueView::from_graphql(node)?;
            match meta::parse_body(&issue.body) {
                Ok((_, m)) => {
                    if m.project == *project && m.area.as_deref() == Some(area) {
                        out.push((m.kind.as_kind(), issue.url.clone()));
                    }
                }
                Err(e) if issue.labels.iter().any(|l| l.starts_with("fl:")) => {
                    unreadable = Some(backend(format!(
                        "{} carries an fl label, but its body {e}, so fl cannot tell whether it \
                         names the area `{area}`. Restore its block from the issue's edit \
                         history, or remove its fl labels",
                        issue.url
                    )));
                    return Ok(false);
                }
                Err(_) => {}
            }
            Ok(true)
        })?;
        match unreadable {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-core --lib` and `cargo test -p fl-github --lib`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

1. `list`'s empty case: return `names.join(", ")` always → `cargo test -p fl-core --lib routing::tests::every_routing_refusal_names_its_remedy` red.
2. `MemIssues::up` in `get_record` and in `list_records`: delete each → `cargo test -p fl-core --lib mem_issues::tests::a_tier_that_is_down_or_unbound_answers_with_an_error_never_an_answer` red. (The other methods' `up` calls are the same line repeated; Task 8's merged-list and fallback tests go red for `list_findings`, `get_finding` and `withdrawals_by`.)
3. `tracker()`'s unbound refusal: delete it → the same test red.
3a. `claims` honours an unbound tier: drop `!self.unbound.get() &&` → the same test red.
4. `tracker()`'s `up`: delete it → the same test red (`tracker().is_err()`).
5. `number` refuses a stranger: return `Ok(0)` instead of `NotOwned` → `mem_issues::tests::an_id_this_tier_never_held_is_not_owned_and_a_public_one_refuses_security` red.
6. `insert_finding`'s security rule: delete it → the same test red.
7. `items_in_area`'s project conjunct (both filters): drop `r.project == *project &&` → the same test red (another project's).
8. `add_alias`'s own-URL rule: drop `alias.as_str().starts_with(ISSUES) ||` → `mem_issues::tests::the_in_memory_github_tier_meets_the_tracker_contract` red (`an_alias_already_in_use_is_refused_and_names_it`).
9. `update_finding` keeps the aliases: drop `stored.also_known_as = kept.also_known_as;` → the conformance test red.
10. `GithubTracker::items_in_area`'s project conjunct: drop it → `cargo test -p fl-github --lib tracker::tests::items_in_an_area_are_found_by_their_block_labelled_or_not` red (issue 4).
11. Its area conjunct: drop it → the same test red.
12. Its labelled-and-unreadable arm: replace its guard with `if false` → `tracker::tests::an_fl_labelled_issue_whose_block_cannot_be_read_refuses_the_area_scan` red.
13. The unlabelled-and-unreadable arm passes over: make `Err(_) => {}` return the error → `items_in_an_area_are_found…` red (issue 5).
14. `visibility`'s missing-field arm: return `Ok("private".into())` there → `tracker::tests::the_visibility_is_read_live_and_only_private_holds_a_security_item` red (`omit_visibility`). Its status check is not observable with the fake: the fake's failed repository read names no visibility either, so both branches refuse. Not a guard the fake can tell apart.
15. `require_private`'s comparison: make it `visibility != "public"` → the same test red (`internal`).

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/routing.rs crates/core/src/store.rs crates/core/src/mem_issues.rs crates/core/src/lib.rs crates/github/src/tracker.rs
git commit -m "feat(core,github): the router's refusals, the GitHub tier it asks, and one in memory

RoutingFault, under StoreError::Routing, names each refusal of routing
spec 4 and its remedy. GithubTier is what the pure router asks of GitHub:
whether it is bound, whether an id is its issue, the tracker opened on
first use, the private-repository rule, and the items in an area. MemIssues
implements it in memory and passes the tracker suite. The GitHub tracker
finds items in an area by their blocks, labelled or not, and reads its
visibility live. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 7: The routing tracker places and creates

`TieredTracker` (routing spec §2) decides where a new item goes — from `--tier` when given, else from its area through the map; a finding's area is the one given, else its record's (§1.1) — and makes every check before anything is written: an area, declared; a map for the project; the GitHub tier available; a sensitive area, a security finding, or a finding about a record in a sensitive area — or in an area the map no longer declares, failing closed — never sent to a non-private repository (§2.1, decisions 13, 21 and 22). Such a finding carries the security mark wherever it goes. Placement is the spec's `create_in(tier, …)` (plan ruling 2). A finding's record is read in the tier that holds it, and a finding in the other tier is written through a `ForeignRecord` only the router builds (§2.5). Lookups go through one function, `route`, which Task 8 builds the `Tracker` role on and plan B extends.

**Blast radius:** new module. `ForeignRecord` gains its crate-private constructor; `for_tests` now uses it.

**Files:**
- Create: `crates/core/src/tiered.rs`
- Modify: `crates/core/src/routing.rs` (`ForeignRecord::checked`)
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `GithubTier`, `Routes`, `RoutingFault`, `ForeignRecord`, `RoutingMap` (Tasks 3, 5, 6); `MemIssues` (Task 6) in tests.
- Produces: `pub struct TieredTracker<'a> { pub catalog: &'a dyn Catalog, pub local: &'a dyn Tracker, pub routes: &'a dyn Routes, pub github: &'a dyn GithubTier }` with `tier_of(&self, id: &Iri) -> Tier`, `place_record(&self, project: &ProjectId, area: Option<&str>, tier: Option<Tier>) -> Result<Placement, StoreError>`, `place_finding(&self, finding: &Finding, tier: Option<Tier>) -> Result<FindingPlacement, StoreError>`, `add_record_at(&self, project: &ProjectId, title: &str, at: &Placement) -> Result<RecordId, StoreError>`, `add_finding_at(&self, finding: Finding, at: &FindingPlacement) -> Result<FindingId, StoreError>`, and crate-private `route<T>(&self, id: &Iri, act: impl Fn(&dyn Tracker) -> Result<T, StoreError>) -> Result<(Tier, T), StoreError>`, `record_of(&self, id: &RecordId) -> Result<(Tier, Record), StoreError>`, `tracker_in(&self, tier: Tier) -> Result<&dyn Tracker, StoreError>`; `Placement` (`tier()`, `area()`, `sensitive()`); `RecordSeen { pub id: RecordId, pub title: String, pub tier: Tier }`; `FindingPlacement` (`at()`, `inherited()`, `record()`, `crosses()`); `pub(crate) fn ForeignRecord::checked(id: RecordId, title: String, tier: Tier) -> ForeignRecord`. The test helpers `world()`, `W::router()` and `EveryProject` live in `tiered::tests` and Task 8 uses them.

- [ ] **Step 1: Write the module's tests**

Create `crates/core/src/tiered.rs` with this `mod tests` at its foot (Step 3 gives the code above it):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::mem_issues::MemIssues;
    use crate::model::{CommandSpec, GateKind, PopulationDelivery, Selector};

    /// A project with the starting map in a local `MemStore`, and an
    /// in-memory GitHub tier.
    struct W {
        local: MemStore,
        issues: MemIssues,
        p: ProjectId,
    }

    fn world() -> W {
        let local = MemStore::default();
        let p = local.add_project("/p").unwrap();
        local.set_routes(&p, &RoutingMap::starting()).unwrap();
        W {
            local,
            issues: MemIssues::default(),
            p,
        }
    }

    impl W {
        fn router(&self) -> TieredTracker<'_> {
            TieredTracker {
                catalog: &self.local,
                local: &self.local,
                routes: &self.local,
                github: &self.issues,
            }
        }

        /// A record in `area`, placed by the map.
        fn record(&self, area: &str, title: &str) -> RecordId {
            let t = self.router();
            let at = t.place_record(&self.p, Some(area), None).unwrap();
            t.add_record_at(&self.p, title, &at).unwrap()
        }
    }

    /// Every project routed by one map, whatever its id: lets a test name
    /// an id the catalog holds as another kind.
    struct EveryProject(RoutingMap);

    impl Routes for EveryProject {
        fn routes(&self, _: &ProjectId) -> Result<Option<RoutingMap>, StoreError> {
            Ok(Some(self.0.clone()))
        }
    }

    fn fault(e: &StoreError) -> Option<&RoutingFault> {
        match e {
            StoreError::Routing(f) => Some(f),
            _ => None,
        }
    }

    // Routing spec §2.1: the tier comes from the item's area through the
    // map, and the area is recorded either way.
    #[test]
    fn a_record_goes_to_the_tier_its_area_routes_to_and_keeps_its_area() {
        let w = world();
        let t = w.router();
        let at = t.place_record(&w.p, Some("code"), None).unwrap();
        assert_eq!((at.tier(), at.area()), (Tier::Local, "code"));
        let local = t.add_record_at(&w.p, "fix", &at).unwrap();
        assert_eq!(
            w.local.get_record(&local).unwrap().unwrap().area.as_deref(),
            Some("code")
        );
        let gh = w.record("design", "look");
        assert_eq!(gh.iri(), &MemIssues::issue(1));
        assert_eq!(
            w.issues.get_record(&gh).unwrap().unwrap().area.as_deref(),
            Some("design")
        );
        assert_eq!(t.tier_of(gh.iri()), Tier::Github);
        assert_eq!(t.tier_of(local.iri()), Tier::Local);
    }

    #[test]
    fn a_tier_given_overrides_the_map_and_the_area_is_recorded_either_way() {
        let w = world();
        let t = w.router();
        let at = t.place_record(&w.p, Some("code"), Some(Tier::Github)).unwrap();
        let r = t.add_record_at(&w.p, "t", &at).unwrap();
        assert_eq!(
            w.issues.get_record(&r).unwrap().unwrap().area.as_deref(),
            Some("code")
        );
        let at = t.place_record(&w.p, Some("design"), Some(Tier::Local)).unwrap();
        assert_eq!(at.tier(), Tier::Local);
    }

    // Routing spec §2.1, §4: no area, or one the map does not declare, is
    // refused naming the declared areas; a project with no map, naming
    // `fl routing set`.
    #[test]
    fn a_create_with_no_area_an_undeclared_one_or_no_map_is_refused() {
        let w = world();
        let t = w.router();
        let err = t.place_record(&w.p, None, None).unwrap_err();
        assert!(matches!(fault(&err), Some(RoutingFault::NoArea { .. })), "{err:?}");
        assert!(err.to_string().contains("code, design, product, security, tests"), "{err}");
        let err = t.place_record(&w.p, Some("ops"), Some(Tier::Local)).unwrap_err();
        assert!(matches!(fault(&err), Some(RoutingFault::Undeclared { .. })), "{err:?}");
        let q = w.local.add_project("/q").unwrap();
        let err = t.place_record(&q, Some("code"), None).unwrap_err();
        assert!(matches!(fault(&err), Some(RoutingFault::Unrouted { .. })), "{err:?}");
        assert_eq!(w.issues.asked(), 0, "nothing asked of GitHub");
    }

    // Routing spec §1.3: "routing never changes tier silently".
    #[test]
    fn a_github_tier_create_with_no_binding_is_refused_and_lands_nowhere() {
        let w = world();
        w.issues.set_unbound(true);
        let t = w.router();
        let err = t.place_record(&w.p, Some("design"), None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "{err:?}"
        );
        assert!(w.local.list_records(&w.p).unwrap().is_empty());
        w.record("code", "t");
        assert_eq!(w.issues.asked(), 1, "a local create never asks GitHub");
    }

    // Routing spec §1.1: a finding's area is the one given, or else its
    // record's — which the placement says, for the note the CLI prints.
    #[test]
    fn a_finding_inherits_its_records_area_and_says_so() {
        let w = world();
        let t = w.router();
        let r = w.record("design", "t");
        let f = Finding::raise(w.p.clone(), r, "rev", "c");
        let at = t.place_finding(&f, None).unwrap();
        assert!(at.inherited());
        assert_eq!((at.at().tier(), at.at().area()), (Tier::Github, "design"));
        let id = t.add_finding_at(f.clone(), &at).unwrap();
        assert_eq!(
            w.issues.get_finding(&id).unwrap().unwrap().area.as_deref(),
            Some("design")
        );
        let mut given = f;
        given.area = Some("code".into());
        let at = t.place_finding(&given, None).unwrap();
        assert!(!at.inherited());
        assert_eq!(at.at().tier(), Tier::Local);
    }

    #[test]
    fn a_finding_whose_record_has_no_area_and_names_none_is_refused() {
        let w = world();
        let t = w.router();
        let old = w.local.add_record(&w.p, "made before areas").unwrap();
        let err = t
            .place_finding(&Finding::raise(w.p.clone(), old, "rev", "c"), None)
            .unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::NothingToInherit { .. })),
            "{err:?}"
        );
    }

    // Routing spec §2.5: a reference into the other tier is stored only
    // through a ForeignRecord the router built after reading the record —
    // and it names the record's primary IRI, even when the finding was
    // raised against an alias.
    #[test]
    fn a_finding_crosses_tiers_only_through_a_checked_foreign_record() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "local one");
        let alias = Iri::parse("https://github.com/o/r/issues/41").unwrap();
        w.local.add_alias(local.iri(), alias.clone()).unwrap();
        let mut f = Finding::raise(w.p.clone(), RecordId(alias), "rev", "c");
        f.area = Some("design".into());
        assert!(
            matches!(w.issues.add_finding(f.clone()), Err(StoreError::NotOwned { .. })),
            "unchecked, the GitHub tier refuses a record it does not hold"
        );
        let at = t.place_finding(&f, None).unwrap();
        assert!(at.crosses());
        assert_eq!(at.record().title, "local one");
        let on_github = t.add_finding_at(f, &at).unwrap();
        assert_eq!(w.issues.get_finding(&on_github).unwrap().unwrap().record, local);

        let remote = w.record("product", "remote one");
        let mut f = Finding::raise(w.p.clone(), remote.clone(), "rev", "c");
        f.area = Some("tests".into());
        let at = t.place_finding(&f, None).unwrap();
        assert!(at.crosses());
        let on_local = t.add_finding_at(f, &at).unwrap();
        assert_eq!(w.local.get_finding(&on_local).unwrap().unwrap().record, remote);

        let mut same = Finding::raise(w.p.clone(), remote, "rev", "c");
        same.area = Some("design".into());
        let at = t.place_finding(&same, None).unwrap();
        assert!(!at.crosses());
        t.add_finding_at(same, &at).unwrap();
    }

    // Routing spec decision 13: a sensitive area makes a security finding.
    #[test]
    fn a_finding_in_a_sensitive_area_is_a_security_finding() {
        let w = world();
        let t = w.router();
        let r = w.record("code", "t");
        let mut f = Finding::raise(w.p.clone(), r, "rev", "c");
        f.area = Some("security".into());
        let at = t.place_finding(&f, None).unwrap();
        assert!(at.at().sensitive());
        let id = t.add_finding_at(f, &at).unwrap();
        assert!(w.issues.get_finding(&id).unwrap().unwrap().security);
    }

    // Routing spec §2.1: the map never sends a security item to a public
    // repository; fl refuses, naming `--tier local`, before it writes.
    #[test]
    fn a_sensitive_area_routed_to_a_public_repository_is_refused_before_anything_is_written() {
        let w = world();
        w.issues.set_public(true);
        let t = w.router();
        let err = t.place_record(&w.p, Some("security"), None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveToPublic { .. })),
            "{err:?}"
        );
        assert!(err.to_string().contains("--tier local"), "{err}");
        let r = w.record("code", "t");
        let mut f = Finding::raise(w.p.clone(), r, "rev", "c");
        f.area = Some("design".into());
        f.security = true;
        let err = t.place_finding(&f, None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveToPublic { .. })),
            "{err:?}"
        );
        // Named by the person, the tier is theirs: the tracker's own refusal.
        let err = t.place_finding(&f, Some(Tier::Github)).unwrap_err();
        assert!(matches!(err, StoreError::SecurityNotPrivate { .. }), "{err:?}");
        assert!(w.issues.list_records(&w.p).unwrap().is_empty());
        assert!(w.issues.list_findings(&w.p).unwrap().is_empty());
        assert_eq!(
            t.place_finding(&f, Some(Tier::Local)).unwrap().at().tier(),
            Tier::Local,
            "the local tier takes it"
        );
    }

    // Routing spec decision 21: nothing about an item in a sensitive area
    // reaches a repository that is not private — not even a finding, in any
    // area, about a local record in a sensitive area.
    #[test]
    fn a_finding_about_a_record_in_a_sensitive_area_never_reaches_a_public_repository() {
        let w = world();
        w.issues.set_public(true);
        let t = w.router();
        let at = t.place_record(&w.p, Some("security"), Some(Tier::Local)).unwrap();
        let secret = t.add_record_at(&w.p, "the key leaks", &at).unwrap();
        let mut f = Finding::raise(w.p.clone(), secret, "rev", "c");
        f.area = Some("design".into());
        let err = t.place_finding(&f, None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveToPublic { .. })),
            "{err:?}"
        );
        assert!(err.to_string().contains("about a record in a sensitive area"), "{err}");
        let err = t.place_finding(&f, Some(Tier::Github)).unwrap_err();
        assert!(matches!(err, StoreError::SecurityNotPrivate { .. }), "{err:?}");
        assert!(w.issues.list_findings(&w.p).unwrap().is_empty());
        // A finding about an ordinary local record is placed; the CLI warns.
        let plain = w.record("code", "t");
        let mut g = Finding::raise(w.p.clone(), plain, "rev", "c");
        g.area = Some("design".into());
        assert_eq!(t.place_finding(&g, None).unwrap().at().tier(), Tier::Github);
        // Kept local, it is a security finding (decision 22).
        let at = t.place_finding(&f, Some(Tier::Local)).unwrap();
        let local = t.add_finding_at(f.clone(), &at).unwrap();
        assert!(w.local.get_finding(&local).unwrap().unwrap().security);
        w.issues.set_public(false);
        let at = t.place_finding(&f, None).unwrap();
        assert_eq!(at.at().tier(), Tier::Github);
        let on_github = t.add_finding_at(f, &at).unwrap();
        assert!(w.issues.get_finding(&on_github).unwrap().unwrap().security);
    }

    // Routing spec decision 22: an area the map no longer declares — removed
    // on the authoring machine while this machine's records still name it —
    // counts as sensitive: fail closed.
    #[test]
    fn a_record_whose_area_the_map_no_longer_declares_counts_as_sensitive() {
        let w = world();
        w.issues.set_public(true);
        let t = w.router();
        let old = w
            .local
            .add_record_with_area(&w.p, "made under an older map", Some("ops"))
            .unwrap();
        let mut f = Finding::raise(w.p.clone(), old, "rev", "c");
        f.area = Some("design".into());
        let err = t.place_finding(&f, None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveToPublic { .. })),
            "{err:?}"
        );
        assert!(err.to_string().contains("--tier local"), "{err}");
        let at = t.place_finding(&f, Some(Tier::Local)).unwrap();
        let id = t.add_finding_at(f, &at).unwrap();
        assert!(w.local.get_finding(&id).unwrap().unwrap().security);
    }

    // Routing spec §2.5: a record in a tier that cannot be reached is an
    // error, never "no such record".
    #[test]
    fn a_record_in_an_unreachable_tier_is_an_error_not_no_such_record() {
        let w = world();
        let t = w.router();
        let r = w.record("design", "t");
        w.issues.set_down(true);
        let mut f = Finding::raise(w.p.clone(), r, "rev", "c");
        f.area = Some("code".into());
        let err = t.place_finding(&f, None).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    // Routing spec §2.5: the GitHub tracker cannot see the catalog, so the
    // router checks a GitHub create's project there.
    #[test]
    fn a_github_create_checks_its_project_in_the_catalog() {
        let w = world();
        let every = EveryProject(RoutingMap::starting());
        let t = TieredTracker {
            catalog: &w.local,
            local: &w.local,
            routes: &every,
            github: &w.issues,
        };
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
        let g = w.local.add_gate(&w.p, "g", kind, sel, 1, "c", "o").unwrap();
        let as_project = ProjectId(g.0);
        let at = t.place_record(&as_project, Some("design"), None).unwrap();
        let err = t.add_record_at(&as_project, "t", &at).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::WrongKind { expected: Kind::Project, found: Kind::Gate, .. }
            ),
            "{err:?}"
        );
        assert!(w.issues.list_records(&as_project).unwrap().is_empty());
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-core --lib tiered::tests::`
Expected: FAIL to compile — the module's code does not exist yet.

- [ ] **Step 3: Implement**

In `crates/core/src/routing.rs`, in `impl ForeignRecord`, add before `for_tests`:

```rust
    /// The router's proof that it read `id` in `tier` (routing spec §2.5).
    pub(crate) fn checked(id: RecordId, title: String, tier: Tier) -> Self {
        Self { id, title, tier }
    }
```

and make `for_tests`'s body `Self::checked(id, title.to_string(), tier)`.

Put this above the tests in `crates/core/src/tiered.rs`:

```rust
//! The routing tracker (routing spec §2): one `Tracker` over a project's
//! two tiers — the local store for developer-level items, a GitHub
//! repository for human-level ones — routing each new item by its area.

use crate::finding::Finding;
use crate::ids::{FindingId, Kind, ProjectId, RecordId};
use crate::iri::Iri;
use crate::model::Record;
use crate::routing::{ForeignRecord, GithubTier, Routes, RoutingFault, RoutingMap, Tier};
use crate::store::{Catalog, StoreError, Tracker};

/// `Tracker` over a project's two tiers (routing spec §2). The CLI builds
/// it for a routed store; the gate engine sees one tracker, as before.
pub struct TieredTracker<'a> {
    /// The local store's catalog. A create on the GitHub tier checks its
    /// project here: the GitHub tracker cannot see the catalog (§2.5).
    pub catalog: &'a dyn Catalog,
    /// The local tier.
    pub local: &'a dyn Tracker,
    /// Each project's routing map: the local store, which holds the map it
    /// authored or imported.
    pub routes: &'a dyn Routes,
    /// The GitHub tier, opened on the first call that needs it (§2.6).
    pub github: &'a dyn GithubTier,
}

/// Where a new record goes, decided before anything is written (§2.1).
/// Only the router builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    tier: Tier,
    area: String,
    sensitive: bool,
}

impl Placement {
    pub fn tier(&self) -> Tier {
        self.tier
    }

    pub fn area(&self) -> &str {
        &self.area
    }

    /// Whether the area is sensitive: a finding placed here is a security
    /// finding (decision 13).
    pub fn sensitive(&self) -> bool {
        self.sensitive
    }
}

/// A finding's record, as the tier that holds it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordSeen {
    pub id: RecordId,
    pub title: String,
    pub tier: Tier,
}

/// Where a new finding goes, and the record it was checked against (§2.1,
/// §2.5). Only the router builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingPlacement {
    at: Placement,
    inherited: bool,
    record: RecordSeen,
    /// The record's area is sensitive, or one the map no longer declares
    /// (decisions 21, 22): the finding is a security item, wherever it goes.
    about_sensitive: bool,
}

impl FindingPlacement {
    pub fn at(&self) -> &Placement {
        &self.at
    }

    /// Whether the area came from the record (§1.1).
    pub fn inherited(&self) -> bool {
        self.inherited
    }

    pub fn record(&self) -> &RecordSeen {
        &self.record
    }

    /// Whether the finding and its record are in different tiers (§2.5).
    pub fn crosses(&self) -> bool {
        self.record.tier != self.at.tier
    }
}

impl<'a> TieredTracker<'a> {
    /// The tier an id belongs to by its form alone: an issue of the bound
    /// repository is GitHub's; anything else is asked of the local tier
    /// first (§2.2).
    pub fn tier_of(&self, id: &Iri) -> Tier {
        if self.github.claims(id) {
            Tier::Github
        } else {
            Tier::Local
        }
    }

    /// The tracker of `tier`; GitHub's is opened now if it is not yet.
    pub(crate) fn tracker_in(&self, tier: Tier) -> Result<&'a dyn Tracker, StoreError> {
        match tier {
            Tier::Local => Ok(self.local),
            Tier::Github => self.github.tracker(),
        }
    }

    fn map_of(&self, project: &ProjectId) -> Result<RoutingMap, StoreError> {
        self.routes
            .routes(project)?
            .ok_or_else(|| RoutingFault::Unrouted { project: project.clone() }.into())
    }

    /// `act` in the tier that owns `id` (§2.2): an issue of the bound
    /// repository in GitHub; any other id in the local tier, then — if the
    /// local tier never held it — in GitHub, whose alias scan finds an item
    /// another machine moved there. With no binding, an issue URL the local
    /// tier does not hold is the missing tier's (§1.3). ⚠ An id neither tier
    /// holds is `Elsewhere`, never `NotOwned`; a tier that cannot be reached
    /// is its own error, never "not held".
    pub(crate) fn route<T>(
        &self,
        id: &Iri,
        act: impl Fn(&dyn Tracker) -> Result<T, StoreError>,
    ) -> Result<(Tier, T), StoreError> {
        if self.github.claims(id) {
            return Ok((Tier::Github, act(self.github.tracker()?)?));
        }
        let mut searched = match act(self.local) {
            Err(StoreError::NotOwned { searched, .. }) => searched,
            other => return Ok((Tier::Local, other?)),
        };
        if !self.github.available() {
            // An issue URL is GitHub's all the same: with no binding it is
            // refused as the missing tier, never as held elsewhere (§1.3).
            if self.github.issue_form(id) {
                self.github.tracker()?;
            }
            return Err(RoutingFault::Elsewhere { id: id.clone(), searched }.into());
        }
        match act(self.github.tracker()?) {
            Err(StoreError::NotOwned { searched: theirs, .. }) => {
                searched.extend(theirs);
                Err(RoutingFault::Elsewhere { id: id.clone(), searched }.into())
            }
            other => Ok((Tier::Github, other?)),
        }
    }

    /// The record `id` names, and the tier that holds it.
    pub(crate) fn record_of(&self, id: &RecordId) -> Result<(Tier, Record), StoreError> {
        match self.route(id.iri(), |t| t.get_record(id))? {
            (tier, Some(r)) => Ok((tier, r)),
            (_, None) => Err(StoreError::NoSuchRecord(id.clone())),
        }
    }

    fn check_project(&self, project: &ProjectId) -> Result<(), StoreError> {
        match self.catalog.kind_of(project.iri())? {
            Kind::Project => Ok(()),
            found => Err(StoreError::WrongKind {
                id: project.iri().clone(),
                expected: Kind::Project,
                found,
            }),
        }
    }

    /// The private-repository rule (GitHub tracker spec §6). When the map
    /// chose the tier, the refusal names `--tier local` (§2.1).
    fn private_or_refuse(&self, by_map: bool, what: &str) -> Result<(), StoreError> {
        match self.github.require_private() {
            Err(StoreError::SecurityNotPrivate { repo, visibility }) if by_map => {
                Err(RoutingFault::SensitiveToPublic {
                    what: what.to_string(),
                    repo,
                    visibility,
                }
                .into())
            }
            other => other,
        }
    }

    fn place(
        &self,
        map: &RoutingMap,
        area: &str,
        tier: Option<Tier>,
        security: bool,
        what: &str,
    ) -> Result<Placement, StoreError> {
        let route = map.route(area).ok_or_else(|| RoutingFault::Undeclared {
            area: area.to_string(),
            declared: map.declared(),
        })?;
        let (tier, by_map) = match tier {
            Some(t) => (t, false),
            None => (route.tier, true),
        };
        if tier == Tier::Github {
            // Before anything is written: a tier this machine cannot open
            // refuses the create here, never after a local write.
            self.github.tracker()?;
            if security || route.sensitive {
                self.private_or_refuse(by_map, what)?;
            }
        }
        Ok(Placement {
            tier,
            area: area.to_string(),
            sensitive: route.sensitive,
        })
    }

    /// Where a new record in `area` goes (§2.1): `tier` when given, else
    /// the area's tier through the project's map.
    pub fn place_record(
        &self,
        project: &ProjectId,
        area: Option<&str>,
        tier: Option<Tier>,
    ) -> Result<Placement, StoreError> {
        let map = self.map_of(project)?;
        let area = area.ok_or_else(|| RoutingFault::NoArea { declared: map.declared() })?;
        self.place(&map, area, tier, false, "this record")
    }

    /// Where a new finding goes (§1.1, §2.1): its area is the one it names,
    /// or else its record's, read in the tier that holds the record.
    pub fn place_finding(
        &self,
        finding: &Finding,
        tier: Option<Tier>,
    ) -> Result<FindingPlacement, StoreError> {
        let map = self.map_of(&finding.project)?;
        let (record_tier, record) = self.record_of(&finding.record)?;
        let (area, inherited) = match (&finding.area, &record.area) {
            (Some(a), _) => (a.clone(), false),
            (None, Some(a)) => (a.clone(), true),
            (None, None) => {
                return Err(RoutingFault::NothingToInherit {
                    record: record.id.clone(),
                    declared: map.declared(),
                }
                .into());
            }
        };
        // Routing spec decision 21: a finding about a record in a sensitive
        // area publishes that record's title, so it is a security item too.
        // ⚠ Decision 22, failing closed: an area the map no longer declares
        // may have been sensitive, so it counts as sensitive.
        let record_sensitive = match record.area.as_deref() {
            Some(a) => map.route(a).is_none_or(|r| r.sensitive),
            None => false,
        };
        let what = if record_sensitive && !finding.security {
            "this finding, about a record in a sensitive area,"
        } else {
            "this finding"
        };
        let at = self.place(&map, &area, tier, finding.security || record_sensitive, what)?;
        Ok(FindingPlacement {
            at,
            inherited,
            record: RecordSeen {
                id: record.id,
                title: record.title,
                tier: record_tier,
            },
            about_sensitive: record_sensitive,
        })
    }

    pub fn add_record_at(
        &self,
        project: &ProjectId,
        title: &str,
        at: &Placement,
    ) -> Result<RecordId, StoreError> {
        if at.tier == Tier::Github {
            self.check_project(project)?;
        }
        self.tracker_in(at.tier)?
            .add_record_with_area(project, title, Some(&at.area))
    }

    /// Write the finding where `at` says: with its area, as a security
    /// finding when its area or its record's is sensitive (decisions 13,
    /// 21, 22), naming its record's primary IRI —
    /// through a `ForeignRecord` when the record is in the other tier.
    pub fn add_finding_at(
        &self,
        mut finding: Finding,
        at: &FindingPlacement,
    ) -> Result<FindingId, StoreError> {
        finding.area = Some(at.at.area.clone());
        finding.security |= at.at.sensitive || at.about_sensitive;
        finding.record = at.record.id.clone();
        if at.at.tier == Tier::Github {
            self.check_project(&finding.project)?;
        }
        let tracker = self.tracker_in(at.at.tier)?;
        if at.crosses() {
            let proof = ForeignRecord::checked(
                at.record.id.clone(),
                at.record.title.clone(),
                at.record.tier,
            );
            tracker.add_finding_checked(finding, proof)
        } else {
            tracker.add_finding(finding)
        }
    }
}
```

In `crates/core/src/lib.rs`, add `pub mod tiered;` after `pub mod store;` and `pub use tiered::{FindingPlacement, Placement, RecordSeen, TieredTracker};`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-core --lib tiered::tests::`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Each run with `cargo test -p fl-core --lib tiered::tests::`:

1. `map_of`'s refusal: return `RoutingMap::starting()` for `None` → `a_create_with_no_area_an_undeclared_one_or_no_map_is_refused` red.
2. `place_record`'s area: replace `area.ok_or_else(…)?` with `area.unwrap_or("code")` → the same test red.
3. `place`'s undeclared refusal: route an unknown area to `local` → the same test red.
4. A tier given wins: make `Some(t) => (route.tier, true)` → `a_tier_given_overrides_the_map_and_the_area_is_recorded_either_way` red.
5. The GitHub tier is opened at placement: delete `self.github.tracker()?;` → `a_github_tier_create_with_no_binding_is_refused_and_lands_nowhere` red.
6. The private rule runs only for GitHub: hoist the `if security || route.sensitive` block above `if tier == Tier::Github` → `a_sensitive_area_routed_to_a_public_repository_is_refused_before_anything_is_written` red (its last assertion).
7. Its `security` conjunct: drop `security ||` → the same test red (the design finding).
8. Its `sensitive` conjunct: drop `|| route.sensitive` → the same test red (the record).
9. `private_or_refuse`'s `by_map` guard: `if true` → the same test red (`--tier github` names the tracker's refusal); `if false` → the same test red.
10. Inheritance: make `(None, Some(a))` return `NothingToInherit` → `a_finding_inherits_its_records_area_and_says_so` red.
11. The given area wins: swap the first two arms' order so a record's area wins → the same test red.
12. `NothingToInherit`: make `(None, None)` use `"code"` → `a_finding_whose_record_has_no_area_and_names_none_is_refused` red.
13. `add_finding_at` writes the area: delete `finding.area = …` → `a_finding_inherits_its_records_area_and_says_so` red.
14. A sensitive area marks the finding: delete `finding.security |= …` → `a_finding_in_a_sensitive_area_is_a_security_finding` red.
15. The record's primary IRI: delete `finding.record = at.record.id.clone();` → `a_finding_crosses_tiers_only_through_a_checked_foreign_record` red (the alias is stored).
16. `crosses`: make `add_finding_at` always take the `add_finding` branch → the same test red (GitHub refuses the local record); always the checked branch → the same test red (the same-tier case refused).
17. `check_project` for a GitHub record: delete it in `add_record_at` → `a_github_create_checks_its_project_in_the_catalog` red. The same line in `add_finding_at` is not observable: `place_finding` has already read the finding's record, and a record exists only under a project its store accepted. Not a guard a test can tell apart; it stays as the GitHub tier's only project check.
18. `route`'s claim by form: delete the `if self.github.claims(id)` block → not observable in this task's tests (a claimed id falls through to the local tier's `NotOwned`, then reaches GitHub anyway); Task 8's `a_lookup_asks_the_tier_whose_form_the_id_has` pins it: an issue of the bound repository that the local tier also holds as an alias is GitHub's answer, not the alias.
19. `route`'s unbound issue form: delete the `if self.github.issue_form(id)` block → Task 8's `a_lookup_asks_the_tier_whose_form_the_id_has` red (`Elsewhere`, not `TierUnavailable`) — again not observable in this task.
20. Decision 21, the record's sensitivity: replace `finding.security || record_sensitive` with `finding.security` → `a_finding_about_a_record_in_a_sensitive_area_never_reaches_a_public_repository` red. Make `record_sensitive` `true` → the same test red (the ordinary record's finding is refused).
21. Its wording: always pass `"this finding"` → the same test red.
22. Decision 22, failing closed: make the `Some(a)` arm `map.route(a).is_some_and(|r| r.sensitive)` → `a_record_whose_area_the_map_no_longer_declares_counts_as_sensitive` red. The `None` arm (a record made before the project was routed, which never had an area to be sensitive) is not exercised by a public-repository test here; not a guard this task's tests tell apart.
23. The security mark: write `finding.security |= at.at.sensitive;` (dropping `|| at.about_sensitive`) → `a_finding_about_a_record_in_a_sensitive_area_never_reaches_a_public_repository` red and `a_record_whose_area_the_map_no_longer_declares_counts_as_sensitive` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/tiered.rs crates/core/src/routing.rs crates/core/src/lib.rs
git commit -m "feat(core): the routing tracker places and creates

TieredTracker decides where a new item goes — the tier given, else its
area through the map; a finding's area, else its record's — and checks
everything first: a declared area, a map, the GitHub tier available, and
no security item sent by the map to a public repository. A finding about
a record in the other tier is written through a ForeignRecord only the
router builds. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 8: The routing tracker is a `Tracker`: lookups, writes, merged lists

The rest of §2: lookups by IRI in the tier that owns the id, falling back to GitHub's alias scan for an id the local store does not hold, and "held on another machine's local tier, or not existing" when neither tier holds it (§2.2); writes in the tier that holds the item; merged lists with each item's tier, which refuse rather than show part of the population (§2.4); withdrawal counts over both tiers; and the items in an area, for `fl routing remove` (§1.2). The tracker conformance suite runs over the router twice — with `code` routed to each tier. An `fl-exec` test drives a real reproduction through the router and checks the run names the record where it lives (§2.5, plan ruling 22).

**Blast radius:** additions to `tiered.rs`; a new `fl-exec` integration test.

**Files:**
- Modify: `crates/core/src/tiered.rs` (`records`, `findings`, `withdrawals_in`, `items_naming_area`, `impl Tracker`; tests)
- Create: `crates/exec/tests/tiered_evidence.rs`

**Interfaces:**
- Consumes: Task 7's `TieredTracker`, `route`, `record_of`, `tracker_in`, `place_*`, `add_*_at`, and its test helpers `world`, `W::router`, `W::record`, `EveryProject`, `fault`.
- Produces: `TieredTracker::records(&self, project: &ProjectId, only: Option<Tier>) -> Result<Vec<(Tier, Record)>, StoreError>`; `findings(&self, project: &ProjectId, only: Option<Tier>) -> Result<Vec<(Tier, Finding)>, StoreError>`; `withdrawals_in(&self, actor: &str, only: Option<Tier>) -> Result<u64, StoreError>`; `items_naming_area(&self, project: &ProjectId, area: &str) -> Result<Vec<(Tier, Kind, Iri)>, StoreError>`; `impl Tracker for TieredTracker<'_>`.

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/tiered.rs`, inside `mod tests`, add `use crate::finding::FindingState;` and `use crate::model::State;` to its imports, and after the last test add:

```rust
    // Routing spec §2.2: by IRI, the router asks the tier that owns it.
    #[test]
    fn a_lookup_asks_the_tier_whose_form_the_id_has() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "l");
        let gh = w.record("design", "g");
        let asked = w.issues.asked();
        assert_eq!(t.get_record(&local).unwrap().unwrap().title, "l");
        assert_eq!(w.issues.asked(), asked, "a local id never opens GitHub");
        assert_eq!(t.get_record(&gh).unwrap().unwrap().title, "g");
        // An issue of the bound repository is GitHub's, even when the local
        // tier holds the same IRI as an alias.
        let taken = MemIssues::issue(9);
        w.local.add_alias(local.iri(), taken.clone()).unwrap();
        assert_eq!(t.get_record(&RecordId(taken)).unwrap(), None, "GitHub's answer");
        w.issues.set_unbound(true);
        let err = t.get_record(&gh).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "an issue URL is the missing tier's, never held elsewhere: {err:?}"
        );
    }

    // Routing spec §1.3, §2.2: with no binding, an alias that is another
    // repository's issue URL is still found in the local tier.
    #[test]
    fn with_no_binding_another_repositorys_issue_url_held_locally_is_found_there() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "l");
        let old = Iri::parse("https://github.com/o/r/issues/41").unwrap();
        w.local.add_alias(local.iri(), old.clone()).unwrap();
        w.issues.set_unbound(true);
        assert_eq!(t.get_record(&RecordId(old)).unwrap().unwrap().id, local);
        let err = t
            .get_record(&RecordId(Iri::parse("https://github.com/o/r/issues/42").unwrap()))
            .unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "{err:?}"
        );
    }

    // Routing spec §2.2: never `NotOwned`, as if the id were malformed.
    #[test]
    fn an_id_neither_tier_holds_is_held_elsewhere_never_not_owned() {
        let w = world();
        let t = w.router();
        let stranger = RecordId(crate::ids::seq_iri(99));
        let err = t.get_record(&stranger).unwrap_err();
        assert!(matches!(fault(&err), Some(RoutingFault::Elsewhere { .. })), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("another machine's local tier")
                && msg.contains("memory")
                && msg.contains("github:acme/widgets"),
            "{msg}"
        );
        w.issues.set_unbound(true);
        let asked = w.issues.asked();
        let err = t.set_record_state(&stranger, State::Doing).unwrap_err();
        assert!(matches!(fault(&err), Some(RoutingFault::Elsewhere { .. })), "{err:?}");
        assert_eq!(w.issues.asked(), asked, "an unbound machine asks no GitHub");
    }

    // Routing spec §2.2: a urn the local store does not hold may be an item
    // another machine moved to GitHub; GitHub's alias scan finds it.
    #[test]
    fn a_local_id_this_store_does_not_hold_is_found_by_githubs_alias_scan() {
        let w = world();
        let t = w.router();
        let gh = w.record("design", "moved here");
        let old = crate::ids::seq_iri(77);
        w.issues.add_alias(gh.iri(), old.clone()).unwrap();
        assert_eq!(t.get_record(&RecordId(old)).unwrap().unwrap().id, gh);
    }

    #[test]
    fn a_fallback_to_an_unreachable_github_is_an_error_not_not_found() {
        let w = world();
        let t = w.router();
        w.issues.set_down(true);
        let err = t.get_record(&RecordId(crate::ids::seq_iri(99))).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    #[test]
    fn writes_go_to_the_tier_that_holds_the_item() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "l");
        let gh = w.record("design", "g");
        t.set_record_state(&local, State::Doing).unwrap();
        t.set_record_state(&gh, State::Review).unwrap();
        assert_eq!(w.local.get_record(&local).unwrap().unwrap().state, State::Doing);
        assert_eq!(w.issues.get_record(&gh).unwrap().unwrap().state, State::Review);
        let f = t.add_finding(Finding::raise(w.p.clone(), gh, "rev", "c")).unwrap();
        let mut back = t.get_finding(&f).unwrap().unwrap();
        back.withdraw("no").unwrap();
        t.update_finding(&back).unwrap();
        assert_eq!(
            w.issues.get_finding(&f).unwrap().unwrap().state,
            FindingState::Withdrawn
        );
    }

    // Routing spec §2.5: the router never trusts a proof it did not build.
    #[test]
    fn the_router_trusts_no_proof_it_did_not_build() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "l");
        let mut f = Finding::raise(w.p.clone(), local.clone(), "rev", "c");
        f.area = Some("design".into());
        let lie = ForeignRecord::for_tests(local, "l", Tier::Github);
        let id = t.add_finding_checked(f, lie).unwrap();
        assert_eq!(t.tier_of(id.iri()), Tier::Github, "placed by its area, as add_finding");
    }

    // Routing spec §2.4: both tiers merged, each item with its tier.
    #[test]
    fn a_merged_list_holds_both_tiers_and_names_each_items_tier() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "l");
        let gh = w.record("design", "g");
        let both: Vec<(Tier, RecordId)> = t
            .records(&w.p, None)
            .unwrap()
            .into_iter()
            .map(|(tier, r)| (tier, r.id))
            .collect();
        assert_eq!(both, vec![(Tier::Local, local.clone()), (Tier::Github, gh.clone())]);
        assert_eq!(t.list_records(&w.p).unwrap().len(), 2);
        assert_eq!(t.records(&w.p, Some(Tier::Local)).unwrap().len(), 1);
        assert_eq!(t.records(&w.p, Some(Tier::Github)).unwrap()[0].1.id, gh);
        t.add_finding(Finding::raise(w.p.clone(), local, "rev", "c"))
            .unwrap();
        assert_eq!(t.findings(&w.p, None).unwrap()[0].0, Tier::Local);
        assert_eq!(t.list_findings(&w.p).unwrap().len(), 1);
    }

    // Routing spec §2.4: "a list that cannot see its whole population fails".
    #[test]
    fn a_merged_list_refuses_when_the_github_tier_cannot_be_read() {
        let w = world();
        let t = w.router();
        w.record("code", "l");
        for (down, unbound) in [(true, false), (false, true)] {
            w.issues.set_down(down);
            w.issues.set_unbound(unbound);
            for err in [
                t.records(&w.p, None).unwrap_err(),
                t.findings(&w.p, None).unwrap_err(),
                t.withdrawals_in("rev", None).unwrap_err(),
                t.list_records(&w.p).unwrap_err(),
            ] {
                assert!(
                    matches!(fault(&err), Some(RoutingFault::TierUnreadable { .. })),
                    "{err:?}"
                );
                assert!(err.to_string().contains("--tier local"), "{err}");
            }
            assert_eq!(
                t.records(&w.p, Some(Tier::Local)).unwrap().len(),
                1,
                "the local tier alone still reads"
            );
        }
        w.issues.set_unbound(false);
        w.issues.set_down(true);
        let err = t.records(&w.p, Some(Tier::Github)).unwrap_err();
        assert!(
            matches!(err, StoreError::Unreachable { .. }),
            "one tier asked for is its own error: {err:?}"
        );
    }

    #[test]
    fn withdrawals_sum_both_tiers_or_count_the_one_asked() {
        let w = world();
        let t = w.router();
        for area in ["code", "design"] {
            let r = w.record(area, "t");
            let f = t
                .add_finding(Finding::raise(w.p.clone(), r, "hasty", "c"))
                .unwrap();
            let mut back = t.get_finding(&f).unwrap().unwrap();
            back.withdraw("no").unwrap();
            t.update_finding(&back).unwrap();
        }
        assert_eq!(t.withdrawals_by("hasty").unwrap(), 2);
        assert_eq!(t.withdrawals_in("hasty", Some(Tier::Local)).unwrap(), 1);
        assert_eq!(t.withdrawals_in("hasty", Some(Tier::Github)).unwrap(), 1);
    }

    // Routing spec §1.2: removing an area needs every item that names it,
    // in both tiers; a tier that cannot be read refuses.
    #[test]
    fn items_naming_an_area_are_found_in_both_tiers_and_an_unreadable_tier_refuses() {
        let w = world();
        let t = w.router();
        let l = w.record("code", "l");
        let at = t.place_record(&w.p, Some("code"), Some(Tier::Github)).unwrap();
        let g = t.add_record_at(&w.p, "g", &at).unwrap();
        let mut f = Finding::raise(w.p.clone(), l.clone(), "rev", "c");
        f.area = Some("code".into());
        let lf = t.add_finding(f).unwrap();
        w.record("design", "other");
        let found = t.items_naming_area(&w.p, "code").unwrap();
        assert_eq!(
            found,
            vec![
                (Tier::Local, Kind::Record, l.0),
                (Tier::Local, Kind::Finding, lf.0),
                (Tier::Github, Kind::Record, g.0),
            ]
        );
        assert!(t.items_naming_area(&w.p, "ops").unwrap().is_empty());
        w.issues.set_down(true);
        assert!(t.items_naming_area(&w.p, "code").is_err());
    }

    /// The tracker suites make items without an area, which a routed
    /// project refuses: this gives each one `code`, which every map here
    /// declares, and passes everything else through.
    struct WithCode<'a>(&'a dyn Tracker);

    impl Tracker for WithCode<'_> {
        fn add_record_with_area(
            &self,
            p: &ProjectId,
            t: &str,
            a: Option<&str>,
        ) -> Result<RecordId, StoreError> {
            self.0.add_record_with_area(p, t, a.or(Some("code")))
        }
        fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
            self.0.get_record(id)
        }
        fn list_records(&self, p: &ProjectId) -> Result<Vec<Record>, StoreError> {
            self.0.list_records(p)
        }
        fn set_record_state(&self, id: &RecordId, s: State) -> Result<(), StoreError> {
            self.0.set_record_state(id, s)
        }
        fn add_finding(&self, f: Finding) -> Result<FindingId, StoreError> {
            self.0.add_finding(f)
        }
        fn add_finding_checked(
            &self,
            f: Finding,
            r: ForeignRecord,
        ) -> Result<FindingId, StoreError> {
            self.0.add_finding_checked(f, r)
        }
        fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
            self.0.get_finding(id)
        }
        fn update_finding(&self, f: &Finding) -> Result<(), StoreError> {
            self.0.update_finding(f)
        }
        fn list_findings(&self, p: &ProjectId) -> Result<Vec<Finding>, StoreError> {
            self.0.list_findings(p)
        }
        fn withdrawals_by(&self, a: &str) -> Result<u64, StoreError> {
            self.0.withdrawals_by(a)
        }
        fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
            self.0.add_alias(primary, alias)
        }
    }

    struct Over {
        local: MemStore,
        issues: MemIssues,
        every: EveryProject,
    }

    impl crate::conformance::Fixture for Over {
        fn with(&self, f: &mut dyn FnMut(&crate::conformance::Bound<'_>)) {
            let router = TieredTracker {
                catalog: &self.local,
                local: &self.local,
                routes: &self.every,
                github: &self.issues,
            };
            let tracker = WithCode(&router);
            f(&crate::conformance::Bound {
                catalog: &self.local,
                tracker: &tracker,
                ledger: &self.local,
                handles: &self.local,
            });
        }
    }

    fn over(code: Tier) -> Over {
        Over {
            local: MemStore::default(),
            issues: MemIssues::default(),
            every: EveryProject(RoutingMap::starting().with("code", code, false)),
        }
    }

    // Routing spec §2: "The `Tracker` conformance suite runs over it."
    #[test]
    fn the_router_meets_the_tracker_contract_with_code_in_either_tier() {
        crate::conformance::tracker(|| over(Tier::Local));
        crate::conformance::tracker(|| over(Tier::Github));
    }
```

Create `crates/exec/tests/tiered_evidence.rs`:

```rust
//! A finding's evidence names its record across tiers (routing spec §2.5):
//! a GitHub-tier finding about a local record is reproduced through the
//! routing tracker, and the run it records is tied to the local record.

use fl_core::mem_issues::{ISSUES, MemIssues};
use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
use fl_core::store::{Catalog, Ledger, Roles, Tracker};
use fl_core::{Finding, MemStore, RoutingMap, TieredTracker};
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
fn a_github_finding_about_a_local_record_tags_its_evidence_with_the_local_iri() {
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
    };
    let r = router
        .add_record_with_area(&p, "fix the parser", Some("code"))
        .unwrap();
    let mut f = Finding::raise(p.clone(), r.clone(), "rev", "it breaks");
    f.area = Some("design".into());
    let fid = router.add_finding(f).unwrap();
    assert!(fid.iri().as_str().starts_with(ISSUES), "the finding is on GitHub");
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
    let gate = local.add_gate(&p, "fails", kind, sel, 1, &head, "o").unwrap();
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
        Some(&r),
        "the run names the record where it lives"
    );
    assert_eq!(
        issues.get_finding(&fid).unwrap().unwrap().reproduction,
        Some(gate)
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-core --lib tiered::tests::` and `cargo test -p fl-exec --test tiered_evidence`
Expected: FAIL to compile — `records`, `findings`, `withdrawals_in`, `items_naming_area` do not exist, and `TieredTracker` is not a `Tracker`.

- [ ] **Step 3: Implement**

In `crates/core/src/tiered.rs`, change the imports to `use crate::finding::Finding; use crate::model::{Record, State}; use crate::store::{Catalog, StoreError, Tracker, as_clause};` (keeping the others), and add after `impl<'a> TieredTracker<'a> { … }`:

```rust
/// Both tiers, local first, or the one asked for.
fn tiers(only: Option<Tier>) -> Vec<Tier> {
    match only {
        Some(t) => vec![t],
        None => Tier::ALL.to_vec(),
    }
}

impl TieredTracker<'_> {
    /// `act` over one tier's tracker. ⚠ In a merged read (`only` is `None`)
    /// a GitHub tier that cannot be read refuses the whole read, naming
    /// `--tier local` (§2.4): a list that cannot see its whole population
    /// fails.
    fn read<T>(
        &self,
        tier: Tier,
        only: Option<Tier>,
        act: impl Fn(&dyn Tracker) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        match self.tracker_in(tier).and_then(act) {
            Err(e) if only.is_none() && tier == Tier::Github => {
                Err(RoutingFault::TierUnreadable { tier, cause: as_clause(&e) }.into())
            }
            other => other,
        }
    }

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
        Ok(out)
    }

    /// The project's findings in both tiers, or in `only`, each with its
    /// tier (§2.4).
    pub fn findings(
        &self,
        project: &ProjectId,
        only: Option<Tier>,
    ) -> Result<Vec<(Tier, Finding)>, StoreError> {
        let mut out = Vec::new();
        for tier in tiers(only) {
            let got = self.read(tier, only, |t| t.list_findings(project))?;
            out.extend(got.into_iter().map(|f| (tier, f)));
        }
        Ok(out)
    }

    /// How many findings `actor` raised and withdrew, summed over both
    /// tiers, or in `only` (§2.4).
    pub fn withdrawals_in(&self, actor: &str, only: Option<Tier>) -> Result<u64, StoreError> {
        let mut n = 0;
        for tier in tiers(only) {
            n += self.read(tier, only, |t| t.withdrawals_by(actor))?;
        }
        Ok(n)
    }

    /// Every item of `project` that names `area` (§1.2): this machine's
    /// local tier, and GitHub by the items' blocks. ⚠ A tier that cannot be
    /// read is an error: the removal this serves must see every item.
    pub fn items_naming_area(
        &self,
        project: &ProjectId,
        area: &str,
    ) -> Result<Vec<(Tier, Kind, Iri)>, StoreError> {
        let mut out = Vec::new();
        for r in self.local.list_records(project)? {
            if r.area.as_deref() == Some(area) {
                out.push((Tier::Local, Kind::Record, r.id.0));
            }
        }
        for f in self.local.list_findings(project)? {
            if f.area.as_deref() == Some(area) {
                out.push((Tier::Local, Kind::Finding, f.id.0));
            }
        }
        for (kind, id) in self.github.items_in_area(project, area)? {
            out.push((Tier::Github, kind, id));
        }
        Ok(out)
    }
}

impl Tracker for TieredTracker<'_> {
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError> {
        let at = self.place_record(project, area, None)?;
        self.add_record_at(project, title, &at)
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.route(id.iri(), |t| t.get_record(id)).map(|(_, r)| r)
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        Ok(self
            .records(project, None)?
            .into_iter()
            .map(|(_, r)| r)
            .collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        self.route(id.iri(), |t| t.set_record_state(id, state))
            .map(drop)
    }

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        let at = self.place_finding(&finding, None)?;
        self.add_finding_at(finding, &at)
    }

    /// ⚠ The router checks every reference itself and never trusts a proof
    /// it did not build (§2.5): this places the finding as `add_finding`
    /// does, and drops `_record`.
    fn add_finding_checked(
        &self,
        finding: Finding,
        _record: ForeignRecord,
    ) -> Result<FindingId, StoreError> {
        self.add_finding(finding)
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        self.route(id.iri(), |t| t.get_finding(id)).map(|(_, f)| f)
    }

    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        self.route(finding.id.iri(), |t| t.update_finding(finding))
            .map(drop)
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        Ok(self
            .findings(project, None)?
            .into_iter()
            .map(|(_, f)| f)
            .collect())
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        self.withdrawals_in(actor, None)
    }

    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        self.route(primary, |t| t.add_alias(primary, alias.clone()))
            .map(drop)
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-core --lib tiered::tests::` and `cargo test -p fl-exec --test tiered_evidence`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Each `fl-core` run with `cargo test -p fl-core --lib tiered::tests::`:

1. `route`'s claim by form (Task 7): delete the `if self.github.claims(id)` block → `a_lookup_asks_the_tier_whose_form_the_id_has` red (the local alias answers).
1a. `route`'s unbound issue form (Task 7): delete the `if self.github.issue_form(id)` block → `a_lookup_asks_the_tier_whose_form_the_id_has` red and `with_no_binding_another_repositorys_issue_url_held_locally_is_found_there` red (`Elsewhere`). Make its guard `if true` → `an_id_neither_tier_holds_is_held_elsewhere_never_not_owned` red (an unbound stranger urn is `TierUnavailable`, not `Elsewhere`).
2. `route`'s unbound short cut: delete `if !self.github.available() { … }` → `an_id_neither_tier_holds_is_held_elsewhere_never_not_owned` red (an unbound machine asks GitHub, and answers `TierUnavailable`).
3. `route`'s second `NotOwned`: make that arm return the `NotOwned` itself → the same test red.
4. Its searched list: drop `searched.extend(theirs);` → the same test red (no `github:acme/widgets`).
5. The alias-scan fallback: replace the second `match act(self.github.tracker()?)` with an immediate `Elsewhere` → `a_local_id_this_store_does_not_hold_is_found_by_githubs_alias_scan` red, and `a_fallback_to_an_unreachable_github_is_an_error_not_not_found` red.
6. `get_record` routes: call `self.local.get_record(id)` instead → `a_lookup_asks_the_tier_whose_form_the_id_has` red.
7. `set_record_state` and `update_finding` route: call the local tier directly in each → `writes_go_to_the_tier_that_holds_the_item` red.
8. `add_finding_checked` places by area: forward to `self.local.add_finding_checked(finding, _record)` → `the_router_trusts_no_proof_it_did_not_build` red.
9. `tiers(None)`: return `vec![Tier::Local]` → `a_merged_list_holds_both_tiers_and_names_each_items_tier` red, and `withdrawals_sum_both_tiers_or_count_the_one_asked` red.
10. `read`'s merged refusal: drop `only.is_none() &&` → `a_merged_list_refuses_when_the_github_tier_cannot_be_read` red (its last assertion). Delete the arm → the same test red. Its `tier == Tier::Github` conjunct is not observable: the local tier here never fails to read; not a guard a test can tell apart.
11. `items_naming_area`'s area filters: drop the `if` on records, then on findings → `items_naming_an_area_are_found_in_both_tiers_and_an_unreadable_tier_refuses` red. Drop the GitHub loop → the same test red.
12. The conformance run: each of the above that changes a tracker result also turns `the_router_meets_the_tracker_contract_with_code_in_either_tier` red; it is the net, not a substitute.
13. The run's record (`fl-exec`, unchanged code): in `crates/exec/src/finding.rs`, `attach_reproduction`'s `Some(&f.record)` → `None` → `cargo test -p fl-exec --test tiered_evidence` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/core/src/tiered.rs crates/exec/tests/tiered_evidence.rs
git commit -m "feat(core): the routing tracker is a Tracker

Lookups ask the tier that owns the id, fall back to GitHub's alias scan
for an id the local store does not hold, and say \"another machine's
local tier\" when neither does — never NotOwned, and never \"not found\"
for a tier that could not be read. Writes go where the item is. Merged
lists carry each item's tier and refuse rather than show part of the
population. The tracker suite runs over the router with code in either
tier; a reproduction's run names its record across tiers. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 9: `fl routing set` and `fl routing show`

The commands that author the map (routing spec §1.2): the first `set` writes the starting set first and says how handles change for this project (§2.3, plan ruling 25); a later `set` changes one area for new items; `show` prints the map. Both take `--project` (plan ruling 10). `set` is refused while the store holds another project (spec decision 20; the store's own refusal from Task 3), naming the remedy. The import that first routes a store prints the same handle-change line (§2.3, rev 2.1). The person's doc, `docs/routing.md`, starts here; later tasks add their sections.

**Blast radius:** a new subcommand. `Command`'s five dispatch methods gain an arm. `fl --help` lists `routing` (the getting-started guide's copy of it changes with it). `fl manifest import` prints one more stderr line, only when the import first routes the store.

**Files:**
- Create: `crates/cli/src/cmd/routing.rs`
- Modify: `crates/cli/src/cmd/mod.rs`, `crates/cli/src/main.rs` (`Command::Routing`)
- Modify: `crates/cli/src/cmd/manifest.rs` (`Import`: the handle-change notice)
- Create: `crates/cli/tests/routing.rs`
- Create: `docs/routing.md`
- Modify: `docs/README.md`, `docs/github-tracker.md` (links), `docs/getting-started.md` (the `fl --help` block, which `crates/cli/tests/getting_started.rs` runs)

**Interfaces:**
- Consumes: `fl_core::routing::{area_name, after_set, AreaRoute, Tier}`, `Routes`; `RedbStore::set_routes`, `routes` (Task 3).
- Produces: `cmd::routing::Cmd { Set { project: Ref, area: String, tier: Tier, sensitive: bool }, Show { project: Ref } }` with `iris`, `has_handle`, `needs_tracker(&self) -> bool`; `pub fn run(ctx: &Ctx<'_>, cmd: Cmd, bound: Option<&config::TrackerBinding>) -> Result<i32>`; `pub fn parse_tier(s: &str) -> Result<Tier, String>` (its first caller outside clap is Task 12); `pub fn handle_change(was_github: bool) -> &'static str`. In `crates/cli/tests/routing.rs`: `world(tracker: &str) -> R`, `BOUND`, `R::{configure, fl_at, fl, ok, routed}`. Output: `<area>\t<tier>\t<sensitive|->`. Unique phrases: `wrote the starting set first`, `handles change in this project`, `has no routing map`.

- [ ] **Step 1: Write the failing tests**

Create `crates/cli/tests/routing.rs`:

```rust
//! A routed project (routing spec): two tiers — the local store and the
//! fake GitHub's `acme/widgets` — and the rule that routes each new item.

use assert_cmd::Command;
use fl_github::fake::FakeGithub;
use predicates::prelude::PredicateBooleanExt;
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
    r.configure(r.home.path(), tracker);
    r
}

impl R {
    /// `home`'s config: this repository's project, its store under `home`,
    /// and `tracker`.
    fn configure(&self, home: &Path, tracker: &str) {
        let cfg = format!(
            "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n{tracker}",
            self.repo.path().canonicalize().unwrap().display(),
            home.join("fl.redb").display()
        );
        fs::create_dir_all(home.join("config/fl")).unwrap();
        fs::write(home.join("config/fl/config.toml"), cfg).unwrap();
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

    fn fl(&self) -> Command {
        self.fl_at(self.home.path())
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

    /// The project, routed with the starting set.
    fn routed(&self) {
        self.ok(&["project", "add", "."]);
        self.ok(&["routing", "set", "--project", "1", "code", "local"]);
    }
}

// Routing spec §1.2, §2.3: the first set writes the starting set, and says
// what changes for a project that was local-only.
#[test]
fn the_first_set_writes_the_starting_set_and_says_how_handles_change() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["routing", "set", "--project", "1", "ops", "github"])
        .assert()
        .success()
        .stdout("ops\tgithub\t-\n")
        .stderr(
            contains("wrote the starting set first")
                .and(contains("security to github, sensitive"))
                .and(contains("handles change in this project"))
                .and(contains("`#3` named local item 3")),
        );
    let shown = g.ok(&["routing", "show", "--project", "1"]);
    assert_eq!(
        shown,
        "code\tlocal\t-\ndesign\tgithub\t-\nops\tgithub\t-\nproduct\tgithub\t-\n\
         security\tgithub\tsensitive\ntests\tlocal\t-\n"
    );
}

#[test]
fn a_github_bound_project_hears_that_a_bare_number_now_names_a_local_item() {
    let g = world(BOUND);
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "local"])
        .assert()
        .success()
        .stderr(contains("a bare number such as `41` named GitHub issue 41"));
    assert!(g.fake.state().requests.is_empty(), "authoring the map needs no GitHub");
}

#[test]
fn a_later_set_changes_one_area_and_says_nothing_of_handles() {
    let g = world("");
    g.routed();
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "github", "--sensitive"])
        .assert()
        .success()
        .stdout("code\tgithub\tsensitive\n")
        .stderr(contains("handles change in this project").not());
    assert!(g.ok(&["routing", "show", "--project", "1"]).starts_with("code\tgithub\tsensitive\n"));
}

#[test]
fn an_area_or_a_tier_that_is_not_one_is_refused_and_nothing_is_written() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["routing", "set", "--project", "1", "Code", "local"])
        .assert()
        .failure()
        .stderr(contains("is not an area name"));
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "cloud"])
        .assert()
        .failure()
        .stderr(contains("is not a tier"));
    g.fl()
        .args(["routing", "show", "--project", "1"])
        .assert()
        .success()
        .stdout("")
        .stderr(contains("has no routing map"));
}

// Routing spec decision 22: a set that only changes a tier keeps the area's
// sensitivity.
#[test]
fn a_tier_change_keeps_the_areas_sensitivity() {
    let g = world("");
    g.routed();
    assert_eq!(
        g.ok(&["routing", "set", "--project", "1", "security", "local"]),
        "security\tlocal\tsensitive\n"
    );
}

// Routing spec decision 20: handles are numbered per store, so a routed
// store holds one project; the refusal names the remedy and writes nothing.
#[test]
fn routing_a_project_that_shares_its_store_is_refused_naming_its_own_store() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    let other = tempfile::tempdir().unwrap();
    git(other.path(), &["init", "-q"]);
    git(
        other.path(),
        &[
            "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q",
            "--allow-empty", "-m", "first",
        ],
    );
    let db = g.home.path().join("fl.redb");
    g.fl()
        .args(["--db", db.to_str().unwrap(), "project", "add"])
        .arg(other.path())
        .assert()
        .success();
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "local"])
        .assert()
        .failure()
        .stderr(contains("needs a store of its own").and(contains("its own `store`")));
    g.fl()
        .args(["routing", "show", "--project", "1"])
        .assert()
        .success()
        .stderr(contains("has no routing map"));
}

// Routing spec §2.3: the import that first routes a store says how handles
// change there, as the first `set` does where the project is authored.
#[test]
fn an_import_that_first_routes_a_store_says_how_handles_change() {
    let g = world("");
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    let other = tempfile::tempdir().unwrap();
    g.configure(other.path(), BOUND);
    g.fl_at(other.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stderr(contains("a bare number such as `41` named GitHub issue 41"));
    g.fl_at(other.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stderr(contains("handles change in this project").not());
}
```

In `crates/cli/src/cmd/routing.rs` (created in Step 3), the unit test:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tier_is_parsed_by_its_wire_name_only() {
        assert_eq!(parse_tier("local"), Ok(Tier::Local));
        assert_eq!(parse_tier("github"), Ok(Tier::Github));
        for bad in ["Local", "remote", ""] {
            assert!(parse_tier(bad).unwrap_err().contains("is not a tier"), "{bad}");
        }
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --test routing`
Expected: FAIL — `fl routing` is not a command (clap: "unrecognized subcommand").

- [ ] **Step 3: Implement**

Create `crates/cli/src/cmd/routing.rs` (its tests from Step 1 at the foot):

```rust
//! `fl routing` (routing spec §1.2): the areas a project declares, the tier
//! each one routes a new item to, and whether it is sensitive.

use crate::config::TrackerBinding;
use crate::ctx::Ctx;
use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::ProjectId;
use fl_core::routing::{self, AreaRoute, Routes, RoutingMap, Tier};
use fl_core::store::Catalog;
use fl_core::{Iri, Kind};

/// A tier by its name, for `fl routing set` and `--tier`.
pub fn parse_tier(s: &str) -> Result<Tier, String> {
    Tier::from_wire(s)
        .ok_or_else(|| format!("`{s}` is not a tier. The tiers are: {}", Tier::wire_values()))
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Route an area to a tier for new items. The project's first `set`
    /// writes the starting set first.
    Set {
        #[arg(long)]
        project: Ref,
        area: String,
        #[arg(value_parser = parse_tier)]
        tier: Tier,
        /// Items made in this area are security items, which the map never
        /// sends to a repository that is not private. Without it, an area
        /// keeps its sensitivity (decision 22).
        #[arg(long)]
        sensitive: bool,
    },
    /// Print the project's routing map.
    Show {
        #[arg(long)]
        project: Ref,
    },
}

impl Cmd {
    fn refs(&self) -> Vec<&Ref> {
        match self {
            Cmd::Set { project, .. } | Cmd::Show { project } => vec![project],
        }
    }

    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }

    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }

    /// Whether this command reads records or findings, in either tier.
    pub fn needs_tracker(&self) -> bool {
        match self {
            Cmd::Set { .. } | Cmd::Show { .. } => false,
        }
    }
}

/// `bound`: the project's tracker binding in the config, when it was read:
/// the first `set` says how handles change from the project's mode before
/// (routing spec §2.3).
pub fn run(ctx: &Ctx<'_>, cmd: Cmd, bound: Option<&TrackerBinding>) -> Result<i32> {
    let store = ctx.store;
    match cmd {
        Cmd::Set {
            project,
            area,
            tier,
            sensitive,
        } => {
            let p = project_of(ctx, &project)?;
            routing::area_name(&area).map_err(|why| anyhow::anyhow!("{why}"))?;
            // Routing spec decision 22: without `--sensitive`, an area keeps
            // its sensitivity; a tier change never clears it.
            let asked = if sensitive { Some(true) } else { None };
            let (map, first) = routing::after_set(store.routes(&p)?.as_ref(), &area, tier, asked);
            store.set_routes(&p, &map)?;
            if first {
                eprintln!(
                    "notice: project {} had no routing map, so fl wrote the starting set first: {}",
                    refs::show(store, Kind::Project, p.iri())?,
                    starting_set()
                );
                eprintln!("{}", handle_change(bound.is_some()));
            }
            print_route(map.route(&area).expect("the area was just set"));
        }
        Cmd::Show { project } => {
            let p = project_of(ctx, &project)?;
            match store.routes(&p)? {
                Some(map) => map.areas.iter().for_each(print_route),
                None => eprintln!(
                    "note: project {} has no routing map; `fl routing set` declares its areas",
                    refs::show(store, Kind::Project, p.iri())?
                ),
            }
        }
    }
    Ok(0)
}

/// The project `r` names in this store.
fn project_of(ctx: &Ctx<'_>, r: &Ref) -> Result<ProjectId> {
    let store = ctx.store;
    let p = ProjectId(refs::resolve(store, store.label(), Kind::Project, r)?);
    if store.get_project(&p)?.is_none() {
        bail!(
            "`{r}` is not a project in the store at {}. Run `fl project list` to see the ones \
             that exist.",
            store.label()
        );
    }
    Ok(p)
}

fn print_route(a: &AreaRoute) {
    let sensitive = if a.sensitive { "sensitive" } else { "-" };
    println!("{}\t{}\t{sensitive}", a.area, a.tier.as_wire());
}

/// The starting set, as a notice says it: read from
/// `RoutingMap::starting()`, so the two cannot drift.
fn starting_set() -> String {
    RoutingMap::starting()
        .areas
        .iter()
        .map(|a| {
            let sensitive = if a.sensitive { ", sensitive" } else { "" };
            format!("{} to {}{sensitive}", a.area, a.tier.as_wire())
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// What changes in how a handle reads once a store is first routed — by
/// the first `set`, or by the import that brings a routing map (routing
/// spec §2.3).
pub fn handle_change(was_github: bool) -> &'static str {
    if was_github {
        "notice: handles change in this project: a bare number such as `41` named GitHub issue \
         41 and now names local item 41; write `#41` for the issue. Local records made before \
         the binding show in lists again."
    } else {
        "notice: handles change in this project: `#3` named local item 3 and now names GitHub \
         issue 3; write `3` for the local item."
    }
}
```

In `crates/cli/src/cmd/mod.rs`, add `pub mod routing;`. In `crates/cli/src/main.rs`, add to `Command`, after `Github` (so `fl --help` lists it last):

```rust
    /// Route a project's new items between its local store and GitHub.
    #[command(subcommand)]
    Routing(cmd::routing::Cmd),
```

and the arms `Command::Routing(c) => c.iris(),` (in `iris`), `Command::Routing(c) => c.has_handle(),` (in `has_handle`), `Command::Routing(c) => c.needs_tracker(),` (in `needs_tracker`, before the `false` arm), and, in `run`'s dispatch, `Command::Routing(c) => cmd::routing::run(&ctx, c, here_binding.as_ref()),`. `project_root` keeps its `_ => None`.

In `crates/cli/src/cmd/manifest.rs`, in `run`'s `Import` arm, replace `let report = store.import_manifest(…)?; print_import(store, &m, &report)?;` with:

```rust
            let was_routed = store.holds_routing()?;
            let report = store.import_manifest(&m, &root.display().to_string())?;
            print_import(store, &m, &report)?;
            // Routing spec §2.3: the import that first routes this store
            // changes what a handle means here, as the first `fl routing set`
            // does where the project is authored.
            if !was_routed && report.areas.is_some() {
                let was_github = matches!(binding, Binding::Github(_));
                eprintln!("{}", crate::cmd::routing::handle_change(was_github));
            }
```

In `docs/getting-started.md`, in the `fl --help` block, add after the `github` line:

```text
  routing     Route a project's new items between its local store and GitHub
```

Create `docs/routing.md`:

```markdown
# Routing items between the local store and GitHub

A project can keep its records and findings in two places at once: the **local tier** — the
project's local store, for developer-level items such as code, code quality and unit tests —
and the **github tier** — the GitHub repository its config binds, for human-level items such as
design, product and security review. Each new record and finding goes to one of the two by its
**area**, a short name such as `code` or `design`, through the project's **routing map**.

## Declaring areas

`fl routing set --project <project> <area> <tier>` routes an area to a tier, `local` or
`github`. An area name is 1 to 32 lowercase letters, digits and `-`. The project's first `set`
writes the starting set first, then the area it names:

| area | tier | sensitive |
|---|---|---|
| `code` | `local` | |
| `design` | `github` | |
| `product` | `github` | |
| `security` | `github` | yes |
| `tests` | `local` | |

`--sensitive` marks an area whose items are security items: a finding made there is a security
finding, and the map never sends such an item to a repository that is not private. Setting an
area that exists changes its tier for new items only; items already made stay where they are. A
`set` without `--sensitive` keeps the area's sensitivity: changing an area's tier never clears it. `fl routing show --project <project>` prints the map, one area per line: its
name, its tier, and `sensitive` or `-`.

The map is authored in the store that authors the project, like its gates, and reaches other
machines in the committed manifest ([sharing-gates.md](sharing-gates.md)): after changing it, run
`fl manifest export` and commit. A store that imported the project refuses `fl routing set`.

A routed project needs a store of its own, because handles are numbered per store: `fl routing
set` is refused while the project's store holds another project, and a routed store takes no
other project afterwards. Give the project its own store with a config entry of its own
`store`.

## Handles change

Once a project has a routing map, `#41` and `owner/repo#41` always name GitHub issue 41, and a
bare `41` names local item 41. The first `fl routing set` says what that changes for the project:

* a project that was local-only: `#3` named local item 3, and now names GitHub issue 3;
* a project that was bound to GitHub: a bare `41` named issue 41, and now names local item 41,
  and local records made before the binding appear in lists again.

The import that first brings a routing map to another machine says the same there.
```

In `docs/README.md`, add after the `github-ledger.md` item:

```markdown
* [routing.md](routing.md) — keep developer-level items in the local store and human-level
  items in GitHub: areas, the routing map, handles, lists and their limits.
```

In `docs/github-tracker.md`, in the paragraph Task 2 added ("An item of a project that routes its items…"), link the phrase: "An item of a project that routes its items between its local store and GitHub ([routing.md](routing.md))".

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli --test routing` and `cargo test -p fl-cli --bin fl -- cmd::routing::tests::`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

1. `area_name` is asked before writing: move it below `set_routes` → not observable: the store's `set_routes` checks every area name too (`RoutingMap::check`, Task 3) and refuses before it writes. The CLI's check gives the refusal before any other read; not a guard a test tells apart.
2. The first set's notices: replace `if first` with `if true` → `cargo test -p fl-cli --test routing -- a_later_set_changes_one_area_and_says_nothing_of_handles` red; with `if false` → `the_first_set_writes_the_starting_set_and_says_how_handles_change` red.
3. `handle_change`'s branch: pass `!bound.is_some()` → `a_github_bound_project_hears_that_a_bare_number_now_names_a_local_item` red.
4. `print_route`'s sensitivity: always print `-` → `a_later_set_changes_one_area_and_says_nothing_of_handles` red.
5. `parse_tier`: accept anything as `Tier::Local` → `cargo test -p fl-cli --bin fl -- cmd::routing::tests::a_tier_is_parsed_by_its_wire_name_only` red.
6. `needs_tracker` is false for both: make it `true` → `a_github_bound_project_hears_that_a_bare_number_now_names_a_local_item` red (GitHub is opened).
6a. `set` without `--sensitive` keeps the area's: pass `Some(sensitive)` instead of `asked` → `a_tier_change_keeps_the_areas_sensitivity` red.
7. The starting set is rendered from `RoutingMap::starting()`: drop `, sensitive` from `starting_set`'s format → `the_first_set_writes_the_starting_set_and_says_how_handles_change` red.
8. The import's notice, its guard: make it `if report.areas.is_some()` (dropping `!was_routed &&`) → `an_import_that_first_routes_a_store_says_how_handles_change` red (the second import says it again); delete the block → the same test red.
9. Decision 20 reaches the CLI: the refusal is the store's (`set_routes`, Task 3, mutation 16 there); `routing_a_project_that_shares_its_store_is_refused_naming_its_own_store` is red under that mutation too.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/cmd/routing.rs crates/cli/src/cmd/mod.rs crates/cli/src/main.rs crates/cli/src/cmd/manifest.rs crates/cli/tests/routing.rs docs/routing.md docs/README.md docs/github-tracker.md docs/getting-started.md
git commit -m "feat(cli): fl routing set and show

The first set writes the starting set first and says how handles change
for the project; a later set changes one area for new items; show prints
the map; set is refused while the store holds another project. The import
that first routes a store says how handles change too. docs/routing.md
starts here. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 10: A routed store gets the routing tracker, with GitHub opened lazily

The CLI builds the router for a routed store (routing spec §1.3, plan ruling 9) and passes it where it passes one tracker today. GitHub is opened on the first call that needs a GitHub item (§2.6, decision 19) — so a local item is made and moved with no request to GitHub at all — except for `fl github …`, which names GitHub items only (plan ruling 26). A routed store whose binding names the GitHub ledger is refused when the command starts (decision 12). `fl record add --area` reaches the router, after the routing-currency check (§1.2, plan ruling 11); `--area` in an unrouted store is refused (§1.1). A record or finding prints as `#41` when it is on GitHub and `41` when it is local (§2.3). `finding reproduce` checks the committed manifest for a GitHub-tier finding, as it does today for any finding of a GitHub-bound project.

**Blast radius:** `cmd::manifest::Binding::Github` becomes a struct variant (its readers: `ledger_node`, Task 9's import notice; its one constructor in `run`). `run()` in `main.rs` — every command. In an unrouted store nothing changes: `holds_routing` is one read of a table that does not exist, and the old arms run as before. `Ctx` gains a field (its four constructions: `main.rs`, and tests in `ctx.rs`, `comment.rs` twice, `cmd/ledger.rs`). Every `refs::show` of a record or finding becomes `Ctx::show_item`, which is `refs::show` in an unrouted store. `finding reproduce`'s publish check reads `Ctx::on_github`, which is `ctx.github.is_some()` in an unrouted store.

**Files:**
- Create: `crates/cli/src/tiers.rs`
- Modify: `crates/cli/src/main.rs` (`mod tiers`, `refuse_routed_github_ledger`, `Command::sets_routing`, `run`; tests)
- Modify: `crates/cli/src/ctx.rs` (`Ctx.tiers`, `show_item`, `on_github`; tests)
- Modify: `crates/cli/src/comment.rs`, `crates/cli/src/cmd/ledger.rs` (tests: `tiers: None`)
- Modify: `crates/cli/src/cmd/routing.rs` (`not_routed`, `github_ledger_refusal`, `Cmd::sets_routing`)
- Modify: `crates/cli/src/cmd/manifest.rs` (`ensure_routing_current`; `Binding::Github` gains the ledger choice; `Import` refuses a routed manifest under the GitHub ledger)
- Modify: `crates/cli/src/cmd/record.rs` (`--area`; `show_item`)
- Modify: `crates/cli/src/cmd/finding.rs` (`show_item`; `on_github`)
- Modify: `crates/cli/tests/routing.rs`
- Modify: `docs/routing.md`

**Interfaces:**
- Consumes: `TieredTracker`, `GithubTier`, `RoutingFault` (Tasks 6–8); `GithubTracker::{require_private, items_in_area}` (Task 6); `RedbStore::holds_routing`, `Routes` (Task 3).
- Produces: `tiers::LazyGithub<'a>` with `new(binding: Option<TrackerBinding>, config: String, open: Opener<'a>) -> Self`, `open(&self) -> Result<&GithubTracker, StoreError>`, and `impl GithubTier` (`claims` takes the configured name, or the opened repository's name now; `issue_form`); `pub type Opener<'a> = Box<dyn Fn(&TrackerBinding) -> anyhow::Result<GithubTracker> + 'a>`; `tiers::Tiers<'a> { pub router: &'a TieredTracker<'a>, pub github: &'a LazyGithub<'a> }`; `Ctx.tiers: Option<&'a Tiers<'a>>`; `Ctx::show_item(&self, kind: Kind, id: &Iri) -> anyhow::Result<String>`; `Ctx::on_github(&self, id: &Iri) -> bool`; `cmd::manifest::ensure_routing_current(store: &RedbStore, project: &ProjectId) -> Result<()>`; `cmd::routing::not_routed(flag: &str) -> anyhow::Error`; `cmd::routing::github_ledger_refusal(repo: &str) -> anyhow::Error`; `cmd::routing::Cmd::sets_routing(&self) -> bool`; `cmd::manifest::Binding::Github { repo: String, github_ledger: bool }` (was `Github(String)`). Unique phrases: `binds no GitHub repository for the project`, `a routed project keeps its runs and decisions in the local ledger`, `needs a routing map, and this project declares none`, `may route items differently`, `changed since the manifest at`.

- [ ] **Step 1: Write the failing tests**

Create `crates/cli/src/tiers.rs` with this `mod tests` at its foot (Step 3 gives the code above it):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Credential;
    use fl_github::fake::FakeGithub;
    use std::cell::Cell;

    fn binding() -> TrackerBinding {
        TrackerBinding {
            github: "acme/widgets".into(),
            credential: Credential::Env,
            ledger: None,
        }
    }

    fn issue(n: u64) -> Iri {
        Iri::parse(&format!("https://github.com/acme/widgets/issues/{n}")).unwrap()
    }

    // Routing spec §2.6: nothing is opened until a GitHub item is needed,
    // and the form of an id is read without opening anything.
    #[test]
    fn nothing_is_opened_until_a_github_item_is_needed() {
        let opened = Cell::new(0);
        let lazy = LazyGithub::new(
            Some(binding()),
            "the config".into(),
            Box::new(|_: &TrackerBinding| {
                opened.set(opened.get() + 1);
                anyhow::bail!("the credential is missing")
            }),
        );
        assert!(lazy.available());
        assert!(lazy.claims(&issue(4)));
        assert!(!lazy.claims(&Iri::parse("https://github.com/acme/other/issues/4").unwrap()));
        assert!(!lazy.claims(&fl_core::ids::seq_iri(4)));
        assert!(lazy.issue_form(&Iri::parse("https://github.com/acme/other/issues/4").unwrap()));
        assert!(!lazy.issue_form(&fl_core::ids::seq_iri(4)));
        assert_eq!(opened.get(), 0);
        let err = lazy.tracker().err().unwrap();
        assert!(matches!(err, StoreError::Backend(ref m) if m.contains("the credential is missing")), "{err:?}");
        assert_eq!(opened.get(), 1);
    }

    // Routing spec §1.3: with no binding the tier is not available, and the
    // refusal names the config entry. It claims no id — the router looks in
    // the local tier first, where an issue URL may be an alias — but knows
    // an issue URL's form, to refuse one the local tier does not hold.
    #[test]
    fn with_no_binding_the_tier_is_unavailable_naming_the_config_entry() {
        let lazy = LazyGithub::new(
            None,
            "/c/fl/config.toml".into(),
            Box::new(|_: &TrackerBinding| unreachable!("no binding, nothing to open")),
        );
        assert!(!lazy.available());
        assert!(!lazy.claims(&issue(4)) && lazy.issue_form(&issue(4)));
        let msg = lazy.open().err().unwrap().to_string();
        assert!(
            msg.contains("binds no GitHub repository for the project")
                && msg.contains("tracker = { github")
                && msg.contains("/c/fl/config.toml"),
            "{msg}"
        );
    }

    #[test]
    fn an_open_that_fails_with_a_store_error_keeps_it_and_one_that_succeeds_is_kept() {
        let lazy = LazyGithub::new(
            Some(binding()),
            "c".into(),
            Box::new(|_: &TrackerBinding| {
                Err(StoreError::RateLimited { reset: "r".into() }.into())
            }),
        );
        assert!(matches!(lazy.open(), Err(StoreError::RateLimited { .. })));
        let fake = FakeGithub::start("acme/widgets");
        let opened = Cell::new(0);
        let lazy = LazyGithub::new(
            Some(binding()),
            "c".into(),
            Box::new(|b: &TrackerBinding| {
                opened.set(opened.get() + 1);
                let client = fl_github::Client::new(
                    &fake.url(),
                    Box::new(fl_github::EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
                );
                Ok(GithubTracker::open(client, &b.github, &fl_core::MemStore::default())?.0)
            }),
        );
        lazy.tracker().unwrap();
        lazy.tracker().unwrap();
        assert_eq!(opened.get(), 1, "opened once");
    }
}
```

In `crates/cli/src/main.rs`, inside `mod tests`, add:

```rust
    // Routing spec decision 12: a routed project uses the local ledger.
    #[test]
    fn a_routed_store_whose_binding_names_the_github_ledger_is_refused() {
        let mut b = entry("/a", "/s.redb", Some("acme/widgets")).tracker.unwrap();
        assert!(refuse_routed_github_ledger(true, Some(&b)).is_ok(), "the local ledger is fine");
        b.ledger = Some(config::LedgerChoice::Github);
        let msg = format!("{:#}", refuse_routed_github_ledger(true, Some(&b)).unwrap_err());
        assert!(
            msg.contains("a routed project keeps its runs and decisions in the local ledger"),
            "{msg}"
        );
        assert!(refuse_routed_github_ledger(false, Some(&b)).is_ok(), "unrouted: as before");
        assert!(refuse_routed_github_ledger(true, None).is_ok());
    }
```

In `crates/cli/tests/routing.rs`, add:

```rust
// Routing spec §2.6: work on local items needs no network and no credential.
#[test]
fn a_local_item_is_made_and_moved_without_a_request_to_github() {
    let g = world(BOUND);
    g.routed();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "fix it", "--area", "code"])
        .assert()
        .success()
        .stdout("1\tfix it\n");
    g.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    assert!(
        g.fake.state().requests.is_empty(),
        "{:?}",
        g.fake.state().requests
    );
}

// Routing spec §1.1, §2.3: a GitHub-tier record is an issue carrying its
// area's label, printed as `#1`.
#[test]
fn a_github_tier_record_is_an_issue_with_its_area_label_printed_as_hash() {
    let g = world(BOUND);
    g.routed();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "look", "--area", "design"])
        .assert()
        .success()
        .stdout("#1\tlook\n");
    assert_eq!(
        g.fake.issue(1).labels,
        vec!["fl:record", "fl:record/todo", "fl:area/design"]
    );
}

#[test]
fn a_routed_create_with_no_area_or_an_undeclared_one_is_refused_naming_the_areas() {
    let g = world("");
    g.routed();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .failure()
        .stderr(contains("needs an area for every new item").and(contains("code, design")));
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t", "--area", "ops"])
        .assert()
        .failure()
        .stderr(contains("`ops` is not an area this project declares"));
}

#[test]
fn area_in_a_project_with_no_routing_map_is_refused_naming_routing_set() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t", "--area", "code"])
        .assert()
        .failure()
        .stderr(contains("needs a routing map, and this project declares none"));
}

// Routing spec §1.3: "routing never changes tier silently".
#[test]
fn a_github_tier_create_on_an_unbound_machine_names_the_config_entry() {
    let g = world("");
    g.routed();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t", "--area", "design"])
        .assert()
        .failure()
        .stderr(
            contains("binds no GitHub repository for the project")
                .and(contains("never puts an item in the other tier")),
        );
    // Nothing landed locally: the next local record is the first.
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "u", "--area", "code"])
        .assert()
        .success()
        .stdout("1\tu\n");
}

// Routing spec decision 12.
#[test]
fn a_routed_binding_with_the_github_ledger_is_refused_when_the_command_starts() {
    let with_ledger =
        "tracker = { github = \"acme/widgets\", credential = \"env\", ledger = \"github\" }\n";
    let g = world(with_ledger);
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["routing", "set", "--project", "1", "code", "local"])
        .assert()
        .failure()
        .stderr(contains("a routed project keeps its runs and decisions in the local ledger"));
    g.fl()
        .args(["routing", "show", "--project", "1"])
        .assert()
        .success()
        .stderr(contains("has no routing map"));
    g.configure(g.home.path(), BOUND);
    g.ok(&["routing", "set", "--project", "1", "code", "local"]);
    g.configure(g.home.path(), with_ledger);
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t", "--area", "code"])
        .assert()
        .failure()
        .stderr(contains("a routed project keeps its runs and decisions in the local ledger"));
}

// Routing spec §1.2: one routing rule per project, not per machine — on
// the authoring machine, once a manifest exists, it must carry the map.
#[test]
fn a_routed_create_on_the_authoring_machine_needs_the_map_exported() {
    let g = world("");
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "a", "--area", "code"]);
    g.ok(&["manifest", "export", "--project", "1"]);
    g.ok(&["record", "add", "--project", "1", "--title", "b", "--area", "code"]);
    g.ok(&["routing", "set", "--project", "1", "ops", "local"]);
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "c", "--area", "code"])
        .assert()
        .failure()
        .stderr(contains("changed since the manifest at"));
    g.ok(&["manifest", "export", "--project", "1"]);
    g.ok(&["record", "add", "--project", "1", "--title", "c", "--area", "code"]);
}

// …and on an importing machine, the import must be current.
#[test]
fn a_routed_create_on_an_importing_machine_needs_the_import_current() {
    let g = world("");
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    git(g.repo.path(), &["add", "-A"]);
    git(g.repo.path(), &["commit", "-qm", "manifest"]);
    let other = tempfile::tempdir().unwrap();
    g.configure(other.path(), "");
    let on_other = |args: &[&str]| g.fl_at(other.path()).args(args).assert();
    on_other(&["manifest", "import"]).success().stdout(contains("routing\t5 areas"));
    on_other(&["record", "add", "--project", "1", "--title", "t", "--area", "code"]).success();
    g.ok(&["routing", "set", "--project", "1", "ops", "local"]);
    g.ok(&["manifest", "export", "--project", "1"]);
    on_other(&["record", "add", "--project", "1", "--title", "u", "--area", "code"])
        .failure()
        .stderr(contains("may route items differently"));
    on_other(&["manifest", "import"]).success();
    on_other(&["record", "add", "--project", "1", "--title", "u", "--area", "code"]).success();
}

// GitHub tracker spec §4.3: a gate a GitHub item names must be in the
// committed manifest — in a routed store, for a GitHub-tier finding.
#[test]
fn reproducing_a_github_tier_finding_checks_the_committed_manifest() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "look", "--area", "design"]);
    g.ok(&[
        "finding", "raise", "--record", "https://github.com/acme/widgets/issues/1",
        "--claim", "it is off", "--by", "rev",
    ]);
    g.ok(&[
        "gate", "add", "--project", "1", "--name", "g", "--glob", "src/**/*.rs",
        "--program", "false",
    ]);
    g.fl()
        .args([
            "finding", "reproduce", "https://github.com/acme/widgets/issues/2", "--gate", "1",
        ])
        .assert()
        .failure()
        .stderr(contains("there is no manifest at"));
}

// Routing spec decision 12: a routed manifest is not imported where the
// binding names the GitHub ledger, and nothing is imported.
#[test]
fn a_routed_manifest_is_not_imported_where_the_binding_names_the_github_ledger() {
    let g = world("");
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    let other = tempfile::tempdir().unwrap();
    g.configure(
        other.path(),
        "tracker = { github = \"acme/widgets\", credential = \"env\", ledger = \"github\" }\n",
    );
    g.fl_at(other.path())
        .args(["manifest", "import"])
        .assert()
        .failure()
        .stderr(contains("a routed project keeps its runs and decisions in the local ledger"));
    g.fl_at(other.path())
        .args(["project", "list"])
        .assert()
        .success()
        .stdout("");
}

// Routing spec §2.3, §2.2: after a repository rename a GitHub item still
// prints as `#n` — its URL names the repository's name now.
#[test]
fn after_a_rename_a_github_item_still_prints_as_hash() {
    let g = world(BOUND);
    g.routed();
    g.fake.rename("acme/gadgets");
    assert_eq!(
        g.ok(&["record", "add", "--project", "1", "--title", "look", "--area", "design"]),
        "#1\tlook\n"
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --test routing` and `cargo test -p fl-cli --bin fl -- tiers::tests:: tests::a_routed_store`
Expected: FAIL to compile (`tiers` does not exist), then the black-box tests FAIL — `--area` is not an argument.

- [ ] **Step 3: Implement**

Put this above the tests in `crates/cli/src/tiers.rs`:

```rust
//! A routed store's two tiers as the CLI holds them (routing spec §1.3,
//! §2.6): the GitHub tier is opened on the first call that needs it, so
//! work on local items needs no network and no credential.

use crate::config::TrackerBinding;
use fl_core::TieredTracker;
use fl_core::ids::{Kind, ProjectId};
use fl_core::iri::Iri;
use fl_core::routing::{GithubTier, RoutingFault, Tier};
use fl_core::store::{StoreError, Tracker};
use fl_github::GithubTracker;
use std::cell::OnceCell;

/// How the GitHub tracker is opened: `open_github` in `main.rs`.
pub type Opener<'a> = Box<dyn Fn(&TrackerBinding) -> anyhow::Result<GithubTracker> + 'a>;

pub struct LazyGithub<'a> {
    binding: Option<TrackerBinding>,
    /// Where a binding goes, for the refusal when there is none.
    config: String,
    open: Opener<'a>,
    opened: OnceCell<GithubTracker>,
}

impl<'a> LazyGithub<'a> {
    pub fn new(binding: Option<TrackerBinding>, config: String, open: Opener<'a>) -> Self {
        Self {
            binding,
            config,
            open,
            opened: OnceCell::new(),
        }
    }

    /// The GitHub tracker, opened now if it is not yet (routing spec §2.6).
    /// An error that is not a store error keeps its whole message chain.
    pub fn open(&self) -> Result<&GithubTracker, StoreError> {
        if let Some(g) = self.opened.get() {
            return Ok(g);
        }
        let Some(b) = &self.binding else {
            return Err(RoutingFault::TierUnavailable {
                tier: Tier::Github,
                why: format!(
                    "this machine binds no GitHub repository for the project: add `tracker = \
                     {{ github = \"owner/repo\", credential = \"env\" }}` to its entry in {}",
                    self.config
                ),
            }
            .into());
        };
        let g = (self.open)(b).map_err(|e| match e.downcast::<StoreError>() {
            Ok(store) => store,
            Err(other) => StoreError::Backend(format!("{other:#}")),
        })?;
        Ok(self.opened.get_or_init(|| g))
    }
}

impl GithubTier for LazyGithub<'_> {
    fn available(&self) -> bool {
        self.binding.is_some()
    }

    /// An issue URL under the configured name — or, once GitHub is open,
    /// under the repository's name now, which differs after a rename and is
    /// the name the tracker writes. With no binding, none: an issue URL is
    /// then looked for in the local tier, where it may be an alias, and
    /// refused as the missing tier if it is not one (`issue_form`).
    fn claims(&self, id: &Iri) -> bool {
        let (Some((name, _)), Some(b)) = (fl_github::meta::parse_issue_url(id), &self.binding)
        else {
            return false;
        };
        name.eq_ignore_ascii_case(&b.github)
            || self
                .opened
                .get()
                .is_some_and(|g| name.eq_ignore_ascii_case(&g.repo().full_name))
    }

    fn issue_form(&self, id: &Iri) -> bool {
        fl_github::meta::is_issue_url(id)
    }

    fn tracker(&self) -> Result<&dyn Tracker, StoreError> {
        Ok(self.open()?)
    }

    fn require_private(&self) -> Result<(), StoreError> {
        self.open()?.require_private()
    }

    fn items_in_area(
        &self,
        project: &ProjectId,
        area: &str,
    ) -> Result<Vec<(Kind, Iri)>, StoreError> {
        self.open()?.items_in_area(project, area)
    }
}

/// A routed store's tiers, as a command sees them.
pub struct Tiers<'a> {
    pub router: &'a TieredTracker<'a>,
    pub github: &'a LazyGithub<'a>,
}
```

In `crates/cli/src/ctx.rs`: add `use fl_core::iri::Iri; use fl_core::ids::Kind; use fl_core::routing::Tier;`, add to `Ctx`:

```rust
    /// A routed store's tiers (routing spec §1.3): the router — which is
    /// also `tracker` — and the GitHub tier it opens lazily. `None` in an
    /// unrouted store.
    pub tiers: Option<&'a crate::tiers::Tiers<'a>>,
```

and to `impl Ctx<'_>`:

```rust
    /// How a person reads a record's or finding's id (routing spec §2.3):
    /// in a routed store a GitHub item is `#41` and a local one `41`, so
    /// every printed handle can be typed back. Elsewhere, as before.
    pub fn show_item(&self, kind: Kind, id: &Iri) -> anyhow::Result<String> {
        let Some(t) = self.tiers else {
            return crate::refs::show(self.handles, kind, id);
        };
        match t.router.tier_of(id) {
            Tier::Local => crate::refs::show(self.store, kind, id),
            Tier::Github => Ok(match t.github.open()?.handle_of(kind, id)? {
                Some(n) => format!("#{n}"),
                None => id.to_string(),
            }),
        }
    }

    /// Whether `id` is written where another machine reads it — on GitHub
    /// — so a gate it names must be in the committed manifest (GitHub
    /// tracker spec §4.3).
    pub fn on_github(&self, id: &Iri) -> bool {
        match self.tiers {
            Some(t) => t.router.tier_of(id) == Tier::Github,
            None => self.github.is_some(),
        }
    }
```

Add `tiers: None,` to the `Ctx` literal in `ctx.rs`'s test, the two in `comment.rs`'s tests, and the one in `cmd/ledger.rs`'s tests.

In `crates/cli/src/cmd/routing.rs`, add after `parse_tier`:

```rust
/// The refusal for `--area` or `--tier` in a store with no routing map.
pub fn not_routed(flag: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`{flag}` needs a routing map, and this project declares none: an area is a name its \
         routing map declares, and a tier one it routes to. `fl routing set --project \
         <project> <area> <tier>` declares one"
    )
}

/// Routing spec decision 12: a routed project keeps its runs and decisions
/// in the local ledger in this release — `SplitLedger::flush` refuses a
/// decision about a record the repository does not hold, so every gated
/// move of a local record would fail.
pub fn github_ledger_refusal(repo: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the binding of `{repo}` names `ledger = \"github\"`, and a routed project keeps its runs \
         and decisions in the local ledger: fl does not publish a routed project's ledger to \
         GitHub yet. Remove `ledger = \"github\"` from the binding"
    )
}
```

and to `impl Cmd`:

```rust
    /// Whether this command writes a routing map.
    pub fn sets_routing(&self) -> bool {
        matches!(self, Cmd::Set { .. })
    }
```

In `crates/cli/src/cmd/manifest.rs`, add `use fl_core::Routes;` and after `ensure_import_current`:

```rust
/// Routing spec §1.2: one routing rule per project, not per machine. On an
/// importing machine the import must be current. On the authoring machine a
/// manifest in the working tree must carry the store's map; with no
/// manifest, no other machine can import the project, so there is no other
/// rule to disagree with.
pub fn ensure_routing_current(store: &RedbStore, project: &ProjectId) -> Result<()> {
    let root = root_of(store, project)?;
    let path = root.join(MANIFEST_PATH);
    if let Some(recorded) = store.imported_hash(project)? {
        let m = manifest_of(&root, project, false)?;
        if m.content_sha256 != recorded {
            bail!(
                "the manifest at {} changed since this store imported it, and it may route items \
                 differently. Run `fl manifest import` before making an item",
                path.display()
            );
        }
        return Ok(());
    }
    if !path
        .try_exists()
        .with_context(|| format!("could not look for {}", path.display()))?
    {
        return Ok(());
    }
    let m = read(&root)?;
    if m.body.project == *project && m.body.routing != store.routes(project)? {
        bail!(
            "the routing map of project {project} changed since the manifest at {} was \
             exported, so another machine would route differently. Run `fl manifest export \
             --project {project}`, then commit",
            path.display()
        );
    }
    Ok(())
}
```

In the same file, `Binding::Github(String)` becomes a struct variant carrying the ledger choice:

```rust
    /// The project is bound to this `owner/repo`, with the GitHub ledger
    /// or not.
    Github { repo: String, github_ledger: bool },
```

`ledger_node`'s arm becomes `Binding::Github { repo, .. } => …`, Task 9's import notice reads `matches!(binding, Binding::Github { .. })`, and in `Import`, right after `let m = read(&root)?;`, add:

```rust
            // Routing spec decision 12: a routed project keeps its ledger
            // local, so a binding that names the GitHub ledger takes none.
            if m.body.routing.is_some()
                && let Binding::Github {
                    repo,
                    github_ledger: true,
                } = binding
            {
                return Err(crate::cmd::routing::github_ledger_refusal(repo));
            }
```

In `run` (`main.rs`), the `manifest_binding` arm `Some(t) => cmd::manifest::Binding::Github(t.github.clone()),` becomes:

```rust
            Some(t) => cmd::manifest::Binding::Github {
                repo: t.github.clone(),
                github_ledger: t.github_ledger(),
            },
```

In `crates/cli/src/cmd/record.rs`, give `Add` the field

```rust
        /// The record's area (routing spec §1.1), which routes it to a tier.
        #[arg(long)]
        area: Option<String>,
```

and in `run`, replace `Cmd::Add { project, title }` and its `let id = ctx.tracker.add_record(&p, &title)?;` with `Cmd::Add { project, title, area }` and:

```rust
            let id = match (ctx.tiers, &area) {
                (None, Some(_)) => return Err(crate::cmd::routing::not_routed("--area")),
                (None, None) => ctx.tracker.add_record(&p, &title)?,
                (Some(_), _) => {
                    crate::cmd::manifest::ensure_routing_current(store, &p)?;
                    ctx.tracker.add_record_with_area(&p, &title, area.as_deref())?
                }
            };
```

In `record.rs` and `finding.rs`, every `refs::show(ctx.handles, Kind::Record, …)` and `refs::show(ctx.handles, Kind::Finding, …)` becomes `ctx.show_item(Kind::Record, …)` / `ctx.show_item(Kind::Finding, …)` — nine call sites:

```bash
sed -i 's/refs::show(ctx\.handles, Kind::\(Record\|Finding\), /ctx.show_item(Kind::\1, /' crates/cli/src/cmd/record.rs crates/cli/src/cmd/finding.rs
grep -n 'refs::show(ctx.handles, Kind::\(Record\|Finding\)' crates/cli/src/cmd/*.rs   # prints nothing
```

In `finding.rs`'s `Reproduce` arm, `if ctx.github.is_some() {` becomes `if ctx.on_github(fid.iri()) {`.

In `crates/cli/src/main.rs`: add `mod tiers;`. In `impl Command`, add:

```rust
    /// Whether the command writes a routing map: decision 12's refusal
    /// reads the binding for it, routed or not yet.
    fn sets_routing(&self) -> bool {
        matches!(self, Command::Routing(c) if c.sets_routing())
    }
```

After `local_only_reason`, add:

```rust
/// Routing spec decision 12, when a command starts.
fn refuse_routed_github_ledger(
    routed: bool,
    binding: Option<&config::TrackerBinding>,
) -> Result<()> {
    if let Some(b) = binding.filter(|b| routed && b.github_ledger()) {
        return Err(cmd::routing::github_ledger_refusal(&b.github));
    }
    Ok(())
}
```

In `run`, right after `let store = RedbStore::open(&path)…?;`, add:

```rust
    // A store in which any project routes its items between two tiers
    // (routing spec §1.3), whatever its maps hold.
    let routed = store.holds_routing()?;
    refuse_routed_github_ledger(
        routed || cli.command.sets_routing(),
        binding.as_ref().or(here_binding.as_ref()),
    )?;
```

In the `let github = match &binding {` arms, add first: `Some(_) if routed => None,` with the comment `// A routed store opens GitHub on the first call that needs it (routing spec §2.6), through `lazy`.` After `let ledger: &dyn fl_core::Ledger = …;`, add:

```rust
    let config_path = config::path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "the config".into());
    let lazy = routed.then(|| {
        tiers::LazyGithub::new(
            binding.clone(),
            config_path,
            Box::new(|b: &config::TrackerBinding| open_github(b, cfg.github.as_ref(), &store)),
        )
    });
    let tiered = lazy.as_ref().map(|l| fl_core::TieredTracker {
        catalog: &store,
        local: &store,
        routes: &store,
        github: l,
    });
    let tiers = match (&tiered, &lazy) {
        (Some(router), Some(github)) => Some(tiers::Tiers { router, github }),
        _ => None,
    };
    // `fl github …` names GitHub items only, so it opens GitHub now, routed
    // or not.
    let routed_github = match (&lazy, &cli.command) {
        (Some(l), Command::Github(_)) => Some(l.open()?),
        _ => None,
    };
```

Replace `let (checked, routed);` and the `let ctx = match &github { … };` with:

```rust
    let (checked, numbered);
    let ctx = match (&tiers, &github) {
        (Some(t), _) => Ctx {
            store: &store,
            tracker: t.router,
            ledger,
            handles: &store,
            github: routed_github,
            github_ledger: None,
            witness: None,
            tracker_label: match &binding {
                Some(b) => format!("{} or GitHub `{}`", store.label(), b.github),
                None => store.label().to_string(),
            },
            tiers: Some(t),
        },
        (None, Some(gh)) => {
            checked = CatalogChecked {
                catalog: &store,
                tracker: gh,
            };
            numbered = KindRouted {
                catalog: &store,
                tracker: gh,
            };
            Ctx {
                store: &store,
                tracker: &checked,
                ledger,
                handles: &numbered,
                github: Some(gh),
                github_ledger: github_ledger.as_ref(),
                witness: witness.as_ref(),
                tracker_label: format!("github:{}", gh.repo().full_name),
                tiers: None,
            }
        }
        (None, None) => Ctx {
            store: &store,
            tracker: &store,
            ledger,
            handles: &store,
            github: None,
            github_ledger: None,
            witness: None,
            tracker_label: store.label().to_string(),
            tiers: None,
        },
    };
```

In `docs/routing.md`, append:

```markdown
## What a routed project needs

A project is routed once it has a routing map, whatever the map holds. The `github` tier is
available on a machine whose config binds the project to a repository (the `tracker` binding of
[github-tracker.md](github-tracker.md)). Without one, the machine works on local items: a
`github`-tier item is refused, naming the missing config entry, and fl never puts it in the local
tier instead.

fl opens GitHub only when a command needs a GitHub item — a GitHub-tier create, a GitHub id or
`#41` handle, a list of both tiers — so work on local items needs neither the network nor a
credential. `fl github …` names GitHub items only, and opens GitHub at the start.

A routed project keeps its runs and decisions in the local store: a binding with `ledger =
"github"` is refused while the project is routed, and `fl routing set` refuses to route a project
whose binding names it.

Every machine routes the same way. On a machine that imported the project, a new item needs the
import to be current (`fl manifest import`). On the machine that authors it, once the project has
a manifest, the manifest must carry the current map (`fl manifest export`, then commit).
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS — the new tests, and every existing CLI test unchanged.

- [ ] **Step 5: Mutation checks**

1. GitHub is not opened at the start in a routed store: delete the `Some(_) if routed => None,` arm → `cargo test -p fl-cli --test routing -- a_local_item_is_made_and_moved_without_a_request_to_github` red.
2. `refuse_routed_github_ledger`'s `routed` conjunct: drop `routed &&` → `cargo test -p fl-cli --bin fl -- tests::a_routed_store_whose_binding_names_the_github_ledger_is_refused` red ("unrouted: as before").
3. Its ledger conjunct: drop `&& b.github_ledger()` → the same test red.
4. `sets_routing` reaches it: pass `routed` alone → `cargo test -p fl-cli --test routing -- a_routed_binding_with_the_github_ledger_is_refused_when_the_command_starts` red (the first `set` lands).
5. The binding it reads: pass `binding.as_ref()` alone → the same test red (`routing set` reads no tracker binding).
6. `LazyGithub::open`'s cache: delete the `if let Some(g)` → `cargo test -p fl-cli --bin fl -- tiers::tests::an_open_that_fails_with_a_store_error_keeps_it_and_one_that_succeeds_is_kept` red.
7. Its no-binding refusal: replace it with a call to `(self.open)` on a default binding → `tiers::tests::with_no_binding_the_tier_is_unavailable_naming_the_config_entry` red.
8. Its downcast: map every error to `Backend` → `an_open_that_fails_with_a_store_error…` red.
9. `claims`'s name comparison: return `true` for every issue URL when bound → `tiers::tests::nothing_is_opened_until_a_github_item_is_needed` red (`acme/other`).
10. `show_item`'s GitHub arm: print the bare number → `cargo test -p fl-cli --test routing -- a_github_tier_record_is_an_issue_with_its_area_label_printed_as_hash` red.
11. `record add`'s unrouted `--area` refusal: make that arm add the record → `area_in_a_project_with_no_routing_map_is_refused_naming_routing_set` red.
12. The routing currency call in `record add`: delete it → `a_routed_create_on_the_authoring_machine_needs_the_map_exported` red and `a_routed_create_on_an_importing_machine_needs_the_import_current` red.
13. `ensure_routing_current`'s import branch compares hashes: make it `return Ok(())` → the importing test red.
14. Its no-manifest pass: delete the `try_exists` early return → `a_routed_create_on_the_authoring_machine_needs_the_map_exported` red (the first create fails reading a missing manifest).
15. Its comparison: replace `m.body.routing != store.routes(project)?` with `false` → the authoring test red. Its `m.body.project == *project` conjunct is not observable: a test world has one project per root. Not a guard a test reaches.
16. `on_github` in `reproduce`: replace it with `ctx.github.is_some()` → `reproducing_a_github_tier_finding_checks_the_committed_manifest` red (the gate runs).
17. `fl github` opens GitHub in a routed store: make `routed_github` always `None` → `cargo test -p fl-cli --test routing -- a_bare_number_given_to_fl_github_is_an_issue` (Task 11) red. Not observable in this task: no test here runs `fl github` in a routed store.
18. `claims` takes the opened repository's name: drop the `|| self.opened.get()…` disjunct → `cargo test -p fl-cli --test routing -- after_a_rename_a_github_item_still_prints_as_hash` red (the issue prints as its URL).
19. `claims` with no binding claims nothing: make the `let … else` return `true` for any issue URL when unbound → `cargo test -p fl-cli --bin fl -- tiers::tests::with_no_binding_the_tier_is_unavailable_naming_the_config_entry` red.
20. `Import`'s decision-12 refusal: delete it → `a_routed_manifest_is_not_imported_where_the_binding_names_the_github_ledger` red; drop its `m.body.routing.is_some() &&` conjunct → `cargo test -p fl-cli --test ledger -- a_machine_with_no_cut_over_is_refused_naming_init` red (an unrouted manifest under the GitHub ledger is refused; checked against a build of this plan).

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/tiers.rs crates/cli/src/main.rs crates/cli/src/ctx.rs crates/cli/src/comment.rs crates/cli/src/cmd/ledger.rs crates/cli/src/cmd/routing.rs crates/cli/src/cmd/manifest.rs crates/cli/src/cmd/record.rs crates/cli/src/cmd/finding.rs crates/cli/tests/routing.rs docs/routing.md
git commit -m "feat(cli): a routed store gets the routing tracker, with GitHub opened lazily

A store in which a project has a routing map gets TieredTracker over the
local store and a GitHub tier opened on the first call that needs it, so
local work makes no request to GitHub; fl github opens it at the start. A
routed store whose binding names the GitHub ledger is refused. record add
takes --area, after the routing-currency check; --area in an unrouted
store is refused. GitHub items print as #41. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 11: In a routed store, `#41` is a GitHub issue and `41` a local item

Handles that never mean two items (routing spec §2.3): `Ref` gains `Issue(n)` for `#41` — today `#41` and `41` parse alike (`crates/cli/src/refs.rs:38-44`). In a routed store `#41` (and `owner/repo#41`) always names GitHub issue 41 and opens GitHub to resolve it; a bare `41` names local item 41, and when no local item holds it, the refusal asks "did you mean `#41`?" if the GitHub tier is available. Outside a routed store both spellings resolve as before. `fl github …` keeps reading a bare number as an issue. And a typed IRI no local store holds reaches the router in a routed store (plan ruling 28): `choose_store` falls back to the bound store when it is routed, and a routed store keeps issue URLs out of the store search even with no binding — so the router can find an item another machine moved to GitHub, say "held on another machine's local tier", or name the missing config entry.

**Blast radius:** `Ref` gains a variant: every `match` on it (`refs.rs`, `cmd/github.rs`, `cmd/ledger.rs`). Every resolution of a typed record or finding goes through `Ctx::resolve_item` (`record move`, `finding *`, `attempt`, `check --record`), which is `refs::resolve` in an unrouted store. `has_handle` counts `#41` as a handle, as it did. `run()`'s store choice: a command that names an IRI opens the bound store once more to read whether it is routed; in an unrouted store nothing else changes.

**Files:**
- Modify: `crates/cli/src/refs.rs` (`Ref::Issue`; tests)
- Modify: `crates/cli/src/ctx.rs` (`resolve_item`)
- Modify: `crates/cli/src/cmd/record.rs`, `finding.rs`, `attempt.rs`, `check.rs` (`resolve_item`)
- Modify: `crates/cli/src/cmd/github.rs`, `crates/cli/src/cmd/ledger.rs` (`Ref::Issue`)
- Modify: `crates/cli/src/main.rs` (`bound_is_routed`, `is_not_owned`; `run`'s store choice)
- Modify: `crates/cli/src/tiers.rs` (`LazyGithub::repo_name`)
- Modify: `crates/cli/tests/routing.rs`
- Modify: `docs/routing.md`, `docs/github-tracker.md` ("Identity")

**Interfaces:**
- Consumes: `Ctx.tiers`, `LazyGithub::open`, `GithubTier::available` (Task 10); `RedbStore::holds_routing` (Task 3).
- Produces: `refs::Ref::Issue(u64)`; `Ctx::resolve_item(&self, kind: Kind, r: &Ref) -> anyhow::Result<Iri>`; `LazyGithub::repo_name(&self) -> Option<&str>`; in `main.rs`, `fn bound_is_routed(bound: &Path) -> Result<bool>` and `fn is_not_owned(e: &anyhow::Error) -> bool`. Unique phrases: ``Did you mean `#``, `in this machine's local tier`.

- [ ] **Step 1: Write the failing tests**

In `crates/cli/src/refs.rs`, inside `mod tests`, replace `a_hash_handle_is_a_handle` with:

```rust
    // Routing spec §2.3: `#41` is GitHub's spelling, kept apart from a bare
    // number, and each prints back as it was typed.
    #[test]
    fn a_hash_number_is_an_issue_and_a_bare_one_a_handle() {
        assert!(matches!(parse("#41"), Ok(Ref::Issue(41))));
        assert!(matches!(parse("41"), Ok(Ref::Handle(41))));
        assert_eq!(parse("#41").unwrap().to_string(), "#41");
        assert_eq!(parse("41").unwrap().to_string(), "41");
        let (issue, handle) = (parse("#41").unwrap(), parse("41").unwrap());
        assert!(has_handle(&[&issue]) && has_handle(&[&handle]));
        assert!(iris(&[&issue, &handle]).is_empty());
        assert!(parse("#").is_err() && parse("#4a").is_err());
    }

    // Outside a routed store, nothing changes: `#1` and `1` name one item.
    #[test]
    fn outside_a_routed_store_a_hash_number_and_a_bare_one_resolve_alike() {
        use fl_core::store::{Catalog, Tracker};
        let s = fl_core::MemStore::default();
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        for typed in [Ref::Handle(1), Ref::Issue(1)] {
            assert_eq!(resolve(&s, "memory", Kind::Record, &typed).unwrap(), r.0);
        }
    }
```

In `crates/cli/tests/routing.rs`, extend Task 10's `after_a_rename_a_github_item_still_prints_as_hash`: after its `assert_eq!`, add

```rust
    let moved = g.ok(&["record", "move", "#1", "--to", "doing"]);
    assert!(moved.starts_with("#1\tdoing\t"), "{moved}");
```

— `#1` now resolves through GitHub, whose answer names the repository's name now. Then add:

```rust
// Routing spec §2.3: in a routed project the two spellings name two items,
// and each prints back as it can be typed.
#[test]
fn a_bare_number_and_a_hash_number_name_different_items_and_print_back_as_typed() {
    let g = world(BOUND);
    g.routed();
    assert_eq!(
        g.ok(&["record", "add", "--project", "1", "--title", "local one", "--area", "code"]),
        "1\tlocal one\n"
    );
    assert_eq!(
        g.ok(&["record", "add", "--project", "1", "--title", "issue one", "--area", "design"]),
        "#1\tissue one\n"
    );
    // An ungated move prints `<handle>\t<state>\tungated: …`.
    let moved = g.ok(&["record", "move", "1", "--to", "doing"]);
    assert!(moved.starts_with("1\tdoing\t"), "{moved}");
    let moved = g.ok(&["record", "move", "#1", "--to", "review"]);
    assert!(moved.starts_with("#1\treview\t"), "{moved}");
    let labels = g.fake.issue(1).labels;
    assert!(
        labels.contains(&"fl:record/review".to_string())
            && !labels.contains(&"fl:record/doing".to_string()),
        "{labels:?}"
    );
}

#[test]
fn a_bare_number_no_local_item_holds_asks_did_you_mean_the_issue() {
    let g = world(BOUND);
    g.routed();
    g.fl()
        .args(["record", "move", "5", "--to", "doing"])
        .assert()
        .failure()
        .stderr(contains("Did you mean `#5`?"));
    assert!(g.fake.state().requests.is_empty(), "a bare number never reaches GitHub");
}

#[test]
fn on_an_unbound_machine_a_bare_number_gets_no_hint() {
    let g = world("");
    g.routed();
    g.fl()
        .args(["record", "move", "5", "--to", "doing"])
        .assert()
        .failure()
        .stderr(contains("in this machine's local tier").and(contains("Did you mean").not()));
}

#[test]
fn a_hash_number_that_is_no_record_is_refused_naming_the_repository() {
    let g = world(BOUND);
    g.routed();
    g.fl()
        .args(["record", "move", "#9", "--to", "doing"])
        .assert()
        .failure()
        .stderr(contains("there is no record #9 in acme/widgets"));
}

// Routing spec §2.3: `fl github …` names GitHub items only, so a bare
// number there is an issue.
#[test]
fn a_bare_number_given_to_fl_github_is_an_issue() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "t", "--area", "design"]);
    g.fl()
        .args(["github", "repair", "1", "--by", "owner"])
        .assert()
        .success()
        .stdout("consistent\t1\ttodo\n");
}

// Routing spec §2.3: `owner/repo#41` names the issue in a routed store too.
#[test]
fn owner_repo_hash_names_the_issue_in_a_routed_store() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "l", "--area", "code"]);
    g.ok(&["record", "add", "--project", "1", "--title", "g", "--area", "design"]);
    let moved = g.ok(&["record", "move", "acme/widgets#1", "--to", "doing"]);
    assert!(moved.starts_with("#1\tdoing\t"), "{moved}");
    assert!(g.fake.issue(1).labels.contains(&"fl:record/doing".to_string()));
}

// Routing spec §2.2: an id no local store holds reaches the router —
// GitHub's alias scan finds an item another machine moved there; otherwise
// it is held on another machine's local tier.
#[test]
fn an_id_no_local_store_holds_is_looked_for_on_github_then_said_to_be_elsewhere() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "look", "--area", "design"]);
    let moved_here = "urn:uuid:00000000-0000-7000-8000-0000000000aa";
    g.fake.web_edit(1, |i| {
        let (prose, mut m) = fl_github::meta::parse_body(&i.body).unwrap();
        m.also_known_as.push(fl_core::Iri::parse(moved_here).unwrap());
        i.body = fl_github::meta::render_body(&prose, &m);
    });
    let moved = g.ok(&["record", "move", moved_here, "--to", "doing"]);
    assert!(moved.starts_with("#1\tdoing\t"), "{moved}");
    g.fl()
        .args([
            "record", "move", "urn:uuid:00000000-0000-7000-8000-0000000000bb", "--to", "doing",
        ])
        .assert()
        .failure()
        .stderr(contains("another machine's local tier"));
}

// Routing spec §1.3: on an unbound machine an issue URL is refused as the
// missing config entry, never searched for as a local id.
#[test]
fn an_issue_url_on_an_unbound_machine_names_the_missing_config_entry() {
    let g = world("");
    g.routed();
    g.fl()
        .args(["record", "move", "https://github.com/acme/widgets/issues/1", "--to", "doing"])
        .assert()
        .failure()
        .stderr(contains("binds no GitHub repository for the project"));
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --bin fl -- refs::tests::` and `cargo test -p fl-cli --test routing`
Expected: FAIL to compile (`Ref::Issue` does not exist), then the black-box handle tests FAIL (`#1` resolves to local item 1).

- [ ] **Step 3: Implement**

In `crates/cli/src/refs.rs`, replace `Ref` with:

```rust
#[derive(Debug, Clone)]
pub enum Ref {
    /// A bare number: a local item in a routed store (routing spec §2.3),
    /// the tracker's own handle elsewhere.
    Handle(u64),
    /// `#41`: a GitHub issue in a routed store; elsewhere the same as a
    /// bare number, as GitHub writes one.
    Issue(u64),
    Iri(Iri),
}
```

In `FromStr`, replace the `// A handle may be written `#41`…` block with:

```rust
        if let Some(digits) = s.strip_prefix('#')
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            return digits
                .parse::<u64>()
                .map(Ref::Issue)
                .map_err(|_| format!("`{s}` is too large to be an issue number"));
        }
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            return s
                .parse::<u64>()
                .map(Ref::Handle)
                .map_err(|_| format!("`{s}` is too large to be a handle"));
        }
```

In `Display`, add `Ref::Issue(n) => write!(f, "#{n}"),`. In `resolve`, make the handle arm `Ref::Handle(n) | Ref::Issue(n) => match …`. In `iris`, the filter's other arm becomes `Ref::Handle(_) | Ref::Issue(_) => None`. In `has_handle`, `matches!(r, Ref::Handle(_) | Ref::Issue(_))`.

In `crates/cli/src/tiers.rs`, add to `impl LazyGithub`, after `new`:

```rust
    /// The configured `owner/repo`, when this machine binds one.
    pub fn repo_name(&self) -> Option<&str> {
        self.binding.as_ref().map(|b| b.github.as_str())
    }
```

and to its third test, after `assert_eq!(opened.get(), 1, "opened once");`, add `assert_eq!(lazy.repo_name(), Some("acme/widgets"));`.

In `crates/cli/src/main.rs`, before `choose_among`, add:

```rust
/// Whether the store at `bound` exists and holds a routing map. ⚠ Never
/// creates a store: only a file that exists is opened.
fn bound_is_routed(bound: &Path) -> Result<bool> {
    if !bound.exists() {
        return Ok(false);
    }
    let store = RedbStore::open(bound)
        .with_context(|| format!("could not open the store at {}", bound.display()))?;
    Ok(store.holds_routing()?)
}

fn is_not_owned(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<StoreError>(), Some(StoreError::NotOwned { .. }))
}
```

and in `run`, replace from `let mut iris = cli.command.iris();` to `let path = choose_store(&bound, entries, &iris, confined)?;` with:

```rust
    let mut iris = cli.command.iris();
    // Whether the bound store is routed (routing spec §1.3), read only when
    // the command names an IRI: in a routed store an issue URL is the github
    // tier's, and an id no store holds is the router's to look for.
    let bound_routed = !iris.is_empty() && bound_is_routed(&bound)?;
    // A GitHub issue URL is the tracker's to resolve: no local store holds
    // one, and searching them would refuse it as NotOwned (spec §2.2).
    if here_binding.is_some() || bound_routed {
        iris.retain(|i| !fl_github::meta::is_issue_url(i));
    }
    let path = match choose_store(&bound, entries, &iris, confined) {
        // Routing spec §2.2: an id no local store holds may be an item
        // another machine moved to GitHub, or one on another machine's local
        // tier. The router says which; refusing here would say neither.
        Err(e) if bound_routed && is_not_owned(&e) => bound.clone(),
        other => other?,
    };
```

In `crates/cli/src/ctx.rs`, add `use crate::refs::Ref; use anyhow::bail; use fl_core::routing::GithubTier;` and to `impl Ctx<'_>`:

```rust
    /// The id a typed record or finding names (routing spec §2.3). In a
    /// routed store `#41` is GitHub issue 41 and a bare `41` local item 41;
    /// a bare number no local item holds asks whether `#41` was meant, when
    /// this machine has the GitHub tier. Elsewhere, as before.
    pub fn resolve_item(&self, kind: Kind, r: &Ref) -> anyhow::Result<Iri> {
        let Some(t) = self.tiers else {
            return crate::refs::resolve(self.handles, &self.tracker_label, kind, r);
        };
        let what = kind.as_wire();
        match r {
            Ref::Iri(i) => Ok(i.clone()),
            Ref::Issue(n) => match t.github.open()?.resolve_handle(kind, *n)? {
                Some(i) => Ok(i),
                None => bail!(
                    "there is no {what} #{n} in {}. List them to see the ones that exist.",
                    t.github.repo_name().unwrap_or("the bound repository")
                ),
            },
            Ref::Handle(n) => match self.store.resolve_handle(kind, *n)? {
                Some(i) => Ok(i),
                None if t.github.available() => bail!(
                    "there is no {what} {n} in this machine's local tier. Did you mean `#{n}`? In \
                     a routed project a bare number names a local item, and `#{n}` names GitHub \
                     issue {n}."
                ),
                None => bail!(
                    "there is no {what} {n} in this machine's local tier. List them to see the \
                     ones that exist."
                ),
            },
        }
    }
```

Route every resolution of a typed record or finding through it — five call sites in `record.rs` (`Move`), `finding.rs` (`finding_id`, `Raise`), `attempt.rs` and `check.rs`:

```bash
perl -0pi -e 's/refs::resolve\(\s*ctx\.handles,\s*&ctx\.tracker_label,\s*Kind::(Record|Finding),\s*([^,()]+?),?\s*\)/ctx.resolve_item(Kind::$1, $2)/g' crates/cli/src/cmd/record.rs crates/cli/src/cmd/finding.rs crates/cli/src/cmd/attempt.rs crates/cli/src/cmd/check.rs
grep -n 'tracker_label,' crates/cli/src/cmd/*.rs   # no `refs::resolve` of a record or finding is left
```

In `crates/cli/src/cmd/github.rs` (`Repair`) and `crates/cli/src/cmd/ledger.rs` (`comment`), the arm `Ref::Handle(n) =>` becomes `Ref::Handle(n) | Ref::Issue(n) =>`: those commands name GitHub items only.

In `docs/routing.md`, at the end of "Handles change", add:

```markdown
`fl record` and `fl finding` read a bare number as a local item: one that no local item holds
is refused, asking whether `#41` was meant. A GitHub item prints as `#41` and a local one as
`41`, so every handle fl prints can be typed back. `fl github …` names GitHub items only, so a
bare number there is still an issue.
```

In `docs/github-tracker.md`, section "Identity", after the sentence ending "so `fl record` refuses the number of a finding.", add: "In a project that routes its items between its local store and GitHub ([routing.md](routing.md)), only `#41` names the issue; a bare `41` names a local item."

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS — including `crates/cli/tests/github.rs`'s `a_handle_may_carry_a_hash_and_an_issue_url_skips_the_local_stores`, unchanged: an unrouted GitHub project reads `#1` as before.

- [ ] **Step 5: Mutation checks**

1. `#41` parses as an issue: make it `Ref::Handle` → `cargo test -p fl-cli --bin fl -- refs::tests::a_hash_number_is_an_issue_and_a_bare_one_a_handle` red.
2. `Display` of an issue: print `{n}` → the same test red.
3. `has_handle` counts an issue: drop `| Ref::Issue(_)` → the same test red.
4. Unrouted, an issue resolves as a handle: make `resolve`'s `Ref::Issue(n)` bail → `refs::tests::outside_a_routed_store_a_hash_number_and_a_bare_one_resolve_alike` red, and `cargo test -p fl-cli --test github -- a_handle_may_carry_a_hash_and_an_issue_url_skips_the_local_stores` red.
5. Routed, `#n` goes to GitHub: resolve `Ref::Issue(n)` in the store → `cargo test -p fl-cli --test routing -- a_bare_number_and_a_hash_number_name_different_items_and_print_back_as_typed` red.
6. Routed, a bare `n` stays local: resolve `Ref::Handle(n)` through GitHub → the same test red, and `a_bare_number_no_local_item_holds_asks_did_you_mean_the_issue` red (a request).
7. The hint's guard: replace `if t.github.available()` with `if true` → `on_an_unbound_machine_a_bare_number_gets_no_hint` red; with `if false` → `a_bare_number_no_local_item_holds_asks_did_you_mean_the_issue` red.
8. The issue refusal names the repository: write "the bound repository" always → `a_hash_number_that_is_no_record_is_refused_naming_the_repository` red.
9. `fl github repair` reads a bare number as an issue: in a routed store this is `Ref::Handle(n) => gh.issue_url(*n)`, unchanged — and `routed_github` (Task 10) opens it; make `routed_github` `None` → `a_bare_number_given_to_fl_github_is_an_issue` red.
10. The store choice falls back to a routed bound store: delete the `Err(e) if bound_routed && is_not_owned(&e)` arm → `an_id_no_local_store_holds_is_looked_for_on_github_then_said_to_be_elsewhere` red ("no store holds"). Make `bound_routed` always `false` → the same test red, and `an_issue_url_on_an_unbound_machine_names_the_missing_config_entry` red.
11. The unbound strip: drop `|| bound_routed` from the `retain` guard → not observable once the fallback exists: the URL reaches `choose_store`, which answers `NotOwned`, and the fallback hands it to the router anyway. The strip saves a search of every configured store; not a guard a test tells apart.
12. Only `NotOwned` falls back: drop `&& is_not_owned(&e)` → not observable with one configured store (every other refusal of `choose_store` needs two stores holding the id). Not a guard a test in this world tells apart.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/refs.rs crates/cli/src/ctx.rs crates/cli/src/main.rs crates/cli/src/tiers.rs crates/cli/src/cmd/record.rs crates/cli/src/cmd/finding.rs crates/cli/src/cmd/attempt.rs crates/cli/src/cmd/check.rs crates/cli/src/cmd/github.rs crates/cli/src/cmd/ledger.rs crates/cli/tests/routing.rs docs/routing.md docs/github-tracker.md
git commit -m "feat(cli): in a routed store #41 is a GitHub issue and 41 a local item

Ref gains Issue for #41. In a routed store #41 resolves on GitHub and a
bare number in the local tier; a bare number no local item holds asks
whether #41 was meant. Outside a routed store both resolve as before, and
fl github keeps reading a bare number as an issue. In a routed store an
IRI no local store holds reaches the router, which finds it on GitHub,
says it is held on another machine's local tier, or names the missing
config entry. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 12: Making items in a routed project: `--tier`, a finding's area, sensitivity, disclosure

The rest of creation in the CLI (routing spec §1.1, §2.1, §2.5): `--tier` on `record add` and `finding raise`; `--area` on `finding raise`, else the record's area, which the output names (plan ruling 24); the routing-currency check on a finding too; a sensitive area or `--security` finding that the map sends to a public repository refused before anything is written, naming `--tier local`; a GitHub finding about a local record in a sensitive area refused on a non-private repository like any security item (spec decision 21 — the router's refusal from Task 7), and one about a local record in any other area warned about before it is written (plan ruling 21). `--tier` and `--area` in an unrouted store are refused (plan ruling 18).

**Blast radius:** `record add` and `finding raise` in a routed store now call the router's placement directly (`place_*`, `add_*_at`) instead of the trait's `add_*`, so the CLI can speak between the decision and the write. Unrouted paths are unchanged except for the two new refusals.

**Files:**
- Modify: `crates/cli/src/cmd/record.rs` (`Add`'s `--tier`)
- Modify: `crates/cli/src/cmd/finding.rs` (`Raise`'s `--area`, `--tier`; `warn_disclosure`)
- Modify: `crates/cli/tests/routing.rs`
- Modify: `docs/routing.md`

**Interfaces:**
- Consumes: `TieredTracker::{place_record, place_finding, add_record_at, add_finding_at}`, `FindingPlacement::{at, inherited, record}` (Task 7); `Ctx.tiers`, `Tiers` (Task 10); `cmd::routing::{parse_tier, not_routed}`; `cmd::manifest::ensure_routing_current`; `GithubTracker::visibility` (Task 6).
- Produces: `record add --tier <local|github>`; `finding raise --area <area> --tier <local|github>`; `fn warn_disclosure(t: &Tiers<'_>, at: &FindingPlacement) -> Result<()>` in `cmd/finding.rs`. Unique phrases: `from its record`, `names its local record`, `security-sensitive`.

- [ ] **Step 1: Write the failing tests**

In `crates/cli/tests/routing.rs`, add:

```rust
// Routing spec §2.1: `--tier` overrides the map; the area is recorded
// either way.
#[test]
fn a_tier_given_puts_a_record_there_with_its_area() {
    let g = world(BOUND);
    g.routed();
    assert_eq!(
        g.ok(&[
            "record", "add", "--project", "1", "--title", "t", "--area", "code", "--tier",
            "github",
        ]),
        "#1\tt\n"
    );
    assert!(g.fake.issue(1).labels.contains(&"fl:area/code".to_string()));
}

#[test]
fn tier_or_area_in_a_project_with_no_routing_map_is_refused() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t", "--tier", "local"])
        .assert()
        .failure()
        .stderr(contains("`--tier` needs a routing map, and this project declares none"));
    g.ok(&["record", "add", "--project", "1", "--title", "t"]);
    for extra in [["--area", "code"], ["--tier", "local"]] {
        let mut args = vec!["finding", "raise", "--record", "1", "--claim", "c", "--by", "r"];
        args.extend(extra);
        g.fl()
            .args(&args)
            .assert()
            .failure()
            .stderr(contains("needs a routing map, and this project declares none"));
    }
}

// Routing spec §1.1: a finding takes its record's area, and says so.
#[test]
fn a_finding_takes_its_records_area_and_says_so() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "look", "--area", "design"]);
    g.fl()
        .args(["finding", "raise", "--record", "#1", "--claim", "off", "--by", "rev"])
        .assert()
        .success()
        .stdout("#2\traised\toff\n")
        .stderr(contains("note: area: design, from its record"));
    g.fl()
        .args([
            "finding", "raise", "--record", "#1", "--claim", "here", "--by", "rev", "--area",
            "code",
        ])
        .assert()
        .success()
        .stdout("1\traised\there\n")
        .stderr(contains("from its record").not());
}

#[test]
fn a_finding_whose_record_has_no_area_needs_one() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.ok(&["record", "add", "--project", "1", "--title", "before routing"]);
    g.ok(&["routing", "set", "--project", "1", "code", "local"]);
    g.fl()
        .args(["finding", "raise", "--record", "1", "--claim", "c", "--by", "r"])
        .assert()
        .failure()
        .stderr(contains("has no area to inherit"));
    g.fl()
        .args(["finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area", "code"])
        .assert()
        .success();
}

// Routing spec §2.1, decision 13: never to a public repository by the map;
// nothing is created, and the refusal names `--tier local`.
#[test]
fn a_sensitive_area_routed_to_a_public_repository_creates_nothing_and_names_tier_local() {
    let g = world(BOUND);
    g.routed();
    g.fake.state().repos[0].visibility = "public".into();
    g.ok(&["record", "add", "--project", "1", "--title", "t", "--area", "code"]);
    for extra in [vec!["--area", "security"], vec!["--area", "design", "--security"]] {
        let mut args = vec!["finding", "raise", "--record", "1", "--claim", "c", "--by", "r"];
        args.extend(extra);
        g.fl()
            .args(&args)
            .assert()
            .failure()
            .stderr(contains("security-sensitive").and(contains("--tier local")));
    }
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "s", "--area", "security"])
        .assert()
        .failure()
        .stderr(contains("security-sensitive"));
    assert_eq!(g.fake.issue_count(), 0, "nothing created on GitHub");
    g.fl()
        .args([
            "finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area",
            "security", "--tier", "local",
        ])
        .assert()
        .success()
        .stdout("1\traised\tc\n");
}

#[test]
fn a_finding_in_a_sensitive_area_on_a_private_repository_is_a_security_finding() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "t", "--area", "code"]);
    g.ok(&["finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area", "security"]);
    assert!(g.fake.issue(1).body.contains("\"security\":true"), "{}", g.fake.issue(1).body);
}

// Routing spec §2.5: a GitHub finding about a local record publishes the
// record's title; on a repository that is not private, fl says so first.
#[test]
fn a_github_finding_about_a_local_record_warns_on_a_public_repository() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "fix the parser", "--area", "code"]);
    g.fl()
        .args(["finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area", "design"])
        .assert()
        .success()
        .stderr(contains("names its local record").not());
    g.fake.state().repos[0].visibility = "public".into();
    g.fl()
        .args(["finding", "raise", "--record", "1", "--claim", "d", "--by", "r", "--area", "design"])
        .assert()
        .success()
        .stderr(contains("warning: acme/widgets is public").and(contains("names its local record")));
    assert!(g.fake.issue(2).body.contains("Record: fix the parser"), "{}", g.fake.issue(2).body);
    // Only a GitHub finding about a LOCAL record publishes a local title.
    g.ok(&["record", "add", "--project", "1", "--title", "look", "--area", "design"]);
    for args in [
        ["finding", "raise", "--record", "#3", "--claim", "e", "--by", "r"],
        ["finding", "raise", "--record", "1", "--claim", "f", "--by", "r"],
    ] {
        g.fl()
            .args(args)
            .assert()
            .success()
            .stderr(contains("names its local record").not());
    }
}

// Routing spec decision 21: a finding about a local record in a sensitive
// area never reaches a public repository, whatever its own area.
#[test]
fn a_finding_about_a_sensitive_local_record_is_refused_on_a_public_repository() {
    let g = world(BOUND);
    g.routed();
    g.fake.state().repos[0].visibility = "public".into();
    g.ok(&[
        "record", "add", "--project", "1", "--title", "the key leaks", "--area", "security",
        "--tier", "local",
    ]);
    g.fl()
        .args(["finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area", "design"])
        .assert()
        .failure()
        .stderr(
            contains("about a record in a sensitive area")
                .and(contains("--tier local"))
                .and(contains("names its local record").not()),
        );
    assert_eq!(g.fake.issue_count(), 0, "nothing created on GitHub");
}

// Routing spec §1.2: a finding is a routed create too.
#[test]
fn a_finding_on_the_authoring_machine_needs_the_map_exported() {
    let g = world("");
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "t", "--area", "code"]);
    g.ok(&["manifest", "export", "--project", "1"]);
    g.ok(&["routing", "set", "--project", "1", "ops", "local"]);
    g.fl()
        .args(["finding", "raise", "--record", "1", "--claim", "c", "--by", "r"])
        .assert()
        .failure()
        .stderr(contains("changed since the manifest at"));
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --test routing`
Expected: FAIL — `--tier` is not an argument of `record add`, and `--area`/`--tier` not of `finding raise`.

- [ ] **Step 3: Implement**

In `crates/cli/src/cmd/record.rs`, add `use fl_core::routing::Tier;` and give `Add`, after `area`:

```rust
        /// The tier, over the one the area routes to (routing spec §2.1).
        #[arg(long, value_parser = crate::cmd::routing::parse_tier)]
        tier: Option<Tier>,
```

and replace Task 10's `let id = match (ctx.tiers, &area) { … };` with:

```rust
            let id = match ctx.tiers {
                None => {
                    if area.is_some() {
                        return Err(crate::cmd::routing::not_routed("--area"));
                    }
                    if tier.is_some() {
                        return Err(crate::cmd::routing::not_routed("--tier"));
                    }
                    ctx.tracker.add_record(&p, &title)?
                }
                Some(t) => {
                    crate::cmd::manifest::ensure_routing_current(store, &p)?;
                    let at = t.router.place_record(&p, area.as_deref(), tier)?;
                    t.router.add_record_at(&p, &title, &at)?
                }
            };
```

(binding `tier` in the `Cmd::Add { project, title, area, tier }` pattern).

In `crates/cli/src/cmd/finding.rs`, add `use crate::tiers::Tiers; use fl_core::FindingPlacement; use fl_core::routing::Tier;`. Give `Raise`, after `security`:

```rust
        /// The finding's area (routing spec §1.1). Without it, the
        /// record's.
        #[arg(long)]
        area: Option<String>,
        /// The tier, over the one the area routes to (routing spec §2.1).
        #[arg(long, value_parser = crate::cmd::routing::parse_tier)]
        tier: Option<Tier>,
```

In `run`, the `Raise` arm binds `area` and `tier`, and replaces `let mut f = Finding::raise(rec.project, r, &by, &claim); f.security = security; let id = ctx.tracker.add_finding(f)?;` with:

```rust
            let project = rec.project.clone();
            let mut f = Finding::raise(rec.project, r, &by, &claim);
            f.security = security;
            f.area = area;
            let id = match ctx.tiers {
                None => {
                    if f.area.is_some() {
                        return Err(crate::cmd::routing::not_routed("--area"));
                    }
                    if tier.is_some() {
                        return Err(crate::cmd::routing::not_routed("--tier"));
                    }
                    ctx.tracker.add_finding(f)?
                }
                Some(t) => {
                    crate::cmd::manifest::ensure_routing_current(store, &project)?;
                    let at = t.router.place_finding(&f, tier)?;
                    if at.inherited() {
                        eprintln!("note: area: {}, from its record", at.at().area());
                    }
                    warn_disclosure(t, &at)?;
                    t.router.add_finding_at(f, &at)?
                }
            };
```

and add, after `explain`:

```rust
/// Routing spec §2.5: a GitHub finding about a local record publishes the
/// record's title and IRI. On a repository that is not private, say so
/// before it is written. (About a record in a sensitive area, the router
/// has already refused: decision 21.) ⚠ A visibility that cannot be read
/// refuses: an unknown visibility is not private.
fn warn_disclosure(t: &Tiers<'_>, at: &FindingPlacement) -> Result<()> {
    if at.at().tier() != Tier::Github || at.record().tier != Tier::Local {
        return Ok(());
    }
    let gh = t.github.open()?;
    let visibility = gh.visibility()?;
    if visibility != "private" {
        eprintln!(
            "warning: {} is {visibility}: this finding's issue names its local record, {:?}, \
             and the record's IRI, and anyone who can read the repository will see them",
            gh.repo().full_name,
            at.record().title
        );
    }
    Ok(())
}
```

In `docs/routing.md`, append:

```markdown
## Making items

In a routed project every new record and finding has an area. `fl record add --area <area>`
routes a record by its area; `--tier local` or `--tier github` puts it in that tier instead, and
the area is recorded either way. `fl finding raise` takes the same two options. With no `--area`,
a finding takes its record's area, and fl says so: `note: area: code, from its record`. A finding
whose record has no area — one made before the project was routed — needs `--area`. An area the
map does not declare is refused, naming the declared ones; so are `--area` and `--tier` in a
project without a routing map.

A finding and its record may be in different tiers. A finding on GitHub about a local record
shows the record's title and id as text; on a repository that is not private, fl warns before it
publishes them — and refuses, saying to use `--tier local`, when the record is in a sensitive
area: nothing about an item in a sensitive area reaches a repository that is not private.

A finding made in a sensitive area is a security finding. When the map would send a security
finding, or any item of a sensitive area, to a repository that is not private, fl refuses and
says to use `--tier local`: it never moves an item to the local tier by itself.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli --test routing`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Each run with `cargo test -p fl-cli --test routing -- <name>`:

1. `record add` passes `--tier`: pass `None` → `a_tier_given_puts_a_record_there_with_its_area` red.
2. `record add`'s unrouted `--tier` refusal: delete it → `tier_or_area_in_a_project_with_no_routing_map_is_refused` red.
3. `finding raise`'s unrouted `--area` and `--tier` refusals: delete each → the same test red.
4. `finding raise` passes the area: drop `f.area = area;` → `a_finding_takes_its_records_area_and_says_so` red (the second raise inherits `design`).
5. The note's guard: `if true` → the same test red (its last assertion); `if false` → the same test red (its first).
6. `finding raise`'s routing currency: delete it → `a_finding_on_the_authoring_machine_needs_the_map_exported` red.
7. `warn_disclosure` is called: delete the call → `a_github_finding_about_a_local_record_warns_on_a_public_repository` red.
8. Its tier conjunct: drop `at.at().tier() != Tier::Github ||` → `a_github_finding_about_a_local_record_warns_on_a_public_repository` red (the local finding about record 1 warns).
9. Its record conjunct: drop `|| at.record().tier != Tier::Local` → the same test red (the GitHub finding about `#3` warns).
10. Its visibility comparison: warn always → the same test red (the private raise).
10a. Decision 21 reaches the CLI: the refusal is the router's (Task 7, mutation 20 there); `a_finding_about_a_sensitive_local_record_is_refused_on_a_public_repository` is red under that mutation too (the finding is warned about and created).
11. The routed arm places before it writes: replace it with `ctx.tracker.add_finding(f)?` → `a_finding_takes_its_records_area_and_says_so` red (no note); the sensitive refusal still comes from the router, so the sensitivity test stays green — the placement is what the note and the warning need.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/cmd/record.rs crates/cli/src/cmd/finding.rs crates/cli/tests/routing.rs docs/routing.md
git commit -m "feat(cli): --tier, a finding's area, sensitivity and disclosure on create

record add and finding raise take --tier; finding raise takes --area, or
its record's area, and says which. A routed create of either checks that
the routing map is current. A sensitive area or security finding the map
sends to a public repository is refused before anything is written,
naming --tier local, and so is a finding about a local record in a
sensitive area; a GitHub finding about any other local record warns on a
repository that is not private. --area and --tier in an unrouted store are
refused. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 13: Lists across both tiers, `--tier`, and `fl finding list --record`

`fl record list` and `fl finding list` read both tiers, merge them, and show each item's tier in a column that appears only in a routed store (routing spec §2.4, plan ruling 17); `--tier local|github` reads one. A tier that cannot be read makes the merged list an error naming `--tier local`, never a partial list. `fl finding list --record <id>` lists one record's findings, from both tiers (plan ruling 19). The withdrawal counts sum both tiers, or count the one `--tier` names and say so (plan ruling 13).

**Blast radius:** `fl record list` and `fl finding list` in a routed store gain a column; unrouted output is byte for byte as before. `finding list`'s `--project` becomes optional, with exactly one of `--project` and `--record` required (clap refuses both or neither).

**Files:**
- Modify: `crates/cli/src/cmd/record.rs` (`List`)
- Modify: `crates/cli/src/cmd/finding.rs` (`List`, `refs`)
- Modify: `crates/cli/tests/routing.rs`
- Modify: `docs/routing.md`

**Interfaces:**
- Consumes: `TieredTracker::{records, findings, withdrawals_in}` (Task 8); `Ctx::{show_item, resolve_item}`; `cmd::routing::{parse_tier, not_routed}`.
- Produces: `fl record list --project <p> [--tier <t>]` printing `<handle>\t<tier>\t<state>\t<title>` in a routed store; `fl finding list (--project <p> | --record <r>) [--state <s>] [--tier <t>]` printing `<handle>\t<tier>\t<state>\t<raised_by>\t<claim>` in a routed store, then `<actor>\twithdrawn: <n>[ (<tier> tier)]`. Unique phrases: `rather than show part of it`, `(local tier)`.

- [ ] **Step 1: Write the failing tests**

In `crates/cli/tests/routing.rs`, add:

```rust
// Routing spec §2.4: both tiers, each item's tier in a column; `--tier`
// reads one.
#[test]
fn a_merged_list_shows_each_items_tier_and_the_tier_flag_reads_one() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "l", "--area", "code"]);
    g.ok(&["record", "add", "--project", "1", "--title", "g", "--area", "design"]);
    assert_eq!(
        g.ok(&["record", "list", "--project", "1"]),
        "1\tlocal\ttodo\tl\n#1\tgithub\ttodo\tg\n"
    );
    assert_eq!(
        g.ok(&["record", "list", "--project", "1", "--tier", "local"]),
        "1\tlocal\ttodo\tl\n"
    );
    assert_eq!(
        g.ok(&["record", "list", "--project", "1", "--tier", "github"]),
        "#1\tgithub\ttodo\tg\n"
    );
}

#[test]
fn an_unrouted_list_has_no_tier_column_and_refuses_tier() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.ok(&["record", "add", "--project", "1", "--title", "t"]);
    assert_eq!(g.ok(&["record", "list", "--project", "1"]), "1\ttodo\tt\n");
    for list in ["record", "finding"] {
        g.fl()
            .args([list, "list", "--project", "1", "--tier", "local"])
            .assert()
            .failure()
            .stderr(contains("`--tier` needs a routing map"));
    }
}

// Routing spec §2.4: "a list that cannot see its whole population fails".
#[test]
fn a_merged_list_with_github_down_is_refused_naming_tier_local() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "l", "--area", "code"]);
    g.fake.state().down = true;
    for list in [["record", "list", "--project", "1"], ["finding", "list", "--project", "1"]] {
        g.fl()
            .args(list)
            .assert()
            .failure()
            .stdout("")
            .stderr(contains("rather than show part of it").and(contains("--tier local")));
    }
    assert_eq!(
        g.ok(&["record", "list", "--project", "1", "--tier", "local"]),
        "1\tlocal\ttodo\tl\n"
    );
}

#[test]
fn an_unbound_machine_lists_its_local_tier_with_tier_local() {
    let g = world("");
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "l", "--area", "code"]);
    g.fl()
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("binds no GitHub repository for the project").and(contains("--tier local")));
    assert_eq!(
        g.ok(&["record", "list", "--project", "1", "--tier", "local"]),
        "1\tlocal\ttodo\tl\n"
    );
}

// Routing spec §2.4: one record's findings, from both tiers.
#[test]
fn finding_list_record_lists_one_records_findings_from_both_tiers() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "a", "--area", "code"]);
    g.ok(&["record", "add", "--project", "1", "--title", "b", "--area", "code"]);
    g.ok(&["finding", "raise", "--record", "1", "--claim", "here", "--by", "rev"]);
    g.ok(&[
        "finding", "raise", "--record", "1", "--claim", "there", "--by", "rev", "--area",
        "design",
    ]);
    g.ok(&["finding", "raise", "--record", "2", "--claim", "other", "--by", "rev"]);
    assert_eq!(
        g.ok(&["finding", "list", "--record", "1"]),
        "1\tlocal\traised\trev\there\n#1\tgithub\traised\trev\tthere\n"
    );
    g.fl()
        .args(["finding", "list", "--record", "1", "--project", "1"])
        .assert()
        .failure();
}

#[test]
fn finding_list_record_works_in_an_unrouted_project_too() {
    let g = world("");
    g.ok(&["project", "add", "."]);
    g.ok(&["record", "add", "--project", "1", "--title", "a"]);
    g.ok(&["record", "add", "--project", "1", "--title", "b"]);
    g.ok(&["finding", "raise", "--record", "1", "--claim", "x", "--by", "rev"]);
    g.ok(&["finding", "raise", "--record", "2", "--claim", "y", "--by", "rev"]);
    assert_eq!(g.ok(&["finding", "list", "--record", "2"]), "2\traised\trev\ty\n");
}

// Routing spec §2.4: the withdrawal counts sum both tiers.
#[test]
fn withdrawal_counts_sum_both_tiers_and_name_the_tier_when_narrowed() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "a", "--area", "code"]);
    g.ok(&["finding", "raise", "--record", "1", "--claim", "x", "--by", "hasty"]);
    g.ok(&[
        "finding", "raise", "--record", "1", "--claim", "y", "--by", "hasty", "--area", "design",
    ]);
    g.ok(&["finding", "withdraw", "1", "--reason", "no"]);
    g.ok(&["finding", "withdraw", "#1", "--reason", "no"]);
    assert!(
        g.ok(&["finding", "list", "--project", "1"]).ends_with("hasty\twithdrawn: 2\n"),
        "summed over both tiers"
    );
    assert!(
        g.ok(&["finding", "list", "--project", "1", "--tier", "local"])
            .ends_with("hasty\twithdrawn: 1 (local tier)\n")
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --test routing`
Expected: FAIL — no tier column, and `--tier` and `--record` are not arguments of the lists.

- [ ] **Step 3: Implement**

In `crates/cli/src/cmd/record.rs`, give `List` the field

```rust
        /// One tier only, in a routed project (routing spec §2.4).
        #[arg(long, value_parser = crate::cmd::routing::parse_tier)]
        tier: Option<Tier>,
```

(`Cmd::List { project, .. } => vec![project]` in `refs`), and replace the `Cmd::List` arm's loop with:

```rust
            match ctx.tiers {
                None => {
                    if tier.is_some() {
                        return Err(crate::cmd::routing::not_routed("--tier"));
                    }
                    for r in ctx.tracker.list_records(&p)? {
                        println!(
                            "{}\t{}\t{}",
                            ctx.show_item(Kind::Record, r.id.iri())?,
                            r.state.as_wire(),
                            r.title
                        );
                    }
                }
                // ⚠ The whole list or an error: `records` refuses when a
                // tier it must read cannot be read (routing spec §2.4).
                Some(t) => {
                    for (in_tier, r) in t.router.records(&p, tier)? {
                        println!(
                            "{}\t{}\t{}\t{}",
                            ctx.show_item(Kind::Record, r.id.iri())?,
                            in_tier.as_wire(),
                            r.state.as_wire(),
                            r.title
                        );
                    }
                }
            }
```

In `crates/cli/src/cmd/finding.rs`, replace `List` with:

```rust
    /// A project's findings, or one record's (routing spec §2.4).
    List {
        #[arg(long, required_unless_present = "record", conflicts_with = "record")]
        project: Option<Ref>,
        /// One record's findings, from both tiers in a routed project.
        #[arg(long)]
        record: Option<Ref>,
        #[arg(long)]
        state: Option<String>,
        /// One tier only, in a routed project.
        #[arg(long, value_parser = crate::cmd::routing::parse_tier)]
        tier: Option<Tier>,
    },
```

In `refs`, `Cmd::List { project, record, .. } => project.iter().chain(record.iter()).collect(),`. Replace the `Cmd::List` arm of `run` with:

```rust
        Cmd::List {
            project,
            record,
            state,
            tier,
        } => {
            let want = match state.as_deref() {
                None => None,
                Some(s) => Some(FindingState::from_wire(s).ok_or_else(|| {
                    anyhow::anyhow!(
                        "`{s}` is not a finding state. Valid states are: {}.",
                        FindingState::wire_values()
                    )
                })?),
            };
            let (p, of) = match (&project, &record) {
                (Some(pr), None) => (
                    ProjectId(refs::resolve(ctx.handles, store.label(), Kind::Project, pr)?),
                    None,
                ),
                (None, Some(r)) => {
                    let id = RecordId(ctx.resolve_item(Kind::Record, r)?);
                    let Some(rec) = ctx.tracker.get_record(&id)? else {
                        bail!(
                            "`{r}` is not a record in {}. Use `fl record list --project \
                             <project>` to see records that exist.",
                            ctx.tracker_label
                        );
                    };
                    (rec.project.clone(), Some(rec))
                }
                _ => unreachable!("clap requires exactly one of --project and --record"),
            };
            if ctx.tiers.is_none() && tier.is_some() {
                return Err(crate::cmd::routing::not_routed("--tier"));
            }
            // ⚠ The whole list or an error: `findings` refuses when a tier
            // it must read cannot be read (routing spec §2.4).
            let listed: Vec<(Option<Tier>, Finding)> = match ctx.tiers {
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
                    .map(|(in_tier, f)| (Some(in_tier), f))
                    .collect(),
            };
            let named = |f: &Finding| {
                of.as_ref()
                    .is_none_or(|r| f.record == r.id || r.also_known_as.contains(f.record.iri()))
            };
            let mut raisers: BTreeSet<String> = Default::default();
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
                        f.raised_by,
                        f.claim
                    ),
                    None => println!(
                        "{shown}\t{}\t{}\t{}",
                        f.state.as_wire(),
                        f.raised_by,
                        f.claim
                    ),
                }
                raisers.insert(f.raised_by.clone());
            }
            // ⚠ Decision 27's cost, printed where it can be seen — over both
            // tiers, or the one `--tier` names, and then it says which.
            for actor in raisers {
                let n = match ctx.tiers {
                    Some(t) => t.router.withdrawals_in(&actor, tier)?,
                    None => ctx.tracker.withdrawals_by(&actor)?,
                };
                if n > 0 {
                    match tier {
                        Some(t) => println!("{actor}\twithdrawn: {n} ({} tier)", t.as_wire()),
                        None => println!("{actor}\twithdrawn: {n}"),
                    }
                }
            }
        }
```

In `docs/routing.md`, append:

```markdown
## Lists

In a routed project `fl record list` and `fl finding list` read both tiers and show each item's
tier in a second column (`1\tlocal\ttodo\t…`, `#1\tgithub\ttodo\t…`); `--tier local` or `--tier
github` reads one. When a tier cannot be read — GitHub is down, or this machine binds no
repository — the list is refused rather than shown in part, and the refusal suggests `--tier
local`. `fl finding list --record <id>` lists one record's findings, from both tiers; it works in
any project. The withdrawal counts under a finding list sum both tiers; with `--tier`, they count
that tier, and say so.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli`
Expected: PASS — every existing list test unchanged.

- [ ] **Step 5: Mutation checks**

Each run with `cargo test -p fl-cli --test routing -- <name>`:

1. `record list` reads both tiers: pass `Some(Tier::Local)` for `tier` → `a_merged_list_shows_each_items_tier_and_the_tier_flag_reads_one` red.
2. The tier column: drop `in_tier.as_wire()` from `record list`'s routed line → the same test red; from `finding list`'s → `finding_list_record_lists_one_records_findings_from_both_tiers` red.
3. The unrouted `--tier` refusals: delete `record list`'s, then `finding list`'s → `an_unrouted_list_has_no_tier_column_and_refuses_tier` red each time.
4. `--record` filters by record: make `named` always `true` → `finding_list_record_lists_one_records_findings_from_both_tiers` red, and `finding_list_record_works_in_an_unrouted_project_too` red.
5. Its alias conjunct: drop `|| r.also_known_as.contains(…)` → not observable here: the stores write a finding's record as the primary. Not a guard a test reaches.
6. The withdrawal sum: use `Some(Tier::Local)` for `withdrawals_in` → `withdrawal_counts_sum_both_tiers_and_name_the_tier_when_narrowed` red.
7. The narrowed suffix: print the plain line always → the same test red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/cmd/record.rs crates/cli/src/cmd/finding.rs crates/cli/tests/routing.rs docs/routing.md
git commit -m "feat(cli): lists across both tiers, --tier, and finding list --record

In a routed store record and finding lists read both tiers and show each
item's tier; --tier reads one; a tier that cannot be read refuses the
list, naming --tier local. finding list --record lists one record's
findings, in any project. Withdrawal counts sum both tiers, or name the
tier asked for. Unrouted output is unchanged. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 14: `fl routing remove`, and clearing a sensitivity, refused while any item names the area

Decision 11 and §1.2: removing an area is refused while any item in either tier names it; the refusal gives the count and lists up to ten of the items; GitHub items are found by their blocks, not their labels, so an item that lost its label is not missed (Task 6); a tier that cannot be read refuses the removal too (plan ruling 13). fl can check only this machine's local tier, and says so (decision 15). Decision 22: `fl routing set … --not-sensitive` clears an area's sensitivity, and is refused by the same check — one function, `refuse_while_named`, serves both. The doc gains its "Removing an area" and "Limits" sections. An end-to-end test runs the re-review's scenario: an area removed where the project is authored, while another machine's record still names it, keeps that record protected (decision 22, Task 7).

**Blast radius:** `fl routing` gains a subcommand that needs the tracker (both tiers), and `set --not-sensitive` needs it too — so in a store not routed yet, `set --not-sensitive` with a binding opens GitHub at the start, as every tracker command of an unrouted GitHub project does. Nothing else changes.

**Files:**
- Modify: `crates/cli/src/cmd/routing.rs` (`Cmd::Remove`, `Cmd::Set`'s `--not-sensitive`, `needs_tracker`, `run`, `refuse_while_named`, `named`)
- Modify: `crates/cli/tests/routing.rs`
- Modify: `docs/routing.md`

**Interfaces:**
- Consumes: `TieredTracker::items_naming_area` (Task 8); `RoutingMap::without` (Task 3); `RedbStore::set_routes`; `Ctx.tiers`.
- Produces: `fl routing remove --project <p> <area>`, printing `removed\t<area>`; `fl routing set … --not-sensitive`; `fn refuse_while_named(ctx: &Ctx<'_>, p: &ProjectId, area: &str, refused: &str) -> Result<()>`. Unique phrases: `is still named by`, `keep it as history`, `Its sensitivity is not cleared`.

- [ ] **Step 1: Write the failing tests**

In `crates/cli/tests/routing.rs`, add:

```rust
// Routing spec decision 11: refused while any item in either tier names the
// area, with the count and the items.
#[test]
fn removing_an_area_items_still_name_is_refused_with_the_count_and_the_items() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "l", "--area", "code"]);
    g.ok(&[
        "record", "add", "--project", "1", "--title", "g", "--area", "code", "--tier", "github",
    ]);
    g.fl()
        .args(["routing", "remove", "--project", "1", "code"])
        .assert()
        .failure()
        .stderr(contains("is still named by 2 item(s)").and(contains("1, #1")));
    assert!(g.ok(&["routing", "show", "--project", "1"]).contains("code\tlocal"));
}

// Routing spec §1.2: by block, not label.
#[test]
fn an_item_that_lost_its_labels_still_blocks_the_removal() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["record", "add", "--project", "1", "--title", "g", "--area", "design"]);
    g.fake.web_edit(1, |i| i.labels.clear());
    g.fl()
        .args(["routing", "remove", "--project", "1", "design"])
        .assert()
        .failure()
        .stderr(contains("is still named by 1 item(s)").and(contains("#1")));
}

#[test]
fn removing_an_area_no_item_names_drops_it_for_new_items() {
    let g = world(BOUND);
    g.routed();
    g.fl()
        .args(["routing", "remove", "--project", "1", "product"])
        .assert()
        .success()
        .stdout("removed\tproduct\n")
        .stderr(contains("keep it as history"));
    assert!(!g.ok(&["routing", "show", "--project", "1"]).contains("product"));
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t", "--area", "product"])
        .assert()
        .failure()
        .stderr(contains("`product` is not an area this project declares"));
}

// Routing spec §1.2: a tier that cannot be read refuses the removal.
#[test]
fn a_removal_is_refused_when_a_tier_cannot_be_read() {
    let g = world(BOUND);
    g.routed();
    g.fake.state().down = true;
    g.fl()
        .args(["routing", "remove", "--project", "1", "product"])
        .assert()
        .failure();
    let unbound = world("");
    unbound.routed();
    unbound
        .fl()
        .args(["routing", "remove", "--project", "1", "product"])
        .assert()
        .failure()
        .stderr(contains("binds no GitHub repository for the project"));
    g.fake.state().down = false;
    assert!(g.ok(&["routing", "show", "--project", "1"]).contains("product"));
}

#[test]
fn removing_an_area_the_map_does_not_declare_is_refused() {
    let g = world("");
    g.routed();
    g.fl()
        .args(["routing", "remove", "--project", "1", "ops"])
        .assert()
        .failure()
        .stderr(contains("`ops` is not an area project 1 declares"));
}

// Routing spec decision 22: clearing a sensitivity is refused while any
// item in either tier names the area — the same check as a removal — and
// writes nothing.
#[test]
fn clearing_a_sensitivity_items_still_name_is_refused_and_writes_nothing() {
    let g = world(BOUND);
    g.routed();
    g.ok(&[
        "record", "add", "--project", "1", "--title", "the key leaks", "--area", "security",
        "--tier", "local",
    ]);
    g.fl()
        .args(["routing", "set", "--project", "1", "security", "local", "--not-sensitive"])
        .assert()
        .failure()
        .stderr(
            contains("is still named by 1 item(s)")
                .and(contains("Its sensitivity is not cleared")),
        );
    assert!(
        g.ok(&["routing", "show", "--project", "1"]).contains("security\tgithub\tsensitive"),
        "nothing written"
    );
    // Only an area that is sensitive asks: clearing `code`, which is not,
    // while an item names it, changes nothing and is not refused.
    g.ok(&["record", "add", "--project", "1", "--title", "t", "--area", "code"]);
    assert_eq!(
        g.ok(&["routing", "set", "--project", "1", "code", "local", "--not-sensitive"]),
        "code\tlocal\t-\n"
    );
}

#[test]
fn clearing_a_sensitivity_no_item_names_works() {
    let g = world(BOUND);
    g.routed();
    assert_eq!(
        g.ok(&["routing", "set", "--project", "1", "security", "github", "--not-sensitive"]),
        "security\tgithub\t-\n"
    );
    g.fl()
        .args([
            "routing", "set", "--project", "1", "security", "github", "--sensitive",
            "--not-sensitive",
        ])
        .assert()
        .failure();
}

// Routing spec decision 22, the re-review's scenario: the authoring machine
// removes a sensitive area that only another machine's records name; after
// the re-import those records still count as sensitive — fail closed.
#[test]
fn a_record_whose_area_was_removed_elsewhere_stays_protected() {
    let g = world(BOUND);
    g.routed();
    g.ok(&["manifest", "export", "--project", "1"]);
    let other = tempfile::tempdir().unwrap();
    g.configure(other.path(), BOUND);
    let on_other = |args: &[&str]| g.fl_at(other.path()).args(args).assert();
    on_other(&["manifest", "import"]).success();
    on_other(&[
        "record", "add", "--project", "1", "--title", "the key leaks", "--area", "security",
        "--tier", "local",
    ])
    .success();
    g.ok(&["routing", "remove", "--project", "1", "security"]);
    g.ok(&["manifest", "export", "--project", "1"]);
    on_other(&["manifest", "import"]).success();
    g.fake.state().repos[0].visibility = "public".into();
    on_other(&["finding", "raise", "--record", "1", "--claim", "c", "--by", "r", "--area", "design"])
        .failure()
        .stderr(contains("--tier local"));
    assert_eq!(g.fake.issue_count(), 0, "nothing published");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --test routing`
Expected: FAIL — `fl routing remove` is not a command, and `--not-sensitive` not an argument of `set`.

- [ ] **Step 3: Implement**

In `crates/cli/src/cmd/routing.rs`, `Set`'s `sensitive` field becomes, with a new field after it:

```rust
        /// Items made in this area are security items, which the map never
        /// sends to a repository that is not private. Without this flag or
        /// `--not-sensitive`, an area keeps its sensitivity.
        #[arg(long, conflicts_with = "not_sensitive")]
        sensitive: bool,
        /// Clear the area's sensitivity. Refused while any item in either
        /// tier names the area.
        #[arg(long)]
        not_sensitive: bool,
```

Add to `Cmd`, after `Set`:

```rust
    /// Remove an area. Refused while any item in either tier names it.
    Remove {
        #[arg(long)]
        project: Ref,
        area: String,
    },
```

`refs` gains `Cmd::Remove { project, .. }` in its one arm (`Cmd::Set { project, .. } | Cmd::Remove { project, .. } | Cmd::Show { project } => vec![project]`), and `needs_tracker` becomes:

```rust
        match self {
            // Every item that names the area, in both tiers (routing spec
            // §1.2, decision 22).
            Cmd::Set { not_sensitive, .. } => *not_sensitive,
            Cmd::Show { .. } => false,
            Cmd::Remove { .. } => true,
        }
```

In `run`, the `Set` arm binds `not_sensitive` too, and replaces Task 9's `let asked = …; let (map, first) = routing::after_set(…);` with:

```rust
            let current = store.routes(&p)?;
            // Routing spec decision 22: neither flag keeps the area's
            // sensitivity; clearing it is refused while any item names it.
            let asked = match (sensitive, not_sensitive) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
            let clears = asked == Some(false)
                && current
                    .as_ref()
                    .and_then(|m| m.route(&area))
                    .is_some_and(|r| r.sensitive);
            if clears {
                refuse_while_named(
                    ctx,
                    &p,
                    &area,
                    "Its sensitivity is not cleared: an item made in a sensitive area stays \
                     protected",
                )?;
            }
            let (map, first) = routing::after_set(current.as_ref(), &area, tier, asked);
```

(On a project's first `set` there is no map yet, so no item can name an area, and nothing is checked.)

Add the arm:

```rust
        Cmd::Remove { project, area } => {
            let p = project_of(ctx, &project)?;
            let shown = refs::show(store, Kind::Project, p.iri())?;
            let Some(map) = store.routes(&p)? else {
                bail!("project {shown} has no routing map, so it declares no area to remove");
            };
            if map.route(&area).is_none() {
                bail!(
                    "`{area}` is not an area project {shown} declares. The declared areas: {}",
                    map.declared().join(", ")
                );
            }
            refuse_while_named(
                ctx,
                &p,
                &area,
                "It is not removed: an item keeps its area for life; remove the area once none \
                 names it",
            )?;
            store.set_routes(&p, &map.without(&area))?;
            println!("removed\t{area}");
            eprintln!(
                "note: another machine's local items that name `{area}` keep it as history; only \
                 new items are refused it"
            );
        }
```

and after `project_of`:

```rust
/// Routing spec decision 11, and decision 22's clearing of a sensitivity:
/// refused while any item in either tier names `area`, with the count and up
/// to ten of the items. ⚠ Both tiers, GitHub by its blocks; a tier that
/// cannot be read is an error, never "no item names it".
fn refuse_while_named(ctx: &Ctx<'_>, p: &ProjectId, area: &str, refused: &str) -> Result<()> {
    let shown = refs::show(ctx.store, Kind::Project, p.iri())?;
    let t = ctx
        .tiers
        .expect("a store that holds a routing map is routed, so it has tiers");
    let items = t.router.items_naming_area(p, area)?;
    if items.is_empty() {
        return Ok(());
    }
    let some: Vec<String> = items
        .iter()
        .take(10)
        .map(|(tier, kind, id)| named(ctx, *tier, *kind, id))
        .collect::<Result<_>>()?;
    let more = if items.len() > 10 { ", …" } else { "" };
    bail!(
        "`{area}` is still named by {} item(s) of project {shown}: {}{more}. {refused}. fl reads \
         only this machine's local tier: another machine's local items may name it too",
        items.len(),
        some.join(", ")
    )
}
```

and after `print_route`:

```rust
/// How a refusal names an item: a GitHub item by its issue number, read
/// from its URL — an item that lost its labels has no handle GitHub's
/// lookup would give — and a local one by its handle.
fn named(ctx: &Ctx<'_>, tier: Tier, kind: Kind, id: &Iri) -> Result<String> {
    Ok(match tier {
        Tier::Github => fl_github::meta::parse_issue_url(id)
            .map(|(_, n)| format!("#{n}"))
            .unwrap_or_else(|| id.to_string()),
        Tier::Local => refs::show(ctx.store, kind, id)?,
    })
}
```

In `docs/routing.md`, append:

```markdown
## Removing an area

`fl routing remove --project <project> <area>` removes an area from the map. It is refused while
any item in either tier names the area; the refusal gives the count and lists up to ten of the
items. fl finds GitHub items by the block in each issue, not by the area label, so an item that
lost its label is still found. A tier that cannot be read refuses the removal. fl can read only
this machine's local tier: an item in another machine's local store keeps the area as history,
and only new items are refused it. Such an item counts as sensitive from then on: fl cannot know
what the removed area was, so a finding about it never reaches a repository that is not private.
fl never deletes an area's label from GitHub. There is no rename: add the new area and remove the
old one.

`fl routing set --project <project> <area> <tier> --not-sensitive` clears an area's sensitivity.
It is refused, by the same check and with the same list, while any item in either tier names the
area.

## Limits

* A machine with no binding for the project lists with `--tier local`: a list of both tiers needs
  both.
* A routed project has a store to itself: a store holding another project cannot be routed.
* An older fl refuses a routed project's manifest (format 3) and its store (format 5) as newer
  formats. On a machine whose store is already bound to GitHub and that runs an older fl, the
  project behaves as an ordinary GitHub-bound project, and makes GitHub items with no area.
* A routed project keeps its ledger local: the GitHub ledger is not available to it yet.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli --test routing`
Expected: PASS.

- [ ] **Step 5: Mutation checks**

Each run with `cargo test -p fl-cli --test routing -- <name>`:

1. The refusal: replace `if items.is_empty()` with `if true` → `removing_an_area_items_still_name_is_refused_with_the_count_and_the_items` red.
2. The count names every item, not the ten shown: print `some.len()` → not distinguishable with fewer than ten items; not a guard a test of this size tells apart.
3. The cap of ten: `take(10)` → `take(1)` → `removing_an_area_items_still_name…` red (`1, #1`).
4. `named`'s GitHub arm reads the number from the URL: call `ctx.show_item` instead → `an_item_that_lost_its_labels_still_blocks_the_removal` red (the lookup of an unlabelled issue fails).
5. The scan's error propagates: replace `items_naming_area(p, area)?` with `.unwrap_or_default()` → `a_removal_is_refused_when_a_tier_cannot_be_read` red.
6. The undeclared refusal: delete it → `removing_an_area_the_map_does_not_declare_is_refused` red (it would print `removed`).
7. `needs_tracker` for `Remove`: make it `false` → `removing_an_area_items_still_name_is_refused_with_the_count_and_the_items` red (the binding is not read, so GitHub cannot be read and the refusal is the tier's, not the count).
8. `set --not-sensitive` asks the same check: delete the `if clears { refuse_while_named(…)?; }` → `clearing_a_sensitivity_items_still_name_is_refused_and_writes_nothing` red.
9. Only a sensitive area asks: drop the "was sensitive" conjunct of `clears` → the same test red (clearing `code` is refused).
10. `needs_tracker` for `set --not-sensitive`: make it `false` → the same test red (the binding is not read, so GitHub cannot be read and the refusal is the tier's, not the count).
11. The flags: make `asked` `Some(true)` for `--not-sensitive` → `clearing_a_sensitivity_no_item_names_works` red.
12. The map is written without the area: write `map` unchanged → `removing_an_area_no_item_names_drops_it_for_new_items` red.

- [ ] **Step 6: Run the trio and commit**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
git add crates/cli/src/cmd/routing.rs crates/cli/tests/routing.rs docs/routing.md
git commit -m "feat(cli): fl routing remove, refused while any item names the area

The refusal counts the items in both tiers and lists up to ten; GitHub
items are found by their blocks, so one that lost its label still counts;
a tier that cannot be read refuses the removal. set --not-sensitive
clears an area's sensitivity, refused by the same check. A record whose
area was removed elsewhere stays protected end to end. docs/routing.md
gains removal and its limits. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

## After the last task

- [ ] `git status --porcelain` prints nothing: every change on the branch is committed.
- [ ] Run the trio once more on the whole branch: `cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace`.
- [ ] An unrouted project is untouched: `git diff origin/main...HEAD -- crates/cli/tests/github.rs crates/cli/tests/findings.rs crates/cli/tests/cli.rs crates/cli/tests/manifest.rs crates/cli/tests/ledger.rs` prints nothing — no existing black-box assertion was changed.
- [ ] No test reaches the network: `grep -rln 'api.github.com\|DEFAULT_API' crates --include='*.rs'` lists the same files as on `main` (`git diff origin/main...HEAD --stat` adds none of them).
- [ ] No plan label crept into code: `git diff origin/main...HEAD -U0 -- '*.rs' | grep '^+' | grep -iE 'task [0-9]|plan (a|b)\b|plan ruling|review focus'` prints nothing.
- [ ] No machine name in what this branch adds: `git diff origin/main...HEAD -U0 | grep '^+' | grep -nF "$HOME"; git diff origin/main...HEAD -U0 | grep '^+' | grep -nwF "$(hostname)"` print nothing.
- [ ] Follow `WORKFLOW.md`: `superpowers:requesting-code-review` on the whole branch, then a pull request against `main`. The pull request names the plan rulings and spec defects of this plan, says the spec is at rev 2.2 (owner decisions 20–22 and the seven defects amended, 2026-10-06), says that plan B (escalation) is written after this merges, and repeats "Plan B — what this plan leaves".

## Spec coverage (plan A's share)

| spec | where |
|---|---|
| decision 2 (routing by area; a flag overrides) | Tasks 7, 10, 12 |
| decision 5 (references cross tiers) | Tasks 5, 7, 8 (`tiered_evidence`), 12 |
| decision 6 (areas and map in the committed manifest; an undeclared area refused) | Tasks 3, 4, 7, 9, 10 |
| decision 7 (area required only in a routed project; `--area` refused without a map) | Tasks 7, 10, 12 |
| decision 8 (two tiers, `local` and `github`) | Task 3 (`Tier`) |
| decision 9 (a routing tracker implementing `Tracker`) | Tasks 7, 8 |
| decision 10 (the area is a label) | Tasks 1, 2 |
| decision 11 (remove refused while items name the area) | Tasks 6, 8, 14 |
| decision 12 (routed + `ledger = "github"` refused) | Task 10 |
| decision 13 (a sensitive area sets the security flag) | Tasks 7, 12 (plan ruling 14 for records) |
| decision 14 (`fl_format` 2; a local record reference) | Tasks 1, 5 (and the tracker spec's §2.3 amendment) |
| decision 15 (a removed area stays on other machines' items) | Task 14 (the note and the doc) |
| decision 17 (no rename) | Task 14 (the doc) |
| decision 19 (GitHub opens lazily) | Task 10 |
| decision 20 (a routed store holds exactly one project) | Tasks 3, 4, 9 |
| decision 21 (nothing about a sensitive item reaches a non-private repository; refuse, not warn) | Tasks 7, 12 |
| decision 22 (decision 21 fails closed: sensitivity kept unless changed, clearing refused while items name the area, an undeclared area counts as sensitive, such findings carry `security`) | Tasks 3, 7, 9, 14 |
| §1.1 the area: model, store, block, label, inheritance, name rules | Tasks 1, 2, 3, 7, 12 |
| §1.2 the map, manifest formats 1–3, the starting set, `set`/`remove`/`show`, import currency | Tasks 3, 4, 9, 10, 12, 14 |
| §1.3 routed projects, availability, no fallback, older fl | Tasks 7, 10, 14 (the doc's limits) |
| §1.4 store format 5 | Tasks 2, 3, 4 |
| §2.1 create, `create_in`, the security rule | Tasks 7, 12 |
| §2.2 lookup, alias-scan fallback, "another machine's local tier" | Task 8 (the "escalating" part is plan B's) |
| §2.3 handles, `Ref::Issue`, printing, `fl github`, the migration notice | Tasks 9, 10, 11 |
| §2.4 merged lists, the tier column, `--tier`, a failed tier, `--record`, withdrawals | Tasks 8, 13 (the "escalating" mark is plan B's) |
| §2.5 references, `ForeignRecord`, `add_finding_checked`, `{id, title}`, disclosure, evidence | Tasks 1, 5, 7, 8, 12 (the tombstone hop is plan B's) |
| §2.6 lazy GitHub | Task 10 |
| §2.7 concurrency (the local store opened exclusively) | unchanged: `RedbStore::open` already holds the file for the command |
| §4 errors (plan A's rows) | Tasks 6 (`RoutingFault`), 7, 8, 10, 11, 12, 13, 14 |
| §5 testing (plan A's items) | every task; the conformance suite over the router in Task 8 |
| decisions 1, 3, 4, 16, 18; §3, §3.4, the escalation rows of §4, §5's escalation and live tests | plan B |
