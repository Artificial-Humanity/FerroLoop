# Claude-Hosted Session `--resume` on Respawn (`#433`, Path 1) Implementation Plan

> **For agentic workers:** Use the repository-required `subagent-dev` skill: a fresh implementer per task, then the `code-review` loop on each task, then a final whole-branch review.

**Goal:** Resume confirmed hosted Claude Code sessions across `fl-host` restarts and respawns via `claude --remote-control ... --resume <vendor_session_id> --name <DisplayName>` so existing Remote Control bridge sessions (`cse_...`) reattach in place without minting stale Recents entries, losing conversation context, or paying a full re-hydration turn.

**Architecture:** Extend `ClaudeHostAdapter` with active-only `.jsonl` transcript lookup, torn-tail JSONL repair, and post-`compact_boundary` active-token estimation; resolve model-relative resume token caps (`min(750_000, 75% of model context_window)`) through `crates/hub/src/context_window.rs`; wire `prepare_resume` + the exact fallback ladder (with per-reason lifecycle notices, `#406` generation preservation, `"Host restarted"` instruction-refresh mail, and strict child identity-env isolation) into `restart_hosted_session` and `spin_up`.

**Tech Stack:** Rust (`fl-vendor-adapters`, `fl-hub`, `fl-wire` / `fl-cli` test fixtures), `serde_json`, existing lifecycle and private-host black-box tests.

**Spec:** [`docs/superpowers/specs/2026-11-09-claude-resume-on-respawn-433-design.md`](../specs/2026-11-09-claude-resume-on-respawn-433-design.md)

---

## Global Operational Mandates & Lead Rulings

1. **Subprocess Identity Scrubbing (`#460`):** Every test, fixture, or command in this plan that spawns `fl` or any vendor CLI (`claude`, `codex`, `agy`, `muse`, `grok`) MUST scrub inherited agent identity variables (`env -u AGENT_PERSONA -u AGENT_NAME -u FL_HOSTED_ID -u AGENT_HYDRATION_GENERATION`).
2. **Model-Relative Resume Cap (Ruling #1):** Active tokens after the last `compact_boundary` are capped at `min(MAX_CLAUDE_RESUME_ACTIVE_TOKENS, window_tokens * 3 / 4)` where `MAX_CLAUDE_RESUME_ACTIVE_TOKENS = 750_000` (overridable via `FL_CLAUDE_MAX_RESUME_ACTIVE_TOKENS`), NOT a flat `750k`. A `200k`-window model caps at `150,000` tokens and MUST refuse a `388k`-token tail. Resolve `window_tokens` in `crates/hub/src/context_window.rs` from existing codebase sources (`record.context_window`, `fl_vendor_adapters::claude::split_window_tag`, `fl_core::lookup_vendor("anthropic-api")` `200_000` for pre-5.x Claude 3/4 models, and `discovery::ASSUMED_LARGE_CONTEXT_WINDOW` `1_000_000` for Claude 5.x / default CLI models) — never introduce a duplicate model table.
3. **Deploy & Respawn Order (Ruling #2):**
   - **How respawns are invoked after a host restart:** `boot_reconcile` (`crates/hub/src/boot_reconcile.rs`) only reconciles already-running panes and sends `"Host restarted"` mail on a `boot_id` change; `reconcile_dead_hosted_sessions` (`crates/hub/src/lifecycle.rs:5242`) marks absent `running` hosted sessions `"stopped"` after two `RuntimePresence::Gone` checks without spawning. Actual respawns after `ferroloop-host.service` restarts are invoked via `POST /api/v1/hosted-sessions/:id/restart` (`lifecycle::restart_hosted_session`) or `POST /api/v1/sessions/spin-up` / `fl agent spin-up` (`lifecycle::spin_up` on an existing non-decommissioned `HostedSessionRecord`). Both entry points therefore share the resume eligibility and fallback ladder.
   - **Required deploy sequence so the very first host restart already resumes:**
     1. Build and install the updated `fl` binary (`~/.local/bin/fl`).
     2. Restart `ferroloop-hub.service` (and `ferrowire.service`) **first** while `ferroloop-host.service` keeps the live sessions running (`ferroloop-hub.service` declares `After=`/`Wants=ferroloop-host.service`, not `PartOf=`, so restarting the hub does not stop `fl-host`).
     3. Restart `ferroloop-host.service` (which also ships `#438`).
     4. Trigger `POST /api/v1/hosted-sessions/:id/restart` (or `spin-up`) against the already-upgraded `fl-hub`; each confirmed Claude agent spawns with `--resume <vendor_session_id>`, reattaches its persisted `cse_...` Remote Control bridge session, and receives `"Host restarted"` instruction-refresh mail.
4. **Exact Fallback Ladder & Lifecycle Notices (Ruling #3):** Every fallback from `--resume` to fresh `prepare_launch` (`unconfirmed_hydration`, `recast_guidance_staged`, `missing_or_tombstoned_session_id`, `missing_or_archived_transcript`, `empty_or_unparseable_transcript`, `context_over_cap`, `resume_launch_did_not_survive`) mints a fresh UUIDv7 `vendor_session_id` + fresh `hydration_generation`, prunes prior `hydration-ack-*` receipts, and emits a lifecycle notice naming the exact reason in `RestartResponse.warnings` / `SpinUpResponse.warnings` (broadcast on `"session.restarted"` / `"session.spun_up"`) and `hub:` stderr logs when `env.log_hydration_warnings` is enabled.
5. **`SIGKILL` Bridge Reattach Proof Without Depending on HTTP `409` (Ruling #4):** As recorded in `Spike433` Step `1b-kill9` (`debug-1b-sigkill.log`: `[bridge:repl] Reattaching to persisted bridge session cse_01SP4ortBQU3hQGNi4NtM8pN at seq 24` -> `[remote-bridge] v2 transport connected`), reattachment depends on the persisted `{"type":"bridge-session","sessionId":"<uuid>","bridgeSessionId":"cse_..."}` record in `<uuid>.jsonl`, not on the transient `Unarchive status=409` HTTP response. `ClaudePostCompactionContext` exposes `has_persisted_bridge_session` / `bridge_session_id` and tests verify a `SIGKILL`-style unarchived transcript (no trailing `Archive` teardown, or mid-turn `SIGKILL` with torn bytes after `bridge-session`) preserves `bridgeSessionId` and resumes cleanly without inspecting HTTP status codes.
6. **Child Identity Environment Isolation (Ruling #6 & `#460`):** A resumed (or freshly spawned) hosted child MUST receive strictly its own hosted session identity: `AGENT_PERSONA=<record.persona>`, `AGENT_HYDRATION_GENERATION=<record.hydration_generation>`, and `FL_HOSTED_ID=<record.session_id>`. Strip `"AGENT_NAME"`, `"AGENT_PERSONA"`, `"AGENT_HYDRATION_GENERATION"`, and `"FL_HOSTED_ID"` from inherited `env_allow` in `HostRuntime::spawn` (and pass `-u` flags in `TmuxRuntime::spawn`) unless explicitly populated in `spec.env`, and prove with an end-to-end fixture that ambient foreign identity env vars cannot cause a resumed session or its `SessionStart:resume` hook to register under another persona.

---

### Task 1: Claude Transcript Preflight, Post-Compaction Estimator, Tail Repair & Model-Relative Window Cap

**Files:**
- Modify `crates/vendor-adapters/src/claude.rs`
- Modify `crates/hub/src/discovery.rs` (expose `pub(crate) const ASSUMED_LARGE_CONTEXT_WINDOW: u64 = 1_000_000`)
- Modify `crates/hub/src/context_window.rs`

**Steps:**
- [ ] Write failing unit tests in `crates/vendor-adapters/src/claude.rs` and `crates/hub/src/context_window.rs`:
  - `find_active_session_transcript_path` finds `<home>/.claude/projects/<slug>/<uuid>.jsonl`, returns `None` when the file exists only under `<home>/.claude/archived/<slug>/<uuid>.jsonl` or is absent, and rejects non-UUID / traversal strings.
  - `repair_session_transcript_tail` leaves complete JSONL transcripts—including one ending with a dangling `assistant` `tool_use` (`stop_reason: "tool_use"`) and `SIGKILL` transcripts lacking teardown records—completely byte-for-byte untouched, while truncating a torn trailing non-JSON fragment (`{"type":"assistant","message":`) back to the last valid newline-terminated JSON record.
  - `estimate_post_compaction_context` computes `turn_count`, `latest_model`, `active_context_tokens` (`input_tokens + cache_creation_input_tokens + cache_read_input_tokens` from the last non-`<synthetic>` `assistant` turn after the last `compact_boundary`, falling back to `compactMetadata.postTokens`), `post_compact_conversational_bytes` (excluding `attachment`, `api-request*`, `queue-operation`, `bridge-session`, `cost-state`, sidechains, and pre-compaction history), and `bridge_session_id: Option<String>` from `{"type":"bridge-session"}` (proving `SIGKILL` bridge-session persistence without HTTP `409` coupling).
  - `context_window::claude_session_context_window` & `claude_resume_active_token_cap`:
    - `claude-opus-5-5` (`1M` window) -> cap `750,000` (`388,396` tokens passes; `895,176` tokens fails).
    - `claude-opus-4-1`, `claude-sonnet-4-5`, or `claude-opus-5-5[200k]` (`200k` window) -> cap `150,000` (`388,396` tokens **fails**; `120,000` tokens passes).
    - Explicit `record.context_window = Some(200_000)` overrides assumed `1M` -> cap `150,000`.
- [ ] Implement `ClaudePostCompactionContext`, `find_active_session_transcript_path`, `repair_session_transcript_tail`, and `estimate_post_compaction_context` on `ClaudeHostAdapter`, plus `claude_session_context_window` and `claude_resume_active_token_cap` in `crates/hub/src/context_window.rs`.
- [ ] Run `cargo fmt --all --check`, `cargo clippy -p fl-vendor-adapters -p fl-hub --all-targets -- -D warnings`, and focused `cargo test -p fl-vendor-adapters -p fl-hub`, then commit with `Co-authored-by: Feira <feira@artificialhumanity.io>`.

---

### Task 2: `"Host restarted"` Instruction-Refresh Notice for Reboot and Same-Boot Service Resume

**Files:**
- Modify `crates/hub/src/boot_reconcile.rs`
- Modify `crates/hub/src/boot_reconcile_tests.rs`

**Steps:**
- [ ] Write failing unit tests in `crates/hub/src/boot_reconcile_tests.rs`:
  - `format_host_restarted_notice` lists explicit relative/role instruction file paths (`AGENTS.md`, `WORKFLOW.md`, `PERSONA.md`, `.agents/roles/<role>.md`, `docs/roles/<role>.md`) whose `mtime` strictly exceeds `baseline_ms`, and falls back to the general instruction-refresh directive when none changed or `cwd`/`baseline_ms` is absent.
  - `send_service_resume_notice_if_unnotified` enqueues one durable `"Host restarted"` system mail when a Claude session resumes on the same `boot_id` after `boot_reconcile` already ran (or when no `EpochRecord` notified that persona in the current boot), and skips sending a duplicate if `EpochRecord` for the current `boot_id` only just queued a pending reboot notice that has not yet been sent—or records/deduplicates cleanly across reboot vs service-restart boundaries.
- [ ] Implement `format_host_restarted_notice`, update `NOTICE_BODY` + `send_notice` in `crates/hub/src/boot_reconcile.rs`, and expose `send_service_resume_notice` for `lifecycle.rs`.
- [ ] Run `cargo fmt --all --check`, `cargo clippy -p fl-hub --all-targets -- -D warnings`, and `cargo test -p fl-hub boot_reconcile`, then commit with `Co-authored-by: Feira <feira@artificialhumanity.io>`.

---

### Task 3: Lifecycle `--resume`, Exact Fallback Ladder with Notices, `/clear` Sync & Anti-Hijack Identity Isolation

**Files:**
- Modify `crates/hub/src/lifecycle.rs`
- Modify `crates/cli/tests/host_lifecycle.rs`

**Steps:**
- [ ] Write failing lifecycle and black-box tests:
  - **Happy-path resume (`restart_hosted_session` and `spin_up` on stopped record):** Confirmed `claude:ada` with active `<sid>.jsonl` under cap spawns via `prepare_resume` (`--remote-control --permission-mode auto --resume <sid> --name Ada`, no `--session-id`, no positional hydration prompt), preserves `vendor_session_id`, `hydration_generation` (`G1`), `HydrationStatus::Confirmed`, and `hydration-ack-<G1>`, and queues `"Host restarted"` instruction-refresh mail (including modified `WORKFLOW.md` when touched after `hydration-ack-<G1>.accepted_at_ms`).
  - **SIGKILL / mid-tool-call transcript resume:** Active `<sid>.jsonl` containing `{"type":"bridge-session","bridgeSessionId":"cse_..."}` + dangling `tool_use` + torn trailing fragment repairs only the torn fragment, keeps the `tool_use` + `bridge-session` lines intact, and resumes `<sid>`.
  - **Complete fallback ladder + per-reason lifecycle notices:**
    1. `HydrationStatus::Unconfirmed` -> fresh UUIDv7 + new `G2` + `prepare_launch` + warning containing `unconfirmed_hydration`.
    2. Missing or `.claude/archived/`-only `.jsonl` -> fresh spawn + warning containing `missing_or_archived_transcript`.
    3. Empty / 0-turn `.jsonl` -> fresh spawn + warning containing `empty_or_unparseable_transcript`.
    4. Active tokens over model-relative cap (`388,396` tokens on `claude-opus-4-1` `200k` window, or `895,176` tokens on `claude-opus-5-5` `1M` window) -> fresh spawn + warning containing `context_over_cap`.
    5. Staged recast assignment (`project` or `role` changed) -> fresh spawn + warning containing `recast_guidance_staged`.
    6. `--resume` process exits within `launch_survived` -> kills dead unit, mints fresh UUIDv7 + `G2`, prunes `hydration-ack-<G1>`, retries once with `prepare_launch`, and emits warning containing `resume_launch_did_not_survive`.
  - **`/clear` route sync:** Live `VendorRoute::Claude { session_id: new_sid, .. }` with active `<new_sid>.jsonl` updates `record.vendor_session_id` before respawn so `--resume` targets `new_sid` rather than pre-`/clear` `old_sid` (ignoring tombstoned IDs).
  - **Child identity-env anti-hijack fixture (`#460` / Ruling #6):** With foreign `AGENT_PERSONA=feira`, `AGENT_NAME=feira`, `FL_HOSTED_ID=antigravity:feira`, and `AGENT_HYDRATION_GENERATION=<foreign_gen>` present in the parent environment, `HostRuntime::spawn` / `resume_spec_for_with_session` produces a `SpawnSpec` for `claude:ada` whose `env` sets `AGENT_PERSONA=ada`, `AGENT_HYDRATION_GENERATION=<ada_gen>`, and `FL_HOSTED_ID=claude:ada` and whose `env_allow` excludes `AGENT_NAME` (and `TmuxRuntime::spawn` passes `-u AGENT_NAME -u AGENT_PERSONA -u AGENT_HYDRATION_GENERATION -u FL_HOSTED_ID`); invoking `fl wire hook claude` (`SessionStart:resume`) under that exact child environment registers **only** `ada` and leaves `feira`'s existing route untouched.
- [ ] Implement child identity-env isolation in `HostRuntime::spawn` and `TmuxRuntime::spawn`, `resume_spec_for_with_session`, `/clear` `vendor_session_id` sync, and the shared `evaluate_claude_resume_eligibility` + `!launch_survived` fallback ladder with lifecycle notices in `restart_hosted_session` and `spin_up`.
- [ ] Run `cargo fmt --all --check`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test -p fl-hub`, and `cargo test -p fl-cli --test host_lifecycle`, then commit with `Co-authored-by: Feira <feira@artificialhumanity.io>`.

---

### Final Verification & Pull Request

- [ ] Run full workspace verification trio (`cargo fmt --all --check`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`) with scrubbed agent identity environment variables.
- [ ] Run whole-branch `code-review` pass against the spec and plan.
- [ ] Push `feira/claude-resume-respawn-433` (carrying both spec and plan + implementation), open PR via `fl github pr create --project ferroloop --by feira` (leaving auto-merge off), and request `ferris`'s review over FerroWire.
