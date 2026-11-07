# Native host: reliable events and PTY addendum

**Status: recovery scope and Lead 1.1/resource engineering targets ACCEPTED;
implementation plan remains UNEXECUTED, not execution/publication authorization.**
Lead disposition `01a1b39c-3b8a` preserves measured feasibility and Task 0 backend/
portable-schema gates; accepted targets are not demonstrated runtime capability.
Prepared by Felicia for Ferris. This document proposes the next bounded host slice;
it does not authorize product implementation, vendor execution, installation,
session adoption, route changes, restarts, migration or teardown.

**Recovery scope ACCEPTED by the Owner**, reported by Ferris in
`01a1b34b-aaad-76d3-96d7-c12394718c8f`: recover a controller/hub
restart while the same host survives. Host crash means interrupted/uncertain
recovery, without journal, surviving-process adoption or automatic respawn guarantees
in this slice. Do not reopen this selected recovery scope or ask the Owner again.

## 1. Baseline and authority

Source baseline: freshly fetched `origin/main`
`44e1bdfc25baff65e6fbbe35d6fad9f83046c66e`, the PR358 merge. The historical
[foundation plan](../plans/2026-11-06-native-host-foundation.md) and
[native-agent-host design](2026-11-03-native-agent-host-design.md) remain archival,
unchanged. This addendum requires its own disposition; it does not silently amend
their approved contracts. In conflicts, this document identifies a proposal rather
than claiming the change is already ratified.

Source observations at this baseline:

- [`types.rs`](../../../crates/host-proto/src/types.rs) defines protocol 1.0,
  unsupported `SpawnKind::Pty`, and no Ack/cursor/Attach/Write/Resize requests.
- [`auth.rs`](../../../crates/host-proto/src/auth.rs) authenticates the offered
  version and negotiates the minimum minor. [`client.rs`](../../../crates/host-proto/src/client.rs)
  gates operations by minor, but [`frame.rs`](../../../crates/host-proto/src/frame.rs)
  still checks encoding against minor zero.
  [`transport/mod.rs`](../../../crates/host-proto/src/transport/mod.rs) HostConn has
  no negotiated-minor field, uses legacy recv/respond codecs, and both Unix/Windows
  admit paths discard the authenticated agreement. Task1 must retain that agreement
  and gate actual RPC/event codecs, not just add detached codec helpers.
- [`output.rs`](../../../crates/host/src/output.rs) retires retained output after
  successful socket writes and reference settlement, not durable controller Ack.
  Retained epochs and unresolved ownership already consume bounded admission.
- [`daemon.rs`](../../../crates/host/src/daemon.rs) retains output by session/epoch
  but replaces and shuts down an older writer. Historical Ack ownership therefore
  cannot be implemented solely through the latest writer map.
- [`input.rs`](../../../crates/host/src/input.rs) implements connection/epoch-bound
  writer leases and permanent fencing after ambiguous partial input. Lease change
  is not proof that an OS write stopped.
- The standalone dependency boundary and platform limitations in
  [host-foundation.md](../../host-foundation.md) remain prerequisites.

The proposed order is reliable pipe events, then PTY ownership and projection.
Controller, service/installer and vendor integration are separate downstream slices.
No broad foundation rewrite, UI, MCP management endpoint or vendor parser belongs
in this host slice. Native tests use synthetic children and clients only.

## 2. Alternatives and proposed approach

| Alternative | Benefit | Cost or limit |
| --- | --- | --- |
| Host reliability then PTY; controller and vendors follow | Small authority boundary, portable mock acceptance, preserves standalone host | Does not by itself enable migration |
| Reliable pipes then controller/Claude before PTY | Earlier structured-adapter feedback | Defers the requested PTY prerequisite; still needs a separately approved Claude interface |
| Host + controller + service + Claude together | End-to-end feature in one delivery | Couples unproven schemas, persistence and platform containment; difficult fault attribution |

Recommend the first. Within it, compare two compatibility approaches:

- **Lead-selected 1.1 opt-in:** keep the 1.0 handshake/frame layouts and existing
  requests/events unchanged. New operations require negotiated minor at least 1,
  followed by a bounded authenticated capability query. Reliable delivery is an
  explicit immutable spawn mode; no legacy session is silently upgraded.
- **Major 2:** clearer separation if compatibility tests show that additive new
  operations cannot preserve the old decoder, framing or authentication transcript.
  It requires explicitly selecting a compatible client before new sessions spawn.

Lead accepted 1.1 subject to the compatibility matrix in section 9. Unknown/absent
capabilities refuse new operations before any spawn or input. Never retry a failed
reliable request as legacy. Do not add capability fields to the existing fixed
handshake or send new frame kinds to 1.0 streams. A capability report describes
implemented features and limits, not vendor conformance or per-agent authorization.
If actual authenticated transport compatibility cannot be preserved, return a
concrete major-2 amendment before implementation; never silently switch. Each
HostConn retains its authenticated negotiated minor on both native admission paths;
request/response and raw/extension event codecs use that connection's value. Client
RPC and event sockets gate independently. Neither self-declared payload versions nor
an unconditional minor1 can grant extension permission.

## 3. Distinct observations and identifiers

Maintain four distinct facts: host transport wrote bytes; controller durably applied
an event; vendor accepted a correlated request; vendor completed a turn. No one fact
implies the next. PTY echo/snapshot and presentation ACK imply none of those semantic
facts. This host does not decode vendor request/thread/turn identities.

Reliable stream identity is `(host_instance, session, session_epoch)`; `seq` is a
strictly increasing logical-event number within that stream. A random host instance
identifier is regenerated at daemon startup and returned through negotiated APIs.
No persisted cursor crosses instances implicitly. Epoch reuse/wrap and sequence
overflow refuse new work; they never wrap into a previously valid identity.

One complete pipe line or lifecycle record consumes one sequence, independent of
fragment count. Diagnostics specific to a connection, such as SlowSubscriber,
are outside this authoritative sequence. Every authoritative record is replayable;
there are no invisible sequence holes from another subscriber's status.

## 4. Proposed reliable pipe protocol

Names below describe proposed operations and required fields, not existing APIs.
The companion [implementation plan](../plans/2026-11-06-host-replay-pty.md) freezes
the review-candidate Rust interfaces and wire tags for Lead execution disposition.

| Operation | Required contract |
| --- | --- |
| Capabilities | Authenticated minor-1 query; host instance, feature set and finite limits |
| SpawnSession | Existing validated spawn fields plus explicit `legacy`/`reliable_pipe`/`pty` mode; PTY options only for PTY |
| ClaimController | Exact stream identity; serialized takeover; returns new connection-bound lease and replay metadata |
| SubscribeReliable | Exact stream, controller-bound subscription ticket, `after_seq`; response gives acknowledged/oldest/latest and captured live watermark |
| Ack | Exact stream, current controller lease, highest contiguous completely applied sequence |
| Observe | Read-only reliable-stream observation with its own bounded queue; no Ack or input authority |

Legacy Spawn/Events/ClaimLease/Send keep their 1.0 behavior for legacy sessions.
Legacy requests cannot mutate, claim or consume a reliable/PTY session by name alone.
Legacy listing cannot represent reliable-mode semantics to a 1.0 client: its
projection lists legacy sessions only. A negotiated listing lists all modes, retained epochs and
capacity pressure; the compatibility limitation must be documented to operators.

### Ownership and split connections

A retained reliable epoch has one controller-authority record independent of its
writer's lifetime. ClaimController is serialized with writer control jobs. For a
current writable epoch it settles/revokes the old writer under the existing actual
OS-write fence before publishing the new lease. A partial write leaves input Fenced
under every lease. For a historical epoch it grants Ack/replay only; it cannot reopen
input or affect a same-name successor. Legacy ClaimLease cannot bypass this record.

RPC and event sockets remain separate. The authenticated RPC controller creates a
single-use random subscription ticket bound to connection ID, host instance, epoch,
lease, replay cursor and a 5-second expiry. The authenticated event connection
consumes it once. Before consumption, at most one outstanding authoritative ticket
per controller; replacement invalidates the earlier ticket. Exactly one active
authoritative event stream is allowed per retained epoch/lease. While active,
another SubscribeReliable returns Busy without minting a ticket, changing its
replay cursor/emitted ceiling or disturbing it. Additional readers use Observe.
After disconnect/revocation, replacement waits for stream authority to settle;
actual retained references remain charged until retirement. Ticket/token values are
redacted. Ack travels over
the owning RPC connection. A same-token/same-user event client is not treated as
that RPC owner merely because it knows the session name.

RPC disconnect revokes the controller lease and tickets but preserves cursor and
retained data. Event disconnect terminates that subscription, not process ownership.
Takeover supersedes old subscriptions; admission is rechecked after every await.
Pending tickets reserve subscriber slots on issue, not only on consumption; both
pending and active subscriptions count toward the existing total limit of 16.
Expiry/revocation releases that slot only after the corresponding references retire.
The host shared token still conveys the foundation's privileged local authority;
this slice does not add per-agent identity authentication or remote RBAC.

### Cursor and atomic replay/live boundary

The host starts with acknowledged sequence zero. For a new controller, its durable
cursor may equal or exceed the host Ack cursor, up to the highest published event:
this covers a committed cursor whose Ack was lost. A lower cursor gets explicit
`CursorExpired` if required records were already retired; never jump silently.
A future cursor or different host/epoch is refused. Only the authoritative
controller may submit that recovery cursor. An omitted cursor is valid only when
the host Ack is zero; it does not mean "start at latest".

Under the output lock, validate the cursor, capture watermark W, register a catch-up
cursor and live continuation, then release the lock. Replay `(after_seq, W]` in order,
then events above W exactly once on that subscription. No transport I/O or waiting
for acknowledgements occurs under a global/session registry lock. References and
catch-up positions remain charged until cancellation or settlement.

The subscriber library yields only complete reassembled events, identifies the
complete-event cursor and never automatically Acks upon read, display or socket
write. Partial fragments are not applied or Acked. The controller must durably commit
application, ownership mapping and its cursor atomically before requesting Ack.
The host cannot prove a client honored that promise; the downstream controller's
fault tests must prove it. This is not exactly-once vendor execution.

Ack is cumulative and monotonic. Duplicate Ack at or below the committed host Ack
is idempotent for the valid owner; stale lease/connection or wrong instance/epoch
still refuses. The per-lease ceiling starts at host Ack and rises only through
its validated durable recovery cursor or complete events fully emitted on its sole
authoritative stream. Event close retains this ceiling for Ack-only retry; after
settlement, a replacement stream on the same lease carries it forward. Takeover
starts a new lease at max(host Ack, its own validated recovery cursor); old-lease
emissions and observer progress cannot raise it. Ack above the ceiling refuses. A
validated recovery cursor covers a prior lost Ack; second-subscribe Busy cannot
change the baseline or ceiling.
The host advances the Ack cursor before replying. Lost reply permits Ack-only retry,
not input resend. Retirement additionally requires existing reference/thread/ownership
conditions. Repeated Ack cannot release memory still referenced by a writer/viewer.

### Full buffers, lifecycle records and abandonment

Keep 8 MiB per-epoch stdout charge and 4,096 data-event count. Charge partial input,
event envelopes, fanout and parked references before reading/allocating. Full reliable
stdout pauses that epoch's reader and reports `consumer_stalled`; it does not drop
protocol data, kill the child or stall another session. Observers may disconnect
with explicit lag without releasing the controller's retained record.

Reserve 16 KiB and 16 records per epoch for authoritative lifecycle events, separate
from data. Emit each bounded lifecycle transition at most once; transport diagnostics
are not journaled there. Reservation must cover the finite state-machine transition
set including errors at saturation, or implementation is blocked. No eviction of an
unacked lifecycle record is allowed. Kill/cleanup proceeds even if stdout is full;
output cancellation/retirement must preserve unread data and unresolved ownership.

There is no automatic abandonment TTL. If the controller never returns, retained
output continues consuming capacity and new spawn may be refused. Discarding pending
data, force retirement or recovery by a different host is outside this slice. Kill
alone is not permission to discard unread output.

## 5. PTY ownership and input

PTY is a separate spawn mode, not a pipe child with synthetic terminal bytes.
The host owns the master/pseudoconsole and process lifetime; attaching/detaching a
viewer does not transfer them. The child has `TERM=xterm-256color`. A pipe child
remains a pipe child for its entire epoch; Claude structured input is not sent via
PTY Write. PTY stdout/stderr form one terminal byte stream; no false separate stderr
guarantee is added.

Unix keeps the dedicated unreaped supervisor and identity-validated, serialized
group-signal contract. Set up the slave as controlling terminal before vendor exec;
do not let the hub own or reap it. Windows must combine ConPTY setup with the
foundation's creation-time Job containment before any child instruction executes.
A portable-pty convenience spawn followed by Job assignment is insufficient. A
library/backend that cannot meet this requirement is not an accepted implementation.
Recheck dependency version/license and OS requirements when implementation is approved.

Write accepts raw bytes, with the foundation's 64-operation/256-KiB aggregate queue,
5-second write deadline, actual-write fencing and Written/NotWritten/Ambiguous
semantics. Never append a newline or retry an ambiguous write. Resize is serialized
with controller mutation, checks exact epoch/lease immediately before the OS call,
and only the current controller can change canonical size. Observer viewport sizes
do not resize the child. Dimensions are 1..1000 columns and 1..400 rows, default
120x40, subject to the explicit cell/memory admission below. Reject out-of-range or
unadmittable sizes before mutation; do not silently clamp. Duplicate identical
successful resize is idempotent; uncertain resize requires observed state/refusal,
not unbounded retry. No-controller size remains the last accepted size.

Shutdown cancels all retained-epoch reader/writer pumps and performs bounded drain
before closing the ConPTY/master; no indefinite join on held-open descendant FDs.
Keep exact exit evidence, Exited-before-successful-Cleanup and Windows exit 0/259.
Linux delegated cgroup/service containment is deferred; macOS/Linux detached
descendant residuals remain explicit. PTY hangup is not proof every descendant died.

## 6. PTY projection, attach and finite admission

Raw PTY bytes receive a per-epoch output sequence distinct from reliable event seq.
PTY lifecycle records use the same controller-Ack journal as reliable pipe lifecycle
records, with their own contiguous event sequence; raw display bytes never enter
that journal. Historical PTY lifecycle Ack authority survives writer retirement.
Attach includes exact identity and optional resume with after-output-seq, schema
and projection revision. Viewport geometry stays client-local; only the current
controller Resize changes canonical PTY size. Replay a covered cursor's suffix only
when the viewer retains its exact continuous terminal/parser state for that epoch and projection revision,
without a gap. Such a resume may have pending parser state; a sequence number alone
does not prove the viewer has it. Otherwise, including initial attach, missing
history, geometry/revision mismatch or gap, require a synchronized snapshot at W
with generation, dimensions, coverage and snapshot ID, then bytes above W.
Coverage explicitly distinguishes current screen, included scrollback and
unavailable history. No partial snapshot is marked synchronized.

Attach RPC admission is explicitly PendingSnapshot or ResumePending, never a
synchronized boolean. The ticketed stream establishes completion: PendingSnapshot
requires the client to validate the complete matching snapshot ID/schema/revision/W,
all ordered state/rows, complete coverage and SnapshotEnd with a continuous suffix.
ResumePending requires a stream-opening ResumeReady confirming the exact retained
state cursor/schema/revision after ticket-consumption coverage revalidation. The
client must actually retain that matching parser/state; RPC acceptance alone proves
neither path. Cancellation, pre-End disconnect, gap, unknown schema or revision
change leaves staged state unsynchronized and discards it; later loss invalidates
an already synchronized view. No implicit fallback can assert synchronization.

A synchronized snapshot carries all bounded state needed to render subsequent
bytes identically: screen cells and attributes, active attributes and character
sets, tab stops, scroll region, saved cursors and screens, active/alternate screen,
cursor and wrap state, and all applicable input/output terminal modes. Visible
rows or a formatted-contents export alone do not establish this contract. The
chosen renderer must expose a documented, bounded way to capture and reconstruct
this continuation state; otherwise it cannot advertise synchronized Attach.

The State/Row wire representation must be portable and explicitly versioned, not
a backend-private memory or unversioned serde dump. Task 0 must deliver a registered
u16 schema ID, byte encoding, complete continuation/cell fields, bounded lengths,
ordering/reconstruction rules and canonical codec/reconstruction fixtures, and
obtain Lead disposition or return blocked before PTY codec/dependency/product edits.
SnapshotBegin and ResumeReady carry that schema; unsupported versions refuse before
reconstruction or a synchronization claim. Reliable framing can proceed separately
while PTY payload support remains unavailable. Backend eligibility alone does not
approve a new wire schema; Task 7 must consume the exact disposed schema/fixtures.

Capture is permitted only at a complete renderer synchronization boundary with
no pending UTF-8 decoder, escape/CSI, OSC or other control-string parser state.
For this slice, W is the last fully parsed output chunk's sequence, and capture
occurs only between complete chunks when the parser is quiescent. A safe point
inside a chunk is not a capture point: consume the whole chunk normally and check
again at its end. Never split, renumber or duplicate a chunk to manufacture W.
The snapshot therefore includes exactly all bytes through W; replay starts with
the next whole chunk. Parser state and sequence publication use this same boundary.

Waiting for a safe boundary, admission and grid/state capture share the existing
100-ms snapshot budget, measured from the Attach snapshot attempt. Arrivals do
not reset the deadline. Keep draining and parsing child output within the existing
bounds while waiting; never block child reads on snapshot readiness, flush a
partial parser state or invent terminal bytes to terminate it. If no safe capture
completes before the deadline, return typed SnapshotNotReady, release the attempt's
reservations and expose only explicitly incomplete live-only coverage if requested.
Do not report synchronized success; a later explicit attempt has its own deadline.

Snapshot capture pins/copies the bounded grid and continuation state and establishes
the post-W cursor under one consistency boundary; serialize one row per frame
outside the process/output locks. One row must fit 256 KiB with envelope. Never
concatenate an entire snapshot into the 1-MiB pipe event reassembler. Client stages
only one bounded snapshot until complete; cancellation frees it. Its ring pin cannot
grow storage: if the suffix falls out while transmitting, abort with ResyncRequired
rather than splice a gap.

Slow viewers lose presentation coverage and receive resync/disconnect; they do not
backpressure PTY reads. No display frame ACK advances the reliable Ack cursor.
With no viewers, keep parsing into the bounded projection/ring. Escape sequences,
combining characters and parser partial strings count toward bounds. A renderer
dependency with unbounded hidden storage is not acceptable. If a valid sequence
cannot be represented within declared bounds, report ProjectionUnavailable and
continue bounded raw draining; never advertise an altered/truncated screen as exact.
Projection is restored only by a defined complete reset/reconstruction, otherwise
requires a new epoch. No terminal bytes trigger vendor parsing or shell execution
in the host.

**Lead-accepted hard defaults/ceilings as engineering acceptance targets:**
Allocation/RSS/performance feasibility still requires Task0 and native evidence;
acceptance neither selects a dependency nor permits an implicit limit increase.

| Resource | Bound and exhaustion behavior |
| --- | --- |
| Total retained epochs/ownership | Existing 64 admission slots, including unresolved/retained history |
| Total subscribers | Existing daemon limit 16, including controller streams, observers and viewers |
| Reliable data | 8 MiB / 4,096 events per epoch; 512 MiB aggregate category ceiling; pause reader |
| Reliable lifecycle reserve | 16 KiB / 16 records per epoch; 1 MiB aggregate reservation |
| PTY epochs | At most 8 within the existing 64 total slots |
| PTY ring | 1 MiB / 4,096 chunks per epoch, oldest presentation bytes evicted with visible gap |
| Projection storage | 16 MiB per PTY / 128 MiB aggregate, including renderer/parser heap |
| Projection geometry | 131,072 total cells: main + alternate + scrollback; default 512 scrollback rows, reject an incompatible size/history combination |
| Snapshot | One per PTY, at most 2 daemon-wide; 16 MiB each / 32 MiB aggregate including pinned/copied grid and one serialized row |
| Parser temporary strings | 64 KiB total per PTY inside its 16-MiB projection budget |
| Viewer pending payload | 256 KiB and 64 items per viewer, with one additional at-most-256-KiB frame in flight; explicit resync/disconnect |
| Additional slice aggregate | 192 MiB reserved ceiling for PTY rings/projections/snapshots/viewer buffers, reliable lifecycle reserve and new authority/ticket/queue metadata |

### Static feasibility and implementation eligibility

The category maxima fit the accepted engineering envelope: 128 MiB projection + 32 MiB
snapshot + 8 MiB rings + 8 MiB viewer pending/in-flight (16 times 512 KiB) +
1 MiB lifecycle + 8 MiB additional authority/ticket/reference metadata = 185 MiB,
leaving 7 MiB unallocated headroom inside 192 MiB. Never allocate that headroom
implicitly. Admission takes the category and aggregate reservation atomically;
queued work does not reserve a category twice. The 8-MiB metadata cap includes
all slice-added queue nodes/cursors/Arc controls; fixed count limits do not replace
byte charging. OS/thread/allocator overhead remains outside this logical bound.

At 120x40 plus 512 scrollback rows the conservative main+alternate+history count
is 71,040 cells; at the 131,072-cell ceiling a 96-byte inclusive cell representation
would consume 12 MiB, leaving 4 MiB for parser/state/index storage. These arithmetic
checks show internal budget consistency, **not a measured renderer bound**. The
selected renderer must document its actual inclusive cell and auxiliary upper
bounds and a snapshot-copy-plus-row bound of at most 16 MiB. If it cannot, reject
that backend; do not silently shrink glyphs/history or call allocator sampling a
hard bound. Quiescence/state export must also be demonstrable from backend source.

Source check for this docs revision: neither vt100 nor portable-pty is in the
baseline Cargo.lock/manifests or the locally searched Cargo source cache. Therefore
the archived version suggestions are not a selected or proven dependency. The plan
has an explicit source-feasibility gate before PTY dependency or product edits.
Ack/replay work can be reviewed independently; no PTY capability or successful
terminal conformance may be claimed until that gate and native tests pass. The gate
also requires a versioned portable snapshot schema/reconstruction disposition and
fixtures before PTY codec work. It returns a bounded source-evidence report or a
precise blocker, not a guessed pass.

The geometry default 120x40 plus two screens and 2,000 scrollback rows would exceed
131,072 cells. Therefore the Lead-accepted usable default is **120x40, 512 scrollback
rows** (71,040 cells); 2,000 is an explicit request only and may be refused. This is
the accepted engineering change from the archived 2,000-row default; it remains
subject to measured backend feasibility, not a claimed implementation result.
Use checked arithmetic and preflight the whole combination before allocating or
resizing; reject rather than silently remove prior history to make room.

All slice-controlled allocations, Arc-held bodies, staged frames, cursors, tickets,
snapshot source copies and reader-ahead storage must be included exactly once in
category and aggregate charges. A shared body charges once until its last reference
retires; per-client copies charge separately. Fixed per-slot metadata is preallocated
or included in reservations. Count limits prevent tiny-record/reference exhaustion.
No allocation occurs before successful reservation; freeing a queue position does
not imply its parked reference freed storage. Measure/account library allocations
or derive checked upper bounds before advertising PTY support. Exceeding the
192-MiB slice ceiling refuses new work or invalidates presentation coverage, never
reclaims authoritative events. No claim of a 704-MiB total RSS limit follows: the
512-MiB existing data category plus 192-MiB new reservation excludes foundation
baseline allocations, runtime/OS stacks, allocator overhead and child memory.
Acceptance must separately measure process high-water memory and document overhead.

## 7. Controller, service and vendor dependencies

The host slice's client is synthetic. Later work must satisfy these seams before
any adapter connects or a real session is adopted:

| Slice | Missing source and required evidence |
| --- | --- |
| Controller/store | `hub/lifecycle.rs` has tmux-oriented HostedSessionRecord, no SessionController. Add one owner, durable delivery/server-request ledgers, request/thread/turn mapping and applied cursor before Ack; recovery before new input; control/prompt lanes; authenticated approval authority; ambiguous-no-resend; finite restart/admission budgets. Mock vendor fault tests span receipt, commit, write and Ack boundaries. |
| Service/installer | `install/services.rs` plans commands but execute is NotImplemented; Windows manager absent. Package fl-host, supervise ownership/auth startup, implement delegated Linux cgroup path and truthful other-platform guarantees, rollback, cancellation during backoff and no restart after shutdown. Current-incarnation readiness must be revocable. |
| Messaging integration | One durable content-exposure ownership contract spanning peer/hook/controller lanes; existing recipient fence survives receipt Ack and route change. Coordinate with separate MQ work; no direct current messaging patch in this slice. |
| Claude | `exec/adapters/claude.rs` is one-shot print JSON; `wire/dispatcher.rs` uses legacy socket/tmux. Neither proves persistent stream, interrupt, approvals, resume or whole-slice bounds. A separately reviewed adapter follows controller readiness. |

Do not synthesize observed death from persisted Running/Killed labels, retry a
vendor operation because a receipt timed out, use terminal frame ACK as a durable
event cursor, or infer least privilege from MCP availability. Do not import Orca's
refusal retarget, Agistry's weak ACK ownership or Relay's hidden-injection fallback.

## 8. Bounded exact-version Claude evidence and conformance proposal

This section proposes later work; **no live vendor invocation or spend is authorized
by this assignment**. Recorded local help was 2.1.317 on November 4; the observed
launcher pointed to 2.1.319 on November 6. Neither executes or proves today's binary.
The fixture advertises print + stream-json input/output, explicit UUID/resume, and
host permission prompts; it lacks complete framing/control schemas. Persisted
system-prompt snapshots may survive resume. Background management ID, conversation
UUID, host epoch and controller lease are different identities.

1. Offline provenance: inspect launcher/target metadata and hash without launching
   it; inventory local versioned SDK schemas/source/docs. A separately approved
   non-model help/version capture may refresh fixtures, but is not performed here
   and must not invoke update/install/doctor/auth/session commands. If offline
   authoritative schema is absent, record the gap and use already-authorized
   relevant public static source/docs retrieval, pinning URL, commit/version, digest
   and license without a renewed permission gate for retrieval alone. Distinguish
   authoritative schemas, local observations and assumptions; never reverse-engineer
   a wire payload from guessed fields or private conversation transcripts. Static
   retrieval authorizes no vendor/model call, install/update/auth command or spend.
2. Produce an evidence matrix for initialization/readiness, JSON record boundaries,
   user/control IDs, partial/final events, EOF, multi-turn liveness, turn interrupt,
   approval request/decision schemas, cancellation and resume. Explicitly distinguish
   a documented SDK feature from native CLI conformance at the selected version.
3. Design offline fake-protocol fixtures for the controller: fragment/oversize data,
   late acceptance after timeout, old-epoch responses, duplicate approvals,
   disconnect-before-decision and resume mismatch. These fixtures validate our
   consumer only; they cannot prove that Claude accepts that protocol.
4. After separate authority and bounded spend allocation, propose an isolated real
   Claude conformance matrix: multiple turns on one child, exact UUID resume,
   process-surviving structured interrupt, request-bound allow/deny, cancellation,
   lost reply and effective policy read-back. No permission bypass, permissive
   fallback, production transcript adoption or current-agent targeting.
5. Whole-slice budget proof must match version/digest, mode, model, billing and
   subagent/tool behavior. `--max-budget-usd` help or usage telemetry alone is not
   proof. Unknown automated bounds refuse automated work; a human turn does not
   authorize an unbounded automated continuation.

## 9. Proposed acceptance gates

These tests are requirements for later implementation, not executed results.
Use injected clock/barriers for bounded unit failures and real fl-host/synthetic
children for native integration. Preserve foundation regression tests unchanged
unless an explicitly reviewed new-mode fixture is required.

| Gate | Required failure scenarios and assertions |
| --- | --- |
| Compatibility | Actual authenticated RPC/event-socket tests cover Unix/Windows admission, raw minor0 Extended/frame9 injection and forged self-declared minor refusing before effects, independent downgraded event socket despite minor1 RPC, and valid minor1 request/response/opened event flow; 1.0 client/new daemon uses only legacy shapes; new client/1.0 daemon refuses new modes before effects; 1.1 negotiation with absent feature refuses; old global Events never consumes reliable data; no enum/frame/auth transcript change leaks into 1.0. |
| Ack/replay | Receive-before-commit crash, commit-before-Ack crash, Ack reply lost, partial fragment disconnect, replay/live publication race, duplicate/future/stale/wrong-instance Ack, controller recovery cursor above host Ack, explicit expired cursor and no automatic application Ack. |
| Retention | Fill 8 MiB / count cap without Ack; clean shutdown/kill/exit retains unread records; historical same-name epochs can be replayed/Acked independently; no forced capacity reclamation, quota release only after references/threads/ownership settle. |
| Authority | Second authoritative SubscribeReliable is Busy without ticket/cursor/ceiling mutation or disturbing the active stream; pending-ticket replacement before consumption, close/settled replacement, takeover and observer ceiling isolation; delay claim/input/resize/approval continuation across takeover or awaited commit; old connection/ticket/lease cannot act or settle new identity; same-token second socket needs the bound ticket; permanent Fenced input stays fenced. |
| PTY | Real Unix controlling terminal and Windows ConPTY+creation-time Job; interactive echo, raw binary/UTF-8, resize, alternate screen, bracketed paste and terminal modes; detach preserves process; observer cannot mutate. |
| Projection | Snapshot + suffix matches a continuously fed terminal oracle, including continuation-relevant modes, attributes/charset, tabs, scroll region, saved cursors/screens and wrap state; emission/resize/respawn during capture; missing/repeated/mixed rows, unknown schema, RPC admission without completion, pre-End disconnect/cancellation, ring gap mid-snapshot, oversized row, many tiny chunks, parser strings/combining growth and unadmittable geometry; incomplete/unavailable never advertised synchronized. |
| Snapshot boundary | Request Attach between fragmented CSI (`ESC [` then `31mX`), OSC/other control strings and UTF-8 code units, including a safe point inside a chunk followed by a partial sequence at its end. Capture only at a quiescent whole-chunk boundary and compare snapshot + suffix to the continuous oracle with no lost/duplicated bytes or sequences. An existing covered viewer with exact continuous parser state may resume mid-sequence; a viewer with only its cursor must require a snapshot. Never-terminating partial sequences and continuous arrivals cannot extend the original 100-ms deadline: return SnapshotNotReady with no synchronized claim, continue bounded child draining and control service, and release reservations. |
| Isolation/bounds | Block one viewer/snapshot while another session lists/cancels; no global lock spans I/O, finite category/aggregate/reference counts; every retained allocation charged before read/copy; cancel frees tickets, pins and snapshots. Measure memory and control/kill latency with stated bounds. |
| Containment/shutdown | Held-open historical stdout/stderr/PTY FDs, ignored signals, detached descendants, exact exit0/259, Exited before successful Cleanup, bounded all-epoch pump cancellation; no false full-tree containment claim on unsupported OS paths. |
| Downstream only | Readiness becomes false before adoption; stale incarnation ready cannot satisfy dependency; cancel backoff/wait prevents surprise spawn; acceptance reply lost never authorizes resend; competing lanes count content exposures, not method calls; ACK-only retry does not expose content again. |

Admission/latency defaults: retain foundation handshake 5 s, started-frame 10 s and
write 5 s deadlines. Additional tests must prove a blocked observer cannot delay
an unrelated already-admitted List/control reply beyond its existing 5-second
write budget, and shutdown remains bounded by declared cleanup/drain budgets.
Snapshot safe-boundary wait, admission and capture together have a 100-ms test
budget at maximum admitted geometry on native CI, without deadline resets; if this
is not defensible without scheduler-sensitive failures, return measured evidence for design disposition rather than silently increase the limit.

Run native Linux/macOS/Windows host/proto strict Clippy and real-binary tests on the
exact implementation head, then appropriate once-final workspace checks and fresh
scoped/whole-branch review. Cross compilation is supplementary, not native evidence.
Keep evidence of meaningful RED/GREEN for behavioral fixes. No vendor calls in CI.
This docs-only addendum needs diff/link/source review, not duplicate foundation suites.

## 10. Research provenance and limited transfer

Lead source reports were read locally; their test code was inspected by their authors,
not executed. Concepts below are design inputs, not independent upstream audits or
FerroLoop capability proofs. No upstream code or dependencies are imported here.

- RMUX `1f4571e74f36be0c033c6294d1616c7d3a6fbda1`: generation/epoch/coverage,
  sequence-hole rejection, typed transport loss. Preserve snapshot/replay distinction.
- shpool `0433875f17a318e7b9334b1c37a2de3f7eaea152`: detach versus host ownership,
  separate input/resize roles, release global locks before waiting on control I/O.
- Pueue `4e5302597e317c90b40f6cc8fbfa02b4b4a93ef8`: durable task state is not a live
  process handle; restored Killed does not prove observed death or safe respawn.
- Zellij `43c8710b3a9845f60d5a567c7410fc1197fabb66`: display pacing/coalescing is
  separate from durable protocol events; shared resize policy is not a writer lease.
- Process Compose `096b393f40cc8e6b07198b510b90541fd7809ca5`: revocable readiness,
  dependency cancellation and finite restart policies; MCP does not imply authority.
- Orca `a2ad899954c62543c98aa3268793d774f0de0cfd`: late echo and post-await
  generation checks, without importing retarget-after-refusal policy.
- Agistry `d56d0a4db6b0c7482a42dfb1ef33454b87ae5e7f`: receipt-before-ACK retry,
  while explicitly retaining acceptance/receipt crash uncertainty and fenced ACK.
- Relay `929454d13c49c688ba08da86f3ddc4c477049001`: exposure ownership before
  competing lanes; no hidden-injection fallback after an uncertain write.

Source report indexes: [communications research](https://github.com/Artificial-Humanity/FerroLoop/issues/350#issuecomment-6697865907)
and [hosting research](https://github.com/Artificial-Humanity/FerroLoop/issues/350#issuecomment-6697937317).
Local copies: `/tmp/ferris-{orca,agistry,relay}-communications-research.md`,
`/tmp/ferris-rmux-hosting-research.md`,
`/tmp/ferris-shpool-pueue-hosting-research.md`,
`/tmp/ferris-zellij-process-compose-hosting-research.md`.

## 11. Material choices and disposition boundary

Recovery and the following engineering choices are settled by Lead disposition
`01a1b39c-3b8a`; product execution/publication remains unapproved:

1. **Owner/Lead accepted:** surviving-host controller recovery; host crash is
   explicitly interrupted/uncertain with no disk-journal/adoption guarantee, no
   abandonment/discard API. Do not reopen this as an Owner question.
2. **Lead selected:** protocol1.1 explicit opt-in, unchanged 1.0 authentication/wire
   shapes and strict downgrade isolation. If those cannot be preserved, return a
   concrete major-2 amendment. One active authoritative event stream per epoch/lease;
   other readers Observe. Historical authority never reopens input.
3. **Lead accepted engineering targets:** documented numeric PTY/global/snapshot/cell
   ceilings, 512-row usable default and nonrenewing100ms criterion. Measured
   allocation/RSS/performance and backend eligibility remain unproven; Task0 may
   return blocked. No implicit dependency choice or limit increase is authorized.
4. **Remaining PTY gate:** exact backend source eligibility and portable versioned
   snapshot schema/reconstruction fixtures require Task0 evidence and separate Lead
   schema disposition before PTY product work. Reliable Tasks1–4 remain independently
   reviewable; this is not execution GO for them.
5. **Evidence authority:** relevant public static source/docs retrieval is already
   authorized within read-only research. Pin provenance/license/version; distinguish
   vendor schemas, local observations and assumptions. No renewed retrieval-only
   permission gate, vendor/model invocation, install/update/auth or spend follows.
   Native vendor conformance still needs separate authority and bounded allocation.

The companion implementation plan provides exact types/paths, dependency order,
exclusive implementer ownership, per-task fresh review, RED/GREEN fault evidence
and final native verification. It is prepared for review and remains unexecuted.
The next engineering milestone is native host mock conformance;
service/controller/vendor readiness follows. No wall-clock migration promise follows.

Eventual migration order is Claude agents first under Ferris oversight, retirement
of replaced legacy sessions only after replacement/retirement checks, Kayla next,
Ferris last. Each migration needs separately authorized identity adoption, continuity
of context/drafts/mail and approval channel, no competing owner, measurable replacement
readiness and rollback. Existing sessions/routes/jobs remain unchanged by this design.
