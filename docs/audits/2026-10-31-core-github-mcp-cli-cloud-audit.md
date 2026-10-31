# Core / GitHub / MCP / CLI cloud audit — 2026-10-31

Scope: an autonomous cloud agent's audit of `crates/core/src` (arch, code_index, decision,
escalation, ferrostate, ferrowire, finding, ids, iri, mem, model, pr_policy, routing, split,
state, tiered, verdict, wire), `crates/github/src` (client, creds, meta, owner, pr, tracker),
`crates/mcp/src` (catalog, freeze, registry, sync) and `crates/cli/src` (comment, config, ctx,
main, preflight, refs, testing, tiers).

Provenance: written by an autonomous cloud agent on its branch
`cloud/audit-2026-10-31-core-github-mcp-cli` (commit `a268729`), cut from `origin/main`'s state
at the start of the session. It was then reviewed independently twice. This repository's branch
is `felicity/cloud-core-github-mcp-cli-audit`: the cloud commit on `ace5150`, the merge of
`origin/main` into it (`844e36c`), and one follow-up commit that fixes what the reviews
accepted. Every `file:line` below was re-derived from the files as they stand after that
follow-up; where a line could not be pinned down, the function is named instead.

Follow-up changes, in one place: [Behaviour change: freezing refuses more](#behaviour-change-freezing-refuses-more)
(M-1, O-4, M-4), the redb part of E-3, the P-2 literal-path fix, the catalog's new-file mode,
the `version()` test (M-3), the empty `--db` (C-4), and the report corrections of P-1, O-3, G-1,
G-2, P-3, E-2, M-5, C-6 and O-13.

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

* **The cloud agent ran no regression test against the unfixed code.** Every test below was
  written together with its fix and passes with it. The status column therefore says *Fixed —
  test written*, never *reproduced*. The follow-up commit's own tests (M-1/O-4, P-2 literal
  paths, redb E-3, catalog mode, `version()`, empty `--db`) were mutation-checked: each fails
  when its fix is neutralised and passes again when it is restored. Several tests
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
  The cloud agent did not check the redb store for the same patterns. The review did: E-3 was in
  redb too and is fixed there; E-2 is still open there (see E-2).

Verification the cloud agent ran at the end (all green, in the cloud sandbox):
`cargo test -p fl-core -p fl-github -p fl-mcp -p fl-cli` (34 suites ok, a count that is the
sandbox's, not this workspace's) and `cargo clippy --workspace --all-targets -- -D warnings` (clean), after
`cargo fmt --all`. No `Atomic*::fetch_update`/`try_update` is used or introduced; no fix needed
an atomic.

Severity is my judgement of impact on a FerroLoop deployment as the docs describe it
(self-hosted, one operator, local agents), not a CVSS score.

## Summary

| # | Sev | Area | Finding | Status |
|---|---|---|---|---|
| P-1 | **High** | core | A forged `Co-authored-by: <owner>` trailer launders a commit the owner did not author | Partial: the owner-trailer forgery is closed; two trailer-trust paths stay open (O-23, O-24) |
| P-2 | **High** | core | Protected-path matching: only 3 pattern shapes, case-sensitive, no path normalisation | Fixed — test written |
| G-1 | **High** | github | PR policy never sees a renamed file's old path; GitHub's 3000-file / 250-commit caps truncate silently | Fixed — test written |
| M-1 | **Medium-High** | mcp | A registry runtime argument can displace the pinned package (`npx -y evil pkg@1`) | Fixed by an allow-list (npm/PyPI, docker, environment names) — test written; see [Behaviour change](#behaviour-change-freezing-refuses-more) |
| P-3 | Low | core | Reviews folded in list order, not review-id order | Fixed — test written |
| G-2 | Medium | github | Installation token not scoped to the one repository | Fixed — test written |
| G-3 | Medium | github | Pagination loops without bound or cycle detection (two copies) | Fixed — test written |
| M-2 | Medium | mcp | Catalog `save` truncates in place, follows a symlink | Fixed — test written |
| M-3 | Medium | mcp | Registry answer for another server/version is pinned under the requested name | Fixed — test written |
| C-1 | Medium | cli | Stop hook that fails to start exits 2 = "block", escaping the loop guard | Fixed — test written |
| C-2 | Medium | cli | `retention_days` multiplication overflows; `fl wire prune` then prunes everything acked | Fixed — test written |
| C-3 | Low-Medium | cli | `is_owner_repo` accepts `../..`, `a/b?x`, `a/b%2f` | Fixed — test written |
| F-1 | Medium | core | Workspace re-import resets a retired/superseded memory to active, one new revision per run | Fixed — test written |
| E-1 | Medium | mem | Imported-state `Tampered` check runs after the aliasing rewrite | Open (O-1) |
| E-2 | Low-Medium | mem | `send_wire` with a used `id` overwrites the stored message | Fixed in MemStore only — redb still returns Ok for an existing message id |
| E-3 | Low-Medium | mem, redb | `seq + 1` overflows on a `u64::MAX` cursor (4 sites in MemStore, 4 in the redb store) | Fixed in both — test written |
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
| O-1 … O-24 | — | various | Not fixed (O-4 is fixed, see M-1; O-3 is fixed, see its note below); see [Open items](#open-items) | Open |

## Findings

### P-1 — High — owner trailer launders a foreign commit (`crates/core/src/pr_policy.rs:346`, `evaluate_pr_policy`)

`name_key` is the email local part whenever the display name has whitespace, and
`evaluate_pr_policy` took any trailer whose `name_key` equals the owner as "owner-authored" and
`continue`d, with no look at the commit's `author_login`. A commit by `mallory` with
`Co-authored-by: Some Body <lmcfarlin@evil.example>` was therefore neither unattributed nor an
unknown co-author. Put it in a PR next to one commit with a real agent trailer, and
`co_authors` is non-empty, `owner_authored_commits` is ignored, and the required tier falls
from Owner to Lead/LabManager.

Fix: a trailer names the owner only on a commit the owner's login authored. On any other commit
it attributes nothing; a commit with no recognised attribution (no agent trailer, not the
owner's) is listed in `unattributed_commits`, which forces the Owner tier.

Test: `pr_policy::tests::owner_trailer_on_a_foreign_commit_does_not_launder_it`.

**Status: partial.** The owner-trailer forgery is closed. A trailer is still trusted in two
ways, both the same laundering class, and both are open:

* `Co-authored-by: <agent>` on a commit by ANY login still counts as attributed. A foreign
  commit with an agent trailer therefore gets a Lead/LabManager tier and needs no owner trailer
  (O-23).
* `is_owner_commit` uses GitHub's `author.login`, which GitHub derives from the commit author
  email. The owner's noreply email is public in `git log`, so anyone can author a commit as the
  owner login and have it treated as owner-authored (O-24).

Closing both needs the same thing: an allow-list of author logins that may carry an attributed
commit (the owner's login and the agents' machine account), or verified commit signatures.

### P-2 — High — protected paths: three pattern shapes, case, `..` (`pr_policy.rs:249`, `normalize_policy_path`; `:279`, `matches_pattern`)

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

Follow-up: a pattern with a literal `[`, `{` or `?` (`app/[id]/**`) went through `globset` and no
longer matched the literal path the plain-text matcher protected (`app/[id]/x`; it matched only
`app/i/x`), a narrowing of protection. `matches_pattern` now also accepts every path the
plain-text matcher accepted (`literal_match`: `dir/**`, `**/name`, bare name, full path), in
addition to the glob. Test:
`pr_policy::tests::a_pattern_with_a_literal_glob_character_still_protects_that_literal_path`.

**Trade-off.** Case folding also protects `.GITHUB/…` on a case-sensitive filesystem where it is
a different directory. That over-protects, which here means "asks the owner", and is the safe
direction.

### G-1 — High — renamed files and GitHub's list caps (`crates/github/src/pr.rs:349`, `files_from_items`; `:281`, `list_pr_commits`)

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
complete. GitHub gives no way to tell. `fl github pr create` has already created the PR when
it then errors on such a list, so the refusal leaves a PR behind.

`previous_filename` is also set for `status: copied`, so a copy out of `.github/` is flagged
protected although the source is unchanged. That over-protects, which is the safe direction.

### M-1 — Medium-High — registry runtime argument displaces the pin (`crates/mcp/src/freeze.rs:864`, `runtime_argument_problem`; loop at `:524` in `Build::package`)

Only OCI refused a positional runtime argument. For npm and PyPI the registry's
`runtime_arguments` were emitted before the pinned `pkg@ver` token, so a positional `evil`
froze to `npx -y evil pkg@1.0.0`, which runs `evil`; `--from evil` / `-p evil` did the same for
`uvx`/`npx`. The entry looked pinned and was not.

First fix (cloud agent): a deny-list of option names spelled as a named argument's name. The
review showed it was only partly effective: a positional argument was allowed whenever it
started with `-` (`--package=evil`, `--from=evil`, `--index-url=...`, `-pevil`); a named
argument with a harmless name and a non-boolean value (`--prefer-offline evil`) renders
`--prefer-offline evil`, and a boolean flag does not consume the next word, so `evil` becomes the
package to run; options were missing from the list (`-i`, `-w`, `--@scope:registry`,
`--userconfig`, `--prefix`, `--node-options`, `--directory`, `--project`) and the check was
case-exact; and registry `environmentVariables` were not screened (`npm_config_registry`,
`UV_INDEX_URL`, `NODE_OPTIONS`, `PATH` repoint the fetch without touching argv).

**Fix (follow-up): fail closed with an allow-list**, for every runtime argument of every
package kind. An argument is accepted only if it is exactly one of the following, compared
case-sensitively, in exactly this spelling (no `=value`, no attached short form, no padding):

* **npm and PyPI (`npx`, `uvx`):** the boolean flags `-y`, `--yes`, `--no-update-notifier`,
  `--prefer-offline`, `--quiet`. As a positional argument its value is the flag. As a named
  argument its name is the flag, its `format` is `boolean`, and its value, if it has one, is the
  literal `true` or `false` (a templated value, or any other word, is refused: it would be
  rendered after the flag and become the package to run).
* **docker (`docker run`):** the boolean flags `-i` and `--rm` (named, as above); `-e` /
  `--env` with a value `NAME=...` whose `NAME` is a valid environment variable name that is not
  on the refused list below (a bare `-e NAME` would pass the host's own variable into the
  container, so it is refused); and `-p` / `--publish` with a value of exactly
  `127.0.0.1:<port>:<port>` (optionally `/tcp` or `/udp`), a port published for this machine
  only. A positional runtime argument stays refused for docker. So are `--privileged`, `-v`,
  `--volume`, `--mount`, `--entrypoint`, `--network`, `--cap-add`, `--device`, `--pid`,
  `--user`, `--security-opt` and everything else not listed.
* **Refused for everything else:** the refusal names the argument (its name, cut at an attached
  `=`; or its place in the list for a positional one) and never echoes a value, because a value
  may be secret-templated. An argument type fl does not know, a named argument with no name,
  and a positional runtime argument with no value or default (a `valueHint`-only placeholder)
  are refused too.

**Environment variables.** A registry `environmentVariables` entry whose NAME matches one of
these, compared without case, is refused whether it is a literal or a secret, for every package
kind (the docker `-e NAME=...` runtime argument is held to the same list): prefixes `npm_`
(so `npm_config_*`, including `NPM_CONFIG_*`), `npx_`, `node_` (`NODE_OPTIONS`, `NODE_PATH`,
`NODE_EXTRA_CA_CERTS`, ...), `uv_`, `pip_`, `python` (`PYTHONPATH`, `PYTHONSTARTUP`,
`PYTHONHOME`, ...), `ld_`, `dyld_`, `docker_` (`DOCKER_HOST`), `xdg_` (`XDG_CONFIG_HOME`),
`ssl_` (`SSL_CERT_FILE`, ...); and the exact names `PATH`, `HOME`, `USERPROFILE`, `APPDATA`,
`SHELL`, `IFS`, `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, `REQUESTS_CA_BUNDLE`,
`CURL_CA_BUNDLE`. The list is a judgement: it covers what repoints a package fetch, loads code
into the runner, or changes what `PATH`/`HOME` resolve to. Look-alikes such as `SAMPLE_HOME` or
`MY_PATH` pass. Values the person supplies with `--env` are not screened: the names come from
the registry.

What informed the lists: the shipped fixtures and tests use `--prefer-offline` (named,
boolean) and `--no-update-notifier` (positional) for npm, and for docker `-p
127.0.0.1:8085:8085`, `-e NAME=value` and `-e NAME={secret}` (the GitHub server's real shape and
`fake.rs`'s tracker fixture). All of them still freeze.

Tests: `freeze::tests::a_runtime_argument_cannot_displace_the_pinned_package` (every spelling
above, both npm and PyPI, uppercase variants, the `valueHint`-only and nameless cases),
`a_docker_runtime_argument_is_refused_unless_it_is_on_the_list`,
`a_registry_environment_variable_that_repoints_the_fetch_is_refused`,
`the_flags_on_the_runtime_list_still_freeze`. With the allow-list neutralised the first two fail;
with the environment screen neutralised the environment test and the docker test fail.

### O-4 (closed by M-1) — docker runtime arguments

The cloud report left docker's `--privileged`, `-v /:/host`, `--entrypoint`, `--network=host`,
`--mount`, `--cap-add`, `--device`, `--pid`, `--user` and `--security-opt` passing as an open
item. The docker branch is now the allow-list above. The row in the open-item table is kept so
the numbering does not move.

### P-3 — Low — review fold order (`pr_policy.rs:716`, `verify_merge_approvals`)

The doc says "folding reviews chronologically", but the loop used the caller's order.
Newest-first input let a stale `CHANGES_REQUESTED` or `APPROVED` overwrite the latest state.

Fix: sort by review `id` (ids only grow) before folding.

Test: `pr_policy::tests::approvals_fold_by_review_id_not_by_list_order` (both directions).

Rated Low after review: this is defence in depth, because GitHub lists reviews oldest-first, so
the old loop saw them in chronological order in practice.

### G-2 — Medium — installation token not scoped (`crates/github/src/creds.rs:157`, `exchange`)

`AppCredentials` documents "installation token for one repository", but the POST to
`access_tokens` sent no body, which grants every repository and permission of the installation.

Fix: send `{"repositories": ["<repo name>"]}`.

Test: `creds::tests::the_installation_token_is_requested_for_the_one_repository` (the fake now
records the request bodies: `State::token_bodies`).

**Regression to know about.** `repositories` is built from `self.repo`. After a repository
rename the installation lookup follows GitHub's redirect to the new name, but the token request
still names the OLD repository, so GitHub will probably answer 422 where the unscoped token used
to work. It fails safe (an error, not a leak); the fix for a renamed repository is to update the
bound repository name in the fl config.

### G-3 — Medium — unbounded pagination (`crates/github/src/client.rs:552`, `collect_pages`; `pr.rs` formerly had its own copy)

`get_all_paged` and a duplicate in `pr.rs` followed `rel="next"` with no page cap and no cycle
check. A server that repeats a URL (a misbehaving GHES or proxy) looped forever, growing `out`.

Fix: one `collect_pages` that errors on a repeated URL or after `MAX_PAGES` (1000); `pr.rs` now
calls `Client::get_all`.

Tests: `client::tests::a_next_link_that_cycles_is_an_error_not_an_endless_list`,
`a_chain_of_pages_past_the_cap_is_an_error`, `a_finite_chain_collects_every_page`.

### M-2 — Medium — catalog `save` (`crates/mcp/src/catalog.rs:639`, `Editor::save`)

`std::fs::write` truncates then writes: a crash or a full disk mid-write leaves a truncated
`.fl/mcp.toml`, the only copy of the committed servers. It also wrote through a symlink.

Fix: write a temp file beside it, copy the old file's permissions, `sync_all`, `persist`
(rename). A symlinked catalog is refused.

Tests: `catalog::tests::save_replaces_the_file_whole_instead_of_truncating_it` (a hard link to
the old file keeps the old bytes), `save_does_not_write_through_a_symlinked_catalog`.

**Trade-off.** Someone who symlinks `.fl/mcp.toml` to a shared file now gets an error from
`save` instead of a write through the link. `Editor::open` still reads through a symlinked
catalog, so `fl mcp add` now fails on a catalog that loads fine. That is deliberate: refusing is
not needed for safety, because `rename` replaces the link itself and never writes through it, but
the refusal keeps the behaviour simple and visible.

Follow-up: a new `.fl/mcp.toml` was created `0600` (the temp file's default; permissions were
copied only when the file already existed), but the catalog is committed and shared with the
team. A new file now gets `0666` less the umask, as `sync.rs`'s `stage` gives a new vendor file
(`new_file_mode`, now `pub(crate)`). Test:
`catalog::tests::a_new_catalog_gets_the_default_file_mode_not_a_private_one`.

### M-3 — Medium — registry answer not matched to the request (`crates/mcp/src/registry.rs:482`, `answer_mismatch`; called from `Registry::version`)

`Registry::version` never compared the returned server name/version with the request, and
`freeze` records the answer's own `name` and `version`: `add --from A --version 1.0.0` could pin
server B at 9.9.9.

Fix: the answer's name must equal the request; an exact version must equal too (`latest` names
none).

Tests: `registry::tests::an_answer_for_another_server_or_version_is_refused` (the helper) and,
added in the follow-up, `registry::tests::version_refuses_an_answer_for_another_server_or_version`
(the `FakeRegistry` answers for another server or version, and `version()` returns the error;
it fails if the call to `answer_mismatch` is removed).

### C-1 — Medium — a hook that fails to start blocks the agent (`crates/cli/src/main.rs:741`, `hook_exit_code`)

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

### C-3 — Low-Medium — `is_owner_repo` (`crates/cli/src/config.rs:611`, `is_owner_repo`)

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

### E-2 — Low-Medium (mem) — `send_wire` overwrites a used id (`crates/core/src/mem.rs:183`, `append_wire_message`; `:1785`, `send_wire`)

`index_imported_unhanded` accepts an id already owned with the same kind, and
`append_wire_message` then replaced the stored message under receipts that point at it.

Fix: a used message id is `AlreadyExists`.

Test: `mem::tests::sending_with_a_used_message_id_is_refused_and_leaves_the_first_message`.
**Not fixed in the redb store.** `store/src/ferrostore/traits_wire.rs::index_message_id` still
returns Ok for an existing message id; only MemStore was changed. Documented here, not changed.

### E-3 — Low-Medium (mem, redb) — `seq + 1` overflow (`mem.rs`: `registry_events_after` :1401, `state_revisions_after` :1740, `wire_messages_after` :2049, `wire_receipts_after` :2063; the same four in `crates/store/src/state.rs` and `ferrowire.rs`)

`registry_events_after`, `state_revisions_after`, `wire_messages_after` and
`wire_receipts_after` used `range((seq + 1)..)`. A cursor of `u64::MAX` panics in debug and
wraps to `range(0..)` in release (a full re-push).

Fix: `range((Bound::Excluded(seq), Bound::Unbounded))`.

Test: `mem::tests::a_cursor_at_u64_max_reads_nothing_instead_of_overflowing`.

Follow-up: the production redb store had the same `after_seq + 1` / `seq + 1` range at the same
four reads (`crates/store/src/state.rs` `registry_events_after` and `state_revisions_after`,
`crates/store/src/ferrowire.rs` `wire_messages_after` and `wire_receipts_after`). They use the
same `Bound::Excluded` range now. Test: `fl_store::tests::a_cursor_at_u64_max_reads_nothing_instead_of_overflowing`,
which runs `fl_core::conformance::a_cursor_at_u64_max_reads_nothing`: it puts one row in each
log first, because the redb reads return early on a missing table and an empty store would pass
by that alone.

### E-4 — Low — receipt merge not commutative (`crates/core/src/ferrowire.rs:278`, `DeliveryReceipt::merge`)

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

### G-5 — Low-Medium — `repair` comment (`crates/github/src/tracker.rs:1584`, `repair`)

The `by` argument went into a GitHub comment raw (`@org/team` pings, markdown). Every other
comment in the crate escapes names.

Fix: `render::escape_capped(by, 200)`.

Test: `tracker::tests::repair_comment_does_not_ping_the_name_it_was_given`.

### G-6 — Low-Medium — deleted reviewer (`crates/github/src/pr.rs:190`, `parse_review_summary`)

GitHub sends `"user": null` for a deleted account. That made `list_pr_reviews` fail, and with it
policy, merge and status for the PR, indefinitely.

Fix: attribute it to `ghost`, which no registered agent or owner can be (fails closed).

Test: `pr::tests::a_review_by_a_deleted_user_parses_as_ghost`.

### C-4 — Low-Medium — empty `$FL_DB` (`crates/cli/src/main.rs:287`, `explicit_db_from`)

`FL_DB=""` (CI templates export it when a variable is unset) became `Some("")`: it confined the
command to a store named "" and skipped the config binding. `config.rs` already treats an empty
XDG variable as unset.

Fix: an empty value is unset.

Follow-up: an empty `--db ""` still became `Some("")`. It is unset too now (it falls through to
`$FL_DB`, then the config and default tiers), the same convention as the environment value.

Test: `main::tests::empty_fl_db_is_unset` (both the environment value and the flag).

### C-5 — Low — `owner/repo#007` (`crates/cli/src/refs.rs:21`, `FromStr`)

`#007` produced `…/issues/007`, a different IRI string from `…/issues/7` for the same issue
(lookup miss, duplicate alias key); a number past `u64` fell out of the issue-URL filter.

Fix: parse as `u64` and format back; too large is an error.

Test: `refs::tests::issue_number_is_normalised_and_bounded`.

### C-6 — Low-Medium — recovery hint (`crates/cli/src/comment.rs:49`, `shell_word`; `:40`, `recovery`)

`recovery()` printed `fl github ledger comment {iri}` in a warning people and agents are told to
run. An IRI only has to be free of whitespace, so `urn:x:$(id);…`, a backtick or an ESC byte is a
valid one.

Fix: anything beyond the characters a plain URL holds is single-quoted; control characters are
shown as text.

Test: `comment::tests::the_recovery_command_quotes_an_iri_with_shell_syntax`.

Cosmetic leftover: bidi and other format characters (Unicode Cf) are not `is_control`, so they
pass through unchanged inside the quotes. They cannot change what the shell runs.

### P-4 — Low — `-->` in the marker (`pr_policy.rs:517`, `format_review_body`)

`extract_review_marker` ends at the first `-->`; `serde_json` does not escape `>`. A `project`
or `reviewer` containing `-->` made the marker unreadable, and a non-owner APPROVED review
without a readable marker blocks the merge.

Fix: write `>` as `>` (valid JSON, only occurs inside strings).

Test: `pr_policy::tests::review_marker_survives_an_arrow_in_a_field`.

### M-4 — Low-Medium — nameless named argument (`freeze.rs:588`, `Build::argument`)

`a.name.clone().unwrap_or_default()` made `["", value]` — `npx -y "" value pkg@1`.

Fix: a named argument without a name is refused.

Test: `freeze::tests::a_named_argument_without_a_name_is_refused_not_passed_as_an_empty_word`.

This is a behaviour change the review found: an OPTIONAL named argument with no name and no
value used to be skipped and is now refused as well (the check runs before the argument's value
is read). See [Behaviour change](#behaviour-change-freezing-refuses-more).

### M-5 — Medium-Low — no fsync before rename (`crates/mcp/src/sync.rs:562`, `stage`; `:766`, `write_record`)

`stage` and `write_record` renamed a temp file that was never `sync_all`'d. After a power loss
on a filesystem without rename-flush heuristics a `.mcp.json` / `.codex/config.toml` can come
back zero-length; an empty ownership record then fails every later plan with `McpError::Record`.

Fix: `sync_all` before `persist`. No test: a power loss is not deterministic and a call-order
test would only restate the code. The directory is still not fsynced (see O-12).

## Behaviour change: freezing refuses more

`fl mcp add` and `fl mcp upgrade` now REFUSE registry entries they used to freeze. A catalog
that was frozen earlier is not touched (an existing entry is never re-checked), but `upgrade`
re-freezes the new version and can hit a refusal the old version did not. The refusals, each
with the way on (add it by hand with `fl mcp add <name> -- <command>`):

* Any runtime argument outside the allow-lists of M-1:
  an unlisted npm/PyPI flag, a flag with a value, an attached form (`--from=x`, `-pevil`), a
  different spelling or case, any docker option beyond `-i`, `--rm`, `-e NAME=value` and a
  loopback `-p`.
* A registry environment variable named like one that repoints a fetch or a load (`npm_config_*`,
  `UV_*`, `PIP_*`, `NODE_*`, `PATH`, `HOME`, `LD_*`, `DYLD_*`, `PYTHON*`, `DOCKER_*`, `XDG_*`,
  `SSL_*`, the proxy variables and a few more, see M-1).
* M-4: an OPTIONAL named argument with no name and no value. It used to be skipped; it is refused
  now, with the required one.
* An optional or required positional RUNTIME argument with no value and no default (a
  `valueHint`-only placeholder) on npm and PyPI. It used to freeze to nothing (optional) or be
  refused as "required and has no value" (required); both are refused by the runtime list now,
  because fl cannot tell what the line would run.
* A named argument that was a boolean by name but carried a value (`--prefer-offline evil`).

A registry that serves entries of these shapes was already outside what fl could pin safely; the
trade is deliberate.

## Open items

Not fixed (O-3 and O-4 are fixed). "Read" = confirmed by reading only; nothing was run.

| # | Sev | Where | What | Why not fixed |
|---|---|---|---|---|
| O-1 | Medium | `mem.rs:302`, `apply_imported_state_revision` (called by `import_state_revisions`, :1748) | **mem.** `import_state_revisions`: a same-key collision rewrites `state_entries` (alias onto `rev.entry_id`, `remove` then `insert`) before the `Tampered` immutability check, so a revision whose `entry_id` is entry A and whose slug is B's destroys A. Also mutates before it can still fail. | The function is intricate and shared with sync semantics; the right fix (check before rewrite, atomic apply) deserves its own change with a hostile-input test. |
| O-2 | Medium | `mem.rs:1468`, `upsert_state` | **mem.** A failed `upsert_state` leaks a minted id and handle (mint before validate). | MemStore only; needs re-ordering mint and validation. |
| O-3 | Medium (design) | `pr_policy.rs:659`, `verify_review_integrity`; `:716`, `verify_merge_approvals` | The review marker's `reviewer`/`role` come from the review body and are not bound to `review.author_login`. Any account whose APPROVED review appears on the PR can claim to be `jarvis` as lab_manager. See the note below the table. | **Fixed** in the follow-up: `[github] reviewer_logins` (default the Reviewer App's `<slug>[bot]`) binds markers to the App login. |
| O-4 | Medium | `freeze.rs:864`, `runtime_argument_problem` | Registry runtime args were deny-listed, not allow-listed; docker's `--privileged`, `-v`, `--entrypoint` passed. | **Fixed** in the follow-up (see M-1). |
| O-5 | Medium | `freeze.rs:674`, `Build::variable` | `upgrade` keeps old literal env values over the new version's `value`/`default` (`kept_env` wins), so a changed fixed value is silently pinned at v1's. | Needs a record of which literals the user supplied versus fl froze. |
| O-6 | Medium | `catalog.rs:601`, `Editor::open`, and `:639`, `save` | `Editor::open`→`save` is a lost-update window with a registry network call in the middle (`cmd/mcp.rs` add/upgrade). `save` is now atomic but still overwrites from a stale document. | Needs a lock file plus compare-before-write; touches the CLI flow. |
| O-7 | Medium | `catalog.rs:385`, `check_server` | `check_server` accepts `http://` URLs (a secret `Authorization` over cleartext), a bad `scheme`/header name, an empty `command`; `is_scheme`/`is_token` run only in freeze and the CLI. | Whether loopback/`http` stays legal is a policy call. |
| O-8 | Low | `freeze.rs:588`, `Build::argument`, and `:674`, `variable` | A later non-secret variable of the same name overwrites a docker `-e NAME` secret with a literal. | Edge case needing a registry that does it. |
| O-9 | Low | `sync.rs:331`, `plan_target`; `:499`, `apply`; `Owned.root` (:77) | `apply` re-reads vendor files under the lock but not the ownership record; two overlapping `fl mcp sync` runs can clobber it. `Owned.root` is written and never read. | Rare; fix is a re-read under the lock. |
| O-10 | Low | `freeze.rs:1180`, `resolve` | `resolve` substitutes sequentially over the substituted string; order-dependent. The secret check runs first, so no leak found. | No exploit; single-pass rewrite. |
| O-11 | Low | `registry.rs:60`, `Registry::new`, and `:154`, `get` | A registry URL with `?`/`#` swallows the request path. | `Registry::new` handles the origin on purpose; whether such URLs are legal is a spec question. |
| O-12 | Low | `catalog.rs:639`, `save`; `sync.rs:499`, `apply` | Parent directories are not fsynced after the rename. A crash between creating and renaming leaves a temp file: `.fl-mcp-*` beside the vendor file in the project root for `sync`, and `.mcp.toml-*` in `.fl/` for the catalog, where `git add .fl` can pick it up. | Platform-specific; low value. |
| O-13 | Medium | `main.rs:328`, `db_path` (and the `create_dir_all` calls in `run`), `store/src/lib.rs:473`, `RedbStore::open` | The redb store and its directory are created with default permissions (0644 in 0755 under umask 022) while holding hub pairing and session data, wire messages and memories. The diff itself creates no new weakly-permissioned file. | **Open, not changed in this PR.** Compatibility with any shared-store setups needs a decision first. Recommendation: create the store's parent directory with `DirBuilder::mode(0o700)` when fl itself creates it; pre-create the redb file with `OpenOptions::mode(0o600)` before `Database::create`; warn when an existing store is group- or world-readable. It belongs in `db_path` and `fl_store`. |
| O-14 | Medium | `config.rs:196` (`wire.token_file`) and `:217` (`owner_token_file`) vs `hub/src/auth.rs:201`, `resolve_owner_token`, and `:220`, `resolve_peer_token` | The doc says the config `token_file` wins over `$FL_WIRE_TOKEN`/`$FL_HUB_OWNER_TOKEN`; the code does the opposite, and an unreadable `token_file` becomes `None` silently. | Lives in `hub` (out of scope); owner picks which precedence is right. |
| O-15 | Low-Medium | `config.rs:437`, `load`, and `:681`, `bound_entry` | Two unnamed project entries with the same basename (or a named one) resolve by file order; `bound_entry` compares `(store, tracker)` only. A command can pick another project's store. | Needs a rule for ambiguous entries (reject at load). |
| O-16 | Low | `config.rs:293`, `sync_target_for_scope`, and `load` | Store sharing is decided by literal path equality (a `..` or symlinked spelling reads as a different store, silently disabling wire/registry sync); one unresolvable root fails every command. The second is documented as deliberate. | Behaviour change; needs canonicalisation policy. |
| O-17 | Medium | `tracker.rs:1933`, `create_escalated` | `create_escalated` adopts any issue whose `fl:meta` block has the expected `create_key`, with no check on the author or on `escalated.from`. In a repo where outsiders can open issues, one could make fl adopt an attacker-authored issue as the escalation target. | Needs the tracker's identity (`identity()`), which differs between PAT and App; a careful change in a 5300-line file. |
| O-18 | Low | `tracker.rs:885`, `ensure_labels`; `:2256`, `add_alias`; `meta.rs:353`, `labels_after`; `:468`, `parse_body` | Label existence is case-sensitive (a repo's `FL:Record` 422s on create); `add_alias` check-then-write race; `labels_after` drops any `fl:`-prefixed label, not only fl's; prose does not round-trip `parse_body` for `\r\n`/trailing space. | Each is minor and needs a decision about normalisation. |
| O-19 | Low-Medium | `arch.rs:199`, `validate_architecture_model`; `:323`, `check_architecture_conformance`; `:457`, `escape_likec4_string` | Duplicate `package` across components is not rejected (later overwrites); a glob with a trailing `/` validates but never matches its contents; `strict_coverage` does not flag an unmapped leaf package. `escape_likec4_string` leaves `\n`. | `arch` semantics are a spec call. |
| O-20 | Low-Medium | `code_index.rs:463`, `index_file`; `:882`, `record_token_gain_index` | `scan_code_index`: `symlink_metadata` then `File::open` follows a swapped-in symlink; no total file/byte cap, one `String` per line. `record_token_gain_*` is a read-modify-write on a store with no CAS; the hub calls it per request, so concurrent queries lose counts. | The counter is a statistic; a CAS needs a store-trait change. |
| O-21 | Low | `ferrostate.rs:494`, `section_kind`; `:562`, `parse_handoff`; `:75`, `resolve_workspace_root` | `section_kind` matches substrings ("Not done", "Abandoned" → Shipped); `parse_handoff` ignores code fences; `resolve_workspace_root` can pick `~` when `~/Notes` exists. | Heuristic parsers; no data loss. |
| O-22 | Low | `routing.rs:96`, `with`; `:152`, `after_set` | `with`/`after_set` do not validate the area name (only `check()` does). | A library caller can write a map `check()` then rejects. |
| O-23 | Medium | `pr_policy.rs:346`, `evaluate_pr_policy` | A `Co-authored-by: <agent>` trailer on a commit by ANY login counts as attributed, so a foreign commit with an agent trailer gets a Lead/LabManager tier and needs no owner trailer (the P-1 laundering class). | Needs a rule for who may author an attributed commit. Fix: an allow-list of author logins for attributed commits (the owner's login and the agents' machine account), or verified signatures. |
| O-24 | Medium | `pr_policy.rs:369`, `is_owner_commit` in `evaluate_pr_policy` | Owner authorship is decided from GitHub's `author.login`, which GitHub derives from the commit author email. The owner's noreply email is public in `git log`, so anyone can author a commit as the owner login. | Same fix as O-23 (author-login allow-list or verified signatures); it needs the signing or login policy decided. |

### O-3 — review marker not bound to the review's login (Medium, fixed)

Rated Medium, not High, after review. `verify_review_integrity` (`pr_policy.rs:659`) and
`verify_merge_approvals` (`:716`) read `reviewer` and `role` from the marker in the review body
and never compare them with `review.author_login`. Any account whose APPROVED review appears on
the PR can claim to be `jarvis` as lab_manager. GitHub's ruleset would not count an outsider's
approval, but fl does. The same loophole lets anyone make `verify-ci` refuse: `verify_review_integrity`
refuses any non-owner APPROVED review on the head that has no marker.

What it does NOT affect:

* The owner tier. An agent marker can never satisfy `RequiredTier::Owner` (`authorize_agent_action`
  always errs for it), and the owner is matched by login. A protected-path PR is safe.
* The forgery only substitutes for a Lead/LabManager review on an unprotected, attributed PR.

Why the impact today is limited: `verify-ci` is not in any workflow under `.github`; `--by` is
self-asserted; every local agent shares the Reviewer App key; the ruleset's approval requirement
and the owner's merges are independent controls.

Recommendation: honour an agent marker only when `review.author_login` equals the configured
Reviewer App login (`<slug>[bot]`, for example a `[github] reviewer_logins` entry in the config,
or taken from `identity()`). Ignore marker-bearing reviews from other logins for tier purposes,
and drop the marker-less refusal for unknown logins. The design spec
(`docs/superpowers/specs/2026-10-26-github-hierarchy-design.md`, `verify-ci`) already scopes the
integrity checks to `<slug>[bot]` reviews. Telling the individual agents apart behind the one App
still needs per-agent signing or an authenticated `--by`. "Unfixable without signing", as the
cloud report first put it, is too strong.

Considered and dropped: `FL_GITHUB_API_URL` accepts any https host and sends the credential there
(documented behaviour, needs control of the environment); eager GitHub open in `main.rs:1112`, the `open_github` call in `run`
(intended); `Iri::parse` accepting control characters and `..` (nothing in core builds paths from
it — callers that do must not trust it, see C-6 for one that printed it).

## Landing note

Written by an autonomous cloud agent and independently reviewed twice before landing. Claims
confirmed real and fixed: P-2, P-3, P-4, G-1 to G-6, F-1, E-2 (MemStore), E-4, M-2 to M-5, C-1
to C-6. Partial before the review follow-up: P-1, M-1, E-3; M-1 and E-3 are fixed by the
follow-up, P-1 stays partial (O-23, O-24). O-3 and O-13 are left open for decision. The review
follow-up also fixed the catalog's new-file mode, the `version()` test (M-3), the empty `--db`
(C-4) and the P-2 literal-path narrowing, and corrected this report. Not changed, on purpose:
`index_message_id` in the redb store (E-2) and everything in the open-items table other than O-4.
