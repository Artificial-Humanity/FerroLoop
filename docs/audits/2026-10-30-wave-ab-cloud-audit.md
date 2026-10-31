# Wave A/B cloud audit — 2026-10-30

Scope: the audit examined FerroLoop `origin/main` at `a25021e` (PRs #98–#107) and was landed on
`0a96ad1`, which adds #108: the Hub (records, artifacts, settings, lifecycle, session actions,
commands, discovery, terminal), FerroStore sync, FerroGate adapters, and the FerroDeck UI (records
board, pairing, settings, help, API client). Wave C (#108: operators, login, passkeys, cloud
adapters) was not audited.

Line numbers are for the tip of branch `cloud/audit-2026-10-30-wave-ab` unless a finding says
"pre-fix". The files this branch edits have shifted by a few lines.

## How to read this report

Every finding has a **verification** status:

* **Reproduced** — a regression test fails on the unfixed code and passes on the fixed code. I ran
  both.
* **Read** — confirmed by reading the code; no failing test was written (usually because the fix
  would be breaking, or the issue is design-level).
* **Delegate** — reported by a read-only sub-reviewer that I did not re-verify line by line.
  Treat as a lead.

Some findings from the sub-reviewers were wrong or weaker than reported. They are listed under
[Checked and not a defect](#checked-and-not-a-defect) so nobody re-raises them.

## Coverage — what was and was not examined

| Area | Depth |
|---|---|
| `hub/records.rs`, `hub/artifacts.rs`, `hub/settings.rs`, `hub/commands.rs`, `hub/session_actions.rs`, `hub/terminal.rs` | Read in full. |
| `hub/lib.rs` project-store locking (`open_project_store_at`) | Read in full. |
| `hub/api.rs` | Only the artifact, prune, settings, model, bulk-action and routing paths. |
| `hub/lifecycle.rs` | Validation, slug and tmux helpers, spin-up/teardown entry points. Not every teardown branch. |
| `hub/discovery.rs` (6.5k lines, ~3.4k non-test) | **Hotspot scan only**: file reads, byte slicing, `to_lowercase` offset reuse. Vendor transcript parsers were not reviewed line by line. |
| `store/ferrostore/sync.rs`, `SyncSlot` in `mod.rs` | Locking and panic paths read by me. FLEN framing and hybrid/vector offload hooks reviewed by a **delegate**. |
| `gate/adapter.rs` (the task named `adapters.rs`; that file does not exist) | Read by me for the fixed items; the rest by a **delegate**. |
| UI: `RecordsBoardPane`, `SettingsModal` | Read by me for the fixed items; remaining findings by a **delegate**. |
| UI: `PairingModal`, `HelpDrawer`, `client.ts` | **Delegate** only, plus targeted reads. `HelpDrawer` reported clean. |

Nothing here is a full security sign-off. In particular `discovery.rs` and the lifecycle teardown
paths deserve a second pass.

## Summary

| # | Sev | Area | Finding | Status |
|---|---|---|---|---|
| F1 | **High** | store | A panic in `SyncAdapter::on_commit` wedges the store for good | Fixed, reproduced |
| F2 | **High** | ui | Stale `listRecords` answer overwrites the board after a project switch | Fixed, reproduced |
| F3 | Medium | hub | Terminal: `exited` never sent, and close/disconnect hangs, while a background job holds the pipes | Fixed, reproduced |
| F4 | Medium | ui | TOTP enrollment can show a made-up, publicly known secret; verify treats a blank answer as success | Fixed, reproduced |
| F5 | Medium | ui | Create/move answers are merged into whichever project is on screen when they arrive | Fixed, reproduced |
| F6 | Medium | gate | `ExternalSecret` derives `Debug`, so `{:?}` prints the plaintext secret | Fixed, reproduced |
| F7 | Medium | hub | Artifact registry: one unreadable entry or a corrupt file is silently wiped by the next save | Fixed, reproduced |
| F8 | Low | hub | Terminal output: a multi-byte character split across 4096-byte reads becomes U+FFFD | Fixed, reproduced |
| F9 | Low | gate | `?tier=local` or `?tier=hub` returns 502 when an unrelated cloud adapter fails | Fixed, reproduced |
| F10 | Low | hub | `SKILL.md` saved with a UTF-8 BOM is not discovered | Fixed, reproduced |
| F12 | Medium | hub | Terminal: unbounded output queue and blocking stdin write | Open, read |
| F13 | Medium | ui | Session token in URLs and passkey private key in `localStorage` | Open, delegate |
| F14 | Low | ui | `request()` treats a non-JSON 2xx as success | Open, delegate |
| F15 | Low | hub | Project falls back to the hub store under any slug | Open, read |
| F16 | Low | gate | Registry shadowing compares names byte-for-byte | Open, delegate |
| F17 | Info | hub | `interrupt` and `model` accept Peer authority | Open, read — confirm intent |
| F18 | Info | store | Cloud search hits are not bounded or checked for non-finite scores | Open, delegate |
| F19 | Info | store | FLEN decoder accepts non-canonical frames | Open, delegate |

---

## Fixed in this branch

### F1 — A panic in `on_commit` wedges the store (High)

`crates/store/src/ferrostore/mod.rs`, `sync_flush` (~947) and `deliver_owned` (~1619); pre-fix
they called `adapter.on_commit(&batch)` directly.

Both paths set `notify_in_flight = true` and `notify_owner` under the sync mutex, release it, call
the adapter, then clear the flag. Nothing cleared the flag on unwind. After one adapter panic,
every other thread's `sync_flush` waits on `sync_cv` for ever, and `attach`, `detach`, `sync_pull`
and cache fills refuse for good ("a notify is in flight").

**Fix:** `FerroStore::call_on_commit` (mod.rs:1589) wraps the call in `catch_unwind`. On a panic it
clears the flag and owner, re-queues the batch at the front (the batch is not lost), wakes waiters,
and re-raises the panic.

**Tests:** `panicking_write_through_adapter_does_not_wedge_the_store`,
`panicking_flush_keeps_the_batch_and_releases_waiters` (`sync.rs`). Without the fix both fail with
`sync_flush wedged` after the 5 s guard.

### F2 — Stale board load wins the race (High)

`ui/src/components/RecordsBoardPane.tsx`, `loadRecords` (pre-fix lines 98–138).

No cancel flag, abort signal or sequence check. Switching project A→B→A while A's first answer is
slow lets the abandoned answer land last: the board shows another project's cards under the wrong
dropdown value, and the old request's `finally` clears the newer request's spinner. An old error
also showed against the new project.

**Fix:** a `loadSeqRef` counter orders loads; only the newest may set records, summary, error or
loading state (lines 99–125).

**Tests:** `Wave2026AuditRegressions.test.tsx`, "keeps the newest project's board when an older
load resolves last" and "does not let an abandoned load's failure or completion clear the newer
load". Both fail on the old component.

### F3 — Terminal hangs when a background job holds the pipes (Medium)

`crates/hub/src/terminal.rs`, wait task (pre-fix: `stdout_done_rx.await; stderr_done_rx.await`
with no bound).

After the shell exits, the task waits for both reader tasks to reach EOF. `sleep 600 &` in the
shell gives the child a copy of the stdout/stderr pipes, so EOF only arrives when that job ends.
Effects: the `exited` frame is not sent; `TerminalSession::shutdown` awaits that task, so closing
a terminal, replacing a session by id, or a WebSocket disconnect (`shutdown_all`) blocks for as
long as the job lives. The kill only reaches the direct child.

**Fix:** the drain wait is bounded by `READER_DRAIN_GRACE` (2 s, terminal.rs:24, used at ~408).

**Tests:** `exit_is_reported_even_when_a_background_job_holds_the_pipes` and
`close_does_not_hang_on_a_pipe_held_by_a_background_job`. I confirmed both fail when the grace is
set to 600 s. (The first draft of the close test passed without the fix because it killed the
shell before it had forked the job; it now waits for a `ready` marker.)

### F4 — TOTP enrollment: invented secret, and verify fails open (Medium)

`ui/src/components/SettingsModal.tsx`, `handleEnrollTotp` (~236) and `handleVerifyTotp` (~262).

* Pre-fix, `res ?? { secret_base32: "JBSWY3DPEHPK3PXP", … }` substituted the well-known demo secret
  when the answer was nullish. An empty-object answer (which `client.ts:474` returns for an empty
  2xx body) skipped the fallback and rendered blanks.
* `res?.ok !== false` marked TOTP verified for `{}` or `null`.

**Fix:** enrollment requires non-blank `secret_base32` and `otpauth_uri` strings, else it shows an
error and no secret. Verification requires `ok === true`.

**Tests:** five tests in `Wave2026AuditRegressions.test.tsx` (empty object, null, blank secret,
missing URI, and verify answers `{}`/`null`/`{ok:false}`). All fail on the old component.

### F5 — Create/move responses merge into the wrong project (Medium)

`RecordsBoardPane.tsx`, `handleCreateRecord` and `handleMoveRecord`.

They merged the response into `records` without checking that the project was unchanged after the
`await`. A create in A appeared on B's board. A move replaced the card whose `display_id` matched,
and display ids are per-project numbers ("1" exists in every project), so it could overwrite an
unrelated card of B.

**Fix:** both capture the project at request time and skip the merge if `projectRef.current`
differs.

**Tests:** "does not add a record created in one project…" and "does not apply a move to a
same-numbered card…".

### F6 — `ExternalSecret` leaks through `Debug` (Medium)

`crates/gate/src/adapter.rs:157`. The type's doc says `value` is plaintext, yet it derived
`Debug`, so `{:?}`, `tracing`, or a logged `Result<Option<ExternalSecret>, _>` printed it.

**Fix:** manual `Debug` (adapter.rs:162) that prints `value: "[REDACTED]"` and keeps the
metadata. **Test:** `external_secret_debug_never_prints_the_value` (plain, wrapped, `{:#?}`).

### F7 — Registry data loss on a damaged file (Medium)

`crates/hub/src/artifacts.rs`, `Registry::load` / `with_registry` (pre-fix).

A registry that failed to parse loaded as empty, and any later change saved over it, so a single
bad byte erased every entry and left the managed copies unreferenced for good. Entries that no
longer decode (for example after a schema change) were dropped the same way.

**Fix:** `load_checked` (artifacts.rs:102) reports whether anything was dropped. If a save is
about to replace such a file, the original is first copied to `registry.json.corrupt`
(`corrupt_backup_path`, :220). A read-only use leaves the file alone. Healthy registries are
unaffected. `Registry::load` is now test-only.

**Tests:** `a_damaged_registry_is_kept_aside_before_the_next_save_replaces_it`,
`a_read_only_use_of_a_damaged_registry_does_not_touch_it`.

Not addressed: `registry.json.corrupt` is overwritten if the registry is damaged a second time.

### F8 — UTF-8 split across reads (Low)

`terminal.rs`, stdout/stderr readers (pre-fix `String::from_utf8_lossy(&buf[..n])` per 4096-byte
read). A character straddling a read boundary became two U+FFFD in the operator's terminal.

**Fix:** `Utf8Chunker` (terminal.rs:31) holds back an incomplete tail and still replaces bytes that
are invalid in themselves. **Tests:** four chunker unit tests (every split point of 1–4-byte
characters, invalid bytes, truncated tail at EOF, the exact 4095+2 case) and an end-to-end
`terminal_output_keeps_multibyte_text_whole`.

Not addressed: `filter_bash_startup_noise` still runs per chunk, so a noise line split across two
reads is not recognised.

### F9 — Unrelated adapter failure breaks a tier filter (Low)

`crates/gate/src/adapter.rs`, `handle_tiered_registry_v01_get` (:103). `?tier=local` still called
every attached adapter, so a down cloud adapter turned a local-only listing into a 502.

**Fix:** adapters of a tier after the requested one are not asked. They cannot change the answer,
because earlier tiers win name clashes. A `cloud` filter still asks the hub adapters, so a hub row
keeps shadowing a cloud row of the same name.

**Tests:** `a_failing_later_tier_does_not_break_an_earlier_tier_filter`,
`tier_filter_keeps_hub_shadowing_cloud_rows`.

### F10 — BOM hides `SKILL.md` frontmatter (Low)

`crates/hub/src/commands.rs`, `parse_frontmatter`. A UTF-8 BOM made the first line `\u{feff}---`,
so the skill was skipped. The leading BOM is now stripped.
**Test:** `frontmatter_survives_a_byte_order_mark_and_windows_line_endings`.

---

## Open findings

### F12 — Terminal resource limits (Medium) — read

* `terminal.rs:119` — frames go through `mpsc::UnboundedSender`. A child that writes faster than
  the WebSocket drains (`yes`) grows hub memory without limit.
* `terminal.rs:490` — `stdin.write_all(..).await` runs inside `handle_input`, which holds
  `&mut self` for the whole WebSocket loop. A child that stops reading stdin blocks that connection,
  including its `close` events, once the pipe is full.
* No cap on sessions per connection; `session_id` is client-chosen and uncapped.

Owner-only (`handle_frame` checks `is_owner()` and `enable_terminal`), so this is self-DoS rather
than a privilege issue. A bounded channel and a write timeout are the fix; both change observable
behaviour, so I left them.

### F13 — Credential storage in the browser (Medium) — delegate, partly read

* `client.ts:341–345` stores the passkey private JWK, exported as extractable, in `localStorage`
  under `fl_hub_passkey_keystore` (:238). Any XSS can read it.
* `client.ts:425` stores the session token in `localStorage`; `getArtifactRawUrl` (:1035) puts it in
  a `?token=` query, where it reaches history, logs and `Referer`.

Fixing this means non-extractable keys in IndexedDB and short-lived signed URLs or cookie auth. It
is a design change, not a patch.

### F14 — Non-JSON 2xx is success (Low) — delegate

`client.ts:474` returns `parsed ?? {}` as `T`. A 200 with an HTML body (captive portal, SPA
fallback on a wrong base URL) resolves as an empty success; a login flow then proceeds as if it had
worked. F4 closes the two TOTP cases; the general fix (reject a non-empty unparsable 2xx body)
needs a pass over callers that rely on empty bodies.

### F15 — Any slug reaches the hub store (Low) — read

`records.rs:130–148`, `open_project_store`. If `<slug>.redb` does not exist the request falls back
to the hub store. `GET /api/v1/records?project=nonexistent` returns the hub store's records labelled
`"project": "nonexistent"`, and an Owner `POST` creates a record in the hub store rooted at
`/workspace/nonexistent`. This is documented as the dev/test fallback, but a typo in the slug writes
into the wrong store without a warning. Suggest 404 for a slug that is not in
`available_projects` once the fallback is no longer needed.

### F16 — Registry name shadowing is exact-match (Low) — delegate

`adapter.rs:333` `push_unique` compares `server.name` byte for byte. A remote tier can list
`io.ferroloop.gate/Shared` (or a lookalike) next to the local `io.ferroloop.gate/shared`. Rows are
tier-tagged, so this is confusion rather than override. Not wired into the hub today (only
`handle_registry_v01_get` is), so latent.

### F17 — Peer authority on session control (Info) — read

`api.rs` dynamic routes: `POST /sessions/:id/{prompt,clear,interrupt,model}` do not check
`is_owner()`; `bulk-action`, `decommission` (for unmanaged sessions) and spin-up of active agents
do. A Peer can therefore interrupt any session or change its model. `prompt`/`clear` being
peer-callable looks deliberate (`actor_name` exists for it). Please confirm `interrupt` and `model`
are meant to be too. No change made.

### F18, F19 — FerroStore (Info) — delegate

* Cloud search hits (`mod.rs` `cloud_hits`) skip the checks remote hits get: results are not
  capped at `limit`, and NaN/∞ scores are accepted. An `Err` from the adapter silently falls back
  to local search with no log.
* The FLEN v1 decoder (`sync.rs`) is memory-safe: lengths use `checked_add`, allocations happen
  after the bounds check, vector dimension is capped. It is not canonical: a `DeleteDocument` frame
  may carry a payload that is ignored, and a `VectorUpsert` may carry both payload bytes and floats.
  Rejecting those would be a protocol tightening; matters only if frames are ever hashed or signed.
  The frame has no checksum or size cap, so callers must bound input.

---

## Checked and not a defect

* **"`..` in a prune `worktree_path` is an existence oracle" (my own hypothesis).** It is not.
  `is_forbidden_artifact_path` (`api.rs:2372`) refuses any `..` component before existence is
  checked, so missing and existing paths answer alike. I wrote the test first, saw it pass on
  unmodified code, and dropped the code change. The test stays as a guard:
  `artifact_prune_treats_dotdot_worktrees_the_same_whether_or_not_they_exist`.
* **"Passkey sign counter is incremented before the server accepts the login, so a failure locks the
  user out"** (delegate). `client.ts:1217`. WebAuthn only needs the counter to increase;
  incrementing first never reuses a value. Left alone.
* **Project-store lock lost wake-up.** `open_project_store_at` uses `notify_one`. With four waiters
  behind one holder every waiter got its turn. `requests_queued_behind_a_held_project_all_get_their_turn`
  guards that. (`notify_all` would be more forgiving but is not needed.)
* **Hosted-session slug collisions.** `hosted_session_slug` maps `:` to `-`, which would collide
  for `a:b-c` and `a-b:c`. It cannot today because no vendor name contains `-`.
  `hosted_session_slugs_stay_one_to_one_with_session_ids` fails if one is added.
* **Path traversal through project slugs or record ids.** `normalize_project` and
  `decode_record_id` hold. `project_slug_tricks_in_query_and_body_never_reach_another_path` covers
  encoded dots, NUL, absolute paths and backslashes in query and body.
* **Concurrent creates on one project.** Eight simultaneous POSTs: each answers 201 or 503, handles
  are unique, and every 201 is listed (`concurrent_creates_on_one_project_neither_lose_nor_duplicate_records`).
* **Concurrent settings and registry updates** are serialised and lose nothing
  (`concurrent_patches_to_different_fields_all_survive`, `concurrent_registry_updates_are_not_lost`).
  Atomic writes replace a symlink instead of writing through it
  (`write_atomic_replaces_a_symlink_instead_of_writing_through_it`).
* **`HelpDrawer`**: no `dangerouslySetInnerHTML`, no raw links, effects cleaned up (delegate).

## Other tests added without a defect behind them

`session_actions.rs` (override precedence by id then lower-cased persona, thinking level kept,
model length counted in characters, C1 controls), `settings.rs` (a rejected patch writes nothing;
out-of-range stored values fall back per field), `commands.rs` (name restriction, one-line bounded
descriptions), `artifacts.rs` (file-name cap keeps a short extension), `records.rs` (moving to the
current state).

## Verification run

```
cargo fmt --all -- --check                         clean
cargo clippy --workspace --all-targets -- -D warnings   clean
cargo test --workspace                             2235 passed, 0 failed
cd ui && npm test -- --run                         20 files, 161 tests passed
cd ui && npx tsc --noEmit                          clean
```

The checkout needed Rust ≥ 1.98 (`rust-version` in `Cargo.toml`); the sandbox shipped 1.97, so the
run used a `rustup update stable` toolchain (1.99.1). The UI needed `npm ci`.

## Recommended follow-ups, in order

1. F12 — bounded terminal output channel and a stdin write timeout.
2. F13 — move the passkey key to a non-extractable store; stop putting the token in URLs.
3. F17 — confirm intended authority on `interrupt` and `model`.
4. A second review pass over `discovery.rs` transcript parsers and `lifecycle.rs` teardown.
