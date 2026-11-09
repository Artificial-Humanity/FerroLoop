# Rendered PTY capture

`fl host capture TARGET` captures recent raw output. `--raw` selects the same
behavior explicitly. Both preserve terminal escapes and accept `--max-bytes`
(default 16,384 trailing bytes).

Use `fl host capture TARGET --rendered` to read the visible screen of a Unix PTY
session. The host continuously projects stdout into a bounded vt100 screen, so
carriage returns, cursor movement and alternate screens remain meaningful after
raw replay has been evicted. OSC, DCS and other terminal control strings are
ignored by the projection; raw capture still preserves their original bytes.

`fl host capture TARGET --rendered --json` includes schema version 1, complete
stream identity (host instance, session and epoch), contents, zero-based cursor
coordinates, rows, columns, generation, output sequence and consumed stdout bytes.
Generation advances for nonempty output chunks and successful resizes. Sequence
and byte counts are progress watermarks, not acknowledgements or replay-resume
promises. A snapshot is coherent at capture time; later output can change it.

`--raw` and `--rendered` conflict. JSON requires rendered capture, and an explicit
`--max-bytes` conflicts with rendered capture because a byte-truncated screen
would be incoherent. Unsupported daemons and platforms refuse rendered capture;
pipe sessions refuse it as the wrong mode. There is no implicit raw fallback.

Successful cleanup releases the PTY parser and its reservation independently of
consumer Ack. Rendered capture and resize then refuse with `SessionFenced`.
Incomplete cleanup keeps the projection. The existing 64 KiB raw tail remains
available through raw capture; output still draining after cleanup can update it.
Neither release nor capture advances Ack. After cleanup, EOF and thread retirement,
spawn/status maintenance retains at most eight completed raw tails and evicts the
oldest first (also under spawn-slot pressure). Live sessions and incomplete cleanup
are ineligible. See [retention diagnostics](host-replay-pty.md#completed-pty-retention-and-diagnostics)
for eviction identity, client errors and `fl host status` output.

The shared projection allowance derives from 64 default sessions times the
conservative per-session storage bound: approximately 593 MB decimal. Reservations
update accounting counters; vt100 allocates visible rows and grows scrollback as
output arrives. The allowance covers high-water geometry, alternate screens,
geometric allocation growth and resize coexistence. It is neither an eager
allocation nor a promised process RSS bound. Non-default geometry and scrollback
can exhaust this finite allowance before all session slots are occupied.

PTY geometry defaults to 40 rows by 120 columns. The host clients' capability-gated
`resize_session` methods require the current stream identity and controller lease.
On Unix the host applies the kernel PTY resize and parser resize in the same
ordered operation, returning its generation. Invalid dimensions, stale authority
and failed ioctls leave the screen geometry unchanged. Windows PTYs, rendered
capture and resize remain unsupported pending ConPTY. Attach remains unsupported.

Acceptance coverage uses an isolated real host and a gated child with more than
8 MiB of redraws and control strings, unread/stalled replay consumers, eviction,
split escapes/UTF-8 and kernel geometry reports. The deterministic in-module pump
test separately forces replay admission failure and verifies continued projection,
raw capture and coherent watermarks; subprocess tests do not inject budget failure.
