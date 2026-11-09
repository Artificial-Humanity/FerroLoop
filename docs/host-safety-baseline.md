# Host safety baseline

Record 183 characterizes capture, delivery pressure, build identity, and current
child devices. It preserves the existing delivery contracts. After integration with Record 184,
first-class Unix PTY support retains its nonblocking I/O design.

## Delivery pressure

The `safety-flood` test child emits 4,608 individually numbered records, each
2,048 bytes before its newline (9 MiB of payload). Every record includes a
carriage return and ANSI color/reset sequences. A distinct final marker follows
the records; a completion file is written only after stdout flush succeeds.
A producer gate and subscriber barriers establish ordering without timing sleeps.
Polling uses finite deadlines.

| Path | Pressure behavior | Evidence |
| --- | --- | --- |
| LegacyPipe, no subscriber | Retains never-written bytes and stalls at the bounded backlog; a subscriber can drain the records and resume the producer | Complete ordered byte comparison, final marker, fresh capture, cleared stall flag |
| LegacyPipe, held subscriber | A slow subscriber can be shed; undelivered bytes remain, and the producer eventually stalls | Backlog charge/count bounds, responsive list/capture/lease/kill, `CleanupComplete` within five seconds without releasing the subscriber barrier |
| ReliablePipe, no subscriber | Evicts DATA when necessary and continues producing; does not advance ACK state | Final capture, explicit replay gap, retained ordered records and Spawned lifecycle, zero ACK credit |
| ReliablePipe, held replay cursor | Backpressures the producer while replay retains its obligations | Stall observed before producer completion, release and ACK recover every numbered record without gaps, fresh final capture and cleanup |
| Sinkless internal pump | Capture retains the newest bytes within its cap | Module tests exercise over 8 MiB through the actual Unix pump; direct capture tests also cover oversized writes and repeated overflow |

Subprocess tests live in `crates/host/tests/host_ipc.rs` and
`crates/host/tests/reliable_events.rs`. They assert the Legacy backlog stays at
most 4,096 events and 8 MiB charged bytes. Reliable unattended replay checks at
most 4,096 retained DATA records and 8 MiB payload; internal journal and slice
budget tests cover charged allocation accounting. On Linux the flood scenarios
also check each isolated daemon's RSS stays below 128 MiB. This is a regression
ceiling, not a memory-usage guarantee or a peak-memory benchmark.

The four subprocess scenarios are portable test definitions; the local baseline
was exercised on Linux. Windows execution remains a CI responsibility. A public
`Host::spawn` always attaches a delivery sink, so it is not evidence for sinkless
behavior. No Windows sinkless subprocess result is claimed.

An unattended LegacyPipe producer can remain stalled indefinitely until a
consumer progresses or cleanup is requested. That is the intentional lossless
contract, not an unattended-progress guarantee. List/capture and containment
cleanup must remain responsive during the stall. ReliablePipe also intentionally
stalls with an active cursor that cannot progress. Neither mode fabricates ACKs.

LegacyPipe admission distinguishes a fresh publication from a catch-up pass.
The first fresh record refused by a subscriber's 64-record / 2 MiB queue starts
one second of overflow grace. Deferred records stay in the existing shared
backlog, so a brief producer burst can drain losslessly. Successful writes do not
renew the episode: it resets only after the queue falls below both 32 records
and 1 MiB and its cursors have caught up to the published records. A permanently
slower consumer can still be shed.

The shared 4096-record / 8 MiB limit takes priority over grace. Capacity pressure
ends affected subscribers' graces immediately; held references stay charged
until released, and bytes nobody wrote remain retained. Pressure from one session
can end an unfiltered subscriber's grace, even when it also follows other sessions.
The existing LegacyPipe producer pacing remains: each record yields for at most
20 ms while a subscriber is backlogged, without holding fanout/order locks or
waiting on socket I/O. Grace applies in addition to this bounded pacing; there is
no unbounded subscriber wait. A shed viewer gets a best-effort `SlowSubscriber`
notice and disconnect; the notice can be its first frame, and it does not receive
a fabricated complete stream. Notice
and data writes share an absolute cutoff of grace expiry plus the existing
five-second transport timeout (or pressure-triggered shedding plus that timeout).
Fragmentation and the notice cannot restart this allowance. Terminal-slot
refusals remain immediate. PTY, ReliablePipe journal and capture behavior are
unchanged.

Injected-clock module tests cover burst reconstruction, nonrenewing episodes,
expiry without another publication, both capacity limits, multiple subscribers,
and the notice/fragment disconnect cutoff, including expiry during the final write
with an empty queue. A deterministic reader-like 16,384-record flood verifies that
bounded pacing preserves a slower draining subscriber's exact sequence. The
original unattended flood subprocess regression remains unchanged.

## Build identity and device truth

Build metadata is obtained from the serving daemon through the advertised,
opt-in host-status extension: package version, commit, dirty flag, and build
time. Capability and session-list response shapes remain unchanged for strict
older decoders. Clients report identity unavailable when an old host does not
advertise the extension; they do not substitute their own build identity.

For LegacyPipe and ReliablePipe, ordinary Pipe launches use nonterminal child
stdio. On Unix, the existing `FL_HOST_PTY=1` opt-in uses an actual PTY while the
reported kind remains Pipe and the mode remains the requested pipe mode. The
child `IsTerminal` probe in `pipe_modes_report_current_device_truth` exposes this
mismatch. Windows does not implement that environment-selected PTY behavior;
this baseline does not claim ConPTY support. Explicit `SpawnKind::Pty` with
`SessionMode::Pty` succeeds on Unix, reports kind and mode as Pty, and passes the
child terminal probe. Non-Unix hosts refuse that request with `Unsupported`.
Capabilities advertise reliable delivery and host status on both platforms, plus
PTY support on Unix.
