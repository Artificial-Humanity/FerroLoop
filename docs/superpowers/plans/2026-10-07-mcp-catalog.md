# MCP catalog — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a project one committed catalog of the MCP servers it uses (`.fl/mcp.toml`), a read-only client for an MCP registry that freezes a server's launch spec into the catalog, and a writer that turns the catalog into Claude Code's, Codex's and Antigravity's own project MCP files — adding, changing and removing only the entries fl wrote, all or nothing, with no network and no secret value anywhere.

**Architecture:** A new crate `fl-mcp` holds the catalog model and its editor (`catalog`), the registry client with its guards and a test fake (`registry`, `fake`), the freeze of a registry entry into a launch spec (`freeze`), one writer per vendor behind a `Vendor` trait (`vendor`), and the plan-and-apply of every target file with ownership records (`sync`). `fl-cli` adds `fl mcp …`, which returns before any store is opened, reads the machine switches from a new `[[mcp]]` table in the user's config, and runs the git checks the crate cannot (fl-exec owns git).

**Tech Stack:** Rust 2024 (`rust-version = "1.98"`), serde/serde_json (`raw_value`; **never** `preserve_order`), `indexmap` (`serde`), `toml` and `toml_edit` 0.25, ureq 3.4 (rustls), sha2, tempfile, tiny_http (the fake), clap 4, assert_cmd/predicates.

**Spec:** `docs/superpowers/specs/2026-09-29-mcp-catalog-design.md` rev 2 (rev 1 approved by the owner 2026-09-29; rev 2 folds in the owner's decisions 10–11 of 2026-10-07 and 12–15 of 2026-10-08, and the corrections under "Spec defects" below, each marked `[agent]`). The code map the plan was written against is `main` at `38129bd`.

**Branch:** `ferris/mcp-catalog`, off `main` once this plan is merged.

---

## Global Constraints

- Verification trio, all green before every commit: `cargo fmt --all --check`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`. Each task runs `cargo fmt --all` first, so code blocks here need not be in rustfmt's exact layout; lines stay within 100 columns.
- Unit tests live in `#[cfg(test)] mod tests` inside the module they test; black-box CLI tests live in `crates/cli/tests/`.
- **No test contacts the network**, except the one live test (Task 9), which is `#[ignore]`d and runs only by hand. Everywhere else the registry is the in-process fake (`fl_mcp::fake::FakeRegistry` on `127.0.0.1`), named in the test catalog as `registry = "http://127.0.0.1:<port>"`.
- **No test touches the real home.** Every black-box test of `fl mcp` sets `HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_DATA_HOME` to a temporary directory, and unsets `CODEX_HOME`; Task 8's guard test fails if any path fl resolved lies outside that directory (spec §7.1, an invariant for tests).
- **`serde_json`'s `preserve_order` stays off** for the whole build: the GitHub ledger needs a `Value`'s keys sorted, and `ledger::layout::tests::json_objects_encode_with_their_keys_sorted` guards it (`Cargo.toml:15-17`). Key order in vendor JSON files is kept with `indexmap` and `serde_json::value::RawValue` (plan ruling 1).
- Spec invariants, verbatim: "Committed; the only source of truth for which servers a project uses" (§2.1); "The launch spec is frozen into the entry when it is added: `sync` reads nothing else, so it needs no network and gives the same output on every machine" (§2.1); "fl never reads, stores or writes a secret's value" (§2.1); "fl never changes or removes an entry it did not write" (§4.3); "All or nothing: every target is planned before any is written; one refusal writes nothing" (§4.3); "A registry that cannot be reached fails `search`, `add --from` and `upgrade` only. `sync` and `check` have no network path" (§5); "No secret value in the catalog, a vendor file, a record, argv, or an error message" (§6); "Nothing the registry says changes what runs without a commit to the catalog" (§6); "No dependency may need a runtime service or a program outside the binary" (§1.1).
- Spec values, verbatim: server names `[a-z0-9-]`, 1–32 chars (§2.1); the registry API version `v0.1` (§3.1); a registry body over 4 MiB is refused (§3.1); `fl mcp search` reads up to 20 pages and says so when it stops early (§3.1); `fl mcp check` exits 0 (matches), 1 (a `sync` would change something), 2 (a refusal, a divergence, or an error) (§4.5); errors exit 2 and print `error: …` (§5).
- **Every existing configuration keeps working:** a config with no `[[mcp]]` table and a project with no `.fl/mcp.toml` behave exactly as before. Every existing test passes, except the assertions a task changes on purpose (named in that task).
- **Every guard gets a mutation check**: revert it, watch the named test go red, restore, confirm with `cmp` against a saved copy. Each task's mutation step lists every guard the task adds — each conjunct of a compound condition, each match arm that must honour an input, and ordering where order matters. Where a line looks like a guard but no input can tell it apart, the step says so and why.
- **A unit-test filter in a mutation step names the full module path** (`cargo test -p fl-mcp --lib sync::tests::`), or it matches nothing. A black-box filter names the test file (`cargo test -p fl-cli --test mcp -- <name>`). At most one filter goes before `--`.
- **A test never asserts with a substring another code path also produces.** Each task names the unique phrase it asserts.
- **No plan names, task numbers or review labels in code comments.** Cite the spec by section ("MCP spec §4.3"), or say the reason. No decision numbers in user-facing help or error text.
- **A change to shared plumbing states its blast radius** in the task that makes it.
- Line numbers cite `38129bd`; an earlier task's edits shift them. Find the named item, not the number.
- Commits are authored by the machine account (`WORKFLOW.md`) and end with `Co-authored-by: Ferris <Ferris@artificialhumanity.io>`. Stage explicit paths — never `git add -A` — and after each commit run `git status --porcelain`, which must print nothing.
- Build output stays in the worktree's own `target/` — never under `/tmp`, which is shared RAM on the build machine.
- The repository is public: no machine paths, host names, user names or lab names in code, tests, messages or docs. Fixture servers are `io.example/…`; fake repositories are `acme/widgets`.

## Review Focus

1. **A secret, set in the environment with a real-looking value, during `add` and `sync`.** It must appear in no catalog, vendor file, ownership record, argv of any process fl starts, or error message. Task 8 (`no_secret_value_reaches_any_file_fl_writes_or_any_message`).
2. **A vendor file someone else edited between `sync`'s plan and its write, or edited by hand since fl wrote it.** Refused, naming the file, the entry and the remedy, and nothing at all is written (every target byte-identical). Task 6 (`a_file_changed_between_plan_and_write_is_refused_and_nothing_is_written`, `a_hand_edited_entry_is_refused_naming_the_remedy`).
3. **A vendor file with foreign entries, comments, or a layout fl did not make.** Foreign entries survive byte for byte, key order is kept, Codex's comments survive; a JSON file with comments or trailing commas (Antigravity accepts JSONC) is refused, never rewritten. Task 5 (`foreign_entries_and_key_order_survive_a_rewrite`, `a_jsonc_file_is_refused_not_rewritten`, `codex_comments_and_layout_survive`).
4. **A registry entry fl cannot freeze honestly** — a deleted server, several launch routes, an OCI image without a tag, a package that serves HTTP locally, a secret inside an argument template, a required argument with no value. Refused before anything is written, each naming what to do. Task 4 (`each_unfreezable_entry_is_refused_naming_the_way_on`).
5. **A registry that misbehaves** — a redirect off its origin, an HTML 502, a body over 4 MiB, a 404, a non-https URL that is not loopback. Each is the registry's failure with its own message; none is a parse error and none follows the redirect. Task 3 (`each_misbehaving_registry_is_refused_with_its_own_message`).

## Rulings this plan makes

The spec leaves these open or ambiguous. Each says why, and what it costs if wrong.

1. **Plan ruling: key order in vendor JSON files is kept with `indexmap::IndexMap<String, Box<RawValue>>`** for the top level and for `mcpServers`, not with `preserve_order` (banned workspace-wide, see Global Constraints). Foreign entries are kept byte for byte, not re-indented; only fl's own entries and the separators around them are written fresh. *If wrong:* a foreign entry's inner whitespace is left as it was, which the spec allowed to change anyway.
2. **Plan ruling: a JSON vendor file that does not parse as strict JSON is refused, never rewritten** — Antigravity accepts JSONC (comments, trailing commas) and a byte-order mark; fl would lose them. The refusal names the file and says to remove the comments or the entry by hand. *If wrong:* a person with a commented Antigravity file removes the comments once.
3. **Owner decision 10 (2026-10-07), as this plan carries it: Antigravity gets a project file, `<root>/.agents/mcp_config.json`, like the other vendors** — generated, gitignored, plain server names, a default vendor. It replaces decision 6's machine-wide opt-in: the `[[mcp]]` table has no `antigravity` key, the catalog no `project` field (its only use was the machine-wide name), and the §4.3 row about two projects sharing a name goes. A live check with agy 1.2.17 on 2026-10-07 showed agy loads that file in a session. *If wrong:* a session started outside the repository does not see the project's servers.
4. **Owner decision 11 (2026-10-07): for Antigravity, a secret environment variable is left out of the entry** (a stdio server inherits agy's environment, checked live) **and a server with a secret header is refused for Antigravity only**, naming the reason; the other vendors still get it. agy expands no `${NAME}` (checked live: the literal text reached the server). *If wrong:* none — no secret value is ever written.
5. **Plan ruling: Antigravity's remote key is `serverUrl`**, which both agy's own docs (as the legacy key) and the web docs accept; both connected in the live check. SSE is refused for Antigravity as for Codex (agy: "The legacy HTTP+SSE transport … is not supported"). *If wrong:* a key rename the next agy accepts anyway.
6. **Plan ruling: the project root is the nearest ancestor of the working directory that holds `.fl/mcp.toml`; with none, the nearest that holds `.git`**, where the first `fl mcp registry` or `fl mcp add` creates the catalog. No `.git` and no catalog is refused. fl-mcp calls no git to find it. *If wrong:* a catalog in a subdirectory of a repository is found from below it only.
7. **Plan ruling: the `[[mcp]]` lookup shares the longest-ancestor rule with `[[project]]`** through one generic helper factored out of `config::bound_entry`; two `[[mcp]]` entries on one root are refused unless identical. *If wrong:* none.
8. **Plan ruling: `fl mcp` returns from `run()` right after the config is loaded**, before any store path is resolved or created; `--db` (a global flag) is refused with `fl mcp`, naming why. *If wrong:* none.
9. **Plan ruling: the registry client is fl-mcp's own, with the guards written there** (spec §9 left "moved to a shared place … decided in the plan"): https only, or http to `127.0.0.1`, `localhost` or `[::1]` with no userinfo; no redirect followed; the status classified before the body is parsed; a body over 4 MiB refused; a `text/html` answer reported as the registry's failure. fl-github's guards live partly in fl-cli and have no body cap or HTML check, so a shared crate would be new code either way; fl-mcp stays free of fl-github. *If wrong:* two similar guards to keep in step.
10. **Plan ruling: `fl-mcp` does not depend on `fl-core`** — it needs nothing from it; the spec's "depends on fl-core only" is read as "at most". *If wrong:* one line in Cargo.toml.
11. **Plan ruling: names and versions in registry paths are percent-encoded by a small encoder in fl-mcp** (unreserved characters kept, every other byte `%XX`), so `/` and `+` are sent as `%2F` and `%2B`. No new dependency. *If wrong:* none; the registry 404s an unencoded `/`.
12. **Plan ruling: `fl mcp search` uses the registry's own `search` parameter (a case-insensitive substring of the server name) with `version=latest`**, up to 20 pages, and keeps only the servers whose name holds the text, in any case (owner decision 13). The registry searches names only (checked 2026-10-07); a match only in a description is not shown. *If wrong:* none; owner decision 13 settles it.
13. **Plan ruling: `add --from` and `upgrade` ask with `include_deleted=true`** and refuse a `deleted` status naming its `statusMessage`; `deprecated` is a warning. The registry hides deleted entries otherwise (a 404). *If wrong:* none.
14. **Plan ruling: an OCI package's `identifier` is used as given** (since schema 2025-10-11 it carries the tag); one with neither a tag nor a digest is refused (the pin would not be exact). The launch is `docker run -i --rm` + the runtime arguments + one `-e NAME` per environment variable + the identifier; **a package whose runtime arguments include a positional argument is refused**, suggesting `fl mcp add <name> -- <command>` by hand, because older entries spell the whole `docker run` line as arguments and fl would duplicate it. *If wrong:* such an entry is added by hand once.
15. **Plan ruling: a named argument renders as two arguments, `<name> <value>`; a named `boolean` argument renders as `<name>` when `true` and not at all when `false`; a positional renders as its value.** `{var}` templates resolve from the argument's `variables` (`value`, then `default`). *If wrong:* an entry that wanted `--flag=value` is added by hand.
16. **Plan ruling: a secret inside an argument is accepted in one shape only — a docker `-e NAME={var}` whose variable is secret — and becomes `-e NAME` plus a secret reference for `NAME`.** Any other argument whose template names a secret variable is refused. An *optional* argument (`isRequired` false) that carries a secret is left out unless `add` names it with `--with <NAME>`, because Claude Code passes an unset `${NAME}` through as literal text (so an optional token would break the server's own fallback). *If wrong:* one more flag than needed.
17. **Plan ruling: a package whose transport is not `stdio` is refused** — it serves HTTP on this machine and needs starting, which fl does not do (spec §0.2). *If wrong:* such a server is added by hand.
18. **Plan ruling: a secret header the registry gives no value is recorded with no scheme and an env name fl derives, `<SERVER>_<HEADER>` uppercased with `-` as `_`** (e.g. `GITHUB_AUTHORIZATION`); the variable holds the whole header value, and `add` prints the name to set. A header value template such as `Bearer {token}` records `scheme = "Bearer"` and an env name from the variable. For Codex, a secret `Authorization` with scheme `Bearer` is `bearer_token_env_var`, a secret header with no scheme is `env_http_headers`, and any other scheme is refused for Codex (it cannot add a prefix). *If wrong:* a person renames one variable.
19. **Plan ruling: Codex's trust check computes the key Codex uses** — the project root, else the git root — and matches it exactly against `[projects."<key>"] trust_level = "trusted"` in `$CODEX_HOME/config.toml` (default `~/.codex/config.toml`), read only; an ancestor's trust does not count (Codex's own rule, checked 2026-10-07). An untrusted project is a warning, not a refusal. *If wrong:* a warning that should not show.
20. **Plan ruling: Claude Code's reserved server names `workspace`, `computer-use` and `claude-in-chrome` are refused as catalog names** (it skips them, checked 2026-10-07; its other reserved names, `Claude Preview` and `Claude Browser`, already break the `[a-z0-9-]` rule). *If wrong:* none.
21. **Plan ruling: the gitignore guard runs in fl-cli** (git lives in fl-exec), for `.mcp.json`, `.codex/config.toml` and `.agents/mcp_config.json`, using a new `Git::is_tracked` beside `Git::is_ignored`; `sync` and `check` both refuse a tracked or unignored target and print the `.gitignore` lines to add. fl does not edit `.gitignore`. *If wrong:* none.
22. **Plan ruling: the catalog is read with `toml` (serde, `deny_unknown_fields`) and edited with `toml_edit`**, so a person's comments in `.fl/mcp.toml` survive `add`, `remove`, `enable`, `disable`, `registry` and `upgrade`. *If wrong:* none.
23. **Plan ruling: ownership records live under `$XDG_STATE_HOME/fl/mcp/`** (default `~/.local/state/fl/mcp/`; a new `config::state_dir` beside `data_dir`), one JSON file per target named by the SHA-256 of the target's canonical path, plus a lock file `sync.lock` taken with `std::fs::File::lock`. *If wrong:* none.

24. **Plan ruling: an entry fl wrote that is gone from the file while the catalog no longer wants it is forgotten** — the record drops it, no file is written. Spec §4.3's "the entry is gone → refuse" protects a person's removal against fl; here the person's removal and the catalog agree. When the **whole file** is gone (a fresh clone, `git clean -fdX`), every recorded entry the catalog still wants is added again and the rest are forgotten (owner decision 14); an entry gone from a file that still exists, and still wanted, is refused as a hand edit. *If wrong:* none; nothing is lost.
25. **Plan ruling: an entry in the record whose bytes differ from the record but equal what fl would write now is adopted**, not refused as hand-edited (the exact-match check runs first). *If wrong:* none.
26. **Plan ruling: `fl mcp check` reports the files, not the records** — an adopt or a forget changes only the record and keeps exit 0 (spec §4.5: "every target matches the catalog"). *If wrong:* none.
27. **Plan ruling: `apply` re-reads every target it will touch before writing any of them** (stronger than spec §4.3's "each target read again just before its write"), so a change found late writes nothing. A second `sync` waits for the lock (one records directory serves every project on the machine). *If wrong:* none.
28. **Plan ruling (from Task 5): a secret read from a variable of another name (`env.KEY = { secret = true, env = "OTHER" }`) is refused for Codex and Antigravity**, which pass a variable to the server under its own name; Claude Code writes `"KEY": "${OTHER}"`. A JSON vendor file with a duplicate key, or whose top level or `mcpServers` is not an object, and a Codex file with `mcp_servers` written inline, are refused. *If wrong:* a person renames one variable.
29. **Plan ruling (from Task 6): a refusal names the fields that differ, never their values** — a hand-edited entry may hold a pasted token (spec §6). *If wrong:* a less specific message.
30. **Plan ruling (extends 16): an optional secret environment variable (`isSecret`, not `isRequired`) is left out unless `add` names it with `--with <NAME>`**, for ruling 16's reason — Claude Code passes an unset `${NAME}` through as literal text. A required secret stays a reference. An environment variable whose value template names a secret variable, and a secret header whose value is `{var}` or `<scheme> {var}`, are recorded as references; any other secret header shape is refused. *If wrong:* one more `--with`.
31. **Plan ruling (found by the live test, 2026-10-08): the registry serves entries that break its own schema** — `io.github.Credda-io/credda-trust` carries an argument of type `flag`, which schema 2025-12-11 does not define, and the whole `search` failed to parse. So **`search` reads only the fields it shows** (name, description, version, status — leniently, an unknown status shown as is), and **an argument's `type` is open** (`Other(String)`, like `registryType`); Task 4's freeze refuses an argument type it does not know, naming it. One malformed server never breaks a search. *If wrong:* none.
32. **Plan ruling (from Task 7): the gitignore guard runs after `sync`'s read-only plan and before anything is printed or written, and checks only targets that hold or will hold an fl entry** — a vendor no server is for, or a file with only someone else's entries, needs no ignore line. *If wrong:* none.
33. **Plan ruling: `sync` and `check` with no catalog are refused, not read as an empty catalog** (which would remove every entry fl wrote); there is no default registry (owner decision 12). *If wrong:* one `fl mcp registry` command.
34. **Plan ruling: by hand, `--env NAME` (no value) records a secret reference, and every `--header NAME=ENV[:SCHEME]` is a secret reference** whose NAME (for `--env`) and ENV (for `--header`) must look like a variable's name — capital letters, digits and `_` — so a pasted token is refused and never echoed; a hand-added `--url` is streamable HTTP. *If wrong:* a literal header is added by editing the catalog.
35. **Plan ruling: `--db` is refused with `fl mcp`; `$FL_DB` is ignored** (ambient), and no store is created. *If wrong:* none.
36. **Plan ruling: no catalog error, parse or validation, repeats a value** — a rule's refusal names the server and the field, never the value (not a variable's name, not a registry's host); and a parse error never quotes the file's text — for the catalog, as for Codex's config and Antigravity's JSON, only the position (the line and the column; for Codex's config, the line) and the parser's message (spec §6: no secret value in an error message). The catalog's parser also quotes the key or value it could not read back (``unknown variant `…` ``, `invalid type: string "…"`), so in its message everything from the first quote to the last before the `, expected` clause becomes `<value>`; the `expected` clause, fl's own messages and `missing field` stay. Codex's and Antigravity's files are read for syntax only, and neither parser's syntax messages quote a value (checked 2026-10-08). *If wrong:* a less helpful message.
37. **Plan ruling: `fl mcp add` refuses every flag that does not belong to the form used** — by-hand stdio (`-- <command>`), by-hand URL (`--url`), or from the registry (`--from`) — naming the flag, rather than ignoring it. *If wrong:* none.
38. **Plan ruling: a vendor target that is a symbolic link, or whose path below the project root passes through a symbolically linked directory (`.codex`, `.agents`), is refused**, naming the path: fl writes only plain files it can see whole. A link could carry the write into a file git tracks — which the gitignore guard cannot see, since it asks about the link's own path — or out of the project. The root is canonicalized first, so a project reached through a link is fine; only the components below it are checked. *If wrong:* a person replaces a link with a plain file.
39. **Plan ruling: an entry fl wrote whose bytes differ from its record but whose meaning equals what fl would write — the same JSON value, or the same TOML table with its keys in any order — is adopted**, recording the bytes as they stand; the file is not rewritten. A formatter or agy's own panel may re-lay a file out. A change of meaning is still refused as a hand edit. The plan prints `adopt <name> (means what fl would write)` for an exact match and a re-laid entry alike. *If wrong:* none.
40. **Owner decision 12 (2026-10-08): no default registry.** `search`, `add --from` and `upgrade` are refused until the catalog names one; the refusal prints the official address as an example.
41. **Owner decision 13 (2026-10-08): search by name, through the registry.** fl sends the registry's `search` and also filters what comes back client-side by name (case-insensitive), so a registry that ignores the parameter cannot flood the list. Spec §3.1's "name and description" is amended to name.
42. **Owner decision 14 (2026-10-08): a vendor file absent as a whole is recreated from the catalog** — after `git clean -fdX` or a fresh clone, fl's own generated file is gone; `sync` writes it anew and replaces the record's entries for it. A file that exists but lost an fl entry is still refused as a hand edit (spec §4.3).
43. **Owner decision 15 (2026-10-08): a machine switch naming a server the catalog no longer has is a warning, not an error** — `sync` and `check` print a `warning:` naming the stale switch and carry on (spec §2.2 amended).

## Spec defects this plan found

Each is amended in the spec's rev 2 (same file), marked `[agent]` unless it is an owner decision.

1. **§1.1 "`serde_json` with `preserve_order`".** Banned workspace-wide by the GitHub ledger. Plan ruling 1.
2. **§0.1 decision 6 and §4.1's Antigravity row.** agy loads a project file. Owner decision 10; plan ruling 3.
3. **§4.2 Antigravity `${NAME}`** (open until checked). Checked: not expanded; stdio inherits. Owner decision 11.
4. **§4.2 SSE.** Antigravity has no SSE either. Plan ruling 5.
5. **§3.1 search "by name and description".** The registry searches names only. Plan ruling 12.
6. **§3.2 deleted servers.** Hidden by default. Plan ruling 13.
7. **§3.2 OCI `<identifier>:<version>`.** The identifier carries the tag; runtime arguments vary by entry. Plan rulings 14, 15.
8. **§3.2 secrets.** A secret can sit in an argument template; a secret header can have no value; a package can serve HTTP. Plan rulings 16, 17, 18.
9. **§4.2 Codex secret headers.** `env_http_headers` takes the whole value. Plan ruling 18.
10. **§4.1 Codex trust.** Exact key, no ancestor. Plan ruling 19.
11. **§9 shared guards.** Plan ruling 9.
12. **§2.1/§1.2 how `fl mcp` finds its root.** Plan ruling 6.
13. **§3.1 the registry's own schema.** The registry serves entries outside it (an argument `type` of `flag`, seen 2026-10-08, failed a whole search), so a search is read leniently and an unknown argument type is refused at freeze. Plan ruling 31.
14. **§0, §1.1, §1.2, §2.1, §4.2–§4.4, §7.2, §7.3: residues of rev 1.** `preserve_order` (ruling 1); Antigravity's cells in §4.2 (decision 11) and the §4.3 row about two projects sharing a `project` name (decision 10); "re-indented" (ruling 1); the third vendor file in §4.4; the opt-in notice and the separator check (decision 10); the CLI's `--with`, `--header NAME=ENV[:SCHEME]`, `--env NAME` and `upgrade`'s flags (rulings 16, 18, 30, 34); a secret header's `env`; how the root is found (ruling 6). Each amended in rev 2.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `Cargo.toml` | modify | member `crates/mcp`; workspace deps `toml_edit`, `indexmap`; `serde_json` `raw_value` stays a crate-level feature |
| `crates/mcp/Cargo.toml` | create | fl-mcp |
| `crates/mcp/src/lib.rs` | create | modules, `McpError` |
| `crates/mcp/src/catalog.rs` | create | `.fl/mcp.toml` model, validation, editor; `Switches` |
| `crates/mcp/src/registry.rs` | create | the read-only client and its guards |
| `crates/mcp/src/fake.rs` | create (feature `fake`) | `FakeRegistry` on tiny_http, fixtures |
| `crates/mcp/src/freeze.rs` | create | registry `server.json` → a catalog entry, or a refusal |
| `crates/mcp/src/vendor/{mod,claude,codex,antigravity,json}.rs` | create | `Vendor` trait, each vendor's file shape, the order-keeping JSON file |
| `crates/mcp/src/sync.rs` | create | desired entries, the plan, ownership records, apply |
| `crates/exec/src/git.rs` | modify | `Git::is_tracked` |
| `crates/cli/src/config.rs` | modify | `[[mcp]]` (`McpEntry`), the shared longest-ancestor helper, `state_dir` |
| `crates/cli/src/cmd/mcp.rs` | create | `fl mcp …` |
| `crates/cli/src/main.rs`, `cmd/mod.rs` | modify | `Command::Mcp`, the early return, `--db` refused |
| `crates/cli/Cargo.toml` | modify | fl-mcp (and its `fake` for tests) |
| `crates/cli/tests/mcp.rs` | create | black-box tests |
| `docs/mcp.md` | create | for a person |
| `docs/README.md`, `README.md`, `docs/getting-started.md` | modify | links; the help block |
| `docs/superpowers/specs/2026-09-29-mcp-catalog-design.md` | modified with this plan (rev 2) | decisions 10–15, the fourteen defects |

## Tasks

| # | Task | Crates |
|---|---|---|
| 1 | The catalog: model, validation and an editor that keeps comments | mcp |
| 2 | The machine switches and the state directory in the user's config | cli |
| 3 | The registry client, its guards, and a fake registry | mcp |
| 4 | Freezing a registry entry into the catalog, or refusing it | mcp |
| 5 | The vendor files: one writer per vendor, foreign entries kept | mcp |
| 6 | `sync`: the plan, ownership records, all or nothing | mcp |
| 7 | `fl mcp` in the CLI | cli, exec |
| 8 | Secrets, the home guard, and the docs | cli, docs |
| 9 | The live registry test | mcp |

---

### Task 1: The catalog: model, validation and an editor that keeps comments

The catalog is committed at `<root>/.fl/mcp.toml` and is "the only source of truth for which servers a project uses" (MCP spec §2.1). This task creates the crate `fl-mcp` (plan ruling 10: no dependency on fl-core) with its error type, `McpError`, whose every refusal names the file, the entry and what to do next (§5), and the module `catalog`: the model of the file as rev 2 gives it (no `project` field), read with `toml` under `deny_unknown_fields` at every level (§2.1, plan ruling 22); the spec's rules, each refusing with the file, the server, the field and a remedy (§2.1); Claude Code's reserved names `workspace`, `computer-use` and `claude-in-chrome` (plan ruling 20); the registry URL check the registry client reuses (plan ruling 9); and an editor over `toml_edit::DocumentMut` that adds, removes, switches and replaces one entry and sets the registry while keeping a person's comments and every other entry byte for byte (plan ruling 22). An `env` or header value is a literal or a secret reference, and the model has nowhere to put a secret's value (§2.1); `{ secret = false }` is refused, not read as a literal. `Server::literal_values` names the values `add` must warn about (§6). A catalog that does not parse is refused with the line, the column and the parser's message only, never the parser's quote of the line, which can hold a pasted token (§6, plan ruling 36); and the parser's message itself quotes the key or value it could not read (``unknown variant `…` ``, `invalid type: string "…"`), so everything from the first quote to the last before its `, expected` clause is replaced by `<value>`. A rule's refusal names the server and the field, never the value: not a variable's name, not a registry's host (plan ruling 36). The `expected` clause is the catalog's own vocabulary and stays, as do fl's own messages and serde's `missing field`. `toml` hands a table's keys over sorted, so the model's maps are `BTreeMap`s, by name; only the editor keeps the file's own order. Each edit is checked as a whole catalog before it is kept, so a refused edit changes nothing.

**Blast radius:** the workspace `Cargo.toml` gains the member `crates/mcp` and two workspace dependencies, `toml_edit = "0.25"` and `indexmap` (`serde`); `Cargo.lock` gains `fl-mcp` and `toml_edit` and nothing else (every other dependency of `toml_edit` is already locked), and `indexmap` 2.14.2, locked but never compiled until now, is compiled through `toml_edit`. fl-mcp turns on `serde_json`'s `raw_value`, which Cargo unifies into the one `serde_json` the workspace builds: it adds the `RawValue` type and changes no `Map` ordering, so `ledger::layout::tests::json_objects_encode_with_their_keys_sorted` stays green; `preserve_order` stays off. `indexmap` and `serde_json` are declared now and first used by the vendor files. No existing crate's code changes.

**Files:**
- Modify: `Cargo.toml` (member `crates/mcp`; workspace dependencies `toml_edit`, `indexmap`)
- Modify: `Cargo.lock` (by cargo)
- Create: `crates/mcp/Cargo.toml`
- Create: `crates/mcp/src/lib.rs` (`pub mod catalog`; `McpError`)
- Create: `crates/mcp/src/catalog.rs` (model, rules, `check_registry_url`, `Editor`; tests)

**Interfaces:**
- Consumes: nothing from an earlier task.
- Produces (`fl_mcp`):
  - `McpError` (`Debug`, `thiserror::Error`), variants `Io { op: &'static str, path: PathBuf, cause: String }`, `Parse { path: PathBuf, cause: String }`, `Invalid { path: PathBuf, server: Option<String>, field: String, problem: String, next: String }`, `NoSuchServer { path: PathBuf, name: String }`, `AlreadyPresent { path: PathBuf, name: String }`. `Invalid` displays as ``<file>: server `<name>`, field `<field>`: <problem>. <next>`` (without ``server `<name>`, `` when `server` is `None`).
  - `catalog::{Catalog, Server, VendorName, Transport, EnvValue, HeaderValue, Editor, RESERVED_NAMES, HEADER}`; `Catalog::path(root: &Path) -> PathBuf`, `Catalog::load(root: &Path) -> Result<Option<Catalog>, McpError>`, `Catalog::parse(text: &str, file: &Path) -> Result<Catalog, McpError>`; `Server::is_for(&self, vendor: VendorName) -> bool`, `Server::literal_values(&self) -> Vec<String>`; `EnvValue::secret_var<'a>(&'a self, key: &'a str) -> Option<&'a str>`; `VendorName::ALL`, `VendorName::as_str(self) -> &'static str`, `Transport::as_str(self) -> &'static str`; `catalog::is_server_name(&str) -> bool`, `catalog::is_env_name(&str) -> bool`, `catalog::check_registry_url(url: &str) -> Result<(), String>` (the `Err` is a clause); `Editor::open(root: &Path) -> Result<Editor, McpError>`, `path(&self) -> &Path`, `catalog(&self) -> &Catalog`, `text(&self) -> String`, `save(&self) -> Result<(), McpError>`, `add(&mut self, name: &str, server: &Server)`, `remove(&mut self, name: &str)`, `set_enabled(&mut self, name: &str, enabled: bool)`, `set_registry(&mut self, url: &str)`, `replace(&mut self, name: &str, server: &Server)`, each `-> Result<(), McpError>`.
- Unique phrases: `is not a valid server name`, `is reserved by Claude Code`, `` `version` is required when `from` is given``, ``is only allowed together with `from` ``, ``a stdio server needs `command` ``, `is not allowed for a stdio server`, ``an http or sse server needs `url` ``, `is not allowed for an http or sse server`, ``the variable it reads is not a valid environment variable name`` (with ``field `env.<KEY>` `` or ``field `headers.<NAME>` ``, and never the variable's name, which may be a pasted token), `fl will not read a registry there`, `is not a valid MCP catalog` (a parse error continues `: line <L>, column <C>: <message>`, every key or value the message quotes from the file shown as `<value>`, e.g. `unknown variant <value>, expected one of `stdio`, `http`, `sse``), `` `secret = false` is not allowed``, ``a secret header needs `env``, `could not read`, `there is no server`, `is already in the catalog`; and the registry clauses `it contains a space or a control character`, `it is neither https:// nor http://`, `it carries a user name or password`, `it names no host`, `its port is not a number`, `its host is not this machine` (the host not named).

- [ ] **Step 1: Write the failing tests**

In `Cargo.toml`, replace the `members` line with:

```toml
members = ["crates/core", "crates/store", "crates/exec", "crates/cli", "crates/github", "crates/mcp"]
```

and after `tiny_http = "0.12"` add:

```toml
toml_edit = "0.25"
indexmap = { version = "2.14", features = ["serde"] }
```

Create `crates/mcp/Cargo.toml`:

```toml
[package]
name = "fl-mcp"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
version.workspace = true

[dependencies]
thiserror.workspace = true
serde.workspace = true
# `raw_value` keeps a foreign entry in a vendor's JSON file byte for byte.
# Never `preserve_order`: see the workspace Cargo.toml.
serde_json = { workspace = true, features = ["raw_value"] }
toml.workspace = true
toml_edit.workspace = true
indexmap.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

Create `crates/mcp/src/lib.rs` (Step 3 adds the error type):

```rust
//! The project's MCP catalog (MCP spec): one committed list of the MCP servers
//! a project uses, and the writer of each agent CLI's own MCP file from it.

pub mod catalog;
```

Create `crates/mcp/src/catalog.rs` holding only the tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpError;

    fn file() -> PathBuf {
        PathBuf::from(".fl/mcp.toml")
    }

    fn parse(text: &str) -> Result<Catalog, McpError> {
        Catalog::parse(text, &file())
    }

    // The editor's own layout for these entries, in the catalog's order (by
    // name): writing them back gives these bytes.
    const FULL: &str = r#"registry = "https://registry.example.com"

[server.docs]
enabled = false
transport = "http"
url = "https://mcp.example.com/mcp"
headers.Authorization = { secret = true, env = "DOCS_TOKEN", scheme = "Bearer" }
headers.X-Api-Key = { secret = true, env = "DOCS_KEY" }
headers.X-Team = "widgets"

[server.events]
enabled = true
transport = "sse"
url = "https://events.example.com/sse"

[server.github]
from = "io.example/github"
version = "2.0.1"
enabled = true
vendors = ["claude", "codex"]
transport = "stdio"
command = "docker"
args = ["run", "-i", "--rm", "-e", "GITHUB_TOKEN", "ghcr.io/example/github:2.0.1"]
env.API_KEY = { secret = true, env = "EXAMPLE_API_KEY" }
env.GITHUB_TOKEN = { secret = true }
env.LOG_LEVEL = "info"
"#;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn every_field_of_a_valid_catalog_is_read() {
        let c = parse(FULL).unwrap();
        assert_eq!(c.registry.as_deref(), Some("https://registry.example.com"));
        assert_eq!(
            c.servers.keys().collect::<Vec<_>>(),
            ["docs", "events", "github"]
        );
        let env: BTreeMap<String, EnvValue> = [
            ("GITHUB_TOKEN", EnvValue::Secret { env: None }),
            ("LOG_LEVEL", EnvValue::Literal("info".into())),
            (
                "API_KEY",
                EnvValue::Secret {
                    env: Some("EXAMPLE_API_KEY".into()),
                },
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(
            c.servers["github"],
            Server {
                from: Some("io.example/github".into()),
                version: Some("2.0.1".into()),
                enabled: true,
                vendors: Some(vec![VendorName::Claude, VendorName::Codex]),
                transport: Transport::Stdio,
                command: Some("docker".into()),
                args: Some(strings(&[
                    "run",
                    "-i",
                    "--rm",
                    "-e",
                    "GITHUB_TOKEN",
                    "ghcr.io/example/github:2.0.1",
                ])),
                env: Some(env),
                url: None,
                headers: None,
            }
        );
        let headers: BTreeMap<String, HeaderValue> = [
            (
                "Authorization",
                HeaderValue::Secret {
                    env: "DOCS_TOKEN".into(),
                    scheme: Some("Bearer".into()),
                },
            ),
            (
                "X-Api-Key",
                HeaderValue::Secret {
                    env: "DOCS_KEY".into(),
                    scheme: None,
                },
            ),
            ("X-Team", HeaderValue::Literal("widgets".into())),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(
            c.servers["docs"],
            Server {
                from: None,
                version: None,
                enabled: false,
                vendors: None,
                transport: Transport::Http,
                command: None,
                args: None,
                env: None,
                url: Some("https://mcp.example.com/mcp".into()),
                headers: Some(headers),
            }
        );
        assert_eq!(c.servers["events"].transport, Transport::Sse);
    }

    #[test]
    fn a_server_that_gives_only_its_launch_is_on_for_every_vendor() {
        let c = parse("[server.files]\ntransport = \"stdio\"\ncommand = \"npx\"\n").unwrap();
        let s = &c.servers["files"];
        assert!(s.enabled);
        assert_eq!((&s.vendors, &s.args, &s.env), (&None, &None, &None));
        for v in VendorName::ALL {
            assert!(s.is_for(v), "{v:?}");
        }
        let c = parse(FULL).unwrap();
        let gh = &c.servers["github"];
        assert!(gh.is_for(VendorName::Claude) && gh.is_for(VendorName::Codex));
        assert!(!gh.is_for(VendorName::Antigravity));
        assert_eq!(
            VendorName::ALL.map(VendorName::as_str),
            ["claude", "codex", "antigravity"]
        );
    }

    #[test]
    fn a_secret_names_its_variable_and_literals_are_listed_for_the_warning() {
        let c = parse(FULL).unwrap();
        let env = c.servers["github"].env.as_ref().unwrap();
        assert_eq!(
            env["GITHUB_TOKEN"].secret_var("GITHUB_TOKEN"),
            Some("GITHUB_TOKEN")
        );
        assert_eq!(
            env["API_KEY"].secret_var("API_KEY"),
            Some("EXAMPLE_API_KEY")
        );
        assert_eq!(env["LOG_LEVEL"].secret_var("LOG_LEVEL"), None);
        assert_eq!(c.servers["github"].literal_values(), ["env.LOG_LEVEL"]);
        assert_eq!(c.servers["docs"].literal_values(), ["headers.X-Team"]);
        assert!(c.servers["events"].literal_values().is_empty());
    }

    #[test]
    fn the_catalog_lives_in_dot_fl_and_an_absent_one_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Catalog::path(dir.path()),
            dir.path().join(".fl").join("mcp.toml")
        );
        assert_eq!(Catalog::load(dir.path()).unwrap(), None);
        // Opening an editor writes nothing until it is saved.
        Editor::open(dir.path()).unwrap();
        assert!(!dir.path().join(".fl").exists());
    }

    #[test]
    fn a_new_catalog_starts_with_the_header_and_one_blank_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut ed = Editor::open(dir.path()).unwrap();
        ed.add("tracker", &tracker("1.0.0")).unwrap();
        assert_eq!(
            ed.text(),
            format!(
                "{HEADER}\n[server.tracker]\n{}",
                TRACKER_BODY.replace(" # on for everyone", "")
            )
        );
    }

    #[test]
    fn a_catalog_that_cannot_be_read_is_an_error_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(Catalog::path(dir.path())).unwrap();
        let err = Catalog::load(dir.path()).unwrap_err();
        assert!(matches!(err, McpError::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("could not read"), "{err}");
        let err = Editor::open(dir.path()).err().unwrap();
        assert!(matches!(err, McpError::Io { .. }), "{err:?}");
    }

    fn refused(text: &str) -> String {
        match parse(text) {
            Err(e @ McpError::Invalid { .. }) => e.to_string(),
            other => panic!("expected a catalog rule to refuse {text:?}, got {other:?}"),
        }
    }

    fn stdio(name: &str, extra: &str) -> String {
        format!("[server.{name}]\ntransport = \"stdio\"\ncommand = \"npx\"\n{extra}")
    }

    fn remote(transport: &str, extra: &str) -> String {
        format!(
            "[server.docs]\ntransport = \"{transport}\"\nurl = \"https://mcp.example.com\"\n{extra}"
        )
    }

    #[test]
    fn each_catalog_rule_refuses_naming_the_server_and_the_field() {
        let long = "a".repeat(33);
        let cases: Vec<(String, &str, &str, &str)> = vec![
            (
                stdio(&long, ""),
                long.as_str(),
                "name",
                "is not a valid server name",
            ),
            (
                stdio("GitHub", ""),
                "GitHub",
                "name",
                "is not a valid server name",
            ),
            (
                stdio("my_server", ""),
                "my_server",
                "name",
                "is not a valid server name",
            ),
            (stdio("\"\"", ""), "", "name", "is not a valid server name"),
            (
                stdio("workspace", ""),
                "workspace",
                "name",
                "is reserved by Claude Code",
            ),
            (
                stdio("computer-use", ""),
                "computer-use",
                "name",
                "is reserved by Claude Code",
            ),
            (
                stdio("claude-in-chrome", ""),
                "claude-in-chrome",
                "name",
                "is reserved by Claude Code",
            ),
            (
                stdio("gh", "from = \"io.example/github\"\n"),
                "gh",
                "version",
                "`version` is required when `from` is given",
            ),
            (
                stdio("gh", "version = \"2.0.1\"\n"),
                "gh",
                "version",
                "is only allowed together with `from`",
            ),
            (
                "[server.gh]\ntransport = \"stdio\"\nargs = [\"x\"]\n".into(),
                "gh",
                "command",
                "a stdio server needs `command`",
            ),
            (
                stdio("gh", "url = \"https://mcp.example.com\"\n"),
                "gh",
                "url",
                "is not allowed for a stdio server",
            ),
            (
                stdio("gh", "headers.X-Team = \"widgets\"\n"),
                "gh",
                "headers",
                "is not allowed for a stdio server",
            ),
            (
                "[server.docs]\ntransport = \"http\"\n".into(),
                "docs",
                "url",
                "an http or sse server needs `url`",
            ),
            (
                "[server.docs]\ntransport = \"sse\"\n".into(),
                "docs",
                "url",
                "an http or sse server needs `url`",
            ),
            (
                remote("http", "command = \"npx\"\n"),
                "docs",
                "command",
                "is not allowed for an http or sse server",
            ),
            (
                remote("sse", "args = []\n"),
                "docs",
                "args",
                "is not allowed for an http or sse server",
            ),
            (
                remote("http", "env.LOG_LEVEL = \"info\"\n"),
                "docs",
                "env",
                "is not allowed for an http or sse server",
            ),
            (
                stdio("gh", "env.TOKEN = { secret = true, env = \"1TOKEN\" }\n"),
                "gh",
                "env.TOKEN",
                "is not a valid environment variable name",
            ),
            (
                stdio("gh", "env.MY-TOKEN = { secret = true }\n"),
                "gh",
                "env.MY-TOKEN",
                "is not a valid environment variable name",
            ),
            (
                remote(
                    "http",
                    "headers.Authorization = { secret = true, env = \"A-B\" }\n",
                ),
                "docs",
                "headers.Authorization",
                "is not a valid environment variable name",
            ),
        ];
        for (text, server, field, phrase) in cases {
            let msg = refused(&text);
            assert!(msg.starts_with(".fl/mcp.toml: "), "{msg}");
            assert!(msg.contains(&format!("server `{server}`")), "{msg}");
            assert!(msg.contains(&format!("field `{field}`")), "{msg}");
            assert!(msg.contains(phrase), "{phrase:?} not in {msg}");
        }
        let msg = refused(&format!(
            "registry = \"http://registry.example.com\"\n{}",
            stdio("gh", "")
        ));
        assert!(msg.contains("field `registry`"), "{msg}");
        assert!(msg.contains("fl will not read a registry there"), "{msg}");
        assert!(msg.contains("is not this machine"), "{msg}");
        assert!(!msg.contains("server `"), "{msg}");
    }

    // A value in the catalog can be a pasted token; a refusal names the
    // server and the field, never the value (MCP spec §6).
    #[test]
    fn a_validation_error_never_repeats_a_value() {
        let cases = [
            (
                remote(
                    "http",
                    "headers.Authorization = { secret = true, env = \"Bearer sk-live-4f9a2c\" }\n",
                ),
                "server `docs`, field `headers.Authorization`: the variable it reads is not a \
                 valid environment variable name",
            ),
            (
                stdio(
                    "gh",
                    "env.TOKEN = { secret = true, env = \"sk-live-4f9a2c-abc\" }\n",
                ),
                "server `gh`, field `env.TOKEN`: the variable it reads is not a valid \
                 environment variable name",
            ),
            (
                format!(
                    "registry = \"http://sk-live-4f9a2c.example.com\"\n{}",
                    stdio("gh", "")
                ),
                "field `registry`: fl will not read a registry there: it is not https, and its \
                 host is not this machine",
            ),
        ];
        for (text, phrase) in cases {
            let msg = refused(&text);
            assert!(msg.contains(phrase), "{phrase:?} not in {msg}");
            assert!(!msg.contains("sk-live") && !msg.contains("4f9a"), "{msg}");
        }
    }

    #[test]
    fn names_and_variables_at_the_limits_are_accepted() {
        for name in ["a", "my-server-2", &"a".repeat(32)] {
            parse(&stdio(name, "")).unwrap();
        }
        // A literal value is the server's own business; only a variable fl
        // names in a vendor file must be a valid name.
        parse(&stdio("gh", "env.MY-LEVEL = \"info\"\n")).unwrap();
        parse(&stdio(
            "gh",
            "env.MY-TOKEN = { secret = true, env = \"_TOKEN_2\" }\n",
        ))
        .unwrap();
        for ok in ["A", "_", "a_1", "GITHUB_TOKEN"] {
            assert!(is_env_name(ok), "{ok}");
        }
        for bad in ["", "1A", "A-B", "A B", "É"] {
            assert!(!is_env_name(bad), "{bad}");
        }
    }

    fn unparsed(text: &str) -> String {
        match parse(text) {
            Err(e @ McpError::Parse { .. }) => e.to_string(),
            other => panic!("expected {text:?} not to parse, got {other:?}"),
        }
    }

    #[test]
    fn unknown_fields_and_malformed_values_are_refused_at_every_level() {
        let cases: Vec<(String, &str)> = vec![
            (
                format!("colour = \"red\"\n{}", stdio("gh", "")),
                "unknown field <value>, expected `registry` or `server`",
            ),
            (
                format!("project = \"app\"\n{}", stdio("gh", "")),
                "unknown field <value>, expected `registry` or `server`",
            ),
            (
                stdio("gh", "cmd = \"npx\"\n"),
                "unknown field <value>, expected one of `from`, `version`",
            ),
            (
                stdio("gh", "env.T = { secret = true, scheme = \"Bearer\" }\n"),
                "unknown field <value>, expected `secret` or `env`",
            ),
            (
                remote(
                    "http",
                    "headers.A = { secret = true, env = \"T\", prefix = \"x\" }\n",
                ),
                "unknown field <value>, expected one of `secret`, `env`, `scheme`",
            ),
            (
                stdio("gh", "env.T = { secret = false }\n"),
                "`secret = false` is not allowed",
            ),
            (
                remote("http", "headers.A = { secret = false, env = \"T\" }\n"),
                "`secret = false` is not allowed",
            ),
            (
                stdio("gh", "env.T = { env = \"T\" }\n"),
                "missing field `secret`",
            ),
            (
                remote("http", "headers.A = { secret = true }\n"),
                "a secret header needs `env",
            ),
            (stdio("gh", "env.T = 3\n"), "a string, or a table"),
            (
                stdio("gh", "vendors = [\"gemini\"]\n"),
                "unknown variant <value>, expected one of `claude`, `codex`, `antigravity`",
            ),
            (
                "[server.gh]\ntransport = \"websocket\"\nurl = \"https://x.example\"\n".into(),
                "unknown variant <value>, expected one of `stdio`, `http`, `sse`",
            ),
            (
                "[server.gh]\ncommand = \"npx\"\n".into(),
                "missing field `transport`",
            ),
        ];
        for (text, phrase) in cases {
            let msg = unparsed(&text);
            assert!(
                msg.starts_with(".fl/mcp.toml is not a valid MCP catalog"),
                "{msg}"
            );
            assert!(msg.contains(phrase), "{phrase:?} not in {msg}");
            // The key or value the file holds is never repeated.
            assert!(
                !msg.contains("unknown field `") && !msg.contains("unknown variant `"),
                "{msg}"
            );
        }
    }

    #[test]
    fn a_parse_error_names_the_line_and_column_and_never_quotes_the_file() {
        // The parser's own message quotes the offending line, and a line can
        // hold a pasted token: a syntax error, and a shape error on a line
        // that parses as TOML.
        let cases = [
            (
                stdio("gh", "env.T = sk-live-4f9a2c\n"),
                "line 4, column 9: ",
            ),
            (
                stdio("gh", "token = \"sk-live-4f9a2c\"\n"),
                "line 4, column 1: unknown field <value>, expected one of `from`",
            ),
            // A value serde quotes back: where a boolean goes, as the
            // transport, and as the team default.
            (
                stdio("gh", "env.T = { secret = \"sk-live-4f9a2c\" }\n"),
                "line 4, column 20: invalid type: string <value>, expected a boolean",
            ),
            (
                "[server.gh]\ntransport = \"sk-live-4f9a2c\"\n".into(),
                "line 2, column 13: unknown variant <value>, expected one of `stdio`, `http`, `sse`",
            ),
            (
                stdio("gh", "enabled = \"sk-live-4f9a2c\"\n"),
                "line 4, column 11: invalid type: string <value>, expected a boolean",
            ),
            // A token holding `, expected` is hidden whole.
            (
                "[server.gh]\ntransport = \"sk-live, expected 4f9a\"\n".into(),
                "line 2, column 13: unknown variant <value>, expected one of `stdio`",
            ),
            // A token holding quotes of its own is hidden whole.
            (
                "[server.gh]\ntransport = 'sk-live`4f9a\"2c'\n".into(),
                "line 2, column 13: unknown variant <value>, expected one of `stdio`",
            ),
        ];
        for (text, phrase) in cases {
            let msg = unparsed(&text);
            assert!(!msg.contains("sk-live") && !msg.contains("4f9a"), "{msg}");
            assert!(
                msg.starts_with(&format!(
                    ".fl/mcp.toml is not a valid MCP catalog: {phrase}"
                )),
                "{phrase:?} not in {msg}"
            );
        }
    }

    #[test]
    fn a_registry_is_https_or_http_on_this_machine() {
        for ok in [
            "https://registry.example.com",
            "https://registry.example.com:8443/v0.1?x=1",
            "http://127.0.0.1:8080",
            "http://localhost",
            "http://[::1]:9/",
        ] {
            assert_eq!(check_registry_url(ok), Ok(()), "{ok}");
        }
        for (bad, phrase) in [
            (
                "ftp://registry.example.com",
                "it is neither https:// nor http://",
            ),
            ("registry.example.com", "it is neither https:// nor http://"),
            ("http://registry.example.com", "is not this machine"),
            ("http://127.0.0.1.example.com", "is not this machine"),
            ("http://[::2]/", "is not this machine"),
            (
                "https://user:pw@registry.example.com",
                "it carries a user name or password",
            ),
            (
                "http://user@127.0.0.1",
                "it carries a user name or password",
            ),
            ("https://", "it names no host"),
            ("http://:80/", "it names no host"),
            ("http://127.0.0.1:/", "its port is not a number"),
            (
                "http://127.0.0.1:80.example.com",
                "its port is not a number",
            ),
            ("http://[::1]x/", "its port is not a number"),
            (
                "https://registry.example.com/a b",
                "it contains a space or a control character",
            ),
            (
                "https://registry.example.com\u{7f}",
                "it contains a space or a control character",
            ),
        ] {
            let why = check_registry_url(bad).unwrap_err();
            assert!(why.contains(phrase), "{bad}: {phrase:?} not in {why}");
        }
    }

    fn editor_on(text: &str) -> (tempfile::TempDir, Editor) {
        let dir = tempfile::tempdir().unwrap();
        let path = Catalog::path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        let ed = Editor::open(dir.path()).unwrap();
        (dir, ed)
    }

    #[test]
    fn a_catalog_the_editor_writes_reads_back_the_same_in_its_own_layout() {
        let dir = tempfile::tempdir().unwrap();
        let c = parse(FULL).unwrap();
        let mut ed = Editor::open(dir.path()).unwrap();
        ed.set_registry(c.registry.as_deref().unwrap()).unwrap();
        for (name, s) in &c.servers {
            ed.add(name, s).unwrap();
        }
        assert_eq!(ed.text(), format!("{HEADER}\n{FULL}"));
        assert_eq!(ed.catalog(), &c);
        ed.save().unwrap();
        let path = Catalog::path(dir.path());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ed.text());
        assert_eq!(Catalog::load(dir.path()).unwrap(), Some(c));
        // Reopened, the file is kept as written: an edit adds no second header.
        let mut ed = Editor::open(dir.path()).unwrap();
        ed.set_enabled("events", false).unwrap();
        assert_eq!(
            ed.text(),
            format!(
                "{HEADER}\n{}",
                FULL.replace(
                    "enabled = true\ntransport = \"sse\"",
                    "enabled = false\ntransport = \"sse\""
                )
            )
        );
    }

    const COMMENTED: &str = r#"# Our servers.
registry = "https://registry.example.com" # the public one

# Files on disk.
[server.files]
transport = "stdio"
command = "npx"
args = ["-y", "@example/files@1.0.0"] # pinned by hand

# The issue tracker.
[server.tracker]
from = "io.example/tracker"
version = "1.0.0"
enabled = true # on for everyone
transport = "stdio"
command = "npx"
args = ["-y", "@example/tracker@1.0.0"]
# Below the tracker.

# Docs, remote.
[server.docs]
transport   =   "http"
url = "https://mcp.example.com/mcp"
"#;

    const TRACKER_BODY: &str = r#"from = "io.example/tracker"
version = "1.0.0"
enabled = true # on for everyone
transport = "stdio"
command = "npx"
args = ["-y", "@example/tracker@1.0.0"]
"#;

    fn tracker(version: &str) -> Server {
        Server {
            from: Some("io.example/tracker".into()),
            version: Some(version.into()),
            enabled: true,
            vendors: None,
            transport: Transport::Stdio,
            command: Some("npx".into()),
            args: Some(strings(&["-y", &format!("@example/tracker@{version}")])),
            env: None,
            url: None,
            headers: None,
        }
    }

    #[test]
    fn switching_a_server_changes_one_value_and_keeps_every_comment() {
        let (_dir, mut ed) = editor_on(COMMENTED);
        ed.set_enabled("tracker", false).unwrap();
        assert_eq!(
            ed.text(),
            COMMENTED.replace("enabled = true # on for", "enabled = false # on for")
        );
        assert!(!ed.catalog().servers["tracker"].enabled);
        let (_dir, mut ed) = editor_on(COMMENTED);
        ed.set_enabled("files", false).unwrap();
        assert_eq!(
            ed.text(),
            COMMENTED.replace("# pinned by hand\n", "# pinned by hand\nenabled = false\n")
        );
    }

    #[test]
    fn replacing_a_server_keeps_the_comments_around_it_and_every_other_entry() {
        let (_dir, mut ed) = editor_on(COMMENTED);
        ed.replace("tracker", &tracker("1.1.0")).unwrap();
        let new_body = r#"from = "io.example/tracker"
version = "1.1.0"
enabled = true
transport = "stdio"
command = "npx"
args = ["-y", "@example/tracker@1.1.0"]
"#;
        assert_eq!(ed.text(), COMMENTED.replace(TRACKER_BODY, new_body));
        assert_eq!(ed.catalog().servers["tracker"], tracker("1.1.0"));
        assert_eq!(
            ed.catalog().servers.keys().collect::<Vec<_>>(),
            ["docs", "files", "tracker"]
        );
    }

    #[test]
    fn removing_a_server_takes_its_comment_above_and_nothing_else() {
        let (_dir, mut ed) = editor_on(COMMENTED);
        ed.remove("tracker").unwrap();
        let gone = format!("\n# The issue tracker.\n[server.tracker]\n{TRACKER_BODY}");
        assert_eq!(ed.text(), COMMENTED.replace(&gone, ""));
        assert_eq!(
            ed.catalog().servers.keys().collect::<Vec<_>>(),
            ["docs", "files"]
        );
    }

    #[test]
    fn adding_then_removing_a_server_leaves_the_file_as_it_was() {
        let only_registry = "registry = \"https://registry.example.com\" # ours\n";
        for text in [COMMENTED, FULL, only_registry, "# Nothing yet.\n"] {
            let (_dir, mut ed) = editor_on(text);
            ed.add("extra", &tracker("3.0.0")).unwrap();
            assert!(
                ed.text()
                    .starts_with(text.trim_end_matches("# Nothing yet.\n"))
            );
            assert!(
                ed.text()
                    .contains("\n[server.extra]\nfrom = \"io.example/tracker\"\n")
            );
            assert_eq!(ed.catalog().servers["extra"], tracker("3.0.0"));
            ed.remove("extra").unwrap();
            assert_eq!(ed.text(), text);
        }
    }

    #[test]
    fn setting_the_registry_keeps_its_comments_or_adds_it_at_the_top() {
        let (_dir, mut ed) = editor_on(COMMENTED);
        ed.set_registry("https://mirror.example.com").unwrap();
        assert_eq!(
            ed.text(),
            COMMENTED.replace("registry.example.com\" #", "mirror.example.com\" #")
        );
        let (_dir, mut ed) = editor_on(&stdio("files", ""));
        ed.set_registry("http://127.0.0.1:8080").unwrap();
        assert_eq!(
            ed.text(),
            format!(
                "registry = \"http://127.0.0.1:8080\"\n{}",
                stdio("files", "")
            )
        );
    }

    #[test]
    fn entries_written_inline_are_edited_in_place() {
        let inline = "[server]\nfiles = { transport = \"stdio\", command = \"npx\" } # inline\n";
        let (_dir, mut ed) = editor_on(inline);
        ed.set_enabled("files", false).unwrap();
        // toml_edit keeps the space that stood before the closing brace.
        assert_eq!(
            ed.text(),
            "[server]\nfiles = { transport = \"stdio\", command = \"npx\" , enabled = false } \
             # inline\n"
        );
        ed.replace("files", &tracker("1.0.0")).unwrap();
        assert!(ed.text().ends_with("# inline\n"), "{}", ed.text());
        assert_eq!(ed.catalog().servers["files"], tracker("1.0.0"));

        let nested = "server = { files = { transport = \"stdio\", command = \"npx\" } }\n";
        let (_dir, mut ed) = editor_on(nested);
        ed.add("tracker", &tracker("1.0.0")).unwrap();
        assert_eq!(ed.catalog().servers["tracker"], tracker("1.0.0"));
        assert_eq!(ed.catalog().servers.len(), 2);
    }

    #[test]
    fn an_edit_the_catalog_would_refuse_changes_nothing() {
        let (_dir, mut ed) = editor_on(COMMENTED);
        let err = ed.add("tracker", &tracker("2.0.0")).unwrap_err();
        assert!(matches!(err, McpError::AlreadyPresent { .. }), "{err:?}");
        assert!(
            err.to_string().contains("is already in the catalog"),
            "{err}"
        );
        for err in [
            ed.remove("nope").unwrap_err(),
            ed.set_enabled("nope", false).unwrap_err(),
            ed.replace("nope", &tracker("1.0.0")).unwrap_err(),
        ] {
            assert!(matches!(err, McpError::NoSuchServer { .. }), "{err:?}");
            assert!(
                err.to_string().contains("there is no server `nope`"),
                "{err}"
            );
        }
        let err = ed.add("workspace", &tracker("1.0.0")).unwrap_err();
        assert!(
            err.to_string().contains("is reserved by Claude Code"),
            "{err}"
        );
        let mut no_command = tracker("1.0.0");
        no_command.command = None;
        let err = ed.add("extra", &no_command).unwrap_err();
        assert!(
            err.to_string().contains("a stdio server needs `command`"),
            "{err}"
        );
        let err = ed.replace("files", &no_command).unwrap_err();
        assert!(
            err.to_string().contains("a stdio server needs `command`"),
            "{err}"
        );
        let err = ed.set_registry("http://registry.example.com").unwrap_err();
        assert!(
            err.to_string()
                .contains("fl will not read a registry there"),
            "{err}"
        );
        assert_eq!(ed.text(), COMMENTED);
        assert_eq!(ed.catalog(), &parse(COMMENTED).unwrap());
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-mcp --lib`
Expected: FAIL to compile (52 errors) — `error[E0432]` (unresolved import `crate::McpError`), `error[E0425]` and `error[E0433]` (cannot find `Catalog`, `Editor`, `Server`, `EnvValue`, `HeaderValue`, `VendorName`, `Transport`, `BTreeMap`, `PathBuf` — the tests take the last two from the implementation's `use` lines — and cannot find `check_registry_url`, `is_env_name`, `HEADER`), and `error[E0422]` (cannot find struct `Server`).

- [ ] **Step 3: Implement**

Replace `crates/mcp/src/lib.rs` with:

```rust
//! The project's MCP catalog (MCP spec): one committed list of the MCP servers
//! a project uses, and the writer of each agent CLI's own MCP file from it.

use std::path::PathBuf;

pub mod catalog;

/// Every refusal names the file, the entry and what to do next (MCP spec §5).
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The file could not be read or written. `op` is `read` or `write`.
    #[error("could not {op} {}: {cause}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        cause: String,
    },
    /// The catalog is not TOML, or not the catalog's shape: an unknown field,
    /// a value of the wrong type, `secret = false`. The cause is the line,
    /// the column and the parser's message, never the file's text.
    #[error(
        "{} is not a valid MCP catalog: {cause}\nFix it by hand; docs/mcp.md describes every field",
        path.display()
    )]
    Parse { path: PathBuf, cause: String },
    /// An entry, or the registry, breaks a catalog rule (MCP spec §2.1).
    /// `server` is `None` for a top-level field.
    #[error("{}: {}field `{field}`: {problem}. {next}", path.display(), on_server(server))]
    Invalid {
        path: PathBuf,
        server: Option<String>,
        field: String,
        problem: String,
        next: String,
    },
    /// An edit names a server the catalog does not have.
    #[error(
        "{}: there is no server `{name}` in the catalog. `fl mcp add` adds one",
        path.display()
    )]
    NoSuchServer { path: PathBuf, name: String },
    /// `add` names a server the catalog already has.
    #[error(
        "{}: server `{name}` is already in the catalog. Remove it first with \
         `fl mcp remove {name}`, or move it to a new version with `fl mcp upgrade {name}`",
        path.display()
    )]
    AlreadyPresent { path: PathBuf, name: String },
}

fn on_server(server: &Option<String>) -> String {
    match server {
        Some(name) => format!("server `{name}`, "),
        None => String::new(),
    }
}
```

At the top of `crates/mcp/src/catalog.rs`, before `#[cfg(test)]`, add:

```rust
//! The catalog, `.fl/mcp.toml` (MCP spec §2.1): committed, and the only source
//! of truth for which MCP servers a project uses. It is read with `toml`, every
//! table refusing a field it does not know, and edited with `toml_edit`, so a
//! person's comments and layout survive every change fl makes to it.

use crate::McpError;
use serde::Deserialize;
use serde::de::{self, MapAccess, Visitor};
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Range;
use std::path::{Path, PathBuf};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike, Value};

/// Server names Claude Code skips, so a server under either would reach no
/// Claude session.
pub const RESERVED_NAMES: [&str; 3] = ["workspace", "computer-use", "claude-in-chrome"];

/// The first lines of a catalog `fl mcp` creates.
pub const HEADER: &str = "\
# The MCP servers this project uses (docs/mcp.md). Commit this file: `fl mcp sync`
# writes each agent CLI's own MCP file from it, and `fl mcp` keeps your comments.
";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    /// The registry `fl mcp search`, `add --from` and `upgrade` read.
    #[serde(default)]
    pub registry: Option<String>,
    /// By name: `toml` reads a table's keys in sorted order. The editor keeps
    /// the file's own order.
    #[serde(default, rename = "server")]
    pub servers: BTreeMap<String, Server>,
}

/// One `[server.<name>]` entry. The launch spec is frozen into it when it is
/// added, so writing the vendor files reads nothing else (MCP spec §2.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// The registry name; `None` for a server added by hand.
    #[serde(default)]
    pub from: Option<String>,
    /// The exact registry version, present exactly when `from` is.
    #[serde(default)]
    pub version: Option<String>,
    /// The team default.
    #[serde(default = "on")]
    pub enabled: bool,
    /// `None`: every vendor.
    #[serde(default)]
    pub vendors: Option<Vec<VendorName>>,
    pub transport: Transport,
    /// stdio only, and required there.
    #[serde(default)]
    pub command: Option<String>,
    /// stdio only.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// stdio only.
    #[serde(default)]
    pub env: Option<BTreeMap<String, EnvValue>>,
    /// http and sse only, and required there.
    #[serde(default)]
    pub url: Option<String>,
    /// http and sse only.
    #[serde(default)]
    pub headers: Option<BTreeMap<String, HeaderValue>>,
}

fn on() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VendorName {
    Claude,
    Codex,
    Antigravity,
}

impl VendorName {
    pub const ALL: [VendorName; 3] = [
        VendorName::Claude,
        VendorName::Codex,
        VendorName::Antigravity,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            VendorName::Claude => "claude",
            VendorName::Codex => "codex",
            VendorName::Antigravity => "antigravity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Stdio,
    Http,
    Sse,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Stdio => "stdio",
            Transport::Http => "http",
            Transport::Sse => "sse",
        }
    }
}

/// An `env` value: a literal, committed as written and public if the
/// repository is (MCP spec §6), or a reference to a variable the agent reads
/// when it starts the server. fl never reads a secret's value (MCP spec §2.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "LiteralOr<EnvSecretTable>")]
pub enum EnvValue {
    Literal(String),
    /// `{ secret = true }`, with `env = "…"` when the variable's name is not
    /// the key's.
    Secret {
        env: Option<String>,
    },
}

impl EnvValue {
    /// The variable a secret reads: `env`, else the key itself.
    pub fn secret_var<'a>(&'a self, key: &'a str) -> Option<&'a str> {
        match self {
            EnvValue::Literal(_) => None,
            EnvValue::Secret { env } => Some(env.as_deref().unwrap_or(key)),
        }
    }
}

/// A header value: a literal, or a reference to the variable `env`, sent as
/// `<scheme> <value>` when `scheme` is given and as the value alone otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "LiteralOr<HeaderSecretTable>")]
pub enum HeaderValue {
    Literal(String),
    Secret { env: String, scheme: Option<String> },
}

impl Server {
    /// Whether this server is written for `vendor`.
    pub fn is_for(&self, vendor: VendorName) -> bool {
        self.vendors.as_ref().is_none_or(|v| v.contains(&vendor))
    }

    /// Every literal `env` and header value, as `env.<NAME>` and
    /// `headers.<NAME>`: each is committed with the catalog, so `add` warns
    /// about each (MCP spec §6).
    pub fn literal_values(&self) -> Vec<String> {
        let env = self.env.iter().flatten();
        let env = env.filter(|(_, v)| matches!(v, EnvValue::Literal(_)));
        let headers = self.headers.iter().flatten();
        let headers = headers.filter(|(_, v)| matches!(v, HeaderValue::Literal(_)));
        env.map(|(k, _)| format!("env.{k}"))
            .chain(headers.map(|(k, _)| format!("headers.{k}")))
            .collect()
    }
}

/// A string, or a table `T` read with its own field names: an untagged enum
/// would hide which field a table got wrong.
enum LiteralOr<T> {
    Literal(String),
    Table(T),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for LiteralOr<T> {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Either<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for Either<T> {
            type Value = LiteralOr<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string, or a table `{ secret = true, … }`")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(LiteralOr::Literal(v.to_string()))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(LiteralOr::Table)
            }
        }
        d.deserialize_any(Either(PhantomData))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvSecretTable {
    secret: bool,
    #[serde(default)]
    env: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderSecretTable {
    secret: bool,
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    scheme: Option<String>,
}

const SECRET_FALSE: &str = "`secret = false` is not allowed: write a literal value as a string, \
                            or a secret as `{ secret = true }`";

const HEADER_NEEDS_ENV: &str = "a secret header needs `env = \"…\"`, the environment variable \
                                that holds its value";

impl TryFrom<LiteralOr<EnvSecretTable>> for EnvValue {
    type Error = String;
    fn try_from(v: LiteralOr<EnvSecretTable>) -> Result<Self, String> {
        match v {
            LiteralOr::Literal(s) => Ok(EnvValue::Literal(s)),
            LiteralOr::Table(t) if !t.secret => Err(SECRET_FALSE.to_string()),
            LiteralOr::Table(t) => Ok(EnvValue::Secret { env: t.env }),
        }
    }
}

impl TryFrom<LiteralOr<HeaderSecretTable>> for HeaderValue {
    type Error = String;
    fn try_from(v: LiteralOr<HeaderSecretTable>) -> Result<Self, String> {
        match v {
            LiteralOr::Literal(s) => Ok(HeaderValue::Literal(s)),
            LiteralOr::Table(t) if !t.secret => Err(SECRET_FALSE.to_string()),
            LiteralOr::Table(t) => match t.env {
                Some(env) => Ok(HeaderValue::Secret {
                    env,
                    scheme: t.scheme,
                }),
                None => Err(HEADER_NEEDS_ENV.to_string()),
            },
        }
    }
}

/// `[a-z0-9-]`, 1 to 32 characters (MCP spec §2.1).
pub fn is_server_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `[A-Za-z_][A-Za-z0-9_]*`: a name every vendor and shell takes as a variable.
pub fn is_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Why fl will not read a registry at `url`, as a clause. A registry is read
/// over https, or over http to this machine only, and never with a user name
/// or password in the URL (MCP spec §3.1).
pub fn check_registry_url(url: &str) -> Result<(), String> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("it contains a space or a control character".to_string());
    }
    let (rest, https) = if let Some(rest) = url.strip_prefix("https://") {
        (rest, true)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (rest, false)
    } else {
        return Err("it is neither https:// nor http://".to_string());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err("it carries a user name or password before the host".to_string());
    }
    // An IPv6 host keeps its brackets: `[::1]`.
    let host_end = if authority.starts_with('[') {
        authority.find(']').map_or(authority.len(), |i| i + 1)
    } else {
        authority.find(':').unwrap_or(authority.len())
    };
    let (host, port) = authority.split_at(host_end);
    if host.is_empty() {
        return Err("it names no host".to_string());
    }
    let port_ok = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !port_ok {
        return Err("its port is not a number".to_string());
    }
    if !https && !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return Err(
            "it is not https, and its host is not this machine (127.0.0.1, localhost or [::1])"
                .to_string(),
        );
    }
    Ok(())
}

/// A rule an entry breaks, before it is placed in a file.
struct Problem {
    field: String,
    problem: String,
    next: &'static str,
}

impl Problem {
    fn new(field: impl Into<String>, problem: impl Into<String>, next: &'static str) -> Self {
        Problem {
            field: field.into(),
            problem: problem.into(),
            next,
        }
    }

    fn at(self, path: &Path, server: Option<&str>) -> McpError {
        McpError::Invalid {
            path: path.to_path_buf(),
            server: server.map(str::to_string),
            field: self.field,
            problem: self.problem,
            next: self.next.to_string(),
        }
    }
}

const ENV_NAME_NEXT: &str = "Name the variable with `env = \"…\"`: letters, digits and _, not \
                             starting with a digit";

fn check_server(name: &str, s: &Server) -> Result<(), Problem> {
    if !is_server_name(name) {
        return Err(Problem::new(
            "name",
            format!(
                "`{name}` is not a valid server name: use 1 to 32 characters of a-z, 0-9 and -"
            ),
            "Rename the entry",
        ));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(Problem::new(
            "name",
            format!("`{name}` is reserved by Claude Code, which skips a server of that name"),
            "Rename the entry",
        ));
    }
    match (&s.from, &s.version) {
        (Some(_), None) => {
            return Err(Problem::new(
                "version",
                "`version` is required when `from` is given: a registry entry is pinned to an \
                 exact version",
                "Add the version the registry gave, or remove `from` for a server added by hand",
            ));
        }
        (None, Some(_)) => {
            return Err(Problem::new(
                "version",
                "`version` is only allowed together with `from`",
                "Add the registry name as `from`, or remove `version`",
            ));
        }
        _ => {}
    }
    match s.transport {
        Transport::Stdio => {
            if s.command.is_none() {
                return Err(Problem::new(
                    "command",
                    "a stdio server needs `command`",
                    "Add the program to start, or set `transport` to \"http\" or \"sse\" with a \
                     `url`",
                ));
            }
            for (field, present) in [("url", s.url.is_some()), ("headers", s.headers.is_some())] {
                if present {
                    return Err(Problem::new(
                        field,
                        format!("`{field}` is not allowed for a stdio server"),
                        "Remove it, or set `transport` to \"http\" or \"sse\"",
                    ));
                }
            }
        }
        Transport::Http | Transport::Sse => {
            if s.url.is_none() {
                return Err(Problem::new(
                    "url",
                    "an http or sse server needs `url`",
                    "Add the server's URL, or set `transport` to \"stdio\" with a `command`",
                ));
            }
            for (field, present) in [
                ("command", s.command.is_some()),
                ("args", s.args.is_some()),
                ("env", s.env.is_some()),
            ] {
                if present {
                    return Err(Problem::new(
                        field,
                        format!("`{field}` is not allowed for an http or sse server"),
                        "Remove it: a remote server takes `url` and `headers`",
                    ));
                }
            }
        }
    }
    for (key, value) in s.env.iter().flatten() {
        if let Some(var) = value.secret_var(key)
            && !is_env_name(var)
        {
            return Err(Problem::new(
                format!("env.{key}"),
                "the variable it reads is not a valid environment variable name",
                ENV_NAME_NEXT,
            ));
        }
    }
    for (key, value) in s.headers.iter().flatten() {
        if let HeaderValue::Secret { env, .. } = value
            && !is_env_name(env)
        {
            return Err(Problem::new(
                format!("headers.{key}"),
                "the variable it reads is not a valid environment variable name",
                ENV_NAME_NEXT,
            ));
        }
    }
    Ok(())
}

impl Catalog {
    /// `<root>/.fl/mcp.toml`.
    pub fn path(root: &Path) -> PathBuf {
        root.join(".fl").join("mcp.toml")
    }

    /// The project's catalog; `None` when it has none. A catalog that exists
    /// but cannot be read is an error, never `None`.
    pub fn load(root: &Path) -> Result<Option<Catalog>, McpError> {
        let path = Self::path(root);
        match read(&path)? {
            Some(text) => Self::parse(&text, &path).map(Some),
            None => Ok(None),
        }
    }

    /// Reads and checks a catalog's text; `file` names it in a refusal.
    pub fn parse(text: &str, file: &Path) -> Result<Catalog, McpError> {
        let catalog: Catalog =
            toml::from_str(text).map_err(|e| parse_error(file, text, e.span(), e.message()))?;
        if let Some(url) = &catalog.registry
            && let Err(why) = check_registry_url(url)
        {
            return Err(Problem::new(
                "registry",
                format!("fl will not read a registry there: {why}"),
                "Use an https URL, or an http URL to this machine",
            )
            .at(file, None));
        }
        for (name, server) in &catalog.servers {
            check_server(name, server).map_err(|p| p.at(file, Some(name)))?;
        }
        Ok(catalog)
    }
}

fn read(path: &Path) -> Result<Option<String>, McpError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(McpError::Io {
            op: "read",
            path: path.to_path_buf(),
            cause: e.to_string(),
        }),
    }
}

/// The catalog does not parse. The parser's own text quotes the offending
/// line, and a line can hold a pasted token (MCP spec §6), so only the line,
/// the column and the parser's message are shown.
fn parse_error(path: &Path, text: &str, span: Option<Range<usize>>, message: &str) -> McpError {
    let at = span.map_or(0, |s| s.start);
    let (mut line, mut column) = (1, 1);
    for (_, c) in text.char_indices().take_while(|&(i, _)| i < at) {
        if c == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    McpError::Parse {
        path: path.to_path_buf(),
        cause: format!(
            "line {line}, column {column}: {}",
            redact(message.trim_end())
        ),
    }
}

/// The parser's message without the value it quotes from the file: serde
/// names the key or the value it could not read (``unknown variant `…` ``,
/// `invalid type: string "…"`), and either can be a pasted token (MCP spec
/// §6). What follows `expected` is the catalog's own vocabulary, and fl's own
/// messages and `missing field` quote nothing from the file, so they stay.
fn redact(message: &str) -> String {
    if [SECRET_FALSE, HEADER_NEEDS_ENV, "missing field "]
        .iter()
        .any(|own| message.starts_with(own))
    {
        return message.to_string();
    }
    let split = message.rfind(", expected").unwrap_or(message.len());
    let (head, tail) = message.split_at(split);
    // From the first quote to the last, so a value holding a quote of its own
    // is hidden whole.
    match (head.find(['`', '"']), head.rfind(['`', '"'])) {
        (Some(first), Some(last)) if last > first => {
            format!("{}<value>{}{tail}", &head[..first], &head[last + 1..])
        }
        (Some(first), _) => format!("{}<value>{tail}", &head[..first]),
        _ => message.to_string(),
    }
}

/// Edits the catalog's text in place. Each edit is checked as a whole
/// catalog before it is kept, so a refused edit changes nothing; only the
/// edited entry or value is written fresh, and every other byte is kept.
#[derive(Debug)]
pub struct Editor {
    path: PathBuf,
    doc: DocumentMut,
    /// `false` while the file does not exist: its text then starts with
    /// [`HEADER`].
    existed: bool,
    catalog: Catalog,
}

impl Editor {
    /// The project's catalog, or an empty one when it has none. Nothing is
    /// written until [`Editor::save`].
    pub fn open(root: &Path) -> Result<Editor, McpError> {
        let path = Catalog::path(root);
        let (text, existed) = match read(&path)? {
            Some(text) => (text, true),
            None => (String::new(), false),
        };
        let catalog = Catalog::parse(&text, &path)?;
        let doc = text
            .parse::<DocumentMut>()
            .map_err(|e| parse_error(&path, &text, e.span(), e.message()))?;
        Ok(Editor {
            path,
            doc,
            existed,
            catalog,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The catalog as edited so far.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The file's text as edited so far.
    pub fn text(&self) -> String {
        let body = self.doc.to_string();
        if self.existed {
            body
        } else {
            format!("{HEADER}\n{}", body.trim_start_matches('\n'))
        }
    }

    /// Writes the file, creating `.fl/` when it is absent.
    pub fn save(&self) -> Result<(), McpError> {
        let io = |e: std::io::Error| McpError::Io {
            op: "write",
            path: self.path.clone(),
            cause: e.to_string(),
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        std::fs::write(&self.path, self.text()).map_err(io)
    }

    /// Adds `[server.<name>]` after every other entry.
    pub fn add(&mut self, name: &str, server: &Server) -> Result<(), McpError> {
        if self.catalog.servers.contains_key(name) {
            return Err(McpError::AlreadyPresent {
                path: self.path.clone(),
                name: name.to_string(),
            });
        }
        // toml_edit writes a table with no position of its own right after
        // the one before it among the servers, which it keeps in the order
        // their headers stand in the file: here, the last. An inline
        // `server` takes the entry as an inline table.
        self.edit(|doc| {
            table_like(servers_mut(doc)).insert(name, Item::Table(server_table(server)));
        })
    }

    /// Removes the entry, with the comment directly above it.
    pub fn remove(&mut self, name: &str) -> Result<(), McpError> {
        self.known(name)?;
        self.edit(|doc| {
            table_like(servers_mut(doc)).remove(name);
        })
    }

    /// Sets the entry's team default, keeping the rest of its text.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<(), McpError> {
        self.known(name)?;
        self.edit(|doc| {
            let entry = table_like(servers_mut(doc))
                .get_mut(name)
                .and_then(Item::as_table_like_mut)
                .expect("a server the catalog read is a table");
            set_value(entry, "enabled", Value::from(enabled));
        })
    }

    /// Sets the top-level `registry`, keeping its comments.
    pub fn set_registry(&mut self, url: &str) -> Result<(), McpError> {
        self.edit(|doc| set_value(doc.as_table_mut(), "registry", Value::from(url)))
    }

    /// Writes `server` in place of the entry, keeping the comments above it
    /// and its place in the file (a table with no position of its own is
    /// written where it stands among the servers). Comments inside the old
    /// entry go with it.
    pub fn replace(&mut self, name: &str, server: &Server) -> Result<(), McpError> {
        self.known(name)?;
        self.edit(|doc| {
            let old = table_like(servers_mut(doc))
                .get_mut(name)
                .expect("a server the catalog read is present");
            let mut table = server_table(server);
            match old {
                Item::Table(old) => {
                    *table.decor_mut() = old.decor().clone();
                    *old = table;
                }
                Item::Value(old) => {
                    let mut new = Value::InlineTable(table.into_inline_table());
                    *new.decor_mut() = old.decor().clone();
                    *old = new;
                }
                _ => unreachable!("a server the catalog read is a table"),
            }
        })
    }

    fn known(&self, name: &str) -> Result<(), McpError> {
        if self.catalog.servers.contains_key(name) {
            Ok(())
        } else {
            Err(McpError::NoSuchServer {
                path: self.path.clone(),
                name: name.to_string(),
            })
        }
    }

    fn edit(&mut self, change: impl FnOnce(&mut DocumentMut)) -> Result<(), McpError> {
        let mut doc = self.doc.clone();
        change(&mut doc);
        self.catalog = Catalog::parse(&doc.to_string(), &self.path)?;
        self.doc = doc;
        Ok(())
    }
}

/// The `server` item, created as a table with no header of its own when the
/// catalog has none.
fn servers_mut(doc: &mut DocumentMut) -> &mut Item {
    doc.entry("server").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    })
}

fn table_like(item: &mut Item) -> &mut dyn TableLike {
    item.as_table_like_mut()
        .expect("the catalog read `server` as a table")
}

/// Sets `key` to `value`, keeping the spaces and comment around the old value.
fn set_value(table: &mut dyn TableLike, key: &str, mut value: Value) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(old) => {
            *value.decor_mut() = old.decor().clone();
            *old = value;
        }
        None => {
            table.insert(key, Item::Value(value));
        }
    }
}

/// An entry in the editor's own layout: one line per field, `env` and
/// `headers` as dotted keys.
fn server_table(s: &Server) -> Table {
    let mut t = Table::new();
    t.decor_mut().set_prefix("\n");
    if let Some(from) = &s.from {
        t.insert("from", toml_edit::value(from.as_str()));
    }
    if let Some(version) = &s.version {
        t.insert("version", toml_edit::value(version.as_str()));
    }
    t.insert("enabled", toml_edit::value(s.enabled));
    if let Some(vendors) = &s.vendors {
        let list: Array = vendors.iter().map(|v| v.as_str()).collect();
        t.insert("vendors", toml_edit::value(list));
    }
    t.insert("transport", toml_edit::value(s.transport.as_str()));
    if let Some(command) = &s.command {
        t.insert("command", toml_edit::value(command.as_str()));
    }
    if let Some(args) = &s.args {
        let list: Array = args.iter().map(String::as_str).collect();
        t.insert("args", toml_edit::value(list));
    }
    if let Some(env) = &s.env {
        let values = env.iter().map(|(k, v)| {
            let value = match v {
                EnvValue::Literal(s) => Value::from(s.as_str()),
                EnvValue::Secret { env } => secret(env.as_deref(), None),
            };
            (k, value)
        });
        t.insert("env", dotted(values));
    }
    if let Some(url) = &s.url {
        t.insert("url", toml_edit::value(url.as_str()));
    }
    if let Some(headers) = &s.headers {
        let values = headers.iter().map(|(k, v)| {
            let value = match v {
                HeaderValue::Literal(s) => Value::from(s.as_str()),
                HeaderValue::Secret { env, scheme } => secret(Some(env), scheme.as_deref()),
            };
            (k, value)
        });
        t.insert("headers", dotted(values));
    }
    t
}

fn secret(env: Option<&str>, scheme: Option<&str>) -> Value {
    let mut t = InlineTable::new();
    t.insert("secret", Value::from(true));
    if let Some(env) = env {
        t.insert("env", Value::from(env));
    }
    if let Some(scheme) = scheme {
        t.insert("scheme", Value::from(scheme));
    }
    Value::InlineTable(t)
}

/// `name.KEY = value` lines inside the entry, rather than a table of their own.
fn dotted<'a>(values: impl Iterator<Item = (&'a String, Value)>) -> Item {
    let mut t = Table::new();
    t.set_dotted(true);
    for (k, v) in values {
        t.insert(k, Item::Value(v));
    }
    Item::Table(t)
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-mcp --lib`
Expected: PASS — 20 passed: `catalog::tests::every_field_of_a_valid_catalog_is_read`, `catalog::tests::a_server_that_gives_only_its_launch_is_on_for_every_vendor`, `catalog::tests::a_secret_names_its_variable_and_literals_are_listed_for_the_warning`, `catalog::tests::the_catalog_lives_in_dot_fl_and_an_absent_one_is_none`, `catalog::tests::a_new_catalog_starts_with_the_header_and_one_blank_line`, `catalog::tests::a_catalog_that_cannot_be_read_is_an_error_not_absent`, `catalog::tests::each_catalog_rule_refuses_naming_the_server_and_the_field`, `catalog::tests::a_validation_error_never_repeats_a_value`, `catalog::tests::names_and_variables_at_the_limits_are_accepted`, `catalog::tests::unknown_fields_and_malformed_values_are_refused_at_every_level`, `catalog::tests::a_parse_error_names_the_line_and_column_and_never_quotes_the_file`, `catalog::tests::a_registry_is_https_or_http_on_this_machine`, `catalog::tests::a_catalog_the_editor_writes_reads_back_the_same_in_its_own_layout`, `catalog::tests::switching_a_server_changes_one_value_and_keeps_every_comment`, `catalog::tests::replacing_a_server_keeps_the_comments_around_it_and_every_other_entry`, `catalog::tests::removing_a_server_takes_its_comment_above_and_nothing_else`, `catalog::tests::adding_then_removing_a_server_leaves_the_file_as_it_was`, `catalog::tests::setting_the_registry_keeps_its_comments_or_adds_it_at_the_top`, `catalog::tests::entries_written_inline_are_edited_in_place`, `catalog::tests::an_edit_the_catalog_would_refuse_changes_nothing`.

- [ ] **Step 5: Mutation checks**

"rules" is `cargo test -p fl-mcp --lib catalog::tests::each_catalog_rule_refuses_naming_the_server_and_the_field`; "limits" is `cargo test -p fl-mcp --lib catalog::tests::names_and_variables_at_the_limits_are_accepted`; "url" is `cargo test -p fl-mcp --lib catalog::tests::a_registry_is_https_or_http_on_this_machine`; "shape" is `cargo test -p fl-mcp --lib catalog::tests::unknown_fields_and_malformed_values_are_refused_at_every_level`; "unreadable" is `cargo test -p fl-mcp --lib catalog::tests::a_catalog_that_cannot_be_read_is_an_error_not_absent`; "refused-edit" is `cargo test -p fl-mcp --lib catalog::tests::an_edit_the_catalog_would_refuse_changes_nothing`; "layout" is `cargo test -p fl-mcp --lib catalog::tests::a_catalog_the_editor_writes_reads_back_the_same_in_its_own_layout`; "header" is `cargo test -p fl-mcp --lib catalog::tests::a_new_catalog_starts_with_the_header_and_one_blank_line`; "switch" is `cargo test -p fl-mcp --lib catalog::tests::switching_a_server_changes_one_value_and_keeps_every_comment`; "replace" is `cargo test -p fl-mcp --lib catalog::tests::replacing_a_server_keeps_the_comments_around_it_and_every_other_entry`; "remove" is `cargo test -p fl-mcp --lib catalog::tests::removing_a_server_takes_its_comment_above_and_nothing_else`; "add-remove" is `cargo test -p fl-mcp --lib catalog::tests::adding_then_removing_a_server_leaves_the_file_as_it_was`; "registry" is `cargo test -p fl-mcp --lib catalog::tests::setting_the_registry_keeps_its_comments_or_adds_it_at_the_top`; "inline" is `cargo test -p fl-mcp --lib catalog::tests::entries_written_inline_are_edited_in_place`; "vendors" is `cargo test -p fl-mcp --lib catalog::tests::a_server_that_gives_only_its_launch_is_on_for_every_vendor`; "literals" is `cargo test -p fl-mcp --lib catalog::tests::a_secret_names_its_variable_and_literals_are_listed_for_the_warning`; "position" is `cargo test -p fl-mcp --lib catalog::tests::a_parse_error_names_the_line_and_column_and_never_quotes_the_file`; "values" is `cargo test -p fl-mcp --lib catalog::tests::a_validation_error_never_repeats_a_value`. Each mutation is one edit of `crates/mcp/src/catalog.rs`; save a copy first, restore it after each, and `cmp` against the copy.

1. `is_server_name`'s upper bound: `(1..=33)` → rules red (the 33-character name).
2. `is_server_name`'s lower bound: `(0..=32)` → rules red (the empty name).
3. `is_server_name` takes lowercase only: `b.is_ascii_alphabetic()` for `b.is_ascii_lowercase()` → rules red (`GitHub`).
4. `is_server_name` refuses `_`: add `|| b == b'_'` → rules red (`my_server`).
5. `is_server_name` takes digits: delete `b.is_ascii_digit() ||` → limits red (`my-server-2`).
6. `is_server_name` takes `-`: delete `|| b == b'-'` → limits red (`my-server-2`).
7. `check_server`'s name check: `if false && !is_server_name(name)` → rules red.
8. `check_server`'s reserved check: `if false && RESERVED_NAMES.contains(&name)` → rules red (`workspace`).
9. `RESERVED_NAMES` holds `computer-use`: `["workspace", "workspace", "claude-in-chrome"]` → rules red (`computer-use`).
10. `from` needs `version`: `(Some(_), None) if false =>` → rules red (`` `version` is required when `from` is given``).
11. `version` needs `from`: `(None, Some(_)) if false =>` → rules red (`is only allowed together with `from``).
12. stdio needs `command`: `if false && s.command.is_none()` → rules red.
13. stdio refuses `url`: `("url", false)` → rules red.
14. stdio refuses `headers`: `("headers", false)` → rules red.
15. http and sse need `url`: `if false && s.url.is_none()` → rules red (the http case).
16. the rule covers sse: split the arm into `Transport::Sse => {}` and `Transport::Http => {` → rules red (the sse case).
17. http and sse refuse `command`: `("command", false)` → rules red.
18. … refuse `args`: `("args", false)` → rules red (`args = []` on sse: present, though empty).
19. … refuse `env`: `("env", false)` → rules red.
20. a secret `env` names a valid variable: `&& false && !is_env_name(var)` → rules red (`env = "1TOKEN"`).
21. `secret_var` defaults to the key: `unwrap_or("KEY")` → rules red (`env.MY-TOKEN = { secret = true }`).
22. a secret header names a valid variable: `&& false && !is_env_name(env)` → rules red.
23. `is_env_name`'s first byte is no digit: `is_ascii_alphanumeric()` in the first test → limits red (`1A`).
24. `is_env_name`'s first byte may be `_`: delete that `|| b == b'_'` → limits red (`_`).
25. `is_env_name` refuses the empty name: `.is_none_or(…)` for `.is_some_and(…)` → limits red (`""`).
26. `is_env_name` refuses `-`: add `|| b == b'-'` to the rest → limits red (`A-B`).
27. `Catalog::parse` checks the registry: `&& let Err(why) = Ok::<(), String>(()).map(|_| url)` → rules red (the registry case).
28. `check_registry_url` refuses a space: delete `c.is_whitespace() ||` → url red (`/a b`).
29. … refuses a control character: delete `|| c.is_control()` → url red (the DEL; a tab is whitespace too, so the case uses DEL).
30. … refuses another scheme: `(url, false)` for the `neither` return → url red (`ftp://…` reports a port, `registry.example.com` reports the host).
31. … marks http as not https: `(rest, true)` in the `http://` arm → url red (`http://registry.example.com` accepted).
32. … refuses a user name or password: `if false && authority.contains('@')` → url red.
33. … refuses no host: `if false && host.is_empty()` → url red (`https://`).
34. … refuses an empty port: delete `!p.is_empty() &&` → url red (`http://127.0.0.1:/`).
35. … refuses a port that is not digits: `.is_some_and(|p| !p.is_empty())` → url red (`:80.example.com`).
36. … needs the `:` before a port: `.strip_prefix("")` → url red (`http://127.0.0.1:8080` refused).
37. … keeps an IPv6 host's brackets: the `[` arm as the `:` arm → url red (`http://[::1]:9/` refused).
38. … refuses http to another host: `if !https && false` → url red.
39. … lets https reach any host: delete `!https &&` → url red (the https cases refused).
40. … accepts `127.0.0.1`: delete it from the `matches!` → url red.
41. … accepts `localhost`: delete it → url red.
42. … accepts `[::1]`: delete it → url red.
43. … checks the user name before splitting the host: move the `@` check to just before the loopback check → url red (`http://user@127.0.0.1` then reports the host, `https://user:pw@…` the port).
44. `EnvValue` refuses `secret = false`: `if false =>` on its `!t.secret` arm → shape red.
45. `HeaderValue` refuses `secret = false`: the same on its arm → shape red.
46. a secret header needs `env`: `None => Ok(HeaderValue::Secret { env: "T".into(), scheme: t.scheme })` → shape red (`headers.A = { secret = true }` parses).
47. `deny_unknown_fields` on `Catalog`: delete it → shape red (`colour`).
48. … on `Server`: delete it → shape red (`cmd`).
49. … on `EnvSecretTable`: delete it → shape red (`scheme` on an env secret).
50. … on `HeaderSecretTable`: delete it → shape red (`prefix`).
51. `read`: an absent file is `None`, any other failure an error: `Err(_) => Ok(None),` before the `NotFound` arm → unreadable red.
52. `add` refuses a present name: `if false && self.catalog.servers.contains_key(name)` in `add` → refused-edit red (the entry is overwritten).
53. `known`: `if true || self.catalog.servers.contains_key(name)` → refused-edit red (`remove("nope")` succeeds).
54. `edit` keeps the old text when the catalog refuses: assign `self.doc = doc;` before `Catalog::parse` → refused-edit red (the text changed).
55. a new entry starts with a blank line: `set_prefix("")` in `server_table` → layout red.
56. `servers_mut` creates `server` with no header of its own: `set_implicit(false)` → layout red (a `[server]` line appears).
57. `remove` removes: replace its `.remove(name);` line with `let _ = name;` → remove red.
58. `set_enabled` writes the value it is given: `Value::from(true)` → switch red.
59. `set_value` keeps the old value's spaces and comment: delete `*value.decor_mut() = old.decor().clone();` → switch red (`# on for everyone` lost).
60. the same mutation → registry red (`# the public one` lost).
61. `replace` keeps the comment above: delete `*table.decor_mut() = old.decor().clone();` → replace red.
62. `replace` keeps an inline entry's comment: delete `*new.decor_mut() = old.decor().clone();` → inline red.
63. `text` adds the header only to a new file: `if false {` for `if self.existed {` → add-remove red.
64. `text` leaves one blank line under the header: `body` for `body.trim_start_matches('\n')` → header red.
65. `open` marks a file it read as existing: `(text, false)` → layout red (the reopened file gains a second header).
66. `save` creates `.fl/`: replace `create_dir_all` with `let _ = dir;` → layout red.
67. `is_for` honours `vendors`: `is_none_or(|_| true)` → vendors red.
68. `is_for` means every vendor when `vendors` is absent: `is_some_and` for `is_none_or` → vendors red.
69. `literal_values` lists literal env values only: negate the `EnvValue::Literal` filter → literals red.
70. … literal headers only: negate the `HeaderValue::Literal` filter → literals red.
71. `Catalog::parse` never quotes the file: `.map_err(|e| McpError::Parse { path: file.to_path_buf(), cause: e.to_string() })?` for its `parse_error` → position red (the token is in the message).
72. `parse_error` counts lines: delete `line += 1;` → position red (`line 1`).
73. … starts a new line at column 1: delete `column = 1;` → position red.
74. … counts columns: delete `column += 1;` → position red (`column 1` for the syntax error).
75. … stops before the error's first character: `i <= at` for `i < at` → position red (`column 10`).
76. `parse_error` redacts: `redact(message.trim_end())` → `message.trim_end()` → position red (`sk-live` is quoted back).
77. … only before the `expected` clause: `let split = message.rfind(", expected").unwrap_or(message.len());` → `let split = message.len();` → shape red (`expected one of `stdio`…` is gone).
78. … the last `expected` clause: `rfind` → `find` in that line → position red (`sk-live, expected 4f9a`: `4f9a` leaks).
79. … from the first quote to the last: in the `last > first` arm, end at the next quote after `first` instead of `last` → position red (the token with a backtick and a quote leaks its end).
80. fl's own messages stay whole: drop `SECRET_FALSE` from the list → shape red.
81. … and `HEADER_NEEDS_ENV`: drop it → shape red.
82. … and `missing field`: drop `"missing field "` → shape red.
83. `RESERVED_NAMES` holds `claude-in-chrome`: `["workspace", "computer-use", "computer-use"]` → rules red (`claude-in-chrome`).
84. A secret `env`'s refusal names no value: ``format!("`{var}` is not a valid environment variable name")`` for its fixed text → values red (`sk-live-4f9a2c-abc`).
85. … a secret header's: ``format!("`{env}` is not a valid environment variable name")`` → values red (`Bearer sk-live-4f9a2c`).
86. The registry's refusal names no host: ``format!("it is not https, and its host is not this machine ({host})")`` → values red.

Not observable:
- `Editor::add`'s refusal of an entry that breaks a rule comes from the same `Catalog::parse` that `load` runs (`edit` parses the whole edited text), so each rule's mutation shows on rules; refused-edit shows that such an edit changes nothing (mutation 54).
- Where a new entry lands and where a replaced one stays: toml_edit writes a table with no position of its own right after the one before it among the servers, and keeps the servers in the order their headers stand in the file, so no position is set and there is no line to revert. add-remove pins that a new entry comes last; replace pins that a replaced entry keeps its place.
- The order of the rules in `check_server`: every case breaks exactly one rule, so a catalog that breaks two is refused for the first, and which one is first is not pinned.
- `server_table` writes each field: not a guard; layout pins every field by exact text, and the round trip through `Catalog::parse`.
- `redact`'s `(Some(first), _)` arm, a single quote with no partner: serde writes a value it quotes between two delimiters, so no message reaches it; it hides the rest of the clause rather than show it.
- The `expect`s and `unreachable!` in the editor: the catalog parsed, so `server` is a table and each entry a table or an inline table; no input reaches them. `open`'s second `Parse` (from `toml_edit`, through the same `parse_error`): a text `toml` reads is one `toml_edit` reads, so no input reaches it.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1210 passed, 19 ignored (1190 and 19 before this task).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/mcp/Cargo.toml crates/mcp/src/lib.rs crates/mcp/src/catalog.rs
git commit -m "feat(mcp): the catalog: model, validation and an editor that keeps comments

A new crate, fl-mcp, holds the project's MCP catalog, .fl/mcp.toml. It
is read with toml, every table refusing a field it does not know; an
env or header value is a literal string or a secret reference, and
secret = false is refused. Each catalog rule refuses naming the file,
the server and the field, and says what to do next: server names are
1 to 32 characters of a-z, 0-9 and -, not workspace, computer-use or
claude-in-chrome; from and version come together; stdio needs command
and takes no url or headers; http and sse need url and take no
command, args or env; a secret's variable is a valid name; the registry is https, or http to
this machine, with no user name or password.

The editor works on the file's own text with toml_edit: adding,
removing, switching and replacing an entry, and setting the registry,
keep every comment and every other entry byte for byte. Each edit is
checked as a whole catalog before it is kept, so a refused edit
changes nothing. A new catalog starts with a short header. A catalog
that does not parse is refused with the line, the column and the
parser's message, never the line itself, which can hold a pasted token,
and any key or value the message quotes back is shown as <value>. No
refusal repeats a value from the catalog. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 2: The machine switches and the state directory in the user's config

A person turns a catalog server off on one machine, or on where the team default is off, in the user's own config, not in the repository (MCP spec §2.2): a new `[[mcp]]` table of `root`, `disable` and `enable`, found by the same longest-ancestor rule as `[[project]]` and independent of it, so a project needs no store binding to use the catalog. This task adds the table (`McpEntry`, `deny_unknown_fields`, no `antigravity` key — owner decision 10, plan ruling 3) to `config::load`, which refuses a relative `root` as it does for `[[project]]` and a name in both `enable` and `disable` of one entry (§2.2: "The same name in `enable` and `disable` is an error"). Whether a switch names a server the catalog has is checked later, by `fl mcp sync` and `check` against the catalog, and a name it lacks is a warning, not an error (owner decision 15). The longest-ancestor rule is factored out of `bound_entry` into one generic helper, `longest_ancestor`, which both `bound_entry` and the new `mcp_entry` call (plan ruling 7); two `[[mcp]]` entries on one root are refused unless they switch alike. And `state_dir` gives the XDG state base by `data_dir`'s rule, default `~/.local/state`, for the ownership records under `$XDG_STATE_HOME/fl/mcp/` (plan ruling 23), with `fl_state_dir`, the wrapper `fl mcp` will call. Nothing reads the table or the state directory yet: Task 7 does.

**Blast radius:** `bound_entry`'s internals. Its one caller is `run()` in `crates/cli/src/main.rs` (`config::bound_entry(entries, &locus)?`); it now asks `longest_ancestor` for the winners and the canonical working directory, and keeps its tie rule, its order of canonicalization and its message word for word — every existing `config::tests` test passes unchanged, and no test changes. `Config` gains a field; it is built only in `config.rs` (`Config::default()` and `load`), so no other literal changes. `File` gains `mcp` with `#[serde(default)]`, so a config with no `[[mcp]]` table loads exactly as before. A config **with** an `[[mcp]]` table is refused by an fl older than this one, for every command, because `File` refuses an unknown key and every command reads the config first (MCP spec §2.2, release scope); the field's doc comment says so. `Config::mcp`, `mcp_entry` and `fl_state_dir` carry `expect(dead_code)` (on `Config::mcp` and `mcp_entry` only outside tests, which call them) until Task 7 reads them — an `expect` that is no longer met is a warning, so Task 7 must remove all three attributes.

**Files:**
- Modify: `crates/cli/src/config.rs` (`McpEntry`, `Config.mcp`, `File.mcp`, `state_dir`, `fl_state_dir`, `load`'s `[[mcp]]` checks, `longest_ancestor`, `bound_entry`, `mcp_entry`; tests)

**Interfaces:**
- Consumes: nothing from Task 1 — this task does not depend on fl-mcp.
- Produces: `#[derive(Debug, Clone, PartialEq, Eq, Deserialize)] #[serde(deny_unknown_fields)] pub struct McpEntry { pub root: PathBuf, #[serde(default)] pub disable: Vec<String>, #[serde(default)] pub enable: Vec<String> }`; `Config` gains `pub mcp: Vec<McpEntry>` (in the file's order; every `root` absolute, no name in both lists of one entry); `pub fn mcp_entry(entries: &[McpEntry], cwd: &Path) -> Result<Option<McpEntry>>` (`None` when `cwd` does not exist or no root is an ancestor of it; the entry returned is the first winner, with its `root` as written in the config, not canonicalized); `pub fn state_dir(xdg_state_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf>`; `pub fn fl_state_dir() -> Option<PathBuf>` (`<state base>/fl` from `$XDG_STATE_HOME` and `$HOME`; Task 7 joins `mcp`). Private: `fn longest_ancestor<'a, T>(items: &'a [T], root_of: impl Fn(&T) -> &Path, cwd: &Path) -> Result<Option<(PathBuf, Vec<&'a T>)>>`.
- Unique phrases: `` `root` in `[[mcp]]` must be an absolute path `` (relative root); `` is in both `enable` and `disable` `` (a name switched both ways; the message is `` in the `[[mcp]]` entry for <root>, `<name>` is in both `enable` and `disable`; keep it in one ``); `` has more than one `[[mcp]]` entry in the config `` and `Keep one of these entries` (a tie; each entry named as `<root> (disable [..], enable [..])`). An unknown key is serde's `` unknown field `<key>` ``.

- [ ] **Step 1: Write the failing tests**

In `crates/cli/src/config.rs`, at the end of `mod tests` (after `fn two_entries_differing_only_by_ledger_are_refused_naming_which_has_it`), add:

```rust
    fn mcp(root: &Path, disable: &[&str], enable: &[&str]) -> McpEntry {
        McpEntry {
            root: root.to_path_buf(),
            disable: disable.iter().map(|s| s.to_string()).collect(),
            enable: enable.iter().map(|s| s.to_string()).collect(),
        }
    }

    // MCP spec §2.2: a project's machine switches, independent of any
    // `[[project]]` binding; both lists default to empty.
    #[test]
    fn an_mcp_entry_loads_with_its_switches() {
        let cfg = load_text(
            "[[mcp]]\nroot = \"/code/app\"\ndisable = [\"github\"]\nenable = [\"sentry\"]\n\
             [[mcp]]\nroot = \"/code/other\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.mcp,
            vec![
                mcp(Path::new("/code/app"), &["github"], &["sentry"]),
                mcp(Path::new("/code/other"), &[], &[]),
            ]
        );
        assert!(cfg.projects.is_empty());
        let cfg = load_text("[[project]]\nroot = \"/r\"\nstore = \"/s.redb\"\n").unwrap();
        assert!(
            cfg.mcp.is_empty(),
            "a config with no [[mcp]] has no switches"
        );
    }

    // A typo in a switch is refused, not ignored; `antigravity` was a key
    // of an earlier design and is not one now (MCP spec §2.2).
    #[test]
    fn an_unknown_key_in_an_mcp_entry_is_refused() {
        for key in ["antigravity", "disabled", "store"] {
            let text = format!("[[mcp]]\nroot = \"/code/app\"\n{key} = \"x\"\n");
            let msg = format!("{:#}", load_text(&text).expect_err(key));
            assert!(
                msg.contains(&format!("unknown field `{key}`")),
                "{key}: {msg}"
            );
        }
    }

    #[test]
    fn a_relative_mcp_root_is_refused() {
        for bad in ["code/app", ""] {
            let text = format!("[[mcp]]\nroot = \"{bad}\"\n");
            let msg = format!("{:#}", load_text(&text).expect_err(bad));
            assert!(
                msg.contains("`root` in `[[mcp]]` must be an absolute path"),
                "{bad:?}: {msg}"
            );
        }
        assert_eq!(
            load_text("[[mcp]]\nroot = \"/code/app\"\n")
                .unwrap()
                .mcp
                .len(),
            1
        );
    }

    // MCP spec §2.2: "The same name in `enable` and `disable` is an error"
    // — within one entry; two projects may switch one name each way.
    #[test]
    fn a_server_both_enabled_and_disabled_is_refused_naming_it() {
        let text = "[[mcp]]\nroot = \"/code/app\"\n\
                    enable = [\"sentry\", \"github\"]\ndisable = [\"docs\", \"github\"]\n";
        let msg = format!("{:#}", load_text(text).unwrap_err());
        assert!(
            msg.contains("`github` is in both `enable` and `disable`"),
            "{msg}"
        );
        assert!(!msg.contains("`sentry` is in both"), "{msg}");
        let cfg = load_text(
            "[[mcp]]\nroot = \"/code/app\"\nenable = [\"github\"]\n\
             [[mcp]]\nroot = \"/code/other\"\ndisable = [\"github\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.mcp.len(), 2);
    }

    // MCP spec §2.2: the `[[project]]` rule — the entry whose root is the
    // longest ancestor of the working directory — and no other.
    #[test]
    fn mcp_entry_picks_the_entry_with_the_longest_root() {
        let outer = tempfile::tempdir().unwrap();
        let inner = outer.path().join("app");
        std::fs::create_dir_all(inner.join("src")).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let entries = vec![
            mcp(outer.path(), &["outer"], &[]),
            mcp(&inner, &["inner"], &[]),
            mcp(outer.path(), &["outer-too"], &[]),
            mcp(elsewhere.path(), &["elsewhere"], &[]),
        ];
        let got = mcp_entry(&entries, &inner.join("src")).unwrap();
        assert_eq!(got, Some(entries[1].clone()), "the longest root wins");
        assert_eq!(
            mcp_entry(&entries, &inner).unwrap(),
            Some(entries[1].clone())
        );
        let none = tempfile::tempdir().unwrap();
        assert_eq!(mcp_entry(&entries, none.path()).unwrap(), None);
        assert_eq!(
            mcp_entry(&entries, &none.path().join("not-there")).unwrap(),
            None,
            "a working directory that does not exist binds nothing"
        );
        assert_eq!(mcp_entry(&[], &inner).unwrap(), None);
    }

    #[test]
    fn two_different_mcp_entries_on_one_root_are_refused_naming_both() {
        let root = tempfile::tempdir().unwrap();
        for (a, b) in [
            (
                mcp(root.path(), &["github"], &[]),
                mcp(root.path(), &["sentry"], &[]),
            ),
            (
                mcp(root.path(), &["github"], &["docs"]),
                mcp(root.path(), &["github"], &[]),
            ),
        ] {
            let msg = format!("{:#}", mcp_entry(&[a, b], root.path()).unwrap_err());
            assert!(
                msg.contains("has more than one `[[mcp]]` entry in the config"),
                "{msg}"
            );
            assert!(msg.contains("Keep one of these entries"), "{msg}");
            assert_eq!(msg.matches(" (disable [").count(), 2, "{msg}");
        }
        let msg = format!(
            "{:#}",
            mcp_entry(
                &[
                    mcp(root.path(), &["github"], &[]),
                    mcp(root.path(), &["sentry"], &["docs"]),
                ],
                root.path()
            )
            .unwrap_err()
        );
        assert!(
            msg.contains("(disable [\"github\"], enable [])")
                && msg.contains("(disable [\"sentry\"], enable [\"docs\"])"),
            "the refusal must name both entries: {msg}"
        );
    }

    // Two entries that say the same thing about one root — however the root
    // is spelled — are one choice, not a conflict.
    #[test]
    fn identical_mcp_entries_on_one_root_are_not_a_conflict() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        let e = mcp(root.path(), &["github"], &["sentry"]);
        let respelled = mcp(&root.path().join("sub/.."), &["github"], &["sentry"]);
        assert_eq!(
            mcp_entry(&[e.clone(), respelled], root.path()).unwrap(),
            Some(e.clone())
        );
        assert_eq!(
            mcp_entry(&[e.clone(), e.clone()], root.path()).unwrap(),
            Some(e)
        );
    }

    #[test]
    fn an_empty_or_relative_xdg_state_home_falls_back_to_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            state_dir(Some(PathBuf::from("")), home.clone()),
            Some(PathBuf::from("/home/u/.local/state")),
            "an empty XDG_STATE_HOME must be treated as unset"
        );
        assert_eq!(
            state_dir(Some(PathBuf::from("relative/state")), home.clone()),
            Some(PathBuf::from("/home/u/.local/state")),
            "a relative XDG_STATE_HOME must be treated as unset"
        );
        assert_eq!(
            state_dir(Some(PathBuf::from("/abs/state")), home.clone()),
            Some(PathBuf::from("/abs/state")),
            "an absolute XDG_STATE_HOME must still win"
        );
        assert_eq!(
            state_dir(None, home),
            Some(PathBuf::from("/home/u/.local/state"))
        );
        assert_eq!(state_dir(Some(PathBuf::from("rel")), None), None);
        assert_eq!(
            state_dir(Some(PathBuf::from("/abs/state")), None),
            Some(PathBuf::from("/abs/state"))
        );
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --bin fl config::tests::`
Expected: FAIL to compile — 21 errors, all of them the missing type, field and functions: "cannot find type `McpEntry` in this scope" (E0425) and "cannot find struct, variant or union type `McpEntry`" (E0422), "cannot find function `mcp_entry` in this scope" and "cannot find function `state_dir` in this scope" (E0425), and "no field `mcp` on type `config::Config`" (E0609).

- [ ] **Step 3: Implement**

In `crates/cli/src/config.rs`, replace `#[derive(Debug, Default)] pub struct Config { … }` and `#[derive(Debug, Deserialize)] #[serde(deny_unknown_fields)] struct File { … }` (between `pub struct GithubApp { … }` and `/// The XDG config base directory`) with:

```rust
/// `[[mcp]]`: this machine's switches for one project's MCP catalog (MCP
/// spec §2.2), found by the same longest-ancestor rule as `[[project]]` and
/// independent of it, so a project needs no store binding to use the
/// catalog. `disable` turns off a server the team default turns on, and
/// `enable` the reverse. Whether each name is in the catalog is checked
/// against the catalog, not here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEntry {
    pub root: PathBuf,
    #[serde(default)]
    pub disable: Vec<String>,
    #[serde(default)]
    pub enable: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Config {
    pub projects: Vec<Entry>,
    pub github: Option<GithubApp>,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by `fl mcp`, not yet built")
    )]
    pub mcp: Vec<McpEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    project: Vec<Entry>,
    #[serde(default)]
    github: Option<GithubApp>,
    /// ⚠ The top level refuses a key it does not know, so an fl older than
    /// this table refuses a config that has one — for every command, since
    /// every command reads the config first. Upgrading fl on that machine is
    /// the remedy (MCP spec §2.2, release scope).
    #[serde(default)]
    mcp: Vec<McpEntry>,
}
```

Replace `fn xdg_base`, with its doc comment (it begins `/// The one rule both XDG bases follow`), by the following, which adds `state_dir` and `fl_state_dir` after `pub fn data_dir` and says "every" XDG base where it said "both":

```rust
/// The XDG state base directory, by the same rule as [`data_dir`]:
/// `xdg_state_home` if it is an ABSOLUTE path, else `home/.local/state`. An
/// empty or relative `$XDG_STATE_HOME` is treated as unset.
pub fn state_dir(xdg_state_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_base(xdg_state_home, home, ".local/state")
}

/// fl's own state directory, `<state base>/fl`, from this process's
/// `$XDG_STATE_HOME` and `$HOME` by [`state_dir`]'s rule. `None` when
/// neither gives an absolute base.
#[expect(dead_code, reason = "read by `fl mcp`, not yet built")]
pub fn fl_state_dir() -> Option<PathBuf> {
    let base = state_dir(
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )?;
    Some(base.join("fl"))
}

/// The one rule every XDG base follows: the variable if it is absolute, else
/// `home` joined with the spec's default for that base.
fn xdg_base(var: Option<PathBuf>, home: Option<PathBuf>, default: &str) -> Option<PathBuf> {
    var.filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(default)))
}
```

In `pub fn load`, after the `if let Some(app) = &file.github && !app.private_key.is_absolute() { … }` check, add:

```rust
    for m in &file.mcp {
        if !m.root.is_absolute() {
            bail!(
                "{}: `root` in `[[mcp]]` must be an absolute path (got `{}`)",
                path.display(),
                m.root.display()
            );
        }
        // MCP spec §2.2: one name switched both ways says nothing.
        if let Some(name) = m.enable.iter().find(|n| m.disable.contains(n)) {
            bail!(
                "{}: in the `[[mcp]]` entry for {}, `{name}` is in both `enable` and \
                 `disable`; keep it in one",
                path.display(),
                m.root.display()
            );
        }
    }
```

and in the `Ok(Config { … })` that ends it, after `github: file.github,`, add:

```rust
        mcp: file.mcp,
```

Replace `pub fn bound_entry`, with its doc comment, by the shared helper and `bound_entry` rebuilt on it (the doc comment and everything from `let mut distinct` on are as before, except that a winner is now `&Entry`, not `&(PathBuf, &Entry)`):

```rust
/// The items whose root is the longest ancestor of `cwd`, in their order in
/// `items`, with `cwd` canonicalized; `None` when `cwd` does not exist or no
/// item's root is an ancestor of it. Both sides are canonicalized, so a
/// symlinked path binds like the real one, and a root that does not exist
/// is skipped. More than one item comes back only when their roots are the
/// same directory: the caller decides whether that tie is a conflict.
fn longest_ancestor<'a, T>(
    items: &'a [T],
    root_of: impl Fn(&T) -> &Path,
    cwd: &Path,
) -> Result<Option<(PathBuf, Vec<&'a T>)>> {
    let Some(cwd) = canonicalize(cwd)? else {
        return Ok(None);
    };
    let mut matches: Vec<(PathBuf, &T)> = Vec::new();
    for item in items {
        let Some(root) = canonicalize(root_of(item))? else {
            continue;
        };
        if cwd.starts_with(&root) {
            matches.push((root, item));
        }
    }
    let Some(longest) = matches.iter().map(|(r, _)| r.components().count()).max() else {
        return Ok(None);
    };
    let winners = matches
        .into_iter()
        .filter(|(r, _)| r.components().count() == longest)
        .map(|(_, item)| item)
        .collect();
    Ok(Some((cwd, winners)))
}

/// The config entry for the project containing `cwd`: the entry whose root
/// is the longest ancestor of `cwd`. Both sides are canonicalized, so a
/// symlinked path binds like the real one.
///
/// §2.6 binds a project to exactly one store, and a project has one
/// tracker. Two (or more) entries whose canonical `root` is identical — the
/// longest match is therefore tied — but whose `(store, tracker)` differs
/// are refused rather than silently picking one: nothing chose between them.
pub fn bound_entry(entries: &[Entry], cwd: &Path) -> Result<Option<Entry>> {
    let Some((cwd, winners)) = longest_ancestor(entries, |e| &e.root, cwd)? else {
        return Ok(None);
    };
    let mut distinct: Vec<(&PathBuf, &Option<TrackerBinding>)> = Vec::new();
    for e in &winners {
        if !distinct.contains(&(&e.store, &e.tracker)) {
            distinct.push((&e.store, &e.tracker));
        }
    }
    if distinct.len() > 1 {
        let names = winners
            .iter()
            .map(|e| {
                let tracker = match &e.tracker {
                    Some(t) if t.github_ledger() => format!("github:{} (ledger github)", t.github),
                    Some(t) => format!("github:{}", t.github),
                    None => "the store's own tracker".to_string(),
                };
                format!("{} -> {} -> {tracker}", e.root.display(), e.store.display())
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "the project at {} is bound to more than one store or tracker in the config: \
             {names}. Remove all but one of these entries",
            cwd.display()
        );
    }
    Ok(Some(winners[0].clone()))
}
```

After `bound_entry`, before `#[cfg(test)]`, add:

```rust
/// The `[[mcp]]` entry for the project containing `cwd`, by
/// [`bound_entry`]'s rule: the entry whose root is the longest ancestor of
/// `cwd` (MCP spec §2.2). Two entries on one root that switch differently
/// are refused, naming both: nothing chose between them. Two that switch
/// alike are one choice, however their roots are spelled.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "read by `fl mcp`, not yet built")
)]
pub fn mcp_entry(entries: &[McpEntry], cwd: &Path) -> Result<Option<McpEntry>> {
    let Some((cwd, winners)) = longest_ancestor(entries, |e| &e.root, cwd)? else {
        return Ok(None);
    };
    let first = winners[0];
    if winners
        .iter()
        .any(|e| (&e.disable, &e.enable) != (&first.disable, &first.enable))
    {
        let names = winners
            .iter()
            .map(|e| {
                format!(
                    "{} (disable {:?}, enable {:?})",
                    e.root.display(),
                    e.disable,
                    e.enable
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "the project at {} has more than one `[[mcp]]` entry in the config: {names}. \
             Keep one of these entries",
            cwd.display()
        );
    }
    Ok(Some(first.clone()))
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli --bin fl config::tests::`
Expected: PASS — 20 tests, among them the 8 new ones (`an_mcp_entry_loads_with_its_switches`, `an_unknown_key_in_an_mcp_entry_is_refused`, `a_relative_mcp_root_is_refused`, `a_server_both_enabled_and_disabled_is_refused_naming_it`, `mcp_entry_picks_the_entry_with_the_longest_root`, `two_different_mcp_entries_on_one_root_are_refused_naming_both`, `identical_mcp_entries_on_one_root_are_not_a_conflict`, `an_empty_or_relative_xdg_state_home_falls_back_to_home`) and the 12 that were there, unchanged — `bound_entry`'s among them.

- [ ] **Step 5: Mutation checks**

Each filter is `cargo test -p fl-cli --bin fl config::tests::<name>`; "relative" is `a_relative_mcp_root_is_refused`, "both" is `a_server_both_enabled_and_disabled_is_refused_naming_it`, "unknown" is `an_unknown_key_in_an_mcp_entry_is_refused`, "loads" is `an_mcp_entry_loads_with_its_switches`, "longest" is `mcp_entry_picks_the_entry_with_the_longest_root`, "different" is `two_different_mcp_entries_on_one_root_are_refused_naming_both`, "identical" is `identical_mcp_entries_on_one_root_are_not_a_conflict`, "state" is `an_empty_or_relative_xdg_state_home_falls_back_to_home`.

1. The `[[mcp]]` root check: `if !m.root.is_absolute() {` → `if false {` → relative red.
2. The enable/disable check: `m.enable.iter().find(|n| m.disable.contains(n))` → `None::<&String>` → both red.
3. It looks past the first name: `m.enable.first().filter(|n| m.disable.contains(n))` → both red (`github` is the second name).
4. It looks within one entry: `m.disable.contains(n)` → `file.mcp.iter().any(|o| o.disable.contains(n))` → both red (two entries switching one name each way load).
5. `McpEntry`'s `#[serde(deny_unknown_fields)]` deleted → unknown red.
6. `#[serde(default)]` on `disable` deleted → loads red (missing field); on `enable` → loads red; on `File`'s `mcp` → loads red (a config with no `[[mcp]]` table is refused).
7. The longest root wins: `.max()` → `.min()` in `longest_ancestor` → longest red.
8. Only the longest wins: `== longest` → `<= longest` → longest red (the outer entries tie with the inner one and are refused).
9. Only an ancestor matches: `if cwd.starts_with(&root) {` → `if true {` → longest red (an unrelated directory binds the inner entry).
10. The tie compares `enable`: `.any(|e| e.disable != first.disable)` → different red (the second pair differs only in `enable`).
11. The tie compares `disable`: `.any(|e| e.enable != first.enable)` → different red.
12. A tie of different entries is refused: `.any(|_| false)` → different red.
13. A tie of identical entries is accepted: `.any(|_| winners.len() > 1)` → identical red.
14. Identical means the switches, at one canonical root: `.any(|e| **e != *first)` → identical red (the respelled root `sub/..` differs as written).
15. The refusal names every entry: `.take(1)` before `.map(|e| {` in `mcp_entry`'s `names` → different red (one `(disable [` where two are asserted).
16. `state_dir`'s default: `".local/state"` → `".local/share"` → state red.
17. `state_dir`'s absolute-only filter: its body → `xdg_state_home.or_else(|| home.map(|h| h.join(".local/state")))` → state red (an empty and a relative variable are used as-is).

`bound_entry`'s tie rule and messages are guarded by its existing tests (`two_entries_with_the_same_root_but_different_stores_are_refused`, `two_entries_differing_only_by_tracker_are_refused_naming_each_tracker`, `two_entries_differing_only_by_ledger_are_refused_naming_which_has_it`, `identical_entries_are_not_a_conflict`, `bound_resolves_a_symlinked_cwd_passed_directly`), unchanged and green; mutations 7–9 act on the helper both lookups share.

Not observable:
- `fl_state_dir` reads this process's environment; a unit test would have to set `XDG_STATE_HOME` in a process whose other tests run in parallel (`std::env::set_var` is `unsafe` in Rust 2024 for that reason). Its rule is `state_dir`'s, pinned above; Task 7's black-box tests run `fl mcp` with `XDG_STATE_HOME` set and Task 8's home guard fails if a record lands outside it.
- The `expect(dead_code)` attributes are not guards: removing one is a warning under `-D warnings` until Task 7 reads the item, and keeping one after is the same warning.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1218 passed, 19 ignored (1210 and 19 before this task).

- [ ] **Step 7: Commit**

```bash
git add crates/cli/src/config.rs
git commit -m "feat(cli): the [[mcp]] machine switches and the XDG state directory

The user's config gains an [[mcp]] table: a project root, and the names
this machine disables or enables against the catalog's team default.
Unknown keys are refused, antigravity among them; a relative root is
refused, and so is a name in both enable and disable of one entry.
mcp_entry finds the entry by the longest-ancestor rule of [[project]],
now one helper both lookups share; bound_entry keeps its tie rule and
its messages. Two [[mcp]] entries on one root are refused unless they
switch alike, naming both. state_dir gives the XDG state base by the
rule data_dir follows, with ~/.local/state as its default. An older fl
refuses a config with an [[mcp]] table. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 3: The registry client, its guards, and a fake registry

`fl mcp search`, `add --from` and `upgrade` read an MCP registry, API `v0.1` (MCP spec §3.1); `sync` and `check` never do (§5). This task gives fl-mcp its own read-only client for it, with the guards written there (plan ruling 9): the address is https, or http to `127.0.0.1`, `localhost` or `[::1]` with no user name, checked at construction with Task 1's `check_registry_url`; no credential is ever sent (§3.1), only `User-Agent: fl/<version>`; no redirect is followed; the status and the content type are judged before the body is read as data; a body over 4 MiB is refused; an HTML page, at any status, is the registry's failure. Each failure has its own message and none reads as a parse error (Review Focus 5). Names and versions are percent-encoded in full by a small encoder, so `/` and `+` reach the registry as `%2F` and `%2B` (plan ruling 11). `search` sends the registry's own `search` parameter with `version=latest` and pages by `metadata.nextCursor` up to 20 pages, saying when it stopped early (ruling 12), and keeps only the servers whose name holds the text, in any case, so a registry that ignores the parameter cannot flood the list (owner decision 13); `versions` and `version` ask with `include_deleted=true`, so a deleted entry comes back with its status for Task 4 to refuse, rather than as a 404 (ruling 13). The response models read exactly the fields freezing needs and ignore every other, because the registry adds fields within `v0.1`; `registryType` and a transport's `type` are open strings in the registry's schema, so they keep an unknown value as `Other(String)` for Task 4 to refuse by name, and so does an argument's `type`, because the registry serves entries its own schema does not allow (plan ruling 31: an argument of type `flag` was seen live). For the same reason `search` reads only what it shows — each server's name, description, version and status, the status as an open string — so one server whose launch spec breaks the schema never fails a page. A fake registry on `tiny_http`, behind a `fake` feature as in fl-github, serves `io.example/…` fixtures (§7.1) and has a one-shot knob for each misbehaviour.

**Blast radius:** fl-mcp only. `McpError` gains four variants (`RegistryAddress`, `Unreachable`, `Registry`, `NotFound`); nothing outside fl-mcp names `McpError` yet, and the catalog's code never builds them. `crates/mcp/Cargo.toml` gains `ureq` and an optional `tiny_http` behind a new feature `fake`, off by default — both already in the workspace through fl-github, so `Cargo.lock` gains two lines in fl-mcp's dependency list and no new package. A build of fl without `fake` holds no fake.

**Files:**
- Modify: `crates/mcp/Cargo.toml` (`ureq`, `tiny_http`, feature `fake`)
- Modify: `crates/mcp/src/lib.rs` (`mod fake`, `mod registry`; four `McpError` variants and `fn missing`)
- Create: `crates/mcp/src/registry.rs` (the client, its guards, the models; tests)
- Create: `crates/mcp/src/fake.rs` (`FakeRegistry`, its knobs, the fixtures)
- Modify: `Cargo.lock` (fl-mcp's dependency list)

**Interfaces:**
- Consumes: Task 1's `catalog::check_registry_url(url: &str) -> Result<(), String>` (its `Err` is a clause) and `McpError`.
- Produces (`fl_mcp::registry`):
  - `pub const API_VERSION: &str = "v0.1"`, `pub const BODY_LIMIT: u64 = 4 << 20`, `pub const SEARCH_PAGES: usize = 20`.
  - `pub struct Registry` with `pub fn new(url: &str) -> Result<Registry, McpError>` (refused: `McpError::RegistryAddress`; a trailing `/` is dropped, a path prefix kept), `pub fn url(&self) -> &str`, `pub fn search(&self, text: &str) -> Result<Search, McpError>`, `pub fn versions(&self, name: &str) -> Result<Vec<ServerResponse>, McpError>`, `pub fn version(&self, name: &str, version: &str) -> Result<ServerResponse, McpError>` (`version` may be `latest`; the answer carries the real one). `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Search { pub servers: Vec<Summary>, pub stopped_early: bool }` — `servers` in the registry's order, each at its latest version; a failure on any page is an error, never a short list. `#[derive(Debug, Clone, PartialEq, Eq, Deserialize)] pub struct Summary { pub name: String, pub description: String /* default "" */, pub version: String, pub status: String }` — `status` as the registry spells it (`active`, `deprecated`, or one fl does not know); nothing else of the entry is read.
  - Models, each `#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]`, unknown fields ignored: `ServerResponse { server: ServerJson, meta: Official }` (`meta` is `_meta."io.modelcontextprotocol.registry/official"`); `Official { status: Status, status_message: Option<String>, is_latest: bool }`; `enum Status { Active, Deprecated, Deleted }` (`Copy`; closed — any other value is "not the registry API's shape"); `ServerJson { name: String, description: String /* default "" */, version: String, packages: Vec<Package>, remotes: Vec<Remote> }`; `Package { registry_type: RegistryType, identifier: String, version: Option<String>, transport: PackageTransport, runtime_arguments: Vec<Argument>, package_arguments: Vec<Argument>, environment_variables: Vec<KeyValueInput> }`; `PackageTransport { kind: TransportKind, url: Option<String> }`; `enum RegistryType { Npm, Pypi, Oci, Other(String) }` and `enum TransportKind { Stdio, StreamableHttp, Sse, Other(String) }`, each with `as_str(&self) -> &str` (the registry's own spelling); `Argument { kind: ArgumentKind, name: Option<String>, value: Option<String>, value_hint: Option<String>, default: Option<String>, is_required: bool, is_secret: bool, format: Option<String>, variables: BTreeMap<String, Input> }`; `enum ArgumentKind { Positional, Named, Other(String) }` (open, like `RegistryType`; not `Copy`), with `as_str(&self) -> &str`; `Input { value, default: Option<String>, is_required, is_secret: bool, format: Option<String> }`; `KeyValueInput { name: String, value: Option<String>, default: Option<String>, is_required: bool, is_secret: bool, variables: BTreeMap<String, Input> }`; `Remote { kind: TransportKind, url: String, headers: Vec<KeyValueInput>, variables: BTreeMap<String, Input> }`. Every field is `pub`; the JSON `type` field is `kind` in Rust.
  - `McpError` gains `RegistryAddress { url: String, clause: String }`, `Unreachable { registry: String, cause: String }`, `Registry { registry: String, request: String, problem: String }` (`request` is `GET <path and query>`), `NotFound { registry: String, name: String, version: Option<String> }`.
- Produces (`fl_mcp::fake`, feature `fake` or `cfg(test)`): `FakeRegistry::start() -> FakeRegistry` (on `127.0.0.1:0`), `url(&self) -> String`, `state(&self) -> MutexGuard<'_, State>`, `add_server(&self, name: &str, version: &str)` (an active, latest npm server). `pub struct State { pub requests: Vec<String> /* "GET <path and query>" */, pub headers: Vec<Vec<(String, String)>> /* names lowercased */, pub entries: Vec<serde_json::Value>, pub page_limit: usize /* 100 */, pub redirect_next: Option<String> /* the Location */, pub html_502_next: bool, pub oversized_next: bool, pub problem_500_next: bool, pub ignores_search: bool /* a setting: list every server whatever `search` asks */ }`; the four misbehaviours are one-shot. Fixture constants: `NOTES` (`io.example/notes`, npm, `NOTES_VERSIONS = ["0.9.0+build.7", "1.0.0", "1.1.0", "1.2.0"]`, the last latest; secret `NOTES_TOKEN`, plain `NOTES_LOG` default `info`, a required positional with default `./notes`), `WEATHER` (pypi `example-weather-mcp` 0.4.1, `--units` default `metric`, secret `WEATHER_API_KEY`), `TRACKER` + `TRACKER_IMAGE` (`ghcr.io/example/tracker-mcp:2.0.1`, no package `version`; runtime `-e TRACKER_PORT=8085` and `-e TRACKER_TOKEN={token}` with `token` secret and required), `DOCS` + `DOCS_URL` (a streamable-http remote, a secret `Authorization` header with no value), `MULTI` (npm, OCI and an sse remote), `LEGACY` + `LEGACY_MESSAGE` (deprecated), `GONE` + `GONE_MESSAGE` (deleted, hidden without `include_deleted=true`), `VERBOSE` (npm `@example/verbose-mcp` 1.0.0 whose one package argument is `{"type": "flag", "name": "--verbose"}`), `UPGRADING` + `UPGRADING_VERSIONS = ["1.0.0", "2.0.0"]` (npm `@example/upgrading-mcp`; 2.0.0, the latest, adds `UPGRADING_HOME`, required, not secret, no value, and `UPGRADING_TOKEN`, an optional secret), `LISTED` (the eight a search shows, in name order).
- Unique phrases: `is not a registry address fl will use` (with Task 1's clause, e.g. `is not this machine`); `cannot reach the registry`; `fl follows no redirect` (with `answered 302 with a redirect off its own origin`, or `a redirect to <location>` on the registry's own origin); `answered with an HTML page` (with `(status 502)`); `larger than 4 MiB`; `answered 500` (any other status: `answered <status>`, then `: <detail>` for `application/problem+json`); `is not the registry API's shape` (a 2xx body that does not read); ``has no server `<name>` `` and ``has no version `<v>` of server `<name>` `` (`NotFound`).

- [ ] **Step 1: Write the failing tests**

In `crates/mcp/Cargo.toml`, replace `indexmap.workspace = true`, the blank line and the `[dev-dependencies]` table after it with:

```toml
indexmap.workspace = true
ureq.workspace = true
tiny_http = { workspace = true, optional = true }

[features]
# The in-process fake registry, for this crate's tests and the CLI's.
fake = ["dep:tiny_http"]

[dev-dependencies]
tempfile.workspace = true
tiny_http.workspace = true
```

In `crates/mcp/src/lib.rs`, replace `pub mod catalog;` with:

```rust
pub mod catalog;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod registry;
```

Create `crates/mcp/src/fake.rs`:

```rust
//! An in-process fake of an MCP registry's read API, `v0.1` (MCP spec §7.1).
//!
//! ⚠ It proves structure, not integration: it agrees with fl because both
//! were written from the same reading of the registry's OpenAPI document. The
//! live test reads the real registry.

use serde_json::{Value, json};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// npm; four versions ([`NOTES_VERSIONS`]), the last one latest; a secret
/// and a plain environment variable, a positional package argument.
pub const NOTES: &str = "io.example/notes";
/// Every version of [`NOTES`], oldest first. The first carries build
/// metadata, so its path segment holds an encoded `+`.
pub const NOTES_VERSIONS: [&str; 4] = ["0.9.0+build.7", "1.0.0", "1.1.0", "1.2.0"];
/// PyPI; a named package argument with a default, a secret variable.
pub const WEATHER: &str = "io.example/weather";
/// OCI, its identifier tagged ([`TRACKER_IMAGE`]); its runtime arguments
/// are `-e TRACKER_PORT=8085` and `-e TRACKER_TOKEN={token}`, `token` secret.
pub const TRACKER: &str = "io.example/tracker";
pub const TRACKER_IMAGE: &str = "ghcr.io/example/tracker-mcp:2.0.1";
/// A streamable-http remote whose `Authorization` header is secret and has
/// no value.
pub const DOCS: &str = "io.example/docs";
pub const DOCS_URL: &str = "https://docs.example.com/mcp";
/// Three launch routes: an npm package, an OCI package and an sse remote.
pub const MULTI: &str = "io.example/multi";
/// Deprecated, with [`LEGACY_MESSAGE`].
pub const LEGACY: &str = "io.example/legacy";
pub const LEGACY_MESSAGE: &str = "Replaced by io.example/notes.";
/// Deleted, with [`GONE_MESSAGE`]: hidden unless a request asks with
/// `include_deleted=true`, as the real registry hides it.
pub const GONE: &str = "io.example/gone";
pub const GONE_MESSAGE: &str = "Withdrawn by its publisher.";
/// npm, with a package argument of type `flag`, which the registry's schema
/// does not define and the registry serves.
pub const VERBOSE: &str = "io.example/verbose";
/// npm, two versions: [`UPGRADING_VERSIONS`]. The second, the latest, adds
/// a required variable that is not secret and has no value,
/// `UPGRADING_HOME`, and an optional secret, `UPGRADING_TOKEN`.
pub const UPGRADING: &str = "io.example/upgrading";
pub const UPGRADING_VERSIONS: [&str; 2] = ["1.0.0", "2.0.0"];
/// Every fixture a search shows by default, in name order: all but [`GONE`].
pub const LISTED: [&str; 8] = [
    DOCS, LEGACY, MULTI, NOTES, TRACKER, UPGRADING, VERBOSE, WEATHER,
];

const SCHEMA: &str = "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json";
const WHEN: &str = "2026-10-01T00:00:00Z";

#[derive(Debug, Default)]
pub struct State {
    /// Every request, as `GET <path and query>`, in order.
    pub requests: Vec<String>,
    /// Each request's headers, names lowercased, in the order of `requests`.
    pub headers: Vec<Vec<(String, String)>>,
    /// What the fake serves: one `ServerResponse` per server version, in
    /// the order they were published.
    pub entries: Vec<Value>,
    /// The most servers one page of `GET /v0.1/servers` holds, whatever
    /// `limit` asks. A setting; the registry's own maximum, 100, by default.
    pub page_limit: usize,
    /// The next request answers 302 with this `Location`. One-shot.
    pub redirect_next: Option<String>,
    /// The next request answers 502 with an HTML page, as a proxy in front
    /// of a registry does. One-shot.
    pub html_502_next: bool,
    /// The next request answers 200 with a valid list over 4 MiB. One-shot.
    pub oversized_next: bool,
    /// The next request answers 500 with `application/problem+json`, as the
    /// real registry once did. One-shot.
    pub problem_500_next: bool,
    /// `GET /v0.1/servers` ignores `search` and lists every server, as a
    /// registry without the parameter would. A setting; off by default.
    pub ignores_search: bool,
}

pub struct FakeRegistry {
    server: Arc<tiny_http::Server>,
    thread: Option<JoinHandle<()>>,
    state: Arc<Mutex<State>>,
    url: String,
}

impl FakeRegistry {
    /// A registry on `127.0.0.1`, serving the fixtures above.
    pub fn start() -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind a local port"));
        let port = server.server_addr().to_ip().expect("an IP listener").port();
        let url = format!("http://127.0.0.1:{port}");
        let state = Arc::new(Mutex::new(State {
            entries: fixtures(),
            page_limit: 100,
            ..State::default()
        }));
        let (srv, st) = (Arc::clone(&server), Arc::clone(&state));
        let thread = std::thread::spawn(move || {
            while let Ok(req) = srv.recv() {
                let url = req.url().to_string();
                let headers = req
                    .headers()
                    .iter()
                    .map(|h| {
                        (
                            h.field.as_str().as_str().to_ascii_lowercase(),
                            h.value.as_str().to_string(),
                        )
                    })
                    .collect();
                let answer = {
                    let mut s = st.lock().unwrap();
                    s.requests.push(format!("{} {url}", req.method()));
                    s.headers.push(headers);
                    route(&mut s, &url)
                };
                let mut resp = tiny_http::Response::from_string(answer.body)
                    .with_status_code(answer.status)
                    .with_header(header("Content-Type", answer.content_type));
                if let Some(location) = answer.location {
                    resp = resp.with_header(header("Location", &location));
                }
                let _ = req.respond(resp);
            }
        });
        Self {
            server,
            thread: Some(thread),
            state,
            url,
        }
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// ⚠ Locks the fake: take one guard and drop it before the next call to
    /// the registry, or the fake deadlocks on its own lock.
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Publishes `name` at `version`: an npm package, active, latest.
    pub fn add_server(&self, name: &str, version: &str) {
        let package = json!({
            "registryType": "npm",
            "identifier": format!("@example/{}", name.rsplit('/').next().unwrap_or(name)),
            "version": version,
            "transport": { "type": "stdio" }
        });
        let entry = entry(name, version, true, vec![package], vec![]);
        self.state().entries.push(entry);
    }
}

impl Drop for FakeRegistry {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn header(k: &str, v: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("an ASCII header")
}

struct Answer {
    status: u16,
    content_type: &'static str,
    body: String,
    location: Option<String>,
}

impl Answer {
    fn json(status: u16, body: Value) -> Self {
        Answer {
            status,
            content_type: "application/json",
            body: body.to_string(),
            location: None,
        }
    }

    fn problem(status: u16, title: &str, detail: &str) -> Self {
        Answer {
            status,
            content_type: "application/problem+json",
            body: json!({ "title": title, "status": status, "detail": detail }).to_string(),
            location: None,
        }
    }
}

fn route(s: &mut State, url: &str) -> Answer {
    if let Some(location) = s.redirect_next.take() {
        // A redirect as most servers send one: with a small HTML body.
        return Answer {
            status: 302,
            content_type: "text/html; charset=utf-8",
            body: format!("<a href=\"{location}\">Found</a>."),
            location: Some(location),
        };
    }
    if std::mem::take(&mut s.html_502_next) {
        return Answer {
            status: 502,
            content_type: "text/html; charset=utf-8",
            body: "<html><body><h1>502 Bad Gateway</h1></body></html>".into(),
            location: None,
        };
    }
    if std::mem::take(&mut s.problem_500_next) {
        return Answer::problem(500, "Internal Server Error", "Failed to get registry list");
    }
    if std::mem::take(&mut s.oversized_next) {
        let mut big = entry(NOTES, "1.2.0", true, vec![], vec![]);
        big["server"]["description"] = Value::String("x".repeat(9 << 19));
        return Answer::json(200, json!({ "servers": [big], "metadata": { "count": 1 } }));
    }
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let param = |key: &str| {
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (k == key).then(|| decode(v))
        })
    };
    let deleted_too = param("include_deleted").as_deref() == Some("true");
    let shown = |e: &&Value| deleted_too || official(e)["status"] != "deleted";
    let Some(rest) = path.strip_prefix("/v0.1/servers") else {
        return Answer::problem(404, "Not Found", "no such endpoint");
    };
    if rest.is_empty() {
        let search = (!s.ignores_search)
            .then(|| param("search").map(|t| t.to_lowercase()))
            .flatten();
        let version = param("version");
        let mut found: Vec<&Value> = s
            .entries
            .iter()
            .filter(shown)
            .filter(|e| match &search {
                Some(t) => name_of(e).to_lowercase().contains(t),
                None => true,
            })
            .filter(|e| match version.as_deref() {
                Some("latest") => official(e)["isLatest"] == true,
                Some(v) => e["server"]["version"] == v,
                None => true,
            })
            .collect();
        // Stable: versions of one server stay in the order they were published.
        found.sort_by_key(|e| name_of(e));
        let start = match param("cursor") {
            Some(c) => found
                .iter()
                .position(|e| cursor_of(e) == c)
                .map_or(found.len(), |i| i + 1),
            None => 0,
        };
        let limit = param("limit")
            .and_then(|l| l.parse::<usize>().ok())
            .unwrap_or(30)
            .min(s.page_limit);
        let page: Vec<Value> = found
            .iter()
            .skip(start)
            .take(limit)
            .map(|e| (*e).clone())
            .collect();
        let mut metadata = json!({ "count": page.len() });
        if start + page.len() < found.len()
            && let Some(last) = page.last()
        {
            metadata["nextCursor"] = Value::String(cursor_of(last));
        }
        return Answer::json(200, json!({ "servers": page, "metadata": metadata }));
    }
    // `/{name}/versions[/{version}]`: an unencoded `/` in the name splits it
    // into one more segment, and finds nothing — as on the real registry.
    let segments: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
    let (name, version) = match segments.as_slice() {
        [name, "versions"] => (decode(name), None),
        [name, "versions", version] => (decode(name), Some(decode(version))),
        _ => return Answer::problem(404, "Not Found", "Server not found"),
    };
    let versions: Vec<Value> = s
        .entries
        .iter()
        .filter(|e| name_of(e) == name)
        .filter(shown)
        .cloned()
        .collect();
    if versions.is_empty() {
        return Answer::problem(404, "Not Found", "Server not found");
    }
    match version.as_deref() {
        None => {
            let count = versions.len();
            Answer::json(
                200,
                json!({ "servers": versions, "metadata": { "count": count } }),
            )
        }
        Some(v) => {
            let hit = versions.into_iter().find(|e| match v {
                "latest" => official(e)["isLatest"] == true,
                v => e["server"]["version"] == v,
            });
            match hit {
                Some(e) => Answer::json(200, e),
                None => Answer::problem(404, "Not Found", "Server version not found"),
            }
        }
    }
}

fn official(entry: &Value) -> &Value {
    &entry["_meta"]["io.modelcontextprotocol.registry/official"]
}

fn name_of(entry: &Value) -> String {
    entry["server"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The cursor format the real registry was seen to use; fl treats it as
/// opaque.
fn cursor_of(entry: &Value) -> String {
    let version = entry["server"]["version"].as_str().unwrap_or_default();
    format!("{}:{version}", name_of(entry))
}

/// `%XX` → the byte; anything else as it is.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An active version of `name`.
fn entry(
    name: &str,
    version: &str,
    latest: bool,
    packages: Vec<Value>,
    remotes: Vec<Value>,
) -> Value {
    let official = json!({
        "status": "active",
        "statusChangedAt": WHEN,
        "publishedAt": WHEN,
        "updatedAt": WHEN,
        "isLatest": latest
    });
    let short = name.rsplit('/').next().unwrap_or(name);
    json!({
        "server": {
            "$schema": SCHEMA,
            "name": name,
            "title": short,
            "description": format!("The {short} server, for fl's tests."),
            "repository": { "url": format!("https://example.com/{short}"), "source": "github" },
            "version": version,
            "packages": packages,
            "remotes": remotes
        },
        "_meta": { "io.modelcontextprotocol.registry/official": official }
    })
}

fn with_status(mut entry: Value, status: &str, message: &str) -> Value {
    let official = &mut entry["_meta"]["io.modelcontextprotocol.registry/official"];
    official["status"] = Value::String(status.to_string());
    official["statusMessage"] = Value::String(message.to_string());
    entry
}

fn fixtures() -> Vec<Value> {
    let mut all = Vec::new();
    for (i, v) in NOTES_VERSIONS.iter().enumerate() {
        let package = json!({
            "registryType": "npm",
            "identifier": "@example/notes-mcp",
            "version": v,
            "runtimeHint": "npx",
            "transport": { "type": "stdio" },
            "packageArguments": [
                { "type": "positional", "valueHint": "notes_dir", "default": "./notes",
                  "isRequired": true, "description": "Where the notes live" }
            ],
            "environmentVariables": [
                { "name": "NOTES_TOKEN", "isSecret": true, "isRequired": true,
                  "description": "The notes service token" },
                { "name": "NOTES_LOG", "default": "info", "format": "string" }
            ]
        });
        let latest = i + 1 == NOTES_VERSIONS.len();
        all.push(entry(NOTES, v, latest, vec![package], vec![]));
    }
    let weather = json!({
        "registryType": "pypi",
        "identifier": "example-weather-mcp",
        "version": "0.4.1",
        "runtimeHint": "uvx",
        "transport": { "type": "stdio" },
        "packageArguments": [
            { "type": "named", "name": "--units", "default": "metric",
              "choices": ["metric", "imperial"] }
        ],
        "environmentVariables": [
            { "name": "WEATHER_API_KEY", "isSecret": true, "isRequired": true }
        ]
    });
    all.push(entry(WEATHER, "0.4.1", true, vec![weather], vec![]));
    let tracker = json!({
        "registryType": "oci",
        "identifier": TRACKER_IMAGE,
        "transport": { "type": "stdio" },
        "runtimeArguments": [
            { "type": "named", "name": "-e", "value": "TRACKER_PORT=8085" },
            { "type": "named", "name": "-e", "value": "TRACKER_TOKEN={token}",
              "variables": {
                  "token": { "format": "string", "isSecret": true, "isRequired": true }
              } }
        ]
    });
    all.push(entry(TRACKER, "2.0.1", true, vec![tracker], vec![]));
    let docs = json!({
        "type": "streamable-http",
        "url": DOCS_URL,
        "headers": [
            { "name": "Authorization", "isSecret": true,
              "description": "Authorization header with a token" }
        ]
    });
    all.push(entry(DOCS, "1.0.0", true, vec![], vec![docs]));
    let multi = vec![
        json!({ "registryType": "npm", "identifier": "@example/multi-mcp", "version": "3.0.0",
                "transport": { "type": "stdio" } }),
        json!({ "registryType": "oci", "identifier": "ghcr.io/example/multi-mcp:3.0.0",
                "transport": { "type": "stdio" } }),
    ];
    let multi_remote = json!({ "type": "sse", "url": "https://multi.example.com/sse" });
    all.push(entry(MULTI, "3.0.0", true, multi, vec![multi_remote]));
    let legacy = json!({ "registryType": "pypi", "identifier": "example-legacy-mcp",
                         "version": "0.1.0", "transport": { "type": "stdio" } });
    let legacy = entry(LEGACY, "0.1.0", true, vec![legacy], vec![]);
    all.push(with_status(legacy, "deprecated", LEGACY_MESSAGE));
    let verbose = json!({ "registryType": "npm", "identifier": "@example/verbose-mcp",
                          "version": "1.0.0", "transport": { "type": "stdio" },
                          "packageArguments": [ { "type": "flag", "name": "--verbose" } ] });
    all.push(entry(VERBOSE, "1.0.0", true, vec![verbose], vec![]));
    for (i, v) in UPGRADING_VERSIONS.iter().enumerate() {
        let latest = i + 1 == UPGRADING_VERSIONS.len();
        let variables = if latest {
            json!([
                { "name": "UPGRADING_HOME", "isRequired": true },
                { "name": "UPGRADING_TOKEN", "isSecret": true }
            ])
        } else {
            json!([])
        };
        let package = json!({ "registryType": "npm", "identifier": "@example/upgrading-mcp",
                              "version": v, "transport": { "type": "stdio" },
                              "environmentVariables": variables });
        all.push(entry(UPGRADING, v, latest, vec![package], vec![]));
    }
    let gone = json!({ "registryType": "npm", "identifier": "@example/gone-mcp",
                       "version": "1.0.0", "transport": { "type": "stdio" } });
    let gone = entry(GONE, "1.0.0", true, vec![gone], vec![]);
    all.push(with_status(gone, "deleted", GONE_MESSAGE));
    all
}
```

Create `crates/mcp/src/registry.rs` holding only the tests (Step 3 adds the client above them):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpError;
    use crate::fake::{self, FakeRegistry};

    fn client(fake: &FakeRegistry) -> Registry {
        Registry::new(&fake.url()).unwrap()
    }

    fn names(servers: &[Summary]) -> Vec<&str> {
        servers.iter().map(|s| s.name.as_str()).collect()
    }

    /// The phrase each failure is told by; a message holds its own and no
    /// other.
    const PHRASES: [&str; 7] = [
        "fl follows no redirect",
        "answered with an HTML page",
        "larger than 4 MiB",
        "has no server",
        "answered 500",
        "cannot reach the registry",
        "is not the registry API's shape",
    ];

    fn holds_only(msg: &str, phrase: &str) {
        assert!(msg.contains(phrase), "{phrase:?} not in: {msg}");
        for other in PHRASES.iter().filter(|p| **p != phrase) {
            assert!(!msg.contains(other), "{other:?} also in: {msg}");
        }
    }

    // MCP spec §3.1: the registry's own `search` parameter, a substring of
    // the name in any case, one entry per server.
    #[test]
    fn search_finds_names_in_any_case_at_their_latest_version() {
        let fake = FakeRegistry::start();
        let found = client(&fake).search("NoTeS").unwrap();
        assert_eq!(names(&found.servers), [fake::NOTES]);
        assert!(!found.stopped_early);
        let notes = &found.servers[0];
        assert_eq!(notes.version, "1.2.0");
        assert_eq!(notes.description, "The notes server, for fl's tests.");
        assert_eq!(notes.status, "active");
        let requests = fake.state().requests.clone();
        assert_eq!(
            requests,
            ["GET /v0.1/servers?search=NoTeS&version=latest&limit=100"]
        );
    }

    // A registry that ignores `search` sends every server; fl shows only
    // the names that hold the text, in any case.
    #[test]
    fn search_keeps_only_names_that_hold_the_text_whatever_the_registry_sends() {
        let fake = FakeRegistry::start();
        fake.add_server("io.example/MixedCase", "1.0.0");
        fake.state().ignores_search = true;
        let registry = client(&fake);
        let found = registry.search("NoTeS").unwrap();
        assert_eq!(names(&found.servers), [fake::NOTES]);
        let found = registry.search("mixedcase").unwrap();
        assert_eq!(names(&found.servers), ["io.example/MixedCase"]);
        let requests = fake.state().requests.clone();
        assert_eq!(
            requests,
            [
                "GET /v0.1/servers?search=NoTeS&version=latest&limit=100",
                "GET /v0.1/servers?search=mixedcase&version=latest&limit=100",
            ]
        );
    }

    #[test]
    fn search_reads_every_page_by_its_cursor() {
        let fake = FakeRegistry::start();
        fake.state().page_limit = 2;
        let found = client(&fake).search("io.example").unwrap();
        assert_eq!(names(&found.servers), fake::LISTED);
        assert!(!found.stopped_early);
        let requests = fake.state().requests.clone();
        assert_eq!(requests.len(), 4, "{requests:?}");
        assert!(
            requests[1].ends_with("&cursor=io.example%2Flegacy%3A0.1.0"),
            "{requests:?}"
        );
    }

    // MCP spec §3.1: up to 20 pages, and it says when it stopped early.
    #[test]
    fn search_stops_after_twenty_pages_and_says_so() {
        let fake = FakeRegistry::start();
        fake.state().page_limit = 1;
        for i in 0..25 {
            fake.add_server(&format!("io.example/many-{i:02}"), "1.0.0");
        }
        for i in 0..20 {
            fake.add_server(&format!("io.example/exact-{i:02}"), "1.0.0");
        }
        let found = client(&fake).search("many").unwrap();
        assert_eq!(found.servers.len(), 20);
        assert!(found.stopped_early, "five servers were left unread");
        assert_eq!(fake.state().requests.len(), 20);

        fake.state().requests.clear();
        let found = client(&fake).search("exact").unwrap();
        assert_eq!(found.servers.len(), 20);
        assert!(!found.stopped_early, "the twentieth page was the last");
        assert_eq!(fake.state().requests.len(), 20);
    }

    #[test]
    fn versions_lists_every_version_of_a_server() {
        let fake = FakeRegistry::start();
        let versions = client(&fake).versions(fake::NOTES).unwrap();
        let found: Vec<&str> = versions.iter().map(|v| v.server.version.as_str()).collect();
        assert_eq!(found, fake::NOTES_VERSIONS);
        let latest: Vec<bool> = versions.iter().map(|v| v.meta.is_latest).collect();
        assert_eq!(latest, [false, false, false, true]);
        let requests = fake.state().requests.clone();
        assert_eq!(
            requests,
            ["GET /v0.1/servers/io.example%2Fnotes/versions?include_deleted=true"]
        );
    }

    // The registry API (MCP spec §3.1): `latest` names the newest
    // version; a name and a version are each one encoded path segment.
    #[test]
    fn version_reads_one_version_or_the_latest() {
        let fake = FakeRegistry::start();
        let registry = client(&fake);
        let latest = registry.version(fake::NOTES, "latest").unwrap();
        assert_eq!(latest.server.version, "1.2.0");
        let old = registry.version(fake::NOTES, "0.9.0+build.7").unwrap();
        assert_eq!(old.server.version, "0.9.0+build.7");
        assert!(!old.meta.is_latest);
        let requests = fake.state().requests.clone();
        assert_eq!(
            requests,
            [
                "GET /v0.1/servers/io.example%2Fnotes/versions/latest?include_deleted=true",
                concat!(
                    "GET /v0.1/servers/io.example%2Fnotes/versions/0.9.0%2Bbuild.7",
                    "?include_deleted=true"
                ),
            ]
        );
    }

    // Each field freezing reads (MCP spec §3.2), with the fields fl does not
    // read (`$schema`, `title`, `repository`, `publishedAt`, `choices`…)
    // ignored.
    #[test]
    fn the_models_carry_every_field_freezing_needs() {
        let fake = FakeRegistry::start();
        let registry = client(&fake);

        let notes = registry.version(fake::NOTES, "latest").unwrap().server;
        assert_eq!(notes.description, "The notes server, for fl's tests.");
        assert!(notes.remotes.is_empty());
        let [npm] = notes.packages.as_slice() else {
            panic!("{:?}", notes.packages)
        };
        assert_eq!(npm.registry_type, RegistryType::Npm);
        assert_eq!(npm.identifier, "@example/notes-mcp");
        assert_eq!(npm.version.as_deref(), Some("1.2.0"));
        assert_eq!(npm.transport.kind, TransportKind::Stdio);
        assert_eq!(npm.transport.url, None);
        assert!(npm.runtime_arguments.is_empty());
        let [dir] = npm.package_arguments.as_slice() else {
            panic!("{:?}", npm.package_arguments)
        };
        assert_eq!(dir.kind, ArgumentKind::Positional);
        assert_eq!(dir.value_hint.as_deref(), Some("notes_dir"));
        assert_eq!(dir.default.as_deref(), Some("./notes"));
        assert!(dir.is_required && !dir.is_secret);
        let [token, log] = npm.environment_variables.as_slice() else {
            panic!("{:?}", npm.environment_variables)
        };
        assert_eq!(token.name, "NOTES_TOKEN");
        assert!(token.is_secret && token.is_required && token.value.is_none());
        assert_eq!(log.name, "NOTES_LOG");
        assert_eq!(log.default.as_deref(), Some("info"));
        assert!(!log.is_secret && !log.is_required);

        let weather = registry.version(fake::WEATHER, "latest").unwrap().server;
        assert_eq!(weather.packages[0].registry_type, RegistryType::Pypi);
        let units = &weather.packages[0].package_arguments[0];
        assert_eq!(units.kind, ArgumentKind::Named);
        assert_eq!(units.name.as_deref(), Some("--units"));
        assert_eq!(units.default.as_deref(), Some("metric"));

        let tracker = registry.version(fake::TRACKER, "latest").unwrap().server;
        let oci = &tracker.packages[0];
        assert_eq!(oci.registry_type, RegistryType::Oci);
        assert_eq!(oci.identifier, fake::TRACKER_IMAGE);
        assert_eq!(oci.version, None);
        let [port, token] = oci.runtime_arguments.as_slice() else {
            panic!("{:?}", oci.runtime_arguments)
        };
        assert_eq!(port.value.as_deref(), Some("TRACKER_PORT=8085"));
        assert!(port.variables.is_empty());
        assert_eq!(token.name.as_deref(), Some("-e"));
        assert_eq!(token.value.as_deref(), Some("TRACKER_TOKEN={token}"));
        assert_eq!(token.format, None);
        let var = &token.variables["token"];
        assert!(var.is_secret && var.is_required && var.value.is_none());
        assert_eq!(var.format.as_deref(), Some("string"));

        let docs = registry.version(fake::DOCS, "latest").unwrap().server;
        assert!(docs.packages.is_empty());
        let [remote] = docs.remotes.as_slice() else {
            panic!("{:?}", docs.remotes)
        };
        assert_eq!(remote.kind, TransportKind::StreamableHttp);
        assert_eq!(remote.url, fake::DOCS_URL);
        assert!(remote.variables.is_empty());
        let [auth] = remote.headers.as_slice() else {
            panic!("{:?}", remote.headers)
        };
        assert_eq!(auth.name, "Authorization");
        assert!(auth.is_secret && auth.value.is_none() && auth.variables.is_empty());

        let multi = registry.version(fake::MULTI, "latest").unwrap().server;
        let types: Vec<&str> = multi
            .packages
            .iter()
            .map(|p| p.registry_type.as_str())
            .collect();
        assert_eq!(types, ["npm", "oci"]);
        assert_eq!(multi.remotes[0].kind, TransportKind::Sse);
        assert_eq!(multi.remotes[0].kind.as_str(), "sse");
    }

    // The registry's type fields are open (any string): an unknown one is
    // kept, for freezing to refuse by name.
    #[test]
    fn an_unknown_package_or_transport_type_is_kept_by_name() {
        let text = r#"{"registryType": "mcpb", "identifier": "x", "transport":
                       {"type": "streamable-http", "url": "http://localhost:{port}/mcp"}}"#;
        let package: Package = serde_json::from_str(text).unwrap();
        assert_eq!(package.registry_type, RegistryType::Other("mcpb".into()));
        assert_eq!(package.registry_type.as_str(), "mcpb");
        assert_eq!(package.transport.kind, TransportKind::StreamableHttp);
        assert_eq!(package.transport.kind.as_str(), "streamable-http");
        assert_eq!(
            package.transport.url.as_deref(),
            Some("http://localhost:{port}/mcp")
        );
        let remote: Remote =
            serde_json::from_str(r#"{"type": "websocket", "url": "wss://x.example.com"}"#).unwrap();
        assert_eq!(remote.kind, TransportKind::Other("websocket".into()));
        assert_eq!(remote.kind.as_str(), "websocket");
        let arg: Argument =
            serde_json::from_str(r#"{"type": "flag", "name": "--verbose"}"#).unwrap();
        assert_eq!(arg.kind, ArgumentKind::Other("flag".into()));
        assert_eq!(arg.kind.as_str(), "flag");
        let named: Argument = serde_json::from_str(r#"{"type": "named", "name": "-v"}"#).unwrap();
        assert_eq!(named.kind.as_str(), "named");
    }

    // The registry serves entries its own schema does not allow: a search
    // reads only what it shows, so one such server never fails a page.
    #[test]
    fn a_server_that_breaks_the_schema_never_fails_a_search() {
        let fake = FakeRegistry::start();
        let weather = fake
            .state()
            .entries
            .iter()
            .find(|e| e["server"]["name"] == fake::WEATHER)
            .cloned();
        let mut broken = weather.unwrap();
        broken["server"]["name"] = "io.example/broken".into();
        broken["server"]["packages"][0]["transport"] = 7.into();
        broken["_meta"]["io.modelcontextprotocol.registry/official"]["status"] = "paused".into();
        fake.state().entries.push(broken);
        let registry = client(&fake);
        let found = registry.search("io.example").unwrap();
        let mut listed = vec!["io.example/broken"];
        listed.extend(fake::LISTED);
        listed.sort();
        assert_eq!(names(&found.servers), listed);
        let status = |name: &str| {
            let s = found.servers.iter().find(|s| s.name == name).unwrap();
            s.status.clone()
        };
        assert_eq!(status("io.example/broken"), "paused");
        assert_eq!(status(fake::LEGACY), "deprecated");
        // An argument type fl does not know is kept, for freezing to refuse.
        let verbose = registry.version(fake::VERBOSE, "latest").unwrap();
        let arg = &verbose.server.packages[0].package_arguments[0];
        assert_eq!(arg.kind, ArgumentKind::Other("flag".into()));
        // Anything else the full entry breaks is the registry's shape, as
        // before.
        let err = registry.version("io.example/broken", "latest").unwrap_err();
        holds_only(&err.to_string(), "is not the registry API's shape");
    }

    // The registry API: a deleted server is hidden unless asked for,
    // so fl asks, to refuse it by its status rather than as not found (MCP
    // spec §3.2 step 2).
    #[test]
    fn a_deleted_server_is_found_only_by_asking_for_it_and_says_so() {
        let fake = FakeRegistry::start();
        let registry = client(&fake);
        let gone = registry.version(fake::GONE, "latest").unwrap();
        assert_eq!(gone.meta.status, Status::Deleted);
        assert_eq!(
            gone.meta.status_message.as_deref(),
            Some(fake::GONE_MESSAGE)
        );
        let all = registry.versions(fake::GONE).unwrap();
        assert_eq!(all[0].meta.status, Status::Deleted);
        let legacy = registry.version(fake::LEGACY, "latest").unwrap();
        assert_eq!(legacy.meta.status, Status::Deprecated);
        assert_eq!(
            legacy.meta.status_message.as_deref(),
            Some(fake::LEGACY_MESSAGE)
        );
        assert!(registry.search("gone").unwrap().servers.is_empty());

        // The fake hides it as the registry does, when not asked.
        let hidden = format!(
            "{}/v0.1/servers/io.example%2Fgone/versions/latest",
            fake.url()
        );
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build(),
        );
        assert_eq!(agent.get(&hidden).call().unwrap().status().as_u16(), 404);
    }

    // MCP spec §3.1 and §5: each misbehaviour is the registry's failure,
    // told apart by its own message; none is a parse error, and a redirect
    // is never followed.
    #[test]
    fn each_misbehaving_registry_is_refused_with_its_own_message() {
        let fake = FakeRegistry::start();
        let elsewhere = FakeRegistry::start();
        let registry = client(&fake);
        let fail = |f: fn(&mut fake::State)| {
            f(&mut fake.state());
            match registry.search("notes") {
                // Not the answer itself: the oversized one is 4.5 MiB.
                Ok(found) => panic!("read {} servers from it", found.servers.len()),
                Err(e) => e.to_string(),
            }
        };

        // Off the origin: another host, another port that begins alike, a
        // scheme-relative address. On it: a path, or the origin in full.
        let own = fake.url();
        let off = "a redirect off its own origin".to_string();
        for (location, told) in [
            (format!("{}/v0.1/servers", elsewhere.url()), off.clone()),
            (format!("{own}9/v0.1/servers"), off.clone()),
            ("//registry.example.com/v0.1/servers".to_string(), off),
            (
                "/v0.1/moved".to_string(),
                "a redirect to /v0.1/moved".to_string(),
            ),
            (
                format!("{own}/v0.1/moved"),
                format!("a redirect to {own}/v0.1/moved"),
            ),
        ] {
            fake.state().redirect_next = Some(location.clone());
            let msg = registry.search("notes").unwrap_err().to_string();
            holds_only(&msg, "fl follows no redirect");
            assert!(
                msg.contains(&format!("answered 302 with {told}")),
                "{location}: {msg}"
            );
            if told.contains("off its own origin") {
                assert!(!msg.contains(&location), "{msg}");
            }
        }
        assert!(
            elsewhere.state().requests.is_empty(),
            "a redirect was followed"
        );
        assert_eq!(fake.state().requests.len(), 5, "a redirect was followed");

        let msg = fail(|s| s.html_502_next = true);
        holds_only(&msg, "answered with an HTML page");
        assert!(msg.contains("(status 502)"), "{msg}");

        let msg = fail(|s| s.oversized_next = true);
        holds_only(&msg, "larger than 4 MiB");

        let msg = fail(|s| s.problem_500_next = true);
        holds_only(&msg, "answered 500");
        assert!(
            msg.contains("answered 500: Failed to get registry list"),
            "{msg}"
        );

        let err = registry
            .version("io.example/nowhere", "latest")
            .unwrap_err();
        assert!(matches!(err, McpError::NotFound { .. }), "{err:?}");
        let msg = err.to_string();
        holds_only(&msg, "has no server");
        assert!(msg.contains("has no server `io.example/nowhere`"), "{msg}");
        let msg = registry
            .versions("io.example/nowhere")
            .unwrap_err()
            .to_string();
        holds_only(&msg, "has no server");
        let msg = registry
            .version(fake::NOTES, "9.9.9")
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("has no version `9.9.9` of server `io.example/notes`"),
            "{msg}"
        );
        for p in PHRASES {
            assert!(!msg.contains(p), "{p:?} in: {msg}");
        }

        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let closed = Registry::new(&format!("http://127.0.0.1:{port}")).unwrap();
        let msg = closed.search("notes").unwrap_err().to_string();
        holds_only(&msg, "cannot reach the registry");

        // And a registry that behaves is read as one, after all of that.
        assert_eq!(
            names(&registry.search("notes").unwrap().servers),
            [fake::NOTES]
        );
    }

    // MCP spec §3.1: reads are unauthenticated.
    #[test]
    fn no_request_carries_a_credential_and_each_names_fl() {
        let fake = FakeRegistry::start();
        let registry = client(&fake);
        registry.search("io.example").unwrap();
        registry.versions(fake::NOTES).unwrap();
        registry.version(fake::DOCS, "latest").unwrap();
        let headers = fake.state().headers.clone();
        assert_eq!(headers.len(), 3);
        for h in headers {
            assert!(!h.iter().any(|(k, _)| k == "authorization"), "{h:?}");
            assert!(!h.iter().any(|(k, _)| k == "cookie"), "{h:?}");
            let agent = h
                .iter()
                .find(|(k, _)| k == "user-agent")
                .map(|(_, v)| v.as_str());
            assert_eq!(agent, Some(concat!("fl/", env!("CARGO_PKG_VERSION"))));
        }
    }

    // The registry API: names and versions are encoded in full, so
    // `/` and `+` reach the registry as `%2F` and `%2B`.
    #[test]
    fn a_path_segment_keeps_only_unreserved_characters() {
        assert_eq!(encode("io.example/notes"), "io.example%2Fnotes");
        assert_eq!(encode("1.0.0+build.7"), "1.0.0%2Bbuild.7");
        assert_eq!(encode("AZaz09-._~"), "AZaz09-._~");
        assert_eq!(encode("a b%c?d&e=f#g:h"), "a%20b%25c%3Fd%26e%3Df%23g%3Ah");
        assert_eq!(encode("é"), "%C3%A9");
        assert_eq!(encode(""), "");
    }

    // Registry text reaches a terminal: no escape sequence, and not a page of
    // it.
    #[test]
    fn registry_text_is_shown_without_control_characters_and_cut_short() {
        assert_eq!(printable("a\u{1b}[31mb\r\nc"), "a[31mbc");
        assert_eq!(printable(&"x".repeat(400)).len(), 300);
    }

    // MCP spec §3.1: https, or http to this machine, with no user name.
    #[test]
    fn a_registry_address_fl_will_not_use_is_refused_at_construction() {
        for (url, clause) in [
            ("http://registry.example.com", "is not this machine"),
            ("http://10.0.0.1:8080", "is not this machine"),
            (
                "https://user@registry.example.com",
                "it carries a user name or password",
            ),
            (
                "ftp://registry.example.com",
                "it is neither https:// nor http://",
            ),
        ] {
            let err = Registry::new(url).err().expect(url);
            assert!(matches!(err, McpError::RegistryAddress { .. }), "{err:?}");
            let msg = err.to_string();
            assert!(
                msg.contains("is not a registry address fl will use"),
                "{msg}"
            );
            assert!(msg.contains(clause), "{url}: {msg}");
            assert!(msg.contains(url), "{msg}");
        }
        for url in [
            "http://localhost:8080",
            "http://[::1]:9",
            "https://registry.example.com",
        ] {
            assert_eq!(Registry::new(url).unwrap().url(), url);
        }
        let trimmed = Registry::new("https://registry.example.com/mirror/").unwrap();
        assert_eq!(trimmed.url(), "https://registry.example.com/mirror");
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-mcp --lib registry::tests::`
Expected: FAIL to compile (37 errors) — `error[E0433]`/`error[E0425]` cannot find type `Registry`, `Summary`, `Package`, `Argument`, `Remote`, `Status`, `RegistryType`, `TransportKind`, `ArgumentKind`, and cannot find function `encode` and `printable`; `error[E0599]` no variant named `RegistryAddress` / `NotFound` found for enum `McpError`. The fake compiles.

- [ ] **Step 3: Implement**

In `crates/mcp/src/lib.rs`, inside `pub enum McpError`, after `AlreadyPresent { path: PathBuf, name: String },`, add:

```rust
    /// A registry address the client will not read (MCP spec §3.1).
    #[error(
        "{url} is not a registry address fl will use: {clause}. Use an https:// address, or \
         http:// to this machine"
    )]
    RegistryAddress { url: String, clause: String },
    /// The registry could not be reached. `sync` and `check` never need it
    /// (MCP spec §5).
    #[error(
        "cannot reach the registry at {registry}: {cause}. Check the address and the network, \
         then retry; `fl mcp sync` and `fl mcp check` need no registry"
    )]
    Unreachable { registry: String, cause: String },
    /// The registry answered, but not with the registry API: a redirect,
    /// an HTML page, a body over 4 MiB, an error status, or a body of the
    /// wrong shape. `request` is `GET <path>`; `problem` says which, and
    /// what to do.
    #[error("the registry at {registry} failed {request}: {problem}")]
    Registry {
        registry: String,
        request: String,
        problem: String,
    },
    /// The registry has no such server, or no such version of it.
    #[error(
        "the registry at {registry} has no {}. `fl mcp search <text>` lists the servers it has",
        missing(name, version)
    )]
    NotFound {
        registry: String,
        name: String,
        version: Option<String>,
    },
```

and at the end of the file, after `fn on_server`, add:

```rust
fn missing(name: &str, version: &Option<String>) -> String {
    match version {
        Some(v) => format!("version `{v}` of server `{name}`"),
        None => format!("server `{name}`"),
    }
}
```

At the top of `crates/mcp/src/registry.rs`, before `#[cfg(test)]`, add:

```rust
//! A read-only client for an MCP registry, API `v0.1` (MCP spec §3.1): the
//! official registry, or any registry that serves the same OpenAPI document.
//!
//! It sends no credential and follows no redirect. It judges the status and
//! the content type before it reads the body as data, and reads no more than
//! 4 MiB of it, so a proxy's HTML page, a moved registry or a runaway answer
//! is reported as the registry's failure, never as a parse error.

use crate::McpError;
use crate::catalog::check_registry_url;
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

/// The registry API version fl reads (MCP spec §3.1).
pub const API_VERSION: &str = "v0.1";
/// The most fl reads of one answer (MCP spec §3.1).
pub const BODY_LIMIT: u64 = 4 << 20;
/// The most pages `search` reads (MCP spec §3.1).
pub const SEARCH_PAGES: usize = 20;
/// Servers per page: the registry's own maximum.
const PAGE_SIZE: usize = 100;
const USER_AGENT: &str = concat!("fl/", env!("CARGO_PKG_VERSION"));

pub struct Registry {
    agent: ureq::Agent,
    /// The address as given, without a trailing `/`.
    base: String,
    /// `scheme://host[:port]` of `base`.
    origin: String,
}

/// What `search` read: the servers whose name holds the text, at their
/// latest version, and whether it stopped before the registry's last page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    pub servers: Vec<Summary>,
    pub stopped_early: bool,
}

/// One server as a search shows it. Only these fields are read, so a server
/// whose launch spec breaks the registry's own schema, which the registry
/// serves, never fails a search.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "SummaryJson")]
pub struct Summary {
    pub name: String,
    pub description: String,
    pub version: String,
    /// As the registry spells it: `active`, `deprecated`, or one fl does not
    /// know, shown as it is.
    pub status: String,
}

impl Registry {
    /// A client for the registry at `url`: https, or http to this machine,
    /// with no user name or password (MCP spec §3.1).
    pub fn new(url: &str) -> Result<Registry, McpError> {
        check_registry_url(url).map_err(|clause| McpError::RegistryAddress {
            url: url.to_string(),
            clause,
        })?;
        let base = url.trim_end_matches('/').to_string();
        let scheme_end = base.find("://").map_or(0, |i| i + 3);
        let origin_end = base[scheme_end..]
            .find(['/', '?', '#'])
            .map_or(base.len(), |i| scheme_end + i);
        Ok(Registry {
            agent: agent(),
            origin: base[..origin_end].to_string(),
            base,
        })
    }

    /// The registry's address, without a trailing `/`.
    pub fn url(&self) -> &str {
        &self.base
    }

    /// The servers whose name holds `text`, in any case, at their latest
    /// version: the registry searches names only. Reads up to
    /// [`SEARCH_PAGES`] pages; a failure on any page is an error, never a
    /// short list.
    pub fn search(&self, text: &str) -> Result<Search, McpError> {
        let mut servers = Vec::new();
        let mut cursor: Option<String> = None;
        let wanted = text.to_lowercase();
        for _ in 0..SEARCH_PAGES {
            let mut path = format!(
                "/{API_VERSION}/servers?search={}&version=latest&limit={PAGE_SIZE}",
                encode(text)
            );
            if let Some(c) = &cursor {
                path.push_str("&cursor=");
                path.push_str(&encode(c));
            }
            let page: ServerList<Summary> = self.get(&path, None)?;
            // By name only (MCP spec §3.1), whatever the registry sends: one
            // that ignores `search` would otherwise list every server it has.
            let named = |s: &Summary| s.name.to_lowercase().contains(&wanted);
            servers.extend(page.servers.into_iter().filter(named));
            match page.metadata.next_cursor.filter(|c| !c.is_empty()) {
                Some(next) => cursor = Some(next),
                None => {
                    return Ok(Search {
                        servers,
                        stopped_early: false,
                    });
                }
            }
        }
        Ok(Search {
            servers,
            stopped_early: true,
        })
    }

    /// Every version of `name`, deleted ones included, in the registry's
    /// order (which it does not document).
    pub fn versions(&self, name: &str) -> Result<Vec<ServerResponse>, McpError> {
        let path = format!(
            "/{API_VERSION}/servers/{}/versions?include_deleted=true",
            encode(name)
        );
        let list: ServerList<ServerResponse> = self.get(&path, Some((name, None)))?;
        Ok(list.servers)
    }

    /// One version of `name`, or its latest with `version` `latest`, even
    /// when deleted: its status says so (MCP spec §3.2 step 2).
    pub fn version(&self, name: &str, version: &str) -> Result<ServerResponse, McpError> {
        let path = format!(
            "/{API_VERSION}/servers/{}/versions/{}?include_deleted=true",
            encode(name),
            encode(version)
        );
        let asked = (version != "latest").then_some(version);
        self.get(&path, Some((name, asked)))
    }

    /// One GET, judged in this order: the transport, a redirect, an HTML
    /// page, a 404 on a server, the body's size, any other status, and only
    /// then the body as data. `server` names what a 404 means is missing.
    fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        server: Option<(&str, Option<&str>)>,
    ) -> Result<T, McpError> {
        let unreachable = |e: ureq::Error| McpError::Unreachable {
            registry: self.base.clone(),
            cause: e.to_string(),
        };
        let failed = |problem: String| McpError::Registry {
            registry: self.base.clone(),
            request: format!("GET {path}"),
            problem,
        };
        let mut resp = self
            .agent
            .get(&format!("{}{path}", self.base))
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(unreachable)?;
        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        let mime = header("content-type");
        let mime = mime
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if (300..400).contains(&status) {
            return Err(failed(self.redirect(status, &header("location"))));
        }
        if mime == "text/html" {
            return Err(failed(format!(
                "it answered with an HTML page (status {status}), not the registry API. The \
                 address may not be a registry, or a proxy in front of it failed; check the \
                 address, or retry later"
            )));
        }
        if status == 404
            && let Some((name, version)) = server
        {
            return Err(McpError::NotFound {
                registry: self.base.clone(),
                name: name.to_string(),
                version: version.map(str::to_string),
            });
        }
        let text = match resp
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .lossy_utf8(true)
            .read_to_string()
        {
            Ok(text) => text,
            Err(ureq::Error::BodyExceedsLimit(_)) => {
                return Err(failed(
                    "its answer is larger than 4 MiB, which fl will not read. The address may \
                     not be a registry; check it"
                        .to_string(),
                ));
            }
            Err(e) => return Err(unreachable(e)),
        };
        if !(200..300).contains(&status) {
            let detail = (mime == "application/problem+json")
                .then(|| serde_json::from_str::<ProblemJson>(&text).ok())
                .flatten()
                .and_then(|p| p.detail)
                .map(|d| format!(": {}", printable(&d)))
                .unwrap_or_default();
            return Err(failed(format!(
                "it answered {status}{detail}. Retry later; if it goes on, the registry's \
                 operator can say why"
            )));
        }
        serde_json::from_str(&text).map_err(|e| {
            failed(format!(
                "its answer is not the registry API's shape ({e}). Check that the address is a \
                 registry serving API {API_VERSION}"
            ))
        })
    }

    /// A redirect names where it points only when that is the registry's
    /// own origin.
    fn redirect(&self, status: u16, location: &str) -> String {
        let same_origin = (location.starts_with('/') && !location.starts_with("//"))
            || location
                .get(..self.origin.len())
                .is_some_and(|o| o.eq_ignore_ascii_case(&self.origin))
                && matches!(
                    location[self.origin.len()..].chars().next(),
                    None | Some('/' | '?' | '#')
                );
        let to = if same_origin {
            format!("to {}", printable(location))
        } else {
            "off its own origin".to_string()
        };
        format!(
            "it answered {status} with a redirect {to}, and fl follows no redirect. If the \
             registry has moved, set its new address with `fl mcp registry <url>`"
        )
    }
}

/// No redirect is followed, and nothing is sent but the request.
fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(30)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// One path segment or query value: the unreserved characters kept, every
/// other byte as `%XX`, so `/` is `%2F` and `+` is `%2B`.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Registry text for a terminal: no control characters, at most 300 of the
/// rest.
fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(300).collect()
}

#[derive(Deserialize)]
struct ProblemJson {
    detail: Option<String>,
}

#[derive(Deserialize)]
struct ServerList<T> {
    servers: Vec<T>,
    metadata: ListMetadata,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListMetadata {
    next_cursor: Option<String>,
}

// The models read the fields fl uses and ignore the rest: the registry adds
// fields within `v0.1`.

/// One server version, as the registry serves it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServerResponse {
    pub server: ServerJson,
    /// The registry's own record of it, `_meta."io.modelcontextprotocol.registry/official"`.
    #[serde(rename = "_meta", deserialize_with = "official")]
    pub meta: Official,
}

#[derive(Deserialize)]
struct MetaJson {
    #[serde(rename = "io.modelcontextprotocol.registry/official")]
    official: Official,
}

fn official<'de, D: Deserializer<'de>>(d: D) -> Result<Official, D::Error> {
    MetaJson::deserialize(d).map(|m| m.official)
}

#[derive(Deserialize)]
struct SummaryJson {
    server: SummaryServer,
    #[serde(rename = "_meta")]
    meta: SummaryMeta,
}

#[derive(Deserialize)]
struct SummaryServer {
    name: String,
    #[serde(default)]
    description: String,
    version: String,
}

#[derive(Deserialize)]
struct SummaryMeta {
    #[serde(rename = "io.modelcontextprotocol.registry/official")]
    official: SummaryOfficial,
}

#[derive(Deserialize)]
struct SummaryOfficial {
    status: String,
}

impl From<SummaryJson> for Summary {
    fn from(j: SummaryJson) -> Self {
        Summary {
            name: j.server.name,
            description: j.server.description,
            version: j.server.version,
            status: j.meta.official.status,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Official {
    pub status: Status,
    pub status_message: Option<String>,
    pub is_latest: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Deprecated,
    Deleted,
}

/// A server's `server.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServerJson {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub packages: Vec<Package>,
    #[serde(default)]
    pub remotes: Vec<Remote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    pub registry_type: RegistryType,
    pub identifier: String,
    /// npm, PyPI and NuGet only: an OCI identifier carries its own tag.
    pub version: Option<String>,
    pub transport: PackageTransport,
    #[serde(default)]
    pub runtime_arguments: Vec<Argument>,
    #[serde(default)]
    pub package_arguments: Vec<Argument>,
    #[serde(default)]
    pub environment_variables: Vec<KeyValueInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PackageTransport {
    #[serde(rename = "type")]
    pub kind: TransportKind,
    /// Where a package that serves HTTP listens, `{var}` templates and all.
    pub url: Option<String>,
}

/// `registryType`: an open string in the registry's schema.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum RegistryType {
    Npm,
    Pypi,
    Oci,
    Other(String),
}

impl From<String> for RegistryType {
    fn from(s: String) -> Self {
        match s.as_str() {
            "npm" => RegistryType::Npm,
            "pypi" => RegistryType::Pypi,
            "oci" => RegistryType::Oci,
            _ => RegistryType::Other(s),
        }
    }
}

impl RegistryType {
    pub fn as_str(&self) -> &str {
        match self {
            RegistryType::Npm => "npm",
            RegistryType::Pypi => "pypi",
            RegistryType::Oci => "oci",
            RegistryType::Other(s) => s,
        }
    }
}

/// A package's or a remote's `type`, kept as given when fl does not know it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum TransportKind {
    Stdio,
    StreamableHttp,
    Sse,
    Other(String),
}

impl From<String> for TransportKind {
    fn from(s: String) -> Self {
        match s.as_str() {
            "stdio" => TransportKind::Stdio,
            "streamable-http" => TransportKind::StreamableHttp,
            "sse" => TransportKind::Sse,
            _ => TransportKind::Other(s),
        }
    }
}

impl TransportKind {
    pub fn as_str(&self) -> &str {
        match self {
            TransportKind::Stdio => "stdio",
            TransportKind::StreamableHttp => "streamable-http",
            TransportKind::Sse => "sse",
            TransportKind::Other(s) => s,
        }
    }
}

/// A runtime or package argument.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Argument {
    #[serde(rename = "type")]
    pub kind: ArgumentKind,
    /// A named argument's flag, leading dashes included.
    pub name: Option<String>,
    /// May hold `{var}` templates, resolved from `variables`.
    pub value: Option<String>,
    pub value_hint: Option<String>,
    pub default: Option<String>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub is_secret: bool,
    /// `string`, `number`, `boolean` or `filepath`.
    pub format: Option<String>,
    #[serde(default)]
    pub variables: BTreeMap<String, Input>,
}

/// An argument's `type`, kept as given when fl does not know it: the
/// registry serves types its own schema does not define.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum ArgumentKind {
    Positional,
    Named,
    Other(String),
}

impl From<String> for ArgumentKind {
    fn from(s: String) -> Self {
        match s.as_str() {
            "positional" => ArgumentKind::Positional,
            "named" => ArgumentKind::Named,
            _ => ArgumentKind::Other(s),
        }
    }
}

impl ArgumentKind {
    pub fn as_str(&self) -> &str {
        match self {
            ArgumentKind::Positional => "positional",
            ArgumentKind::Named => "named",
            ArgumentKind::Other(s) => s,
        }
    }
}

/// A `{var}` an argument, a header or a remote URL names.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    pub value: Option<String>,
    pub default: Option<String>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub is_secret: bool,
    pub format: Option<String>,
}

/// An environment variable or a header.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyValueInput {
    pub name: String,
    /// May hold `{var}` templates, resolved from `variables`.
    pub value: Option<String>,
    pub default: Option<String>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub is_secret: bool,
    #[serde(default)]
    pub variables: BTreeMap<String, Input>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Remote {
    #[serde(rename = "type")]
    pub kind: TransportKind,
    /// May hold `{var}` templates, resolved from `variables`.
    pub url: String,
    #[serde(default)]
    pub headers: Vec<KeyValueInput>,
    #[serde(default)]
    pub variables: BTreeMap<String, Input>,
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-mcp --lib registry::tests::`
Expected: PASS — 15 passed: `search_finds_names_in_any_case_at_their_latest_version`, `search_keeps_only_names_that_hold_the_text_whatever_the_registry_sends`, `search_reads_every_page_by_its_cursor`, `search_stops_after_twenty_pages_and_says_so`, `versions_lists_every_version_of_a_server`, `version_reads_one_version_or_the_latest`, `the_models_carry_every_field_freezing_needs`, `an_unknown_package_or_transport_type_is_kept_by_name`, `a_server_that_breaks_the_schema_never_fails_a_search`, `a_deleted_server_is_found_only_by_asking_for_it_and_says_so`, `each_misbehaving_registry_is_refused_with_its_own_message`, `no_request_carries_a_credential_and_each_names_fl`, `a_path_segment_keeps_only_unreserved_characters`, `registry_text_is_shown_without_control_characters_and_cut_short`, `a_registry_address_fl_will_not_use_is_refused_at_construction`. `cargo test -p fl-mcp --lib` passes 35 (Task 1's 20 with them).

- [ ] **Step 5: Mutation checks**

Each filter is `cargo test -p fl-mcp --lib registry::tests::<name>`; "misbehaving" is `each_misbehaving_registry_is_refused_with_its_own_message`, "twenty" is `search_stops_after_twenty_pages_and_says_so`, "pages" is `search_reads_every_page_by_its_cursor`, "case" is `search_finds_names_in_any_case_at_their_latest_version`, "versions" is `versions_lists_every_version_of_a_server`, "version" is `version_reads_one_version_or_the_latest`, "deleted" is `a_deleted_server_is_found_only_by_asking_for_it_and_says_so`, "credential" is `no_request_carries_a_credential_and_each_names_fl`, "encoder" is `a_path_segment_keeps_only_unreserved_characters`, "address" is `a_registry_address_fl_will_not_use_is_refused_at_construction`, "printable" is `registry_text_is_shown_without_control_characters_and_cut_short`, "open" is `an_unknown_package_or_transport_type_is_kept_by_name`, "schema" is `a_server_that_breaks_the_schema_never_fails_a_search`, "flood" is `search_keeps_only_names_that_hold_the_text_whatever_the_registry_sends`. Each mutation is one edit of `crates/mcp/src/registry.rs` (of `crates/mcp/src/fake.rs` for 33–34); save a copy first, restore it after each, and `cmp` against the copy. None of these goes red by failing to compile (36 is one mutation of two lines, so that it compiles).

1. No redirect followed: `.max_redirects(0)` → `.max_redirects(10)` → misbehaving red (the off-origin 302 is followed to the second fake, which answers).
2. A 3xx is classified: `if (300..400).contains(&status) {` → `if false {` → misbehaving red (the redirect's own HTML body is reported as an HTML page) — so the redirect check also must come before the HTML check.
3. An off-origin location is not named: `let to = if same_origin {` → `let to = if true {` → misbehaving red (the second fake's address is in the message).
4. A same-origin location is named: `let to = if same_origin {` → `let to = if false {` → misbehaving red.
5. A scheme-relative location is off-origin: `(location.starts_with('/') && !location.starts_with("//"))` → `location.starts_with('/')` → misbehaving red (`//registry.example.com/…`).
6. The origin is compared: `.is_some_and(|o| o.eq_ignore_ascii_case(&self.origin))` → `.is_some_and(|_| true)` → misbehaving red.
7. The origin ends where the location's does: delete the `&& matches!(location[self.origin.len()..].chars().next(), None | Some('/' | '?' | '#'))` conjunct → misbehaving red (the same host on a port that begins alike, `<origin>9`, reads as the origin).
8. The HTML check: `if mime == "text/html" {` → `if false {` → misbehaving red (the 502 is reported as `answered 502`).
9. A content type's parameters are dropped before it is compared: delete `.split(';').next().unwrap_or_default()` from the `mime` chain → misbehaving red (`text/html; charset=utf-8` is not `text/html`).
10. The body cap: `.limit(BODY_LIMIT)` → `.limit(10 << 20)` (ureq's own default) → misbehaving red (the 4.5 MiB list is read as an answer).
11. The cap's value: `pub const BODY_LIMIT: u64 = 4 << 20;` → `5 << 20` → misbehaving red.
12. The status is classified before the body is parsed: `if !(200..300).contains(&status) {` → `if false {` → misbehaving red (the problem+json 500 becomes `is not the registry API's shape`).
13. A problem's detail is shown: `let detail = (mime == "application/problem+json")` → `let detail = false` → misbehaving red.
14. A 404 on a server is "not found": `if status == 404` → `if false` → misbehaving red (`answered 404`).
15. `versions` names its server on a 404: `self.get(&path, Some((name, None)))?` → `self.get(&path, None)?` → misbehaving red.
16. `version` names the version asked: `let asked = (version != "latest").then_some(version);` → `let asked = None;` → misbehaving red (``has no version `9.9.9` `` is missing).
17. The encoder sends `/` as `%2F`: add `| b'/'` to `matches!(b, b'-' | b'.' | b'_' | b'~')` → encoder red; and versions red (the fake, like the registry, finds nothing at an unencoded `/`).
18. The encoder sends `+` as `%2B`: add `| b'+'` → version red.
19. `version` asks for deleted entries: `"/{API_VERSION}/servers/{}/versions/{}?include_deleted=true"` → without `?include_deleted=true` → deleted red (`io.example/gone` is not found).
20. `versions` asks for deleted entries: the same in `"/{API_VERSION}/servers/{}/versions?include_deleted=true"` → deleted red.
21. The page cap: `pub const SEARCH_PAGES: usize = 20;` → `21` → twenty red (21 servers read).
22. The page cap from below: `20` → `19` → twenty red (the twentieth page is not read).
23. The early stop is said: `stopped_early: true,` → `stopped_early: false,` → twenty red.
24. The cursor is carried: `Some(next) => cursor = Some(next),` → `Some(_) => cursor = None,` → pages red (page one, over and over).
25. One entry per server: `servers?search={}&version=latest&limit={PAGE_SIZE}` → `servers?search={}&limit={PAGE_SIZE}` → case red (four versions of `io.example/notes`).
26. No credential: add `.header("Authorization", "Bearer x")` after `.header("User-Agent", USER_AGENT)` → credential red.
27. fl names itself: delete `.header("User-Agent", USER_AGENT)` → credential red (ureq's own agent string is sent).
28. The address check: delete `check_registry_url(url).map_err(|clause| McpError::RegistryAddress { … })?;` → address red.
29. A trailing `/` is dropped: `let base = url.trim_end_matches('/').to_string();` → `let base = url.to_string();` → address red.
30. Registry text loses its control characters: `text.chars().filter(|c| !c.is_control()).take(300)` → `text.chars().take(300)` → printable red.
31. And is cut at 300 characters: delete `.take(300)` → printable red.
32. An unknown registry type is kept: in `impl From<String> for RegistryType`, `"oci" => RegistryType::Oci, _ => RegistryType::Other(s),` → `_ => RegistryType::Oci,` → open red.
33. The fake hides a deleted entry unless asked: `let shown = |e: &&Value| deleted_too || official(e)["status"] != "deleted";` → `let shown = |_: &&Value| true;` → deleted red (`search("gone")` finds it, and the plain request is not a 404).
34. The fake's search ignores case: `let search = param("search").map(|t| t.to_lowercase());` → `let search = param("search");` → case red.
35. A search reads no launch spec: add `#[serde(default)] packages: Vec<Package>,` to `struct SummaryServer` → schema red (the broken server's package fails the page).
36. A search reads the status as a string: `status: String` → `status: Status` in `struct SummaryOfficial`, and `status: format!("{:?}", j.meta.official.status).to_lowercase(),` in `From<SummaryJson>` → schema red (`paused` fails the page).
37. The status is carried: `status: j.meta.official.status,` → `status: "active".into(),` → schema red (`paused`, `deprecated`).
38. … the description: `description: j.server.description,` → `description: String::new(),` → case red.
39. … the version: `version: j.server.version,` → `version: String::new(),` → case red.
40. An unknown argument type is kept: in `impl From<String> for ArgumentKind`, `_ => ArgumentKind::Other(s),` → `_ => ArgumentKind::Named,` → open red, schema red.
41. … and named as given: `ArgumentKind::Other(s) => s,` → `ArgumentKind::Other(_) => "other",` → open red.
42. `named` is spelled as the registry spells it: `ArgumentKind::Named => "named",` → `"positional"` → open red.
43. A search keeps only names that hold the text: `servers.extend(page.servers.into_iter().filter(named));` → `servers.extend(page.servers);` (and `let _ = named;`) → flood red (the fake, ignoring `search`, sends every server).
44. … in any case of the name: `s.name.to_lowercase().contains(&wanted)` → `s.name.contains(&wanted)` → flood red (`io.example/MixedCase`).
45. … in any case of the text: `let wanted = text.to_lowercase();` → `let wanted = text.to_string();` → flood red (`NoTeS`).

Not observable:
- The fake's `ignores_search` knob is test equipment, not a guard: the client's own filter hides what it sends, so mutation 43 is what shows the knob sends every server.
- `.filter(|c| !c.is_empty())` on `nextCursor`: the registry's generic API document says an empty cursor also ends the list, but the official registry and the fake omit it on the last page; without the filter an empty cursor would be sent back once more, and the fake has no knob that sends one.
- The `&& let Some((name, version)) = server` conjunct binds the name a 404 reports; `search` passes `None`, and a 404 on `GET /v0.1/servers` is a registry that is not one, reported as `answered 404` — no fixture serves that.
- The 30-second global timeout is not a guard any test can provoke without waiting it out.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1233 passed, 19 ignored (1218 and 19 before this task).

- [ ] **Step 7: Commit**

```bash
git add Cargo.lock crates/mcp/Cargo.toml crates/mcp/src/lib.rs crates/mcp/src/fake.rs crates/mcp/src/registry.rs
git commit -m "feat(mcp): the registry client, its guards, and a fake registry

fl-mcp reads an MCP registry, API v0.1, with its own client: search by
the registry's name search at each server's latest version, paged by
cursor up to 20 pages and saying when it stopped early, and keeping
only the servers whose name holds the text, whatever the registry
sends; every version
of a server; one version, or the latest. Deleted entries are asked for,
so freezing can refuse them by their status. Names and versions are
percent-encoded in full, so / and + reach the registry as %2F and %2B.
The client sends no credential and follows no redirect, naming where
one points only on the registry's own origin. It judges the status and
the content type before the body: a redirect, an HTML page, a body over
4 MiB, a 404 on a server, and any other status (with a problem detail)
each get their own message, never a parse error. A registry address
that is neither https nor http to this machine is refused. The models
read the fields freezing needs and ignore the rest; package, transport
and argument types stay open strings, since the registry serves entries
its own schema does not allow, and a search reads only the name,
description, version and status it shows, so one such server never
fails a page. FakeRegistry serves fixtures on
127.0.0.1 behind the fake feature, with a knob for each misbehaviour.
Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 4: Freezing a registry entry into the catalog, or refusing it

`fl mcp add --from` freezes one version of a registry server into the catalog, so `sync` needs no network and gives the same output on every machine (MCP spec §2.1, §3.2); `upgrade` shows what a new version would change and refuses one that is not newer (§3.3). This task writes both as pure functions over Task 3's models: `freeze` turns a `ServerResponse` into a Task 1 `Server`, or refuses it naming what to do instead (Review Focus 4). It records the version the registry returned, never `latest` (§3.2 step 1); refuses a `deleted` status naming its `statusMessage` and warns on `deprecated` (step 2, plan ruling 13); takes the route `--package npm|pypi|oci` or `--remote` names, or the only one on offer, and otherwise refuses listing them (step 3). npm runs as `npx -y <identifier>@<version>`, PyPI as `uvx <identifier>==<version>`, OCI as `docker run -i --rm` + the runtime arguments + one `-e NAME` per variable + the identifier as given; an image with no exact tag, a package with a positional runtime argument, a package that is not stdio, and a package type other than npm, PyPI and OCI are refused (steps 4 and 6, plan rulings 14 and 17), and so is an argument whose `type` is neither `positional` nor `named`, naming the type (plan ruling 31: the registry serves a `flag` type its schema does not define), optional or not, so it is never passed in a shape fl guessed or left out unseen. Arguments render by plan ruling 15, `{var}` templates resolved from `variables` (`value`, then `default`); a required argument with no value is refused. Secrets stay references (§2.1, §6; plan rulings 16 and 18): a required secret variable as `{ secret = true }`; a secret inside an argument only as docker's `-e NAME={var}`, which becomes `-e NAME` and a reference; an optional argument that needs a secret, or an optional secret variable, left out with a note unless `--with <NAME>` names it (an agent passes an unset `${NAME}` on as text, so recording it would break the server's own fallback; plan ruling 16, applied to optional secret variables as well); a secret header with no value as `<SERVER>_<HEADER>` (the catalog name, uppercased, `-` as `_`), and `Bearer {token}` as `scheme = "Bearer"` with `<SERVER>_<VAR>`. A non-secret required variable takes its value from `--env NAME=value`, recorded as a literal and warned about with every other literal (§3.2 step 5, §6); a value given for a secret is refused without being shown. For `upgrade`, `FreezeOptions::upgrading` carries the pinned entry's route, literal values and included secrets to the new version, `diff` lists every changed field of the launch spec, and `check_upgrade` compares by semantic-version precedence.

**Blast radius:** fl-mcp only. `McpError` gains two variants (`Unfreezable`, `NotNewer`) after `NotFound`; nothing outside fl-mcp names `McpError` yet. `registry::printable` becomes `pub(crate)` so freezing shows registry text the same way; its behaviour is unchanged. No dependency changes.

**Files:**
- Modify: `crates/mcp/src/lib.rs` (`mod freeze`; two `McpError` variants)
- Modify: `crates/mcp/src/registry.rs` (`printable` is `pub(crate)`)
- Create: `crates/mcp/src/freeze.rs` (`freeze`, `Route`, `FreezeOptions`, `Frozen`, `diff`, `Change`, `check_upgrade`; tests)

**Interfaces:**
- Consumes: Task 1's `catalog::{Server, EnvValue, HeaderValue, Transport, is_env_name}` (and `Catalog`, `Editor` in tests); Task 3's `registry::{ServerResponse, ServerJson, Package, Remote, Argument, ArgumentKind, Input, KeyValueInput, RegistryType, TransportKind, Status}` and `printable`; in tests, `fake::{FakeRegistry, NOTES, WEATHER, TRACKER, TRACKER_IMAGE, DOCS, DOCS_URL, MULTI, LEGACY, LEGACY_MESSAGE, GONE, GONE_MESSAGE, VERBOSE}` and `Registry::version`. `ArgumentKind` is open and not `Copy` (Task 3), so it is matched by reference.
- Produces (`fl_mcp::freeze`):
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Route { Npm, Pypi, Oci, Remote }`; `Route::flag(self) -> &'static str` (`--package npm`, `--package pypi`, `--package oci`, `--remote`); `Route::of(server: &Server) -> Option<Route>` (`http`/`sse` → `Remote`; stdio `npx` → `Npm`, `uvx` → `Pypi`, `docker` → `Oci`; any other command → `None`).
  - `#[derive(Debug, Clone, Default, PartialEq, Eq)] pub struct FreezeOptions { pub name: String, pub route: Option<Route>, pub env: BTreeMap<String, String>, pub with: BTreeSet<String>, pub kept_env: BTreeMap<String, String>, pub kept_with: BTreeSet<String> }` — `name` is the catalog name; `env` (`--env`) and `with` (`--with`, an optional secret argument's key or an optional secret variable's name) must each name something, `kept_*` are used where they apply and ignored elsewhere. `FreezeOptions::upgrading(name: &str, pinned: &Server) -> FreezeOptions` (route from `Route::of`, `kept_env` = the pinned literal `env` values, `kept_with` = the pinned secret `env` keys; `env` and `with` empty).
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Frozen { pub server: Server, pub warnings: Vec<String>, pub secrets: Vec<String>, pub notes: Vec<String> }` — `server` has `from` = the registry name, `version` = `server.version`, `enabled = true`, `vendors = None` (`upgrade` carries the pinned `enabled` and `vendors` itself); `warnings` = the deprecated status, then one per `Server::literal_values()` entry; `secrets` = every variable a person sets, sorted; `notes` = optional secret arguments and variables left out, each naming its `--with`.
  - `pub fn freeze(resp: &ServerResponse, opts: &FreezeOptions) -> Result<Frozen, McpError>` — refusals are `McpError::Unfreezable`.
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Change { pub field: String, pub old: Option<String>, pub new: Option<String> }`, `Display` as `<field>: <old> -> <new>` (`(none)` for absent); `pub fn diff(old: &Server, new: &Server) -> Vec<Change>` — fields in the order `from`, `version`, `transport`, `command`, `args`, `env.<NAME>`…, `url`, `headers.<NAME>`…; values as the catalog spells them (`"1.2.0"`, `["-y", "…"]`, `{ secret = true, env = "X" }`); `enabled` and `vendors` are not compared.
  - `pub fn check_upgrade(name: &str, pinned: &str, offered: &str, named: bool) -> Result<(), McpError>` — `named` is `--to`; refusals are `McpError::NotNewer`.
  - `McpError` gains `Unfreezable { from: String, version: String, problem: String, next: String }` (``"`{from}` {version} cannot be frozen into the catalog: {problem}. {next}"``) and `NotNewer { name: String, problem: String, next: String }` (``"server `{name}`: {problem}. {next}"``).
- Unique phrases (`Unfreezable` problems): `is deleted in the registry (<message>)` · `offers no launch route` · `offers more than one launch route:` (then each route: `` `--package npm` (<identifier>)``, `` `--remote` (<type>, <url>)``, `` a `<type>` package, which fl cannot freeze``) · ``offers no `<flag>` route, only`` · ``offers the `<flag>` route more than once`` · `is not an exact version` · `names no exact version` · ``a `<type>` package; fl freezes npm, PyPI and OCI packages only`` · ``its package's transport is `<type>`, not stdio`` · `has neither a tag nor a digest` · ``is tagged `latest` `` · `a positional runtime argument (` · ``its argument `<label>` holds a secret`` · `is required and has no value` · `names an unset variable` · `which has neither a value nor a default` · `which is a secret` · `which fl does not know` · `an argument type fl does not know` (``its argument `<label>` is of type `<type>`, an argument type fl does not know``) · `a secret is never recorded` (next ``Set <NAME> in the environment instead``) · `needs a value` (next ``Give it with `--env <NAME>=<value>` ``) · `names no environment variable of this launch route` · `names no optional argument or variable that needs a secret` · `a shape fl cannot record` · `is required, and the registry gives it no value` · `cannot be an environment variable name`. Warnings and notes: `is deprecated in the registry` · ``is a literal value: it will be committed with the catalog`` · ``it is optional and needs a secret. `--with <NAME>` includes it``. `NotNewer`: `is not newer` · `cannot be compared with it` · ``fl mcp upgrade <name> --to <version>` moves it there anyway``.

- [ ] **Step 1: Write the failing tests**

In `crates/mcp/src/lib.rs`, replace the three lines `#[cfg(any(test, feature = "fake"))]`, `pub mod fake;` and `pub mod registry;` with:

```rust
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod freeze;
pub mod registry;
```

Create `crates/mcp/src/freeze.rs` (its implementation goes between the module comment and the tests in Step 3):

```rust
//! Freezing a registry entry into a catalog entry (MCP spec §3.2): the launch
//! spec is written out in full when a server is added, so `sync` reads nothing
//! else. What fl cannot freeze honestly is refused, naming what to do instead.
//! And what `upgrade` shows and checks (MCP spec §3.3). Nothing here reads the
//! network: these functions take what the registry client read.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, Editor};
    use crate::fake::{self, FakeRegistry};
    use crate::registry::Registry;
    use serde_json::{Value, json};

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn named(name: &str) -> FreezeOptions {
        FreezeOptions {
            name: name.to_string(),
            ..FreezeOptions::default()
        }
    }

    fn routed(name: &str, route: Route) -> FreezeOptions {
        FreezeOptions {
            route: Some(route),
            ..named(name)
        }
    }

    /// One fixture of the fake registry, as `add --from` reads it.
    fn fixture(name: &str, version: &str) -> ServerResponse {
        let fake = FakeRegistry::start();
        Registry::new(&fake.url())
            .unwrap()
            .version(name, version)
            .unwrap()
    }

    fn response(server: Value) -> ServerResponse {
        response_with(server, "active", None)
    }

    fn response_with(server: Value, status: &str, message: Option<&str>) -> ServerResponse {
        let mut official = json!({
            "status": status,
            "statusChangedAt": "2026-10-01T00:00:00Z",
            "publishedAt": "2026-10-01T00:00:00Z",
            "isLatest": true
        });
        if let Some(m) = message {
            official["statusMessage"] = json!(m);
        }
        serde_json::from_value(json!({
            "server": server,
            "_meta": { "io.modelcontextprotocol.registry/official": official }
        }))
        .unwrap()
    }

    /// `io.example/sample` 1.0.0 with these packages and remotes.
    fn sample(packages: Value, remotes: Value) -> Value {
        json!({
            "name": "io.example/sample",
            "description": "A sample server.",
            "version": "1.0.0",
            "packages": packages,
            "remotes": remotes
        })
    }

    fn with(mut base: Value, extra: Value) -> Value {
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    fn npm(extra: Value) -> Value {
        let base = json!({
            "registryType": "npm",
            "identifier": "@example/sample-mcp",
            "version": "1.0.0",
            "transport": { "type": "stdio" }
        });
        with(base, extra)
    }

    fn oci(extra: Value) -> Value {
        let base = json!({
            "registryType": "oci",
            "identifier": "ghcr.io/example/sample-mcp:1.0.0",
            "transport": { "type": "stdio" }
        });
        with(base, extra)
    }

    fn remote(extra: Value) -> Value {
        let base = json!({ "type": "streamable-http", "url": "https://sample.example.com/mcp" });
        with(base, extra)
    }

    /// The real shape of GitHub's server 2.0.1 (an OCI package whose token
    /// is an optional `-e NAME={token}` runtime argument, and a remote),
    /// under a fixture name.
    fn github() -> ServerResponse {
        response(json!({
            "name": "io.example/github",
            "description": "Connect AI assistants to GitHub.",
            "version": "2.0.1",
            "packages": [{
                "registryType": "oci",
                "identifier": "ghcr.io/example/github-mcp-server:2.0.1",
                "transport": { "type": "stdio" },
                "runtimeArguments": [
                    { "value": "127.0.0.1:8085:8085", "type": "named", "name": "-p" },
                    { "value": "GITHUB_OAUTH_CALLBACK_PORT=8085", "type": "named", "name": "-e" },
                    { "description": "Optional GitHub Personal Access Token.",
                      "value": "GITHUB_PERSONAL_ACCESS_TOKEN={token}",
                      "variables": { "token": { "format": "string", "isSecret": true } },
                      "type": "named", "name": "-e" }
                ]
            }],
            "remotes": [{
                "type": "streamable-http", "url": "https://api.example.com/mcp/",
                "headers": [{ "isSecret": true, "name": "Authorization" }]
            }]
        }))
    }

    /// The catalog takes the frozen entry as it is, and reads it back
    /// equal; the catalog's text, for a look at what is committed.
    fn committed(name: &str, server: &Server) -> String {
        let dir = tempfile::tempdir().unwrap();
        let mut editor = Editor::open(dir.path()).unwrap();
        editor.add(name, server).unwrap();
        let text = editor.text();
        let back = Catalog::parse(&text, editor.path()).unwrap();
        assert_eq!(&back.servers[name], server, "{text}");
        text
    }

    fn secret() -> EnvValue {
        EnvValue::Secret { env: None }
    }

    fn literal(value: &str) -> EnvValue {
        EnvValue::Literal(value.to_string())
    }

    // MCP spec §3.2 step 4: npm runs as `npx -y <identifier>@<version>`,
    // with the package arguments that have a value appended.
    #[test]
    fn an_npm_package_freezes_to_npx_at_its_exact_version() {
        let frozen = freeze(&fixture(fake::NOTES, "latest"), &named("notes")).unwrap();
        let s = &frozen.server;
        assert_eq!(s.from.as_deref(), Some(fake::NOTES));
        assert_eq!(s.version.as_deref(), Some("1.2.0"));
        assert!(s.enabled);
        assert_eq!(s.vendors, None);
        assert_eq!(s.transport, Transport::Stdio);
        assert_eq!(s.command.as_deref(), Some("npx"));
        let args = strings(&["-y", "@example/notes-mcp@1.2.0", "./notes"]);
        assert_eq!(s.args, Some(args));
        let env = BTreeMap::from([
            ("NOTES_LOG".to_string(), literal("info")),
            ("NOTES_TOKEN".to_string(), secret()),
        ]);
        assert_eq!(s.env, Some(env));
        assert_eq!((&s.url, &s.headers), (&None, &None));
        assert_eq!(frozen.secrets, ["NOTES_TOKEN"]);
        assert_eq!(frozen.warnings.len(), 1, "{:?}", frozen.warnings);
        assert!(frozen.warnings[0].contains("`env.NOTES_LOG` is a literal value"));
        assert!(frozen.warnings[0].contains("will be committed with the catalog"));
        assert!(frozen.notes.is_empty());
        committed("notes", s);
    }

    #[test]
    fn a_pypi_package_freezes_to_uvx_at_its_exact_version() {
        let frozen = freeze(&fixture(fake::WEATHER, "latest"), &named("weather")).unwrap();
        let s = &frozen.server;
        assert_eq!(s.version.as_deref(), Some("0.4.1"));
        assert_eq!(s.command.as_deref(), Some("uvx"));
        let args = strings(&["example-weather-mcp==0.4.1", "--units", "metric"]);
        assert_eq!(s.args, Some(args));
        let env = BTreeMap::from([("WEATHER_API_KEY".to_string(), secret())]);
        assert_eq!(s.env, Some(env));
        assert_eq!(frozen.secrets, ["WEATHER_API_KEY"]);
        assert!(frozen.warnings.is_empty(), "{:?}", frozen.warnings);
        committed("weather", s);
    }

    // MCP spec §3.2 step 4: `docker run -i --rm`, the runtime arguments, one
    // `-e NAME` per variable, and the identifier as given.
    #[test]
    fn an_oci_package_freezes_to_docker_run_with_the_image_as_given() {
        // The token's argument is optional (its variable is required once
        // the argument is passed), so it is named to be included.
        let mut opts = named("tracker");
        opts.with.insert("TRACKER_TOKEN".to_string());
        let frozen = freeze(&fixture(fake::TRACKER, "latest"), &opts).unwrap();
        let s = &frozen.server;
        assert_eq!(s.version.as_deref(), Some("2.0.1"));
        assert_eq!(s.command.as_deref(), Some("docker"));
        let args = [
            "run",
            "-i",
            "--rm",
            "-e",
            "TRACKER_PORT=8085",
            "-e",
            "TRACKER_TOKEN",
            fake::TRACKER_IMAGE,
        ];
        assert_eq!(s.args, Some(strings(&args)));
        let env = BTreeMap::from([("TRACKER_TOKEN".to_string(), secret())]);
        assert_eq!(s.env, Some(env));
        assert_eq!(frozen.secrets, ["TRACKER_TOKEN"]);
        committed("tracker", s);

        // Each environment variable is passed into the container by name; a
        // digest pins as well as a tag; a registry's port is not a tag.
        for image in [
            "ghcr.io/example/sample-mcp@sha256:0123456789abcdef",
            "registry.example.com:5000/example/sample-mcp:1.0.0",
        ] {
            let package = oci(json!({
                "identifier": image,
                "environmentVariables": [
                    { "name": "SAMPLE_REGION", "default": "eu" },
                    { "name": "SAMPLE_KEY", "isSecret": true, "isRequired": true },
                    { "name": "SAMPLE_DEBUG" }
                ]
            }));
            let frozen = freeze(&response(sample(json!([package]), json!([]))), &named("s"));
            let s = frozen.unwrap().server;
            let args = [
                "run",
                "-i",
                "--rm",
                "-e",
                "SAMPLE_REGION",
                "-e",
                "SAMPLE_KEY",
                image,
            ];
            assert_eq!(s.args, Some(strings(&args)));
            let env = BTreeMap::from([
                ("SAMPLE_KEY".to_string(), secret()),
                ("SAMPLE_REGION".to_string(), literal("eu")),
            ]);
            assert_eq!(s.env, Some(env));
        }

        // A secret in an argument is taken only as docker's own
        // `-e NAME={var}`, with `NAME` a variable name and `{var}` defined.
        let token = json!({ "token": { "isSecret": true } });
        let refused = [
            (
                "runtimeArguments",
                "--label",
                "SAMPLE_TOKEN={token}",
                token.clone(),
                false,
            ),
            (
                "runtimeArguments",
                "-e",
                "sample-token={token}",
                token.clone(),
                false,
            ),
            (
                "runtimeArguments",
                "-e",
                "SAMPLE_TOKEN=x{token}",
                token.clone(),
                false,
            ),
            (
                "runtimeArguments",
                "-e",
                "SAMPLE_TOKEN={nope}",
                json!({}),
                true,
            ),
            (
                "packageArguments",
                "-e",
                "SAMPLE_TOKEN={token}",
                token,
                false,
            ),
        ];
        for (list, name, value, variables, secret) in refused {
            let argument = json!({
                "type": "named", "name": name, "value": value, "isRequired": true,
                "isSecret": secret, "variables": variables
            });
            let package = oci(json!({ (list): [argument] }));
            let err = freeze(&response(sample(json!([package]), json!([]))), &named("s"));
            let msg = err.unwrap_err().to_string();
            assert!(
                msg.contains("holds a secret"),
                "{list} {name} {value}: {msg}"
            );
        }
    }

    // MCP spec §3.2 step 4: an optional argument that carries a secret is left
    // out, with a note, unless `--with <NAME>` names it; then the docker
    // `-e NAME={var}` becomes `-e NAME` and a secret reference.
    #[test]
    fn an_optional_secret_argument_is_left_out_unless_named_with_with() {
        let frozen = freeze(&github(), &routed("github", Route::Oci)).unwrap();
        let s = &frozen.server;
        let args = [
            "run",
            "-i",
            "--rm",
            "-p",
            "127.0.0.1:8085:8085",
            "-e",
            "GITHUB_OAUTH_CALLBACK_PORT=8085",
            "ghcr.io/example/github-mcp-server:2.0.1",
        ];
        assert_eq!(s.args, Some(strings(&args)));
        assert_eq!(s.env, None);
        assert!(frozen.secrets.is_empty());
        assert_eq!(frozen.notes.len(), 1, "{:?}", frozen.notes);
        assert!(frozen.notes[0].contains("it is optional and needs a secret"));
        assert!(frozen.notes[0].contains("`--with GITHUB_PERSONAL_ACCESS_TOKEN` includes it"));
        committed("github", s);

        let mut opts = routed("github", Route::Oci);
        opts.with.insert("GITHUB_PERSONAL_ACCESS_TOKEN".to_string());
        let frozen = freeze(&github(), &opts).unwrap();
        let s = &frozen.server;
        let args = [
            "run",
            "-i",
            "--rm",
            "-p",
            "127.0.0.1:8085:8085",
            "-e",
            "GITHUB_OAUTH_CALLBACK_PORT=8085",
            "-e",
            "GITHUB_PERSONAL_ACCESS_TOKEN",
            "ghcr.io/example/github-mcp-server:2.0.1",
        ];
        assert_eq!(s.args, Some(strings(&args)));
        let env = BTreeMap::from([("GITHUB_PERSONAL_ACCESS_TOKEN".to_string(), secret())]);
        assert_eq!(s.env, Some(env));
        assert_eq!(frozen.secrets, ["GITHUB_PERSONAL_ACCESS_TOKEN"]);
        assert!(frozen.notes.is_empty(), "{:?}", frozen.notes);
        committed("github", s);
    }

    // As for an optional argument: an optional secret variable is left out,
    // with a note, unless `--with <NAME>` names it, or the pinned entry had it.
    #[test]
    fn an_optional_secret_variable_is_left_out_unless_named_with_with() {
        let package = oci(json!({
            "environmentVariables": [
                { "name": "SAMPLE_KEY", "isSecret": true, "isRequired": true },
                { "name": "SAMPLE_EXTRA", "isSecret": true }
            ]
        }));
        let entry = response(sample(json!([package]), json!([])));
        let frozen = freeze(&entry, &named("sample")).unwrap();
        let s = &frozen.server;
        let args = [
            "run",
            "-i",
            "--rm",
            "-e",
            "SAMPLE_KEY",
            "ghcr.io/example/sample-mcp:1.0.0",
        ];
        assert_eq!(s.args, Some(strings(&args)));
        let env = BTreeMap::from([("SAMPLE_KEY".to_string(), secret())]);
        assert_eq!(s.env, Some(env));
        assert_eq!(frozen.secrets, ["SAMPLE_KEY"]);
        assert_eq!(frozen.notes.len(), 1, "{:?}", frozen.notes);
        assert!(
            frozen.notes[0].contains("Left out `SAMPLE_EXTRA`: it is optional and needs a secret")
        );
        assert!(frozen.notes[0].contains("`--with SAMPLE_EXTRA` includes it"));

        let mut opts = named("sample");
        opts.with.insert("SAMPLE_EXTRA".to_string());
        let frozen = freeze(&entry, &opts).unwrap();
        let args = [
            "run",
            "-i",
            "--rm",
            "-e",
            "SAMPLE_KEY",
            "-e",
            "SAMPLE_EXTRA",
            "ghcr.io/example/sample-mcp:1.0.0",
        ];
        assert_eq!(frozen.server.args, Some(strings(&args)));
        let env = BTreeMap::from([
            ("SAMPLE_EXTRA".to_string(), secret()),
            ("SAMPLE_KEY".to_string(), secret()),
        ]);
        assert_eq!(frozen.server.env, Some(env.clone()));
        assert_eq!(frozen.secrets, ["SAMPLE_EXTRA", "SAMPLE_KEY"]);
        assert!(frozen.notes.is_empty(), "{:?}", frozen.notes);
        committed("sample", &frozen.server);

        // `upgrade` keeps it.
        let carried = FreezeOptions::upgrading("sample", &frozen.server);
        assert!(carried.kept_with.contains("SAMPLE_EXTRA"));
        let next = freeze(&entry, &carried).unwrap().server;
        assert_eq!(next.env, Some(env));
    }

    // MCP spec §3.2 step 4: a secret header with no value reads the variable
    // `<SERVER>_<HEADER>`, which holds the whole value; `Bearer {token}`
    // keeps its scheme.
    #[test]
    fn a_remote_freezes_to_its_url_and_a_secret_header_to_a_derived_variable() {
        let frozen = freeze(&fixture(fake::DOCS, "latest"), &named("docs")).unwrap();
        let s = &frozen.server;
        assert_eq!(s.from.as_deref(), Some(fake::DOCS));
        assert_eq!(s.version.as_deref(), Some("1.0.0"));
        assert_eq!(s.transport, Transport::Http);
        assert_eq!(s.url.as_deref(), Some(fake::DOCS_URL));
        assert_eq!((&s.command, &s.args, &s.env), (&None, &None, &None));
        let auth = HeaderValue::Secret {
            env: "DOCS_AUTHORIZATION".to_string(),
            scheme: None,
        };
        let headers = BTreeMap::from([("Authorization".to_string(), auth)]);
        assert_eq!(s.headers, Some(headers));
        assert_eq!(frozen.secrets, ["DOCS_AUTHORIZATION"]);
        committed("docs", s);

        // The catalog name, not the registry's, names the variable.
        let frozen = freeze(&fixture(fake::DOCS, "latest"), &named("team-docs")).unwrap();
        assert_eq!(frozen.secrets, ["TEAM_DOCS_AUTHORIZATION"]);
        // A catalog name may start with a digit; a variable may not.
        let err = freeze(&fixture(fake::DOCS, "latest"), &named("9docs")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("`9DOCS_AUTHORIZATION` cannot be an environment variable name"),
            "{msg}"
        );

        let sse = json!({
            "type": "sse",
            "url": "https://{tenant}.example.com/sse",
            "variables": { "tenant": { "default": "acme" } },
            "headers": [
                { "name": "Authorization", "value": "Bearer {token}",
                  "variables": { "token": { "isSecret": true } } },
                { "name": "X-Api-Key", "value": "{key}",
                  "variables": { "key": { "isSecret": true } } },
                { "name": "X-Region", "value": "eu", "default": "us" },
                { "name": "X-Team", "value": "{team}",
                  "variables": { "team": { "default": "widgets" } } },
                { "name": "X-Trace" }
            ]
        });
        let frozen = freeze(&response(sample(json!([]), json!([sse]))), &named("sample"));
        let frozen = frozen.unwrap();
        let s = &frozen.server;
        assert_eq!(s.transport, Transport::Sse);
        assert_eq!(s.url.as_deref(), Some("https://acme.example.com/sse"));
        let headers = BTreeMap::from([
            (
                "Authorization".to_string(),
                HeaderValue::Secret {
                    env: "SAMPLE_TOKEN".to_string(),
                    scheme: Some("Bearer".to_string()),
                },
            ),
            (
                "X-Api-Key".to_string(),
                HeaderValue::Secret {
                    env: "SAMPLE_KEY".to_string(),
                    scheme: None,
                },
            ),
            (
                "X-Region".to_string(),
                HeaderValue::Literal("eu".to_string()),
            ),
            (
                "X-Team".to_string(),
                HeaderValue::Literal("widgets".to_string()),
            ),
        ]);
        assert_eq!(s.headers, Some(headers));
        assert_eq!(frozen.secrets, ["SAMPLE_KEY", "SAMPLE_TOKEN"]);
        assert_eq!(frozen.warnings.len(), 2, "{:?}", frozen.warnings);
        assert!(frozen.warnings[0].contains("`headers.X-Region` is a literal value"));
        assert!(frozen.warnings[1].contains("`headers.X-Team` is a literal value"));
        committed("sample", s);

        // A scheme is one word, and the variable must be defined.
        for (value, variables) in [
            ("Bearer x {token}", json!({ "token": {} })),
            (" {token}", json!({ "token": {} })),
            ("Bearer {nope}", json!({})),
        ] {
            let header = json!({
                "name": "Authorization", "value": value, "isSecret": true,
                "variables": variables
            });
            let r = remote(json!({ "headers": [header] }));
            let err = freeze(&response(sample(json!([]), json!([r]))), &named("sample"));
            let msg = err.unwrap_err().to_string();
            assert!(msg.contains("a shape fl cannot record"), "{value}: {msg}");
        }
    }

    // MCP spec §3.2 step 4: how each argument is passed.
    #[test]
    fn arguments_render_by_kind_format_and_variables() {
        let package = npm(json!({
            "runtimeArguments": [
                { "type": "named", "name": "--prefer-offline", "value": "true",
                  "format": "boolean" },
                { "type": "positional", "value": "--no-update-notifier" }
            ],
            "packageArguments": [
                { "type": "named", "name": "--port", "value": "8080" },
                { "type": "named", "name": "--level", "value": "debug", "default": "info" },
                { "type": "named", "name": "--verbose", "value": "true", "format": "boolean" },
                { "type": "named", "name": "--quiet", "default": "false", "format": "boolean" },
                { "type": "positional", "valueHint": "dir", "value": "{root}/data",
                  "variables": { "root": { "default": "/srv" } } },
                { "type": "named", "name": "--mode", "value": "{mode}",
                  "variables": { "mode": { "value": "fast", "default": "slow" } } },
                { "type": "named", "name": "--color" },
                { "type": "named", "name": "--region", "value": "{region}",
                  "variables": { "region": {} } },
                { "type": "positional", "value": "{literal}" }
            ]
        }));
        let frozen = freeze(&response(sample(json!([package]), json!([]))), &named("s"));
        let args = [
            "-y",
            "--prefer-offline",
            "--no-update-notifier",
            "@example/sample-mcp@1.0.0",
            "--port",
            "8080",
            "--level",
            "debug",
            "--verbose",
            "/srv/data",
            "--mode",
            "fast",
            "{literal}",
        ];
        assert_eq!(frozen.unwrap().server.args, Some(strings(&args)));

        // A docker `-e NAME={var}` is only docker's: for npx it is an
        // argument that holds a secret.
        let package = npm(json!({
            "runtimeArguments": [
                { "type": "named", "name": "-e", "value": "SAMPLE_TOKEN={token}",
                  "isRequired": true, "variables": { "token": { "isSecret": true } } }
            ]
        }));
        let err = freeze(&response(sample(json!([package]), json!([]))), &named("s"));
        assert!(err.unwrap_err().to_string().contains("holds a secret"));
    }

    // MCP spec §3.2 step 5: a required variable with no value takes one from
    // `--env`, recorded as a literal and warned about.
    #[test]
    fn a_required_variable_takes_its_value_from_env_and_is_warned_as_committed() {
        let package = npm(json!({
            "environmentVariables": [
                { "name": "SAMPLE_HOME", "isRequired": true },
                { "name": "SAMPLE_DEBUG" },
                { "name": "SAMPLE_LEVEL", "default": "info" },
                { "name": "SAMPLE_MODE", "value": "fixed", "default": "other" },
                { "name": "SAMPLE_URL", "value": "https://{host}/",
                  "variables": { "host": { "default": "example.com" } } },
                { "name": "SAMPLE_AUTH", "value": "Bearer {token}", "isRequired": true,
                  "variables": { "token": { "isSecret": true } } }
            ]
        }));
        let entry = response(sample(json!([package]), json!([])));
        let mut opts = named("sample");
        opts.env
            .insert("SAMPLE_HOME".to_string(), "/srv/sample".to_string());
        opts.env
            .insert("SAMPLE_LEVEL".to_string(), "debug".to_string());
        let frozen = freeze(&entry, &opts).unwrap();
        // A variable whose value names a secret is a secret: the person
        // sets its whole value.
        let env = BTreeMap::from([
            ("SAMPLE_AUTH".to_string(), secret()),
            ("SAMPLE_HOME".to_string(), literal("/srv/sample")),
            ("SAMPLE_LEVEL".to_string(), literal("debug")),
            ("SAMPLE_MODE".to_string(), literal("fixed")),
            ("SAMPLE_URL".to_string(), literal("https://example.com/")),
        ]);
        assert_eq!(frozen.server.env, Some(env));
        assert_eq!(frozen.secrets, ["SAMPLE_AUTH"]);
        assert_eq!(frozen.warnings.len(), 4, "{:?}", frozen.warnings);
        assert!(frozen.warnings[0].contains("`env.SAMPLE_HOME` is a literal value"));
        assert!(frozen.warnings[1].contains("`env.SAMPLE_LEVEL` is a literal value"));
    }

    // MCP spec §3.2 step 3.
    #[test]
    fn a_route_is_chosen_with_package_or_remote() {
        let multi = fixture(fake::MULTI, "latest");
        let cases = [
            (Route::Npm, "npx", vec!["-y", "@example/multi-mcp@3.0.0"]),
            (
                Route::Oci,
                "docker",
                vec!["run", "-i", "--rm", "ghcr.io/example/multi-mcp:3.0.0"],
            ),
        ];
        for (route, command, args) in cases {
            let s = freeze(&multi, &routed("multi", route)).unwrap().server;
            assert_eq!(s.command.as_deref(), Some(command));
            assert_eq!(s.args, Some(strings(&args)));
            assert_eq!(Route::of(&s), Some(route));
        }
        let s = freeze(&multi, &routed("multi", Route::Remote))
            .unwrap()
            .server;
        assert_eq!(s.transport, Transport::Sse);
        assert_eq!(s.url.as_deref(), Some("https://multi.example.com/sse"));
        assert_eq!(s.headers, None);
        assert_eq!(Route::of(&s), Some(Route::Remote));
        let pypi = freeze(&fixture(fake::WEATHER, "latest"), &named("w")).unwrap();
        assert_eq!(Route::of(&pypi.server), Some(Route::Pypi));
        let by_hand = Server {
            command: Some("node".to_string()),
            ..pypi.server
        };
        assert_eq!(Route::of(&by_hand), None);
    }

    // MCP spec §3.2 step 2.
    #[test]
    fn a_deleted_server_is_refused_and_a_deprecated_one_warned_about() {
        let err = freeze(&fixture(fake::GONE, "latest"), &named("gone")).unwrap_err();
        assert!(matches!(err, McpError::Unfreezable { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("`io.example/gone` 1.0.0 cannot be frozen"),
            "{msg}"
        );
        assert!(msg.contains(&format!(
            "is deleted in the registry ({})",
            fake::GONE_MESSAGE
        )));

        let frozen = freeze(&fixture(fake::LEGACY, "latest"), &named("legacy")).unwrap();
        assert_eq!(frozen.server.command.as_deref(), Some("uvx"));
        assert_eq!(frozen.warnings.len(), 1, "{:?}", frozen.warnings);
        let warning = &frozen.warnings[0];
        assert!(
            warning.contains("is deprecated in the registry"),
            "{warning}"
        );
        assert!(warning.contains(fake::LEGACY_MESSAGE), "{warning}");
    }

    // MCP spec §3.2 step 1: the version the registry returned, never the
    // word `latest`.
    #[test]
    fn the_frozen_entry_records_the_registrys_version_never_latest() {
        for (name, version) in [
            (fake::NOTES, "1.2.0"),
            (fake::TRACKER, "2.0.1"),
            (fake::DOCS, "1.0.0"),
        ] {
            let frozen = freeze(&fixture(name, "latest"), &named("s")).unwrap();
            assert_eq!(frozen.server.version.as_deref(), Some(version));
            let text = committed("s", &frozen.server);
            assert!(!text.contains("latest"), "{text}");
        }
        let frozen = freeze(&fixture(fake::NOTES, "1.0.0"), &named("s")).unwrap();
        assert_eq!(frozen.server.version.as_deref(), Some("1.0.0"));
        let args = strings(&["-y", "@example/notes-mcp@1.0.0", "./notes"]);
        assert_eq!(frozen.server.args, Some(args));
    }

    // MCP spec §3.2: each entry fl cannot freeze honestly is refused before
    // anything is written, with its own phrase and the way on.
    #[test]
    fn each_unfreezable_entry_is_refused_naming_the_way_on() {
        const VALUE: &str = "s3cr3t-value-0123456789";
        let plain = |packages: Value| response(sample(packages, json!([])));
        let remote_only = |r: Value| response(sample(json!([]), json!([r])));
        let opts_with = |f: &dyn Fn(&mut FreezeOptions)| {
            let mut o = named("sample");
            f(&mut o);
            o
        };
        let header = |h: Value| remote_only(remote(json!({ "headers": [h] })));
        let cases: Vec<(&str, ServerResponse, FreezeOptions, &str, &str)> = vec![
            (
                "deleted",
                response_with(
                    sample(json!([npm(json!({}))]), json!([])),
                    "deleted",
                    Some("Withdrawn."),
                ),
                named("sample"),
                "is deleted in the registry (Withdrawn.)",
                "`fl mcp search <text>` lists",
            ),
            (
                "no route",
                plain(json!([])),
                named("sample"),
                "offers no launch route",
                "`fl mcp add sample --url <url>`",
            ),
            (
                "several routes",
                response(sample(
                    json!([
                        npm(json!({})),
                        oci(json!({})),
                        npm(json!({ "registryType": "mcpb" }))
                    ]),
                    json!([remote(json!({ "type": "sse" }))]),
                )),
                named("sample"),
                "offers more than one launch route: `--package npm` (@example/sample-mcp), \
                 `--package oci` (ghcr.io/example/sample-mcp:1.0.0), a `mcpb` package, which \
                 fl cannot freeze, `--remote` (sse, https://sample.example.com/mcp)",
                "Choose one with `--package <type>` or `--remote`",
            ),
            (
                "a route not on offer",
                plain(json!([npm(json!({}))])),
                routed("sample", Route::Pypi),
                "offers no `--package pypi` route, only `--package npm`",
                "Choose one it offers",
            ),
            (
                "a route offered twice",
                response(sample(
                    json!([]),
                    json!([remote(json!({})), remote(json!({ "type": "sse" }))]),
                )),
                routed("sample", Route::Remote),
                "offers the `--remote` route more than once",
                "`fl mcp add sample --url <url>`",
            ),
            (
                "the server's version is not exact",
                response(with(
                    sample(json!([npm(json!({}))]), json!([])),
                    json!({ "version": "latest" }),
                )),
                named("sample"),
                "its version `latest` is not an exact version",
                "add the server by hand",
            ),
            (
                "a package with no version",
                plain(json!([npm(json!({ "version": null }))])),
                named("sample"),
                "names no exact version",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "mcpb",
                plain(json!([npm(json!({ "registryType": "mcpb" }))])),
                named("sample"),
                "a `mcpb` package; fl freezes npm, PyPI and OCI packages only",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "nuget",
                plain(json!([npm(json!({ "registryType": "nuget" }))])),
                named("sample"),
                "a `nuget` package",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "an unknown registry type",
                plain(json!([npm(json!({ "registryType": "gem" }))])),
                named("sample"),
                "a `gem` package",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "a package that serves HTTP",
                plain(json!([npm(json!({
                    "transport": { "type": "streamable-http",
                                   "url": "http://localhost:{port}/mcp" }
                }))])),
                named("sample"),
                "its package's transport is `streamable-http`, not stdio",
                "Start it yourself",
            ),
            (
                "an untagged image",
                plain(json!([oci(json!({
                    "identifier": "registry.example.com:5000/example/sample-mcp"
                }))])),
                named("sample"),
                "has neither a tag nor a digest",
                "naming an exact image",
            ),
            (
                "an image tagged latest",
                plain(json!([oci(json!({
                    "identifier": "ghcr.io/example/sample-mcp:latest"
                }))])),
                named("sample"),
                "is tagged `latest`",
                "naming an exact image",
            ),
            (
                "a positional runtime argument",
                plain(json!([oci(json!({
                    "runtimeArguments": [
                        { "type": "positional", "value": "run" },
                        { "type": "named", "name": "--rm", "value": "true",
                          "format": "boolean" }
                    ]
                }))])),
                named("sample"),
                "a positional runtime argument (`run`)",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "a secret in an argument that is not a docker -e",
                plain(json!([npm(json!({
                    "packageArguments": [
                        { "type": "named", "name": "--token", "value": "{token}",
                          "isRequired": true,
                          "variables": { "token": { "isSecret": true } } }
                    ]
                }))])),
                named("sample"),
                "its argument `--token {token}` holds a secret",
                "passing the secret in an environment variable",
            ),
            (
                "an argument that is itself a secret",
                plain(json!([npm(json!({
                    "packageArguments": [
                        { "type": "named", "name": "--api-key", "isSecret": true,
                          "isRequired": true }
                    ]
                }))])),
                named("sample"),
                "its argument `--api-key` holds a secret",
                "passing the secret in an environment variable",
            ),
            (
                "a required argument with no value",
                plain(json!([npm(json!({
                    "packageArguments": [
                        { "type": "positional", "valueHint": "notes_dir", "isRequired": true }
                    ]
                }))])),
                named("sample"),
                "its argument `notes_dir` is required and has no value",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "a required argument naming an unset variable",
                plain(json!([npm(json!({
                    "packageArguments": [
                        { "type": "named", "name": "--root", "value": "{root}",
                          "isRequired": true, "variables": { "root": {} } }
                    ]
                }))])),
                named("sample"),
                "names an unset variable, `{root}`",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "a remote URL variable with no default",
                remote_only(remote(json!({
                    "url": "https://{tenant}.example.com/mcp",
                    "variables": { "tenant": { "isRequired": true } }
                }))),
                named("sample"),
                "the remote URL names `{tenant}`, which has neither a value nor a default",
                "`fl mcp add sample --url <url>`",
            ),
            (
                "a secret remote URL variable",
                remote_only(remote(json!({
                    "url": "https://sample.example.com/{key}/mcp",
                    "variables": { "key": { "isSecret": true, "default": "k" } }
                }))),
                named("sample"),
                "the remote URL names `{key}`, which is a secret",
                "`fl mcp add sample --url <url>`",
            ),
            (
                "a remote of an unknown type",
                remote_only(remote(json!({ "type": "websocket" }))),
                named("sample"),
                "its remote is of type `websocket`, which fl does not know",
                "`fl mcp add sample --url <url>`",
            ),
            (
                "a value for a secret",
                plain(json!([npm(json!({
                    "environmentVariables": [
                        { "name": "SAMPLE_TOKEN", "isSecret": true, "isRequired": true }
                    ]
                }))])),
                opts_with(&|o| {
                    o.env.insert("SAMPLE_TOKEN".to_string(), VALUE.to_string());
                }),
                "`--env SAMPLE_TOKEN=…` names a secret, and a secret is never recorded",
                "Set SAMPLE_TOKEN in the environment instead",
            ),
            (
                "a required variable with no value",
                plain(json!([npm(json!({
                    "environmentVariables": [{ "name": "SAMPLE_HOME", "isRequired": true }]
                }))])),
                named("sample"),
                "its environment variable `SAMPLE_HOME` needs a value",
                "Give it with `--env SAMPLE_HOME=<value>`",
            ),
            (
                "an --env naming no variable",
                plain(json!([npm(json!({}))])),
                opts_with(&|o| {
                    o.env.insert("SAMPLE_NOPE".to_string(), VALUE.to_string());
                }),
                "`--env SAMPLE_NOPE=…` names no environment variable of this launch route",
                "Drop it",
            ),
            (
                "a --with naming no argument",
                plain(json!([npm(json!({}))])),
                opts_with(&|o| {
                    o.with.insert("SAMPLE_NOPE".to_string());
                }),
                "`--with SAMPLE_NOPE` names no optional argument or variable that needs a secret",
                "Drop it",
            ),
            (
                "a secret header in another shape",
                header(json!({
                    "name": "Cookie", "value": "session={sid}",
                    "variables": { "sid": { "isSecret": true } }
                })),
                named("sample"),
                "its secret header `Cookie` has the value `session={sid}`, a shape fl cannot \
                 record",
                "`fl mcp add sample --url <url> --header Cookie`",
            ),
            (
                "a required header with no value",
                header(json!({ "name": "X-Tenant", "isRequired": true })),
                named("sample"),
                "its header `X-Tenant` is required, and the registry gives it no value",
                "`fl mcp add sample --url <url> --header X-Tenant`",
            ),
            (
                "an argument of a type fl does not know",
                plain(json!([npm(json!({
                    "packageArguments": [
                        { "type": "flag", "name": "--verbose", "isRequired": true }
                    ]
                }))])),
                named("sample"),
                "its argument `--verbose` is of type `flag`, an argument type fl does not know",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "a variable that cannot be named in the environment",
                plain(json!([npm(json!({
                    "environmentVariables": [
                        { "name": "sample-token", "isSecret": true, "isRequired": true }
                    ]
                }))])),
                named("sample"),
                "`sample-token` cannot be an environment variable name",
                "add the server by hand",
            ),
        ];
        let mut messages = Vec::new();
        for (what, entry, opts, phrase, next) in &cases {
            let err = freeze(entry, opts).expect_err(what);
            assert!(
                matches!(err, McpError::Unfreezable { .. }),
                "{what}: {err:?}"
            );
            let msg = err.to_string();
            assert!(msg.contains("`io.example/sample` "), "{what}: {msg}");
            assert!(msg.contains(phrase), "{what}: {phrase:?} not in: {msg}");
            assert!(msg.contains(next), "{what}: {next:?} not in: {msg}");
            assert!(!msg.contains(VALUE), "{what}: a value in: {msg}");
            messages.push(msg);
        }
        for (i, (what, _, _, phrase, _)) in cases.iter().enumerate() {
            for (j, msg) in messages.iter().enumerate() {
                assert!(
                    i == j || !msg.contains(phrase),
                    "{what}'s {phrase:?} also in: {msg}"
                );
            }
        }
    }

    // The registry serves argument types its schema does not define: such
    // an argument is refused by its type, optional or not, never passed in
    // a shape fl guessed or left out unseen.
    #[test]
    fn an_argument_of_a_type_fl_does_not_know_is_refused_naming_it() {
        let verbose = fixture(fake::VERBOSE, "latest");
        let err = freeze(&verbose, &named("verbose")).unwrap_err();
        assert!(matches!(err, McpError::Unfreezable { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.starts_with("`io.example/verbose` 1.0.0 cannot be frozen into the catalog: "),
            "{msg}"
        );
        assert!(
            msg.contains(
                "its argument `--verbose` is of type `flag`, an argument type fl does not know"
            ),
            "{msg}"
        );
        assert!(msg.contains("`fl mcp add verbose -- <command>`"), "{msg}");
    }

    // MCP spec §3.3: `upgrade` freezes the new version with what the pinned
    // entry chose, where it still applies.
    #[test]
    fn upgrade_keeps_the_pinned_choices_where_they_still_apply() {
        let at = |version: &str, extra: bool| {
            let mut env = vec![json!({ "name": "SAMPLE_HOME", "isRequired": true })];
            let mut runtime = vec![json!({
                "type": "named", "name": "-e", "value": "SAMPLE_TOKEN={token}",
                "variables": { "token": { "isSecret": true } }
            })];
            if !extra {
                env.clear();
                runtime.clear();
            }
            let package = oci(json!({
                "identifier": format!("ghcr.io/example/sample-mcp:{version}"),
                "runtimeArguments": runtime,
                "environmentVariables": env
            }));
            let remote = remote(json!({}));
            let server = with(
                sample(json!([package]), json!([remote])),
                json!({ "version": version }),
            );
            response(server)
        };
        let mut opts = routed("sample", Route::Oci);
        opts.env
            .insert("SAMPLE_HOME".to_string(), "/srv/sample".to_string());
        opts.with.insert("SAMPLE_TOKEN".to_string());
        let pinned = freeze(&at("1.0.0", true), &opts).unwrap().server;

        let carried = FreezeOptions::upgrading("sample", &pinned);
        assert_eq!(carried.name, "sample");
        assert_eq!(carried.route, Some(Route::Oci));
        assert!(carried.env.is_empty() && carried.with.is_empty());
        let next = freeze(&at("1.1.0", true), &carried).unwrap().server;
        let args = [
            "run",
            "-i",
            "--rm",
            "-e",
            "SAMPLE_TOKEN",
            "-e",
            "SAMPLE_HOME",
            "ghcr.io/example/sample-mcp:1.1.0",
        ];
        assert_eq!(next.args, Some(strings(&args)));
        let env = BTreeMap::from([
            ("SAMPLE_HOME".to_string(), literal("/srv/sample")),
            ("SAMPLE_TOKEN".to_string(), secret()),
        ]);
        assert_eq!(next.env, Some(env));

        // A value named on the command line wins over the pinned one.
        let mut given = carried.clone();
        given
            .env
            .insert("SAMPLE_HOME".to_string(), "/data".to_string());
        let next = freeze(&at("1.1.0", true), &given).unwrap().server;
        assert_eq!(next.env.unwrap()["SAMPLE_HOME"], literal("/data"));

        // What the new version no longer has is dropped, not refused.
        let next = freeze(&at("1.2.0", false), &carried).unwrap().server;
        let args = ["run", "-i", "--rm", "ghcr.io/example/sample-mcp:1.2.0"];
        assert_eq!(next.args, Some(strings(&args)));
        assert_eq!(next.env, None);
    }

    // MCP spec §3.3: the difference between the pinned launch spec and the
    // new one.
    #[test]
    fn diff_lists_each_changed_field_of_the_launch_spec() {
        let old = freeze(&fixture(fake::NOTES, "1.1.0"), &named("notes")).unwrap();
        let new = freeze(&fixture(fake::NOTES, "1.2.0"), &named("notes")).unwrap();
        let shown: Vec<String> = diff(&old.server, &new.server)
            .iter()
            .map(Change::to_string)
            .collect();
        assert_eq!(
            shown,
            [
                r#"version: "1.1.0" -> "1.2.0""#,
                concat!(
                    r#"args: ["-y", "@example/notes-mcp@1.1.0", "./notes"] -> "#,
                    r#"["-y", "@example/notes-mcp@1.2.0", "./notes"]"#
                ),
            ]
        );
        assert!(diff(&new.server, &new.server).is_empty());

        let mut changed = new.server.clone();
        let env = changed.env.as_mut().unwrap();
        env.remove("NOTES_LOG");
        env.insert("NOTES_TOKEN".to_string(), literal("x"));
        env.insert(
            "NOTES_HOME".to_string(),
            EnvValue::Secret {
                env: Some("HOME_DIR".to_string()),
            },
        );
        changed.command = Some("node".to_string());
        changed.from = Some("io.example/other".to_string());
        let shown: Vec<String> = diff(&new.server, &changed)
            .iter()
            .map(Change::to_string)
            .collect();
        assert_eq!(
            shown,
            [
                r#"from: "io.example/notes" -> "io.example/other""#,
                r#"command: "npx" -> "node""#,
                r#"env.NOTES_HOME: (none) -> { secret = true, env = "HOME_DIR" }"#,
                r#"env.NOTES_LOG: "info" -> (none)"#,
                r#"env.NOTES_TOKEN: { secret = true } -> "x""#,
            ]
        );

        let docs = freeze(&fixture(fake::DOCS, "latest"), &named("docs"))
            .unwrap()
            .server;
        let mut moved = docs.clone();
        moved.transport = Transport::Sse;
        moved.url = Some("https://docs.example.com/sse".to_string());
        let headers = moved.headers.as_mut().unwrap();
        headers.insert(
            "Authorization".to_string(),
            HeaderValue::Secret {
                env: "DOCS_TOKEN".to_string(),
                scheme: Some("Bearer".to_string()),
            },
        );
        headers.insert(
            "X-Team".to_string(),
            HeaderValue::Literal("widgets".to_string()),
        );
        let shown: Vec<String> = diff(&docs, &moved).iter().map(Change::to_string).collect();
        assert_eq!(
            shown,
            [
                r#"transport: "http" -> "sse""#,
                r#"url: "https://docs.example.com/mcp" -> "https://docs.example.com/sse""#,
                concat!(
                    r#"headers.Authorization: { secret = true, env = "DOCS_AUTHORIZATION" } -> "#,
                    r#"{ secret = true, env = "DOCS_TOKEN", scheme = "Bearer" }"#
                ),
                r#"headers.X-Team: (none) -> "widgets""#,
            ]
        );
        let change = &diff(&docs, &moved)[0];
        assert_eq!(change.field, "transport");
        assert_eq!(change.old.as_deref(), Some(r#""http""#));
        assert_eq!(change.new.as_deref(), Some(r#""sse""#));
    }

    // MCP spec §3.3: a version that is not newer is refused unless named
    // with `--to`; versions compare by semantic-version precedence.
    #[test]
    fn an_upgrade_must_be_newer_unless_named_with_to() {
        let order = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.2.0",
            "1.10.0",
            "2.0.0",
        ];
        for pair in order.windows(2) {
            let (older, newer) = (pair[0], pair[1]);
            assert!(
                check_upgrade("s", older, newer, false).is_ok(),
                "{older} -> {newer}"
            );
            let err = check_upgrade("s", newer, older, false).unwrap_err();
            assert!(matches!(err, McpError::NotNewer { .. }), "{err:?}");
            assert!(
                err.to_string().contains("is not newer"),
                "{newer} -> {older}: {err}"
            );
            assert!(
                check_upgrade("s", newer, older, true).is_ok(),
                "--to {older}"
            );
        }
        // Build metadata does not count; the same version is not newer.
        for (pinned, offered) in [("0.9.0+build.7", "0.9.0+build.8"), ("1.2.0", "1.2.0")] {
            let err = check_upgrade("notes", pinned, offered, false).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains(&format!("server `notes`: it is pinned to {pinned}")),
                "{msg}"
            );
            assert!(
                msg.contains(&format!("the registry's {offered} is not newer")),
                "{msg}"
            );
            assert!(
                msg.contains(&format!("`fl mcp upgrade notes --to {offered}`")),
                "{msg}"
            );
        }
        for (pinned, offered) in [
            ("1.0.0", "2026.10"),
            ("2026.10", "1.0.0"),
            ("1.0.0", "1.0.0.1"),
            ("1.0.0", "1.0.0-"),
            ("1.0.0", "1.0.0-rc..1"),
            ("1.0.0", "1.+1.0"),
            ("1.0.0", "v2.0.0"),
        ] {
            let err = check_upgrade("notes", pinned, offered, false).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("cannot be compared with it"),
                "{pinned} -> {offered}: {msg}"
            );
            assert!(
                msg.contains(&format!("`fl mcp upgrade notes --to {offered}`")),
                "{msg}"
            );
            assert!(check_upgrade("notes", pinned, offered, true).is_ok());
        }
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-mcp --lib freeze::tests::`
Expected: FAIL to compile (113 errors) — `error[E0425]`/`error[E0433]` cannot find function `freeze`, `diff` and `check_upgrade`, and cannot find type `Route`, `FreezeOptions`, `Change`, `ServerResponse`, `Server`, `EnvValue`, `HeaderValue`, `Transport`, `BTreeMap` and `McpError` (`use super::*` brings in nothing yet).

- [ ] **Step 3: Implement**

In `crates/mcp/src/lib.rs`, inside `pub enum McpError`, after the `NotFound { … },` variant, add:

```rust
    /// A registry entry fl cannot freeze into the catalog honestly (MCP spec
    /// §3.2). `from` and `version` are the registry's; `next` says what to
    /// do instead.
    #[error("`{from}` {version} cannot be frozen into the catalog: {problem}. {next}")]
    Unfreezable {
        from: String,
        version: String,
        problem: String,
        next: String,
    },
    /// `upgrade` to a version that is not newer than the pinned one, without
    /// `--to` (MCP spec §3.3).
    #[error("server `{name}`: {problem}. {next}")]
    NotNewer {
        name: String,
        problem: String,
        next: String,
    },
```

In `crates/mcp/src/registry.rs`, replace `fn printable(text: &str) -> String {` with:

```rust
pub(crate) fn printable(text: &str) -> String {
```

In `crates/mcp/src/freeze.rs`, between the module comment and `#[cfg(test)]` (a blank line before `#[cfg(test)]`), insert:

```rust
use crate::McpError;
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, is_env_name};
use crate::registry::{
    Argument, ArgumentKind, Input, KeyValueInput, Package, RegistryType, Remote, ServerJson,
    ServerResponse, Status, TransportKind, printable,
};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A launch route, as `add` names it: `--package npm|pypi|oci` or `--remote`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Npm,
    Pypi,
    Oci,
    Remote,
}

impl Route {
    /// The flag that chooses it.
    pub fn flag(self) -> &'static str {
        match self {
            Route::Npm => "--package npm",
            Route::Pypi => "--package pypi",
            Route::Oci => "--package oci",
            Route::Remote => "--remote",
        }
    }

    /// The route a frozen entry was taken from, read from its command or its
    /// transport; `None` for a command fl does not freeze to.
    pub fn of(server: &Server) -> Option<Route> {
        match server.transport {
            Transport::Http | Transport::Sse => Some(Route::Remote),
            Transport::Stdio => match server.command.as_deref() {
                Some("npx") => Some(Route::Npm),
                Some("uvx") => Some(Route::Pypi),
                Some("docker") => Some(Route::Oci),
                _ => None,
            },
        }
    }
}

/// What `add --from` (or `upgrade`) asks of the freeze.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreezeOptions {
    /// The catalog name: it names the variables fl derives for secret
    /// headers, and the commands a refusal suggests.
    pub name: String,
    /// `None`: the only route the entry offers.
    pub route: Option<Route>,
    /// `--env NAME=value`: a value for a non-secret environment variable,
    /// recorded as a literal. Each must name one of the package's.
    pub env: BTreeMap<String, String>,
    /// `--with NAME`: an optional argument or environment variable that
    /// needs a secret, included. Each must name one.
    pub with: BTreeSet<String>,
    /// From the pinned entry, on `upgrade`: its literal values and the
    /// secrets it included, used where they still apply and ignored where
    /// they do not. `env` and `with` win over them.
    pub kept_env: BTreeMap<String, String>,
    pub kept_with: BTreeSet<String>,
}

impl FreezeOptions {
    /// The options `upgrade` freezes the new version with: the pinned entry's
    /// route, literal values and included secrets.
    pub fn upgrading(name: &str, pinned: &Server) -> FreezeOptions {
        let env = pinned.env.iter().flatten();
        let kept_env = env
            .clone()
            .filter_map(|(k, v)| match v {
                EnvValue::Literal(value) => Some((k.clone(), value.clone())),
                EnvValue::Secret { .. } => None,
            })
            .collect();
        // An included `-e NAME={var}` became a secret `NAME`; an included
        // optional variable is a secret `NAME` too.
        let kept_with = env
            .filter(|(_, v)| matches!(v, EnvValue::Secret { .. }))
            .map(|(k, _)| k.clone())
            .collect();
        FreezeOptions {
            name: name.to_string(),
            route: Route::of(pinned),
            kept_env,
            kept_with,
            ..FreezeOptions::default()
        }
    }
}

/// A frozen entry, and what `add` prints about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frozen {
    pub server: Server,
    /// A deprecated status, and every literal value the entry records: each
    /// is committed with the catalog, public if the repository is (MCP spec
    /// §6).
    pub warnings: Vec<String>,
    /// The variables a person sets before a session starts the server,
    /// sorted.
    pub secrets: Vec<String>,
    /// Optional arguments left out, and how to include each.
    pub notes: Vec<String>,
}

/// Freezes `resp` into a catalog entry by the route `opts` names, or by the
/// only route it offers. Refused: [`McpError::Unfreezable`], naming what to
/// do instead. No secret value enters the entry or a message.
pub fn freeze(resp: &ServerResponse, opts: &FreezeOptions) -> Result<Frozen, McpError> {
    freeze_entry(resp, opts).map_err(|r| McpError::Unfreezable {
        from: printable(&resp.server.name),
        version: printable(&resp.server.version),
        problem: r.problem,
        next: r.next,
    })
}

/// Why an entry cannot be frozen, and what to do instead.
struct Refusal {
    problem: String,
    next: String,
}

fn refuse<T>(problem: impl Into<String>, next: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal {
        problem: problem.into(),
        next: next.into(),
    })
}

fn by_hand(name: &str) -> String {
    format!("Add it by hand with `fl mcp add {name} -- <command>`")
}

fn by_hand_url(name: &str) -> String {
    format!("Add it by hand with `fl mcp add {name} --url <url>`")
}

fn freeze_entry(resp: &ServerResponse, opts: &FreezeOptions) -> Result<Frozen, Refusal> {
    let s = &resp.server;
    let n = &opts.name;
    let mut warnings = Vec::new();
    let said = || match &resp.meta.status_message {
        Some(m) => printable(m),
        None => "no reason given".to_string(),
    };
    match resp.meta.status {
        Status::Deleted => {
            return refuse(
                format!("it is deleted in the registry ({})", said()),
                "Choose another server: `fl mcp search <text>` lists them",
            );
        }
        Status::Deprecated => warnings.push(format!(
            "`{}` {} is deprecated in the registry ({})",
            printable(&s.name),
            printable(&s.version),
            said()
        )),
        Status::Active => {}
    }
    if !is_exact(&s.version) {
        return refuse(
            format!(
                "its version `{}` is not an exact version",
                printable(&s.version)
            ),
            format!(
                "The registry gives no version to pin; add the server by hand with \
                 `fl mcp add {n} -- <command>`"
            ),
        );
    }
    let mut b = Build {
        opts,
        env: BTreeMap::new(),
        env_names: BTreeSet::new(),
        with_keys: BTreeSet::new(),
        notes: Vec::new(),
    };
    let launch = match choose(s, opts)? {
        Offer::Package(p) => b.package(p)?,
        Offer::Remote(r) => b.remote(r)?,
    };
    for k in opts.env.keys() {
        if !b.env_names.contains(k) {
            return refuse(
                format!("`--env {k}=…` names no environment variable of this launch route"),
                "Drop it, or check its name against the registry entry",
            );
        }
    }
    for k in &opts.with {
        if !b.with_keys.contains(k) {
            return refuse(
                format!("`--with {k}` names no optional argument or variable that needs a secret"),
                "Drop it, or check its name against the registry entry",
            );
        }
    }
    let server = Server {
        from: Some(s.name.clone()),
        version: Some(s.version.clone()),
        enabled: true,
        vendors: None,
        env: (!b.env.is_empty()).then_some(b.env),
        ..launch
    };
    let secret_headers: Vec<&String> = (server.headers.iter().flatten())
        .filter_map(|(_, v)| match v {
            HeaderValue::Secret { env, .. } => Some(env),
            HeaderValue::Literal(_) => None,
        })
        .collect();
    let names = server.env.iter().flatten().map(|(k, _)| k);
    if let Some(bad) = names
        .chain(secret_headers.iter().copied())
        .find(|v| !is_env_name(v))
    {
        return refuse(
            format!(
                "`{}` cannot be an environment variable name",
                printable(bad)
            ),
            format!(
                "fl cannot pass it to the server; add the server by hand with \
                 `fl mcp add {n} -- <command>` or `fl mcp add {n} --url <url>`"
            ),
        );
    }
    let env_secrets = (server.env.iter().flatten()).filter_map(|(k, v)| v.secret_var(k));
    let secrets: BTreeSet<&str> = env_secrets
        .chain(secret_headers.iter().map(|e| e.as_str()))
        .collect();
    let secrets = secrets.into_iter().map(str::to_string).collect();
    warnings.extend(server.literal_values().into_iter().map(|field| {
        format!(
            "`{field}` is a literal value: it will be committed with the catalog, and is \
             public if the repository is"
        )
    }));
    Ok(Frozen {
        server,
        warnings,
        secrets,
        notes: b.notes,
    })
}

/// A version fl may pin: not empty, and not the word `latest`.
fn is_exact(version: &str) -> bool {
    !version.is_empty() && version != "latest"
}

#[derive(Clone, Copy)]
enum Offer<'a> {
    Package(&'a Package),
    Remote(&'a Remote),
}

impl Offer<'_> {
    fn route(self) -> Option<Route> {
        match self {
            Offer::Package(p) => match p.registry_type {
                RegistryType::Npm => Some(Route::Npm),
                RegistryType::Pypi => Some(Route::Pypi),
                RegistryType::Oci => Some(Route::Oci),
                RegistryType::Other(_) => None,
            },
            Offer::Remote(_) => Some(Route::Remote),
        }
    }

    fn label(self) -> String {
        match (self, self.route()) {
            (Offer::Package(p), Some(route)) => {
                format!("`{}` ({})", route.flag(), printable(&p.identifier))
            }
            (Offer::Package(p), None) => format!(
                "a `{}` package, which fl cannot freeze",
                printable(p.registry_type.as_str())
            ),
            (Offer::Remote(r), _) => format!(
                "`--remote` ({}, {})",
                printable(r.kind.as_str()),
                printable(&r.url)
            ),
        }
    }
}

/// MCP spec §3.2 step 3: the route named, or the only one on offer.
fn choose<'a>(s: &'a ServerJson, opts: &FreezeOptions) -> Result<Offer<'a>, Refusal> {
    let n = &opts.name;
    let offers: Vec<Offer> = (s.packages.iter().map(Offer::Package))
        .chain(s.remotes.iter().map(Offer::Remote))
        .collect();
    let list = || {
        let labels: Vec<String> = offers.iter().map(|o| o.label()).collect();
        labels.join(", ")
    };
    if offers.is_empty() {
        return refuse(
            "it offers no launch route: no package and no remote",
            format!(
                "Add it by hand with `fl mcp add {n} -- <command>` or `fl mcp add {n} --url <url>`"
            ),
        );
    }
    let Some(route) = opts.route else {
        if let [only] = offers.as_slice() {
            return Ok(*only);
        }
        return refuse(
            format!("it offers more than one launch route: {}", list()),
            "Choose one with `--package <type>` or `--remote`",
        );
    };
    let mut hits = offers.iter().filter(|o| o.route() == Some(route));
    match (hits.next(), hits.next()) {
        (Some(only), None) => Ok(*only),
        (None, _) => refuse(
            format!("it offers no `{}` route, only {}", route.flag(), list()),
            "Choose one it offers",
        ),
        (Some(_), Some(_)) => refuse(
            format!(
                "it offers the `{}` route more than once: {}",
                route.flag(),
                list()
            ),
            match route {
                Route::Remote => by_hand_url(n),
                _ => by_hand(n),
            },
        ),
    }
}

/// The parts of an entry one route builds.
struct Build<'a> {
    opts: &'a FreezeOptions,
    env: BTreeMap<String, EnvValue>,
    /// The package's environment variables, which `--env` may name.
    env_names: BTreeSet<String>,
    /// The optional arguments that need a secret, which `--with` may name.
    with_keys: BTreeSet<String>,
    notes: Vec<String>,
}

impl Build<'_> {
    /// MCP spec §3.2 steps 4 and 6: a package fl can run as it is pinned,
    /// over stdio, or a refusal.
    fn package(&mut self, p: &Package) -> Result<Server, Refusal> {
        let n = &self.opts.name;
        let id = printable(&p.identifier);
        if let RegistryType::Other(kind) = &p.registry_type {
            return refuse(
                format!(
                    "it is a `{}` package; fl freezes npm, PyPI and OCI packages only",
                    printable(kind)
                ),
                by_hand(n),
            );
        }
        if p.transport.kind != TransportKind::Stdio {
            return refuse(
                format!(
                    "its package's transport is `{}`, not stdio: it serves HTTP on this \
                     machine, and fl does not start servers",
                    printable(p.transport.kind.as_str())
                ),
                format!("Start it yourself, then add it by hand with `fl mcp add {n} --url <url>`"),
            );
        }
        let docker = p.registry_type == RegistryType::Oci;
        let (command, mut args, pinned) = if docker {
            if let Err(problem) = image_pin(&p.identifier) {
                return refuse(
                    problem,
                    format!(
                        "Add it by hand with `fl mcp add {n} -- docker run -i --rm <image>`, \
                         naming an exact image"
                    ),
                );
            }
            let args = strings(&["run", "-i", "--rm"]);
            ("docker", args, p.identifier.clone())
        } else {
            let version = p.version.as_deref().unwrap_or_default();
            if !is_exact(version) {
                let gives = match version {
                    "" => "none".to_string(),
                    v => format!("`{}`", printable(v)),
                };
                return refuse(
                    format!("its package `{id}` names no exact version (it gives {gives})"),
                    by_hand(n),
                );
            }
            match p.registry_type {
                RegistryType::Npm => {
                    let pinned = format!("{}@{version}", p.identifier);
                    ("npx", strings(&["-y"]), pinned)
                }
                _ => ("uvx", Vec::new(), format!("{}=={version}", p.identifier)),
            }
        };
        for a in &p.runtime_arguments {
            if docker && a.kind == ArgumentKind::Positional {
                return refuse(
                    format!(
                        "its OCI package has a positional runtime argument (`{}`), so fl \
                         cannot tell its `docker run` line from the one it would write",
                        label(a)
                    ),
                    by_hand(n),
                );
            }
            args.extend(self.argument(a, docker)?);
        }
        for v in &p.environment_variables {
            if self.variable(v)? && docker {
                args.extend(["-e".to_string(), v.name.clone()]);
            }
        }
        args.push(pinned);
        for a in &p.package_arguments {
            args.extend(self.argument(a, false)?);
        }
        Ok(Server {
            from: None,
            version: None,
            enabled: true,
            vendors: None,
            transport: Transport::Stdio,
            command: Some(command.to_string()),
            args: Some(args),
            env: None,
            url: None,
            headers: None,
        })
    }

    /// One argument as it is passed: a named one as `<name> <value>`, a named
    /// boolean as `<name>` or nothing, a positional as its value; or none. A
    /// secret only as docker's `-e NAME`, and an optional one only when named
    /// with `--with`: an agent passes an unset `${NAME}` on as text.
    fn argument(&mut self, a: &Argument, docker: bool) -> Result<Vec<String>, Refusal> {
        let n = &self.opts.name;
        let label = label(a);
        // Refused before anything else is read of it, optional or not: fl
        // cannot tell how such an argument is passed (the registry serves
        // types its own schema does not define).
        if let ArgumentKind::Other(kind) = &a.kind {
            return refuse(
                format!(
                    "its argument `{label}` is of type `{}`, an argument type fl does not know",
                    printable(kind)
                ),
                by_hand(n),
            );
        }
        let template = a.value.as_deref().or(a.default.as_deref());
        let secret = a.is_secret
            || template.is_some_and(|t| names_in(t, &a.variables).any(|(_, i)| i.is_secret));
        if secret {
            let shape = env_shape(a);
            let key = shape.clone().unwrap_or_else(|| label.clone());
            if !a.is_required {
                self.with_keys.insert(key.clone());
                if !self.opts.with.contains(&key) && !self.opts.kept_with.contains(&key) {
                    self.notes.push(format!(
                        "Left out `{label}`: it is optional and needs a secret. \
                         `--with {key}` includes it"
                    ));
                    return Ok(Vec::new());
                }
            }
            if docker && let Some(name) = shape {
                self.env
                    .insert(name.clone(), EnvValue::Secret { env: None });
                return Ok(vec![a.name.clone().unwrap_or_default(), name]);
            }
            return refuse(
                format!(
                    "its argument `{label}` holds a secret, which fl would have to commit; a \
                     secret in an argument is taken only as docker's `-e NAME={{var}}`"
                ),
                format!(
                    "Add it by hand with `fl mcp add {n} -- <command>`, passing the secret in an \
                     environment variable"
                ),
            );
        }
        let Some(template) = template else {
            if a.is_required {
                return refuse(
                    format!("its argument `{label}` is required and has no value"),
                    by_hand(n),
                );
            }
            return Ok(Vec::new());
        };
        let value = match resolve(template, &a.variables) {
            Ok(value) => value,
            Err(var) if a.is_required => {
                return refuse(
                    format!("its argument `{label}` names an unset variable, `{{{var}}}`"),
                    by_hand(n),
                );
            }
            Err(_) => return Ok(Vec::new()),
        };
        let name = a.name.clone().unwrap_or_default();
        Ok(match (&a.kind, a.format.as_deref(), value.as_str()) {
            (ArgumentKind::Positional, ..) => vec![value],
            (ArgumentKind::Named, Some("boolean"), "true") => vec![name],
            (ArgumentKind::Named, Some("boolean"), "false") => Vec::new(),
            (ArgumentKind::Named, ..) => vec![name, value],
            (ArgumentKind::Other(_), ..) => unreachable!("refused above"),
        })
    }

    /// One environment variable: a secret reference, or a literal from
    /// `--env`, the pinned entry, or the registry. Whether it is passed. An
    /// optional secret is left out unless named with `--with`, as an optional
    /// secret argument is: an agent passes an unset `${NAME}` on as text.
    fn variable(&mut self, v: &KeyValueInput) -> Result<bool, Refusal> {
        let name = &v.name;
        let shown = printable(name);
        self.env_names.insert(name.clone());
        let template = v.value.as_deref().or(v.default.as_deref());
        let secret = v.is_secret
            || template.is_some_and(|t| names_in(t, &v.variables).any(|(_, i)| i.is_secret));
        if secret {
            if self.opts.env.contains_key(name) {
                return refuse(
                    format!("`--env {shown}=…` names a secret, and a secret is never recorded"),
                    format!("Set {shown} in the environment instead"),
                );
            }
            if !v.is_required {
                self.with_keys.insert(name.clone());
                if !self.opts.with.contains(name) && !self.opts.kept_with.contains(name) {
                    self.notes.push(format!(
                        "Left out `{shown}`: it is optional and needs a secret. \
                         `--with {shown}` includes it"
                    ));
                    return Ok(false);
                }
            }
            self.env
                .insert(name.clone(), EnvValue::Secret { env: None });
            return Ok(true);
        }
        let given = (self.opts.env.get(name))
            .or(self.opts.kept_env.get(name))
            .cloned();
        let value = given.or_else(|| template.and_then(|t| resolve(t, &v.variables).ok()));
        match value {
            Some(value) => {
                self.env.insert(name.clone(), EnvValue::Literal(value));
                Ok(true)
            }
            None if v.is_required => refuse(
                format!("its environment variable `{shown}` needs a value"),
                format!(
                    "Give it with `--env {shown}=<value>`; the value is committed with the catalog"
                ),
            ),
            None => Ok(false),
        }
    }

    /// MCP spec §3.2 step 4, the remote route.
    fn remote(&mut self, r: &Remote) -> Result<Server, Refusal> {
        let n = &self.opts.name;
        let transport = match &r.kind {
            TransportKind::StreamableHttp => Transport::Http,
            TransportKind::Sse => Transport::Sse,
            other => {
                return refuse(
                    format!(
                        "its remote is of type `{}`, which fl does not know",
                        printable(other.as_str())
                    ),
                    by_hand_url(n),
                );
            }
        };
        for (var, input) in names_in(&r.url, &r.variables) {
            if input.is_secret {
                return refuse(
                    format!("the remote URL names `{{{var}}}`, which is a secret"),
                    by_hand_url(n),
                );
            }
        }
        let url = resolve(&r.url, &r.variables).or_else(|var| {
            refuse(
                format!(
                    "the remote URL names `{{{var}}}`, which has neither a value nor a default"
                ),
                by_hand_url(n),
            )
        })?;
        let mut headers = BTreeMap::new();
        for h in &r.headers {
            if let Some(value) = self.header(h)? {
                headers.insert(h.name.clone(), value);
            }
        }
        Ok(Server {
            from: None,
            version: None,
            enabled: true,
            vendors: None,
            transport,
            command: None,
            args: None,
            env: None,
            url: Some(url),
            headers: (!headers.is_empty()).then_some(headers),
        })
    }

    /// One header: a secret as a reference to `<SERVER>_<HEADER>`, or to
    /// `<SERVER>_<VAR>` for a value `{var}` or `<scheme> {var}`; a literal as
    /// it is; an optional header with no value left out.
    fn header(&mut self, h: &KeyValueInput) -> Result<Option<HeaderValue>, Refusal> {
        let n = &self.opts.name;
        let shown = printable(&h.name);
        let next = format!("Add it by hand with `fl mcp add {n} --url <url> --header {shown}`");
        let template = h.value.as_deref().or(h.default.as_deref());
        let secret = h.is_secret
            || template.is_some_and(|t| names_in(t, &h.variables).any(|(_, i)| i.is_secret));
        if secret {
            let Some(template) = template else {
                let env = derived(n, &h.name);
                return Ok(Some(HeaderValue::Secret { env, scheme: None }));
            };
            let (scheme, rest) = match template.rsplit_once(' ') {
                Some((scheme, rest)) => (Some(scheme), rest),
                None => (None, template),
            };
            let var = rest.strip_prefix('{').and_then(|r| r.strip_suffix('}'));
            if let Some(var) = var
                && h.variables.contains_key(var)
                && scheme.is_none_or(is_scheme)
            {
                return Ok(Some(HeaderValue::Secret {
                    env: derived(n, var),
                    scheme: scheme.map(str::to_string),
                }));
            }
            return refuse(
                format!(
                    "its secret header `{shown}` has the value `{}`, a shape fl cannot record",
                    printable(template)
                ),
                next,
            );
        }
        match template.map(|t| resolve(t, &h.variables)) {
            Some(Ok(value)) => Ok(Some(HeaderValue::Literal(value))),
            _ if h.is_required => refuse(
                format!("its header `{shown}` is required, and the registry gives it no value"),
                next,
            ),
            _ => Ok(None),
        }
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// An argument as a message shows it: a named one's flag and template, a
/// positional's hint or template.
fn label(a: &Argument) -> String {
    let template = a.value.as_deref().or(a.default.as_deref());
    let text = match (&a.kind, &a.name, template) {
        (ArgumentKind::Positional, ..) | (_, None, _) => (a.value_hint.as_deref().or(template))
            .unwrap_or("a positional argument")
            .to_string(),
        (_, Some(name), Some(t)) => format!("{name} {t}"),
        (_, Some(name), None) => name.clone(),
    };
    printable(&text)
}

/// docker's `-e NAME={var}` (or `--env`): the `NAME` it sets.
fn env_shape(a: &Argument) -> Option<String> {
    if a.kind != ArgumentKind::Named || !matches!(a.name.as_deref(), Some("-e" | "--env")) {
        return None;
    }
    let template = a.value.as_deref().or(a.default.as_deref())?;
    let (name, rest) = template.split_once('=')?;
    let var = rest.strip_prefix('{')?.strip_suffix('}')?;
    (is_env_name(name) && a.variables.contains_key(var)).then(|| name.to_string())
}

/// An OCI identifier pins exactly when its last path segment carries a tag
/// other than `latest`, or a digest (`@sha256:…`, whose colon reads as a tag
/// here, which is as good). A registry's port is in an earlier segment.
fn image_pin(id: &str) -> Result<(), String> {
    let last = id.rsplit('/').next().unwrap_or(id);
    let id = printable(id);
    match last.split_once(':') {
        None => Err(format!(
            "its image `{id}` has neither a tag nor a digest, so the pin would not be exact"
        )),
        Some((_, "latest")) => Err(format!(
            "its image `{id}` is tagged `latest`, which moves, so the pin would not be exact"
        )),
        Some(_) => Ok(()),
    }
}

/// An HTTP authentication scheme, such as `Bearer`: one token.
fn is_scheme(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `<SERVER>_<PART>`, uppercased, `-` as `_`.
fn derived(server: &str, part: &str) -> String {
    format!("{server}_{part}")
        .to_ascii_uppercase()
        .replace('-', "_")
}

/// Each `{name}` in `template` that `variables` defines, in order. A `{name}`
/// it does not define is text, as the registry's schema says.
fn names_in<'a>(
    template: &'a str,
    variables: &'a BTreeMap<String, Input>,
) -> impl Iterator<Item = (&'a str, &'a Input)> {
    template.split('{').skip(1).filter_map(move |part| {
        let name = part.split_once('}')?.0;
        variables.get_key_value(name).map(|(k, i)| (k.as_str(), i))
    })
}

/// `template` with each variable it names replaced by the variable's value,
/// else its default. `Err`: the first variable with neither.
fn resolve(template: &str, variables: &BTreeMap<String, Input>) -> Result<String, String> {
    let mut out = template.to_string();
    for (name, input) in names_in(template, variables) {
        let Some(value) = input.value.as_deref().or(input.default.as_deref()) else {
            return Err(printable(name));
        };
        out = out.replace(&format!("{{{name}}}"), value);
    }
    Ok(out)
}

/// One field of a launch spec that `upgrade` would change (MCP spec §3.3).
/// `old` and `new` are shown as in the catalog; `None` is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub field: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |v: &Option<String>| v.clone().unwrap_or_else(|| "(none)".to_string());
        write!(
            f,
            "{}: {} -> {}",
            self.field,
            show(&self.old),
            show(&self.new)
        )
    }
}

/// Every field of the launch spec that differs between `old` and `new`, in
/// the catalog's order: `from`, `version`, `transport`, `command`, `args`,
/// each `env.<NAME>`, `url`, each `headers.<NAME>`. A person's own choices,
/// `enabled` and `vendors`, are not part of it.
pub fn diff(old: &Server, new: &Server) -> Vec<Change> {
    let mut out = Vec::new();
    let mut field = |name: String, a: Option<String>, b: Option<String>| {
        if a != b {
            out.push(Change {
                field: name,
                old: a,
                new: b,
            });
        }
    };
    let text = |s: &Option<String>| s.as_ref().map(|s| format!("{s:?}"));
    field("from".into(), text(&old.from), text(&new.from));
    field("version".into(), text(&old.version), text(&new.version));
    let transport = |s: &Server| Some(format!("{:?}", s.transport.as_str()));
    field("transport".into(), transport(old), transport(new));
    field("command".into(), text(&old.command), text(&new.command));
    let args = |s: &Server| s.args.as_ref().map(|a| format!("{a:?}"));
    field("args".into(), args(old), args(new));
    let (a, b) = (
        old.env.clone().unwrap_or_default(),
        new.env.clone().unwrap_or_default(),
    );
    for k in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
        field(
            format!("env.{k}"),
            a.get(k).map(show_env),
            b.get(k).map(show_env),
        );
    }
    field("url".into(), text(&old.url), text(&new.url));
    let (a, b) = (
        old.headers.clone().unwrap_or_default(),
        new.headers.clone().unwrap_or_default(),
    );
    for k in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
        field(
            format!("headers.{k}"),
            a.get(k).map(show_header),
            b.get(k).map(show_header),
        );
    }
    out
}

fn show_env(v: &EnvValue) -> String {
    match v {
        EnvValue::Literal(s) => format!("{s:?}"),
        EnvValue::Secret { env: None } => "{ secret = true }".to_string(),
        EnvValue::Secret { env: Some(e) } => format!("{{ secret = true, env = {e:?} }}"),
    }
}

fn show_header(v: &HeaderValue) -> String {
    match v {
        HeaderValue::Literal(s) => format!("{s:?}"),
        HeaderValue::Secret { env, scheme: None } => format!("{{ secret = true, env = {env:?} }}"),
        HeaderValue::Secret {
            env,
            scheme: Some(s),
        } => format!("{{ secret = true, env = {env:?}, scheme = {s:?} }}"),
    }
}

/// `upgrade`'s rule (MCP spec §3.3): the registry's version must be newer
/// than the pinned one by semantic-version precedence, unless the person
/// named it with `--to` (`named`). A version that is not a semantic version
/// cannot be compared, and is refused unless named.
pub fn check_upgrade(name: &str, pinned: &str, offered: &str, named: bool) -> Result<(), McpError> {
    if named {
        return Ok(());
    }
    let (pinned, offered) = (printable(pinned), printable(offered));
    let problem = match (SemVer::parse(&pinned), SemVer::parse(&offered)) {
        (Some(p), Some(o)) if o.precedence(&p).is_gt() => return Ok(()),
        (Some(_), Some(_)) => {
            format!("it is pinned to {pinned}, and the registry's {offered} is not newer")
        }
        _ => format!(
            "it is pinned to {pinned}, and the registry's {offered} cannot be compared with it \
             as a semantic version"
        ),
    };
    Err(McpError::NotNewer {
        name: name.to_string(),
        problem,
        next: format!("`fl mcp upgrade {name} --to {offered}` moves it there anyway"),
    })
}

/// A semantic version, for its precedence: the core, then the pre-release
/// identifiers; build metadata does not count.
struct SemVer<'a> {
    core: [u64; 3],
    pre: Vec<&'a str>,
}

impl<'a> SemVer<'a> {
    fn parse(v: &'a str) -> Option<SemVer<'a>> {
        let v = v.split_once('+').map_or(v, |(v, _)| v);
        let (core, pre): (&str, Vec<&str>) = match v.split_once('-') {
            Some((core, pre)) => (core, pre.split('.').collect()),
            None => (v, Vec::new()),
        };
        if pre.iter().any(|p| p.is_empty()) {
            return None;
        }
        let mut parts = core.split('.').map(number);
        let core = [parts.next()??, parts.next()??, parts.next()??];
        parts.next().is_none().then_some(SemVer { core, pre })
    }

    fn precedence(&self, other: &SemVer) -> Ordering {
        let pre = match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                let pairs = self.pre.iter().zip(&other.pre);
                let first = pairs.map(|(a, b)| identifier(a, b)).find(|o| o.is_ne());
                first.unwrap_or_else(|| self.pre.len().cmp(&other.pre.len()))
            }
        };
        self.core.cmp(&other.core).then(pre)
    }
}

/// A numeric part. Rust's parse also takes a leading `+`, but none reaches
/// here: a `+` starts the build metadata, which is cut off first.
fn number(s: &str) -> Option<u64> {
    s.parse().ok()
}

/// Pre-release identifiers: numbers by value and below words, words by
/// their ASCII order.
fn identifier(a: &str, b: &str) -> Ordering {
    match (number(a), number(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-mcp --lib freeze::tests::`
Expected: PASS — 16 passed: `an_npm_package_freezes_to_npx_at_its_exact_version`, `a_pypi_package_freezes_to_uvx_at_its_exact_version`, `an_oci_package_freezes_to_docker_run_with_the_image_as_given`, `an_optional_secret_argument_is_left_out_unless_named_with_with`, `an_optional_secret_variable_is_left_out_unless_named_with_with`, `a_remote_freezes_to_its_url_and_a_secret_header_to_a_derived_variable`, `arguments_render_by_kind_format_and_variables`, `a_required_variable_takes_its_value_from_env_and_is_warned_as_committed`, `a_route_is_chosen_with_package_or_remote`, `a_deleted_server_is_refused_and_a_deprecated_one_warned_about`, `the_frozen_entry_records_the_registrys_version_never_latest`, `each_unfreezable_entry_is_refused_naming_the_way_on`, `an_argument_of_a_type_fl_does_not_know_is_refused_naming_it`, `upgrade_keeps_the_pinned_choices_where_they_still_apply`, `diff_lists_each_changed_field_of_the_launch_spec`, `an_upgrade_must_be_newer_unless_named_with_to`. `cargo test -p fl-mcp --lib` passes 51 (Tasks 1 and 3's 35 with them).

- [ ] **Step 5: Mutation checks**

Each filter is `cargo test -p fl-mcp --lib freeze::tests::<name>`; "each" is `each_unfreezable_entry_is_refused_naming_the_way_on`, "unknown" is `an_argument_of_a_type_fl_does_not_know_is_refused_naming_it`, "npm" is `an_npm_package_freezes_to_npx_at_its_exact_version`, "pypi" is `a_pypi_package_freezes_to_uvx_at_its_exact_version`, "oci" is `an_oci_package_freezes_to_docker_run_with_the_image_as_given`, "with" is `an_optional_secret_argument_is_left_out_unless_named_with_with`, "variable" is `an_optional_secret_variable_is_left_out_unless_named_with_with`, "remote" is `a_remote_freezes_to_its_url_and_a_secret_header_to_a_derived_variable`, "args" is `arguments_render_by_kind_format_and_variables`, "env" is `a_required_variable_takes_its_value_from_env_and_is_warned_as_committed`, "route" is `a_route_is_chosen_with_package_or_remote`, "status" is `a_deleted_server_is_refused_and_a_deprecated_one_warned_about`, "latest" is `the_frozen_entry_records_the_registrys_version_never_latest`, "upgrade" is `upgrade_keeps_the_pinned_choices_where_they_still_apply`, "diff" is `diff_lists_each_changed_field_of_the_launch_spec`, "newer" is `an_upgrade_must_be_newer_unless_named_with_to`. Each mutation is one edit of `crates/mcp/src/freeze.rs`; save a copy first, restore it after each, and `cmp` against the copy. None of these goes red by failing to compile.

1. A deleted server is refused: the `Status::Deleted` arm guarded with `if false`, and `Status::Active => {}` made `_ => {}` → status red.
2. A deprecated server is warned about: `Status::Deprecated => warnings.push(format!(` → `Status::Deprecated => drop(format!(` → status red.
3. The status message is shown: `Some(m) => printable(m),` → `Some(_) => "no reason given".to_string(),` → status red.
4. The registry's version must be exact: `if !is_exact(&s.version) {` → `if false {` → each red.
5. `is_exact`'s first conjunct (not empty): `!version.is_empty() && version != "latest"` → `version != "latest"` → each red.
6. `is_exact`'s second conjunct (not `latest`): `!version.is_empty() && version != "latest"` → `!version.is_empty()` → each red.
7. `--env` must name a variable: `if !b.env_names.contains(k) {` → `if false {` → each red.
8. `--with` must name an optional argument: `if !b.with_keys.contains(k) {` → `if false {` → each red.
9. Every variable fl records must be a variable name: `.find(|v| !is_env_name(v))` → `.find(|_| false)` → each red.
10. The variables secret headers read are checked too (`9DOCS_AUTHORIZATION`): `.chain(secret_headers.iter().copied())` → `.chain(Vec::<&String>::new())` → remote red.
11. They are listed in `secrets`: `.chain(secret_headers.iter().map(|e| e.as_str()))` → `.chain(Vec::<&str>::new())` → remote red.
12. Each literal value is warned about: `warnings.extend(server.literal_values().into_iter()` → `warnings.extend(Vec::<String>::new().into_iter()` → npm red.
13. No route at all: `if offers.is_empty() {` → `if false {` → each red.
14. The only route is taken, and only when it is the only one: `if let [only] = offers.as_slice() {` → `if let [only, ..] = offers.as_slice() {` → each red.
15. A named route is chosen by its kind: `.filter(|o| o.route() == Some(route))` → `.filter(|_| true)` → route red.
16. A route offered twice is refused: `match (hits.next(), hits.next()) {` → `match (hits.next(), None::<&Offer>) {` → each red.
17. A package type fl does not run is no route (the listing would name the `mcpb` package `--package npm`): `RegistryType::Other(_) => None,` → `RegistryType::Other(_) => Some(Route::Npm),` → each red.
18. A package type fl does not run is refused: `if let RegistryType::Other(kind) = &p.registry_type {` → `if let RegistryType::Other(kind) = &RegistryType::Npm {` → each red.
19. A package that is not stdio is refused: `if p.transport.kind != TransportKind::Stdio {` → `if false {` → each red.
20. OCI runs with docker (without it the OCI package falls to the npm/PyPI branch and has no version): `let docker = p.registry_type == RegistryType::Oci;` → `let docker = false;` → oci red.
21. The image must pin exactly: `if let Err(problem) = image_pin(&p.identifier) {` → `if let Err(problem) = Ok::<(), String>(()) {` → each red.
22. Only the last path segment carries a tag (a registry's port is not one): `let last = id.rsplit('/').next().unwrap_or(id);` → `let last = id;` → each red.
23. `latest` is not a pin: `Some((_, "latest")) => Err(format!(` → `Some((_, "never")) => Err(format!(` → each red.
24. An npm or PyPI package must name an exact version: `if !is_exact(version) {` → `if false {` → each red.
25. npm pins `@<version>`: `let pinned = format!("{}@{version}", p.identifier);` → `let pinned = p.identifier.clone();` → npm red.
26. npm runs `npx -y`: `("npx", strings(&["-y"]), pinned)` → `("npx", Vec::new(), pinned)` → npm red.
27. PyPI pins `==<version>`: `format!("{}=={version}", p.identifier)` → `format!("{}@{version}", p.identifier)` → pypi red.
28. OCI runs `docker run -i --rm`: `strings(&["run", "-i", "--rm"])` → `strings(&["run"])` → oci red.
29. A positional runtime argument is refused for OCI: `if docker && a.kind == ArgumentKind::Positional {` → `if false {` → each red.
30. …for OCI only (npm's `--no-update-notifier` is kept): `if docker && a.kind == ArgumentKind::Positional {` → `if a.kind == ArgumentKind::Positional {` → args red.
31. A docker `-e` is taken only among docker's own runtime arguments: `args.extend(self.argument(a, docker)?);` → `args.extend(self.argument(a, true)?);` → args red.
32. `-e NAME` per variable for docker only: `if self.variable(v)? && docker {` → `if self.variable(v)? {` → npm red.
33. …and only for a variable that is passed (`SAMPLE_DEBUG` has no value): `if self.variable(v)? && docker {` → `if self.variable(v).map(|_| true)? && docker {` → oci red.
34. Package arguments come after the identifier: move `args.push(pinned);` after the `for a in &p.package_arguments` loop → npm red.
35. A package argument is never docker's `-e`: `args.extend(self.argument(a, false)?);` → `args.extend(self.argument(a, docker)?);` → oci red.
36. An argument marked secret holds a secret (`--api-key`): in `argument`, `let secret = a.is_secret ||` → `let secret = false ||` → each red.
37. An argument naming a secret variable holds a secret (`--token {token}`): in `argument`, drop the `|| template.is_some_and(…)` disjunct → each red.
38. An optional secret argument is left out: `if !a.is_required {` → `if false {` → with red.
39. …unless `--with` names it: `if !self.opts.with.contains(&key) && !self.opts.kept_with.contains(&key) {` → `if !self.opts.kept_with.contains(&key) {` → with red.
40. …or the pinned entry had it: `if !self.opts.with.contains(&key) && !self.opts.kept_with.contains(&key) {` → `if !self.opts.with.contains(&key) {` → upgrade red.
41. `--with` may name it: delete `self.with_keys.insert(key.clone());` → with red.
42. The secret `-e` becomes `-e NAME` for docker only: `if docker && let Some(name) = shape {` → `if let Some(name) = shape {` → args red.
43. The shape is docker's `-e` or `--env` (`--label SAMPLE_TOKEN={token}` is not): `if a.kind != ArgumentKind::Named || !matches!(a.name.as_deref(), Some("-e" | "--env")) {` → `if a.kind != ArgumentKind::Named {` → oci red.
44. `NAME` must be a variable name (`-e sample-token={token}` is refused for that, not for the shape): `(is_env_name(name) && a.variables.contains_key(var)).then(|| name.to_string())` → `a.variables.contains_key(var).then(|| name.to_string())` → oci red.
45. `{var}` must be defined (`-e SAMPLE_TOKEN={nope}`, marked secret): `(is_env_name(name) && a.variables.contains_key(var)).then(|| name.to_string())` → `is_env_name(name).then(|| name.to_string())` → oci red.
46. The value after `=` is exactly `{var}` (`SAMPLE_TOKEN=x{token}`): in `env_shape`, `rest.strip_prefix('{')?` → `rest.trim_start_matches(|c| c != '{').strip_prefix('{')?` → oci red.
47. A required argument with no value is refused: the `if a.is_required {` under `let Some(template) = template else` made `if false {` → each red.
48. …an optional one is left out: the same `if a.is_required {` made `if true {` (`--color`) → args red.
49. A required argument naming an unset variable is refused: `Err(var) if a.is_required => {` → `Err(var) if false => {` → each red.
50. …an optional one is left out (`--region`): `Err(var) if a.is_required => {` → `Err(var) if true => {` → args red.
51. A named boolean `true` is its name alone: `(ArgumentKind::Named, Some("boolean"), "true") => vec![name],` → `(ArgumentKind::Named, Some("boolean"), "true") => vec![name, "true".into()],` → args red.
52. A named boolean `false` is nothing: `(ArgumentKind::Named, Some("boolean"), "false") => Vec::new(),` → `(ArgumentKind::Named, Some("boolean"), "false") => vec![name],` → args red.
53. A named argument is two arguments: `(ArgumentKind::Named, ..) => vec![name, value],` → `(ArgumentKind::Named, ..) => vec![format!("{name}={value}")],` → args red.
54. A positional is its value: `(ArgumentKind::Positional, ..) => vec![value],` → `(ArgumentKind::Positional, ..) => vec![name, value],` → args red.
55. An argument's `value` comes before its `default` (`--level debug`): in `argument`, `let template = a.value.as_deref().or(a.default.as_deref());` → the two swapped → args red.
56. A variable's `value` comes before its `default` (`--mode fast`): `let Some(value) = input.value.as_deref().or(input.default.as_deref()) else {` → `let Some(value) = input.default.as_deref().or(input.value.as_deref()) else {` → args red.
57. A variable marked secret is a reference: in `variable`, `let secret = v.is_secret ||` → `let secret = false ||` → npm red.
58. A variable whose value names a secret is a reference (`SAMPLE_AUTH`): in `variable`, drop the `|| template.is_some_and(…)` disjunct → env red.
59. `--env` for a secret is refused: `if self.opts.env.contains_key(name) {` → `if false {` → each red.
60. `--env` wins over the pinned value: `(self.opts.env.get(name)).or(self.opts.kept_env.get(name))` → the two swapped → upgrade red.
61. The pinned value is used: `(self.opts.env.get(name)).or(self.opts.kept_env.get(name))` → `(self.opts.env.get(name))` → upgrade red.
62. A given value wins over the registry's (`SAMPLE_LEVEL`): `let value = given.or_else(|| template.and_then(|t| resolve(t, &v.variables).ok()));` → `let value = template.and_then(|t| resolve(t, &v.variables).ok()).or(given);` → env red.
63. A required variable with no value is refused: `None if v.is_required => refuse(` → `None if false => refuse(` → each red.
64. An optional one is left out (`SAMPLE_DEBUG`): `None if v.is_required => refuse(` → `None if true => refuse(` → env red.
65. `--env` may name the package's variables: delete `self.env_names.insert(name.clone());` → env red.
66. `streamable-http` is `http`: `TransportKind::StreamableHttp => Transport::Http,` → `TransportKind::StreamableHttp => Transport::Sse,` → remote red.
67. `sse` is `sse`: `TransportKind::Sse => Transport::Sse,` → `TransportKind::Sse => Transport::Http,` → remote red.
68. A secret in the remote URL is refused: `if input.is_secret {` → `if false {` → each red.
69. The remote URL is resolved, and an unset variable refused: `let url = resolve(&r.url, &r.variables).or_else(|var| {` → `let url = Ok::<String, String>(r.url.clone()).or_else(|var| {` → each red.
70. A secret header with no value reads `<SERVER>_<HEADER>`: `let env = derived(n, &h.name);` → `let env = derived(n, "token");` → remote red.
71. …with `-` as `_` (`TEAM_DOCS_AUTHORIZATION`): in `derived`, delete `.replace('-', "_")` → remote red.
72. …uppercased: in `derived`, delete `.to_ascii_uppercase()` → remote red.
73. A scheme must be one token (`Bearer x {token}`): delete `&& scheme.is_none_or(is_scheme)` → remote red.
74. …not empty (` {token}`): `!s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')` → `s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')` → remote red.
75. …of letters, digits and `-` (`Bearer x`): `!s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')` → `!s.is_empty()` → remote red.
76. The header's variable must be defined (`Bearer {nope}`): delete `&& h.variables.contains_key(var)` → remote red.
77. A header marked secret is a reference (`docs`): in `header`, `let secret = h.is_secret ||` → `let secret = false ||` → remote red.
78. A header whose value names a secret is a reference (`Bearer {token}`): in `header`, drop the `|| template.is_some_and(…)` disjunct → remote red.
79. A required header with no value is refused: `_ if h.is_required => refuse(` → `_ if false => refuse(` → each red.
80. An optional one is left out (`X-Trace`): `_ if h.is_required => refuse(` → `_ if true => refuse(` → remote red.
81. The difference lists changed fields only: `if a != b {` → `if true {` → diff red.
82. …including `from`: delete `field("from".into(), text(&old.from), text(&new.from));` → diff red.
83. `--to` lifts the rule: `if named {` → `if false {` → newer red.
84. The new version must be newer, not equal: `if o.precedence(&p).is_gt()` → `if o.precedence(&p).is_ge()` → newer red.
85. A release is above its pre-releases: `(true, false) => Ordering::Greater,` → `(true, false) => Ordering::Less,` → newer red.
86. …and a pre-release below its release: `(false, true) => Ordering::Less,` → `(false, true) => Ordering::Greater,` → newer red.
87. A numeric identifier is below a word: `(Some(_), None) => Ordering::Less,` → `(Some(_), None) => Ordering::Greater,` → newer red.
88. Numeric identifiers compare by value (`beta.2` < `beta.11`): `(Some(x), Some(y)) => x.cmp(&y),` → `(Some(_), Some(_)) => a.cmp(b),` → newer red.
89. A longer set of identifiers is above its prefix (`alpha` < `alpha.1`): `first.unwrap_or_else(|| self.pre.len().cmp(&other.pre.len()))` → `first.unwrap_or(Ordering::Equal)` → newer red.
90. Build metadata does not count: delete `let v = v.split_once('+').map_or(v, |(v, _)| v);` → newer red.
91. An empty identifier is not a version (`1.0.0-`): `if pre.iter().any(|p| p.is_empty()) {` → `if false {` → newer red.
92. Exactly three core parts (`1.0.0.1`): `parts.next().is_none().then_some(SemVer { core, pre })` → `Some(SemVer { core, pre })` → newer red.
93. `Route::of` reads docker as OCI: `Some("docker") => Some(Route::Oci),` → `Some("docker") => None,` → route red.
94. …and a remote as `--remote`: `Transport::Http | Transport::Sse => Some(Route::Remote),` → `Transport::Http | Transport::Sse => None,` → route red.
95. `upgrading` keeps the literal values: `EnvValue::Literal(value) => Some((k.clone(), value.clone())),` → `EnvValue::Literal(_) => None,` → upgrade red.
96. …and the included secrets: `.filter(|(_, v)| matches!(v, EnvValue::Secret { .. }))` → `.filter(|_| false)` → upgrade red.
97. …and the route (two are on offer): `route: Route::of(pinned),` → `route: None,` → upgrade red.
98. `from` is the registry's name: `from: Some(s.name.clone()),` → `from: None,` → npm red.
99. `version` is the registry's version: `version: Some(s.version.clone()),` → `version: Some("latest".to_string()),` → latest red.
100. No `env` table when there is none: `env: (!b.env.is_empty()).then_some(b.env),` → `env: Some(b.env),` → with red.
101. No `headers` table when there is none: `headers: (!headers.is_empty()).then_some(headers),` → `headers: Some(headers),` → route red.
102. A tag other than `latest` pins: `Some((_, "latest")) => Err(format!(` → `Some((_, "latest")) | Some(_) => Err(format!(` → oci red.
103. A variable's `value` comes before its `default` (`SAMPLE_MODE`): `let template = v.value.as_deref().or(v.default.as_deref());` → `let template = v.default.as_deref().or(v.value.as_deref());` → env red.
104. A header's `value` comes before its `default` (`X-Region`): `let template = h.value.as_deref().or(h.default.as_deref());` → `let template = h.default.as_deref().or(h.value.as_deref());` → remote red.
105. A variable's template is resolved (`SAMPLE_URL`): `given.or_else(|| template.and_then(|t| resolve(t, &v.variables).ok()))` → `given.or_else(|| template.map(str::to_string))` → env red.
106. A header's template is resolved (`X-Team`): `match template.map(|t| resolve(t, &h.variables)) {` → `match template.map(|t| Ok::<String, String>(t.to_string())) {` → remote red.
107. An optional secret variable is left out (`SAMPLE_EXTRA`): `if !v.is_required {` → `if false {` → variable red.
108. …unless `--with` names it: `if !self.opts.with.contains(name) && !self.opts.kept_with.contains(name) {` → `if !self.opts.kept_with.contains(name) {` → variable red.
109. …or the pinned entry had it: `if !self.opts.with.contains(name) && !self.opts.kept_with.contains(name) {` → `if !self.opts.with.contains(name) {` → variable red.
110. `--with` may name it: delete `self.with_keys.insert(name.clone());` → variable red.
111. An argument of a type fl does not know is refused: `if let ArgumentKind::Other(kind) = &a.kind {` → `if let ArgumentKind::Other(kind) = &a.kind && false {` → each red (the required one is refused as having no value), unknown red (the optional one is left out and the freeze succeeds).
112. …naming its type: `printable(kind)` in that refusal → `"?"` → each red, unknown red.
113. `label` names such an argument by its flag: `(ArgumentKind::Positional, ..) | (_, None, _) =>` → `(ArgumentKind::Positional | ArgumentKind::Other(_), ..) | (_, None, _) =>` → each red, unknown red (`a positional argument`).

Not observable:
- `env_shape`'s `a.kind != ArgumentKind::Named` conjunct: its answer is used only for docker's runtime arguments, where a positional is refused before it is read, and a positional has no `name` for the `-e` check to match anyway; an argument of a type fl does not know is refused before `env_shape` is called.
- The `unreachable!` arm for `ArgumentKind::Other` in `argument`'s last `match`: such an argument is refused at the top of `argument`, so no input reaches it.
- A digest needs no check of its own: `@sha256:…` puts a colon in the last path segment, which reads as a tag other than `latest`, so an explicit `@` test could not be told apart and is not written.
- `number` uses Rust's own parse, whose one extra (a leading `+`) cannot reach it: a `+` starts the build metadata, which is cut off first. A digits-only check there could not be told apart and is not written.
- The `"no reason given"` text for a status with no `statusMessage`, and the `"a positional argument"` label for a positional with neither a hint nor a value, are wording, not guards.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1249 passed, 19 ignored (1233 and 19 before this task).

- [ ] **Step 7: Commit**

```bash
git add crates/mcp/src/lib.rs crates/mcp/src/registry.rs crates/mcp/src/freeze.rs
git commit -m "feat(mcp): freeze a registry entry into the catalog, or refuse it

fl-mcp freezes one version of a registry server into a catalog entry
with no network: the route the person names with --package or --remote,
or the only one on offer; the version the registry returned, never the
word latest. npm runs as npx -y <identifier>@<version>, PyPI as uvx
<identifier>==<version>, OCI as docker run -i --rm with the runtime
arguments, one -e NAME per variable and the image as given. A named
argument is passed as name and value, a boolean as its name or not at
all, a positional as its value, with {var} templates resolved. A secret
becomes a reference: an environment variable by its name, a secret
header by a variable fl derives from the server's name, and a secret
inside an argument only as docker's -e NAME={var}. An optional argument
or variable that needs a secret is left out unless --with names it. Each
entry fl cannot freeze honestly is refused naming what to do instead: a
deleted server, several routes, an image with no exact tag, a package
that serves HTTP, a positional runtime argument, a package type fl does
not run, an argument of a type fl does not know, a secret elsewhere in
an argument, a required value nobody gave, a value given for a secret. For upgrade: the difference between
two launch specs, field by field, and a version that is not newer by
semantic-version precedence refused unless named with --to. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 5: The vendor files: one writer per vendor, foreign entries kept

Each agent CLI reads its own project MCP file (MCP spec §4.1): Claude Code `<root>/.mcp.json`, Codex `<root>/.codex/config.toml`, Antigravity `<root>/.agents/mcp_config.json` (owner decision 10, plan ruling 3). This task adds the module `vendor`: a `Vendor` trait with one implementation per vendor, each turning a catalog entry into its own shape (§4.2) or refusing the server for that vendor alone, naming the reason — "the other vendors still get it" (§4.2); and a file model, `VendorFile`, that reads a target's current bytes, gives each entry's own bytes by name, and gives the new bytes with entries added, replaced or removed and everything else kept (§4.3 "What is kept"). It does no I/O and never reads the environment: a secret is written as a reference or not at all (§2.1, §6), and a test reads a variable cargo sets and searches every rendered entry for its value, and searches the vendor sources for a read of the environment. Task 6 reads and writes the files, takes the lock and keeps the records.

The shapes: Claude Code writes a secret as `${NAME}` and always gives a remote entry its `type` (facts §2: a `url` without `type` is skipped). Codex has no `type`; a secret variable is forwarded by name (`env_vars`); a secret `Authorization` header with scheme `Bearer` is `bearer_token_env_var`, a secret header with no scheme is `env_http_headers`, and any other scheme is refused for Codex; SSE is refused (plan ruling 18, facts §3.3–§3.4). Antigravity leaves a secret variable out for the stdio server to inherit, refuses a server with a secret header, writes a remote's URL under `serverUrl`, and refuses SSE (owner decision 11, plan rulings 4 and 5). One refusal the rulings do not name, because the vendor cannot express it: a secret read from a variable whose name is not its key (`env.GITHUB_TOKEN = { secret = true, env = "GH_PAT" }`) is refused for Codex and Antigravity, which both hand the server the variable under its own name; Claude Code writes `"GITHUB_TOKEN": "${GH_PAT}"`.

The JSON file (plan rulings 1 and 2) is read into an `IndexMap<String, Box<RawValue>>` at the top level and again for `mcpServers`, never with `preserve_order`: every other top-level key and every entry fl does not touch is written back byte for byte in its place; fl's entries, and the layout around the entries, are written fresh at two spaces a level. A file that is not strict JSON — comments, trailing commas, a byte-order mark, bytes that are not UTF-8 — is refused, never rewritten. So is a file whose top level or `mcpServers` is not an object, or with a key twice (a map would keep one and lose the other). A missing file starts as `{"mcpServers": {}}`; a file without `mcpServers` gains it, last, only when fl writes an entry. Codex's file is edited with `toml_edit` (§4.3): a replaced entry keeps its place and the lines above its header, a new one goes last among the servers with a blank line above it, and the lines directly above an entry's header are the entry's and go with it when fl removes it. A refusal to read a TOML file shows the parser's message and line number but never quotes the line, which could hold a value. Neither file is deserialized into typed fields, so the parsers' messages are syntax only, and neither `toml_edit`'s nor `serde_json`'s syntax messages quote a value (checked 2026-10-08: each quotes only what it expected), so neither needs the catalog's redaction (plan ruling 36).

What Task 6 hashes: `Rendered::bytes()` is exactly what `VendorFile::entry(name)` gives back once the entry is written, so the SHA-256 of each tells whether the entry is still what fl wrote (§4.3). For JSON both are the entry's own bytes as they stand in the file (indented to their depth); for Codex both are the entry alone, `[mcp_servers.<name>]` and its body, without the lines above its header. A change to whitespace or a comment inside fl's entry is therefore a change. The catalog's `VendorName` (Task 1) names the vendors; this task adds no second enum for them.

**Blast radius:** `crates/mcp/src/lib.rs` gains one line, `pub mod vendor;`. No dependency changes: `serde_json` (`raw_value`), `indexmap` (`serde`) and `toml_edit` were declared by Task 1 and are first used here. No existing code changes.

**Files:**
- Modify: `crates/mcp/src/lib.rs` (`pub mod vendor`)
- Create: `crates/mcp/src/vendor/mod.rs` (`Vendor`, `vendor`, `Rendered`, `VendorRefusal`, `FileRefusal`, `VendorFile`; the field model every vendor renders through; the shared test fixtures; tests)
- Create: `crates/mcp/src/vendor/json.rs` (the order-keeping JSON file and its rendering; tests)
- Create: `crates/mcp/src/vendor/claude.rs` (`Claude`; tests)
- Create: `crates/mcp/src/vendor/codex.rs` (`Codex`, the TOML file; tests)
- Create: `crates/mcp/src/vendor/antigravity.rs` (`Antigravity`; tests)

**Interfaces:**
- Consumes (Task 1): `catalog::{Server, VendorName, Transport, EnvValue, HeaderValue}`, `VendorName::ALL`, `VendorName::as_str`, `Transport::as_str`, `EnvValue::secret_var`; the tests build each fixture with `Catalog::parse`, so every fixture keeps the catalog's rules.
- Produces (`fl_mcp::vendor`):
  - `pub trait Vendor: Sync { fn name(&self) -> VendorName; fn title(&self) -> &'static str; fn target(&self) -> &'static str; fn render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal>; fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal>; }` — `title` is `Claude Code` / `Codex` / `Antigravity`; `target` is relative to the project root (`.mcp.json`, `.codex/config.toml`, `.agents/mcp_config.json`); `open` takes `None` for a file that does not exist, and `path` only names the file in a refusal.
  - `pub fn vendor(name: VendorName) -> &'static dyn Vendor`; unit structs `Claude`, `Codex`, `Antigravity`.
  - `pub struct Rendered` (`Debug, Clone, PartialEq, Eq`): `text(&self) -> &str`, `bytes(&self) -> &[u8]`. Deterministic for a given `(name, server)`; a Codex entry's text includes its name (`[mcp_servers.<name>]`), a JSON entry's does not.
  - `pub struct VendorRefusal { pub vendor: VendorName, pub server: String, pub reason: String }` (`Debug, Clone, PartialEq, Eq, thiserror::Error`); `reason` is a clause; Display: ``<title> cannot run server `<server>`: <reason>. It is left out of <target>; the other vendors still get it. To say so in the catalog, give the server a `vendors` list without `<vendor>` ``.
  - `pub struct FileRefusal { pub path: PathBuf, pub problem: String, pub next: String }` (`Debug, Clone, PartialEq, Eq, thiserror::Error`); Display `<path>: <problem>. <next>`. It is not an `McpError` variant; Task 6 wraps it.
  - `pub struct VendorFile` (`Debug, Clone`): `names(&self) -> Vec<String>` (file order), `entry(&self, name: &str) -> Option<Vec<u8>>` (same form as `Rendered::bytes`), `set(&mut self, name: &str, entry: &Rendered)` (in place, or last; `entry` must be this vendor's rendering of `name`), `remove(&mut self, name: &str) -> bool`, `to_bytes(&self) -> Vec<u8>`. `to_bytes` re-lays a JSON file's outer layout, so a caller writes only when an entry changed.
- Unique phrases: refusal Display ``cannot run server`` and `the other vendors still get it`; reasons `Codex connects to streamable HTTP servers only, not SSE`, `Codex sends a secret header either whole or as`, `Codex forwards a secret variable under its own name`, `Antigravity does not support the legacy SSE transport`, `so a secret header cannot reach the server`, `so a secret cannot be read under another name`; file refusals `it is not strict JSON (` (next: ``Remove them by hand, then run `fl mcp sync` again``), `its top level is not a JSON object`, `` `mcpServers` is not a JSON object``, `appears twice at the top level`, `` appears twice in `mcpServers` ``, `it is not valid TOML (`, `` `mcp_servers` is not a table`` (next for each of these: ``Fix it by hand, then run `fl mcp sync` again``).

- [ ] **Step 1: Write the failing tests**

In `crates/mcp/src/lib.rs`, after `pub mod registry;` add:

```rust
pub mod vendor;
```

Create `crates/mcp/src/vendor/mod.rs` holding the module lines, the shared fixtures and the tests:

```rust
mod antigravity;
mod claude;
mod codex;
mod json;

/// Catalog entries the vendor tests share, each the body of one
/// `[server.<name>]` table.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::Vendor;
    use crate::catalog::{Catalog, Server};
    use std::path::Path;

    /// A stdio server with a literal and a secret environment variable.
    pub(crate) const STDIO: &str = r#"
transport = "stdio"
command = "npx"
args = ["-y", "@example/files-mcp@1.4.2"]
env.LOG_LEVEL = "debug"
env.FILES_TOKEN = { secret = true }
"#;

    /// A stdio server whose only variable is a secret.
    pub(crate) const ONLY_SECRET: &str = r#"
transport = "stdio"
command = "files-mcp"
env.FILES_TOKEN = { secret = true }
"#;

    /// A secret read from a variable whose name is not the key's.
    pub(crate) const RENAMED: &str = r#"
transport = "stdio"
command = "files-mcp"
env.GITHUB_TOKEN = { secret = true, env = "GH_PAT" }
"#;

    /// An http remote with a secret Bearer header and a literal one.
    pub(crate) const BEARER: &str = r#"
transport = "http"
url = "https://mcp.example.com/mcp"
headers.Authorization = { secret = true, env = "DOCS_TOKEN", scheme = "Bearer" }
headers.X-Client = "fl"
"#;

    /// An http remote with a secret header that carries the whole value.
    pub(crate) const WHOLE: &str = r#"
transport = "http"
url = "https://mcp.example.com/mcp"
headers.X-Api-Key = { secret = true, env = "DOCS_KEY" }
"#;

    /// An http remote with literal headers only.
    pub(crate) const PLAIN_REMOTE: &str = r#"
transport = "http"
url = "https://mcp.example.com/mcp"
headers.X-Client = "fl"
"#;

    /// An SSE remote.
    pub(crate) const SSE: &str = r#"
transport = "sse"
url = "https://events.example.com/sse"
"#;

    /// The catalog's own reading of one entry, so a fixture keeps every
    /// catalog rule.
    pub(crate) fn server(body: &str) -> Server {
        let text = format!("[server.s]\n{body}");
        let mut catalog = Catalog::parse(&text, Path::new(".fl/mcp.toml")).unwrap();
        catalog.servers.remove("s").unwrap()
    }

    /// A new vendor file holding one entry, as fl writes it.
    pub(crate) fn file_with(vendor: &dyn Vendor, name: &str, body: &str) -> String {
        let entry = vendor.render(name, &server(body)).unwrap();
        let mut file = vendor.open(Path::new("f"), None).unwrap();
        file.set(name, &entry);
        String::from_utf8(file.to_bytes()).unwrap()
    }

    /// Why `vendor` refuses the entry.
    pub(crate) fn refusal(vendor: &dyn Vendor, body: &str) -> String {
        vendor.render("s", &server(body)).unwrap_err().reason
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use std::path::Path;

    #[test]
    fn each_vendor_writes_its_own_project_file() {
        let seen: Vec<_> = VendorName::ALL
            .iter()
            .map(|&v| (vendor(v).name(), vendor(v).title(), vendor(v).target()))
            .collect();
        assert_eq!(
            seen,
            [
                (VendorName::Claude, "Claude Code", ".mcp.json"),
                (VendorName::Codex, "Codex", ".codex/config.toml"),
                (
                    VendorName::Antigravity,
                    "Antigravity",
                    ".agents/mcp_config.json"
                ),
            ]
        );
    }

    #[test]
    fn an_entry_fl_writes_reads_back_as_the_bytes_it_rendered() {
        for v in VendorName::ALL {
            let v = vendor(v);
            let foreign = match v.name() {
                VendorName::Codex => "[mcp_servers.theirs]\ncommand = \"their-tool\"\n",
                _ => "{\"mcpServers\": {\"theirs\": {\"command\": \"their-tool\"}}}",
            };
            for start in [None, Some(foreign.as_bytes())] {
                let mut file = v.open(Path::new("f"), start).unwrap();
                let files = v.render("files", &server(STDIO)).unwrap();
                let tools = v.render("tools", &server(PLAIN_REMOTE)).unwrap();
                file.set("files", &v.render("files", &server(PLAIN_REMOTE)).unwrap());
                file.set("tools", &tools);
                file.set("files", &files);
                assert_eq!(file.entry("files").as_deref(), Some(files.bytes()));
                let bytes = file.to_bytes();
                let back = v.open(Path::new("f"), Some(&bytes)).unwrap();
                assert_eq!(
                    back.entry("files").as_deref(),
                    Some(files.bytes()),
                    "{}",
                    v.title()
                );
                assert_eq!(
                    back.entry("tools").as_deref(),
                    Some(tools.bytes()),
                    "{}",
                    v.title()
                );
                assert_eq!(back.entry("nope"), None);
                let mut names = vec!["files", "tools"];
                if start.is_some() {
                    names.insert(0, "theirs");
                }
                assert_eq!(back.names(), names, "{}", v.title());
            }
        }
    }

    #[test]
    fn removing_what_was_added_leaves_the_file_as_it_was() {
        for v in VendorName::ALL {
            let v = vendor(v);
            let text = match v.name() {
                VendorName::Codex => "model = \"o3\"\n\n[mcp_servers.theirs]\ncommand = \"t\"\n",
                _ => "{\n  \"mcpServers\": {\n    \"theirs\": {\"command\": \"t\"}\n  }\n}\n",
            };
            let mut file = v.open(Path::new("f"), Some(text.as_bytes())).unwrap();
            file.set("files", &v.render("files", &server(STDIO)).unwrap());
            assert_ne!(file.to_bytes(), text.as_bytes());
            assert!(file.remove("files"));
            assert!(!file.remove("files"));
            assert_eq!(
                String::from_utf8(file.to_bytes()).unwrap(),
                text,
                "{}",
                v.title()
            );
        }
    }

    #[test]
    fn a_secret_value_in_the_environment_reaches_no_rendered_entry() {
        // Set for every test run by cargo, so a vendor that read the
        // environment would find a value here.
        let value = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
        let stdio = "transport = \"stdio\"\ncommand = \"npx\"\n\
                     env.CARGO_MANIFEST_DIR = { secret = true }\n";
        let remote = "transport = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n\
                      headers.Authorization = { secret = true, env = \"CARGO_MANIFEST_DIR\" }\n";
        let mut written = Vec::new();
        for v in VendorName::ALL {
            for body in [stdio, remote] {
                if let Ok(entry) = vendor(v).render("s", &server(body)) {
                    assert!(
                        !entry.text().contains(&value),
                        "{}: {}",
                        v.as_str(),
                        entry.text()
                    );
                    written.push(entry.text().to_string());
                }
            }
        }
        // Antigravity refuses the secret header; the other five are written.
        assert_eq!(written.len(), 5);
        assert!(written[0].contains("\"CARGO_MANIFEST_DIR\": \"${CARGO_MANIFEST_DIR}\""));
        assert!(written[1].contains("\"Authorization\": \"${CARGO_MANIFEST_DIR}\""));
        let code = include_str!("mod.rs");
        let code = &code[..code.find("#[cfg(test)]").unwrap()];
        for source in [
            code,
            include_str!("json.rs"),
            include_str!("claude.rs"),
            include_str!("codex.rs"),
            include_str!("antigravity.rs"),
        ] {
            for read in ["env::var", "var_os"] {
                assert!(
                    !source.contains(read),
                    "a vendor reads the environment: {read}"
                );
            }
        }
    }

    #[test]
    fn a_refusal_names_the_vendor_the_server_the_reason_and_the_file() {
        let refused = vendor(VendorName::Codex)
            .render("events", &server(SSE))
            .unwrap_err();
        assert_eq!(
            refused,
            VendorRefusal {
                vendor: VendorName::Codex,
                server: "events".into(),
                reason: refused.reason.clone(),
            }
        );
        assert_eq!(
            refused.to_string(),
            format!(
                "Codex cannot run server `events`: {}. It is left out of .codex/config.toml; the \
                 other vendors still get it. To say so in the catalog, give the server a \
                 `vendors` list without `codex`",
                refused.reason
            )
        );
    }
}
```

Create `crates/mcp/src/vendor/json.rs` holding only the order-keeping JSON file's tests:

```rust
#[cfg(test)]
mod tests {
    use crate::vendor::fixtures::*;
    use crate::vendor::{Antigravity, Claude, Vendor};
    use std::path::Path;

    const FOREIGN: &str = r#"{
  "$schema": "https://example.com/schema.json",
  "mcpServers": {
    "docs": {"type": "http", "url": "https://old.example.com/mcp"},
    "theirs":   {
        "command" : "their-tool",   "args": [ "--fast" ]
      },
    "files": {"command": "old"}
  },
  "zeta": [1, 2,
     3]
}"#;

    #[test]
    fn foreign_entries_and_key_order_survive_a_rewrite() {
        let mut file = Claude
            .open(Path::new("f"), Some(FOREIGN.as_bytes()))
            .unwrap();
        let theirs = file.entry("theirs").unwrap();
        assert_eq!(
            String::from_utf8(theirs.clone()).unwrap(),
            "{\n        \"command\" : \"their-tool\",   \"args\": [ \"--fast\" ]\n      }"
        );
        file.set("docs", &Claude.render("docs", &server(WHOLE)).unwrap());
        assert!(file.remove("files"));
        file.set("events", &Claude.render("events", &server(SSE)).unwrap());
        let written = String::from_utf8(file.to_bytes()).unwrap();
        assert_eq!(
            written,
            r#"{
  "$schema": "https://example.com/schema.json",
  "mcpServers": {
    "docs": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "X-Api-Key": "${DOCS_KEY}"
      }
    },
    "theirs": {
        "command" : "their-tool",   "args": [ "--fast" ]
      },
    "events": {
      "type": "sse",
      "url": "https://events.example.com/sse"
    }
  },
  "zeta": [1, 2,
     3]
}
"#
        );
        let mut file = Claude
            .open(Path::new("f"), Some(written.as_bytes()))
            .unwrap();
        assert_eq!(file.entry("theirs").unwrap(), theirs);
        assert_eq!(file.names(), ["docs", "theirs", "events"]);
        assert!(file.remove("docs"));
        assert_eq!(file.names(), ["theirs", "events"]);
        assert!(file.remove("events"));
        assert_eq!(
            String::from_utf8(file.to_bytes()).unwrap(),
            r#"{
  "$schema": "https://example.com/schema.json",
  "mcpServers": {
    "theirs": {
        "command" : "their-tool",   "args": [ "--fast" ]
      }
  },
  "zeta": [1, 2,
     3]
}
"#
        );
    }

    #[test]
    fn a_jsonc_file_is_refused_not_rewritten() {
        let cases = [
            ("{\n  // mine\n  \"mcpServers\": {}\n}", "line 2 column 3"),
            (
                "{\"mcpServers\": {\"a\": {\"command\": \"x\"},}}",
                "line 1 column 39",
            ),
            ("\u{feff}{\"mcpServers\": {}}", "line 1 column 1"),
        ];
        for vendor in [&Claude as &dyn Vendor, &Antigravity] {
            for (text, at) in cases {
                let err = vendor.open(Path::new("p/f.json"), Some(text.as_bytes()));
                let err = err.unwrap_err().to_string();
                assert!(
                    err.starts_with("p/f.json: it is not strict JSON ("),
                    "{err}"
                );
                assert!(err.contains(at), "{err}");
                assert!(
                    err.ends_with(
                        "), so fl will not rewrite it and lose its comments, trailing commas or \
                         byte-order mark. Remove them by hand, then run `fl mcp sync` again"
                    ),
                    "{err}"
                );
            }
        }
        let err = Claude
            .open(Path::new("p/f.json"), Some(b"{\"a\": \"\xff\"}"))
            .unwrap_err();
        assert!(err.problem.starts_with("it is not strict JSON ("), "{err}");
    }

    #[test]
    fn a_json_file_that_is_not_an_object_of_servers_is_refused() {
        let cases = [
            ("[]", "its top level is not a JSON object"),
            ("{\"mcpServers\": []}", "`mcpServers` is not a JSON object"),
            (
                "{\"a\": 1, \"a\": 2}",
                "the key `a` appears twice at the top level",
            ),
            (
                "{\"mcpServers\": {\"x\": {}, \"x\": {}}}",
                "the server `x` appears twice in `mcpServers`",
            ),
        ];
        for (text, problem) in cases {
            let err = Claude
                .open(Path::new("p/f.json"), Some(text.as_bytes()))
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("p/f.json: {problem}. Fix it by hand, then run `fl mcp sync` again")
            );
        }
    }

    #[test]
    fn a_missing_file_starts_with_empty_mcp_servers() {
        let file = Claude.open(Path::new("f"), None).unwrap();
        assert!(file.names().is_empty());
        assert_eq!(file.to_bytes(), b"{\n  \"mcpServers\": {}\n}\n");
    }

    #[test]
    fn a_file_without_mcp_servers_gains_it_only_when_an_entry_is_added() {
        let text = "{\"theme\": \"dark\"}";
        let mut file = Claude.open(Path::new("f"), Some(text.as_bytes())).unwrap();
        assert!(file.names().is_empty());
        assert!(!file.remove("files"));
        assert_eq!(file.to_bytes(), b"{\n  \"theme\": \"dark\"\n}\n");
        file.set(
            "files",
            &Claude.render("files", &server(ONLY_SECRET)).unwrap(),
        );
        assert_eq!(
            String::from_utf8(file.to_bytes()).unwrap(),
            r#"{
  "theme": "dark",
  "mcpServers": {
    "files": {
      "command": "files-mcp",
      "env": {
        "FILES_TOKEN": "${FILES_TOKEN}"
      }
    }
  }
}
"#
        );
        let empty = Claude.open(Path::new("f"), Some(b"{}")).unwrap();
        assert_eq!(empty.to_bytes(), b"{}\n");
    }
}
```

Create `crates/mcp/src/vendor/claude.rs` holding only Claude Code's tests:

```rust
#[cfg(test)]
mod tests {
    use super::Claude;
    use crate::vendor::fixtures::*;

    #[test]
    fn a_stdio_server_gets_its_command_args_and_env_with_a_secret_as_a_reference() {
        assert_eq!(
            file_with(&Claude, "files", STDIO),
            r#"{
  "mcpServers": {
    "files": {
      "command": "npx",
      "args": [
        "-y",
        "@example/files-mcp@1.4.2"
      ],
      "env": {
        "FILES_TOKEN": "${FILES_TOKEN}",
        "LOG_LEVEL": "debug"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn a_secret_under_another_name_reads_that_variable() {
        assert_eq!(
            file_with(&Claude, "gh", RENAMED),
            r#"{
  "mcpServers": {
    "gh": {
      "command": "files-mcp",
      "env": {
        "GITHUB_TOKEN": "${GH_PAT}"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn an_http_remote_carries_its_type_and_a_secret_bearer_header_as_a_reference() {
        assert_eq!(
            file_with(&Claude, "docs", BEARER),
            r#"{
  "mcpServers": {
    "docs": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${DOCS_TOKEN}",
        "X-Client": "fl"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn a_secret_header_with_no_scheme_is_the_reference_alone() {
        assert_eq!(
            file_with(&Claude, "docs", WHOLE),
            r#"{
  "mcpServers": {
    "docs": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "X-Api-Key": "${DOCS_KEY}"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn an_sse_remote_is_written_with_its_type() {
        assert_eq!(
            file_with(&Claude, "events", SSE),
            r#"{
  "mcpServers": {
    "events": {
      "type": "sse",
      "url": "https://events.example.com/sse"
    }
  }
}
"#
        );
    }
}
```

Create `crates/mcp/src/vendor/codex.rs` holding only Codex's tests:

```rust
#[cfg(test)]
mod tests {
    use super::Codex;
    use crate::vendor::Vendor;
    use crate::vendor::fixtures::*;
    use std::path::Path;

    #[test]
    fn a_stdio_server_forwards_a_secret_by_name_and_writes_literals_only() {
        assert_eq!(
            file_with(&Codex, "files", STDIO),
            r#"[mcp_servers.files]
command = "npx"
args = ["-y", "@example/files-mcp@1.4.2"]
env = { LOG_LEVEL = "debug" }
env_vars = ["FILES_TOKEN"]
"#
        );
    }

    #[test]
    fn a_server_whose_env_is_all_secret_gets_no_env_table() {
        assert_eq!(
            file_with(&Codex, "files", ONLY_SECRET),
            "[mcp_servers.files]\ncommand = \"files-mcp\"\nenv_vars = [\"FILES_TOKEN\"]\n"
        );
    }

    #[test]
    fn a_secret_bearer_authorization_is_the_bearer_token_variable() {
        assert_eq!(
            file_with(&Codex, "docs", BEARER),
            r#"[mcp_servers.docs]
url = "https://mcp.example.com/mcp"
bearer_token_env_var = "DOCS_TOKEN"
http_headers = { X-Client = "fl" }
"#
        );
        // A header name and a scheme are matched without regard to case.
        let lower = "transport = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n\
                     headers.authorization = { secret = true, env = \"T\", scheme = \"bearer\" }\n";
        assert_eq!(
            file_with(&Codex, "docs", lower),
            "[mcp_servers.docs]\nurl = \"https://mcp.example.com/mcp\"\n\
             bearer_token_env_var = \"T\"\n"
        );
    }

    #[test]
    fn a_secret_header_with_no_scheme_is_read_whole_from_its_variable() {
        assert_eq!(
            file_with(&Codex, "docs", WHOLE),
            r#"[mcp_servers.docs]
url = "https://mcp.example.com/mcp"
env_http_headers = { X-Api-Key = "DOCS_KEY" }
"#
        );
    }

    #[test]
    fn an_sse_remote_is_refused_for_codex() {
        assert_eq!(
            refusal(&Codex, SSE),
            "Codex connects to streamable HTTP servers only, not SSE"
        );
    }

    #[test]
    fn a_secret_header_codex_cannot_prefix_is_refused() {
        let token = "transport = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n\
                     headers.Authorization = { secret = true, env = \"T\", scheme = \"Token\" }\n";
        let elsewhere = "transport = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n\
                         headers.X-Auth = { secret = true, env = \"T\", scheme = \"Bearer\" }\n";
        for (body, header, scheme) in [
            (token, "Authorization", "Token"),
            (elsewhere, "X-Auth", "Bearer"),
        ] {
            assert_eq!(
                refusal(&Codex, body),
                format!(
                    "the secret header `{header}` has the scheme `{scheme}`, and Codex sends a \
                     secret header either whole or as `Authorization: Bearer`"
                )
            );
        }
    }

    #[test]
    fn a_secret_under_another_name_is_refused_for_codex() {
        assert_eq!(
            refusal(&Codex, RENAMED),
            "`env.GITHUB_TOKEN` reads the variable `GH_PAT`, and Codex forwards a secret \
             variable under its own name"
        );
    }

    const COMMENTED: &str = r#"# Codex settings for this repository.
model = "o3"   # the default model

# A server someone added by hand.
[mcp_servers.theirs]
command = "their-tool"
args = [ "--fast" ]   # keep it quick

[mcp_servers.theirs.env]
MODE = "x"

# Profiles sit between the servers.
[profiles.fast]
model = "o4-mini"

# The docs server, as fl wrote it.
[mcp_servers.docs]
url = "https://old.example.com/mcp"
"#;

    #[test]
    fn codex_comments_and_layout_survive() {
        let mut file = Codex
            .open(Path::new("f"), Some(COMMENTED.as_bytes()))
            .unwrap();
        let theirs = file.entry("theirs").unwrap();
        file.set("docs", &Codex.render("docs", &server(BEARER)).unwrap());
        file.set(
            "files",
            &Codex.render("files", &server(ONLY_SECRET)).unwrap(),
        );
        let written = String::from_utf8(file.to_bytes()).unwrap();
        assert_eq!(
            written,
            r#"# Codex settings for this repository.
model = "o3"   # the default model

# A server someone added by hand.
[mcp_servers.theirs]
command = "their-tool"
args = [ "--fast" ]   # keep it quick

[mcp_servers.theirs.env]
MODE = "x"

# Profiles sit between the servers.
[profiles.fast]
model = "o4-mini"

# The docs server, as fl wrote it.
[mcp_servers.docs]
url = "https://mcp.example.com/mcp"
bearer_token_env_var = "DOCS_TOKEN"
http_headers = { X-Client = "fl" }

[mcp_servers.files]
command = "files-mcp"
env_vars = ["FILES_TOKEN"]
"#
        );
        let mut file = Codex
            .open(Path::new("f"), Some(written.as_bytes()))
            .unwrap();
        assert_eq!(file.entry("theirs").unwrap(), theirs);
        assert_eq!(file.names(), ["theirs", "docs", "files"]);
        assert!(file.remove("docs"));
        assert!(file.remove("files"));
        assert_eq!(
            String::from_utf8(file.to_bytes()).unwrap(),
            // The comment above an entry is the entry's, and goes with it.
            COMMENTED.replace(
                "\n# The docs server, as fl wrote it.\n[mcp_servers.docs]\n\
                 url = \"https://old.example.com/mcp\"\n",
                ""
            )
        );
    }

    #[test]
    fn a_first_entry_in_a_file_with_no_servers_goes_last() {
        let text = "model = \"o3\"\n\n[profiles.fast]\nmodel = \"o4-mini\"\n";
        let mut file = Codex.open(Path::new("f"), Some(text.as_bytes())).unwrap();
        file.set(
            "files",
            &Codex.render("files", &server(ONLY_SECRET)).unwrap(),
        );
        assert_eq!(
            String::from_utf8(file.to_bytes()).unwrap(),
            format!(
                "{text}\n[mcp_servers.files]\ncommand = \"files-mcp\"\n\
                 env_vars = [\"FILES_TOKEN\"]\n"
            )
        );
    }

    #[test]
    fn a_codex_file_fl_cannot_read_as_a_table_of_servers_is_refused() {
        // A line of the file can hold a value, so the refusal quotes none.
        let cases: [(&[u8], &str); 4] = [
            (
                b"model = \"o3\"\n\ntoken = \"sk-live-4f9a\" x\n",
                "it is not valid TOML (line 3: ",
            ),
            (b"model = \"\xff\"\n", "it is not valid TOML (invalid utf-8"),
            (b"mcp_servers = 3\n", "`mcp_servers` is not a table"),
            (
                b"mcp_servers = { x = { command = \"x\" } }\n",
                "`mcp_servers` is not a table",
            ),
        ];
        for (text, problem) in cases {
            let err = Codex.open(Path::new("p/.codex/config.toml"), Some(text));
            let err = err.unwrap_err().to_string();
            assert!(!err.contains("sk-live-4f9a"), "{err}");
            assert!(
                err.starts_with(&format!("p/.codex/config.toml: {problem}")),
                "{err}"
            );
            assert!(
                err.ends_with("Fix it by hand, then run `fl mcp sync` again"),
                "{err}"
            );
        }
    }
}
```

Create `crates/mcp/src/vendor/antigravity.rs` holding only Antigravity's tests:

```rust
#[cfg(test)]
mod tests {
    use super::Antigravity;
    use crate::vendor::fixtures::*;

    #[test]
    fn a_stdio_server_leaves_a_secret_out_for_the_server_to_inherit() {
        assert_eq!(
            file_with(&Antigravity, "files", STDIO),
            r#"{
  "mcpServers": {
    "files": {
      "command": "npx",
      "args": [
        "-y",
        "@example/files-mcp@1.4.2"
      ],
      "env": {
        "LOG_LEVEL": "debug"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn a_server_whose_env_is_all_secret_gets_no_env() {
        assert_eq!(
            file_with(&Antigravity, "files", ONLY_SECRET),
            r#"{
  "mcpServers": {
    "files": {
      "command": "files-mcp"
    }
  }
}
"#
        );
    }

    #[test]
    fn a_remote_is_written_under_server_url_with_its_literal_headers() {
        assert_eq!(
            file_with(&Antigravity, "docs", PLAIN_REMOTE),
            r#"{
  "mcpServers": {
    "docs": {
      "serverUrl": "https://mcp.example.com/mcp",
      "headers": {
        "X-Client": "fl"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn an_sse_remote_is_refused_for_antigravity() {
        assert_eq!(
            refusal(&Antigravity, SSE),
            "Antigravity does not support the legacy SSE transport; only a streamable HTTP \
             endpoint or a stdio server"
        );
    }

    #[test]
    fn any_secret_header_is_refused_for_antigravity() {
        for (body, header) in [(BEARER, "Authorization"), (WHOLE, "X-Api-Key")] {
            assert_eq!(
                refusal(&Antigravity, body),
                format!(
                    "the header `{header}` is a secret, and Antigravity expands no `${{…}}` in a \
                     header, so a secret header cannot reach the server"
                )
            );
        }
    }

    #[test]
    fn a_secret_under_another_name_is_refused_for_antigravity() {
        assert_eq!(
            refusal(&Antigravity, RENAMED),
            "`env.GITHUB_TOKEN` reads the variable `GH_PAT`, but a stdio server inherits \
             Antigravity's environment, so a secret cannot be read under another name"
        );
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-mcp --lib`
Expected: FAIL to compile (25 errors) — `error[E0432]` (unresolved imports `super::Claude`, `super::Codex`, `super::Antigravity`, `super::Vendor`, `crate::vendor::Vendor` and `crate::vendor::{Antigravity, Claude, Vendor}`), `error[E0433]` (cannot find type `VendorName` — the tests take it from the implementation's `use` line), `error[E0425]` (cannot find function `vendor`) and `error[E0422]` (cannot find struct `VendorRefusal`).

- [ ] **Step 3: Implement**

In `crates/mcp/src/vendor/mod.rs`, replace the four `mod` lines at the top (everything above the fixtures' doc comment) with:

```rust
//! The vendor files (MCP spec §4.1, §4.2): each agent CLI's own project MCP
//! file, written from the catalog. Each vendor turns a catalog entry into its
//! own shape, or refuses it naming the reason, and reads and rewrites its file
//! keeping every entry fl does not touch. Nothing here reads the environment:
//! a secret is written as a reference, never as a value (MCP spec §2.1).

mod antigravity;
mod claude;
mod codex;
mod json;

pub use antigravity::Antigravity;
pub use claude::Claude;
pub use codex::Codex;

use crate::catalog::{EnvValue, HeaderValue, Server, VendorName};
use std::fmt;
use std::path::{Path, PathBuf};

/// One agent CLI's project MCP file.
pub trait Vendor: Sync {
    fn name(&self) -> VendorName;
    /// The vendor's name for a person.
    fn title(&self) -> &'static str;
    /// The file, relative to the project root.
    fn target(&self) -> &'static str;
    /// The entry exactly as fl writes it for the server `name`, or why this
    /// vendor cannot run the server; the other vendors still get it (MCP spec
    /// §4.2).
    fn render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal>;
    /// The file as it stands: `None` when it does not exist yet. `path` only
    /// names the file in a refusal.
    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal>;
}

/// The vendor behind `name`.
pub fn vendor(name: VendorName) -> &'static dyn Vendor {
    match name {
        VendorName::Claude => &Claude,
        VendorName::Codex => &Codex,
        VendorName::Antigravity => &Antigravity,
    }
}

/// An entry as fl writes it. Its bytes are the entry's own bytes in the file
/// once written, which [`VendorFile::entry`] gives back, so hashing both tells
/// whether the entry is still what fl wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    text: String,
}

impl Rendered {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }
}

/// A server one vendor cannot run as the catalog gives it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct VendorRefusal {
    pub vendor: VendorName,
    pub server: String,
    /// The reason, as a clause.
    pub reason: String,
}

impl fmt::Display for VendorRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = vendor(self.vendor);
        write!(
            f,
            "{} cannot run server `{}`: {}. It is left out of {}; the other vendors still get \
             it. To say so in the catalog, give the server a `vendors` list without `{}`",
            v.title(),
            self.server,
            self.reason,
            v.target(),
            self.vendor.as_str()
        )
    }
}

/// A vendor file fl will not rewrite.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {problem}. {next}", path.display())]
pub struct FileRefusal {
    pub path: PathBuf,
    pub problem: String,
    pub next: String,
}

/// A vendor file read into memory, its entries by name. Changing one entry
/// leaves every other entry, and everything outside the servers' section,
/// as it was.
#[derive(Debug, Clone)]
pub struct VendorFile {
    kind: Kind,
}

#[derive(Debug, Clone)]
enum Kind {
    Json(json::JsonFile),
    Toml(codex::TomlFile),
}

impl VendorFile {
    /// Every server's name, in the file's order.
    pub fn names(&self) -> Vec<String> {
        match &self.kind {
            Kind::Json(f) => f.names(),
            Kind::Toml(f) => f.names(),
        }
    }

    /// The server's entry as it stands in the file, in the same form as
    /// [`Rendered::bytes`].
    pub fn entry(&self, name: &str) -> Option<Vec<u8>> {
        match &self.kind {
            Kind::Json(f) => f.entry(name),
            Kind::Toml(f) => f.entry(name),
        }
    }

    /// Writes the entry: in place of the one with this name, or last.
    /// `entry` is this file's vendor's rendering of the server `name`.
    pub fn set(&mut self, name: &str, entry: &Rendered) {
        match &mut self.kind {
            Kind::Json(f) => f.set(name, entry),
            Kind::Toml(f) => f.set(name, entry),
        }
    }

    /// Removes the entry; `false` when there is none.
    pub fn remove(&mut self, name: &str) -> bool {
        match &mut self.kind {
            Kind::Json(f) => f.remove(name),
            Kind::Toml(f) => f.remove(name),
        }
    }

    /// The whole file.
    pub fn to_bytes(&self) -> Vec<u8> {
        match &self.kind {
            Kind::Json(f) => f.to_bytes(),
            Kind::Toml(f) => f.to_bytes(),
        }
    }
}

/// An entry before it takes a vendor's syntax: each field is a string, a
/// list of strings, or a table of strings. Every field a vendor writes is one
/// of these.
enum Field {
    Text(String),
    List(Vec<String>),
    Map(Vec<(String, String)>),
}

/// The fields of one entry, in the order the vendor writes them.
#[derive(Default)]
struct Fields(Vec<(&'static str, Field)>);

impl Fields {
    fn text(&mut self, key: &'static str, value: &str) {
        self.0.push((key, Field::Text(value.to_string())));
    }

    /// Left out when empty.
    fn list(&mut self, key: &'static str, values: Vec<String>) {
        if !values.is_empty() {
            self.0.push((key, Field::List(values)));
        }
    }

    /// Left out when empty.
    fn map(&mut self, key: &'static str, values: Vec<(String, String)>) {
        if !values.is_empty() {
            self.0.push((key, Field::Map(values)));
        }
    }
}

/// A stdio server's command: the catalog requires it.
fn command(server: &Server) -> &str {
    server
        .command
        .as_deref()
        .expect("a stdio server has a command")
}

/// A remote server's url: the catalog requires it.
fn url(server: &Server) -> &str {
    server
        .url
        .as_deref()
        .expect("an http or sse server has a url")
}

fn args(server: &Server) -> Vec<String> {
    server.args.clone().unwrap_or_default()
}

fn env(server: &Server) -> impl Iterator<Item = (&String, &EnvValue)> {
    server.env.iter().flatten()
}

fn headers(server: &Server) -> impl Iterator<Item = (&String, &HeaderValue)> {
    server.headers.iter().flatten()
}

/// The refusal for `server` by `vendor`.
fn refuse(vendor: VendorName, server: &str, reason: String) -> VendorRefusal {
    VendorRefusal {
        vendor,
        server: server.to_string(),
        reason,
    }
}
```

In `crates/mcp/src/vendor/json.rs`, insert at the top of the file, above `#[cfg(test)]`:

```rust
//! The JSON file Claude Code and Antigravity read: `mcpServers`, one entry per
//! server. Key order is kept with an `IndexMap` of raw values, never with
//! `serde_json`'s `preserve_order` (see the workspace Cargo.toml): every entry
//! fl does not write, and every other top-level key, is kept byte for byte;
//! only fl's entries and the layout around them are written fresh. A file
//! that is not strict JSON is refused, never rewritten.

use super::{Field, FileRefusal, Rendered};
use indexmap::IndexMap;
use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use std::fmt;
use std::path::Path;

const SERVERS: &str = "mcpServers";

/// A server's line in `mcpServers`, two levels in; a rendered entry's own
/// lines are indented from there.
const ENTRY_INDENT: &str = "    ";

const FIX_BY_HAND: &str = "Fix it by hand, then run `fl mcp sync` again";

/// An entry as fl writes it: pretty, two spaces a level, at its depth in the
/// file.
pub(super) fn render(fields: &[(&'static str, Field)]) -> Rendered {
    let mut entry = IndexMap::new();
    for (key, field) in fields {
        let value = match field {
            Field::Text(s) => serde_json::Value::from(s.as_str()),
            Field::List(values) => serde_json::Value::from(values.clone()),
            Field::Map(values) => {
                let map = values
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::from(v.as_str())));
                serde_json::Value::Object(map.collect())
            }
        };
        entry.insert(*key, value);
    }
    let text = serde_json::to_string_pretty(&entry).expect("strings always serialize");
    // A JSON string holds no raw newline, so each one is a line break.
    Rendered {
        text: text.replace('\n', &format!("\n{ENTRY_INDENT}")),
    }
}

/// An object's members in the file's order, each value as its own bytes.
struct Members {
    members: IndexMap<String, Box<RawValue>>,
    /// The first key seen twice: kept apart, since a map keeps only one.
    twice: Option<String>,
}

impl<'de> Deserialize<'de> for Members {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Members, D::Error> {
        d.deserialize_map(MembersVisitor)
    }
}

struct MembersVisitor;

impl<'de> Visitor<'de> for MembersVisitor {
    type Value = Members;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Members, A::Error> {
        let mut members = IndexMap::new();
        let mut twice = None;
        while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
            if members.contains_key(&key) {
                twice.get_or_insert(key);
            } else {
                members.insert(key, value);
            }
        }
        Ok(Members { members, twice })
    }
}

#[derive(Debug, Clone)]
pub(super) struct JsonFile {
    /// The top level; `mcpServers`, when present, holds its old bytes, and
    /// `servers` stands in for it.
    top: IndexMap<String, Box<RawValue>>,
    /// `None` when the file has no `mcpServers` and fl has written nothing.
    servers: Option<IndexMap<String, Box<RawValue>>>,
}

pub(super) fn open(path: &Path, bytes: Option<&[u8]>) -> Result<JsonFile, FileRefusal> {
    let Some(bytes) = bytes else {
        let mut file = JsonFile {
            top: IndexMap::new(),
            servers: None,
        };
        file.servers_mut();
        return Ok(file);
    };
    let refuse = |problem: String, next: &str| FileRefusal {
        path: path.to_path_buf(),
        problem,
        next: next.to_string(),
    };
    let not_strict = |cause: String| {
        refuse(
            format!(
                "it is not strict JSON ({cause}), so fl will not rewrite it and lose its \
                 comments, trailing commas or byte-order mark"
            ),
            "Remove them by hand, then run `fl mcp sync` again",
        )
    };
    let text = std::str::from_utf8(bytes).map_err(|e| not_strict(e.to_string()))?;
    let value: &RawValue = serde_json::from_str(text).map_err(|e| not_strict(e.to_string()))?;
    if !value.get().starts_with('{') {
        return Err(refuse(
            "its top level is not a JSON object".into(),
            FIX_BY_HAND,
        ));
    }
    let top: Members = serde_json::from_str(text).expect("a strict JSON object");
    if let Some(key) = top.twice {
        let problem = format!("the key `{key}` appears twice at the top level");
        return Err(refuse(problem, FIX_BY_HAND));
    }
    let servers = match top.members.get(SERVERS) {
        None => None,
        Some(raw) if !raw.get().starts_with('{') => {
            return Err(refuse(
                format!("`{SERVERS}` is not a JSON object"),
                FIX_BY_HAND,
            ));
        }
        Some(raw) => {
            let servers: Members = serde_json::from_str(raw.get()).expect("a strict JSON object");
            if let Some(name) = servers.twice {
                let problem = format!("the server `{name}` appears twice in `{SERVERS}`");
                return Err(refuse(problem, FIX_BY_HAND));
            }
            Some(servers.members)
        }
    };
    Ok(JsonFile {
        top: top.members,
        servers,
    })
}

impl JsonFile {
    /// `mcpServers`, added last when the file has none.
    fn servers_mut(&mut self) -> &mut IndexMap<String, Box<RawValue>> {
        if self.servers.is_none() {
            let empty = RawValue::from_string("{}".into()).expect("valid JSON");
            self.top.insert(SERVERS.into(), empty);
        }
        self.servers.get_or_insert_default()
    }

    pub(super) fn names(&self) -> Vec<String> {
        self.servers
            .iter()
            .flatten()
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub(super) fn entry(&self, name: &str) -> Option<Vec<u8>> {
        let entry = self.servers.as_ref()?.get(name)?;
        Some(entry.get().as_bytes().to_vec())
    }

    pub(super) fn set(&mut self, name: &str, entry: &Rendered) {
        let raw = RawValue::from_string(entry.text.clone()).expect("fl renders valid JSON");
        self.servers_mut().insert(name.to_string(), raw);
    }

    pub(super) fn remove(&mut self, name: &str) -> bool {
        let servers = self.servers.as_mut();
        servers.is_some_and(|s| s.shift_remove(name).is_some())
    }

    pub(super) fn to_bytes(&self) -> Vec<u8> {
        let top = self.top.iter().map(|(key, raw)| match &self.servers {
            Some(servers) if key == SERVERS => {
                let servers = servers.iter().map(|(k, v)| (k, v.get().to_string()));
                (key, object(servers, ENTRY_INDENT))
            }
            _ => (key, raw.get().to_string()),
        });
        let mut out = object(top, "  ");
        out.push('\n');
        out.into_bytes()
    }
}

/// `{}`, or one member a line at `indent`, the closing brace two spaces
/// further out.
fn object<'a>(members: impl Iterator<Item = (&'a String, String)>, indent: &str) -> String {
    let lines: Vec<String> = members
        .map(|(key, value)| {
            let key = serde_json::to_string(key).expect("a string always serializes");
            format!("{indent}{key}: {value}")
        })
        .collect();
    if lines.is_empty() {
        return "{}".into();
    }
    let close = &indent[2..];
    format!("{{\n{}\n{close}}}", lines.join(",\n"))
}
```

In `crates/mcp/src/vendor/claude.rs`, insert at the top of the file, above `#[cfg(test)]`:

```rust
//! Claude Code's `<root>/.mcp.json`. Claude Code expands `${NAME}` in `env`
//! and in headers, so a secret is written as a reference to its variable. A
//! remote entry always carries `type`: Claude Code skips a `url` without one.

use super::{Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal};
use super::{Kind, args, command, env, headers, json, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::path::Path;

pub struct Claude;

impl Vendor for Claude {
    fn name(&self) -> VendorName {
        VendorName::Claude
    }

    fn title(&self) -> &'static str {
        "Claude Code"
    }

    fn target(&self) -> &'static str {
        ".mcp.json"
    }

    fn render(&self, _name: &str, server: &Server) -> Result<Rendered, VendorRefusal> {
        let mut fields = Fields::default();
        match server.transport {
            Transport::Stdio => {
                fields.text("command", command(server));
                fields.list("args", args(server));
                let env = env(server).map(|(key, value)| {
                    let value = match value {
                        EnvValue::Literal(v) => v.clone(),
                        EnvValue::Secret { .. } => {
                            let var = value.secret_var(key).expect("a secret names a variable");
                            format!("${{{var}}}")
                        }
                    };
                    (key.clone(), value)
                });
                fields.map("env", env.collect());
            }
            Transport::Http | Transport::Sse => {
                fields.text("type", server.transport.as_str());
                fields.text("url", url(server));
                let headers = headers(server).map(|(name, value)| {
                    let value = match value {
                        HeaderValue::Literal(v) => v.clone(),
                        HeaderValue::Secret { env, scheme: None } => format!("${{{env}}}"),
                        HeaderValue::Secret {
                            env,
                            scheme: Some(scheme),
                        } => format!("{scheme} ${{{env}}}"),
                    };
                    (name.clone(), value)
                });
                fields.map("headers", headers.collect());
            }
        }
        Ok(json::render(&fields.0))
    }

    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal> {
        let kind = Kind::Json(json::open(path, bytes)?);
        Ok(VendorFile { kind })
    }
}
```

In `crates/mcp/src/vendor/codex.rs`, insert at the top of the file, above `#[cfg(test)]`:

```rust
//! Codex's `<root>/.codex/config.toml`, edited with `toml_edit` so that its
//! comments and layout survive (MCP spec §4.3). Each server is a
//! `[mcp_servers.<name>]` table. Codex expands no `${NAME}`: a secret
//! variable is forwarded by name (`env_vars`), a secret header is read whole
//! from its variable (`env_http_headers`) or is the Bearer token
//! (`bearer_token_env_var`). Codex speaks streamable HTTP only, and has no
//! `type` field.

use super::{Field, Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal};
use super::{Kind, args, command, env, headers, refuse, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::path::Path;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

const SERVERS: &str = "mcp_servers";

pub struct Codex;

impl Vendor for Codex {
    fn name(&self) -> VendorName {
        VendorName::Codex
    }

    fn title(&self) -> &'static str {
        "Codex"
    }

    fn target(&self) -> &'static str {
        ".codex/config.toml"
    }

    fn render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal> {
        let refuse = |reason: String| refuse(VendorName::Codex, name, reason);
        let mut fields = Fields::default();
        match server.transport {
            Transport::Stdio => {
                fields.text("command", command(server));
                fields.list("args", args(server));
                let (mut literals, mut forwarded) = (Vec::new(), Vec::new());
                for (key, value) in env(server) {
                    match value {
                        EnvValue::Literal(v) => literals.push((key.clone(), v.clone())),
                        EnvValue::Secret { .. } => {
                            let var = value.secret_var(key).expect("a secret names a variable");
                            if var != key {
                                return Err(refuse(format!(
                                    "`env.{key}` reads the variable `{var}`, and Codex forwards \
                                     a secret variable under its own name"
                                )));
                            }
                            forwarded.push(var.to_string());
                        }
                    }
                }
                fields.map("env", literals);
                fields.list("env_vars", forwarded);
            }
            Transport::Sse => {
                return Err(refuse(
                    "Codex connects to streamable HTTP servers only, not SSE".into(),
                ));
            }
            Transport::Http => {
                fields.text("url", url(server));
                let (mut literals, mut from_env, mut bearer) = (Vec::new(), Vec::new(), None);
                for (header, value) in headers(server) {
                    match value {
                        HeaderValue::Literal(v) => literals.push((header.clone(), v.clone())),
                        HeaderValue::Secret { env, scheme: None } => {
                            from_env.push((header.clone(), env.clone()));
                        }
                        HeaderValue::Secret {
                            env,
                            scheme: Some(scheme),
                        } if header.eq_ignore_ascii_case("authorization")
                            && scheme.eq_ignore_ascii_case("bearer") =>
                        {
                            bearer = Some(env);
                        }
                        HeaderValue::Secret {
                            scheme: Some(scheme),
                            ..
                        } => {
                            return Err(refuse(format!(
                                "the secret header `{header}` has the scheme `{scheme}`, and \
                                 Codex sends a secret header either whole or as \
                                 `Authorization: Bearer`"
                            )));
                        }
                    }
                }
                if let Some(env) = bearer {
                    fields.text("bearer_token_env_var", env);
                }
                fields.map("http_headers", literals);
                fields.map("env_http_headers", from_env);
            }
        }
        let mut table = Table::new();
        for (key, field) in fields.0 {
            let value = match field {
                Field::Text(s) => Value::from(s),
                Field::List(values) => Value::Array(values.into_iter().collect::<Array>()),
                Field::Map(values) => {
                    Value::InlineTable(values.into_iter().collect::<InlineTable>())
                }
            };
            table.insert(key, Item::Value(value));
        }
        Ok(Rendered {
            text: alone(name, &Item::Table(table)),
        })
    }

    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal> {
        let kind = Kind::Toml(open(path, bytes)?);
        Ok(VendorFile { kind })
    }
}

/// One entry as a file of its own: `[mcp_servers.<name>]` and its body,
/// without what stands above its header. This is both what fl writes and
/// what it reads back, so the two can be compared.
fn alone(name: &str, entry: &Item) -> String {
    let mut entry = entry.clone();
    if let Item::Table(t) = &mut entry {
        t.decor_mut().clear();
    }
    let mut servers = Table::new();
    servers.set_implicit(true);
    servers.insert(name, entry);
    let mut doc = DocumentMut::new();
    doc.insert(SERVERS, Item::Table(servers));
    doc.to_string()
}

#[derive(Debug, Clone)]
pub(super) struct TomlFile {
    doc: DocumentMut,
}

fn open(path: &Path, bytes: Option<&[u8]>) -> Result<TomlFile, FileRefusal> {
    let Some(bytes) = bytes else {
        return Ok(TomlFile {
            doc: DocumentMut::new(),
        });
    };
    let refuse = |problem: String| FileRefusal {
        path: path.to_path_buf(),
        problem,
        next: "Fix it by hand, then run `fl mcp sync` again".into(),
    };
    let text =
        std::str::from_utf8(bytes).map_err(|e| refuse(format!("it is not valid TOML ({e})")))?;
    // The parser's own message quotes the line; a line can hold a value, so
    // only its line number and message are shown.
    let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        let at = e.span().map_or(0, |s| s.start);
        let line = text[..at].matches('\n').count() + 1;
        refuse(format!(
            "it is not valid TOML (line {line}: {})",
            e.message().trim_end()
        ))
    })?;
    if doc.get(SERVERS).is_some_and(|item| !item.is_table()) {
        return Err(refuse(format!("`{SERVERS}` is not a table")));
    }
    Ok(TomlFile { doc })
}

impl TomlFile {
    fn servers(&self) -> Option<&Table> {
        self.doc.get(SERVERS).and_then(Item::as_table)
    }

    /// `mcp_servers`, added with no header of its own when the file has none.
    fn servers_mut(&mut self) -> &mut Table {
        if !self.doc.contains_key(SERVERS) {
            let mut servers = Table::new();
            servers.set_implicit(true);
            self.doc.insert(SERVERS, Item::Table(servers));
        }
        self.doc[SERVERS]
            .as_table_mut()
            .expect("checked when the file was read")
    }

    pub(super) fn names(&self) -> Vec<String> {
        let servers = self.servers().into_iter();
        servers
            .flat_map(|s| s.iter().map(|(name, _)| name.to_string()))
            .collect()
    }

    pub(super) fn entry(&self, name: &str) -> Option<Vec<u8>> {
        let entry = self.servers()?.get(name)?;
        Some(alone(name, entry).into_bytes())
    }

    pub(super) fn set(&mut self, name: &str, entry: &Rendered) {
        let mut doc: DocumentMut = entry.text.parse().expect("fl renders valid TOML");
        let mut new = std::mem::take(&mut doc[SERVERS][name])
            .into_table()
            .expect("fl renders a table");
        let first = self.doc.as_table().is_empty();
        let servers = self.servers_mut();
        match servers.get(name) {
            // In place, under the same lines above it.
            Some(Item::Table(old)) => {
                new.set_position(old.position());
                *new.decor_mut() = old.decor().clone();
            }
            // Last among the servers, a blank line above it.
            _ => {
                new.set_position(None);
                new.decor_mut().set_prefix(if first { "" } else { "\n" });
            }
        }
        servers.insert(name, Item::Table(new));
    }

    pub(super) fn remove(&mut self, name: &str) -> bool {
        let servers = self.doc.get_mut(SERVERS).and_then(Item::as_table_mut);
        servers.is_some_and(|s| s.remove(name).is_some())
    }

    pub(super) fn to_bytes(&self) -> Vec<u8> {
        self.doc.to_string().into_bytes()
    }
}
```

In `crates/mcp/src/vendor/antigravity.rs`, insert at the top of the file, above `#[cfg(test)]`:

```rust
//! Antigravity's `<root>/.agents/mcp_config.json`. Antigravity expands no
//! `${NAME}`, in `env` or in headers, but a stdio server inherits its
//! environment: so a secret variable is left out of the entry, and a server
//! with a secret header is refused for Antigravity alone (MCP spec §4.2).
//! Antigravity has no SSE; its remote key is `serverUrl`.

use super::{Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal};
use super::{Kind, args, command, env, headers, json, refuse, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::path::Path;

pub struct Antigravity;

impl Vendor for Antigravity {
    fn name(&self) -> VendorName {
        VendorName::Antigravity
    }

    fn title(&self) -> &'static str {
        "Antigravity"
    }

    fn target(&self) -> &'static str {
        ".agents/mcp_config.json"
    }

    fn render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal> {
        let refuse = |reason: String| refuse(VendorName::Antigravity, name, reason);
        let mut fields = Fields::default();
        match server.transport {
            Transport::Stdio => {
                fields.text("command", command(server));
                fields.list("args", args(server));
                let mut literals = Vec::new();
                for (key, value) in env(server) {
                    match value {
                        EnvValue::Literal(v) => literals.push((key.clone(), v.clone())),
                        EnvValue::Secret { .. } => {
                            let var = value.secret_var(key).expect("a secret names a variable");
                            if var != key {
                                return Err(refuse(format!(
                                    "`env.{key}` reads the variable `{var}`, but a stdio server \
                                     inherits Antigravity's environment, so a secret cannot be \
                                     read under another name"
                                )));
                            }
                        }
                    }
                }
                fields.map("env", literals);
            }
            Transport::Sse => {
                return Err(refuse(
                    "Antigravity does not support the legacy SSE transport; only a streamable \
                     HTTP endpoint or a stdio server"
                        .into(),
                ));
            }
            Transport::Http => {
                fields.text("serverUrl", url(server));
                let mut literals = Vec::new();
                for (header, value) in headers(server) {
                    match value {
                        HeaderValue::Literal(v) => literals.push((header.clone(), v.clone())),
                        HeaderValue::Secret { .. } => {
                            return Err(refuse(format!(
                                "the header `{header}` is a secret, and Antigravity expands no \
                                 `${{…}}` in a header, so a secret header cannot reach the server"
                            )));
                        }
                    }
                }
                fields.map("headers", literals);
            }
        }
        Ok(json::render(&fields.0))
    }

    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal> {
        let kind = Kind::Json(json::open(path, bytes)?);
        Ok(VendorFile { kind })
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-mcp --lib vendor::`
Expected: PASS — 31 passed, 51 filtered out: `vendor::tests::each_vendor_writes_its_own_project_file`, `vendor::tests::an_entry_fl_writes_reads_back_as_the_bytes_it_rendered`, `vendor::tests::removing_what_was_added_leaves_the_file_as_it_was`, `vendor::tests::a_secret_value_in_the_environment_reaches_no_rendered_entry`, `vendor::tests::a_refusal_names_the_vendor_the_server_the_reason_and_the_file`, `vendor::json::tests::foreign_entries_and_key_order_survive_a_rewrite`, `vendor::json::tests::a_jsonc_file_is_refused_not_rewritten`, `vendor::json::tests::a_json_file_that_is_not_an_object_of_servers_is_refused`, `vendor::json::tests::a_missing_file_starts_with_empty_mcp_servers`, `vendor::json::tests::a_file_without_mcp_servers_gains_it_only_when_an_entry_is_added`, `vendor::claude::tests::a_stdio_server_gets_its_command_args_and_env_with_a_secret_as_a_reference`, `vendor::claude::tests::a_secret_under_another_name_reads_that_variable`, `vendor::claude::tests::an_http_remote_carries_its_type_and_a_secret_bearer_header_as_a_reference`, `vendor::claude::tests::a_secret_header_with_no_scheme_is_the_reference_alone`, `vendor::claude::tests::an_sse_remote_is_written_with_its_type`, `vendor::codex::tests::a_stdio_server_forwards_a_secret_by_name_and_writes_literals_only`, `vendor::codex::tests::a_server_whose_env_is_all_secret_gets_no_env_table`, `vendor::codex::tests::a_secret_bearer_authorization_is_the_bearer_token_variable`, `vendor::codex::tests::a_secret_header_with_no_scheme_is_read_whole_from_its_variable`, `vendor::codex::tests::an_sse_remote_is_refused_for_codex`, `vendor::codex::tests::a_secret_header_codex_cannot_prefix_is_refused`, `vendor::codex::tests::a_secret_under_another_name_is_refused_for_codex`, `vendor::codex::tests::codex_comments_and_layout_survive`, `vendor::codex::tests::a_first_entry_in_a_file_with_no_servers_goes_last`, `vendor::codex::tests::a_codex_file_fl_cannot_read_as_a_table_of_servers_is_refused`, `vendor::antigravity::tests::a_stdio_server_leaves_a_secret_out_for_the_server_to_inherit`, `vendor::antigravity::tests::a_server_whose_env_is_all_secret_gets_no_env`, `vendor::antigravity::tests::a_remote_is_written_under_server_url_with_its_literal_headers`, `vendor::antigravity::tests::an_sse_remote_is_refused_for_antigravity`, `vendor::antigravity::tests::any_secret_header_is_refused_for_antigravity`, `vendor::antigravity::tests::a_secret_under_another_name_is_refused_for_antigravity`.

- [ ] **Step 5: Mutation checks**

Each name below is `cargo test -p fl-mcp --lib vendor::<module>::tests::<test>`: "targets" is `vendor::tests::each_vendor_writes_its_own_project_file`; "read-back" is `vendor::tests::an_entry_fl_writes_reads_back_as_the_bytes_it_rendered`; "add-remove" is `vendor::tests::removing_what_was_added_leaves_the_file_as_it_was`; "secret" is `vendor::tests::a_secret_value_in_the_environment_reaches_no_rendered_entry`; "foreign" is `vendor::json::tests::foreign_entries_and_key_order_survive_a_rewrite`; "jsonc" is `vendor::json::tests::a_jsonc_file_is_refused_not_rewritten`; "shape" is `vendor::json::tests::a_json_file_that_is_not_an_object_of_servers_is_refused`; "missing" is `vendor::json::tests::a_missing_file_starts_with_empty_mcp_servers`; "gains" is `vendor::json::tests::a_file_without_mcp_servers_gains_it_only_when_an_entry_is_added`; "claude-stdio" is `vendor::claude::tests::a_stdio_server_gets_its_command_args_and_env_with_a_secret_as_a_reference`; "claude-renamed" is `vendor::claude::tests::a_secret_under_another_name_reads_that_variable`; "claude-bearer" is `vendor::claude::tests::an_http_remote_carries_its_type_and_a_secret_bearer_header_as_a_reference`; "claude-whole" is `vendor::claude::tests::a_secret_header_with_no_scheme_is_the_reference_alone`; "claude-sse" is `vendor::claude::tests::an_sse_remote_is_written_with_its_type`; "codex-stdio" is `vendor::codex::tests::a_stdio_server_forwards_a_secret_by_name_and_writes_literals_only`; "codex-all-secret" is `vendor::codex::tests::a_server_whose_env_is_all_secret_gets_no_env_table`; "codex-bearer" is `vendor::codex::tests::a_secret_bearer_authorization_is_the_bearer_token_variable`; "codex-whole" is `vendor::codex::tests::a_secret_header_with_no_scheme_is_read_whole_from_its_variable`; "codex-sse" is `vendor::codex::tests::an_sse_remote_is_refused_for_codex`; "codex-scheme" is `vendor::codex::tests::a_secret_header_codex_cannot_prefix_is_refused`; "codex-renamed" is `vendor::codex::tests::a_secret_under_another_name_is_refused_for_codex`; "codex-layout" is `vendor::codex::tests::codex_comments_and_layout_survive`; "codex-first" is `vendor::codex::tests::a_first_entry_in_a_file_with_no_servers_goes_last`; "codex-file" is `vendor::codex::tests::a_codex_file_fl_cannot_read_as_a_table_of_servers_is_refused`; "agy-stdio" is `vendor::antigravity::tests::a_stdio_server_leaves_a_secret_out_for_the_server_to_inherit`; "agy-all-secret" is `vendor::antigravity::tests::a_server_whose_env_is_all_secret_gets_no_env`; "agy-remote" is `vendor::antigravity::tests::a_remote_is_written_under_server_url_with_its_literal_headers`; "agy-sse" is `vendor::antigravity::tests::an_sse_remote_is_refused_for_antigravity`; "agy-header" is `vendor::antigravity::tests::any_secret_header_is_refused_for_antigravity`; "agy-renamed" is `vendor::antigravity::tests::a_secret_under_another_name_is_refused_for_antigravity`. Each mutation is one edit of the named file under `crates/mcp/src/vendor/`; save a copy first, restore it after each, and `cmp` against the copy.

`mod.rs`:
1. `vendor` maps each name to its own vendor: `VendorName::Codex => &Claude,` → targets red.
2. `Fields::list` leaves an empty list out: `if true {` for `if !values.is_empty() {` in `list` → claude-renamed red (`"args": []`).
3. the same mutation → codex-all-secret red (`args = []`).
4. `Fields::map` leaves an empty table out: `if true {` for `if !values.is_empty() {` in `map` → agy-all-secret red (`"env": {}`).
5. the same mutation → codex-all-secret red (`env = {}`).

`json.rs`:
6. bytes that are not UTF-8 are refused: `let text = &*String::from_utf8_lossy(bytes);` for the `from_utf8` line → jsonc red (the `\xff` file is read).
7. a byte-order mark is refused: add `let text = text.trim_start_matches('\u{feff}');` above the strict parse → jsonc red.
8. the top level is an object: `if false && !value.get().starts_with('{')` → shape red (`[]` reaches the `expect`).
9. a key twice at the top level: `top.twice.filter(|_| false)` → shape red.
10. `mcpServers` is an object: `Some(raw) if false && !raw.get().starts_with('{') =>` → shape red.
11. a server twice: `servers.twice.filter(|_| false)` → shape red.
12. `MembersVisitor` records the key seen twice: `if false && members.contains_key(&key)` → shape red (the second value replaces the first, unseen).
13. a missing file starts with `mcpServers`: delete `file.servers_mut();` in `open` → missing red.
14. `servers_mut` adds `mcpServers` to the top level: `let _ = empty;` for `self.top.insert(SERVERS.into(), empty);` → gains red.
15. `to_bytes` writes the servers in place of their old bytes: `Some(servers) if false && key == SERVERS =>` → foreign red.
16. `set` keeps a replaced entry's place: add `self.servers_mut().shift_remove(name);` before the insert → foreign red (`docs` moves last).
17. `remove` keeps the others' order: `swap_remove` for `shift_remove` → foreign red (`events` moves before `theirs`).
18. a rendered entry is indented to its depth: `text,` for `text: text.replace('\n', &format!("\n{ENTRY_INDENT}")),` → claude-sse red.
19. an empty object is `{}`: delete the `if lines.is_empty()` return in `object` → missing red.

`claude.rs`:
20. a secret env reads its variable, not its key: `format!("${{{key}}}")` → claude-renamed red.
21. a secret env is written as a reference: `var.to_string()` for `format!("${{{var}}}")` → claude-stdio red.
22. a literal env is its value: `EnvValue::Literal(v) => format!("${{{v}}}"),` → claude-stdio red.
23. a remote always carries `type`: delete `fields.text("type", server.transport.as_str());` → claude-bearer red.
24. the same mutation → claude-sse red.
25. `type` is the transport: `fields.text("type", "http");` → claude-sse red.
26. a secret header with a scheme is `<scheme> ${ENV}`: `} => format!("${{{env}}}{}", scheme.len() * 0),` → claude-bearer red.
27. with no scheme it is `${ENV}` alone: `format!("Bearer ${{{env}}}")` in the `scheme: None` arm → claude-whole red.
28. a literal header is its value: `HeaderValue::Literal(v) => format!("${{{v}}}"),` → claude-bearer red.

`codex.rs`:
29. SSE is refused: replace the `Transport::Sse` arm and the next line, `Transport::Http => {`, with `Transport::Http | Transport::Sse => {` → codex-sse red.
30. a secret read under another name is refused: `if false && var != key {` → codex-renamed red.
31. a secret is forwarded, not written: `literals.push((key.clone(), var.to_string()));` for `forwarded.push(var.to_string());` → codex-stdio red.
32. `bearer_token_env_var` is for `Authorization` only: `} if true` for `} if header.eq_ignore_ascii_case("authorization")` → codex-scheme red (`X-Auth` with `Bearer`).
33. … and for the scheme Bearer only: `&& !scheme.is_empty() =>` → codex-scheme red (`Token`).
34. the header's name in any case: `} if header == "Authorization"` → codex-bearer red (`authorization`).
35. the scheme in any case: `&& scheme == "Bearer" =>` → codex-bearer red (`bearer`).
36. the Bearer variable is written: replace the `if let Some(env) = bearer { … }` block with `let _ = bearer;` → codex-bearer red.
37. a secret header with no scheme is read whole: `literals.push((header.clone(), env.clone()));` in the `scheme: None` arm → codex-whole red.
38. any other scheme is refused: replace that arm's `return Err(…);` with `let _ = (header, scheme);` → codex-scheme red.
39. a literal header goes to `http_headers`: `from_env.push(…)` in the `HeaderValue::Literal` arm → codex-bearer red.
40. a file that is not UTF-8 is refused: `&*String::from_utf8_lossy(bytes);` for the `from_utf8` expression → codex-file red.
41. the parser's quoted line is not shown: `e.to_string().trim_end()` for `e.message().trim_end()` → codex-file red (`sk-live-4f9a` appears).
42. the line number is counted: `let line = 1 + at * 0;` → codex-file red (`line 3`).
43. `mcp_servers` is a table: `is_some_and(|item| false && !item.is_table())` → codex-file red.
44. … and an inline table is not one: `!item.is_table() && !item.is_inline_table()` → codex-file red.
45. `alone` leaves out what stands above the header: delete the `if let Item::Table(t) = &mut entry { … }` block → read-back red (an entry after another one carries its blank line).
46. a replaced entry keeps its position: delete `new.set_position(old.position());` → codex-layout red (`docs` moves above the profiles).
47. … and the lines above its header: delete `*new.decor_mut() = old.decor().clone();` → codex-layout red.
48. a new entry takes no position from its rendering: delete `new.set_position(None);` → codex-layout red.
49. the first entry of an empty file has no blank line above it: `set_prefix("\n")` → codex-stdio red.
50. any other new entry has one: `set_prefix("")` → codex-layout red.
51. `servers_mut` adds `mcp_servers` with no header of its own: `servers.set_implicit(false);` in `servers_mut` → codex-first red (a `[mcp_servers]` line).
52. `remove` removes: `s.get(name).is_some()` for `s.remove(name).is_some()` → add-remove red.

`antigravity.rs`:
53. SSE is refused: the same merge as 29 → agy-sse red.
54. a secret header is refused: replace the `HeaderValue::Secret` arm's `return Err(…);` with `let _ = header;` → agy-header red.
55. a secret read under another name is refused: `if false && var != key {` → agy-renamed red.
56. a secret variable is left out: add `literals.push((key.clone(), format!("${{{var}}}")));` above `if var != key {` → agy-stdio red.
57. the remote key is `serverUrl`: `fields.text("url", url(server));` → agy-remote red.
58. literal headers are written: `HeaderValue::Literal(_) => {}` → agy-remote red.

Targets and secrets:
59. Claude Code's file: `".mcp.json"` → `"mcp.json"` in `claude.rs` → targets red.
60. Codex's file: `".codex/config.toml"` → `".codex/config.json"` in `codex.rs` → targets red.
61. Antigravity's file: `".agents/mcp_config.json"` → `"mcp_config.json"` in `antigravity.rs` → targets red.
62. a vendor that wrote a variable's value: `std::env::var(var).unwrap_or_default()` for `format!("${{{var}}}")` in `claude.rs` → secret red.
63. a vendor that only read the environment: add `let _ = std::env::var_os("HOME");` above `let mut fields = Fields::default();` in `codex.rs` → secret red (the source search).

Not observable:
- Codex's `forwarded.push(var.to_string())` against the key: mutation 30's refusal makes the variable and the key the same name, so no input tells them apart.
- A JSON file with comments or trailing commas is refused by `serde_json`'s own strict parse; no line of ours decides it. Mutations 6 and 7 cover the two inputs our lines decide.
- Which reason shows when an entry breaks one vendor's rule twice (two secrets read under other names, two secret headers): the first in key order; not pinned.
- The `expect`s: `command` and `url` (the catalog requires them), `a secret names a variable` (`secret_var` is `Some` for a secret), `fl renders valid JSON`, `fl renders valid TOML`, `fl renders a table`, `strings always serialize`, `a strict JSON object` (the text parsed as strict JSON and starts with `{`), `checked when the file was read`. No input reaches them.
- `VendorRefusal`'s and `FileRefusal`'s wording: pinned by exact text in `a_refusal_names_the_vendor_the_server_the_reason_and_the_file`, shape and codex-file; not guards.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1280 passed, 19 ignored (1249 and 19 before this task).

- [ ] **Step 7: Commit**

```bash
git add crates/mcp/src/lib.rs crates/mcp/src/vendor/mod.rs crates/mcp/src/vendor/json.rs crates/mcp/src/vendor/claude.rs crates/mcp/src/vendor/codex.rs crates/mcp/src/vendor/antigravity.rs
git commit -m "feat(mcp): the vendor files: one writer per vendor, foreign entries kept

Each agent CLI gets its own project MCP file from the catalog: Claude
Code's .mcp.json, Codex's .codex/config.toml and Antigravity's
.agents/mcp_config.json. Each vendor renders a catalog entry in its own
shape, or refuses it for that vendor alone, naming the reason: Codex and
Antigravity have no SSE; Codex sends a secret header whole or as a
Bearer token and refuses any other scheme; Antigravity expands no
references, so a secret variable is left for the server to inherit and
a secret header is refused; a secret read under another name is refused
where the server inherits it by name. Claude Code writes a secret as a
reference to its variable and always gives a remote entry its type.
Nothing here reads the environment.

The JSON file keeps key order with an IndexMap of raw values, never
preserve_order: every entry fl does not touch, and every other key,
stays byte for byte, and a file that is not strict JSON (comments,
trailing commas, a byte-order mark) is refused, never rewritten.
Codex's file is edited with toml_edit, so its comments and layout
survive and a replaced entry keeps its place. An entry fl writes reads
back as the bytes it rendered, so it can be hashed. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 6: `sync`: the plan, ownership records, all or nothing

`fl mcp sync` writes every vendor file from the catalog, and `fl mcp check` asks whether it would (MCP spec §4.3, §4.5). This task adds the module `sync`, pure of git and of the CLI: it builds the entries the catalog and this machine's switches want in each vendor's file, classifies every name in every target by §4.3's table, and applies the result — all or nothing. "fl never changes or removes an entry it did not write" (§4.3): it knows its own entries by an **ownership record** per target (plan ruling 23), a JSON file named by the SHA-256 of the target's canonical path, holding each entry's name, its project root and the SHA-256 of the entry's bytes as written — Task 5's hash contract: `Rendered::bytes()` is exactly what `VendorFile::entry(name)` gives back once written. A record holds names, paths and hashes; fl writes only references (§2.1), so there is no secret value to put in one. Antigravity is a project target like the others (owner decision 10, plan ruling 3); there is no machine-wide row and no two-projects row (spec rev 2).

The desired entries: every server whose team default (`enabled`) holds, unless this machine's `[[mcp]]` switches turn it off (`disable`) or on (`enable`), rendered by every vendor in its `vendors` (default all three). A switch naming a server the catalog does not have is ignored with a warning naming it, carried in the plan, and the plan goes on (owner decision 15, §2.2 rev 2). A vendor that cannot run a server (Task 5's `VendorRefusal`) leaves it out of that vendor's file and is reported; it is not a refusal (§4.2), and `check` does not fail on it.

The classes, in the order they are tested: the entry in the file is **exactly what fl would write** → unchanged when the record holds its hash, else **adopt** (the record changes, the file does not: this recovers a crash between a rename and its record, and also a person who restored an entry by hand); fl wrote it and it **means what fl would write**, laid out anew — a formatter, or agy's panel, re-indented it or reordered its keys (the same JSON value, the same TOML table) → unchanged when the record holds its current hash, else **adopt**, recording the bytes as they stand (plan ruling 39); the entry is **as fl wrote it** (its hash is the record's) → **update**, or **remove** when the catalog no longer wants it here; fl wrote it, it is gone from the file and the catalog no longer wants it → **forget** (the record drops it; no file changes, so there is nothing to protect); wanted, in neither the file nor the record → **add**; fl wrote it and the **whole file is gone** (a fresh clone, `git clean -fdX`), still wanted → **add**: fl's own generated file is written anew (owner decision 14); in the file, not wanted, not fl's → **untouched**; every other case is **refused** — fl wrote it and it has changed since (*hand-edited*), fl wrote it and it is gone from a file that still exists, though still wanted (*removed*), or it is not fl's and differs from what fl would write (*someone else's*). `fl mcp sync --replace <name>` turns that name's refusal into a **replace**: fl's entry is written over it (or it is removed, when no longer wanted). A refusal names the file, the entry, the difference in brief, and the remedy (§4.3). The difference names the fields that differ, never a value — a hand-edited entry may hold a token someone pasted, and §6 allows no secret value in an error message.

A target that is a symbolic link, or whose path below the project root passes through a linked directory (`.codex`, `.agents`), stops the plan, naming the path: fl writes only plain files it can see whole (plan ruling 38). A link would carry the write into another file — one git tracks, which the gitignore guard cannot see because it asks about the link's own path, or one outside the project. The root is canonicalized first, so a project reached through a link is fine; only the components below it are checked.

All or nothing (§4.3): `plan` reads every target and its record and writes nothing; `apply` refuses a plan that holds a refusal, then takes the lock `<records>/sync.lock` with `std::fs::File::lock` — **waiting** for another `sync` to finish rather than refusing, since one records directory serves every project on the machine — and holds it to the end. Under the lock it reads every target it will touch again, and if any changed since the plan (`agy mcp add` or a person may have written it) it refuses and writes nothing at all; reading each one only just before its own write would leave the earlier targets written. Then each new file is staged beside its target (`tempfile::Builder::tempfile_in`), with the original's permission bits copied, or for a new file the mode `File::create` would give (`0o666` less the umask; a bare `NamedTempFile` is `0o600`); then each is renamed over its target, and its record written after the rename. `.codex/` and `.agents/` are created as needed. A sync with nothing to do takes no lock and writes nothing. `Plan::check` gives `Matches` (0), `Changes` (1: a file would change) or `Refused` (2); an error is 2 as well, which Task 7 maps. A change to a record alone (adopt, forget) is not a change to "every target matches the catalog" (§4.5).

**Blast radius:** `crates/mcp/src/lib.rs` gains `pub mod sync;`, four `McpError` variants (`VendorFile`, `Record`, `Refused`, `Changed`) after `NotNewer`, and a private helper `lines`; `Io`'s doc comment names its new op, `lock`; existing variants and their messages are unchanged. `crates/mcp/Cargo.toml` gains `sha2` (a workspace dependency fl-store and fl-github already use) and `tempfile` as a normal dependency (it stays a dev-dependency too); `Cargo.lock` gains one line, `"sha2"` under `fl-mcp`. No existing code changes.

**Files:**
- Modify: `crates/mcp/Cargo.toml` (`sha2`, `tempfile`)
- Modify: `Cargo.lock` (regenerated by cargo)
- Modify: `crates/mcp/src/lib.rs` (`pub mod sync`; the `McpError` variants)
- Create: `crates/mcp/src/sync.rs` (`Switches`, `Record`, `Owned`, `Plan`, `Target`, `Entry`, `Action`, `Refusal`, `RefusalKind`, `Check`, `plan`, `apply`, `LOCK`; tests)

**Interfaces:**
- Consumes (Task 1): `catalog::{Catalog, VendorName}`, `Catalog::path`, `Catalog::parse` (tests), `Server::enabled`, `Server::is_for`, `VendorName::ALL`. (Task 5): `vendor::vendor`, `Vendor::{title, target, render, open}`, `Rendered::bytes`, `VendorFile::{names, entry, set, remove, to_bytes}`, `VendorRefusal` (its `vendor`, `server` and Display), `FileRefusal`.
- Produces (`fl_mcp::sync`):
  - `pub const LOCK: &str = "sync.lock";`
  - `#[derive(Debug, Clone, Default, PartialEq, Eq)] pub struct Switches { pub enable: Vec<String>, pub disable: Vec<String> }` — Task 7 fills it from Task 2's `McpEntry` (none → `Switches::default()`); a name in both lists is refused by the config, and `disable` would win.
  - `pub fn plan(root: &Path, catalog: &Catalog, switches: &Switches, records: &Path, replace: &[String]) -> Result<Plan, McpError>` — reads only; `records` is the records directory (Task 7 passes `<fl_state_dir>/mcp`); a `replace` name refused nowhere changes nothing.
  - `pub fn apply(plan: &Plan) -> Result<(), McpError>` — `Refused` when the plan holds a refusal, `Changed` when a target changed since the plan; waits for the lock.
  - `#[derive(Debug, Clone)] pub struct Plan` (`Send`): `targets(&self) -> &[Target]` (Claude Code, Codex, Antigravity, in that order), `skipped(&self) -> &[VendorRefusal]`, `warnings(&self) -> &[String]` (a machine switch naming a server the catalog lacks; not a refusal), `refusals(&self) -> Vec<&Refusal>`, `check(&self) -> Check`; `Display` prints it (below).
  - `#[derive(Debug, Clone)] pub struct Target { pub vendor: VendorName, pub path: PathBuf /* root.join(target) */, pub entries: Vec<Entry>, .. }`, `changes(&self) -> bool` (the file would change). Entries: the file's names in its order, then the record's and the catalog's by name.
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Entry { pub name: String, pub action: Action }`
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum Action { Add, Update, Remove, Adopt, Forget, Replace { difference: String }, Unchanged, Untouched, Refused(Refusal) }`
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Refusal { pub path: PathBuf, pub name: String, pub kind: RefusalKind, pub difference: String }` with `Display`; `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum RefusalKind { HandEdited, Removed, Foreign }`.
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Check { Matches, Changes, Refused }`, `exit_code(self) -> u8` (0, 1, 2).
  - `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)] #[serde(deny_unknown_fields)] pub struct Record { pub target: PathBuf, pub entries: BTreeMap<String, Owned> }`, `pub struct Owned { pub root: PathBuf, pub sha256: String }` (same derives). The file: `<records>/<hex SHA-256 of the canonical target path's bytes>.json`, pretty JSON and a newline, e.g. `{"target": "<canonical target>", "entries": {"notes": {"root": "<canonical root>", "sha256": "<hex>"}}}`; mode `0o600` (tempfile's). The canonical path of a file that does not exist yet is its nearest existing ancestor's, canonical, joined with the rest.
  - `McpError` gains `VendorFile(#[from] vendor::FileRefusal)` (transparent; also a target reached through a link), `Record { path: PathBuf, cause: String }`, `Refused { refusals: Vec<sync::Refusal> }`, `Changed { path: PathBuf }`; `Io` gains the op `lock`.
- Plan output (`Display`, no trailing newline): per target `<title>, <path>:` then one line per change, `  add|update|remove <name>`, `  adopt <name> (means what fl would write)` (an exact match, or one laid out anew: plan ruling 39), `  forget <name> (removed by hand, no longer wanted)`, `  replace <name> (<difference>)`, `  refused <name>` — or `<title>, <path>: no change`; then each skipped `VendorRefusal`, then each refusal's sentence. The warnings are not in the `Display`; Task 7 prints them as `warning: …`.
- Unique phrases: refusals ``was changed by hand since fl wrote it``, ``was removed by hand since fl wrote it``, ``is an entry fl did not write, and it differs from what fl would write``; remedies ``Restore the entry, or run `fl mcp sync --replace <name>`, which shows the difference and overwrites it`` (removed: ``…, which writes it again``; someone else's: ``Rename or remove it, or run `fl mcp sync --replace <name>`, which shows the difference and overwrites it``); differences ``` `<field>` differs ```, `is only in the file`, `is only in fl's`, `only its layout or comments differ`, `it is not in the file`, `the catalog no longer has it here, so fl would remove it`, `its text differs`; the warning ``the `[[mcp]]` entry for this project in your fl config has `<name>` in `<list>`, and <catalog> has no such server; fl ignores it. Remove it from that entry``; errors ``it is a symbolic link, and fl writes only plain files it can see whole`` and ``<dir> is a symbolic link, and fl writes only plain files it can see whole`` (next ``Replace the link with a plain file or directory, then run `fl mcp sync` again``), `nothing was written:`, ``changed while fl was planning its write: another program or a person wrote it. Nothing was written; run `fl mcp sync` again``, `could not lock`.

- [ ] **Step 1: Write the failing tests**

In `crates/mcp/Cargo.toml`, under `[dependencies]`, after `tiny_http = { workspace = true, optional = true }` (the last line of the table) add:

```toml
sha2.workspace = true
tempfile.workspace = true
```

In `crates/mcp/src/lib.rs`, after `pub mod registry;` add:

```rust
pub mod sync;
```

Create `crates/mcp/src/sync.rs` holding the tests (the implementation goes above them in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Two servers every vendor can run: a stdio server with a secret and an
    /// http remote.
    const CATALOG: &str = r#"
[server.docs]
transport = "http"
url = "https://docs.example.com/mcp"

[server.notes]
transport = "stdio"
command = "npx"
args = ["-y", "@example/notes-mcp@1.2.0"]
env.NOTES_TOKEN = { secret = true }
"#;

    const PINNED: &str = "@example/notes-mcp@1.2.0";
    const UPGRADED: &str = "@example/notes-mcp@1.3.0";

    /// A project root and a records directory, both in one temporary
    /// directory, never the real home.
    struct Fixture {
        dir: TempDir,
        root: PathBuf,
        records: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        fs::create_dir(&root).unwrap();
        let records = dir.path().join("state").join("fl").join("mcp");
        Fixture { dir, root, records }
    }

    fn catalog(text: &str) -> Catalog {
        Catalog::parse(text, Path::new(".fl/mcp.toml")).unwrap()
    }

    fn off(names: &[&str]) -> Switches {
        Switches {
            disable: names.iter().map(|n| n.to_string()).collect(),
            ..Switches::default()
        }
    }

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    impl Fixture {
        fn plan(&self, text: &str) -> Plan {
            self.plan_with(text, &Switches::default(), &[])
        }

        fn plan_with(&self, text: &str, switches: &Switches, replace: &[&str]) -> Plan {
            let replace: Vec<String> = replace.iter().map(|n| n.to_string()).collect();
            plan(
                &self.root,
                &catalog(text),
                switches,
                &self.records,
                &replace,
            )
            .unwrap()
        }

        fn sync(&self, text: &str) {
            apply(&self.plan(text)).unwrap();
        }

        fn path(&self, v: VendorName) -> PathBuf {
            self.root.join(vendor::vendor(v).target())
        }

        fn read(&self, v: VendorName) -> Option<String> {
            fs::read_to_string(self.path(v)).ok()
        }

        fn write(&self, v: VendorName, text: &str) {
            let path = self.path(v);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        /// A person's edit of the file: `from` must be in it.
        fn edit(&self, v: VendorName, from: &str, to: &str) {
            let text = self.read(v).unwrap();
            assert!(text.contains(from), "{from} not in {text}");
            self.write(v, &text.replacen(from, to, 1));
        }

        /// The entry as it stands in the file.
        fn entry(&self, v: VendorName, name: &str) -> Option<Vec<u8>> {
            let bytes = fs::read(self.path(v)).ok();
            let file = vendor::vendor(v).open(&self.path(v), bytes.as_deref());
            file.unwrap().entry(name)
        }

        /// Every file in the fixture but the lock, with its bytes.
        fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
            fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
                for entry in fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else if path.file_name().unwrap() != LOCK {
                        out.push((path.clone(), fs::read(&path).unwrap()));
                    }
                }
            }
            let mut out = Vec::new();
            walk(self.dir.path(), &mut out);
            out.sort();
            out
        }

        fn record(&self, v: VendorName) -> Option<serde_json::Value> {
            let canonical = fs::canonicalize(self.path(v)).unwrap();
            let name = format!("{}.json", sha(canonical.as_os_str().as_encoded_bytes()));
            let text = fs::read_to_string(self.records.join(name)).ok()?;
            Some(serde_json::from_str(&text).unwrap())
        }
    }

    /// What fl writes for `name` from `text`'s catalog.
    fn rendered(v: VendorName, text: &str, name: &str) -> Vec<u8> {
        let catalog = catalog(text);
        let entry = vendor::vendor(v).render(name, &catalog.servers[name]);
        entry.unwrap().bytes().to_vec()
    }

    /// The target's actions by entry name.
    fn actions(plan: &Plan, v: VendorName) -> Vec<(String, Action)> {
        let target = plan.targets().iter().find(|t| t.vendor == v).unwrap();
        let entries = target.entries.iter();
        entries
            .map(|e| (e.name.clone(), e.action.clone()))
            .collect()
    }

    fn each(action: Action) -> Vec<(String, Action)> {
        vec![("docs".into(), action.clone()), ("notes".into(), action)]
    }

    fn refusal(action: &Action) -> String {
        match action {
            Action::Refused(r) => r.to_string(),
            other => panic!("not a refusal: {other:?}"),
        }
    }

    #[test]
    fn a_desired_entry_absent_from_the_file_and_the_record_is_added() {
        let f = fixture();
        let plan = f.plan(CATALOG);
        for v in VendorName::ALL {
            assert_eq!(actions(&plan, v), each(Action::Add), "{v:?}");
        }
        apply(&plan).unwrap();
        for v in VendorName::ALL {
            for name in ["docs", "notes"] {
                let expected = rendered(v, CATALOG, name);
                assert_eq!(f.entry(v, name), Some(expected.clone()), "{v:?} {name}");
                let record = f.record(v).unwrap();
                assert_eq!(record["entries"][name]["sha256"], sha(&expected));
            }
        }
    }

    #[test]
    fn an_entry_fl_wrote_is_updated_when_the_catalog_changes() {
        let f = fixture();
        f.sync(CATALOG);
        let upgraded = CATALOG.replace(PINNED, UPGRADED);
        let plan = f.plan(&upgraded);
        for v in VendorName::ALL {
            let expected = vec![
                ("docs".into(), Action::Unchanged),
                ("notes".into(), Action::Update),
            ];
            assert_eq!(actions(&plan, v), expected, "{v:?}");
        }
        apply(&plan).unwrap();
        for v in VendorName::ALL {
            let expected = rendered(v, &upgraded, "notes");
            assert_eq!(f.entry(v, "notes"), Some(expected.clone()), "{v:?}");
            assert_eq!(
                f.record(v).unwrap()["entries"]["notes"]["sha256"],
                sha(&expected)
            );
        }
    }

    #[test]
    fn an_entry_fl_wrote_is_removed_when_the_catalog_no_longer_has_it() {
        let f = fixture();
        f.sync(CATALOG);
        let docs_only = CATALOG.split("[server.notes]").next().unwrap();
        let plan = f.plan(docs_only);
        for v in VendorName::ALL {
            let expected = vec![
                ("docs".into(), Action::Unchanged),
                ("notes".into(), Action::Remove),
            ];
            assert_eq!(actions(&plan, v), expected, "{v:?}");
        }
        apply(&plan).unwrap();
        for v in VendorName::ALL {
            assert_eq!(f.entry(v, "notes"), None, "{v:?}");
            assert!(f.entry(v, "docs").is_some(), "{v:?}");
            let record = f.record(v).unwrap();
            assert!(record["entries"].get("notes").is_none(), "{record}");
        }
    }

    #[test]
    fn a_hand_edited_entry_is_refused_naming_the_remedy() {
        let f = fixture();
        f.sync(CATALOG);
        f.edit(VendorName::Claude, PINNED, "@example/notes-mcp@1.2.1");
        let before = f.snapshot();
        let plan = f.plan(CATALOG);
        let claude = actions(&plan, VendorName::Claude);
        assert_eq!(claude[0], ("docs".into(), Action::Unchanged));
        assert_eq!(
            refusal(&claude[1].1),
            format!(
                "{}: `notes` was changed by hand since fl wrote it (`args` differs). Restore \
                 the entry, or run `fl mcp sync --replace notes`, which shows the difference \
                 and overwrites it",
                f.path(VendorName::Claude).display()
            )
        );
        assert_eq!(plan.check(), Check::Refused);
        let err = apply(&plan).unwrap_err();
        assert!(matches!(err, McpError::Refused { .. }), "{err:?}");
        assert!(err.to_string().contains("nothing was written"), "{err}");
        assert!(err.to_string().contains("was changed by hand"), "{err}");
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn an_entry_removed_by_hand_is_refused_unless_the_catalog_no_longer_wants_it() {
        let f = fixture();
        f.sync(CATALOG);
        let codex = vendor::vendor(VendorName::Codex);
        let path = f.path(VendorName::Codex);
        let mut file = codex.open(&path, Some(&fs::read(&path).unwrap())).unwrap();
        assert!(file.remove("notes"));
        fs::write(&path, file.to_bytes()).unwrap();

        let plan = f.plan(CATALOG);
        assert_eq!(
            refusal(&actions(&plan, VendorName::Codex)[1].1),
            format!(
                "{}: `notes` was removed by hand since fl wrote it. Restore the entry, or run \
                 `fl mcp sync --replace notes`, which writes it again",
                path.display()
            )
        );

        let docs_only = CATALOG.split("[server.notes]").next().unwrap();
        let plan = f.plan(docs_only);
        assert_eq!(
            actions(&plan, VendorName::Codex)[1],
            ("notes".into(), Action::Forget)
        );
        assert_eq!(
            actions(&plan, VendorName::Claude)[1],
            ("notes".into(), Action::Remove)
        );
        apply(&plan).unwrap();
        let record = f.record(VendorName::Codex).unwrap();
        assert!(record["entries"].get("notes").is_none(), "{record}");
        assert!(f.plan(docs_only).targets().iter().all(|t| !t.changes()));
    }

    #[test]
    fn someone_elses_entry_under_a_desired_name_is_refused() {
        let f = fixture();
        let theirs = "{\n  \"mcpServers\": {\n    \"notes\": {\"command\": \"notes-mcp\", \
                      \"timeout\": 30}\n  }\n}\n";
        f.write(VendorName::Claude, theirs);
        let plan = f.plan(CATALOG);
        let claude = actions(&plan, VendorName::Claude);
        assert_eq!(claude[0].0, "notes");
        assert_eq!(claude[1], ("docs".into(), Action::Add));
        assert_eq!(
            refusal(&claude[0].1),
            format!(
                "{}: `notes` is an entry fl did not write, and it differs from what fl would \
                 write (`args` is only in fl's, `command` differs, `env.NOTES_TOKEN` is only \
                 in fl's, `timeout` is only in the file). Rename or remove it, or run `fl mcp \
                 sync --replace notes`, which shows the difference and overwrites it",
                f.path(VendorName::Claude).display()
            )
        );
        assert_eq!(actions(&plan, VendorName::Codex), each(Action::Add));
    }

    #[test]
    fn an_exact_match_not_in_the_record_is_adopted() {
        let f = fixture();
        f.sync(CATALOG);
        let synced = f.snapshot();
        fs::remove_dir_all(&f.records).unwrap();
        let plan = f.plan(CATALOG);
        for v in VendorName::ALL {
            assert_eq!(actions(&plan, v), each(Action::Adopt), "{v:?}");
        }
        assert_eq!(plan.check(), Check::Matches);
        apply(&plan).unwrap();
        assert_eq!(f.snapshot(), synced);
    }

    #[test]
    fn a_crash_between_the_write_and_its_record_is_recovered_by_adopting() {
        let f = fixture();
        let plan = f.plan(CATALOG);
        // A directory where the Claude Code record goes: the record cannot be
        // written, as if fl stopped right after the rename.
        let canonical = fs::canonicalize(&f.root).unwrap().join(".mcp.json");
        let blocked = record_file(&f.records, &canonical);
        fs::create_dir_all(&blocked).unwrap();
        let err = apply(&plan).unwrap_err();
        assert!(err.to_string().contains("could not write"), "{err}");
        assert_eq!(
            f.entry(VendorName::Claude, "notes"),
            Some(rendered(VendorName::Claude, CATALOG, "notes"))
        );

        fs::remove_dir(&blocked).unwrap();
        let plan = f.plan(CATALOG);
        assert_eq!(actions(&plan, VendorName::Claude), each(Action::Adopt));
        assert_eq!(actions(&plan, VendorName::Codex), each(Action::Add));
        apply(&plan).unwrap();
        let plan = f.plan(CATALOG);
        for v in VendorName::ALL {
            assert_eq!(actions(&plan, v), each(Action::Unchanged), "{v:?}");
        }
    }

    #[test]
    fn an_entry_fl_did_not_write_and_does_not_want_is_left_untouched() {
        let f = fixture();
        let theirs = "{\n  \"mcpServers\": {\n    \"theirs\": {\"command\": \"theirs-mcp\"}\n  \
                      },\n  \"other\": 1\n}\n";
        f.write(VendorName::Claude, theirs);
        let foreign = "# Mine.\n[mcp_servers.theirs]\ncommand = \"theirs-mcp\"  # keep\n";
        f.write(VendorName::Codex, foreign);
        let kept = |f: &Fixture| {
            (
                f.entry(VendorName::Claude, "theirs"),
                f.entry(VendorName::Codex, "theirs"),
            )
        };
        let before = kept(&f);

        let plan = f.plan(CATALOG);
        assert_eq!(
            actions(&plan, VendorName::Claude)[0],
            ("theirs".into(), Action::Untouched)
        );
        apply(&plan).unwrap();
        assert_eq!(kept(&f), before);
        assert!(f.read(VendorName::Claude).unwrap().contains("\"other\": 1"));

        let plan = f.plan("");
        let expected = vec![
            ("theirs".into(), Action::Untouched),
            ("docs".into(), Action::Remove),
            ("notes".into(), Action::Remove),
        ];
        assert_eq!(actions(&plan, VendorName::Codex), expected);
        apply(&plan).unwrap();
        assert_eq!(kept(&f), before);
        assert_eq!(f.read(VendorName::Codex).unwrap(), foreign);
    }

    #[test]
    fn a_file_changed_between_plan_and_write_is_refused_and_nothing_is_written() {
        let f = fixture();
        f.sync(CATALOG);
        let plan = f.plan(&CATALOG.replace(PINNED, UPGRADED));
        assert!(plan.targets().iter().all(|t| t.changes()));
        // `agy mcp add`, or a person, writes the last target after the plan.
        f.edit(
            VendorName::Antigravity,
            "\"mcpServers\": {",
            "\"mcpServers\": {\"x\": {},",
        );
        let before = f.snapshot();
        let err = apply(&plan).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "{} changed while fl was planning its write: another program or a person \
                 wrote it. Nothing was written; run `fl mcp sync` again",
                f.path(VendorName::Antigravity).display()
            )
        );
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn one_refusal_writes_nothing_in_any_of_the_three_files() {
        let f = fixture();
        let theirs = "[mcp_servers.notes]\ncommand = \"notes-mcp\"\n";
        f.write(VendorName::Codex, theirs);
        let before = f.snapshot();
        let plan = f.plan(CATALOG);
        assert_eq!(actions(&plan, VendorName::Claude), each(Action::Add));
        assert_eq!(actions(&plan, VendorName::Antigravity), each(Action::Add));
        assert!(matches!(
            actions(&plan, VendorName::Codex)[0].1,
            Action::Refused(_)
        ));
        assert!(apply(&plan).is_err());
        assert_eq!(f.snapshot(), before);
        assert_eq!(f.read(VendorName::Claude), None);
        assert_eq!(f.read(VendorName::Antigravity), None);
    }

    #[test]
    fn replace_overwrites_only_the_named_entry() {
        let f = fixture();
        f.sync(CATALOG);
        let docs = "https://docs.example.com/mcp";
        f.edit(VendorName::Claude, PINNED, "@example/notes-mcp@1.2.1");
        f.edit(VendorName::Claude, docs, "https://docs.example.com/v2");
        f.edit(
            VendorName::Claude,
            "{\n    \"docs\"",
            "{\n    \"theirs\": {},\n    \"docs\"",
        );
        let theirs = f.entry(VendorName::Claude, "theirs");

        let plan = f.plan_with(CATALOG, &Switches::default(), &["notes"]);
        let claude = actions(&plan, VendorName::Claude);
        assert_eq!(claude[0], ("theirs".into(), Action::Untouched));
        assert!(refusal(&claude[1].1).contains("`docs` was changed by hand"));
        let difference = "`args` differs".to_string();
        assert_eq!(claude[2], ("notes".into(), Action::Replace { difference }));
        assert_eq!(plan.check(), Check::Refused);

        f.edit(VendorName::Claude, "https://docs.example.com/v2", docs);
        let edited_docs = f.entry(VendorName::Claude, "docs");
        let plan = f.plan_with(CATALOG, &Switches::default(), &["notes"]);
        apply(&plan).unwrap();
        let notes = rendered(VendorName::Claude, CATALOG, "notes");
        assert_eq!(f.entry(VendorName::Claude, "notes"), Some(notes.clone()));
        assert_eq!(f.entry(VendorName::Claude, "docs"), edited_docs);
        assert_eq!(f.entry(VendorName::Claude, "theirs"), theirs);
        let record = f.record(VendorName::Claude).unwrap();
        assert_eq!(record["entries"]["notes"]["sha256"], sha(&notes));
    }

    #[test]
    fn a_disabled_server_is_removed_from_every_vendor_file_it_was_in() {
        let f = fixture();
        f.sync(CATALOG);
        let plan = f.plan_with(CATALOG, &off(&["notes"]), &[]);
        for v in VendorName::ALL {
            assert_eq!(
                actions(&plan, v)[1],
                ("notes".into(), Action::Remove),
                "{v:?}"
            );
        }
        apply(&plan).unwrap();
        for v in VendorName::ALL {
            assert_eq!(f.entry(v, "notes"), None, "{v:?}");
            assert!(f.entry(v, "docs").is_some(), "{v:?}");
        }
    }

    #[test]
    fn the_machine_switches_turn_a_server_on_or_off_here() {
        let f = fixture();
        let docs_off = CATALOG.replace(
            "transport = \"http\"",
            "enabled = false\ntransport = \"http\"",
        );
        let names = |plan: &Plan| -> Vec<String> {
            actions(plan, VendorName::Claude)
                .into_iter()
                .map(|(n, _)| n)
                .collect()
        };
        assert_eq!(names(&f.plan(&docs_off)), ["notes"]);
        let on = Switches {
            enable: vec!["docs".into()],
            ..Switches::default()
        };
        assert_eq!(names(&f.plan_with(&docs_off, &on, &[])), ["docs", "notes"]);
        assert_eq!(
            names(&f.plan_with(CATALOG, &off(&["notes"]), &[])),
            ["docs"]
        );
        assert_eq!(names(&f.plan_with(CATALOG, &on, &[])), ["docs", "notes"]);
    }

    // Owner decision 15: a switch naming a server the catalog no longer has
    // is ignored, with a warning; the rest of the plan goes on.
    #[test]
    fn a_machine_switch_naming_an_unknown_server_is_a_warning() {
        let f = fixture();
        let switches = Switches {
            enable: vec!["sentry".into()],
            disable: vec!["notes".into(), "old".into()],
        };
        let plan = f.plan_with(CATALOG, &switches, &[]);
        assert_eq!(
            plan.warnings(),
            [
                format!(
                    "the `[[mcp]]` entry for this project in your fl config has `sentry` in \
                     `enable`, and {} has no such server; fl ignores it. Remove it from that \
                     entry",
                    f.root.join(".fl/mcp.toml").display()
                ),
                format!(
                    "the `[[mcp]]` entry for this project in your fl config has `old` in \
                     `disable`, and {} has no such server; fl ignores it. Remove it from that \
                     entry",
                    f.root.join(".fl/mcp.toml").display()
                ),
            ]
        );
        let docs_only = vec![("docs".to_string(), Action::Add)];
        assert_eq!(actions(&plan, VendorName::Claude), docs_only);
        assert_eq!(plan.check(), Check::Changes);
        assert!(f.plan(CATALOG).warnings().is_empty());
    }

    // Owner decision 14: after a fresh clone or `git clean -X`, fl's own
    // files are gone as a whole; `sync` writes them anew from the catalog.
    #[test]
    fn a_vendor_file_gone_as_a_whole_is_written_anew() {
        let f = fixture();
        f.sync(CATALOG);
        for v in VendorName::ALL {
            fs::remove_file(f.path(v)).unwrap();
        }
        let docs_only = CATALOG.split("[server.notes]").next().unwrap();
        let plan = f.plan(docs_only);
        for v in VendorName::ALL {
            let expected = vec![
                ("docs".to_string(), Action::Add),
                ("notes".to_string(), Action::Forget),
            ];
            assert_eq!(actions(&plan, v), expected, "{v:?}");
        }
        assert_eq!(plan.check(), Check::Changes);
        apply(&plan).unwrap();
        for v in VendorName::ALL {
            assert_eq!(f.entry(v, "docs").unwrap(), rendered(v, CATALOG, "docs"));
            let record = f.record(v).unwrap();
            let names: Vec<&String> = record["entries"].as_object().unwrap().keys().collect();
            assert_eq!(names, ["docs"], "{v:?}");
        }
        assert_eq!(f.plan(docs_only).check(), Check::Matches);
    }

    // A formatter, or agy's own panel, may lay fl's entry out anew: the same
    // meaning is adopted, not refused (a changed value still is).
    #[test]
    fn fl_s_entry_laid_out_anew_with_the_same_meaning_is_adopted() {
        let f = fixture();
        f.sync(CATALOG);
        let claude = f.read(VendorName::Claude).unwrap();
        let compact =
            serde_json::to_string(&serde_json::from_str::<serde_json::Value>(&claude).unwrap());
        f.write(VendorName::Claude, &compact.unwrap());
        let codex = f.read(VendorName::Codex).unwrap();
        let args = "args = [\"-y\", \"@example/notes-mcp@1.2.0\"]\n";
        let reordered = codex.replacen(args, "", 1).replacen(
            "[mcp_servers.notes]\n",
            &format!("[mcp_servers.notes]\n{args}"),
            1,
        );
        assert_ne!(reordered, codex);
        f.write(VendorName::Codex, &reordered);
        let plan = f.plan(CATALOG);
        assert_eq!(actions(&plan, VendorName::Claude), each(Action::Adopt));
        assert_eq!(
            actions(&plan, VendorName::Codex),
            vec![
                ("docs".to_string(), Action::Unchanged),
                ("notes".to_string(), Action::Adopt)
            ]
        );
        assert_eq!(plan.check(), Check::Matches);
        assert!(
            plan.to_string()
                .contains("\n  adopt notes (means what fl would write)\n"),
            "{plan}"
        );
        apply(&plan).unwrap();
        assert_eq!(
            f.read(VendorName::Codex).unwrap(),
            reordered,
            "the file is kept"
        );
        let again = f.plan(CATALOG);
        assert_eq!(actions(&again, VendorName::Claude), each(Action::Unchanged));
        assert!(
            again
                .targets()
                .iter()
                .all(|t| t.record.is_none() && !t.changes())
        );
        // A change of meaning in the new layout is still refused.
        f.edit(VendorName::Codex, "@example/notes-mcp@1.2.0", UPGRADED);
        let plan = f.plan(CATALOG);
        assert!(refusal(&actions(&plan, VendorName::Codex)[1].1).contains("`args` differs"));
    }

    // A vendor file that is a link, or lies under a linked directory, could
    // carry fl's write into a file it cannot see whole — one git tracks, or
    // one outside the project: refused, naming the path, and nothing is
    // written. The project root itself may be reached through a link.
    #[test]
    fn a_vendor_file_reached_through_a_link_is_refused() {
        let f = fixture();
        let refused = |root: &Path| {
            let plan = plan(
                root,
                &catalog(CATALOG),
                &Switches::default(),
                &f.records,
                &[],
            );
            plan.unwrap_err().to_string()
        };
        let next = "Replace the link with a plain file or directory, then run `fl mcp sync` again";
        // In the project: a file someone shares, through `.mcp.json`.
        fs::create_dir(f.root.join("docs")).unwrap();
        let shared = f.root.join("docs/shared.json");
        fs::write(&shared, "{\"mcpServers\": {}}\n").unwrap();
        std::os::unix::fs::symlink("docs/shared.json", f.path(VendorName::Claude)).unwrap();
        assert_eq!(
            refused(&f.root),
            format!(
                "{}: it is a symbolic link, and fl writes only plain files it can see whole. \
                 {next}",
                f.path(VendorName::Claude).display()
            )
        );
        fs::remove_file(f.path(VendorName::Claude)).unwrap();
        // A linked directory in the project.
        fs::create_dir_all(f.root.join("gen/agents")).unwrap();
        std::os::unix::fs::symlink("gen/agents", f.root.join(".agents")).unwrap();
        assert_eq!(
            refused(&f.root),
            format!(
                "{}: {} is a symbolic link, and fl writes only plain files it can see whole. \
                 {next}",
                f.path(VendorName::Antigravity).display(),
                f.root.join(".agents").display()
            )
        );
        fs::remove_file(f.root.join(".agents")).unwrap();
        // A linked directory that leads outside.
        let elsewhere = f.dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, f.root.join(".codex")).unwrap();
        assert!(refused(&f.root).contains(&format!(
            "{} is a symbolic link, and fl writes only plain files it can see whole",
            f.root.join(".codex").display()
        )));
        fs::remove_file(f.root.join(".codex")).unwrap();
        assert_eq!(
            fs::read_to_string(&shared).unwrap(),
            "{\"mcpServers\": {}}\n"
        );
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
        assert!(!f.records.exists());
        // The project root itself, through a link: fine.
        let link = f.dir.path().join("app-link");
        std::os::unix::fs::symlink(&f.root, &link).unwrap();
        let plan = plan(
            &link,
            &catalog(CATALOG),
            &Switches::default(),
            &f.records,
            &[],
        );
        apply(&plan.unwrap()).unwrap();
        assert!(f.read(VendorName::Claude).unwrap().contains("notes"));
    }

    #[test]
    fn check_is_0_when_everything_matches_1_with_changes_2_on_a_refusal() {
        let f = fixture();
        let empty = f.plan("");
        assert_eq!(empty.check(), Check::Matches);
        // Nothing to do writes nothing, not even the lock.
        apply(&empty).unwrap();
        assert!(!f.records.exists());
        let plan = f.plan(CATALOG);
        assert_eq!(plan.check(), Check::Changes);
        assert_eq!(plan.check().exit_code(), 1);
        apply(&plan).unwrap();
        let plan = f.plan(CATALOG);
        assert_eq!(plan.check(), Check::Matches);
        assert_eq!(plan.check().exit_code(), 0);
        assert_eq!(
            f.plan(&CATALOG.replace(PINNED, UPGRADED)).check(),
            Check::Changes
        );
        f.edit(VendorName::Codex, PINNED, "@example/notes-mcp@1.2.1");
        let plan = f.plan(CATALOG);
        assert_eq!(plan.check(), Check::Refused);
        assert_eq!(plan.check().exit_code(), 2);
    }

    #[test]
    fn the_plan_prints_each_target_its_actions_and_each_refusal() {
        let f = fixture();
        f.sync(CATALOG);
        f.edit(VendorName::Codex, PINNED, "@example/notes-mcp@1.2.1");
        let next = CATALOG
            .replace(PINNED, UPGRADED)
            .replace("[server.docs]", "[server.gone]")
            + "\n[server.events]\ntransport = \"sse\"\nurl = \"https://events.example.com/sse\"\n";
        let plan = f.plan(&next);
        let path = |v| f.path(v).display().to_string();
        let expected = format!(
            "Claude Code, {claude}:\n  remove docs\n  update notes\n  add events\n  add gone\n\
             Codex, {codex}:\n  remove docs\n  refused notes\n  add gone\n\
             Antigravity, {agy}:\n  remove docs\n  update notes\n  add gone\n\
             Codex cannot run server `events`: Codex connects to streamable HTTP servers only, \
             not SSE. It is left out of .codex/config.toml; the other vendors still get it. To \
             say so in the catalog, give the server a `vendors` list without `codex`\n\
             Antigravity cannot run server `events`: Antigravity does not support the legacy \
             SSE transport; only a streamable HTTP endpoint or a stdio server. It is left out \
             of .agents/mcp_config.json; the other vendors still get it. To say so in the \
             catalog, give the server a `vendors` list without `antigravity`\n\
             {codex}: `notes` was changed by hand since fl wrote it (`args` differs). Restore \
             the entry, or run `fl mcp sync --replace notes`, which shows the difference and \
             overwrites it",
            claude = path(VendorName::Claude),
            codex = path(VendorName::Codex),
            agy = path(VendorName::Antigravity),
        );
        assert_eq!(plan.to_string(), expected);
        assert_eq!(plan.check(), Check::Refused);
        f.edit(VendorName::Codex, "@example/notes-mcp@1.2.1", PINNED);
        let plan = f.plan(CATALOG);
        let expected = format!(
            "Claude Code, {}: no change\nCodex, {}: no change\nAntigravity, {}: no change",
            path(VendorName::Claude),
            path(VendorName::Codex),
            path(VendorName::Antigravity)
        );
        assert_eq!(plan.to_string(), expected);
    }

    #[test]
    fn a_vendor_that_cannot_run_a_server_skips_it_and_the_others_still_get_it() {
        let f = fixture();
        let text = r#"
[server.events]
transport = "sse"
url = "https://events.example.com/sse"

[server.local]
vendors = ["codex"]
transport = "stdio"
command = "local-mcp"
"#;
        let plan = f.plan(text);
        let add = |name: &str| vec![(name.to_string(), Action::Add)];
        assert_eq!(actions(&plan, VendorName::Claude), add("events"));
        assert_eq!(actions(&plan, VendorName::Codex), add("local"));
        assert_eq!(actions(&plan, VendorName::Antigravity), vec![]);
        let skipped: Vec<_> = plan
            .skipped()
            .iter()
            .map(|r| (r.vendor, r.server.as_str()))
            .collect();
        assert_eq!(
            skipped,
            [
                (VendorName::Codex, "events"),
                (VendorName::Antigravity, "events")
            ]
        );
        assert_eq!(plan.check(), Check::Changes);
        apply(&plan).unwrap();
        assert_eq!(f.read(VendorName::Antigravity), None);
        // A record for Claude Code and Codex, none for an untouched target.
        assert_eq!(fs::read_dir(&f.records).unwrap().count(), 3);
        assert_eq!(f.plan(text).check(), Check::Matches);
    }

    #[test]
    fn a_record_holds_each_entry_s_name_root_and_hash_only() {
        let f = fixture();
        f.sync(CATALOG);
        let root = fs::canonicalize(&f.root).unwrap();
        let target = root.join(".mcp.json");
        let hash = |name| sha(&rendered(VendorName::Claude, CATALOG, name));
        assert_eq!(
            f.record(VendorName::Claude).unwrap(),
            serde_json::json!({
                "target": target,
                "entries": {
                    "docs": { "root": root, "sha256": hash("docs") },
                    "notes": { "root": root, "sha256": hash("notes") },
                }
            })
        );
        let mut names: Vec<_> = fs::read_dir(&f.records)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names.len(), 4, "{names:?}");
        assert_eq!(names.last().unwrap(), LOCK);
    }

    #[cfg(unix)]
    #[test]
    fn the_record_is_keyed_by_the_canonical_path() {
        let f = fixture();
        f.sync(CATALOG);
        let link = f.dir.path().join("link");
        std::os::unix::fs::symlink(&f.root, &link).unwrap();
        let through = |text: &str| {
            plan(&link, &catalog(text), &Switches::default(), &f.records, &[]).unwrap()
        };
        let same = through(CATALOG);
        for v in VendorName::ALL {
            assert_eq!(actions(&same, v), each(Action::Unchanged), "{v:?}");
        }
        apply(&through(&CATALOG.replace(PINNED, UPGRADED))).unwrap();
        let root = fs::canonicalize(&f.root).unwrap();
        let record = f.record(VendorName::Claude).unwrap();
        assert_eq!(record["entries"]["notes"]["root"], root.to_str().unwrap());
    }

    #[test]
    fn a_record_fl_cannot_read_is_refused_naming_the_remedy() {
        let f = fixture();
        f.sync(CATALOG);
        let canonical = fs::canonicalize(f.path(VendorName::Codex)).unwrap();
        let file = record_file(&f.records, &canonical);
        let unknown = r#"{"target": "x", "entries": {}, "project": "app"}"#;
        let inner = r#"{"target": "x", "entries": {"a": {"root": "r", "sha256": "0", "v": 1}}}"#;
        for text in ["{", unknown, inner] {
            fs::write(&file, text).unwrap();
            let err = plan(
                &f.root,
                &catalog(CATALOG),
                &Switches::default(),
                &f.records,
                &[],
            );
            let err = err.unwrap_err().to_string();
            let head = format!(
                "{} is not an ownership record fl can read: ",
                file.display()
            );
            assert!(err.starts_with(&head), "{err}");
            let remedy = "Delete it; the next `fl mcp sync` adopts every entry that still \
                          matches the catalog";
            assert!(err.ends_with(remedy), "{err}");
        }
    }

    #[test]
    fn a_vendor_file_fl_cannot_read_stops_the_plan() {
        let f = fixture();
        f.write(VendorName::Claude, "{ // mine\n}\n");
        let err = plan(
            &f.root,
            &catalog(CATALOG),
            &Switches::default(),
            &f.records,
            &[],
        );
        let err = err.unwrap_err();
        assert!(matches!(err, McpError::VendorFile(_)), "{err:?}");
        assert!(err.to_string().contains("it is not strict JSON ("), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_original_file_mode_is_kept_and_a_new_file_gets_the_default_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let f = fixture();
        f.sync(CATALOG);
        let plain = f.dir.path().join("plain");
        fs::File::create(&plain).unwrap();
        for v in VendorName::ALL {
            assert_eq!(mode(&f.path(v)), mode(&plain), "{v:?}");
        }
        // A mode the umask would change, so only a copy keeps it.
        let claude = f.path(VendorName::Claude);
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o646)).unwrap();
        f.sync(&CATALOG.replace(PINNED, UPGRADED));
        assert_eq!(mode(&claude), 0o646);
    }

    #[test]
    fn a_second_sync_waits_while_another_holds_the_lock() {
        let f = fixture();
        let plan = f.plan(CATALOG);
        fs::create_dir_all(&f.records).unwrap();
        let held = fs::File::create(f.records.join(LOCK)).unwrap();
        held.lock().unwrap();
        let waiting = std::thread::spawn(move || apply(&plan));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!waiting.is_finished());
        assert_eq!(f.read(VendorName::Claude), None);
        drop(held);
        waiting.join().unwrap().unwrap();
        assert!(f.read(VendorName::Claude).is_some());
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-mcp --lib`
Expected: FAIL to compile (269 errors) — `error[E0425]`/`error[E0433]`/`error[E0422]`: cannot find `plan`, `apply`, `record_file`, `Switches`, `Plan`, `Action`, `Check`, `LOCK`, and `fs`, `Path`, `PathBuf`, `VendorName`, `Catalog`, `McpError`, `Sha256`, `vendor` (the tests take these from the implementation's `use` lines).

- [ ] **Step 3: Implement**

In `crates/mcp/src/lib.rs`, after the `NotNewer { … },` variant (the last one), add:

```rust
    /// A vendor file fl will not rewrite.
    #[error(transparent)]
    VendorFile(#[from] vendor::FileRefusal),
    /// An ownership record that is not one.
    #[error(
        "{} is not an ownership record fl can read: {cause}. Delete it; the next \
         `fl mcp sync` adopts every entry that still matches the catalog",
        path.display()
    )]
    Record { path: PathBuf, cause: String },
    /// `sync` refused one entry or more, so it wrote nothing (MCP spec §4.3).
    #[error("nothing was written:\n{}", lines(refusals))]
    Refused { refusals: Vec<sync::Refusal> },
    /// A target changed between the plan and the write (MCP spec §4.3).
    #[error(
        "{} changed while fl was planning its write: another program or a person wrote it. \
         Nothing was written; run `fl mcp sync` again",
        path.display()
    )]
    Changed { path: PathBuf },
```

and at the end of the file, after `fn missing` and one blank line, add:

```rust
fn lines(refusals: &[sync::Refusal]) -> String {
    let lines: Vec<String> = refusals.iter().map(ToString::to_string).collect();
    lines.join("\n")
}
```

and replace the first line of `Io`'s doc comment, `/// The file could not be read or written. `op` is `read` or `write`.`, with:

```rust
    /// The file could not be read, written or locked. `op` is `read`,
    /// `write` or `lock`.
```

In `crates/mcp/src/sync.rs`, above `#[cfg(test)]`, insert:

```rust
//! `fl mcp sync` and `fl mcp check` (MCP spec §4.3, §4.5): the entries the
//! catalog wants in each vendor file, a plan for every target, and its
//! application. fl changes and removes only the entries it wrote, and knows
//! them by an ownership record per target, kept on this machine and never in
//! a repository. Every target is planned before any is written, and one
//! refusal writes nothing. Nothing here runs git or reads the network.

use crate::McpError;
use crate::catalog::{Catalog, VendorName};
use crate::vendor::{self, Rendered, VendorRefusal};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// The lock file in the records directory, held for the whole of a write.
pub const LOCK: &str = "sync.lock";

/// This machine's switches for the project, from the `[[mcp]]` entry in the
/// user's config (MCP spec §2.2). A name the catalog does not have is
/// ignored, with a warning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Switches {
    /// On here, though the team default is off.
    pub enable: Vec<String>,
    /// Off here, though the team default is on. The config refuses a name
    /// in both lists; were one in both, `disable` would win.
    pub disable: Vec<String>,
}

/// The ownership record of one target: every entry fl wrote there, with the
/// project root it came from and the SHA-256 of its bytes as written. It
/// holds names, paths and hashes only; a secret's value is never in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The target's canonical path.
    pub target: PathBuf,
    pub entries: BTreeMap<String, Owned>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owned {
    /// The project root, canonical.
    pub root: PathBuf,
    /// Hex SHA-256 of the entry's bytes as fl wrote them.
    pub sha256: String,
}

/// What `sync` does to every target, worked out before anything is written.
#[derive(Debug, Clone)]
pub struct Plan {
    records: PathBuf,
    targets: Vec<Target>,
    skipped: Vec<VendorRefusal>,
    warnings: Vec<String>,
}

/// One vendor file and what happens to each name in it.
#[derive(Debug, Clone)]
pub struct Target {
    pub vendor: VendorName,
    /// The project root joined with the vendor's file.
    pub path: PathBuf,
    /// Every name in the file, the record or the catalog: the file's own
    /// order first, then the rest by name.
    pub entries: Vec<Entry>,
    canonical: PathBuf,
    /// The file as planned; `None` when it did not exist.
    before: Option<Vec<u8>>,
    /// The file to write; `None` when no entry in it changes.
    after: Option<Vec<u8>>,
    record_file: PathBuf,
    /// The record to write; `None` when it does not change.
    record: Option<Record>,
}

impl Target {
    /// Whether `sync` would change the file.
    pub fn changes(&self) -> bool {
        self.after.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub action: Action,
}

/// The classes of MCP spec §4.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Wanted; in neither the file nor the record, or the whole file is
    /// gone (a fresh clone, `git clean -X`) and fl writes it anew.
    Add,
    /// fl's, as fl wrote it, and the catalog wants it different.
    Update,
    /// fl's, as fl wrote it, and the catalog no longer wants it here.
    Remove,
    /// Exactly what fl would write, but not in the record: recovers a crash
    /// between a write and its record. Or fl's, laid out anew (by a
    /// formatter, or agy's panel) but meaning what fl would write. The
    /// record changes, the file does not.
    Adopt,
    /// fl's, removed by hand, and no longer wanted: only the record changes.
    Forget,
    /// A refusal `--replace` named: overwritten (or removed, when no longer
    /// wanted).
    Replace {
        difference: String,
    },
    /// fl's, and already what the catalog wants.
    Unchanged,
    /// Not fl's and not wanted: never touched.
    Untouched,
    Refused(Refusal),
}

/// An entry fl will not change, and why (MCP spec §4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub path: PathBuf,
    pub name: String,
    pub kind: RefusalKind,
    /// Which fields differ, never their values; empty for `Removed`.
    pub difference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    /// fl wrote it and it has changed since.
    HandEdited,
    /// fl wrote it and it is gone, though the catalog still wants it.
    Removed,
    /// fl did not write it, and it is not what fl would write.
    Foreign,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (path, name, difference) = (self.path.display(), &self.name, &self.difference);
        let replace = format!("run `fl mcp sync --replace {name}`");
        match self.kind {
            RefusalKind::HandEdited => write!(
                f,
                "{path}: `{name}` was changed by hand since fl wrote it ({difference}). Restore \
                 the entry, or {replace}, which shows the difference and overwrites it"
            ),
            RefusalKind::Removed => write!(
                f,
                "{path}: `{name}` was removed by hand since fl wrote it. Restore the entry, or \
                 {replace}, which writes it again"
            ),
            RefusalKind::Foreign => write!(
                f,
                "{path}: `{name}` is an entry fl did not write, and it differs from what fl \
                 would write ({difference}). Rename or remove it, or {replace}, which shows \
                 the difference and overwrites it"
            ),
        }
    }
}

/// What `fl mcp check` reports (MCP spec §4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// Every target matches the catalog.
    Matches,
    /// A `sync` would change a file.
    Changes,
    /// A `sync` would refuse.
    Refused,
}

impl Check {
    /// 0, 1 or 2; an error is 2 as well.
    pub fn exit_code(self) -> u8 {
        match self {
            Check::Matches => 0,
            Check::Changes => 1,
            Check::Refused => 2,
        }
    }
}

impl Plan {
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// The servers a vendor cannot run, each left out of that vendor's file
    /// only (MCP spec §4.2). Reported; not a refusal.
    pub fn skipped(&self) -> &[VendorRefusal] {
        &self.skipped
    }

    /// A machine switch naming a server the catalog does not have: it is
    /// ignored, and said so (MCP spec §2.2). Not a refusal.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn refusals(&self) -> Vec<&Refusal> {
        let entries = self.targets.iter().flat_map(|t| &t.entries);
        entries
            .filter_map(|e| match &e.action {
                Action::Refused(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    pub fn check(&self) -> Check {
        if !self.refusals().is_empty() {
            Check::Refused
        } else if self.targets.iter().any(Target::changes) {
            Check::Changes
        } else {
            Check::Matches
        }
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines = Vec::new();
        for target in &self.targets {
            let title = vendor::vendor(target.vendor).title();
            let head = format!("{title}, {}:", target.path.display());
            let mut body = Vec::new();
            for Entry { name, action } in &target.entries {
                let line = match action {
                    Action::Add => format!("add {name}"),
                    Action::Update => format!("update {name}"),
                    Action::Remove => format!("remove {name}"),
                    Action::Adopt => format!("adopt {name} (means what fl would write)"),
                    Action::Forget => format!("forget {name} (removed by hand, no longer wanted)"),
                    Action::Replace { difference } => format!("replace {name} ({difference})"),
                    Action::Refused(_) => format!("refused {name}"),
                    Action::Unchanged | Action::Untouched => continue,
                };
                body.push(format!("  {line}"));
            }
            if body.is_empty() {
                lines.push(format!("{head} no change"));
            } else {
                lines.push(head);
                lines.extend(body);
            }
        }
        lines.extend(self.skipped.iter().map(ToString::to_string));
        lines.extend(self.refusals().iter().map(ToString::to_string));
        write!(f, "{}", lines.join("\n"))
    }
}

/// Plans every vendor file under `root` against the catalog and this
/// machine's switches. `records` is the directory of ownership records;
/// `replace` names the refused entries to overwrite. Reads, never writes.
pub fn plan(
    root: &Path,
    catalog: &Catalog,
    switches: &Switches,
    records: &Path,
    replace: &[String],
) -> Result<Plan, McpError> {
    let (desired, skipped, warnings) = desired(root, catalog, switches);
    let owner = canonical(root)?;
    let targets = VendorName::ALL
        .into_iter()
        .map(|v| plan_target(root, &owner, v, &desired[&v], records, replace))
        .collect::<Result<_, _>>()?;
    Ok(Plan {
        records: records.to_path_buf(),
        targets,
        skipped,
        warnings,
    })
}

/// One vendor's file: every name in it, in its record or in `want`,
/// classified, and the file and the record as they would become.
fn plan_target(
    root: &Path,
    owner: &Path,
    name: VendorName,
    want: &BTreeMap<String, Rendered>,
    records: &Path,
    replace: &[String],
) -> Result<Target, McpError> {
    let v = vendor::vendor(name);
    let path = root.join(v.target());
    // A link would carry fl's write into a file it cannot see whole: one git
    // tracks (the gitignore guard asks about the link's own path), or one
    // outside the project.
    if let Some(link) = link_in(owner, Path::new(v.target()))? {
        let problem = if link == Path::new(v.target()) {
            "it is a symbolic link, and fl writes only plain files it can see whole".to_string()
        } else {
            format!(
                "{} is a symbolic link, and fl writes only plain files it can see whole",
                root.join(&link).display()
            )
        };
        return Err(McpError::VendorFile(vendor::FileRefusal {
            path,
            problem,
            next: "Replace the link with a plain file or directory, then run `fl mcp sync` again"
                .into(),
        }));
    }
    let canonical = canonical(&path)?;
    let before = read(&canonical, &path)?;
    let mut file = v.open(&path, before.as_deref())?;
    let record_file = record_file(records, &canonical);
    let old = read_record(&record_file)?.unwrap_or_else(|| Record {
        target: canonical.clone(),
        entries: BTreeMap::new(),
    });
    let mut record = old.clone();
    let mut names = file.names();
    let rest: BTreeSet<&String> = old.entries.keys().chain(want.keys()).collect();
    for n in rest {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }
    let mut changed = false;
    let mut entries = Vec::new();
    for n in names {
        let current = file.entry(&n);
        let fl = want.get(&n);
        let owned = old.entries.get(&n).map(|o| o.sha256.as_str());
        let replacing = replace.contains(&n);
        let gone = before.is_none();
        let action = classify(
            name,
            &path,
            &n,
            current.as_deref(),
            fl,
            owned,
            gone,
            replacing,
        );
        // The hash of what fl owns once this entry is done: `None` when
        // nothing.
        let wrote = match &action {
            Action::Add | Action::Update | Action::Remove | Action::Replace { .. } => {
                match fl {
                    Some(fl) => file.set(&n, fl),
                    None => _ = file.remove(&n),
                }
                changed = true;
                fl.map(|fl| sha256(fl.bytes()))
            }
            // The entry stays as it stands in the file.
            Action::Adopt => current.as_deref().map(sha256),
            Action::Forget => None,
            Action::Unchanged | Action::Untouched | Action::Refused(_) => {
                entries.push(Entry { name: n, action });
                continue;
            }
        };
        match wrote {
            Some(sha256) => {
                let owned = Owned {
                    root: owner.to_path_buf(),
                    sha256,
                };
                record.entries.insert(n.clone(), owned);
            }
            None => _ = record.entries.remove(&n),
        }
        entries.push(Entry { name: n, action });
    }
    Ok(Target {
        vendor: name,
        path,
        entries,
        canonical,
        after: changed.then(|| file.to_bytes()),
        before,
        record_file,
        record: (record != old).then_some(record),
    })
}

/// One name in one target, by MCP spec §4.3's table. `current` is the entry
/// in the file, `fl` what fl would write, `owned` the hash in the record;
/// `gone`, that the whole file is absent.
#[allow(clippy::too_many_arguments)]
fn classify(
    vendor: VendorName,
    path: &Path,
    name: &str,
    current: Option<&[u8]>,
    fl: Option<&Rendered>,
    owned: Option<&str>,
    gone: bool,
    replace: bool,
) -> Action {
    match (current, fl, owned) {
        (Some(cur), Some(fl), owned) if cur == fl.bytes() => {
            if owned == Some(sha256(cur).as_str()) {
                Action::Unchanged
            } else {
                Action::Adopt
            }
        }
        // fl's entry, laid out anew but meaning what fl would write.
        (Some(cur), Some(fl), Some(hash)) if same_meaning(vendor, name, cur, fl.bytes()) => {
            if sha256(cur) == hash {
                Action::Unchanged
            } else {
                Action::Adopt
            }
        }
        (Some(cur), fl, Some(hash)) if sha256(cur) == hash => {
            if fl.is_some() {
                Action::Update
            } else {
                Action::Remove
            }
        }
        (None, None, Some(_)) => Action::Forget,
        (None, Some(_), None) => Action::Add,
        // The whole file is gone (a fresh clone, `git clean -X`): fl's own
        // generated file is written anew. One that lost only this entry was
        // edited by hand, and is refused below.
        (None, Some(_), Some(_)) if gone => Action::Add,
        (Some(_), None, None) => Action::Untouched,
        // Changed or removed since fl wrote it, or someone else's in the way.
        (current, fl, owned) => {
            let difference = difference(vendor, name, current, fl);
            let kind = if owned.is_none() {
                RefusalKind::Foreign
            } else if current.is_none() {
                RefusalKind::Removed
            } else {
                RefusalKind::HandEdited
            };
            if replace {
                Action::Replace { difference }
            } else {
                Action::Refused(Refusal {
                    path: path.to_path_buf(),
                    name: name.to_string(),
                    kind,
                    difference,
                })
            }
        }
    }
}

/// Writes the plan: nothing at all when it holds a refusal or when a target
/// changed since it was planned. Holds the lock in the records directory
/// throughout, waiting for another `sync` to finish first.
pub fn apply(plan: &Plan) -> Result<(), McpError> {
    let refusals = plan.refusals();
    if !refusals.is_empty() {
        let refusals = refusals.into_iter().cloned().collect();
        return Err(McpError::Refused { refusals });
    }
    let work: Vec<&Target> = plan
        .targets
        .iter()
        .filter(|t| t.after.is_some() || t.record.is_some())
        .collect();
    if work.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(&plan.records).map_err(|e| io_error("write", &plan.records, e))?;
    let lock_path = plan.records.join(LOCK);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| io_error("write", &lock_path, e))?;
    lock.lock().map_err(|e| io_error("lock", &lock_path, e))?;
    // Every target read again before any is written: `agy mcp add`, or a
    // person, may have written one since the plan (MCP spec §4.3).
    for target in &work {
        if read(&target.canonical, &target.path)? != target.before {
            return Err(McpError::Changed {
                path: target.path.clone(),
            });
        }
    }
    let mut staged = Vec::new();
    for target in &work {
        let temp = match &target.after {
            Some(bytes) => Some(stage(target, bytes)?),
            None => None,
        };
        staged.push((target, temp));
    }
    for (target, temp) in staged {
        if let Some(temp) = temp {
            temp.persist(&target.canonical)
                .map_err(|e| io_error("write", &target.path, e.error))?;
        }
        // After the rename: a crash between the two leaves an entry exactly
        // as fl writes it and no record, which the next plan adopts.
        if let Some(record) = &target.record {
            write_record(&plan.records, &target.record_file, record)?;
        }
    }
    drop(lock);
    Ok(())
}

/// The new content in a temporary file beside the target, with the
/// original's permission bits, or a new file's default ones.
fn stage(target: &Target, bytes: &[u8]) -> Result<NamedTempFile, McpError> {
    let io = |e: io::Error| io_error("write", &target.path, e);
    let dir = target
        .canonical
        .parent()
        .expect("a target is in the project root");
    fs::create_dir_all(dir).map_err(io)?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(".fl-mcp-");
    if target.before.is_none()
        && let Some(mode) = new_file_mode()
    {
        builder.permissions(mode);
    }
    let mut temp = builder.tempfile_in(dir).map_err(io)?;
    temp.write_all(bytes).map_err(io)?;
    if target.before.is_some() {
        let mode = fs::metadata(&target.canonical).map_err(io)?.permissions();
        fs::set_permissions(temp.path(), mode).map_err(io)?;
    }
    Ok(temp)
}

/// What `File::create` gives: the umask applies to it.
#[cfg(unix)]
fn new_file_mode() -> Option<fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    Some(fs::Permissions::from_mode(0o666))
}

#[cfg(not(unix))]
fn new_file_mode() -> Option<fs::Permissions> {
    None
}

/// The entries the catalog wants in each vendor's file, as fl writes them.
type Desired = BTreeMap<VendorName, BTreeMap<String, Rendered>>;

/// The entries the catalog wants, the servers a vendor cannot run, and a
/// warning for each machine switch naming a server the catalog does not
/// have, which is ignored (MCP spec §2.2).
fn desired(
    root: &Path,
    catalog: &Catalog,
    switches: &Switches,
) -> (Desired, Vec<VendorRefusal>, Vec<String>) {
    let mut warnings = Vec::new();
    let lists = [("enable", &switches.enable), ("disable", &switches.disable)];
    for (list, names) in lists {
        for name in names.iter().filter(|n| !catalog.servers.contains_key(*n)) {
            warnings.push(format!(
                "the `[[mcp]]` entry for this project in your fl config has `{name}` in \
                 `{list}`, and {} has no such server; fl ignores it. Remove it from that entry",
                Catalog::path(root).display()
            ));
        }
    }
    let mut desired: Desired = VendorName::ALL.map(|v| (v, BTreeMap::new())).into();
    let mut skipped = Vec::new();
    for (name, server) in &catalog.servers {
        let on = if switches.disable.contains(name) {
            false
        } else if switches.enable.contains(name) {
            true
        } else {
            server.enabled
        };
        if !on {
            continue;
        }
        for v in VendorName::ALL {
            if !server.is_for(v) {
                continue;
            }
            match vendor::vendor(v).render(name, server) {
                Ok(entry) => {
                    desired
                        .get_mut(&v)
                        .expect("every vendor")
                        .insert(name.clone(), entry);
                }
                Err(refusal) => skipped.push(refusal),
            }
        }
    }
    (desired, skipped, warnings)
}

/// Which fields of the entry differ from what fl would write: names only,
/// never a value, which could be a secret someone typed in.
fn difference(
    vendor: VendorName,
    name: &str,
    current: Option<&[u8]>,
    fl: Option<&Rendered>,
) -> String {
    let (current, fl) = match (current, fl) {
        (None, _) => return "it is not in the file".to_string(),
        (_, None) => return "the catalog no longer has it here, so fl would remove it".to_string(),
        (Some(current), Some(fl)) => (current, fl.bytes()),
    };
    let (Some(theirs), Some(ours)) = (fields(vendor, name, current), fields(vendor, name, fl))
    else {
        return "its text differs".to_string();
    };
    let keys: BTreeSet<&String> = theirs.keys().chain(ours.keys()).collect();
    let mut parts = Vec::new();
    for key in keys {
        match (theirs.get(key), ours.get(key)) {
            (Some(a), Some(b)) if a != b => parts.push(format!("`{key}` differs")),
            (Some(_), None) => parts.push(format!("`{key}` is only in the file")),
            (None, Some(_)) => parts.push(format!("`{key}` is only in fl's")),
            _ => {}
        }
    }
    if parts.is_empty() {
        "only its layout or comments differ".to_string()
    } else {
        parts.join(", ")
    }
}

/// Whether two entries mean the same, whatever their layout: the same JSON
/// value, or the same TOML table, keys in any order.
fn same_meaning(vendor: VendorName, name: &str, a: &[u8], b: &[u8]) -> bool {
    match (value(vendor, name, a), value(vendor, name, b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// An entry as a value; `None` when it does not parse.
fn value(vendor: VendorName, name: &str, bytes: &[u8]) -> Option<serde_json::Value> {
    match vendor {
        VendorName::Codex => {
            let doc: toml::Table = toml::from_str(std::str::from_utf8(bytes).ok()?).ok()?;
            let entry = doc.get("mcp_servers")?.get(name)?;
            serde_json::to_value(entry).ok()
        }
        VendorName::Claude | VendorName::Antigravity => serde_json::from_slice(bytes).ok(),
    }
}

/// An entry's fields by dotted name, each value in a form fit only for
/// comparing; `None` when the entry is not a table.
fn fields(vendor: VendorName, name: &str, bytes: &[u8]) -> Option<BTreeMap<String, String>> {
    let value = value(vendor, name, bytes)?;
    fn flatten(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    let key = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    flatten(&key, value, out);
                }
            }
            other => {
                out.insert(prefix.to_string(), other.to_string());
            }
        }
    }
    if !value.is_object() {
        return None;
    }
    let mut out = BTreeMap::new();
    flatten("", &value, &mut out);
    Some(out)
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The record of the target at `canonical`, named by the hash of its path.
fn record_file(records: &Path, canonical: &Path) -> PathBuf {
    records.join(format!(
        "{}.json",
        sha256(canonical.as_os_str().as_encoded_bytes())
    ))
}

fn read_record(file: &Path) -> Result<Option<Record>, McpError> {
    let Some(bytes) = read(file, file)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| McpError::Record {
            path: file.to_path_buf(),
            cause: e.to_string(),
        })
}

fn write_record(records: &Path, file: &Path, record: &Record) -> Result<(), McpError> {
    let io = |e: io::Error| io_error("write", file, e);
    let mut bytes = serde_json::to_vec_pretty(record).map_err(|e| io(e.into()))?;
    bytes.push(b'\n');
    let mut temp = NamedTempFile::new_in(records).map_err(io)?;
    temp.write_all(&bytes).map_err(io)?;
    temp.persist(file).map_err(|e| io(e.error))?;
    Ok(())
}

/// The path with every link resolved; for a file that does not exist yet,
/// its nearest existing ancestor's, joined with the rest.
fn canonical(path: &Path) -> Result<PathBuf, McpError> {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        match fs::canonicalize(at) {
            Ok(real) => return Ok(rest.iter().rev().fold(real, |p, c| p.join(c))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => match (at.parent(), at.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_owned());
                    at = parent;
                }
                _ => return Err(io_error("read", path, e)),
            },
            Err(e) => return Err(io_error("read", path, e)),
        }
    }
}

/// The first symbolic link on the way from the canonical project root to
/// `target`, relative to it: the root itself may be reached through a link,
/// nothing below it may.
fn link_in(owner: &Path, target: &Path) -> Result<Option<PathBuf>, McpError> {
    let mut at = PathBuf::new();
    for part in target.components() {
        at.push(part);
        match fs::symlink_metadata(owner.join(&at)) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(Some(at)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error("read", &owner.join(&at), e)),
        }
    }
    Ok(None)
}

/// The file's bytes; `None` when it does not exist. `path` names it.
fn read(file: &Path, path: &Path) -> Result<Option<Vec<u8>>, McpError> {
    match fs::read(file) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error("read", path, e)),
    }
}

fn io_error(op: &'static str, path: &Path, e: io::Error) -> McpError {
    McpError::Io {
        op,
        path: path.to_path_buf(),
        cause: e.to_string(),
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-mcp --lib sync::`
Expected: PASS — 27 passed, 82 filtered out: `sync::tests::a_desired_entry_absent_from_the_file_and_the_record_is_added`, `sync::tests::an_entry_fl_wrote_is_updated_when_the_catalog_changes`, `sync::tests::an_entry_fl_wrote_is_removed_when_the_catalog_no_longer_has_it`, `sync::tests::a_hand_edited_entry_is_refused_naming_the_remedy`, `sync::tests::an_entry_removed_by_hand_is_refused_unless_the_catalog_no_longer_wants_it`, `sync::tests::someone_elses_entry_under_a_desired_name_is_refused`, `sync::tests::an_exact_match_not_in_the_record_is_adopted`, `sync::tests::a_crash_between_the_write_and_its_record_is_recovered_by_adopting`, `sync::tests::an_entry_fl_did_not_write_and_does_not_want_is_left_untouched`, `sync::tests::a_file_changed_between_plan_and_write_is_refused_and_nothing_is_written`, `sync::tests::one_refusal_writes_nothing_in_any_of_the_three_files`, `sync::tests::replace_overwrites_only_the_named_entry`, `sync::tests::a_disabled_server_is_removed_from_every_vendor_file_it_was_in`, `sync::tests::the_machine_switches_turn_a_server_on_or_off_here`, `sync::tests::a_machine_switch_naming_an_unknown_server_is_a_warning`, `sync::tests::a_vendor_file_gone_as_a_whole_is_written_anew`, `sync::tests::fl_s_entry_laid_out_anew_with_the_same_meaning_is_adopted`, `sync::tests::a_vendor_file_reached_through_a_link_is_refused`, `sync::tests::check_is_0_when_everything_matches_1_with_changes_2_on_a_refusal`, `sync::tests::the_plan_prints_each_target_its_actions_and_each_refusal`, `sync::tests::a_vendor_that_cannot_run_a_server_skips_it_and_the_others_still_get_it`, `sync::tests::a_record_holds_each_entry_s_name_root_and_hash_only`, `sync::tests::the_record_is_keyed_by_the_canonical_path`, `sync::tests::a_record_fl_cannot_read_is_refused_naming_the_remedy`, `sync::tests::a_vendor_file_fl_cannot_read_stops_the_plan`, `sync::tests::the_original_file_mode_is_kept_and_a_new_file_gets_the_default_mode`, `sync::tests::a_second_sync_waits_while_another_holds_the_lock`.

- [ ] **Step 5: Mutation checks**

Each name below is `cargo test -p fl-mcp --lib sync::tests::<test>`: "added" is `a_desired_entry_absent_from_the_file_and_the_record_is_added`; "updated" is `an_entry_fl_wrote_is_updated_when_the_catalog_changes`; "removed" is `an_entry_fl_wrote_is_removed_when_the_catalog_no_longer_has_it`; "hand" is `a_hand_edited_entry_is_refused_naming_the_remedy`; "gone" is `an_entry_removed_by_hand_is_refused_unless_the_catalog_no_longer_wants_it`; "foreign" is `someone_elses_entry_under_a_desired_name_is_refused`; "adopt" is `an_exact_match_not_in_the_record_is_adopted`; "crash" is `a_crash_between_the_write_and_its_record_is_recovered_by_adopting`; "untouched" is `an_entry_fl_did_not_write_and_does_not_want_is_left_untouched`; "changed" is `a_file_changed_between_plan_and_write_is_refused_and_nothing_is_written`; "one" is `one_refusal_writes_nothing_in_any_of_the_three_files`; "replace" is `replace_overwrites_only_the_named_entry`; "disabled" is `a_disabled_server_is_removed_from_every_vendor_file_it_was_in`; "switches" is `the_machine_switches_turn_a_server_on_or_off_here`; "unknown" is `a_machine_switch_naming_an_unknown_server_is_a_warning`; "anew" is `a_vendor_file_gone_as_a_whole_is_written_anew`; "relaid" is `fl_s_entry_laid_out_anew_with_the_same_meaning_is_adopted`; "link" is `a_vendor_file_reached_through_a_link_is_refused`; "check" is `check_is_0_when_everything_matches_1_with_changes_2_on_a_refusal`; "printed" is `the_plan_prints_each_target_its_actions_and_each_refusal`; "skip" is `a_vendor_that_cannot_run_a_server_skips_it_and_the_others_still_get_it`; "canonical" is `the_record_is_keyed_by_the_canonical_path`; "bad-record" is `a_record_fl_cannot_read_is_refused_naming_the_remedy`; "mode" is `the_original_file_mode_is_kept_and_a_new_file_gets_the_default_mode`; "lock" is `a_second_sync_waits_while_another_holds_the_lock`. Each mutation is one edit of `crates/mcp/src/sync.rs`; save a copy first, restore it after each, and `cmp` against the copy.

The classes (`classify`):
1. an exact match fl's record holds is unchanged, else adopted: `if true {` for `if owned == Some(sha256(cur).as_str()) {` → adopt red (unchanged, not adopted).
2. the same line, `if false {` → updated red (`docs` adopted, not unchanged).
3. the exact-match arm: `if false && cur == fl.bytes()` → adopt red (refused as someone else's).
4. the as-fl-wrote-it arm: `if false && sha256(cur) == hash` → updated red (refused as hand-edited).
5. update or remove: `if true {` for `if fl.is_some() {` → removed red (`Update` where `Remove` is due).
6. forget: delete `(None, None, Some(_)) => Action::Forget,` → gone red (refused as removed).
7. add: delete `(None, Some(_), None) => Action::Add,` → added red (refused as someone else's).
8. untouched: delete `(Some(_), None, None) => Action::Untouched,` → untouched red (refused).
9. someone else's: `let kind = if false {` for `let kind = if owned.is_none() {` → foreign red (named hand-edited).
10. removed: `} else if false {` for `} else if current.is_none() {` → gone red (named hand-edited).
11. `--replace` overwrites: `if false {` for `if replace {` → replace red.
12. … only the named entry: `let replacing = !replace.is_empty();` → replace red (`docs` replaced too).

The plan of a target (`plan_target`):
13. a change is written: delete `changed = true;` → added red (no file written).
14. adopt records the entry: `Action::Adopt => None,` → adopt red (no record).
15. forget drops it from the record: delete `Action::Forget => None,` and add `Action::Forget |` to the no-op arm → gone red (the record keeps `notes`).
16. wanted names are planned: `old.entries.keys().collect();` for `old.entries.keys().chain(want.keys()).collect();` → added red.
17. recorded names are planned: `want.keys().collect();` for the same line → gone red (no `Forget`).
18. a record is written only when it changes: `record: Some(record),` → skip red (a record for the untouched Antigravity target).
19. the record's root is canonical: `let owner = root.to_path_buf();` in `plan` → canonical red (the link's path is recorded).
20. the record is keyed by the canonical target: `let canonical = path.clone();` → canonical red (through the link every entry is someone else's).
21. a file that does not exist yet keeps its own name: `Ok(real) => return Ok(real),` in `canonical` → added red (`.mcp.json` resolves to the root directory).

The desired entries (`desired`):
22. a switch that names no catalog server is warned about: `names.iter().filter(|n| false && !catalog.servers.contains_key(*n))` → unknown red.
23. `disable` turns a server off here: `let on = if false {` → disabled red.
24. `enable` turns it on here: `} else if false {` for `} else if switches.enable.contains(name) {` → switches red.
25. otherwise the team default: `true` for `server.enabled` → switches red (`docs` with `enabled = false` is written).
26. a server's `vendors`: `if false {` for `if !server.is_for(v) {` → skip red (`local` reaches every vendor).
27. a vendor refusal is reported: `Err(_) => {}` for `Err(refusal) => skipped.push(refusal),` → skip red.

The write (`apply`, `stage`):
28. a refusal writes nothing: `if false {` for `if !refusals.is_empty() {` → one red (Claude Code's and Antigravity's files written).
29. a target whose record alone changes is written: `.filter(|t| t.after.is_some())` → adopt red (the records are not restored).
30. nothing to do takes no lock: `if false {` for `if work.is_empty() {` → check red (the records directory appears).
31. the lock: delete `lock.lock().map_err(|e| io_error("lock", &lock_path, e))?;` → lock red (the second sync finishes while the lock is held).
32. a target changed since the plan is refused: `if false {` for `if read(&target.canonical, &target.path)? != target.before {` → changed red.
33. every target is checked before any is written: delete the checking loop and put the same check at the top of the rename loop (`for (target, temp) in staged {`) → changed red (Claude Code's and Codex's files written before Antigravity's is refused).
34. the record after the rename: swap the `if let Some(temp) = temp { … }` and `if let Some(record) = &target.record { … }` blocks → crash red (`.mcp.json` not written when the record fails).
35. the original's mode is copied: delete `fs::set_permissions(temp.path(), mode).map_err(io)?;` → mode red (`0o600`, not `0o646`).
36. a new file gets the default mode: `let _ = mode;` for `builder.permissions(mode);` → mode red (`0o600`).

The check, the output, the difference, the record:
37. changes: `} else if false {` for `} else if self.targets.iter().any(Target::changes) {` → check red.
38. a refusal outranks a change: swap the first two branches of `check` → printed red (`Changes` for a plan with updates and a refusal).
39. `Check::Matches => 1,` → check red.
40. `Check::Changes => 2,` → check red.
41. `Check::Refused => 1,` → check red.
42. unchanged and untouched entries are not listed: `Action::Unchanged | Action::Untouched => format!("keep {name}"),` → printed red.
43. a target with no change says so: `if false {` for `if body.is_empty() {` → printed red.
44. the difference names fields, never values: ``format!("`{key}` differs: {a}")`` → hand red.
45. only fields that differ: `(Some(_), Some(_)) =>` for `(Some(a), Some(b)) if a != b =>` → hand red (`` `command` differs `` too).
46. a record with an unknown field is refused: delete `#[serde(deny_unknown_fields)]` above `pub struct Record` → bad-record red.
47. … and an entry of one: delete it above `pub struct Owned` → bad-record red.

Owner decisions 14 and 15, and plan rulings 38 and 39:
48. a target reached through a link is refused: `if let Some(link) = link_in(owner, Path::new(v.target()))? && false {` → link red (`.mcp.json` through the link is planned).
49. a whole file gone is written anew: `(None, Some(_), Some(_)) if false && gone => Action::Add,` → anew red (refused as removed).
50. … only when the whole file is gone: `let gone = false;` for `let gone = before.is_none();` → anew red.
51. … and an entry gone from a file that remains is still refused: `(None, Some(_), Some(_)) => Action::Add,` (no `if gone`) → gone red (`notes` added, not refused).
52. fl's entry with the same meaning is adopted: `if false && same_meaning(vendor, name, cur, fl.bytes())` in that arm's guard → relaid red (refused as hand-edited).
53. … the same meaning only: `(Some(_), Some(_)) => true,` for `(Some(a), Some(b)) => a == b,` in `same_meaning` → relaid red (the changed `args` is adopted).
54. … and once adopted it is unchanged: `if false && sha256(cur) == hash {` in that arm → relaid red (adopted again).
55. an adopted entry records the bytes in the file: `Action::Adopt => fl.map(|fl| sha256(fl.bytes())),` → relaid red (the next plan adopts it again).
56. a Codex entry is compared as itself: `let entry = doc.get("mcp_servers")?;` in `value` → relaid red (`notes.args` differs).
57. a stale switch is warned about in either list: `let lists = [("enable", &switches.enable)];` → unknown red (`old` in `disable` is not named).
58. the link check (48) finds a link at the file itself: `… if meta.file_type().is_symlink() && at != target => …` in `link_in` → link red (`.mcp.json`).
59. … and at a directory on the way: `… && at == target => …` → link red (`.agents`).
60. … without following it: `fs::symlink_metadata(…).and_then(|_| fs::metadata(…))` → link red (a followed link is never a link).
61. … below the root only: `link_in(Path::new(""), &path)` (the whole path, the root's own link included) → link red (the project reached through `app-link` is refused).
62. the refusal names the linked directory: `let problem = if true {` → link red.
63. the plan names an adoption by meaning: `"adopt {name} (already as fl writes it)"` → relaid red.

Not observable:
- The `t.after.is_some() ||` conjunct of `apply`'s filter: every file change also changes the record (each add, update, remove and replace inserts or drops that entry's hash), so a target with new bytes always has a new record.
- `.truncate(false)` on the lock file (it is empty) and `drop(lock)` (the function returns next): neither changes what is written.
- `new_file_mode`'s non-unix branch: no test runs off unix.
- The `expect`s `a target is in the project root` (a target is `root` joined with a relative path) and `every vendor` (`desired` starts with an empty map per vendor): no input reaches them.
- The differences `only its layout or comments differ`, `its text differs`, `it is not in the file` and `the catalog no longer has it here, so fl would remove it`: wording, shown in a refusal or a replace; the arms that choose between them are 44 and 45.
- Refusal and error wording: pinned by exact text in hand, gone, foreign, changed, unknown, printed and bad-record; not guards.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1307 passed, 19 ignored (1280 and 19 before this task).

- [ ] **Step 7: Commit**

```bash
git add Cargo.lock crates/mcp/Cargo.toml crates/mcp/src/lib.rs crates/mcp/src/sync.rs
git commit -m "feat(mcp): sync: the plan, ownership records, all or nothing

fl mcp sync builds the entries the catalog and this machine's switches
want in each vendor file, and classifies every name in every target:
add; update or remove an entry fl wrote while it is still as fl wrote
it; refuse one changed or removed by hand since, or someone else's
entry in the way, naming the file, the entry, which fields differ
(never a value) and the remedy; adopt an exact match fl has no record
of, which recovers a crash between a write and its record, and an entry
of fl's laid out anew that means what fl would write. A vendor file gone
as a whole, after a fresh clone or git clean, is written anew. A switch
naming a server the catalog lacks is a warning; a server one vendor
cannot run is left out of that vendor's file only. A vendor file that
is a symbolic link, or lies under a linked directory, is refused: fl
writes only plain files it can see whole.

Which entries fl wrote lives in an ownership record per target, named
by the SHA-256 of its canonical path: each entry's name, project root
and the hash of its bytes, nothing else. Every target is planned
before any is written and one refusal writes nothing. The write holds
a lock, waiting for another sync; reads every target again and refuses
all of them if one changed since the plan; stages each file beside its
target with the original's mode; renames, then writes the record. The
check result maps to exit codes 0, 1 and 2. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 7: `fl mcp` in the CLI

A person drives the catalog with `fl mcp …` (MCP spec §1.2): `registry`, `search`, `add` (from the registry, or by hand), `remove`, `enable`, `disable`, `upgrade`, `sync` and `check`. This task wires Tasks 1–6 into `fl-cli` and adds what the crate cannot do itself, because git lives in fl-exec and the config in fl-cli: the project root, the machine switches, the records directory, the gitignore guard, and Codex's trust check.

**The root** (plan ruling 6): the nearest ancestor of the working directory that holds `.fl/mcp.toml`; with none, the nearest that holds `.git`, where `registry` and `add` create the catalog; with neither, refused. No git runs to find it. `sync`, `check` and every edit but `registry` and `add` need a catalog: a project with none is refused, naming `fl mcp add` — a missing catalog is never read as an empty one, which would remove every entry fl wrote.

**No store** (plan ruling 8, spec §1.1): `run()` hands `fl mcp` over right after `config::load`, before any store path is resolved or its directory created, so no `fl mcp` command creates or opens a store. `--db` names a store, so it is refused with `fl mcp`, naming why; `$FL_DB` is the environment's, set for every command, so it is ignored. The exhaustive matches on `Command` (`iris`, `has_handle`, `needs_tracker`, the final dispatch) gain an arm each; none is reached.

**The registry** is read by `search`, `add --from` and `upgrade` only; `sync` and `check` build no registry client (spec §5 — "`sync` and `check` have no network path"). The catalog's `registry` names it; a catalog that names none is refused with `fl mcp registry <url>` and the official address as an example — fl assumes no registry. `search` prints each server's name, version and description, and its status when it is not `active` (`deprecated`, or one fl does not know, as the registry spells it; plan ruling 31) — each through `registry::printable`, which Task 3 wrote and this task makes public, so a description's escape sequences never reach the terminal. The client keeps only names that hold the text (owner decision 13, Task 3). `add --from` refuses a name the catalog already has before it reads the registry, then freezes the entry (Task 4) with `--version` (default `latest`), `--package npm|pypi|oci` or `--remote`, `--env NAME=value` and `--with NAME`, and prints the launch it recorded (the transport and the command with its arguments, or the URL and its headers' names; never a value), the freeze's warnings (deprecated; each literal value, which will be committed — spec §6), the variables a person sets, and the optional secrets it left out with the `--with` that includes each (plan ruling 30). `upgrade` (spec §3.3) freezes the registry's latest (or `--to V`) with the pinned entry's route, literals and included secrets, and with its own `--env NAME=value` and `--with NAME` for what the new version needs — the remedies its own refusals name — refuses a version that is not newer unless named with `--to`, keeps the pinned `enabled` and `vendors`, prints the difference field by field, and rewrites that entry only; it does not sync — the change reaches the vendor files after a person commits it.

**By hand** (spec §1.2): `add <name> [--env NAME[=VALUE]]… -- <command> [args…]` records a stdio server: `NAME=VALUE` is a literal (warned: it will be committed), `NAME` alone a secret reference `{ secret = true }` the agent CLI reads from its environment — so `NAME` must look like a variable's name, by `--header`'s rule below, and a pasted token there is refused without being repeated (plan ruling 34). `add` by hand prints the launch it recorded as `add --from` does. `add <name> --url <url> [--header NAME[=ENV[:SCHEME]]]…` records a streamable HTTP server whose headers are **secrets by reference only**, in plan ruling 18's shape: `NAME` alone reads `<SERVER>_<NAME>` (uppercased, `-` as `_`) holding the whole value; `NAME=ENV` reads `ENV`; `NAME=ENV:SCHEME` sends `<SCHEME> <value>`. A literal header cannot be given this way, so it cannot be taken for a secret: what follows `=` must look like a variable's name (capital letters, digits and `_` — stricter than the catalog's own rule, so a pasted token with a lower-case letter, a space or a second word is refused), and the refusal never repeats what was typed (spec §6). A flag given twice for one name is refused. **Each form of `add` takes its own flags** (plan ruling 37): `--version`, `--package`, `--remote` and `--with` go with `--from` only, and `--header` with `--url` only; one given to another form is refused naming it, never ignored. clap refuses `--from`, `--url` and a command together, and `--env` with `--url`; its `requires` does not hold once a form's own flag is given (`fl mcp add z --header Authorization -- node` passed clap and dropped the header), so fl checks the rest itself.

**`sync` and `check`** read the catalog, this machine's switches — Task 2's `config::mcp_entry` for the root, mapped into Task 6's `Switches` (none: the defaults) — and the records directory `config::fl_state_dir()/mcp` (plan ruling 23; no absolute `$XDG_STATE_HOME` and no `$HOME`: refused), and call `sync::plan`. Each of the plan's warnings — a switch naming a server the catalog lacks (owner decision 15) — is printed as `warning: …` on stderr. Then, before anything else is printed or written:

- **The gitignore guard** (plan ruling 21, spec §4.4; a vendor file reached through a link was already refused by `sync::plan`, plan ruling 38, so the guard's question about the path is the question about the file): for every target the plan writes or holds an entry of fl's in (any entry but one fl leaves untouched or only forgets), `Git::is_tracked` — new in fl-exec, beside `is_ignored` — and then `Git::is_ignored`. A tracked file is refused naming `git rm --cached <file>`; a file git would not ignore is refused; both print the anchored lines (`/.mcp.json`, `/.codex/config.toml`, `/.agents/mcp_config.json`) for the root's `.gitignore`. fl does not edit `.gitignore`. A git that cannot answer is an error, never "ignored". The guard reads the plan's targets, so a vendor no server is for, or a file holding only someone else's entries, needs no ignore line; `plan` itself only reads.
- **Codex trust** (plan ruling 19, spec §4.1): when the plan leaves an entry of fl's in Codex's file, `$CODEX_HOME/config.toml` (an empty `$CODEX_HOME` is unset; default `$HOME/.codex/config.toml`) is read, never written. Codex's keys, in its order: the project root — the nearest directory holding `.git`, canonical — then, for a linked worktree, the main checkout's root (read from the worktree's `.git` file and its `commondir`, without running git). The first key with a `[projects."<key>"]` table decides, and only `trust_level = "trusted"` trusts; an ancestor's entry does not count. Otherwise a `warning:` names the file and the two lines to add. An absent file, one fl cannot read, and one that is not TOML are warnings too, never refusals; a parse error names its line number and shows none of the file's text (it may hold a token).

`sync` prints the plan (Task 6's `Display`) and applies it; a refused plan writes nothing and says so after the refusals it printed. `check` prints the plan and exits with `Plan::check` — 0 (matches), 1 (a `sync` would change something), 2 (a refusal) — and every error exits 2 through `main` (spec §4.5, §5).

**Blast radius:** `run()` in `crates/cli/src/main.rs` gains a `match cli` right after `config::load` that returns for `Command::Mcp` and hands every other command back unchanged, so every other command runs exactly as before. The four exhaustive matches on `Command` gain an `Mcp` arm. `crates/exec/src/git.rs`: `Git::is_committed` now asks the new `Git::is_tracked` whether the path is tracked — the same `git ls-files -- <rel>` test it ran inline, so its one caller (`fl manifest export`'s currency check) sees the same answer. `crates/cli/src/config.rs` loses the three `expect(dead_code)` attributes Task 2 put on `Config::mcp`, `mcp_entry` and `fl_state_dir` (Task 2's note), and nothing else. `fl --help` gains the line `mcp …`, so `docs/getting-started.md`'s help transcript, which `crates/cli/tests/getting_started.rs` executes, gains that line here (its command count, 65, is unchanged); Task 8 writes the rest of the docs and must not add the line again. `Cargo.lock` gains `"fl-mcp"` under `fl-cli`. `crates/mcp/src/registry.rs`: `printable` becomes `pub` (it was `pub(crate)`, from Task 4), so `fl mcp search` and `add` show registry text the way freezing does; its behaviour is unchanged.

**Files:**
- Modify: `crates/cli/Cargo.toml` (`fl-mcp`; `fl-mcp` with `fake` under dev-dependencies)
- Modify: `Cargo.lock` (regenerated by cargo)
- Modify: `crates/exec/src/git.rs` (`Git::is_tracked`; `is_committed` calls it; tests)
- Modify: `crates/cli/src/main.rs` (`Command::Mcp`, the early return, the `--db` refusal, the dispatch arms)
- Modify: `crates/cli/src/cmd/mod.rs` (`pub mod mcp;`)
- Modify: `crates/cli/src/config.rs` (three attributes removed)
- Create: `crates/cli/src/cmd/mcp.rs` (`Cmd`, `run`, and their helpers)
- Create: `crates/cli/tests/mcp.rs` (black-box tests)
- Modify: `docs/getting-started.md` (one line of the `fl --help` transcript)
- Modify: `crates/mcp/src/registry.rs` (`printable` is `pub`)

**Interfaces:**
- Consumes (Task 1): `catalog::{Catalog, Editor, Server, EnvValue, HeaderValue, Transport, VendorName}`, `Catalog::{path, load}`, `Editor::{open, path, catalog, save, add, remove, set_enabled, set_registry, replace}`, `Server::literal_values`, `EnvValue::secret_var`, `McpError::{AlreadyPresent, NoSuchServer}`. (Task 2): `config::{McpEntry, mcp_entry, fl_state_dir}`, `Config::mcp`. (Task 3): `registry::{Registry, SEARCH_PAGES, printable}`, `Registry::{new, search, version}`, `Search { servers, stopped_early }`, `Summary { name, version, description, status }`; tests: `fake::{FakeRegistry, NOTES, WEATHER, TRACKER, TRACKER_IMAGE, DOCS, DOCS_URL, MULTI, LEGACY, UPGRADING, LISTED}`, `FakeRegistry::{start, url, state, add_server}`, `State::{requests, page_limit, entries}`. (Task 4): `freeze::{freeze, diff, check_upgrade, FreezeOptions, Route}`, `FreezeOptions::upgrading`, `Frozen { server, warnings, secrets, notes }`, `Change`'s `Display`. (Task 5): `vendor::vendor`, `Vendor::target`. (Task 6): `sync::{plan, apply, Plan, Switches, Action, Target}`, `Plan::{targets, check, warnings}`, `Plan`'s `Display`, `Check::exit_code`, `McpError::Refused`.
- Produces:
  - `fl_exec::git::Git::is_tracked(root: &Path, rel: &str) -> Result<bool, ExecError>` — in git's index (committed, or added), ignored or not; a git that cannot answer is `Err(ExecError::Git(_))`, never `false`.
  - `crate::cmd::mcp::{Cmd, run}`: `pub fn run(cmd: Cmd, switches: &[McpEntry], cwd: &Path) -> anyhow::Result<i32>`; `Command::Mcp(cmd::mcp::Cmd)` (`#[command(subcommand)]`).
  - The command line: `fl mcp registry <URL>`, `search <TEXT>`, `add <NAME> [--from <REGISTRY_NAME> [--version <VERSION>] [--package npm|pypi|oci | --remote] [--with <NAME>]…] [--env <NAME[=VALUE]>]… [--url <URL> [--header <NAME[=ENV[:SCHEME]]>]…] [-- <COMMAND>…]`, `remove <NAME>`, `enable <NAME>`, `disable <NAME>`, `upgrade <NAME> [--to <VERSION>] [--env <NAME=VALUE>]… [--with <NAME>]…`, `sync [--replace <NAME>]…`, `check`. `--version`, `--package`, `--remote` and `--with` need `--from`, and `--header` needs `--url`: fl refuses each given to another form, naming it (exit 2); `--from` conflicts with `--url` and a command, `--url` with a command, and `--env` with `--url` (clap refuses these, exit 2).
  - Output: `search` prints one line per server, `<name> <version>  <description>`, with ` (<status>)` after the version when the status is not `active`, each without control characters; `add` prints ``added `<name>` to <catalog>``, then the launch on a line of its own, `  stdio: <command> <args…>` (an argument that is empty or holds a space shown quoted) or `  http: <url>` / `  sse: <url>`, with `, headers <Name>, <Name>` when it sends any, then each warning as `warning: …` and each note as `note: …` on stderr, then `set <A>, <B> in the environment the agent CLI starts in; fl records only the name` when the entry reads secrets; `sync` and `check` print the plan; each of the plan's warnings and the Codex warning is `warning: …` on stderr.
  - Test fixture (`crates/cli/tests/mcp.rs`, private): `World` — a temporary home holding the repository `app` (whose `.gitignore` ignores the three vendor files) and a running `FakeRegistry` (`fake: Option<FakeRegistry>`; `take()` stops it); `fl_in(cwd)` sets `HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_DATA_HOME` into it and removes `CODEX_HOME` and `FL_DB`; `run`/`run_in` → `(code, stdout, stderr)`, `ok`, `refused`, `with_registry`, `read`, `exists`, `write_config`, `write_codex`, `requests`; consts `IGNORED`, `CLAUDE`, `CODEX`, `AGY`, `CATALOG`; fns `git`, `repo_at`, `trusted(dir)`.
- Unique phrases: ``error: `fl mcp` opens no store, so `--db` names nothing here`` · `is in no git repository and below no MCP catalog` · `has no MCP catalog` · ``names no registry. `fl mcp registry <url>` sets one`` · `the catalog reads servers from` · ``added `<name>` to`` · `in the environment the agent CLI starts in; fl records only the name` · `is a literal value: it will be committed` (by hand: the same sentence as Task 4's warning) · ``removed `<name>` from`` · ``is now on for the team`` / ``is now off for the team`` · ``upgrade `<name>`:`` · `Review and commit it` · `nothing to change` · `was added by hand` · ``say where `<name>` comes from`` · ``` `--env` with no `=` names a secret's environment variable``` · `` `--env <NAME>` needs `=<value>` with `fl mcp upgrade` `` · ``has no place in `fl mcp add <name> <form>`: it goes with `<form>` only`` (the forms `--from <registry-name>`, `--url <url>`, `-- <command>`) · `` `--env <NAME>` is given twice`` (by hand) · `` `--env <NAME>=…` is given twice`` (`--from`) · `` `--header <NAME>` is given twice`` · ``needs `=<value>` with `--from` `` · `never a value` · `starts with a header name` · `no server's name holds` · `note: fl stopped after 20 pages of results` · `nothing was written; each refusal above names its remedy` · `fl has nowhere to keep the record` · ``is tracked by git: `git rm --cached <file>` stops tracking it`` · `is not ignored by git` · `Make sure <root>/.gitignore holds these lines:` · `fl does not edit .gitignore` · `warning: Codex loads <root>/.codex/config.toml only in a project its user trusts` · `does not exist` · `does not trust` · `fl could not read` · `is not valid TOML (line <n>)` · `neither $CODEX_HOME nor $HOME is set`. Exec: none new (git's own error text).

- [ ] **Step 1: Write the failing tests**

In `crates/cli/Cargo.toml`, under `[dependencies]`, after `fl-github = { path = "../github" }` add:

```toml
fl-mcp = { path = "../mcp" }
```

and under `[dev-dependencies]`, after `fl-github = { path = "../github", features = ["fake"] }`:

```toml
fl-mcp = { path = "../mcp", features = ["fake"] }
```

In `crates/exec/src/git.rs`, inside `mod tests`, after `is_ignored_outside_a_repository_is_an_error_not_false` and one blank line, add:

```rust
    #[test]
    fn is_tracked_tells_a_tracked_path_from_an_untracked_or_ignored_one() {
        let d = repo();
        assert!(Git::is_tracked(d.path(), "README.md").unwrap(), "committed");
        fs::write(d.path().join(".gitignore"), ".mcp.json\n").unwrap();
        fs::write(d.path().join(".mcp.json"), "{}").unwrap();
        assert!(!Git::is_tracked(d.path(), ".mcp.json").unwrap(), "ignored");
        assert!(!Git::is_tracked(d.path(), "absent.json").unwrap(), "absent");
        fs::write(d.path().join("new.rs"), "fn b() {}").unwrap();
        assert!(!Git::is_tracked(d.path(), "new.rs").unwrap(), "untracked");
        // Ignored, and tracked anyway: a `git add -f` makes it tracked.
        let run = Command::new("git")
            .args(["add", "-f", ".mcp.json"])
            .current_dir(d.path())
            .output()
            .unwrap();
        assert!(run.status.success());
        assert!(Git::is_tracked(d.path(), ".mcp.json").unwrap(), "added");
    }

    #[test]
    fn is_tracked_outside_a_repository_is_an_error_not_false() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            Git::is_tracked(d.path(), ".mcp.json"),
            Err(ExecError::Git(_))
        ));
    }
```

Create `crates/cli/tests/mcp.rs`:

```rust
//! `fl mcp`, driven as a black box (MCP spec §1.2): a git repository in a
//! private home, and the in-process fake registry. No test reaches the
//! network or the real home: `HOME` and every `XDG_*` base point into one
//! temporary directory, and `CODEX_HOME` and `FL_DB` are unset.

use assert_cmd::Command;
use fl_mcp::fake::{self, FakeRegistry};
use std::fs;
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

/// A git repository at `dir` with one commit holding `files`.
fn repo_at(dir: &Path, files: &[(&str, &str)]) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "t"]);
    for (rel, text) in files {
        fs::write(dir.join(rel), text).unwrap();
    }
    fs::write(dir.join("README.md"), "widgets\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "first"]);
}

/// The `.gitignore` lines a project that uses `fl mcp` commits.
const IGNORED: &str = "/.mcp.json\n/.codex/config.toml\n/.agents/mcp_config.json\n";

const CLAUDE: &str = ".mcp.json";
const CODEX: &str = ".codex/config.toml";
const AGY: &str = ".agents/mcp_config.json";
const CATALOG: &str = ".fl/mcp.toml";

/// One machine: a temporary home holding the repository `app`, and the fake
/// registry.
struct World {
    home: tempfile::TempDir,
    fake: Option<FakeRegistry>,
}

impl World {
    /// `app` ignores the three vendor files.
    fn new() -> World {
        World::ignoring(IGNORED)
    }

    fn ignoring(gitignore: &str) -> World {
        let home = tempfile::tempdir().unwrap();
        repo_at(&home.path().join("app"), &[(".gitignore", gitignore)]);
        World {
            home,
            fake: Some(FakeRegistry::start()),
        }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn app(&self) -> PathBuf {
        self.home().join("app")
    }

    fn fake(&self) -> &FakeRegistry {
        self.fake.as_ref().expect("the fake is running")
    }

    /// `fl` in `cwd`, with every base directory in the temporary home.
    fn fl_in(&self, cwd: &Path) -> Command {
        let h = self.home();
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", h)
            .env("XDG_CONFIG_HOME", h.join("config"))
            .env("XDG_STATE_HOME", h.join("state"))
            .env("XDG_DATA_HOME", h.join("data"))
            .env_remove("CODEX_HOME")
            .env_remove("FL_DB")
            .current_dir(cwd);
        c
    }

    /// `fl mcp <args>` in `cwd`: its exit code, stdout and stderr.
    fn run_in(&self, cwd: &Path, args: &[&str]) -> (i32, String, String) {
        let out = self.fl_in(cwd).arg("mcp").args(args).output().unwrap();
        (
            out.status.code().expect("an exit code"),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        self.run_in(&self.app(), args)
    }

    /// `fl mcp <args>` in `app`, which must succeed: stdout and stderr.
    fn ok(&self, args: &[&str]) -> (String, String) {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 0, "fl mcp {args:?}\nstdout:\n{out}\nstderr:\n{err}");
        (out, err)
    }

    /// `fl mcp <args>` in `app`, which must exit 2: stdout and stderr.
    fn refused(&self, args: &[&str]) -> (String, String) {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 2, "fl mcp {args:?}\nstdout:\n{out}\nstderr:\n{err}");
        (out, err)
    }

    /// The catalog names the fake as its registry.
    fn with_registry(&self) {
        let url = self.fake().url();
        self.ok(&["registry", &url]);
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.app().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn exists(&self, rel: &str) -> bool {
        self.app().join(rel).exists()
    }

    fn write_config(&self, text: &str) {
        let dir = self.home().join("config").join("fl");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), text).unwrap();
    }

    /// `~/.codex/config.toml` in the temporary home.
    fn write_codex(&self, text: &str) {
        let dir = self.home().join(".codex");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), text).unwrap();
    }

    fn requests(&self) -> Vec<String> {
        self.fake().state().requests.clone()
    }
}

/// `[projects."<dir>"] trust_level = "trusted"` for Codex.
fn trusted(dir: &Path) -> String {
    let key = dir.canonicalize().unwrap();
    format!(
        "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
        key.display()
    )
}

#[test]
fn registry_and_add_from_it_then_sync_write_every_vendor_file() {
    let w = World::new();
    let (out, _) = w.ok(&["registry", &w.fake().url()]);
    assert!(out.contains("the catalog reads servers from"), "{out}");
    assert!(
        w.read(CATALOG)
            .contains(&format!("registry = \"{}\"", w.fake().url()))
    );

    // npm: the only route on offer.
    let (out, err) = w.ok(&["add", "notes", "--from", fake::NOTES]);
    assert!(out.contains("added `notes` to"), "{out}");
    // The launch it recorded.
    assert!(
        out.contains("\n  stdio: npx -y @example/notes-mcp@1.2.0 ./notes\n"),
        "{out}"
    );
    assert!(
        err.contains(
            "warning: `env.NOTES_LOG` is a literal value: it will be committed with the catalog"
        ),
        "{err}"
    );
    assert!(
        out.contains("set NOTES_TOKEN in the environment the agent CLI starts in"),
        "{out}"
    );
    let catalog = w.read(CATALOG);
    assert!(catalog.contains("from = \"io.example/notes\""), "{catalog}");
    assert!(catalog.contains("version = \"1.2.0\""), "{catalog}");
    // OCI, with its optional secret named.
    let (out, err) = w.ok(&[
        "add",
        "tracker",
        "--from",
        fake::TRACKER,
        "--with",
        "TRACKER_TOKEN",
    ]);
    assert!(out.contains("set TRACKER_TOKEN"), "{out}");
    assert!(!err.contains("note:"), "{err}");
    // OCI, and a remote, each out of three routes.
    w.ok(&["add", "multi", "--from", fake::MULTI, "--package", "oci"]);
    let (out, _) = w.ok(&["add", "events", "--from", fake::MULTI, "--remote"]);
    assert!(
        out.contains("\n  sse: https://multi.example.com/sse\n"),
        "{out}"
    );
    w.ok(&[
        "add",
        "multi-npm",
        "--from",
        fake::MULTI,
        "--package",
        "npm",
    ]);
    // The only route, named.
    w.ok(&[
        "add",
        "weather",
        "--from",
        fake::WEATHER,
        "--package",
        "pypi",
    ]);
    // A remote with a secret header.
    let (out, _) = w.ok(&["add", "docs", "--from", fake::DOCS]);
    assert!(out.contains("set DOCS_AUTHORIZATION"), "{out}");
    let catalog = w.read(CATALOG);
    assert!(
        catalog.contains("\"ghcr.io/example/multi-mcp:3.0.0\""),
        "{catalog}"
    );
    assert!(
        catalog.contains("transport = \"sse\"\nurl = \"https://multi.example.com/sse\""),
        "{catalog}"
    );
    assert!(
        catalog.contains("args = [\"-y\", \"@example/multi-mcp@3.0.0\"]"),
        "{catalog}"
    );
    assert!(
        catalog.contains("headers.Authorization = { secret = true, env = \"DOCS_AUTHORIZATION\" }"),
        "{catalog}"
    );

    let (out, _) = w.ok(&["sync"]);
    assert!(out.contains("add notes"), "{out}");
    // Antigravity cannot send a secret header; the others still get `docs`.
    assert!(
        out.contains("Antigravity cannot run server `docs`"),
        "{out}"
    );
    let claude = w.read(CLAUDE);
    for name in ["notes", "tracker", "multi", "docs"] {
        assert!(
            claude.contains(&format!("\"{name}\": {{")),
            "{name}: {claude}"
        );
    }
    assert!(
        claude.contains("\"NOTES_TOKEN\": \"${NOTES_TOKEN}\""),
        "{claude}"
    );
    assert!(claude.contains(fake::TRACKER_IMAGE), "{claude}");
    assert!(claude.contains("\"${DOCS_AUTHORIZATION}\""), "{claude}");
    let codex = w.read(CODEX);
    for name in ["notes", "tracker", "multi", "docs"] {
        assert!(
            codex.contains(&format!("[mcp_servers.{name}]")),
            "{name}: {codex}"
        );
    }
    assert!(
        codex.contains("env_http_headers = { Authorization = \"DOCS_AUTHORIZATION\" }"),
        "{codex}"
    );
    let agy = w.read(AGY);
    for name in ["notes", "tracker", "multi"] {
        assert!(agy.contains(&format!("\"{name}\": {{")), "{name}: {agy}");
    }
    assert!(!agy.contains("\"docs\""), "{agy}");
}

#[test]
fn check_exits_0_when_synced_1_after_a_catalog_change_and_2_on_a_refusal() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    let (code, out, _) = w.run(&["check"]);
    assert_eq!(code, 1, "never synced: {out}");
    w.ok(&["sync"]);
    let (code, out, _) = w.run(&["check"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(".mcp.json: no change"), "{out}");

    w.ok(&["disable", "notes"]);
    let before = w.read(CLAUDE);
    let (code, out, _) = w.run(&["check"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("  remove notes"), "{out}");
    assert_eq!(w.read(CLAUDE), before, "check writes nothing");

    w.ok(&["enable", "notes"]);
    let edited = w.read(CLAUDE).replace("notes-mcp@1.2.0", "notes-mcp@9.9.9");
    fs::write(w.app().join(CLAUDE), &edited).unwrap();
    let (code, out, _) = w.run(&["check"]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("was changed by hand since fl wrote it"),
        "{out}"
    );
    assert_eq!(w.read(CLAUDE), edited, "check writes nothing");
    let (out, err) = w.refused(&["sync"]);
    assert!(
        out.contains("was changed by hand since fl wrote it"),
        "{out}"
    );
    assert!(
        err.contains("error: nothing was written; each refusal above names its remedy"),
        "{err}"
    );
    assert_eq!(w.read(CLAUDE), edited);
}

#[test]
fn disable_then_sync_removes_the_server_from_every_file_and_enable_brings_it_back() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    w.ok(&["add", "weather", "--from", fake::WEATHER]);
    w.ok(&["sync"]);
    let (out, _) = w.ok(&["disable", "notes"]);
    assert!(out.contains("`notes` is now off for the team"), "{out}");
    assert!(w.read(CATALOG).contains("enabled = false"));
    w.ok(&["sync"]);
    for rel in [CLAUDE, CODEX, AGY] {
        let text = w.read(rel);
        assert!(!text.contains("notes"), "{rel}: {text}");
        assert!(text.contains("weather"), "{rel}: {text}");
    }
    let (out, _) = w.ok(&["enable", "notes"]);
    assert!(out.contains("`notes` is now on for the team"), "{out}");
    w.ok(&["sync"]);
    for rel in [CLAUDE, CODEX, AGY] {
        assert!(w.read(rel).contains("notes"), "{rel}");
    }
}

#[test]
fn a_machine_switch_turns_a_server_off_on_this_machine_only() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    w.ok(&["add", "weather", "--from", fake::WEATHER]);
    w.ok(&["sync"]);
    let catalog = w.read(CATALOG);
    w.write_config(&format!(
        "[[mcp]]\nroot = \"{}\"\ndisable = [\"notes\"]\n",
        w.app().display()
    ));
    let (out, _) = w.ok(&["sync"]);
    assert!(out.contains("  remove notes"), "{out}");
    for rel in [CLAUDE, CODEX, AGY] {
        let text = w.read(rel);
        assert!(!text.contains("notes"), "{rel}: {text}");
        assert!(text.contains("weather"), "{rel}: {text}");
    }
    assert_eq!(w.read(CATALOG), catalog, "the team default is unchanged");
    // A switch naming a server the catalog lacks is a warning, and the rest
    // goes on: `notes` is on here again.
    w.write_config(&format!(
        "[[mcp]]\nroot = \"{}\"\ndisable = [\"gone\"]\n",
        w.app().display()
    ));
    let (out, err) = w.ok(&["sync"]);
    assert!(
        err.contains(
            "warning: the `[[mcp]]` entry for this project in your fl config has `gone` in \
             `disable`"
        ),
        "{err}"
    );
    assert!(err.contains("has no such server; fl ignores it"), "{err}");
    assert!(out.contains("  add notes"), "{out}");
    assert!(w.read(CLAUDE).contains("notes"));
    let (code, _, err) = w.run(&["check"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("fl ignores it"), "{err}");
    w.write_config("");
    let (_, err) = w.ok(&["sync"]);
    assert!(!err.contains("fl ignores it"), "{err}");
}

#[test]
fn remove_takes_the_server_out_of_the_catalog_and_sync_out_of_every_file() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    w.ok(&["add", "weather", "--from", fake::WEATHER]);
    w.ok(&["sync"]);
    let (out, _) = w.ok(&["remove", "notes"]);
    assert!(out.contains("removed `notes` from"), "{out}");
    assert!(!w.read(CATALOG).contains("[server.notes]"));
    let (code, _, _) = w.run(&["check"]);
    assert_eq!(code, 1, "remove does not sync");
    w.ok(&["sync"]);
    for rel in [CLAUDE, CODEX, AGY] {
        assert!(!w.read(rel).contains("notes"), "{rel}");
    }
    let (_, err) = w.refused(&["remove", "notes"]);
    assert!(
        err.contains("there is no server `notes` in the catalog"),
        "{err}"
    );
}

#[test]
fn upgrade_shows_the_difference_and_rewrites_only_that_entry_without_syncing() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES, "--version", "1.1.0"]);
    w.ok(&["add", "weather", "--from", fake::WEATHER]);
    w.ok(&["sync"]);
    // A person's own choices survive the upgrade.
    w.ok(&["disable", "notes"]);
    let catalog = w.read(CATALOG).replacen(
        "enabled = false\n",
        "enabled = false\nvendors = [\"claude\"]\n",
        1,
    );
    fs::write(w.app().join(CATALOG), &catalog).unwrap();
    let weather = &catalog[catalog.find("[server.weather]").unwrap()..];
    let claude = w.read(CLAUDE);

    let (out, _) = w.ok(&["upgrade", "notes"]);
    assert!(out.contains("upgrade `notes`:"), "{out}");
    assert!(out.contains("  version: \"1.1.0\" -> \"1.2.0\""), "{out}");
    assert!(out.contains("@example/notes-mcp@1.2.0"), "{out}");
    assert!(out.contains("Review and commit it"), "{out}");
    let now = w.read(CATALOG);
    assert!(now.contains("version = \"1.2.0\""), "{now}");
    assert!(
        now.contains("enabled = false\nvendors = [\"claude\"]\n"),
        "{now}"
    );
    assert!(now.ends_with(weather), "only `notes` is rewritten:\n{now}");
    assert_eq!(w.read(CLAUDE), claude, "upgrade does not sync");
    let (code, _, _) = w.run(&["check"]);
    assert_eq!(code, 1);

    let (_, err) = w.refused(&["upgrade", "notes"]);
    assert!(err.contains("is not newer"), "{err}");
    // `--to` moves it anyway, even back.
    let (out, _) = w.ok(&["upgrade", "notes", "--to", "1.1.0"]);
    assert!(out.contains("  version: \"1.2.0\" -> \"1.1.0\""), "{out}");
    let pinned = w.read(CATALOG);
    let (out, _) = w.ok(&["upgrade", "notes", "--to", "1.1.0"]);
    assert!(out.contains("nothing to change"), "{out}");
    assert_eq!(w.read(CATALOG), pinned);
    w.ok(&["add", "local", "--", "node", "server.js"]);
    let (_, err) = w.refused(&["upgrade", "local"]);
    assert!(err.contains("was added by hand"), "{err}");
}

#[test]
fn add_by_hand_records_a_command_and_a_remote_with_secret_headers_by_reference() {
    let w = World::new();
    w.with_registry();
    let (out, err) = w.ok(&[
        "add",
        "local",
        "--env",
        "LOG=debug",
        "--env",
        "API_TOKEN",
        "--",
        "node",
        "server.js",
        "--port",
        "1",
    ]);
    assert!(out.contains("set API_TOKEN"), "{out}");
    assert!(
        out.contains("\n  stdio: node server.js --port 1\n"),
        "{out}"
    );
    assert!(!out.contains("debug"), "a value is never shown: {out}");
    // An argument with a space, an empty one, and one with a control
    // character are shown so they read as one each.
    let (out, _) = w.ok(&[
        "add",
        "spaced",
        "--",
        "node",
        "my server.js",
        "",
        "a\u{1b}b",
    ]);
    assert!(
        out.contains("\n  stdio: node \"my server.js\" \"\" ab\n"),
        "{out}"
    );
    assert!(
        err.contains("warning: `env.LOG` is a literal value: it will be committed"),
        "{err}"
    );
    let catalog = w.read(CATALOG);
    for line in [
        "command = \"node\"",
        "args = [\"server.js\", \"--port\", \"1\"]",
        "env.API_TOKEN = { secret = true }",
        "env.LOG = \"debug\"",
    ] {
        assert!(catalog.contains(line), "{line}: {catalog}");
    }

    let (out, _) = w.ok(&[
        "add",
        "docs",
        "--url",
        fake::DOCS_URL,
        "--header",
        "Authorization=DOCS_TOKEN:Bearer",
        "--header",
        "X-Team",
    ]);
    assert!(out.contains("set DOCS_TOKEN, DOCS_X_TEAM"), "{out}");
    assert!(
        out.contains(&format!(
            "\n  http: {}, headers Authorization, X-Team\n",
            fake::DOCS_URL
        )),
        "{out}"
    );
    let catalog = w.read(CATALOG);
    for line in [
        "transport = \"http\"",
        "headers.Authorization = { secret = true, env = \"DOCS_TOKEN\", scheme = \"Bearer\" }",
        "headers.X-Team = { secret = true, env = \"DOCS_X_TEAM\" }",
    ] {
        assert!(catalog.contains(line), "{line}: {catalog}");
    }
    w.ok(&["sync"]);
    let claude = w.read(CLAUDE);
    assert!(
        claude.contains("\"Authorization\": \"Bearer ${DOCS_TOKEN}\""),
        "{claude}"
    );
    assert!(
        claude.contains("\"API_TOKEN\": \"${API_TOKEN}\""),
        "{claude}"
    );
    assert_eq!(w.requests().len(), 0, "nothing by hand reads the registry");

    // A value where a variable's name goes is refused, and never repeated.
    for given in [
        "Authorization=Bearer ghp_example0token",
        "Authorization=ghp_example0token",
        "Authorization=GHP_EXAMPLE0TOKEN:Bearer abc",
        "Bearer ghp_example0token",
    ] {
        let (_, err) = w.refused(&["add", "leak", "--url", fake::DOCS_URL, "--header", given]);
        assert!(
            err.contains("never a value") || err.contains("starts with a header name"),
            "{given}: {err}"
        );
        assert!(!err.to_lowercase().contains("example0token"), "{err}");
        assert!(!err.contains("abc"), "{err}");
    }
    // `--env NAME` is a secret's variable: a token pasted there is refused,
    // and never repeated.
    for given in ["ghp_example0token", "sk live example0token", "Api_Token"] {
        let (_, err) = w.refused(&["add", "leak", "--env", given, "--", "node"]);
        assert!(
            err.contains("`--env` with no `=` names a secret's environment variable"),
            "{given}: {err}"
        );
        assert!(!err.to_lowercase().contains("example0token"), "{err}");
        assert!(!err.contains("Api_Token"), "{err}");
    }
    assert!(!w.read(CATALOG).contains("leak"));
}

// `upgrade` takes `--env` and `--with` for what the new version needs, so
// the remedies its own refusals name work.
#[test]
fn upgrade_takes_env_and_with_for_what_the_new_version_needs() {
    let w = World::new();
    w.with_registry();
    w.ok(&[
        "add",
        "home",
        "--from",
        fake::UPGRADING,
        "--version",
        "1.0.0",
    ]);
    w.ok(&[
        "add",
        "full",
        "--from",
        fake::UPGRADING,
        "--version",
        "1.0.0",
    ]);
    let (_, err) = w.refused(&["upgrade", "home"]);
    assert!(
        err.contains("its environment variable `UPGRADING_HOME` needs a value"),
        "{err}"
    );
    assert!(
        err.contains("Give it with `--env UPGRADING_HOME=<value>`"),
        "{err}"
    );
    let (out, err) = w.ok(&["upgrade", "home", "--env", "UPGRADING_HOME=/srv/notes"]);
    assert!(out.contains("  version: \"1.0.0\" -> \"2.0.0\""), "{out}");
    assert!(
        err.contains("`--with UPGRADING_TOKEN` includes it"),
        "{err}"
    );
    let (out, err) = w.ok(&[
        "upgrade",
        "full",
        "--env",
        "UPGRADING_HOME=/srv/notes",
        "--with",
        "UPGRADING_TOKEN",
    ]);
    assert!(out.contains("upgrade `full`:"), "{out}");
    assert!(!err.contains("note:"), "{err}");
    let catalog = w.read(CATALOG);
    let full = catalog.find("[server.full]").unwrap();
    let home = catalog.find("[server.home]").unwrap();
    let (full, home) = if full < home {
        (&catalog[full..home], &catalog[home..])
    } else {
        (&catalog[full..], &catalog[home..full])
    };
    for entry in [full, home] {
        assert!(
            entry.contains("env.UPGRADING_HOME = \"/srv/notes\""),
            "{entry}"
        );
    }
    assert!(
        full.contains("env.UPGRADING_TOKEN = { secret = true }"),
        "{full}"
    );
    assert!(!home.contains("UPGRADING_TOKEN"), "{home}");
    let (_, err) = w.refused(&[
        "upgrade",
        "full",
        "--to",
        "2.0.0",
        "--env",
        "UPGRADING_HOME",
    ]);
    assert!(
        err.contains("`--env UPGRADING_HOME` needs `=<value>` with `fl mcp upgrade`"),
        "{err}"
    );
}

#[test]
fn sync_and_check_make_no_request_and_need_no_registry() {
    let mut w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    assert!(!w.requests().is_empty(), "add reads the registry");
    w.fake().state().requests.clear();
    w.ok(&["sync"]);
    let (code, _, _) = w.run(&["check"]);
    assert_eq!(code, 0);
    assert_eq!(w.requests(), Vec::<String>::new());

    w.fake.take();
    w.ok(&["disable", "notes"]);
    w.ok(&["sync"]);
    assert!(!w.read(CLAUDE).contains("notes"));
    let (code, _, err) = w.run(&["check"]);
    assert_eq!(code, 0, "{err}");
    // `add --from` needs the registry, and says so; a name already taken
    // is refused before the registry is read.
    let (_, err) = w.refused(&["add", "weather", "--from", fake::WEATHER]);
    assert!(err.contains("cannot reach the registry"), "{err}");
    let (_, err) = w.refused(&["add", "notes", "--from", fake::NOTES]);
    assert!(err.contains("is already in the catalog"), "{err}");
}

#[test]
fn a_vendor_file_git_does_not_ignore_is_refused_printing_the_lines_to_add() {
    let w = World::ignoring("/.mcp.json\n/.agents/mcp_config.json\n");
    let narrow = "[server.local]\nvendors = [\"claude\", \"antigravity\"]\n\
                  transport = \"stdio\"\ncommand = \"node\"\n";
    fs::create_dir_all(w.app().join(".fl")).unwrap();
    fs::write(w.app().join(CATALOG), narrow).unwrap();
    // A vendor file fl does not write need not be ignored, even holding an
    // entry of someone else's.
    let mine = "[mcp_servers.mine]\ncommand = \"mine\"\n";
    fs::create_dir_all(w.app().join(".codex")).unwrap();
    fs::write(w.app().join(CODEX), mine).unwrap();
    w.ok(&["sync"]);

    fs::write(
        w.app().join(CATALOG),
        narrow.replace("vendors = [\"claude\", \"antigravity\"]\n", ""),
    )
    .unwrap();
    let before = w.read(CLAUDE);
    for cmd in ["sync", "check"] {
        let (out, err) = w.refused(&[cmd]);
        assert!(out.is_empty(), "{cmd} prints no plan: {out}");
        assert!(
            err.contains("  .codex/config.toml is not ignored by git"),
            "{err}"
        );
        assert!(!err.contains("  .mcp.json"), "{err}");
        assert!(
            err.contains(&format!(
                "Make sure {} holds these lines:\n/.codex/config.toml\n",
                w.app().join(".gitignore").display()
            )),
            "{err}"
        );
    }
    assert_eq!(w.read(CODEX), mine, "nothing was written");
    assert_eq!(w.read(CLAUDE), before);
}

#[test]
fn a_vendor_file_fl_no_longer_has_an_entry_in_need_not_be_ignored() {
    let w = World::new();
    let local = "[server.local]\ntransport = \"stdio\"\ncommand = \"node\"\n";
    fs::create_dir_all(w.app().join(".fl")).unwrap();
    fs::write(w.app().join(CATALOG), local).unwrap();
    w.ok(&["sync"]);
    // Removed from Codex's file by hand, no longer wanted there, and the
    // file no longer ignored: fl forgets the entry and writes nothing there.
    fs::write(w.app().join(CODEX), "").unwrap();
    fs::write(
        w.app().join(CATALOG),
        local.replace(
            "[server.local]\n",
            "[server.local]\nvendors = [\"claude\"]\n",
        ),
    )
    .unwrap();
    fs::write(
        w.app().join(".gitignore"),
        "/.mcp.json\n/.agents/mcp_config.json\n",
    )
    .unwrap();
    let (out, _) = w.ok(&["sync"]);
    assert!(out.contains("  forget local"), "{out}");
    assert_eq!(w.read(CODEX), "");
}

#[test]
fn a_tracked_vendor_file_is_refused_naming_git_rm_cached() {
    let w = World::new();
    fs::create_dir_all(w.app().join(".fl")).unwrap();
    fs::write(
        w.app().join(CATALOG),
        "[server.local]\ntransport = \"stdio\"\ncommand = \"node\"\n",
    )
    .unwrap();
    w.ok(&["sync"]);
    git(&w.app(), &["add", "-f", CLAUDE]);
    let (_, err) = w.refused(&["check"]);
    assert!(
        err.contains(
            "  .mcp.json is tracked by git: `git rm --cached .mcp.json` stops tracking it"
        ),
        "{err}"
    );
    assert!(!err.contains(".codex/config.toml is"), "{err}");
}

#[test]
fn codex_trust_is_warned_unless_the_project_root_itself_is_trusted() {
    let w = World::new();
    fs::create_dir_all(w.app().join(".fl")).unwrap();
    fs::write(
        w.app().join(CATALOG),
        "[server.local]\ntransport = \"stdio\"\ncommand = \"node\"\n",
    )
    .unwrap();
    let key = w.app().canonicalize().unwrap();
    let warned = format!(
        "warning: Codex loads {} only in a project its user trusts",
        w.app().join(CODEX).display()
    );
    let lines = format!(
        "[projects.\"{}\"]\ntrust_level = \"trusted\"",
        key.display()
    );

    // No Codex config at all.
    let (_, err) = w.ok(&["sync"]);
    assert!(err.contains(&warned), "{err}");
    assert!(err.contains("does not exist"), "{err}");
    assert!(err.contains(&lines), "{err}");
    // An ancestor's trust does not count.
    w.write_codex(&trusted(w.home()));
    let (_, err) = w.ok(&["check"]);
    assert!(err.contains(&warned), "{err}");
    assert!(err.contains("does not trust"), "{err}");
    // Untrusted, said outright.
    w.write_codex(&trusted(&w.app()).replace("\"trusted\"", "\"untrusted\""));
    let (_, err) = w.ok(&["check"]);
    assert!(err.contains("does not trust"), "{err}");
    // A file that is not TOML is a warning, not a refusal, and shows no line.
    w.write_codex("model = \"o3\"\napi_key = \"sk-example-value\n");
    let (_, err) = w.ok(&["sync"]);
    assert!(err.contains("is not valid TOML (line 2)"), "{err}");
    assert!(!err.contains("sk-example-value"), "{err}");
    // The exact key: silence.
    w.write_codex(&format!("model = \"o3\"\n\n{}", trusted(&w.app())));
    let (_, err) = w.ok(&["sync"]);
    assert!(!err.contains("Codex"), "{err}");
    // `$CODEX_HOME` is read instead of `~/.codex`.
    let elsewhere = w.home().join("codex-home");
    fs::create_dir_all(&elsewhere).unwrap();
    let out = w
        .fl_in(&w.app())
        .env("CODEX_HOME", &elsewhere)
        .args(["mcp", "check"])
        .output()
        .unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains(&warned), "{err}");
    assert!(
        err.contains(&elsewhere.join("config.toml").display().to_string()),
        "{err}"
    );
    // An empty `$CODEX_HOME` is unset.
    let out = w
        .fl_in(&w.app())
        .env("CODEX_HOME", "")
        .args(["mcp", "check"])
        .output()
        .unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(!err.contains("Codex"), "{err}");
    // A file fl cannot read is a warning too.
    fs::create_dir_all(elsewhere.join("config.toml")).unwrap();
    let out = w
        .fl_in(&w.app())
        .env("CODEX_HOME", &elsewhere)
        .args(["mcp", "check"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("fl could not read"), "{err}");
    // A catalog with nothing for Codex needs no trust.
    fs::write(
        w.app().join(CATALOG),
        "[server.local]\nvendors = [\"claude\"]\ntransport = \"stdio\"\ncommand = \"node\"\n",
    )
    .unwrap();
    w.write_codex("");
    let (_, err) = w.ok(&["sync"]);
    assert!(!err.contains("Codex"), "{err}");
}

#[test]
fn a_worktree_is_trusted_through_its_main_checkout() {
    let w = World::new();
    fs::create_dir_all(w.app().join(".fl")).unwrap();
    fs::write(
        w.app().join(CATALOG),
        "[server.local]\ntransport = \"stdio\"\ncommand = \"node\"\n",
    )
    .unwrap();
    git(&w.app(), &["add", CATALOG]);
    git(&w.app(), &["commit", "-qm", "catalog"]);
    let wt = w.home().join("wt");
    git(&w.app(), &["worktree", "add", "-q", wt.to_str().unwrap()]);
    w.write_codex(&trusted(&w.app()));
    let (code, _, err) = w.run_in(&wt, &["sync"]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("Codex"), "{err}");
    assert!(wt.join(CLAUDE).exists(), "the worktree is the root");
    // The worktree's own entry comes first, and decides.
    let own = trusted(&wt).replace("\"trusted\"", "\"untrusted\"");
    w.write_codex(&format!("{own}\n{}", trusted(&w.app())));
    let (code, _, err) = w.run_in(&wt, &["check"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("does not trust"), "{err}");
}

#[test]
fn db_is_refused_with_fl_mcp_and_fl_db_is_ignored() {
    let w = World::new();
    let db = w.home().join("elsewhere").join("fl.redb");
    for args in [
        vec!["--db", db.to_str().unwrap(), "mcp", "check"],
        vec!["mcp", "check", "--db", db.to_str().unwrap()],
    ] {
        let out = w.fl_in(&w.app()).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(
            err.contains("error: `fl mcp` opens no store, so `--db` names nothing here"),
            "{err}"
        );
    }
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    let out = w
        .fl_in(&w.app())
        .env("FL_DB", &db)
        .args(["mcp", "sync"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(!db.exists() && !db.parent().unwrap().exists());
}

#[test]
fn no_fl_mcp_command_creates_or_opens_a_store() {
    let w = World::new();
    w.with_registry();
    let (out, _) = w.ok(&["search", "io.example"]);
    for name in fake::LISTED {
        assert!(out.contains(name), "{name}: {out}");
    }
    assert!(out.contains("The notes server, for fl's tests."), "{out}");
    w.ok(&["add", "notes", "--from", fake::NOTES, "--version", "1.1.0"]);
    w.ok(&["add", "local", "--", "node", "server.js"]);
    w.ok(&["disable", "local"]);
    w.ok(&["enable", "local"]);
    w.ok(&["upgrade", "notes"]);
    w.ok(&["sync"]);
    w.run(&["check"]);
    w.ok(&["sync", "--replace", "notes"]);
    w.ok(&["remove", "local"]);
    assert!(
        !w.home().join("data").exists(),
        "fl mcp created {:?}",
        fs::read_dir(w.home().join("data")).map(|d| d.count())
    );
}

#[test]
fn the_root_is_the_nearest_catalog_even_below_a_nested_repository() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "local", "--", "node", "server.js"]);
    // A repository inside the project, such as a vendored library.
    let inner = w.app().join("vendor").join("lib");
    repo_at(&inner, &[]);
    let deep = inner.join("src");
    fs::create_dir_all(&deep).unwrap();
    let (code, out, err) = w.run_in(&deep, &["sync"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(w.exists(CLAUDE), "the catalog's directory is the root");
    assert!(!inner.join(CLAUDE).exists());
    assert!(!deep.join(CLAUDE).exists());
}

#[test]
fn with_no_catalog_the_root_is_the_nearest_git_directory() {
    let w = World::new();
    let src = w.app().join("src").join("deep");
    fs::create_dir_all(&src).unwrap();
    let (code, _, err) = w.run_in(&src, &["add", "local", "--", "node", "server.js"]);
    assert_eq!(code, 0, "{err}");
    assert!(w.exists(CATALOG), "created at the repository's root");
    assert!(!src.join(CATALOG).exists());
    // `sync` with no catalog is refused, naming how to make one.
    let other = w.home().join("other");
    repo_at(&other, &[(".gitignore", IGNORED)]);
    let (code, _, err) = w.run_in(&other, &["sync"]);
    assert_eq!(code, 2);
    assert!(err.contains("has no MCP catalog"), "{err}");
}

#[test]
fn outside_a_repository_and_any_catalog_fl_mcp_is_refused() {
    let w = World::new();
    let bare = w.home().join("plain");
    fs::create_dir_all(&bare).unwrap();
    let (code, _, err) = w.run_in(&bare, &["add", "local", "--", "node", "server.js"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("is in no git repository and below no MCP catalog"),
        "{err}"
    );
    assert!(!bare.join(".fl").exists());
}

#[test]
fn search_lists_what_the_registry_holds_and_says_when_it_stopped_early() {
    let w = World::new();
    let (_, err) = w.refused(&["search", "notes"]);
    assert!(
        err.contains("names no registry. `fl mcp registry <url>` sets one"),
        "{err}"
    );
    w.with_registry();
    // Registry text reaches the terminal without its control characters.
    w.fake()
        .add_server("io.example/shady\u{1b}[0m", "1.0.0\u{1b}[0m");
    {
        let mut state = w.fake().state();
        let shady = state.entries.last_mut().unwrap();
        shady["server"]["description"] = "\u{1b}[2J\u{7}cleared".into();
        shady["_meta"]["io.modelcontextprotocol.registry/official"]["status"] =
            "\u{1b}[31mpaused".into();
    }
    let (out, _) = w.ok(&["search", "shady"]);
    assert_eq!(
        out,
        "io.example/shady[0m 1.0.0[0m ([31mpaused)  [2Jcleared\n"
    );
    let (out, _) = w.ok(&["search", "legacy"]);
    assert_eq!(
        out,
        format!(
            "{} 0.1.0 (deprecated)  The legacy server, for fl's tests.\n",
            fake::LEGACY
        )
    );
    let (out, _) = w.ok(&["search", "notes"]);
    assert_eq!(
        out,
        format!("{} 1.2.0  The notes server, for fl's tests.\n", fake::NOTES)
    );
    let (out, _) = w.ok(&["search", "nothing-by-that-name"]);
    assert!(
        out.contains("no server's name holds `nothing-by-that-name`"),
        "{out}"
    );
    w.fake().state().page_limit = 1;
    for n in 0..15 {
        w.fake()
            .add_server(&format!("io.example/extra-{n:02}"), "1.0.0");
    }
    let (out, err) = w.ok(&["search", "io.example"]);
    assert_eq!(out.lines().count(), 20, "{out}");
    assert!(
        err.contains("note: fl stopped after 20 pages of results"),
        "{err}"
    );
}

#[test]
fn a_flag_given_twice_or_an_add_with_no_source_is_refused() {
    let w = World::new();
    w.with_registry();
    let (_, err) = w.refused(&["add", "local"]);
    assert!(err.contains("say where `local` comes from"), "{err}");
    let (_, err) = w.refused(&["add", "local", "--env", "A=1", "--env", "A=2", "--", "node"]);
    assert!(err.contains("`--env A` is given twice"), "{err}");
    let (_, err) = w.refused(&[
        "add",
        "docs",
        "--url",
        fake::DOCS_URL,
        "--header",
        "X-Key",
        "--header",
        "X-Key=K",
    ]);
    assert!(err.contains("`--header X-Key` is given twice"), "{err}");
    let (_, err) = w.refused(&[
        "add",
        "weather",
        "--from",
        fake::WEATHER,
        "--env",
        "U=1",
        "--env",
        "U=2",
    ]);
    assert!(err.contains("`--env U=…` is given twice"), "{err}");
    let (_, err) = w.refused(&["add", "weather", "--from", fake::WEATHER, "--env", "UNITS"]);
    assert!(
        err.contains("`--env UNITS` needs `=<value>` with `--from`"),
        "{err}"
    );
    assert!(!w.exists(CATALOG) || !w.read(CATALOG).contains("[server."));
    assert_eq!(
        w.requests().len(),
        0,
        "each is refused before the registry is read"
    );
}

// Each form of `add` takes its own flags; one of another form is refused by
// name, never ignored, and nothing is added.
#[test]
fn a_flag_of_another_form_of_add_is_refused_naming_it() {
    let w = World::new();
    w.with_registry();
    let before = w.read(CATALOG);
    let url = fake::DOCS_URL;
    let cases: [(&[&str], &str); 10] = [
        (
            &["add", "z", "--header", "Authorization", "--", "node"],
            "`--header` has no place in `fl mcp add z -- <command>`: it goes with `--url <url>`",
        ),
        (
            &["add", "z", "--version", "1", "--", "node"],
            "`--version` has no place in `fl mcp add z -- <command>`: it goes with \
             `--from <registry-name>`",
        ),
        (
            &["add", "z", "--package", "npm", "--", "node"],
            "`--package` has no place in `fl mcp add z -- <command>`",
        ),
        (
            &["add", "z", "--remote", "--", "node"],
            "`--remote` has no place in `fl mcp add z -- <command>`",
        ),
        (
            &["add", "z", "--with", "A", "--", "node"],
            "`--with` has no place in `fl mcp add z -- <command>`",
        ),
        (
            &["add", "z", "--version", "1", "--url", url],
            "`--version` has no place in `fl mcp add z --url <url>`: it goes with \
             `--from <registry-name>`",
        ),
        (
            &["add", "z", "--package", "npm", "--url", url],
            "`--package` has no place in `fl mcp add z --url <url>`",
        ),
        (
            &["add", "z", "--remote", "--url", url],
            "`--remote` has no place in `fl mcp add z --url <url>`",
        ),
        (
            &["add", "z", "--with", "A", "--url", url],
            "`--with` has no place in `fl mcp add z --url <url>`",
        ),
        (
            &[
                "add",
                "z",
                "--from",
                fake::NOTES,
                "--header",
                "Authorization",
            ],
            "`--header` has no place in `fl mcp add z --from <registry-name>`: it goes with \
             `--url <url>`",
        ),
    ];
    for (args, phrase) in cases {
        let (_, err) = w.refused(args);
        assert!(err.contains(phrase), "{args:?}: {phrase:?} not in {err}");
    }
    // clap refuses the forms given together, and `--env` with `--url`.
    let clap: [(&[&str], &str); 4] = [
        (
            &["add", "z", "--url", url, "--", "node"],
            "'--url <URL>' cannot be used with",
        ),
        (
            &["add", "z", "--from", fake::NOTES, "--", "node"],
            "'--from <REGISTRY_NAME>' cannot be used with",
        ),
        (
            &["add", "z", "--from", fake::NOTES, "--url", url],
            "'--from <REGISTRY_NAME>' cannot be used with '--url <URL>'",
        ),
        (
            &["add", "z", "--env", "A", "--url", url],
            "'--env <NAME[=VALUE]>' cannot be used with '--url <URL>'",
        ),
    ];
    for (args, phrase) in clap {
        let (_, err) = w.refused(args);
        assert!(err.contains(phrase), "{args:?}: {phrase:?} not in {err}");
    }
    assert_eq!(w.read(CATALOG), before, "nothing was added");
    assert_eq!(
        w.requests().len(),
        0,
        "each is refused before the registry is read"
    );
}

// A vendor file that is a link would carry fl's write into the file it
// points at — here one git tracks, which the gitignore guard, asking about
// the link's own path, cannot see: refused, and nothing is written.
#[test]
fn a_vendor_file_that_links_to_a_tracked_file_is_refused() {
    let w = World::new();
    w.ok(&["add", "local", "--", "node", "server.js"]);
    let shared = "{\"mcpServers\": {}}\n";
    fs::create_dir_all(w.app().join("docs")).unwrap();
    fs::write(w.app().join("docs/shared.json"), shared).unwrap();
    git(&w.app(), &["add", "docs/shared.json"]);
    git(&w.app(), &["commit", "-qm", "shared"]);
    std::os::unix::fs::symlink("docs/shared.json", w.app().join(CLAUDE)).unwrap();
    let (out, err) = w.refused(&["sync"]);
    assert!(
        err.contains(
            ".mcp.json: it is a symbolic link, and fl writes only plain files it can see whole"
        ),
        "{err}"
    );
    assert_eq!(out, "");
    assert_eq!(w.read("docs/shared.json"), shared);
    assert!(!w.exists(CODEX) && !w.exists(AGY), "nothing is written");
    let (code, _, _) = w.run(&["check"]);
    assert_eq!(code, 2);
}

#[test]
fn sync_with_no_state_directory_is_refused() {
    let w = World::new();
    w.ok(&["add", "local", "--", "node", "server.js"]);
    let out = w
        .fl_in(&w.app())
        .env_remove("HOME")
        .env_remove("XDG_STATE_HOME")
        .args(["mcp", "sync"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("fl has nowhere to keep the record"), "{err}");
    assert!(!w.exists(CLAUDE));
    // With a state directory and no home, Codex's config cannot be found.
    let out = w
        .fl_in(&w.app())
        .env_remove("HOME")
        .args(["mcp", "sync"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("neither $CODEX_HOME nor $HOME is set"),
        "{err}"
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-exec --lib git::tests::`
Expected: FAIL to compile (6 errors) — ``error[E0599]: no associated function or constant named `is_tracked` found for struct `git::Git` ``.

Run: `cargo test -p fl-cli --test mcp`
Expected: FAIL — 0 passed, 24 failed; each `fl mcp …` exits 2 with `error: unrecognized subcommand 'mcp'`.

- [ ] **Step 3: Implement**

In `crates/exec/src/git.rs`, in `impl Git`, replace the first line of `is_committed`'s body,

```rust
        let tracked = !git(root, &["ls-files", "--", rel])?.is_empty();
```

with

```rust
        let tracked = Self::is_tracked(root, rel)?;
```

and after `is_committed` (before the doc comment of `is_ignored`) add:

```rust
    /// Whether `rel` (relative to `root`) is in git's index: committed, or
    /// added and not yet committed, ignored or not. A path git does not know
    /// is `false`; a git that cannot answer is an error, never `false`.
    pub fn is_tracked(root: &Path, rel: &str) -> Result<bool, ExecError> {
        Ok(!git(root, &["ls-files", "--", rel])?.is_empty())
    }
```

In `crates/cli/src/cmd/mod.rs`, after `pub mod manifest;` add:

```rust
pub mod mcp;
```

In `crates/cli/src/config.rs`, remove the three attributes Task 2 added, leaving each item as it was: above `pub mcp: Vec<McpEntry>,` in `Config`,

```rust
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by `fl mcp`, not yet built")
    )]
```

above `pub fn fl_state_dir()`,

```rust
#[expect(dead_code, reason = "read by `fl mcp`, not yet built")]
```

and above `pub fn mcp_entry(`,

```rust
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "read by `fl mcp`, not yet built")
)]
```

In `crates/cli/src/main.rs`, in `enum Command`, after the `Routing(cmd::routing::Cmd),` variant add:

```rust
    /// A project's MCP servers: one committed catalog, written into each agent CLI's file.
    #[command(subcommand)]
    Mcp(cmd::mcp::Cmd),
```

In `Command::iris`, after `Command::Routing(c) => c.iris(),` add:

```rust
            Command::Mcp(_) => Vec::new(),
```

In `Command::has_handle`, after `Command::Routing(c) => c.has_handle(),` add:

```rust
            Command::Mcp(_) => false,
```

In `Command::needs_tracker`, replace `| Command::Manifest(_) => false,` with:

```rust
            | Command::Manifest(_)
            | Command::Mcp(_) => false,
```

In `run`, right after `let cfg = config::load(config::path().as_deref())?;` add:

```rust
    // MCP spec §1.1: `fl mcp` never opens a store, so it returns here, before
    // any store path is resolved or its directory created. `$FL_DB` is the
    // environment's, set for every command, so it is ignored here; `--db`
    // was given to this command, and is refused rather than ignored.
    let cli = match cli {
        Cli {
            db,
            command: Command::Mcp(c),
        } => {
            if db.is_some() {
                bail!(
                    "`fl mcp` opens no store, so `--db` names nothing here. Drop it; `fl mcp` \
                     works on the project the current directory is in"
                );
            }
            return cmd::mcp::run(c, &cfg.mcp, &cwd);
        }
        cli => cli,
    };
```

In `run`'s final `match cli.command`, after the `Command::Routing(c) => …` arm add:

```rust
        Command::Mcp(_) => unreachable!("`fl mcp` returns before any store is opened"),
```

In `docs/getting-started.md`, in the `fl --help` transcript, after the line `  routing     Route a project's new items between its local store and GitHub` add:

```text
  mcp         A project's MCP servers: one committed catalog, written into each agent CLI's file
```

In `crates/mcp/src/registry.rs`, replace `pub(crate) fn printable(text: &str) -> String {` with:

```rust
pub fn printable(text: &str) -> String {
```

Create `crates/cli/src/cmd/mcp.rs`:

```rust
//! `fl mcp` (MCP spec §1.2): the project's catalog of MCP servers, the
//! registry it freezes them from, and each agent CLI's own MCP file written
//! from it. It opens no store: `run()` in `main.rs` hands over before any
//! store path is resolved. `sync` and `check` build no registry client, so
//! they have no network path (MCP spec §5).

use crate::config::{self, McpEntry};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};
use fl_exec::git::Git;
use fl_exec::population::ExecError;
use fl_mcp::McpError;
use fl_mcp::catalog::{Catalog, Editor, EnvValue, HeaderValue, Server, Transport, VendorName};
use fl_mcp::freeze::{self, FreezeOptions, Route};
use fl_mcp::registry::{Registry, printable};
use fl_mcp::sync::{self, Action, Plan, Switches};
use fl_mcp::vendor;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum Cmd {
    /// Set the registry the catalog's servers come from.
    Registry { url: String },
    /// List the registry's servers whose name holds TEXT.
    Search { text: String },
    /// Add a server: from the registry (--from), or by hand (--url, or -- <command>).
    Add(Add),
    /// Remove a server from the catalog.
    Remove { name: String },
    /// Turn a server on for the team: its default in the catalog.
    Enable { name: String },
    /// Turn a server off for the team: its default in the catalog.
    Disable { name: String },
    /// Move a registry server to its newer version, showing the difference.
    /// Writes the catalog only: commit it, then `fl mcp sync`.
    Upgrade {
        name: String,
        /// This version, newer or not.
        #[arg(long, value_name = "VERSION")]
        to: Option<String>,
        /// NAME=VALUE: a literal the new version needs, committed with the
        /// catalog.
        #[arg(long = "env", value_name = "NAME=VALUE")]
        env: Vec<String>,
        /// Include an optional variable or argument that needs a secret.
        #[arg(long = "with", value_name = "NAME")]
        with: Vec<String>,
    },
    /// Write each agent CLI's MCP file from the catalog. Reads no registry.
    Sync {
        /// Overwrite this entry although it was changed by hand, or is not
        /// fl's. Repeatable.
        #[arg(long, value_name = "NAME")]
        replace: Vec<String>,
    },
    /// Whether `sync` would change anything: exit 0 (no), 1 (yes), 2 (a
    /// refusal or an error). Writes nothing and reads no registry.
    Check,
}

#[derive(Args)]
pub struct Add {
    /// The server's name in the catalog and in every vendor file.
    name: String,
    /// The registry's name for the server.
    #[arg(long, value_name = "REGISTRY_NAME", conflicts_with_all = ["url", "command"])]
    from: Option<String>,
    /// The registry version to freeze. Default: its latest.
    #[arg(long, value_name = "VERSION", requires = "from")]
    version: Option<String>,
    /// The package to launch, when the registry offers more than one route.
    #[arg(long, value_enum, requires = "from", conflicts_with = "remote")]
    package: Option<Package>,
    /// Connect to the registry's remote, when it offers more than one route.
    #[arg(long, requires = "from")]
    remote: bool,
    /// NAME=VALUE: a literal, committed with the catalog. By hand, NAME
    /// alone is a secret the agent CLI reads from its environment.
    #[arg(long = "env", value_name = "NAME[=VALUE]", conflicts_with = "url")]
    env: Vec<String>,
    /// Include an optional variable or argument that needs a secret.
    #[arg(long = "with", value_name = "NAME", requires = "from")]
    with: Vec<String>,
    /// The URL of a streamable HTTP server, added by hand.
    #[arg(long, conflicts_with = "command")]
    url: Option<String>,
    /// NAME[=ENV[:SCHEME]]: a secret header, read from the variable ENV
    /// (default <SERVER>_<NAME>) and sent after SCHEME (such as Bearer).
    #[arg(long = "header", value_name = "NAME[=ENV[:SCHEME]]", requires = "url")]
    header: Vec<String>,
    /// The command that starts the server, and its arguments, added by hand.
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Package {
    Npm,
    Pypi,
    Oci,
}

pub fn run(cmd: Cmd, switches: &[McpEntry], cwd: &Path) -> Result<i32> {
    let root = root(cwd)?;
    match cmd {
        Cmd::Registry { url } => {
            let mut editor = Editor::open(&root)?;
            editor.set_registry(&url)?;
            editor.save()?;
            println!(
                "the catalog reads servers from {url}: {}",
                editor.path().display()
            );
        }
        Cmd::Search { text } => search(&root, &text)?,
        Cmd::Add(add) => {
            let form = match (&add.from, &add.url, add.command.is_empty()) {
                (Some(_), _, _) => Form::From,
                (None, Some(_), _) => Form::Url,
                (None, None, false) => Form::Command,
                (None, None, true) => bail!(
                    "say where `{}` comes from: `--from <registry-name>`, `--url <url>`, or \
                     `-- <command> [args…]`",
                    add.name
                ),
            };
            refuse_strays(&add, form)?;
            match form {
                Form::From => add_from(&root, add)?,
                Form::Url | Form::Command => add_by_hand(&root, add)?,
            }
        }
        Cmd::Remove { name } => {
            let mut editor = Editor::open(&root)?;
            editor.remove(&name)?;
            editor.save()?;
            println!(
                "removed `{name}` from {}. `fl mcp sync` takes it out of each agent CLI's file",
                editor.path().display()
            );
        }
        Cmd::Enable { name } => set_enabled(&root, &name, true)?,
        Cmd::Disable { name } => set_enabled(&root, &name, false)?,
        Cmd::Upgrade {
            name,
            to,
            env,
            with,
        } => {
            let env = literals(&env, "`fl mcp upgrade`")?;
            upgrade(&root, &name, to.as_deref(), env, with)?
        }
        Cmd::Sync { replace } => {
            let plan = plan(&root, switches, &replace)?;
            println!("{plan}");
            sync::apply(&plan).map_err(|e| match e {
                McpError::Refused { .. } => {
                    anyhow!("nothing was written; each refusal above names its remedy")
                }
                e => e.into(),
            })?;
        }
        Cmd::Check => {
            let plan = plan(&root, switches, &[])?;
            println!("{plan}");
            return Ok(plan.check().exit_code().into());
        }
    }
    Ok(0)
}

/// The three forms of `add` (MCP spec §1.2): from the registry, a server at
/// a URL, or a command.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    From,
    Url,
    Command,
}

impl Form {
    fn shape(self) -> &'static str {
        match self {
            Form::From => "--from <registry-name>",
            Form::Url => "--url <url>",
            Form::Command => "-- <command>",
        }
    }
}

/// A flag of one form of `add` given to another is refused by name, never
/// ignored. clap refuses `--from`, `--url` and a command together, and
/// `--env` with `--url`; its `requires` does not hold once a form's own flag
/// is given, so the flags that need one form are checked here.
fn refuse_strays(add: &Add, form: Form) -> Result<()> {
    let needs = [
        ("--version", add.version.is_some(), Form::From),
        ("--package", add.package.is_some(), Form::From),
        ("--remote", add.remote, Form::From),
        ("--with", !add.with.is_empty(), Form::From),
        ("--header", !add.header.is_empty(), Form::Url),
    ];
    for (flag, given, own) in needs {
        if given && form != own {
            bail!(
                "`{flag}` has no place in `fl mcp add {} {}`: it goes with `{}` only. Drop it",
                add.name,
                form.shape(),
                own.shape()
            );
        }
    }
    Ok(())
}

/// The project root (MCP spec §2.1): the nearest ancestor of `cwd` that
/// holds `.fl/mcp.toml`, else the nearest that holds `.git`, where
/// `registry` and `add` create the catalog. No git runs to find it.
fn root(cwd: &Path) -> Result<PathBuf> {
    let found = (cwd.ancestors().find(|d| Catalog::path(d).is_file()))
        .or_else(|| cwd.ancestors().find(|d| d.join(".git").exists()));
    match found {
        Some(root) => Ok(root.to_path_buf()),
        None => bail!(
            "{} is in no git repository and below no MCP catalog (.fl/mcp.toml), so `fl mcp` \
             has no project to work on. Run it inside the project's repository",
            cwd.display()
        ),
    }
}

/// The catalog `sync` and `check` read; a project without one is refused.
fn catalog(root: &Path) -> Result<Catalog> {
    match Catalog::load(root)? {
        Some(catalog) => Ok(catalog),
        None => bail!(
            "{} has no MCP catalog ({}). `fl mcp add` creates one",
            root.display(),
            Catalog::path(root).display()
        ),
    }
}

/// A client for the registry the catalog names. Only `search`, `add --from`
/// and `upgrade` build one.
fn registry(catalog: &Catalog, path: &Path) -> Result<Registry> {
    let Some(url) = &catalog.registry else {
        bail!(
            "{} names no registry. `fl mcp registry <url>` sets one, such as \
             https://registry.modelcontextprotocol.io",
            path.display()
        );
    };
    Ok(Registry::new(url)?)
}

fn search(root: &Path, text: &str) -> Result<()> {
    let catalog = Catalog::load(root)?.unwrap_or_default();
    let found = registry(&catalog, &Catalog::path(root))?.search(text)?;
    for s in &found.servers {
        // A status other than active is shown as the registry spells it.
        let status = match s.status.as_str() {
            "active" => String::new(),
            other => format!(" ({})", printable(other)),
        };
        // Registry text reaches the terminal without its control characters.
        let (name, version) = (printable(&s.name), printable(&s.version));
        println!("{name} {version}{status}  {}", printable(&s.description));
    }
    if found.servers.is_empty() {
        println!("no server's name holds `{text}`");
    }
    if found.stopped_early {
        eprintln!(
            "note: fl stopped after {} pages of results; a longer TEXT narrows the search",
            fl_mcp::registry::SEARCH_PAGES
        );
    }
    Ok(())
}

fn add_from(root: &Path, add: Add) -> Result<()> {
    let from = add.from.as_deref().expect("add_from is called with --from");
    let mut editor = Editor::open(root)?;
    // Refused before the registry is read.
    if editor.catalog().servers.contains_key(&add.name) {
        return Err(McpError::AlreadyPresent {
            path: editor.path().to_path_buf(),
            name: add.name,
        }
        .into());
    }
    let env = literals(&add.env, "`--from`")?;
    let route = match (add.package, add.remote) {
        (Some(Package::Npm), _) => Some(Route::Npm),
        (Some(Package::Pypi), _) => Some(Route::Pypi),
        (Some(Package::Oci), _) => Some(Route::Oci),
        (None, true) => Some(Route::Remote),
        (None, false) => None,
    };
    let opts = FreezeOptions {
        name: add.name.clone(),
        route,
        env,
        with: add.with.iter().cloned().collect(),
        ..FreezeOptions::default()
    };
    let registry = registry(editor.catalog(), editor.path())?;
    let found = registry.version(from, add.version.as_deref().unwrap_or("latest"))?;
    let frozen = freeze::freeze(&found, &opts)?;
    editor.add(&add.name, &frozen.server)?;
    editor.save()?;
    added(
        &add.name,
        editor.path(),
        &frozen.server,
        &frozen.warnings,
        &frozen.secrets,
        &frozen.notes,
    );
    Ok(())
}

/// `--env NAME=VALUE` for a registry server: literals only, since the
/// registry says which variables are secrets and fl records those by
/// reference itself. `with` names the command, for the refusal.
fn literals(given: &[String], with: &str) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for given in given {
        let Some((name, value)) = given.split_once('=') else {
            bail!(
                "`--env {given}` needs `=<value>` with {with}: the registry says which \
                 variables are secrets, and fl records those by reference itself"
            );
        };
        if env.insert(name.to_string(), value.to_string()).is_some() {
            bail!("`--env {name}=…` is given twice");
        }
    }
    Ok(env)
}

fn add_by_hand(root: &Path, add: Add) -> Result<()> {
    let mut env = BTreeMap::new();
    for given in &add.env {
        let (name, value) = match given.split_once('=') {
            Some((name, value)) => (name, EnvValue::Literal(value.to_string())),
            // A secret reference: the name is the variable, so a token pasted
            // here is refused, and never repeated (MCP spec §6).
            None if !is_variable(given) => bail!(
                "`--env` with no `=` names a secret's environment variable (capital letters, \
                 digits and `_`), never a value; this one is not such a name. fl records a \
                 secret by reference: put the value in that variable, and name the variable"
            ),
            None => (given.as_str(), EnvValue::Secret { env: None }),
        };
        if env.insert(name.to_string(), value).is_some() {
            bail!("`--env {name}` is given twice");
        }
    }
    let mut headers = BTreeMap::new();
    for given in &add.header {
        let (name, value) = header(&add.name, given)?;
        if headers.insert(name.clone(), value).is_some() {
            bail!("`--header {name}` is given twice");
        }
    }
    let server = match &add.url {
        Some(url) => Server {
            url: Some(url.clone()),
            headers: (!headers.is_empty()).then_some(headers),
            ..by_hand(Transport::Http)
        },
        None => {
            let (command, args) = add.command.split_first().expect("a command was given");
            Server {
                command: Some(command.clone()),
                args: (!args.is_empty()).then(|| args.to_vec()),
                env: (!env.is_empty()).then_some(env),
                ..by_hand(Transport::Stdio)
            }
        }
    };
    let mut editor = Editor::open(root)?;
    editor.add(&add.name, &server)?;
    editor.save()?;
    let warnings: Vec<String> = (server.literal_values().iter())
        .map(|field| {
            format!(
                "`{field}` is a literal value: it will be committed with the catalog, and is \
                 public if the repository is"
            )
        })
        .collect();
    let env_secrets = (server.env.iter().flatten()).filter_map(|(k, v)| v.secret_var(k));
    let header_secrets = (server.headers.iter().flatten()).filter_map(|(_, v)| match v {
        HeaderValue::Secret { env, .. } => Some(env.as_str()),
        HeaderValue::Literal(_) => None,
    });
    let secrets: BTreeSet<&str> = env_secrets.chain(header_secrets).collect();
    let secrets: Vec<String> = secrets.into_iter().map(str::to_string).collect();
    added(&add.name, editor.path(), &server, &warnings, &secrets, &[]);
    Ok(())
}

/// A server added by hand: on for the team, for every vendor, nothing set.
fn by_hand(transport: Transport) -> Server {
    Server {
        from: None,
        version: None,
        enabled: true,
        vendors: None,
        transport,
        command: None,
        args: None,
        env: None,
        url: None,
        headers: None,
    }
}

/// `--header NAME[=ENV[:SCHEME]]`: a secret header, recorded by reference
/// (MCP spec §2.1). With no ENV the variable is `<SERVER>_<NAME>`, uppercased
/// with `-` as `_`, and holds the whole value. What follows `=` must look
/// like a variable's name, so a value typed there is refused — and the
/// refusal never repeats it (MCP spec §6).
fn header(server: &str, given: &str) -> Result<(String, HeaderValue)> {
    let (name, rest) = match given.split_once('=') {
        Some((name, rest)) => (name, Some(rest)),
        None => (given, None),
    };
    if !is_token(name) {
        bail!(
            "a `--header` starts with a header name (letters, digits and `-`), then \
             optionally `=<ENV>` and `:<SCHEME>`; this one does not"
        );
    }
    let (env, scheme) = match rest {
        None => (
            format!("{server}_{name}")
                .to_ascii_uppercase()
                .replace('-', "_"),
            None,
        ),
        Some(rest) => {
            let (env, scheme) = match rest.split_once(':') {
                Some((env, scheme)) => (env, Some(scheme)),
                None => (rest, None),
            };
            if !is_variable(env) || !scheme.is_none_or(is_token) {
                bail!(
                    "`--header {name}=…`: after `=` comes the name of an environment variable \
                     (capital letters, digits and `_`), then optionally `:` and a scheme such \
                     as `Bearer`, never a value. fl records a header by reference, and the \
                     agent CLI reads the variable when it starts the server: put the value in \
                     that variable"
                );
            }
            (env.to_string(), scheme.map(str::to_string))
        }
    };
    Ok((name.to_string(), HeaderValue::Secret { env, scheme }))
}

/// A header name or an authentication scheme: letters, digits and `-`.
fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A variable's name as `--header` and `--env NAME` take it: capital letters,
/// digits and `_`, not starting with a digit. Stricter than the catalog's
/// rule, so a pasted token is not taken for a name.
fn is_variable(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// What `add` says: the launch it recorded, the warnings and notes the
/// freeze made, and the variables a person sets.
fn added(
    name: &str,
    path: &Path,
    server: &Server,
    warnings: &[String],
    secrets: &[String],
    notes: &[String],
) {
    println!("added `{name}` to {}", path.display());
    println!("  {}", launch(server));
    for w in warnings {
        eprintln!("warning: {w}");
    }
    if !secrets.is_empty() {
        println!(
            "set {} in the environment the agent CLI starts in; fl records only the name",
            secrets.join(", ")
        );
    }
    for n in notes {
        eprintln!("note: {n}");
    }
    println!("`fl mcp sync` writes it into each agent CLI's file");
}

/// The launch a catalog entry records, as `add` shows it: the command and
/// its arguments, or the URL and the names of its headers — never a value
/// (MCP spec §6). Registry text loses its control characters.
fn launch(server: &Server) -> String {
    let shown = |s: &String| {
        let s = printable(s);
        if s.is_empty() || s.contains(char::is_whitespace) {
            format!("{s:?}")
        } else {
            s
        }
    };
    let transport = server.transport.as_str();
    match server.transport {
        Transport::Stdio => {
            let words = server.command.iter().chain(server.args.iter().flatten());
            let words: Vec<String> = words.map(shown).collect();
            format!("{transport}: {}", words.join(" "))
        }
        Transport::Http | Transport::Sse => {
            let url = server.url.as_ref().map(shown).unwrap_or_default();
            let names: Vec<String> = server
                .headers
                .iter()
                .flatten()
                .map(|(k, _)| shown(k))
                .collect();
            match names.is_empty() {
                true => format!("{transport}: {url}"),
                false => format!("{transport}: {url}, headers {}", names.join(", ")),
            }
        }
    }
}

fn set_enabled(root: &Path, name: &str, enabled: bool) -> Result<()> {
    let mut editor = Editor::open(root)?;
    editor.set_enabled(name, enabled)?;
    editor.save()?;
    println!(
        "`{name}` is now {} for the team in {}. `fl mcp sync` writes the change",
        if enabled { "on" } else { "off" },
        editor.path().display()
    );
    Ok(())
}

/// MCP spec §3.3: the registry's version, frozen with the pinned entry's
/// choices, shown against the pinned one and written in its place. Never
/// syncs: the change reaches the vendor files after it is committed.
fn upgrade(
    root: &Path,
    name: &str,
    to: Option<&str>,
    env: BTreeMap<String, String>,
    with: Vec<String>,
) -> Result<()> {
    let mut editor = Editor::open(root)?;
    let Some(pinned) = editor.catalog().servers.get(name).cloned() else {
        return Err(McpError::NoSuchServer {
            path: editor.path().to_path_buf(),
            name: name.to_string(),
        }
        .into());
    };
    let (Some(from), Some(version)) = (&pinned.from, &pinned.version) else {
        bail!(
            "server `{name}` was added by hand, so the registry has no newer version of it. \
             Edit {} by hand",
            editor.path().display()
        );
    };
    let registry = registry(editor.catalog(), editor.path())?;
    let found = registry.version(from, to.unwrap_or("latest"))?;
    freeze::check_upgrade(name, version, &found.server.version, to.is_some())?;
    // `--env` and `--with` add to what the pinned entry chose, where the new
    // version needs it.
    let opts = FreezeOptions {
        env,
        with: with.into_iter().collect(),
        ..FreezeOptions::upgrading(name, &pinned)
    };
    let mut frozen = freeze::freeze(&found, &opts)?;
    frozen.server.enabled = pinned.enabled;
    frozen.server.vendors = pinned.vendors.clone();
    let changes = freeze::diff(&pinned, &frozen.server);
    if changes.is_empty() {
        println!("`{name}` is already as {from} {version} freezes it; nothing to change");
        return Ok(());
    }
    println!("upgrade `{name}`:");
    for change in &changes {
        println!("  {change}");
    }
    for w in &frozen.warnings {
        eprintln!("warning: {w}");
    }
    for n in &frozen.notes {
        eprintln!("note: {n}");
    }
    editor.replace(name, &frozen.server)?;
    editor.save()?;
    println!(
        "rewrote `{name}` in {}. Review and commit it; `fl mcp sync` then writes it into each \
         agent CLI's file",
        editor.path().display()
    );
    Ok(())
}

/// The plan `sync` applies and `check` reports, refused when git would
/// commit a file it writes, with a warning when Codex will not read its file.
fn plan(root: &Path, switches: &[McpEntry], replace: &[String]) -> Result<Plan> {
    let catalog = catalog(root)?;
    let switches = match config::mcp_entry(switches, root)? {
        Some(e) => Switches {
            enable: e.enable,
            disable: e.disable,
        },
        None => Switches::default(),
    };
    let records = config::fl_state_dir()
        .context(
            "neither an absolute $XDG_STATE_HOME nor $HOME is set, so fl has nowhere to keep \
             the record of the entries it writes",
        )?
        .join("mcp");
    let plan = sync::plan(root, &catalog, &switches, &records, replace)?;
    for warning in plan.warnings() {
        eprintln!("warning: {warning}");
    }
    ignored(root, &plan)?;
    if let Some(warning) = codex_trust(root, &plan) {
        eprintln!("warning: {warning}");
    }
    Ok(plan)
}

/// Whether the target holds, or will hold, an entry of fl's.
fn fl_writes(target: &sync::Target) -> bool {
    (target.entries.iter()).any(|e| !matches!(e.action, Action::Untouched | Action::Forget))
}

/// MCP spec §4.4: every vendor file fl writes is generated on each machine,
/// so git must ignore it. A file that is tracked, or that git would not
/// ignore, is refused, naming the lines to add; fl does not edit
/// `.gitignore`. Asked of git, which fails rather than answer `no`.
fn ignored(root: &Path, plan: &Plan) -> Result<()> {
    let (mut problems, mut lines) = (Vec::new(), Vec::new());
    for target in plan.targets().iter().filter(|t| fl_writes(t)) {
        let rel = vendor::vendor(target.vendor).target();
        let git = |e: ExecError| {
            anyhow!("{e}. fl asks git whether {rel} is ignored before writing it (MCP spec §4.4)")
        };
        if Git::is_tracked(root, rel).map_err(git)? {
            problems.push(format!(
                "  {rel} is tracked by git: `git rm --cached {rel}` stops tracking it"
            ));
        } else if !Git::is_ignored(root, rel).map_err(git)? {
            problems.push(format!("  {rel} is not ignored by git"));
        } else {
            continue;
        }
        lines.push(format!("/{rel}"));
    }
    if problems.is_empty() {
        return Ok(());
    }
    bail!(
        "fl writes these files on each machine from the catalog, so git must ignore them:\n{}\n\
         Make sure {} holds these lines:\n{}\nfl does not edit .gitignore. Nothing was written",
        problems.join("\n"),
        root.join(".gitignore").display(),
        lines.join("\n")
    )
}

/// MCP spec §4.1: Codex reads a project's `.codex/config.toml` only when its
/// user trusts the project, by an exact key — the project root (the
/// directory holding `.git`), else the main checkout's root; an ancestor's
/// trust does not count. `$CODEX_HOME/config.toml` (default
/// `~/.codex/config.toml`) is read, never written. A warning when fl has an
/// entry there that Codex will not load; a file fl cannot read or parse is
/// one too, never a refusal, and shows none of its text.
fn codex_trust(root: &Path, plan: &Plan) -> Option<String> {
    let codex = plan
        .targets()
        .iter()
        .find(|t| t.vendor == VendorName::Codex)?;
    let writes = (codex.entries.iter()).any(|e| {
        !matches!(
            e.action,
            Action::Untouched | Action::Remove | Action::Forget
        )
    });
    if !writes {
        return None;
    }
    let keys = trust_keys(root);
    let key = keys[0].to_string_lossy().to_string();
    let config = match std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        Some(home) => Some(PathBuf::from(home).join("config.toml")),
        None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex/config.toml")),
    };
    let why = match &config {
        None => "neither $CODEX_HOME nor $HOME is set, so fl cannot read Codex's config".into(),
        Some(file) => match std::fs::read_to_string(file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                format!("{} does not exist", file.display())
            }
            Err(e) => format!("fl could not read {} ({e})", file.display()),
            Ok(text) => match text.parse::<toml::Table>() {
                Err(e) => {
                    let at = e.span().map_or(0, |s| s.start).min(text.len());
                    let line = text[..at].matches('\n').count() + 1;
                    format!(
                        "{} is not valid TOML (line {line}), so fl cannot tell whether Codex \
                         trusts this project",
                        file.display()
                    )
                }
                Ok(table) if trusts(&table, &keys) => return None,
                Ok(_) => format!("{} does not trust {key}", file.display()),
            },
        },
    };
    let file = config.map_or("Codex's config".into(), |f| f.display().to_string());
    Some(format!(
        "Codex loads {} only in a project its user trusts, and {why}. To trust it, add to \
         {file}:\n[projects.{}]\ntrust_level = \"trusted\"",
        root.join(".codex/config.toml").display(),
        toml::Value::String(key.clone())
    ))
}

/// The keys Codex looks this project up by, in its order: the project root
/// (the nearest directory holding `.git`, canonical), then the main
/// checkout's root when that is a linked worktree. No `.git`: the root.
fn trust_keys(root: &Path) -> Vec<PathBuf> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let Some(project) = root.ancestors().find(|d| d.join(".git").exists()) else {
        return vec![root];
    };
    let mut keys = vec![project.to_path_buf()];
    if let Some(main) = main_checkout(project).filter(|m| m != project) {
        keys.push(main);
    }
    keys
}

/// The main checkout of a linked worktree: its `.git` is a file naming the
/// worktree's git directory, whose `commondir` names the main `.git`.
fn main_checkout(project: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(project.join(".git")).ok()?;
    let gitdir = project.join(text.strip_prefix("gitdir:")?.trim());
    let common = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = gitdir.join(common.trim()).canonicalize().ok()?;
    common.parent().map(Path::to_path_buf)
}

/// Codex's rule: the first key with a `[projects."<key>"]` entry decides,
/// and only `trust_level = "trusted"` trusts.
fn trusts(config: &toml::Table, keys: &[PathBuf]) -> bool {
    let Some(projects) = config.get("projects").and_then(toml::Value::as_table) else {
        return false;
    };
    let entry = (keys.iter())
        .filter_map(|k| k.to_str())
        .find_map(|k| projects.get(k));
    entry
        .and_then(|e| e.get("trust_level"))
        .and_then(toml::Value::as_str)
        == Some("trusted")
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-exec --lib git::tests::`
Expected: PASS — 10 passed, including `git::tests::is_tracked_tells_a_tracked_path_from_an_untracked_or_ignored_one` and `git::tests::is_tracked_outside_a_repository_is_an_error_not_false`.

Run: `cargo test -p fl-cli --test mcp`
Expected: PASS — 24 passed: `registry_and_add_from_it_then_sync_write_every_vendor_file`, `check_exits_0_when_synced_1_after_a_catalog_change_and_2_on_a_refusal`, `disable_then_sync_removes_the_server_from_every_file_and_enable_brings_it_back`, `a_machine_switch_turns_a_server_off_on_this_machine_only`, `remove_takes_the_server_out_of_the_catalog_and_sync_out_of_every_file`, `upgrade_shows_the_difference_and_rewrites_only_that_entry_without_syncing`, `upgrade_takes_env_and_with_for_what_the_new_version_needs`, `add_by_hand_records_a_command_and_a_remote_with_secret_headers_by_reference`, `sync_and_check_make_no_request_and_need_no_registry`, `a_vendor_file_git_does_not_ignore_is_refused_printing_the_lines_to_add`, `a_vendor_file_fl_no_longer_has_an_entry_in_need_not_be_ignored`, `a_tracked_vendor_file_is_refused_naming_git_rm_cached`, `a_vendor_file_that_links_to_a_tracked_file_is_refused`, `codex_trust_is_warned_unless_the_project_root_itself_is_trusted`, `a_worktree_is_trusted_through_its_main_checkout`, `db_is_refused_with_fl_mcp_and_fl_db_is_ignored`, `no_fl_mcp_command_creates_or_opens_a_store`, `the_root_is_the_nearest_catalog_even_below_a_nested_repository`, `with_no_catalog_the_root_is_the_nearest_git_directory`, `outside_a_repository_and_any_catalog_fl_mcp_is_refused`, `search_lists_what_the_registry_holds_and_says_when_it_stopped_early`, `a_flag_given_twice_or_an_add_with_no_source_is_refused`, `a_flag_of_another_form_of_add_is_refused_naming_it`, `sync_with_no_state_directory_is_refused`.

Run: `cargo test -p fl-cli --test getting_started`
Expected: PASS — the help transcript holds the `mcp` line.

- [ ] **Step 5: Mutation checks**

Each is one edit of the named file; save a copy first, restore it after each, and `cmp` against the copy. "Run X" is `cargo test -p fl-cli --test mcp -- X` (for fl-exec, `cargo test -p fl-exec --lib git::tests::X`); each goes red.

`crates/cli/src/main.rs`:
1. The early return is before any store: insert `let (p, _) = db_path(explicit_db(None), None)?; let _store = RedbStore::open(&p)?;` before `return cmd::mcp::run(c, &cfg.mcp, &cwd);` (the store opened first, as the other commands do) → run `no_fl_mcp_command_creates_or_opens_a_store` (`data/fl/fl.redb` appears).
2. `--db` is refused: `if false && db.is_some() {` → run `db_is_refused_with_fl_mcp_and_fl_db_is_ignored`.

`crates/cli/src/cmd/mcp.rs` — the root and the catalog:
3. The catalog arm: `let found = cwd.ancestors().find(|d| d.join(".git").exists());` for the two-arm `let found = …;` → run `the_root_is_the_nearest_catalog_even_below_a_nested_repository` (the nested repository is taken for the root, and has no catalog).
4. The `.git` arm: `.or_else(|| cwd.ancestors().find(|d| d.join(".git").exists()).map(|_| cwd));` → run `with_no_catalog_the_root_is_the_nearest_git_directory` (the catalog lands in `src/deep`).
5. Neither is refused: `match found.or(Some(cwd)) {` → run `outside_a_repository_and_any_catalog_fl_mcp_is_refused`.
6. No catalog is not an empty one: `match Catalog::load(root)?.or(Some(Catalog::default())) {` in `catalog` → run `with_no_catalog_the_root_is_the_nearest_git_directory`.

The gitignore guard (`ignored`, `fl_writes`, `run`):
7. The tracked arm: `if false && Git::is_tracked(root, rel).map_err(git)? {` → run `a_tracked_vendor_file_is_refused_naming_git_rm_cached` (named "not ignored", no `git rm --cached`).
8. The unignored arm: `} else if false && !Git::is_ignored(root, rel).map_err(git)? {` → run `a_vendor_file_git_does_not_ignore_is_refused_printing_the_lines_to_add`.
9. Only the targets fl writes: `for target in plan.targets().iter() {` → run `a_vendor_file_git_does_not_ignore_is_refused_printing_the_lines_to_add` (the first sync, with nothing for Codex, is refused).
10. An untouched entry is not fl's: `!matches!(e.action, Action::Forget)` in `fl_writes` → same test (Codex's file holds only `mine`).
11. A forgotten entry is not fl's: `!matches!(e.action, Action::Untouched)` in `fl_writes` → run `a_vendor_file_fl_no_longer_has_an_entry_in_need_not_be_ignored`.
12. The guard runs before the plan is printed: in `Cmd::Sync`, print a plan made by `sync::plan` directly, then call `plan(…)` → run `a_vendor_file_git_does_not_ignore_is_refused_printing_the_lines_to_add` (stdout is not empty).

Codex trust (`codex_trust`, `trust_keys`, `trusts`):
13. The key is exact: `.find_map(|k| projects.iter().find(|(p, _)| Path::new(k).starts_with(p.as_str())).map(|(_, v)| v));` → run `codex_trust_is_warned_unless_the_project_root_itself_is_trusted` (the home's entry silences it).
14. A worktree's main checkout: `main_checkout(project).filter(|_| false)` → run `a_worktree_is_trusted_through_its_main_checkout`.
15. The first key with an entry decides: `.filter_map(|k| projects.get(k)).find(|e| e.get("trust_level").and_then(toml::Value::as_str) == Some("trusted"));` → run `a_worktree_is_trusted_through_its_main_checkout` (the worktree's own `untrusted` is passed over).
16. Only `trusted` trusts: `.is_some()` for `== Some("trusted")` → run `codex_trust_is_warned_unless_the_project_root_itself_is_trusted`.
17. Only when fl has an entry in Codex's file: `if false {` for `if !writes {` → same test (the last sync, with nothing for Codex, warns).
18. A removal is not an entry: drop `Action::Remove |` from `writes` → same test.
19. `$CODEX_HOME`: `std::env::var_os("CODEX_HOME_UNSET")` → same test (the trusted `~/.codex` is read).
20. A file that is not TOML is a warning: `if e.span().is_some() { return None; }` at the top of the parse-error arm → same test.
21. … and shows none of the file's text: append `: {e}` to its message → same test (`sk-example-value` is printed).
22. `check`'s exit code: `return Ok(0);` → run `check_exits_0_when_synced_1_after_a_catalog_change_and_2_on_a_refusal`.
23. `return Ok(i32::from(plan.check() != sync::Check::Matches));` → same test (1 for a refusal).

No network on `sync` and `check`:
24. Insert `registry(&catalog, &Catalog::path(root))?.search("x")?;` before `sync::plan` in `plan` → run `sync_and_check_make_no_request_and_need_no_registry`.
25. A name already taken is refused before the registry is read: `if false && editor.catalog().servers.contains_key(&add.name) {` → same test ("cannot reach the registry").

The switches, `add` and `upgrade`:
26. This machine's switches: `let switches = match None::<McpEntry> {` → run `a_machine_switch_turns_a_server_off_on_this_machine_only`.
27. `--package oci`: `(Some(Package::Oci), _) => Some(Route::Npm),` → run `registry_and_add_from_it_then_sync_write_every_vendor_file`.
28. `--remote`: `(None, true) => None,` → same test (`MULTI` offers three routes).
29. `--with`: `with: BTreeSet::new(),` → same test (a note, no `TRACKER_TOKEN`).
30. After `=` a variable's name, by `--header`'s rule: `!fl_mcp::catalog::is_env_name(env)` for `!is_variable(env)` → run `add_by_hand_records_a_command_and_a_remote_with_secret_headers_by_reference` (`ghp_example0token` is taken for a name).
31. A scheme is one token: `if !is_variable(env) {` → same test.
32. A header name first: `if false && !is_token(name) {` → same test (the catalog's refusal repeats the token).
33. `--env` with `--from` needs a value: `given.split_once('=').or(Some((given.as_str(), "")))` → run `a_flag_given_twice_or_an_add_with_no_source_is_refused`.
34. `--env` twice with `--from`: `… .is_some() && false {` → same test.
35. `--env` twice by hand: likewise → same test.
36. `--header` twice: likewise → same test.
37. `upgrade` keeps `enabled`: delete `frozen.server.enabled = pinned.enabled;` → run `upgrade_shows_the_difference_and_rewrites_only_that_entry_without_syncing`.
38. … and `vendors`: delete `frozen.server.vendors = pinned.vendors.clone();` → same test.
39. Nothing to change writes nothing: `if false {` for `if changes.is_empty() {` → same test.
40. A version that is not newer needs `--to`: `true` for `to.is_some()` in `check_upgrade` → same test.
41. `search` says when it stopped early: `if false {` for `if found.stopped_early {` → run `search_lists_what_the_registry_holds_and_says_when_it_stopped_early`.
42. … and when it found nothing: `if false {` for `if found.servers.is_empty() {` → same test.
43. No state directory is refused: `.or(Some(PathBuf::from("/nonexistent-state")))` after `config::fl_state_dir()` → run `sync_with_no_state_directory_is_refused`.
44. An `add` with no source is refused: `(None, None, false) => Form::Command,` → `(None, None, _) => Form::Command,` → run `a_flag_given_twice_or_an_add_with_no_source_is_refused` (a panic, exit 101).
45. An empty `$CODEX_HOME` is unset: `match std::env::var_os("CODEX_HOME") {` → run `codex_trust_is_warned_unless_the_project_root_itself_is_trusted`.
46. `--package npm`: `(Some(Package::Npm), _) => Some(Route::Oci),` → run `registry_and_add_from_it_then_sync_write_every_vendor_file`.
47. `--package pypi`: `(Some(Package::Pypi), _) => Some(Route::Npm),` → same test.
48. `search` shows a status that is not `active`: `other => format!(" ({other})"),` → `_ => String::new(),` → run `search_lists_what_the_registry_holds_and_says_when_it_stopped_early` (`(deprecated)` is missing).
49. … and not `active`: `"active" => String::new(),` → `"active" => " (active)".into(),` → same test.

Each form's own flags ("forms" is `a_flag_of_another_form_of_add_is_refused_naming_it`):
50. The check runs: wrap `refuse_strays(&add, form)?;` in `if false { … }` → run forms (`--header … -- node` is added).
51. A stray flag is refused: `if given && form != own {` → `if given && false {` → run forms.
52. … only outside its own form: `if given && form != own {` → `if given {` → run `registry_and_add_from_it_then_sync_write_every_vendor_file` (`--with` with `--from`) and `add_by_hand_records_a_command_and_a_remote_with_secret_headers_by_reference` (`--header` with `--url`).
53. `--version` is seen: `add.version.is_some()` → `false` in its row → run forms (`--version 1 -- node` is added).
54. `--package` is seen: `add.package.is_some()` → `false` → run forms.
55. `--remote` is seen: `add.remote` → `false` → run forms.
56. `--with` is seen: `!add.with.is_empty()` → `false` → run forms.
57. `--header` is seen: `!add.header.is_empty()` → `false` → run forms.
58. `--header` belongs to `--url`: `Form::Url` → `Form::Command` in its row → run forms.
59. `--version` belongs to `--from`: `Form::From` → `Form::Command` in its row → run forms.
60. A URL is its own form: `(None, Some(_), _) => Form::Url,` → `(None, Some(_), _) => Form::Command,` → run forms (the message names `-- <command>`).
61. clap: `--env` with `--url`: delete `conflicts_with = "url"` from `env`'s `#[arg]` → run forms.
62. clap: `--from` with a command: `conflicts_with_all = ["url", "command"]` → `conflicts_with = "url"` → run forms.
63. clap: `--from` with `--url`: `conflicts_with_all = ["url", "command"]` → `conflicts_with = "command"` → run forms.
64. clap: `--url` with a command: delete `conflicts_with = "command"` from `url`'s `#[arg]` → run forms.

Registry text, the launch, `--env NAME`, `upgrade`'s flags and the plan's warnings:
67. `search` shows a status without control characters: `other => format!(" ({other})"),` for `other => format!(" ({})", printable(other)),` → run `search_lists_what_the_registry_holds_and_says_when_it_stopped_early` (an escape reaches stdout).
68. … the version: `s.version.clone()` for `printable(&s.version)` → same test.
69. … the name: `s.name.clone()` for `printable(&s.name)` → same test.
70. … the description: `s.description` for `printable(&s.description)` → same test.
71. The plan's warnings are printed: delete the `for warning in plan.warnings() { … }` loop in `plan` → run `a_machine_switch_turns_a_server_off_on_this_machine_only`.
72. `--env NAME` by hand is a variable's name: `None if false && !is_variable(given) => bail!(` → run `add_by_hand_records_a_command_and_a_remote_with_secret_headers_by_reference` (`ghp_example0token` is recorded as a key).
73. `add` prints the launch: delete `println!("  {}", launch(server));` → same test.
74. … with its headers' names: `false => format!("{transport}: {url}"),` → same test.
75. … an empty argument quoted: `if s.contains(char::is_whitespace) {` → same test.
76. … an argument with a space quoted: `if s.is_empty() {` → same test.
77. … without control characters: `let s = s.clone();` for `let s = printable(s);` → same test.
78. `add --from` prints the launch it froze: `&by_hand(Transport::Stdio),` for `&frozen.server,` → run `registry_and_add_from_it_then_sync_write_every_vendor_file`.
79. `upgrade --env` reaches the freeze: `env: BTreeMap::new(),` for `env,` in `upgrade`'s `FreezeOptions` → run `upgrade_takes_env_and_with_for_what_the_new_version_needs`.
80. `upgrade --with` reaches it: `with: BTreeSet::new(),` → same test.
81. `upgrade --env` is read: ``let env = BTreeMap::new();`` for ``let env = literals(&env, "`fl mcp upgrade`")?;`` → same test.
82. A vendor file that links to a tracked file is refused before the gitignore guard can be fooled: in `crates/mcp/src/sync.rs`, `if let Some(link) = link_in(owner, Path::new(v.target()))? && false {` → run `a_vendor_file_that_links_to_a_tracked_file_is_refused` (the guard asks about `.mcp.json`, which is ignored, and the write lands in `docs/shared.json`).

`crates/exec/src/git.rs`:
65. `Ok(git(root, &["ls-files", "--", rel])?.is_empty())` in `is_tracked` → run `is_tracked_tells_a_tracked_path_from_an_untracked_or_ignored_one`.
66. A git that cannot answer is an error: `.unwrap_or_default()` for `?` → run `is_tracked_outside_a_repository_is_an_error_not_false`.

Not observable:
- clap's `requires` on `--version`, `--package`, `--remote`, `--with` and `--header`: once a form is given, `refuse_strays` refuses first-hand what `requires` would; with no form, deleting one only turns clap's refusal into `say where … comes from` (exit 2 either way).
- The `Command::Mcp` arms of `iris`, `has_handle`, `needs_tracker` and the final dispatch: `run()` returns for `fl mcp` before any of them is reached; they exist because the matches are exhaustive.
- `trust_keys` with no `.git` above the root (a catalog outside any repository): the gitignore guard runs first, and git, which cannot answer there, refuses the command.
- `.filter(|m| m != project)` in `trust_keys`: a main checkout's `.git` is a directory, so `main_checkout` is `None` for it; only a linked worktree reaches the filter, and its root is never the main checkout's.
- Wording: the help text, the `add`/`remove`/`enable`/`disable`/`upgrade` reports, the unreadable-file and no-home warnings and the no-`$HOME` refusal are pinned by their phrases in the tests above; the arms that choose them are the guards listed.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1333 passed, 19 ignored (1307 and 19 before this task).

- [ ] **Step 7: Commit**

```bash
git add Cargo.lock crates/cli/Cargo.toml crates/cli/src/cmd/mod.rs crates/cli/src/cmd/mcp.rs crates/cli/src/config.rs crates/cli/src/main.rs crates/cli/tests/mcp.rs crates/exec/src/git.rs crates/mcp/src/registry.rs docs/getting-started.md
git commit -m "feat(cli): fl mcp: the catalog, the registry and the vendor files

fl mcp registry, search, add, remove, enable, disable, upgrade, sync
and check. The project root is the nearest directory holding
.fl/mcp.toml, else the nearest holding .git, where registry and add
create the catalog; neither is refused. fl mcp returns from run()
right after the config is loaded, before any store path is resolved,
so no fl mcp command creates or opens a store; --db is refused with it
and FL_DB, the environment's, is ignored.

add --from freezes a registry entry and prints its warnings, the
variables to set and the optional secrets it left out; add by hand
takes a command, or a URL with secret headers recorded by reference
as NAME=ENV:SCHEME, refusing a value typed where a variable's name
goes without repeating it. A flag of one form given to another, such as
--header with a command or --version without --from, is refused by
name, never ignored. add prints the launch it recorded, never a value;
by hand, --env NAME must be a variable's name. search shows a status
other than active, and no registry text reaches the terminal with its
control characters. sync and check warn about a switch naming a server
the catalog lacks. upgrade takes --env and --with for what the new
version needs, shows the difference and rewrites that entry only; it
does not sync. sync and check plan from the
catalog and this machine's [[mcp]] switches, with the records under
the XDG state directory, and build no registry client. Both refuse a
vendor file fl writes that git tracks or would not ignore, printing
the lines to add, and warn when Codex does not trust the project by
its exact key, read from CODEX_HOME or ~/.codex, never written. check
exits 0, 1 or 2. fl-exec gains Git::is_tracked. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 8: Secrets, the home guard, and the docs

Two invariants of the whole feature are proven from the outside here, against the binary Tasks 1–7 built. **Secrets** (MCP spec §6, §7.2, Review Focus 1): "No secret value in the catalog, a vendor file, a record, argv, or an error message." A black-box test sets a real-looking value for every variable a secret reads — each registry route that carries one (npm, PyPI, the docker `-e` of an OCI image with its optional secret included by `--with`, a remote's header), a hand-added `--env NAME` and `--header`, and a secret read from a variable of another name — runs `registry`, `add`, `sync`, `check`, `upgrade` and refusals, and after every command searches the catalog, each vendor file, each ownership record, the arguments fl started git with (a wrapper ahead of git on `PATH` logs them) and everything fl printed. Every value holds one marker, so a value cut short is found too. `add --from … --env NAME=<value>` for a variable the registry marks secret is refused without repeating the value (Task 4), and so is a token typed after `--header NAME=` (Task 7). A person who pastes a token into fl's entry is refused naming the field only (plan ruling 29), and `--replace` shows the same and writes the reference back. A token pasted into the catalog where its grammar has no place for it — unquoted, where a boolean goes, as the transport, inside a secret's table, as a key — makes `fl mcp check` refuse with the line and column and never the token, even where the parser would quote it back, and a token where a header's variable name goes makes a rule refuse naming the field, not the token (plan ruling 36); for these commands the catalog itself, which holds the token, is not searched. And a token given as `--env <token>` by hand is refused without being repeated (plan ruling 34). **The home guard** (spec §7.1, "an invariant for tests"): a test runs every `fl mcp` command with nothing of the environment but `PATH`, `HOME` and the `XDG_*` bases (and `TMPDIR`), the home and the project in two temporary directories, and fails if fl writes anything outside its records (`$XDG_STATE_HOME/fl/mcp`) and the project's catalog and vendor files — a listing with modification times and bytes, before and after each command — or names a path outside the two directories in anything it prints. The refusals it provokes name each kind of file fl reads (the config, the catalog, a vendor file, Codex's config, a record, `.gitignore`), so a read elsewhere shows. It proves where the records are, and that this process's own `$HOME` is never named. It is proven from outside: fl gains no debugging switch. Both tests pass when written: Tasks 1–7 built the behaviour, and the mutation checks (Step 5) show each search bites.

**The docs** (spec §8): `docs/mcp.md` for a person — the catalog and every field, secret references and why values are never written, the machine switches, the three vendors and their limits (Claude Code's unset-variable behaviour; Codex's trust and streamable HTTP only; Antigravity's project file, inherited environment, no secret header, no SSE), the commands, `check` as a gate, the `.gitignore` lines, each refusal family and why, the registry (no credential; search by name; deprecated and deleted) and where the ownership records live — including that `search` shows a status other than `active` (plan ruling 31), each form of `add` takes its own flags (ruling 37), and a catalog that does not parse is refused with its line and column, never the line's text (ruling 36). A test holds the page to the binary: every subcommand `fl mcp --help` lists, and every key and value the catalog's own refusals list (serde's "expected one of …"), must be named in it, and `README.md` and `docs/README.md` must link it — so a new command or field cannot ship undocumented. That test is this task's failing test. Task 7 added the `fl --help` line to `docs/getting-started.md`; it is not added again, and that page's executed test still passes (65 commands). The repository is public: the page names no machine path or user (`/home/you/code/app` is the existing docs' placeholder), and its examples are `io.example/…` and `example.com`.

**Blast radius:** tests and documents only; no code changes. `crates/cli/tests/mcp.rs` (Task 7's file) gains four `use` lines and, at its end, three tests and their helpers; Task 7's `World` and helpers are used unchanged. `README.md` and `docs/README.md` gain one link each.

**Files:**
- Modify: `crates/cli/tests/mcp.rs` (imports; `MARK`, `SECRETS`, `secret`, `Secrets`; `Listing`, `listing`, `changed`, `paths_named`; `repository_file`; three tests)
- Create: `docs/mcp.md`
- Modify: `docs/README.md` (one entry under "Contents")
- Modify: `README.md` (one line under "Documentation")

**Interfaces:**
- Consumes (Task 7): the test fixture in `crates/cli/tests/mcp.rs` — `World::{new, home, app, fake, fl_in, run, refused, read, write_codex}`, `repo_at`, `IGNORED`, `CLAUDE`, `CODEX`, `AGY`, `CATALOG`; the CLI surface and its output (`fl mcp --help`'s `Commands:` list); the phrases `never a value` and `does not trust`. (Task 3): `fake::{FakeRegistry, NOTES, WEATHER, TRACKER, DOCS, DOCS_URL}`, `FakeRegistry::{start, url}`. (Task 4): the refusal ``names a secret, and a secret is never recorded``; the fixtures' frozen shapes (an optional secret left out unless `--with`; `DOCS_AUTHORIZATION` derived). (Task 5): each vendor's secret shape (`${NAME}`, `Bearer ${NAME}`, `env_vars`, Antigravity leaving secrets out). (Task 6): the plan line `  replace <name> (<difference>)` with `` `env.NOTES_TOKEN` differs``; records as `<records>/<hex>.json` plus `sync.lock`. (Task 1): the catalog's serde refusals ``unknown field `…`, expected …`` / ``unknown variant `…`, expected …``.
- Produces: tests `no_secret_value_reaches_any_file_fl_writes_or_any_message`, `every_fl_mcp_command_reads_and_writes_only_in_its_home_and_its_project`, `the_docs_name_every_fl_mcp_command_and_every_catalog_field` (all in `crates/cli/tests/mcp.rs`); private helpers `const MARK: &str`, `const SECRETS: [(&str, &str); 8]`, `fn secret(name: &str) -> &'static str`, `struct Secrets<'w>` (`new`, `run -> (i32, String)`, `ok`, `refused -> String`, `stdout`, `texts`, `leaked`; fields `pasted: bool` and `malformed: bool`), `type Listing = BTreeMap<PathBuf, Option<(SystemTime, Vec<u8>)>>`, `fn listing(dir: &Path) -> Listing`, `fn changed(&Listing, &Listing) -> BTreeSet<PathBuf>`, `fn paths_named(text: &str) -> Vec<PathBuf>`, `fn repository_file(rel: &str) -> String`. `docs/mcp.md` with sections "The project", "The catalog", "Values and secret references", "Each machine's switches", "The agent CLIs", "What fl keeps", "The commands", "`fl mcp sync`", "`fl mcp check`", "The `.gitignore` lines", "What fl refuses, and why", "The registry", "Where fl keeps what it wrote" — the page the catalog's header and its parse error already point at (Task 1).
- Unique phrases (test messages): `a secret's value is in` · `outside its home and its project` · `in the home` / `in the project's directory` · `no record under $XDG_STATE_HOME/fl/mcp` · `docs/mcp.md does not name`.

- [ ] **Step 1: Write the failing tests**

In `crates/cli/tests/mcp.rs`, replace the imports

```rust
use fl_mcp::fake::{self, FakeRegistry};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Sys;
```

with

```rust
use fl_mcp::fake::{self, FakeRegistry};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Sys;
use std::time::SystemTime;
```

and at the end of the file, after `sync_with_no_state_directory_is_refused`, add:

```rust
/// What every value in [`SECRETS`] holds, so a value written whole, cut
/// short or quoted is still found.
const MARK: &str = "Qv7Wd3";

/// A real-looking value for every variable a secret reads in
/// `no_secret_value_reaches_any_file_fl_writes_or_any_message`: the
/// fixtures' `NOTES_TOKEN` (npm), `WEATHER_API_KEY` (PyPI), `TRACKER_TOKEN`
/// (a docker `-e`, optional, included with `--with`) and
/// `DOCS_AUTHORIZATION` (a remote's header, its variable named by fl), and
/// the ones added by hand there. None has the shape of a real provider's
/// token.
const SECRETS: [(&str, &str); 8] = [
    ("NOTES_TOKEN", "nt_Qv7Wd3x8kLm2Pa9Yc4KdTr5Uw1Zs6Bv0"),
    ("WEATHER_API_KEY", "wk_Qv7Wd3e1b0e7f29c4d8a6f"),
    ("TRACKER_TOKEN", "tt_Qv7Wd3Zp8mT4rKq2Ln5xHj"),
    (
        "DOCS_AUTHORIZATION",
        "Bearer dk.Qv7Wd3.c2lnbmF0dXJlLXRva2Vu",
    ),
    ("API_TOKEN", "at_Qv7Wd3h4nd9q2LxWm8Rt3e"),
    ("WEB_TOKEN", "wt_Qv7Wd3c51e8b0a4f7d29"),
    ("WEB_X_TEAM", "acme_Qv7Wd3_team"),
    ("GH_PAT", "pat_Qv7Wd3r3n4m3dK9s1Vb"),
];

fn secret(name: &str) -> &'static str {
    let found = SECRETS.iter().find(|(n, _)| *n == name);
    found.unwrap_or_else(|| panic!("no secret {name}")).1
}

/// `fl mcp` in `app` with every secret in [`SECRETS`] set in its
/// environment, and `git` behind a wrapper that logs the arguments fl
/// starts it with. After each command, every file fl writes, git's
/// arguments and what the command printed are searched for [`MARK`].
struct Secrets<'w> {
    w: &'w World,
    path: OsString,
    log: PathBuf,
    /// The command, and what it printed: stdout, then stderr.
    said: Vec<(String, String, String)>,
    /// A person put a value in `.mcp.json`, so it is not searched.
    pasted: bool,
    /// A person put a value in a catalog that does not parse, so the
    /// catalog is not searched.
    malformed: bool,
}

impl<'w> Secrets<'w> {
    fn new(w: &'w World) -> Secrets<'w> {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::var_os("PATH").expect("PATH is set");
        let real = std::env::split_paths(&path)
            .map(|d| d.join("git"))
            .find(|g| g.is_file())
            .expect("git is on PATH");
        let bin = w.home().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let log = w.home().join("git-argv.log");
        let wrapper = bin.join("git");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
                log.display(),
                real.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let dirs = std::iter::once(bin).chain(std::env::split_paths(&path));
        Secrets {
            w,
            path: std::env::join_paths(dirs).unwrap(),
            log,
            said: Vec::new(),
            pasted: false,
            malformed: false,
        }
    }

    /// `fl mcp <args>`: its exit code and stderr, after the search.
    fn run(&mut self, args: &[&str]) -> (i32, String) {
        let mut c = self.w.fl_in(&self.w.app());
        c.env("PATH", &self.path);
        for (name, value) in SECRETS {
            c.env(name, value);
        }
        let out = c.arg("mcp").args(args).output().unwrap();
        // Named by its subcommand only: an argument may hold a value.
        let what = format!("command {} (fl mcp {})", self.said.len(), args[0]);
        let said = String::from_utf8(out.stdout).unwrap();
        let err = String::from_utf8(out.stderr).unwrap();
        self.said.push((what.clone(), said, err.clone()));
        let leaked = self.leaked();
        assert!(
            leaked.is_empty(),
            "after {what}, a secret's value is in {leaked:?}"
        );
        (out.status.code().expect("an exit code"), err)
    }

    fn ok(&mut self, args: &[&str]) {
        let (code, err) = self.run(args);
        assert_eq!(code, 0, "fl mcp {}: {err}", args[0]);
    }

    /// `fl mcp <args>`, which must exit 2: its stderr.
    fn refused(&mut self, args: &[&str]) -> String {
        let (code, err) = self.run(args);
        assert_eq!(code, 2, "fl mcp {}: {err}", args[0]);
        err
    }

    /// What the last command printed on stdout.
    fn stdout(&self) -> &str {
        &self.said.last().expect("a command ran").1
    }

    /// Every file fl writes — the catalog, the vendor files, the records —
    /// git's arguments, and every message: their contents.
    fn texts(&self) -> Vec<(String, String)> {
        let mut paths: Vec<PathBuf> = [CATALOG, CLAUDE, CODEX, AGY]
            .iter()
            .filter(|rel| !(self.pasted && **rel == CLAUDE))
            .filter(|rel| !(self.malformed && **rel == CATALOG))
            .map(|rel| self.w.app().join(rel))
            .collect();
        if let Ok(records) = fs::read_dir(self.w.home().join("state/fl/mcp")) {
            paths.extend(records.map(|e| e.unwrap().path()));
        }
        paths.push(self.log.clone());
        let mut texts: Vec<(String, String)> = (paths.iter())
            .filter_map(|p| Some((p.display().to_string(), fs::read_to_string(p).ok()?)))
            .collect();
        for (what, out, err) in &self.said {
            texts.push((format!("{what}, stdout"), out.clone()));
            texts.push((format!("{what}, stderr"), err.clone()));
        }
        texts
    }

    /// Where a secret's value is.
    fn leaked(&self) -> Vec<String> {
        (self.texts().into_iter())
            .filter(|(_, text)| text.contains(MARK))
            .map(|(what, _)| what)
            .collect()
    }
}

#[test]
fn no_secret_value_reaches_any_file_fl_writes_or_any_message() {
    let w = World::new();
    // Codex's config, which fl reads for trust, holds a key of its own.
    w.write_codex(&format!(
        "model = \"o3\"\napi_key = \"{}\"\n",
        secret("API_TOKEN")
    ));
    let mut s = Secrets::new(&w);
    s.ok(&["registry", &w.fake().url()]);
    // Each registry route that carries a secret.
    s.ok(&["add", "notes", "--from", fake::NOTES, "--version", "1.1.0"]);
    s.ok(&["add", "weather", "--from", fake::WEATHER]);
    s.ok(&[
        "add",
        "tracker",
        "--from",
        fake::TRACKER,
        "--with",
        "TRACKER_TOKEN",
    ]);
    s.ok(&["add", "docs", "--from", fake::DOCS]);
    // By hand: a secret variable, and secret headers.
    s.ok(&[
        "add",
        "local",
        "--env",
        "API_TOKEN",
        "--",
        "node",
        "server.js",
    ]);
    s.ok(&[
        "add",
        "web",
        "--url",
        "https://web.example.com/mcp",
        "--header",
        "Authorization=WEB_TOKEN:Bearer",
        "--header",
        "X-Team",
    ]);
    // A secret read from a variable of another name, which only Claude
    // Code can pass on.
    let mut catalog = w.read(CATALOG);
    catalog.push_str(
        "\n[server.renamed]\nvendors = [\"claude\"]\ntransport = \"stdio\"\n\
         command = \"gh-mcp\"\nenv.GITHUB_TOKEN = { secret = true, env = \"GH_PAT\" }\n",
    );
    fs::write(w.app().join(CATALOG), catalog).unwrap();
    s.ok(&["sync"]);
    s.ok(&["check"]);
    s.ok(&["upgrade", "notes"]);
    s.ok(&["sync"]);

    // A secret given as a value is refused, and not repeated.
    let given = format!("NOTES_TOKEN={}", secret("NOTES_TOKEN"));
    let err = s.refused(&["add", "leak", "--from", fake::NOTES, "--env", &given]);
    assert!(
        err.contains("`--env NOTES_TOKEN=…` names a secret, and a secret is never recorded"),
        "{err}"
    );
    let given = format!("Authorization={}", secret("WEB_TOKEN"));
    let err = s.refused(&["add", "leak", "--url", fake::DOCS_URL, "--header", &given]);
    assert!(err.contains("never a value"), "{err}");

    // A person pastes the value into fl's entry: refused, naming the field
    // and not its value; `--replace` says the same and writes the reference
    // back.
    let pasted = w
        .read(CLAUDE)
        .replace("${NOTES_TOKEN}", secret("NOTES_TOKEN"));
    assert!(pasted.contains(MARK));
    fs::write(w.app().join(CLAUDE), pasted).unwrap();
    s.pasted = true;
    assert_eq!(s.run(&["check"]).0, 2);
    assert_eq!(s.run(&["sync"]).0, 2);
    s.ok(&["sync", "--replace", "notes"]);
    assert!(
        s.stdout()
            .contains("  replace notes (`env.NOTES_TOKEN` differs)"),
        "{}",
        s.stdout()
    );
    s.pasted = false;
    assert_eq!(s.leaked(), Vec::<String>::new());

    // A value pasted where the catalog's grammar has no place for it: the
    // parse error names the line and the column, and never the value, even
    // where the parser would quote it back.
    let catalog = w.read(CATALOG);
    let token = secret("API_TOKEN");
    s.malformed = true;
    for bad in [
        format!("x = {token}\n"),
        format!("[server.bad]\ntransport = \"stdio\"\ncommand = \"x\"\nenabled = \"{token}\"\n"),
        format!("[server.bad]\ntransport = \"{token}\"\nurl = \"https://x.example.com\"\n"),
        format!(
            "[server.bad]\ntransport = \"stdio\"\ncommand = \"x\"\n\
             env.T = {{ secret = \"{token}\" }}\n"
        ),
        format!("[server.bad]\ntransport = \"stdio\"\ncommand = \"x\"\n{token} = 1\n"),
    ] {
        fs::write(w.app().join(CATALOG), format!("{catalog}\n{bad}")).unwrap();
        let err = s.refused(&["check"]);
        assert!(err.contains("is not a valid MCP catalog: line "), "{err}");
    }
    // A catalog that parses, with a value where a variable's name goes:
    // the rule's refusal names the field, not the value.
    let bad = format!(
        "[server.bad]\ntransport = \"http\"\nurl = \"https://x.example.com\"\n\
         headers.Authorization = {{ secret = true, env = \"Bearer {token}\" }}\n"
    );
    fs::write(w.app().join(CATALOG), format!("{catalog}\n{bad}")).unwrap();
    let err = s.refused(&["check"]);
    assert!(
        err.contains("field `headers.Authorization`: the variable it reads is not a valid"),
        "{err}"
    );
    fs::write(w.app().join(CATALOG), &catalog).unwrap();
    s.malformed = false;
    // By hand, a value where `--env NAME` wants a variable's name.
    let err = s.refused(&["add", "leak", "--env", token, "--", "node"]);
    assert!(
        err.contains("names a secret's environment variable"),
        "{err}"
    );
    assert_eq!(s.leaked(), Vec::<String>::new());

    // What was searched is what matters: every reference is in place, a
    // record for each vendor file, git was started, and Codex's config
    // was read.
    let claude = w.read(CLAUDE);
    for reference in [
        "${NOTES_TOKEN}",
        "${WEATHER_API_KEY}",
        "${TRACKER_TOKEN}",
        "${DOCS_AUTHORIZATION}",
        "${API_TOKEN}",
        "Bearer ${WEB_TOKEN}",
        "${WEB_X_TEAM}",
        "${GH_PAT}",
    ] {
        assert!(claude.contains(reference), "{reference}: {claude}");
    }
    let texts = s.texts();
    let records = (texts.iter())
        .filter(|(what, _)| what.contains("/state/fl/mcp/") && what.ends_with(".json"))
        .count();
    assert_eq!(records, 3, "a record for each vendor file");
    let argv = fs::read_to_string(&s.log).unwrap();
    assert!(
        argv.contains("ls-files") && argv.contains("check-ignore"),
        "{argv}"
    );
    assert!(
        (s.said.iter()).any(|(_, _, err)| err.contains("does not trust")),
        "Codex's config was read"
    );
}

/// Each entry under a directory by its path relative to it: `None` for a
/// directory, else the file's modification time and bytes.
type Listing = BTreeMap<PathBuf, Option<(SystemTime, Vec<u8>)>>;

/// `dir`'s [`Listing`]. `.git` is left out: git, which fl starts, keeps it.
fn listing(dir: &Path) -> Listing {
    let mut out = Listing::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            let rel = path.strip_prefix(dir).unwrap().to_path_buf();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                todo.push(path);
                out.insert(rel, None);
            } else {
                let bytes = fs::read(&path).unwrap_or_default();
                out.insert(rel, Some((meta.modified().unwrap(), bytes)));
            }
        }
    }
    out
}

/// The entries that are new, changed or gone between two listings.
fn changed(before: &Listing, after: &Listing) -> BTreeSet<PathBuf> {
    (before.keys().chain(after.keys()))
        .filter(|p| before.get(*p) != after.get(*p))
        .cloned()
        .collect()
}

/// Every absolute path `text` names: a `/` that begins a word (after a
/// space, a quote, a backtick or a bracket), to the end of the word, less
/// a trailing `.`, `:` or `,`.
fn paths_named(text: &str) -> Vec<PathBuf> {
    let edge = |c: char| c.is_whitespace() || "`\"'()[]{}<>,;".contains(c);
    let mut out = Vec::new();
    let mut prev = ' ';
    for (i, c) in text.char_indices() {
        if c == '/' && edge(prev) {
            let word = text[i..].split(edge).next().unwrap_or_default();
            out.push(PathBuf::from(word.trim_end_matches(['.', ':', ','])));
        }
        prev = c;
    }
    out
}

#[test]
fn every_fl_mcp_command_reads_and_writes_only_in_its_home_and_its_project() {
    let home = tempfile::tempdir().unwrap();
    let place = tempfile::tempdir().unwrap();
    let (h, app) = (home.path(), place.path().join("widgets"));
    repo_at(&app, &[(".gitignore", IGNORED)]);
    for dir in ["config/fl", "cache", "run", "tmp"] {
        fs::create_dir_all(h.join(dir)).unwrap();
    }
    let config = h.join("config/fl/config.toml");
    let switches = format!("[[mcp]]\nroot = \"{}\"\n", app.display());
    fs::write(&config, &switches).unwrap();
    let fake = FakeRegistry::start();
    let roots = [
        h.to_path_buf(),
        h.canonicalize().unwrap(),
        place.path().to_path_buf(),
        place.path().canonicalize().unwrap(),
    ];
    // What fl may write: its records, and the project's catalog and vendor
    // files (each with the directories above it).
    let records = Path::new("state/fl/mcp");
    let project = [CATALOG, CLAUDE, CODEX, AGY].map(|rel| Path::new("widgets").join(rel));
    // The `.gitignore` lines a refusal prints, which are not paths.
    let anchors: Vec<PathBuf> = IGNORED.lines().map(PathBuf::from).collect();
    let path = std::env::var_os("PATH").expect("PATH is set");
    let (mut named, mut said) = (BTreeSet::new(), Vec::new());

    // `fl mcp <args>` with nothing of this process's environment but PATH:
    // every place fl knows of comes from these variables and the
    // working directory. Its exit code.
    let mut fl = |args: &[&str]| -> i32 {
        let before = (listing(h), listing(place.path()));
        let out = Command::cargo_bin("fl")
            .unwrap()
            .env_clear()
            .env("PATH", &path)
            .env("HOME", h)
            .env("XDG_CONFIG_HOME", h.join("config"))
            .env("XDG_STATE_HOME", h.join("state"))
            .env("XDG_DATA_HOME", h.join("data"))
            .env("XDG_CACHE_HOME", h.join("cache"))
            .env("XDG_RUNTIME_DIR", h.join("run"))
            .env("TMPDIR", h.join("tmp"))
            .current_dir(&app)
            .arg("mcp")
            .args(args)
            .output()
            .unwrap();
        for p in changed(&before.0, &listing(h)) {
            assert!(
                records.starts_with(&p) || p.starts_with(records),
                "fl mcp {args:?} wrote {} in the home",
                p.display()
            );
        }
        for p in changed(&before.1, &listing(place.path())) {
            assert!(
                project.iter().any(|f| f.starts_with(&p)),
                "fl mcp {args:?} wrote {} in the project's directory",
                p.display()
            );
        }
        for text in [out.stdout, out.stderr] {
            let text = String::from_utf8(text).unwrap();
            for p in paths_named(&text)
                .into_iter()
                .filter(|p| !anchors.contains(p))
            {
                assert!(
                    roots.iter().any(|r| p.starts_with(r)),
                    "fl mcp {args:?} named {}, outside its home and its project:\n{text}",
                    p.display()
                );
                named.insert(p);
            }
            said.push(text);
        }
        out.status.code().expect("an exit code")
    };

    assert_eq!(fl(&["registry", &fake.url()]), 0);
    assert_eq!(fl(&["search", "io.example"]), 0);
    assert_eq!(
        fl(&["add", "notes", "--from", fake::NOTES, "--version", "1.1.0"]),
        0
    );
    assert_eq!(
        fl(&[
            "add",
            "tracker",
            "--from",
            fake::TRACKER,
            "--with",
            "TRACKER_TOKEN"
        ]),
        0
    );
    assert_eq!(fl(&["add", "docs", "--from", fake::DOCS]), 0);
    assert_eq!(
        fl(&[
            "add",
            "local",
            "--env",
            "API_TOKEN",
            "--",
            "node",
            "server.js"
        ]),
        0
    );
    assert_eq!(
        fl(&[
            "add",
            "web",
            "--url",
            "https://web.example.com/mcp",
            "--header",
            "Authorization=WEB_TOKEN:Bearer"
        ]),
        0
    );
    assert_eq!(fl(&["disable", "local"]), 0);
    assert_eq!(fl(&["enable", "local"]), 0);
    assert_eq!(fl(&["upgrade", "notes"]), 0);
    // Codex's config does not exist yet: a warning names where it looked.
    assert_eq!(fl(&["sync"]), 0);
    assert_eq!(fl(&["check"]), 0);

    // The records are in the temporary `$XDG_STATE_HOME`, one for each
    // vendor file, and each names a file in the project.
    let kept: Vec<PathBuf> = fs::read_dir(h.join(records))
        .unwrap_or_else(|e| panic!("no record under $XDG_STATE_HOME/fl/mcp: {e}"))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    assert_eq!(kept.len(), 3, "{kept:?}");
    let canonical = app.canonicalize().unwrap().display().to_string();
    for record in &kept {
        let text = fs::read_to_string(record).unwrap();
        assert!(text.contains(&canonical), "{text}");
    }

    // Codex's config, read where `$HOME` puts it.
    fs::create_dir_all(h.join(".codex")).unwrap();
    fs::write(h.join(".codex/config.toml"), "model = \"o3\"\n").unwrap();
    assert_eq!(fl(&["check"]), 0);
    // A hand edit is refused, naming the file; `--replace` overwrites it.
    let claude = app.join(CLAUDE);
    let edited = fs::read_to_string(&claude)
        .unwrap()
        .replace("notes-mcp@1.2.0", "notes-mcp@9.9.9");
    fs::write(&claude, edited).unwrap();
    assert_eq!(fl(&["check"]), 2);
    assert_eq!(fl(&["sync", "--replace", "notes"]), 0);
    // A record fl cannot read is refused, naming it; without it, `sync`
    // adopts what matches.
    fs::write(&kept[0], "{").unwrap();
    assert_eq!(fl(&["check"]), 2);
    fs::remove_file(&kept[0]).unwrap();
    assert_eq!(fl(&["sync"]), 0);
    // A vendor file git would not ignore is refused, naming `.gitignore`.
    fs::write(app.join(".gitignore"), "").unwrap();
    assert_eq!(fl(&["check"]), 2);
    fs::write(app.join(".gitignore"), IGNORED).unwrap();
    // A config fl cannot read is refused, naming it.
    fs::write(&config, "[[mcp]]\nroots = \"x\"\n").unwrap();
    assert_eq!(fl(&["check"]), 2);
    fs::write(&config, &switches).unwrap();
    assert_eq!(fl(&["remove", "tracker"]), 0);
    assert_eq!(fl(&["sync"]), 0);

    // Each kind of file fl reads was named at least once, inside.
    let record = kept[0].strip_prefix(h).unwrap();
    for tail in [
        Path::new("widgets").join(CATALOG),
        Path::new("widgets").join(CLAUDE),
        Path::new("widgets/.gitignore").to_path_buf(),
        Path::new(".codex/config.toml").to_path_buf(),
        Path::new("config/fl/config.toml").to_path_buf(),
        record.to_path_buf(),
    ] {
        assert!(
            named.iter().any(|p| p.ends_with(&tail)),
            "{} was never named: {named:?}",
            tail.display()
        );
    }
    // This process's own home is never named, unless the temporary
    // directories lie inside it.
    let real = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(real) =
        real.filter(|r| r.parent().is_some() && !roots.iter().any(|t| t.starts_with(r)))
    {
        let real = real.display().to_string();
        for text in &said {
            assert!(!text.contains(&real), "{text}");
        }
    }
}

/// A file of the repository, by its path from the repository's root.
fn repository_file(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn the_docs_name_every_fl_mcp_command_and_every_catalog_field() {
    let w = World::new();
    let doc = repository_file("docs/mcp.md");
    // Every subcommand, as `fl mcp --help` lists them.
    let out = w.fl_in(&w.app()).args(["mcp", "--help"]).output().unwrap();
    let help = String::from_utf8(out.stdout).unwrap();
    let commands: Vec<&str> = (help.lines())
        .skip_while(|l| *l != "Commands:")
        .skip(1)
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_whitespace().next())
        .filter(|c| *c != "help")
        .collect();
    assert!(commands.len() >= 9, "{help}");
    for c in &commands {
        assert!(
            doc.contains(&format!("fl mcp {c}")),
            "docs/mcp.md does not name `fl mcp {c}`"
        );
    }
    // Every key and value of the catalog, as its own refusals list them.
    fs::create_dir_all(w.app().join(".fl")).unwrap();
    let mut words = BTreeSet::new();
    for probe in [
        "bogus = 1\n",
        "[server.x]\nbogus = 1\n",
        "[server.x]\ntransport = \"stdio\"\ncommand = \"a\"\nenv.A = { secret = true, bogus = 1 }\n",
        "[server.x]\ntransport = \"http\"\nurl = \"https://a.example\"\n\
         headers.A = { secret = true, env = \"B\", bogus = 1 }\n",
        "[server.x]\ntransport = \"bogus\"\n",
        "[server.x]\nvendors = [\"bogus\"]\ntransport = \"stdio\"\ncommand = \"a\"\n",
    ] {
        fs::write(w.app().join(CATALOG), probe).unwrap();
        let (_, err) = w.refused(&["check"]);
        let (_, listed) = err
            .split_once(", expected ")
            .unwrap_or_else(|| panic!("{err}"));
        let listed = listed.lines().next().unwrap_or_default();
        words.extend(listed.split('`').skip(1).step_by(2).map(str::to_string));
    }
    assert!(words.len() >= 20, "{words:?}");
    for word in &words {
        assert!(
            doc.contains(&format!("`{word}`")),
            "docs/mcp.md does not name `{word}`"
        );
    }
    // Both indexes link it.
    assert!(
        repository_file("README.md").contains("(docs/mcp.md)"),
        "README.md does not link docs/mcp.md"
    );
    assert!(
        repository_file("docs/README.md").contains("(mcp.md)"),
        "docs/README.md does not link mcp.md"
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fl-cli --test mcp`
Expected: FAIL — 26 passed, 1 failed: `the_docs_name_every_fl_mcp_command_and_every_catalog_field` panics with `…/docs/mcp.md: No such file or directory (os error 2)`. The two guard tests pass already — Tasks 1–7 built what they check; Step 5 shows each one fails when a guard is reverted.

- [ ] **Step 3: Implement**

Create `docs/mcp.md`:

````markdown
# A project's MCP servers

Each agent CLI keeps its own list of MCP servers, in its own file and format: `.mcp.json` for
Claude Code, `.codex/config.toml` for Codex, `.agents/mcp_config.json` for Antigravity. Kept by
hand, the copies drift. With `fl mcp`, a project has **one MCP catalog**, `.fl/mcp.toml`,
committed with the code, and fl writes every agent CLI's file from it. Turn a server on once and
every agent CLI in the project has it; turn it off once and all of them lose it.

The catalog holds the exact command each server runs, frozen when the server is added. Writing
the agent CLIs' files reads nothing else: no registry and no network, so every machine gets the
same files from the same commit. fl never reads, stores or writes a secret's value; the catalog
names the environment variable that holds it, and the agent CLI reads the variable when it
starts the server.

`fl mcp` opens no store and needs no project in fl's config. `--db` is refused with it, and
`$FL_DB` is ignored.

## The project

`fl mcp` works on the nearest directory, from the working directory up, that holds
`.fl/mcp.toml`; with none, on the nearest that holds `.git`, where the first `fl mcp registry` or
`fl mcp add` creates the catalog. A directory with neither above it is refused. Every vendor file
is written in that directory.

## The catalog

```toml
registry = "https://registry.modelcontextprotocol.io"

[server.notes]
from = "io.example/notes"
version = "1.2.0"
enabled = true
transport = "stdio"
command = "npx"
args = ["-y", "@example/notes-mcp@1.2.0", "./notes"]
env.NOTES_LOG = "info"
env.NOTES_TOKEN = { secret = true }

[server.docs]
enabled = true
vendors = ["claude", "codex"]
transport = "http"
url = "https://docs.example.com/mcp"
headers.Authorization = { secret = true, env = "DOCS_TOKEN", scheme = "Bearer" }
```

The top level has two keys:

| key | meaning |
|---|---|
| `registry` | The registry `fl mcp search`, `fl mcp add --from` and `fl mcp upgrade` read. Optional; there is no default. |
| `server` | One table per server, `[server.<name>]`. |

A server's name is 1 to 32 lowercase letters, digits and `-`, and it is the server's name in
every vendor file. `workspace`, `computer-use` and `claude-in-chrome` are refused: Claude Code
reserves them. A
server's table holds:

| key | meaning |
|---|---|
| `from` | The registry's name for the server, such as `io.example/notes`. Absent for a server added by hand. |
| `version` | The exact registry version the launch was frozen from. Present exactly when `from` is. |
| `enabled` | The team default: `true` or `false`. Default `true`. |
| `vendors` | The agent CLIs that get the server: any of `claude`, `codex` and `antigravity`. Default: all three. |
| `transport` | `stdio` (fl's vendor files start the server), `http` (streamable HTTP) or `sse` (the older HTTP with server-sent events). |
| `command` | The program a `stdio` server runs. Required for `stdio`, refused otherwise. |
| `args` | Its arguments, a list of strings. `stdio` only. |
| `env` | Its environment variables, `env.<NAME> = …`. `stdio` only. |
| `url` | The address of an `http` or `sse` server. Required for those, refused for `stdio`. |
| `headers` | The headers sent to an `http` or `sse` server, `headers.<Name> = …`. |

An unknown key is refused, not ignored, so a typo cannot pass for a setting. fl's edits keep
the catalog's comments and layout.

### Values and secret references

A value in `env` or `headers` is a literal string or a secret reference. A literal is committed
with the catalog, and is public if the repository is: `fl mcp add` warns about every literal it
records. A secret reference names the variable that holds the value, which fl never reads:

| reference | the value comes from |
|---|---|
| `env.API_TOKEN = { secret = true }` | the variable `API_TOKEN` |
| `env.API_TOKEN = { secret = true, env = "OTHER" }` | the variable `OTHER` |
| `headers.X-Api-Key = { secret = true, env = "DOCS_KEY" }` | `DOCS_KEY`, the whole header value |
| `headers.Authorization = { secret = true, env = "DOCS_TOKEN", scheme = "Bearer" }` | `DOCS_TOKEN`, sent as `Bearer <value>` |

In a reference, `secret` is always `true`: `secret = false` is refused, since a literal is
written as a string. `env` names the variable when it is not the key's own name; a secret header
needs `env`, and `scheme` is optional. Set each variable in the environment the agent CLI starts in; fl writes only
its name.

## Each machine's switches

A person turns a server on or off on their own machine in fl's config,
`$XDG_CONFIG_HOME/fl/config.toml` (default `~/.config/fl/config.toml`), never in the repository:

```toml
[[mcp]]
root = "/home/you/code/app"
disable = ["notes"]        # off on this machine, though the team default is on
enable = ["sentry"]        # on here, though the team default is off
```

`root` must be an absolute path. An `[[mcp]]` entry applies to the project at `root` and to every
project below it; the entry with the longest `root` wins, and it is independent of any
`[[project]]` entry. Two entries for one root are refused unless they say the same. A name in both
`enable` and `disable` is refused. A name the catalog does not have, such as a server removed
since, is ignored: `fl mcp sync` and `fl mcp check` print a warning naming it and carry on.
An fl older than this one refuses a config that has an `[[mcp]]` table; upgrade fl on that machine.

## The agent CLIs

| agent CLI | file, in the project | entries under |
|---|---|---|
| Claude Code | `.mcp.json` | `mcpServers` |
| Codex | `.codex/config.toml` | `[mcp_servers.<name>]` |
| Antigravity | `.agents/mcp_config.json` | `mcpServers` |

A server one agent CLI cannot run is left out of that file only, with a message naming the reason;
the others still get it. To say so in the catalog, give the server a `vendors` list without that
agent CLI.

**Claude Code** gets `command`, `args` and `env` for a `stdio` server, and `type`, `url` and
`headers` for an `http` or `sse` one. A secret is written as `"${NAME}"`, or `"Bearer ${NAME}"`
with a scheme, and Claude Code reads the variable when it starts the server. Claude Code passes an
unset `${NAME}` on as the literal text `${NAME}`, so a server whose secret is optional would get
that text instead of nothing: `fl mcp add` leaves an optional secret out unless named with
`--with`.

**Codex** gets `command`, `args`, `env` (literals) and `env_vars` (the names of the secret
variables it forwards) for a `stdio` server. It connects to streamable HTTP only, so an `sse`
server is left out. For an `http` server it gets `url`; a secret `Authorization` header with the
scheme `Bearer` becomes `bearer_token_env_var`, a secret header with no scheme becomes
`env_http_headers` (the variable holds the whole value), and a literal header `http_headers`. A
secret header with any other scheme, and a secret read from a variable of another name (`env =
"OTHER"`), are refused for Codex: it can add neither a prefix nor a new name. Codex reads a
project's `.codex/config.toml` only when its user trusts the project. `fl mcp sync` and `fl mcp
check` read `$CODEX_HOME/config.toml` (default `~/.codex/config.toml`), never write it, and warn
unless it trusts the project's root, the directory holding `.git`: a `[projects."<root>"]` table
with `trust_level = "trusted"`. In a linked worktree, the main checkout's root is looked up when
the worktree's own has no table. A parent directory's trust does not count: Codex matches the
exact path. The warning prints the two lines to add.

**Antigravity** gets `command`, `args` and `env` for a `stdio` server, and `serverUrl` and
`headers` for an `http` one. It expands no `${NAME}`, but a `stdio` server inherits Antigravity's
environment: a secret variable is left out of the entry, and the server reads it from that
environment under its own name, so a secret read from a variable of another name is refused for
Antigravity. A server with a secret header is left out of Antigravity's file, since the header
cannot reach the server. Antigravity has no SSE, so an `sse` server is left out too.

### What fl keeps

fl adds, changes and removes only the entries it wrote. Every other entry in a vendor file stays
as it is, byte for byte, in its place; the keys keep their order. Codex's file keeps its comments
and layout. In a JSON file, the layout between the entries is rewritten with two-space indentation;
nothing but whitespace changes outside fl's own entries. A JSON file that is not strict JSON — comments, trailing commas, a
byte-order mark — is refused, never rewritten, since Antigravity accepts such files and a rewrite
would lose what they hold. Remove them by hand, then run `fl mcp sync` again.

## The commands

```text
fl mcp registry <url>
fl mcp search <text>
fl mcp add <name> --from <registry-name> [--version V] [--package npm|pypi|oci | --remote] [--env NAME=value]… [--with NAME]…
fl mcp add <name> [--env NAME[=value]]… -- <command> [args…]
fl mcp add <name> --url <url> [--header NAME[=ENV[:SCHEME]]]…
fl mcp remove <name>
fl mcp enable <name>
fl mcp disable <name>
fl mcp upgrade <name> [--to V] [--env NAME=value]… [--with NAME]…
fl mcp sync [--replace <name>]…
fl mcp check
```

`fl mcp registry <url>` sets the catalog's `registry`. `fl mcp search <text>` lists the registry's
servers whose name holds the text, with each one's latest version and description, and its status
when it is not `active`, such as `(deprecated)`.

`fl mcp add <name> --from <registry-name>` reads the registry's latest version, or `--version V`,
and freezes its launch into the catalog under `<name>`, recording the version the registry
returns. A server the registry offers several ways to start needs `--package npm`, `--package
pypi`, `--package oci` or `--remote`, and the refusal lists them. A variable the registry marks
secret becomes a reference. A required variable that is not secret and has no default needs
`--env NAME=value`, which is committed. An optional secret variable or argument is left out, with a
note naming the `--with NAME` that includes it. `add` prints the variables to set. The launch it
freezes:

| route | command and arguments |
|---|---|
| npm | `npx -y <package>@<version>` and the package's arguments |
| PyPI | `uvx <package>==<version>` and the package's arguments |
| OCI | `docker run -i --rm`, the image's runtime arguments, `-e NAME` for each variable passed, the image as the registry names it, and the package's arguments |
| remote | its `url`, `transport` from the remote's type, and its headers |

By hand, `fl mcp add <name> -- <command> [args…]` records a `stdio` server: `--env NAME=value` is a
literal, and `--env NAME` alone a secret reference, so `NAME` must be a variable's name, as after
`--header NAME=` below. `fl mcp add <name> --url <url>` records an
`http` server, whose `--header` is always a secret reference: `--header NAME` reads the variable
`<SERVER>_<NAME>` (uppercased, `-` as `_`), `--header NAME=ENV` reads `ENV`, and `--header
NAME=ENV:SCHEME` sends `<SCHEME> <value>`. What follows `=` must be a variable's name — capital
letters, digits and `_` — so a token typed there is refused, and the refusal does not repeat it. A
literal header is added by editing the catalog.

Each form of `add` takes its own flags: `--version`, `--package`, `--remote` and `--with` go with
`--from` only, `--header` with `--url` only, and `--env` with `--from` or a command. Any other is
refused, naming it, rather than ignored.

`add` prints the launch it recorded: the transport and the command with its arguments, or the URL
and the names of its headers. It never prints a value.

`fl mcp remove`, `fl mcp enable` and `fl mcp disable` change the catalog: the server, or its team
default.

`fl mcp upgrade <name>` reads the registry's latest version, or `--to V`, freezes it the way the
server was added (the same route, literals and included secrets), prints the difference field by
field, and rewrites that one entry, keeping its `enabled` and `vendors`. A version that is not
newer is refused unless named with `--to`. A new version may need what the old one did not: give a
required value with `--env NAME=value` and include an optional secret with `--with NAME`, as `add`
takes them. A server added by hand is refused: edit the catalog.

None of these writes a vendor file. Commit the catalog, then run `fl mcp sync`.

### `fl mcp sync`

`sync` reads the catalog and this machine's switches, plans every vendor file, prints the plan,
and writes it: `add`, `update` or `remove` for each entry it changes, `adopt` for an entry already
as fl writes it, `forget` for one a person removed that the catalog no longer wants. An entry of
fl's that a formatter, or Antigravity's own panel, laid out anew is adopted when it means the same
(the same JSON value, or the same TOML table with its keys in any order). A vendor file that is gone
as a whole, after a fresh clone or `git clean -fdX`, is written anew; an entry gone from a file
that still exists was removed by hand, and is refused. A vendor file that is a symbolic link, or
lies under a linked `.codex` or `.agents`, is refused: fl writes only plain files it can see whole,
never through a link into a file git tracks or one outside the project. The project itself may be
reached through a link. It is all or nothing:
every file is planned before any is written, and one refusal writes nothing.
The files are written under a lock, each read again before any is written; a file that changed
since it was planned is refused, and nothing is written. Each file is written to a temporary file
beside it and renamed over it, keeping its mode.

### `fl mcp check`

`check` plans as `sync` does and writes nothing:

| exit | meaning |
|---|---|
| 0 | every vendor file matches the catalog |
| 1 | a `sync` would change something; the plan lists what |
| 2 | a refusal, or an error |

`sync` and `check` never contact a registry or the network, so `check` can serve as a gate: run it
before an agent session, and `sync` when it exits 1. On a fresh clone, where no vendor file exists
yet, it exits 1; an exit of 2 means the catalog, the switches or a vendor file needs a person.

## The `.gitignore` lines

The vendor files are generated on each machine from the catalog, so git must ignore them. The
project's `.gitignore` holds:

```text
/.mcp.json
/.codex/config.toml
/.agents/mcp_config.json
```

`sync` and `check` refuse a vendor file fl writes, or holds an entry in, that git tracks or would
not ignore, and print the lines to add; a tracked one also names `git rm --cached <file>`. fl does
not edit `.gitignore`. A vendor file fl has no entry in needs no line.

## What fl refuses, and why

Every refusal exits 2 and names the file, the entry and what to do next; an error prints
`error: …`.

* **The catalog.** A key fl does not know, a value of the wrong kind, a name it does not accept, a
  `stdio` server with no `command` or with a `url`, an `http` or `sse` server with no `url`, a
  `from` without a `version`. The catalog is the only source of truth, so fl reads none of it
  until all of it is right. A catalog that does not parse is refused with the line, the column and
  the parser's message, never the line itself, which could hold a pasted token; a key or value the
  message would quote is shown as `<value>`. A rule's refusal names the server and the field,
  never the value.
* **The registry's address.** Anything but `https://`, or `http://` to this machine
  (`127.0.0.1`, `localhost` or `[::1]`), and an address with a user name or password: a registry
  entry is code that will run on every teammate's machine.
* **A registry that misbehaves.** A redirect (fl follows none), an HTML page, a body larger than 4
  MiB, an answer that is not the registry API's shape, an error status: each is reported as the
  registry's failure. `sync` and `check` need no registry.
* **A registry entry fl cannot freeze honestly.** A deleted server, naming the publisher's
  message; several launch routes and no `--package` or `--remote`; a package type other than npm,
  PyPI and OCI, such as `mcpb`, `cargo` or `nuget`; a package that serves HTTP itself, which would
  need starting; an OCI image with neither a tag nor a digest, or tagged `latest`; a version that is
  not exact; runtime arguments with a positional argument; a secret inside an argument, except
  docker's `-e NAME={secret}`, which becomes `-e NAME` and a reference; a required argument or
  variable with no value; an argument of a type fl does not know. Each refusal names what to do,
  often `fl mcp add <name> -- <command>` by hand.
* **A secret given as a value.** `--env NAME=value` for a variable the registry marks secret, and a
  value after `--header NAME=`: a secret is never recorded. Set it in the environment instead. The
  refusal does not repeat the value.
* **An upgrade that is not newer**, unless named with `--to`.
* **A vendor file fl cannot edit safely.** A JSON file that is not strict JSON, whose top level or
  `mcpServers` is not an object, or with a key twice; a Codex file that is not valid TOML, or whose
  `mcp_servers` is not a table. Fix it by hand; the message never shows the file's text.
* **An entry someone else changed.** An entry fl wrote that was changed by hand since (not just laid
  out anew), or removed by hand from a file that still exists while the catalog still wants it,
  and an entry fl did not write that differs from what fl would write under the same name. fl
  never changes or removes an entry it did not write. The
  refusal names the fields that differ, never their values, since a hand-edited entry may hold a
  pasted token. Restore the entry, or run `fl mcp sync --replace <name>`, which shows the same
  difference and writes fl's entry over it.
* **A file that changed while `sync` planned**, by another program or a person. Run `sync` again.
* **A vendor file git would commit.** See the `.gitignore` lines above.
* **A vendor file reached through a link**: the file, or a directory on its way, is a symbolic
  link.
* **No catalog**, for `sync`, `check` and the edits other than `registry` and `add`: fl never reads
  a missing catalog as an empty one, which would remove every entry it wrote. **No registry** in the
  catalog, for `search`, `add --from` and `upgrade`.

A vendor that cannot run a server — an `sse` server for Codex or Antigravity, a secret header for
Antigravity, a secret under another name or a header scheme Codex cannot send — is not a refusal:
the server is left out of that one file, and the message names the reason.

## The registry

Any registry that serves the MCP registry API, version `v0.1`, will do; the official one is
`https://registry.modelcontextprotocol.io`. fl sends no credential: it reads only what anyone can.
`fl mcp search` uses the registry's own search, which matches a substring of the server's name,
not its description, and keeps only the servers whose name holds the text, whatever the registry
sends. It reads up to 20 pages of results, saying so when it stops there, and prints the
registry's text without its control characters.

`add --from` and `upgrade` record the version the registry returns, never the word `latest`. A
server the registry marks deprecated is added with a warning that gives its message; one marked
deleted is refused. Nothing the registry says changes what runs on anyone's machine until a
person commits the catalog.

## Where fl keeps what it wrote

For each vendor file, fl keeps an **ownership record** in `$XDG_STATE_HOME/fl/mcp/` (default
`~/.local/state/fl/mcp/`): a JSON file named by the SHA-256 of the vendor file's canonical path,
listing each entry fl wrote there, the project root it came from, and the SHA-256 of the entry as
written. That is how `sync` tells its own entries from a person's. The records belong to the
machine and are never in a repository. The same directory holds `sync.lock`, which a second `sync`
waits for.

A record fl cannot read is refused, naming it: delete it, and the next `sync` adopts every entry
that is exactly as fl would write it. An entry that differs is then refused as one fl did not
write, and `--replace` takes it back.
````

In `docs/README.md`, under "Contents", after the `routing.md` entry (its second line ends `areas, the routing map, handles, lists and their limits.`), add:

```markdown
* [mcp.md](mcp.md) — one committed catalog of a project's MCP servers, written into Claude
  Code's, Codex's and Antigravity's own files: the catalog, secrets, each machine's switches, the
  commands, what fl refuses and why.
```

In `README.md`, under "Documentation", after the `docs/github-tracker.md` line, add:

```markdown
* [docs/mcp.md](docs/mcp.md) — one committed catalog of a project's MCP servers, written into each agent CLI's own file: the catalog, secrets, switches, commands and refusals.
```

`docs/getting-started.md` is not changed: Task 7 added its `mcp` line.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-cli --test mcp`
Expected: PASS — 27 passed.

Run: `cargo test -p fl-cli --test getting_started`
Expected: PASS — 1 passed (the guide's 65 commands, unchanged).

- [ ] **Step 5: Mutation checks**

For each: make the change, run the named test, watch it fail, restore the file and confirm with `cmp` against a copy saved first. The secret test: `cargo test -p fl-cli --test mcp -- no_secret_value_reaches_any_file_fl_writes_or_any_message`; the home guard: `cargo test -p fl-cli --test mcp -- every_fl_mcp_command_reads_and_writes_only_in_its_home_and_its_project`; the docs: `cargo test -p fl-cli --test mcp -- the_docs_name_every_fl_mcp_command_and_every_catalog_field`.

A value reaches a file fl writes (secret test):
1. Claude Code's writer writes the variable's value: in `crates/mcp/src/vendor/claude.rs`, the env arm's `format!("${{{var}}}")` → `std::env::var(var).unwrap_or_else(|_| format!("${{{var}}}"))` → red, `after command 7 (fl mcp sync), a secret's value is in [".../app/.mcp.json"]`.
2. … a header's value: `} => format!("{scheme} ${{{env}}}"),` → `} => format!("{scheme} {}", std::env::var(env).unwrap_or_default()),` → red, `.mcp.json`.
3. Codex's writer: in `crates/mcp/src/vendor/codex.rs`, `forwarded.push(var.to_string());` → `forwarded.push(std::env::var(var).unwrap_or_default());` → red, `.codex/config.toml`.
4. Antigravity's writer: in `crates/mcp/src/vendor/antigravity.rs`, after `let var = value.secret_var(key).expect("a secret names a variable");` insert `literals.push((key.clone(), std::env::var(var).unwrap_or_default()));` → red, `.agents/mcp_config.json`.
5. The catalog: in `crates/cli/src/cmd/mcp.rs` (`add_by_hand`), `None => (given.as_str(), EnvValue::Secret { env: None }),` → `None => (given.as_str(), EnvValue::Literal(std::env::var(given).unwrap_or_default())),` → red, `after command 5 (fl mcp add)`, `.fl/mcp.toml`.

A value reaches a message:
6. The `--from` refusal repeats it: in `crates/mcp/src/freeze.rs`, `` format!("`--env {shown}=…` names a secret, and a secret is never recorded"), `` → `` format!("`--env {shown}={}` names a secret, and a secret is never recorded", self.opts.env[name]), `` → red, `command 11 (fl mcp add), stderr`.
7. The `--header` refusal repeats it: in `crates/cli/src/cmd/mcp.rs`, `` "`--header {name}=…`: after `=` `` → `` "`--header {name}={rest}`: after `=` `` → red, `command 12 (fl mcp add), stderr`.
8. A refusal shows a hand-edited value: in `crates/mcp/src/sync.rs` (`difference`), `` parts.push(format!("`{key}` differs")) `` → `` parts.push(format!("`{key}` differs ({a})")) `` → red, `command 13 (fl mcp check), stdout`.
9. The Codex warning shows the file it read: in `crates/cli/src/cmd/mcp.rs` (`codex_trust`), `Ok(_) => format!("{} does not trust {key}", file.display()),` → `Ok(table) => format!("{} does not trust {key} ({table})", file.display()),` → red, `command 7 (fl mcp sync), stderr`.

A value reaches git's arguments:
10. In `crates/cli/src/cmd/mcp.rs` (`ignored`), `if Git::is_tracked(root, rel).map_err(git)? {` → `if Git::is_tracked(root, &format!("{rel}{}", std::env::var("NOTES_TOKEN").unwrap_or_default())).map_err(git)? {` → red, `git-argv.log`.

The home guard:
11. The records outside the home (as a path compiled in would be): in `crates/cli/src/cmd/mcp.rs` (`plan`), after `.join("mcp");` insert `let records = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mcp-records");` → red, `no record under $XDG_STATE_HOME/fl/mcp`. Then `rm -rf crates/cli/mcp-records`.
12. `$XDG_STATE_HOME` ignored for `~/.local/state`: in `crates/cli/src/config.rs` (`fl_state_dir`), `std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),` → `None,` → red, `fl mcp ["sync"] wrote .local in the home`.
13. Codex's config read from a fixed place: in `crates/cli/src/cmd/mcp.rs` (`codex_trust`), `None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex/config.toml")),` → `None => Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".codex/config.toml")),` → red, `fl mcp ["sync"] named …/crates/cli/.codex/config.toml, outside its home and its project`.
14. A stray write in the project: after `ignored(root, &plan)?;` in `plan` insert `std::fs::write(root.join(".fl-mcp-plan"), "")?;` → red, `fl mcp ["sync"] wrote widgets/.fl-mcp-plan in the project's directory`.

The docs:
15. A command undocumented: in `docs/mcp.md`, every `fl mcp upgrade` → `fl mcp up-grade` → red, ``docs/mcp.md does not name `fl mcp upgrade` ``.
16. A value undocumented: `` `antigravity` `` → `antigravity` → red, ``docs/mcp.md does not name `antigravity` ``.
17. `README.md`'s link: `(docs/mcp.md)` → `(docs/MCP.md)` → red, `README.md does not link docs/mcp.md`.
18. `docs/README.md`'s link: `[mcp.md](mcp.md)` → `[mcp.md](./mcp.md)` → red, `docs/README.md does not link mcp.md`.

The parse error and `--env NAME` (secret test):
19. The catalog's parse error quotes a value: in `crates/mcp/src/catalog.rs`, `redact(message.trim_end())` → `message.trim_end()` → red (a `check`'s stderr holds the token).
20. `--env <token>` by hand is recorded: in `crates/cli/src/cmd/mcp.rs`, `None if !is_variable(given) => bail!(` → `None if false && !is_variable(given) => bail!(` → red (the token lands in the catalog as a key).
21. The search skips the catalog only while it is malformed: `.filter(|rel| !(self.malformed && **rel == CATALOG))` → `.filter(|rel| !(false && **rel == CATALOG))` → red (the token the test wrote is found there): the search would find a value in the catalog.
22. A rule's refusal repeats the value: in `crates/mcp/src/catalog.rs`, the secret header's refusal ``"the variable it reads is not a valid environment variable name"`` → ``format!("`{env}` is not a valid environment variable name")`` → red (a `check`'s stderr holds the token).

Not observable:
- The ownership records are searched after every command, but no code path in fl reads a variable's value, so no single-line change puts one in a record without first putting it in the rendered entry, which check 1 catches in the vendor file the same command writes.
- The check that this process's own `$HOME` is never named runs only when neither temporary directory lies in it, and then a path under it is already outside both: mutation 13 is caught by the named-path check first. It stays for a mention that is not at a word's start.
- `env_clear()` and the extra `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR` and `TMPDIR` are the test's setting, not a guard: fl reads none of them today, and they make any future use land inside the listed home.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1336 passed, 19 ignored (1333 and 19 before this task).

- [ ] **Step 7: Commit**

```bash
git add README.md crates/cli/tests/mcp.rs docs/README.md docs/mcp.md
git commit -m "test(cli): fl mcp keeps every secret value out and stays in its home; docs

A black-box test sets a real-looking value for every secret the
fixtures and hand-added servers read, runs registry, add --from by
each route that carries a secret, add by hand with a secret env and
secret headers, sync, check, upgrade, two refusals of a value given
where a secret goes, and a hand edit holding a pasted token, and
after each command searches the catalog, every vendor file, every
ownership record, the arguments fl started git with and everything
it printed. None holds a value.

A second runs every fl mcp command with nothing of the environment
but PATH, HOME and the XDG bases, the home and the project in two
temporary directories: fl writes only its records under
XDG_STATE_HOME/fl/mcp and the project's catalog and vendor files, and
every path it names lies in one of the two.

It also writes a token where the catalog's grammar has no place for it,
and where a header's variable name goes: fl mcp check refuses naming
the line and column, or the field, never the token.

docs/mcp.md describes the catalog and every field, secret references,
the machine switches, the three vendors and their limits, the
commands and the flags each form of add takes, check as a gate, the
gitignore lines, each refusal and why, the registry and the ownership
records; a test holds it to every
subcommand and catalog field the binary knows. README.md and
docs/README.md link it. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

### Task 9: The live registry test

The one place fl reads the real registry (MCP spec §7.3: "An ignored test that reads the real public registry"). Every other test in this plan uses the in-process fake; this one is `#[ignore]`d, so `cargo test` and CI never touch the network, and an owner runs it by hand to learn whether the registry, the models of Task 3 and the freeze rules of Task 4 still fit each other. It uses the crate's own client (Task 3) against `https://registry.modelcontextprotocol.io` and sends no credential. It reads two things the spec names: `search("github")` (the registry searches names only, so every result must hold the text, in either case), and the GitHub server at `latest` (`io.github.github/github-mcp-server`), which today offers an OCI package and a streamable-http remote whose `Authorization` header is a secret with no value. The version test checks what the registry returned (a concrete version, never the string `latest`; status `active` or `deprecated`; a package or a remote), then freezes the entry (Task 4) by `Route::Remote` and checks that the version recorded is the one returned and that the header is a reference to an environment variable with no literal value anywhere, and freezes it again with no route chosen and checks that the refusal lists the OCI package and the remote (spec §3.2 step 3). Nothing pins an exact version or count, so the registry's content may change under it; a failure means the shape or the server's routes changed.

**Blast radius:** none for existing code. One new file, `crates/mcp/tests/live.rs`, an integration test of `fl-mcp`; `Cargo.toml` and `Cargo.lock` do not change (`fl-mcp` already depends on `ureq`, and the test needs nothing else). The workspace's test count gains 2 ignored tests. No file is written by the test; no environment variable is read or set by it.

**Files:**
- Create: `crates/mcp/tests/live.rs`

**Interfaces:**
- Consumes (Task 1): `catalog::{HeaderValue, Transport}`, `Server::{from, version, transport, url, headers, literal_values}`. (Task 3): `registry::{Registry, Status}`, `Registry::{new, search, version}`, `Search::servers` (each a `Summary`, read by its `name`), `ServerResponse::{server, meta}`, `ServerJson::{name, version, packages, remotes}`, `Official::status`, `McpError`. (Task 4): `freeze::{freeze, FreezeOptions, Route}`, `Frozen::{server, secrets}`, `McpError::Unfreezable`.
- Produces: no public item. Two ignored integration tests, `search_finds_github_servers_by_name` and `github_server_latest_freezes_by_its_remote_route`.
- Unique phrases: the ignore reason `live: reads the public MCP registry at registry.modelcontextprotocol.io`; the run command `cargo test -p fl-mcp --test live -- --ignored --nocapture`.

- [ ] **Step 1: Write the tests**

Create `crates/mcp/tests/live.rs`:

```rust
//! The one test that reads the real public MCP registry (MCP spec §7.3).
//!
//! Every other test of this crate talks to the in-process fake registry; these
//! two are `#[ignore]`d so `cargo test` never touches the network. They check
//! that the client, the models and the freeze rules still fit what the
//! registry really sends:
//!
//! * `search("github")` finds a server, and every server it returns has the
//!   text in its name (the registry searches names only);
//! * the GitHub server's `latest` answers with a concrete version, a live
//!   status and a launch route, and freezing it by its remote route records
//!   that version and the `Authorization` header as a reference to an
//!   environment variable. Freezing it with no route chosen is refused, and
//!   the refusal lists the routes.
//!
//! Run them by hand:
//!
//! ```text
//! cargo test -p fl-mcp --test live -- --ignored --nocapture
//! ```
//!
//! No credential is sent and no file is written. The assertions name no exact
//! version and no count, so the registry's content may change under them; a
//! failure means the registry's shape or the GitHub server's routes changed.

use fl_mcp::McpError;
use fl_mcp::catalog::{HeaderValue, Transport};
use fl_mcp::freeze::{FreezeOptions, Route, freeze};
use fl_mcp::registry::{Registry, Status};

const REGISTRY: &str = "https://registry.modelcontextprotocol.io";
const GITHUB: &str = "io.github.github/github-mcp-server";

fn registry() -> Registry {
    Registry::new(REGISTRY).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
#[ignore = "live: reads the public MCP registry at registry.modelcontextprotocol.io"]
fn search_finds_github_servers_by_name() {
    let found = registry()
        .search("github")
        .unwrap_or_else(|e| panic!("{e}"));
    let names: Vec<&str> = found.servers.iter().map(|s| s.name.as_str()).collect();
    println!(
        "search(\"github\"): {} servers, e.g. {:?}",
        names.len(),
        &names[..names.len().min(3)]
    );
    assert!(
        !names.is_empty(),
        "search(\"github\") found no server: the search or its parsing broke"
    );
    for name in &names {
        assert!(
            name.to_lowercase().contains("github"),
            "`{name}` has no `github` in its name: the registry searches names only"
        );
    }
}

#[test]
#[ignore = "live: reads the public MCP registry at registry.modelcontextprotocol.io"]
fn github_server_latest_freezes_by_its_remote_route() {
    let resp = registry()
        .version(GITHUB, "latest")
        .unwrap_or_else(|e| panic!("{e}"));
    let version = resp.server.version.clone();
    println!(
        "{GITHUB}: version {version}, {} package(s), {} remote(s)",
        resp.server.packages.len(),
        resp.server.remotes.len()
    );

    // The registry resolves `latest`; a concrete version is what gets pinned.
    assert_eq!(resp.server.name, GITHUB);
    assert!(
        !version.is_empty() && version != "latest",
        "version is `{version}`, not a concrete version"
    );
    assert!(
        matches!(resp.meta.status, Status::Active | Status::Deprecated),
        "the GitHub server's latest version is {:?}",
        resp.meta.status
    );
    assert!(
        !resp.server.packages.is_empty() || !resp.server.remotes.is_empty(),
        "the GitHub server offers no package and no remote"
    );

    // By its remote route: the version the registry returned is the one
    // recorded, and the secret header is a reference, never a value.
    let mut opts = FreezeOptions {
        name: "github".to_string(),
        route: Some(Route::Remote),
        ..FreezeOptions::default()
    };
    let frozen = freeze(&resp, &opts).unwrap_or_else(|e| panic!("{e}"));
    let server = &frozen.server;
    assert_eq!(server.from.as_deref(), Some(GITHUB));
    assert_eq!(server.version.as_deref(), Some(version.as_str()));
    assert!(matches!(server.transport, Transport::Http | Transport::Sse));
    assert!(
        server
            .url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"))
    );
    let headers = server.headers.as_ref().expect("the remote's headers");
    let auth = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v)
        .expect("an Authorization header");
    let HeaderValue::Secret { env, .. } = auth else {
        panic!("the Authorization header is not a secret reference: {auth:?}");
    };
    assert!(!env.is_empty(), "the secret names no environment variable");
    assert!(
        frozen.secrets.contains(env),
        "{env} is not among the variables a person sets: {:?}",
        frozen.secrets
    );
    assert!(
        server.literal_values().is_empty(),
        "a literal value was frozen: {:?}",
        server.literal_values()
    );

    // With no route chosen, an entry that offers several is refused, and the
    // refusal lists them.
    opts.route = None;
    let err = freeze(&resp, &opts).expect_err("two routes and none chosen");
    let McpError::Unfreezable { problem, next, .. } = &err else {
        panic!("not the freeze refusal: {err}");
    };
    println!("refused as expected: {err}");
    assert!(
        problem.contains("more than one launch route"),
        "the refusal does not say there are several routes: {problem}"
    );
    assert!(
        problem.contains("--package oci"),
        "no OCI route listed: {problem}"
    );
    assert!(
        problem.contains("--remote"),
        "no remote route listed: {problem}"
    );
    assert!(
        next.contains("--package") && next.contains("--remote"),
        "{next}"
    );
}
```

- [ ] **Step 2: Run them**

A live test has no offline red run: the code it exercises already exists (Tasks 3 and 4), and the registry cannot be made to fail on demand, so there is no failing state to capture before an implementation. What can be shown offline is that it compiles, that it is ignored by default, and that its assertions are not vacuous.

Run: `cargo test -p fl-mcp --test live --no-run`
Expected: compiles, and prints `Executable tests/live.rs (target/debug/deps/live-<hash>)`.

Run: `cargo test -p fl-mcp --test live`
Expected: PASS with both tests ignored: `test github_server_latest_freezes_by_its_remote_route ... ignored, live: reads the public MCP registry at registry.modelcontextprotocol.io`, the same for `search_finds_github_servers_by_name`, and `test result: ok. 0 passed; 0 failed; 2 ignored`.

The owner runs the live check by hand, with network access:

```bash
cargo test -p fl-mcp --test live -- --ignored --nocapture
```

Expected: `test result: ok. 2 passed`, with `--nocapture` printing the search size, the GitHub server's version and its package and remote counts, and the refusal the second freeze met. (The drafter did not run this against the network. It did run the same two tests, unchanged but for the ignore attribute and the address, against a throwaway local server that returned the registry's captured response for `latest` (the facts file's §1.10) and a one-server search result: both passed, and the printed refusal read: ``cannot be frozen into the catalog: it offers more than one launch route: `--package oci` (ghcr.io/github/github-mcp-server:2.0.1), `--remote` (streamable-http, https://api.githubcopilot.com/mcp/). Choose one with `--package <type>` or `--remote` ``. That stand-in was deleted and is not part of the change. Run against the registry on 2026-10-08, the search first failed whole on a server whose argument type is `flag`, which led to plan ruling 31; with Task 3's lenient search both tests passed, the search reading 2000 servers.)

- [ ] **Step 3: Implement**

Nothing to implement: the tests exercise the registry client and the freeze rules of Tasks 3 and 4 as they stand. `crates/mcp/Cargo.toml` needs no change.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-mcp --test live`
Expected: PASS — 0 passed, 2 ignored (as in Step 2).

- [ ] **Step 5: Mutation checks**

Not applicable. A mutation check reverts a guard in the implementation and watches a test go red; this task adds no implementation, and its tests cannot run offline, so a reverted guard would show nothing here (the guards they exercise are mutation-checked in Tasks 3 and 4, against the fake). What each assertion would catch, when the owner runs it:

1. `!names.is_empty()` — the search returns nothing for `github` (a broken query string, the `version=latest` or `limit` parameter rejected, or a parser that drops every server).
2. Every name holds `github`, compared in lower case — the registry began to search descriptions or tags (spec §3.1 says names only), or the client returned servers from a wrong page.
3. `resp.server.name == GITHUB` — the client asked for a different server than it was given (a name not escaped, so the `/` split the path).
4. `version` is not empty and not `latest` — the client returned the word it sent, not the version the registry resolved (spec §3.2: record the concrete version).
5. Status is `Active` or `Deprecated` — the registry's latest version of the server is `deleted`, or the status field moved.
6. A package or a remote is offered — the entry's `packages` and `remotes` were not parsed.
7. `freeze` by `Route::Remote` succeeds — the Remote route is refused for a real entry: the `Authorization` header with no value, the streamable-http type, or the URL.
8. `from` is the name and `version` is the version the registry returned — the entry was frozen with a stale or defaulted version.
9. The transport is http or sse and the URL starts with `https://` — the remote was recorded as stdio or without its address.
10. The `Authorization` header is `HeaderValue::Secret` with a non-empty `env` that is among `frozen.secrets`, and `literal_values()` is empty — a secret header was recorded as a literal, or a value reached the entry. This is the check that no secret value is ever written.
11. With no route chosen, the freeze is `Unfreezable` whose problem says `more than one launch route` and lists `--package oci` and `--remote`, and whose remedy names both flags — the freeze picked one route silently, or the refusal stopped listing them (spec §3.2 step 3). If the GitHub server one day drops its package or its remote, this fails by design: the example the spec uses has changed.

- [ ] **Step 6: Run the trio**

```bash
cargo fmt --all && cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace
```

Expected: all green — 1336 passed, 21 ignored (1336 and 19 before this task; the two new tests are the ignored ones).

- [ ] **Step 7: Commit**

```bash
git add crates/mcp/tests/live.rs
git commit -m "test(mcp): the live registry test, ignored: search and freeze the GitHub server

Two ignored tests read the real public registry through fl's own client:
search for github finds servers and every name holds the text (the
registry searches names only); the GitHub server at latest answers with
a concrete version (never the word latest), an active or deprecated
status and a launch route, freezes by its remote route recording that
version with the Authorization header as a reference to an environment
variable and no value, and is refused with no route chosen, the refusal
listing the OCI package and the remote.

No credential is sent and no file is written. Nothing pins an exact
version or count. Run by hand: cargo test -p fl-mcp --test live --
--ignored --nocapture. A plain cargo test lists both as ignored.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
git status --porcelain
```

---

## After the last task

1. The whole-branch review (subagent-driven development's final review) on the most capable model, against this plan and the spec's rev 2.
2. The live registry test (Task 9) is run once by hand: `cargo test -p fl-mcp --test live -- --ignored --nocapture`. It reads the public registry only and sends no credential. Its result goes in the PR.
3. A PR against `main`; the owner merges.
