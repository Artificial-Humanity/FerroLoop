# FerroWatch Per-Agent Usage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use subagent-dev, with a fresh implementation and independent review for each task. Steps use checkbox syntax.

**Goal:** Report retained, incrementally collected token usage by persona, vendor and model through CLI, an Owner-only hub API and FerroDeck.

**Architecture:** A single-flight hub collector is the only writer. It parses complete appended JSONL records into per-source hourly buckets and atomically writes each source document with its byte cursor and parser baseline. Separate identity mapping documents are joined at query time. CLI and API only read the dedicated FerroStore collection; redb is explicitly unsupported.

**Tech Stack:** Rust, FerroStore (ferro engine), React/TypeScript, Vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-11-10-ferrowatch-agent-usage-design.md`. Ferris approved the implementation on this storage basis; the issue is the owner-approved scope.

## Global Constraints

- No raw transcript content in persistent records, diagnostics or HTTP output. Read live files only; never mutate them.
- Preserve source wire identity, supported vendor meanings and missing versus zero. For requested `[since, now)` UTC, query whole hours `[floor(since), ceil(now))` and expose requested and effective boundaries; already aligned endpoints stay unchanged. Do not assign uncertain identities to a persona.
- Enforce bounded line, batch and scan work. Persist coverage/error state; incomplete data must not look complete. Retain deleted sources and mark ambiguous rewrites incomplete.
- Source cursor, generation, parser baseline and hourly counts move together via one existing atomic FerroStore document write. One hub-owned single-flight collector writes; CLI/API read only. Do not add cross-process CAS, per-event rows, or redb support.
- Work under `/var/tmp` for target and temporary directories. Follow AGENTS.md and WORKFLOW.md, full verification and the Felix co-author trailer.

## Review Focus

1. An incomplete trailing line is not skipped or counted twice: Task 2 scanner test.
2. Crash before a single source write changes neither cursor nor counts, and rescan is idempotent: Task 3 tests.
3. Rewritten/rotated sources and duplicate snapshots cannot inflate totals: Tasks 2 and 3 vendor and reset tests.
4. Late parent/persona associations and deleted sources retain counts without guessing: Task 3 attribution tests.
5. A peer, malformed window or unavailable vendor cannot receive Owner data or fabricated zero: Tasks 4–6 tests.

Task numbering preserves the approved plan references: the former Task 1 storage work is folded into Task 3; execute Tasks 2 through 7 in order.

### Task 2: Incremental vendor ingestion

**Files:** create `crates/hub/src/usage/{mod.rs,scan.rs,parsers.rs}` and module tests; create `crates/watch/src/usage.rs` for shared serializable usage document types and export it; modify hub module exports.

**Interfaces:** Define `SourceUsageDocument` with source/session identity, vendor, byte offset and generation, parser baseline, `(model, UTC hour)` buckets, undated counts and coverage state; each bucket holds the four token categories, turns and per-category unavailable flags. Define small identity mapping documents with session, persona, parent, launch time and provenance. `scan_source(source: &UsageSource, current: Option<&SourceUsageDocument>, limits: ScanLimits) -> Result<SourceUsageDocument, UsageScanError>` produces a candidate in memory without writing; `parse_line(vendor: UsageVendor, bytes: &[u8], state: &mut ParserState) -> ParseOutcome` emits counts/IDs only. The bounded parser baseline includes Codex cumulative totals and recent Claude message IDs with the prior values and bucket identity needed to apply corrections safely.

- [ ] Write sanitized per-vendor fixture tests: Antigravity token-bearing step kinds and timestamp/model; Codex `event_msg`/`token_count` cumulative totals, repeated snapshots and counter resets; Claude multiple entries and later corrections for one message; Muse `model_completed`; Grok missing fields yield unavailable. Never assert or retain prompt text.
- [ ] Add scanner tests for two appended chunks, a partial newline, malformed and oversized records, file replacement/truncation, repeated/full transcript overlap and bounded work. Preserve prior counts and mark ambiguous generations or corrections outside safe deduplication state incomplete rather than adding twice. Establish source identities from sanitized actual-format evidence rather than inventing turn IDs.
- [ ] Run failing tests, implement the parsers and scanner, then rerun fixtures and focused `cargo test -p fl-hub` tests; commit.

### Task 3: Collector, storage and attribution

**Files:** create `crates/hub/src/usage/{collector.rs,attribution.rs}`; extend `crates/watch/src/usage.rs` with shared attribution resolution; modify hub startup/lifecycle and `crates/hub/src/ws.rs` only where the background scheduler is owned; extend shared usage types as needed.

**Interfaces:** `collect_once(store: &AnyStore, sources: &[UsageSource], identities: &IdentitySnapshot, limits: ScanLimits) -> CollectionStatus` obtains the ferro engine through `as_ferro()`, reads existing source documents, scans to candidates and writes each via one `put_doc(collection, id, &value)`. Use a non-reserved dedicated collection such as `watch_usage_v1`; `get_doc` reads IDs and `query_by_field` enumerates documents using an indexed type discriminator (then `get_doc` loads each returned ID). The collection is not registered as searchable/renderable FerroState. Separate mapping-document writes contain session/conversation ID, persona, parent ID, launch time and provenance. `resolve_persona(session: &str, mappings: &IdentitySnapshot) -> Attribution` lives in the shared watch module for query-time aggregation; resolved, unresolved and ambiguous identities stay distinct. Return an explicit unsupported error when `as_ferro()` returns `None` for redb.

- [ ] Write fixtures for internal Antigravity conversations whose “Message sent to <parent id>” edge arrives before/after usage, nested parents, missing parents/cycles, duplicate subagent launch, decommissioned parent, ambiguous routes and late hosted/tombstone/passive identity. Query-time joins must preserve unresolved totals, assign late bindings without rewriting source buckets and count one subagent per known launch. Assert no transcripts stored.
- [ ] Test one in-flight collector, first scan then append, crash after parsing but before `put_doc` leaving the old document unchanged, restart and idempotent rescan, retained source documents after deletion, truncation/rotation and ambiguous rewrite coverage. Test ferro collection isolation from FerroState search/render and explicit redb unsupported behavior.
- [ ] Run tests red, implement the single-flight background task and atomic document persistence; do not scan on GET and do not add a new store transaction/CAS layer. Run tests green and commit.

### Task 4: Shared window aggregation and CLI

**Files:** extend `crates/watch/src/usage.rs`; modify `crates/watch/src/lib.rs`, `crates/cli/src/cmd/watch.rs` and CLI integration tests.

**Interfaces:** `aggregate_usage(store: &AnyStore, query: &UsageReportQuery, now_ms: i64) -> Result<UsageReport, WatchError>` reads source and mapping documents from ferro, joins attribution at query time and returns requested and effective half-open UTC windows, grouped rows, coverage, unknown/unattributed/undated counts and category availability. `UsageGroupBy` is persona, vendor or model; default is persona. `fl watch usage [--since 48h] [--by persona|vendor|model] [--json]` reads only workspace documents and returns explicit unsupported for redb.

- [ ] Test floor/ceil hour boundaries (including already-aligned endpoints), grouping, overflow-safe counters, late attribution, unknown identities/model, unavailable vs zero, malformed ranges and offline store reads without starting a scanner. Add black-box CLI text/JSON, effective-window disclosure and invalid-argument tests.
- [ ] Run red, implement shared aggregation and CLI, run focused watch and CLI suites, commit.

### Task 5: Owner-only hub usage API

**Files:** create `crates/hub/src/usage_api.rs` and endpoint tests; modify existing hub route dispatch and `crates/hub/src/lib.rs` module declarations.

**Interface:** `GET /api/v1/usage?since=<duration-or-UTC-timestamp>&by=<persona|vendor|model>` returns the Task 4 `UsageReport`, including requested and effective hour-rounded boundaries; no scan is triggered by the request.

- [ ] Test Owner 200, Peer 403, unauthenticated 401, invalid/duplicate query 400, rounded boundaries, deterministic JSON, unavailable data, explicit unsupported redb and store failure 503. Assert no transcript fields appear.
- [ ] Run red, implement authorization before data access and share Task 4 validation/aggregation. Run hub API tests and commit.

### Task 6: FerroDeck usage view

**Files:** modify `ui/src/api/client.ts`, `ui/src/types/api.ts`, `ui/src/components/FerroWatchPane.tsx` and relevant CSS; add `ui/src/__tests__/FerroWatchUsage.test.tsx`.

**Interface:** Typed client reads Task 5 response. FerroWatch renders a sortable, keyboard-accessible table with persona/vendor/model, input/output/cache read/cache write, turns and subagents; disclose effective window boundaries and show loading, empty, stale/incomplete and error states and unavailable as “No data.” Seven-day sparkline only if provided cheaply by stored data.

- [ ] Write component tests for Owner data, ordering, keyboard sorting, unavailable values, effective window, loading/error/empty and responsive table semantics. Run red, implement, then run UI tests, lint and production build; commit.

### Task 7: Integrated evidence and delivery

**Files:** fixture/integration tests as needed, with private output outside the repository.

- [ ] Test collection → durable restart → CLI/API/UI with isolated home and workspace, parent attribution and unchanged totals on rescan. Verify the 2026-11-08 through 2026-11-10 retained Antigravity window against the issue's approximate 17.4M input, 1.5M output and 247M cache-read, documenting requested and effective UTC boundaries and coverage without exposing content. If a source lacks reliable identities, surface incompleteness and report the discrepancy.
- [ ] Run `cargo test --workspace`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo fmt --all --check`, full UI tests/lint/build and isolated browser checks at desktop/tablet/phone. Run final independent whole-branch review and remediate through the project workflow.
- [ ] Push the branch and open a PR through `fl github pr create`; route it to the reported reviewer. Preserve the exact manual-comparison evidence and any remaining coverage limits in its description.
