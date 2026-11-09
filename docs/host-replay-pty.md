# Reliable host events

The standalone host supports explicitly negotiated reliable **pipe** sessions and
Unix PTY sessions. Controller acknowledgement and replay last while that host
process survives, subject to the mode-specific retention rules below. Protocol
capabilities describe the serving host; these APIs do not install a service or
authorize migration. Unix PTYs also support [rendered capture](host-rendered-capture.md).
The historical [foundation contract](host-foundation.md) describes legacy pipe
sessions and platform containment limitations.

## Negotiate before spawning

Authenticate each RPC and event socket independently. Request protocol minor 1,
then query `Capabilities`. The reliable feature bit is 1; Unix hosts advertise
PTY bit 2. Check the advertised bits before using an optional extension.
The host retains each connection's authenticated negotiated minor. Minor-zero
connections cannot send Extended requests or receive extension frame 9. Authentication
and legacy 1.0 wire shapes are unchanged. An unsupported request is an explicit
refusal, never permission to retry with legacy semantics.

`SpawnSession` selects an immutable `legacy`, `reliable_pipe` or `pty` mode,
validated before native child side effects. Unsupported platform/mode combinations
refuse explicitly. A reliable
stream is identified by `(host_instance, session, session_epoch)`, with a fresh
random instance at each host startup. A session name alone cannot grant authority.
Legacy List returns only legacy sessions. Negotiated ListSessions includes retained
epochs, their mode, input state, acknowledged cursor and latest published sequence.
Legacy Events, ClaimLease, Send, Signal and Kill cannot consume or mutate a reliable
epoch, including during same-name replacement.

## Explicit controller application and Ack

`ClaimController(stream)` grants a connection-bound lease. Takeover waits for the
actual serialized input writer fence, then rechecks the connection, generation and
epoch before publishing ownership. Partial input remains permanently Fenced. A
claim on a historical epoch grants replay/Ack only and cannot reopen its input or
affect its same-name successor. Input outcomes retain Written, NotWritten and
Ambiguous semantics; uncertainty never authorizes an input resend.

`SubscribeReliable(stream, lease, after_seq)` captures a replay watermark and
registers the live continuation atomically. It returns a random single-use ticket,
bound to that live RPC, stream, lease and cursor for five seconds. A **separate**
authenticated event socket consumes the ticket with OpenStream. The host replies
Opened before sending extension frames. Converting an RPC connection to any event
stream revokes its former RPC authority; a controller cannot consume its own ticket
on its owning RPC socket. Pending work prevents conversion until it retires.

Exactly one authoritative event stream may be active per epoch/lease. Another
SubscribeReliable returns Busy without changing that stream's ticket, cursor or Ack
ceiling. A pending ticket may be replaced before consumption. Event close or
revocation cancels the old stream; replacement waits for actual stream settlement.
References still held by a writer continue consuming capacity. Observe offers a
separate read-only subscription with no input or Ack authority. Observer lag is
reported explicitly; it never silently jumps to a new cursor.

The typed client provides `claim_controller`, `subscribe_reliable`,
`ReliableEventStream::next_event`, `cursor`, and explicit `ack`. `next_event` yields
complete reassembled events only. Reading, displaying, or writing a socket does
**not** Ack an event. The controller must atomically and durably commit its application
result, stream ownership mapping and applied cursor **before** requesting Ack.
The host cannot verify that the controller honored this promise.

One complete line or authoritative lifecycle transition consumes one contiguous
sequence number, independent of fragments. A line may be up to 1 MiB; bounded
32-KiB parts carry it without assigning additional sequences. Partial fragments
cannot advance the client applied cursor or the host's emitted Ack ceiling.
Connection notices are outside the reliable sequence. Replay covers `(after, W]`
then the live suffix above W without a gap or duplicate on that subscription.

An omitted recovery cursor is valid only while host Ack is zero. A durable cursor
at or above host Ack and no higher than latest can recover an Ack lost in transit.
A retired lower cursor receives CursorExpired; a future cursor receives FutureCursor.
Different host instances and epochs refuse explicitly. The per-lease Ack ceiling
starts at host Ack and rises only through validated recovery cursors or complete
events fully emitted on its authoritative stream. Observers and partial emissions
cannot raise it. Same-lease stream close/replacement retains its completed-event
ceiling for Ack-only retry. A takeover starts a fresh ceiling without old-lease
emission credit. Duplicate Ack remains idempotent only for the current valid owner.
The host advances its Ack before replying, so a lost reply permits Ack-only retry.

## Finite retention and backpressure

| Resource | Bound |
| --- | --- |
| Retained epochs / unresolved ownership slots | 64 |
| Legacy subscriptions + pending tickets + active extension streams | 16 total |
| Reliable data per epoch | 8 MiB and 4,096 records |
| Reliable data across the host | 512 MiB |
| Lifecycle reservation per epoch | 16 KiB and 16 records |
| Lifecycle reservation across the host | 1 MiB |
| Projection charge across the host | 64 × default PTY storage bound (approximately 593 MB decimal) |
| Additional slice charge / metadata category | Projection allowance + 64 MiB / 8 MiB |
| Active RPC registry entries | 128 |
| Outstanding Extended operations per RPC | 64, subject to metadata admission |

These are logical owned-allocation charges, not a claim about total process RSS.
Projection reservations update counters rather than eagerly allocating the full
bound; vt100 allocates visible rows and grows scrollback as output arrives.
Successful PTY cleanup releases the projection independently of Ack, retaining
the existing 64 KiB raw tail, including any later drain. Completed rendered
capture and resize explicitly refuse; see [rendered capture](host-rendered-capture.md).
ReliablePipe and LegacyPipe retention and acknowledgment behavior is unchanged.
Read buffers, copies, envelopes and retained references are admitted before their
new ownership. Shared event bodies are charged until their last actual reference
retires, even after Ack removes the journal entry. Pending operations retain their
permit through queued and in-flight response retirement. Admission may refuse
earlier when an aggregate category is full.

Stdout records contain at most 1 MiB and flush at each read boundary. Protocol
minor 2 carries `terminated`: append LF only when true. Missing JSON metadata and
old binary frames mean true. A minor 0/1 subscriber closes at an unterminated
record; the child and unconsumed record survive under normal retention rules.

With a live reliable replay cursor, stdout pauses at capacity and reports
consumer_stalled. A stalled live subscriber can still backpressure its producer.
With no live replay cursor or held record reference, admission evicts the oldest
unacknowledged **data** record as needed for record or byte capacity. Lifecycle
records remain retained, and eviction never advances Ack. A later minor-2 reader
receives explicit `Gap { from_seq, next_seq }` notices before missing ranges,
interleaved with surviving lifecycle records in sequence order, followed by exact
retained bytes. The high-level client surfaces a Gap event without advancing its
ACK cursor. Minor-1 readers receive Unsupported and disconnect at a gap.

Finite lifecycle reservation allows Exited and CleanupComplete to progress
independently of data occupancy. Exited precedes successful CleanupComplete;
cleanup and process death remain distinct observations. For reliable pipes, Kill is not permission
to discard lifecycle records. No abandonment TTL exists. Non-reliable output
retains its existing backpressure policy and never uses unattended eviction.

## Completed PTY retention and diagnostics

PTY completion has a separate policy from pipe replay. Successful containment
cleanup immediately drops the parser and projection reservation. Leader exit
alone is not proof of cleanup: a bare session still needs an explicit cleanup
request. Output may continue draining into its raw tail after cleanup.

After successful cleanup, stdout EOF and retirement of the epoch's threads,
spawn/status maintenance fences the completed PTY authority, cancels pending
claims/tickets/streams, and releases journal-owned replay records. It never
advances Ack. Outstanding references retain their real allocation charges until
those references drop. A retained completed PTY offers only its final raw tail;
controller/replay/rendered operations refuse with `SessionFenced`.

The host retains at most eight completed PTY epochs, each with at most 64 KiB of
raw tail (512 KiB total, plus bounded bookkeeping). Maintenance evicts oldest
completed PTYs first, including when an otherwise admissible spawn needs a slot.
Live PTYs, incomplete cleanup and pipe epochs are ineligible. There is no retention
timer: regular spawn/status operations maintain the bound. Each eviction is logged
with its full host instance, session and epoch identity.

`PTY_RETENTION_FEATURE` (bit 32) gates the opt-in extension request
`{"op":"pty_retention_status"}`. Its response is:

```json
{"op":"pty_retention_status","status":{"projection_reserved_bytes":0,"projection_limit_bytes":592968704,"completed_pty_sessions":0,"completed_pty_limit":8,"evicted_total":0,"recent_evictions":[]}}
```

The numeric projection limit above illustrates the current default geometry; it
is derived from the storage bound, not a fixed wire constant. Recent evictions
contain at most 64 full `StreamId` values, oldest first; `evicted_total` is
cumulative for this host instance. No fields are added to existing `HostStatus`
or `SessionSummary` responses. Async/blocking clients expose
`pty_retention_status()` as `Option`: `None` means unavailable on an older host,
never invented zero usage. Unsupported diagnostic requests are never sent to it.

`fl host status` shows reservation use/limit, completed count/limit and eviction
history. Its JSON includes `pty_retention_diagnostics` (`available` or
`unavailable`) and `pty_retention` (the status object or `null`). Reservations are
accounting allowances, not measured RSS. If a PTY spawn or resize hits resource
limits, inspect status, reduce geometry/scrollback or stop unused live sessions;
resource exhaustion can also have causes other than projection capacity.

Typed clients enrich a stale-epoch refusal with one bounded diagnostic lookup.
An identity present in recent history gets an explicit `PTY epoch evicted` error;
an older or unknown identity remains `stale epoch (possibly evicted)` without
claiming proof. Mutations are not retried. Raw protocol clients keep the existing
`StaleEpoch` refusal and can query the negotiated diagnostic themselves. Old
clients never receive an unsolicited new response or refusal variant.

## Recovery boundary and evidence

Controller or hub restart can recover while the same host survives: reload the
durable application/mapping/cursor, claim that exact stream, subscribe after the
committed cursor, and Ack only after subsequent durable commits. This is not
exactly-once vendor execution. Host transport delivery, controller application,
vendor acceptance and vendor turn completion are different facts.

After **host-daemon crash**, the old instance, cursor and lease do not become valid
on a new host. Recovery must expose **interrupted/uncertain**, not silently treat
history as empty, adopt a saved process label, or launch a replacement. No disk
journal/adoption guarantee is provided by this slice. Foundation survivor cleanup
rules still apply, including macOS/Linux detached-descendant limitations. Neither
an empty new session list nor an old PID proves arbitrary descendants died.

`crates/host/tests/reliable_events.rs` runs real isolated hosts and synthetic
controller subprocesses. Its bounded application snapshot commits the applied
records, exact ownership mapping and cursor together before Ack. Tests kill that
controller before commit, after commit/before Ack, and after independently observed
host Ack/before reply, then restart it while checking the same host child PID/epoch.
Other cases disconnect inside a 1-MiB event, publish across W, replay all 4,500
numbered lines after real count saturation/kill/replacement, and refuse spawn at
retained capacity. A separate abrupt host restart verifies stale-instance refusals
and the harness's durable interrupted/uncertain status without automatic respawn.
This proves the synthetic harness, not a future FerroStore controller or vendor.

Local Linux execution and Windows cross compilation are distinct from native
acceptance. Actual macOS and Windows real-binary CI remains required before release
claims. A merged foundation or successful cross check does not establish migration
readiness. PTY backend/schema eligibility, controller persistence, authenticated
service readiness/installer packaging and exact-version vendor conformance remain
separate reviewed prerequisites. PTY display projection/snapshot progress and
vendor receipts must never be substituted for durable reliable-event application.

## Hosted-session ownership (feature 16)

`SESSION_OWNER_FEATURE` is capability bit **16** in the protocol 1.1 extension.
After negotiating it, a client may send the following extension request (inside
its usual `Extended` envelope):

```json
{"op":"check_session_owner","stream":{"host":"0123456789abcdef0123456789abcdef","session":"Ada.claude","epoch":1},"hosted_id":"claude:ada"}
```

The response is `{"op":"session_owner","matches":true}` or `false`. The query
returns only a boolean; `ListSessions` and the legacy protocol shapes are unchanged.
An ownership match requires a `hosted_id` of 1–128 UTF-8 bytes. An empty query
returns `false`; values over 128 bytes are rejected during validation. The host
retains a fixed-size owner marker from `FL_HOSTED_ID` in the explicit, allowed
launch environment; inherited environment alone supplies no marker. Missing or
oversized launch markers do not establish ownership.

The host checks the full `(host instance, session ID, epoch)` against its retained
session before comparing the marker. A stale stream cannot establish ownership of
a same-name replacement. The query grants no controller lease: mutation still
requires `ClaimController` and `KillSession` on that same stream. The lifecycle
uses the exact `<Persona>.<vendor>` ID throughout, never a persona prefix.

Updated lifecycle clients preflight spin-up, restart and execute-remodel before
registry, hosted-record or route changes. A host without this feature produces an
HTTP 409 / nonzero CLI result naming `SESSION_OWNER_FEATURE` and instructing the
operator to upgrade and restart `ferroloop-host.service`. Invalid owner IDs produce
HTTP 400. No ownership request is sent to an older host.

For teardown there is one compatibility exception: a durable `runtime_spawned=true`
record permits stopping its exact recorded vendor unit on an older host without
feature 16. This preserves the existing recorded-session stop contract during
rollout. It does not permit owner adoption of a stale `runtime_spawned=false` unit;
that operation refuses before cleanup when ownership cannot be queried. On a host
with feature 16, even a recorded runtime requires matching ownership. Missing or
mismatched markers explicitly refuse recorded teardown and name the exact manual
`fl host kill <Persona>.<vendor>` command for operator inspection. Foreign or
unmarked units behind stale false records remain running. Failure to negotiate
capabilities is not treated as evidence of an old host.
