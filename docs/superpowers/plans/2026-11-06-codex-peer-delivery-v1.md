# Codex Peer Wire Delivery v1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build disabled-by-default local Codex peer delivery with durable cross-lane arbitration, byte-bound one-use handoffs, and
indefinite suppression after uncertainty.

**Architecture:** Add a pure delivery model and transactional store contract, then complete persistence and mutation fences before
connecting a local broker to push and hook lanes. The existing daemon issues connection-bound grants; immutable messages and durable fences
determine eligibility independently of compatibility receipts. Ship inspection and isolated conformance coverage with policy disabled.

**Tech Stack:** Existing Rust workspace (edition 2024, rust-version 1.98), fl-core/fl-store/fl-wire/fl-cli, redb and ferro transactions,
serde, SHA-256, ring randomness, Unix peer credentials; no external database or new daemon.

**Spec:** [Approved design snapshot](../specs/2026-11-06-codex-peer-delivery-v1-design.md). Read it together with this plan.

## Global Constraints

These selected contract lines and bounds are copied verbatim from the approved specification; the full specification remains binding.

- Local Codex peer delivery, separate from Owner and Host
- One participating automatic handoff; uncertainty holds indefinitely
- First successful prepare wins
- Hook v1 is PostToolUse additionalContext only, one message/invocation
- Only trusted configured local node `ai-lab-0` issues permits
- Stable registry identity; unresolved names and alias conflicts hold
- Separate evidence and manual acknowledgment
- Indefinite compact suppression, separate body retention
- Ship disabled with format/RPC/old-writer fences
- No direct-store claim fallback, remote permit, automatic home selection, relocation API, or failover.
- **v1 initial allow-list is empty**; current -32600/-32601/-32602 source classification alone cannot reopen.
- 3 lifetime prepares/key, including crashed/refused/transiently failed preflight; persisted, never reset by restart/ACK/admin. One prepare
  and one potential handoff per invocation; later explicit arrival/hook/route event may request another safe prepare. No timer retry loop.
- One spawn or one steer frame or one stdout response per armed attempt. No child/steer probing/model turn to investigate delivery.
- Existing subject512/body262144 bytes; managed identity/path fields each4096 bytes; hook stdin64KiB; envelope512KiB; encoded hook JSON or
  steer JSON1MiB (steer frame +14 bytes). Oversized existing/imported data stays readable but held for automatic delivery.
- Local RPC operation5s with bounded connect; prepare→arm lifetime20s; steer connect/preflight/ack total10s; queue runtime60s; push
  invocation total85s. Hook invocation20s, including stdin2s/stdout2s. Remaining deadline bounds every operation; after arm any timeout
  holds. Prepared expiry is CAS invalidation, never an armed lease.
- 16 managed active attempts total, 1/recipient; max64 managed protocol connections; retain existing daemon128 dispatch workers/512 tracked
  recipients/1024 pending IDs per recipient/128 overflow-recipient bounds. Managed limits never raise these. Full capacity refuses prepare;
  overflow keeps persisted inbox/fence and does not spin.
- At most3 attempt summaries; each≤8KiB, total ledger row≤32KiB excluding frozen payload. Error excerpt≤300 chars; annotation
  aggregate≤4KiB. Overflow sets held flag without deleting identity/digest/suppression. Frozen payload≤1MiB+14; one active copy/key.
- Compact ID/digest/recipient/authority/outcome reservations indefinite; no finite expiry reopens them. Body/frozen-payload retention
  follows reviewed protected policy; unresolved bodies are not automatically pruned. Delivery adds no autonomous retry or inference-probe
  loop and invokes no turn/start RPC. Delivered input may cause model work; preserve applicable existing budget checks without claiming
  native sessions are universally gated.

Additional execution constraints: preserve #150 restrictions, the original operator source tree, and Host separation. No install,
hook/wrapper edits, live probes, queue purge, backlog reinjection, service restart or activation is authorized by this plan. Format **10
only if still unallocated at implementation review**, otherwise obtain review of the next value. Already-open old writers require separate
process fencing; open-time version refusal cannot fence a previously armed process. No native-session universal spend-gate claim is
permitted.

## Review Focus

1. Lost arm reply, duplicate arm, or copied/reconnected handle must produce no new output permission: `managed_lost_arm_reply_never_emits`
and `managed_connection_handles_are_nontransferable` (Tasks 7, 11). 2. Recycled persona names, receipt aliases, and conflicting IDs must
never redirect an old message: `managed_identity_alias_conflict_holds` and `managed_prune_identical_reimport_stays_suppressed` (Task 5). 3.
JSON escaping/duplicate keys, transcript replacement, and unsupported hook events must expose no managed text:
`managed_hook_strict_identity_and_bytes` and `managed_hook_disabled_events_emit_empty` (Task 9). 4. Idle-to-active races, binary
replacement, or post-write mismatched RPC replies must not fall back or retry: `managed_push_idle_race_has_one_handoff` and
`managed_steer_ambiguous_reply_holds` (Task 8). 5. Stale restore, missing protected collections, and raw/session/sync mutation bypass must
close delivery: `managed_restore_never_resumes` and `managed_all_mutation_surfaces_fenced` (Tasks 5, 6).

---

## Provenance and authorization boundary

Planning baseline: `origin/main` / HEAD `3e9df03d24542c366beef15085535c1c6bd5382b`, including merged operator PR #345. Worktree: branch
`freya/codex-peer-delivery-plan-143`. The operator ordering prerequisite is satisfied; this work requests no further operator changes.

The snapshot is a byte-identical archival copy of the approved `freya-peer-delivery-143-v1-design.md`, SHA-256
`e6dbda0a4f19ccb3e0b461e6908423cf6db4fac05dc089e22319cd367ec8d801`. Its candidate/pending wording is preserved as historical text. Final
Lead approval `urn:uuid:01a1aedf-0a0c-7557-97a5-6ba0df996a1a` supersedes that pending label **for design approval and planning only**. It
grants no peer implementation, execution or activation GO. This document drafts future work; its checkboxes are intentionally unchecked.

The source baseline below was rechecked read-only after #345. The snapshot's older source table is historical evidence, not evidence of
current installed behavior. No live daemon, Codex session, hook, queue or runtime capability was probed, and no product test was run while
authoring these documents. Private working notes are excluded from these public artifacts.

## Current source map and planned file boundaries

All current references below mean revision `3e9df03d24542c366beef15085535c1c6bd5382b`; line numbers are navigation anchors, not future patch
offsets.

| Current path:symbol:line | Current behavior / planned ownership |
| --- | --- |
| `crates/core/src/ferrowire.rs:WireMessage:217`; `store.rs:MessageStore:830` | Immutable envelope/receipt API has no delivery CAS. Preserve strict schemas and add a separate `wire_delivery` domain. |
| `crates/core/src/mem.rs:MemStore:42`; `:MessageStore:1827` | `RefCell<Inner>` reference implementation; one mutable borrow per atomic operation, no existing mutex. |
| `crates/core/src/agent.rs:AgentRecord:200`; `:AgentRole::as_wire:134`; `at.rs:At::as_str:83` | Canonical registry IDs, role spellings and fixed 24-byte timestamps. |
| `crates/store/src/lib.rs:FORMAT_WITH_AREA_DELETE:168`; `:FORMAT_LADDER:173`; `:check_format:185` | Latest schema 9; candidate schema 10 is free only at this baseline. |
| `crates/store/src/ferrowire.rs:MessageStore:427`; `:update_wire_receipt:750`; `:import_wire_batch:898`; `:prune_wire:1056` | Native redb transaction integration, receipt projection, import/prune fences. |
| `crates/store/src/ferrostore/traits_wire.rs:MessageStore:162`; `:update_wire_receipt:386`; `:import_wire_batch:487`; `:prune_wire:602` | Native ferro equivalents. |
| `crates/store/src/any.rs:forwarding macro:1140`; `:MessageStore forwarding:1249`; `ferrostore/migrate.rs:copy_tables:49` | Forward new trait; extend explicit migration and verification. |
| `crates/store/src/ferrostore/doc.rs:is_reserved:70`; `mod.rs:put_doc:2194`, `:delete_doc:2258`, `:commit_if_seq:3538`, `:apply_sync_batches:4169` | Existing `__` fences do not protect dependent ordinary collections; add typed mutation admission. |
| `crates/store/src/ferrostore/transaction.rs:put_doc:42`; `session.rs:commit_txn:318`; `server.rs:commit_txn:2101`, `:apply_sync_batches:2222` | Public transaction/session/raw/remote/sync boundaries all need fences. |
| `crates/wire/src/daemon.rs:LocalRequest:202`, `:LocalClient:277`, `:DaemonConfig:352`, socket bind `:1416`, request accept loop `:1491` | Ordinary local RPC is single-request and lacks managed UID/phase semantics; add a separate negotiated persistent path. |
| `crates/wire/src/dispatcher.rs:format_push_header:37`, `:format_push_message:49`, `:run_codex_queue:875`, `:OperatorPromptIntent:182` | Pin existing peer envelope, avoid candidate/spawn-retry helper for managed queue, preserve operator API semantics. |
| `crates/wire/src/codex_control.rs:steer_active_turn:242`, `:STEER_TIMEOUT:32` | Split managed read-only preflight from frozen final steer frame; retain operator steering path. |
| `crates/cli/src/cmd/wire.rs:execute_read:622`, `:ack_messages:655`, `:run_vendor_hook:1253`; `config.rs:WireConfig:299` | New explicit Codex profile and trusted config; managed reads/ACK/claims cannot use store fallback. |
| `crates/exec/src/state_sync.rs:sync_state:801` | Sync carries messages/receipts, never local handles or permit authority. |

Proposed new files are focused units: `crates/core/src/wire_delivery.rs` (schema/transition contract), `wire_delivery/c14n.rs` (pure
encoding), `wire_delivery/conformance.rs` (feature-gated shared unit assertions); `crates/store/src/wire_delivery.rs` (redb),
`ferrostore/traits_wire_delivery.rs` (ferro), `wire_delivery_guard.rs` (mutation admission), `wire_delivery_continuity.rs` (local
export/restore verification); `crates/wire/src/wire_delivery/{mod,broker,protocol,policy,render,push}.rs` (runtime concerns);
`crates/cli/src/cmd/wire_delivery_hook.rs` (strict hook). Add module declarations where these are created; do not restructure unrelated
code.

Subprocess tests live in `crates/cli/tests/wire_peer_delivery.rs`, backend subprocess tests in
`crates/store/tests/wire_delivery_continuity.rs`. Their helpers live under the corresponding `tests/support/` directory and use temporary
HOME/CODEX_HOME/store/socket/route/executable paths. Unit tests stay in `#[cfg(test)] mod tests` within the module they test. No root
`tests/` workspace package is assumed. Source fixture constructors, fake clock and fault injection are module-local unless explicitly
exported through the existing `conformance` feature; a named test below owns its table inputs and observed counts, rather than relying on
unlisted magic helpers.

## Execution and commit gates

Execution requires a separate explicit implementation GO and review of this plan. Then follow `WORKFLOW.md`: `subagent-dev` supplies a fresh
implementer for each task, `code-review` supplies fresh task review/fix/re-review loops, and a fresh whole-branch review follows the final
task. The standard header's alternative does not waive this repository workflow. Review accepted findings against source; fresh fixers
handle remediation, with at most three rounds before escalation.

Every task's final checkbox means: its red test was observed failing for the stated behavior, its focused green check passed, the diff is
scoped, and fresh task review accepted it. Use configured machine author `artificially-human`; each future commit carries `Co-authored-by:
Freya <freya@artificialhumanity.io>`. Never commit/push this planning draft or execute its commands merely because they appear below. All
commands in Tasks 1–12 are **future execution instructions**.

## Task 1: Pure schema, canonicalizer and transition vocabulary

**Files:** Create `crates/core/src/wire_delivery.rs`, `crates/core/src/wire_delivery/c14n.rs`; modify `crates/core/src/lib.rs` module
declaration. Unit tests in those modules.

**Interfaces:** Define the following public domain types before another task consumes them; all persisted types are versioned, strict serde
schemas. Existing types are `WireMessage`, `DeliveryReceipt`, `MessageId`, `AgentId`, `AgentRecord`, `At`, `StoreError`.

- `Digest([u8;32])`, `PhaseToken([u8;32])` (secret; redacted Debug), `InstanceId([u8;32])`, `WorkerId([u8;32])`, `ConnectionId([u8;32])`,
  `Revision(u64)`; core receives randomness/time as data, never performs I/O.
- `DeliveryKey { original_id: MessageId, recipient_id: AgentId }`; `Lane::{Push,Hook}`; `Transport::{Queue,Steer,Stdout}`;
  `Eligibility::{Eligible,Prepared,MutationArmed,Settled,Held}`. -
  `HoldReason::{Disabled,UnknownIdentity,AliasConflict,GenerationConflict,WrongHome,BudgetExhausted,Capacity,Expired,UnsupportedVersion,UnsupportedBackend,LegacyUncertain,ContinuityLost,Corrupt,Unconfirmed,EvidenceOverflow}`.
- `Binding { recipient_id: AgentId, binding_revision: Revision, authority_id: String, local_epoch: u64, policy_hash: Digest }`; `RoutePin {
  thread_id: String, generation: u64, fingerprint: Digest, executable: Option<String>, executable_digest: Option<Digest> }`.
- `AttemptBinding { token: PhaseToken, daemon: InstanceId, worker: WorkerId, connection: ConnectionId, lane: Lane }`; `FrozenPayload {
  renderer: String, transport: Transport, expected_turn: Option<String>, bytes: Vec<u8>, digest: Digest }`. -
  `DeliveryEvidence::{PreMutationRefused,DefinitiveNoMutation,MutationUnconfirmed,VendorQueued,AppServerAccepted{thread:String,turn:String},HookEmissionCompleted}`;
  only the first two permit safe release; error details are separately bounded strings.
- `AckProvenance::{ManualReadAck,LegacyAckUnknown}`; `AcknowledgmentState { manual_at:Option<At>, legacy_unknown:bool }` stores at most
  the first verified manual ACK timestamp and one legacy/import provenance flag. `AckProvenance` classifies trusted typed inputs; no
  unbounded event list, caller-supplied provenance or model-consumption assertion is accepted. Both fields may be present: a later explicit
  ACK never erases the fact that an earlier imported ACK had unknown provenance.
- `AttemptSummary { binding: AttemptBinding, route: RoutePin, frozen: Option<FrozenPayload>, prepared_at: At, armed_at: Option<At>,
  evidence: Option<DeliveryEvidence>, detail: String, capacity:CapacityState }`.
- `DeliveryRecord { schema:u16, key:DeliveryKey, generation:Digest, binding:Binding, revision:Revision, eligibility:Eligibility,
  prepares:u8, attempts:Vec<AttemptSummary>, acknowledgment:AcknowledgmentState, held:Option<HoldReason>, annotation:String }`;
  row schema starts at 1.
- `CapacityState::{Held,Released,Retired}` is a fixed-size persisted per-attempt flag; `Retired` is a permanent capacity-only tombstone.
  `LifetimeRegistration { id:Digest, revision:Revision, home:Digest, boot:Digest, instance:InstanceId, worker:WorkerId,
  connection:ConnectionId, emitter_scope:Digest }` is a pure persisted descriptor of the exact registered local worker and its complete
  possible-emitter scope, not a numeric PID. Its public data/serde construction conveys no authority: only an exact immutable row minted
  by Task 3 admission and independently validated by the owning store is accepted; neither a copied descriptor nor a caller revision
  enrolls a scope. Add `lifetime:LifetimeRegistration` to `AttemptBinding` before prepare/arm.
  `ActiveOwner { key:DeliveryKey, attempt:AttemptBinding }` includes the exact token/instance/worker/connection and lifetime registration;
  `CapacityRetirementTarget { owner:ActiveOwner, record_revision:Revision, recipient_revision:Revision, global_revision:Revision }`.
- `RecipientControl { binding:Binding, revision:Revision, active:Option<ActiveOwner>, barrier:Option<DeliveryKey>, identity_conflict:bool }`;
  `GlobalControl { revision:Revision, active_attempts:u8, active_index:Vec<ActiveOwner> }` has at most16 entries; count equals index length.
  `CapacityRetirementResult::{Retired{view:DeliveryView},AlreadyReleased{view:DeliveryView},Unproven,Stale}` grants no delivery authority.
  `Continuity { schema:u16, cutover:At, policy_hash:Digest, local_epoch:u64,
  enabled:bool, intact:bool }`; `IdReservation { original_id:MessageId, generation:Digest }`.
- `IdentityResolution::{Unresolved,Unique{recipient_id:AgentId,binding_revision:Revision},Conflict}`;
  `IdentityIndexRow { key:String, resolution:IdentityResolution }`; `IdentityContext { registry:Option<AgentRecord>,
  selector:IdentityResolution, receipt_alias:IdentityResolution }`. Collision-free bounded selector/alias keys point to durable canonical
  IDs or a conflict summary; they never embed an unbounded list of historical aliases/attempts.
- `DeliverySnapshot { records:Vec<DeliveryRecord>, ids:Vec<IdReservation>, recipients:Vec<RecipientControl>,
  identities:Vec<IdentityIndexRow>, global:GlobalControl, continuity:Continuity }` exists only for local continuity boundaries (Task 6);
  `EvidenceView { lane:Lane, route:RoutePin, prepared_at:At, armed_at:Option<At>, evidence:Option<DeliveryEvidence>, detail:String,
  payload_digest:Option<Digest>, capacity:CapacityState }`; `DeliveryView { key:DeliveryKey, generation:Digest, binding:Binding, revision:Revision,
  eligibility:Eligibility, prepares:u8, attempts:Vec<EvidenceView>, acknowledgment:AcknowledgmentState, held:Option<HoldReason>,
  annotation:String, barrier:Option<DeliveryKey> }` is the redacted inspection form without tokens/frozen bytes.
- `PrepareInput { key:DeliveryKey, generation:Digest, binding:Binding, attempt:AttemptBinding, route:RoutePin, now:At }`; `ArmInput {
  key:DeliveryKey, revision:Revision, attempt:AttemptBinding, route:RoutePin, frozen:FrozenPayload, now:At }`.
- `ReleaseInput { key:DeliveryKey, revision:Revision, attempt:AttemptBinding, proof:DeliveryEvidence, now:At }`; `SettleInput {
  key:DeliveryKey, revision:Revision, attempt:AttemptBinding, evidence:DeliveryEvidence, detail:String, now:At }`.
- `ManualAckInput { key:DeliveryKey, at:At }`; `EnrollmentInput { bindings:Vec<Binding>, continuity:Continuity }` is consumed only by
  Task 6's disabled continuity enrollment, never a hot per-key command. -
  `DeliveryCommand::{Prepare(PrepareInput),Arm(ArmInput),Release(ReleaseInput),Settle(SettleInput),ManualAck(ManualAckInput),InvalidatePrepared{key:DeliveryKey,revision:Revision,now:At},Annotate{key:DeliveryKey,text:String},Close{key:DeliveryKey}}`.
  - `DeliveryResult::{Prepared{record:DeliveryRecord},Armed{record:DeliveryRecord},Applied{view:DeliveryView},Refused{reason:HoldReason}}`;
  `Armed` is durable state, **not** an emission permit.
- `WireDeliveryStore::apply_delivery(&self, command:DeliveryCommand)->Result<DeliveryResult,StoreError>`;
  `inspect_delivery(&self,key:&DeliveryKey)->Result<Option<DeliveryView>,StoreError>`;
  `active_delivery_owners(&self)->Result<Vec<CapacityRetirementTarget>,StoreError>` reads only the bounded active index and at most16
  corresponding point-read records/controls; `retire_orphan_capacity(&self,target:CapacityRetirementTarget)->Result<CapacityRetirementResult,StoreError>`.
  Retirement obtains proof from the store-owned trusted verifier defined in Task 3, never from the caller. No proof/retirement variant is
  added to public `DeliveryCommand` or RPC. None of these methods materializes a full snapshot.
- `TransitionContext { key:DeliveryKey, record:Option<DeliveryRecord>, reservation:Option<IdReservation>,
  recipient:RecipientControl, global:GlobalControl, continuity:Continuity, message:Option<WireMessage>,
  receipt:Option<DeliveryReceipt>, identity:IdentityContext, lifetime:Option<LifetimeRegistration> }` contains only the target key and its
  indexed predicates. `lifetime` is the owning store's currently validated admission row, never copied from the command; prepare and Arm
  require exact equality with the attempt descriptor, including instance/worker/connection/scope/revision. Core consumes only this data
  and performs no OS I/O. Mem supplies it only from its module-local admission fixture; ordinary production Mem prepare/Arm refuse
  UnsupportedBackend and retirement stays Unproven.
- `DeliveryDelta { record:Option<DeliveryRecord>, reservation:Option<IdReservation>, recipient:Option<RecipientControl>,
  global:Option<GlobalControl>, receipt:Option<DeliveryReceipt>, result:DeliveryResult }`; `None` means no write to that slot, never deletion.
  Index/binding enrollment and mutation maintenance use typed transactions in Tasks 5–6, not whole-snapshot replacement.
- `transition(ctx:TransitionContext,command:DeliveryCommand)->Result<DeliveryDelta,StoreError>` validates the per-key contract. Stores
  construct context, check predicates and apply only changed slots inside one native atomic operation. Required point reads are target
  record, original-ID reservation, recipient control, global control, continuity, immutable message, canonical receipt, registry ID and
  selector/receipt-alias indexes, plus the one exact lifetime-admission control row at prepare/Arm. No table/recipient/history scan or
  receipt-alias enumeration is permitted on this path; missing/corrupt
  required metadata refuses rather than rebuilding from history. Independent inspection uses the same bounded key/recipient reads.
- Prepare atomically requires `global.active_attempts < 16`, `recipient.active.is_none()`, no barrier/conflict and all target predicates;
  committing Prepared increments the count, inserts the exact `ActiveOwner` into the bounded index, sets the recipient slot and marks
  capacity Held together with the lifetime prepare counter. Arm keeps that ownership; safe release, Prepared invalidation or terminal
  settlement changes Held to Released, removes only that exact indexed/recipient owner and decrements once. Retirement changes Held to
  Retired with the same exact-owner removal/decrement; it preserves eligibility, mutation uncertainty, original attempt outcome/ACK,
  lifetime prepares, ID suppression and the affected recipient barrier. It grants no replay/output/new eligibility. A still-live or unproven
  worker keeps capacity Held and inspectable. Settlement of uncertainty retains the recipient barrier even after freeing active capacity.
  Only Armed attempts with Held capacity use retirement; unarmed Prepared attempts use existing CAS invalidation. Disconnect alone never
  retires Armed capacity. Duplicate/refused commands change neither prepares nor capacity.
- Retirement CAS compares the exact key/attempt token/instance/worker/connection/lifetime registration and record/recipient/global revisions
  in one native transaction; proof must cover that exact registration revision. A stale target/observation refuses, never clears another
  attempt's slot, and requires a fresh bounded read and fresh verifier observation rather than blind retries. Released/Retired is durable
  independently of outcome/ACK, so duplicate retirement or late settlement never decrements twice. An exact historical attempt already
  Released/Retired returns AlreadyReleased without writes, even if a newer owner exists; stale ownership may never mutate that newer slot.
  If settlement wins first, retirement
  observes Released and is idempotent; if retirement wins first, a still-authorized settlement may update only evidence/receipt under the
  ordinary exact-attempt CAS rules, preserving Retired and the barrier. Stale settlement must re-read/revalidate, never acquire a new
  connection grant. Lost connection-bound settlement authority stays lost: reconciliation cannot reconstruct it or forge an outcome.
- Worker exit racing verification either leaves proof Unproven/capacity Held or yields positive proof of complete scope termination;
  client disconnect, elapsed time, new epoch, missing numeric PID, receipt/ACK or administrative annotation are never such proof. Crash
  before retirement commit retains capacity; crash after commit/lost reply preserves Retired and exact-once capacity removal on reopen.
  Late evidence/worker exit cannot create output authority, reset counters or clear the uncertainty barrier. Serializable transactions
  (Mem's single borrow) protect global/recipient predicates across keys. Indefinite retained suppression and Retired tombstones do not
  consume active slots. No aggregate snapshot-size cap may block Arm rejection, settlement, ACK, invalidation, retirement or suppression.
  Existing per-row bounds remain binding; reserve fixed space for capacity/ACK/hold flags, preserving mutation evidence on overflow.
- `CapacityRetirementDelta { record:Option<DeliveryRecord>, recipient:Option<RecipientControl>, global:Option<GlobalControl>,
  result:CapacityRetirementResult }` follows the same bounded point-update/None-means-no-write rules as `DeliveryDelta`, with no receipt,
  reservation or eligibility change. Define
  `transition_capacity_retirement(ctx:TransitionContext,target:CapacityRetirementTarget)->Result<CapacityRetirementDelta,StoreError>` as a
  pure exact-owner/revision predicate/delta function, not a store mutation or proof API. Only a store's internally verified retirement path may
  commit its delta. Mem uses a module-local lifetime-verifier fixture for conformance; ordinary production Mem retirement returns Unproven.
- `canonical_generation(msg:&WireMessage)->Result<Digest,StoreError>`;
  `canonical_payload(generation:Digest,renderer:&str,bytes:&[u8])->Result<Digest,StoreError>` in `c14n.rs`.

- [ ] **Step 1: Write failing unit tests.** Local test fixture `golden_message()` uses
  id=thread_id=`urn:uuid:00000000-0000-7000-8000-000000000001`, all options None, sender_name=`alice`, to=AgentName(`bob`), kind=Mail,
  urgency=Normal, durable=false, subject=`Hello`, body=`Body`, created_at=`2026-11-06T00:00:00.000Z`.
  `managed_c14n_golden_fields_and_selectors` compares literal LP byte vectors and independent SHA-256 constants for all 15 fields in spec
  §4; cover absent/present options, false/true, Unicode without normalization, each selector tag and custom role. Assert changing
  reply/correlation/selector kind changes generation, stored whitespace is preserved, At encodes exactly 24 bytes, payload includes raw
  32-byte digest and count `u64be(1)`. Assertion anchors for the named tests (each variable is the observed result of the fixture operation
  described above):

```rust
assert_ne!(canonical_generation(&original).unwrap(), canonical_generation(&changed_reply).unwrap());
assert_ne!(canonical_generation(&original).unwrap(), canonical_generation(&changed_selector_kind).unwrap());
```

- [ ] **Step 2: Run** `cargo test -p fl-core managed_c14n_`; expect FAIL for missing canonicalizer/types, not a zero-test success.
- [ ] **Step 3: Implement the declared schema and encoders.** Exact domain prefixes `FerroLoop/WireDelivery/generation/v1` and
  `FerroLoop/WireDelivery/payload/v1`; canonicalizer `wire-delivery-c14n-v1`; renderer `wire-peer-envelope-v1`. Implement spec §4
  LP/option/bool/selector order exactly; do not use JSON serialization as generation encoding.
- [ ] **Step 4: Run** `cargo test -p fl-core managed_c14n_`; expect all named tests PASS; check the literal golden byte fixtures against the
  design independently before freezing their hashes. The specified fixture generation byte length is 281 and digest hex is
  `1f7850e9a8504f1f77793182b2ebca7f12b68665b432ffba1c1df32c50f60413`; payload bytes `b"example\n"` with that generation/renderer yield
  digest `cbeaaa916435db1d6e91c7932fa644e7f98c3a12b3632ad00d12770b2d7e030a`. These are independently encoded planning vectors, not executed
  product-test results.
- [ ] **Step 5: Commit gate.** Stage only these files; fresh review checks strict serde and schema/signature consistency; commit
  `feat(core): define peer delivery canonical contract` with the Freya trailer.

## Task 2: Atomic MemStore reference semantics and reusable conformance

**Files:** Modify `crates/core/src/mem.rs`, `wire_delivery.rs`, `lib.rs`; create `crates/core/src/wire_delivery/conformance.rs` gated with
existing `conformance` feature or test cfg.

**Interfaces:** Consume Task 1 types/trait. Produce `transition` and `impl WireDeliveryStore for MemStore`. Test helper
`delivery_conformance(store:&impl WireDeliveryStore, seed:impl Fn(&WireMessage))->()` supplies fixed messages/registry via the caller's
existing typed store APIs. Add `fixture_message()->WireMessage`, `fixture_commands()->Vec<DeliveryCommand>` in that conformance module, with
deterministic IDs/tokens/time; each owning backend installs its own module-local admission fixture before invoking shared assertions.
No public fixture verifier/observation constructor or production Mem admission is added; shared assertions consume accepted registrations
as ordinary data and cannot mint store authority. No threads share `MemStore` across its non-Sync boundary.

- [ ] **Step 1: Write failing unit tests.** `managed_mem_first_prepare_wins`: two sequential competing commands at one revision give one
  Prepared and one refusal, counter=1. `managed_transition_crash_matrix`: parameterize every crash row in spec §7; no Armed/Settled/Held row
  becomes Eligible without typed no-mutation proof. `managed_three_prepares_lifetime`: third safe release exhausts lifetime; fourth prepare
  refused, even after ACK/annotation/restart simulation. Assertion anchors for the named tests (each variable is the observed result of the
  fixture operation described above):

```rust
assert_eq!(prepared_winners, 1);
assert_eq!(record.prepares, 3);
assert!(matches!(fourth_prepare, DeliveryResult::Refused { reason: HoldReason::BudgetExhausted }));
```

- [ ] **Step 2: Run** `cargo test -p fl-core managed_`; expect FAIL because store trait/transition is incomplete.
- [ ] **Step 3: Implement one mutable-borrow operation.** Re-read message, alias-resolved receipts, binding/control and revision in the
  borrow using the indexed bounded `TransitionContext`; apply only `DeliveryDelta` slots and receipt projection together. No randomness or
  clock enters MemStore. Prepared expiry
  invalidates by CAS; arm at 20s fails; arm never acts as an expiring lease. Persist recipient barrier for unconfirmed Armed; ACK/admin
  cannot clear it. Capacity refuses without consuming another message's history.
- [ ] **Step 4: Extend and run** `cargo test -p fl-core managed_`; expect PASS for stale revision, wrong worker, immutable digest mismatch
  at prepare/arm, expiry/ACK racing arm, identical settlement idempotence, contradictory settlement Held, evidence bounds, one frozen copy
  and barrier across new IDs. Successful queue/steer/stdout project Delivered; manual ACK projects Acked; unconfirmed preserves prior
  receipt. `managed_ack_preserves_independent_evidence` covers ACK after Armed with no outcome, settled acceptance and
  MutationUnconfirmed: acknowledgment changes independently, original evidence/attempt count/prepares/barrier do not; idempotent repeated
  ACK retains the first timestamp. ACK before arm invalidates Prepared without resetting counters. Include LegacyAckUnknown followed by
  manual ACK retaining both provenance fields; manual ACK and legacy import never manufacture a mutation outcome or new attempt.
  `managed_hot_context_independent_of_retained_history` grows unrelated settled/held rows, IDs, aliases and recipients, then instruments
  point reads, rows decoded and delta writes for prepare/arm/settle/ACK/inspect: the same fixed lookup set and target-row work is observed.
  `managed_capacity_predicates_are_atomic` races the last global slot across recipients and two keys of one recipient; exactly one wins,
  refused keys keep their own prepare counters, and unrelated retained history cannot consume a target's budget. Mem uses sequential
  competing-command/interleaving cases; Tasks 3–4 run actual concurrent durable writer races without sharing non-Sync MemStore.
  `managed_capacity_retirement_preserves_uncertainty` fills16 slots, retires one exactly owned Armed attempt with verified ended-scope
  fixture evidence and admits one unrelated recipient; original ID and later IDs for the affected recipient remain barred, with unchanged
  outcomes/ACK/prepares. `managed_capacity_retirement_race_orders` exercises settlement→retirement and retirement→still-authorized
  settlement, duplicate retirement and worker exit before/during/after verification: one decrement total, permanent Retired where applicable,
  no other/newer attempt's slot cleared and no new output or barrier clearance.
- [ ] **Step 5: Commit gate.** Fresh reviewer checks full transition table, empty post-write rejection allow-list and no model-action
  guarantee; commit `feat(core): add atomic peer delivery reference semantics` with the Freya trailer.

## Task 3: Native durable redb delivery transactions

**Files:** Create `crates/store/src/wire_delivery.rs`; modify `crates/store/src/lib.rs`, `ferrowire.rs`; unit tests in the new module.

**Interfaces:** Consume Task 1 `WireDeliveryStore` and Task 2 conformance. Produce `impl WireDeliveryStore for RedbStore`; internal
`apply_delivery_in(tx:&redb::WriteTransaction,command:DeliveryCommand)->Result<DeliveryResult,StoreError>`. Table names exactly
`__wire_delivery_v1`, `__wire_delivery_ids_v1`, `__wire_delivery_control_v1`; strict versioned JSON values, collision-free key encoding
(length-prefixed original ID and registry ID, hex key).

**Local admission interfaces (fl-store, before broker consumption):** Export `wire_delivery_lifetime` as a public module with only the
following local Rust facade; core retains only the pure descriptor above. `LocalAdmissionInput` has private fields, is non-Clone and has
no serde, wire representation or public field/unchecked constructor. All OS-handle facade definitions and their broker consumers are
Linux-cfg gated; other platforms retain disabled managed behavior. Define these public types and the two input factories:

```rust
pub struct LocalAdmissionInput { /* private validated-candidate fields */ }
pub enum LocalAdmissionError {
    UnsupportedBackend, Unproven, Capacity, IdentityConflict, Store(StoreError),
}
pub struct BrokerScopeHandles {
    pub home_dir: std::os::fd::OwnedFd,
    pub worker_pidfd: std::os::fd::OwnedFd,
    pub containment: std::os::fd::OwnedFd,
    pub creation_control: std::os::fd::OwnedFd,
    pub egress: Vec<std::os::fd::OwnedFd>,
}
pub struct HookScopeHandles {
    pub home_dir: std::os::fd::OwnedFd,
    pub peer_socket: std::os::fd::OwnedFd,
    pub containment: std::os::fd::OwnedFd,
    pub creation_control: std::os::fd::OwnedFd,
    pub egress: Vec<std::os::fd::OwnedFd>,
}
pub struct LocalWorkerBinding {
    pub instance: InstanceId, pub worker: WorkerId, pub connection: ConnectionId,
}
impl LocalAdmissionInput {
    pub fn broker_owned(binding: LocalWorkerBinding, handles: BrokerScopeHandles)
        -> Result<Self, LocalAdmissionError>;
    pub fn hook_peer(binding: LocalWorkerBinding, handles: HookScopeHandles)
        -> Result<Self, LocalAdmissionError>;
}
pub trait LocalLifetimeAdmissionStore: WireDeliveryStore {
    fn admit_local_lifetime(&self, input: LocalAdmissionInput)
        -> Result<LifetimeRegistration, LocalAdmissionError>;
    fn close_local_lifetime(&self, registration: &LifetimeRegistration)
        -> Result<(), LocalAdmissionError>;
}
```

`LocalAdmissionError` is local/non-serializable and implements `std::error::Error`; provide
`impl From<LocalAdmissionError> for StoreError` with redacted backend-error details, without parsing strings to grant authority.
These factories are defined/implemented in this task, not private helpers assumed available to fl-wire. They consume real owned local
handles, bound cardinality/type checking and collect kernel identities; the resulting input is only a validation candidate. They accept no
home/boot/proof JSON, PID string, epoch or `trusted` flag. `broker_owned` requires the actual worker pidfd obtained by the broker's owned
worker creation path (race-free pidfd acquisition), plus the broker-owned creation/egress controls for that worker's closed scope.
`hook_peer` derives the peer process identity and owned lifetime handle from the connected Unix socket's kernel credentials and
SO_PEERPIDFD or a reviewed race-free equivalent; a claimed PID/UID/session or matching socket pathname is insufficient. Without that
kernel facility it refuses. Hook identity/transcript checks remain Task 9's additional recipient/route checks and confer no lifetime
coverage. No arbitrary externally supplied observer, proof implementation or boolean enters either API.

The store-owned admission producer revalidates every candidate independently: descriptor kinds, ownership and physical home match its
already-pinned trusted local store/authority; validated local procfs boot identity; actual worker identity and its membership in the
kernel-enforced closed scope; and exclusive owned creation/egress controls covering **all** potential emitters, future descendants and
endpoints. A pidfd/list of children, open directory, advisory lock, cgroup membership alone or claimed ownership does not establish closure.
The supported local containment/egress profile must establish that descendants cannot escape or transfer emission/creation rights outside
the scope and that its controls permit irreversible closure/fencing; otherwise admission returns
`LocalAdmissionError::UnsupportedBackend` or `Unproven`,
with no registration and no Arm. The facade is never serialized or exposed as a public local/remote RPC verb. This is the existing
closed-scope-handle proof route, not a new general sandbox, Host implementation or authority service. Unproven ordinary hook/vendor environments stay disabled; this task must not invent a broader
containment implementation to claim support. All descriptor/kernel I/O is in fl-store, outside store/global locks.

`impl LocalLifetimeAdmissionStore for RedbStore` durably mints a random unique registration ID and immutable revision bound to the exact
validated physical home/boot/instance/worker/connection/scope, and keeps its owned handles in the store-owned verifier registry. Store the
bounded provenance row under `lifetime/<id>` in existing `__wire_delivery_control_v1`, atomically before returning registration; no new
ledger/table/service. Caller-chosen binding IDs identify the broker's handles but cannot authenticate a worker: admission rejects an ID
already mapped to another kernel identity, and independently matches hook peer/broker-owned scope as above. Repeated identical binding
admission returns the same row only after revalidation; conflicting coverage/identity refuses, never rewrites it. Reopen loads durable
provenance, never synthesizes lost handles or trusts the serialized descriptor as physical-home/boot proof.

At prepare **and** Arm the native backend privately validates the row's exact binding, live registry ownership, closed immutable scope and
admissible state; construct `TransitionContext.lifetime` from that row, then re-read its revision/state in the same transaction as ordinary
record/recipient/global CAS. No raw caller registration can bypass this check. Kernel validation occurs outside the transaction; scope
closure is monotonic and any registry close/fence invalidates the validation generation before a transaction can grant Arm. Internal
`LifetimeRegistryRow` and validation tickets/constructors remain private; native commits compare the validation generation under the same
registry synchronization as admission/close, with no lock spanning external I/O. Dropped/lost handles cannot validate a new prepare/Arm.

Privately define `LifetimeRegistryRow { registration:LifetimeRegistration, state:LifetimeAdmissionState, generation:Revision }` and
`LifetimeAdmissionState::{Open,Closing,Closed}`; public registration fields/revision never change. Define crate-private
`LocalLifetimeVerifier::validate_admission(&self,registration:&LifetimeRegistration)->Result<Option<AdmissionValidation>,StoreError>`;
`AdmissionValidation` is non-serializable with private constructors and binds that row's exact registration/generation. Native prepare/Arm
commit helpers consume it only after the generation/state recheck; the public core transition cannot mint it. Close moves Open→Closing
and advances generation before egress closure, then moves to Closed only after the verifier's complete positive scope proof. A store-owned
registry gate coordinates native write transactions in fixed lock order and never spans kernel I/O; independent native handles/processes
must compare the same durable generation/state in their transaction, so a per-handle in-memory map cannot bypass close.

Bound admission to64 live/pending scopes total (shared across owning handles/processes for the same store), at most16 egress descriptors
per scope and one containment/creation-control/worker handle set; one containment handle must cover any descendant population. Refuse excess before
minting without consuming a prepare. Maintain a durable at-most64-entry live-admission index in the existing control namespace; mint/index
reservation and close/index release are native atomic updates. The provenance row is at most8KiB; there is no scan of historical
registrations. At most16 scopes can own active attempts. Unreferenced unused admissions are revoked on connection teardown and their live slot removed only after positive
complete closure; referenced immutable provenance remains with retained attempts and does not occupy a live-admission slot after positive complete closure. Losing descriptors without that proof
retains its live slot/ownership and Held capacity. A close request identifies an exact registration but is **not** proof: the store revokes
new prepare/Arm, closes owned creation/output controls and observes or fences the whole scope itself; unresolved emitters remain held.
Fencing/cleanup does not settle an attempt or decrement capacity; only the existing exact-target retirement CAS can do that. Reopen reads
only the bounded64 live-admission index and16 active-owner index and their point rows; it preserves unresolved ownership/live-slot limits
and never resumes output rights. Same-home prior-boot proof can positively close these old scopes; daemon restart alone cannot. A changed
scope requires a new registration before a new prepare; invalidate an old Prepared attempt by its existing CAS first. Armed scope expansion is forbidden;
future descendants are allowed only inside its already-proven immutable containment. Replacement/revision changes never transfer an old
permit/settlement grant. A positive retirement proof may also durably finalize that exact registry row as Closed and release its live
admission-index slot in the native retirement transaction after registry-generation CAS; the other referenced attempts retain their own
Held capacity until separately retired. Already Closed is idempotent, and stale proof cannot close a replacement registration. Close, crash
and admission commit faults preserve these rules and do not reset three-prepares limits.

**Internal lifetime/fence interfaces:** Add `crates/store/src/wire_delivery_lifetime.rs` (and its module declaration) for the trusted local
verifier shared by redb/ferro behind the public admission facade above. Internal verifier/observation visibility is crate-private for the
two owning backend modules, never a public construction/serialization interface. `LocalLifetimeVerifier::observe(&self,target:&CapacityRetirementTarget)->Result<Option<CapacityOnlyObservation>,StoreError>`
returns an opaque, non-serializable observation with private constructors. It binds the exact `ActiveOwner` and registered
scope revision; it is not a `DeliveryEvidence` or no-mutation proof. Production stores own this verifier and accept neither caller booleans,
proof structs nor caller-selected verifier implementations. Test-only injection stays owning-module/conformance fixtures. An internal
`retire_capacity_in(tx:&redb::WriteTransaction,target:&CapacityRetirementTarget,observation:CapacityOnlyObservation)->Result<CapacityRetirementResult,StoreError>`
validates the exact observation/target and runs the pure capacity delta. The ferro equivalent uses `&mut super::Txn` in Task 4. Obtain the
observation outside the transaction and all store/global locks; its positive proof must be irreversible for the registered lifetime, then
CAS current exact ownership and all three revisions inside the native transaction. Changed registration/coverage refuses stale proof.

At trusted admission, pin authority/physical-home identity, a trusted kernel boot identity and a unique immutable lifetime registration for
the exact instance/worker and all possible local emitters (including hook client and vendor descendants) before granting any permit.
The stable scope identity covers future descendants through owned creation rights/containment, not a one-time process list; it cannot
silently acquire a new emitter after verification. Broker/verifier admission closes the scope against unregistered emission: if complete
coverage cannot be established, no managed Arm is allowed. Use owned kernel lifetime handles and a closed emitter-registration scope, never numeric PID absence/reuse inference. The concrete
same-boot route requires the owned worker joined, its output/child-creation rights irreversibly closed, and positive termination or an
irreversible trusted kernel fence of **every** registered potentially emitting process/descendant/endpoint. Main-child exit or join alone is
insufficient; missing descendants/handles/coverage returns Unproven. No new Host component or external authority/service is assumed.

A second concrete recovery route after a full kernel reboot reads a validated local procfs kernel boot identity, checks the same pinned
trusted physical home/authority and admission boot identity, and positively establishes that every registered old local emitter belonged to
the previous boot. A verified different boot on that same home proves old local processes/descendants ended, even after broker/store reopen;
a hostname, daemon epoch, caller boot string, moved/copied store or restored snapshot cannot supply this proof. Missing/unsupported home or
kernel provenance returns Unproven. Mutation already submitted to a vendor may persist; retirement still grants no nonmutation/consumption
claim. Same-boot daemon restart that lost all usable lifetime handles retains capacity unless a complete trusted irreversible fence can be
verified; no liveness promise bypasses that boundary. Registration/boot provenance and active ownership survive ordinary reopen/migration;
restore remains disabled and cannot manufacture lifetime proof.

- [ ] **Step 1: Write failing unit tests.** `managed_redb_transaction_conformance` invokes the shared assertions; `managed_redb_cas_reopen`:
  competing writer handles serialize, one prepare wins, counters/barrier survive close/reopen. Inject failure before commit and after
  durable commit/before response; assert row+receipt/control either all absent or all committed, never split. Assertion anchors for the
  named tests (each variable is the observed result of the fixture operation described above):

```rust
assert_eq!(prepared_winners, 1);
assert_eq!(reopened.prepares, committed.prepares);
assert_eq!(reopened_barrier, committed_barrier);
```

- [ ] **Step 2: Run** `cargo test -p fl-store managed_redb_`; expect FAIL for missing trait implementation.
- [ ] **Step 3: Implement native transaction admission.** Construct transition context from one write transaction; call `transition`;
  atomically apply ledger, reservation, control and receipt/log delta. No external I/O or global route lock crosses transaction lifetime.
  Point-read the Task 1 predicates and update the global count/recipient active slot in that same transaction; do not deserialize retained
  history. Store control/index rows in namespaced `__wire_delivery_control_v1` keys; no extra table or full snapshot per operation.
  Allocate `FORMAT_WITH_WIRE_DELIVERY` as 10 only after rechecking `FORMAT_LADDER`; update ladder/error newest atomically with
  enrollment/first ledger write on both backends (ferro completion follows Task 4). No managed caller is connected yet.
- [ ] **Step 4: Run** `cargo test -p fl-store managed_redb_`; expect PASS. Include corrupt/missing table and unknown row/schema checks:
  refuse managed operations without silently creating replacements; pre-enrollment ordinary stores retain their old format. Run the shared
  ACK/evidence and growing-history lookup assertions; competing keys must atomically enforce global16/recipient1 without a scan. Reopen
  must preserve independent acknowledgment fields and original attempt evidence, including an ACK after Armed before settlement.
  Add `managed_redb_orphan_capacity_reopen`: persist16 Armed orphans, reopen, and assert unrelated prepare refuses until valid internally
  verified retirement; Unproven retains16, valid retirement yields15 and permits one unrelated prepare. Original/later affected-recipient
  IDs stay suppressed. `managed_redb_retirement_commit_faults` injects before-commit, after-commit/before-reply and reopen: Retired flag,
  index removal, recipient owner and count commit together; repeat/late authorized settlement cannot double-decrement. Reuse the shared
  race-order assertions. `managed_lifetime_proof_scope_and_boot` rejects wrong key/token/instance/worker/connection/registration/revision,
  incomplete emitter coverage, live descendant, stale observations and reused process identities; tests joined-complete-scope and pinned
  same-home prior-boot proof positively, and rejects new epoch/caller boot/snapshot/moved-home claims. These are fixtures, not runtime proof.
  `managed_redb_local_admission_contract` covers both real-descriptor factory paths with owning-module kernel fixtures and rejects forged
  binding/foreign home, caller boot/PID claims, wrong descriptor kinds, escaped/live descendant or endpoint, incomplete creation/egress
  coverage, unsupported kernel facilities and copied registration without registry ownership. Prove exact valid admission→prepare→Arm;
  register/close/replace racing prepare and Arm must refuse stale validation. Cover64/65 live scopes,16/17 egress handles, unused cleanup,
  loss-of-handles hold, immutable replacement, prior-boot reopen and before/after admission-commit faults. No caller observer injection;
  deterministic proof fixtures remain module-local and are not an exported production factory.
- [ ] **Step 5: Commit gate.** Fresh review checks durable commit boundaries and no policy activation; commit `feat(store): persist redb
  peer delivery transitions` with the Freya trailer.

## Task 4: Native ferro transactions and AnyStore forwarding

**Files:** Create `crates/store/src/ferrostore/traits_wire_delivery.rs`; modify `ferrostore/mod.rs`, `ferrostore/doc.rs`,
`crates/store/src/any.rs`. Unit tests in the owning modules.

**Interfaces:** Consume Tasks 1–3; produce `impl WireDeliveryStore for FerroStore`, `impl LocalLifetimeAdmissionStore for FerroStore`,
forwarding `impl WireDeliveryStore for AnyStore` and `impl LocalLifetimeAdmissionStore for AnyStore`,
internal `apply_delivery_in(tx:&mut super::Txn,command:DeliveryCommand)->Result<DeliveryResult,StoreError>` in the ferro module, plus
`retire_capacity_in(tx:&mut super::Txn,target:&CapacityRetirementTarget,observation:CapacityOnlyObservation)->Result<CapacityRetirementResult,StoreError>`
using Task 3's private shared verifier and admission producer/registry. Forward both admission/close methods to the native backend;
`Inner::Remote` (including same-host Unix) returns UnsupportedBackend before consuming authority, opening an embedded fallback or creating
a ledger. Use the same admission provenance, bounds, prepare/Arm validation and lifecycle as redb, with the same logical collection
names/schema/key encoding inside existing `with_write` transaction machinery.

- [ ] **Step 1: Write failing unit tests.** `managed_ferro_transaction_conformance` runs shared assertions;
  `managed_any_store_forwards_delivery` covers both native variants and admission/close forwarding; `managed_ferro_atomic_reopen` injects
  append/sync errors and validates no
  partial receipt/control projection or new permission after unknown commit. Assertion anchors for the named tests (each variable is the
  observed result of the fixture operation described above):

```rust
assert_eq!(ferro_outcomes, redb_outcomes);
assert_eq!(via_any_outcomes, native_outcomes);
assert_eq!(reopened_barrier, committed_barrier);
```

- [ ] **Step 2: Run** `cargo test -p fl-store managed_ferro_` and `cargo test -p fl-store managed_any_store_`; expect FAIL for missing
  implementations.
- [ ] **Step 3: Implement native context/delta application and forwarding.** Use one durable `with_write` per transition, including
  reservations, binding revision, indexed point reads, global/recipient capacity predicates and receipt log. Apply only `DeliveryDelta`;
  retained history must not affect hot-operation decoding/work. Raw remote authentication conveys no permit authority.
  **Lead-accepted disabled-v1 limitation** (`urn:uuid:01a1aef6-fede-73a1-a562-68d49f21594c`): implement native delivery transactions for
  embedded FerroStore; every `Inner::Remote` handle, including a same-host Unix daemon-backed store, returns `UnsupportedBackend` for the
  new delivery trait while preserving unrelated remote APIs. Installation/activation for deployments requiring that backend remains
  BLOCKED until a compatible persistence path is separately reviewed. No embedded fallback or new ledger is implied; this acceptance is
  neither Owner approval nor implementation/activation GO. No managed prepare/arm/settle verbs are added to remote RPC. Enforce both
  schema step and required collection validation on reopen; never synthesize a missing managed history.
- [ ] **Step 4: Run the Step 2 commands**; expect PASS including two local handles racing CAS, reopen counters, recipient ordering barrier
  and unknown-format refusal. Mem/redb/ferro must yield identical conformance outcomes, including growing-history point-read/work counts,
  global16/recipient1 atomic refusal with unchanged losing-key prepares, and independent ACK/evidence persistence across reopen.
  `managed_ferro_local_admission_contract` repeats Task 3's admission/prepare/Arm/revision/close/bounds/fault cases;
  `managed_any_store_local_admission` proves native forwarding and Remote/same-host-Unix refusal without fallback/new ledger.
  `managed_ferro_orphan_capacity_reopen` and `managed_ferro_retirement_commit_faults` repeat Task 3's 16-orphan, proof refusal, race, atomic
  fault/lost-reply and exact-once retirement assertions with native ferro transactions and reopened handles; AnyStore forwards both
  retirement and bounded active discovery. `managed_any_store_remote_delivery_unsupported` covers TCP and same-host Unix daemon-backed
  `Inner::Remote`: no retirement/prepare fallback, new ledger or external handoff; unrelated remote APIs retain their behavior.
- [ ] **Step 5: Commit gate.** Fresh review checks native serialization rather than read-then-write emulation; commit `feat(store): add
  ferro peer delivery transactions` with the Freya trailer.

## Task 5: Identity, import/prune and every generic mutation fence

**Files:** Create `crates/store/src/wire_delivery_guard.rs`; modify `crates/core/src/mem.rs`, `crates/store/src/{ferrowire,state}.rs`,
`ferrostore/{doc,mod,traits_wire,traits_state,transaction,session,server,sync}.rs`; preserve existing registry APIs. Unit tests stay in these
modules.

**Interfaces:** Consume `Binding`, `IdReservation`, `RecipientControl`, Task 1 transition inputs. Produce internal
`MutationOrigin::{Public,PeerSync,TypedWire,TypedRegistry,Delivery,Continuity}` and
`MutationContext { continuity:Continuity, reservation:Option<IdReservation>, recipient:Option<RecipientControl>,
record:Option<DeliveryRecord>, identity:IdentityContext }` and
`admit_delivery_mutation(ctx:&MutationContext,origin:MutationOrigin,collection:&str,id:&str,before:Option<&serde_json::Value>,after:Option<&serde_json::Value>)->Result<(),StoreError>`
in the guard module. Construct one bounded context per affected object from indexed point reads in the same transaction/overlay; no
`DeliverySnapshot` or dependent-history scan. Original-ID reservations identify protected messages, recipient controls identify protected
registry IDs, alias indexes resolve receipt identities and `(message,canonical recipient)` directly locates any needed record. Guard both
old/new selector or alias keys when changed; reject ambiguous public mutations without walking dependents. Origins are crate-private
constructed call paths, never caller-provided strings/remote fields. Core applies equivalent identity/receipt/prune invariants under its
own borrow without depending on fl-store. Before enrollment, preserve existing ordinary mutation behavior; required managed metadata is
validated only on the enrolled path. Add internal
`project_delivery_sync(batch:&SyncBatch)->Result<SyncBatch,StoreError>` for the accepted limitation below; never infer permit authority from projection.

- [ ] **Step 1: Write failing tests.** `managed_all_mutation_surfaces_fenced` enumerates ordinary put/delete, batch/raw op, Transaction,
  session commit, remote raw commit, inbound sync, compact/reopen, typed send/import/update/prune, registry update/import/delete and
  delete+recreate. Assert protected rows/required ordinary message/identity/receipt dependents cannot be rewritten or removed outside typed
  validated transactions; unrelated records remain writable. Assertion anchors for the named tests (each variable is the observed result of
  the fixture operation described above):

```rust
assert!(unauthorized_batch.is_err());
assert_eq!(after_batch, before_batch);
assert_eq!(reimported_view.prepares, pruned_view.prepares);
```

- [ ] **Step 2: Run** `cargo test -p fl-store managed_all_mutation_` and `cargo test -p fl-core managed_identity_`; expect FAIL because
  ordinary dependent collections remain mutable.
- [ ] **Step 3: Implement admission at staging and final commit boundaries.** Cover all raw/unchecked paths, including `commit_if_seq`,
  `commit_remote`, inbound application and remote server handlers. Preserve managed original-ID reservations indefinitely. Resolve durable
  `AgentRecord.id`; coalesce receipt aliases transactionally; unresolved/ambiguous/conflicting histories Held. Rename retains binding;
  recycled name cannot redirect. Maintain canonical selector/receipt-alias indexes transactionally at enrollment and every typed registry/
  receipt/import boundary; the hot path uses their bounded resolution/conflict summaries. A newly conflicting identity sets recipient control
  conflict/revision so all affected keys hold on use without rewriting their retained history. Preserve distinct old histories; never merge
  them to grant permission. Imported receipts may suppress/display but never enroll, relocate, grant or clear uncertainty; an unproven ACK
  updates `acknowledgment.legacy_unknown`, not `AttemptSummary.evidence`. Atomic ACK projection preserves any original mutation outcome.
  Fence inbound ordinary dependent records, remote cache-fill (`fill_doc_miss`/`install_fetched_doc`) and lazy fetch as well as writes.
- [ ] **Step 3a: Implement the Lead-accepted sequence-preserving projection.** Lead acceptance
  `urn:uuid:01a1aef6-fede-73a1-a562-68d49f21594c` accepts this disabled-v1 limitation, not Owner approval or implementation/activation GO. Source basis:
  `sync.rs:SyncOp:14` already supports `DeleteDoc`; `mod.rs:prepare_sync_batches:4509` requires the exact next sequence and nonempty input;
  `:admit_inbound:4551` drops non-schema reserved deletes while the containing sequence still advances, even when all applied ops drop.
  `:register_committed_for:4266` currently clones local ops, and `:apply_sync_batches:4216` forwards the original inbound batch. Neither is
  safe for local delivery-private data. Define projection to remove operations on the three local delivery collections (including their
  control/index/continuity/frozen-payload namespaces); keep ordinary permitted operations and the existing schema-version behavior in order.
  If nothing remains, emit exactly `SyncOp::DeleteDoc { collection:"__wire_delivery_progress_v1", id:"0" }`. This constant reserved delete
  contains no message/recipient/authority ID, token, digest, payload or evidence; it never materializes a collection and conveys no state or
  authority. It is a wire projection only, not a fourth persisted ledger collection. Preserve the original `SyncBatch.seq`; never drop a batch, renumber, send empty input, or invent a new `Noop` variant. The
  existing reserved-op rule makes this marker an ignored operation for compatible legacy receivers; existing too-new schema refusal remains.
  Apply the identical idempotent projection before any adapter callback or pending enqueue, including unchanged-forwarded inbound batches,
  adapter retries, remote sync serialization and any replay path. Inbound marker handling must bypass ordinary protected-mutation rejection
  only for this exact constant and leave all delivery records unchanged. Projection failures use bounded generic diagnostics without raw
  input/private fields; error/log paths and retry buffers may not retain or emit the unprojected batch. Do not weaken reserved/data fences.
  **Lead-accepted bounded fail-closed reopen limitation:** current `sync.rs` documents process-local pending/cursors; `write_through_disk_commit_survives_reopen_without_an_adapter`
  shows reopen has no adapter and `synced_seq=0`. Projection does not make pending retries durable. Keep a failed projected batch queued
  unchanged for in-process retry; after reopen of a nonempty managed store, refuse adapter reattachment/replay under this v1 plan rather
  than guess a cursor or resend earlier sequences. No automatic sync resume or recovery protocol is proposed. A deployment requiring sync
  across reopen has installation/activation BLOCKED until separately reviewed durable egress/cursor continuity exists; no embedded fallback,
  new ledger or implied readiness follows acceptance. This limitation is distinct from remote permit authority. Scope this refusal to the
  managed-store path; preserve unmanaged sync behavior.
  Local continuity export is never peer-sync recovery permission.
- [ ] **Step 4: Add** `managed_identity_alias_conflict_holds`: unresolved name later uniquely bound works only without prior conflict;
  distinct armed aliases retain both histories and hold. Add `managed_prune_identical_reimport_stays_suppressed`: prune eligible retained
  body per protected policy, keep ID/digest/binding/outcome reservation, identical import stays suppressed, changed same-ID body
  rejects/holds. Unresolved bodies never auto-prune.
- [ ] **Step 5: Run** `cargo test -p fl-store managed_` and `cargo test -p fl-core managed_`; expect PASS, including ACK/import racing Arm
  and sync bypass attempts. Verify unauthorized mutation is atomic rejection, not a partially applied batch. Add
  `managed_guard_lookup_work_is_history_independent`: grow unrelated retained rows and measure the same indexed reads/decoded rows per
  guarded mutation, including mixed authorized/unauthorized batches whose atomic rejection changes no row, counter or receipt.
  `managed_sync_projection_preserves_sequences` covers consecutive all-local and mixed commits: exactly one nonempty projected batch per
  original sequence; ordinary ops keep order/content, all-local commits contain only the constant marker. Feed them to a fixture of the
  baseline reserved-op admission and assert the receiver's exact final store sequence with no gap; too-new schema still refuses.
  `managed_sync_forwarding_redacts_private_ops` feeds otherwise unchanged inbound batches with private puts/deletes through forwarding,
  including all-private input; assert projection idempotence and no local private values/IDs/tokens/payload/evidence in serialized callback,
  queued, remote, forwarded or failure-diagnostic bytes. Ordinary permitted peer message/receipt sync semantics remain intact.
  `managed_sync_failed_adapter_retry_reopen` fails the first callback, checks unchanged projected bytes/sequence on in-process retry and
  exact receiver sequence after success, then reopens both disk fixtures: persisted store sequence is unchanged, adapter/pending remain
  absent and managed adapter reattachment/replay is refused without emitting a batch. Test crash before retry similarly: no cursor advance,
  invented success, empty batch, automatic resend or sequence renumbering. This asserts the bounded limitation, not durable pending recovery.
- [ ] **Step 6: Commit gate.** Fresh review audits the call graph for every unchecked/native mutation surface before approving; commit
  `fix(store): fence managed wire identity and mutation paths` with the Freya trailer.

## Task 6: Migration, local continuity export/restore and legacy enrollment

**Files:** Create `crates/store/src/wire_delivery_continuity.rs`, `crates/store/tests/wire_delivery_continuity.rs`; modify
`crates/store/src/{any,lib}.rs`, `ferrostore/{migrate,verify,compact}.rs` as needed by their existing copy/verification paths. No
deployment/backup scripts change.

**Interfaces:** Consume Task 1 snapshot/control and prior backends. Produce `ContinuityArchive { schema:u16, source_format:u64,
snapshot:DeliverySnapshot, messages:Vec<WireMessage>, receipts:Vec<DeliveryReceipt>, agents:Vec<AgentRecord>, digest:Digest }` and
`ContinuityReport { intact:bool, disabled:bool, rows:u64, digest:Digest }` in the new module. Define store-local extension trait
`DeliveryContinuityStore: WireDeliveryStore` with
`enroll_delivery_disabled(&self,input:EnrollmentInput)->Result<ContinuityReport,StoreError>` and
`snapshot_delivery_continuity(&self)->Result<ContinuityArchive,StoreError>` (one
consistent snapshot of delivery plus messages/receipts/registry) and
`restore_delivery_disabled(&self,archive:&ContinuityArchive)->Result<ContinuityReport,StoreError>` (one typed native transaction); implement
for RedbStore/FerroStore/AnyStore. APIs: `export_delivery_continuity(store:&impl
DeliveryContinuityStore)->Result<ContinuityArchive,StoreError>` and `restore_delivery_continuity(store:&impl
DeliveryContinuityStore,archive:&ContinuityArchive)->Result<ContinuityReport,StoreError>`. No public resume or permit import API.

- [ ] **Step 1: Write failing tests.** `managed_migration_copies_every_fence`: redb→ferro verifies all three collections,
  row/digest/projection/bindings/barrier/counters, independent ACK provenance plus attempt outcomes, identity indexes, global/recipient active
  consistency and read-only source retention. `managed_restore_never_resumes`: export then restore into
  an empty temporary store yields disabled continuity; stale/missing/corrupt archive or deleted collection refuses managed operation, no
  empty-ledger recovery. `managed_legacy_enrollment_suppresses_backlog`: Pending/Delivered→LegacyUncertain, Acked→LegacyAckUnknown, Expired
  suppressed; zero prepare/spawn/stdout/steer. Assertion anchors for the named tests (each variable is the observed result of the fixture
  operation described above):

```rust
assert!(restored.disabled);
assert_eq!(restored_snapshot.ids, exported.snapshot.ids);
assert_eq!(legacy_handoffs, 0);
```

- [ ] **Step 2: Run** `cargo test -p fl-store managed_migration_`, `cargo test -p fl-store managed_legacy_`, `cargo test -p fl-store --test
  wire_delivery_continuity`; expect FAIL for missing archive/verification paths.
- [ ] **Step 3: Implement continuity-complete migration/export/restore.** Read archive content from one consistent backend snapshot; verify
  canonical message and payload bindings, compact reservations, receipt projections, independent ACK provenance/outcomes and schema before
  publication/reopen. Full retained snapshots/scans are confined to these disabled continuity/enrollment/migration boundaries; validate all
  secondary indexes and rederive global/recipient active consistency here, never on ordinary delivery/guard paths. Enrollment canonicalizes
  legacy receipt aliases and records unresolved/conflicting identity summaries before hot operations become available. Existing manifest
  export remains a project manifest, not a continuity backup; document its insufficiency. Extend physical checkpoint/compact/copy
  verification so managed collections cannot disappear. Restore always disables, even a valid archive; stored epoch alone never proves
  old-process fencing.
- [ ] **Step 4: Add subprocess** `managed_old_open_writer_requires_external_fence`: isolated fixtures demonstrate old-format opener refuses
  upgraded copy but already-open writer is not fenced by that check; assert enrollment/cutover evidence remains blocked without external
  fence attestation. Fixture binary lives under test support, not an installed historical runtime. Refuse stale restore continuity without
  claiming to detect all out-of-band snapshot replacement.
- [ ] **Step 5: Run Step 2 commands**; expect PASS; additionally require reopened archive/migration preserves three prepares and recipient
  barrier. Add `managed_ack_and_outcome_survive_continuity`: Armed/no-outcome, settled accepted and unconfirmed fixtures each receive
  explicit ACK; legacy imported ACK then explicit ACK retains both provenance flags. Reopen/export/restore/migration preserve ACK and original
  outcomes independently, without new attempts, counter reset, barrier clearance or model-consumption claim.
  `managed_capacity_tombstones_survive_continuity` verifies Held/Released/Retired exact-attempt flags, bounded active index/count consistency
  and lifetime admission provenance through reopen/export/disabled restore/migration; duplicate retirement or late settlement after reopen
  changes no capacity twice and cannot clear a different attempt. Disabled restore itself supplies no fence proof. No managed integration proceeds
  until Tasks 5–6 reviews pass.
- [ ] **Step 6: Commit gate.** Fresh review checks absence of automatic resume and distinction between local continuity export and peer
  synchronization; commit `feat(store): preserve disabled peer delivery continuity` with the Freya trailer.

## Task 7: Trusted local broker, persistent RPC and one-use arm reply

**Files:** Create `crates/wire/src/wire_delivery/{mod,policy,protocol,broker,render}.rs`; modify `crates/wire/src/{lib,daemon}.rs`,
`crates/cli/src/{config,cmd/wire}.rs`; unit tests in those modules.

**Interfaces:** Consume store contract and complete fences. Define `ManagedPolicy { enabled:bool, node_id:String, daemon_uid:u32,
socket:PathBuf, registry_allowlist:Vec<AgentId>, codex_executable:PathBuf, codex_executable_digest:Digest, codex_control_socket:PathBuf,
transcript_root:PathBuf, policy_hash:Digest }` in policy module; default is disabled, no enrolled IDs, no inferred home.
`VerifiedConnection`, `ManagedClient`, `ManagedBroker<S:LocalLifetimeAdmissionStore>` and `WorkerContext` are opaque runtime types;
import `fl_store::wire_delivery_lifetime::{LocalLifetimeAdmissionStore,LocalAdmissionInput,LocalWorkerBinding,BrokerScopeHandles,HookScopeHandles}`.
This bound includes `WireDeliveryStore`; no downcast, private verifier access or undeclared registration factory is needed. Define
`InvocationBudget` using monotonic deadline and operation minima. Define `PreparedAttempt` (key/revision/attempt/route) and non-Clone
`EmissionPermit` (private fields, consumed once).
`ManagedBroker::prepare(&self,worker:&WorkerContext,input:PrepareInput)->Result<PreparedAttempt,StoreError>`;
`arm(&self,worker:&WorkerContext,prepared:PreparedAttempt,frozen:FrozenPayload,now:At)->Result<EmissionPermit,StoreError>`;
`settle(&self,worker:&WorkerContext,input:SettleInput)->Result<DeliveryView,StoreError>`. Store Armed alone never creates a permit.
Before prepare, broker-owned push workers pass their actual owned creation-path pidfd and scope/egress descriptors to
`LocalAdmissionInput::broker_owned`; hook workers pass the accepted verified connection's duplicated connected socket and validated local
scope/egress descriptors to `LocalAdmissionInput::hook_peer`. Broker IDs come from the already-issued instance/worker/connection context,
never request fields. The store independently checks the actual kernel identity/closed coverage; Task 9's claims cannot substitute for
missing handles. Call `store.admit_local_lifetime(input)` and retain its immutable returned registration in private `WorkerContext`;
fill `AttemptBinding.lifetime` from it at prepare/Arm, never a request descriptor. If scope controls cannot be acquired/proven, refuse the
managed lane; caller-chosen socket/path/UID or broker ID alone cannot enroll it. Connection teardown/worker shutdown invokes
`store.close_local_lifetime(&registration)` once; that revokes new grants but supplies no retirement proof. Define internal
`ManagedBroker::reconcile_orphan_capacity(&self)->Result<Vec<CapacityRetirementResult>,StoreError>`: inspect only
`store.active_delivery_owners()` (at most16), invoke `store.retire_orphan_capacity(target)` once per candidate, and leave stale/unproven
entries held/inspectable. Startup/observed worker exit may request this bounded capacity reconciliation; it performs no prepare, model
probe or replay and has no public RPC/CLI verb. Broker restart alone grants no proof. The verifier performs local lifetime checks outside
native transactions; it never inherits lost settlement or emission authority.
`render_peer_envelope(msg:&WireMessage)->Result<Vec<u8>,StoreError>` in render module pins the existing envelope exactly.

Protocol module defines negotiated capability `peer_delivery_v1`, version=1; `HookClaim { session_id:String, transcript_path:PathBuf,
cwd:PathBuf }`, `ArmSelection::{Queue,Steer{turn:String,rpc_id:u64,mask:[u8;4]},Stdout}`, and strict tagged
`ManagedRequest::{Hello{version:u16,lane:Lane,hook:Option<HookClaim>},Prepare{key:DeliveryKey},Arm{token:PhaseToken,revision:Revision,selection:ArmSelection},Settle{token:PhaseToken,revision:Revision,evidence:DeliveryEvidence,detail:String,now:At},ManualAck{input:ManualAckInput},Inspect{key:DeliveryKey}}`.
`ManagedReply::{Hello{version:u16},Prepared{view:DeliveryView,token:PhaseToken},Armed{view:DeliveryView,frozen:FrozenPayload},View{view:Option<DeliveryView>},Refused{reason:HoldReason}}`;
The broker fills AttemptBinding from its verified connection and issued token; caller strings never supply worker authority. A received
Armed reply creates one local permit only while the outstanding first Arm request matches; repeated replies create none. Prepare never
accepts caller payload; Arm carries bounded route/preflight selection, and broker reconstructs final bytes from immutable message plus
trusted lane encoder. Hook claims cannot prepare until Task 9 provides broker-side identity validation.
`ManagedClient::connect_verified(policy:&ManagedPolicy,budget:&InvocationBudget)->io::Result<Self>`; prepare/arm/settle use that same
persistent connection. Managed arms cannot be called over ordinary or remote frames.

- [ ] **Step 1: Write failing tests.** `managed_connection_handles_are_nontransferable`: foreign UID, copied token on another connection,
  reconnect, wrong daemon/worker/key/lane/revision/route/payload all fail; duplicate Arm returns inspection without a second permit.
  `managed_policy_socket_validation`: reject relative/shared/symlink/unowned paths, wrong socket/directory modes, chmod failure and
  hostname-only authority; verify Linux SO_PEERCRED at both ends. Assertion anchors for the named tests (each variable is the observed
  result of the fixture operation described above):

```rust
assert_eq!(permits_from_duplicate_arm, 0);
assert_eq!(permits_after_reconnect, 0);
assert!(foreign_uid_prepare.is_err());
```

- [ ] **Step 2: Run** `cargo test -p fl-wire managed_broker_`, `cargo test -p fl-wire managed_connection_`, `cargo test -p fl-cli
  managed_policy_`; expect FAIL for missing managed path.
- [ ] **Step 3: Implement verified persistent transport and broker.** Owned private directory0700/socket0600, hard validation failure;
  daemon instance/worker/connection/phase handles random 256 bits via existing ring. Bound connect/read/write to remaining deadline and
  RPC5s; max64 managed connections, 16 active attempts, 1/recipient. Other platforms stay disabled without reviewed peer-credential
  equivalent. Same-UID administrator trust is explicit. Keep ordinary single-request RPC compatibility separate.
- [ ] **Step 4: Implement broker Arm commit ordering.** Recompute immutable generation at prepare and arm; final payload digest only after
  read-only preflight/final serialization. Durable arm commit precedes first possible mutation; only first confirmed Arm reply on bound live
  connection grants permit. Arm timeout/error/lost reply forbids sending, even if Inspect shows Armed. Disconnect invalidates only Prepared
  via CAS, never Armed. Reject arbitrary caller frozen bytes; renderer uses stored fields. No lock/transaction spans vendor I/O.
  Complete Task 3's trusted lifetime admission/closed-scope tracking for direct hook output, steering endpoints and queue child descendants;
  a disconnected hook client or surviving vendor child is potentially emitting until positive ended/fenced proof. Worker exit may trigger
  capacity-only reconciliation; it never creates a mutation outcome. Retired remains independent of ACK/evidence and permanent on later
  settlement. `managed_broker_orphan_reconciliation_is_capacity_only` proves copied/forged claims, disconnect, timeout, daemon replacement,
  PID absence and new epoch keep slots held; complete verified scope termination retires once, with zero permits/replays/settlement transfers.
- [ ] **Step 5: Run Step 2 commands**; expect PASS plus max-connections/attempt-capacity/20s lifetime tests, handshake mismatch, redacted
  Inspect, malformed/oversize frames and remote prepare rejection. Existing daemon worker/queue caps remain unchanged; capacity never loops.
- [ ] **Step 6: Commit gate.** Fresh review checks local authority and one-use permit construction; commit `feat(wire): broker disabled
  local peer delivery permits` with the Freya trailer.

## Task 8: Read-only Codex preflight, frozen arm bytes and single push handoff

**Files:** Create `crates/wire/src/wire_delivery/push.rs`; modify `crates/wire/src/{codex_control,dispatcher}.rs` only at managed seams.
Unit tests in these modules.

**Interfaces:** Consume Task 7 broker/permit/envelope/budget. Add opaque `ManagedCodexConnection`;
`ManagedPreflight::{Active{thread:String,turn:String},Idle{thread:String}}`;
`preflight_peer(sock:&Path,thread:&str,budget:&InvocationBudget)->Result<(ManagedCodexConnection,ManagedPreflight),StoreError>`;
`freeze_peer_steer(conn:&mut
ManagedCodexConnection,preflight:&ManagedPreflight,generation:Digest,text:&[u8])->Result<FrozenPayload,StoreError>`;
`send_peer_steer(conn:ManagedCodexConnection,payload:FrozenPayload,permit:EmissionPermit)->DeliveryEvidence`. Define
`dispatch_peer<S:LocalLifetimeAdmissionStore>(broker:&ManagedBroker<S>,worker:&WorkerContext,key:DeliveryKey,budget:InvocationBudget)->Result<DeliveryView,StoreError>`
in push module. A queue helper
`send_peer_queue(policy:&ManagedPolicy,route:&RoutePin,payload:FrozenPayload,permit:EmissionPermit,budget:&InvocationBudget)->DeliveryEvidence`
performs exactly one OS spawn, preserving existing vendor child environment policy.

- [ ] **Step 1: Write failing tests.** `managed_push_positive_idle_only`: exact thread/direct-input=true/idle allows queue; unloaded,
  unsupported, ambiguous status, missing fields and read errors allow no queue. `managed_steer_ambiguous_reply_holds`: exact RPC
  ID/turn/thread accepts; mismatch, EOF, post-write -32600/-32601/-32602, timeout and partial frame all remain unconfirmed; allow-list is
  empty. Assertion anchors for the named tests (each variable is the observed result of the fixture operation described above):

```rust
assert_eq!(active_steer_frames, 1);
assert_eq!(idle_queue_spawns, 1);
assert_eq!(ambiguous_preflight_handoffs, 0);
assert_eq!(post_write_retry_handoffs, 0);
```

- [ ] **Step 2: Run** `cargo test -p fl-wire managed_push_` and `cargo test -p fl-wire managed_steer_`; expect FAIL for missing separated
  functions.
- [ ] **Step 3: Implement preflight and frozen encoding.** Active requires exactly one latest nonempty `inProgress` turn. Finish JSON/RPC
  ID/expectedTurnId and WebSocket mask/frame after preflight, before Arm; hash the complete frame, then write that exact frame once. No
  reserialization/mask change after arm. Idle queue freezes exact message UTF-8 argument; pin one absolute executable identity/digest and
  route fingerprint. Broker owns serialization; no caller substitution.
- [ ] **Step 4: Implement push state handling.** One prepare/invocation, read-only preflight before Arm, single consumed permit; queue spawn
  uses `Command::spawn` once, never `spawn_retrying_text_busy` or candidate loop. Trusted OS no-child spawn error may settle
  DefinitiveNoMutation for a later event, never same-invocation fallback. Spawned timeout/nonzero/kill/lost status is unconfirmed. Total85s,
  queue60s, steering10s and remaining deadline apply; do not add turn/start/resume/interrupt/clear/compact.
- [ ] **Step 5: Add** `managed_push_idle_race_has_one_handoff`: idle becoming active after preflight cannot cause steer fallback;
  executable/route replacement before Arm fails, replacement after Arm does not trigger another target. Assert captured spawn count≤1 and
  steer frames≤1 with total automatic handoffs≤1. Run Step 2 commands; expect PASS including output1MiB/frame+14 limits and pre-arm
  envelope512KiB rejection.
- [ ] **Step 6: Commit gate.** Fresh review checks operator `OperatorPromptIntent::OwnerSendNow` path retains behavior, peer sender name
  Owner gains no intent, existing non-Codex dispatch unchanged; commit `feat(wire): bind managed Codex push to one handoff` with the Freya
  trailer.

## Task 9: Bounded, identity-proven PostToolUse-only hook

**Files:** Create `crates/cli/src/cmd/wire_delivery_hook.rs`, `crates/wire/src/wire_delivery/hook.rs`; modify
`crates/cli/src/cmd/{mod,wire}.rs`, wire renderer/module declaration; unit tests in these modules, subprocess tests added in Task 11.

**Interfaces:** Consume verified ManagedClient and broker-rendered frozen output. Define `HookIdentity { recipient:AgentId,
session_id:String, thread_id:String, binding_revision:Revision, route:RoutePin }`; `HookInput { hook_event_name:String, session_id:String,
transcript_path:PathBuf, cwd:PathBuf }`; `parse_managed_hook(raw:&[u8])->Result<HookInput,StoreError>`;
`prove_hook_identity(input:&HookInput,policy:&ManagedPolicy)->Result<HookIdentity,StoreError>` in the wire hook module, called by the broker
against the verified connection claim before prepare and revalidated at Arm;
`run_managed_codex_hook(input:std::os::fd::BorrowedFd<'_>,output:std::os::fd::BorrowedFd<'_>,policy:&ManagedPolicy)->Result<(),StoreError>`.
The wire hook module owns pure parser/identity proof; CLI owns bounded poll-driven stdin/stdout descriptors and client orchestration,
avoiding any wire→CLI dependency. Transcript parser fixtures use literal reviewed schema version under module test data; fixture support
does not claim installed schema approval. Explicit CLI lane `fl wire hook codex`; existing generic shim cannot implicitly claim Codex
support.

- [ ] **Step 1: Write failing tests.** `managed_hook_disabled_events_emit_empty`: Stop/UserPromptSubmit/SessionStart/unknown event and no
  eligible item yield exact `b"{}\n"`, zero prepares, zero ACK. `managed_hook_strict_identity_and_bytes`: reject duplicate keys, alias
  conflict, unbounded stdin, relative paths, symlink escape, unowned/oversized transcript, session/thread/persona mismatch and route
  replacement; shared AGENT_PERSONA and `.codex` path prove nothing. Assertion anchors for the named tests (each variable is the observed
  result of the fixture operation described above):

```rust
assert_eq!(disabled_stdout, b"{}\n");
assert_eq!(disabled_prepares, 0);
assert_eq!(emitted_original_ids, vec![original_id]);
assert_eq!(partial_write_evidence, DeliveryEvidence::MutationUnconfirmed);
```

- [ ] **Step 2: Run** `cargo test -p fl-cli managed_hook_`; expect FAIL for missing explicit Codex lane/parser.
- [ ] **Step 3: Implement strict bounded parsing and identity proof.** Require exact `hook_event_name:"PostToolUse"`, nonempty session,
  absolute cwd/transcript; identity/path fields≤4096, stdin≤64KiB within2s, whole invocation20s. Contained owned transcript parser ties
  reviewed header/session, persona unique registry ID and existing current route/thread/revision. Retain transcript-first/route-replacement
  safeguards; do not register replacement route as identity evidence. Unproven installed schema/profile stays disabled.
- [ ] **Step 4: Implement one-message hook emission.** Select lowest MessageID eligible item, prepare then render full compact UTF-8
  JSON+LF, arm before any stdout byte, write+flush within2s. Exact output is
  `{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"Incoming FerroWire message(s) for '<persona>':\n\n<one existing
  peer envelope with original ID>"}}` plus LF; JSON escaping is real serialization. Broker freezes complete bytes and digest; stdout
  cap1MiB. Complete local write/flush settles HookEmissionCompleted; partial/error/timeout after arm settles MutationUnconfirmed, never ACK
  or claim consumption.
- [ ] **Step 5: Run Step 2 command**; expect PASS including hostile body escaping, multiple inbox items (exactly one emitted), `{}` no-claim
  behavior, lost arm reply, broken pipe after first byte and flush failure. Assert stored original/thread/reply/correlation IDs unchanged
  and no Stop/block reason or batch response.
- [ ] **Step 6: Commit gate.** Fresh review checks no wrapper/hook installation edits and no unsupported-runtime inference from fixtures;
  commit `feat(cli): add disabled strict Codex PostToolUse delivery` with the Freya trailer.

## Task 10: Typed manual ACK, legacy suppression and disabled integration

**Files:** Modify `crates/cli/src/cmd/wire.rs`, `crates/wire/src/{daemon,wire_delivery/protocol,wire_delivery/broker}.rs`; unit tests in
owning modules.

**Interfaces:** Consume `ManualAckInput`, negotiated ManagedClient and prior dispatch/hook functions. Produce
`ManagedClient::manual_ack(input:ManualAckInput)->Result<DeliveryView,StoreError>` and
`ManagedClient::inspect(key:&DeliveryKey)->Result<Option<DeliveryView>,StoreError>`; add read-only `fl wire delivery inspect <message-id>
--recipient-id <agent-id>` and explicit negotiated intent for existing manual Read+ACK/ACK commands. No public
arm/release/replay/proof/reset/admin-enable command is added. Annotation/Close retain original fences/counters/barriers.

- [ ] **Step 1: Write failing tests.** `managed_manual_ack_is_distinct`: before arm revokes Prepared; after arm records ManualReadAck
  in `DeliveryView.acknowledgment` alongside unchanged attempt evidence, preserves possible mutation/barrier, receipt Acked. Test Armed with
  absent outcome, settled acceptance and MutationUnconfirmed separately; no extra attempt or prepare, no counter reset or barrier clearance.
  Repeated ACK is idempotent with the first verified timestamp retained. Legacy/imported ACK sets the independent legacy flag, and a later
  explicit ACK retains that flag alongside manual provenance and any mutation evidence. `managed_legacy_clients_never_claim`: old acking
  Read/Ack rejected
  for managed recipients; imported unproven ACK becomes LegacyAckUnknown and suppresses; read-only inspection remains available and grants
  nothing. Assertion anchors for the named tests (each variable is the observed result of the fixture operation described above):

```rust
assert_eq!(after_ack.prepares, before_ack.prepares);
assert_eq!(after_ack_barrier, before_ack_barrier);
assert_eq!(fallback_handoffs, 0);
```

- [ ] **Step 2: Run** `cargo test -p fl-wire managed_manual_`, `cargo test -p fl-wire managed_legacy_`, `cargo test -p fl-cli
  managed_no_fallback_`; expect FAIL for old fallback/routing semantics.
- [ ] **Step 3: Integrate classification before automatic dispatch/extraction.** Enrolled managed recipients always consult ledger/control,
  even when policy disabled; never fall through to legacy automatic push/hook. New client+old daemon or version mismatch fails closed;
  daemon absent cannot claim/read+ACK through direct store. Managed send may persist inbox but cannot auto-dispatch through fallback. Legacy
  receipts never drive retry. Existing unmanaged/operator/non-Codex paths retain behavior.
- [ ] **Step 4: Run Step 2 commands**; expect PASS including disconnected daemon, unsupported negotiation, forged caller intent fields,
  managed disabling with existing fences, inspection redaction/held reasons and ACK/import races. Inspection must show both ACK provenance
  fields and the original per-attempt outcomes after ACK, including Armed without an outcome; settlement after ACK preserves Acked projection
  and independent acknowledgment. Add
  `managed_no_fallback_send_stays_stored`: direct persisted send emits zero external handoffs.
- [ ] **Step 5: Commit gate.** Fresh review checks no public uncertain rearm path, counters untouched by ACK/admin and no legacy automatic
  re-entry; commit `feat(wire): enforce managed ACK and compatibility boundaries` with the Freya trailer.

## Task 11: Isolated cross-lane/crash/limit conformance and regressions

**Files:** Create `crates/cli/tests/wire_peer_delivery.rs`, `crates/cli/tests/support/wire_peer_delivery.rs`; extend
`crates/store/tests/wire_delivery_continuity.rs`. Add only needed existing dev-dependency references; no production test API accepting
permits.

**Interfaces:** Test support defines `PeerRig::new(backend:BackendKind)->Self` with isolated environment; `start_daemon()->Child`,
`publish()->MessageId`, `invoke_hook(event:&str)->Output`, `handoffs()->Vec<Handoff>`, `crash_at(point:CrashPoint)`, `restart()->()`,
`inspect()->DeliveryView`. `Handoff::{QueueSpawn,SteerFrame,HookStdout}` counts automatic external handoffs;
`CrashPoint::{BeforePrepareCommit,AfterPrepareCommit,BeforeArmCommit,AfterArmCommitBeforeReply,AfterArmReplyBeforeOutput,PartialOutput,AfterVendorAcceptBeforeSettle,AfterSettleCommitBeforeReply}`
uses fake server/child barriers and connection interruption; internal durable-write injection remains owning-module tests. Test fixtures use
synthetic identities and runtime evidence, never installed sessions.

- [ ] **Step 1: Write failing subprocess tests.** `managed_push_hook_first_prepare_wins`, `managed_two_daemons_share_one_ledger_winner`,
  `managed_lost_arm_reply_never_emits`, `managed_all_crash_points_suppress_replay`; assert losing/stale/restarted participant emits zero,
  winner emits at most once, arm-unknown emits none and remains suppressed. Run both durable backends; Mem reference remains unit
  conformance. Assertion anchors for the named tests (each variable is the observed result of the fixture operation described above):

```rust
assert!(rig.handoffs().len() <= 1);
assert_eq!(loser_handoffs, 0);
assert_eq!(lost_arm_reply_handoffs, 0);
assert_eq!(handoffs_after_restart, handoffs_before_restart);
```

- [ ] **Step 2: Run** `cargo test -p fl-cli --test wire_peer_delivery managed_`; expect FAIL against the specific missing/racy behavior, not
  a fixture/tool-discovery failure. If integration already passes, demonstrate each new assertion catches a deliberately omitted fence in a
  local temporary patch, then revert that patch before review.
- [ ] **Step 3: Complete isolated fixtures and resolve only exercised integration gaps.** Use fake pinned executables/local sockets, bounded
  polling/barriers and sandboxed child env; no real Codex/network discovery. Check every spec §7 crash row and preflight/arm interleaving;
  recipient barrier prevents later IDs overtaking uncertainty after restart. No hook/route event automatically creates timer-driven retries.
- [ ] **Step 4: Add limit table tests.** At exact/one-over bounds: prepare3, stdin64KiB, field4096, subject512/body262144, envelope512KiB,
  JSON1MiB/frame+14, summaries3×8KiB/row32KiB, error300 chars/annotation4KiB, connections64/active16/recipient1; existing128/512/1024/128
  caps stay fixed. Fake-clock deadline tests cover RPC5s, prepared20s, steer10s, queue60s, push85s, hook20s/stdin2s/stdout2s. Exhaustion
  holds or refuses without legacy fallback, spin or inference probe.
  Add black-box `managed_sixteen_orphans_restart_capacity_retirement`: fill16 Armed attempts on distinct recipients, lose their
  connection-bound settlement paths and restart the broker; a 17th unrelated recipient cannot prepare. An unproven/live old emitter keeps
  the refusal. After valid trusted exact-scope retirement, one unrelated prepare succeeds; the retired original and later IDs at its
  affected recipient remain suppressed. Assert zero new output, unchanged original outcome/ACK/prepares/barrier and no forged settlement.
  `managed_retirement_settlement_restart_races` covers both settlement/retirement orders, worker-exit races, duplicate/restarted retirement,
  stale/wrong scope or revision, reused process identity, crash before/after commit/lost reply and late authorized settlement: exactly one
  count decrement, no different/newer attempt owner cleared and tombstone persists. Fixtures positively close/join every registered emitter;
  test-only lifetime providers in isolated conformance binaries exercise pinned boot transitions where a real reboot is unavailable. No
  production flag/proof endpoint selects synthetic observations, and fixture boot evidence grants no installed-runtime approval.
- [ ] **Step 5: Run Step 2 command and store continuity subprocess tests**; expect PASS. Run existing focused operator, other-vendor,
  route-replacement and state-sync regression modules; require their existing assertions pass. Never equate accepted input/stdout completion
  with model consumption or exactly-once effects.
- [ ] **Step 6: Commit gate.** Fresh review checks independent black-box scenarios and no mirror-only tests; commit `test(wire): cover
  managed handoff races and crash fences` with the Freya trailer.

## Task 12: Disabled inspection documentation and separate cutover evidence artifact

**Files:** Create `docs/codex-peer-delivery.md`, `docs/codex-peer-delivery-cutover-evidence.md`; update CLI help only for implemented
inspection/manual ACK; keep install/service/wrapper/config files unchanged.

**Interfaces:** Documentation consumes actual implemented inspection schema and conformance results. Evidence template uses
`ArtifactEvidence { source_path, resolved_path, source_revision, build_identity, protocol_identity, sha256, argv_env_resolution, endpoint,
stop_fence_reference, continuity_reference, reviewer_verdict }` as named document fields, not a new runtime API. Unknown installed values
are explicitly `UNVERIFIED`; no secrets/session bodies/private configuration enter the public document.

- [ ] **Step 1: Write document acceptance assertions.** A reviewer can trace every evidence field to a source path or explicitly missing
  evidence; default policy is disabled; no enable/restart/replay sequence appears; unresolved fields produce BLOCKED. Inventory
  daemon/service/launcher, hook chain including unchanged wrapper resolution, pinned queue/control executable, CLI/manual clients,
  hub/state-sync/FerroStore writers, fallback/remote paths and old/open writers. Cover source/resolved path, digest, build/protocol,
  argv/env and endpoint for each.
- [ ] **Step 2: Review template before filling it.** Expected initial verdict BLOCKED: installed identity/transcript schema/hook host
  framing/runtime support and complete continuity/old-process fencing remain unproven. No mock or source test changes this verdict. No live
  inventory/probe runs without its separate authorization.
- [ ] **Step 3: Document evidence semantics and rollback limits.**
  VendorQueued/AppServerAccepted/HookEmissionCompleted/Unconfirmed attempt outcomes stay distinct from the independent persisted
  ManualReadAck/LegacyAckUnknown provenance fields, including when both kinds of evidence coexist. State recipient barrier and
  three lifetime prepares; safe new-ID manual instruction carries duplicate-effect risk and cannot clear the original barrier. Restore stays
  disabled; rollback retains compatible binaries with ledger intact, never old snapshot dedup claims. Document accepted persistence/egress
  support limitations and the managed-only sync reattachment blocker without implying durable pending recovery. The artifact is input for separate
  JARVIS/#150 review only, never activation approval or automatic enabling.
- [ ] **Step 4: Future final verification.** Run `cargo fmt --all --check`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo
  test --workspace`, `cargo test -p fl-cli --test wire_peer_delivery`, and `cargo test -p fl-store --test wire_delivery_continuity`.
  Expected all PASS, zero new warnings, isolated smoke captures no real external handoff. Record exact commands/revision/results and
  remaining activation blockers; do not claim unrun checks.
- [ ] **Step 5: Final commit/review gate.** Fresh task review then fresh whole-branch `code-review`; accepted fixes require fresh fixer and
  scoped re-review. Commit `docs(wire): record disabled peer delivery review evidence` with Freya trailer. Only after separately granted
  implementation/publication authority, follow WORKFLOW reviewer tier and current-head CI/approval gates for PR/landing. No activation
  follows merge automatically.

## Writing-plans self-review and handoff

Spec coverage: Tasks 1–2 cover canonical bytes, state/receipt semantics, budgets and Mem reference; Tasks 3–6 cover durable transactions,
all mutation/identity/import/prune fences, migration/continuity and disabled legacy enrollment; Tasks 7–10 cover trusted broker, framing,
final Arm, push/hook/manual and compatibility; Tasks 11–12 cover deterministic acceptance, regressions and separate rollout evidence. Every
Review Focus item maps to named owning tests. Signatures/types are defined in Interfaces before consumers; no later task may invent a
fallback or grant constructor.

Remaining gaps are activation evidence, not delegated implementation assumptions: exact installed transcript schema and hook host behavior;
installed Codex build/method contracts; all resolved executable/wrapper/daemon/store writer paths and digests; compatible old/open-process
fencing and continuity attestation. The evidence template records these as unverified. Lead message
`urn:uuid:01a1aef6-fede-73a1-a562-68d49f21594c` accepts Task 4's `Inner::Remote` persistence limitation (including same-host Unix
daemon-backed stores) and Task 5's constant reserved-delete projection plus managed reopen/reattachment refusal as disabled-v1 limitations.
Installation/activation for deployments depending on either unsupported path remains BLOCKED pending separately reviewed persistence/egress
continuity; no embedded fallback, new ledger, implied readiness, Owner approval or implementation/activation GO follows acceptance. Ordinary/
unmanaged behavior remains unchanged. These limitations grant no remote permit authority; the marker carries no distributed delivery
evidence/authority. Public docs include no private notes or operational configuration.

This plan and the unchanged snapshot are ready for review as documentation. Implementation commands, tests and commits above have not been
executed by this planning work. The next decision is plan acceptance and a separate explicit implementation GO; activation remains a later
JARVIS/#150 decision.
