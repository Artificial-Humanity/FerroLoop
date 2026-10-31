# Core / GitHub / MCP / CLI cloud audit — 2026-10-31

Scope: an autonomous cloud agent's audit of `crates/core/src` (arch, code_index, decision,
escalation, ferrostate, ferrowire, finding, ids, iri, mem, model, pr_policy, routing, split,
state, tiered, verdict, wire), `crates/github/src` (client, creds, meta, owner, pr, tracker),
`crates/mcp/src` (catalog, freeze, registry, sync) and `crates/cli/src` (comment, config, ctx,
main, preflight, refs, testing, tiers). Branch `cloud/audit-2026-10-31-core-github-mcp-cli`,
cut from `origin/main`'s state at the start of the session.

Build note: the workspace needs rustc 1.98; the sandbox had 1.97 and was updated to 1.99.1. No
repository file changed for that.

## How this audit was done, and what it does not claim

Five read-only reviewers each read the non-test code of one area (core part 1: mem, state,
ferrostate, ferrowire, wire, ids, iri, model, finding, decision; core part 2: arch, code_index,
escalation, pr_policy, routing, split, tiered, verdict; github; mcp; cli). They reported
candidates with a file, a line and a trigger. **I then re-read the code for each candidate I
fixed** and wrote the test and fix myself. Candidates I did not fix are listed under
[Open items](#open-items) with the reason; most were confirmed by reading only.

Honest limits:

* **No regression test was run against the unfixed code.** Every test below was written
  together with its fix and passes with it. I did not stash the fix to watch the test fail. The
  status column therefore says *Fixed — test written*, never *reproduced*. Several tests
  (the `u64::MAX` cursor, the hard-link `save` test, the `>` in a marker, the cycling `Link`
  header) are such that the old code cannot pass them by inspection, but that is argument, not
  a run.
* `tiered.rs`, `escalation.rs`, `verdict.rs`, `split.rs`, `decision.rs`, `finding.rs`, `ids.rs`,
  `iri.rs`, `wire.rs`, `model.rs`, `preflight.rs`, `testing.rs`, `tiers.rs`, `owner.rs`: no
  defect found that I could confirm. That is a statement about what the reviewers found, not a
  proof.
* `fake.rs` (github) and the `ledger/` module were read only where a finding led into them. The
  `vendor/*.rs` renderers of the MCP crate were skimmed.
* Several findings are in `MemStore`, the in-memory test and conformance store (marked **mem**).
  I did not check the redb store for the same patterns.

Verification run at the end (all green): `cargo test -p fl-core -p fl-github -p fl-mcp -p fl-cli`
(34 suites ok) and `cargo clippy --workspace --all-targets -- -D warnings` (clean), after
`cargo fmt --all`. No `Atomic*::fetch_update`/`try_update` is used or introduced; no fix needed
an atomic.

Severity is my judgement of impact on a FerroLoop deployment as the docs describe it
(self-hosted, one operator, local agents), not a CVSS score.

## Summary

| # | Sev | Area | Finding | Status |
|---|---|---|---|---|
| P-1 | **High** | core | A forged `Co-authored-by: <owner>` trailer launders a commit the owner did not author | Fixed — test written |
| P-2 | **High** | core | Protected-path matching: only 3 pattern shapes, case-sensitive, no path normalisation | Fixed — test written |
| G-1 | **High** | github | PR policy never sees a renamed file's old path; GitHub's 3000-file / 250-commit caps truncate silently | Fixed — test written |
| M-1 | **Medium-High** | mcp | A registry runtime argument can displace the pinned package (`npx -y evil pkg@1`) | Fixed (positional + package-naming options) — test written; named-arg allow-list open (O-4) |
| P-3 | Medium | core | Reviews folded in list order, not review-id order | Fixed — test written |
| G-2 | Medium | github | Installation token not scoped to the one repository | Fixed — test written |
| G-3 | Medium | github | Pagination loops without bound or cycle detection (two copies) | Fixed — test written |
| M-2 | Medium | mcp | Catalog `save` truncates in place, follows a symlink | Fixed — test written |
| M-3 | Medium | mcp | Registry answer for another server/version is pinned under the requested name | Fixed — test written |
| C-1 | Medium | cli | Stop hook that fails to start exits 2 = "block", escaping the loop guard | Fixed — test written |
| C-2 | Medium | cli | `retention_days` multiplication overflows; `fl wire prune` then prunes everything acked | Fixed — test written |
| C-3 | Low-Medium | cli | `is_owner_repo` accepts `../..`, `a/b?x`, `a/b%2f` | Fixed — test written |
| F-1 | Medium | core | Workspace re-import resets a retired/superseded memory to active, one new revision per run | Fixed — test written |
| E-1 | Medium | mem | Imported-state `Tampered` check runs after the aliasing rewrite | Open (O-1) |
| E-2 | Low-Medium | mem | `send_wire` with a used `id` overwrites the stored message | Fixed — test written |
| E-3 | Low-Medium | mem | `seq + 1` overflows on a `u64::MAX` cursor (4 sites) | Fixed — test written |
| E-4 | Low | core | `DeliveryReceipt::merge` not commutative when names differ | Fixed — test written |
| G-4 | Low-Medium | github | `parse_issue_url` lets `..`, `?`, `%` into `/repos/{name}` | Fixed — test written |
| G-5 | Low-Medium | github | `repair`'s `by` goes into a GitHub comment unescaped | Fixed — test written |
| G-6 | Low-Medium | github | One review by a deleted user fails the whole review listing | Fixed — test written |
| C-4 | Low-Medium | cli | `$FL_DB=""` counts as an explicit store | Fixed — test written |
| C-5 | Low | cli | `owner/repo#007` builds a different IRI from `#7`; huge `N` escapes the issue filter | Fixed — test written |
| C-6 | Low-Medium | cli | The "run this to post it" hint prints the IRI unquoted | Fixed — test written |
| P-4 | Low | core | `-->` inside a review marker's JSON ends the HTML comment early | Fixed — test written |
| M-4 | Low-Medium | mcp | Named argument with no `name` freezes to an empty word | Fixed — test written |
| M-5 | Medium-Low | mcp | No fsync before rename in `stage` and `write_record` | Fixed — no test (not deterministic) |
| O-1 … O-22 | — | various | Not fixed; see [Open items](#open-items) | Open |

## Findings

### P-1 — High — owner trailer launders a foreign commit (`crates/core/src/pr_policy.rs:379`, `evaluate_pr_policy`)

`name_key` is the email local part whenever the display name has whitespace, and
`evaluate_pr_policy` took any trailer whose `name_key` equals the owner as "owner-authored" and
`continue`d, with no look at the commit's `author_login`. A commit by `mallory` with
`Co-authored-by: Some Body <lmcfarlin@evil.example>` was therefore neither unattributed nor an
unknown co-author. Put it in a PR next to one commit with a real agent trailer, and
`co_authors` is non-empty, `owner_authored_commits` is ignored, and the required tier falls
from Owner to Lead/LabManager.

Fix: a trailer names the owner only on a commit the owner's login authored. On any other commit
it attributes nothing; a commit with no recognised attribution (no agent trailer, not the
owner's) is listed in `unattributed_commits`, which forces the Owner tier (`pr_policy.rs:392`).

Test: `pr_policy::tests::owner_trailer_on_a_foreign_commit_does_not_launder_it`.

### P-2 — High — protected paths: three pattern shapes, case, `..` (`pr_policy.rs:249`, `:263`, `is_protected_path`)

`matches_pattern` understood `dir/**`, `**/x` and a bare name; anything else was compared
literally. A user's `extra_protected_patterns = ["*.pem"]` looked active and protected nothing
(fail-open). Matching was case-sensitive, so `.GitHub/workflows/ci.yml` (the same directory on
macOS/Windows checkouts) was unprotected, and `./a/../.github/x`, `a//b` and `\` separators were
not normalised.

Fix: `normalize_policy_path` folds case, treats `\` as `/`, drops `.` and empty segments and
resolves `..`; real globs (`*.pem`, `docs/*.md`, `ci/**/*.yml`) go through `globset` (already a
dependency of this crate), with `/` kept as a literal separator; a pattern that fails to compile
protects its literal text rather than nothing. The three old shapes keep their old meaning.

Test: `pr_policy::tests::protected_patterns_cover_globs_case_and_dot_segments`.

**Trade-off.** Case folding also protects `.GITHUB/…` on a case-sensitive filesystem where it is
a different directory. That over-protects, which here means "asks the owner", and is the safe
direction.

### G-1 — High — renamed files and GitHub's list caps (`crates/github/src/pr.rs:349`, `files_from_items`; `list_pr_commits`)

`list_pr_files` returned only each entry's `filename`. For a rename GitHub puts the old path in
`previous_filename`, so moving `.github/workflows/ci.yml` to `x.yml` showed `x.yml` only and
`is_protected_path` never saw the protected source. Separately `/pulls/{n}/files` stops at 3000
entries and `/pulls/{n}/commits` at 250, with no error: a PR past the cap looked fully listed.
The CLI feeds both lists to `evaluate_pr_policy` (`cmd/pr.rs`).

Fix: a rename lists both paths; a listing at the cap is an error naming the cap
(`PR_FILES_CAP`, `PR_COMMITS_CAP`), because the files or commits past it cannot be checked.

Tests: `pr::tests::a_renamed_file_lists_its_old_path_too`,
`pr::tests::a_file_listing_at_githubs_cap_is_an_error`.

**Trade-off.** A PR of exactly 3000 files, or 250 commits, is refused although it might be
complete. GitHub gives no way to tell.

### M-1 — Medium-High — registry runtime argument displaces the pin (`crates/mcp/src/freeze.rs:829`, `displaces_package`; loop at `:560`)

Only OCI refused a positional runtime argument. For npm and PyPI the registry's
`runtime_arguments` were emitted before the pinned `pkg@ver` token, so a positional `evil`
froze to `npx -y evil pkg@1.0.0`, which runs `evil`; `--from evil` / `-p evil` did the same for
`uvx`/`npx`. The entry looked pinned and was not.

Fix: for non-docker lines a positional runtime argument that does not start with `-` is refused,
as are options that name another package or index (`-p`, `--package`, `--from`, `--call`, `-c`,
`--registry`, `-f`, `--find-links`, `--extra-index-url`, `--with*`, `--index*`,
`--default-index*`). The existing tested shape (`--no-update-notifier` as a positional flag, a
`--prefer-offline` named one) still freezes.

Test: `freeze::tests::a_runtime_argument_cannot_displace_the_pinned_package`.

**Open trade-off (O-4).** This is a deny-list of known-dangerous options, not an allow-list. An
unknown option that takes a package (or docker's `--privileged`, `-v /:/host`, `--entrypoint`)
still passes. Severity depends on how far the registry is trusted; an allow-list is the real fix
and changes what registry entries freeze, so it is the owner's call.

### P-3 — Medium — review fold order (`pr_policy.rs:718`, `verify_merge_approvals`)

The doc says "folding reviews chronologically", but the loop used the caller's order.
Newest-first input let a stale `CHANGES_REQUESTED` or `APPROVED` overwrite the latest state.

Fix: sort by review `id` (ids only grow) before folding.

Test: `pr_policy::tests::approvals_fold_by_review_id_not_by_list_order` (both directions).

### G-2 — Medium — installation token not scoped (`crates/github/src/creds.rs:199`)

`AppCredentials` documents "installation token for one repository", but the POST to
`access_tokens` sent no body, which grants every repository and permission of the installation.

Fix: send `{"repositories": ["<repo name>"]}`.

Test: `creds::tests::the_installation_token_is_requested_for_the_one_repository` (the fake now
records the request bodies: `State::token_bodies`).

### G-3 — Medium — unbounded pagination (`crates/github/src/client.rs:552`, `collect_pages`; `pr.rs` `get_paginated`)

`get_all_paged` and a duplicate in `pr.rs` followed `rel="next"` with no page cap and no cycle
check. A server that repeats a URL (a misbehaving GHES or proxy) looped forever, growing `out`.

Fix: one `collect_pages` that errors on a repeated URL or after `MAX_PAGES` (1000); `pr.rs` now
calls `Client::get_all`.

Tests: `client::tests::a_next_link_that_cycles_is_an_error_not_an_endless_list`,
`a_chain_of_pages_past_the_cap_is_an_error`, `a_finite_chain_collects_every_page`.

### M-2 — Medium — catalog `save` (`crates/mcp/src/catalog.rs:639`)

`std::fs::write` truncates then writes: a crash or a full disk mid-write leaves a truncated
`.fl/mcp.toml`, the only copy of the committed servers. It also wrote through a symlink.

Fix: write a temp file beside it, copy the old file's permissions, `sync_all`, `persist`
(rename). A symlinked catalog is refused.

Tests: `catalog::tests::save_replaces_the_file_whole_instead_of_truncating_it` (a hard link to
the old file keeps the old bytes), `save_does_not_write_through_a_symlinked_catalog`.

**Trade-off.** Someone who symlinks `.fl/mcp.toml` to a shared file now gets an error from
`save` instead of a write through the link.

### M-3 — Medium — registry answer not matched to the request (`crates/mcp/src/registry.rs:482`, `answer_mismatch`)

`Registry::version` never compared the returned server name/version with the request, and
`freeze` records the answer's own `name` and `version`: `add --from A --version 1.0.0` could pin
server B at 9.9.9.

Fix: the answer's name must equal the request; an exact version must equal too (`latest` names
none).

Test: `registry::tests::an_answer_for_another_server_or_version_is_refused`.

### C-1 — Medium — a hook that fails to start blocks the agent (`crates/cli/src/main.rs:739`, `hook_exit_code`)

`main` mapped any error from `run_claude_hook_entry` (a malformed `config.toml`, no home
directory) to exit 2. For a Stop hook exit 2 means "block, keep going", and the
`stop_hook_active` guard that ends the loop is only read after the hook has started. A broken
config therefore made an agent loop with no ceiling, against AGENTS.md's token-ceiling rule.

Fix: a hook entry that fails to start exits 1 (a non-blocking error).

Test: `main::tests::a_hook_that_cannot_start_does_not_block_the_agent`.

Not changed: `agent-bus-mcp` and `agent-msg` entries still exit 2 on error; exit 2 has no
blocking meaning for them.

### C-2 — Medium — `retention_days` overflow (`crates/cli/src/cmd/wire.rs:39`, `retention_cutoff_ms`)

`days * 86_400_000` was unchecked on a config-supplied `u64`. In a release build
`retention_days = 213503982335` wraps to a tiny value, the cutoff lands near "now", and
`fl wire prune` deletes every acknowledged transient message; a debug build panics.

Fix: `saturating_mul`.

Test: `wire::tests::retention_cutoff_saturates_instead_of_wrapping`.

### C-3 — Low-Medium — `is_owner_repo` (`crates/cli/src/config.rs:611`)

"Two non-empty parts, no whitespace" accepted `../..`, `a/b?x=1`, `a/b#c`, `a/b%2f`. The value
reaches `format!("{}/repos/{}/installation")` where the App JWT is sent, and the tracker's API
paths and IRIs.

Fix: each part is `[A-Za-z0-9._-]+` and not `.`/`..`.

Test: `config::tests::owner_repo_rejects_traversal_and_url_metacharacters`.

### F-1 — Medium — workspace re-import resets curated memory (`crates/core/src/ferrostate.rs:831`, `apply_workspace_import`)

Every scanned memory is upserted with `lifecycle: Active`, `superseded_by: None`. After an
operator retired a memory, the next `fl state import-workspace` made it active again, and every
run wrote a new revision even when nothing changed.

Fix: for a memory that already exists, keep its lifecycle and `superseded_by`, and skip the
upsert (still counted as imported) when the result equals what is stored.

Test: `ferrostate::tests::reimport_keeps_a_retired_memory_retired_and_writes_no_new_revision`.

### E-2 — Low-Medium (mem) — `send_wire` overwrites a used id (`crates/core/src/mem.rs:1829`)

`index_imported_unhanded` accepts an id already owned with the same kind, and
`append_wire_message` then replaced the stored message under receipts that point at it.

Fix: a used message id is `AlreadyExists`.

Test: `mem::tests::sending_with_a_used_message_id_is_refused_and_leaves_the_first_message`.
I did not check what the redb store does for the same call (`store/src/ferrostore/traits_wire.rs`
uses the same `index_message_id` pattern).

### E-3 — Low-Medium (mem) — `seq + 1` overflow (`mem.rs:1404`, `:1743`, `:2053`, `:2070`)

`registry_events_after`, `state_revisions_after`, `wire_messages_after` and
`wire_receipts_after` used `range((seq + 1)..)`. A cursor of `u64::MAX` panics in debug and
wraps to `range(0..)` in release (a full re-push).

Fix: `range((Bound::Excluded(seq), Bound::Unbounded))`.

Test: `mem::tests::a_cursor_at_u64_max_reads_nothing_instead_of_overflowing`.

### E-4 — Low — receipt merge not commutative (`crates/core/src/ferrowire.rs:299`)

`DeliveryReceipt::merge` is documented commutative. With two different real recipient names the
left side won, so replicas merging in opposite orders diverged permanently.

Fix: rank real name > `urn:` placeholder > empty; between equals the lesser string wins.

Test: `ferrowire::tests::receipt_merge_is_commutative_when_the_names_differ`.

### G-4 — Low-Medium — `parse_issue_url` (`crates/github/src/meta.rs:646`)

Owner and repo were checked non-empty only, then used in `format!("/repos/{name}")`. An IRI
`https://github.com/../x/issues/1` or `…/b%3Fq/issues/1` sent an authenticated GET to an
unintended path. (The response must still match the bound repository's `node_id`, so the impact
is extra authenticated reads.)

Fix: GitHub's name characters only, never `.`/`..`.

Test: `meta::tests::an_issue_url_with_a_traversing_or_odd_repo_name_is_not_an_issue_url`.

### G-5 — Low-Medium — `repair` comment (`crates/github/src/tracker.rs:1665`)

The `by` argument went into a GitHub comment raw (`@org/team` pings, markdown). Every other
comment in the crate escapes names.

Fix: `render::escape_capped(by, 200)`.

Test: `tracker::tests::repair_comment_does_not_ping_the_name_it_was_given`.

### G-6 — Low-Medium — deleted reviewer (`crates/github/src/pr.rs:188`, `parse_review_summary`)

GitHub sends `"user": null` for a deleted account. That made `list_pr_reviews` fail, and with it
policy, merge and status for the PR, indefinitely.

Fix: attribute it to `ghost`, which no registered agent or owner can be (fails closed).

Test: `pr::tests::a_review_by_a_deleted_user_parses_as_ghost`.

### C-4 — Low-Medium — empty `$FL_DB` (`crates/cli/src/main.rs:286`, `explicit_db_from`)

`FL_DB=""` (CI templates export it when a variable is unset) became `Some("")`: it confined the
command to a store named "" and skipped the config binding. `config.rs` already treats an empty
XDG variable as unset.

Fix: an empty value is unset.

Test: `main::tests::empty_fl_db_is_unset`.

### C-5 — Low — `owner/repo#007` (`crates/cli/src/refs.rs:41`)

`#007` produced `…/issues/007`, a different IRI string from `…/issues/7` for the same issue
(lookup miss, duplicate alias key); a number past `u64` fell out of the issue-URL filter.

Fix: parse as `u64` and format back; too large is an error.

Test: `refs::tests::issue_number_is_normalised_and_bounded`.

### C-6 — Low-Medium — recovery hint (`crates/cli/src/comment.rs:40`, `shell_word`)

`recovery()` printed `fl github ledger comment {iri}` in a warning people and agents are told to
run. An IRI only has to be free of whitespace, so `urn:x:$(id);…`, a backtick or an ESC byte is a
valid one.

Fix: anything beyond the characters a plain URL holds is single-quoted; control characters are
shown as text.

Test: `comment::tests::the_recovery_command_quotes_an_iri_with_shell_syntax`.

### P-4 — Low — `-->` in the marker (`pr_policy.rs:502`, `format_review_body`)

`extract_review_marker` ends at the first `-->`; `serde_json` does not escape `>`. A `project`
or `reviewer` containing `-->` made the marker unreadable, and a non-owner APPROVED review
without a readable marker blocks the merge.

Fix: write `>` as `>` (valid JSON, only occurs inside strings).

Test: `pr_policy::tests::review_marker_survives_an_arrow_in_a_field`.

### M-4 — Low-Medium — nameless named argument (`freeze.rs:591`)

`a.name.clone().unwrap_or_default()` made `["", value]` — `npx -y "" value pkg@1`.

Fix: a named argument without a name is refused.

Test: `freeze::tests::a_named_argument_without_a_name_is_refused_not_passed_as_an_empty_word`.

### M-5 — Medium-Low — no fsync before rename (`crates/mcp/src/sync.rs:584`, `:772`)

`stage` and `write_record` renamed a temp file that was never `sync_all`'d. After a power loss
on a filesystem without rename-flush heuristics a `.mcp.json` / `.codex/config.toml` can come
back zero-length; an empty ownership record then fails every later plan with `McpError::Record`.

Fix: `sync_all` before `persist`. No test: a power loss is not deterministic and a call-order
test would only restate the code. The directory is still not fsynced (see O-12).

## Open items

Not fixed. "Read" = confirmed by reading only; nothing was run.

| # | Sev | Where | What | Why not fixed |
|---|---|---|---|---|
| O-1 | Medium | `mem.rs:339-375` | **mem.** `import_state_revisions`: a same-key collision rewrites `state_entries` (alias onto `rev.entry_id`, `remove` then `insert`) before the `Tampered` immutability check, so a revision whose `entry_id` is entry A and whose slug is B's destroys A. Also mutates before it can still fail. | The function is intricate and shared with sync semantics; the right fix (check before rewrite, atomic apply) deserves its own change with a hostile-input test. |
| O-2 | Medium | `mem.rs:1497` | **mem.** A failed `upsert_state` leaks a minted id and handle (mint before validate). | MemStore only; needs re-ordering mint and validation. |
| O-3 | High (design) | `pr_policy.rs:696-725` | The review marker's `reviewer`/`role` come from the review body and are not bound to the review's GitHub login. Anyone who can post an APPROVED review, the PR author included, can embed a marker naming a registered lead and pass `authorize_agent_action`. | Cannot be fixed in code alone: needs a login on `AgentRecord` or a signed marker. If agents share one bot account it is unfixable without signing. Owner decision. |
| O-4 | Medium | `freeze.rs:564-640` | Registry named runtime args are deny-listed (M-1), not allow-listed; docker's `--privileged`, `-v`, `--entrypoint` still pass. | An allow-list changes which registry entries freeze. |
| O-5 | Medium | `freeze.rs:672-676` | `upgrade` keeps old literal env values over the new version's `value`/`default` (`kept_env` wins), so a changed fixed value is silently pinned at v1's. | Needs a record of which literals the user supplied versus fl froze. |
| O-6 | Medium | `catalog.rs:601-648` | `Editor::open`→`save` is a lost-update window with a registry network call in the middle (`cmd/mcp.rs` add/upgrade). `save` is now atomic but still overwrites from a stale document. | Needs a lock file plus compare-before-write; touches the CLI flow. |
| O-7 | Medium | `catalog.rs:385-482` | `check_server` accepts `http://` URLs (a secret `Authorization` over cleartext), a bad `scheme`/header name, an empty `command`; `is_scheme`/`is_token` run only in freeze and the CLI. | Whether loopback/`http` stays legal is a policy call. |
| O-8 | Low | `freeze.rs:596-598,668-678` | A later non-secret variable of the same name overwrites a docker `-e NAME` secret with a literal. | Edge case needing a registry that does it. |
| O-9 | Low | `sync.rs:352,499-556` | `apply` re-reads vendor files under the lock but not the ownership record; two overlapping `fl mcp sync` runs can clobber it. `Owned.root` is written and never read. | Rare; fix is a re-read under the lock. |
| O-10 | Low | `freeze.rs:1031-1040` | `resolve` substitutes sequentially over the substituted string; order-dependent. The secret check runs first, so no leak found. | No exploit; single-pass rewrite. |
| O-11 | Low | `registry.rs:65,150` | A registry URL with `?`/`#` swallows the request path. | `Registry::new` handles the origin on purpose; whether such URLs are legal is a spec question. |
| O-12 | Low | `catalog.rs:639`, `sync.rs:547` | Parent directories are not fsynced after the rename; `.fl-mcp-*` temp files can survive a crash in the project root. | Platform-specific; low value. |
| O-13 | Medium | `main.rs:334,813,1222,1257`, `store/src/lib.rs:473` | Stores and their directories are created with default permissions (0644 in 0755 under umask 022); the redb store holds hub pairing codes, wire messages, memories. | Crosses into the `store` crate (out of scope) and into how the hub stores tokens; I did not check whether it holds raw tokens or hashes. |
| O-14 | Medium | `config.rs:194,215` vs `hub/src/auth.rs:201-235` | The doc says the config `token_file` wins over `$FL_WIRE_TOKEN`/`$FL_HUB_OWNER_TOKEN`; the code does the opposite, and an unreadable `token_file` becomes `None` silently. | Lives in `hub` (out of scope); owner picks which precedence is right. |
| O-15 | Low-Medium | `config.rs:468-478,679-702,275` | Two unnamed project entries with the same basename (or a named one) resolve by file order; `bound_entry` compares `(store, tracker)` only. A command can pick another project's store. | Needs a rule for ambiguous entries (reject at load). |
| O-16 | Low | `config.rs:323,647` | Store sharing is decided by literal path equality (a `..` or symlinked spelling reads as a different store, silently disabling wire/registry sync); one unresolvable root fails every command. The second is documented as deliberate. | Behaviour change; needs canonicalisation policy. |
| O-17 | Medium | `tracker.rs:1938,2008-2016` | `create_escalated` adopts any issue whose `fl:meta` block has the expected `create_key`, with no check on the author or on `escalated.from`. In a repo where outsiders can open issues, one could make fl adopt an attacker-authored issue as the escalation target. | Needs the tracker's identity (`identity()`), which differs between PAT and App; a careful change in a 5300-line file. |
| O-18 | Low | `tracker.rs:896-916`, `:2255-2271`, `meta.rs:359-363,468-507` | Label existence is case-sensitive (a repo's `FL:Record` 422s on create); `add_alias` check-then-write race; `labels_after` drops any `fl:`-prefixed label, not only fl's; prose does not round-trip `parse_body` for `\r\n`/trailing space. | Each is minor and needs a decision about normalisation. |
| O-19 | Low-Medium | `arch.rs:335-340,294,238,356-394` | Duplicate `package` across components is not rejected (later overwrites); a glob with a trailing `/` validates but never matches its contents; `strict_coverage` does not flag an unmapped leaf package. `escape_likec4_string` leaves `\n`. | `arch` semantics are a spec call. |
| O-20 | Low-Medium | `code_index.rs:463-482,882-915` | `scan_code_index`: `symlink_metadata` then `File::open` follows a swapped-in symlink; no total file/byte cap, one `String` per line. `record_token_gain_*` is a read-modify-write on a store with no CAS; the hub calls it per request, so concurrent queries lose counts. | The counter is a statistic; a CAS needs a store-trait change. |
| O-21 | Low | `ferrostate.rs:494-513,597-621,75-82` | `section_kind` matches substrings ("Not done", "Abandoned" → Shipped); `parse_handoff` ignores code fences; `resolve_workspace_root` can pick `~` when `~/Notes` exists. | Heuristic parsers; no data loss. |
| O-22 | Low | `routing.rs:96-110,152-164` | `with`/`after_set` do not validate the area name (only `check()` does). | A library caller can write a map `check()` then rejects. |

Considered and dropped: `FL_GITHUB_API_URL` accepts any https host and sends the credential there
(documented behaviour, needs control of the environment); eager GitHub open in `main.rs:1112`
(intended); `Iri::parse` accepting control characters and `..` (nothing in core builds paths from
it — callers that do must not trust it, see C-6 for one that printed it).
