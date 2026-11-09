# Generation-Fenced Hydration Prompt Acceptance (Issue #406) — Approved Design

Status: Approved with amendments by `ferris` on 2026-11-09 (`urn:uuid:01a1bed8-57de-74ac-a830-2de3f1fc0e50`). Implementation is queued after #409 Part 2(b).

## Problem and Scope

Issue #383 D2b (`2026-11-09-hydration-outcome-design.md`) added `HydrationStatus::{Confirmed, Unconfirmed, Failed}` alongside `hydration_dispatched: bool`. Fresh hosted launches pass the hydration prompt to the vendor CLI via argv (`spin_up`, `restart_hosted_session`, `remodel` in `Execute` mode). Because process survival (`launch_survived`), PTY echo, and passive route discovery cannot prove that the vendor CLI accepted the prompt rather than stalling on a trust dialog, login gate, or slow startup, ordinary fresh launches start at `hydration_status: Unconfirmed`.

This design defines a bounded, generation-fenced confirmation signal using the launched session's own Step 6 `fl wire register ... --replace` call, without adding a new CLI subcommand, background readiness poller, vendor-specific UI, or whole-record overwrite race on `HostedSessionRecord`.

## Distinction: Prompt Acceptance, Process Registration, and Hydration Completion

- **`hydration_dispatched: bool`** means the hydration prompt was handed off to the vendor (argv spawn or transport delivery).
- **`hydration_status: HydrationStatus`** (`confirmed | unconfirmed | failed`) records **status evidence for the current hydration submission**, not completion of all startup hydration steps:
  - When a launched session runs `fl wire register <Name> --project <project> --role <role> --replace` carrying the current launch's generation token `G`, the resulting receipt proves:
    1. On the **Step 6 prompt channel** (used by all vendors, and the sole channel for `codex`): the session received the hydration prompt for generation `G` and executed Step 6.
    2. On the **runtime environment channel** (available only on per-session CLI processes: `claude`, `antigravity`, `muse`, `grok`): the launched process for generation `G` reached an active turn and ran `fl wire register --replace` (even if the agent omitted the inline env prefix from Step 6, this strictly proves the launched process ran `register --replace`, not that it parsed the prompt text).
  - In neither case does a receipt prove that subsequent steps (such as Step 7 `fl wire read --ack`) or every prior state read succeeded without error.
  - **Status-only boundary (Amendment A2):** A hydration receipt is status evidence for `hydration_status` only. Nothing may gate a permission, warrant, or structural authority on `hydration_status` or on `HydrationReceipt`.
  - **No automatic path writes a receipt (Amendment A2):** Vendor hooks (`fl wire hook claude|muse|grok`), passive route discovery (`scan_and_save_route_table`), implicit caller route capture (`resolve_caller_with_source`), and any startup registration never invoke `execute_register` with `replace = true` and a generation token `G`.
  - Neither `unconfirmed` nor `failed` ever triggers an automatic duplicate prompt send or escalates `OperatorPromptIntent::Ordinary` to `OwnerSendNow`.

## Generation Token and Confirmation Signal

### 1. Strictly Monotonic Per-Submission `hydration_generation` (Amendment A3)

Every fresh hydration submission in `spin_up` (`crates/hub/src/lifecycle.rs`), `restart_hosted_session`, and `remodel` (`RemodelMode::Execute`) mints a new generation token `G` through a single helper in `fl-hub` (`next_hydration_generation()`) **before** `env.runtime.spawn` or `deliver_prompt`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub hydration_generation: Option<String>,
```

- **Strict monotonicity (`next_hydration_generation()`):** The helper combines `now_ms()` with a process-global monotonic `AtomicU64` state (advancing the timestamp/counter pair whenever `now_ms()` ties or regresses) and formats a fixed-width, lowercase hex UUIDv7-shaped token so two generations minted within the same millisecond are strictly ordered (`G1 < G2` iff `G1` was minted before `G2` on the same host).
- **Comparison contract:**
  - **Exact equality** (`receipt.generation == record.hydration_generation`) decides confirmation (`HydrationStatus::Confirmed`).
  - **Lexical ordering** (`G_caller < G_record`) is used **only** for the stale-register refusal (Case 4), and `G` is compared only against values minted by `next_hydration_generation()` on the same host.
- `HostedSessionRecord` stores `hydration_generation: Some(G)` for the active submission.
- `decommission` (`Prepare` or `Force`) and `remodel` (`RemodelMode::Prepare`, which sends a decommission handoff prompt rather than a hydration prompt) set `hydration_generation: None`.
- Legacy `HostedSessionRecord` JSON without `hydration_generation` deserializes to `None`.

### 2. Channel Binding on `fl wire register --replace` (Amendment A1)

Step 6's `fl wire register ... --replace` is bound to `G` without adding a new CLI command:

1. **Step 6 prompt channel (all vendors, and the sole channel for `codex`):**
   When `spin_up`, `restart_hosted_session`, or `remodel` (`Execute`) synthesizes the hydration prompt for generation `G`, Step 6 prefixes `AGENT_HYDRATION_GENERATION=<G>` alongside `AGENT_PERSONA=<name_lower>`:
   ```bash
   AGENT_PERSONA=<name_lower> AGENT_HYDRATION_GENERATION=<G> fl wire register <Name> --project <project> --role <role> --replace
   ```
   Callers of `canonical_reforge_prompt` that do not mint a launch generation (such as static preview endpoints) keep the existing Step 6 text unchanged.
   - **Codex constraint (Amendment A1):** Codex sessions share a single `codex` app-server daemon (`launch_env_for_vendor` in `crates/wire/src/vendor_adapter.rs` already omits `AGENT_PERSONA` for `codex` so a shared app-server does not leak one persona's env into another). Therefore, the runtime-env channel does **not** reach Codex tool subshells and must **not** be injected into `codex` launch environments. Codex relies exclusively on the Step 6 prompt channel (`AGENT_HYDRATION_GENERATION=<G>` in the Step 6 command).
2. **Spawned runtime environment channel (per-session CLI vendors only: `claude`, `antigravity`, `muse`, `grok`):**
   For non-`codex` hosted launches, `HostRuntime::spawn` and `TmuxRuntime::spawn` also pass `AGENT_HYDRATION_GENERATION=<G>` in the spawned session environment (and `AGENT_HYDRATION_GENERATION` is added to `BASELINE_EXACT` in `crates/core/src/child_env.rs`, avoiding `NEVER_PREFIXES = &["FL_", "FERROGATE_"]`).
3. **Strict signal qualification in `execute_register` (`crates/cli/src/cmd/wire.rs`):**
   A hydration receipt is written **only** when all of the following hold:
   - `fl wire register` is invoked with `--replace` (`replace == true`).
   - `AGENT_HYDRATION_GENERATION` is present in the process environment and is a valid non-empty generation token `G` matching the `next_hydration_generation()` format.
   - If `FL_HOSTED_ID` is present in the process environment, its `<vendor>:<persona>` matches the registered persona.
   - An explicit `fl wire register --replace` without `AGENT_HYDRATION_GENERATION` (for example, an operator or unmanaged shell) keeps today's registration behavior and writes no hydration receipt.

## Cross-Process Fencing and Receipt Storage (No CAS on `HostedSessionRecord`)

`HostedSessionRecord` is stored at workspace runtime slug `hosted-session-<vendor>-<persona>` using `MergeMode::LastWriter` (`crates/hub/src/lifecycle.rs:2132`), and `UpsertStateArgs` (`crates/core/src/state.rs:435`) has no compare-and-swap (`expected_revision`). If `fl wire register` read-modified-wrote `hosted-session-<vendor>-<persona>` directly, it would race with lifecycle persistence (`spin_up` / `restart_hosted_session` post-spawn save) and concurrent operator mutations (`set_hosted_session_model`, `set_hosted_session_posture`, `reconcile_hosted_sessions`).

### Rule 1: Disjoint Writer Slugs (`hosted-session-*` vs. `hydration-ack-*`)

- `fl wire register --replace` **never** writes `hosted-session-<vendor>-<persona>`.
- Instead, when `replace == true` and `AGENT_HYDRATION_GENERATION=<G>` is accepted, `execute_register` writes a separate workspace runtime entry at slug `hydration-ack-<G>`:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  pub struct HydrationReceipt {
      pub session_id: String,
      pub persona: String,
      pub generation: String,
      pub accepted_at_ms: u64,
  }
  ```
- Because `<G>` is unique per launch submission, `hydration-ack-<G>` has no write contention with `hosted-session-<vendor>-<persona>`.

### Rule 2: Single-Pass Read Projection in `list_hosted_sessions` (Amendment A5)

- `list_hosted_sessions` (`crates/hub/src/lifecycle.rs:2155`) is the single loader used by every hosted-record read path (`find_hosted_session`, `set_hosted_session_model`, `set_hosted_session_posture`, `reconcile_hosted_sessions`, `GET /api/v1/sessions`, `fl agent ls`).
- `list_hosted_sessions` already executes a single `store.list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))` call. In that same pass, it partitions runtime entries by slug prefix:
  - `hosted-session-*` -> `HostedSessionRecord`
  - `hydration-ack-*` -> `HydrationReceipt`, indexed by `(persona_lower, generation)` (and `session_id` when non-empty).
- For each `HostedSessionRecord`, if:
  - `record.hydration_status == HydrationStatus::Unconfirmed`, and
  - `record.hydration_generation.as_deref() == Some(G)`, and
  - a `HydrationReceipt` exists with exact equality `receipt.generation == G`, matching `persona` (case-insensitive), and compatible `session_id` (empty or equal to `record.session_id`),
  then `list_hosted_sessions` projects `record.hydration_status = HydrationStatus::Confirmed`.
- Because `find_hosted_session` and all read-modify-write helpers (`set_hosted_session_model`, `set_hosted_session_posture`) load records through `list_hosted_sessions`, they automatically observe and preserve `HydrationStatus::Confirmed` with **zero additional store queries**.

### Rule 3: Post-Spawn Lifecycle Check and Receipt Pruning (Amendment A5)

- In `spin_up`, `restart_hosted_session`, and `remodel` (`Execute`), immediately before the post-spawn `persist_hosted_session(store, &record, by)` call, if `record.hydration_status == HydrationStatus::Unconfirmed` and `record.hydration_generation.as_deref() == Some(G)`, the lifecycle function checks whether `hydration-ack-<G>` already exists for `(record.persona, G)`.
- If the receipt arrived while `spawn` / `launch_survived` was running, `record.hydration_status` (and the returned `SpinUpResponse` / `RestartResponse` / `RemodelResponse`) is promoted to `HydrationStatus::Confirmed` before `persist_hosted_session` writes the record.
- **Bounded pruning on persist and decommission (Amendment A5):** Whenever `persist_hosted_session` saves a `HostedSessionRecord` (including during `spin_up`, `restart_hosted_session`, `remodel`, and `decommission`), it deletes any `hydration-ack-*` runtime entries in `StateScope::Workspace` for `record.persona` whose `receipt.generation.as_str() != record.hydration_generation.as_deref().unwrap_or("")`. On `decommission` (`hydration_generation: None`), all `hydration-ack-*` receipts for that persona are removed. Across any number of restarts (e.g., 70 consecutive restarts), at most one `hydration-ack-*` entry exists per persona.

## Ordering and Lifecycle Cases

### 1. Acknowledgement Before Lifecycle Persistence Completes

- **Scenario:** `spin_up` or `restart_hosted_session` mints `G1` via `next_hydration_generation()` and spawns the vendor process. While `launch_survived` or `apply_startup_prompt_rules` is still running—and in `restart_hosted_session`, before `persist_hosted_session` at line 5203 has written `G1` to `hosted-session-*`—the child runs `fl wire register ... --replace` with `AGENT_HYDRATION_GENERATION=G1`.
- **Handling:**
  - `execute_register` inspects the current live `HostedSessionRecord` (if any) for `persona`. Because `G1` is newer than any prior record's `G0` (`G1 > G0` by `next_hydration_generation()`), `execute_register` accepts the registration and writes `hydration-ack-<G1>`.
  - Neither `execute_register` nor `persist_hosted_session` overwrites the other's slug.
  - Rule 3 promotes `record.hydration_status` to `Confirmed` when `persist_hosted_session` runs (and prunes only receipts with `generation != G1`, preserving `hydration-ack-<G1>`), and Rule 2 projects `Confirmed` on all subsequent reads.

### 2. Delayed Startup

- **Scenario:** The vendor CLI takes several seconds to initialize MCP servers or awaits operator release of a startup dialog before processing its argv prompt.
- **Handling:**
  - `spin_up` / `restart_hosted_session` finishes `launch_survived` and returns `hydration_dispatched: true`, `hydration_status: Unconfirmed`, `hydration_generation: Some(G1)`. No background poller is spawned and no prompt is resent.
  - When the session later processes the prompt and executes Step 6 (`fl wire register ... --replace`), `hydration-ack-<G1>` is persisted.
  - All subsequent `list_hosted_sessions` / `find_hosted_session` reads project `hydration_status: Confirmed`.

### 3. Duplicate Acknowledgements

- **Scenario:** The same session runs `fl wire register ... --replace` with `AGENT_HYDRATION_GENERATION=G1` multiple times during its lifetime.
- **Handling:**
  - If `hydration-ack-<G1>` already exists for `(persona, G1)`, `execute_register` preserves the existing receipt (keeping the original `accepted_at_ms`) and returns success (`Ok(0)`).
  - Read projection remains `HydrationStatus::Confirmed`.

### 4. Supersession by `restart`, `remodel`, `spin_up`, or `decommission` (Amendment A4)

- **Scenario:** Launch 1 starts with `G1`. Before or after `G1` is acknowledged, a `restart`, `remodel` (`Execute`), or re-`spin_up` launches the persona with `G2` (`G2 > G1` from `next_hydration_generation()`), or `decommission` stops the session (`status: decommissioned` / `decommissioning`, `hydration_generation: None`). A lingering subshell from Launch 1 later runs `fl wire register ... --replace` with `AGENT_HYDRATION_GENERATION=G1`.
- **Handling:**
  - **Confirmation fencing:** Rule 2 checks exact equality `record.hydration_generation == Some(receipt.generation)`. Because the active record has `Some(G2)` (or `None`), a receipt for `G1` never confirms `G2`.
  - **Narrow stale-register refusal (Amendment A4):**
    - Registrations **without** `AGENT_HYDRATION_GENERATION` keep today's behavior in full and are never refused by generation fencing.
    - When `AGENT_HYDRATION_GENERATION=G_caller` **is** present on `fl wire register`, `execute_register` checks the persona's hosted record:
      1. If a record for `persona` has `status` in `["decommissioning", "decommissioned"]`, OR
      2. If a record for `persona` has `hydration_generation: Some(G_record)` where `G_caller < G_record`,
      then `execute_register` refuses the registration before touching the agent registry, route table, or `hydration-ack-*`, returning an error that names `persona`, states that the session was superseded by a newer launch, and instructs this process to exit:
      ```text
      registration refused for `<persona>`: session launch generation `<G_caller>` was superseded by a newer launch; this process should exit
      ```

## UI Freshness, Amendments A7–A8, and Orthogonal Follow-Up Notes (Amendments A6–A8)

- **FerroDeck UI freshness (Amendment A6):** Because `hydration-ack-<G>` is written by the `fl` CLI process (`fl wire register --replace`) rather than inside `fl-hub`, the hub does not emit an immediate push event when the receipt lands. FerroDeck displays `hydration_status: confirmed` on its next hosted-sessions fetch. This is an accepted limitation; no new poller or timer is added.
- **Amendment A7 (ruled by `ferris` 2026-11-09 — Multi-vendor persona scoping):** When a persona runs on multiple vendors simultaneously, `fence_hydration_register` refuses only when `!has_matching_live && (superseded_by_newer || all_matching_decommissioned)`, and `prune_stale_hydration_receipts` retains receipts for every generation held by any non-decommissioned record of that persona. Also, `execute_register` ignores `AGENT_HYDRATION_GENERATION` when `AGENT_PERSONA` or `FL_HOSTED_ID` names a different persona than the target being registered.
- **Amendment A8 (ruled by `ferris` 2026-11-09 — Reuse generation when no new process is spawned):** When `spin_up` or `restart_hosted_session` does not spawn a new runtime process (e.g., `adopted_old_session`, existing runtime unit, or prompt-only delivery), it retains the record's existing valid `hydration_generation` (minting only if the record has none) so the live process's environment token is not superseded.
- **Known Limitation (M7):** Under the existing no-CAS `MergeMode::LastWriter` model on `HostedSessionRecord`, a concurrent setter (`set_hosted_session_model`, `set_hosted_session_posture`) that writes back a pre-restart snapshot can transiently write a stale `hydration_status` until the next lifecycle write.
- **Durable operator warning history:** Per the comment on #406 regarding #383 D6+D5, automatic boot/host-restart guidance warnings are returned on `spin-up` / `remodel` / `restart` responses and logged by the hub, not stored on `HostedSessionRecord`. `HydrationReceipt` remains single-purpose and bounded to at most one active receipt per live hosted session.

## Verification Plan (Once #409 Part 2(b) Lands)

1. **Monotonic generation helper unit test (`crates/hub/src/lifecycle.rs`, Amendment A3):**
   - Repeated calls to `next_hydration_generation()` (including simulated same-millisecond calls) produce strictly increasing tokens (`g1 < g2 < ...`).
2. **Channel binding & Codex path tests (`crates/hub/src/lifecycle.rs` & `crates/wire/src/vendor_adapter.rs`, Amendment A1):**
   - `codex` launch env omits `AGENT_HYDRATION_GENERATION` (matching `AGENT_PERSONA`), while `claude`, `antigravity`, `muse`, and `grok` receive it.
   - End-to-end Codex `spin_up` test where executing the Step 6 command from `SpinUpResponse.prompt` (`AGENT_PERSONA=... AGENT_HYDRATION_GENERATION=<G> fl wire register ... --replace`) transitions `hydration_status` from `unconfirmed` to `confirmed`.
3. **No automatic receipt paths (`crates/cli/src/cmd/wire.rs`, Amendment A2):**
   - Vendor hooks (`fl wire hook claude`, `fl wire hook muse`, `fl wire hook grok`), passive route discovery (`scan_and_save_route_table`), and implicit route capture (`resolve_caller_with_source`) never write `hydration-ack-*`, even if `AGENT_HYDRATION_GENERATION` is set in their environment.
4. **Ordering, projection, and 70-restart pruning tests (`crates/hub/src/lifecycle.rs` & `crates/cli/src/cmd/wire.rs`, Amendments A4 & A5):**
   - Early ack (`hydration-ack-<G>` written before post-spawn `persist_hosted_session`) promotes `SpinUpResponse` / `RestartResponse` and stored record to `Confirmed` without clobbering `runtime_spawned` or `restart_count`.
   - Delayed ack projects `HydrationStatus::Confirmed` through `list_hosted_sessions`, through `find_hosted_session`, and across read-modify-write helpers (`set_hosted_session_model`, `set_hosted_session_posture`).
   - Duplicate `fl wire register --replace` with `G` is idempotent and preserves `accepted_at_ms`.
   - Stale generation (`G_caller < G_record` or decommissioned record) with `AGENT_HYDRATION_GENERATION` is refused with the persona-named exit message and leaves routes untouched; registration **without** `AGENT_HYDRATION_GENERATION` succeeds as today.
   - Running 70 consecutive `restart_hosted_session` cycles (with receipts written along the way) leaves at most one `hydration-ack-*` entry in the store, and `decommission` leaves zero.
5. **Black-box CLI test (`crates/cli/tests/host_lifecycle.rs`):**
   - Hosted session that runs `fl wire register ... --replace` transitions from `unconfirmed` to `confirmed` without a duplicate prompt send.
