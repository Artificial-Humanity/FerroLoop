# PTY retention implementation plan

> **For agentic workers:** Use repository-required `subagent-dev`: a fresh implementer per task, then `code-review`, followed by a final whole-branch review.

**Goal:** Admit default PTYs through the documented session limit and bound completed PTY retention without consumer acknowledgments.

**Architecture:** Release projection on proven cleanup; budget default capacity from the existing storage bound. Compact completed PTYs behind the existing authority fence, retain eight raw tails, and expose bounded eviction diagnostics through an opt-in extension.

**Tech stack:** Rust, existing reservations/Arc ownership, host protocol and private daemon fixtures.

**Spec:** `docs/superpowers/specs/2026-11-09-pty-retention-design.md`

## Global constraints

No live host actions, config writes, dependencies, UI or #383/#376 edits.
ReliablePipe/LegacyPipe acknowledgment behavior is unchanged. Never fake Ack or
release a charge while its actual buffer is retained. Eight completed PTYs, 64 KiB
raw tail each, 64 recent eviction identities, cumulative count. Strict existing
wire shapes stay unchanged; new diagnostic messages require capability discovery.

## Review focus

- Cleanup may precede output EOF: raw tail still drains; parser cannot reappear.
- An unresolved cleanup or live/pipe epoch never qualifies for forced retirement.
- Held references and cancel callbacks cannot corrupt accounting or deadlock.
- Old clients/hosts never receive or are sent unsupported diagnostic frames.
- Completed retention cannot consume the last slot needed by an admissible live PTY.

### Task 1: Default projection capacity and cleanup release

**Files:** `crates/host/src/{projection,slice_budget,session}.rs`; focused inline
tests and `crates/host/tests/host_ipc.rs`; rendered/replay docs where behavior changes.

**Interfaces:** Internal `projection::default_storage_bound() -> usize` derives
from `PtyOptions::default()`. Internal `slice_budget::projection_limit() -> usize`
uses that bound times DEFAULT_MAX_SESSIONS; `additional_limit() -> usize` adds the
old non-projection headroom. `Session::note_cleanup` is the existing cleanup entry.
Keep public interfaces unchanged in this task. Task 2 consumes these limit helpers
and `SliceBudget::used(ChargeKind::Projection)` for diagnostics.

- [ ] Add failing tests: 64 default projections admitted and release to baseline;
      65th refused; successful cleanup drops projection; incomplete cleanup retains
      it; output can still update 64 KiB raw tail after release; Ack unchanged.
- [ ] Observe RED. Implement derived global projection/additional limits while
      preserving finite per-epoch/category admission and overflow checking.
- [ ] Release projection on successful cleanup outside the cleanup lock. Serialize
      with output/resize through the existing projection mutex, and make completed
      rendered access refuse explicitly without recreating the parser.
- [ ] Add a real isolated-daemon test for at least 16 simultaneous default PTYs,
      synthetic gated children and cleanup; update tests that intentionally capture
      a completed screen to assert the new raw-tail-only policy.
- [ ] Run `cargo test -p fl-host` and affected strict Clippy/fmt/diff checks. Document
      accounting reservation versus actual allocation and new completed capture rule.
- [ ] Commit with Fiona trailer; fresh independent task review before Task 2.

### Task 2: Completed retention, diagnostics and operator errors

**Files:** Prefer `crates/host/src/retention.rs` for bounded retention bookkeeping;
integrate `lib.rs`, `session.rs`, `output.rs`, `authority.rs`, `reliable.rs`,
`daemon.rs`. Add negotiated diagnostics to `crates/host-proto/src/{extension,client}.rs`
and public exports as needed. CLI `crates/cli/src/cmd/host.rs`; host/CLI integration
tests; `docs/host-replay-pty.md` and `docs/host-rendered-capture.md`.

**Interfaces:** New `PTY_RETENTION_FEATURE = 32`, `ExtensionRequest::PtyRetentionStatus`
and corresponding response carrying `PtyRetentionStatus`. Public status fields:
`projection_reserved_bytes: u64`, `projection_limit_bytes: u64`,
`completed_pty_sessions: u32`, `completed_pty_limit: u32`, `evicted_total: u64`,
`recent_evictions: Vec<StreamId>` bounded to 64 on decode/admission. Async/blocking
`pty_retention_status() -> Result<Option<PtyRetentionStatus>, HostError>` capability
gates the query. `None` means older host; no new fields on HostStatus/SessionSummary.
Internal exact function signatures may follow existing authority patterns, keeping
the spec's lock and reference ordering. Do not invent a new on-wire refusal enum.

- [ ] Add failing deterministic tests for PTY compaction without Ack, preserved
      reference charges, cleanup/EOF/thread eligibility, oldest-first eight-tail
      limit and pressure eviction below the 64-slot admission limit.
- [ ] Implement journal-owned compaction after cleanup/EOF with no fabricated Ack;
      fence/cancel completed authority, detach re-entrant objects under lock and
      release them afterward. Preserve held references and reliable-pipe tests.
- [ ] Enforce completed retention during existing spawn/status maintenance; log
      each exact evicted StreamId and record bounded recent history/cumulative count.
      Preserve raw capture for retained completed sessions independently of authority.
- [ ] Add bounded capability/query/response and async/blocking client helper. New
      clients enrich stale responses for known evictions into explicit identity-named
      errors via one bounded diagnostic lookup, preserving stale/unknown behavior
      outside the retained history. Avoid recursive diagnostic requests.
- [ ] Render diagnostics in `fl host status` human/JSON output, explicitly unavailable
      on older hosts. Enrich PTY capacity errors with limit/use and actionable remedy;
      do not transform unrelated spawn failures into misleading projection errors.
- [ ] Add isolated-daemon tests for over64 sequential PTYs without consumers/Acks;
      final tail bytes, baseline projection charge, bounded inventory, eviction
      order/count/identity, late-reader errors and stale same-name epochs. Include
      held stream/ticket races and old-client/old-host decode fixtures.
- [ ] Update public docs with PTY-specific retention, eight-tail/64-history limits,
      capability bit, request/response, accounting/RSS distinction and CLI remedies.
- [ ] Run focused checks, then `cargo test --workspace`,
      `cargo clippy --workspace --all-targets -- -D warnings`,
      `cargo fmt --all --check`, `git diff --check`. Commit with Fiona trailer.
- [ ] Fresh task review/fix loop, final whole-branch review, current-main integration,
      ready PR via `fl github pr create`, Ferris current-head approval and CI, then
      author merge. Ferris/Jarvis own deployment; no live host changes here.
