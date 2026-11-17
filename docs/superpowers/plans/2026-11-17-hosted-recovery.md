# Hosted restart and bounded recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use subagent-dev for task implementation and code-review for fresh task/fix/whole-branch reviews. Steps use checkbox syntax.

**Goal:** Recover hosted crash loops safely and provide saved-record CLI restart (#573).
**Architecture:** PR B based on PR A #572. Reuse shared restart lifecycle while adding strict CLI selection/Caller authorization. Keep incident state durable and policy decisions pure; actual launches remain fenced by the existing session lock.
**Tech Stack:** Rust, clap, serde, registry/Wire and lifecycle store.
**Spec:** Ferris approved plan wire01a1ebc3-0d21 with approval01a1ebc5-4dea. A plan is docs/superpowers/plans/2026-11-17-codex-prerequisite-recovery.md.

## Global Constraints

- #539 local compile checks and exact source-audited pure filters ONLY. Full/live-capable/host/child/service/socket/store tests CI-only.
- No deployment/restart/terminal probes/reset/backup/restore/purge/activation/stage2 proof; no ui/ edits; no daemon auto-start.
- Two PRs: A #572 then B #573 based A. Native Windows/macOS CI green before landing.
- Keep HTTP restart behavior and serialized record/protocol compatibility. New state fields serde-default/empty omitted.
- All wire commands AGENT_PERSONA=fiona. Machine git identity and Fiona coauthor trailer.
- Windows broad check fails preexisting #560: compare base/head diagnostic sets rather than fix platform issues; core/hostproto/host GNU libs that pass must remain green.

## Review Focus

- CLI persona ambiguity and historical/decommissioning/non-hosted records must fail without changing unrelated records or authority.
- Six-launch slow ceiling must survive early deaths, missing prerequisites, hub restarts and failed persistence.
- Successful process spawn alone must not reset an incident; healthy10min must be observed.
- Manual/auto/decommission races must not start duplicate runtimes or overwrite current-generation state.
- Notifications must reach registry-role LabManager when Lead stopped/no Lead, without losing failed-recipient retries.

### Task B1: Saved-record CLI restart

**Files:** crates/cli/src/cmd/agent.rs, crates/hub/src/lifecycle.rs (shared strict CLI wrapper/selection), CLI isolated tests under crates/cli/tests/.
**Interfaces:** Add public lifecycle restart_agent_session<S>(store: &S, env: &LifecycleEnv<'_>, target: &str, by: &str, caller: Caller) -> Result<RestartResponse, LifecycleError> with existing store trait bounds. Add pure select_restart_target(records: &[HostedSessionRecord], target: &str) -> Result<String, LifecycleError> returning exact saved session id; no first-match ambiguity.
**Behavior:** fl agent restart <persona-or-session-id> [--by owner] [--json]. Explicit Caller::Owner from CLI, same adapter/environment setup as spin-up. No model/role/posture/config overrides. Reject non-Owner before effects; reject reserved automatic actor audit name from CLI. Preserve saved model/thinking/posture/cwd/node/context/project/role/recast rules. Exact id wins, persona selects unique eligible non-historical record, ambiguity refused with guidance to use exact id; missing/non-hosted/decommissioning/decommissioned/retired target refused. Revalidate CLI target status under session lock to close selection/decommission race. Keep existing HTTP/API prepared-record restart semantics: factor private restart options/body as needed rather than globally forbidding previously allowed HTTP operation.

- [x] Write pure parser/selection/authority regression tests first: defaultby, JSON, no override flags; exactidvsambiguouspersona; historical exclusion; forbidden statuses/authority no side effects.
- [x] Compile test-first RED for the missing parser/selection/authority interfaces (exit101); execute only audited exact pure filters after implementation. Store/runtime regressions stay compile-only locally.
- [x] Implement wrapper/strict target option and command handler returning existing RestartResponse JSON, concise text/warnings; no retyping settings.
- [x] Add isolated subprocess CLI regression proving saved model/thinking/posture/cwd/node/context and no override through arguments; CI-only under #539. Add fake lifecycle regression for decommission race refusal; CI-only if store/runtime dependent.
- [x] Run touched compile/check+strictclippy/alltargets, fmt/diff, audited exact pure filters each1pass; GNU core/hostproto/host libs. Record commands/exits/source audit. Never run a test just because its runtime is fake: store/helper suites remain CI-only.
- [x] Commit with Fiona trailer, clean status, report <=15lines. No push.
- [ ] Fresh per-task code-review; coordinator validates findings, fresh fixer/scoped review max3 rounds.

B1 validation: test-first compile failed on the missing interfaces (exit101); all-target compile and strict clippy passed. Four individually audited exact pure filters each executed and passed one test. Core/host-proto/host Windows GNU library checks passed. Store/lifecycle/subprocess regressions were compiled, and remain CI-only under #539.

### Task B2: Slow recovery and role-based notices

**Files:** crates/hub/src/lifecycle.rs; optional focused pure policy module crates/hub/src/lifecycle/recovery.rs; docs/vendor-runtimes.md; targeted module/CI tests.
**Interfaces:** Extend HostedRespawnState only with recovery_started_at_ms: Option<u64>, recovery_attempts: u8, recovery_healthy_since_ms: Option<u64>, crash_loop_notified_recipients: Vec<String>. Defaults0/None/empty, omit empty, update deletion predicate. Retain existing fields; reuse next_allowed_at_ms current phase deadline. Pure policy consumes explicit now and state; no clock/store/runtime work inside numeric policy.

- [ ] Write pure clock-driven policy tests before code: fast3in10min preserved; guard enters incident with firstdue+600000ms; old crashloop initializes now+600000ms; atmost6 actual recovery launches spaced>=600000ms across short-lived successes; missing prerequisite blocks uncharged; overdue reconciler makes one attempt; backward/forward clock changes no burst.
- [ ] Implement incident persistence before spawn under lock; prerequisite_unavailable retains slow incident and cannot bypass slow deadline. State-write failure prevents probe/launch where required. Reject manual/decommission/cleanexit/non-onfailure records. Do not refill fast budget after early successful launch; reset incident only after observed healthy running10min or explicit successful manual restart. Failed/refused manual restart preserves state.
- [ ] Add pure serialization old/new/empty-state and notice-recipient policy tests. Registry active project Lead normally; if affected persona is Lead or no Lead exists, active LabManager role(s) across registry, no hard-coded jarvis name/self-notice. Track successful recipient deliveries; retry absent/failed targets; ensure exhaustion message distinct from initial incident and legacy boolean cannot suppress new required fallback notice. Include next deadline/final-stop and manual command in notices.
- [ ] CI-only integrated tests: six early-death launches then final stopped, persisted hub restart/resume clocks, readiness interruption, healthy reset, manualsuccess/failure, failed writes, manual/auto/decommission races, notification partial failure/new Lead/no manager/selfLead. Do not execute local store/socket/child tests.
- [ ] Update docs and PR explaining today seven records were already manually recovered; feature is future bounded recovery. State no automatic daemon startup.
- [ ] Check/clippy touched alltargets, audited exact pure1pass each, formatting/diff, GNU differential proof as needed. Commit with Fiona trailer.
- [ ] Fresh code-review/fixer/scoped re-review max3rounds; final whole-branch fresh review.
- [ ] Keep B current with merged A/main. Push/fl github pr create, actual Linux full tests and native Windows/macOS CI, Ferris current-head approval, author merge. No deployment.
