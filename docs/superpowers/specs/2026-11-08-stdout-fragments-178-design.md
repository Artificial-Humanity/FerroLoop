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

### Approved unattended reliable eviction amendment

Production sessions currently use reliable pipes without a subscriber. When no
live replay cursor or held record reference exists, admit stdout by dropping the
oldest unacknowledged data records as needed for byte/record capacity. Never drop
lifecycle records or advance `acked`. Cursor admission and eviction share the
journal lock. Replay finds retained records by actual sequence, not an ACK offset.
At each missing range, emit minor-2 `Gap { from_seq, next_seq }` before the next
retained record. A failed gap write never advances the cursor. Older surviving
lifecycle records precede their following gaps. The high-level client surfaces a
typed Gap event and does not grant ACK cursor credit for it. Old peers receive
Unsupported and close only their subscription. Live cursors preserve lossless
backpressure; a stalled live subscriber remains a documented risk. Non-reliable
output keeps existing backpressure with no eviction.

Test 100,000 LF-terminated admissions and 100,000 unterminated admissions with no
subscriber; both stay live within budget. Attach later and verify each gap plus
exact retained bytes. Separately prove active-cursor saturation retains all data.

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
