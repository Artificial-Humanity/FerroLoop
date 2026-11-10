# FerroWatch Per-Agent Usage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use subagent-dev, with a fresh implementation and independent review for each task. Steps use checkbox syntax.

**Goal:** Report retained, incrementally collected token usage by persona, vendor and model through CLI, an Owner-only hub API and FerroDeck.

**Architecture:** The hub owns a bounded background collector. It parses only appended complete JSONL records into count-only observations, commits them atomically with a source cursor, and resolves known conversation/persona associations without guessing. The workspace store is the durable source for the shared time-window aggregation used by the CLI and API.

**Tech Stack:** Rust, FerroStore and redb, React/TypeScript, Vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-11-10-ferrowatch-agent-usage-design.md`.

## Global Constraints

- No raw transcript content in persistent records, diagnostics or HTTP output. Read live files only; never mutate them.
- Preserve source wire identity, supported vendor meanings, missing versus zero and `[since, now)` UTC windows. Do not assign uncertain identities to a persona.
- Enforce bounded line, batch and scan work. Persist coverage/error state; incomplete data must not look complete.
- Checkpoint and normalized observations commit in one transaction for both backends, including remote FerroStore; plain StateStore upsert is not CAS.
- Work under `/var/tmp` for target and temporary directories. Follow AGENTS.md and WORKFLOW.md, full verification and the Felix co-author trailer.

## Review Focus

1. An incomplete trailing line is not skipped or counted twice: Task 2 scanner test.
2. Two writers and a crash at commit cannot advance a cursor without its observations: Task 1 store contract test.
3. Rewritten/rotated sources and duplicate snapshots cannot inflate totals: Task 2 vendor and reset tests.
4. Late parent/persona associations and deleted sources retain exact counts without guessing: Task 3 attribution tests.
5. A peer, malformed window or unavailable vendor cannot receive Owner data or fabricated zero: Tasks 4 and 5 API/CLI/UI tests.

### Task 1: Atomic usage storage contract

**Files:** create `crates/core/src/usage.rs`, `crates/store/src/usage.rs`, `crates/store/src/ferrostore/traits_usage.rs`; modify core exports, `crates/store/src/any.rs`, store schema/migration/verification registration, and existing shared store contract tests.

**Interfaces:** `UsageStore::read_usage_cursor(&self, source: &str) -> Result<Option<UsageCursor>, StoreError>`; `UsageStore::commit_usage_batch(&self, batch: &UsageBatch) -> Result<UsageCommit, StoreError>`; `UsageStore::query_usage(&self, query: &UsageQuery) -> Result<UsagePage, StoreError>`. `UsageBatch` contains a source identity, expected previous cursor/generation, next complete-line byte offset, normalized observations and completeness diagnostics. `UsageObservation` stores only stable event ID, source/session/parent IDs, optional persona, model, UTC event time, vendor, optional token-category counts and turn/subagent counts. The commit result distinguishes applied, already applied and conflict. Query pages are bounded and deterministic.

- [ ] Add a backend contract test: initial cursor 0 and one observation commit together; repeating the same batch is idempotent; different concurrent batches against one cursor yield one winner; rejected/failed transactions preserve the prior cursor and observations; reopening after commit keeps both. Verify deterministic pagination and retained rows after source removal.
- [ ] Run it and observe failure. Implement schema and both redb and FerroStore transactions, including FerroStore's remote validated transaction path. Validate sizes/counts before commit; support corrections keyed by stable event identity without inflating totals. Extend `AnyStore` dispatch and existing migration/integrity coverage as needed.
- [ ] Run the contract against redb, local FerroStore and remote FerroStore, plus affected format/migration tests; commit.

### Task 2: Incremental vendor ingestion

**Files:** create `crates/hub/src/usage/{mod.rs,scan.rs,parsers.rs}` and module tests; modify hub module exports.

**Interfaces:** `scan_source(source: &UsageSource, cursor: Option<&UsageCursor>, limits: ScanLimits) -> Result<UsageBatch, UsageScanError>` reads a bounded source chunk without writing the store; `parse_line(vendor: UsageVendor, bytes: &[u8], state: &mut ParserState) -> ParseOutcome` emits counts/IDs only. Persist parser baseline with the cursor whenever cumulative counters require it.

- [ ] Write sanitized per-vendor fixture tests: Antigravity token-bearing step kinds and timestamp/model; Codex `event_msg`/`token_count` cumulative totals, repeated snapshots and counter resets; Claude multiple entries and later corrections for one message; Muse `model_completed`; Grok missing fields yield unavailable. Never assert or retain prompt text.
- [ ] Add scanner tests for two appended chunks, a partial newline, malformed and oversized records, file replacement/truncation, repeated/full transcript overlap, bounded work and previously committed data surviving deletion. Ambiguous source generations are marked incomplete rather than added twice.
- [ ] Run failing tests, implement the parsers and scanner, then rerun fixtures and `cargo test -p fl-hub` focused tests; commit. Establish source identities from sanitized actual-format evidence rather than inventing turn IDs.

### Task 3: Collector and attribution

**Files:** create `crates/hub/src/usage/{collector.rs,attribution.rs}`; modify hub startup/lifecycle and `crates/hub/src/ws.rs` only where the background scheduler is owned.

**Interfaces:** `collect_once(store: &impl UsageStore, sources: &[UsageSource], identities: &IdentitySnapshot, limits: ScanLimits) -> CollectionStatus`; `resolve_persona(session: &str, parent_edges: &[ParentEdge], identities: &IdentitySnapshot) -> Attribution`. `Attribution` represents resolved, unresolved or ambiguous; parent edges carry IDs and an optional launch timestamp only.

- [ ] Write fixtures for internal Antigravity conversations whose “Message sent to <parent id>” edge arrives before/after usage, nested parents, missing parents/cycles, duplicate subagent launch, decommissioned parent, ambiguous routes and late hosted/tombstone/passive identity. Assert one subagent per known launch, unresolved totals preserved, and no transcripts stored.
- [ ] Test single-flight scheduling, first scan then append, concurrent collectors using the storage conflict contract, bounded retries, source loss and restart. Run tests red, implement a background task that does no filesystem scan on a GET request, then run green and commit.

### Task 4: Shared window aggregation and CLI

**Files:** create `crates/watch/src/usage.rs`; modify `crates/watch/src/lib.rs`, `crates/cli/src/cmd/watch.rs` and CLI integration tests.

**Interfaces:** `aggregate_usage(store: &impl UsageStore, query: &UsageReportQuery, now_ms: i64) -> Result<UsageReport, WatchError>` returns a half-open UTC window, grouped rows, coverage, unknown/unattributed/undated counts and category availability. `UsageGroupBy` is persona, vendor or model; default is persona. `fl watch usage [--since 48h] [--by persona|vendor|model] [--json]` reads only workspace rows.

- [ ] Test exact window boundaries, grouping, overflow-safe counters, unknown identities/model, unavailable vs zero, malformed ranges and offline store reads without starting a scanner. Add black-box CLI text/JSON and invalid-argument tests.
- [ ] Run red, implement shared aggregation and CLI, run focused watch and CLI suites, commit.

### Task 5: Owner-only hub usage API

**Files:** create `crates/hub/src/usage_api.rs` and endpoint tests; modify existing hub route dispatch and `crates/hub/src/lib.rs` module declarations.

**Interface:** `GET /api/v1/usage?since=<duration-or-UTC-timestamp>&by=<persona|vendor|model>` returns the Task 4 `UsageReport`; no scan is triggered by the request.

- [ ] Test Owner 200, Peer 403, unauthenticated 401, invalid/duplicate query 400, exact since boundaries, deterministic JSON, unavailable data and store failure 503. Assert no transcript fields appear.
- [ ] Run red, implement authorization before data access and share Task 4 validation/aggregation. Run hub API tests and commit.

### Task 6: FerroDeck usage view

**Files:** modify `ui/src/api/client.ts`, `ui/src/types/api.ts`, `ui/src/components/FerroWatchPane.tsx` and relevant CSS; add `ui/src/__tests__/FerroWatchUsage.test.tsx`.

**Interface:** Typed client reads Task 5 response. FerroWatch renders a sortable, keyboard-accessible table with persona/vendor/model, input/output/cache read/cache write, turns and subagents; show loading, empty, stale/incomplete and error states and unavailable as “No data.” Seven-day sparkline only if provided cheaply by stored data.

- [ ] Write component tests for Owner data, ordering, keyboard sorting, unavailable values, loading/error/empty and responsive table semantics. Run red, implement, then run UI tests, lint and production build; commit.

### Task 7: Integrated evidence and delivery

**Files:** fixture/integration tests as needed, with private output outside the repository.

- [ ] Test collection → durable restart → CLI/API/UI with isolated home and workspace, parent attribution and unchanged totals on rescan. Verify the 2026-11-08 through 2026-11-10 retained Antigravity window against the issue's approximate 17.4M input, 1.5M output and 247M cache-read, documenting UTC boundary and coverage without exposing content. If a source lacks reliable identities, surface incompleteness and report the discrepancy.
- [ ] Run `cargo test --workspace`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo fmt --all --check`, full UI tests/lint/build and isolated browser checks at desktop/tablet/phone. Run final independent whole-branch review and remediate through the project workflow.
- [ ] Push the branch and open a PR through `fl github pr create`; route it to the reported reviewer. Preserve the exact manual-comparison evidence and any remaining coverage limits in its description.
