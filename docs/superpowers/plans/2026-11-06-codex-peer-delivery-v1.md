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
  `DeliveryEvidence::{PreMutationRefused,DefinitiveNoMutation,MutationUnconfirmed,VendorQueued,AppServerAccepted{thread:String,turn:String},HookEmissionCompleted,ManualReadAck,LegacyAckUnknown}`;
  only the first two permit safe release; error details are separately bounded strings.
- `AttemptSummary { binding: AttemptBinding, route: RoutePin, frozen: Option<FrozenPayload>, prepared_at: At, armed_at: Option<At>,
  evidence: Option<DeliveryEvidence>, detail: String }`.
- `DeliveryRecord { schema:u16, key:DeliveryKey, generation:Digest, binding:Binding, revision:Revision, eligibility:Eligibility,
  prepares:u8, attempts:Vec<AttemptSummary>, held:Option<HoldReason>, annotation:String }`; row schema starts at 1.
- `RecipientControl { binding:Binding, barrier:Option<DeliveryKey> }`; `Continuity { schema:u16, cutover:At, policy_hash:Digest,
  local_epoch:u64, enabled:bool, intact:bool }`; `IdReservation { original_id:MessageId, generation:Digest }`.
- `DeliverySnapshot { records:Vec<DeliveryRecord>, ids:Vec<IdReservation>, recipients:Vec<RecipientControl>, continuity:Continuity }`;
  `EvidenceView { lane:Lane, route:RoutePin, prepared_at:At, armed_at:Option<At>, evidence:Option<DeliveryEvidence>, detail:String,
  payload_digest:Option<Digest> }`; `DeliveryView { key:DeliveryKey, generation:Digest, binding:Binding, revision:Revision,
  eligibility:Eligibility, prepares:u8, attempts:Vec<EvidenceView>, held:Option<HoldReason>, annotation:String, barrier:Option<DeliveryKey>
  }` is the redacted inspection form without tokens/frozen bytes.
- `PrepareInput { key:DeliveryKey, generation:Digest, binding:Binding, attempt:AttemptBinding, route:RoutePin, now:At }`; `ArmInput {
  key:DeliveryKey, revision:Revision, attempt:AttemptBinding, route:RoutePin, frozen:FrozenPayload, now:At }`.
- `ReleaseInput { key:DeliveryKey, revision:Revision, attempt:AttemptBinding, proof:DeliveryEvidence, now:At }`; `SettleInput {
  key:DeliveryKey, revision:Revision, attempt:AttemptBinding, evidence:DeliveryEvidence, detail:String, now:At }`.
- `ManualAckInput { key:DeliveryKey, at:At }`; `EnrollmentInput { bindings:Vec<Binding>, continuity:Continuity }`. -
  `DeliveryCommand::{Enroll(EnrollmentInput),Prepare(PrepareInput),Arm(ArmInput),Release(ReleaseInput),Settle(SettleInput),ManualAck(ManualAckInput),InvalidatePrepared{key:DeliveryKey,revision:Revision,now:At},Annotate{key:DeliveryKey,text:String},Close{key:DeliveryKey}}`.
  - `DeliveryResult::{Prepared{record:DeliveryRecord},Armed{record:DeliveryRecord},Applied{view:DeliveryView},Refused{reason:HoldReason}}`;
  `Armed` is durable state, **not** an emission permit.
- `WireDeliveryStore::apply_delivery(&self, command:DeliveryCommand)->Result<DeliveryResult,StoreError>`;
  `inspect_delivery(&self,key:&DeliveryKey)->Result<Option<DeliveryView>,StoreError>`;
  `delivery_snapshot(&self)->Result<DeliverySnapshot,StoreError>`.
- `TransitionContext { snapshot:DeliverySnapshot, message:Option<WireMessage>, receipts:Vec<DeliveryReceipt>, agents:Vec<AgentRecord> }`;
  `DeliveryDelta { snapshot:DeliverySnapshot, receipts:Vec<DeliveryReceipt>, result:DeliveryResult }`.
- `transition(ctx:TransitionContext,command:DeliveryCommand)->Result<DeliveryDelta,StoreError>` validates all invariants; stores construct
  context and apply delta inside one native atomic operation.
- `canonical_generation(msg:&WireMessage)->Result<Digest,StoreError>`;
  `canonical_payload(generation:Digest,renderer:&str,bytes:&[u8])->Result<Digest,StoreError>` in `c14n.rs`.

- [ ] **Step 1: Write failing unit tests.** Local test fixture `golden_message()` uses
  id=thread_id=`urn:uuid:00000000-0000-7000-8000-000000000001`, all options None, sender_name=`alice`, to=AgentName(`bob`), kind=Mail,
  urgency=Normal, durable=false, subject=`Hello`, body=`Body`, created_at=`2026-11-06T00:00:00.000Z`.
  `managed_c14n_golden_fields_and_selectors` compares literal LP byte vectors and independent SHA-256 constants for all 16 fields in spec
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
deterministic IDs/tokens/time; no threads share `MemStore` across its non-Sync boundary.

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
  borrow; validate before replacing state and receipt projection together. No randomness or clock enters MemStore. Prepared expiry
  invalidates by CAS; arm at 20s fails; arm never acts as an expiring lease. Persist recipient barrier for unconfirmed Armed; ACK/admin
  cannot clear it. Capacity refuses without consuming another message's history.
- [ ] **Step 4: Extend and run** `cargo test -p fl-core managed_`; expect PASS for stale revision, wrong worker, immutable digest mismatch
  at prepare/arm, expiry/ACK racing arm, identical settlement idempotence, contradictory settlement Held, evidence bounds, one frozen copy
  and barrier across new IDs. Successful queue/steer/stdout project Delivered; manual ACK projects Acked; unconfirmed preserves prior
  receipt.
- [ ] **Step 5: Commit gate.** Fresh reviewer checks full transition table, empty post-write rejection allow-list and no model-action
  guarantee; commit `feat(core): add atomic peer delivery reference semantics` with the Freya trailer.

## Task 3: Native durable redb delivery transactions

**Files:** Create `crates/store/src/wire_delivery.rs`; modify `crates/store/src/lib.rs`, `ferrowire.rs`; unit tests in the new module.

**Interfaces:** Consume Task 1 `WireDeliveryStore` and Task 2 conformance. Produce `impl WireDeliveryStore for RedbStore`; internal
`apply_delivery_in(tx:&redb::WriteTransaction,command:DeliveryCommand)->Result<DeliveryResult,StoreError>`. Table names exactly
`__wire_delivery_v1`, `__wire_delivery_ids_v1`, `__wire_delivery_control_v1`; strict versioned JSON values, collision-free key encoding
(length-prefixed original ID and registry ID, hex key).

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
  Allocate `FORMAT_WITH_WIRE_DELIVERY` as 10 only after rechecking `FORMAT_LADDER`; update ladder/error newest atomically with
  enrollment/first ledger write on both backends (ferro completion follows Task 4). No managed caller is connected yet.
- [ ] **Step 4: Run** `cargo test -p fl-store managed_redb_`; expect PASS. Include corrupt/missing table and unknown row/schema checks:
  refuse managed operations without silently creating replacements; pre-enrollment ordinary stores retain their old format.
- [ ] **Step 5: Commit gate.** Fresh review checks durable commit boundaries and no policy activation; commit `feat(store): persist redb
  peer delivery transitions` with the Freya trailer.

## Task 4: Native ferro transactions and AnyStore forwarding

**Files:** Create `crates/store/src/ferrostore/traits_wire_delivery.rs`; modify `ferrostore/mod.rs`, `ferrostore/doc.rs`,
`crates/store/src/any.rs`. Unit tests in the owning modules.

**Interfaces:** Consume Tasks 1–3; produce `impl WireDeliveryStore for FerroStore`, forwarding `impl WireDeliveryStore for AnyStore`,
internal `apply_delivery_in(tx:&mut super::Txn,command:DeliveryCommand)->Result<DeliveryResult,StoreError>` in the ferro module. Use the
same logical collection names/schema/key encoding as redb, within existing `with_write` transaction machinery.

- [ ] **Step 1: Write failing unit tests.** `managed_ferro_transaction_conformance` runs shared assertions;
  `managed_any_store_forwards_delivery` covers both variants; `managed_ferro_atomic_reopen` injects append/sync errors and validates no
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
  reservations, binding revision and receipt log. Raw remote authentication conveys no permit authority. Plan proposal: implement native delivery transactions for embedded FerroStore; an `Inner::Remote` handle returns `UnsupportedBackend` for the new delivery trait while preserving unrelated remote APIs. This is a persistence-support limitation, not an approved prohibition on remote persistence: a deployment using that backend remains disabled until its compatible local transaction path is separately reviewed. No managed prepare/arm/settle verbs are added to remote RPC. Enforce both schema step and required collection validation on reopen; never synthesize a missing managed history.
- [ ] **Step 4: Run the Step 2 commands**; expect PASS including two local handles racing CAS, reopen counters, recipient ordering barrier
  and unknown-format refusal. Mem/redb/ferro must yield identical conformance outcomes.
- [ ] **Step 5: Commit gate.** Fresh review checks native serialization rather than read-then-write emulation; commit `feat(store): add
  ferro peer delivery transactions` with the Freya trailer.

## Task 5: Identity, import/prune and every generic mutation fence

**Files:** Create `crates/store/src/wire_delivery_guard.rs`; modify `crates/core/src/mem.rs`, `crates/store/src/{ferrowire,state}.rs`,
`ferrostore/{doc,mod,traits_wire,traits_state,transaction,session,server}.rs`; preserve existing registry APIs. Unit tests stay in these
modules.

**Interfaces:** Consume `Binding`, `IdReservation`, `RecipientControl`, Task 1 transition inputs. Produce internal
`MutationOrigin::{Public,PeerSync,TypedWire,TypedRegistry,Delivery,Continuity}` and
`admit_delivery_mutation(snapshot:&DeliverySnapshot,origin:MutationOrigin,collection:&str,id:&str,before:Option<&serde_json::Value>,after:Option<&serde_json::Value>)->Result<(),StoreError>`
in the guard module. Origins are crate-private constructed call paths, never caller-provided strings/remote fields. Core applies equivalent
identity/receipt/prune invariants under its own borrow without depending on fl-store.

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
  recycled name cannot redirect. Imported receipts may suppress/display but never enroll, relocate, grant or clear uncertainty. Explicitly
  exclude local ledger/handles/frozen payload from outbound batches and unchanged forwarded inbound batches: reserved `__` operations
  currently can travel outbound. Fence inbound ordinary dependent records, remote cache-fill (`fill_doc_miss`/`install_fetched_doc`) and
  lazy fetch as well as writes. Do not sync local delivery authority.
- [ ] **Step 4: Add** `managed_identity_alias_conflict_holds`: unresolved name later uniquely bound works only without prior conflict;
  distinct armed aliases retain both histories and hold. Add `managed_prune_identical_reimport_stays_suppressed`: prune eligible retained
  body per protected policy, keep ID/digest/binding/outcome reservation, identical import stays suppressed, changed same-ID body
  rejects/holds. Unresolved bodies never auto-prune.
- [ ] **Step 5: Run** `cargo test -p fl-store managed_` and `cargo test -p fl-core managed_`; expect PASS, including ACK/import racing Arm
  and sync bypass attempts. Verify unauthorized mutation is atomic rejection, not a partially applied batch.
- [ ] **Step 6: Commit gate.** Fresh review audits the call graph for every unchecked/native mutation surface before approving; commit
  `fix(store): fence managed wire identity and mutation paths` with the Freya trailer.

## Task 6: Migration, local continuity export/restore and legacy enrollment

**Files:** Create `crates/store/src/wire_delivery_continuity.rs`, `crates/store/tests/wire_delivery_continuity.rs`; modify
`crates/store/src/{any,lib}.rs`, `ferrostore/{migrate,verify,compact}.rs` as needed by their existing copy/verification paths. No
deployment/backup scripts change.

**Interfaces:** Consume Task 1 snapshot/control and prior backends. Produce `ContinuityArchive { schema:u16, source_format:u64,
snapshot:DeliverySnapshot, messages:Vec<WireMessage>, receipts:Vec<DeliveryReceipt>, agents:Vec<AgentRecord>, digest:Digest }` and
`ContinuityReport { intact:bool, disabled:bool, rows:u64, digest:Digest }` in the new module. Define store-local extension trait
`DeliveryContinuityStore: WireDeliveryStore` with `snapshot_delivery_continuity(&self)->Result<ContinuityArchive,StoreError>` (one
consistent snapshot of delivery plus messages/receipts/registry) and
`restore_delivery_disabled(&self,archive:&ContinuityArchive)->Result<ContinuityReport,StoreError>` (one typed native transaction); implement
for RedbStore/FerroStore/AnyStore. APIs: `export_delivery_continuity(store:&impl
DeliveryContinuityStore)->Result<ContinuityArchive,StoreError>` and `restore_delivery_continuity(store:&impl
DeliveryContinuityStore,archive:&ContinuityArchive)->Result<ContinuityReport,StoreError>`. No public resume or permit import API.

- [ ] **Step 1: Write failing tests.** `managed_migration_copies_every_fence`: redb→ferro verifies all three collections,
  row/digest/projection/bindings/barrier/counters and read-only source retention. `managed_restore_never_resumes`: export then restore into
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
  canonical message and payload bindings, compact reservations, receipt projections and schema before publication/reopen. Existing manifest
  export remains a project manifest, not a continuity backup; document its insufficiency. Extend physical checkpoint/compact/copy
  verification so managed collections cannot disappear. Restore always disables, even a valid archive; stored epoch alone never proves
  old-process fencing.
- [ ] **Step 4: Add subprocess** `managed_old_open_writer_requires_external_fence`: isolated fixtures demonstrate old-format opener refuses
  upgraded copy but already-open writer is not fenced by that check; assert enrollment/cutover evidence remains blocked without external
  fence attestation. Fixture binary lives under test support, not an installed historical runtime. Refuse stale restore continuity without
  claiming to detect all out-of-band snapshot replacement.
- [ ] **Step 5: Run Step 2 commands**; expect PASS; additionally require reopened archive/migration preserves three prepares and recipient
  barrier. No managed integration proceeds until Tasks 5–6 reviews pass.
- [ ] **Step 6: Commit gate.** Fresh review checks absence of automatic resume and distinction between local continuity export and peer
  synchronization; commit `feat(store): preserve disabled peer delivery continuity` with the Freya trailer.

## Task 7: Trusted local broker, persistent RPC and one-use arm reply

**Files:** Create `crates/wire/src/wire_delivery/{mod,policy,protocol,broker,render}.rs`; modify `crates/wire/src/{lib,daemon}.rs`,
`crates/cli/src/{config,cmd/wire}.rs`; unit tests in those modules.

**Interfaces:** Consume store contract and complete fences. Define `ManagedPolicy { enabled:bool, node_id:String, daemon_uid:u32,
socket:PathBuf, registry_allowlist:Vec<AgentId>, codex_executable:PathBuf, codex_executable_digest:Digest, codex_control_socket:PathBuf,
transcript_root:PathBuf, policy_hash:Digest }` in policy module; default is disabled, no enrolled IDs, no inferred home.
`VerifiedConnection`, `ManagedClient`, `ManagedBroker<S:WireDeliveryStore>` and `WorkerContext` are opaque runtime types. Define
`InvocationBudget` using monotonic deadline and operation minima. Define `PreparedAttempt` (key/revision/attempt/route) and non-Clone
`EmissionPermit` (private fields, consumed once).
`ManagedBroker::prepare(&self,worker:&WorkerContext,input:PrepareInput)->Result<PreparedAttempt,StoreError>`;
`arm(&self,worker:&WorkerContext,prepared:PreparedAttempt,frozen:FrozenPayload,now:At)->Result<EmissionPermit,StoreError>`;
`settle(&self,worker:&WorkerContext,input:SettleInput)->Result<DeliveryView,StoreError>`. Store Armed alone never creates a permit.
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
`dispatch_peer<S:WireDeliveryStore>(broker:&ManagedBroker<S>,worker:&WorkerContext,key:DeliveryKey,budget:InvocationBudget)->Result<DeliveryView,StoreError>`
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
  alongside evidence, preserves possible mutation/barrier, receipt Acked. `managed_legacy_clients_never_claim`: old acking Read/Ack rejected
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
  managed disabling with existing fences, inspection redaction/held reasons and ACK/import races. Add
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
  VendorQueued/AppServerAccepted/HookEmissionCompleted/Unconfirmed/ManualReadAck/LegacyAckUnknown stay distinct. State recipient barrier and
  three lifetime prepares; safe new-ID manual instruction carries duplicate-effect risk and cannot clear the original barrier. Restore stays
  disabled; rollback retains compatible binaries with ledger intact, never old snapshot dedup claims. The artifact is input for separate
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
fencing and continuity attestation. The evidence template records these as unverified. The Task 4 remote-backed persistence support limitation is an implementation-plan proposal requiring review; it does not claim that the design grants remote permit authority or forbids all remote persistence. Public docs include no private notes or operational configuration.

This plan and the unchanged snapshot are ready for review as documentation. Implementation commands, tests and commits above have not been
executed by this planning work. The next decision is plan acceptance and a separate explicit implementation GO; activation remains a later
JARVIS/#150 decision.
