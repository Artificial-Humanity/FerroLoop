# Codex peer Wire delivery v1 design candidate

**DESIGN CANDIDATE — 2026-11-06. Lead design choices accepted; final design approval pending this wording check. Implementation and activation remain unauthorized.**

This revises `freya-peer-delivery-143-proposal.md` (SHA-256 `980d6688464851374aba4a1d7ce31e03e5a9417925bc990e16297630fb63315d`). Ferris/Lead selections in message `01a1aea6-c44b-740e-bbff-ecc90be77d75` and acceptances in message `01a1aed1-1b9b-77b9-9f89-db1e83c74e56` are the design basis, not Owner approval or implementation GO. Operator PR #345 is merged at `3e9df03d`; the required ordering is satisfied and no new operator work is requested. After final design approval, the next step is a separate reviewable implementation plan; none is authored or executed here. Implementation review and separately authorized JARVIS/#150 rollout remain required. This is a specification, not an execution plan.

## 1. Selected contract

The following are **Lead selections**. Ferris also explicitly accepts the crate package and numerical limits below, the empty initial post-write rejection allow-list, the persistent recipient uncertainty barrier, and three lifetime prepares even when transient failures exhaust them. Held reasons remain inspectable; manual ACK and administrative annotation cannot clear uncertainty or reset counters. These are accepted design choices; final design approval remains pending this wording check.

| Lead selection | v1 consequence |
| --- | --- |
| Local Codex peer delivery, separate from Owner and Host | Preserve original Wire/reply/correlation/thread IDs. Peer text never obtains `OwnerSendNow`, Host lifecycle, or operator authorization. |
| One participating automatic handoff; uncertainty holds indefinitely | Durable arbitration key is `(original WireID, canonical registry agentID)`. No uncertain same-key replay, override, or administrative rearm. No exactly-once model-action claim. |
| First successful prepare wins | Push cannot preempt a prepared hook. An armed attempt cannot be taken over by elapsed time, route change, restart, or missing transcript. |
| Hook v1 is PostToolUse additionalContext only, one message/invocation | Stop, UserPromptSubmit, and SessionStart expose no managed message and do not claim/ack it. Unsupported or unprovable framing/identity disables that lane. |
| Only trusted configured local node `ai-lab-0` issues permits | Existing ferrowire daemon/private local socket brokers all managed attempts. No direct-store claim fallback, remote permit, automatic home selection, relocation API, or failover. |
| Stable registry identity; unresolved names and alias conflicts hold | Trusted local configuration plus registry selects home/recipient. Peer text, imported receipts, or a sender named Owner cannot select authority. |
| Separate evidence and manual acknowledgment | Queued, accepted steer, completed hook stdout, unconfirmed, explicit manual ACK, and legacy ACK provenance remain distinct. |
| Indefinite compact suppression, separate body retention | Protect ledger/identity fences across writes, prune, import, backup, migration, and restore. Missing continuity disables managed delivery. |
| Ship disabled with format/RPC/old-writer fences | Pre-cutover Pending/Delivered without proof is legacy uncertainty. No backlog seeding/reinjection. #150 restrictions remain binding. |

A person may deliberately publish a **new ID**, reference the uncertain original, and accept duplicate-effect risk. That is a separate manual instruction; it neither clears the original fence nor belongs to automatic recovery.

## 2. Source baseline and limits of evidence

All source references are relative to `/home/lmcfarlin/.codex/worktrees/cb30/FerroLoop`, read at clean HEAD `f4a3ff309799fb6f7aec7adc0a6647a40d57a930`; local `origin/main` is `2ffaff6df0ad985f2f780bf5551f78c085804fd4`. Recheck after operator landing. Private `notes/README.md`, `notes/STATE.md`, and `notes/north-star.md` were read through the umbrella checkout; the dated notes are not runtime evidence. No live session, daemon, queue, or hook was probed.

| Source | Existing contract requiring change or preservation |
| --- | --- |
| `crates/wire/src/dispatcher.rs:820`, `:859`, `:870`, `:880` | Codex queue returns unit/string; only pre-spawn `Spawn` permits candidate fallback. Post-spawn `Exit`, including timeout, is terminal within that call. |
| `crates/wire/src/dispatcher.rs:523`, `:2340`, `:2388` | Queue timeout is 60s. Outer list/side-effect/update still retries a changed route on an error and projects success to Acked; Delivered remains eligible. |
| `crates/wire/src/codex_control.rs:242`, `:287`, `:318`, `:341`, `:687` | Thread/direct-input/latest-turn preflight and expectedTurnId exist. Response IDs and accepted turn/thread are checked. Current Refused classification of JSON-RPC validation errors is not itself documented runtime proof permitting peer retry. |
| `crates/wire/src/codex_control.rs:32`, `:445`, `:470` | Steering has a 10s overall deadline and 1MiB bounds; preserve bounded connect/write/read handling. Source/mocks do not prove installed support. |
| `crates/cli/src/cmd/wire.rs:622`, `:655`, `:1253`, `:1342`, `:1416`, `:1447` | Read/Ack can fall back to store. Hook stdin is currently unbounded; transcript persona takes precedence, but hook reads+acks before stdout and before event filtering. Preserve identity/route guards; replace managed extraction. |
| `crates/wire/src/daemon.rs:176`, `:201`, `:355`, `:371`, `:881`, `:912` | Existing socket selection, unversioned local requests, daemon config, queue caps, and separate Read/Ack transactions do not supply delivery ownership. |
| `crates/cli/src/cmd/wire.rs:1214` | Node startup derives hostname; `DaemonConfig.node_id` alone does not establish a trusted managed home. |
| `crates/wire/src/daemon.rs:1416`, `:1419`, `:1491` | Unix bind attempts chmod0600 but ignores failure; accepted requests have no OS caller-credential check or caller-bound worker handle. These guarantees are missing prerequisites. |
| `crates/core/src/ferrowire.rs:217`, `:254`, `:285`; `crates/core/src/store.rs:830` | Strict immutable message/receipt schemas, monotonic receipt merge, and MessageStore without claim CAS. Receipt merge is not arbitration. |
| `crates/store/src/ferrostore/traits_wire.rs:293`, `:386`, `:487`, `:602` | Unread includes Delivered; transactional receipt update/import do not span vendor writes; prune deletes message/ID/inbox/log data. redb and MemStore require equivalent changes. |
| `crates/store/src/lib.rs:168`, `:173`; `crates/store/src/ferrostore/migrate.rs:32`, `:260` | Maximum format is 9; explicit table-copy migration needs ledger support as well as a version fence. |
| `crates/store/src/ferrostore/doc.rs:51`, `:70`; `mod.rs:2194`, `:2258`; `transaction.rs:42`; `session.rs:271`; `server.rs:2101`, `:2222` | Reserved `__` checks alone do not protect dependent nonreserved Wire/identity records or every transaction/sync surface. |
| `crates/exec/src/state_sync.rs:151`, `:436`, `:904`, `:1047` | Message/receipt sync is separate from local exclusive ownership. v1 adds no distributed arbitration/evidence protocol. |

Task-supplied observation: the same WireID appeared in hook context and later queued input. This is consistent with competing handoffs, not proof of two publishes or exact race order. A receipt-only Acked→Delivered change cannot close the race or the side-effect/persistence crash window.

## 3. Concrete implementation package

**Lead-accepted crate package:** one disabled-by-default, local-only v1 capability spanning the existing crates; no new daemon, external database, or service.

| Owner | Required responsibility |
| --- | --- |
| `fl-core`: new `wire_delivery` types/trait; existing `ferrowire`, `store`, `mem` | Versioned ledger, canonicalizer, typed outcomes, transition validation and atomic MemStore reference semantics. |
| `fl-store`: redb `ferrowire`, ferro `traits_wire`, `any`, migration/verification and store mutation boundaries | Equivalent serialized transactions, protected collections, format upgrade, export/backup/restore continuity, generic-write and old-writer fences. |
| `fl-wire`: daemon plus new internal broker/renderer module; dispatcher and `codex_control` | Broker-owned grants, local caller checks, versioned local RPC, typed queue/steer evidence, preflight separated from first mutating write. Existing operator semantics remain unchanged. |
| `fl-cli`: `cmd/wire`, trusted configuration | Strict managed hook, explicit manual-ACK protocol, fail-closed negotiation; no managed direct-store fallback. |
| `tests/` | Isolated subprocess/CLI/socket/race scenarios. Unit/conformance tests remain with owning modules. |

Use a backend-neutral `WireDeliveryStore` trait, forwarded by AnyStore. Low-level MessageStore operations remain available but enforce managed-record invariants. MemStore applies each ledger operation atomically under its existing state lock; persistent backends use one durable write transaction per transition. No transaction or global route lock spans external I/O.

Proposed protected logical collections/tables: `__wire_delivery_v1` (keyed ledger), `__wire_delivery_ids_v1` (global original-ID/digest reservation), and `__wire_delivery_control_v1` (recipient binding, cutover/continuity metadata and ordering fence). Raise format to **10 only if still unallocated at implementation review**, otherwise the next reviewed value, atomically with managed enrollment/first ledger write. Unknown formats and missing/corrupt required collections refuse managed operation; never create an empty replacement ledger.

### Local caller and authority boundary

**Required new checks, not existing capabilities:** managed startup requires an explicit reviewed `ai-lab-0` local policy and registry-ID allow-list, a configured absolute Unix socket in an owned private directory, verified daemon UID ownership, socket0600/directory0700, and hard failure on permission/ownership/symlink validation failure. `/tmp/fl-wire.sock` and environment hostname discovery cannot establish authority. The trusted policy and its hash are recorded at enrollment; imported registry data cannot silently enroll or relocate an agent.

On both ends verify OS peer UID against the configured trusted daemon UID (Linux SO_PEERCRED; another platform needs its separately reviewed equivalent). Assign a random daemon instance and server-generated connection/worker identity after this check. In-process push workers receive internal handles. Hook prepare/arm/settle share **one persistent verified local connection** and a server-issued random256-bit phase handle bound to key, lane, connection, daemon instance, revision, route and payload. Reconnection cannot recover a send permit. Caller strings/PIDs, argv0, persona environment, digest knowledge, copied tokens, and remote PSK authentication do not satisfy these checks.

The existing daemon handles one request per ordinary connection (`daemon.rs:1503`, `:1527`); managed protocol needs a bounded multi-request connection and negotiated `peer_delivery_v1` handshake. Version mismatch or broken connection refuses managed delivery without store fallback. Inspection redacts handles. Remote frames expose no prepare/arm/settle/admin verbs.

Same-UID processes and the local administrator are trusted: arbitrary code with socket/store/vendor access can bypass this protocol. These checks prevent untrusted peer/stale participating callers from acquiring authority, not malicious same-UID code. No new Owner/peer authorization framework is implied. Until these connection checks and identity evidence exist, **managed delivery stays disabled**.

## 4. Durable state and byte binding

Each ledger row stores original ID/generation digest; canonical recipientID and binding revision; trusted authorityID/local epoch; CAS revision; eligibility; persisted prepare count; active attempt; and compact terminal evidence. Attempts contain random256-bit token, daemon/worker binding, lane (`push`, `hook`), selected transport (`steer`, `queue`, `stdout`) frozen at arm, phase, pinned Codex thread/route generation/fingerprint, expected turn if steering, renderer version, frozen-payload digest/reference, timestamps, and typed outcome. Tokens authorize no vendor-side deduplication.

Resolve durable registry `AgentRecord.id` (`crates/core/src/agent.rs:200`), never a lowercased name. Unknown/ambiguous names remain readable but held. Coalesce receipt aliases transactionally; binding a formerly unresolved name requires unique registry evidence and no conflicting prior reservation. Renames retain the binding; recycled names cannot redirect old messages. An alias fold encountering distinct armed/spent histories preserves both and holds; v1 has no general identity relocation/merge repair API.

**Canonicalizer proposal:** `wire-delivery-c14n-v1` / SHA-256. Define `LP(b)=u64be(byte_length)||b`. Hash `LP(UTF8("FerroLoop/WireDelivery/generation/v1"))` then LP-encoded fields, in exact order: `id, thread_id, reply_to, correlation_id, sender_id, sender_name, sender_host, to, kind, urgency, durable, subject, body, created_at, deadline_at`. Strings use stored UTF-8 unchanged; At uses `as_str()`'s fixed24-byte spelling (`crates/core/src/at.rs:19`, `:83`); option is `0` absent or `1||LP(value)` present; bool is one byte0/1. Selector encoding is `LP(tag)` then LP-encoded components: `agent_name(name)`, `agent_id(IRI)`, `all`, `area(area)`, `role(role)`, `area_role(area,role)`, `council`, `group(group)`. Preserve selector kind. Kind/urgency use current `as_wire()` values (`ferrowire.rs:162`, `:180`); role uses `AgentRole::as_wire()` (`agent.rs:134`), including `custom:<slug>`. Golden fixtures pin these spellings; schema evolution requires a new reviewed canonicalizer.

For this **single-message** v1, payload digest is SHA-256 of `LP("FerroLoop/WireDelivery/payload/v1")||LP(renderer_version)||u64be(1)||LP(raw32byte_generation_digest)||LP(exact_payload_bytes)`. At prepare, the broker recomputes the generation digest and validates the immutable-message binding. After read-only preflight, freeze the selected transport and exact payload: queue's exact UTF-8 message argument, hook's complete JSON line including newline, or steer's complete serialized WebSocket frame including JSON/RPC ID/expectedTurnId and mask. The final steer frame and expectedTurnId are established after read-only preflight; prepare does not require them. At arm, the broker revalidates the generation/immutable-message binding and recomputes and validates the final payload digest before the durable arm commit grants any mutation. The broker renders from immutable stored fields and rejects arbitrary caller payload substitution. Unknown versions hold. Send the frozen bytes once; no payload substitution or late re-render after arm. Route/transport/executable identity is pinned separately. Pin renderer `wire-peer-envelope-v1` to `dispatcher.rs:37`/`:49`: `[fl-wire <kind>/<urgency>] from <sender_name> (id: <id>, thread: <thread_id>): <subject>\n\n<body>`, interpolating unchanged stored fields. Reply/correlation IDs remain unchanged in immutable storage/digests. Hook context uses this same envelope. No batch arming exists.

### Transition contract

| Transition | Durable rule |
| --- | --- |
| Eligible→Prepared | Serializable resolve/digest/ACK/expiry/home check; first prepare wins; increment persisted prepare counter and bind worker. Losers emit nothing. |
| Prepared→Eligible/Held | CAS release only with typed proof that this worker never armed; counter never resets. Disconnect/restart/expiry may invalidate Prepared by CAS; stale arm then fails. |
| Prepared→MutationArmed | Revalidate key/generation/immutable-message binding/identity/route/receipt/expiry/revision and recompute/validate the final frozen-payload digest after preflight; durable commit precedes spawn, steer write or first stdout byte. Only the first confirmed arm reply on the bound live connection grants one use. Repeated arm/inspection grants none. |
| MutationArmed→Settled | Typed evidence with matching token/revision; atomic evidence and receipt projection. Identical settlement is idempotent; contradictory evidence holds. Store failure/lost reply preserves suppression. |
| Armed→new eligibility | Only reviewed typed proven-no-mutation evidence permits bounded new preparation. No expiry takeover or administrative uncertain rearm. |
| Any→ManualReadAck/legacy ACK | Revoke only unarmed attempts; after arm record ACK alongside evidence without claiming cancellation/consumption. |

Arm commit error, timeout, or lost arm reply means **do not send**, even if inspection later finds Armed. Crash after arm but before any output is deliberately indistinguishable from possible mutation and holds indefinitely. Armed uncertainty also persists a recipient ordering barrier, matching the existing daemon's refusal to let later messages overtake an unconfirmed push (`daemon.rs:778`); new IDs do not bypass that barrier automatically. Administrative annotation/closure cannot clear it by inventing nonmutation.

## 5. Lane behavior and evidence

Push prepares first, pins one route and executable, and performs read-only preflight. Active delivery requires exact thread identity, `canAcceptDirectInput:true`, status active, exactly one latest `inProgress` turn with nonempty ID; arm then write one `turn/steer` with expectedTurnId. Accept only matching RPC ID, exact turn and matching thread if supplied. Positive matching idle metadata with direct input allowed permits queue under the same prepare: arm before a single OS spawn of one pinned executable with `queue --thread ... --message ...`. Preserve the Codex child environment allow-list; managed delivery does not call the existing spawn-retry helper. Queue success records VendorQueued. Idle→active races never trigger an additional steer.

Unsupported/unloaded/ambiguous preflight never authorizes queue. Release safely before arm; a later eligible hook/push event may consume another prepare. Once armed, generic rediscovery, changed-route retry, candidate-binary fallback, or receipt-derived retry is forbidden. No turn/start/resume/interrupt/clear/compact is added for peer delivery.

**Hook profile proposal `codex-posttooluse-v1`:** require explicit Codex lane selection, exact `hook_event_name:"PostToolUse"`, nonempty `session_id`, absolute `transcript_path` and `cwd`; reject conflicting aliases/duplicate keys. Require a reviewed local transcript schema/parser fixture tying transcript session header to session_id, transcript persona to unique registryID, and current registry/route to the same Codex thread and route revision. Shared daemon AGENT_PERSONA or a path containing `.codex` is insufficient. Bounded, contained, owned transcript lookup must preserve current transcript-first/route-replacement guards without registering a replacement route as proof. The exact installed transcript header/schema and hook host behavior remain **unproven activation blockers**.

Select at most one eligible message by MessageID order; prepare/render/arm before stdout. The only managed output is compact UTF-8 JSON plus LF:

```json
{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"Incoming FerroWire message(s) for '<persona>':\n\n<one existing peer envelope with original ID>"}}
```

No eligible item or rejected event/identity returns `{}` plus LF, with no ACK/claim. Complete write+flush records HookEmissionCompleted, which proves only local stdout completion, not host acceptance or model consumption. Any partial write/pipe failure after arm is unconfirmed. No managed Stop/block reason, UserPromptSubmit, SessionStart, or batch response exists. Exact request/output bytes, strict parsing and identity fixtures plus supported-runtime host evidence must be reviewed before enabling this profile; mocks alone cannot establish runtime support.

| Observation | Evidence/eligibility |
| --- | --- |
| Instrumented pre-arm failure; trusted OS spawn failure proving no child | PreMutationRefused/DefinitiveNoMutation; bounded future prepare after CAS. No same-invocation retry. |
| Matching documented no-side-effect RPC rejection | Eligible only for a reviewed versioned tuple of runtime build, method, error shape/code and contract reference. **v1 initial allow-list is empty**; current -32600/-32601/-32602 source classification alone cannot reopen. |
| Spawned child nonzero/timeout/kill/lost status; post-write error/mismatch/EOF/timeout | MutationUnconfirmed; indefinite suppression. stderr, transcript absence, route changes and timeout are never nonmutation proof. |
| Successful queue / exact steer acceptance / complete hook stdout | VendorQueued / AppServerAccepted(thread,turn) / HookEmissionCompleted; suppress all automatic lanes; project Delivered. |
| Explicit new manual read+ACK or ACK | ManualReadAck; project Acked without consumption claim. Legacy/imported ACK without provenance is LegacyAckUnknown and suppresses. |

There is no public recovery-proof submission or replay API. Trusted worker-produced evidence alone uses typed settlement; administration may inspect, annotate or close while retaining fences. Unconfirmed keeps its prior compatibility receipt; ledger eligibility is authoritative.

## 6. Accepted measurable limits

These are **Lead-accepted design constants**, not claims about installed behavior. Check every bound before arm; exhaustion persists Held/BudgetExhausted and never falls back to legacy delivery.

| Resource | v1 hard bound and exhaustion behavior |
| --- | --- |
| Prepare budget | 3 lifetime prepares/key, including crashed/refused/transiently failed preflight; persisted, never reset by restart/ACK/admin. One prepare and one potential handoff per invocation; later explicit arrival/hook/route event may request another safe prepare. No timer retry loop. |
| External mutation | One spawn or one steer frame or one stdout response per armed attempt. No child/steer probing/model turn to investigate delivery. |
| Message/output | Existing subject512/body262144 bytes; managed identity/path fields each4096 bytes; hook stdin64KiB; envelope512KiB; encoded hook JSON or steer JSON1MiB (steer frame +14 bytes). Oversized existing/imported data stays readable but held for automatic delivery. |
| Deadlines | Local RPC operation5s with bounded connect; prepare→arm lifetime20s; steer connect/preflight/ack total10s; queue runtime60s; push invocation total85s. Hook invocation20s, including stdin2s/stdout2s. Remaining deadline bounds every operation; after arm any timeout holds. Prepared expiry is CAS invalidation, never an armed lease. |
| Concurrency/metadata | 16 managed active attempts total, 1/recipient; max64 managed protocol connections; retain existing daemon128 dispatch workers/512 tracked recipients/1024 pending IDs per recipient/128 overflow-recipient bounds. Managed limits never raise these. Full capacity refuses prepare; overflow keeps persisted inbox/fence and does not spin. |
| Evidence | At most3 attempt summaries; each≤8KiB, total ledger row≤32KiB excluding frozen payload. Error excerpt≤300 chars; annotation aggregate≤4KiB. Overflow sets held flag without deleting identity/digest/suppression. Frozen payload≤1MiB+14; one active copy/key. |
| Retention/spend | Compact ID/digest/recipient/authority/outcome reservations indefinite; no finite expiry reopens them. Body/frozen-payload retention follows reviewed protected policy; unresolved bodies are not automatically pruned. Delivery adds no autonomous retry or inference-probe loop and invokes no turn/start RPC. Delivered input may cause model work; preserve applicable existing budget checks without claiming native sessions are universally gated. |

## 7. Compatibility, persistence and crash acceptance

Read-only inbox inspection remains available and grants no emission permit. New explicit manual read+ACK/ACK calls identify intent. New daemon rejects legacy acking Read/Ack for managed recipients; an old client cannot self-label as a hook or owner. New client against old daemon refuses managed operation. Managed direct CLI send may persist a message but cannot dispatch by alternate-store fallback. Disabling policy stops claims while retaining fences and does not restore legacy automatic delivery.

Protected typed transactions must cover generic put/delete, transaction/session commit, raw/batch/sync operations, registry binding changes, send/import/update/prune and delete+recreate. Managed immutable message/identity records cannot bypass the ledger merely because their collection lacks `__`. Imported receipts can suppress/display, never grant permits or choose home. Existing immutable-ID conflicts hold/reject; late identical import after body prune retains suppression. No remote delivery ledger/handle synchronization is added.

Backup/export and redb→ferro migration must include all three collections, binding/control metadata, ID reservations and evidence, with row/digest/projection verification before reopen. Local export is continuity data, not peer permit import. Restore enters disabled state; only separately reviewed continuity/fencing evidence can resume service. A stale snapshot or missing ledger cannot establish continuity. Future physical relocation requires Owner/JARVIS old-process fencing and intact ledger transfer; v1 provides no relocation API. A stored epoch cannot fence an already armed old process at the vendor.

| Crash point | Required restart result |
| --- | --- |
| Before prepare commit | No grant; eligible only if transaction definitively absent. |
| Prepared commit/reply lost; worker disconnects | No output; CAS invalidate unarmed row; counter retained. |
| Arm commit unknown/reply lost | No output permission; preserve any armed fence, hold unresolved state. |
| Armed, before spawn/write; partial stdout/frame; child accepted before settlement | Unconfirmed indefinitely; no replay. |
| Accepted/completed, settlement fails | Armed fence suppresses; evidence may settle idempotently, never resend. |
| Settlement commits, reply lost | Inspection shows settled; duplicate settlement harmless; no new permit. |
| Old process suspended/restarted; ACK/import races arm | Stale revision fails before arm; after arm ACK cannot retract side effect. |
| Ledger/continuity absent/corrupt or restore stale | Managed delivery disabled, no empty-ledger initialization. |

## 8. Disabled cutover and review gates

Pre-cutover Pending/Delivered with no ledger proof becomes **LegacyUncertain**, not new eligibility; Acked becomes LegacyAckUnknown and Expired stays suppressed. Enrollment writes suppression metadata only, never claims/seeds backlog for delivery. No replay, queue purge or reinjection is part of design/rollout. Already-open old daemons must be retired before format activation: open-time format checks cannot stop existing writers. Older binaries refuse upgraded stores; rollback retains compatible binaries with policy disabled, never deletes ledger or restores an old snapshot as dedup-safe recovery.

**Inventory for separate JARVIS/#150 review:** each artifact needs absolute source/resolved path, build/source/protocol identity and digest, argv/env resolution, and matching stop/fence evidence. Current installed values are not asserted here.

| Artifact to inventory | Known source/history; unresolved activation evidence |
| --- | --- |
| ferrowire service/launcher and `fl wire daemon` | Existing service name `ferrowire.service`; exact installed launcher, process tree, build and store/socket endpoint unverified. |
| Hook chain | Historical read-only observation: `~/.codex/hooks.json` PostToolUse/Stop→`/usr/local/bin/agent-bus-claude-hook`→AI-Lab-AMD `scripts/agent-bus/agent_bus/pre_compact.py`→`/home/lmcfarlin/.local/bin/fl` with hook argv0. Current resolved paths/digests/framing remain unverified. |
| Codex queue/control targets | Source accepts FL_WIRE_CODEX_BIN, PATH codex and `~/.local/bin/codex`; managed v1 pins one reviewed absolute executable and control socket. Installed path/build/capability unverified. |
| Other emitters/store writers | CLI/manual clients, daemon workers, hub, state sync, FerroStore/ferrostore clients, fallback and remote dispatch paths require complete compatible build/path inventory or explicit fencing. |

An unchanged wrapper resolving to that script and invoking `fl` is **not activation approval**. Do not alter/bypass the wrapper under #150. If its permitted invocation cannot participate in this protocol, managed delivery stays disabled. Coordinated inventory/stop/drain/restart and any live validation require their own concrete reviewed authorization; none is performed here.

Before implementation can be accepted, tests must deterministically cover: generation/payload golden vectors; three-backend CAS/conformance and migration/reopen; push/hook/two-daemon races; binding/alias conflicts; stale/foreign/copied handles; strict PostToolUse identity/framing and disabled events; positive idle versus ambiguous preflight; exact steer acceptance and rejection allow-list; every crash row above; spawned-child timeout without retry; ACK/import/prune/generic-write/sync bypass; old/open writers and mixed RPC versions; limits/counters across restart; stale restore; and operator/other-vendor nonregression. Count handoffs, not model actions. Unit tests stay in modules; subprocess cases use isolated HOME/CODEX_HOME/store/socket/routes and fake binaries/sockets with no live discovery. Implementation must pass cargo test, warning-free cargo clippy --all-targets and integration smoke checks. No implementation tests were run for this document.

**Remaining Lead question:** final design approval after this prepare/arm wording check. Message `01a1aed1-1b9b-77b9-9f89-db1e83c74e56` accepts the crate package, numerical bounds, empty initial post-write rejection allow-list, persistent recipient uncertainty barrier, and three lifetime prepares even when transient failures exhaust them; held reasons remain inspectable, and manual ACK/administrative annotation cannot clear uncertainty or reset counters. Missing installed identity/framing/build/continuity evidence remain activation blockers to prove, not invitations to assume authority. Operator PR #345 is already merged at `3e9df03d`, satisfying the ordering requirement. Only after final design approval comes a separate reviewable implementation plan; no implementation plan is authored or executed here, and no Owner approval, implementation GO, or new operator work is implied.
