# FerroStore P0 Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use the repository's `subagent-dev` skill to implement this plan task by task, with fresh implementers and the `code-review` loop required by WORKFLOW.md. Steps use checkbox (`- [ ]`) syntax. **This document is for Ferris review; it authorizes no implementation or operational change.**

**Goal:** Produce a regenerable store inventory and fixture-proven, owner-side backup and offline restore, with recurring operation disabled until its capture proof passes.

**Architecture:** Separate metadata collection, immutable engine capture, recovery bundle verification/publication, and offline installation. Run recurring capture inside each serving daemon while its listener stays available; a batch coordinator supplies a shared batch ID without promising a global atomic cut. Keep recovery control separate from ordinary data access and fail closed when ownership or durability is uncertain.

**Tech Stack:** Existing Rust workspace, FerroStore snapshot/container codecs, `ContainerFs` crash simulation, serde versioned manifests, existing SHA-256 support, isolated subprocess tests. Reuse existing dependencies; no database, cloud account, or new external service.

**Spec:** [P0 design of record](../specs/2026-11-09-ferrostore-p0-recovery-design.md), merged in `7cf84ded` (#449). Read it with the [runtime inventory](../../architecture/ferrostore/README.md). The implementation source baseline for this plan is `921a5b0551f4937c873ea09612c411ff329c4c18`; `7cf84ded` identifies the spec merge, not the current checkout.

**Decision source:** FerroState `project:ferroloop/ferrostore-p0-backup-decisions`, revision `urn:uuid:01a1c238-506d-7338-a5be-1bd0e74bad4d`, Ferris + Jarvis, 2026-11-09. Its protected batch-config decision below supplements the earlier spec's minimal-config proposal. Council synthesis remains the rationale, not operational authorization.

## Global constraints

- P0 only. No P1 ownership cutover, P2 durable RPC receipts, engine replacement, automatic migration, compaction-as-backup, deployment, or live recovery drill.
- Every successful image is self-consistent and recoverable at one published sequence **S**, observed under the existing commit barrier and current-file lock. S is the store commit sequence, not a WAL offset or superblock epoch, and includes all commits acknowledged before the capture cut. Before P1 single ownership, external failed-sync publications may be included with unknown outcomes; capture does not claim to exclude them. Capture, logical expectations and query expectations use the same immutable pin; never sample live collections separately.
- One concurrent backup per physical store. Enforce operation deadlines, cancellation and memory/disk quotas; destination failures must not poison a healthy source.
- Recurring capture runs in-process in the serving daemon, with the socket listener kept up. Hourly/6-hour schedules remain **off until pinned capture is proven on fixtures**. A service-local mutex alone does not exclude legacy embedded writers.
- At most **1 s writer delay per store**; backup temporary disk at most **2L + R**, with two images bounded by source size L and a separately capped metadata/configuration/directory reserve R. Restore retains its Task 6 bound. **Backup staging** is `/data/fl-backups/.staging`. **Restore staging** is a private sibling directory beside the destination on its filesystem, because publication from the backup NVMe to the live-store NVMe cannot use atomic rename. Neither uses `/tmp`. Task 6 fixes restore admission/accounting explicitly; insufficient budget means refusal, not a relaxed quota or omitted independent copy.
- Destination `/data/fl-backups` is the second physical NVMe (`nvme0n1`, xfs), separate from source `nvme1n1`. Jarvis provisions mode **0700**, owner `lmcfarlin`, datashare setgid cleared. It covers disk loss, **not host loss**. Off-host/cloud copy requires an owner decision.
- `fl.fls` and `ferroloop.fls`: hourly, one batch ID. Other served projects and Council: every **6 h**. An unserved store cannot participate in recurring daemon capture: report its unmet schedule explicitly; only an announced, separately authorized one-off quiescence is available until an owner serves it.
- Retain **24 hourly + 7 daily + 4 weekly** generations; never delete the last verified generation. Maintain capacity reserve **2x the bundle set**. One failed run alerts Jarvis/Ferris; two consecutive failures page the owner.
- Recovery-time target **30 min per store**, incident decision through verified usable service. Code-index rebuild is separate, best effort; report degraded readiness explicitly.
- Each batch retains `config.toml` and `warrants.json` at **0600**. Optional `vault.enc` is ciphertext only. `~/.config/fl/vault.key` is **NEVER** in a bundle or anywhere on the backup destination; key escrow is pending owner decision. Private reports must not print credentials or payloads.
- Exclude only reproducible code-sidecar contents, preserving effective index configuration and source-root/revision dependencies. Keep authoritative vectors. Preserve old `.redb`, `pre-v2-backup-20261101/`, and `ferroloop-website.fls`; retirement needs recovery proof and owner approval.
- Backup publication and restore replacement require flushed files, directory sync and durable journal reconciliation. Rename-before-directory-sync is **outcome-unknown**, never success. A durable restore admission guard blocks ordinary updated openers until installation and effect reconciliation finish; administrative exclusion still covers old binaries and existing handles. This is not P1 lifetime ownership.
- Fault injection, forced termination and restoration use fixtures only. Reject real home/config/store/socket paths and inherited `FL_DB`; no live content access is authorized by this plan.
- Native Linux, macOS and Windows evidence is required before claiming support on each platform. Unsupported filesystem/ACL/directory-durability operations must refuse, not silently downgrade.

## Review focus

1. A binding resolves differently in config and hub, or is a hardlink/symlink alias: inventory exposes both edges; backup/restore refuses ambiguous identity (Task 1).
2. A concurrent external opener commits or compacts between preparation and capture: image and expectations remain at exactly published S, including all commits acknowledged before the capture cut, with bounded capture retry and no long writer barrier. An external failed-sync publication may be included; typed metadata says exclusion proof is unavailable before single ownership (Task 2; engine hazard #456).
3. Small or incompressible sources exceed the operation quota or available space after adding the bounded reserve: admission refuses before capture; source and prior backups remain usable (Task 4).
4. A crash loses the rename's directory entry after apparent publication/replacement: reconciliation proves the surviving image and keeps admission closed until durability is established (Tasks 4 and 6).
5. Restored receipts lag real vendor effects, or references require another store's sequence: readiness remains blocked for affected operations and nothing blindly replays (Task 6).

## Scope, sequencing and interfaces

Tasks 1–4 produce manual fixture recovery. Task 5 adds dormant recurring coordination. Task 6 proves offline replacement, and Task 7 assembles acceptance evidence. These form one recovery contract; do not enable scheduling after an individual task. Each task has its own review gate and commit.

All signatures below are **proposed**, not existing APIs. Public recovery types live in new `crates/store/src/recovery/mod.rs`; engine internals stay in `ferrostore/recovery.rs`. Use `Result<T, RecoveryError>` for recovery operations. `RecoveryError` distinguishes refused input, quota/deadline, unsupported, verification failure and outcome-unknown, with operation IDs and redacted reasons. Convert existing `StoreError` without mislabeling source health.

Shared types defined with their first producer:

- Task 1: `StoreMap`, `InventoryInputs`, `StoreIdentity`, `BindingEdge`, `ConsumerEdge`. Identity is a private catalog ID plus explicit lineage availability, never an inode masquerading as a persistent UUID.
- Task 2: opaque `CapturedStore` owns one immutable state/sequence/format pin; `CaptureBudget` holds absolute deadline, pin-byte limit and writer-delay limit. `CancelToken` is a cloneable atomic cancellation flag; `OperationLimits` holds absolute deadline, byte quotas and checked accounting. `ImageDescriptor` and `RecoveryImageWriter` are defined in the recovery module with Task 2; `RetentionDecision` in policy.rs with Task 5. No unbounded default budgets.
- Task 3: `ValidationProfile`, `ValidationExpectations`, `VerificationReport`, `BundleManifestV1`; string operation/batch IDs use existing UUID generation. A validation report binds image SHA-256 and profile version.
- Task 4: `BackupRequest`, `VerifiedBundle`, `PublicationOutcome` (`Durable` or `OutcomeUnknown`), and `RecoveryFs` (restricted directory/file operations with a deterministic fault seam).
- Task 5: `RecoveryPolicy`, `BatchRequest`, `BatchReport`, `RecoveryEvent`, `CaptureProof`. Policy schema is versioned, distinct from on-disk store format.
- Task 6: `RestoreRequest`, `OfflineLease`, `RestoreJournal`, `RestoreOutcome`, `ReadinessReport`, `RestoreGuardV1` and private `RestoreAccess`. `OfflineLease` represents an administrator's established exclusion plus process-local operation ownership; it must never be described as enforced P1 lifetime ownership. It binds the physical store identity/path, operation, quiescence evidence and expiry for either explicit offline backup or restore; an absent daemon is not a lease.

### Task 1: Regenerate the canonical metadata map

**Files:** Create `crates/store/src/recovery/{mod.rs,inventory.rs}` and `crates/cli/src/cmd/recovery.rs`. Modify `crates/store/src/lib.rs`, `crates/cli/src/{main.rs,config.rs}`, `crates/cli/src/cmd/mod.rs`; add parity tests in `crates/hub/src/records.rs`. Black-box tests: new `crates/cli/tests/recovery_inventory.rs`.

**Interfaces:** `collect_inventory(inputs: &InventoryInputs) -> Result<StoreMap, RecoveryError>`; CLI adapter `inventory_inputs(config: &Config, config_path: &Path, roots: &[PathBuf]) -> anyhow::Result<InventoryInputs>`. `fl recovery inventory --root PATH --json` uses the effective config path and explicit overrides. The collector never opens a store or contacts a socket.

- [ ] Add unit fixtures for `.fls` forced backend, `.redb` mapping, missing project/workspace fallback, hub reserved/invalid slug and redb-only refusal. Assert config/hub edges remain separate, even when equal. Add alias/hardlink, symlink chain, unreadable directory, archived/unclassified and omitted-root cases; `complete == false` for unresolved scope.
- [ ] Add the black-box fixture: refuse inherited production paths, install a socket stub that fails if contacted, and instrument opens to reject every `.fls` content read. Assert only metadata/config/unit inputs are read, no files created under source roots, and normalized output is deterministic across enumeration order.
- [ ] Run `cargo test -p fl-store recovery::inventory` and `cargo test -p fl-cli --test recovery_inventory`; expect missing-module/command failures before implementation. Keep RED evidence.
- [ ] Implement all field groups from spec §1: identity, classifications, binding precedence/fallback, configured versus observed mode, consumer edges with source symbols, filesystem/unit/build metadata, recovery and index-config references. Reuse `choose_backend`/`physical_path` and `Config` resolution; parity-test hub rules without importing hub into store. A service being active does not prove exclusive ownership.
- [ ] Derive known consumers from the inspected entrypoints and version their source revision; accept explicit deployment extension inputs for scripts/downstream libraries. Unknown consumer coverage is incomplete, never an invented complete census. Read unit metadata through a bounded adapter, not shell-interpolated commands; preserve unresolved running build identity.
- [ ] Run those tests plus `cargo test -p fl-hub records`; expect all passing. Commit `feat(recovery): add metadata-only canonical store map` with machine identity and Fiona trailer; fresh task review.

### Task 2: Prove immutable published-state capture with a short barrier

**Files:** Create `crates/store/src/ferrostore/recovery.rs`; modify `ferrostore/{mod.rs,container_io.rs,crash_sim.rs}` and recovery shared types. Black-box tests: new `crates/store/tests/recovery_capture.rs`. Unit tests remain `#[cfg(test)] mod tests` in the new module.

**Interfaces:** `FerroStore::capture_recovery(&self, budget: &CaptureBudget, cancel: &CancelToken) -> Result<CapturedStore, RecoveryError>`; `CapturedStore::sequence(&self) -> u64`; `CapturedStore::write_image(&self, target: &mut dyn RecoveryImageWriter, limits: &OperationLimits) -> Result<ImageDescriptor, RecoveryError>`. `RecoveryImageWriter` exposes checked `write_at`, `set_len`, `sync_all`; `ImageDescriptor` records bytes, sequence, format/schema/lineage presence and `external_publish_status: ExternalPublishStatus`. `CapturedStore::external_publish_status()` exposes the same typed serde enum, reused by the later manifest. Every pre-P1 capture carries `ExternalPublishStatus::ExclusionUnprovenBeforeSingleOwnership`: this reports unavailable exclusion proof, not a detected failed write. Remote or memory-only handles refuse physical backup; no auto-open fallback.

- [ ] Add RED tests `capture_keeps_one_committed_sequence`, `capture_survives_external_compaction`, `capture_labels_external_outcome_unknown`, `capture_rejects_poison`, and `capture_budget_refuses_before_long_barrier`. Cover group commit, sync publish, checkpoint, external process writes, pinned unknown collections/tables/vectors, cancellation and exceeded memory accounting.
- [ ] In `capture_labels_external_outcome_unknown`, use two independent openers in the exact `SimFs` hazard #456 scenario: the external writer acknowledges S1, then publishes S2 whose slot fsync fails, poisoning that writer while its cache remains at S1. The healthy capturing opener adopts published S2 under the existing current-file lock. Capture S2 and prove independent image reopen is self-consistent at S2, with descriptor/manifest `external_publish_status` set to `ExternalPublishStatus::ExclusionUnprovenBeforeSingleOwnership`; simulated power-loss reopening of the source returns S1. Keep capturing-handle poison/outcome-unknown refusal. Comment that P1 changes this test to reject the second opener; neither per-handle poison nor quiescence alone proves historical exclusion.
- [ ] Run `cargo test -p fl-store ferrostore::recovery` and `cargo test -p fl-store --test recovery_capture`; expect missing capture API and retain RED evidence.
- [ ] Prepare state outside the barrier, then acquire existing commit ordering and the cross-process current-file lock, refresh/validate against the current published superblock, and pin the immutable `Arc<DocState>` with published S, including all commits acknowledged before the capture cut. Preserve lock ordering of `Disk::lock_current` and commit paths; include external openers, not just daemon mutexes. Verify no shared mutable indexes can mutate the pin.
- [ ] Add deadline-aware lock admission rather than calling an indefinitely blocking flock. Bound stale-preparation retries to three after the initial attempt (four total attempts) within the absolute budget; exhaustion returns a distinct stale error with the operation ID. Release the barrier before encoding, hashing, domain queries or I/O to the destination. If refresh under the barrier cannot meet 1 s or pin independence is unproven, refuse recurring capability and stop that rollout; a long full-copy barrier is not a recurring fallback.
- [ ] Write a standalone snapshot/container at S with existing codecs, preserving format/features and explicit lineage absence. No live checkpoint/compaction, source rewrite/fsync, engine/schema/persistent-poison change or forced flush of ephemeral MQ. Record exclusions and unhealthy-source status separately from destination errors.
- [ ] Use simulated elapsed time for deadline behavior and real fixture measurement for p50/p95/p99/max writer delay and retained bytes. Exceeding 1 s prevents a passing capture proof. Measure cold refresh and external-writer contention, not only a warm daemon cache.
- [ ] Run both test commands GREEN. Commit `feat(recovery): pin committed store state for backup`; fresh task review must inspect every source mutation path that could invalidate the pin.

### Task 3: Verify independent restored images and domain contents

**Files:** Create `crates/store/src/recovery/{manifest.rs,verify.rs,profiles.rs}`; extend `ferrostore/recovery.rs` and module exports. Black-box tests: new `crates/store/tests/recovery_verify.rs`.

**Interfaces:** `CapturedStore::expectations(&self, profile: &ValidationProfile, limits: &OperationLimits) -> Result<ValidationExpectations, RecoveryError>`; `verify_existing_copy(copy_path: &Path, expected: &ValidationExpectations, profile: &ValidationProfile, limits: &OperationLimits) -> Result<VerificationReport, RecoveryError>` verifies an already-created independent image without allocating another image file; bounded working memory remains charged to `limits`. `copy_and_verify(image: &Path, copy_path: &Path, expected: &ValidationExpectations, profile: &ValidationProfile, limits: &OperationLimits) -> Result<VerificationReport, RecoveryError>` creates one independent private copy at the explicit destination and delegates to `verify_existing_copy`. `open_recovery_image_existing(path: &Path) -> Result<FerroStore, RecoveryError>` is a direct existing-only, no-migration verifier open, never `auto()` or a create-capable helper.

- [ ] Add RED tests for equal counts/wrong payload, missing/changed revision head, wrong receipt status, unresolved references, dropped unknown collections, vector/column corruption, absent versus null, incompatible schema/features, wrong S/identity, and missing image accidentally initialized empty.
- [ ] Add fixture profiles for workspace, project ledger/records, Council privacy/revisions, small/empty stores and mixed tables/vectors. Assert all persisted values are hashed with versioned length-framed typed encoding, array order preserved, and unknown collections preserved but domain coverage explicitly unknown. Privacy fixtures must prove verification reports contain no sensitive text.
- [ ] Run `cargo test -p fl-store recovery::` and `cargo test -p fl-store --test recovery_verify`; retain the failures before implementing.
- [ ] Compute expectations only from `CapturedStore`. Compare canonical logical digests plus domain values, revisions, persisted mail/receipts, gates/attempts/registry references and declared dependencies. Mark transient dedupe facilities absent; do not invent durable receipts. Capture expected point/absent, Scan/Query, lexical inclusion/ranking, revision/handoff, privacy and applicable vector results from the same pin; profile-version explicit vector tolerance.
- [ ] Implement `copy_and_verify` with exclusive, no-follow creation of one separate non-hardlinked private file at `copy_path`; reject source/destination aliases and existing destinations. Hash the source before/after to prove it stayed immutable. Implement `verify_existing_copy` as existing-only verification that opens the supplied copy directly, verifies, closes and reopens it, and checks its digest before/after; it must not create another image. Test both interfaces with allocation counters and inert adapters: one image allocation for the wrapper, zero for existing-copy verification, and a fixture socket trap that fails on network or live-source fallback.
- [ ] Implement every manifest field in spec §2, including image SHA-256, source map digest, build IDs, per-member checksums, dependency set, exclusions and explicit local/independent-copy status. Retain effective code-index defaults and source/revision availability, excluding only the derived sidecar.
- [ ] Run both commands GREEN. Commit `feat(recovery): verify restored images against captured expectations`; fresh task review.

### Task 4: Bound staging and publish complete durable bundles

**Files:** Create `crates/store/src/recovery/{fs.rs,backup.rs,catalog.rs}`; extend manifest/shared types and engine test filesystem adapters. Black-box tests: new `crates/store/tests/recovery_backup.rs`.

**Interfaces:** `backup(owner: &FerroStore, request: &BackupRequest, fs: &dyn RecoveryFs, cancel: &CancelToken) -> Result<PublicationOutcome, RecoveryError>`; `reconcile_publication(operation_id: &str, fs: &dyn RecoveryFs) -> Result<PublicationOutcome, RecoveryError>`. `BackupRequest` binds store identity/map digest, operation/batch ID, destination, capture/operation limits, validation profile and approved configuration-member descriptors. `VerifiedBundle` is constructed only after independent verification and durable publication.

- [ ] Add RED tests for source/destination identity alias, no-follow escapes, concurrent backup, inadequate space, arithmetic overflow and the small/incompressible-source operation and 2L+R limits. Assert failure before capture if the bounded output estimate cannot accommodate two full images plus metadata; streaming quota checks also stop growth before the limit. A reflink is not proof of independent validation or free future space.
- [ ] Add crash/fault cases for every image/manifest/report flush and directory sync, short write, ENOSPC, cancellation, kill, permission denial and collision. Assert old backups survive, source writes continue, final names never contain partial bundles, and rename-before-dirsync yields outcome-unknown rather than success.
- [ ] Run `cargo test -p fl-store recovery::backup` and `cargo test -p fl-store --test recovery_backup`; keep RED evidence.
- [ ] Implement admit → capture → private write → independent verify → atomic publish. Create directories 0700/files0600 exclusively, validate same publication filesystem and ownership, reject symlink/hardlink substitutions using handles/file identity. Use Task 3's `copy_and_verify(candidate, verification_copy, expected, profile, limits)` for backup verification, with both paths explicitly inside the admitted staging allocation. Account image, verifier copy, manifests/configs and all staging against the same 2L+R and operation quotas; delete the verifier copy only after its durable report is bound to the candidate. Keep time/memory limits explicit and abort cleanly.
- [ ] Flush all final members and staging directory before rename; use a unique immutable final name, then sync the parent before catalog success. Recover uncertain names by operation ID plus complete-member checksums; never broadly sweep staging or select uncertain bundles for restore. Catalog writes are themselves atomic and recoverable; reconcile a durable bundle missing its catalog entry.
- [ ] Admit only the allowlisted batch configs. Reject a vault-key pathname or identical file identity, including symlink/hardlink aliases, and never follow arbitrary referenced secret paths. A protected config snapshot may contain credentials under the later Ferris/Jarvis decision; it cannot be published as a public artifact. `vault.enc` remains optional and opaque; vault key never copied. No production copies in this task.
- [ ] Run tests GREEN, including proof that destination sync failure does not set source `Poison`. Commit `feat(recovery): publish verified bounded backup bundles`; fresh task review.

Task 4 implementation decisions approved by Ferris on 2026-11-10 (FerroWire
`01a1c381-a9d7-74ae-a9df-3000dff52273`):

- The image ceiling defaults to source physical file length L; callers may only
  lower it. There is no pre-capture engine estimator. Admission requires twice
  the ceiling plus R to fit min(2L + R, operation quota, available space minus
  an explicit margin). R adds the caller's bounded metadata/config/catalog
  payload budget to the observed-unit directory/rounding margin, capped in total
  at512MiB. Default L is admitted when quota and free space cover the reserve.
  This corrects the earlier 2L ruling (Ferris PR467 review, 2026-11-10). Streaming
  overflow reports `ImageExceedsCeiling`; fixture evidence records image/L ratios.
- Exactly one configured approved backup root is supported. Reject other roots
  and inventory alias status Unknown or ReviewRequired. Hold a `flock` on the
  physical inventory identity's lock file under that root's `.locks/` for the
  operation. No source-file lock, multiple-root/host coordination or P1 ownership
  claim is introduced. Lock diagnostics contain operation ID, pid and start time; they are not exclusion evidence. Identity-checked operation lock names are unlinked while held, with stale-inode contenders rejected.
- The immutable manifest retains NotPublished and independent-copy Verified.
  Only an atomic catalog receipt written after bundle rename and parent sync
  attests Durable. It binds manifest/image hashes and every member checksum;
  catalog temporary-file sync, exclusive rename and directory sync are required.
  Reconciliation checks only the requested operation's complete bundle before
  repairing an absent receipt. A mismatch is quarantined by operation ID, never
  deleted or selected for restore. Partial staging cleanup is operation-specific.

Task 4 implementation admission currently accepts only native Linux ext4/XFS
allocation geometry with a consistent power-of-two 512–65536-byte unit. It
derives R from32 prospective directory entries times129 units per entry (two
index-path blocks per64 levels plus a root),26 file/lock rounding units, and the
caller's metadata/config/catalog payload budget. R is capped at512MiB, not set
to that cap. At4KiB the directory/rounding part is17,014,784 bytes. This is an
approved conservative P0 safety margin, not a universal ext4/XFS allocation proof.
There is no historical directory-size or operation-count admission limit.
Clean ENOSPC/EDQUOT refusal is required at every directory entry, write and
allocation step, including failed quarantine. Earlier backups/receipts and the
source remain unchanged; final names contain only complete bundles. Post-rename
uncertainty stays OutcomeUnknown. Task7 must compare actual native ext4 directory
block growth across1000 mixed operations against the charged reserve and report
the ratio; any excess is a bug that blocks rollout. XFS evidence is still pending.
All file payloads are charged and allocated before writes; reflinks provide no
admission credit. Quarantine withdraws an existing receipt by exclusive rename to
.quarantine/<operation>/receipt.json and syncs both directories. Transient resource
errors do not quarantine. Read-only library status reports requested payload,
derived R, and bounded complete quarantine count/allocated-byte totals; a failed
scan returns an error. FL_RECOVERY_TEST_DIR explicitly selects native ext4/XFS
fixture storage; unsupported temporary filesystems fail clearly, never skip.

### Task 5: Add dormant owner-side schedules and batch accounting

**Files:** Create `crates/store/src/recovery/{policy.rs,scheduler.rs,control.rs}`; modify `crates/store/src/cli/{serve.rs,args.rs}`, `crates/store/src/ferrostore/{server.rs,auth.rs}`, `crates/cli/src/cmd/recovery.rs`. Black-box tests: new `crates/store/tests/recovery_daemon.rs` and `crates/cli/tests/recovery_batch.rs`.

**Interfaces:** `RecoveryScheduler::start(owner: FerroStore, policy: RecoveryPolicy, proof: Option<CaptureProof>) -> Result<RecoveryScheduler, RecoveryError>`; `request_batch(request: &BatchRequest) -> Result<BatchReport, RecoveryError>`; `select_retained(catalog: &[VerifiedBundle], policy: &RecoveryPolicy, now: SystemTime) -> Result<RetentionDecision, RecoveryError>`. `RecoveryEvent` contains IDs/status/reason/counts only. `CaptureProof` binds build, engine/schema and tested limits/profile; it is not an untrusted client flag that enables production schedules.

- [ ] Add RED fake-clock tests for hourly shared-batch workspace/FerroLoop, 6-hour projects, missed/failed runs, overlapping ticks, restart, disabled/no-proof policy and an unserved Council target. Assert no recurrence is enabled by defaults, no overlapping backup for the same physical store, no silent embedded fallback, and no global-sequence promise.
- [ ] Add daemon fixture tests that keep normal client handshakes/reads/writes available during slow encode/verify, including a capture failure. Assert backup work uses the existing owner handle and keeps the listener up. Policy must reject missing memory/time/disk limits; proposed fixture starting ceilings are 512 MiB retained pin and 10 min backup total, to be measured and approved before operational activation.
- [ ] Run `cargo test -p fl-store --test recovery_daemon` and `cargo test -p fl-cli --test recovery_batch`; keep RED evidence.
- [ ] Add versioned `--recovery-policy PATH` to shared serve arguments; absent policy means disabled with no recovery I/O or worker. A supplied missing/malformed/invalid policy or an enabled policy with closed activation gates must leave the data daemon serving normally with recovery inactive. Only recovery operations refuse; owner-only recovery status and startup logs expose `policy_invalid` with a fixed safe reason code, or the exact missing gates, never policy contents or paths. Introduce narrow recovery start/status controls on the existing server, authorized only for the local owner administrative principal, never generic read/write scope or remote TCP clients. Requests name a configured policy/store and operation/batch ID, not arbitrary paths. Extend method registry/auth tests from the same capability source; CLI uses explicit connection and refuses absent daemon, never `auto()`.
- [ ] The workspace daemon is the sole configured batch coordinator: its policy contains explicit member endpoints, intervals and `coordinator = true`; project daemons use `coordinator = false` and accept owner-authorized requests only. Its scheduler sends common batch IDs to each configured owner; each daemon performs capture/write/verify itself. A missing coordinator is reported as overdue coverage when status is inspected, not silently replaced by embedded backup. Persist per-store sequences and partial batch states; retain configs once per batch with manifest references. A batch is complete only when every requested member is durable; independent store success may survive a failed batch but cannot masquerade as a complete dependency set. Restart resumes status by operation ID, never a duplicate backup race.
- [ ] Implement deterministic retention using UTC buckets, newest verified member per bucket and a union of 24 hourly, 7 daily and 4 weekly generations. Protect last verified, pinned rollback and dependency/config members; garbage-collect a shared member only when no retained bundle references it. Reserve free capacity for twice the prospective bundle set in addition to existing retained data; insufficient space alerts and blocks new admission, never deletes the last recovery point to make room.
- [ ] Emit one failure event to the configured Jarvis/Ferris sink, and a second-consecutive event to the owner sink; successful scheduled run resets the failure count, polling/status does not. Runtime tests use inert sinks, bounded retries and operation-ID dedupe. No actual messages or live policy installation in fixture testing.
- [ ] Run both tests GREEN plus authorization/retention unit tests (`cargo test -p fl-store recovery::` and `cargo test -p fl-store tests_auth`). Commit `feat(recovery): add disabled owner-side backup scheduling`; fresh task review. Do not provision Council or change any service unit as part of this task.

- Task 5 follow-up: a separate small CLI/hub PR supplies the reviewed production RecoveryEventSink adapter before Task 7 activation; the store has no wire dependency and recurrence refuses a missing adapter.

### Task 6: Explicit offline backup and verified restore with durable fences

The exchange, lease, readiness and purge contracts below incorporate Ferris's 2026-11-10 Task 6 ruling. Send this plan diff for review before implementing the exchange path. All execution and crash proof in this task use fixtures; operational exclusion and dispatch restart are separate runbook actions.

**Files:** Create `crates/store/src/recovery/{restore.rs,readiness.rs,admission.rs,purge.rs}`; modify `crates/store/src/{any.rs,cli.rs}`, `crates/store/src/ferrostore/{mod.rs,recovery.rs}`, `crates/store/src/cli/{recovery.rs,data.rs,serve.rs,args.rs}` and frontends `crates/cli/src/cmd/store.rs`, `crates/store/src/bin/ferrostore.rs`. Black-box tests: new `crates/store/tests/{recovery_restore.rs,recovery_offline_backup.rs,recovery_purge.rs}`. Keep unit tests in their owning modules.

**Interfaces:** `backup_offline(request: &BackupRequest, lease: &OfflineLease, fs: &dyn RecoveryFs, cancel: &CancelToken) -> Result<PublicationOutcome, RecoveryError>` reuses Tasks 2–4; `restore_offline(request: &RestoreRequest, lease: &OfflineLease, fs: &dyn RecoveryFs) -> Result<RestoreOutcome, RecoveryError>`; `resume_restore(journal: &RestoreJournal, lease: &OfflineLease, fs: &dyn RecoveryFs) -> Result<RestoreOutcome, RecoveryError>`; `rollback_restore(journal: &RestoreJournal, lease: &OfflineLease, fs: &dyn RecoveryFs) -> Result<RestoreOutcome, RecoveryError>`; `check_restore_admission(physical_path: &Path) -> Result<(), RecoveryError>`; `assess_readiness(report: &VerificationReport, dependencies: &DependencyManifest, reconciliation: &ReconciliationRecord) -> ReadinessReport`. `create_offline_lease(request: &OfflineLeaseRequest, fs: &dyn RecoveryFs, limits: &OperationLimits) -> Result<OfflineLease, RecoveryError>` writes the administrative attestation; `finalize_restore(journal: &RestoreJournal, lease: &OfflineLease, reconciliation: &ReconciliationRecord, fs: &dyn RecoveryFs) -> Result<RestoreOutcome, RecoveryError>` is the explicit operator admission step. `purge_quarantined(request: &PurgeRequest, lease: &OfflineLease, fs: &dyn RecoveryFs, limits: &OperationLimits) -> Result<PurgeReport, RecoveryError>` defaults to inspection. `PurgeRequest` selects one operation ID and carries an optional matching confirmation; `PurgeReport` lists exact names, sizes, SHA-256 digests and audit/outcome state. `ReconciliationRecord` in readiness.rs records affected scopes, verified external outcomes and unresolved items; reuse Task 3's `DependencyManifest` in manifest.rs. Shared `RecoveryArgs` exposes lease creation, inspect/verify, backup-offline, restore/resume/rollback/finalize and purge through both frontends. Offline actions require `--lease PATH --request PATH` and a bound source/destination; purge requires `--confirm <op-id>` to mutate. An unavailable daemon never implicitly selects offline backup.

- [ ] Add RED restore tests for wrong identity/schema/binding, missing dependency, active owner/cached users, unreadable original, insufficient same-filesystem staging, stale session/request, newer external effects and rollback selection. `restore_rejects_cross_filesystem_staging` refuses before mutation; `restore_accounts_candidate_rollback_and_metadata` enforces the peak-byte equation below, including small-store refusal. `restore_exchange_requires_linux_single_link_unaliased_target` refuses non-Linux, unsupported `RENAME_EXCHANGE`, `nlink != 1`, and any inventory alias status other than `NoAliasObserved`; no copy-and-delete fallback is allowed.
- [ ] Add `restore_preserves_original_inode_without_second_image_allocation` and `restore_absent_target_verifies_single_candidate`: existing-target restore preserves the exact original inode as rollback without allocating another full image; absent-target restore uses `NOREPLACE`. In both, one distinct candidate (`C` once, `R = 0`) is independently closed/reopened and verified at S, backup checksums stay unchanged, and `C + V + M <= 2 * L`. Retaining a backup hardlink, omitting verification or counting metadata as free must fail.
- [ ] Add lease tests `lease_creation_is_private_bound_and_short_lived` and `lease_expiry_never_releases_restore_guard`: assert mode 0600 under the approved root, expiry no more than one hour after issuance, action/op/catalog/path/file-or-absent/bundle/build/profile binding, and explicit exclusion attestation. Restore leases bind the selected bundle digest; offline-backup leases bind the existing source instead, and purge leases bind the selected quarantined operation. Missing, expired, future/malformed or mismatched leases refuse. Lease expiry does not remove a guard; restart reconciliation requires a renewed matching lease and renewed operator confirmation.
- [ ] Add `offline_backup_unserved_store_requires_lease` in `recovery_offline_backup.rs`: an isolated Council-shaped fixture without a daemon succeeds only with a matching unexpired administrative lease, using Tasks 2–4's capture/verification/publication path. Refuse absent/expired/wrong-identity lease, an active owner, and ordinary remote requests to an absent daemon. A socket trap proves the explicit action uses existing-only source access without migration, creation or automatic fallback.
- [ ] Add `restore_completed_not_ready_stays_fenced` and `restore_finalize_requires_reconciliation_but_not_full_index`: ordinary opens remain refused after durable installation until explicit finalize records valid reconciliation. Missing/unknown/newer-than-S effect evidence or unresolved identity/dependency blocks finalize. Lexical/degraded/full index status is reported separately; an incomplete index alone does not block finalize. Only private read-only `RestoreAccess` can inspect fenced images; neither library nor CLI contacts vendors, resends effects or enables dispatch.
- [ ] Add `restore_exchange_and_reverse_exchange_recover_every_cut`: inject failures before/after durable guard creation, each original/candidate flush, each intent write/flush/directory sync, exchange, old-inode `NOREPLACE` relocation, reverse exchange, both directory syncs after every rename, installation completion, finalize and guard removal/sync. Cover absent-target `NOREPLACE` too. At each cut fresh direct `open`, `auto`, create-capable CLI and `serve` refuse without creating, migrating or contacting a data socket whenever the guard exists or cannot be validated. Before guard durability no destination/rollback mutation occurs. Reconciliation selects only journal-bound surviving identities and available digests; malformed/missing journal data or ambiguity remains fenced, never guessed from timestamps/names. Native Linux filesystem fixtures prove each claimed operation.
- [ ] Add purge RED tests in `recovery_purge.rs`: `purge_defaults_to_exact_dry_run_and_requires_matching_confirmation`, `purge_refuses_live_active_referenced_or_replaced_material`, and `purge_audit_is_durable_before_every_unlink`. Assert exact operation-owned names/sizes/digests, no dry-run writes or receipt withdrawal, no mutation for absent/wrong confirmation, receipt-withdrawal completion before confirmed purge admission, and refusal for symlinks/replacements, unreadable digest inputs, active operations, live catalog receipts or unproven dependency/config reference closure. Fault every audit/write/sync/unlink/parent-sync boundary; interrupted deletion resumes against the same audited identity set without touching another operation or reporting planned names as already removed.
- [ ] Run `cargo test -p fl-store --test recovery_restore`, `cargo test -p fl-store --test recovery_offline_backup`, `cargo test -p fl-store --test recovery_purge` and `cargo test -p fl-store recovery::`; retain RED evidence before implementation.
- [ ] Implement lease creation in admission.rs and shared CLI parsing. `OfflineLeaseRequest` binds operation ID, action, private catalog identity, canonical destination parent identity plus basename, expected current file identity/digest or explicit absence, selected bundle digest for restore (existing-source binding for offline backup, selected quarantine binding for purge), build/profile, issuance/expiry and an explicit attestation that all legacy/cached store handles and external dispatch are excluded. If original bytes cannot be read, record the held filesystem identity, read-error code and forensic classification rather than inventing a digest. Write the lease exclusively mode 0600 under the approved root with bounded private-file I/O; reject lifetimes above one hour. Hold process-local operation ownership separately. The lease is an administrative attestation, not a cryptographic token: lease, flock, PID/socket probes and service-state checks prove nothing about exclusion. Who may issue it is a Task 7 decision. Preserve resident fl-host/vendor sessions; no automatic service shutdown or vendor manipulation is added. Offline backup opens only the existing physical source and refuses unhealthy capture; it never enables recurrence or provisions a daemon.
- [ ] Implement `RestoreGuardV1` at `<physical-store-path>.restore-guard` with operation ID, canonical destination identity, journal location and phase. Under the lease, create it exclusively mode 0600, then durably sync it and its parent before any destination/rollback mutation. Check after physical path resolution but before any normal create, migration, direct/auto open, socket contact or serve startup; missing is allowed, present/unreadable/malformed fails closed with an operation-ID diagnostic when available. Recheck at the engine-open boundary. Refuse alias ambiguity; for an existing target require inventory `NoAliasObserved` and held `nlink == 1`. This sidecar does not fence legacy binaries or already-open handles, change the store schema, or establish P1 ownership.
- [ ] Only verifier/reconciler obtains private `RestoreAccess` from matching guard, journal and valid lease. It permits existing-only, read-only inspection of that operation's staged/selected image with external effects disabled; no environment switch, generic force flag or public normal-open bypass. No lease expiry clears a guard. Restart needs renewed administrative exclusion and a renewed lease. Durable install completion is not readiness: all ordinary opens remain refused until the operator invokes finalize with recorded typed reconciliation evidence. Clear the guard only after durable completion and readiness/effect reconciliation, then sync its parent before reopening admission; failed removal sync leaves outcome-unknown and administrative exclusion in force.
- [ ] Create staging as a unique mode-0700 sibling beside the destination and verify filesystem identity before copying. Fix `L` at admission to existing destination length, or selected verified image length if absent. Record immutable backup `B`, existing target `D`, candidate `C`, newly allocated rollback/forensic bytes `R`, verifier scratch `V`, and journal/guard/other metadata `M`. `B` and `D` stay capacity-accounted but are not new allocations. Preserve the original inode itself, so no second image is allocated and `R = 0`; every actual metadata allocation remains charged. Require checked `C + R + V + M <= 2 * L`, peak streaming enforcement, free space and original-target headroom on each filesystem. No reflink discount or later-permanent allocation exemption is allowed. Refuse before mutation if the bound cannot fit; no budget amendment is implied.
- [ ] Copy the immutable selected bundle image once into the private candidate, flush it, and invoke Task 3's `verify_existing_copy(candidate, expected, profile, limits)` directly. Do not call `copy_and_verify` on it or allocate another full verification image. Check distinct candidate/backup identity and the immutable bundle digest before/after; retain independent close/reopen checks. Charge verifier working files to `V`, candidate once to `C`.
- [ ] Implement a bounded versioned journal with operation/lease/store bindings, original and candidate identities, available digests, forensic classification, exact private names, phase and absolute limits. Existing-target replacement is Linux-only: verify readable old data as rollback (otherwise retain the original as forensic without claiming verified rollback), durably flush original/candidate as required, and persist/sync intent **before each step**. First `RENAME_EXCHANGE` candidate and destination; sync both directories. Then `NOREPLACE`-rename the old inode from the candidate name to the unique rollback/forensic name; sync both directories again. Record durable installation completion. Refuse unsupported exchange, non-Linux or cross-filesystem operations without a copy fallback. An absent target uses journaled `NOREPLACE`, also syncing both directories. Keep backup immutable and original inode preserved; never use legacy redb as current rollback or delete unreadable original material.
- [ ] Implement resume and explicit rollback from the journal's identities/available digests with all admission fenced. Verify the chosen complete image and finish its required directory durability before recording the decision. Reverse exchange for verified rollback is itself preceded by durable intent and followed by both directory syncs; any follow-on relocation uses its own durable intent and `NOREPLACE`. Preserve both generations. Forensic material is retained, never mislabeled as a verified rollback choice. Missing/corrupt metadata or an unidentifiable surviving name refuses reconciliation. No timestamp/name guessing or lease-expiry cleanup is allowed.
- [ ] Implement explicit finalize in readiness.rs and shared CLI. Require recorded invalidation/draining/restart of old clients, transactions, sessions and caches plus typed dependency/effect reconciliation evidence; P0 adds no persistent generation field and makes no stale-request safety claim for legacy clients. Unknown or newer-than-S vendor/outbox/replication effects never auto-resend. Require operator resolution for cross-store sequence mismatch and retain explicit unknown outcomes for images whose writes may be outcome-unknown. Library/CLI never contact vendors or enable dispatch. Report lexical/degraded/full index readiness separately; it does not gate finalize by itself.
- [ ] Implement quarantine purge in purge.rs under the approved root and matching physical-store administrative exclusion/operation ownership. Dry run never writes an audit or withdraws a receipt; report a pending withdrawal as requiring reconciliation. Confirmed execution first finishes a half-withdrawn receipt, then revalidates the operation and refuses any remaining live catalog receipt, active operation, symlink/replacement or unproven dependency/config reference closure. Dry run lists exact owned names, sizes and SHA-256 digests; an unreadable digest is refusal, not a weaker audit. Mutate only with `--confirm <op-id>` matching the selected operation. Before unlinking, durably publish and directory-sync a catalog purge audit containing operation ID, exact names/sizes/digests, time and actor. Record intent separately from per-name removal/completion so a crash never claims an intended removal already happened. Revalidate held identities before each unlink and sync affected parents. Delete only this quarantined operation's verified-owned quarantine/staging/final names; leave shared configs and other operations untouched. Never general retention, glob sweeps, legacy `.redb` or historic-file deletion.
- [ ] Run all four Task 6 test commands GREEN. Commit `feat(recovery): add offline journaled restore and readiness checks`; fresh task review. Implementation approval does not authorize a live restore, backup, deployment or operational activation.


**Task 6 implementation rulings (Ferris, 2026-11-10):**

- Each explicit resume/finalize or rollback action receives a fresh finite reconciliation deadline, capped by the renewed lease expiry. Persist attempt number and deadline; status reports deadline, remaining time and both counters. Original disk, memory and metadata ceilings never increase. Expiry stops at a journal boundary without cleanup, automatic renewal or guard removal.
- Renewed leases bind the current destination generation while the journal preserves immutable original admission. Full initial-request equality distinguishes an unchanged initial lease; renewal must prove all held names, identities and available digests match the current phase before recording any attempt. An intent permits only the exact before/after layouts of its one pending transition. Installed and selected finalize phases admit only the candidate; rolled-back and explicit `FinalizeOriginalIntent`/`OriginalFinalized` phases admit only the original verified identity and digest. Equal digests never imply equal generation roles. Mixed layouts or phase-mismatched known generations refuse without mutation.
- Resume and normal finalize share 12 attempts. Rollback and explicit rollback-finalize share a separate reserve of four. Neither counter resets on restart. Once the rollback reserve is exhausted, all further mutation refuses and status requires forensic handling. Rollback-finalize may use that reserve only for the journal-proven original **verified** identity and digest, with the same typed reconciliation evidence; it cannot admit the selected backup or an unreadable forensic original.
- An originally absent destination can be rolled back by journaled `NOREPLACE` relocation of the installed candidate into owned staging, with both directory syncs. Explicit absence-finalize consumes rollback reserve and requires original-absence proof, destination absence now without following links, retained candidate identity/digest and an operator attestation that no client opened the path while installed. It durably removes the guard without deleting the candidate or creating a store. Failed proof keeps the guard and requires forensic handling.
- P0 retains all restore candidates, rollback generations and forensic originals. Status lists their operation IDs, journal-bound paths, identities, sizes and available digests, plus retained file-byte totals; unreadable forensic digests remain explicitly unavailable. **Restore-artifact purge is deferred** to a separate typed design and Lead review. Backup-root quarantine purge has no authority over arbitrary destination-parent siblings. Task 7 must include the checklist item “retained restore artifacts reviewed.”
- Each restore has a stable private `journal.bin.lock` beside the journal, created exclusively with no-follow mode 0600 at admission, then file/directory synced and retained. The journal records its device/inode; restart never creates, replaces or repairs it. Hold bounded exclusive flock across each restore/resume/rollback/finalize action, reread the latest journal under that lock, and verify the recorded lock identity before any attempt choice or mutation. Read-only status/loading may hold a shared lock. This serializes recovery operations only; exclusion of ordinary/legacy clients remains administrative.
- Strict 2L admission precedes staging/lock allocation. A transient read-only catalog shared lock may gather preflight sizes, but is released before acquiring the operation lock. Private 0700 sibling staging and its charged stable lock may precede guard publication; hold operation EX before reacquiring catalog SH, then fully revalidate the selected bundle across that unlocked gap. Publish and sync the guard before candidate copying or any original/destination/rollback mutation. No operation-lock acquisition/wait occurs while catalog SH is held. Failed admission retains metadata without cleanup; a redacted typed failure preserves the exact boxed cause and only proven held staging/lock identities, with unknown partial metadata explicitly unproven.
- A narrow negative-only absence-finalize path may durably record a charged rollback attempt and forensic handling for a failed original-absence layout under the unchanged valid initial attestation and held journal/guard/operation proof. It cannot verify/adopt a candidate, rename material or remove the guard. Exhausted reserve or insufficient safe record capacity refuses without mutation. Normal initial-lease actions retain exact phase validation, and renewed impossible layouts always refuse before any attempt mutation.
- Task 6 capacity reporting: `B` includes the checked lengths of every selected bundle-owned file, including its manifest and validation/profile/report metadata; it remains existing capacity and never enlarges or enters the new-allocation `C + R + V + M <= 2L` allowance. Shared batch configuration retains its checked reference and is accounted once per batch, separately from each bundle-owned `B`. The journal separately fixes the initial read-byte ceiling for existing bundle/configuration checks; restart uses the lesser of that ceiling and current reconciliation limits. The immutable new-allocation ceiling remains enforced independently by publication I/O.
- The stable private `root/.locks/catalog.lock` is never unlinked or recreated. Publication receipt writes, reconciliation, quarantine, shared config publication and completed `.batch-<UUID>.json` records hold a shared lock. Purge holds an exclusive lock from reference scan through unlink; bounded lock wait refuses on timeout. Lock order is physical store, operation, then catalog, with borrowed lock-aware helpers rather than recursive acquisition. Dry-run inspection never bootstraps or syncs a missing lock. Administrative exclusion still covers legacy binaries and non-FerroLoop actors; this is no P1 ownership claim.
- Reference closure uses bounded verified other live receipts/manifests and typed completed batch records. Any explicit pointer to the target operation ID or bundle name in `reference_dependencies`, shared-config descriptors or completed batch records refuses; equal member digests across independent bundles are not references. A dry-run digest binds confirmation, and closure is recomputed after half-withdrawal. Changed, incomplete or unproven closure refuses. Shared configs and batch records are never deleted.
- Validation expectations use recovery metadata **format 2**. Finite vector scores persist as canonical 16-character lowercase hexadecimal IEEE-754 bits, so default JSON parse/reserialize preserves exact member checksums. Recomputed score comparison retains the profile's existing `1e-9` tolerance. Engine, snapshot, validation profile, manifest and verification report versions are unchanged; no global JSON parser feature is enabled. Format 1 expectations, including vector-free format 1, are explicitly unsupported with no migration or reinterpretation. No format 1 bundles were produced outside fixtures. The unchanged `Vec<f32>` probe profile is covered by a default-parser digest round-trip test.

### Task 6 R4 design-record update (2026-11-13, PR #481)

This dated update records the implemented review corrections and current limits;
it leaves the original task checkboxes and operational gates unchanged.
**Operational HOLD remains: no deployment, restart, live backup, restore or purge.
Only isolated local fixture tests are in scope.**

- Generic resume accepts `Prepared`, `ExchangeIntent`, `RelocateIntent`,
  `Installed`, `RolledBack` and `OriginalAbsenceRestored`, subject to the existing
  authority, layout and evidence checks. `ReverseIntent` refuses before an attempt
  and names `rollback_restore`; a rollback already started never resumes forward.
  Admitted/abandoned and terminal/finalize-intent phases name their typed action.
- In post-exchange `RelocateIntent`, an original without a verification report
  cannot support verified rollback. Resume can finish from the independently
  verified preserved candidate, on the exchanged or installed layout, even when
  the selected bundle is unavailable. Pre-exchange forward work still requires
  the selected bundle. Forensic material remains retained and unverified.
- Completed resume calls (`Installed`, `RolledBack`, `OriginalAbsenceRestored`)
  and completed rollback calls (`RolledBack`, `OriginalAbsenceRestored`) append no
  frame and consume no attempt after the existing authority/layout/evidence
  checks. Work still needed shares the unchanged 12-attempt resume/finalize limit
  and separate four-attempt rollback reserve; renewal never resets them.
- Admission reserves terminal capacity **before staging, lock or journal
  creation**, using maximum-width placeholder identities. It reuses one owned
  journal, then installs actual identities and rechecks the exact reservation.
  Refusal names `metadata_byte_limit`, offered bytes and an at-least required
  amount, with headroom for serialized budget widths; an impossible reservation
  states the supported maximum. Neither the metadata cap nor strict restore
  `2L`, disk/read/memory ceilings or cumulative schedule changes: at most
  `5 + 12 * 4 + 4 * 4 = 69` writer frames within the 96-frame reservation.
- Fresh terminal acceptance reserves fixed maxima **O128** for the accepting
  operator and **X256** for accepting-lease exclusion evidence, measured as JSON
  encoded bytes including quotes and escapes. Both operator occurrences (lease
  and reconciliation) are reserved. The reconciliation/absence evidence cap is
  derived from the saved journal's immutable fields and remaining frame room.
  One-byte-over new inputs refuse for free and state offered size and limit.
  Existing shape, working-memory and whole-frame checks can refuse first; this
  changes refusal priority without increasing any budget or acceptance bound.
  Authenticated already-accepted evidence bypasses fresh-input limits on retry.

The first post-release change to the accepting operator or exclusion-evidence
allowances (O128 and X256 encoded bytes) must add a persisted reservation-policy
version. First acceptance from earlier branch journals is unsupported because
those journals exist only in fixtures on this unmerged operational HOLD;
historically accepted contexts retain their authenticated retry path. No fallback
for a tight earlier fixture journal is provided.

Restore pins and purge audit intent/member/completion rows publish through fully
written, file-synced and hash-checked private temporary files, atomic `NOREPLACE`
rename and required directory syncs. Existing final names are never overwritten;
a malformed final pin still fails closed. Typed cleanup/resume handles only the
owning operation's checked temp under its locks. Four isolated mutations now
turn the corresponding regressions red: audit temp file sync, post-rename catalog
sync, unpublished pin hash check and the initial monotonic lease-deadline cap.
Each exact source restoration passes. The frozen-wall test proves the cap stops
namespace mutation even when wall time does not advance. The journal-boundary
expiry regression uses an unfinished forward intent and injected expiry after
charging, with ample real-time allowance; completed Installed calls are free.

Rollback retains its early `verify_preserved_candidate` check. A corrupt
preserved candidate refuses with exact journal/frame/attempt and artifact proofs.
The order-only mutant moving that check after `attempt` remains **green**, as
Ferris ruled equivalent: `continuation_limits`, rollback's `inspect_layout` and
`attempt_scoped`'s `inspect_phase_layout` independently hash the candidate before
charging; journal loading also validates generation/report bindings. No check is
weakened or private injection added to manufacture a red mutation result.

Read-only status exposes `accepted_reconciliation` and
`accepted_absence_reconciliation`. Selected/original INTENT retries require
byte-identical stored reconciliation. Absence finalization has no separate
INTENT phase; already-terminal cleanup uses persisted evidence and ignores
caller replacement evidence on both paths. Status does not consume attempts or
mutate artifacts. The canonical 19,238-byte pin region remains SHA-256
`7a6196e820b1b09fd77f3caa6f59515d4b0d36d8ab0996210269b28912c59bd0`
through all R4 code changes.

**Purge limitation:** inspection and confirmed purge require full bundle
verification **and a matching receipt**. They cannot purge most corrupt or
incomplete quarantine material, including an image digest mismatch, missing
report, truncated manifest or missing receipt. A corrupt live catalogued bundle
or completed-batch member with a quarantined receipt can also block inspection
and confirmed purge of an unrelated healthy target throughout the approved root.
Member-name diagnostics cover only some failures: a corrupt image, truncated
manifest or missing report in a completed-batch member names that member relative
to the root and retains its typed cause. The blocker remains unnamed when a
member directory is missing or replaced by a symlink, a member file has mode 0644
or is replaced by a symlink, a quarantined `receipt.json` contains garbage, or the
manifest hash differs from the batch record. A corrupt live catalogued bundle
also remains unnamed. These refusals remain fail-closed; they do not accept
corrupt material or delete anything. Regression snapshots prove retained
recovery artifacts and source unchanged with zero unlinks; a confirmed request
may refresh only existing store-lock timestamp bookkeeping. The root-wide
blocking and remaining unnamed diagnostics remain under
[#514](https://github.com/Artificial-Humanity/FerroLoop/issues/514); an as-found
purge mode remains deferred, with no automatic cleanup or manual deletion
authorization. The restore floor for stores under about 8 MiB remains
[#515](https://github.com/Artificial-Humanity/FerroLoop/issues/515), and scheduled
exhaustive CI remains [#516](https://github.com/Artificial-Humanity/FerroLoop/issues/516).

**Known limits and deferred nits:** zeroing the entire shared 4 KiB block
containing the previous frame's tail and final frame's header can make the
journal unloadable; this is outside the tested page old-or-new tear model. An
orphan pin temp whose staging directory was removed remains harmless but may
persist until same-operation typed cleanup. Broader stray-entry, forensic-abandon
and expiry-cause diagnostics, and clarification of the existing op-ID "own temp"
comment, are deferred; no additional pin exception or repair/unfence path is
introduced. The oversized parsed-JSON unit fixture now uses the common fixture
root.

**Fixture verification modes:** the workspace dev profile builds both `sha2` and
`fl-store` at `opt-level = 3`. Default exchange/reverse and completion/guard-cut
matrices use deterministic samples retaining event-kind endpoints, semantic
neighbors and original errno assignments on `journal_fixture`; exhaustive mode
selects their original complete schedules. The two terminal pairwise matrices
remain 12 cases each by default, with exact 96-case removal and 288-case sync
schedules in exhaustive mode. All modes retain their recovery/fencing assertions
and schedule-coverage tests. The subprocess poll sleeps 5 ms under its existing
15-second deadline.

From the repository root, use isolated native Linux ext4/XFS fixture storage;
set `FL_RECOVERY_TEST_DIR` to an isolated directory there when the default
temporary filesystem is unsupported. Portable commands include:

```sh
# Default recovery_restore suite (FL_RECOVERY_EXHAUSTIVE unset).
cargo test --workspace --test recovery_restore -- --test-threads=1

# All four full schedules, using each existing test's exact name as TEST_NAME.
FL_RECOVERY_EXHAUSTIVE=1 cargo test --workspace --test recovery_restore TEST_NAME -- --exact --test-threads=1
```

The four test names are
`restore_exchange_and_reverse_exchange_recover_every_cut`,
`restore_selected_and_original_finalize_recover_every_completion_and_guard_cut`,
`restore_terminal_all_generations_are_held_across_removal_and_later_authority_open`
and
`restore_terminal_absent_guard_and_pin_recheck_authority_and_layout_after_every_sync`.

R4 item 5 focused local harness times were 36.55/17.10 s for the default
exchange/finalization matrices and 60.91/33.26 s for exhaustive schedules. Cuts
were forward 49/71, reverse 12/17 and each finalize role 15/21 (default/full).
A cached unoptimized test-only rebuild took 2.69 s, the first optimized
package/test rebuild 52.86 s, and a subsequent integration-test rebuild 13.35 s.
These are different incremental rebuilds with cached dependencies, **not a cold
compile comparison or measured cold compile delta**.

Each R4 item has focused green checks and fresh scoped review. Final-head main
integration, workspace/Windows clippy, full workspace tests, exhaustive schedules
and whole-branch review remain pending for the coordinator. The required **CI
job <=30 minutes** and **CI recovery_restore <=10 minutes** remain unverified
until measured on the exact final head. Earlier R3 local results are historical
and do not establish either R4 target or operational readiness.

### Task 7: Package reproducible acceptance evidence and rollout gates

**Files:** Create `docs/operations/ferrostore-recovery.md`, `crates/store/tests/recovery_acceptance.rs`, and `crates/store/examples/recovery_fixture_baseline.rs`; update the architecture inventory only where implemented capability is now true. Evidence lives in private project scratch, with a redacted schema/example in docs.

**Interfaces:** `RecoveryAcceptanceReportV1` records source/build/platform/fixture identity, per-store-class profile, logical/sequence proof, fault outcomes, p50/p95/p99 latency, throughput, opens/connects/retries, RSS/pin bytes, backup/verify/restore duration, writer delay and queue peaks. No live dataset or secret-bearing report is committed.

Required activation blockers from Task 5 review (must be implemented and reviewed before Task 7 activation, not waived by Task 5 merge):

- [ ] Report `scheduled_blocked` when Running/OutcomeUnknown members prevent another schedule, naming bounded member IDs/status and documenting owner reconciliation or mark-failed resolution by operation ID.
- [ ] Prune terminal journal history to a bounded last-N window without removing bindings referenced by retained material or undelivered events; report `history_full` and remaining headroom. Never prune unresolved work to admit another backup.
- [ ] Enforce one overall coordinator-status deadline across members and report the timed-out members.
- [ ] Quiesce must join every worker even after one panics, and aggregate shutdown errors rather than returning before remaining joins.
- [ ] Detect backward clock jumps and expose scheduling suppression/clock status rather than silently waiting for time to catch up.
- [ ] Obtain an explicit owner decision on whether `local_owner` (all same-UID agent sessions) may trigger a bounded manual backup or issue an administrative offline lease, or whether separate owner authorization is required; record both decisions before activation.
- [ ] Before any live restore, deploy guard-aware binaries for every store opener and verify the opener inventory. Legacy binaries bypass the sidecar guard; a fixture test `acceptance_rejects_legacy_guard_unaware_opener` must reject an activation report that lacks this deployment proof. Administrative exclusion remains mandatory for cached/already-open handles.

- [ ] Add a RED acceptance test that rejects a report missing any authoritative class, native platform proof, unknown-collection coverage, measured quota, dependency/effect reconciliation or a restore time within 30 min. Do not assert wall-clock scheduling in unit tests; fake clocks verify policy and measured runs supply performance evidence.
- [ ] Run `cargo test -p fl-store --test recovery_acceptance`; retain RED. Implement report aggregation and an inert fixture example that generates cold/warm small/large/concurrent cases, domain transaction boundaries and failure matrices from spec §4. Never use a production path as a benchmark fixture.
- [ ] Run GREEN and the final trio: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Run native macOS/Windows tests for supported paths; list unsupported paths explicitly. Do not assert Windows safety from Linux cross-compilation.
- [ ] Future operator guard checklist (documentation requirement only; Task 7 remains unimplemented and operational activation stays on HOLD): inspect the matching restore journal and prefer typed abandonment with that matching journal. A stray guard may be cleared manually only after journal state, retained artifacts, originals and administrative exclusion have been reviewed and the Lead operational gate explicitly permits it; never automatically delete a guard or recommend unconditional removal. Review retained restore artifacts before clearing. Record that running daemons check the guard at bind, not per request, and legacy binaries and already-open/cached handles are not fenced; administrative exclusion is still required.
- [ ] Write the operator runbook: regenerate map; check all retained classes/dependencies; Jarvis verifies destination hardware/ownership; review budgets/proof; select one-off versus served mode; private backup verification; offline restore/reconciliation; rollback; alert handling. Capture inventory of domain multi-record atomicity and full Scan/Query read sets, with bounded retries and no external-effect reissue.
- [ ] State separate go/no-go gates: Ferris accepts code/proof; Jarvis provisions and approves operational scheduling; owner approves any off-host transfer, key escrow and later history retirement; a live drill needs an announced approved window. Unserved Council's missed six-hour coverage stays visible until its own daemon is authorized. Off-host status remains pending, not silently satisfied by the second NVMe.
- [ ] Commit `docs(recovery): document fixture proof and operational gates`; whole-branch independent review, required PR review and current-head CI before merge. Send the plan/evidence to Ferris; no deployment or operational activation follows automatically.

## Plan self-review and handoff

Coverage: spec §1 → Task 1 and Task 7; §2 capture → Task 2, verification → Task 3, durable publication/budgets → Task 4, recurrence/retention → Task 5; §3 → Task 6; §4 → every task's fixtures plus Task 7; §5 → global constraints and rollout gates. All five review-focus cases have named task tests. Later interfaces consume the shared types defined above; no new store schema or ownership guarantee is assumed.

Before implementation, Ferris must review this plan, including the proposed administrative control/API, durable restore admission guard, memory/time ceilings, conservative capacity accounting and unserved-store activation gap. No source-store reads, bundles, policy installation, live drills, alerts or filesystem provisioning were performed to write it. Off-host destination and vault-key escrow remain owner decisions, and historic-file retirement remains deferred.
