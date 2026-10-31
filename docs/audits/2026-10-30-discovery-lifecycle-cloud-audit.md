# Discovery and lifecycle cloud audit — 2026-10-30

Scope: `crates/hub/src/discovery.rs` (vendor transcript parsers, model and thinking discovery,
quota and route resolution) and `crates/hub/src/lifecycle.rs` (spin-up, two-phase decommission,
hosted-session restart). The audit ran against `origin/main` `5799421`; the fixes were landed
on `origin/main` `6e9ff06`, and this report cites that tree. The artifact pre-archiving step lives
in `crates/hub/src/api.rs` (`discover_session_artifact_candidates`, `archive_session_artifacts`);
the audit read it only as far as it calls into the two files above.

Line numbers are for the commit that adds this report, on top of `6e9ff06` (they drift with later
edits; the symbol names are the stable anchor).

## How to read this report

Each finding has a **verification** status:

* **Reproduced** — the cloud agent recorded that the regression test fails when it removes the fix
  and passes with it. Both runs were on the cloud branch; the landing review did not repeat them.
* **Test only** — the regression test passes with the fix; it was not run against the unfixed
  code, or it cannot fail on the old code (for example it tests a new helper).
* **Read** — confirmed by reading the code. No fix, or no test, because the change is a design
  decision or cannot run under `cfg(test)`.

Severity is my judgement for a single-user, self-hosted hub where the transcripts and route table
are written by local agent CLIs. Nothing here is remotely exploitable without a request or a file
that an agent already controls.

## Summary

| # | Sev | File | Finding | Status |
|---|---|---|---|---|
| F1 | **High** | discovery.rs | UTC-offset parser panics on a multibyte character | Fixed, reproduced |
| F2 | Medium | discovery.rs | Session id and route ids are joined into paths unchecked | Fixed, reproduced |
| F3 | Medium | discovery.rs | Token sums overflow `u64` | Fixed, reproduced |
| F4 | Medium | discovery.rs | Transcript read is all-or-nothing: invalid UTF-8, no size cap, FIFO hang | Fixed, reproduced (FIFO, cap); test only (UTF-8); known limits |
| F5 | Medium | discovery.rs | A quoted `<SYSTEM_MESSAGE>` tag deletes the operator's own text | Fixed, reproduced |
| F6 | Medium | lifecycle.rs | A re-spin-up inherits runtime ownership from a decommissioned record | Fixed, reproduced |
| F7 | Medium | lifecycle.rs | `tmux` calls have no timeout while the store guard is held | Fixed (bounded, not removed), test only |
| F8 | Low | discovery.rs | Grok quota poll reads the whole `unified.jsonl` under the global probe lock | Fixed, with a trade-off |
| F9 | Low | discovery.rs | No cap on a single JSONL line | Fixed, reproduced |
| F10 | Low | lifecycle.rs | A bare persona resolves to a decommissioned sibling before a live one | Fixed, reproduced |
| O1 | Low | lifecycle.rs | Bare persona with two live vendors picks the first by `session_id` | Open, pinned by a test |
| O2 | Low | lifecycle.rs | `kill_tmux_runtime` reports success when `tmux` is missing | Open, read |
| O3 | Low | discovery.rs | A wire header naming the owner is shown as the user | Open, read |
| O4 | Low | discovery.rs | Adapter detection matches `.codex` / `.claude` anywhere in the path | Open, read |
| O5 | Low | discovery.rs | Transcript reads follow symlinks; metadata/open race | Open, read |
| O6 | Low | lifecycle.rs | `check_text` lets Unicode format characters through | Open, read |
| O7 | Info | discovery.rs | `<agent-message ...>` open tag ends at the first `>` | Open, read |

## Fixed findings

### F1 — UTC-offset parser panics on a multibyte character (High)

`parse_utc_offset_minutes` (discovery.rs:1339) splits a four-byte offset with
`rest.split_at(2)` (:1352). `split_at` panics when byte 2 is not a char boundary. `+1é1` is
`1`, `é` (two bytes), `1`: four bytes, boundary at 1 and 3, so the split at 2 panics.

The input is untrusted text. `parse_iso8601_unix_millis` reaches it from Grok's
`config.currentPeriod.end` (`grok_quota_from_home`, :1471–:1477) and from the `resets at <token>` hint in a Muse failure reason
(`parse_reset_instant_from_reason`, :1366). A transcript or log line that carries that token
panics the thread that polls quota, and quota polls run inside discovery on every session list.

Fix: `if !rest.is_ascii() { return None; }` before the split.
Tests: `utc_offset_with_multibyte_text_is_rejected_not_a_panic` (four inputs, plus the Muse path).

### F2 — Session and route ids are joined into paths unchecked (Medium)

`find_transcript_file_in_home` (:1145) joins `session_id` into four vendor layouts, among them
`~/.grok/sessions/{id}.jsonl` (:1178) and `brain/{id}/.system_generated/logs/transcript.jsonl`.
The id comes from the API (`/sessions/:id/transcript`, context, quota), from the route table
(`VendorRoute::{Claude,Codex,Muse,Grok}` ids) and from attempt rows. `../../outside/secret` read
`~/outside/secret.jsonl`; an absolute id replaced the base outright. The Antigravity route branch
in `discover_sessions_impl` (:314–:320) joined `conversation_id` the same way. The parsed turns are
returned to the API caller, so this was an arbitrary `*.jsonl` read, and fixed-name reads
(`session.jsonl`, `transcript.jsonl`, `chat_history.jsonl`) under any directory.

`discover_session_artifact_candidates` (api.rs:2677; the check is at :2685–:2689) already refuses
ids with `/`, `\`, `..`, NUL for the brain-dir scan; discovery did not.

Fix: `is_safe_path_component` (:923) — non-empty, at most 255 bytes, not `.` or `..`, no `/`, `\`
or NUL — checked at the top of `find_transcript_file_in_home` and on the Antigravity route join.
Colons and dots inside a name (`claude:ada`, UUIDs) still pass.
Tests: `session_ids_that_climb_out_of_the_vendor_dirs_find_no_transcript`,
`an_antigravity_route_cannot_name_a_transcript_outside_the_brain_dir`.

### F3 — Token sums overflow `u64` (Medium)

Context-usage reads add JSON-controlled `u64` values with `+`: Claude's four usage fields, Muse's
`input + output` (:1879), and Antigravity's three (`json_u64_token` already saturates a float to
`u64::MAX`, so one field at `1e30` is enough). Debug builds panic; release builds wrap and show a
tiny context size for a full one.

Fix: `saturating_add` at all three sites (Muse :1879–:1880, Claude :1926–:1928, and the Antigravity
sum in `antigravity_context_from_transcript`, :1582–:1583).
Test: `token_sums_saturate_instead_of_overflowing` (all three adapters).

### F4 — Transcript read is all-or-nothing (Medium)

Every transcript, context and quota read used `fs::read_to_string`:

* One invalid UTF-8 byte made the read fail, so the **whole** transcript vanished from the UI
  (the code then fell through to the next lookup and, usually, an empty list).
* No size cap: a multi-gigabyte log was loaded into memory per request, and the quota path does it
  while holding the global probe lock.
* `fs::read_to_string` on a FIFO blocks until a writer opens it. A transcript path that is a FIFO
  hangs the request thread for good.

Fix: `read_transcript_lossy` (:930). It requires a regular file (checked with `stat`, which does
not block), reads lossy UTF-8, and reads at most 64 MiB from the **tail**, dropping the cut-off
first line. `read_transcript` is the 64 MiB wrapper used at every call site (:1650–:1682,
:1807–:1940, :1980, :1994).
Tests: `invalid_utf8_in_one_line_does_not_hide_the_transcript` (test only),
`a_transcript_over_the_cap_is_read_from_its_tail_without_the_cut_line`,
`a_fifo_transcript_is_refused_instead_of_blocking_the_reader` (reproduced: 10 s timeout without
the `is_file` check).

Known limits of the tail read:

* A tail cut that lands exactly on a line start still drops that one whole line.
* Everything before the cut is lost to every reader of that transcript, not only the cut-off
  first line. That includes any head-only line, such as a session header.
* `parse_jsonl_transcript` numbers turns from the text it is given (`index` is `turns.len()` when
  the turn is pushed). For a transcript over 64 MiB the text starts at the cut, so the `index`
  values are relative to the cut and shift as the file grows and the cut moves. Whether the UI
  keys on `index` across polls was not checked.

### F5 — A quoted `<SYSTEM_MESSAGE>` tag deletes the operator's text (Medium)

`normalize_system_turn` treated any turn containing `\n<SYSTEM_MESSAGE>` as a harness block
(`normalize_system_turn`, :2685; the block test is :2696–:2698). For a real operator message such as `Please explain this:\n<SYSTEM_MESSAGE>...`, the
branch that finds `\n<SYSTEM_MESSAGE>` keeps only the text **after** the tag, so the operator's
own words were dropped from the transcript, and the turn was relabelled.

Fix: the mid-text form counts only for a `system` turn or after the harness preamble
(`The following is a <SYSTEM_MESSAGE>`, `SYSTEM_MESSAGE_PREAMBLE`). A turn that starts with the tag
is unchanged.
Test: `an_operator_message_that_quotes_a_system_message_tag_keeps_its_text`.

### F6 — A re-spin-up inherits runtime ownership from a decommissioned record (Medium)

`spin_up` carried `runtime_spawned` over from the previous record (`spin_up`, lifecycle.rs:663–:665) whether or
not that record was live. Teardown leaves a decommissioned record with `runtime_spawned: true`
after it killed the tmux session. If the operator then starts their own tmux session called
`Ada`, a re-spin-up finds the name taken (`spawn_tmux_runtime` returns false) but keeps the
inherited `true`, and the next teardown runs `kill-session -t =Ada` on the operator's session.

Fix: carry the flag over only from a live record; teardown clears the flag after a kill that
succeeded (`teardown_session`, :1075–:1077).
Tests: `respin_of_a_decommissioned_record_does_not_inherit_runtime_ownership` (reproduced),
`respin_of_a_live_record_keeps_runtime_ownership`. The teardown half runs only with a real tmux
(`spawn_runtime` is off under `cfg(test)`), so it has no unit test.

### F7 — `tmux` calls have no timeout while the store guard is held (Medium)

`tmux_ok` used `Command::status()`, which waits forever. `spin_up`, `restart_hosted_session` and
`teardown_session` are called from the handlers `handle_post_session_spin_up` (api.rs:4605),
`handle_post_session_decommission` (:4636) and `handle_post_hosted_session_restart` (:4738), each
of which holds the project store guard (`HubStoreGuard`, which owns the `store_lock` mutex guard).
A wedged tmux server (stuck socket, stopped process) therefore blocked every request that needs
the store.

Fix: `command_succeeds_within` (:502) spawns the child, polls `try_wait`, and kills and reaps it
at a 10 s deadline (`TMUX_TIMEOUT`, :492). `tmux_ok` (:494) calls it. tmux is still invoked with
separate argv elements and no shell.
Test: `command_succeeds_within_reports_exit_status_and_kills_a_wedged_command`. It tests the new
helper only. The helper is new code, so the test cannot fail on the old code, and it does not
prove that `tmux_ok` goes through the helper (verification: test only, not reproduced).

Limit: the store guard is bounded, not freed. Each tmux call can still hold it for up to 10 s, and
one operation makes several calls: teardown runs `kill-session` and then `has-session` (up to
20 s); a spin-up runs `new-session` and, if persisting the record then fails, `kill-session` and
`has-session` (up to 30 s). During that time every request that needs the store waits.

### F8 — Grok quota poll reads the whole log under the probe lock (Low)

`grok_quota_from_home` (:1443) read all of `~/.grok/logs/unified.jsonl` for every Grok session on
every discovery, inside `with_probe_lock` (:1603, :1634), to find the **latest** billing line.
Fix (status: fixed, with the limitation below): read only the newest 4 MiB
(`QUOTA_LOG_TAIL_BYTES`, :919) through `read_transcript_lossy`.
Test: `grok_quota_reads_only_the_tail_of_a_huge_log`.

Trade-off: if the latest `billing: fetched credits config` line is older than the newest 4 MiB of
the log, quota goes from shown to `None`, where the old whole-file read still showed it. The test
pins this: with the only billing line outside the window, `grok_quota_from_home` returns `None`.
The limit buys a bounded read under the probe lock; a long, chatty log with a rare billing line
shows no Grok quota.

### F9 — No cap on a single JSONL line (Low)

`parse_jsonl_transcript` parsed any line. A line over 8 MiB (`TRANSCRIPT_LINE_MAX_BYTES`, :917; checked at :2072) is now
skipped. Turn `index` values stay dense.
Test: `an_oversized_line_is_skipped_and_its_neighbours_survive`.

### F10 — A bare persona resolves to a decommissioned sibling first (Low)

`find_hosted_session`'s persona fallback took the first match by `session_id`. With a
decommissioned `claude:ada` and a live `codex:ada`, a bare `ada` reached the dead record: restart
returned 409 and teardown was a no-op while the live session stayed up. The fallback now prefers
a live record (`find_hosted_session`, lifecycle.rs:355; persona fallback :363–:375).
Test: `bare_persona_prefers_a_live_record_over_a_decommissioned_sibling`.

## Open findings

* **O1 (Low).** With two **live** records for one persona on different vendors, a bare persona
  (or a discovered id whose persona is bare) picks the first by `session_id`, so `ada` tears down
  `claude:ada`, not `codex:ada`. Returning a 409 would break a discovered-id flow that today
  works, so I left the behaviour and pinned it with
  `bare_persona_with_two_live_records_resolves_to_the_first_by_session_id`. Decide whether the
  API should refuse an ambiguous bare id.
* **O2 (Low).** `kill_tmux_runtime` (lifecycle.rs:535–:537) returns `kill || !has-session`. When the
  `tmux` binary is missing both fail and the second term turns that into "stopped", so teardown
  reports `runtime_stopped: true`. Needs a `Result`-returning tmux helper; not unit-testable
  without editing `PATH`.
* **O3 (Low).** A wire/NATS header whose sender is `owner` or the hard-coded `lmcfarlin`
  (discovery.rs:2806, :2837) is shown with the `user` role. Display only, but any agent that can
  write that header looks like the operator in the transcript view. The name should come from
  config.
* **O4 (Low).** `adapter_for_transcript` (:1698) and the context readers (:1806, :1894) use
  `contains(".codex")` / `contains(".claude")` on the whole path, so `~/.claude-old/...` or a project
  folder named `.codex` selects the wrong adapter.
* **O5 (Low).** `read_transcript_lossy` follows symlinks and checks the type with `stat` before
  `open`; a swap to a FIFO between the two still blocks. Needs a write into the vendor dirs,
  which already means local code execution as the user. `O_NONBLOCK` on open would close it.
* **O6 (Low).** `check_text` (lifecycle.rs:164) rejects control characters only. Bidi overrides
  and zero-width characters in `model` and `cwd` are accepted and echoed to the UI.
* **O7 (Info).** `strip_agent_message_open_tag` (discovery.rs:2638) ends the tag at the first
  `>`, so `from="a>b"` leaves `b">` in the body.

## Checked and not a defect

* **Deep JSON.** `serde_json` is at default features (no `unbounded_depth`), so it rejects nesting
  past 128 levels with an error instead of recursing. A 200 000-deep line, a 300-deep
  `tool_result` chain and 10 000 nested brackets in a tool `arguments` string are all skipped or
  kept as a string. `extract_content_and_tools` recursion is bounded by that limit. Pinned by
  `malformed_truncated_and_deeply_nested_lines_are_skipped` and
  `nested_tool_result_content_below_the_depth_limit_still_flattens`.
* **Tag wrappers.** `replace_agy_blocks` lowercases with `to_ascii_lowercase`, which keeps byte
  offsets, so multibyte text around a tag is safe; an unclosed tag breaks the loop; 100 000
  repeated opens run in linear time. `<\agent-message>`, `<agent-messages>` and a quoted
  `<task-notification>` classify as documented. Pinned by
  `agy_wrappers_handle_unclosed_nested_mixed_case_and_multibyte_text`,
  `agent_message_tag_spellings_and_malformed_tags`,
  `harness_notice_openers_only_count_at_the_start`,
  `nats_and_wire_headers_with_odd_bytes_never_panic`.
* **Command injection.** Every `tmux` call passes argv elements; there is no shell. Personas are
  `[a-z][a-z0-9-]{0,35}` (a leading `-` cannot read as an option), the kill target is `=name`
  (exact match), and `cwd` is a separate argument after `-c`. Pinned by
  `spin_up_rejects_hostile_persona_and_text_fields`.
* **Oversized or hostile session ids in lifecycle.** `SessionTarget::resolve` rejects them before
  any side effect, including a 1 MiB id. Pinned by
  `a_hostile_or_oversized_session_id_is_rejected_before_any_side_effect`.
* **PID reuse.** The two files hold no PIDs. `claude-{persona}-{pid}` (discovery.rs:293) is a
  label only. Route PID liveness is in `fl_wire`, outside this scope.
* **Lock contention.** Lifecycle handlers serialize on the store guard, so read-modify-write on
  hosted records is atomic between handlers. The guard was held across unbounded `tmux` calls
  (F7, fixed). `deliver_prompt` also runs under the guard; I did not audit its timeout in
  `fl_wire`.
* **Artifact pre-archiving.** Candidates are vetted with `canonicalize` against the allowed roots
  and a forbidden-path list in `vet_artifact_file` (api.rs:2763) before anything is copied; ids with `..` are
  skipped for the brain dir. Not re-audited in depth.
* **`fetch_update`.** `grep -rn fetch_update crates/` finds none, so nothing needed replacing with
  a `compare_exchange_weak` loop.

## Build notes

The workspace needs `rustc 1.98` (`rust-version` in `Cargo.toml`); the default toolchain in the
audit container was 1.97 and refused to build. All commands ran with `RUSTUP_TOOLCHAIN=1.98`.

Verification on the commit landed on `main` `6e9ff06` (measured by the coordinator, not by the
cloud agent): `cargo test --workspace` 2415 passed, 0 failed; `cargo clippy -- -D warnings`,
`cargo fmt --check` and `fl arch check` clean. The cloud agent's own fmt, clippy and test run was
on its cloud commit and does not carry over to this one.

## Coverage

| Area | Depth |
|---|---|
| discovery.rs transcript parsing, role/tag normalization, context and quota readers, `find_transcript_file_in_home`, AGY cache and RPC | Read in full. |
| discovery.rs model discovery (Codex, Grok, Claude settings), Muse event turns, worktree and architecture scans | Skimmed; no defect found in what I read. |
| lifecycle.rs | Read in full. |
| api.rs lifecycle handlers and artifact archiving | Read the call sites only. |
| `fl_wire` route dispatch and PID handling | Not audited. |

## Landing note

This audit was produced by an autonomous cloud agent (branch
`cloud/audit-2026-10-30-discovery-lifecycle`, commit `1082f9c`). It was independently reviewed: the
ten claimed defects F1-F10 were all confirmed real; the F8 fix is partial (see its trade-off) and
the F7 test is weak (see its note). The fixes and this report were landed as a fresh commit on
`6e9ff06`, with the line citations re-derived from that tree.
