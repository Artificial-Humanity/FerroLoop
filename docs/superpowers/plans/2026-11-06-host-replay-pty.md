# Host reliable events and PTY Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add explicit reliable pipe Ack/replay, then bounded host-owned PTY input,
resize and synchronized attach, without enabling a live vendor or migration.

**Architecture:** Protocol 1.1 explicitly opts into new immutable session modes;
legacy 1.0 stays unchanged. An epoch-owned authority/journal survives writer
retirement and controller disconnect. PTY display projection and its byte cursor
remain separate from reliable lifecycle events and vendor receipts.

**Tech Stack:** Existing Rust host/proto, Tokio, serde, platform libc/windows-sys;
workspace base64 0.22 for bounded extension payloads. PTY/renderer dependency selection
is blocked on Task 0's source-feasibility gate; no version is guessed here.

**Spec:** [2026-11-06-host-replay-pty-addendum.md](../specs/2026-11-06-host-replay-pty-addendum.md).

**Status:** UNEXECUTED. Owner accepted surviving-host recovery through Ferris
`01a1b34b-aaad`. Lead `01a1b39c-3b8a` accepted protocol 1.1 opt-in, documented
numeric/default/100-ms engineering targets and the recovery scope, subject to
measured feasibility gates. Product execution/publication still needs explicit
Lead disposition; Task 0 backend/schema eligibility remains a gate. No command
below is permission to run it now. All source signatures
describe proposed code, not existing exports. Base `44e1bdfc`; archive unchanged.

## Global Constraints

- Accepted recovery: preserve agents across controller/hub restart while host lives;
  host crash is interrupted/uncertain, no journal/adoption/automatic-respawn guarantee.
- Reliable pipe first, PTY second. No controller/store/service/installer/vendor
  implementation or present messaging-policy changes in this host plan.
- Keep host/proto standalone: no hub/wire/store/exec/CLI dependency and no PTY library
  in proto/hub/wire. No vendor parser, inference, permission bypass or live probes.
- Existing 64 retained epochs/ownership slots and 16 total subscribers; 256-KiB
  frames, 1-MiB logical pipe line plus 4-KiB envelope, 64 fragments maximum.
- Reliable data 8 MiB / 4,096 events per epoch; lifecycle 16 KiB / 16 records per
  epoch; aggregate data 512 MiB and lifecycle 1 MiB. No unacked/ownership eviction.
- PTY maximum 8 epochs, 1-MiB/4,096-chunk ring each, 16-MiB projection/parser each,
  131,072 total cells, parser temporary strings 64 KiB inside projection budget.
- Snapshot one per PTY and two globally, 16 MiB each including source copy/state/row;
  viewer 256-KiB/64-item pending plus at most one 256-KiB frame in flight.
- Additional slice aggregate 192 MiB; category maxima total 185 MiB including 8 MiB
  metadata, leaving 7 MiB unallocated. Logical charged bytes are not total RSS.
- PTY default 120x40 with 512 scrollback rows; sizes 1..1000 columns / 1..400 rows
  still require cell/memory admission. Reject incompatible geometry before effects.
- Snapshot safe-boundary wait/admission/capture share one nonrenewing 100-ms budget;
  no quiescent whole-chunk boundary means SnapshotNotReady, never guessed state.
- New input keeps 64 operations / 256-KiB queue and 5-second actual-write timeout;
  permanent Fenced state, exact epoch/connection lease and no ambiguous resend.
- Ticket lifetime 5 seconds; one outstanding per controller. Legacy authentication,
  5-second handshake/write and 10-second started-frame deadlines remain unchanged.
- Fresh exclusive implementer per task, fresh read-only review, fresh fixer/scoped
  re-review for accepted findings, machine author with Felicia co-author trailer.
  Use project's `subagent-dev` orchestration and `code-review` loop at execution.

## Review Focus

- Same-name historical epoch after latest writer retirement must still replay/Ack
  without granting input to the successor: Tasks 2, 3 and 4.
- Commit succeeds but Ack/reply is lost; late receipts cannot cause duplicate input
  or release an unrelated epoch: Tasks 3 and 4; vendor acceptance stays downstream.
- Resize/snapshot can encounter partial CSI/OSC/UTF-8 and existing wrap/charset state;
  no formatted-screen shortcut may claim synchronization: Tasks 0, 7 and 8.
- Cancel a ticket/snapshot after admission while references remain parked; capacity
  cannot be reused until actual retirement: Tasks 2, 3, 7 and 8.
- Host crash, stale readiness and saved task labels must not trigger adoption or
  duplicate spawn: Task 4 refusal tests, downstream dependency gates in section 4.

## 1. Files and shared interfaces

New small modules own new behavior; do not split/reformat unrelated foundation code.

| Files | Responsibility |
| --- | --- |
| `crates/host-proto/src/extension.rs` (new) | Versioned request/response DTOs, strict identity/byte types and bounded extension stream codec |
| `host-proto/src/{lib,types,frame,client}.rs` | Export/gate extension, legacy wrappers, explicit new client methods |
| `crates/host-proto/src/transport/mod.rs` | Retain authenticated negotiated minor in HostConn on Unix/Windows admission; gate RPC and event codecs on each actual connection |
| `crates/host/src/reliable.rs` (new) | Ack journal, captured-watermark replay and retained-reference charges |
| `crates/host/src/authority.rs` (new) | Per-epoch controller, ticket, connection revocation/takeover |
| `crates/host/src/slice_budget.rs` (new) | Atomic category/aggregate reservations and RAII charges |
| `crates/host/src/pty/{mod,unix,windows,projection}.rs` (new) | Platform PTY owner, bounded renderer adapter, snapshots/ring |
| `host/src/{lib,daemon,input,output,session,protocol,supervisor}.rs`, `host/src/child/{unix,windows}.rs` | Narrow integration with existing spawn gate, writer, supervisor/Job and pump retirement |
| `host-proto/tests/{frame,transport}.rs`, `host/tests/{host_ipc,containment}.rs` | Preserve existing tests; add negotiated compatibility/containment cases |
| `crates/host/tests/{reliable_events,pty_ipc}.rs` (new) | Native black-box synthetic-child contracts |
| `crates/host/src/bin/fl-host-testchild.rs` | Deterministic raw-output/PTY/barrier fixtures; no real vendor |
| `docs/host-replay-pty.md` (new at execution), `docs/host-foundation.md` | New-mode user contract and link; preserve historical foundation limitations |

Paths abbreviated with `host/` or `host-proto/` above are under `crates/`.
Source-only evidence artifacts belong under `docs/superpowers/evidence/` when the
corresponding evidence task is authorized; none are claimed to exist yet.

### Protocol contract frozen for review

`extension.rs` defines strict serde types (snake_case tags). `HostInstance([u8;16])`
and `Ticket([u8;32])` serialize as fixed-length lowercase hex, reject wrong length/
alphabet before use and redact Debug. `StreamId { host: HostInstance, session:
SessionId, epoch: SessionEpoch }`, `Cursor { stream: StreamId, seq: u64 }`,
`ConnId(u64)` and `SnapshotId(u64)` never alias a name alone. Never use a client-
supplied ID to look up another epoch or manufacture an authority record.

Add `HostRequest::Extended { id: RequestId, request: ExtensionRequest }` and
matching `HostResponse::Extended { id, response: ExtensionResponse }`. Their outer
wire tag is `extended`, supported only at negotiated minor >=1. Do not modify
existing variant fields or auth transcript. Add `encode_request_for_minor(minor:
u16, request: &HostRequest) -> Result<Frame, HostError>` and equivalent
`decode_request_for_minor`, `encode_response_for_minor` and
`decode_response_for_minor`; existing encode/decode wrappers retain minor-zero
semantics. In `transport/mod.rs`, add private `HostConn.negotiated_minor: u16` and
`HostConn::negotiated_minor(&self) -> u16`. Both Unix and Windows `admit` branches
retain the authenticated agreement returned by `auth::server_with_token` rather
than discard `_agreed`; every HostConn constructor/test fixture sets that value.
`recv` and `respond` use their connection's minor-aware request/response codecs.
`write_frame` must also reject ExtensionEvent/frame9 below minor 1; the extension
stream writer additionally requires successful Opened on that connection. Client
RPC and event response/frame decoders retain and gate on their own authenticated
connection minor, including a separately connected event socket. The daemon reads
the accessor; no peer payload, cached other-connection minor or unconditional 1 can
override negotiated permission. Authentication and 1.0 wire shapes stay unchanged.

`ExtensionRequest` variants (all request IDs remain in the outer envelope):

| Tag and fields | Response contract |
| --- | --- |
| `capabilities` | `Capabilities { host, features: u64, limits: ExtensionLimits }`; reliable bit 1, PTY bit 2, backend-dependent truth |
| `list_sessions` | `Sessions { records: Vec<SessionSummary> }`, at most 64 records; summary is exact stream, mode, existing ChildState, retained Ack/latest, input state |
| `spawn_session { spec: SpawnSpec, mode: SessionMode, pty: Option<PtyOptions> }` | `Session { stream, state: ChildState }`; mode LegacyPipe/ReliablePipe/Pty, reject contradictory kind/options |
| `claim_controller { stream }` | `Controller { lease: ControllerLease, acked: u64, latest: u64 }` |
| `subscribe_reliable { stream, lease, after_seq: Option<u64> }` | `Subscription { ticket, acked, oldest, latest, watermark }`, captures/registers cursor before issuing ticket; Busy without mutation if this epoch/lease already has an active authoritative stream |
| `observe { stream, after_seq: Option<u64> }` | Same ticketed read-only subscription, no durable recovery Ack baseline or mutation authority |
| `open_stream { ticket }` | `Opened`, then connection becomes extension stream; ticket consumption checks owning RPC is still active |
| `ack { cursor, lease }` | `Acked { through: u64 }`, including valid-owner duplicate Ack |
| `send_line { stream, lease, line: String }` | `Input { outcome: SendOutcome }`, ReliablePipe only |
| `signal_session { stream, lease, signal: SignalKind }` | `Controlled`, same foundation supported-signal semantics |
| `kill_session { stream, lease }` | `Controlled`, preserves unread records and cleanup proof |
| `write { stream, lease, bytes: RawInput }` | `Input { outcome: SendOutcome }`, PTY only; RawInput base64, decoded <=64 KiB and serialized frame within limit |
| `resize { stream, lease, cols: u16, rows: u16 }` | `Resized { cols, rows, projection_revision: u64 }` after admitted OS operation |
| `attach { stream, resume: Option<ProjectionResume> }` | `Attachment { ticket, projection_revision, admission: AttachmentAdmission }`; PendingSnapshot or ResumePending only, never a synchronization claim; viewer has no mutation authority |

`PtyOptions { cols:u16, rows:u16, scrollback_rows:u16 }` uses spec defaults.
`ProjectionResume { output_seq:u64, revision:u64, schema:u16, exact_state:bool }`: true is a
client assertion that the same terminal/parser state is retained, not proof checked
by the host; false forces snapshot. A fresh viewer must not set it true from a
stored number alone. Viewport geometry stays client-local; only the current
controller Resize changes canonical PTY geometry. An observer starting below retired
Ack gets CursorExpired;
it cannot silently substitute a current snapshot for semantic events.
`Coverage { screen_complete:bool, state_complete:bool, scrollback_rows:u16,
history_prefix_lost:bool }` explicitly reports completeness; a synchronized
snapshot requires both complete booleans true. Missing history never means lossless
protocol replay. Client terminal state is not recovered merely by storing Coverage.

Failures are `ExtensionResponse::Refused { code: ExtensionRefusal }`: Unsupported,
WrongMode, WrongInstance, StaleEpoch, StaleLease, WrongConnection, CursorExpired,
FutureCursor, TicketInvalid, ResourceLimit, SessionFenced, SnapshotNotReady,
ProjectionUnavailable, ResyncRequired, UnsupportedSchema, Busy and Io. `Input`
retains actual write outcomes; a refusal does not authorize fallback or resend. Backend partial
Resize failure returns Io and invalidates synchronized projection until its
geometry/state is re-established; never report requested geometry as observed.

Add `FrameKind::ExtensionEvent = 9`, emitted/accepted only on an explicitly opened
minor-1 stream. Its JSON payload is `ExtensionFrame` tagged below; base64 decoding
is length-prechecked before allocation. Serialized envelope is <=4 KiB excluding
encoded bytes; every frame remains <=256 KiB. No generic unbounded serde Vec decode.

- `ReliablePart { seq:u64, kind:Line|Lifecycle, more:bool, bytes:BoundedBytes }`:
  decoded part <=32 KiB, logical event <=1 MiB +4 KiB, <=64 parts. Lifecycle bytes
  encode the existing authoritative ControlEvent subset, <=4 KiB. Connection
  diagnostics never occupy a reliable seq. Exact stream identity was ticket-bound.
- `PtyData { output_seq:u64, revision:u64, bytes:BoundedBytes }`: <=32-KiB raw chunk.
- `ResumeReady { output_seq:u64, revision:u64, schema:u16 }`: stream-opening
  confirmation for ResumePending only. Revalidate ticket-bound epoch/revision,
  supported schema and continuous ring suffix at consumption; emit before any data.
  The client may mark synchronized only after matching this confirmation to its
  retained exact parser/state at output_seq, never from the RPC admission alone.
- `SnapshotBegin { id, through:u64, revision, schema:u16, cols, rows, coverage:Coverage }`,
  `SnapshotPart { id, kind:State|Row, index:u32, last:bool, bytes:BoundedBytes }`,
  `SnapshotEnd { id }`: raw part <=128 KiB, each complete row one frame; state may
  use bounded parts. Client counts all parts/state/rows within 16 MiB before decode.
  State/Row encoding is the portable versioned schema approved in Task 0, never a
  backend-private memory/serde dump. Unknown schema refuses before reconstruction.
  Task 1 may implement reliable framing independently but must leave these PTY
  payload variants unavailable until that schema disposition; Task 7 consumes its
  exact schema and canonical reconstruction/codec fixtures.
- `Notice { code: ExtensionRefusal }`: explicit incomplete/resync/terminal status,
  never a reliable cursor. Do not claim SnapshotEnd until all state/rows sent and
  the pinned suffix remains covered.

`AttachmentAdmission` has exactly PendingSnapshot and ResumePending. Neither means
that the viewer is synchronized. PendingSnapshot becomes synchronized only after
the client validates matching snapshot ID/schema/revision/W, complete ordered state
and rows, both Coverage completeness flags, then SnapshotEnd with a continuous
post-W suffix. Until then it stages bounded state without publishing an exact view.
ResumePending requires ResumeReady and the matching retained exact state described
above; if that state is absent, refuse and request a snapshot explicitly. A gap,
revision change, cancellation, malformed/unknown-schema frame or disconnect before
completion discards staged state and leaves synchronization false. After either
completion path, later gaps/revision changes invalidate synchronization again. No
failed/incomplete path silently falls back to an exact-state claim.

### Host internal interfaces

Definitions live in the named modules; imports use these exact names/signatures.
`ExtensionRefusal` is the shared error. Existing `HostError` remains for IO/transport.

```rust
// slice_budget.rs
enum ChargeKind { ReliableData, Lifecycle, Projection, Ring, Snapshot, Viewer, Metadata }
struct SliceBudget; struct Reservation; // RAII, move-only, releases on final owner
fn reserve(budget: &Arc<SliceBudget>, kind: ChargeKind, bytes: usize)
    -> Result<Reservation, ExtensionRefusal>;
// reliable.rs
struct ReliableJournal; struct ReplayCursor; struct ReliableRecord;
struct ChargedBytes; // owns Reservation and pre-reserved bounded byte allocation
enum ReliableBody { Line(ChargedBytes), Lifecycle(ControlEvent, Reservation) }
fn append(j: &ReliableJournal, event: ReliableBody)
    -> Result<u64, ExtensionRefusal>;
fn begin_replay(j: &ReliableJournal, after: u64)
    -> Result<(u64, ReplayCursor), ExtensionRefusal>; // W plus registered continuation
fn acknowledge(j: &ReliableJournal, through: u64) -> Result<(), ExtensionRefusal>;
// authority.rs; ControllerGrant and TicketGrant mirror Controller/Subscription responses
struct AuthorityRegistry; struct ControllerGrant; struct TicketGrant;
async fn claim(reg: &AuthorityRegistry, conn: ConnId, stream: &StreamId)
    -> Result<ControllerGrant, ExtensionRefusal>;
fn issue_ticket(reg: &AuthorityRegistry, conn: ConnId, cursor: Cursor,
    lease: Option<ControllerLease>, observer: bool, now: Instant)
    -> Result<TicketGrant, ExtensionRefusal>;
fn consume_ticket(reg: &AuthorityRegistry, event_conn: ConnId, ticket: Ticket, now: Instant)
    -> Result<ReplayCursor, ExtensionRefusal>;
fn revoke_connection(reg: &AuthorityRegistry, conn: ConnId);
// pty/mod.rs: implementation-specific owned handles stay private to platform modules
struct PtyOwner; struct PtySize { cols: u16, rows: u16 }
fn resize(owner: &PtyOwner, size: PtySize) -> Result<(), HostError>;
// pty/projection.rs: chosen backend must implement this bounded adapter
struct Projection; struct Snapshot; struct ProjectionResume; // last is proto DTO re-export
fn feed(p: &mut Projection, output_seq: u64, bytes: &[u8]) -> Result<(), ExtensionRefusal>;
fn capture(p: &Projection, deadline: Instant) -> Result<Snapshot, ExtensionRefusal>;
```

`Reservation::allocate_bytes(self, len: usize) -> Result<ChargedBytes,
ExtensionRefusal>` is the only constructor for production read buffers; validate
the reservation covers len before allocation, and expose mutable slices for bounded
reads. ChargedBytes retains that charge in every journal/ref owner. `append` only
takes already-reserved input; no allocation-before-charge path is allowed.
Journal acknowledgement is called only after registry validates the
current owning connection/lease, scope and the sole authoritative stream's
fully emitted ceiling or validated durable recovery baseline (never observer
progress). Exactly one authoritative event stream may be active per retained
epoch/lease. A second SubscribeReliable is Busy without minting a ticket or changing
that stream's cursor/ceiling; other readers use Observe. An unconsumed outstanding
ticket may still be replaced, invalidating its predecessor. After disconnect or
revocation, replacement waits for old stream authority to settle; actual parked
references remain charged until retirement. Keep a per-lease Ack ceiling: start
at host Ack, raise only by its validated recovery cursor or fully emitted complete
events on its sole authoritative stream. Event close retains that ceiling for
Ack-only retry; a settled replacement on the same lease carries it forward.
Takeover starts a new lease at max(host Ack, its own validated recovery cursor);
old-lease emissions and all observer progress cannot raise the new ceiling. Registry
claim awaits the actual writer fence without a global lock, then revalidates stream,
connection and claim generation before publishing ownership. ReplayCursor retains
the charged shared records; cancelling it does not prematurely release those bytes.

## 2. Task sequence and gates

### Task 0: Source eligibility and early Claude evidence (parallel read-only lane)

**Files:** Create `docs/superpowers/evidence/2026-11-06-host-backend-eligibility.md`
and `docs/superpowers/evidence/2026-11-06-claude-stream-interface.md` only after
evidence work is assigned. Consume the spec, current local fixtures and installed
launcher metadata; produce Eligibility { eligible|blocked, exact sources, gaps }
and a reviewable portable snapshot-schema/codec-fixture contract in the backend
evidence document. This is a design deliverable, not permission for product edits.

- [ ] Record baseline/machine/platform and searched source provenance without
  launching a vendor. Keep recorded 2.1.317 help versus observed 2.1.319 target
  distinct; hash/metadata alone are not a runtime version/conformance proof.
- [ ] For PTY/renderer candidates, inspect local exact source/license or retrieve
  relevant public static source/docs under the existing research authorization;
  no renewed permission is needed merely for that retrieval. Pin source URL,
  commit/version, digest and license; this does not authorize install/update, auth,
  vendor/model invocation or spend. Produce a table proving
  bounded cell/glyph/auxiliary/parser storage, complete continuation-state export,
  quiescence detection and snapshot copy+row <=16 MiB. List every allocation path
  and unsupported sequence behavior. Reject a candidate with unbounded hidden heap.
- [ ] Specify a versioned portable State/Row wire schema and client reconstruction
  contract: registered u16 schema ID, byte encoding, complete continuation fields,
  cell/glyph representation, bounded counts/lengths, canonical row/state order,
  duplicate/missing-part rejection and unknown-version refusal. Include bounded
  canonical encode/decode/reconstruction fixtures, including alternate screens and
  continuation state; reject unversioned backend-private dumps. Obtain Lead schema
  disposition or return blocked before any PTY codec/dependency/product edits.
  Do not guess the renderer representation before eligibility source is available.
- [ ] Inspect platform backend source for slave/session/controlling-terminal setup,
  Windows ConPTY + creation-time Job attribute combination and cancellation/drain.
  Do not accept a wrapper that spawns before containment. Pin dependency and license
  only after evidence supports it; no dependency/code edit in this task.
- [ ] In parallel, inventory authoritative local or retrieved public static Claude
  SDK schemas/docs with pinned provenance for init,
  framing, IDs, interrupts, approvals, EOF, persistence and exact-UUID resume. Record
  absent evidence rather than invent fields. Do not run --help/--version, a model,
  update/install/auth/doctor or contact a live session under this plan's docs scope.
- [ ] Verification: peer reads each claim against the exact cited file/version.
  `git diff --check` must exit 0; evidence explicitly says source-only/not executed.
- [ ] Commit docs with Felicia trailer; fresh source review. **PTY Tasks 5–8 require
  eligible backend and portable-schema disposition; missing renderer/schema evidence
  is a real stop, not a pass.**
  Reliable Tasks 1–4 can proceed after their separate execution GO. Claude evidence
  gaps go to Lead early and do not block the synthetic reliable-host milestone.

### Task 1: Negotiated protocol and bounded codec

**Files:** Create proto `extension.rs`; modify proto lib/types/frame/client,
`transport/mod.rs` and Cargo.toml for existing workspace base64 only; test
`tests/frame.rs`, `tests/transport.rs` and unit codec.
**Consumes:** section 1 DTOs. **Produces:** encode/decode-for-minor,
`HostConn::negotiated_minor`, authenticated transport gating and extension types;
feature report bits remain false until the corresponding host task is ready.
Reliable DTO/framing can proceed independently; PTY State/Row payload codecs and
schema acceptance require Task 0 disposition, otherwise they remain unavailable.

- [ ] Write `extension_requires_minor_one`, `legacy_wire_shapes_unchanged`,
  `extension_base64_preflight_bounds` and `extension_reassembly_requires_complete`.
  Assert minor 0 rejects Extended and frame9, frame1..8 encodings unchanged, forged
  overlength hex/base64/part counts reject before heap growth, incomplete parts yield
  no logical event, and wrong stream/seq/more ordering refuses. Use a 1-MiB line,
  a >1-MiB line, >64 parts and malicious huge declared length.
- [ ] Add actual authenticated socket tests
  `authenticated_minor_zero_rpc_rejects_raw_extension`,
  `authenticated_minor_zero_event_socket_rejects_frame9` and
  `authenticated_minor_one_rpc_and_event_stream_work`. Negotiate independently on
  RPC/event sockets; inject raw Extended requests/frame9 outside normal client
  helpers, including a forged payload minor and an event socket downgraded to0
  while RPC is1. Assert refusal before request effects/stream admission, no frame9
  emitted to minor0, unchanged valid legacy traffic, and valid minor1 request/
  response plus opened extension event flow. Use scripted authenticated peers for
  transport tests; Task3 repeats end-to-end once authority is wired. Both platform
  admission paths must execute on their native jobs.
- [ ] Run `cargo test -p fl-host-proto --test frame extension_`, named authenticated
  `--test transport` filters and unit filters; record meaningful RED failures.
- [ ] Implement strict DTOs, negotiated codec, explicit `supports_message(1,
  "extended")`, bounded base64/serde visitor and new-frame permission check.
  Preserve minor-zero wrappers; decoder cannot trust a frame to self-declare minor.
  Retain authenticated agreed minor through Unix/Windows `admit` into HostConn;
  gate `recv`, `respond`, raw/extension frame writes and client RPC/event decode
  with that actual connection's minor. Update every constructor/fixture explicitly.
- [ ] Run focused tests GREEN, `cargo test -p fl-host-proto`, then strict proto
  Clippy. Commit and fresh review; no unconditional feature advertisement yet.

### Task 2: Accounted reliable epoch journal

**Files:** Create host `slice_budget.rs`, `reliable.rs`; modify output/session/lib
at mode-specific boundaries, add unit tests in those new modules.
**Consumes:** StreamId, Cursor, Refusal. **Produces:** journal/reservation APIs above;
legacy OutputSession settlement is untouched for legacy epochs.

- [ ] Write `socket_write_does_not_retire_reliable_record`,
  `snapshot_watermark_has_no_gap_or_duplicate`, `capacity_waits_for_last_reference`,
  `reliable_saturation_preserves_lifecycle` and `journal_limits_checked_before_read`.
  Assert data8MiB/4096 and lifecycle16KiB/16, contiguous logical seq independent of
  fragments, future Ack refusal, Ack+last-reference settlement releases once,
  tiny-record/parked-reference count caps, and second epoch makes progress while
  first is stalled. Cover sequence/byte arithmetic overflow without huge allocations.
- [ ] Run `cargo test -p fl-host --lib reliable::` and `slice_budget::`; record RED.
- [ ] Implement RAII atomic category+aggregate admission, append/replay/W capture,
  Ack retirement and lifecycle one-shot finite transition accounting. Reserve before
  pump read; shared buffers charge once, actual copies separately. No I/O under locks.
- [ ] Run filters GREEN and existing host lib tests. Commit and fresh review.
  Keep reliable spawn capability disabled until Task 3's authority is wired.

### Task 3: Authority, tickets and explicit reliable client

**Files:** Create `authority.rs`; modify daemon/input/lib/output plus proto client;
extend `host_ipc.rs` with private deterministic barriers and synthetic child fixtures.
**Consumes:** journal/codec. **Produces:** `claim`, ticket/revoke APIs;
`AsyncHostClient::claim_controller(stream: StreamId)` and
`subscribe_reliable(stream: StreamId, lease: ControllerLease, after: Option<u64>)`
return typed grant / `ReliableEventStream`; its `next_event()` yields complete
events only and never Acks. `ack(cursor: Cursor, lease)` is explicit owning RPC.

- [ ] Write `historical_epoch_ack_survives_writer_retirement`,
  `ticket_bound_to_live_rpc_lease_and_consumed_once`,
  `stale_claim_continuation_cannot_publish_owner`, `ack_lost_reply_is_idempotent`,
  `partial_event_cannot_advance_emitted_ceiling`, `cancelled_ticket_keeps_real_refs`
  `legacy_cannot_access_reliable_epoch`, `second_authoritative_subscribe_is_busy`,
  `closed_stream_replacement_waits_for_authority_settlement`,
  `takeover_revokes_old_stream_ceiling` and `observer_cannot_advance_ack_ceiling`.
  Assert Busy does not mint a ticket, replace cursor/ceiling or disturb active flow;
  pending-ticket replacement before consumption still invalidates its predecessor;
  close/revoke plus replacement preserves charged references and a single
  authoritative emitted/recovery baseline: same-lease close/replacement preserves
  completed-event ceiling for Ack-only retry, while takeover resets to
  max(host Ack, the new lease's validated recovery cursor). Assert wrong instance/epoch/connection,
  superseded/expired5s ticket, duplicate valid Ack, future Ack, revoked owner and
  permanent Fenced outcomes. Hold a claim at its awaited writer fence, disconnect
  or respawn, release it and prove no new-epoch write/authority publication.
- [ ] Run named `cargo test -p fl-host --test host_ipc <name>` filters, record RED.
- [ ] Integrate per-epoch authority and actual-write claim barrier; publish only
  after generation recheck. Subscribe captures W before ticket issue and expires
  its charged cursor after5s. Reserve the subscriber slot on ticket issue; pending
  tickets plus active streams never exceed16. Enforce one active authoritative
  event stream per epoch/lease; second SubscribeReliable returns Busy before any
  mutation. Replacement waits for old stream authority settlement; Observe remains
  separate and cannot change Ack baseline/ceiling. Emit event9 only after Opened on
  the bound minor1 event socket, rechecking its own negotiated permission.
  Observe has no durable recovery baseline/Ack; historical claim cannot reopen input.
- [ ] Enable reliable bit only after code and tests implement the full contract.
  Test 1.0/new and new/1.0 pairing with scripted auth/legacy clients, no fallback.
- [ ] Run GREEN filters, host/proto tests and strict Clippy; commit and fresh review.

### Task 4: Native reliable recovery and first delivery boundary

**Files:** Create `tests/reliable_events.rs`, extend testchild; create
`docs/host-replay-pty.md`. Use existing private fixture/barrier patterns, no generic
production test env authority hook.
**Consumes:** fully wired reliable client. **Produces:** real-host recovery evidence,
first independently reviewable Ack/replay branch checkpoint before PTY changes.

- [ ] Write a synthetic controller harness with an isolated durable cursor file
  and crash points before commit, after commit/before Ack and after host Ack/before
  reply. Application log keyed by stream+seq must contain each event once across
  harness restart; host child PID/epoch stays unchanged. Commit durability here
  tests the harness, not the future FerroStore controller implementation.
- [ ] Add actual socket disconnect inside a 1-MiB fragmented event and concurrent
  publication across W; exact events1..N once after recovery. Hold a controller
  unread to saturation, kill/cleanup/replace same-name epoch, then replay and Ack
  old epoch without losing data or reopening input. Quota refusal precedes spawn.
- [ ] Restart host separately; old instance/cursor/lease must be refused and new
  status explicitly interrupted/uncertain. Preserve foundation survivor cleanup
  safety; do not claim arbitrary child death or automatically launch a replacement.
- [ ] Run new native tests RED/GREEN, existing host/proto tests and strict Clippy.
  Native macOS/Windows must execute the real host after normal PR CI is authorized;
  Linux/cross compilation cannot satisfy those gates. Do not weaken workflows.
- [ ] Commit/docs/fresh review, then whole reliable checkpoint review and Lead
  handback. Any PR/push follows normal source/required-native review gates; this
  plan is not itself permission to publish. PTY code waits on Task0's eligibility.

### Task 5: Unix PTY owner with existing supervisor

**Files:** New pty/mod.rs and pty/unix.rs; narrow modifications to child/unix.rs,
protocol.rs, supervisor.rs, session.rs and testchild; new `tests/pty_ipc.rs`.
**Consumes:** eligible backend decision, PtyOptions/authority/budget.
**Produces:** `PtyOwner`, `PtySize`, resize; a private `spawn_pty_supervised` adapter
returns the same identity/publication/cleanup ownership facts as spawn_supervised,
with master read/write/control handles instead of pipe stdout/stderr.
Backend tests run through a dedicated fl-host-testchild worker entry calling a
doc-hidden `fl_host::pty_backend_worker` fixture helper, matching the foundation's
containment-worker pattern. It drives the platform owner directly; it is not a
daemon request, capability bypass or env-controlled production admission hook.
Until Tasks6–7 finish, new PTY daemon requests still refuse as Unsupported.

- [ ] RED tests `pty_slave_is_controlling_terminal_before_exec`,
  `pty_detach_preserves_owner`, `pty_input_is_raw_and_fenced`,
  `pty_resize_requires_current_controller`, `retained_pty_shutdown_is_bounded`.
  Child proves isatty/foreground group and observed size, raw bytes unchanged, stale
  lease cannot resize/write, inherited master/slave capabilities do not leak across
  exec, held-open descendant FD cannot hang shutdown, no false Mac adoption claim.
- [ ] Run `cargo test -p fl-host --test pty_ipc <name>` and preserve RED evidence.
- [ ] Implement validated slave setup and supervisor-owned setsid/controlling
  terminal before exec; keep identity publish-before-Go barrier and only supervisor
  reaping. Extend input writer with raw jobs and serialized Resize, existing5s
  deadline/64ops/256KiB cap, no newline or ambiguous retry. Do not move process
  ownership into hub or let PTY close imply observed subtree death.
- [ ] Run native Unix GREEN and existing containment/IPC tests; commit/review.
  PTY capability remains false pending projection and Windows native acceptance.

### Task 6: Windows ConPTY with creation-time containment

**Files:** New pty/windows.rs; narrow child/windows.rs/session/input integration;
same `pty_ipc.rs`, cfg(windows) native cases.
**Consumes:** common PtyOwner API and Task0 platform source proof.
**Produces:** creation-time Job-contained ConPTY owner, bounded read/write/resize
and ClosePseudoConsole drain/cancellation path compatible with all-epoch shutdown.

- [ ] RED tests `conpty_is_in_job_before_child_exec`,
  `conpty_resize_and_raw_write`, `conpty_close_cancels_blocked_io`,
  `conpty_exit259_and_zero_preserved`, `conpty_historical_epoch_shutdown`.
  Retain independent process HANDLE/signaled proof and Job accounting; assert one
  exact Exited precedes successful Cleanup. No Ctrl-C-to-turn-interrupt claim.
- [ ] Run those on native Windows and retain RED. Linux GNU check is supplementary.
- [ ] Implement pseudoconsole startup attributes alongside creation-time Job-list
  containment, retained process/Job identity and safe inherited-handle list. If the
  chosen wrapper cannot express this, stop with source evidence, not assign after
  spawn. Serialize actual writer/Resize ownership and bounded drain before console
  close; cancellation signals every retained epoch. Preserve error capture.
- [ ] Native GREEN plus unchanged Windows containment/pipe IPC tests, strict
  all-target Clippy; commit/fresh review. Do not advertise Windows support from
  compile-only evidence, and no dependency/version improvisation beyond Task0.

### Task 7: Bounded renderer, raw ring and atomic synchronized attach

**Files:** New pty/projection.rs; extend slice_budget, daemon, proto extension/client;
unit tests here and native pty_ipc cases.
**Consumes:** eligible backend with proven capture/quiescence/allocation APIs,
Task 0 approved versioned portable State/Row schema and reconstruction fixtures,
PtyOwner and common stream codec. **Produces:** feed/capture, bounded Snapshot
iterator and client `attach(stream, resume)` yielding snapshot state/rows then data.

- [ ] RED unit cases `projection_budget_reserves_before_parse`,
  `snapshot_requires_quiescent_whole_chunk`,
  `snapshot_deadline_never_renews_on_arrival`, `snapshot_continuation_state_matches`,
  `ring_gap_during_snapshot_requires_resync`, `snapshot_cancel_retires_charged_copy`,
  `attach_rpc_admission_is_not_synchronized`, `disconnect_before_end_stays_unsynchronized`,
  `gap_before_end_discards_staged_snapshot`, `resume_requires_matching_resume_ready`
  and `snapshot_unknown_schema_refuses_before_reconstruction`.
  Use injected monotonic time, partial UTF-8/CSI/OSC, quiescence only inside chunk,
  attributes/charset/tabs/scroll region/saved cursors/alternate screen/wrap; compare
  complete snapshot+suffix to continuous terminal oracle. A cursor alone cannot
  resume a fresh parser, but exact retained parser state may resume mid-sequence
  only after matching ResumeReady. Exercise pending RPC -> stream opening -> all
  validated parts -> End transitions and rejected schema/revision/cancel paths.
  Round-trip Task 0 canonical schema fixtures before comparing the native oracle.
- [ ] Add allocation/count boundary cases: cap131072cells, default71040cells,
  huge glyph/control strings,16MiB projection,1MiB/4096ring,2global snapshots,
  16MiB snapshot+row,192MiB aggregate,8MiB metadata, base64 encoded-frame maximum.
  Partial or oversized state never marks synchronized; refuse incompatible resize
  before OS mutation. SnapshotNotReady releases attempt reservations at100ms.
- [ ] Run `cargo test -p fl-host --lib pty::projection::` RED, implement bounded
  adapter/ring/capture without global lock held over I/O. Parse complete raw chunks,
  implement exactly the approved portable State/Row codec and reconstruction,
  capture full continuation state at quiescence, frame rows incrementally, keep
  pins charged; raw drain never waits on a viewer. ProjectionUnavailable is honest
  after unsupported overflow, not a silently altered grid. Geometry/state revision
  changes invalidate stale snapshots/cursors and cannot splice old dimensions.
- [ ] Unit GREEN, client decoder/staging tests, native `pty_ipc` snapshot oracle
  tests; commit/fresh review. Enable PTY bit only for a fully eligible implemented
  platform backend; evidence/actual acceptance still must accompany release claims.

### Task 8: Native isolation, limits and final host verification

**Files:** pty_ipc/reliable_events/containment tests and new-mode docs only unless a
test exposes a real bug; fixes receive separate fresh fixer/re-review.
**Consumes:** Tasks1–7 approved. **Produces:** exact-head branch evidence/limits.

- [ ] Add real-host `slow_viewer_cannot_block_other_session_control`,
  `attach_cannot_inherit_observer_mutation`, `resize_race_requires_revision`,
  `max_geometry_memory_and_pending_reference_bounds` and `host_shutdown_all_modes`.
  Pause socket reads with barriers and bounded data, assert healthy order and
  explicit resync; list/control reply for an unrelated admitted session stays inside
  5s; no global lock across waits; old-epoch held handles/FDs cancel and retire safely.
- [ ] Native real-binary Linux/macOS/Windows tests must execute; record logical
  charge peaks and process high-water RSS/overhead separately. Source arithmetic or
  sampled RSS is not proof of allocator bounds. If100ms snapshot timing is unstable,
  return measured cause for design review rather than relaxing it silently.
- [ ] Run once on final current-main exact head: `cargo fmt --all --check`,
  `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`,
  strict Windows GNU host/proto all-target Clippy and `fl arch check`. Use existing
  verified binary/command provenance for arch; do not install/start any service.
  Native CI runs existing host/proto jobs, no workflow weakening or vendor calls.
- [ ] Fresh whole-branch review, resolve findings with exclusive fixer/scoped
  re-review, recheck current base/trailers/clean state and return exact SHA, logs,
  quantitative test groups, native job outcomes and residual platform limitations.
- [ ] Lead execution/push/PR dispositions and normal required approvals still
  apply. This milestone establishes host acceptance only, never live cutover.

## 3. Dependency and parallelism rules

Task0 Claude evidence may run early in parallel with reliable design/implementation,
read-only and without sharing a writer. Task0 PTY eligibility gates5–8, not1–4.
Task1 ->2 ->3 ->4 is the reliable checkpoint; Task5 ->6 ->7 ->8 is the PTY sequence.
One exclusive product-tree writer at a time; parallel reviews/evidence are read-only.
No task is allowed to auto-start the next unapproved product stage from a docs GO.
Treat plans as task-sized review gates, not a command batch to execute unattended.

## 4. Separate downstream plans and replacement readiness

Do not implement these inside host Tasks1–8:

- Controller/store plan: persisted intent/delivery/server-request state; applied
  cursor/mapping commit before Ack; recover before new admission; exact thread/turn
  and authority; ambiguous-no-resend; separate control/prompt lanes; bounded restart
  and whole-slice admission. Fake-controller Task4 is not this store integration.
- Service/installer plan: package fl-host, authenticated current-incarnation
  readiness, Linux delegated cgroup path, native truthful other-platform service
  guarantees, cancellation/backoff/shutdown and rollback. Neither test binary nor
  merged source means a service was installed. Stale Ready cannot satisfy adoption.
- Messaging plan: shared durable exposure ownership across peer/hook/controller;
  existing recipient fences survive ACK/route changes. Coordinate Ferris's MQ work;
  no retarget/full-content fallback after uncertainty or weak ACK-ownership import.
- Claude adapter/conformance plan: consume Task0 evidence, then separately authorized
  isolated native persistent multi-turn, exact UUID resume, structured interrupt,
  request-bound approvals and policy read-back tests; no permission bypass. A static
  schema or fixture validates our parser, not vendor behavior. Live calls/spend need
  separate explicit allocation. Unproven automated slice bounds remain fail-closed.
- Canary/retirement plan: explicit approved adoption, context/draft/mail continuity,
  real approval channel, exclusive execution owner, measurable replacement soak and
  rollback before old session retirement. Owner order: Claude first, gated replaced
  legacy-session retirement, Kayla next, Ferris last. No wall-clock migration promise.

## 5. Review/disposition checklist

- [x] Recovery accepted; no repeat Owner question.
- [x] Lead `01a1b39c-3b8a` accepts 1.1 opt-in, unchanged 1.0 auth/wire shapes, strict
  downgrade isolation and no abandonment API. If incompatible, return a concrete
  major-2 amendment, never silently change the contract.
- [ ] Lead approves Task0 portable versioned snapshot schema and reconstruction
  fixtures before PTY codecs; missing schema evidence blocks PTY, not reliable work.
- [x] Lead accepts documented numeric resource/default/100ms criteria as engineering
  targets, including 512-row default. Task0/native evidence must establish allocation,
  RSS and performance feasibility; ineligible backend returns blocked. No implicit
  limit increase or dependency selection follows from acceptance.
- [x] Relevant public static source/docs retrieval is authorized for read-only
  evidence; no repeated permission request for retrieval alone. Runtime/vendor
  invocation, installs/updates/auth and spend remain outside that authorization.
- [ ] Fresh builder and source-aware reviewers check spec/plan/interface consistency;
  accepted material findings go to fresh fixer and fresh scoped re-review.
- [ ] After design/plan disposition, Lead explicitly authorizes the execution stage.
  Until then only these docs are changed; no product build/test/vendor execution.
