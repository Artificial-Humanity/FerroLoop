# Codex launch prerequisite Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use subagent-dev for implementation and code-review for reviews/fixes. Steps use checkbox syntax.

**Goal:** Prevent missing Codex app-server prerequisites from spending hosted crash-loop budgets (#572).
**Architecture:** Add an adapter-owned, connect-only launch preflight. Lifecycle persists a distinct stopped prerequisite refusal with a durable 60-second readiness retry, with accounting and launch fenced by the existing per-session lock.
**Tech Stack:** Rust, serde, existing vendor adapter contracts and lifecycle store.
**Spec:** Ferris approved wire plan urn:uuid:01a1ebc3-0d21-7705-9193-507feda890cc with adjustments urn:uuid:01a1ebc5-4dea-7076-a246-29aa8ef18dcf. PR A is #572; separate PR B (#573) follows A.

## Global Constraints

- Base origin/main c17f210359d90fa64e64afe2ce13de38720383d0.
- #539: local compile checks and exact source-audited pure filters only; full/live-capable/host/child/service/store suites CI-only.
- No deployment, session/service restart, terminal probe, reset, backup/restore/purge/activation or stage2 proof.
- No Watch projection or UI edits. Felix owns those changes in PR #580 (#577); this branch supplies the raw hosted-record API fields with the agreed names.
- No daemon auto-start. Reference clean systemd user daemon environment, Before=ferroloop-hub.service, due #576.
- Keep serialized records compatible, no fl-host protocol/schema changes.
- Co-authored-by: Fiona <fiona@artificialhumanity.io>.

## Review Focus

- Probe endpoint must be the effective pinned CODEX_HOME launch endpoint; FL_WIRE_CODEX_CONTROL_SOCK delivery override must not falsely authorize launch.
- Stale socket files, unavailable platform and permission failures must refuse before launches.
- No attempt charged for prerequisite failure, including a dependency lost between outer and locked checks.
- No auto recovery of intentional stops/decommissioned/clean exits or duplicate runtime due manual/auto races.
- Persist deadline before retry; failed writes fail closed; bounded detail includes only socket path/probe result.

### Task A1: Adapter prerequisite and integrated lifecycle recovery

**Files:** core/src/vendor_adapter.rs (+exports as needed), vendor-adapters/src/codex.rs, hub/src/lifecycle.rs, hub/src/api.rs and exact affected Rust constructors/exhaustive matches; docs/vendor-runtimes.md. Paths under crates/ unless docs.
**Interfaces:** Add a default adapter preflight hook taking the explicit effective config home and returning typed prerequisite refusal code/detail. Keep prepare_launch deterministic. Generic lifecycle maps refusal to named HTTP409 error; retain existing JSON/error compatibility. Add only HostedSessionRecord.launch_prerequisite_detail: Option<String> (serde default and omitted None); stop_reason string value prerequisite_unavailable. Reuse HostedRespawnState.next_allowed_at_ms for 60-second prerequisite retry deadline. No #573 fields yet.

- [x] Write regressions first for old record deserialize, endpoint resolution including mismatch override, bounded detail, policy retry deadline and zero accounting.
- [x] Run RED only if test is audited pure; otherwise compile or defer RED runtime to CI and record limitation.
- [x] Implement adapter Codex connect-only preflight bounded250ms, cfg Unix; non-Unix named refusal. No daemon start/RPC/thread access. Missing adapter cannot bypass production Codex prerequisite check. Existing fake runtime tests require injected deterministic preflight, never ambient probing.
- [x] Wire preflight before launch effects for spinup/restart/remodel; same effective CODEX_HOME pinned into launch. Blocked automatic record stopped/prerequisite_unavailable with bounded detail. Reconcile retries at most once/session/60s, persists deadline, passes no attempt into fast budget, and revalidates under session lock before launching. Preserve intentional-stop exclusions, conflict/race behavior, no runtime calls when disabled. Failed state persistence prevents launch.
- [x] Add private-socket CI tests for absent/stale/refused/ready/timeout and isolated store/race/failed-persistence integration regressions. Keep tests in module tests or tests/ per AGENTS. Never execute live-capable tests locally.
- [x] Update docs with prerequisite/refusal/cadence, clean systemd user unit reference and Before ordering. No app-server auto-start option.
- [ ] Compile touched crates/all tests without execution, strict clippy, fmt/diff; Windows GNU cross-compile touched crates. Run only individually audited pure exact filters. Use persistent scratch targets/logs and bounded jobs; no /tmp.
- [ ] Commit task; report tests and exact source-audited filters, CI-only gaps. Do not push.
- [ ] Fresh read-only code-review, verified finding adjudication, fresh fixer/scoped re-review max3 rounds.

### Task A2: Final whole-branch review and publication

- [ ] Coordinator checks git cleanliness/trailer, all required compile results and source audit evidence.
- [ ] Fresh whole-branch review of c17f2103..HEAD; use code-review loop.
- [ ] Push and fl github pr create against main, report #539 local suite restriction and actual CI proof. Ferris reviews current head. Only author merge after approvals/checks and current main; no deployment.

### Follow-on PR B (#573), after A

Saved-record fl agent restart <persona-or-session-id> [--by owner] [--json] under Caller::Owner, no overrides, strict target selection, saved settings. Retain fast3/10min, then six recovery launches spaced10min; healthy10min or successful explicit manual restart resets incident. Unavailable prerequisite does not consume recovery launch; failed writes prevent launch. Old crash_loop records schedule first due now+10min; today's7 already recovered manually. Registry role-based notices with per-recipient state and retry. Add serde-default HostedRespawnState.recovery_started_at_ms: Option<u64>, recovery_attempts: u8, recovery_healthy_since_ms: Option<u64>, crash_loop_notified_recipients: Vec<String>; update empty-state deletion. Preserve existing fields. Separate task plan and code-review before PR B.

## A1 implementation evidence

Implemented adapter preflight, persisted prerequisite retry and raw hosted-session API fields. Per Ferris's scope adjustment (wire urn:uuid:01a1ebe6-91c5-73fe-b121-c86f0d30536f), this branch drops its Watch projection and associated test; Felix's PR #580 (#577) owns the Watch and UI changes. HostedSessionRecord.launch_prerequisite_detail and stop_reason prerequisite_unavailable remain the integration contract. Private-socket, fake-runtime lifecycle and isolated-store API regressions are authored for CI only under #539. The failed-save regression uses a scoped test-only respawn-save fault and checks zero prerequisite calls, runtime launches and accounting changes. Existing per-session lifecycle locks also fence reconciler classification writes.

Linux all-target compilation and strict clippy passed for fl-core/fl-hub/fl-cli/fl-vendor-adapters/fl-watch. Six independently audited exact pure filters initially executed one passing test each: endpoint choice, bounded detail, Watch JSON, retry policy, old-record compatibility and typed HTTP refusal. The Watch JSON test was removed with the projection scope transfer to PR #580. Lead review then replaced the endpoint-only assertion with an actual preflight/socket/isolated-subprocess regression; it is now CI-only, leaving four original pure filters. The new intentional-stop eligibility and bounded reservation filters are independently audited pure string/numeric policies. Store, process, service, socket and fake-runtime regressions receive local compile/lint checks only under #539.

Lead review remediation preserves intentional manual stop reasons on prerequisite refusal, charges other automatic preparation failures, and re-probes a failed Codex startup once to refund only that attempt if the prerequisite vanished. Diagnostics clear at every reason transition and before the first running persistence. Reconcile logs prerequisite refusals with its existing warning rate limit. The documentation uses systemd `restart` for the oneshot unit and describes all refusal entry points and the event.

Windows GNU broad touched-library compilation remains blocked by existing fl-exec/fl-store platform diagnostics under #560. fl-core library compilation passed; coordinator compares identical base/head broad diagnostics and verifies compiling host leaves separately. No #560 production changes are included. Native platform CI, fresh review and publication remain coordinator-owned.
