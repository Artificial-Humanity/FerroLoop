# FerroLoop — Sub-project 4: Workflow Graph (`fl flow`) Specification

**Date:** 2026-10-26  
**Author:** Ferris (`ferris`, FerroLoop Lead)  
**Status:** Draft (Pending Spec Review)

---

## 0. Purpose and Scope

Sub-projects 1–3 and the GitHub-Level Agent Rights Hierarchy delivered the durable state database (`FORMAT_WITH_STATE = 6`), FerroWire agent messaging (`FORMAT_WITH_WIRE = 7`), five-vendor CLI adapters with token/cost/liveness telemetry (`ClaudeAdapter`, `CodexAdapter`, `AntigravityAdapter`, `MuseAdapter`, `GrokAdapter`), and tier-aware PR governance.

This specification defines **Sub-project 4: Workflow Graph (`fl flow`)**:
1. **Pure Workflow Graph Model & Scheduler (`fl_core::workflow`):** Directed workflow definitions (`WorkflowDef`, `StepDef`, `EdgeDef`, `JoinPolicy`, `AssigneeRule`, `StepCompletion`, `ApprovalTier`), static graph validation (`validate_workflow_def`), run state (`WorkflowRun`, `StepInstance`, `WorkflowRunStatus`, `StepOutcome`), deterministic graph scheduling with conditional loops and parallel fan-out/fan-in (`advance_workflow`), review-independence agent selection (`select_step_agent`), and tiered human/lead sign-off authorization (`authorize_step_sign_off`).
2. **Store-Backed Workflow Persistence (`fl-store` `FORMAT_WITH_WORKFLOW = 8`):** First-class `WorkflowId` (`Kind::Workflow`) and `WorkflowRunId` (`Kind::WorkflowRun`) with local numeric handles (`1`, `2`, …), `WorkflowStore` trait implemented by both `MemStore` and `RedbStore`, and `fl/state` orphan-branch synchronization (`workflows/` and `workflow_runs/`).
3. **Workflow Execution Runner, Curated Handoffs, & `.fl/flows/<slug>.toml` Sharing (`fl_exec::workflow`):** Step execution over `AnyAdapter` (or FerroWire task-offer dispatch), per-step exit gate evaluation (`run_single_gate`) with ledger `Decision` flushing, curated `HandoffEntry` and declared `StateScope::Workflow` facts (never passing raw vendor transcripts across steps), and deterministic TOML export/import (`.fl/flows/<slug>.toml`).
4. **CLI (`fl flow`) & MCP (`fl state mcp`) Surface (`fl-cli`):** Complete `fl flow` subcommand hierarchy (`add`, `list`, `show`, `export`, `import`, `start`, `status`, `assign`, `step`, `run`, `approve`, `reject`) and four MCP tools (`workflow_list`, `workflow_status`, `workflow_handoff`, `workflow_step`).

---

## 1. Pure Workflow Graph Model and Scheduler (`crates/core/src/workflow.rs`, `ids.rs`, `store.rs`)

### 1.1 Identity and Store Format (`FORMAT_WITH_WORKFLOW = 8`)

In `crates/core/src/ids.rs`:
- `Kind` adds two variants:
  - `Kind::Workflow` (wire `"workflow"`)
  - `Kind::WorkflowRun` (wire `"workflow_run"`)
- `id_type!(WorkflowId, Kind::Workflow);` and `id_type!(WorkflowRunId, Kind::WorkflowRun);` give workflow definitions and workflow runs `urn:uuid:...` IRIs and per-kind numeric handles (`1`, `2`, …) in `Handles`.
- In `crates/core/src/store.rs`, `KindRouted::owner` routes `Kind::Workflow | Kind::WorkflowRun => self.catalog`.
- In `crates/store/src/lib.rs`, `pub const FORMAT_WITH_WORKFLOW: u64 = 8;`. Writing the first workflow definition or workflow run to `RedbStore` raises `META["format_version"]` to `FORMAT_WITH_WORKFLOW` (`8`) in the same write transaction so older `fl` binaries refuse the store cleanly rather than ignoring workflow tables. `RedbStore::open` accepts format versions `2..=FORMAT_WITH_WORKFLOW`.

### 1.2 Workflow Definition (`WorkflowDef`) and Static Graph Types

```rust
pub const END_STEP: &str = "END";
pub const DEFAULT_MAX_TOTAL_STEPS: u32 = 64;
pub const DEFAULT_STEP_MAX_ATTEMPTS: u32 = 1;
pub const DEFAULT_STEP_MAX_VISITS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTier {
    #[default]
    Owner,
    LabManager,
    Lead,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssigneeRule {
    /// Explicit agent persona (`name_key`, e.g. `"felicia"`).
    Agent { name: String },
    /// Automatic selection from active `AgentRegistry` records.
    Selector {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<AgentRole>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        area: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        adapter: Option<String>,
    },
    /// Pauses until assigned via `fl flow assign`.
    #[default]
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum StepCompletion {
    /// Runs the listed FerroLoop gates over their declared populations and records a ledger `Decision`.
    Gated {
        gates: Vec<GateId>,
        #[serde(default)]
        regret: Regret,
    },
    /// Enters `State::NeedsHuman` and requires sign-off from `approval_tier` (never self-approved).
    HumanSignOff {
        #[serde(default)]
        approval_tier: ApprovalTier,
    },
    /// Intermediate agent step that transitions directly to `Done` when the attempt completes.
    #[default]
    Ungated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum JoinPolicy {
    /// Waits for all incoming predecessor steps in the current wave to reach `Done`.
    #[default]
    All,
    /// Proceeds as soon as 1 incoming predecessor reaches `Done`.
    Any,
    /// Proceeds once `required` incoming predecessors reach `Done`.
    NOfM { required: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepDef {
    pub step_id: String,
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub assignee: AssigneeRule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviews_step: Option<String>,
    #[serde(default)]
    pub require_different_vendor: bool,
    #[serde(default)]
    pub completion: StepCompletion,
    #[serde(default)]
    pub join_policy: JoinPolicy,
    #[serde(default = "default_step_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_step_max_visits")]
    pub max_visits: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub isolate_worktree: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeCondition {
    Pass,
    Fail,
    NeedsHuman,
    Always,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeDef {
    pub from_step: String,
    pub to_step: String, // step_id or "END"
    pub on: EdgeCondition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDef {
    pub id: WorkflowId,
    pub project: ProjectId,
    pub slug: String,
    pub title: String,
    pub start_step: String,
    pub steps: Vec<StepDef>,
    pub edges: Vec<EdgeDef>,
    #[serde(default = "default_max_total_steps")]
    pub max_total_steps: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub declared_keys: BTreeMap<String, MergeMode>,
    pub authored_at_commit: String,
    pub authored_by: String,
}
```

### 1.3 Static Graph Validation (`validate_workflow_def`)

`validate_workflow_def(def: &WorkflowDef) -> Result<(), WorkflowError>` enforces at definition creation and import time:
1. `validate_slug(&def.slug)` succeeds, and `def.title.trim()` is non-empty.
2. `def.steps` is non-empty; every `step.step_id` passes `validate_slug` (and is not `"END"`), has a non-empty `title`, and appears at most once (`DuplicateStepId`).
3. `def.start_step` names an existing `step_id` in `def.steps` (`UnknownStartStep`).
4. `def.max_total_steps > 0`, and for every step `step.max_attempts > 0` and `step.max_visits > 0` (`InvalidStepLimits`).
5. Every `EdgeDef` has `from_step` in `def.steps` and `to_step` either equal to `"END"` or in `def.steps` (`UnknownEdgeStep`).
6. Every step with `reviews_step = Some(target)` references a known `target` in `def.steps` with `target != step.step_id` (`InvalidReviewTarget`).
7. Every step with `JoinPolicy::NOfM { required }` has `required >= 1` and `required as usize <= incoming_edges(step_id).count()` (`InvalidJoinThreshold`).
8. Every key in `def.declared_keys` passes `validate_slug` (`InvalidDeclaredKey`).
9. Every step with `StepCompletion::Gated { gates, .. }` has `!gates.is_empty()` (`EmptyStepGates`, preserving the invariant that an empty gate list verifies nothing).
10. **Reachability:** Every step reachable from `start_step` (and `start_step` itself) can reach `"END"` along at least one directed path in `def.edges` (`UnreachableEnd { step_id }`).

### 1.4 Workflow Run State and Pure Step Scheduler (`WorkflowRun`, `advance_workflow`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Running,
    NeedsHuman,
    Completed,
    Failed,
    Canceled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Pass,
    Fail,
    NeedsHuman,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepInstance {
    pub step_id: String,
    pub visit_count: u32,
    pub attempt_count: u32,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_used: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<RecordId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_attempt: Option<Iri>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<StepOutcome>,
    pub tokens_used: u64,
    pub cost_usd_micros: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_human_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: WorkflowRunId,
    pub workflow_id: WorkflowId,
    pub project: ProjectId,
    pub title: String,
    pub status: WorkflowRunStatus,
    pub active_steps: Vec<String>,
    pub steps: BTreeMap<String, StepInstance>,
    pub total_steps_executed: u32,
    pub total_tokens: u64,
    pub total_cost_usd_micros: u64,
    pub created_at: At,
    pub updated_at: At,
    pub by: String,
}
```

#### Initialization (`WorkflowRun::new`)
- Creates a `StepInstance` for every `StepDef` in `def.steps` with `visit_count = 0`, `attempt_count = 0`, `state = State::Todo`, `tokens_used = 0`, `cost_usd_micros = 0`.
- Sets `start_step`'s `visit_count = 1`, puts `def.start_step` in `active_steps`, and sets `status = WorkflowRunStatus::Running`.

#### Pure Scheduler (`advance_workflow`)
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepAttemptReport {
    pub step_id: String,
    pub outcome: StepOutcome,
    pub attempt_iri: Option<Iri>,
    pub tokens_used: u64,
    pub cost_usd_micros: u64,
    pub reason: Option<String>,
}
```

`advance_workflow(def: &WorkflowDef, run: &mut WorkflowRun, report: StepAttemptReport, at: At) -> Result<(), WorkflowError>` executes deterministically:
1. **Accounting:**
   - Looks up `step` in `run.steps` and `step_def` in `def.steps`.
   - Increments `step.attempt_count += 1`, `run.total_steps_executed += 1`.
   - Adds `report.tokens_used` to `step.tokens_used` and `run.total_tokens`.
   - Adds `report.cost_usd_micros` to `step.cost_usd_micros` and `run.total_cost_usd_micros`.
   - Updates `step.latest_attempt = report.attempt_iri` and `run.updated_at = at`.
2. **Ceiling Checks (Escalate to `NeedsHuman`):**
   - If `run.total_steps_executed > def.max_total_steps`, or `def.budget_usd_micros.is_some_and(|b| run.total_cost_usd_micros > b)`, or `def.max_tokens.is_some_and(|t| run.total_tokens > t)`, or `step_def.budget_usd_micros.is_some_and(|b| step.cost_usd_micros > b)`, or `step_def.max_tokens.is_some_and(|t| step.tokens_used > t)`:
     - Sets `step.state = State::NeedsHuman`, `step.last_outcome = Some(StepOutcome::NeedsHuman)`, `step.needs_human_reason = Some(...)`, and `run.status = WorkflowRunStatus::NeedsHuman`. Returns `Ok(())`.
3. **In-Step Retry Preservation on `Fail`:**
   - If `report.outcome == StepOutcome::Fail` and `step.attempt_count < step_def.max_attempts`:
     - Keeps `step_id` in `run.active_steps`, sets `step.state = State::Todo`, `step.last_outcome = Some(StepOutcome::Fail)`, and preserves all completed sibling steps in `run.steps` so the next `fl flow step` retries only the failed step. Returns `Ok(())`.
4. **Human Sign-Off / Escalation Routing:**
   - If `report.outcome == StepOutcome::NeedsHuman` (or `report.outcome == StepOutcome::Pass` on a step with `StepCompletion::HumanSignOff { .. }` when not yet approved):
     - If `def.edges` has an outgoing edge from `step_id` with `EdgeCondition::NeedsHuman`, follows that edge below; otherwise holds `step_id` in `State::NeedsHuman`, sets `step.last_outcome = Some(StepOutcome::NeedsHuman)`, `step.needs_human_reason = report.reason`, and sets `run.status = WorkflowRunStatus::NeedsHuman`. Returns `Ok(())`.
5. **Edge Evaluation & Fan-Out / Fan-In:**
   - Removes `step_id` from `run.active_steps`.
   - Sets `step.last_outcome = Some(report.outcome)` and `step.state = match report.outcome { StepOutcome::Pass => State::Done, StepOutcome::Fail => State::Review, StepOutcome::NeedsHuman => State::NeedsHuman }`.
   - Collects all outgoing edges from `step_id` where `edge.on == EdgeCondition::Always` or `edge.on` matches `report.outcome`.
   - If no outgoing edges match:
     - If `report.outcome == StepOutcome::Fail` and `run.active_steps.is_empty()`, sets `run.status = WorkflowRunStatus::Failed`.
     - Returns `Ok(())`.
   - If any matching edge targets `"END"` and `run.active_steps.is_empty()`:
     - Sets `run.status = WorkflowRunStatus::Completed` and returns `Ok(())`.
   - For each matching target `to_step != "END"`:
     - Evaluates `target_def.join_policy` across all incoming edges `(pred -> to_step)`:
       - `JoinPolicy::All`: all predecessor steps with an edge into `to_step` have `state == State::Done` (or the incoming edge matched `report.outcome` from `step_id`). Specifically, for fan-in with `N` incoming edges, counts how many predecessor steps currently have `state == State::Done`; `All` requires all `N` predecessors `Done`, `Any` requires `>= 1` predecessor `Done`, and `NOfM { required }` requires `>= required` predecessors `Done`.
     - When the join policy is satisfied and `to_step` is not already in `run.active_steps`:
       - Increments `target_step.visit_count += 1`, resets `target_step.attempt_count = 0`, `target_step.needs_human_reason = None`.
       - If `target_step.visit_count > target_def.max_visits`, sets `target_step.state = State::NeedsHuman`, `target_step.last_outcome = Some(StepOutcome::NeedsHuman)`, `target_step.needs_human_reason = Some(format!("step `{to_step}` exceeded max_visits ({})", target_def.max_visits))`, pushes `to_step` to `run.active_steps`, and sets `run.status = WorkflowRunStatus::NeedsHuman`.
       - Otherwise sets `target_step.state = State::Todo` and pushes `to_step` to `run.active_steps`.

### 1.5 Review Independence & Agent Selection (`select_step_agent`)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedStepAgent {
    pub agent_name: String,
    pub adapter: Option<String>,
}
```

`select_step_agent(def: &WorkflowDef, run: &WorkflowRun, step_id: &str, agents: &[AgentRecord], override_adapter: Option<&str>) -> Result<Option<SelectedStepAgent>, WorkflowError>`:
1. Finds `step_def` in `def.steps`.
2. If `step_def.reviews_step` is `Some(upstream_id)`:
   - Looks up `upstream = run.steps.get(upstream_id)`.
   - `forbidden_agent = upstream.and_then(|u| u.assigned_agent.as_deref())`.
   - `forbidden_vendor = if step_def.require_different_vendor { upstream.and_then(|u| u.adapter_used.as_deref()) } else { None }`.
3. Checks candidate agents (`agent.status == AgentStatus::Active`):
   - A candidate is rejected by `validate_reviewer_independence` if `Some(agent.name_key.as_str()) == forbidden_agent` (`ReviewIndependenceViolation::SameAgent { step_id, reviews_step, agent }`).
   - If `forbidden_vendor` is `Some(v)` and the chosen adapter equals `v` (case-insensitively), rejected with `ReviewIndependenceViolation::SameVendor { step_id, reviews_step, vendor }`.
4. Evaluates `step_def.assignee`:
   - `AssigneeRule::Manual`: returns `Ok(None)`.
   - `AssigneeRule::Agent { name }`: finds active agent with `name_key == name.to_ascii_lowercase()`, verifies review independence, and returns `Ok(Some(SelectedStepAgent { agent_name, adapter: override_adapter.map(ToOwned::to_owned) }))`.
   - `AssigneeRule::Selector { role, area, adapter }`: filters active agents matching `role` (if `Some`) and `agent.covers_area(area)` (if `Some`), excluding `forbidden_agent`, in deterministic `name_key` order. Uses `override_adapter.or(adapter.as_deref())`, verifies `forbidden_vendor`, and returns the first eligible agent or `Err(WorkflowError::NoEligibleAgent { step_id })`.

### 1.6 Tiered Sign-Off Authorization (`authorize_step_sign_off`)

When a step is in `State::NeedsHuman`, `authorize_step_sign_off(project_slug: &str, step_def: &StepDef, step_instance: &StepInstance, caller: &str, agents: &[AgentRecord], owner_login: &str) -> Result<(), WorkflowError>`:
1. Normalizes `caller_key = caller.trim().to_ascii_lowercase()`.
2. **Self-approval prohibition:** If `step_instance.assigned_agent.as_deref() == Some(caller_key.as_str())`, refuses with `WorkflowError::SelfApprovalForbidden { step_id, caller: caller_key }` (mapped to CLI exit code `2`).
3. Determines required `ApprovalTier`:
   - If `step_def.completion` is `StepCompletion::HumanSignOff { approval_tier }`, uses `approval_tier`.
   - Otherwise (for example, budget ceiling, visit cap, or gate escalation), defaults to `ApprovalTier::Owner`.
4. Checks caller authority:
   - If `caller_key == owner_login.to_ascii_lowercase()` or `caller_key == "owner"`, allowed for all tiers (`Owner`, `LabManager`, `Lead`).
   - Otherwise looks up an `Active` `AgentRecord` with `name_key == caller_key`:
     - `ApprovalTier::Owner`: refuses any non-owner caller with `WorkflowError::InsufficientApprovalAuthority`.
     - `ApprovalTier::LabManager`: requires `agent.role == AgentRole::LabManager`.
     - `ApprovalTier::Lead`: requires `agent.role == AgentRole::LabManager` or `(agent.role == AgentRole::Lead && agent.covers_area(project_slug))`.

---

## 2. Store Trait and `RedbStore` Persistence (`crates/core/src/store.rs`, `crates/store/src/workflow.rs`)

### 2.1 `WorkflowStore` Trait

In `crates/core/src/store.rs`:

```rust
pub trait WorkflowStore {
    fn upsert_workflow_def(&self, def: WorkflowDef) -> Result<WorkflowDef, StoreError>;
    fn get_workflow_def(&self, id: &WorkflowId) -> Result<Option<WorkflowDef>, StoreError>;
    fn get_workflow_def_by_slug(
        &self,
        project: &ProjectId,
        slug: &str,
    ) -> Result<Option<WorkflowDef>, StoreError>;
    fn list_workflow_defs(&self, project: Option<&ProjectId>) -> Result<Vec<WorkflowDef>, StoreError>;

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

`StoreError` adds:
- `NoSuchWorkflow(String)`
- `NoSuchWorkflowRun(String)`
- `InvalidWorkflow(String)`

### 2.2 `MemStore` and `RedbStore` Tables (`FORMAT_WITH_WORKFLOW = 8`)

In `crates/store/src/workflow.rs`:
- `WORKFLOW_DEFS: TableDefinition<&str, &str> = TableDefinition::new("workflow_defs");` (`WorkflowId` IRI -> JSON `WorkflowDef`)
- `WORKFLOW_SLUGS: TableDefinition<(&str, &str), &str> = TableDefinition::new("workflow_slugs");` (`(ProjectId IRI, slug)` -> `WorkflowId` IRI)
- `WORKFLOW_RUNS: TableDefinition<&str, &str> = TableDefinition::new("workflow_runs");` (`WorkflowRunId` IRI -> JSON `WorkflowRun`)
- Writing to `WORKFLOW_DEFS` or `WORKFLOW_RUNS` calls `raise_to(&tx, FORMAT_WITH_WORKFLOW)?`, records `Kind::Workflow` or `Kind::WorkflowRun` in `IDS`, and assigns a sequential per-kind numeric handle in `HANDLES` / `BY_HANDLE`.
- If `upsert_workflow_def` updates an existing `(project, slug)` entry, it preserves the existing `WorkflowId` and numeric handle while updating the stored `WorkflowDef`.

---

## 3. Workflow Execution Runner, Curated Handoffs, `.fl/flows/<slug>.toml`, and `fl/state` Sync (`crates/exec/src/workflow.rs`, `state_sync.rs`)

### 3.1 `.fl/flows/<slug>.toml` Portable Manifest Format

A portable `WorkflowManifest` serializes to/from `<project_root>/.fl/flows/<slug>.toml` (or JSON):

```toml
slug = "review-loop"
title = "Implement and Independent Review"
start_step = "implement"
max_total_steps = 16
budget_usd_micros = 5000000
max_tokens = 200000
authored_at_commit = "0123456789abcdef0123456789abcdef01234567"
authored_by = "ferris"

[declared_keys]
branch = "last_writer"
changed_files = "append"

[[steps]]
step_id = "implement"
title = "Implement feature"
prompt = "Implement the requested change and run unit tests."
assignee = { kind = "agent", name = "felicia" }
completion = { mode = "gated", gates = ["urn:uuid:..."], regret = "low" }
max_attempts = 2
max_visits = 3
isolate_worktree = false

[[steps]]
step_id = "review"
title = "Review implementation"
prompt = "Review the changes from implement."
assignee = { kind = "selector", role = "lead", area = "ferroloop" }
reviews_step = "implement"
require_different_vendor = false
completion = { mode = "human_sign_off", approval_tier = "lead" }

[[edges]]
from_step = "implement"
to_step = "review"
on = "pass"

[[edges]]
from_step = "review"
to_step = "implement"
on = "fail"

[[edges]]
from_step = "review"
to_step = "END"
on = "pass"
```

- Gate references in `.fl/flows/<slug>.toml` may be written either as a Gate `urn:uuid:...` IRI or as a gate name (`"cargo-test"`), which `import_workflow_toml` resolves against the project's `Catalog::list_gates(&project_id)` (and `export_workflow_toml` exports using gate names when unambiguous or IRIs).

### 3.2 Curated Step Handoffs and Declared `StateScope::Workflow` Facts

To prevent context-window bloat and cross-vendor transcript pollution:
1. **Run Scope Slug:** Every `WorkflowRun` has a deterministic state scope `StateScope::Workflow(run_scope_slug(&run.id))` where `run_scope_slug` extracts a valid `[a-z][a-z0-9_-]{0,63}` slug (`format!("run-{}", &sha256_hex(run.id.as_str())[..24])`).
2. **Curated `HandoffEntry` per Step Visit:**
   - When a step finishes an execution or sign-off, `record_step_handoff` writes a `HandoffEntry` under `StateScope::Workflow(run_scope_slug)` with slug `format!("step-{step_id}-v{visit_count}")`:
     - `summary`: single-line outcome summary (`<= 240` chars).
     - `shipped`: paths touched or gate pass summaries.
     - `in_flight`: active worktree branch (if any).
     - `blockers`: failure excerpt or `NeedsHuman` reason (if any).
     - `next_steps`: recommended next actions.
     - `notes`: structured evidence URNs (`attempt`, `gate_runs`, `record_id`) and optional operator/agent notes.
3. **Declared Runtime Facts (`write_workflow_fact`):**
   - `write_workflow_fact(state_store, def, run, key, value, at, by)` checks that `key` is present in `def.declared_keys`.
   - If `key` is not declared in `def.declared_keys`, returns `Err(WorkflowError::UndeclaredWorkflowKey { workflow: def.slug, key })` (`exit 2`).
   - Otherwise writes `StatePayload::Runtime(RuntimeEntry { value, merge_mode })` using the `MergeMode` declared in `def.declared_keys[key]` under `StateScope::Workflow(run_scope_slug)`.
4. **Prompt Assembly (`build_step_instruction`):**
   - Builds the step's instruction from:
     - `step_def.prompt`
     - Predecessor `HandoffEntry` summaries, blockers, and next steps (for all edges `(pred -> step_id)`)
     - Current declared `StateScope::Workflow(run_scope_slug)` runtime facts.
   - Never injects raw vendor JSONL transcripts from earlier steps.

### 3.3 Step Execution (`execute_workflow_step`)

Given a `WorkflowRun` and a ready `step_id` in `run.active_steps`:
1. **Agent Assignment:** If `step.assigned_agent` is `None`, invokes `select_step_agent`.
   - If `AssigneeRule::Manual` and still unassigned, returns a structured status indicating the step awaits `fl flow assign`.
2. **Offer-Only Mode (`--offer-only`):**
   - Sends a `MessageKind::TaskOffer` over `MessageStore` (and local socket push when available) addressed to `WireAddress::AgentName(assigned_agent)` with subject `format!("Workflow `{}` step `{}` ready", def.slug, step_id)` and body containing the assembled instruction, leaving the step in `State::Doing`.
3. **Adapter Execution Mode:**
   - Ensures the step has a bound `RecordId` in `Tracker` (creating a record titled `format!("{} / {}", run.title, step_def.title)` if `step.record_id` is `None`).
   - If `step_def.isolate_worktree` is `true`, creates or reuses a git branch `fl/flow/<run-short>/<step_id>` in a dedicated worktree under `<project_root>/.fl/worktrees/<run-short>-<step_id>`.
   - Runs the configured adapter (`AnyAdapter`) with `AttemptSpec`, appends the `Attempt` to `Ledger`, and flushes `decision::for_attempt`.
4. **Completion & Exit Gate Evaluation:**
   - If the attempt failed (`Crashed`, `Timeout`, `Refused`), records `StepOutcome::Fail`.
   - If the attempt completed (`AttemptStatus::Completed`):
     - `StepCompletion::Ungated`: records `StepOutcome::Pass`.
     - `StepCompletion::HumanSignOff { .. }`: transitions the step to `State::NeedsHuman` (`StepOutcome::NeedsHuman`) and sends an urgent `MessageKind::ApprovalRequest` FerroWire message to the target approver tier (`Lead`, `LabManager`, or `Owner`).
     - `StepCompletion::Gated { gates, regret }`: runs each gate via `run_single_gate` (applying `regret` staleness), flushes a `Decision` (`Outcome::Move` to `State::Done` when all gates pass, or `allowed: false` on failure), and produces `StepOutcome::Pass` when all gates pass or `StepOutcome::Fail` when any gate fails.
5. Writes the step's curated `HandoffEntry`, calls `advance_workflow`, and saves the updated `WorkflowRun` in `WorkflowStore`.

### 3.4 `fl/state` Branch Synchronization (`crates/exec/src/state_sync.rs`)

`sync_state` and `verify_state` in `crates/exec/src/state_sync.rs` are extended so stores implementing `WorkflowStore` also replicate workflow definitions and runs on `fl/state`:
- `workflows/<workflow_id_filename>.json`: canonical pretty-printed JSON of each `WorkflowDef`.
- `workflow_runs/<run_id_filename>.json`: canonical pretty-printed JSON of each `WorkflowRun`.
- During `sync_state`, remote `workflows/*.json` and `workflow_runs/*.json` files are imported (selecting the run with the newer `(total_steps_executed, updated_at)` on concurrent updates), and local definitions and runs are exported to the `fl/state` tree.

---

## 4. CLI (`fl flow`) and MCP Tools (`crates/cli/src/cmd/flow.rs`, `state_mcp.rs`)

### 4.1 `fl flow` CLI Subcommands

`crates/cli/src/main.rs` adds `Command::Flow { #[command(subcommand)] cmd: cmd::flow::FlowCmd }`:

1. `fl flow add --slug <slug> --title <title> --file <path> [--project <ref>] --by <author> [--json]`
   - Reads a `.toml` or `.json` workflow definition, resolves gate names/IRIs, validates with `validate_workflow_def`, stamps `authored_at_commit` from `Git::head`, and stores it via `upsert_workflow_def`.
2. `fl flow list [--project <ref>] [--json]`
   - Lists workflow definitions with their local numeric handle (`Kind::Workflow`), `slug`, `title`, step count, and budgets.
3. `fl flow show <workflow> [--project <ref>] [--json]`
   - Resolves `<workflow>` by handle, IRI, or slug and displays steps, edges, join policies, declared keys, and budgets.
4. `fl flow export <workflow> [--out <path>] [--project <ref>] [--json]`
   - Exports the workflow definition to `<out>` or `<project_root>/.fl/flows/<slug>.toml` using `write_atomic_non_symlink`.
5. `fl flow import <path> [--project <ref>] --by <author> [--json]`
   - Imports a `.fl/flows/<slug>.toml` (or `.json`) file into `fl-store`.
6. `fl flow start <workflow> --title <title> [--project <ref>] --by <author> [--json]`
   - Creates and persists a new `WorkflowRun` with `start_step` active.
7. `fl flow status [<run>] [--project <ref>] [--json]`
   - Without `<run>`, lists workflow runs with handles (`Kind::WorkflowRun`), status, active steps, total steps executed, tokens, and cost.
   - With `<run>`, shows full per-step status (`state`, `visit_count`, `attempt_count`, `assigned_agent`, `adapter_used`, `tokens_used`, `cost_usd_micros`, `last_outcome`, `needs_human_reason`).
8. `fl flow assign <run> <step_id> --agent <persona> [--adapter <vendor>] --by <author> [--json]`
   - Validates `ReviewConstraint` (`reviews_step` author and vendor separation) against the `AgentRegistry` and upstream step execution, exiting with code `2` on violation, and updates `step.assigned_agent` and `step.adapter_used`.
9. `fl flow step <run> [--step <step_id>] [--adapter <vendor>] [--binary <path>] [--offer-only] --by <author> [--json]`
   - Executes (or dispatches a FerroWire task offer for) the specified active step (or all currently ready `active_steps`).
10. `fl flow run <run> [--adapter <vendor>] [--binary <path>] --by <author> [--json]`
    - Repeatedly executes ready active steps until the run reaches `Completed`, `NeedsHuman`, or `Failed`. Exit code `0` on `Completed`, `1` on `Failed` or `NeedsHuman`, `2` on policy/validation refusal.
11. `fl flow approve <run> <step_id> --by <approver> [--note <text>] [--json]`
    - Verifies `authorize_step_sign_off` (exiting `2` on `SelfApprovalForbidden` or `InsufficientApprovalAuthority`), records a ledger `Decision` (`Outcome::Move` to `State::Done`), writes the curated `HandoffEntry`, and advances the step with `StepOutcome::Pass`.
12. `fl flow reject <run> <step_id> --by <approver> --reason <text> [--json]`
    - Verifies `authorize_step_sign_off` (exiting `2` on refusal), records a ledger `Decision`, writes the curated `HandoffEntry` with `--reason` in `blockers`, and advances the step with `StepOutcome::Fail` (triggering the `Fail` loop edge back to the implementation step if configured, or failing the run).

### 4.2 MCP Server Tools (`crates/cli/src/cmd/state_mcp.rs`)

`fl state mcp` exposes four additional tools in `tool_definitions()` and `dispatch_tool()`:
1. `workflow_list`: lists workflow definitions and workflow runs (optional `project` filter).
2. `workflow_status`: returns a `WorkflowRun`'s full JSON state plus all `HandoffEntry` and `RuntimeEntry` records in `StateScope::Workflow(run_scope_slug)`.
3. `workflow_handoff`: allows an agent executing a step to record a curated `HandoffEntry` (`summary`, `shipped`, `in_flight`, `blockers`, `next_steps`, `notes`) and optional declared runtime facts (`facts: { "<key>": <value> }`, validated against `def.declared_keys`).
4. `workflow_step`: allows an agent or orchestrator to record a step completion or advance a ready step (`run_id`, `step_id`, `outcome`, `by`).

### 4.3 Author-Owned PR Merge Authorization (`crates/core/src/pr_policy.rs`, `crates/cli/src/cmd/pr.rs`)

To match the workspace Author-Owned Merge rule (`.agents` commit `6c0e738`), `crates/core/src/pr_policy.rs` separates review authorization (`authorize_agent_action`) from merge authorization (`authorize_agent_merge(eval: &PrPolicyEvaluation, caller: &AgentRecord) -> Result<(), PolicyRefusal>`):
1. `authorize_agent_action` (used on `fl github pr review` and inside `verify_merge_approvals` when checking whether an agent's `FlReviewMarker` approval is valid) retains `SelfReviewForbidden` and tier checks so co-authors can never approve their own PRs.
2. `authorize_agent_merge` verifies:
   - `caller.status == AgentStatus::Active` (else `UnknownOrInactiveAgent`).
   - `eval.unattributed_commits.is_empty()` and `eval.unknown_co_authors.is_empty()`.
   - `caller` is a recorded commit co-author in `eval.co_authors` (author-owned merge across all tiers, including `Lead`, `LabManager`, and `Owner`-reviewed PRs touching protected paths once `lmcfarlin` approves on GitHub), OR `authorize_agent_action(eval, caller)` succeeds.
3. `PrCmd::Merge` in `crates/cli/src/cmd/pr.rs` calls `authorize_agent_merge(&eval, &caller)` followed by `verify_merge_approvals(&pr.head_sha, &eval, &reviews, &agents, DEFAULT_OWNER_LOGIN, &reviewer_logins)`.

---

## 5. Testing Requirements

1. **`fl-core` Unit Tests (`crates/core/src/workflow.rs`, `crates/core/tests/wire_refs.rs`):**
   - Wire round-trip and snake_case formatting for `Kind::Workflow`, `Kind::WorkflowRun`, `ApprovalTier`, `AssigneeRule`, `StepCompletion`, `JoinPolicy`, `EdgeCondition`, `WorkflowRunStatus`, and `StepOutcome`.
   - `validate_workflow_def` tests for duplicate step IDs, missing start step, self-review `reviews_step`, empty gate list in `StepCompletion::Gated`, invalid `NOfM` threshold, undeclared keys, and unreachable `"END"` (dead-end cycle detection).
   - `advance_workflow` tests for linear pass, conditional review -> fix -> review loop (`Pass` / `Fail` edges), in-step retry (`attempt_count < max_attempts` preserving completed parallel siblings), `max_visits` loop cap escalating to `NeedsHuman`, `max_total_steps` and `budget_usd_micros` / `max_tokens` ceilings escalating to `NeedsHuman`, and parallel fan-out + `All`, `Any`, and `NOfM` fan-in joins.
   - `select_step_agent` and `authorize_step_sign_off` tests verifying mandatory author!=reviewer separation, optional `require_different_vendor = true` separation, self-approval refusal, and `ApprovalTier::{Lead, LabManager, Owner}` enforcement.
2. **`fl-store` Conformance Tests (`crates/store/src/workflow.rs`):**
   - Shared conformance tests running against both `MemStore` and `RedbStore` verifying `FORMAT_WITH_WORKFLOW = 8` upgrade, `WorkflowId` and `WorkflowRunId` handle resolution, slug lookup, and run updates.
3. **`fl-exec` Integration Tests (`crates/exec/src/workflow.rs`, `crates/exec/src/state_sync.rs`):**
   - `.fl/flows/<slug>.toml` export/import round-trip with gate name resolution.
   - Curated `HandoffEntry` creation, `declared_keys` enforcement (rejecting undeclared keys), and downstream prompt construction without raw transcripts.
   - `fl/state` git sync replication of `workflows/` and `workflow_runs/` across two stores.
4. **`fl-cli` End-to-End Tests (`crates/cli/tests/workflow.rs`):**
   - Full CLI workflow lifecycle (with isolated `HOME` and removed ambient session env vars): `fl flow export` / `fl flow import`, `fl flow start`, gated step execution with fake adapter binary, review -> fix -> review loop with author/reviewer separation (`felicia` implements, `ferris` reviews), self-assign/self-approve refusal (`exit 2`), `fl flow approve` / `fl flow reject` ledger decision recording, and MCP `workflow_list` / `workflow_status` / `workflow_handoff` / `workflow_step` calls.
