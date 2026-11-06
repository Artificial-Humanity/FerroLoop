# Issue #361 — bounded read-only delivery-fence evidence (proposal)

Status: Freya proposal for Ferris selection/review, NOT implementation GO. Design only; no recovery API. Companion #360 and held Record143 stay separate.

## Source and intent

Audited running Wire a79bc3e3f4adb8ae699c30d63bf2ee90b230b4d5. Cached origin/main 44e1bdfc25baff65e6fbbe35d6fad9f83046c66e has byte-identical daemon.rs, identity.rs and cli/cmd/wire.rs; peer dispatcher moved to dispatch_message_selected at line2340 after operator-only additions. Recheck actual main before any later implementation.

Today dispatcher.rs dispatch_message_selected throws away selected route and transport Result detail; update_wire_receipt failures are ignored, and daemon.rs dispatch_and_propagate maps failed/missing receipt reads to Unconfirmed. run_recipient_queue also folds get-message errors/panics into Unconfirmed. Recipient suppression and overflow share a BTreeSet capped at128; no status evidence exists. Status only clones current peers/routes (daemon.rs1053); CLI serializes it (cli/cmd/wire.rs1142). Read/ACK and registration never release the recipient fence.

Goal: distinguish failure stages and show why future automatic pushes are suppressed, without changing delivery, receipt, retry, ordering, authorization or suppression behavior. These diagnostics cannot establish model consumption, trusted nonmutation, or recovery eligibility. Historical Kayla cause remains unknown; new fields cannot reconstruct it.

## Alternatives and recommendation

1. Logs only: smallest patch, but callers cannot inspect current holds and logs may be absent/expired.
2. Bounded in-memory evidence + additive Status capability + sanitized log: RECOMMENDED. Matches existing lifetime fences and needs no store format or migration.
3. Persist an authoritative hold/attempt ledger: not this issue; overlaps Record143 and changes restart/legacy-writer semantics. Reject for this slice.

## Proposed contract (choices pending Lead selection)

Internally return a diagnostic report alongside the unchanged receipt/attempt result from a new daemon-focused dispatch helper. Keep existing public dispatch wrappers' behavior/signatures. Capture evidence at the actual boundaries, not by parsing stderr or issuing additional probes/reads. Never add a retry to obtain evidence. Existing route refresh/retry behavior is recorded, not enlarged or claimed safe by this change.

Keep first failure and subsequent bounded failures distinct: message lookup failed/missing; initial receipt lookup failed; transport returned error; receipt update failed; final receipt lookup failed/missing; unresolved receipt status; worker panic. Separately classify queue overflow, worker-spawn failure and global metadata-cap saturation. Every diagnostic is observational; all transport errors remain mutation-unknown. No raw panic payload or vendor/store error string is emitted in new diagnostic fields.

For each held recipient, retain only: validated normalized recipient when it fits the existing slug cap64; otherwise an explicit unknown/invalid-recipient marker plus a fixed-size digest of the original receipt name, without echoing raw unbounded text or changing the actual delivery/suppression key; trigger message ID when <=256 bytes (otherwise omitted plus digest and explicit truncation flag), fence time captured when hold is created (not the job's enqueue timestamp), reason enum, bounded failure-stage list (max4, retaining the decisive final hold cause even if intermediate events are omitted, with an omitted count), up to2 route-dispatch observations with a saturating omitted-observation count and saturation flag, and suppression scope `daemon_lifetime`. Preserve original first-hold evidence rather than replacing it with later ACK/route observations. Route observations contain adapter kind, domain-separated digest of nonsecret session identity, route updated_at snapshot and explicit flag when known; null/unknown for unavailable observations. Never serialize entire VendorRoute into this evidence: it can contain CSRF/key paths. Do not include message body/subject, argv, cwd, socket/key paths, stdout/stderr, tokens or arbitrary exception strings.

Existing queue.route_generation is a wake counter, not a registered session generation. DO NOT call it authority/session generation. Expose `registration_generation: null` until one actually exists. Identity digest identifies the attempted snapshot; it proves neither liveness nor authorization. Record the actual `route_dispatch_calls` in a saturating fixed-width counter with an explicit saturation flag, independently of the two-observation storage cap. The dispatcher loops over all matching same-name receipts, so total calls are not necessarily limited to two; omitted observations are reported separately. This is not a vendor-process/emission count: one dispatch_to_route can have internal binary fallbacks, and one receipt attempt can include refreshed-route dispatch. Missing detail is explicitly unknown.

Keep evidence and fence insertion atomic under the existing queue/fence lock order. Do not hold these locks across vendor calls, store callbacks, log I/O or serialization. Store at most128 recipient records, with a tested <=4KiB serialized diagnostic-row bound. If evidence storage fills, retain existing fail-closed suppression/global saturation flag; expose a saturating omitted-evidence count and global-cap reason, never evict a fence to retain diagnostics. Status clones the bounded snapshot, drops locks, then serializes; it does no discovery, store reads/writes, queue maintenance or transport calls.

Add optional `dispatch_diagnostics` to DaemonStatus, with serde default None, a schema version1 and instance-local nonce/time. Some(empty) means supported/no observed holds; None means unavailable/old daemon/offline fallback, never 'no holds'. Old clients ignore the extra field; a new client decoding an old response must preserve unavailable. CLI human status gives a count/reason summary; JSON exposes the bounded records. Diagnostic output follows current private local socket access; no new remote/public endpoint or identity authority. No changes to existing route output are implied.

## Persistence/API decisions for Ferris before implementation

- Recommend volatile diagnostic lifetime plus journal copy, NOT store persistence or format allocation. Restart loses the in-memory diagnostic snapshot; explicitly state that, without treating restart as safe recovery.
- Recommend additive optional Status field rather than a new RPC or endpoint. Capability absence must be distinguishable from an empty supported set. Confirm this is the desired public JSON surface.
- Recommend enum-only cause details. If raw error excerpts or durable history are desired, separately design redaction/retention rather than adding truncation-only 'sanitization'.
- True registration generations do not exist here; approve snapshot identity + unknown generation, or scope a separate authority change. Never invent one from timestamps.

## Isolated regression matrix (proposed, not run)

Use temporary stores/HOME/routes and fake local transports; no real vendor binaries or live sockets. Inject stage-specific store faults in cfg(test) wrappers, not production behavior seams.

1. Failed fake transport produces transport-stage evidence and recipient hold; later Send stores, attempts=0, no further transport call.
2. Fake transport receives original, then receipt update fails: evidence shows transport returned OK + receipt-write failure; retain unknown mutation and no resend.
3. Fake transport and receipt update succeed, then final receipt read fails: show receipt-read cause even if later snapshot is ACKed; no false transport-failed claim.
4. Initial receipt read fails before any route dispatch; missing message and worker panic are distinct bounded categories, without granting retry permission.
5. ACK/Read(ack:true), registration and route changes leave the original hold and evidence unchanged; unrelated recipient remains independent.
6. Fake route refresh leads to two captured dispatch observations but one receipt attempt; do not claim actual emission count or duplicate safety.
7. Read-only Status and repeated formatting cause zero vendor calls, writes, ACKs, scans or fence changes. Inject secrets/control text in errors/routes and prove absent from new fields/logs.
8. Cap128/max-row limits, global saturation, oversized IDs, invalid/oversized imported recipient names, multiple same-name receipts, observation/counter saturation, missing observations and old/new JSON fixtures preserve truthfulness and existing suppression. Logs/status do not overwrite the initiating cause.

Verification after later GO: meaningful failure-before/fix-after diagnostics tests; existing daemon/dispatcher + CLI status regressions; fl-wire and affected CLI checks/clippy/fmt. Repository landing checks per WORKFLOW. No recovery/unfence/restart/replay/Record143 implementation or runtime rollout is included.
