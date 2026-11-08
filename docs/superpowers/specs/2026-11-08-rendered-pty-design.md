# Rendered PTY capture and ordered resize

Record 185 implements Milestone 3 of the Owner-approved Council plan, as assigned
by Ferris on 2026-11-08. The assigned deliverables select a pinned in-memory vt100
parser, opt-in rendered capture, ordered Unix resize, and an explicit Windows
ConPTY gate. The following implementation limits are Fiona's engineering choices.

## Intent and scope

Interactive TUIs redraw existing cells with carriage returns, cursor movement,
alternate screens and ANSI sequences. A trailing raw-byte buffer cannot reconstruct
those cells after earlier escape transitions disappear. Every first-class Pty
session therefore owns a persistent screen projection, fed before replay admission
or eviction of the corresponding output. LegacyPipe and ReliablePipe retain their
existing delivery and capture contracts. No consumers switch capture modes implicitly.

This change does not implement ConPTY, migrate hub/vendor callers, or introduce an
Attach stream handoff. Epoch-scoped rendered snapshots and existing Observe/gap
semantics are available independently. The broader Council Attach item needs a
separate snapshot-to-replay contract; its reserved request remains Unsupported.

## Approach

The assigned persistent parser is the only approach that preserves terminal state
after replay loss. Parsing a retained tail would lose escape state; reconstructing
on capture would also repeat work and miss evicted bytes. A session-local mutex
orders bounded projection updates, snapshots and resize. No output read/poll or
network write holds that mutex. The existing stdin control worker serializes resize
authorization with controller takeover and disconnect, avoiding a new worker.

Pin `vt100 = "=0.18.1"` in fl-host and commit Cargo.lock. This release has fixed-size
32-byte cells with at most 22 content bytes, supports NonZeroU16 geometry and
bounded scrollback, and is MIT licensed. Source inspected from the published crate:
https://docs.rs/vt100/0.18.1/vt100/struct.Parser.html.

## Limits and terminal input

- Initial geometry is 40 rows by 120 columns; explicit valid PtyOptions override it.
- Maximum geometry is 128 rows, 256 columns, and 8,192 cells; all dimensions are nonzero.
- Scrollback defaults to 512 rows and is capped at 512; zero is allowed.
- Reserve Projection storage from the journal's epoch SliceBudget before allocating
  the parser or spawning its child. Include both screens, scrollback, row/vector
  overhead and resize peak storage. Retain a conservative high-water reservation
  after shrinking so retained capacities stay charged. Growth admission precedes
  both ioctl and parser resize; refusal changes neither geometry.
- A constant-state streaming filter discards OSC, DCS, SOS, PM and APC strings,
  including split introducers/terminators, until ST (or BEL for OSC), CAN or SUB.
  Payload storage is zero. Track UTF-8 continuation state so bytes inside a valid
  multibyte character are not mistaken for standalone C1 controls. Normal CSI,
  cursor controls, alternate-screen transitions and printable UTF-8 reach vt100.
  Unterminated strings remain ignored without growing memory; termination restores
  ordinary parsing. This projection does not execute clipboard, title or terminal
  query callbacks. Raw capture still receives the original bytes.
- Bound text and JSON response allocation. At most 196,608 rendered text bytes are
  permitted; justify the serialized bound against MAX_FRAME_PAYLOAD (256 KiB),
  including escaping and metadata. Refuse an oversized snapshot rather than silently
  truncating its screen. Use Snapshot/metadata reservations through response write.

## Ordering and identity

Both normal journal reads and the Pty fallback scratch-read path feed the projection
exactly once before accept_read/drop_pty_unadmitted. Prior replay eviction in
prepare_read only removes bytes already projected. Within the projection lock,
process the new chunk, preserve the existing bounded raw tail, perform its journal
admission/drop, and update a coherent watermark. Do not wait for journal space.

A snapshot has schema 1, its complete StreamId, contents, zero-based cursor row and
column, rows and columns, generation, output_seq, and output_bytes. Generation starts
at zero and advances on each nonempty processed chunk and each successful resize.
output_bytes counts original stdout bytes consumed, including discarded string
payloads. output_seq is the journal's latest sequence after that chunk's admission
or drop; it is a progress watermark, not an ACK or an exact replay-resume promise.
Counters must not wrap. Capture, resize, and updates observe these fields under the
same mutex. An old host instance/session epoch cannot select a replacement session.

Unix resize applies TIOCSWINSZ to a dedicated duplicate of the PTY master and then
Parser::set_size under the projection lock. A successful response identifies that
linearization point, even if a later output chunk/resize changes the screen before
the response reaches the caller. Keep the descriptor separate from the stdin file
taken by WriterHandle. Revalidate connection, lease, epoch and session liveness at
execution; keep the lease fence through the bounded mutation. Use the existing
bounded control queue and deadline. Failed ioctl leaves parser size/generation intact.

## Opt-in protocol and CLI

Add feature bit 8 (`RENDERED_PTY_FEATURE`) on Unix only; retain existing feature bits
and exact Capabilities/Sessions/raw response shapes. New request CaptureRendered
contains StreamId; new response RenderedCapture contains a strict RenderedScreen
structure with the fields above. Async and blocking capture_rendered clients check
the capability first and return Unsupported without sending an unknown request to
an older daemon. Pipe sessions refuse rendered capture explicitly.

Enable the reserved Resize {stream, lease, cols, rows} and
Resized {cols, rows, projection_revision} shapes, with projection_revision equal to
the successful resize generation. Add capability-gated async/blocking resize_session
helpers. Windows refuses valid Pty spawn, resize and rendered capture with Unsupported;
pipe-mode resize on Unix returns WrongMode. Invalid dimensions are rejected before
allocation or mutation. Old decoders receive none of these responses unless asked.

`fl host capture TARGET` and `--raw` retain existing output and max-bytes behavior.
`--rendered` prints screen contents, and `--rendered --json` emits the full snapshot.
Raw/rendered are mutually exclusive. Reject --json without --rendered and explicit
--max-bytes with --rendered; never pretend a byte limit truncates a coherent screen.
Do not add an unrelated interactive attach UI or change existing callers.

## Evidence

Unit tests cover split escape/UTF-8 sequences, primary/alternate screens, control
string floods, bounds, budget refusal and release, monotonic metadata and atomic
failed resize. Real subprocess tests emit more than 8 MiB of redraws, verify clean
screen cells/cursor with a bounded raw tail, exercise replay eviction and the
admission-failure drain path, and verify child TIOCGWINSZ agrees with parser geometry.
Test stale epochs, lease takeover/disconnect, pipe refusals, native Windows gates,
strict old wire shapes, capability fallback and CLI raw compatibility. Use gates
and condition polling, not timing assumptions. Existing Record190 remains separate.
