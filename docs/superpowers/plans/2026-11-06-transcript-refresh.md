# Transcript Refresh Availability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use the repository `subagent-dev` skill: a fresh implementer per sequential task, immediate `code-review`/fresh fixer/scoped review, then whole-branch review. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep last-good same-session history visible during a transient transcript read failure without turning genuine empty history, a changed identity or a reset into stale-history reuse.

**Architecture:** Preserve observed, unavailable and absent-source distinctions at the transcript GET boundary. Use one small shared request-acceptance primitive, while App and standalone ChatPane retain ownership-keyed display state. Reset/auth/selection generations fence late responses; no server-side history cache is introduced.

**Tech Stack:** Existing Rust fl-hub, React/TypeScript, Vitest/Testing Library and temporary HOME/store fixtures; no new dependency.

**Spec:** [Accepted snapshot](../specs/2026-11-06-transcript-refresh-design.md), SHA256 `dbab187e7f50b2919201e19f9d94f319881fe4e889ab301dfe22a6723e02eaf0`.

## Global Constraints

- Source baseline: fetched `origin/main` `44e1bdfc25baff65e6fbbe35d6fad9f83046c66e`; isolated branch `freya/transcript-refresh-363`. Recheck source differences before execution if base changes.
- Lead contract approval/execution GO: `01a1b261-1978-7195-96ab-f7588e35d0c2`. Execute after fresh source-aware plan review confirms no material expansion. Felix cleared UI overlap in `01a1b261-8a25-720c-b41a-6cd678ea7e1b`.
- HTTP503 `code: transcript_unavailable`; reasons exactly `unstable_or_unreadable` or `source_not_available`; no fabricated turns or private path/raw error. HTTP200 empty remains valid only after existing fallback precedence.
- `resolved_session_id` is additive and optional; the existing `session_id` request echo is not proof of alias resolution. Existing Option/Vec compatibility helpers and queue/busy semantics stay unchanged.
- UI data belongs to an auth generation, agent, resolved concrete session and local reset epoch. Selection epoch also prevents A→B→A response reuse. No previous-owner data may render for one paint during effect cleanup.
- No vendor/live session probes, delivery changes, automatic retry/resend, receipt/queue/draft/uncertainty changes, route/cache repair, peer143/host work, installs, wrappers or service changes.
- Each task supplies real RED/GREEN evidence, scoped tests and a clean machine-authored commit with `Co-authored-by: Freya <freya@artificialhumanity.io>`. Coordinator verifies exports and review before next task. No blanket repeated baseline suite.

## Review Focus

- A stable zero-turn source falls through to other existing transcript/attempt sources; an unreadable source is not absence or empty. Task1 pins fallback precedence.
- A persona-only request/cache cannot prove an old concrete session remains selected; same agent/new session and unresolved alias cannot share visible history. Tasks2/3 pin ownership.
- A late success after a newer failure, A→B→A, or logout/relogin must not overwrite newer data, error or loading state. Tasks2/3 pin deferred-response races.
- Clear/Reforge succeeds or fails while reads are pending: success fences old responses; failure keeps last-good and error. Tasks2/3 pin both outcomes.
- 401/403 follow auth/access handling, not generic transient stale status; optimistic prompts and unconfirmed queue holds survive unavailable refresh. Tasks2/3 pin classification and unchanged delivery behavior.

## File Map and Existing Paths

- `crates/hub/src/discovery.rs`: `TranscriptReads`, `transcript_observation_from_sessions`, test hook and parser/fallback logic.
- `crates/hub/src/api.rs`: GET `/api/v1/sessions/:id/transcript`; `resolve_session_target_in` currently echoes unresolved input, so cannot itself prove resolution.
- `crates/hub/src/latency_tests.rs`: `hub_with_ferris`, `write_ferris_transcript`, `after_next_transcript_read` fixtures; no new subprocess or live transport tests.
- `ui/src/types/api.ts`, `ui/src/api/client.ts`: additive resolved identity; existing `HubApiError.details` already preserves bounded error JSON.
- New `ui/src/lib/transcriptReadGuard.ts` and unit tests: only request ownership/order/error classification helpers, not a general fetching framework.
- `ui/src/components/ChatPane.tsx`, `ui/src/__tests__/ChatPane.test.tsx`: standalone fetch/display and Clear/Reforge lifecycle; a narrow optional controlled-reset callback/refresh-state prop for App.
- `ui/src/App.tsx`, `ui/src/__tests__/ContextAndWireUx.test.tsx`: all foreground/initial/background reads, persona badge cache, selection handlers, auth transitions, controlled ChatPane and teardown.

---

### Task1: Preserve transcript availability and proved identity at the GET boundary

**Files:** Modify `crates/hub/src/discovery.rs`, `crates/hub/src/api.rs`, `crates/hub/src/latency_tests.rs`, `ui/src/types/api.ts`; unit tests in discovery.rs only if needed for fallback construction.

**Interfaces:**
- Produce crate-private `TranscriptObservation { Observed(Vec<TranscriptTurn>), Unavailable, NoSource }` and `observe_transcript_from_sessions(session_id: &str, sessions: &[DiscoveredSessionSummary], snap: &DiscoveryStoreSnapshot, home_override: Option<&Path>, files: &mut TranscriptReads) -> TranscriptObservation`.
- Existing `transcript_observation_from_sessions(...) -> Option<Vec<TranscriptTurn>>` delegates with `Observed(v) => Some(v)`, `Unavailable => None`, `NoSource => Some(vec![])`, preserving existing consumers. Existing Vec wrapper remains unchanged.
- GET maps Unavailable/NoSource to503 bounded reasons; Observed retains existing200 body and adds `resolved_session_id` only when the requested id/persona has an actual matching discovered session. A resolver fallback echo is never a match. Keep attempt excerpt behavior; an observed attempt need not fabricate a session match.
- Add `resolved_session_id?: string | null` to `SessionTranscriptResponse`; absent from older servers means unknown. No unrelated client request changes.

- [ ] Write failing `transcript_get_unstable_observation_is_unavailable_then_recovers`: temp fixture with populated transcript, stable GET succeeds; install `after_next_transcript_read` to rewrite/remove after the actual transcript read, GET must503/code/reason with no turns/path; next stable read recovers. Stub existing queue delivery even if queues are empty, so fixture cannot dispatch a real vendor.
- [ ] Write `transcript_get_distinguishes_observed_empty_absent_and_fallback`: empty source with no fallback=>200[], no source=>503/source_not_available, observed empty plus matching attempt excerpt=>200/excerpt. Pin existing compatibility wrappers returning their previous Option/Vec meanings for NoSource.
- [ ] Write `transcript_get_only_reports_proved_resolved_identity`: concrete/session alias match yields correct id, unmatched input never echoes as resolved identity. Preserve request session_id compatibility.
- [ ] Run focused new tests; capture actual behavioral RED (not merely compile failure). Implement the minimal typed observer, preserving existing source order, read caching and early unavailable propagation. Do not add reread retries or parser changes. Do not redesign GET's existing drain/busy logic.
- [ ] Run `cargo test -p fl-hub --lib transcript_get_` and affected discovery/latency filters; verify genuine empty and attempt fallback tests pass. Record exact commands/counts and commit. Immediate fresh review must confirm wrapper/queue/busy compatibility and no live fixtures before Task2.

### Task2: Shared read fencing and standalone ChatPane retention/reset controls

**Files:** Create `ui/src/lib/transcriptReadGuard.ts`, `ui/src/__tests__/transcriptReadGuard.test.ts`; modify `ui/src/components/ChatPane.tsx`, `ui/src/__tests__/ChatPane.test.tsx` and API type imports if needed. No App edits yet.

**Interfaces:**
- Export `TranscriptReadOwner { authEpoch: number; selectionEpoch: number; agentKey: string; sessionId: string | null; resetEpoch: number }`, `TranscriptReadTicket` and `createTranscriptReadGuard()` returning `{ begin(owner): TranscriptReadTicket; accepts(ticket, currentOwner): boolean; invalidate(): void }`. Ticket copies owner values and uses a monotonically increasing sequence; latest-started request wins per exact owner. Invalidation makes all prior tickets stale. No module-global data/counters.
- Export `isTransientTranscriptReadError(error: unknown): boolean`: explicit transcript_unavailable503 and network/read failures true;401/403 false and excluded before generic classification. Other known nontransient HTTP refusals do not become stale-history success. A successful body with missing/malformed turns is not observed-empty; handle it as unavailable rather than using `?? []`. Inspect existing HubApiError; do not change its global behavior.
- Add optional ChatPane `transcriptRefreshState?: 'loading' | 'ready' | 'stale' | 'unavailable' | 'denied'` for controlled rendering, and optional `onResetSession?: (targetId: string, mode: ClearSessionMode) => Promise<unknown>`. When supplied, delegate the same existing clear operation to the parent; otherwise retain direct apiClient/fallback behavior. Neither interface sends a second reset.
- Standalone display snapshot records concrete session ownership. A sessionId prop is a known concrete request identity; a persona-only response needs a positive `resolved_session_id` before it can own reusable last-good. Contradictory returned resolved id is rejected/refetched by existing scheduling, not silently installed. The legacy session_id echo alone cannot resolve an alias.

- [ ] RED unit tests: latest request succeeds then old success/error/finally rejected; newer failure then older success rejected; invalidation and A→B→A epoch rejected. Same session string under changed auth/reset epoch rejected. Tickets copy owner rather than retain mutable objects.
- [ ] RED standalone tests: populated->503/network failure->recovery retains matching turns and shows `History refresh unavailable; showing last loaded history`; initial unavailable shows unavailable, not no-history;200[] clears;401/403 do not show stale notice/old data. Test unknown alias cannot reuse a concrete-session snapshot.
- [ ] RED deferred tests: session/agent/auth changes immediately mask old turns before effects; stale completion cannot update turns/error/loading/queue/busy metadata; Clear/Reforge with pending read success cannot resurrect pre-reset history, failure preserves last-good and error. Preserve existing draft and unconfirmed hold assertions.
- [ ] Implement helpers and standalone ownership state. Generate an auth epoch for apiClient/authority identity transitions, invalidate on unmount/selection/reset, and increment reset epoch when reset begins. If standalone use lacks an authority prop, token/client identity changes must still invalidate in-flight reads without serializing or logging credentials; never rely only on equal authority tiers. While reset is pending accept no transcript commits. On reset failure keep the captured same-owner last-good under the new generation; on success drop old snapshot and refetch. Do not treat reset transport success as proof of vendor consumption.
- [ ] Render only matching-owner turns, and gate the empty state on a successful ready observation. Show stale/unavailable/denied separately without changing composer/draft/send or queue hold behavior. Keep controlled and standalone paths distinct; controlled App receives state/callback props in Task3.
- [ ] Run `npm --prefix ui test -- --run src/__tests__/transcriptReadGuard.test.ts src/__tests__/ChatPane.test.tsx` and `npm --prefix ui run lint`. Commit with RED/GREEN evidence and immediate fresh review. Verify exported signatures before Task3.

### Task3: Apply the same rule to App reads, caches, auth/selection and controlled resets

**Files:** Modify `ui/src/App.tsx`, `ui/src/__tests__/ContextAndWireUx.test.tsx`; narrow additional existing App integration test files only if they own a required scenario. Consume Task2 helper and ChatPane props, no second request framework.

**Interfaces:**
- Route all transcript result/error/finally acceptance in `scanBackgroundTranscripts`, `refreshSessionsAndTranscript`, `loadAllInitialState` and selection-load effect through the Task2 guard. Capture tickets at read dispatch, never after await. For initial bulk loading capture auth/view generation before that work starts; late old-auth initial work cannot become current just because loggedOut changed back to false.
- Keep badge data persona-indexed if needed, but selected-chat snapshots must include concrete session/auth/reset ownership; every direct cache-to-view selection path must check it. An unresolved alias or a new concrete session never adopts persona-only cached turns. Selection generation advances synchronously at every selection entry point and on derived concrete-session changes, not only after useEffect.
- Pass matching snapshot turns and refresh state to controlled ChatPane. Wire `onResetSession` to one App handler around existing `apiClient.clearSession`: invalidate reads before mutation, preserve same-owner last-good on failure, discard on success and refetch. Teardown uses equivalent invalidation but retains existing queue semantics; failed teardown must not permanently suppress future observations. Only reset's successfully discarded target optimistic view changes as required; do not edit delivery receipt/queue uncertainty logic.

- [ ] RED App integration `keeps_last_good_history_during_unavailable_refresh`: fixed agent/session, nonempty response ->503/network failure ->recovery; old history/cache remain with stale notice, next accepted success clears notice. Initial failure never renders another session or false no-history. Successful200[] does clear.
- [ ] RED deferred integration cases: newest-first/oldest-last, newer failure then older success, agent A→B→A, same agent sessionA→sessionB and background-cache ownership, unresolved alias, logout/relogin, unmount. Assert rejected old requests cannot alter turns/cache/status/error/busy/queue/context/quota; no stale response may synthesize idle or empty queue. Preserve existing background badge tests.
- [ ] RED reset cases: pending pre-reset response after successful clear/reforge/teardown cannot restore history; failed reset retains last-good/error and allows a later legitimate refresh. Verify reset delegated exactly once. Include draft/optimistic/unconfirmed-hold regressions;401/403 classification must preserve auth/access isolation.
- [ ] Implement the ownership-keyed foreground snapshot and bounded same-session retention using Task2 helpers; update all existing cache-to-view handlers (roster, triage, mobile, selected-session paths) and auth transitions consistently. Use explicit auth generation rather than trusting authority tier alone. Do not broaden unrelated application data fetching or auth protocol.
- [ ] Run focused `npm --prefix ui test -- --run src/__tests__/ContextAndWireUx.test.tsx src/__tests__/ChatPane.test.tsx src/__tests__/transcriptReadGuard.test.ts`, then full `npm --prefix ui test -- --run` and `npm --prefix ui run build` once. Record actual results and review task diff before advancing.
- [ ] Run affected Rust checks if Task1 changed after its review, then required final `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` once for the final source. Logs stay outside the repo. Check whether ui/dist is tracked; include only tracked output if repository requires it. No installs/deployment or live probes.
- [ ] Commit clean source with Freya trailer. Fresh whole-branch review against the exact accepted contract follows task approval; resolve accepted findings through fresh fixer/scoped review (max3 rounds). Return exact head, base, changed paths, RED/GREEN/current-check evidence and review dispositions to Ferris before publication. No push/PR until that handback's publication disposition; source review/required green CI/exact-head approval govern any later author merge.

## Plan Self-Review

All accepted requirements map to Task1 availability/fallback/identity or Tasks2/3 shared request ownership, transient classification, reset and caller integration. The five Review Focus cases have named test owners. New interfaces are limited to one observation enum/function, one request-guard module and two optional controlled ChatPane props; no dependency or persistent schema is added. Tests above are planned, not run. Any actual source/contract mismatch or need for a materially broader fetching/auth/reset design returns to Ferris before dependent code.
