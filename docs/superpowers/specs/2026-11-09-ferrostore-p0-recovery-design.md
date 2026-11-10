# FerroStore P0: inventory and recovery baseline

Status: **design for Ferris and Jarvis review; no implementation or operational approval**.
Author: Fiona. Date: 2026-11-09. Source baseline: `main@cd12cada`.

## Mandate and limits

Council Petition 2 recommends stabilizing ownership and proving recovery before
changing engines or cutting clients over to strict managed access. This document
specifies P0. It does not implement P1 lifetime ownership, P2 reconnect/receipts,
service changes, migration, compaction, or an engine comparison.

The primary decision record is FerroState, scope `project:the-council`, key
`ferris-petition-2-synthesis`, revision
`urn:uuid:01a1c228-4c29-7240-a245-41b7a736aaac`, especially “Backup and restore
contract” and P0. See also the [runtime architecture inventory](../../architecture/ferrostore/README.md).
The Council record is advice; Ferris owns acceptance and Jarvis owns host operations.

This design's investigation read source, allowlisted configuration bindings,
filesystem metadata and unit metadata. Apart from the explicitly requested Council
record, it did not query live store contents, open containers, inspect headers,
copy live stores, run benchmarks against them, or inject failures. Sizes below
are observations, not content identity or recovery evidence. No backup guarantee
exists merely because this spec is committed.

P0's eventual acceptance is a reproducible inventory plus a fixture-proven,
owner-side backup/restore path. Production recovery drills require a separately
approved window and private artifacts; this design assignment authorizes none.

## Approach

The Council's chosen contract rules out an uncoordinated live file copy and a
project manifest export. A copy may mix generations; `export_manifest` covers a
project definition, not the entire store and its recovery state.

Fiona proposes serializing an immutable published engine state captured by the
owner into a fresh standalone image, using existing snapshot/container primitives
where their invariants fit. A second option is a physical copy under a barrier
held for the entire copy. It can be correct but delays writers for storage I/O
and copies dead extents. The pinned-state approach limits the barrier to capture
and preserves sequence while allowing later commits. It costs retained memory;
bounds and measurements below are acceptance requirements. It must not call the
live compaction or checkpoint operation as a disguised backup.

This is a proposed implementation seam, not a new public CLI/API commitment.
No new persisted store schema is necessary merely to identify an artifact: use
a private recovery catalog identity plus the existing store lineage, where
available. Any future need for a new on-disk identity is a separate design decision.

## 1. Regenerable canonical store map

### Collection and resolution

Produce a private, versioned `store-map.json` from a deterministic metadata-only
collector. This is a proposed artifact, not a command implemented by this change.
Record collection UTC, host identifier, collector build/source revision, explicit
scan roots, configuration provenance, warnings and completeness. Sort records
and edges deterministically; distinguish volatile observations from stable keys.

Enumerate the union of:

1. Every `.fls` under configured storage roots, recursively, including archived,
   unbound, derived and temporary candidates; record unreadable directories.
2. Workspace and project bindings from the effective config path, CLI `--db` and
   `FL_DB` overrides where known, and each caller's backend/environment selection.
3. Unit store/socket paths, standalone deployments, and documented maintenance
   paths outside those roots. Add their parents to the explicit scope; do not
   silently crawl unrelated home directories or pretend unknown installations
   were inventoried.
4. Code sidecars derived from every primary path and other observed store aliases.

A complete map means every candidate in the declared deployment scope is accounted
for, including missing configured targets and unclassified files. It is not a
claim to have found every `.fls` on every machine. Missing permissions, unresolved
aliases, unknown overrides and omitted roots prevent a complete status. No open,
repair, migration, discovery query or auto-creating helper may be used to resolve
an inventory entry. Metadata collection must not contact a store socket.

The inventory implementation reports partial scans as `complete = false`. The
CLI can return that metadata map when no config or deployment artifact needs to
be read. Otherwise an incomplete candidate preflight refuses the artifact read:
a hidden reverse `.fls` symlink could name even an artifact outside the unreadable
directory. Library collection likewise defers all unit and executable reads when
its candidate scan is incomplete, leaving digests unknown with explicit warnings.

Config, deployment and unit artifacts retain the 1 MiB read limit. Installed
executables are never invoked; their SHA-256 is streamed through a fixed 64 KiB
buffer under a separate 512 MiB cap, including growth during the read. At most one
extra byte is read to detect overflow. Oversized or unsafe artifacts have no
digest. Both readers retain no-follow, regular-file, single-link, identity and
store-alias guards. These are metadata digests, not store-content reads.

For each file record:

| Field group | Required data |
| --- | --- |
| Identity | Catalog key, given paths, canonical path, existence/type, symlink chain, filesystem identity and hardlink count, identity confidence |
| Classification | Authoritative candidate, derived, historical candidate, or unclassified; evidence and owner decision; never infer disposal from size/name |
| Binding edges | Caller, scope/slug/root, config file/key or override source, logical path, selected backend, physical path, precedence, fallback target and mismatch |
| Ownership | Declared mode and its source, configured endpoint/unit, intended owner, observed unit state, observed access mode if separately measured, unresolved conflicts |
| Consumers | CLI, MCP, hub, governor, wire, maintenance tools and downstream library clients; read/write/index effects; handle lifetime and source symbol |
| Metadata | Size, modification time, permissions/ACL summary, filesystem, unit/config digests, installed executable path and build identity if available |
| Recovery | Backup policy reference, last verified artifact reference, index rebuild configuration reference; unknown is explicit |

Filesystem inode/device identity detects aliases in one observation; it is not a
persistent store UUID and changes on compaction or restore. Resolve symlinks for
identity but preserve each caller's acceptance rules: the hub refuses a symlinked
project file even where library paths follow it. Report hardlinks as an unresolved
ownership risk; no automatic unlink, merge or alias rewrite. Distinct files with
similar names remain distinct. Inventory identity metadata carries an explicit
`alias_status` (`unknown`, `no_alias_observed`, or `review_required`); symlink and
hardlink conflicts require ownership review. Older maps without the field decode
as unknown. This adds no persistent store schema or intrinsic identity field.

The collector must reuse or test parity with production path-resolution rules;
it must not independently invent a third resolver. A `.fls` suffix forces ferro;
otherwise `FL_STORE_BACKEND` selects the engine (default ferro), and a ferro
logical `x.redb` resolves to `x.fls`. Legacy `.redb` siblings are listed separately
as migration/history evidence, never treated as up-to-date backups. Invalid
`FL_STORE_BACKEND` values are refused by inventory engine selection (a `.fls`
suffix still forces ferro). This deliberately differs from the hub's
`physical_store_file_with` metadata helper, which falls back to the logical path
on an invalid value and leaves the later opener to report the error. Inventory
has no later store open, so it must not imply successful engine resolution.

`config.toml` maps `project:<slug>` to `[[project]].store`; an absent binding can
fall back to the workspace state store. The hub instead resolves a validated slug
under `~/.local/share/fl/{slug}.redb`, maps its backend, and can fall back to its
own store when the project file is absent. An unmigrated redb-only project is a
409, not that fallback. Preserve these as separate edges even when equal today.
Explicitly flag a future config/hub disagreement before any recovery or cutover.
The hub's own workspace edge is also distinct, even when it selects `fl.fls` at
the same path as config. It reuses `resolve_hub_store_path`: `--db` then `FL_DB`,
`[state].store`, the cwd-bound project, then the workspace default. Project hub
fallbacks use this selected store. These edges describe source-derived selection
for the supplied invocation/config/cwd, not an observed running daemon binding.

No current explicit managed/standalone policy is established by these bindings.
Record `declared_mode = unspecified_legacy_auto` for auto-open consumers, and
`direct_embedded` for bypass consumers. A running service is an observed service,
not proof of exclusive ownership. Desired managed/standalone policy is a separate
field awaiting P1, never retrospectively reported as enforced.

### Metadata seed, not a content census

Observed on ai-lab-0 on 2026-11-09. Paths below are relative to the deployment's
`~/.local/share/fl/`; roots in config are summarized by project slug. All 11 files
were regular, mode 0600, one hardlink at observation. Exact volatile values belong
in the regenerated private artifact, not a maintained public inventory table.

| Physical file | Config logical binding | Hub lookup | Service metadata | Class / consumers |
| --- | --- | --- | --- | --- |
| `fl.fls` | `[state].store = …/fl.redb` | Hub own configured store; `fl` reserved as project slug | `ferrostore.service`, active | Workspace; CLI/MCP, hub, governor, wire, direct store tools |
| `ferroloop.fls` | `ferroloop → …/ferroloop.redb` | `ferroloop → same physical file` | `ferrostore@ferroloop`, active | Project; project CLI/MCP, hub, IRI scans, direct tools |
| `kaggle-gemma4.fls` | `kaggle-gemma4 → …/kaggle-gemma4.redb` | Same physical file | `ferrostore@kaggle-gemma4`, active | Project consumers |
| `lucida.fls` | `lucida → …/lucida.redb` | Same physical file | `ferrostore@lucida`, active | Project consumers |
| `prosodia.fls` | `prosodia → …/prosodia.redb` | Same physical file | `ferrostore@prosodia`, active | Project consumers |
| `sonora.fls` | `sonora → …/sonora.redb` | Same physical file | `ferrostore@sonora`, active | Project consumers |
| `website.fls` | `website → …/website.redb` | Same physical file | `ferrostore@website`, active | Project consumers |
| `ferroloop-website.fls` | `ferroloop-website → …/ferroloop-website.redb` | Same physical file | Template available; instance inactive | Separate project candidate; project consumers; no retirement inference |
| `the-council.fls` | `the-council → …/the-council.fls` | Logical `.redb` resolves to same `.fls` | Template available; instance inactive | Council project; project consumers |
| `fl.code.fls` | Derived from workspace primary | Code search sidecar, not a project slug | No owner unit identified | Derived; CLI/MCP sync writes, hub cached read-only handles |
| `pre-v2-backup-20261101/store.fls` | No current binding found | Outside hub project lookup | No unit identified | Historical candidate from path only; contents/validity/consumers unverified; retain in map |

All configured project paths were within that storage root. No store/backend
override was found in the inspected workspace service, FerroLoop service, hub,
wire or host unit environment; the inspection shell's `FL_STORE_BACKEND` was unset.
This does not establish other caller environments or the access modes of live
handles. Template availability for inactive instances does not mean they are served.

Consumer edges are source-derived possibilities, not claims of observed traffic:
wire holds its initial workspace handle; hub opens per request with workspace or
per-path serialization; governor holds a handle after first use; CLI/MCP may scan
other configured stores for an IRI. Direct store data commands bypass auto-routing.
The host daemon opens no store; host client CLI commands do. Code-index search can
write the sidecar. Completeness also requires a deployment-specific scripts and
external-library census before operational acceptance.

### Transaction and performance baseline

Record build identities of installed and running binaries separately; unresolved
running identity remains unknown. Hash binaries/configuration artifacts, not live
store contents, during the metadata phase. Record unit relationships and cached
handle lifetimes; do not restart services to identify them.

Before P1/P4, use isolated representative fixtures to measure p50/p95/p99 latency,
throughput, physical opens, socket connects/handshakes, retries, RSS, pinned-state
bytes, backup/verify/restore duration, writer delay and queue peaks. Separate cold
and warm runs, store sizes and concurrency; pin toolchain/build and hardware in
results. No live query/open benchmark is authorized here. Actual deployment
traffic measurements require a later scope decision and read-only instrumentation.

Inventory domain transaction boundaries, not just primitive names: state head
plus revision updates, registry/wire receipts, gate/attempt/ledger changes and
replication/outbox records. Preserve multi-record atomicity, point/absent reads
and full Scan/Query read-set validation. Search-derived decisions need their own
validated predicate/version; a search outside the transaction is not a validated
read. Measure bounded retry behavior without reissuing external effects.

## 2. Backup contract and state machine

A successful artifact is one self-consistent recoverable store at published
sequence **S**, observed under the existing commit barrier and current-file lock.
S is the store commit sequence, not a WAL byte offset, superblock epoch, MQ topic
sequence or wall-clock time. S includes all commits acknowledged before the capture
cut. Before P1 single ownership, it may also include an external publication whose
writer reported failed sync: its outcome is unknown even though live openers can
already see it. Capture does not claim to exclude such writes. Recovery point is
the last completed verified backup.
No point-in-time recovery, zero media-failure loss, or multi-store atomic cut is
promised. A backup set carries a vector of per-store identities/sequences and
cross-store dependencies; global consistency needs coordinated quiescence or an
explicit reconciliation plan.

1. **Admit.** Resolve a single store identity and intended owner from the map.
   Authenticate the administrative caller; reject ambiguous destinations and
   source/destination aliases. Allocate a bounded operation with cancellation,
   deadline, byte/memory/disk quotas and one concurrent backup per physical store.
   Destination space must cover the image, verification copy, metadata and margin.
2. **Capture.** On the physical owner, reject poison or outcome-unknown state
   on the capturing handle. Refresh and pin the immutable published state plus S
   under the existing commit barrier and current-file lock, including all commits
   acknowledged before the capture cut. A healthy handle cannot detect another
   opener's failed-sync outcome from its own poison status (engine hazard #456);
   record that exclusion proof is unavailable before single ownership. Do not
   add a source fsync or engine/schema/persistent-poison change. Bind all logical
   hashes and expected query results to this same captured state. Later writes may
   proceed only after the pin is independent of mutable live structures. If that
   property cannot be proved, hold a bounded copy barrier or refuse; never mix separately
   sampled “current” collections. Preserve documents, tables, vector data/config,
   metadata and committed receipts. Do not force memory-only MQ data into the
   source as a side effect: report non-durable/ephemeral exclusions explicitly.
3. **Write privately.** Create a unique staging directory on the publication
   filesystem, mode 0700 (equivalent restricted ACL on Windows), with exclusive,
   no-follow file creation at 0600. Serialize a valid self-contained image at S,
   including existing store lineage and schema; do not synthesize empty defaults
   when something is missing. Write a private manifest and validation expectations.
   Flush image and metadata and sync staging directory before verification.
4. **Verify independently.** Make a separate, non-hardlinked restore copy in an
   isolated directory. Reopen it using the intended compatible reader, with no
   migration, initialization, remote socket fallback or outbound adapters. Validate
   checksums, format/schema, identity, S and the logical checks below. The operation
   must fail if it silently creates an empty database. Close/reopen after validation;
   the candidate image must remain unchanged. Count equality alone never passes.
5. **Publish.** Write the verification report and checksums, flush all final members
   and sync staging directory. Atomically rename the complete bundle to a unique,
   immutable final name on the same filesystem; do not overwrite a previous backup.
   Sync its parent directory. Only then acknowledge success and update the catalog.
   Failure after rename but before directory sync is outcome-unknown, not success:
   reconcile the operation ID and verify the final bundle on restart. An incomplete
   bundle never occupies a final name; a complete but unacknowledged bundle is
   quarantined from automated restore selection until durability is established.
6. **Replicate/retain later.** Copy only completed bundles to the approved independent
   failure domain, verify their checksums there, and record completion separately.
   Retention cannot delete the last verified recoverable generation or its required
   configuration. It starts only after interval/retention/copy policy is approved.

Current multiple-opener behavior complicates “owner-side.” A service-local mutex
alone is insufficient. The capture seam must refresh under the engine's existing
cross-process current-file lock and pin one immutable published state across all
writers, including group commit and sync publication; fixture tests must prove it.
If pin independence cannot be established before P1, P0's recovery operation is
available only under explicit administrative quiescence of every physical opener.
Quiescence alone cannot prove exclusion of historical external outcome-unknown
writes; this amendment adds no quiescence requirement. No claim of enforced single
ownership is made. Offline restore always requires quiescence.

Existing serialization, private-file and durability primitives are candidates for
reuse; `compact_disk`'s live rename, epoch changes and cleanup policy are not the
backup API. Destination failure must not poison or mutate a healthy source handle.
Report an already unhealthy source separately. A failed backup leaves previous
backups intact and the source usable; incomplete private artifacts are removed or
quarantined by operation ID, never swept using a broad pattern over live paths.

Task 4 publication clarification (Ferris approval, 2026-11-10,
FerroWire `01a1c381-a9d7-74ae-a9df-3000dff52273`): use one configured approved
backup root and a destination-side `flock` keyed by physical inventory identity
under `.locks/`. Unknown/review-required aliases refuse. This does not lock the
source or establish P1 ownership; multiple roots/hosts are outside P0. Before
capture, admit an enforced per-image ceiling, defaulting to source file length L
and only lowerable by the caller. Twice that ceiling plus bounded metadata,
configuration and catalog reserve R must fit min(2L + R, operation quota, free
space minus a stated margin). R is derived from observed allocation geometry plus
bounded metadata/config/catalog payloads and is capped at512MiB. This corrects
Ferris’s earlier 2L admission ruling; a default unchanged-store backup is admitted
when quota and space cover R. A ceiling overrun is `ImageExceedsCeiling`.
Record fixture image/L ratios for acceptance.

Publication does not rewrite the immutable manifest: it remains NotPublished
with independently Verified copy evidence. A separate catalog receipt, created
only after final rename and parent sync, binds the manifest digest, image digest
and member checksums and attests Durable. Its own temporary-file flush, exclusive
rename and catalog-directory sync are required. A complete bundle without a
receipt is unavailable to restore selection until operation-specific checksum
reverification and durable receipt repair. Mismatched bundles are quarantined by
operation ID, never deleted. Incomplete staging cleanup is similarly scoped to
that exact operation, never a directory-wide sweep.

### Bundle metadata and logical verification

The versioned manifest records: backup operation ID; catalog store identity and
available intrinsic lineage; source alias map digest; format/container, snapshot
and application schema versions (absence is explicit); engine/feature bits;
source published sequence; typed `external_publish_status`; capture/publication
times; creator and verifier build identities; image length and SHA-256; checksums
of each retained configuration and validation member; logical content digest and validation profile version; per-store
reference dependencies; exclusions; and separate local/independent-copy status.
Every pre-P1 capture records
`ExternalPublishStatus::ExclusionUnprovenBeforeSingleOwnership` in both the image
descriptor and manifest. This typed enum reports unavailable exclusion proof, not
a detected failed write. The report must bind to this exact image digest. Checksums
detect corruption, not malicious substitution: protect the catalog and storage access; any signed manifest
or encrypted off-host copy requires an explicit key/recovery policy.

Verification compares the restored copy with expectations computed from the pin:

- Canonical logical hashes over **all** persisted collections and identifiers,
  typed values, column definitions/rows, vector values/configuration and metadata.
  Use a versioned unambiguous encoding; preserve types, null/absent distinctions
  and array order. Also compare domain values, not just one aggregate digest.
- FerroState entry IDs, scope, payload, lifecycle/privacy flags, head revision,
  history and references; revision heads resolve and sensitive/ever-sensitive
  exclusion remains intact. Verification output must not print sensitive payloads.
- Wire mail/receipts and any persisted operation outcomes/outbox state; compare
  identities, payload digests, statuses and references. Current RPC dedupe caches
  are transient, not durable receipts: do not claim P2 guarantees or fabricate
  outcomes. Unsupported/absent facilities remain explicitly absent.
- Gates/transitions, records/findings, attempts/ledger, registry/runtime references
  and collection-specific invariants. Unknown collections are preserved and hashed;
  absence of a domain verifier is reported, not silently called domain-verified.
- Fixed representative point, absent-key, Scan/Query, state revision/handoff,
  lexical ranking, privacy and vector queries applicable to that store class.
  Use deterministic fixtures and declared vector tolerances where necessary;
  do not substitute approximate ranking for exact lexical inclusion.
- Internal references must resolve or match documented pre-existing exceptions.
  External references are checked against a declared dependency manifest and
  recovery-set policy; a separate store's later sequence is not proof of an atomic
  cross-store restore. Missing dependencies block readiness pending reconciliation.

Exclude reproducible `<primary>.code.fls` contents only. Keep primary state and
its authoritative data; do not drop all vectors simply because code vectors are
derived. Retain code-index source-root identity, repository/revision availability,
scan/ignore rules, language/parser versions, embedding space/dimensions/config,
ranking version and rebuild budgets. Capture effective defaults as versioned
configuration, not merely absent config keys. Include required source/config
recovery dependencies and secure secret references; do not dump all `config.toml`
credentials into the bundle. Secrets needed to recover must have a separately
verified protected recovery path. Rebuild the sidecar after draining its users;
report lexical-only degraded readiness separately from full index readiness.

## 3. Offline restore

The following details incorporate Ferris's 2026-11-10 Task 6 ruling; implementation
remains fixture-only until the operational gates are separately approved.

1. Select a completed verified artifact, expected store identity, compatible build
   and explicit destination. Check local/independent-copy status, manifest/image
   checksums, dependency set, versions, space and permissions. Fail closed on an
   unsupported schema or changed binding; restore is not implicit migration.
2. Stop admission and drain store users under an announced bounded window,
   preserving resident vendor sessions/context. Require an administrative lease
   created mode 0600 under the approved root, bound to operation/action, catalog
   identity, canonical destination parent identity/basename, current file
   identity/digest or explicit absence, selected bundle digest and build/profile,
   with issuance/expiry no more than one hour apart and explicit operator
   attestation that legacy/cached store handles and external dispatch are excluded.
   If original bytes are unreadable, bind held filesystem identity and recorded
   read-error/forensic classification without inventing a digest. This is not a
   cryptographic token or P1 ownership: lease, flock and PID/socket probes prove
   nothing about exclusion. Restart needs renewed exclusion and a renewed lease.
   Task 7 settles who may issue leases, together with same-UID agent authority.
3. Durably create and parent-sync a private operation-bound restore guard before
   destination/rollback mutation. All normal create, migration, direct/auto open,
   socket contact and serve paths check it, with an engine-open recheck. Existing,
   unreadable or malformed guards fail closed. Lease expiry never clears it.
   Only private operation-bound `RestoreAccess` permits existing-only, read-only
   inspection while fenced. Before live restore, every opener must run a
   guard-aware binary; legacy binaries bypass this sidecar, and already-open
   handles still require administrative exclusion.
4. Restore into a unique private sibling directory on the target filesystem,
   keeping the backup immutable. Allocate exactly one independent candidate and
   use `verify_existing_copy` for close/reopen identity, S, logical, domain and
   query checks with external effects disabled. Existing target/backup bytes stay
   capacity-accounted; new candidate, verifier scratch and all metadata must fit
   the strict `C + R + V + M <= 2 * L` bound. Preserve the original inode itself
   (`R = 0`), rather than allocate a rollback copy; no metadata or reflink discount
   widens the budget. Refuse before mutation when it cannot fit.
5. P0 installation is Linux-only. Existing targets require held `nlink == 1` and
   inventory alias status `NoAliasObserved`; refuse non-Linux, unsupported
   `RENAME_EXCHANGE`, ambiguous aliases or cross-filesystem operations. Verify
   readable original data as rollback; retain an unreadable original as forensic
   material, never a verified rollback claim. Flush the original/candidate as
   required, then durably write and directory-sync intent before **each** step:
   exchange candidate and destination with `RENAME_EXCHANGE`, sync both
   directories, then `NOREPLACE`-rename the old inode from the candidate name to
   its unique rollback/forensic name and sync both directories again. An absent
   destination uses journaled `NOREPLACE`. Record durable installation completion.
   Crash recovery selects only journal-bound identities and available digests,
   verifies the selected complete image and completes directory durability; missing
   or corrupt metadata and ambiguous names keep admission fenced. Explicit
   rollback uses the reverse exchange, also journaled and directory-synced, while
   preserving both generations. Never delete unreadable original material or use
   stale redb files as current rollback.
6. Completed installation is not readiness: keep **all ordinary opens refused**
   until explicit operator finalize records typed evidence that stale clients,
   RPC sessions, transactions, caches and retry assumptions were drained/restarted
   and dependency/effect reconciliation completed. Receipts newer than S may be
   lost despite real effects: preserve explicit outcome-unknown and reconcile;
   never blindly resend or promise exactly-once. Unresolved identity, reference
   or external-effect ambiguity blocks finalize. Library/CLI never contact
   vendors or enable dispatch. Lexical/degraded/full index readiness is reported
   separately and does not by itself block finalize. Only after durable completion
   and recorded readiness may finalize remove the guard and sync its parent;
   failed removal sync leaves outcome-unknown with administrative exclusion in
   force. Keep an audit of backup, horizon, reconciliation and rollback point.

Quarantine purge is a separate explicit operation-ID action under the approved
root and physical-store administrative exclusion. Confirmed execution completes
any half-withdrawn receipt first; refuse remaining live catalog receipts, active
operations, unproven
reference closure, symlinks/replacements and unreadable digest inputs. Default to
a dry run listing exact owned names, sizes and SHA-256 digests, with no audit
write or receipt withdrawal; report pending reconciliation instead. Require
matching `--confirm <op-id>` to act and revalidate after any receipt withdrawal. Before unlinking, durably write and directory-sync a
catalog audit with operation ID, exact names/sizes/digests, time and actor; keep
intent and completed removals distinct for crash recovery. Revalidate identity
before each unlink and sync affected parents. Remove only that quarantined
operation's verified-owned quarantine/staging/final names, leaving shared configs
and other operations untouched. Never general retention, glob sweeps, legacy
`.redb` or historic-file deletion.

Native Linux filesystem fixtures must prove every claimed exchange, rollback,
purge and directory-durability cut. Runtime transaction IDs and restored
persistent store lineage are distinct: no generation accepts writes before its
name is durable, and administrative exclusion is still required for legacy or
cached users. No engine-format or exclusive-ownership claim is added.

## 4. Fixture-only proof and failure injection

Use `ContainerFs`/simulated disk seams for unit-level faults in store modules and
isolated subprocess scenarios in `tests/`. Fixtures must cover workspace state,
project records/ledger, Council revisions/privacy, small independent projects,
empty stores, unknown collections, durable receipts, vectors/tables and derived
sidecar exclusion. Cover every inventoried authoritative **class**, then map each
retained store to a passing profile; no tiny-store exemption. These are planned
tests, not results already obtained.

| Injection / adversary | Required evidence |
| --- | --- |
| Concurrent commits, group commit, sync publish, checkpoint and compaction around capture | Exactly S; no torn multi-record transaction; source can advance and remains usable |
| Short write, full disk, permission denial, cancellation or kill during image/metadata write | No final partial artifact; old backups intact; bounded cleanup and source progress |
| File flush or directory sync fails before/after rename | No success on uncertain durability; restart reconciles complete/unpublished artifact by ID |
| Truncate/corrupt image, metadata, receipt, revision or dependency; valid count but wrong payload | Reopen/hash/domain/query verification refuses publication or replacement |
| Wrong store, incompatible schema/feature, alias/hardlink/symlink escape, malicious destination | Fail before source or active destination mutation; no secret-bearing diagnostic |
| Verifier accidentally sees live socket or external adapters | Test fails; verifier must use isolated copy and cannot produce external effects |
| Restore crash before replacement, immediately after rename but before directory sync, or during catalog update | Staged image, rollback generation and journal intent were flushed before rename; recover deterministically from journal digests and surviving images, finish required directory syncs, and keep admission closed until the complete selected image is durable |
| Stale client, cached transaction, repeated request or pending external effect after restore | Old session rejected; no automatic resend; explicit unknown/reconciliation outcome |
| Two-store reference mismatch and restored older receipt horizon | Block unsafe readiness; document reconciliation, no invented global consistent cut |
| Oversized source/pin, slow disk/verifier, timeout and repeated cancellation | Enforced memory/disk/deadline/concurrency bounds; no runaway retries |
| Index omitted and rebuild interrupted | Authoritative data intact, configuration sufficient, bounded restartable rebuild |

Fault runners must use fresh sandbox roots, fixture identities and inert transport
stubs. They must reject production paths, inherited `FL_DB`, real config/home and
real service endpoints. Never run failure injection, forced termination, corruption,
replacement or deletion against live stores. Native Linux, macOS and Windows
filesystem behavior must be tested before claiming each platform's support.

P0 passes only when the map regenerates, all retained classes restore in isolation,
logical hashes/domain checks match, failures never publish a partial backup or
leave an unusable source, and restore failure states have tested recovery. Publish
build/fixture identities and measured resource results with the evidence. Design
approval alone does not meet this gate or authorize P3 cutovers.

## 5. Decisions for Ferris and Jarvis

| Decision | Required answer before operational protection is claimed |
| --- | --- |
| Backup interval / recovery point | Per-store schedule and maximum acceptable age of the last completed verified backup; handling of missed/failed runs |
| Retention | Generation/time policy, capacity reserve, minimum verified rollback generations, deletion authority and receipt/dependency preservation |
| Independent failure domain | Destination surviving source disk/host loss, ownership/access and encryption/key recovery, transfer/verification policy; a second directory on the same disk is insufficient |
| Recovery-time target | Measured budget from incident decision through verified restore, reconciliation and usable service; separate full code-index readiness target |
| Ownership prerequisite | Whether pinned capture can be proved under existing locks or P0 initially requires administrative quiescence; who inventories/excludes legacy writers |
| Acceptance budgets | Backup memory/disk/concurrency/time bounds and permitted writer delay; fixture sizes/load and later live observation window |
| Cross-store recovery | Which stores require coordinated recovery and how references/external outcomes are reconciled; no implicit global cut |
| Unclassified/history | Identity and purpose of unbound historical files and external consumers; preserve until evidence supports a separate retirement decision |

## Source anchors for implementation planning

Paths below are relative to repository root; symbols are preferable to drifting
line numbers. These are evidence for existing mechanisms, not a claim that backup
or restore APIs already exist.

- `crates/cli/src/config.rs`: `Config::state_store_for_scope`, `workspace_state_store`,
  `Entry::matches_slug`; `crates/cli/src/cmd/state_mcp.rs`: `open_for_id_or_key` and
  scope/IRI scanning paths.
- `crates/hub/src/records.rs`: `project_redb_with`, `physical_store_file_with`,
  `open_project_store`; `crates/hub/src/lib.rs`: store guards/governor cache.
- `crates/store/src/any.rs`: `choose_backend`, `physical_path`, `export_manifest`.
- `crates/store/src/ferrostore/mod.rs`: `FerroStore::auto`, `open_with`,
  `Disk::lock_current`, immutable `read_state`, `encode_state_snapshot`, poisoning
  and sequence handling; `layout.rs`: format versions and snapshot codecs.
- `crates/store/src/ferrostore/compact.rs`: private sibling write/sync/publication
  mechanics, with live replacement deliberately excluded from backup.
- `crates/store/src/ferrostore/container_io.rs` and `tests_compact.rs`: filesystem
  seams and crash-proof patterns; `session.rs`: transactional read-set validation.
- `crates/store/src/code_knowledge.rs`: sidecar path, root identity, `IndexPolicy`,
  sync budgets and exact fallback; `crates/store/src/health.rs`: metadata-only probes.
