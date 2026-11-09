# PTY projection capacity and completed retention (#408)

Status: bounded PTY retirement approved by Ferris on 2026-11-09; this document
records his capacity, observability and locking conditions before implementation.

## Outcome and approaches

Default PTYs must fit the advertised 64-session capacity, and an unattended exited
PTY must not reserve its parser forever waiting for consumer acknowledgments.
Projection release alone would move the unattended failure from 14 sessions to 64
retained epochs. Lazy projection reservation changes allocation safety substantially.
Ferris selected explicit projection release plus bounded completed-PTY retirement,
with a default capacity allowance derived from the advertised session limit.

ReliablePipe and LegacyPipe retain their existing retention and acknowledgment
contracts. No live agent launches, deployment, service restart or UI work.

## Capacity and allocation

Keep pre-allocation accounting, deriving global projection admission from
`DEFAULT_MAX_SESSIONS * storage_bound(PtyOptions::default())`. This is approximately
593 MB decimal, not an eager allocation. `Reservation` only updates counters;
vt100 allocates visible rows and grows its scrollback as output arrives. The bound
includes primary/alternate screens, high-water scrollback, geometric allocation
growth and resize coexistence. It is an upper accounting allowance, not a promised
RSS limit. Document both that distinction and the possible worst-case charge.

Increase the aggregate additional allowance consistently by the projection-budget
delta, preserving the previous non-projection headroom. Retain per-category checks
and checked arithmetic. Derive the default, do not duplicate a magic 593 MB value.
No new configuration is required for an accounting-only reservation; if inspection
shows the full amount is actually allocated eagerly, stop and apply Ferris's
requirement for a configurable host allowance before proceeding.

Tests prove all 64 default projection admissions fit, the next exceeds admission,
and reservations return to baseline. Real daemon tests exercise at least 16 live
default PTYs. Non-default geometry can exhaust the finite shared bound; expose
current use/limit in host status and actionable client errors directing the operator
to inspect status, reduce geometry/scrollback, or stop unused live sessions.

## Completed PTY policy

At a successful CleanupComplete, drop the parser and its projection reservation
under the projection mutex, independent of Ack. Incomplete containment never
qualifies. Stop rendered capture/resize with an explicit completed/fenced result;
raw capture remains available as a final bounded tail. The existing 64 KiB raw
tail is the retained display artifact, not a fabricated replay acknowledgment.
The stdout pump may still finish draining after cleanup and update that raw tail.

After cleanup, EOF and epoch thread retirement, compact completed PTY journal
ownership and retain at most eight completed PTY epochs (at most 512 KiB raw tails,
plus bounded bookkeeping). Retire oldest completed PTYs first on retention pressure
or spawn-slot pressure. Do not evict live sessions, incomplete cleanup or pipe
epochs. Completed PTYs must not prevent admission up to the live-session cap.
Normal status/spawn maintenance enforces this bound without an unbounded background
worker or a new timer. Cleanup releases projection immediately even before that
maintenance runs.

Completed PTY compaction/retirement cancels its authority and releases pending
tickets/streams; it never advances Ack. Actual outstanding Arc-held buffers retain
their reservations until the final reference drops. No accounting is reset merely
because an epoch is removed from inventory. Small in-flight references are bounded
by existing connection/operation limits; tests exercise held references and races.

## Observable eviction and wire compatibility

Keep a cumulative eviction count and a bounded recent history of 64 full StreamIds
(host instance, session, epoch), and log each eviction with that identity. Host
status exposes the count, recent history, completed-PTY count and projection use/
limit through a new capability-gated diagnostic query. Existing HostStatus and
SessionSummary shapes remain unchanged because their decoders reject unknown fields.
Old hosts return diagnostic availability as unavailable, never invented zeroes.

Modern high-level clients use the bounded diagnostic query to turn a stale-epoch
refusal for a known evicted epoch into an explicit `evicted` error naming the
identity. Unknown/older-than-history epochs remain explicitly stale/possibly evicted;
never falsely claim a specific eviction. Preserve existing on-wire refusal variants
for old clients: do not send a new undecodable enum variant on old requests.
Raw protocol clients can query the negotiated diagnostic explicitly. Document this
boundary and test that old clients never receive a new response shape unsolicited.

## Lock and reference ordering

Projection release and output processing serialize on the session projection
mutex. Do not hold the cleanup mutex while taking projection/journal or authority
locks; publish state and drop its guard before downstream notifications. A pump
which has already taken the projection lock finishes first; later output cannot
recreate a parser.

Serialize admission/retention through the existing spawn gate. Preserve the
existing registry ordering: output/epoch selection, then authority state, then
journal/session locks. Recheck cleanup/EOF/thread eligibility under the final
authority fence before detaching. Admission of claims/tickets must not race an
epoch out of inventory; subsequent requests see the recorded eviction.

Cancel and detach pending objects under authority state, but drop ReplayCursors,
StreamGuards, final-write waiters and other re-entrant objects after unlocking
authority state. Existing in-flight references keep their actual memory charges.
Never wait for IO or join a live thread while holding registry, journal or
projection locks. Reuse existing cancellation/final-write settlement mechanisms.

## Verification

Private real-daemon fixtures: more than 14 concurrent default PTYs; more than 64
sequential exited PTYs without a subscriber or Ack; stable bounded completed tails;
projection budget returning to baseline; oldest-first eviction count/identity;
late readers getting an explicit eviction diagnosis. Assert final raw tail bytes,
unchanged Ack, incomplete-cleanup non-eligibility and live/pipe non-eviction.
Deterministic unit race tests cover outstanding references, cursor/ticket admission,
cleanup versus output, and cancellation without deadlock. Client/CLI fixtures cover
status JSON, unavailable older hosts and actionable capacity errors.

Use ordinary CI prerequisites, isolated endpoints and synthetic children. Run the
workspace test/Clippy/fmt trio and independent task/final reviews before a PR.
