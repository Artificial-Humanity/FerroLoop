# Bounded stdout fragments — Record 178

Status: approved by Ferris with the retention amendment below.

## Problem and outcome

Both host output paths treat more than 1 MiB without LF as a protocol failure and
clean up the child. Terminal redraws can produce arbitrarily long runs without LF.
Such output must remain bounded, promptly visible, and byte-exact without killing
the child. Retain existing backlog, record, transport, and slice-budget ceilings.

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
CR, ANSI escapes, and arbitrary byte sequences remain unchanged. Quota exhaustion
retains pending data and applies backpressure; it must not become child cleanup.

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

Reliable records retire only through `acknowledge` in `crates/host/src/reliable.rs`;
an absent subscriber does not ACK. Non-reliable records retire after a completed
subscriber write and satisfaction of outstanding holders/cursors (`settle_locked`
in `crates/host/src/output.rs`). An absent subscriber does not retire data there
either. Preserve these guarantees; never introduce silent eviction or implicit ACK.

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
Also exercise a no-LF stream across multiple 1 MiB record boundaries. Finite
retention still backpressures when actual byte/line limits are exhausted: arbitrary
unacknowledged output or more than 4096 distinct LF records cannot be retained
indefinitely under the existing contract. This change does not promise otherwise.

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
