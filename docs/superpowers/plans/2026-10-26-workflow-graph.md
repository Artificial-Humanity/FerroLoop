# Workflow Graph (`fl flow`) and Author-Owned PR Merge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver Sub-project 4 (`fl flow`): a directed workflow graph engine with conditional loops, parallel fan-out/fan-in joins, review-independence enforcement, per-step gate verification, curated step handoffs (`StateScope::Workflow`), `.fl/flows/<slug>.toml` sharing, `fl/state` branch synchronization, `fl flow` CLI and `fl state mcp` tools, plus author-owned pull-request merge authorization (`authorize_agent_merge`).

**Architecture:** We layer the workflow graph across the existing four crates: (1) `fl-core` holds the pure workflow graph types (`WorkflowDef`, `StepDef`, `EdgeDef`, `WorkflowRun`, `StepInstance`), static graph validator (`validate_workflow_def`), deterministic step scheduler (`advance_workflow`), review-independence agent selector (`select_step_agent`), tiered sign-off policy (`authorize_step_sign_off`), `WorkflowStore` trait, and `authorize_agent_merge`; (2) `fl-store` persists workflow definitions and runs in `RedbStore` at `FORMAT_WITH_WORKFLOW = 8` alongside `MemStore` conformance tests; (3) `fl-exec` provides `.fl/flows/<slug>.toml` export/import, curated `HandoffEntry` and `declared_keys` runtime state, adapter/gate step execution (`execute_workflow_step`), and `fl/state` git synchronization of `workflows/` and `workflow_runs/`; (4) `fl-cli` wires `fl flow` subcommands, `fl state mcp` workflow tools, and `authorize_agent_merge` in `fl github pr merge`.

**Tech Stack:** Rust 2024 (`1.98`), `serde`, `serde_json`, `toml`, `redb` (`4.3`), `clap` (`4.6`), `tokio`, `sha2`, `uuid` (v7).

**Spec:** `docs/superpowers/specs/2026-10-26-workflow-graph-design.md`

## Global Constraints

- `pub const FORMAT_WITH_WORKFLOW: u64 = 8;` in `crates/store/src/lib.rs`; writing the first workflow definition or workflow run raises `META["format_version"]` to `8` in the same write transaction, while `RedbStore::open` accepts formats `2..=FORMAT_WITH_WORKFLOW`.
- `Kind::Workflow` (wire `"workflow"`) and `Kind::WorkflowRun` (wire `"workflow_run"`) in `crates/core/src/ids.rs`, routed to `self.catalog` in `KindRouted::owner` (`crates/core/src/store.rs`), with full `urn:uuid:...` IRIs on the wire and per-kind local numeric handles (`1`, `2`, …).
- Every stored reference field in `WorkflowDef`, `StepDef`, `StepInstance`, and `WorkflowRun` (`WorkflowId`, `WorkflowRunId`, `ProjectId`, `GateId`, `RecordId`, `Iri`) must serialize as a full IRI and be covered by `crates/core/tests/wire_refs.rs`.
- `validate_workflow_def` must reject empty step gate lists (`StepCompletion::Gated { gates, .. }` with `gates.is_empty()`), self-review (`reviews_step == Some(step_id)`), invalid `JoinPolicy::NOfM` thresholds, invalid slugs, and any step reachable from `start_step` that cannot reach `"END"`.
- `advance_workflow` must preserve completed sibling steps on in-step retries (`attempt_count < max_attempts`), escalate to `WorkflowRunStatus::NeedsHuman` (`State::NeedsHuman`) when `max_visits`, `max_total_steps`, `budget_usd_micros`, or `max_tokens` ceilings are exceeded, and evaluate `JoinPolicy::{All, Any, NOfM}` deterministically.
- `select_step_agent` and `authorize_step_sign_off` must forbid assigning or approving a review/sign-off step with the same agent that executed the reviewed/current step (`SelfApprovalForbidden` / `ReviewIndependenceViolation::SameAgent`, CLI exit code `2`), and enforce `require_different_vendor` when `true`.
- Never pass raw vendor JSONL transcripts across workflow steps; `build_step_instruction` assembles only `step_def.prompt`, predecessor `HandoffEntry` summaries/blockers/next_steps, and declared `StateScope::Workflow` runtime facts (`def.declared_keys`).
- `authorize_agent_merge` in `crates/core/src/pr_policy.rs` allows a recorded commit `co_author` in `eval.co_authors` to merge their own pull request across all tiers once `verify_merge_approvals` succeeds for `head_sha`, while `authorize_agent_action` continues to forbid self-review on `fl github pr review` and inside `verify_merge_approvals`.
- Prefix all `cargo` commands with `export PATH="$HOME/.cargo/bin:$PATH"`.
- Include `Co-authored-by: Ferris <ferris@artificialhumanity.io>` in every commit message.

---

### Task 1: Pure Workflow Graph Model, Scheduler, Sign-Off Policy, and `authorize_agent_merge` (`crates/core`)

**Files:**
- Modify: `crates/core/src/ids.rs`
- Modify: `crates/core/src/store.rs`
- Create: `crates/core/src/workflow.rs`
- Modify: `crates/core/src/pr_policy.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/core/tests/wire_refs.rs`

**Interfaces:**
- Consumes: `Iri`, `Kind`, `ProjectId`, `GateId`, `RecordId`, `At`, `State`, `Regret`, `MergeMode`, `AgentRecord`, `AgentRole`, `AgentStatus`, `validate_slug`, `PrPolicyEvaluation`, `PolicyRefusal`.
- Produces:
  - `Kind::Workflow` (`"workflow"`), `Kind::WorkflowRun` (`"workflow_run"`), `WorkflowId`, `WorkflowRunId` in `crates/core/src/ids.rs`
  - `StoreError::{NoSuchWorkflow, NoSuchWorkflowRun, InvalidWorkflow}` and `pub trait WorkflowStore` in `crates/core/src/store.rs`
  - `crates/core/src/workflow.rs`: `END_STEP`, `DEFAULT_MAX_TOTAL_STEPS`, `DEFAULT_STEP_MAX_ATTEMPTS`, `DEFAULT_STEP_MAX_VISITS`, `ApprovalTier`, `AssigneeRule`, `StepCompletion`, `JoinPolicy`, `StepDef`, `EdgeCondition`, `EdgeDef`, `WorkflowDef`, `WorkflowRunStatus`, `StepOutcome`, `StepInstance`, `WorkflowRun`, `StepAttemptReport`, `SelectedStepAgent`, `ReviewIndependenceViolation`, `WorkflowError`, `validate_workflow_def`, `validate_reviewer_independence`, `select_step_agent`, `authorize_step_sign_off`, `advance_workflow`
  - `authorize_agent_merge(eval: &PrPolicyEvaluation, caller: &AgentRecord) -> Result<(), PolicyRefusal>` in `crates/core/src/pr_policy.rs`

- [ ] **Step 1: Add `Kind::Workflow`, `Kind::WorkflowRun`, `WorkflowId`, and `WorkflowRunId` in `crates/core/src/ids.rs` and `crates/core/src/store.rs`**

In `crates/core/src/ids.rs`, add `Workflow` and `WorkflowRun` to `Kind`, `wire_names!`, and `id_type!`:

```rust
pub enum Kind {
    Project,
    Gate,
    Record,
    Finding,
    State,
    Agent,
    Message,
    Workflow,
    WorkflowRun,
}

crate::wire::wire_names!(Kind as kind_wire {
    Project => "project",
    Gate => "gate",
    Record => "record",
    Finding => "finding",
    State => "state",
    Agent => "agent",
    Message => "message",
    Workflow => "workflow",
    WorkflowRun => "workflow_run",
});
```

And at the bottom of `id_type!` invocations in `crates/core/src/ids.rs`:

```rust
id_type!(WorkflowId, Kind::Workflow);
id_type!(WorkflowRunId, Kind::WorkflowRun);
```

In `crates/core/src/store.rs`:
1. Import `WorkflowId, WorkflowRunId` from `crate::ids` and `WorkflowDef, WorkflowRun` from `crate::workflow`.
2. Add variants to `StoreError`:
```rust
    #[error("no such workflow: {0}")]
    NoSuchWorkflow(String),
    #[error("no such workflow run: {0}")]
    NoSuchWorkflowRun(String),
    #[error("invalid workflow operation: {0}")]
    InvalidWorkflow(String),
```
3. Route `Kind::Workflow | Kind::WorkflowRun` in `KindRouted::owner`:
```rust
        match kind {
            Kind::Project
            | Kind::Gate
            | Kind::State
            | Kind::Agent
            | Kind::Message
            | Kind::Workflow
            | Kind::WorkflowRun => self.catalog,
            Kind::Record | Kind::Finding => self.tracker,
        }
```
4. Define `WorkflowStore` in `crates/core/src/store.rs`:
```rust
/// Directed workflow definitions and stateful workflow runs (workflow spec §2.1).
pub trait WorkflowStore {
    fn upsert_workflow_def(&self, def: WorkflowDef) -> Result<WorkflowDef, StoreError>;
    fn get_workflow_def(&self, id: &WorkflowId) -> Result<Option<WorkflowDef>, StoreError>;
    fn get_workflow_def_by_slug(
        &self,
        project: &ProjectId,
        slug: &str,
    ) -> Result<Option<WorkflowDef>, StoreError>;
    fn list_workflow_defs(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<WorkflowDef>, StoreError>;

    fn create_workflow_run(&self, run: WorkflowRun) -> Result<WorkflowRun, StoreError>;
    fn update_workflow_run(&self, run: WorkflowRun) -> Result<WorkflowRun, StoreError>;
    fn get_workflow_run(&self, id: &WorkflowRunId) -> Result<Option<WorkflowRun>, StoreError>;
    fn list_workflow_runs(
        &self,
        project: Option<&ProjectId>,
        workflow_id: Option<&WorkflowId>,
    ) -> Result<Vec<WorkflowRun>, StoreError>;
}
```

- [ ] **Step 2: Create `crates/core/src/workflow.rs` with full types, validation, scheduler, agent selection, sign-off policy, and unit tests**

Create `crates/core/src/workflow.rs` implementing:
- Constants:
  - `pub const END_STEP: &str = "END";`
  - `pub const DEFAULT_MAX_TOTAL_STEPS: u32 = 64;`
  - `pub const DEFAULT_STEP_MAX_ATTEMPTS: u32 = 1;`
  - `pub const DEFAULT_STEP_MAX_VISITS: u32 = 5;`
- Enums and structs with `wire_names!`/`wire_parse!` where appropriate:
  - `ApprovalTier { Owner, LabManager, Lead }` (`#[default] Owner`, `wire_names!` `"owner"`, `"lab_manager"`, `"lead"`)
  - `AssigneeRule { Agent { name: String }, Selector { role: Option<AgentRole>, area: Option<String>, adapter: Option<String> }, Manual }`
  - `StepCompletion { Gated { gates: Vec<GateId>, regret: Regret }, HumanSignOff { approval_tier: ApprovalTier }, Ungated }`
  - `JoinPolicy { All, Any, NOfM { required: u32 } }`
  - `StepDef`, `EdgeCondition { Pass, Fail, NeedsHuman, Always }`, `EdgeDef`, `WorkflowDef`
  - `WorkflowRunStatus { Running, NeedsHuman, Completed, Failed, Canceled }` (`wire_names!` + `Display`, `is_terminal(&self) -> bool`)
  - `StepOutcome { Pass, Fail, NeedsHuman }` (`wire_names!` + `Display`)
  - `StepInstance`, `WorkflowRun`, `StepAttemptReport`, `SelectedStepAgent`
  - `ReviewIndependenceViolation { SameAgent { step_id: String, reviews_step: String, agent: String }, SameVendor { step_id: String, reviews_step: String, vendor: String } }`
  - `WorkflowError` covering all validation, scheduling, independence, undeclared key, and sign-off refusal variants (`InvalidSlug`, `EmptyTitle`, `EmptySteps`, `ReservedStepId`, `DuplicateStepId`, `EmptyStepTitle`, `UnknownStartStep`, `InvalidStepLimits`, `UnknownEdgeStep`, `InvalidReviewTarget`, `InvalidJoinThreshold`, `InvalidDeclaredKey`, `EmptyStepGates`, `UnreachableEnd`, `UnknownStep`, `StepNotActive`, `RunNotRunning`, `ReviewIndependence`, `NoEligibleAgent`, `SelfApprovalForbidden`, `InsufficientApprovalAuthority`, `UndeclaredWorkflowKey`).
- `validate_workflow_def(def: &WorkflowDef) -> Result<(), WorkflowError>` enforcing all 10 static checks from spec §1.3 (including BFS/reverse-BFS reachability from `start_step` to ensure every step reachable from `start_step` has a directed path to `END_STEP`).
- `WorkflowRun::new(id: WorkflowRunId, def: &WorkflowDef, title: impl Into<String>, at: At, by: impl Into<String>) -> Result<Self, WorkflowError>` initializing all steps to `State::Todo`, setting `start_step` to `visit_count = 1` and putting `start_step` in `active_steps`.
- `validate_reviewer_independence(def: &WorkflowDef, run: &WorkflowRun, step_id: &str, candidate_agent: &str, candidate_adapter: Option<&str>) -> Result<(), WorkflowError>`
- `select_step_agent(def: &WorkflowDef, run: &WorkflowRun, step_id: &str, agents: &[AgentRecord], override_adapter: Option<&str>) -> Result<Option<SelectedStepAgent>, WorkflowError>`
- `authorize_step_sign_off(project_slug: &str, step_def: &StepDef, step_instance: &StepInstance, caller: &str, agents: &[AgentRecord], owner_login: &str) -> Result<(), WorkflowError>`
- `advance_workflow(def: &WorkflowDef, run: &mut WorkflowRun, report: StepAttemptReport, at: At) -> Result<(), WorkflowError>` implementing spec §1.4 (accounting, ceilings, in-step retries preserving completed siblings, `HumanSignOff` / `NeedsHuman` handling, outgoing edge matching, `JoinPolicy::{All, Any, NOfM}` fan-in, `max_visits` cap, and `Completed`/`Failed` status transitions).
- Comprehensive `#[cfg(test)] mod tests` in `crates/core/src/workflow.rs` testing:
  1. `validate_workflow_def_accepts_valid_loop_and_fan_out_graphs_and_rejects_invalid_shapes` (duplicate step ID, missing start step, self-review `reviews_step`, empty gate list in `StepCompletion::Gated`, invalid `NOfM` threshold, invalid declared key, dead-end step unable to reach `"END"`).
  2. `advance_workflow_executes_review_fix_review_loop_and_enforces_max_visits_cap`.
  3. `advance_workflow_in_step_retry_preserves_completed_parallel_siblings_and_fans_in`.
  4. `advance_workflow_join_policies_any_and_n_of_m_activate_join_step_at_threshold`.
  5. `advance_workflow_escalates_to_needs_human_on_step_and_workflow_budget_token_and_step_ceilings`.
  6. `select_step_agent_and_validate_reviewer_independence_enforce_author_and_vendor_separation`.
  7. `authorize_step_sign_off_forbids_self_approval_and_enforces_lead_lab_manager_and_owner_tiers`.

- [ ] **Step 3: Add `authorize_agent_merge` in `crates/core/src/pr_policy.rs` and re-export in `crates/core/src/lib.rs`**

In `crates/core/src/pr_policy.rs`, add `authorize_agent_merge` right after `authorize_agent_action`:

```rust
/// Verify whether `caller` is authorized to merge a pull request with policy `eval`
/// once required approvals (`verify_merge_approvals`) are satisfied.
///
/// Under the Author-Owned Merge rule (spec §4.3), a registered active co-author in
/// `eval.co_authors` may merge their own pull request across all tiers (`Lead`,
/// `LabManager`, and `Owner`-reviewed PRs) once the required reviewer tier has
/// approved `head_sha`. Non-co-author reviewers meeting `authorize_agent_action`
/// are also accepted.
pub fn authorize_agent_merge(
    eval: &PrPolicyEvaluation,
    caller: &AgentRecord,
) -> Result<(), PolicyRefusal> {
    if caller.status != AgentStatus::Active {
        return Err(PolicyRefusal::UnknownOrInactiveAgent {
            agent: caller.name_key.clone(),
        });
    }
    if !eval.unattributed_commits.is_empty() {
        return Err(PolicyRefusal::UnattributedCommitsRequireOwner {
            commits: eval.unattributed_commits.clone(),
        });
    }
    if !eval.unknown_co_authors.is_empty() {
        return Err(PolicyRefusal::UnknownCoAuthorsRequireOwner {
            co_authors: eval.unknown_co_authors.clone(),
        });
    }
    if eval
        .co_authors
        .iter()
        .any(|c| c.eq_ignore_ascii_case(&caller.name_key))
    {
        return Ok(());
    }
    authorize_agent_action(eval, caller)
}
```

Add a unit test `author_owned_merge_allows_co_author_across_tiers_while_forbidding_self_review` in `crates/core/src/pr_policy.rs::tests`.

Update `crates/core/src/lib.rs` to declare `pub mod workflow;` and re-export `WorkflowId`, `WorkflowRunId`, `WorkflowStore`, `authorize_agent_merge`, and the public types/functions from `workflow`.

Update `crates/core/tests/wire_refs.rs` to test `WorkflowDef` and `WorkflowRun` with `assert_every_reference_refuses_a_handle` and add `workflow_wire_types_round_trip_cleanly`.

- [ ] **Step 4: Run `fl-core` tests**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test -p fl-core`
Expected: PASS (all unit tests and `wire_refs` tests green).

- [ ] **Step 5: Commit Task 1**

```bash
git add crates/core/src/ids.rs crates/core/src/store.rs crates/core/src/workflow.rs crates/core/src/pr_policy.rs crates/core/src/lib.rs crates/core/tests/wire_refs.rs
git commit -m "feat(core): add workflow graph model, scheduler, and author-owned merge policy

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

### Task 2: `WorkflowStore` Persistence in `MemStore` and `RedbStore` (`FORMAT_WITH_WORKFLOW = 8`) (`crates/core/src/mem.rs`, `crates/store`)

**Files:**
- Modify: `crates/core/src/mem.rs`
- Modify: `crates/core/src/conformance.rs`
- Modify: `crates/store/src/lib.rs`
- Create: `crates/store/src/workflow.rs`

**Interfaces:**
- Consumes: `WorkflowStore`, `WorkflowDef`, `WorkflowRun`, `WorkflowId`, `WorkflowRunId`, `validate_workflow_def` from Task 1.
- Produces:
  - `impl WorkflowStore for MemStore` in `crates/core/src/mem.rs`
  - `pub const FORMAT_WITH_WORKFLOW: u64 = 8;` in `crates/store/src/lib.rs`
  - `impl WorkflowStore for RedbStore` in `crates/store/src/workflow.rs`
  - `pub fn workflow_store<S, G>(make: impl Fn() -> (S, G))` conformance suite in `crates/core/src/conformance.rs`

- [ ] **Step 1: Implement `WorkflowStore` for `MemStore` in `crates/core/src/mem.rs`**

In `crates/core/src/mem.rs`:
1. Add to `Inner`:
```rust
    workflow_defs: BTreeMap<WorkflowId, WorkflowDef>,
    workflow_by_slug: BTreeMap<(ProjectId, String), WorkflowId>,
    workflow_runs: BTreeMap<WorkflowRunId, WorkflowRun>,
```
2. Implement `WorkflowStore for MemStore`:
   - `upsert_workflow_def`:
     - Calls `validate_workflow_def(&def).map_err(|e| StoreError::InvalidWorkflow(e.to_string()))?`.
     - Verifies `def.project` is owned as `Kind::Project` in `inner.owned` (or returns `NotOwned` / `WrongKind`).
     - Resolves canonical project ID.
     - If `(project, def.slug)` already exists in `workflow_by_slug`, preserves the existing canonical `WorkflowId` (and numeric handle) and updates `workflow_defs`.
     - Otherwise indexes `def.id` as `Kind::Workflow` via `inner.index_imported(Kind::Workflow, def.id.iri())?`, inserts into `workflow_by_slug` and `workflow_defs`, and returns the stored `WorkflowDef`.
   - `get_workflow_def`:
     - If `id` is held in `inner.owned` under a kind other than `Kind::Workflow`, returns `Err(StoreError::WrongKind { .. })`.
     - Returns `Ok(inner.workflow_defs.get(&WorkflowId(canonical)).cloned())`.
   - `get_workflow_def_by_slug`:
     - Checks `project` kind (`Kind::Project`), looks up `(project, slug)` in `workflow_by_slug`, and returns the `WorkflowDef`.
   - `list_workflow_defs`:
     - If `project` is `Some(p)`, checks `p` is `Kind::Project` and filters by `d.project == p`, sorted deterministically by `(d.project, d.slug)`.
   - `create_workflow_run`:
     - Verifies `run.project` is `Kind::Project` and `run.workflow_id` exists in `workflow_defs` (or returns `StoreError::NoSuchWorkflow(run.workflow_id.to_string())`).
     - Indexes `run.id` as `Kind::WorkflowRun` via `inner.index_imported(Kind::WorkflowRun, run.id.iri())?` (refusing if `run.id` already exists in `workflow_runs` with `StoreError::AlreadyExists(run.id.0)`).
     - Inserts `run` into `workflow_runs` and returns it.
   - `update_workflow_run`:
     - Verifies `run.id` exists in `workflow_runs` (else `StoreError::NoSuchWorkflowRun(run.id.to_string())`), overwrites it, and returns it.
   - `get_workflow_run`:
     - If `id` is held in `inner.owned` under a kind other than `Kind::WorkflowRun`, returns `Err(StoreError::WrongKind { .. })`.
     - Returns `Ok(inner.workflow_runs.get(&WorkflowRunId(canonical)).cloned())`.
   - `list_workflow_runs`:
     - If `project` is `Some(p)`, verifies `p` is `Kind::Project`.
     - Filters `workflow_runs.values()` by `project` and `workflow_id`, sorted by `run.id`.

- [ ] **Step 2: Add `workflow_store` conformance suite in `crates/core/src/conformance.rs`**

In `crates/core/src/conformance.rs`:
1. Add `pub workflows: &'a dyn WorkflowStore` to `Bound<'a>` and wire it in `Single` and `SplitOver`.
2. Add `const WORKFLOW_STORE_CASES: usize = 3;` and `pub fn workflow_store<S: WorkflowStore + Catalog + Handles, G>(make: impl Fn() -> (S, G))` testing:
   - `workflow_def_upserts_resolves_by_slug_and_handle_and_validates_graph`: creates a project, upserts a `WorkflowDef`, verifies `handle_of(Kind::Workflow, ...)` is `Some(1)` and `resolve_handle(Kind::Workflow, 1)` matches, re-upserts with updated title under the same `(project, slug)` and confirms the `WorkflowId` and handle `1` are preserved, and verifies an invalid `WorkflowDef` is refused with `StoreError::InvalidWorkflow`.
   - `workflow_run_lifecycle_persists_and_filters_by_project_and_workflow`: creates a workflow run, checks `handle_of(Kind::WorkflowRun, ...)` is `Some(1)`, advances the run via `advance_workflow`, saves via `update_workflow_run`, and verifies `get_workflow_run` and `list_workflow_runs`.
   - `workflow_methods_refuse_wrong_kind_ids`: passing a `ProjectId` IRI as a `WorkflowId` or `WorkflowRunId` returns `StoreError::WrongKind`.
3. Call `crate::conformance::workflow_store(|| (MemStore::default(), ()));` in `crates/core/src/mem.rs::tests`.

- [ ] **Step 3: Implement `FORMAT_WITH_WORKFLOW = 8` and `WorkflowStore` for `RedbStore` in `crates/store`**

In `crates/store/src/lib.rs`:
1. Add `pub mod workflow;`.
2. Add:
```rust
/// ⚠ The format of a store that holds workflow definitions or workflow runs.
/// The first workflow write raises the store to 8 in the same transaction, so
/// an older fl refuses the store with `FormatVersion` rather than ignoring its
/// workflow tables. This build opens 2 to 8.
pub const FORMAT_WITH_WORKFLOW: u64 = 8;
```
3. In `RedbStore::open`, accept `v == FORMAT_WITH_WORKFLOW` and update `newest: FORMAT_WITH_WORKFLOW`.

Create `crates/store/src/workflow.rs` implementing `WorkflowStore for RedbStore`:
- Tables:
  - `pub(crate) const WORKFLOW_DEFS: TableDefinition<&str, &str> = TableDefinition::new("workflow_defs");`
  - `pub(crate) const WORKFLOW_SLUGS: TableDefinition<(&str, &str), &str> = TableDefinition::new("workflow_slugs");`
  - `pub(crate) const WORKFLOW_RUNS: TableDefinition<&str, &str> = TableDefinition::new("workflow_runs");`
- Every write transaction calls `raise_format(&tx, FORMAT_WITH_WORKFLOW)?`.
- `upsert_workflow_def`, `get_workflow_def`, `get_workflow_def_by_slug`, `list_workflow_defs`, `create_workflow_run`, `update_workflow_run`, `get_workflow_run`, and `list_workflow_runs` match `MemStore` semantics and pass `fl_core::conformance::workflow_store`.
- Add tests in `crates/store/src/lib.rs::tests`:
  - `redb_store_meets_the_workflow_store_contract`
  - `workflow_write_raises_the_store_to_format_8_and_survives_reopen`

- [ ] **Step 4: Run `fl-core` and `fl-store` test suites**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test -p fl-core -p fl-store`
Expected: PASS.

- [ ] **Step 5: Commit Task 2**

```bash
git add crates/core/src/mem.rs crates/core/src/conformance.rs crates/store/src/lib.rs crates/store/src/workflow.rs
git commit -m "feat(store): persist workflows and runs in RedbStore at format 8

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

### Task 3: Workflow Execution Runner, Curated Handoffs, `.fl/flows/<slug>.toml`, and `fl/state` Sync (`crates/exec`)

**Files:**
- Modify: `crates/exec/Cargo.toml`
- Create: `crates/exec/src/workflow.rs`
- Modify: `crates/exec/src/state_sync.rs`
- Modify: `crates/exec/src/lib.rs`

**Interfaces:**
- Consumes: `WorkflowDef`, `WorkflowRun`, `WorkflowStore`, `StateStore`, `AgentRegistry`, `MessageStore`, `Catalog`, `Tracker`, `Ledger`, `AnyAdapter`, `run_single_gate`, `decision::for_attempt`, `decision::for_move`.
- Produces:
  - `crates/exec/src/workflow.rs`:
    - `FLOWS_DIR: &str = ".fl/flows"`
    - `WorkflowManifest`, `StepManifest`, `StepCompletionManifest`, `export_workflow_toml`, `import_workflow_toml`, `parse_workflow_file`
    - `run_scope_slug(run_id: &WorkflowRunId) -> String`
    - `step_handoff_slug(step_id: &str, visit_count: u32) -> String`
    - `record_step_handoff`, `write_workflow_fact`, `list_workflow_handoffs`, `list_workflow_facts`, `build_step_instruction`
    - `StepExecutionOptions`, `StepExecutionReport`, `StepExecutionAction`, `execute_workflow_step`, `approve_workflow_step`, `reject_workflow_step`
  - Extended `StateSyncReport`, `StateVerifyReport`, `sync_state`, and `verify_state` in `crates/exec/src/state_sync.rs` replicating `workflows/<id>.json` and `workflow_runs/<id>.json` on `fl/state`.

- [ ] **Step 1: Add `toml.workspace = true` to `crates/exec/Cargo.toml`**

In `crates/exec/Cargo.toml`, add `toml.workspace = true` under `[dependencies]`.

- [ ] **Step 2: Create `crates/exec/src/workflow.rs` with `.fl/flows/<slug>.toml` export/import, curated handoffs, declared runtime facts, prompt assembly, and step execution**

In `crates/exec/src/workflow.rs`, implement:
1. **Portable `.fl/flows/<slug>.toml` Manifest (`WorkflowManifest`):**
   - `pub const FLOWS_DIR: &str = ".fl/flows";`
   - `WorkflowManifest` mirrors `WorkflowDef` without machine-local `id` or `project`, and in `StepCompletionManifest::Gated { gates: Vec<String>, regret: Regret }` allows each entry in `gates` to be either a gate `urn:uuid:...` IRI or a gate name (`"cargo-test"`).
   - `export_workflow_toml(catalog: &dyn Catalog, def: &WorkflowDef) -> Result<String, ExecError>`: maps each `GateId` in `StepCompletion::Gated` to the gate's `name` if found in `catalog.list_gates(&def.project)` (or `gate_id.to_string()` as fallback) and serializes with `toml::to_string_pretty`.
   - `import_workflow_toml(catalog: &dyn Catalog, project: &ProjectId, text: &str, default_commit: &str, default_by: &str) -> Result<WorkflowDef, ExecError>`: parses TOML (or JSON if `text.trim_start().starts_with('{')`), resolves gate names or `urn:uuid:...` IRIs against `catalog.list_gates(project)`, mints a `WorkflowId(stamp::entry_id())` (preserved if `upsert_workflow_def` finds an existing `(project, slug)`), validates with `validate_workflow_def`, and returns `WorkflowDef`.
2. **Curated Step Handoffs & Declared Runtime Facts (`StateScope::Workflow`):**
   - `pub fn run_scope_slug(run_id: &WorkflowRunId) -> String`: computes `format!("run-{}", &sha256_hex(run_id.as_str())[..24])`.
   - `pub fn step_handoff_slug(step_id: &str, visit_count: u32) -> String`: `format!("step-{step_id}-v{visit_count}")`.
   - `pub fn record_step_handoff(state_store: &dyn StateStore, run_id: &WorkflowRunId, step_id: &str, visit_count: u32, handoff: HandoffEntry, at: At, by: &str) -> Result<StateEntry, ExecError>`.
   - `pub fn write_workflow_fact(state_store: &dyn StateStore, def: &WorkflowDef, run: &WorkflowRun, key: &str, value: serde_json::Value, at: At, by: &str) -> Result<StateEntry, WorkflowError>`:
     - Checks `let Some(&merge_mode) = def.declared_keys.get(key)` (returning `Err(WorkflowError::UndeclaredWorkflowKey { workflow: def.slug.clone(), key: key.to_string() })` if absent).
     - Upserts `StatePayload::Runtime(RuntimeEntry { value, merge_mode })` under `StateScope::Workflow(run_scope_slug(&run.id))`.
   - `pub fn build_step_instruction(state_store: &dyn StateStore, def: &WorkflowDef, run: &WorkflowRun, step_id: &str) -> Result<String, ExecError>`:
     - Starts with `step_def.prompt`.
     - Appends a `## Predecessor Handoffs` section with the latest `HandoffEntry` (`summary`, `shipped`, `in_flight`, `blockers`, `next_steps`) for every predecessor step with an edge `(pred -> step_id)` (and for `step_id`'s own previous visit if `visit_count > 1`, so a retry/loop step sees the rejection/failure blockers).
     - Appends a `## Workflow Runtime Facts` section with current `RuntimeEntry` key/values in `StateScope::Workflow(run_scope_slug(&run.id))`.
     - Never includes raw vendor JSONL transcripts.
3. **Step Execution (`execute_workflow_step`, `approve_workflow_step`, `reject_workflow_step`):**
   - `execute_workflow_step`:
     - Validates `run.status == WorkflowRunStatus::Running` and `run.active_steps.contains(&step_id)`.
     - Resolves agent assignment via `select_step_agent` if `step.assigned_agent.is_none()`. If `AssigneeRule::Manual` and unassigned, returns `StepExecutionAction::AwaitingAssignment`.
     - If `options.offer_only` is `true`: sends a `MessageKind::TaskOffer` via `MessageStore` to `WireAddress::AgentName(agent_name)` with subject `format!("Workflow `{}` step `{}` ready", def.slug, step_id)` and body containing the instruction from `build_step_instruction`, updates `step.state = State::Doing`, persists the run, and returns `StepExecutionAction::Offered { message_id, agent }`.
     - Otherwise ensures a `RecordId` is bound to `step.record_id` (creating a record via `tracker.add_record(&run.project, &format!("{} / {}", run.title, step_def.title))` if `None`, and moving it to `State::Doing`).
     - If `step_def.isolate_worktree` is `true`, creates/reuses branch `fl/flow/<run-short>/<step_id>` in `<project_root>/.fl/worktrees/<run-short>-<step_id>` via `git worktree add -B ...` and records `step.worktree_branch`.
     - Runs `AnyAdapter` with `AttemptSpec`, records `Attempt` in `Ledger`, and flushes `decision::for_attempt`.
     - Evaluates `step_def.completion`:
       - If `outcome.status != AttemptStatus::Completed`: `StepOutcome::Fail`.
       - `StepCompletion::Ungated`: `StepOutcome::Pass`.
       - `StepCompletion::HumanSignOff { approval_tier }`: sends an urgent `MessageKind::ApprovalRequest` FerroWire message to the target approver (`@area:<project>/lead` for `Lead`, `@role:lab_manager` for `LabManager`, or `"owner"` for `Owner`), records a `HandoffEntry`, and advances with `StepOutcome::NeedsHuman` (`reason: Some(format!("awaiting {} sign-off", approval_tier.as_wire()))`).
       - `StepCompletion::Gated { gates, regret }`: runs each gate in `gates` via `evaluate::run_single_gate_with_regret` (adding a thin pub helper in `crates/exec/src/evaluate.rs` that passes `regret` to `run_gate`), flushes a `Decision` (`decision::for_move` on the step's record), and sets `StepOutcome::Pass` if all gates pass or `StepOutcome::Fail` with gate failure excerpts in `blockers`.
     - Writes the step's curated `HandoffEntry`, calls `advance_workflow`, updates `WorkflowStore`, and returns `StepExecutionReport`.
   - `approve_workflow_step` and `reject_workflow_step`:
     - Verify `authorize_step_sign_off`.
     - Ensure the step has a `RecordId`, flush a ledger `Decision` (`Outcome::Move` to `State::Done` on approve, or `State::Review` / `allowed: false` on reject), write the curated `HandoffEntry` (recording `note` or `reason`), call `advance_workflow` with `StepOutcome::Pass` (for approve) or `StepOutcome::Fail` (for reject, first setting `step.attempt_count = step_def.max_attempts` so rejection immediately follows the outgoing `Fail` edge rather than doing an in-step retry), and update `WorkflowStore`.

- [ ] **Step 3: Extend `crates/exec/src/state_sync.rs` to sync `workflows/` and `workflow_runs/` on `fl/state`**

In `crates/exec/src/state_sync.rs`:
1. Add `WorkflowStore` to the trait bounds of `sync_state` and `verify_state` (`S: StateStore + AgentRegistry + MessageStore + WorkflowStore`).
2. In `BranchSnapshot`:
   - Add `workflows_raw: BTreeMap<String, Vec<u8>>`, `workflows: Vec<WorkflowDef>`, `workflow_runs_raw: BTreeMap<String, Vec<u8>>`, `workflow_runs: Vec<WorkflowRun>`.
   - In `load_snapshot`, parse `workflows/<fname>.json` into `WorkflowDef` and `workflow_runs/<fname>.json` into `WorkflowRun`.
   - In `write_snapshot_tree`, write `workflows/` and `workflow_runs/` subtrees when non-empty.
   - During `sync_state`:
     - Import remote `WorkflowDef`s via `store.upsert_workflow_def` (when the project exists locally or by indexing the project/workflow) and remote `WorkflowRun`s (creating if new, or updating if remote `(total_steps_executed, updated_at)` is newer than local).
     - Export all local `store.list_workflow_defs(None)?` and `store.list_workflow_runs(None, None)?` into `next_snapshot.workflows_raw` and `next_snapshot.workflow_runs_raw` so any new or updated workflow definition or run triggers a commit on `fl/state`.

- [ ] **Step 4: Run `fl-exec` tests**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test -p fl-exec`
Expected: PASS (including new unit/integration tests in `crates/exec/src/workflow.rs` and `crates/exec/src/state_sync.rs`).

- [ ] **Step 5: Commit Task 3**

```bash
git add crates/exec/Cargo.toml crates/exec/src/evaluate.rs crates/exec/src/workflow.rs crates/exec/src/state_sync.rs crates/exec/src/lib.rs
git commit -m "feat(exec): add workflow runner, TOML export/import, handoffs, and state sync

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

### Task 4: CLI `fl flow` Subcommands, MCP Tools (`fl state mcp`), Author-Owned Merge CLI Wiring, and End-to-End Tests (`crates/cli`)

**Files:**
- Create: `crates/cli/src/cmd/flow.rs`
- Modify: `crates/cli/src/cmd/mod.rs`
- Modify: `crates/cli/src/main.rs`
- Modify: `crates/cli/src/cmd/state_mcp.rs`
- Modify: `crates/cli/src/cmd/pr.rs`
- Modify: `crates/cli/tests/github_pr.rs`
- Create: `crates/cli/tests/workflow.rs`

**Interfaces:**
- Consumes: `fl_core::workflow::*`, `fl_core::WorkflowStore`, `fl_core::authorize_agent_merge`, `fl_exec::workflow::*`.
- Produces:
  - `fl flow {add, list, show, export, import, start, status, assign, step, run, approve, reject}` in `crates/cli/src/cmd/flow.rs`
  - MCP tools `workflow_list`, `workflow_status`, `workflow_handoff`, `workflow_step` in `crates/cli/src/cmd/state_mcp.rs`
  - Author-owned PR merge wiring in `crates/cli/src/cmd/pr.rs` (`authorize_agent_merge`)

- [ ] **Step 1: Wire `authorize_agent_merge` in `crates/cli/src/cmd/pr.rs` and update `crates/cli/tests/github_pr.rs`**

In `crates/cli/src/cmd/pr.rs`:
1. Import `authorize_agent_merge` from `fl_core`.
2. In `PrCmd::Merge` (around line 550), replace `authorize_agent_action(&eval, &caller)` with `authorize_agent_merge(&eval, &caller)`.
3. In `crates/cli/tests/github_pr.rs`, verify that:
   - `felicia` (`assistant` co-author of PR #1) still cannot self-approve PR #1 (`self_review_forbidden`) and cannot merge PR #1 before `ferris` approves (`missing_valid_approval`),
   - Once `ferris` (`lead`) approves PR #1, `felicia` can merge her own PR #1 via `fl github pr merge 1 --method squash --by felicia`,
   - Non-co-author peer `alex` (`assistant`) is still refused when attempting to merge PR #1 (`insufficient_role`).

- [ ] **Step 2: Create `crates/cli/src/cmd/flow.rs` and wire `Command::Flow` in `crates/cli/src/main.rs`**

Create `crates/cli/src/cmd/flow.rs` implementing `FlowCmd`:
- `Add { slug: Option<String>, title: Option<String>, file: PathBuf, project: Option<Ref>, by: String, json: bool }`
- `List { project: Option<Ref>, json: bool }`
- `Show { workflow: String, project: Option<Ref>, json: bool }`
- `Export { workflow: String, out: Option<PathBuf>, project: Option<Ref>, json: bool }`
- `Import { path: PathBuf, project: Option<Ref>, by: String, json: bool }`
- `Start { workflow: String, title: String, project: Option<Ref>, by: String, json: bool }`
- `Status { run: Option<String>, project: Option<Ref>, json: bool }`
- `Assign { run: String, step_id: String, agent: String, adapter: Option<String>, by: String, json: bool }`
- `Step { run: String, step: Option<String>, adapter: Option<String>, binary: Option<String>, offer_only: bool, timeout_secs: u64, by: String, json: bool }`
- `Run { run: String, adapter: Option<String>, binary: Option<String>, timeout_secs: u64, by: String, json: bool }`
- `Approve { run: String, step_id: String, by: String, note: Option<String>, json: bool }`
- `Reject { run: String, step_id: String, by: String, reason: String, json: bool }`

Ensure policy/validation refusals (`ReviewIndependence`, `SelfApprovalForbidden`, `InsufficientApprovalAuthority`, `UndeclaredWorkflowKey`, `NoEligibleAgent`) exit with code `2`, while run `Failed` or `NeedsHuman` in `fl flow run` / `fl flow step` exits with code `1`, and `Completed` / `Pass` exits with code `0`.

Wire `pub mod flow;` in `crates/cli/src/cmd/mod.rs` and `Command::Flow(cmd::flow::Cmd)` in `crates/cli/src/main.rs` (passing `&ctx`, `&cfg.adapters`, and `ws_store` for agent registry lookups, just like `Command::Github`).

- [ ] **Step 3: Add `workflow_list`, `workflow_status`, `workflow_handoff`, and `workflow_step` to `crates/cli/src/cmd/state_mcp.rs`**

In `crates/cli/src/cmd/state_mcp.rs`:
- Add the 4 tool definitions in `tool_definitions()` and handlers in `dispatch_tool()`:
  - `workflow_list`: returns JSON `{ "workflows": [...], "runs": [...] }`.
  - `workflow_status`: resolves `run_id` (handle or IRI) and returns `{ "run": ..., "workflow": ..., "handoffs": [...], "facts": [...] }`.
  - `workflow_handoff`: records a `HandoffEntry` for `(run_id, step_id)` and optional `facts` map (validated via `write_workflow_fact` against `def.declared_keys`).
  - `workflow_step`: advances `(run_id, step_id)` with `outcome` (`"pass"`, `"fail"`, `"needs_human"`), optional `summary` / `reason`, and `by`.

- [ ] **Step 4: Create end-to-end integration tests in `crates/cli/tests/workflow.rs`**

Create `crates/cli/tests/workflow.rs` (remembering to isolate `HOME` and `.env_remove` `AGENT_PERSONA`, `CLAUDE_CODE_MESSAGING_SOCKET`, `ANTIGRAVITY_CONVERSATION_ID`, and `CODEX_THREAD_ID`) testing:
1. `workflow_export_import_and_gated_review_loop_end_to_end`:
   - Initializes a git repo and project, registers `jarvis` (`lab_manager`), `ferris` (`lead`), and `felicia` (`assistant`), and adds a command gate `check-file`.
   - Imports a `.fl/flows/review-loop.toml` workflow where `implement` (`felicia`, `gated` by `check-file`) transitions on `pass` to `review` (`reviews_step = "implement"`, `require_different_vendor = true`, `human_sign_off` with `approval_tier = "lead"`), which transitions on `fail` back to `implement` and on `pass` to `END`.
   - Verifies `fl flow export` writes `.fl/flows/review-loop.toml` and `fl flow list` / `fl flow show` work by handle `1` and slug `review-loop`.
   - Starts a run (`fl flow start review-loop --title "Build feature" --by ferris`).
   - Runs `implement` with a fake adapter script; verifies gate runs and `review` becomes active in `NeedsHuman`.
   - Verifies `fl flow assign <run> review --agent felicia` is refused with exit code `2` (same agent as `implement`).
   - Verifies `fl flow approve <run> review --by felicia` is refused with exit code `2` (`SelfApprovalForbidden` / `InsufficientApprovalAuthority`).
   - Rejects `review` as `ferris` (`fl flow reject 1 review --by ferris --reason "Add missing edge case"`), verifying the run loops back to `implement` (`visit_count == 2`).
   - Re-runs `implement` (`fl flow step 1 --step implement ...`), then approves `review` as `ferris` (`fl flow approve 1 review --by ferris --note "LGTM"`), verifying the run reaches `Completed` and records ledger decisions.
2. `workflow_mcp_tools_list_status_handoff_and_declared_keys_enforcement`:
   - Drives `fl state mcp` over stdio to test `workflow_list`, `workflow_status`, `workflow_handoff` (verifying declared keys succeed and undeclared keys return an error), and `workflow_step`.

- [ ] **Step 5: Run full workspace test suite and clippy**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS with zero warnings.

- [ ] **Step 6: Commit Task 4**

```bash
git add crates/cli/src/cmd/flow.rs crates/cli/src/cmd/mod.rs crates/cli/src/main.rs crates/cli/src/cmd/state_mcp.rs crates/cli/src/cmd/pr.rs crates/cli/tests/github_pr.rs crates/cli/tests/workflow.rs
git commit -m "feat(cli): add fl flow subcommands, workflow MCP tools, and author-owned PR merge

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```
