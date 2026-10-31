//! Pure directed workflow graph domain model, static validator, deterministic
//! scheduler, review-independence agent selector, and tiered sign-off policy
//! (workflow spec §1).

use crate::agent::{AgentRecord, AgentRole, AgentStatus};
use crate::at::At;
use crate::ids::{GateId, ProjectId, RecordId, WorkflowId, WorkflowRunId};
use crate::iri::Iri;
use crate::model::{Regret, State};
use crate::pr_policy::DEFAULT_OWNER_LOGIN;
use crate::state::{MergeMode, validate_slug};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

/// Reserved terminal target name for workflow edges (`to_step = "END"`).
pub const END_STEP: &str = "END";

/// Default maximum total step executions across a single workflow run before escalating to `NeedsHuman`.
pub const DEFAULT_MAX_TOTAL_STEPS: u32 = 64;

/// Default maximum attempts for a single step visit before following a `Fail` edge or failing.
pub const DEFAULT_STEP_MAX_ATTEMPTS: u32 = 1;

/// Default maximum times a step may be visited in loops before escalating to `NeedsHuman`.
pub const DEFAULT_STEP_MAX_VISITS: u32 = 5;

fn default_max_total_steps() -> u32 {
    DEFAULT_MAX_TOTAL_STEPS
}

fn default_step_max_attempts() -> u32 {
    DEFAULT_STEP_MAX_ATTEMPTS
}

fn default_step_max_visits() -> u32 {
    DEFAULT_STEP_MAX_VISITS
}

/// Authority tier required to approve or reject a `NeedsHuman` workflow step (spec §1.2, §1.6).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTier {
    #[default]
    Owner,
    LabManager,
    Lead,
}

crate::wire::wire_names!(ApprovalTier as approval_tier_wire {
    Owner => "owner",
    LabManager => "lab_manager",
    Lead => "lead",
});
crate::wire::wire_parse!(ApprovalTier as approval_tier_parse);

impl fmt::Display for ApprovalTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// Rule for assigning an agent to execute a workflow step (spec §1.2).
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

/// Exit verification mode when a step attempt completes (spec §1.2).
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

/// Fan-in synchronization policy for steps with multiple incoming edges (spec §1.2).
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

/// Definition of a single node in a `WorkflowDef` graph (spec §1.2).
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

/// Condition under which a directed `EdgeDef` is traversed after a step finishes (spec §1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeCondition {
    Pass,
    Fail,
    NeedsHuman,
    Always,
}

crate::wire::wire_names!(EdgeCondition as edge_condition_wire {
    Pass => "pass",
    Fail => "fail",
    NeedsHuman => "needs_human",
    Always => "always",
});
crate::wire::wire_parse!(EdgeCondition as edge_condition_parse);

impl fmt::Display for EdgeCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// Directed edge between two steps (or from a step to `"END"`) in a `WorkflowDef` (spec §1.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeDef {
    pub from_step: String,
    pub to_step: String,
    pub on: EdgeCondition,
}

/// Complete directed workflow graph definition (spec §1.2).
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

/// High-level lifecycle status of a `WorkflowRun` (spec §1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Running,
    NeedsHuman,
    Completed,
    Failed,
    Canceled,
}

crate::wire::wire_names!(WorkflowRunStatus as workflow_run_status_wire {
    Running => "running",
    NeedsHuman => "needs_human",
    Completed => "completed",
    Failed => "failed",
    Canceled => "canceled",
});
crate::wire::wire_parse!(WorkflowRunStatus as workflow_run_status_parse);

impl WorkflowRunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

impl fmt::Display for WorkflowRunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// Outcome of a single workflow step attempt or sign-off (spec §1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Pass,
    Fail,
    NeedsHuman,
}

crate::wire::wire_names!(StepOutcome as step_outcome_wire {
    Pass => "pass",
    Fail => "fail",
    NeedsHuman => "needs_human",
});
crate::wire::wire_parse!(StepOutcome as step_outcome_parse);

impl fmt::Display for StepOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// Mutable runtime state of one step within a `WorkflowRun` (spec §1.4).
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

/// Persistent state of a single execution of a `WorkflowDef` (spec §1.4).
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

impl WorkflowRun {
    /// Initialize a new `WorkflowRun` from a validated `WorkflowDef` with `start_step` active
    /// at `visit_count = 1` and all steps in `State::Todo`.
    pub fn new(
        id: WorkflowRunId,
        def: &WorkflowDef,
        title: impl Into<String>,
        at: At,
        by: impl Into<String>,
    ) -> Result<Self, WorkflowError> {
        validate_workflow_def(def)?;
        let title = title.into();
        if title.trim().is_empty() {
            return Err(WorkflowError::EmptyTitle);
        }
        let mut steps = BTreeMap::new();
        for step_def in &def.steps {
            let visit_count = u32::from(step_def.step_id == def.start_step);
            steps.insert(
                step_def.step_id.clone(),
                StepInstance {
                    step_id: step_def.step_id.clone(),
                    visit_count,
                    attempt_count: 0,
                    state: State::Todo,
                    assigned_agent: None,
                    adapter_used: None,
                    record_id: None,
                    latest_attempt: None,
                    last_outcome: None,
                    tokens_used: 0,
                    cost_usd_micros: 0,
                    worktree_branch: None,
                    needs_human_reason: None,
                },
            );
        }
        Ok(Self {
            id,
            workflow_id: def.id.clone(),
            project: def.project.clone(),
            title,
            status: WorkflowRunStatus::Running,
            active_steps: vec![def.start_step.clone()],
            steps,
            total_steps_executed: 0,
            total_tokens: 0,
            total_cost_usd_micros: 0,
            created_at: at.clone(),
            updated_at: at,
            by: by.into(),
        })
    }
}

/// Input report fed into `advance_workflow` after an attempt or sign-off decision (spec §1.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepAttemptReport {
    pub step_id: String,
    pub outcome: StepOutcome,
    pub attempt_iri: Option<Iri>,
    pub tokens_used: u64,
    pub cost_usd_micros: u64,
    pub reason: Option<String>,
}

/// Agent and optional adapter selected for a step by `select_step_agent` (spec §1.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedStepAgent {
    pub agent_name: String,
    pub adapter: Option<String>,
}

/// Structured violation when a candidate reviewer conflicts with the step it reviews (spec §1.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ReviewIndependenceViolation {
    #[error(
        "review independence violation: step `{step_id}` reviews `{reviews_step}` and cannot be assigned to the same agent (`{agent}`)"
    )]
    SameAgent {
        step_id: String,
        reviews_step: String,
        agent: String,
    },
    #[error(
        "review independence violation: step `{step_id}` reviews `{reviews_step}` with `require_different_vendor = true` and cannot use the same adapter vendor (`{vendor}`)"
    )]
    SameVendor {
        step_id: String,
        reviews_step: String,
        vendor: String,
    },
}

/// Errors produced by workflow validation, scheduling, agent selection, or sign-off policy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkflowError {
    #[error("invalid workflow or step slug `{0}`")]
    InvalidSlug(String),
    #[error("workflow title must not be empty")]
    EmptyTitle,
    #[error("workflow definition must contain at least one step")]
    EmptySteps,
    #[error("step_id `{0}` is reserved and cannot be used as a step name")]
    ReservedStepId(String),
    #[error("duplicate step_id `{0}` in workflow definition")]
    DuplicateStepId(String),
    #[error("step `{step_id}` must have a non-empty title")]
    EmptyStepTitle { step_id: String },
    #[error("start_step `{0}` does not exist in workflow steps")]
    UnknownStartStep(String),
    #[error("invalid step or workflow limits: {detail}")]
    InvalidStepLimits { detail: String },
    #[error("edge `{from_step} -> {to_step}` references an unknown step")]
    UnknownEdgeStep { from_step: String, to_step: String },
    #[error("step `{step_id}` has invalid reviews_step target `{target}`")]
    InvalidReviewTarget { step_id: String, target: String },
    #[error(
        "step `{step_id}` has JoinPolicy::NOfM(required = {required}) with {incoming} incoming edge(s)"
    )]
    InvalidJoinThreshold {
        step_id: String,
        required: u32,
        incoming: usize,
    },
    #[error("invalid declared workflow runtime key `{0}`")]
    InvalidDeclaredKey(String),
    #[error(
        "step `{step_id}` uses StepCompletion::Gated with an empty gate list; an empty gate list verifies nothing"
    )]
    EmptyStepGates { step_id: String },
    #[error("step `{step_id}` is reachable from start_step but has no directed path to `END`")]
    UnreachableEnd { step_id: String },
    #[error("unknown workflow step `{0}`")]
    UnknownStep(String),
    #[error("step `{0}` is not currently active in this workflow run")]
    StepNotActive(String),
    #[error("workflow run is in terminal status `{0}` and cannot advance")]
    RunNotRunning(WorkflowRunStatus),
    #[error(transparent)]
    ReviewIndependence(#[from] ReviewIndependenceViolation),
    #[error("no eligible active agent found in registry for step `{step_id}`")]
    NoEligibleAgent { step_id: String },
    #[error(
        "self-approval forbidden: `{caller}` is assigned to step `{step_id}` and cannot approve or reject their own step"
    )]
    SelfApprovalForbidden { step_id: String, caller: String },
    #[error(
        "caller `{caller}` lacks required `{required}` approval authority for step `{step_id}`"
    )]
    InsufficientApprovalAuthority {
        step_id: String,
        caller: String,
        required: ApprovalTier,
    },
    #[error(
        "undeclared workflow runtime key `{key}` for workflow `{workflow}`; declare it in `declared_keys` first"
    )]
    UndeclaredWorkflowKey { workflow: String, key: String },
    #[error("workflow budget exhausted before step `{step_id}` could run: {detail}")]
    BudgetExhausted { step_id: String, detail: String },
}

impl WorkflowError {
    /// Whether this error represents a policy or static validation refusal (CLI exit code `2`).
    pub fn is_policy_refusal(&self) -> bool {
        matches!(
            self,
            Self::InvalidSlug(_)
                | Self::EmptyTitle
                | Self::EmptySteps
                | Self::ReservedStepId(_)
                | Self::DuplicateStepId(_)
                | Self::EmptyStepTitle { .. }
                | Self::UnknownStartStep(_)
                | Self::InvalidStepLimits { .. }
                | Self::UnknownEdgeStep { .. }
                | Self::InvalidReviewTarget { .. }
                | Self::InvalidJoinThreshold { .. }
                | Self::InvalidDeclaredKey(_)
                | Self::EmptyStepGates { .. }
                | Self::UnreachableEnd { .. }
                | Self::ReviewIndependence(_)
                | Self::NoEligibleAgent { .. }
                | Self::SelfApprovalForbidden { .. }
                | Self::InsufficientApprovalAuthority { .. }
                | Self::UndeclaredWorkflowKey { .. }
                | Self::BudgetExhausted { .. }
        )
    }
}

/// Validate all 10 static graph invariants on `def` (spec §1.3).
pub fn validate_workflow_def(def: &WorkflowDef) -> Result<(), WorkflowError> {
    // 1. Slug and title.
    let normalized_slug =
        validate_slug(&def.slug).map_err(|_| WorkflowError::InvalidSlug(def.slug.clone()))?;
    if normalized_slug != def.slug {
        return Err(WorkflowError::InvalidSlug(def.slug.clone()));
    }
    if def.title.trim().is_empty() {
        return Err(WorkflowError::EmptyTitle);
    }

    // 2. Non-empty steps, valid non-reserved step_ids, non-empty titles, no duplicates.
    if def.steps.is_empty() {
        return Err(WorkflowError::EmptySteps);
    }
    let mut step_ids = BTreeSet::new();
    for step in &def.steps {
        if step.step_id.eq_ignore_ascii_case(END_STEP) {
            return Err(WorkflowError::ReservedStepId(step.step_id.clone()));
        }
        let norm_step = validate_slug(&step.step_id)
            .map_err(|_| WorkflowError::InvalidSlug(step.step_id.clone()))?;
        if norm_step != step.step_id {
            return Err(WorkflowError::InvalidSlug(step.step_id.clone()));
        }
        if step.title.trim().is_empty() {
            return Err(WorkflowError::EmptyStepTitle {
                step_id: step.step_id.clone(),
            });
        }
        if !step_ids.insert(step.step_id.as_str()) {
            return Err(WorkflowError::DuplicateStepId(step.step_id.clone()));
        }
    }

    // 3. start_step exists.
    if !step_ids.contains(def.start_step.as_str()) {
        return Err(WorkflowError::UnknownStartStep(def.start_step.clone()));
    }

    // 4. Positive limits.
    if def.max_total_steps == 0 {
        return Err(WorkflowError::InvalidStepLimits {
            detail: "max_total_steps must be greater than 0".to_string(),
        });
    }
    for step in &def.steps {
        if step.max_attempts == 0 || step.max_visits == 0 {
            return Err(WorkflowError::InvalidStepLimits {
                detail: format!(
                    "step `{}` must have max_attempts > 0 and max_visits > 0",
                    step.step_id
                ),
            });
        }
    }

    // 5. Every edge connects known steps (or targets "END").
    for edge in &def.edges {
        let from_ok = step_ids.contains(edge.from_step.as_str());
        let to_ok = edge.to_step == END_STEP || step_ids.contains(edge.to_step.as_str());
        if !from_ok || !to_ok {
            return Err(WorkflowError::UnknownEdgeStep {
                from_step: edge.from_step.clone(),
                to_step: edge.to_step.clone(),
            });
        }
    }

    // 6–9. Per-step review target, join threshold, declared keys, and gate list checks.
    for step in &def.steps {
        if let Some(target) = &step.reviews_step
            && (target == &step.step_id || !step_ids.contains(target.as_str()))
        {
            return Err(WorkflowError::InvalidReviewTarget {
                step_id: step.step_id.clone(),
                target: target.clone(),
            });
        }
        if let JoinPolicy::NOfM { required } = step.join_policy {
            let incoming = def
                .edges
                .iter()
                .filter(|e| e.to_step == step.step_id)
                .count();
            if required == 0 || (required as usize) > incoming {
                return Err(WorkflowError::InvalidJoinThreshold {
                    step_id: step.step_id.clone(),
                    required,
                    incoming,
                });
            }
        }
        if let StepCompletion::Gated { gates, .. } = &step.completion
            && gates.is_empty()
        {
            return Err(WorkflowError::EmptyStepGates {
                step_id: step.step_id.clone(),
            });
        }
    }

    for key in def.declared_keys.keys() {
        let norm_key =
            validate_slug(key).map_err(|_| WorkflowError::InvalidDeclaredKey(key.clone()))?;
        if &norm_key != key {
            return Err(WorkflowError::InvalidDeclaredKey(key.clone()));
        }
    }

    // 10. Reachability: every step reachable from `start_step` (and `start_step` itself)
    // must be able to reach `END_STEP` along at least one directed path.
    let mut reachable_from_start = BTreeSet::new();
    let mut forward_queue = VecDeque::new();
    reachable_from_start.insert(def.start_step.as_str());
    forward_queue.push_back(def.start_step.as_str());
    while let Some(curr) = forward_queue.pop_front() {
        for edge in &def.edges {
            if edge.from_step == curr
                && edge.to_step != END_STEP
                && reachable_from_start.insert(edge.to_step.as_str())
            {
                forward_queue.push_back(edge.to_step.as_str());
            }
        }
    }

    let mut can_reach_end = BTreeSet::new();
    let mut reverse_queue = VecDeque::new();
    for edge in &def.edges {
        if edge.to_step == END_STEP && can_reach_end.insert(edge.from_step.as_str()) {
            reverse_queue.push_back(edge.from_step.as_str());
        }
    }
    while let Some(curr) = reverse_queue.pop_front() {
        for edge in &def.edges {
            if edge.to_step == curr && can_reach_end.insert(edge.from_step.as_str()) {
                reverse_queue.push_back(edge.from_step.as_str());
            }
        }
    }

    for step in &def.steps {
        let id = step.step_id.as_str();
        if reachable_from_start.contains(id) && !can_reach_end.contains(id) {
            return Err(WorkflowError::UnreachableEnd {
                step_id: step.step_id.clone(),
            });
        }
    }

    Ok(())
}

/// Validate that assigning `candidate_agent` (and optional `candidate_adapter`) to `step_id`
/// satisfies author and vendor separation from `step_def.reviews_step` (spec §1.5).
pub fn validate_reviewer_independence(
    def: &WorkflowDef,
    run: &WorkflowRun,
    step_id: &str,
    candidate_agent: &str,
    candidate_adapter: Option<&str>,
) -> Result<(), WorkflowError> {
    let step_def = def
        .steps
        .iter()
        .find(|s| s.step_id == step_id)
        .ok_or_else(|| WorkflowError::UnknownStep(step_id.to_string()))?;

    let Some(reviews_step) = &step_def.reviews_step else {
        return Ok(());
    };

    let upstream = run.steps.get(reviews_step);
    let candidate_key = candidate_agent.trim().to_ascii_lowercase();

    if let Some(upstream_agent) = upstream.and_then(|u| u.assigned_agent.as_deref())
        && upstream_agent.eq_ignore_ascii_case(&candidate_key)
    {
        return Err(ReviewIndependenceViolation::SameAgent {
            step_id: step_id.to_string(),
            reviews_step: reviews_step.clone(),
            agent: candidate_key,
        }
        .into());
    }

    if step_def.require_different_vendor {
        let chosen_adapter = candidate_adapter.or(match &step_def.assignee {
            AssigneeRule::Selector { adapter, .. } => adapter.as_deref(),
            _ => None,
        });
        if let (Some(upstream_vendor), Some(cand_vendor)) = (
            upstream.and_then(|u| u.adapter_used.as_deref()),
            chosen_adapter,
        ) && upstream_vendor.eq_ignore_ascii_case(cand_vendor.trim())
        {
            return Err(ReviewIndependenceViolation::SameVendor {
                step_id: step_id.to_string(),
                reviews_step: reviews_step.clone(),
                vendor: cand_vendor.trim().to_ascii_lowercase(),
            }
            .into());
        }
    }

    Ok(())
}

/// Select an active agent for `step_id` according to `step_def.assignee` while enforcing
/// review independence against `step_def.reviews_step` (spec §1.5).
pub fn select_step_agent(
    def: &WorkflowDef,
    run: &WorkflowRun,
    step_id: &str,
    agents: &[AgentRecord],
    override_adapter: Option<&str>,
) -> Result<Option<SelectedStepAgent>, WorkflowError> {
    let step_def = def
        .steps
        .iter()
        .find(|s| s.step_id == step_id)
        .ok_or_else(|| WorkflowError::UnknownStep(step_id.to_string()))?;

    match &step_def.assignee {
        AssigneeRule::Manual => Ok(None),
        AssigneeRule::Agent { name } => {
            let target = name.trim().to_ascii_lowercase();
            let agent = agents
                .iter()
                .find(|a| a.status == AgentStatus::Active && a.name_key == target)
                .ok_or_else(|| WorkflowError::NoEligibleAgent {
                    step_id: step_id.to_string(),
                })?;
            validate_reviewer_independence(def, run, step_id, &agent.name_key, override_adapter)?;
            Ok(Some(SelectedStepAgent {
                agent_name: agent.name_key.clone(),
                adapter: override_adapter.map(ToOwned::to_owned),
            }))
        }
        AssigneeRule::Selector {
            role,
            area,
            adapter,
        } => {
            let chosen_adapter = override_adapter.or(adapter.as_deref());
            let area_norm = area.as_ref().map(|a| a.trim().to_ascii_lowercase());
            let mut candidates: Vec<&AgentRecord> = agents
                .iter()
                .filter(|a| {
                    a.status == AgentStatus::Active
                        && role.as_ref().is_none_or(|r| &a.role == r)
                        && area_norm.as_deref().is_none_or(|ar| a.covers_area(ar))
                })
                .collect();
            candidates.sort_by(|a, b| a.name_key.cmp(&b.name_key));

            for candidate in candidates {
                match validate_reviewer_independence(
                    def,
                    run,
                    step_id,
                    &candidate.name_key,
                    chosen_adapter,
                ) {
                    Ok(()) => {
                        return Ok(Some(SelectedStepAgent {
                            agent_name: candidate.name_key.clone(),
                            adapter: chosen_adapter.map(ToOwned::to_owned),
                        }));
                    }
                    Err(WorkflowError::ReviewIndependence(
                        ReviewIndependenceViolation::SameAgent { .. },
                    )) => {
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }

            Err(WorkflowError::NoEligibleAgent {
                step_id: step_id.to_string(),
            })
        }
    }
}

/// Verify whether `caller` is authorized to approve or reject a step in `State::NeedsHuman`
/// (spec §1.6). Self-approval by `step_instance.assigned_agent` is always forbidden.
pub fn authorize_step_sign_off(
    project_slug: &str,
    step_def: &StepDef,
    step_instance: &StepInstance,
    caller: &str,
    agents: &[AgentRecord],
    owner_login: &str,
) -> Result<(), WorkflowError> {
    let caller_key = caller.trim().to_ascii_lowercase();
    let owner_key = if owner_login.trim().is_empty() {
        DEFAULT_OWNER_LOGIN.to_string()
    } else {
        owner_login.trim().to_ascii_lowercase()
    };

    if step_instance
        .assigned_agent
        .as_deref()
        .is_some_and(|assigned| assigned.eq_ignore_ascii_case(&caller_key))
    {
        return Err(WorkflowError::SelfApprovalForbidden {
            step_id: step_def.step_id.clone(),
            caller: caller_key,
        });
    }

    let required = match step_def.completion {
        StepCompletion::HumanSignOff { approval_tier } => approval_tier,
        _ => ApprovalTier::Owner,
    };

    if caller_key == owner_key || caller_key == "owner" {
        return Ok(());
    }

    let Some(agent) = agents
        .iter()
        .find(|a| a.status == AgentStatus::Active && a.name_key.eq_ignore_ascii_case(&caller_key))
    else {
        return Err(WorkflowError::InsufficientApprovalAuthority {
            step_id: step_def.step_id.clone(),
            caller: caller_key,
            required,
        });
    };

    let proj = project_slug.trim().to_ascii_lowercase();
    let authorized = match required {
        ApprovalTier::Owner => false,
        ApprovalTier::LabManager => agent.role == AgentRole::LabManager,
        ApprovalTier::Lead => {
            agent.role == AgentRole::LabManager
                || (agent.role == AgentRole::Lead && agent.covers_area(&proj))
        }
    };

    if !authorized {
        return Err(WorkflowError::InsufficientApprovalAuthority {
            step_id: step_def.step_id.clone(),
            caller: caller_key,
            required,
        });
    }

    Ok(())
}

fn edge_matches(on: EdgeCondition, outcome: StepOutcome) -> bool {
    match on {
        EdgeCondition::Always => true,
        EdgeCondition::Pass => outcome == StepOutcome::Pass,
        EdgeCondition::Fail => outcome == StepOutcome::Fail,
        EdgeCondition::NeedsHuman => outcome == StepOutcome::NeedsHuman,
    }
}

/// Why `step_id` may not start another attempt: the run has already used up a ceiling that
/// [`advance_workflow`] enforces only after an attempt has finished. `None` when it may run.
///
/// The ceilings are `>=` here and `>` there on purpose: `advance_workflow` lets the attempt that
/// reaches a limit complete and flags the one that crosses it, which means the crossing attempt
/// has already spent its budget. Refusing it up front keeps a run within its limits.
pub fn preflight_ceiling(def: &WorkflowDef, run: &WorkflowRun, step_id: &str) -> Option<String> {
    if run.total_steps_executed >= def.max_total_steps {
        return Some(format!(
            "workflow reached max_total_steps ({})",
            def.max_total_steps
        ));
    }
    if let Some(budget) = def.budget_usd_micros
        && run.total_cost_usd_micros >= budget
    {
        return Some(format!("workflow reached budget_usd_micros ({budget})"));
    }
    if let Some(max_tok) = def.max_tokens
        && run.total_tokens >= max_tok
    {
        return Some(format!("workflow reached max_tokens ({max_tok})"));
    }
    let step_def = def.steps.iter().find(|s| s.step_id == step_id)?;
    let step = run.steps.get(step_id)?;
    if let Some(step_budget) = step_def.budget_usd_micros
        && step.cost_usd_micros >= step_budget
    {
        return Some(format!(
            "step `{step_id}` reached budget_usd_micros ({step_budget})"
        ));
    }
    if let Some(step_max_tok) = step_def.max_tokens
        && step.tokens_used >= step_max_tok
    {
        return Some(format!(
            "step `{step_id}` reached max_tokens ({step_max_tok})"
        ));
    }
    None
}

/// Pure deterministic workflow step scheduler (spec §1.4).
pub fn advance_workflow(
    def: &WorkflowDef,
    run: &mut WorkflowRun,
    report: StepAttemptReport,
    at: At,
) -> Result<(), WorkflowError> {
    if run.status.is_terminal() {
        return Err(WorkflowError::RunNotRunning(run.status));
    }

    let step_def = def
        .steps
        .iter()
        .find(|s| s.step_id == report.step_id)
        .ok_or_else(|| WorkflowError::UnknownStep(report.step_id.clone()))?;

    if !run.active_steps.iter().any(|s| s == &report.step_id) {
        return Err(WorkflowError::StepNotActive(report.step_id.clone()));
    }

    let (
        was_needs_human,
        step_visit_count,
        step_attempt_count,
        step_tokens_used,
        step_cost_usd_micros,
    ) = {
        let step = run
            .steps
            .get_mut(&report.step_id)
            .ok_or_else(|| WorkflowError::UnknownStep(report.step_id.clone()))?;
        let was_nh = step.state == State::NeedsHuman;
        let is_sign_off_resolution = was_nh
            && step.attempt_count > 0
            && report.attempt_iri.is_none()
            && report.tokens_used == 0
            && report.cost_usd_micros == 0;

        if !is_sign_off_resolution {
            step.attempt_count += 1;
            run.total_steps_executed += 1;
        }
        step.tokens_used = step.tokens_used.saturating_add(report.tokens_used);
        run.total_tokens = run.total_tokens.saturating_add(report.tokens_used);
        step.cost_usd_micros = step.cost_usd_micros.saturating_add(report.cost_usd_micros);
        run.total_cost_usd_micros = run
            .total_cost_usd_micros
            .saturating_add(report.cost_usd_micros);
        if report.attempt_iri.is_some() {
            step.latest_attempt = report.attempt_iri.clone();
        }
        run.updated_at = at;

        (
            was_nh,
            step.visit_count,
            step.attempt_count,
            step.tokens_used,
            step.cost_usd_micros,
        )
    };

    // 2. Ceiling checks (escalate to NeedsHuman).
    let ceiling_reason = if run.total_steps_executed > def.max_total_steps {
        Some(format!(
            "workflow exceeded max_total_steps ({})",
            def.max_total_steps
        ))
    } else if let Some(budget) = def.budget_usd_micros
        && run.total_cost_usd_micros > budget
    {
        Some(format!("workflow exceeded budget_usd_micros ({budget})"))
    } else if let Some(max_tok) = def.max_tokens
        && run.total_tokens > max_tok
    {
        Some(format!("workflow exceeded max_tokens ({max_tok})"))
    } else if let Some(step_budget) = step_def.budget_usd_micros
        && step_cost_usd_micros > step_budget
    {
        Some(format!(
            "step `{}` exceeded budget_usd_micros ({step_budget})",
            step_def.step_id
        ))
    } else if let Some(step_max_tok) = step_def.max_tokens
        && step_tokens_used > step_max_tok
    {
        Some(format!(
            "step `{}` exceeded max_tokens ({step_max_tok})",
            step_def.step_id
        ))
    } else {
        None
    };

    if let Some(reason) = ceiling_reason {
        let step = run
            .steps
            .get_mut(&report.step_id)
            .expect("step checked above");
        step.state = State::NeedsHuman;
        step.last_outcome = Some(StepOutcome::NeedsHuman);
        step.needs_human_reason = Some(reason);
        run.status = WorkflowRunStatus::NeedsHuman;
        return Ok(());
    }

    // 3. In-step retry preservation on Fail (preserving completed parallel siblings).
    if !was_needs_human
        && report.outcome == StepOutcome::Fail
        && step_attempt_count < step_def.max_attempts
    {
        let step = run
            .steps
            .get_mut(&report.step_id)
            .expect("step checked above");
        step.state = State::Todo;
        step.last_outcome = Some(StepOutcome::Fail);
        step.needs_human_reason = report.reason;
        return Ok(());
    }

    // 4. HumanSignOff / NeedsHuman escalation routing.
    let requires_sign_off_hold = !was_needs_human
        && report.outcome == StepOutcome::Pass
        && matches!(step_def.completion, StepCompletion::HumanSignOff { .. });

    if report.outcome == StepOutcome::NeedsHuman || requires_sign_off_hold {
        let has_needs_human_edge = def
            .edges
            .iter()
            .any(|e| e.from_step == report.step_id && e.on == EdgeCondition::NeedsHuman);
        if !has_needs_human_edge {
            let step = run
                .steps
                .get_mut(&report.step_id)
                .expect("step checked above");
            step.state = State::NeedsHuman;
            step.last_outcome = Some(StepOutcome::NeedsHuman);
            step.needs_human_reason = report.reason.or_else(|| match step_def.completion {
                StepCompletion::HumanSignOff { approval_tier } => {
                    Some(format!("awaiting {} sign-off", approval_tier.as_wire()))
                }
                _ => Some("step requested human intervention".to_string()),
            });
            run.status = WorkflowRunStatus::NeedsHuman;
            return Ok(());
        }
    }

    // 5. Edge evaluation & fan-out / fan-in.
    let effective_outcome = if requires_sign_off_hold {
        StepOutcome::NeedsHuman
    } else {
        report.outcome
    };

    run.active_steps.retain(|s| s != &report.step_id);
    {
        let step = run
            .steps
            .get_mut(&report.step_id)
            .expect("step checked above");
        step.last_outcome = Some(effective_outcome);
        step.state = match effective_outcome {
            StepOutcome::Pass => State::Done,
            StepOutcome::Fail => State::Review,
            StepOutcome::NeedsHuman => State::NeedsHuman,
        };
        step.needs_human_reason = if effective_outcome == StepOutcome::NeedsHuman {
            report.reason
        } else {
            None
        };
    }

    if run.status == WorkflowRunStatus::NeedsHuman {
        let still_needs_human = run.active_steps.iter().any(|active_id| {
            run.steps
                .get(active_id)
                .is_some_and(|s| s.state == State::NeedsHuman)
        });
        if !still_needs_human {
            run.status = WorkflowRunStatus::Running;
        }
    }

    let matching_edges: Vec<&EdgeDef> = def
        .edges
        .iter()
        .filter(|e| e.from_step == report.step_id && edge_matches(e.on, effective_outcome))
        .collect();

    if matching_edges.is_empty() {
        if effective_outcome == StepOutcome::Fail && run.active_steps.is_empty() {
            run.status = WorkflowRunStatus::Failed;
        }
        return Ok(());
    }

    let mut saw_end = false;
    for edge in &matching_edges {
        if edge.to_step == END_STEP {
            saw_end = true;
            continue;
        }

        let target_def = def
            .steps
            .iter()
            .find(|s| s.step_id == edge.to_step)
            .expect("validate_workflow_def guarantees edge target exists");

        let join_satisfied = if matches!(edge.on, EdgeCondition::Fail | EdgeCondition::NeedsHuman) {
            true
        } else {
            let incoming_preds: Vec<&str> = def
                .edges
                .iter()
                .filter(|e| {
                    e.to_step == edge.to_step
                        && matches!(e.on, EdgeCondition::Pass | EdgeCondition::Always)
                })
                .map(|e| e.from_step.as_str())
                .collect();

            let done_count = incoming_preds
                .iter()
                .filter(|&&pred| {
                    pred == report.step_id
                        || run.steps.get(pred).is_some_and(|s| s.state == State::Done)
                })
                .count();

            let target_visit = run
                .steps
                .get(&edge.to_step)
                .map(|s| s.visit_count)
                .unwrap_or(0);

            match target_def.join_policy {
                JoinPolicy::All => done_count == incoming_preds.len(),
                JoinPolicy::Any => done_count >= 1 && target_visit < step_visit_count,
                JoinPolicy::NOfM { required } => {
                    done_count >= (required as usize) && target_visit < step_visit_count
                }
            }
        };

        if join_satisfied && !run.active_steps.iter().any(|s| s == &edge.to_step) {
            let target_step = run
                .steps
                .get_mut(&edge.to_step)
                .expect("target step exists in run.steps");
            target_step.visit_count += 1;
            target_step.attempt_count = 0;
            target_step.needs_human_reason = None;

            if target_step.visit_count > target_def.max_visits {
                target_step.state = State::NeedsHuman;
                target_step.last_outcome = Some(StepOutcome::NeedsHuman);
                target_step.needs_human_reason = Some(format!(
                    "step `{}` exceeded max_visits ({})",
                    edge.to_step, target_def.max_visits
                ));
                run.active_steps.push(edge.to_step.clone());
                run.status = WorkflowRunStatus::NeedsHuman;
            } else {
                target_step.state = State::Todo;
                run.active_steps.push(edge.to_step.clone());
            }
        }
    }

    if saw_end {
        // Prune any remaining active steps whose outgoing edges only feed into already-visited
        // Any/NOfM join steps so optional parallel branches do not block workflow completion.
        run.active_steps.retain(|active_id| {
            let outgoing: Vec<&EdgeDef> = def
                .edges
                .iter()
                .filter(|e| &e.from_step == active_id)
                .collect();
            if outgoing.is_empty() {
                return true;
            }
            !outgoing.iter().all(|e| {
                e.to_step != END_STEP
                    && def.steps.iter().any(|s| {
                        s.step_id == e.to_step
                            && matches!(s.join_policy, JoinPolicy::Any | JoinPolicy::NOfM { .. })
                    })
                    && run
                        .steps
                        .get(&e.to_step)
                        .is_some_and(|s| s.visit_count > 0 && s.state == State::Done)
            })
        });

        if run.active_steps.is_empty() && run.status == WorkflowRunStatus::Running {
            run.status = WorkflowRunStatus::Completed;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{AgentId, seq_iri};

    fn make_step(step_id: &str, title: &str) -> StepDef {
        StepDef {
            step_id: step_id.into(),
            title: title.into(),
            prompt: format!("Execute {step_id}"),
            assignee: AssigneeRule::Manual,
            reviews_step: None,
            require_different_vendor: false,
            completion: StepCompletion::Ungated,
            join_policy: JoinPolicy::All,
            max_attempts: 1,
            max_visits: 3,
            budget_usd_micros: None,
            max_tokens: None,
            isolate_worktree: false,
        }
    }

    fn base_workflow_def() -> WorkflowDef {
        let mut implement = make_step("implement", "Implement feature");
        implement.assignee = AssigneeRule::Agent {
            name: "felicia".into(),
        };
        implement.completion = StepCompletion::Gated {
            gates: vec![GateId(seq_iri(10))],
            regret: Regret::Low,
        };

        let mut review = make_step("review", "Review feature");
        review.reviews_step = Some("implement".into());
        review.require_different_vendor = true;
        review.assignee = AssigneeRule::Selector {
            role: Some(AgentRole::Lead),
            area: Some("ferroloop".into()),
            adapter: Some("codex".into()),
        };

        let mut declared_keys = BTreeMap::new();
        declared_keys.insert("branch".into(), MergeMode::LastWriter);

        WorkflowDef {
            id: WorkflowId(seq_iri(1)),
            project: ProjectId(seq_iri(2)),
            slug: "review-loop".into(),
            title: "Implement and Review".into(),
            start_step: "implement".into(),
            steps: vec![implement, review],
            edges: vec![
                EdgeDef {
                    from_step: "implement".into(),
                    to_step: "review".into(),
                    on: EdgeCondition::Pass,
                },
                EdgeDef {
                    from_step: "review".into(),
                    to_step: "implement".into(),
                    on: EdgeCondition::Fail,
                },
                EdgeDef {
                    from_step: "review".into(),
                    to_step: END_STEP.into(),
                    on: EdgeCondition::Pass,
                },
            ],
            max_total_steps: 16,
            budget_usd_micros: Some(5_000_000),
            max_tokens: Some(200_000),
            declared_keys,
            authored_at_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            authored_by: "ferris".into(),
        }
    }

    fn make_agent(
        seq: u64,
        name: &str,
        project: &str,
        role: AgentRole,
        extra_areas: &[&str],
    ) -> AgentRecord {
        let name_key = name.to_ascii_lowercase();
        AgentRecord {
            id: AgentId(seq_iri(seq)),
            name_key: name_key.clone(),
            display_name: name.to_string(),
            email: format!("{name_key}@artificialhumanity.io"),
            project: project.to_string(),
            role,
            extra_areas: extra_areas.iter().map(|s| (*s).to_string()).collect(),
            status: AgentStatus::Active,
            updated_at: At::from_unix_millis(1_000),
            by: "owner".to_string(),
        }
    }

    #[test]
    fn validate_workflow_def_accepts_valid_loop_and_fan_out_graphs_and_rejects_invalid_shapes() {
        let valid_loop = base_workflow_def();
        assert_eq!(validate_workflow_def(&valid_loop), Ok(()));

        // Valid fan-out / fan-in graph with NOfM join.
        let mut fan_out = base_workflow_def();
        let mut join = make_step("join", "Join results");
        join.join_policy = JoinPolicy::NOfM { required: 2 };
        fan_out.steps = vec![
            make_step("start", "Start"),
            make_step("branch_a", "Branch A"),
            make_step("branch_b", "Branch B"),
            join,
        ];
        fan_out.start_step = "start".into();
        fan_out.edges = vec![
            EdgeDef {
                from_step: "start".into(),
                to_step: "branch_a".into(),
                on: EdgeCondition::Pass,
            },
            EdgeDef {
                from_step: "start".into(),
                to_step: "branch_b".into(),
                on: EdgeCondition::Pass,
            },
            EdgeDef {
                from_step: "branch_a".into(),
                to_step: "join".into(),
                on: EdgeCondition::Pass,
            },
            EdgeDef {
                from_step: "branch_b".into(),
                to_step: "join".into(),
                on: EdgeCondition::Pass,
            },
            EdgeDef {
                from_step: "join".into(),
                to_step: END_STEP.into(),
                on: EdgeCondition::Pass,
            },
        ];
        assert_eq!(validate_workflow_def(&fan_out), Ok(()));

        // 1. Duplicate step ID
        let mut dup = base_workflow_def();
        dup.steps.push(make_step("implement", "Duplicate"));
        assert_eq!(
            validate_workflow_def(&dup),
            Err(WorkflowError::DuplicateStepId("implement".into()))
        );

        // 2. Reserved step ID ("END")
        let mut reserved = base_workflow_def();
        reserved.steps.push(make_step("END", "Reserved"));
        assert_eq!(
            validate_workflow_def(&reserved),
            Err(WorkflowError::ReservedStepId("END".into()))
        );

        // 3. Missing start step
        let mut bad_start = base_workflow_def();
        bad_start.start_step = "nonexistent".into();
        assert_eq!(
            validate_workflow_def(&bad_start),
            Err(WorkflowError::UnknownStartStep("nonexistent".into()))
        );

        // 4. Self-review `reviews_step`
        let mut self_review = base_workflow_def();
        self_review.steps[0].reviews_step = Some("implement".into());
        assert_eq!(
            validate_workflow_def(&self_review),
            Err(WorkflowError::InvalidReviewTarget {
                step_id: "implement".into(),
                target: "implement".into(),
            })
        );

        // 5. Empty gate list in StepCompletion::Gated
        let mut empty_gates = base_workflow_def();
        empty_gates.steps[0].completion = StepCompletion::Gated {
            gates: vec![],
            regret: Regret::Low,
        };
        assert_eq!(
            validate_workflow_def(&empty_gates),
            Err(WorkflowError::EmptyStepGates {
                step_id: "implement".into(),
            })
        );

        // 6. Invalid NOfM threshold (required > incoming edges)
        let mut bad_join = fan_out.clone();
        bad_join.steps[3].join_policy = JoinPolicy::NOfM { required: 3 };
        assert_eq!(
            validate_workflow_def(&bad_join),
            Err(WorkflowError::InvalidJoinThreshold {
                step_id: "join".into(),
                required: 3,
                incoming: 2,
            })
        );

        // 7. Invalid declared key
        let mut bad_key = base_workflow_def();
        bad_key
            .declared_keys
            .insert("123-bad-key".into(), MergeMode::LastWriter);
        assert_eq!(
            validate_workflow_def(&bad_key),
            Err(WorkflowError::InvalidDeclaredKey("123-bad-key".into()))
        );

        // 8. Dead-end step reachable from start_step unable to reach "END"
        let mut dead_end = base_workflow_def();
        dead_end.steps.push(make_step("stuck", "Stuck cycle"));
        dead_end.edges.push(EdgeDef {
            from_step: "review".into(),
            to_step: "stuck".into(),
            on: EdgeCondition::NeedsHuman,
        });
        dead_end.edges.push(EdgeDef {
            from_step: "stuck".into(),
            to_step: "stuck".into(),
            on: EdgeCondition::Always,
        });
        assert_eq!(
            validate_workflow_def(&dead_end),
            Err(WorkflowError::UnreachableEnd {
                step_id: "stuck".into(),
            })
        );
    }

    #[test]
    fn advance_workflow_executes_review_fix_review_loop_and_enforces_max_visits_cap() {
        let mut def = base_workflow_def();
        def.steps[0].max_visits = 2;
        def.steps[1].max_visits = 2;

        let mut run = WorkflowRun::new(
            WorkflowRunId(seq_iri(100)),
            &def,
            "Build feature",
            At::from_unix_millis(1_000),
            "ferris",
        )
        .unwrap();
        assert_eq!(run.active_steps, vec!["implement"]);
        assert_eq!(run.steps["implement"].visit_count, 1);

        // Visit 1 of implement -> Pass -> activates review (visit 1)
        advance_workflow(
            &def,
            &mut run,
            StepAttemptReport {
                step_id: "implement".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: Some(seq_iri(201)),
                tokens_used: 100,
                cost_usd_micros: 1_000,
                reason: None,
            },
            At::from_unix_millis(2_000),
        )
        .unwrap();
        assert_eq!(run.active_steps, vec!["review"]);
        assert_eq!(run.steps["implement"].state, State::Done);
        assert_eq!(run.steps["review"].visit_count, 1);
        assert_eq!(run.steps["review"].state, State::Todo);

        // Visit 1 of review -> Fail -> loops back to implement (visit 2)
        advance_workflow(
            &def,
            &mut run,
            StepAttemptReport {
                step_id: "review".into(),
                outcome: StepOutcome::Fail,
                attempt_iri: Some(seq_iri(202)),
                tokens_used: 50,
                cost_usd_micros: 500,
                reason: Some("Missing test coverage".into()),
            },
            At::from_unix_millis(3_000),
        )
        .unwrap();
        assert_eq!(run.active_steps, vec!["implement"]);
        assert_eq!(run.steps["implement"].visit_count, 2);
        assert_eq!(run.steps["implement"].attempt_count, 0);
        assert_eq!(run.steps["implement"].state, State::Todo);

        // Visit 2 of implement -> Pass -> activates review (visit 2)
        advance_workflow(
            &def,
            &mut run,
            StepAttemptReport {
                step_id: "implement".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: Some(seq_iri(203)),
                tokens_used: 120,
                cost_usd_micros: 1_200,
                reason: None,
            },
            At::from_unix_millis(4_000),
        )
        .unwrap();
        assert_eq!(run.active_steps, vec!["review"]);
        assert_eq!(run.steps["review"].visit_count, 2);

        // Clone run to test normal completion on Pass vs max_visits escalation on Fail.
        let mut passing_run = run.clone();
        advance_workflow(
            &def,
            &mut passing_run,
            StepAttemptReport {
                step_id: "review".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: Some(seq_iri(204)),
                tokens_used: 40,
                cost_usd_micros: 400,
                reason: None,
            },
            At::from_unix_millis(5_000),
        )
        .unwrap();
        assert_eq!(passing_run.status, WorkflowRunStatus::Completed);
        assert!(passing_run.active_steps.is_empty());
        assert_eq!(passing_run.total_steps_executed, 4);
        assert_eq!(passing_run.total_tokens, 310);
        assert_eq!(passing_run.total_cost_usd_micros, 3_100);

        // Failing review a second time tries to visit implement a 3rd time (> max_visits = 2) -> NeedsHuman!
        advance_workflow(
            &def,
            &mut run,
            StepAttemptReport {
                step_id: "review".into(),
                outcome: StepOutcome::Fail,
                attempt_iri: Some(seq_iri(205)),
                tokens_used: 40,
                cost_usd_micros: 400,
                reason: Some("Still broken".into()),
            },
            At::from_unix_millis(5_000),
        )
        .unwrap();
        assert_eq!(run.status, WorkflowRunStatus::NeedsHuman);
        assert_eq!(run.steps["implement"].visit_count, 3);
        assert_eq!(run.steps["implement"].state, State::NeedsHuman);
        assert!(
            run.steps["implement"]
                .needs_human_reason
                .as_deref()
                .unwrap()
                .contains("exceeded max_visits (2)")
        );
    }

    #[test]
    fn advance_workflow_in_step_retry_preserves_completed_parallel_siblings_and_fans_in() {
        let mut branch_b = make_step("branch_b", "Branch B");
        branch_b.max_attempts = 2;

        let def = WorkflowDef {
            id: WorkflowId(seq_iri(1)),
            project: ProjectId(seq_iri(2)),
            slug: "parallel-retry".into(),
            title: "Parallel Fan-Out and Retry".into(),
            start_step: "start".into(),
            steps: vec![
                make_step("start", "Start"),
                make_step("branch_a", "Branch A"),
                branch_b,
                make_step("join", "Join All"),
            ],
            edges: vec![
                EdgeDef {
                    from_step: "start".into(),
                    to_step: "branch_a".into(),
                    on: EdgeCondition::Pass,
                },
                EdgeDef {
                    from_step: "start".into(),
                    to_step: "branch_b".into(),
                    on: EdgeCondition::Pass,
                },
                EdgeDef {
                    from_step: "branch_a".into(),
                    to_step: "join".into(),
                    on: EdgeCondition::Pass,
                },
                EdgeDef {
                    from_step: "branch_b".into(),
                    to_step: "join".into(),
                    on: EdgeCondition::Pass,
                },
                EdgeDef {
                    from_step: "join".into(),
                    to_step: END_STEP.into(),
                    on: EdgeCondition::Pass,
                },
            ],
            max_total_steps: 16,
            budget_usd_micros: None,
            max_tokens: None,
            declared_keys: BTreeMap::new(),
            authored_at_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            authored_by: "ferris".into(),
        };

        let mut run = WorkflowRun::new(
            WorkflowRunId(seq_iri(101)),
            &def,
            "Parallel run",
            At::from_unix_millis(1_000),
            "ferris",
        )
        .unwrap();

        // Start passes -> fans out to branch_a and branch_b.
        advance_workflow(
            &def,
            &mut run,
            StepAttemptReport {
                step_id: "start".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 10,
                cost_usd_micros: 100,
                reason: None,
            },
            At::from_unix_millis(2_000),
        )
        .unwrap();
        assert_eq!(run.active_steps, vec!["branch_a", "branch_b"]);

        // branch_a passes -> marked Done, join not yet active.
        advance_workflow(
            &def,
            &mut run,
            StepAttemptReport {
                step_id: "branch_a".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 20,
                cost_usd_micros: 200,
                reason: None,
            },
            At::from_unix_millis(3_000),
        )
        .unwrap();
        assert_eq!(run.active_steps, vec!["branch_b"]);
        assert_eq!(run.steps["branch_a"].state, State::Done);

        // branch_b fails on attempt 1 of 2 -> stays active in Todo, branch_a stays Done!
        advance_workflow(
            &def,
            &mut run,
            StepAttemptReport {
                step_id: "branch_b".into(),
                outcome: StepOutcome::Fail,
                attempt_iri: None,
                tokens_used: 30,
                cost_usd_micros: 300,
                reason: Some("transient error".into()),
            },
            At::from_unix_millis(4_000),
        )
        .unwrap();
        assert_eq!(run.active_steps, vec!["branch_b"]);
        assert_eq!(run.steps["branch_b"].attempt_count, 1);
        assert_eq!(run.steps["branch_b"].state, State::Todo);
        assert_eq!(run.steps["branch_a"].state, State::Done);

        // branch_b passes on attempt 2 of 2 -> join activates!
        advance_workflow(
            &def,
            &mut run,
            StepAttemptReport {
                step_id: "branch_b".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 25,
                cost_usd_micros: 250,
                reason: None,
            },
            At::from_unix_millis(5_000),
        )
        .unwrap();
        assert_eq!(run.active_steps, vec!["join"]);
        assert_eq!(run.steps["join"].visit_count, 1);
    }

    #[test]
    fn advance_workflow_join_policies_any_and_n_of_m_activate_join_step_at_threshold() {
        let make_fan_in_def = |policy: JoinPolicy| {
            let mut join = make_step("join", "Join");
            join.join_policy = policy;
            WorkflowDef {
                id: WorkflowId(seq_iri(1)),
                project: ProjectId(seq_iri(2)),
                slug: "fan-in".into(),
                title: "Fan-In Policy".into(),
                start_step: "start".into(),
                steps: vec![
                    make_step("start", "Start"),
                    make_step("a", "A"),
                    make_step("b", "B"),
                    make_step("c", "C"),
                    join,
                ],
                edges: vec![
                    EdgeDef {
                        from_step: "start".into(),
                        to_step: "a".into(),
                        on: EdgeCondition::Pass,
                    },
                    EdgeDef {
                        from_step: "start".into(),
                        to_step: "b".into(),
                        on: EdgeCondition::Pass,
                    },
                    EdgeDef {
                        from_step: "start".into(),
                        to_step: "c".into(),
                        on: EdgeCondition::Pass,
                    },
                    EdgeDef {
                        from_step: "a".into(),
                        to_step: "join".into(),
                        on: EdgeCondition::Pass,
                    },
                    EdgeDef {
                        from_step: "b".into(),
                        to_step: "join".into(),
                        on: EdgeCondition::Pass,
                    },
                    EdgeDef {
                        from_step: "c".into(),
                        to_step: "join".into(),
                        on: EdgeCondition::Pass,
                    },
                    EdgeDef {
                        from_step: "join".into(),
                        to_step: END_STEP.into(),
                        on: EdgeCondition::Pass,
                    },
                ],
                max_total_steps: 16,
                budget_usd_micros: None,
                max_tokens: None,
                declared_keys: BTreeMap::new(),
                authored_at_commit: "0123456789abcdef0123456789abcdef01234567".into(),
                authored_by: "ferris".into(),
            }
        };

        // 1. JoinPolicy::Any activates join as soon as `a` completes.
        let def_any = make_fan_in_def(JoinPolicy::Any);
        let mut run_any = WorkflowRun::new(
            WorkflowRunId(seq_iri(102)),
            &def_any,
            "Any join",
            At::from_unix_millis(1_000),
            "ferris",
        )
        .unwrap();
        advance_workflow(
            &def_any,
            &mut run_any,
            StepAttemptReport {
                step_id: "start".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 0,
                cost_usd_micros: 0,
                reason: None,
            },
            At::from_unix_millis(2_000),
        )
        .unwrap();
        advance_workflow(
            &def_any,
            &mut run_any,
            StepAttemptReport {
                step_id: "a".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 0,
                cost_usd_micros: 0,
                reason: None,
            },
            At::from_unix_millis(3_000),
        )
        .unwrap();
        assert!(run_any.active_steps.contains(&"join".to_string()));
        assert_eq!(run_any.steps["join"].visit_count, 1);

        // 2. JoinPolicy::NOfM { required: 2 } waits on `a`, activates on `b`, and does not re-trigger on `c`.
        let def_nofm = make_fan_in_def(JoinPolicy::NOfM { required: 2 });
        let mut run_nofm = WorkflowRun::new(
            WorkflowRunId(seq_iri(103)),
            &def_nofm,
            "2 of 3 join",
            At::from_unix_millis(1_000),
            "ferris",
        )
        .unwrap();
        advance_workflow(
            &def_nofm,
            &mut run_nofm,
            StepAttemptReport {
                step_id: "start".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 0,
                cost_usd_micros: 0,
                reason: None,
            },
            At::from_unix_millis(2_000),
        )
        .unwrap();
        advance_workflow(
            &def_nofm,
            &mut run_nofm,
            StepAttemptReport {
                step_id: "a".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 0,
                cost_usd_micros: 0,
                reason: None,
            },
            At::from_unix_millis(3_000),
        )
        .unwrap();
        assert!(!run_nofm.active_steps.contains(&"join".to_string()));

        advance_workflow(
            &def_nofm,
            &mut run_nofm,
            StepAttemptReport {
                step_id: "b".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 0,
                cost_usd_micros: 0,
                reason: None,
            },
            At::from_unix_millis(4_000),
        )
        .unwrap();
        assert!(run_nofm.active_steps.contains(&"join".to_string()));
        assert_eq!(run_nofm.steps["join"].visit_count, 1);

        // Completing join finishes the workflow cleanly.
        advance_workflow(
            &def_nofm,
            &mut run_nofm,
            StepAttemptReport {
                step_id: "join".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 0,
                cost_usd_micros: 0,
                reason: None,
            },
            At::from_unix_millis(5_000),
        )
        .unwrap();
        assert_eq!(run_nofm.status, WorkflowRunStatus::Completed);
    }

    #[test]
    fn advance_workflow_escalates_to_needs_human_on_step_and_workflow_budget_token_and_step_ceilings()
     {
        // 1. Step budget ceiling
        let mut def_step_budget = base_workflow_def();
        def_step_budget.steps[0].budget_usd_micros = Some(500);
        let mut run1 = WorkflowRun::new(
            WorkflowRunId(seq_iri(110)),
            &def_step_budget,
            "Step budget",
            At::from_unix_millis(1_000),
            "ferris",
        )
        .unwrap();
        advance_workflow(
            &def_step_budget,
            &mut run1,
            StepAttemptReport {
                step_id: "implement".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 10,
                cost_usd_micros: 600,
                reason: None,
            },
            At::from_unix_millis(2_000),
        )
        .unwrap();
        assert_eq!(run1.status, WorkflowRunStatus::NeedsHuman);
        assert_eq!(run1.steps["implement"].state, State::NeedsHuman);
        assert!(
            run1.steps["implement"]
                .needs_human_reason
                .as_deref()
                .unwrap()
                .contains("step `implement` exceeded budget_usd_micros")
        );

        // 2. Workflow token ceiling
        let mut def_wf_tokens = base_workflow_def();
        def_wf_tokens.max_tokens = Some(1_000);
        let mut run2 = WorkflowRun::new(
            WorkflowRunId(seq_iri(111)),
            &def_wf_tokens,
            "Workflow tokens",
            At::from_unix_millis(1_000),
            "ferris",
        )
        .unwrap();
        advance_workflow(
            &def_wf_tokens,
            &mut run2,
            StepAttemptReport {
                step_id: "implement".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 1_500,
                cost_usd_micros: 100,
                reason: None,
            },
            At::from_unix_millis(2_000),
        )
        .unwrap();
        assert_eq!(run2.status, WorkflowRunStatus::NeedsHuman);
        assert!(
            run2.steps["implement"]
                .needs_human_reason
                .as_deref()
                .unwrap()
                .contains("workflow exceeded max_tokens")
        );

        // 3. Workflow max_total_steps ceiling
        let mut def_max_steps = base_workflow_def();
        def_max_steps.max_total_steps = 1;
        let mut run3 = WorkflowRun::new(
            WorkflowRunId(seq_iri(112)),
            &def_max_steps,
            "Max steps",
            At::from_unix_millis(1_000),
            "ferris",
        )
        .unwrap();
        advance_workflow(
            &def_max_steps,
            &mut run3,
            StepAttemptReport {
                step_id: "implement".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 10,
                cost_usd_micros: 10,
                reason: None,
            },
            At::from_unix_millis(2_000),
        )
        .unwrap();
        assert_eq!(run3.status, WorkflowRunStatus::Running);
        advance_workflow(
            &def_max_steps,
            &mut run3,
            StepAttemptReport {
                step_id: "review".into(),
                outcome: StepOutcome::Pass,
                attempt_iri: None,
                tokens_used: 10,
                cost_usd_micros: 10,
                reason: None,
            },
            At::from_unix_millis(3_000),
        )
        .unwrap();
        assert_eq!(run3.status, WorkflowRunStatus::NeedsHuman);
        assert!(
            run3.steps["review"]
                .needs_human_reason
                .as_deref()
                .unwrap()
                .contains("workflow exceeded max_total_steps (1)")
        );
    }

    #[test]
    fn select_step_agent_and_validate_reviewer_independence_enforce_author_and_vendor_separation() {
        let mut def = base_workflow_def();
        def.steps[1].assignee = AssigneeRule::Selector {
            role: None,
            area: Some("ferroloop".into()),
            adapter: Some("codex".into()),
        };

        let agents = vec![
            make_agent(1, "Felicia", "ferroloop", AgentRole::Assistant, &[]),
            make_agent(2, "Ferris", "ferroloop", AgentRole::Lead, &[]),
        ];

        let mut run = WorkflowRun::new(
            WorkflowRunId(seq_iri(120)),
            &def,
            "Review independence",
            At::from_unix_millis(1_000),
            "ferris",
        )
        .unwrap();

        // Select agent for `implement` (explicitly assigned to felicia).
        let selected_impl =
            select_step_agent(&def, &run, "implement", &agents, Some("claude")).unwrap();
        assert_eq!(
            selected_impl,
            Some(SelectedStepAgent {
                agent_name: "felicia".into(),
                adapter: Some("claude".into()),
            })
        );

        // Record that felicia executed `implement` using `claude`.
        let impl_step = run.steps.get_mut("implement").unwrap();
        impl_step.assigned_agent = Some("felicia".into());
        impl_step.adapter_used = Some("claude".into());

        // 1. Directly validating `felicia` for `review` fails with SameAgent.
        assert_eq!(
            validate_reviewer_independence(&def, &run, "review", "felicia", Some("codex")),
            Err(WorkflowError::ReviewIndependence(
                ReviewIndependenceViolation::SameAgent {
                    step_id: "review".into(),
                    reviews_step: "implement".into(),
                    agent: "felicia".into(),
                }
            ))
        );

        // 2. Validating `ferris` with the same vendor (`claude`) fails with SameVendor.
        assert_eq!(
            validate_reviewer_independence(&def, &run, "review", "ferris", Some("claude")),
            Err(WorkflowError::ReviewIndependence(
                ReviewIndependenceViolation::SameVendor {
                    step_id: "review".into(),
                    reviews_step: "implement".into(),
                    vendor: "claude".into(),
                }
            ))
        );

        // 3. `select_step_agent` skips `felicia` (alphabetically first!) and picks `ferris` with `codex`.
        let selected_review = select_step_agent(&def, &run, "review", &agents, None).unwrap();
        assert_eq!(
            selected_review,
            Some(SelectedStepAgent {
                agent_name: "ferris".into(),
                adapter: Some("codex".into()),
            })
        );

        // 4. Overriding adapter to `claude` fails with SameVendor.
        assert_eq!(
            select_step_agent(&def, &run, "review", &agents, Some("claude")),
            Err(WorkflowError::ReviewIndependence(
                ReviewIndependenceViolation::SameVendor {
                    step_id: "review".into(),
                    reviews_step: "implement".into(),
                    vendor: "claude".into(),
                }
            ))
        );
    }

    #[test]
    fn authorize_step_sign_off_forbids_self_approval_and_enforces_lead_lab_manager_and_owner_tiers()
    {
        let agents = vec![
            make_agent(1, "Jarvis", "ai-lab-amd", AgentRole::LabManager, &[]),
            make_agent(2, "Ferris", "ferroloop", AgentRole::Lead, &[]),
            make_agent(3, "Felicia", "ferroloop", AgentRole::Assistant, &[]),
            make_agent(4, "Sonya", "sonora", AgentRole::Lead, &[]),
        ];

        let mut step_def = make_step("review", "Review");
        step_def.completion = StepCompletion::HumanSignOff {
            approval_tier: ApprovalTier::Lead,
        };

        let step_instance = StepInstance {
            step_id: "review".into(),
            visit_count: 1,
            attempt_count: 1,
            state: State::NeedsHuman,
            assigned_agent: Some("felicia".into()),
            adapter_used: Some("claude".into()),
            record_id: None,
            latest_attempt: None,
            last_outcome: Some(StepOutcome::NeedsHuman),
            tokens_used: 0,
            cost_usd_micros: 0,
            worktree_branch: None,
            needs_human_reason: Some("awaiting lead sign-off".into()),
        };

        // 1. Self-approval by assigned agent (`felicia`) is forbidden.
        assert_eq!(
            authorize_step_sign_off(
                "ferroloop",
                &step_def,
                &step_instance,
                "felicia",
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Err(WorkflowError::SelfApprovalForbidden {
                step_id: "review".into(),
                caller: "felicia".into(),
            })
        );

        // 2. Even if `ferris` (Lead) is assigned to the step, `ferris` cannot self-approve it!
        let mut ferris_assigned = step_instance.clone();
        ferris_assigned.assigned_agent = Some("ferris".into());
        assert_eq!(
            authorize_step_sign_off(
                "ferroloop",
                &step_def,
                &ferris_assigned,
                "ferris",
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Err(WorkflowError::SelfApprovalForbidden {
                step_id: "review".into(),
                caller: "ferris".into(),
            })
        );

        // 3. Lead of another project (`sonya`) cannot approve a `ferroloop` Lead-tier step.
        assert_eq!(
            authorize_step_sign_off(
                "ferroloop",
                &step_def,
                &step_instance,
                "sonya",
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Err(WorkflowError::InsufficientApprovalAuthority {
                step_id: "review".into(),
                caller: "sonya".into(),
                required: ApprovalTier::Lead,
            })
        );

        // 4. Project Lead (`ferris`), LabManager (`jarvis`), and Owner (`lmcfarlin`) can approve Lead-tier step.
        assert_eq!(
            authorize_step_sign_off(
                "ferroloop",
                &step_def,
                &step_instance,
                "ferris",
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Ok(())
        );
        assert_eq!(
            authorize_step_sign_off(
                "ferroloop",
                &step_def,
                &step_instance,
                "jarvis",
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Ok(())
        );
        assert_eq!(
            authorize_step_sign_off(
                "ferroloop",
                &step_def,
                &step_instance,
                DEFAULT_OWNER_LOGIN,
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Ok(())
        );

        // 5. LabManager-tier step refuses Project Lead (`ferris`) and accepts `jarvis` and Owner.
        step_def.completion = StepCompletion::HumanSignOff {
            approval_tier: ApprovalTier::LabManager,
        };
        assert_eq!(
            authorize_step_sign_off(
                "ferroloop",
                &step_def,
                &step_instance,
                "ferris",
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Err(WorkflowError::InsufficientApprovalAuthority {
                step_id: "review".into(),
                caller: "ferris".into(),
                required: ApprovalTier::LabManager,
            })
        );
        assert_eq!(
            authorize_step_sign_off(
                "ferroloop",
                &step_def,
                &step_instance,
                "jarvis",
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Ok(())
        );

        // 6. Owner-tier step (or ceiling escalation on an Ungated/Gated step) refuses `jarvis` and accepts Owner.
        step_def.completion = StepCompletion::Ungated;
        assert_eq!(
            authorize_step_sign_off(
                "ferroloop",
                &step_def,
                &step_instance,
                "jarvis",
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Err(WorkflowError::InsufficientApprovalAuthority {
                step_id: "review".into(),
                caller: "jarvis".into(),
                required: ApprovalTier::Owner,
            })
        );
        assert_eq!(
            authorize_step_sign_off(
                "ferroloop",
                &step_def,
                &step_instance,
                "lmcfarlin",
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Ok(())
        );
    }

    #[test]
    fn preflight_ceiling_refuses_a_step_once_any_limit_is_reached_not_after_it_is_crossed() {
        let mut def = base_workflow_def();
        let mut run = WorkflowRun::new(
            WorkflowRunId(seq_iri(120)),
            &def,
            "Preflight",
            At::from_unix_millis(1_000),
            "ferris",
        )
        .unwrap();
        assert_eq!(preflight_ceiling(&def, &run, "implement"), None);

        // Steps: one below the limit may run, the limit itself may not.
        def.max_total_steps = 3;
        run.total_steps_executed = 2;
        assert_eq!(preflight_ceiling(&def, &run, "implement"), None);
        run.total_steps_executed = 3;
        assert!(
            preflight_ceiling(&def, &run, "implement")
                .unwrap()
                .contains("max_total_steps (3)")
        );
        run.total_steps_executed = 0;

        // Workflow cost and tokens.
        def.budget_usd_micros = Some(100);
        run.total_cost_usd_micros = 99;
        assert_eq!(preflight_ceiling(&def, &run, "implement"), None);
        run.total_cost_usd_micros = 100;
        assert!(
            preflight_ceiling(&def, &run, "implement")
                .unwrap()
                .contains("budget_usd_micros")
        );
        run.total_cost_usd_micros = 0;
        def.max_tokens = Some(50);
        run.total_tokens = 50;
        assert!(
            preflight_ceiling(&def, &run, "implement")
                .unwrap()
                .contains("max_tokens (50)")
        );
        run.total_tokens = 0;

        // Per-step limits apply to that step only.
        def.steps[0].max_tokens = Some(10);
        run.steps.get_mut("implement").unwrap().tokens_used = 10;
        assert!(
            preflight_ceiling(&def, &run, "implement")
                .unwrap()
                .contains("step `implement`")
        );
        assert_eq!(preflight_ceiling(&def, &run, "review"), None);
        // An unknown step has no step limits to hit.
        assert_eq!(preflight_ceiling(&def, &run, "nope"), None);
    }
}
