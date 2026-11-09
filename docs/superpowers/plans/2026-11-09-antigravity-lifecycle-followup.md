# Antigravity CLI Lifecycle Follow-up Implementation Plan

> **For agentic workers:** Follow the repository's `subagent-dev` and `code-review` workflow:
> a fresh implementer per task, then independent review and scoped remediation.

**Goal:** Recover missing Antigravity cleanup identity/connections and set native display titles.

**Architecture:** Keep the existing CLI adapter and lifecycle paths. Share bounded native-session
resolution between teardown and an optional CLI-adapter finalization hook; persist verified IDs.

**Tech Stack:** Rust, existing host protocol, Linux procfs, existing ConnectRPC cleanup client.

**Spec:** `docs/superpowers/specs/2026-11-09-antigravity-lifecycle-followup-design.md`

## Global Constraints

- No AntigravityHostAdapter, remote-control surfacing, or startup trajectory RPC work.
- Keep Records 179/181 behavior and permission-posture validation unchanged.
- Discovery plus deletion retains the existing two-second total budget.
- No live-machine probing from temporary-home tests; no vendor inference or service changes.
- Exact native UUIDs only; preserve sibling vendors, personas, and process generations.

## Review Focus

- Cached persona routes may belong to a different native generation: require corroboration
  before accepting an unknown native ID (Task 1).
- Process-local network tables contain unrelated namespace listeners: correlate socket inodes
  and refuse ambiguity (Task 1).
- Process death or replacement can race discovery: recheck host ownership/stream and birth
  evidence before using recovered targets (Task 1).
- Native identity may arrive after the initial launch request: retry through normal refresh
  and avoid unchanged state writes (Task 2).
- Existing annotation content can contain comments, escaped strings, and unrelated fields:
  preserve them and refuse unsafe paths (Task 2).

### Task 1: Resolve missing cleanup identity and endpoint

**Files:** Create `crates/wire/src/vendor_adapter/antigravity_session.rs`; modify
`crates/wire/src/vendor_adapter.rs` and the cleanup slice of `crates/hub/src/lifecycle.rs`.
Unit tests stay with the module; real host/CLI integration tests belong in `crates/cli/tests/`.

**Interface:** Export a small resolver request containing exact hosted ID, host target,
optional known native UUID, optional cached route evidence, and explicit live-probe permission.
Return optional verified native UUID and paired endpoint/token with redacted Debug output.
Take an absolute deadline. Keep process/host probes injectable for tests; default unsupported
runtime implementations must not acquire live-host authority. The implementer may choose
concrete Rust type names and must report the exported signatures for Task 2.

- [x] Write failing missing-route cleanup and resolver tests, including the five applicable
  identity/connection hazards in the spec and Review Focus.
- [x] Run focused tests and record the expected failure.
- [x] Implement bounded exact-host/process resolution, descendant environment recovery,
  and known-UUID idle LS-child listener recovery. Unsupported evidence fails closed.
- [x] Integrate fallback before route removal/runtime stop. Include discovery in the total
  deadline and retain the existing successful cached-route behavior and diagnostics.
- [x] Run affected wire/hub tests and meaningful isolated cleanup integration checks.
- [x] Commit with Fiona's co-author trailer and report the resolver interface.

### Task 2: Finalize titles on the existing CLI path

**Files:** Extend the Task 1 module and `VendorCliAdapter`; modify narrow spin-up and
running-session refresh slices in `crates/hub/src/lifecycle.rs`; add isolated integration tests.

**Interface:** Add an optional CLI-adapter finalization method with a no-op default. Its request
contains the exact session binding, known native ID, home, and display name. It returns a
verified native ID when available. Antigravity uses Task 1's resolver; other adapters are
unchanged. Lifecycle persists a newly resolved ID and retries when identity is delayed.

- [ ] Write failing tests for exact UUID/title creation, escaping and existing-field
  preservation, no-op repeat, unsafe paths, delayed identity, and actual lifecycle invocation.
- [ ] Run focused tests and record the expected failure.
- [ ] Implement bounded atomic annotation updates and the optional finalization hook.
- [ ] Call after successful spin-up and from running-session refresh; preserve launch success
  on unavailable native identity and avoid repeated unchanged writes.
- [ ] Run affected tests, then full `cargo test --workspace`,
  `cargo clippy --all-targets --workspace -- -D warnings`, and `cargo fmt --all --check`.
- [ ] Commit with Fiona's co-author trailer; run independent whole-branch review.

## PR and landing

Open a separate PR with `fl github pr create`, request Ferris review, and merge only after
exact-head approval and green CI. Include the missing-route defect and the manually removed
Ferris (`4df33135-e9a4-4f85-adfd-3b09dc339628`) and Kayla
(`6e786011-f4af-43fc-928b-a8c92a5da9fd`) cascades. State explicitly that Records 179/181 are
already implemented and the Record 186 adapter work is deferred.
