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

- [x] Add failing tests: 64 default projections admitted and release to baseline;
      65th refused; successful cleanup drops projection; incomplete cleanup retains
      it; output can still update 64 KiB raw tail after release; Ack unchanged.
- [x] Observe RED. Implement derived global projection/additional limits while
      preserving finite per-epoch/category admission and overflow checking.
- [x] Release projection on successful cleanup outside the cleanup lock. Serialize
      with output/resize through the existing projection mutex, and make completed
      rendered access refuse explicitly without recreating the parser.
- [x] Add a real isolated-daemon test for at least 16 simultaneous default PTYs,
      synthetic gated children and cleanup; update tests that intentionally capture
      a completed screen to assert the new raw-tail-only policy.
- [x] Run `cargo test -p fl-host` and affected strict Clippy/fmt/diff checks. Document
      accounting reservation versus actual allocation and new completed capture rule.
- [x] Commit with Fiona trailer.
- [ ] Fresh independent task review before Task 2.

Implementation notes: RED reproduced projection admission failure at zero-based
session 14, retained projection charge after successful cleanup, and refusal of
the isolated sixteen-PTY fixture. The fixture uses existing gated synthetic
children; completed raw capture is asserted before same-name replacement while
live rendered assertions remain unchanged. Existing projection exhaustion tests
now use epoch budgets to preserve their original 128 MiB constraint. A narrow
`input.rs` resize error mapping preserves `SessionFenced` if cleanup releases the
projection between the writer's completion check and resize. This is the only
source-file scope addition. The first host-suite run caught mixed manual/typed
request IDs in the new fixture; it now uses typed IDs consistently.
Final validation: `cargo test -p fl-host` passed 229 tests (98 unit, 30 foundation,
84 IPC, 17 reliable events), strict host all-target Clippy, workspace formatting
and diff checks passed. Workspace verification remains scheduled after Task 2.


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


### Task 2 restart checkpoint (2026-11-09, unfinished)

Implementation was paused at the coordinator's request for an imminent host restart.
Current WIP adds the separate PTY completion fence, bounded eight-tail retention,
64-identity eviction history, capability-32 diagnostic query, typed client failure
diagnostics, and CLI human/JSON reporting. Completion order is recorded at successful
cleanup using a monotonic sequence; inventory maintenance sorts by that order.
The authority fence detaches pending tickets and final-write state before dropping
or settling them. Observers also consult the journal completion fence because they
do not occupy the controller's active slot. Journal compaction preserves Ack and
actual externally held reservation ownership.

Observed RED: no-Ack maintenance retained 116 ReliableData bytes rather than zero;
new diagnostic JSON was initially rejected as an unknown variant; CLI initially
omitted diagnostics. Initial compaction test and host/proto unit suites passed
(99 host + 21 proto) before later race/compatibility additions. CLI host_status's
real-daemon and old-host tests now both pass. The subsequent retention-focused unit
run passed 10 tests but failed the held-reference assertion: its 97-byte charge
includes the wire envelope, not only the 10-byte payload. The fixture now compares
against its actual held reservation baseline, but has not been rerun.

The new 74-epoch private-daemon fixture is still failing at epoch zero. Its first
version incorrectly used the existing `lines` child mode, which deliberately parks;
that was replaced with `stdout-fragments lines <private marker> eof` for a complete
64-KiB tail and `exit 0` for later epochs. The latest run still times out at epoch
zero and needs evidence from state/cleanup/EOF/thread progress before another fix.
It no longer polls Observe, so the sequential workload creates no subscribers or
Ack. Client capability/uncertain-capacity tests were added after the last proto test
run and remain unverified.

Resume: diagnose the epoch-zero fixture, rerun deterministic held-reference and
final-write tests, check existing PTY integration expectations after bounded status
maintenance (including the sixteen-live fixture), complete public documentation and
remaining coverage, then run focused checks and the full workspace test/Clippy/fmt
trio. Full workspace verification and independent Task 2 review have NOT run.
Logs: the approved artifact directory contains `408-task2-{red,green,proto-red,unit,
retention,ipc,cli-red,cli-green}.log`. No live-host launch or configuration change
was performed. This checkpoint is not an approval or completion claim.
