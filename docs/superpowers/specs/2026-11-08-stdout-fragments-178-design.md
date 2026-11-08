# Bounded stdout fragments — Record 178

Status: approved by Ferris, including unattended eviction and explicit gap reporting.

## Problem and outcome

Both host output paths treat more than 1 MiB without LF as a protocol failure and
clean up the child. Terminal redraws can produce arbitrarily long runs without LF.
Such output must remain bounded and promptly visible without killing the child.
Retained output must reconstruct byte-exactly; unattended eviction must report
loss explicitly. Retain existing backlog, record, transport, and slice-budget ceilings.

## Approach

Carry `terminated: bool` through internal stdout records, reliable wire parts,
reassembly, and `HostEvent::Line`. True means the original stream contained an LF
after these bytes. False means concatenate the next record without inserting LF.
Transport `more` still means another frame belongs to the same bounded record.
Every fragment of one transport record must agree on termination.

Publish trailing nonempty output at each completed OS read, with false termination,
and split any larger admitted input into records of at most `MAX_STDOUT_LINE`.
Publish LF-delimited segments with true termination, including an empty segment
when an LF follows a previously emitted fragment. EOF never invents an LF.
CR, ANSI escapes, and arbitrary byte sequences remain unchanged. With a live
reliable cursor, quota exhaustion retains pending data and applies backpressure.
Without one, apply the bounded eviction rule below. Neither case causes cleanup.

## Compatibility

The current reliable JSON decoder denies unknown fields. The binary line decoder
rejects all flag values except 0 and 1. An additive field alone cannot preserve
old-reader compatibility for fragments. Negotiate protocol minor 2 for fragments.
Use binary header bit 1 (value 2) for unterminated, keeping bit 0 for transport
continuation. Reliable JSON defaults missing `terminated` to true and omits true
when encoding, preserving existing terminated-record encodings.

For minor 0/1 subscriptions, continue emitting existing terminated records. At an
unterminated record, refuse/close that subscription explicitly, retain unconsumed
data under the normal bounded retention rules, and leave the child alive. Never
silently add LF, discard data, acknowledge an unwritten record, or emit unsupported
frames to an old connection. New readers accept old frames as terminated records.

## Alternatives

- A new Fragment kind still requires negotiation because old enums reject it.
- A cap-only flush delays interactive output until 1 MiB; read-boundary flush is
  simpler than introducing an idle timer and provides prompt output.
- A byte-stream/PTY protocol redesign exceeds this repair's scope.

## Unattended retention and coalescing

Before this repair, reliable records retire only through `acknowledge` in
`crates/host/src/reliable.rs`; an absent subscriber does not ACK. There is no ring
overwrite or timed eviction. LF-terminated unattended sessions also reach the cap:
each LF creates another retained record. Non-reliable records retire after a completed
subscriber write and satisfaction of outstanding holders/cursors (`settle_locked`
in `crates/host/src/output.rs`). An absent subscriber does not retire data there
either. `crates/hub/src/lifecycle.rs` uses `spawn_reliable_session` for hosted
production sessions. A repository call-site search found no production hub, CLI,
or wire subscriber or journal ACK sender: `AsyncHostClient::ack` defines the API,
the daemon handles it, and tests plus `fl-host-testchild` exercise it. Wire message
ACKs are unrelated. Production capture reads `stdout_tail` and does not ACK it.

Append newly read unterminated bytes to the latest unterminated data record, up to
1 MiB, if it has never been exposed to a reader. Preserve its sequence. Freeze a
reliable record permanently before `ReplayCursor::next` exposes it, even if the
reference is later dropped without ACK. For non-reliable output, freeze before
subscriber admission; never mutate queued records or their byte-accounting values.
Use existing journal and fanout/order locking to prevent races with subscription.
Precharge replacement bytes and changed envelopes before allocation/mutation.
Coalescing must remain possible at the record cap when the tail is eligible.
Do not coalesce across an intervening lifecycle record or LF-terminated record.
Readers already attached still receive promptly flushed immutable records.

Test 100,000 one-byte read admissions with an LF-terminated read every 1,000 reads,
without a subscriber. This keeps total bytes and true line count within existing
caps while independently exceeding the read count cap. Both paths must remain
unstalled, within budget, and reconstruct exactly for a later minor-2 reader.
Also exercise a no-LF stream across multiple 1 MiB record boundaries.

## Bounded unattended reliable eviction

Coalescing alone cannot keep an unattended production session running indefinitely.
When no reliable subscriber holds a live cursor on the stream, and admitting the
next record would exhaust the data-record cap or byte budget, retire the oldest
unacknowledged data records until bounded admission can proceed. Never evict a
lifecycle record, advance the acknowledged watermark, or claim delivery of evicted
data. Preserve lifecycle records even when retained data sequence numbers have gaps;
do not assume the retained journal remains contiguous after eviction.

Record the loss explicitly. A later subscriber must receive a gap/notice naming
the first sequence it can still receive, before replaying retained data. The reader
must be able to resume at that sequence and reconstruct the retained bytes exactly.
Do not turn a gap into a silent cursor jump or acknowledge data on the reader's
behalf. Protocol changes needed for this notice belong to negotiated minor 2.

While any live reliable cursor exists, preserve lossless backpressure and do not
evict data. Cursor admission, release, eviction, and record exposure must be
serialized so a newly attached reader cannot race eviction. A stalled live reader
can still stall the producer; this is a known risk deliberately outside this fix.

Hard gates: no subscriber and 100,000 LF-terminated plus unterminated admissions
must never stall the reliable producer while record count and byte charges stay
bounded; a later subscriber receives a gap then exact retained bytes; a live
subscriber still receives lossless backpressure. Exercise both record and byte
pressure, preserved lifecycle records, and unchanged ACK state after eviction.
The non-reliable path retains its existing delivery guarantees and the coalescing
and 100,000-read tests above; this eviction policy applies to the reliable journal.

## Validation and boundaries

Exercise more than 2 MiB without LF through both actual host paths, reconstruct
exact bytes, observe no ProtocolFailure, and prove the producer remains alive.
Cover LF at a cap/read boundary, empty lines, CR/ANSI, binary bytes, EOF tails,
transport multipart termination consistency, saturated queues, and reliable replay
with ACKs. Test legacy decoder compatibility for unchanged frames and explicit
subscription-only rejection for unsupported fragments. Trace all consumers; update
any byte reconstruction to add LF only for terminated records.

Use isolated fixtures, not live vendors. Do not install or restart services in
this change. Report shared Claude-state contention separately; do not fix it here.
