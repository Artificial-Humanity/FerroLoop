# Wire / Exec / Store cloud audit — 2026-10-30

Scope: FerroLoop `origin/main` at `6e9ff06`, audited on branch
`cloud/audit-2026-10-30-wire-store-exec`. Line numbers are for the tip of that branch (the files
this branch edits have shifted from `6e9ff06`).

## Scope differences from the task

Three of the files the task named do not exist at those paths. I audited what carries the same
role and say so here so nobody assumes they were covered under their own name.

| Named | Audited instead |
|---|---|
| `crates/wire/src/relay.rs` | `crates/wire/src/ws.rs` (RFC 6455 framing, upgrade) and `crates/wire/src/peer.rs` (PSK handshake, peer frames). The relay loop itself is in `daemon.rs` (`handle_peer_connection`). |
| `crates/exec/src/harness.rs` | `crates/exec/src/adapters/harness.rs` (process harness and `GitSnapshot`), plus `crates/exec/src/command.rs` (gate command execution) and `crates/exec/src/git.rs` (git plumbing). |
| `crates/store/src/ferrostore/wal.rs` | WAL frames live in `crates/store/src/ferrostore/layout.rs` (`encode_frame`, `decode_wal`) and are replayed from `mod.rs` (`read_consistent`) and `state.rs`. |

Also read, because the named files call into them: `core/src/workflow.rs` (`advance_workflow`),
`exec/src/record.rs`, `store/src/ferrostore/traits_wire.rs` (`import_wire_batch`,
`update_wire_receipt`), `store/src/ferrostore/traits_workflow.rs` (`update_workflow_run`).

Build note: the workspace needs rustc 1.98 and the sandbox had 1.97; I updated the sandbox
toolchain (1.99.1). No repository file changed for that.

## How to read this report

Every finding has a **status**:

* **Fixed — reproduced.** A regression test failed on the unfixed code (I ran it) and passes now.
* **Fixed — test written.** The fix and a test are in, but I did not run the test against the
  unfixed code (usually because that run would be unsafe, or the test was written with the fix).
* **Open — read.** Confirmed by reading the code; not fixed and no test, with the reason.

Severity is my judgement of impact on a FerroLoop deployment as the docs describe it
(self-hosted, one operator, local agents).

## Summary

| # | Sev | Area | Finding | Status |
|---|---|---|---|---|
| W-1 | **Critical** | wire | PSK handshake can be passed without the PSK (reflection) | Fixed — reproduced |
| W-2 | **High** | wire | A read timeout mid-frame discards bytes and desynchronises the peer stream | Fixed — reproduced |
| D-1 | **High** | wire | Route-table lock held across vendor CLI calls; `Status`/`RegisterRoute` freeze | Fixed — reproduced |
| X-1 | **High** | exec | Concurrent fan-out branches overwrite each other's result (lost update, spend under-counted) | Fixed — reproduced |
| C-1 | **High** | exec | Gate `timeout_secs` never starts if stdin delivery blocks | Fixed — reproduced |
| C-2 | **High** | exec | A background process holding the output pipes holds the gate past its timeout | Fixed — reproduced |
| H-1 | **High** | exec | Attempt spend ceiling is only compared with zero, never enforced | Fixed — test written |
| S-2 | **High** | store | HNSW delete strands the survivors (97% unfindable in the worst case) | Fixed — reproduced (residual noted, O-8) |
| S-3 | **High** | store | Run-length/dictionary columns can claim terabytes from a small blob | Fixed — test written |
| X-3 | Medium | exec | `max_total_steps`/budget ceilings are checked after the attempt that crosses them | Fixed — reproduced |
| I-1 | Medium | wire | Route-table save: shared temp name races, secrets file created world-readable first | Fixed — reproduced |
| D-2 | Medium | wire | Local socket request line unbounded; daemon thread per connection | Fixed — test written |
| D-3 | Medium | wire | Peer connection never times out; inbound handshakes unbounded; empty PSK accepted | Fixed — test written |
| C-3 | Medium | exec | Population paths that are not UTF-8 reach the gate command corrupted | Fixed — reproduced |
| C-4 | Medium | exec | Timeout kills only the direct child; its descendants keep running | Fixed — reproduced (command), test written (harness H-2) |
| G-1 | Medium | exec | `git diff`/`ls-tree`/`mktree` mangle names git would quote; option-shaped args reach git | Fixed — test written |
| X-2 | Medium | exec | `abort_workflow_run` rewrites a Completed/Failed run to Canceled | Fixed — test written |
| R-1 | Medium | exec | `move_record` writes the new state from a stale snapshot | Fixed (narrowed) — test written |
| W-3 | Medium | wire | Antigravity RPC accepts an empty/garbage reply as "status 0"; header injection via `procedure`/`csrf` | Fixed — test written |
| W-4 | Low | wire | WebSocket: FIN, RSV, control-frame length, masking rules not enforced; URL header injection | Fixed — test written |
| W-5 | Low | wire | tmux paste leaves a zombie when the stdin write fails; vendor CLIs have no timeout | Fixed (CLIs, zombie) — test written for CLI timeout |
| H-3 | Low | exec | `GitSnapshot` reads whole dirty files into memory to hash them | Fixed — test written |
| S-4 | Low | store | HNSW search re-sorts its result list for every node visited | Fixed (no test; covered by existing equivalence tests) |
| O-1 … O-17 | — | various | Not fixed; see [Open items](#open-items) | Open — read |

## Findings

### W-1 — Critical — PSK handshake reflection (`crates/wire/src/peer.rs`)

`handshake_peer` has both ends run the same exchange: each sends a challenge, then each sends
`Auth { mac }` *before* it has verified the other's. The MAC was
`HMAC(psk, "fl-wire-v1:" || verifier_nonce || prover_nonce || prover_host)` with nothing saying
which end produced it (old `build_mac_input`, `peer.rs:~70`). An attacker who can reach
`listen_addr` needs no PSK:

1. Open connection A and B to the daemon; read its nonces `nA`, `nB`.
2. On A, send a Challenge whose nonce is `nB`. The daemon answers with
   `Auth(HMAC(psk, nB || nA || host_daemon))`.
3. On B, send a Challenge with nonce `nA` and `host = host_daemon`, then that MAC. The daemon
   verifies `HMAC(psk, nB || nA || host_daemon)` — equal — and accepts.

The attacker is then an authenticated peer: the daemon sends it a `SyncBatch` of every stored
message and receipt, and accepts `Publish` frames that are stored and **dispatched to local
agents** (`daemon.rs`, `process_incoming_peer_frame`).

Fix (`peer.rs:90`, `peer.rs:189`): the MAC input now carries the prover's role
(`PeerRole::Client`/`Server`, taken from `WsStream::is_client()`), and a side verifies with the
*opposite* role. A MAC the daemon makes as server can only verify as a server MAC, never as the
client MAC it expects. `WIRE_PROTOCOL_VERSION` is now 2, so a v1 peer is refused with a clear
"unsupported wire protocol version" instead of a bare MAC failure.

**Deployment note:** both ends of every peer link must upgrade together.

Test: `peer::tests::psk_handshake_is_not_open_to_a_reflection_attack` — a real daemon-side
`handshake_peer` pair plus the attack above. It failed on the unfixed code
(`reflected MAC authenticated an attacker that does not hold the PSK`) and passes now.
`daemon::tests::a_peer_with_the_wrong_psk_is_refused_and_receives_no_messages` covers the
ordinary wrong-key case end to end.

### W-2 — High — mid-frame timeout loses bytes (`crates/wire/src/ws.rs`)

The peer loop sets a 50 ms read timeout (`daemon.rs`, `handle_peer_connection`). The old
`read_frame` used `read_exact` for the header, extended length, mask and payload. When the
timeout fires after some of a frame has been read, `read_exact` has already consumed those bytes
and returns the error; the loop treats `WouldBlock`/`TimedOut` as "nothing yet" and calls
`read_frame` again, which parses the middle of the old frame as a new header. Any frame that
straddles a 50 ms gap (a large `SyncBatch`, a slow link) kills the connection with a protocol
error, or worse is mis-parsed.

Fix (`ws.rs:284`, `ws.rs:385`): `WsStream` keeps an `rbuf`; `read_frame` appends what the socket
gives it and only consumes a frame once `parse_frame` sees all of it. A timeout leaves the
partial frame in the buffer and the next call resumes it.

Test: `ws::tests::frame_split_across_a_read_timeout_is_resumed_not_lost`. Ran on `origin/main`
(copied into a scratch worktree): fails; passes now.

### D-1 — High — `routes` lock held across vendor CLIs (`crates/wire/src/daemon.rs:345`)

`dispatch_and_propagate` held `routes.lock()` for the whole of `dispatch_message_for_store`,
which runs `codex`, `muse`, `grok`, `agy` and `tmux` (some with sleeps of 350 + 250 ms, `codex`
with no limit at all). `Status` and `RegisterRoute` take the same lock, so one slow or hung CLI
froze them; with a hung CLI forever.

Fix: dispatch works on a clone of the table and merges it back with `merge_newer`; a separate
`dispatch_gate` mutex keeps dispatch one-at-a-time so the same pending message is not pushed
twice by two threads. Vendor CLIs now run under `run_status_with_timeout`
(`dispatcher.rs:187`, 60 s limit, child killed and reaped).

Test: `daemon::tests::status_and_route_registration_do_not_wait_behind_a_slow_vendor_delivery`
(ran on `origin/main`: `Status/RegisterRoute waited 1.73s`; now well under a second), and
`dispatcher::tests::vendor_cli_that_hangs_is_killed_at_the_deadline`.

### X-1 — High — lost update between concurrent workflow steps (`crates/exec/src/workflow.rs`)

`execute_workflow_step` loads the `WorkflowRun`, runs the attempt (up to `timeout_secs`, default
900 s), then calls `update_workflow_run(run)` with its stale copy. `update_workflow_run`
(`store/.../traits_workflow.rs:156`) is a blind overwrite — no revision check. With a fan-out,
two branches executing at once (the hub's `/workflow/.../execute` handler runs on request
threads) each write back a run in which the *other* branch is still active. Observed: the run
ends without one branch's completion, and that branch's step and tokens are missing from the run
totals — so the ceilings in `advance_workflow` are checked against an under-count, and the
fan-in `join` never sees both branches. (The test failed on the unfixed code at its assertions
on the final run state; I did not record which of them fired first.)

Fix: the commit phase (`workflow.rs:1300`, `commit_step_snapshot` at `:834`) takes a process-wide
`RUN_COMMIT_LOCK` (`:49`), re-reads the stored run, copies in only *this step's* `StepInstance`,
runs `advance_workflow` on the fresh run and writes that. `approve`, `reject` and `abort` take
the same lock. The embedded store admits one process at a time, so an in-process lock covers the
overlap that exists; a multi-process store would need a revision/compare-and-swap in
`update_workflow_run` instead.

Test: `crates/cli/tests/workflow_concurrency.rs::concurrent_fan_out_branches_both_land_and_their_spend_is_summed`
(real `RedbStore`, two threads, a stub CLI that sleeps). Failed before the fix; passes now. It
lives in `fl-cli` because `fl-exec` may not depend on `fl-store` (the architecture check in
`crates/cli/tests/hub.rs` rejects it — I tried it first).

### C-1, C-2, C-3, C-4 — gate command execution (`crates/exec/src/command.rs`)

* **C-1 (High).** The population was written to the child's stdin *before* the deadline loop
  began. A command that never reads stdin, given more than the 64 KiB pipe buffer of paths,
  blocked that write, so `timeout_secs` never applied (test: 20 000 paths into `sleep 6` with
  `timeout_secs = 1` took 6 s). Now written from its own thread.
* **C-2 (High).** After the command exited, `join_drain` waited for the pipes to reach EOF with
  no deadline. `sh -c 'sleep 6 & echo started'` therefore held the gate for 6 s (and
  `sleep 3600 &` for an hour). Drains now fill a shared, capped buffer (256 KiB) and the
  runner waits at most `PIPE_GRACE` (500 ms) after the command exits, then kills the group and
  uses what it read.
* **C-3 (Medium).** Paths went through `Path::display()`, which turns non-UTF-8 bytes into
  U+FFFD; the command was handed a file that does not exist, and a gate that `test -e`s its
  input failed (or a lenient one passed vacuously). Args, stdin and the list file now carry the
  raw bytes.
* **C-4 (Medium).** A timeout killed only the direct child. The command now runs as its own
  process-group leader (`process_group(0)`, `command.rs:83`) and a timeout (or a stuck pipe)
  kills the group (`kill_group`, `:186`; the `kill` shell built-in, so no libc binding). The
  adapter harness got the same treatment (H-2 below).

Tests: `a_command_that_ignores_a_large_stdin_population_still_times_out`,
`a_background_process_holding_the_pipes_does_not_hold_the_gate`,
`a_path_that_is_not_utf8_reaches_the_command_byte_for_byte`,
`a_timeout_kills_the_whole_process_group_not_just_the_shell` — all four failed before the fix.

### H-1 — High — attempt spend ceiling not enforced (`crates/exec/src/adapters/harness.rs:133`, `:262`)

`AttemptSpec::budget_usd_micros` was compared with `0` (refuse to start) and nothing else. The
harness computed the running cost at every heartbeat and ignored it, so an attempt past its
ceiling ran until it exited or hit `timeout_secs` (default 900 s). `advance_workflow` flags the
overshoot afterwards, but the money is spent. AGENTS.md requires hard ceilings that "fail fast".

Fix: the harness checks the running cost at least once a second (independent of the heartbeat
interval) and, past the ceiling, kills the process group and returns `Crashed` with a
`spend ceiling of N micro-USD` excerpt and the cost seen so far. Spending *exactly* the ceiling
is allowed. No new `AttemptStatus` variant was added (it would touch the stored schema); the
excerpt carries the reason.

Limits of this fix: it can only act on cost the vendor CLI has reported by the time it is
checked, and AGENTS.md notes token/cost telemetry is still partial. It bounds overshoot to about
one second of spend plus whatever the CLI had not yet reported.

Tests: `an_attempt_that_passes_its_spend_ceiling_is_stopped_not_left_to_finish`,
`spending_exactly_the_ceiling_is_allowed_and_a_zero_ceiling_starts_nothing`,
`a_timeout_also_kills_what_the_agent_started`. Written alongside the fix; not run against the
old harness.

### X-3 — Medium — ceilings checked after the attempt that crosses them

`advance_workflow` escalates when `total_steps_executed > max_total_steps` (and likewise for
cost/tokens), i.e. *after* the over-limit attempt has run. With `max_total_steps = 1`, the
second step executed and was then flagged. New `fl_core::workflow::preflight_ceiling`
(`core/src/workflow.rs:863`) answers "has the run already reached a limit?" with `>=`;
`execute_workflow_step` calls it before starting an adapter attempt (`workflow.rs:989`), moves
the run to `NeedsHuman` and returns the new `WorkflowError::BudgetExhausted`
(a policy refusal, CLI exit 2). Offer-only steps spend nothing and are not gated.

Behaviour change: a run whose limit is exactly reached now stops before the next attempt rather
than after it. Test: `a_step_is_refused_before_it_runs_once_max_total_steps_is_used_up`
(failed before; the stub CLI records each launch, so "did not run" is observed, not inferred),
plus `core::workflow::tests::preflight_ceiling_refuses_a_step_once_any_limit_is_reached_not_after_it_is_crossed`.

### S-2 — High — HNSW delete strands the survivors (`crates/store/src/ferrostore/vector.rs:621`)

`remove_node` removed the node and its back-links and did nothing for the nodes that had been
reachable only through it. Measured with a search wider than the collection (so a miss means
"unreachable", not "low recall"): deleting every second of 500 vectors at `m = 2` left **243 of
250** survivors that a search for their own value never returned; at `m = 4`, 5 of 150. They
remain `get`-able and are counted, which is why nothing complained.

Fix: `repair_layer` (`:649`) reconnects the deleted node's former neighbours on each layer —
each keeps its closest `max_links` among its own links plus the other orphans, links stay
reciprocal, over-full nodes are pruned, and a chain through the orphans (by id, deterministic)
keeps them one component, evicting a full node's farthest link only if the victim keeps at least
two. Result on the same data: 11 of 250 (a fresh build of the same 250 vectors loses 5).

The repository already knew part of this: `search_still_fills_the_limit_after_random_deletes`
in `columnar_vector_stress.rs` was `#[ignore]`d as a "known gap: HNSW delete does not reconnect
neighbours". Its own comment said to drop the `#[ignore]` once `delete_vector` repairs
connectivity; it passes with this fix and is now enabled.

Test: `crates/store/tests/vector_delete_connectivity.rs` — compares reachability after deletes
with a fresh build of the survivors (allowed: control + 5%), checks the repaired graph survives
`checkpoint()` and reopen (`validate_graph`, `vector.rs:1201`, re-checks symmetry, link caps and
ordering — an invariant slip here would be a corrupt-on-reload store), and deletes to empty and
reinserts. The unrepaired code fails the first test by a wide margin.

**Compatibility note:** deletes now produce a different (better-connected) graph than before.
Snapshots store the graph, so existing stores are unaffected until they replay new deletes;
two replicas on different versions replaying the same deletes would differ in graph shape
(never in stored vectors or `get` results). See O-8 for what remains.

### S-3 — High — decode amplification in RLE/dictionary columns (`crates/store/src/ferrostore/columnar.rs:25`)

A row group's `row_count` is bounded only by its null bitset (`row_count / 8` bytes), and the
RLE/dictionary decoders clone one stored value per row (`vec![value; n]`, dictionary lookups).
A blob of a few MiB can therefore ask for terabytes: 1 MiB of bitset allows 8 M rows, times a
1 KiB value is 8 GB. `validate_row_group` runs on WAL replay (`state.rs:137`) and on snapshot
decode (`layout.rs:551`), so a corrupt or hostile `.fls` file (or a synced `AppendColumnGroup`)
aborts the process with an allocation failure while opening the store.

Fix: `check_expansion` (`columnar.rs:25`) caps decoded size at 256 MiB for `decode_rle_text`,
`decode_rle_bytes` and `decode_dict_text`, *before* allocating. The encoder (`utf8_codec`,
`choose_codec` for `Bytes`) falls back to plain storage past the same limit, so nothing FerroStore
writes becomes undecodable.

Tests: `run_length_and_dictionary_columns_refuse_to_expand_past_the_limit`,
`the_encoder_stays_plain_where_rle_or_a_dictionary_could_not_be_read_back`,
`a_row_count_the_bitset_cannot_cover_is_refused_without_allocating_for_it`,
`delta_and_run_codecs_reject_truncated_oversized_and_zero_length_runs` (includes `i64::MIN`/`MAX`
wrapping deltas round-tripping). I did **not** run the amplification test against the old
decoder: the attack allocates what it claims.

### I-1 — Medium — `RouteTable::save` (`crates/wire/src/identity.rs:324`)

Every save wrote `routes.json.tmp` then renamed it. Two savers (daemon and hub, or two threads)
truncated each other's temp file and could rename a half-written one into place; the file was
also created with the default mode and `chmod 0600` afterwards, so the CSRF tokens in it were
briefly readable. Now: a temp name unique per process and call (`SAVE_SEQ`, `identity.rs:333`),
`create_new` with mode `0600` at creation, temp removed on failure.
`resolve_claude_key_path` (`:1429`) now takes the lexicographically first `<pid>.*.key` instead
of whichever `read_dir` returns first.

Tests: `concurrent_saves_never_leave_a_corrupt_route_table` (failed on `origin/main` with
"a reader saw a torn route table"), `saved_route_table_is_private_round_trips_and_leaves_no_temp_files`,
`claude_key_lookup_matches_the_exact_pid_prefix_and_is_deterministic`.

### D-2, D-3 — local socket and peer connection limits (`crates/wire/src/daemon.rs`)

* **D-2.** The local request was read with an unbounded `read_line`; the 5 s timeout is per read,
  so a client sending bytes steadily grew the buffer without bound. Now capped at
  `MAX_LOCAL_REQUEST_BYTES` (2 MiB; a send carries ≤ 256 KiB of body), with an error reply.
  Accepted streams are also set blocking: a non-blocking listener hands out non-blocking streams
  on BSD/macOS, where the old code would have read `WouldBlock` as EOF and dropped every request.
* **D-3.** `PEER_READ_TIMEOUT` (45 s) was defined and never used: the loop's 50 ms poll turned
  every timeout into "keep waiting", so a dead peer stayed registered forever. The loop now
  closes after `ws.idle_for() >= PEER_READ_TIMEOUT` (pongs count). Inbound connections are capped
  at 64 (`SlotGuard`, a `compare_exchange_weak` loop — no `fetch_update`), because each costs a
  thread before it has proved it holds the PSK. `spawn_daemon` refuses an empty PSK when it
  listens or dials, and refuses to delete a file at `socket_path` that is not a socket. All
  daemon locks go through `lock()`, which recovers a poisoned mutex instead of panicking every
  later request.

Tests: `oversize_request_line_is_refused_and_the_daemon_keeps_serving`,
`malformed_and_unknown_requests_get_an_error_not_a_dead_daemon`,
`a_networked_daemon_needs_a_pre_shared_key`, `startup_never_deletes_a_file_that_is_not_a_socket`,
`a_second_daemon_on_a_live_socket_is_refused_and_a_stale_socket_is_replaced`,
`inbound_peer_slots_are_capped_and_released`, `inbound_peer_slots_never_exceed_the_limit_under_contention`,
`a_poisoned_daemon_lock_is_recovered_not_propagated`,
`subscriber_filters_match_recipient_and_correlation_case_insensitively`. The idle-peer timeout
itself (45 s) has no test.

### W-3, W-4, W-5 — dispatcher and WebSocket hygiene

* **W-3 (`dispatcher.rs:657`).** `procedure` and `csrf` are written into the HTTP request line and
  a header; a CR/LF/space would add headers or a second request. Now refused (callers already
  validate `csrf`; `procedure` comes from constants — defence in depth). A server that closes
  without a status line used to produce `Ok((0, "", ""))`; it is now an error.
  `is_owner_sender` is used for the Antigravity operator label instead of a second inline copy
  of the same check (`:97`).
* **W-4 (`ws.rs:385`).** `parse_frame` now enforces: RSV bits zero; FIN set on control frames and
  on data frames (fragmentation was never supported — a FIN=0 text frame used to be parsed as if
  complete); control payload ≤ 125; clients mask, servers do not; length checked before the
  payload is waited for; `write_frame` refuses > 16 MiB. `WsStream::connect` rejects URLs with
  control characters or spaces (they were written into the request line and `Host`).
* **W-5 (`dispatcher.rs:1173`).** When writing to `tmux load-buffer` failed the child was left
  unreaped (a zombie per failed delivery); now killed and waited.

Tests: `agy_rpc_treats_a_close_without_a_status_line_as_an_error`,
`agy_rpc_refuses_a_procedure_or_csrf_that_could_inject_headers`,
`agy_rpc_reassembles_a_chunked_body_and_cuts_one_over_the_cap`,
`agy_rpc_survives_a_short_body_a_garbage_chunk_size_and_a_zero_content_length`,
`operator_body_prefix_is_not_doubled_and_slash_commands_stay_bare`,
`a_subject_with_a_newline_cannot_forge_a_second_push_header`, and in `ws.rs`
`malformed_frames_are_protocol_errors`, `oversize_length_header_is_refused_before_the_payload_arrives`,
`peer_closing_mid_frame_is_an_error_and_a_clean_close_is_closed`,
`empty_and_126_byte_payloads_round_trip`, `two_frames_in_one_segment_are_both_delivered_in_order`,
`connect_rejects_urls_that_could_inject_request_headers`,
`host_port_split_handles_bracketed_ipv6_and_bad_ports`, `http_header_reader_stops_at_16_kib`.

### G-1 — Medium — git plumbing (`crates/exec/src/git.rs`)

* `changed_between`, `ls_tree` and `mktree` read and wrote git's default text format, in which a
  path with a non-ASCII byte, quote, tab or newline is C-quoted (`"caf\303\251.rs"`). Callers got
  the quoted text as the file name: a gate population named files that do not exist, and a tree built
  from such a listing stored the mangled name. (State-sync's own entries are hash-named, so I did
  not find it hitting that path.) All three now use `-z`.
* Revisions, object ids, remotes and branches went to git as positional arguments, so a value
  starting with `-` was read as an option (`--output=<file>` on `git diff`; `--upload-pack=<cmd>`
  on `git fetch`/`ls-remote`/`push` runs a command). `plain_arg` (`git.rs:35`) refuses empty,
  `-`-leading and control-character values in every function that takes one. Where those values
  come from (`state_sync.rs` targets) I did not trace to a hostile source; this is closing the
  door, not a known exploit.

Tests: `changed_between_reports_real_names_for_paths_git_would_quote`,
`ls_tree_and_mktree_round_trip_names_git_would_quote`,
`values_that_git_would_read_as_options_are_refused_before_git_runs` (asserts a sentinel file from
`--output=` is never created), `rev_list_first_parent_orders_oldest_first_and_honours_the_lower_bound`.

### X-2, R-1 — Medium — run and record state

* **X-2.** `abort_workflow_run` (`workflow.rs:1622`) set `Canceled` on any run, including a
  `Completed` or `Failed` one. It now refuses finished runs, leaves a `Canceled` run untouched
  (idempotent, keeps `by`), and cancels `Running`/`NeedsHuman`.
* **R-1.** `move_record` (`record.rs:50`) chose transitions from the caller's `record.state`,
  ran the gates (possibly minutes), then wrote the new state unconditionally. If the record had
  moved meanwhile, the gates that ran belonged to a state it was no longer in. It now re-reads
  the record before flushing the decision and refuses if the state differs. This narrows the
  window; it is not a compare-and-swap (`Tracker` has none), so see O-14.

Tests: `abort_refuses_a_finished_run_and_is_idempotent_for_a_canceled_one`,
`a_move_from_a_stale_snapshot_is_refused_and_writes_nothing`. Plus unit tests for
`sanitize_handoff_summary` (char-boundary truncation), `run_scope_slug`/`step_handoff_slug` and
`latest_handoff_for_step` (visit 10 beats visit 2; lookalike step ids ignored).

### H-2, H-3, S-4 — smaller fixes

* **H-2.** The harness now starts the agent as a process-group leader and kills the group on a
  timeout or spend-ceiling stop (`harness.rs:157`, `:360`); previously a build or test run the
  agent had started outlived the attempt.
* **H-3.** `hash_worktree_path` (`harness.rs:775`) hashed dirty files with `std::fs::read`, i.e.
  whole files in memory, twice per attempt, for every dirty or untracked file in the repo. It
  streams in 64 KiB chunks; the digest (`sha256("file:" + bytes)`) is unchanged.
* **S-4.** `search_layer` (`vector.rs:975`) pushed then re-sorted `best` for every visited
  node (O(ef log ef) each). It now inserts at `partition_point` — same order, same result.

## Checked and not a defect

* `is_owner_sender` (`dispatcher.rs:1261`): exact ASCII case-insensitive match, no trimming;
  `validate_slug` and `validate_sender_name` normalise names; the hub refuses owner names for
  peer-derived actors. The tmux-paste path is reachable only from `dispatch_operator_prompt_to_route`
  with an owner sender, and `claude_live_tmux_pane` accepts only `%<digits>` pane ids.
  `dispatch_to_route` (peer messages) never pastes. Not spoofable from `WireMessage` fields.
* Handshake nonces are fixed at 32 bytes, so the MAC's concatenation is unambiguous;
  `hmac::verify` is constant-time; challenge/Auth order violations are rejected.
* Agy RPC reader: header line (8 KiB), section (64 KiB), body (256 KiB) and total deadline are
  bounded; chunk size parse failures end the body without panicking.
* `decode_wal`: every truncation point of a tail frame drops exactly that frame; a corrupt
  non-final frame is an error; a 4 GiB length claim is a torn tail, not an allocation; unknown
  tags, counts larger than the payload, and trailing bytes are errors (new tests pin all of it).
* Columnar delta/RLE/dictionary decoders bound `Vec::with_capacity` by payload size (apart from
  S-3's value cloning) and reject zero-length and past-the-end runs; wrapping `i64` deltas
  round-trip at `MIN`/`MAX`.
* `import_wire_batch` validates every message (`WireMessage::validate`) and refuses a message
  that differs from the stored copy (`WireMessageTampered`); receipt merge is monotonic.
* `RouteTable::upsert` and `get` normalise persona names; invalid names are ignored.

## Open items

Read, not fixed. None was reproduced.

| # | Sev | Where | Item |
|---|---|---|---|
| O-1 | Medium | `daemon.rs` `process_incoming_peer_frame`; `traits_wire.rs:463` | `import_wire_batch` is all-or-nothing, and the daemon ignores its error (`.is_ok()`). One invalid or tampered message in a peer's `SyncBatch` makes every batch containing it fail silently, on every reconnect. |
| O-2 | Low | `daemon.rs` | Peer and subscriber channels are unbounded `mpsc`; a stalled consumer grows memory. A subscriber whose client hung up is only noticed on the next event. |
| O-3 | Low | `daemon.rs:75` | The fallback socket path is `/tmp/fl-wire.sock`, predictable and shared: another local user can bind it first. Used only when neither `XDG_RUNTIME_DIR` nor `HOME` is set. |
| O-4 | Low | `dispatcher.rs` (muse/grok/codex delivery) | An operator body that starts with `-` is passed as a positional/value argument to vendor CLIs and may be read as an option, depending on each CLI's parser. Not testable without those CLIs. |
| O-5 | Low | `dispatcher.rs` (muse/grok) | `tmux_target` may be a session/window name; `tmux -t <name>` resolves to the *active* pane, which need not be the agent's (the Claude path already restricts to `%<id>`). |
| O-6 | Low | `workflow.rs:441` | `write_workflow_fact` reports a state-store failure as `WorkflowError::InvalidStepLimits`. |
| O-7 | Low | `workflow.rs:1079` | `expect("adapter thread did not panic")` / runtime build `expect` turn an adapter panic into a panic of the caller (the hub request thread). |
| O-8 | Medium | `vector.rs` | Even insert-only HNSW graphs strand nodes at small `m` (37 of 499 at `m = 2`, 1 of 299 at `m = 4`): the strictly reciprocal link rule lets a neighbour's prune drop a new node's only link. S-2's repair cannot fix what insertion already lost. Needs a "never prune a node's last link" rule in `link_layer`/`prune`; I held back because it changes the persisted graph shape for every insert. |
| O-9 | Low | `layout.rs:664`, `mod.rs:2081` | `decode_wal` runs on the superblock-committed extent only, so a short or CRC-bad *final* frame inside it is data the superblock said was durable; it is dropped silently as a "torn tail". That is probably right after a crash between WAL write and superblock write, but it also hides truncation of committed bytes. Worth a log line or a stricter check keyed on the superblock's recorded frame count if it has one. |
| O-10 | Low | `ws.rs:239` | `WsStream::accept` upgrades on any request carrying `Sec-WebSocket-Key`; it does not check method, `Upgrade`, `Connection` or version. |
| O-11 | Low | `dispatcher.rs` | `tmux display-message`, `send-keys`, `capture-pane` run with no timeout; a wedged tmux server blocks dispatch (now only dispatch, not `Status`). |
| O-12 | Low | `command.rs:35` | The output excerpt is stdout then stderr, cut at 4 KiB: a command with > 4 KiB on stdout never shows its stderr. |
| O-13 | Low | `command.rs` | Stdin/file-list delivery is newline-delimited, so a path containing a newline splits into two. Args delivery is unaffected. A gate with a very large population over `Args` fails at spawn with `E2BIG` (an Error verdict, not a pass). |
| O-14 | Medium | `record.rs`, `Tracker` | `move_record`'s re-check (R-1) is not atomic: `set_record_state` has no expected-state parameter, so two moves of the same record can still interleave between the check and the write. Needs a compare-and-set on `Tracker`. |
| O-15 | Low | `workflow.rs:49` | `RUN_COMMIT_LOCK` is per process. Fine for the embedded store (one process at a time); wrong if a networked/shared store is ever used for runs. |
| O-16 | Low | `identity.rs:178` | `agy_ls_port` accepts port `0` and ignores trailing junk after the digits (`127.0.0.1:8080abc` → 8080). Harmless today (the dial goes to loopback only) but loose. |
| O-17 | Low | `identity.rs:284` | `RouteTable::load_file` accepts the strict form without normalising or validating persona keys, so an entry keyed `Kayla` is stored but never found by `get("kayla")`. The file is `0600`, so this is a hand-edit problem only. |

## Behaviour changes to be aware of

1. **Peer wire protocol is now version 2.** Upgrade both ends of a peer link together.
2. A networked daemon (`listen_addr` or `peers` set) with an **empty PSK is refused** at startup.
3. A workflow step is refused (and the run moved to `NeedsHuman`) when the run has *reached* a
   ceiling, one attempt earlier than before. New error: `WorkflowError::BudgetExhausted`.
4. An attempt that passes its spend ceiling is **killed**, status `Crashed`.
5. `abort_workflow_run` errors on a Completed/Failed run.
6. Gate commands, and the agent processes the harness starts, run in their own process group.
7. Deleting a vector now re-links its neighbours (graph shape differs from before after deletes).
8. `Git::*` functions return an error for `-`-leading arguments.

## Verification

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace` (rustc 1.99.1). No `AtomicX::fetch_update` is used anywhere; the one new
lock-free counter (`SlotGuard`) uses a `compare_exchange_weak` loop.
