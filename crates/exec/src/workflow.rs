//! Workflow execution runner, curated step handoffs (`StateScope::Workflow`),
//! and portable `.fl/flows/<slug>.toml` manifest export/import (workflow spec §3).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use fl_core::agent::{AgentRole, AgentStatus};
use fl_core::at::At;
use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
use fl_core::ids::{MessageId, ProjectId, WorkflowId, WorkflowRunId};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, AttemptStatus, PathsTouched};
use fl_core::model::{Regret, State};
use fl_core::pr_policy::DEFAULT_OWNER_LOGIN;
use fl_core::state::{
    HandoffEntry, MAX_SUMMARY_CHARS, MergeMode, RuntimeEntry, SensitivityPatch, StateConcern,
    StateEntry, StatePayload, StateScope, UpsertStateArgs, validate_slug,
};
use fl_core::store::{
    AgentRegistry, Catalog, Ledger, MessageStore, Roles, StateStore, StoreError, Tracker,
    WorkflowStore,
};
use fl_core::telemetry::{AdapterKind, RateCard};
use fl_core::workflow::{
    ApprovalTier, AssigneeRule, DEFAULT_MAX_TOTAL_STEPS, DEFAULT_STEP_MAX_ATTEMPTS,
    DEFAULT_STEP_MAX_VISITS, EdgeDef, JoinPolicy, StepAttemptReport, StepCompletion, StepDef,
    StepOutcome, WorkflowDef, WorkflowError, WorkflowRun, WorkflowRunStatus, advance_workflow,
    authorize_step_sign_off, preflight_ceiling, validate_workflow_def,
};
pub use fl_core::workflow::{SelectedStepAgent, select_step_agent, validate_reviewer_independence};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::adapters::AnyAdapter;
use crate::decision;
use crate::evaluate::{GateReport, TransitionReport, run_single_gate_with_regret};
use crate::population::{ExecError, refused_publish};
use crate::runner::{AttemptError, AttemptSpec, Runner};
use crate::stamp;

/// Serialises the read-modify-write that commits a step's result to its stored [`WorkflowRun`].
///
/// A step runs for minutes between loading the run and committing, and sibling fan-out branches
/// commit to the same stored run. Every commit therefore re-reads the run under this lock and
/// applies only its own step's changes (see `execute_workflow_step`). It covers threads in one
/// process, which is the only way two commits can overlap: the embedded store admits one
/// process at a time.
static RUN_COMMIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_run_commits() -> std::sync::MutexGuard<'static, ()> {
    RUN_COMMIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Relative directory inside a project root holding shared workflow definitions.
pub const FLOWS_DIR: &str = ".fl/flows";

fn default_max_total_steps() -> u32 {
    DEFAULT_MAX_TOTAL_STEPS
}

fn default_step_max_attempts() -> u32 {
    DEFAULT_STEP_MAX_ATTEMPTS
}

fn default_step_max_visits() -> u32 {
    DEFAULT_STEP_MAX_VISITS
}

/// Portable representation of [`StepCompletion`] in `.fl/flows/<slug>.toml`, allowing
/// gates to be referenced by either gate name (`"cargo-test"`) or `urn:uuid:...` IRI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum StepCompletionManifest {
    Gated {
        gates: Vec<String>,
        #[serde(default)]
        regret: Regret,
    },
    HumanSignOff {
        #[serde(default)]
        approval_tier: ApprovalTier,
    },
    #[default]
    Ungated,
}

/// Portable representation of [`StepDef`] in `.fl/flows/<slug>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepManifest {
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
    pub completion: StepCompletionManifest,
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

/// Portable representation of [`WorkflowDef`] serialized to `.fl/flows/<slug>.toml` (spec §3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowManifest {
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub title: String,
    pub start_step: String,
    #[serde(default = "default_max_total_steps")]
    pub max_total_steps: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub authored_at_commit: String,
    #[serde(default)]
    pub authored_by: String,
    #[serde(default)]
    pub declared_keys: BTreeMap<String, MergeMode>,
    pub steps: Vec<StepManifest>,
    pub edges: Vec<EdgeDef>,
}

/// Convert a [`WorkflowDef`] into a portable [`WorkflowManifest`], mapping gate IRIs
/// to gate names when unambiguous in `catalog`.
pub fn export_workflow_manifest(
    catalog: &dyn Catalog,
    def: &WorkflowDef,
) -> Result<WorkflowManifest, ExecError> {
    let gates = catalog
        .list_gates(&def.project)
        .map_err(|e| ExecError::Store(e.to_string()))?;

    let mut name_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for g in &gates {
        *name_counts.entry(g.name.as_str()).or_insert(0) += 1;
    }

    let steps = def
        .steps
        .iter()
        .map(|s| {
            let completion = match &s.completion {
                StepCompletion::Gated {
                    gates: step_gates,
                    regret,
                } => {
                    let mapped = step_gates
                        .iter()
                        .map(|gid| {
                            if let Some(found) = gates.iter().find(|g| &g.id == gid)
                                && name_counts.get(found.name.as_str()).copied().unwrap_or(0) == 1
                            {
                                found.name.clone()
                            } else {
                                gid.to_string()
                            }
                        })
                        .collect();
                    StepCompletionManifest::Gated {
                        gates: mapped,
                        regret: *regret,
                    }
                }
                StepCompletion::HumanSignOff { approval_tier } => {
                    StepCompletionManifest::HumanSignOff {
                        approval_tier: *approval_tier,
                    }
                }
                StepCompletion::Ungated => StepCompletionManifest::Ungated,
            };
            StepManifest {
                step_id: s.step_id.clone(),
                title: s.title.clone(),
                prompt: s.prompt.clone(),
                assignee: s.assignee.clone(),
                reviews_step: s.reviews_step.clone(),
                require_different_vendor: s.require_different_vendor,
                completion,
                join_policy: s.join_policy,
                max_attempts: s.max_attempts,
                max_visits: s.max_visits,
                budget_usd_micros: s.budget_usd_micros,
                max_tokens: s.max_tokens,
                isolate_worktree: s.isolate_worktree,
            }
        })
        .collect();

    Ok(WorkflowManifest {
        slug: def.slug.clone(),
        title: def.title.clone(),
        start_step: def.start_step.clone(),
        max_total_steps: def.max_total_steps,
        budget_usd_micros: def.budget_usd_micros,
        max_tokens: def.max_tokens,
        authored_at_commit: def.authored_at_commit.clone(),
        authored_by: def.authored_by.clone(),
        declared_keys: def.declared_keys.clone(),
        steps,
        edges: def.edges.clone(),
    })
}

/// Serialize `def` to pretty-printed TOML for `.fl/flows/<slug>.toml` (spec §3.1).
pub fn export_workflow_toml(catalog: &dyn Catalog, def: &WorkflowDef) -> Result<String, ExecError> {
    let manifest = export_workflow_manifest(catalog, def)?;
    toml::to_string_pretty(&manifest)
        .map_err(|e| ExecError::Store(format!("failed to serialize workflow TOML: {e}")))
}

/// Parse a `.toml` or `.json` workflow manifest string into [`WorkflowManifest`].
pub fn parse_workflow_file(text: &str) -> Result<WorkflowManifest, ExecError> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        serde_json::from_str(text)
            .map_err(|e| ExecError::Store(format!("failed to parse workflow JSON: {e}")))
    } else {
        toml::from_str(text)
            .map_err(|e| ExecError::Store(format!("failed to parse workflow TOML: {e}")))
    }
}

/// Resolve a [`WorkflowManifest`] against `catalog.list_gates(project)`, mint a fresh
/// [`WorkflowId`], validate all static graph invariants, and return [`WorkflowDef`].
pub fn import_workflow_manifest(
    catalog: &dyn Catalog,
    project: &ProjectId,
    manifest: WorkflowManifest,
    default_commit: &str,
    default_by: &str,
) -> Result<WorkflowDef, ExecError> {
    let project_gates = catalog
        .list_gates(project)
        .map_err(|e| ExecError::Store(e.to_string()))?;

    let mut steps = Vec::with_capacity(manifest.steps.len());
    for s in manifest.steps {
        let completion = match s.completion {
            StepCompletionManifest::Gated { gates, regret } => {
                let mut resolved_gates = Vec::with_capacity(gates.len());
                for gate_ref in gates {
                    let trimmed = gate_ref.trim();
                    if let Some(found) = project_gates
                        .iter()
                        .find(|g| g.id.as_str() == trimmed || g.name == trimmed)
                    {
                        resolved_gates.push(found.id.clone());
                    } else {
                        return Err(ExecError::BadSelector(format!(
                            "step `{}` references unknown gate `{trimmed}` in project {}",
                            s.step_id, project
                        )));
                    }
                }
                StepCompletion::Gated {
                    gates: resolved_gates,
                    regret,
                }
            }
            StepCompletionManifest::HumanSignOff { approval_tier } => {
                StepCompletion::HumanSignOff { approval_tier }
            }
            StepCompletionManifest::Ungated => StepCompletion::Ungated,
        };
        steps.push(StepDef {
            step_id: s.step_id,
            title: s.title,
            prompt: s.prompt,
            assignee: s.assignee,
            reviews_step: s.reviews_step,
            require_different_vendor: s.require_different_vendor,
            completion,
            join_policy: s.join_policy,
            max_attempts: s.max_attempts,
            max_visits: s.max_visits,
            budget_usd_micros: s.budget_usd_micros,
            max_tokens: s.max_tokens,
            isolate_worktree: s.isolate_worktree,
        });
    }

    let authored_at_commit = if manifest.authored_at_commit.trim().is_empty() {
        default_commit.trim().to_string()
    } else {
        manifest.authored_at_commit.trim().to_string()
    };
    let authored_by = if manifest.authored_by.trim().is_empty() {
        default_by.trim().to_string()
    } else {
        manifest.authored_by.trim().to_string()
    };

    let def = WorkflowDef {
        id: WorkflowId(stamp::entry_id()),
        project: project.clone(),
        slug: manifest.slug,
        title: manifest.title,
        start_step: manifest.start_step,
        steps,
        edges: manifest.edges,
        max_total_steps: manifest.max_total_steps,
        budget_usd_micros: manifest.budget_usd_micros,
        max_tokens: manifest.max_tokens,
        declared_keys: manifest.declared_keys,
        authored_at_commit,
        authored_by,
    };

    validate_workflow_def(&def).map_err(|e| ExecError::Store(e.to_string()))?;
    Ok(def)
}

/// Parse a `.toml` or `.json` workflow definition, resolve gates against `catalog`,
/// validate with [`validate_workflow_def`], and return a [`WorkflowDef`] (spec §3.1).
pub fn import_workflow_toml(
    catalog: &dyn Catalog,
    project: &ProjectId,
    text: &str,
    default_commit: &str,
    default_by: &str,
) -> Result<WorkflowDef, ExecError> {
    let manifest = parse_workflow_file(text)?;
    import_workflow_manifest(catalog, project, manifest, default_commit, default_by)
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in &digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Deterministic `StateScope::Workflow` slug for `run_id`: `run-<24 hex chars>` (spec §3.2).
pub fn run_scope_slug(run_id: &WorkflowRunId) -> String {
    format!("run-{}", &sha256_hex(run_id.as_str())[..24])
}

/// Deterministic `HandoffEntry` slug for a step visit: `step-<step_id>-v<visit_count>` (spec §3.2).
pub fn step_handoff_slug(step_id: &str, visit_count: u32) -> String {
    format!("step-{step_id}-v{visit_count}")
}

fn sanitize_handoff_summary(raw: &str, fallback: &str) -> String {
    let candidate = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_else(|| fallback.trim());
    let single_line: String = candidate
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = single_line.trim();
    if trimmed.is_empty() {
        return "Step completed".to_string();
    }
    if trimmed.chars().count() <= MAX_SUMMARY_CHARS {
        trimmed.to_string()
    } else {
        trimmed.chars().take(MAX_SUMMARY_CHARS).collect()
    }
}

/// Record a curated [`HandoffEntry`] for `(run_id, step_id, visit_count)` under
/// `StateScope::Workflow(run_scope_slug(run_id))` (spec §3.2).
pub fn record_step_handoff(
    state_store: &dyn StateStore,
    run_id: &WorkflowRunId,
    step_id: &str,
    visit_count: u32,
    mut handoff: HandoffEntry,
    at: At,
    by: &str,
) -> Result<StateEntry, ExecError> {
    if !handoff.summary.trim().is_empty() {
        handoff.summary = sanitize_handoff_summary(&handoff.summary, "Step handoff");
    }
    state_store
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workflow(run_scope_slug(run_id)),
            slug: step_handoff_slug(step_id, visit_count),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Handoff(handoff),
            at,
            by: by.to_string(),
        })
        .map_err(|e| ExecError::Store(e.to_string()))
}

/// Write a declared workflow runtime fact under `StateScope::Workflow(run_scope_slug(&run.id))`,
/// rejecting any key not declared in `def.declared_keys` with [`WorkflowError::UndeclaredWorkflowKey`] (spec §3.2).
pub fn write_workflow_fact(
    state_store: &dyn StateStore,
    def: &WorkflowDef,
    run: &WorkflowRun,
    key: &str,
    value: serde_json::Value,
    at: At,
    by: &str,
) -> Result<StateEntry, WorkflowError> {
    let Some(&merge_mode) = def.declared_keys.get(key) else {
        return Err(WorkflowError::UndeclaredWorkflowKey {
            workflow: def.slug.clone(),
            key: key.to_string(),
        });
    };

    state_store
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workflow(run_scope_slug(&run.id)),
            slug: key.to_string(),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Runtime(RuntimeEntry { value, merge_mode }),
            at,
            by: by.to_string(),
        })
        .map_err(|e| WorkflowError::InvalidStepLimits {
            detail: e.to_string(),
        })
}

/// List all curated [`HandoffEntry`] state records for `run_id`.
pub fn list_workflow_handoffs(
    state_store: &dyn StateStore,
    run_id: &WorkflowRunId,
) -> Result<Vec<StateEntry>, ExecError> {
    state_store
        .list_state(
            Some(&StateScope::Workflow(run_scope_slug(run_id))),
            Some(StateConcern::Handoff),
        )
        .map_err(|e| ExecError::Store(e.to_string()))
}

/// List all declared [`RuntimeEntry`] state records for `run_id`.
pub fn list_workflow_facts(
    state_store: &dyn StateStore,
    run_id: &WorkflowRunId,
) -> Result<Vec<StateEntry>, ExecError> {
    state_store
        .list_state(
            Some(&StateScope::Workflow(run_scope_slug(run_id))),
            Some(StateConcern::Runtime),
        )
        .map_err(|e| ExecError::Store(e.to_string()))
}

fn latest_handoff_for_step<'a>(
    entries: &'a [StateEntry],
    target_step_id: &str,
) -> Option<(&'a str, &'a HandoffEntry)> {
    let prefix = format!("step-{target_step_id}-v");
    let mut best: Option<(u32, &'a str, &'a HandoffEntry)> = None;
    for entry in entries {
        let Some(ver_str) = entry.slug.strip_prefix(&prefix) else {
            continue;
        };
        let Ok(ver) = ver_str.parse::<u32>() else {
            continue;
        };
        let StatePayload::Handoff(ref h) = entry.payload else {
            continue;
        };
        if best.as_ref().is_none_or(|(best_v, _, _)| ver >= *best_v) {
            best = Some((ver, entry.slug.as_str(), h));
        }
    }
    best.map(|(_, slug, h)| (slug, h))
}

/// Assemble the prompt instruction for `step_id` from `step_def.prompt`, curated
/// predecessor `HandoffEntry` records, and declared `StateScope::Workflow` runtime facts,
/// never including raw vendor JSONL transcripts (spec §3.2).
pub fn build_step_instruction(
    state_store: &dyn StateStore,
    def: &WorkflowDef,
    run: &WorkflowRun,
    step_id: &str,
) -> Result<String, ExecError> {
    let step_def = def
        .steps
        .iter()
        .find(|s| s.step_id == step_id)
        .ok_or_else(|| {
            ExecError::Store(WorkflowError::UnknownStep(step_id.to_string()).to_string())
        })?;

    let mut out = step_def.prompt.clone();

    let handoff_entries = list_workflow_handoffs(state_store, &run.id)?;
    let mut relevant_steps = Vec::new();
    let mut seen = BTreeSet::new();
    for edge in &def.edges {
        if edge.to_step == step_id && seen.insert(edge.from_step.clone()) {
            relevant_steps.push(edge.from_step.clone());
        }
    }
    if seen.insert(step_id.to_string()) {
        relevant_steps.push(step_id.to_string());
    }

    let mut collected_handoffs = Vec::new();
    for pred_id in &relevant_steps {
        if let Some((slug, handoff)) = latest_handoff_for_step(&handoff_entries, pred_id) {
            collected_handoffs.push((pred_id.as_str(), slug, handoff));
        }
    }

    if !collected_handoffs.is_empty() {
        out.push_str("\n\n## Predecessor Handoffs\n");
        for (pred_id, slug, h) in collected_handoffs {
            out.push_str(&format!("\n### `{pred_id}` (`{slug}`): {}\n", h.summary));
            if !h.shipped.is_empty() {
                out.push_str(&format!("- Shipped: {}\n", h.shipped.join(", ")));
            }
            if !h.in_flight.is_empty() {
                out.push_str(&format!("- In flight: {}\n", h.in_flight.join(", ")));
            }
            if !h.blockers.is_empty() {
                out.push_str(&format!("- Blockers: {}\n", h.blockers.join("; ")));
            }
            if !h.next_steps.is_empty() {
                out.push_str(&format!("- Next steps: {}\n", h.next_steps.join("; ")));
            }
        }
    }

    let fact_entries = list_workflow_facts(state_store, &run.id)?;
    if !fact_entries.is_empty() {
        out.push_str("\n\n## Workflow Runtime Facts\n\n");
        for entry in fact_entries {
            if let StatePayload::Runtime(rt) = entry.payload {
                out.push_str(&format!(
                    "- `{}` ({}): {}\n",
                    entry.slug,
                    rt.merge_mode.as_wire(),
                    serde_json::to_string(&rt.value).unwrap_or_default()
                ));
            }
        }
    }

    Ok(out)
}

/// Alias for [`build_step_instruction`].
pub fn build_step_prompt(
    state_store: &dyn StateStore,
    def: &WorkflowDef,
    run: &WorkflowRun,
    step_id: &str,
) -> Result<String, ExecError> {
    build_step_instruction(state_store, def, run, step_id)
}

/// Bundle of stores used during workflow execution and sign-off.
#[derive(Clone, Copy)]
pub struct WorkflowEnv<'a> {
    pub workflows: &'a dyn WorkflowStore,
    pub state: &'a dyn StateStore,
    pub registry: &'a dyn AgentRegistry,
    pub wire: &'a dyn MessageStore,
    pub roles: Roles<'a>,
}

impl<'a> WorkflowEnv<'a> {
    pub fn single<S>(store: &'a S) -> Self
    where
        S: WorkflowStore + StateStore + AgentRegistry + MessageStore + Catalog + Tracker + Ledger,
    {
        Self {
            workflows: store,
            state: store,
            registry: store,
            wire: store,
            roles: Roles::single(store),
        }
    }
}

/// Errors produced by workflow step execution, assignment, or sign-off.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowExecError {
    #[error(transparent)]
    Workflow(#[from] WorkflowError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Exec(#[from] ExecError),
    #[error(transparent)]
    Attempt(#[from] AttemptError),
}

impl WorkflowExecError {
    /// Whether this error is a policy or static validation refusal (CLI exit code `2`).
    pub fn is_policy_refusal(&self) -> bool {
        match self {
            Self::Workflow(e) => e.is_policy_refusal(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StepExecutionOptions {
    pub step_id: Option<String>,
    pub adapter_override: Option<String>,
    pub binary_override: Option<String>,
    pub rate_card: Option<RateCard>,
    pub timeout_secs: u64,
    pub budget_usd_micros: u64,
    pub offer_only: bool,
    pub owner_login: String,
    pub by: String,
}

impl Default for StepExecutionOptions {
    fn default() -> Self {
        Self {
            step_id: None,
            adapter_override: None,
            binary_override: None,
            rate_card: None,
            timeout_secs: 900,
            budget_usd_micros: 1_000_000,
            offer_only: false,
            owner_login: DEFAULT_OWNER_LOGIN.to_string(),
            by: "fl".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum StepExecutionAction {
    AwaitingAssignment,
    Offered {
        message_id: MessageId,
        agent: String,
    },
    Executed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        adapter: String,
        attempt_id: Iri,
        outcome: StepOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval_message_id: Option<MessageId>,
    },
}

#[derive(Debug, Clone)]
pub struct StepExecutionReport {
    pub step_id: String,
    pub action: StepExecutionAction,
    pub gate_reports: Vec<GateReport>,
    pub handoff: Option<StateEntry>,
    pub run: WorkflowRun,
}

/// Start and persist a new [`WorkflowRun`] for `def`.
pub fn start_workflow_run(
    workflows: &dyn WorkflowStore,
    def: &WorkflowDef,
    title: impl Into<String>,
    at: At,
    by: impl Into<String>,
) -> Result<WorkflowRun, WorkflowExecError> {
    let run_id = WorkflowRunId(stamp::entry_id());
    let run = WorkflowRun::new(run_id, def, title, at, by)?;
    Ok(workflows.create_workflow_run(run)?)
}

/// Assign an active agent (and optional adapter vendor) to `step_id` in `run_id`,
/// enforcing author and vendor separation against `reviews_step` (spec §1.5, §4.1).
#[allow(clippy::too_many_arguments)]
pub fn assign_workflow_step(
    workflows: &dyn WorkflowStore,
    registry: &dyn AgentRegistry,
    def: &WorkflowDef,
    run_id: &WorkflowRunId,
    step_id: &str,
    agent: &str,
    adapter: Option<&str>,
    at: At,
    by: &str,
) -> Result<WorkflowRun, WorkflowExecError> {
    let mut run = workflows
        .get_workflow_run(run_id)?
        .ok_or_else(|| StoreError::NoSuchWorkflowRun(run_id.to_string()))?;

    if !def.steps.iter().any(|s| s.step_id == step_id) {
        return Err(WorkflowError::UnknownStep(step_id.to_string()).into());
    }

    let agents = registry.list_agents(None, false)?;
    let target_key = agent.trim().to_ascii_lowercase();
    let active_agent = agents
        .iter()
        .find(|a| a.status == AgentStatus::Active && a.name_key == target_key)
        .ok_or_else(|| WorkflowError::NoEligibleAgent {
            step_id: step_id.to_string(),
        })?;

    validate_reviewer_independence(def, &run, step_id, &active_agent.name_key, adapter)?;

    let step = run
        .steps
        .get_mut(step_id)
        .ok_or_else(|| WorkflowError::UnknownStep(step_id.to_string()))?;
    step.assigned_agent = Some(active_agent.name_key.clone());
    if let Some(ad) = adapter {
        step.adapter_used = Some(ad.trim().to_ascii_lowercase());
    }
    run.updated_at = at;
    run.by = by.to_string();

    Ok(workflows.update_workflow_run(run)?)
}

fn infer_project_area_slug(
    registry: &dyn AgentRegistry,
    project_root: &str,
    def: &WorkflowDef,
) -> String {
    for step in &def.steps {
        if let AssigneeRule::Selector {
            area: Some(ref area),
            ..
        } = step.assignee
            && let Ok(slug) = validate_slug(area)
        {
            return slug;
        }
    }

    let basename = Path::new(project_root)
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| validate_slug(s).ok());

    if let Ok(areas) = registry.list_areas() {
        if let Some(ref base) = basename
            && areas.iter().any(|a| &a.area_key == base)
        {
            return base.clone();
        }
        if areas.len() == 1 {
            return areas[0].area_key.clone();
        }
    }

    if let Ok(agents) = registry.list_agents(None, false) {
        for step in &def.steps {
            if let AssigneeRule::Agent { ref name } = step.assignee
                && let Some(ag) = agents
                    .iter()
                    .find(|a| a.name_key.eq_ignore_ascii_case(name))
            {
                return ag.project.clone();
            }
        }
    }

    basename.unwrap_or_else(|| def.slug.clone())
}

fn ensure_isolated_worktree(
    project_root: &Path,
    run_id: &WorkflowRunId,
    step_id: &str,
) -> Result<(PathBuf, String), ExecError> {
    let run_short = &sha256_hex(run_id.as_str())[..12];
    let branch = format!("fl/flow/{run_short}/{step_id}");
    let wt_dir = project_root
        .join(".fl/worktrees")
        .join(format!("{run_short}-{step_id}"));

    if !wt_dir.exists() {
        if let Some(parent) = wt_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ExecError::Git(format!("could not create worktree parent dir: {e}"))
            })?;
        }
        let out = Command::new("git")
            .args([
                "worktree",
                "add",
                "-B",
                &branch,
                wt_dir.to_str().unwrap_or_default(),
                "HEAD",
            ])
            .current_dir(project_root)
            .output()
            .map_err(|e| ExecError::Git(format!("could not run git worktree add: {e}")))?;
        if !out.status.success() {
            return Err(ExecError::Git(format!(
                "git worktree add failed in {}: {}",
                project_root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    }

    Ok((wt_dir, branch))
}

/// Persist `local`'s copy of `step_id` onto the stored run's current state and return the result.
fn commit_step_snapshot(
    workflows: &dyn WorkflowStore,
    local: &WorkflowRun,
    step_id: &str,
) -> Result<WorkflowRun, WorkflowExecError> {
    let _commit = lock_run_commits();
    let mut fresh = workflows
        .get_workflow_run(&local.id)?
        .ok_or_else(|| StoreError::NoSuchWorkflowRun(local.id.to_string()))?;
    if let Some(step) = local.steps.get(step_id) {
        fresh.steps.insert(step_id.to_string(), step.clone());
    }
    fresh.updated_at = local.updated_at.clone();
    Ok(workflows.update_workflow_run(fresh)?)
}

/// Execute (or dispatch a FerroWire task offer for) an active step in `run_id` (spec §3.3).
pub fn execute_workflow_step(
    env: WorkflowEnv<'_>,
    run_id: &WorkflowRunId,
    options: &StepExecutionOptions,
) -> Result<StepExecutionReport, WorkflowExecError> {
    let mut run = env
        .workflows
        .get_workflow_run(run_id)?
        .ok_or_else(|| StoreError::NoSuchWorkflowRun(run_id.to_string()))?;

    let def = env
        .workflows
        .get_workflow_def(&run.workflow_id)?
        .ok_or_else(|| StoreError::NoSuchWorkflow(run.workflow_id.to_string()))?;

    if run.status != WorkflowRunStatus::Running {
        return Err(WorkflowError::RunNotRunning(run.status).into());
    }

    let step_id = match &options.step_id {
        Some(explicit) => {
            if !def.steps.iter().any(|s| &s.step_id == explicit) {
                return Err(WorkflowError::UnknownStep(explicit.clone()).into());
            }
            if !run.active_steps.iter().any(|s| s == explicit) {
                return Err(WorkflowError::StepNotActive(explicit.clone()).into());
            }
            explicit.clone()
        }
        None => run
            .active_steps
            .iter()
            .find(|sid| {
                run.steps
                    .get(*sid)
                    .is_some_and(|st| st.state != State::NeedsHuman)
            })
            .or_else(|| run.active_steps.first())
            .cloned()
            .ok_or_else(|| WorkflowError::StepNotActive("<none>".into()))?,
    };

    let step_def = def
        .steps
        .iter()
        .find(|s| s.step_id == step_id)
        .ok_or_else(|| WorkflowError::UnknownStep(step_id.clone()))?
        .clone();

    let agents = env.registry.list_agents(None, false)?;

    // 1. Resolve agent assignment.
    let existing_agent = run
        .steps
        .get(&step_id)
        .and_then(|s| s.assigned_agent.clone());

    let assigned_agent = match existing_agent {
        Some(agent_name) => {
            let cand_adapter = options.adapter_override.as_deref().or_else(|| {
                run.steps
                    .get(&step_id)
                    .and_then(|s| s.adapter_used.as_deref())
            });
            validate_reviewer_independence(&def, &run, &step_id, &agent_name, cand_adapter)?;
            Some(agent_name)
        }
        None => {
            match select_step_agent(
                &def,
                &run,
                &step_id,
                &agents,
                options.adapter_override.as_deref(),
            )? {
                Some(selected) => {
                    let step = run.steps.get_mut(&step_id).expect("step exists");
                    step.assigned_agent = Some(selected.agent_name.clone());
                    if let Some(ad) = selected.adapter {
                        step.adapter_used = Some(ad);
                    }
                    Some(selected.agent_name)
                }
                None => {
                    return Ok(StepExecutionReport {
                        step_id,
                        action: StepExecutionAction::AwaitingAssignment,
                        gate_reports: Vec::new(),
                        handoff: None,
                        run,
                    });
                }
            }
        }
    };

    let instruction = build_step_instruction(env.state, &def, &run, &step_id)?;

    // 2. Offer-only mode (`--offer-only`).
    if options.offer_only {
        let agent_name = assigned_agent.expect("assigned above");
        let now = stamp::now();
        let (msg, _) = env.wire.send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: options.by.clone(),
            sender_host: None,
            to: WireAddress::AgentName(agent_name.clone()),
            kind: MessageKind::TaskOffer,
            urgency: MessageUrgency::Normal,
            durable: true,
            subject: format!("Workflow `{}` step `{}` ready", def.slug, step_id),
            body: instruction,
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: now.clone(),
        })?;
        let step = run.steps.get_mut(&step_id).expect("step exists");
        step.state = State::Doing;
        run.updated_at = now;
        run = commit_step_snapshot(env.workflows, &run, &step_id)?;
        return Ok(StepExecutionReport {
            step_id,
            action: StepExecutionAction::Offered {
                message_id: msg.id,
                agent: agent_name,
            },
            gate_reports: Vec::new(),
            handoff: None,
            run,
        });
    }

    // 3. Adapter execution mode.
    //
    // Fail fast on an exhausted budget: the ceilings in `advance_workflow` are checked after an
    // attempt has run, so without this the attempt that crosses a ceiling has already spent it.
    if let Some(detail) = preflight_ceiling(&def, &run, &step_id) {
        let _commit = lock_run_commits();
        let mut fresh = env
            .workflows
            .get_workflow_run(&run.id)?
            .ok_or_else(|| StoreError::NoSuchWorkflowRun(run.id.to_string()))?;
        if !fresh.status.is_terminal() {
            if let Some(step) = fresh.steps.get_mut(&step_id) {
                step.state = State::NeedsHuman;
                step.last_outcome = Some(StepOutcome::NeedsHuman);
                step.needs_human_reason = Some(detail.clone());
            }
            fresh.status = WorkflowRunStatus::NeedsHuman;
            fresh.updated_at = stamp::now();
            env.workflows.update_workflow_run(fresh)?;
        }
        return Err(WorkflowError::BudgetExhausted { step_id, detail }.into());
    }

    let raw_adapter = options
        .adapter_override
        .clone()
        .or_else(|| run.steps.get(&step_id).and_then(|s| s.adapter_used.clone()))
        .or_else(|| match &step_def.assignee {
            AssigneeRule::Selector { adapter, .. } => adapter.clone(),
            _ => None,
        })
        .unwrap_or_else(|| "claude".to_string());

    let Some(adapter_kind) = AdapterKind::from_wire(&raw_adapter) else {
        return Err(AttemptError::UnknownAdapter(raw_adapter).into());
    };

    if let Some(ref ag) = assigned_agent {
        validate_reviewer_independence(&def, &run, &step_id, ag, Some(adapter_kind.as_wire()))?;
    }

    let project = env
        .roles
        .catalog
        .get_project(&run.project)?
        .ok_or_else(|| ExecError::BadSelector(format!("project {} does not exist", run.project)))?;
    let project_root = PathBuf::from(&project.root);

    let record_id = match run.steps.get(&step_id).and_then(|s| s.record_id.clone()) {
        Some(id) => id,
        None => {
            let rec_title = format!("{} / {}", run.title, step_def.title);
            let id = env.roles.tracker.add_record(&run.project, &rec_title)?;
            run.steps.get_mut(&step_id).expect("step exists").record_id = Some(id.clone());
            id
        }
    };

    env.roles
        .tracker
        .set_record_state(&record_id, State::Doing)?;

    let exec_root = if step_def.isolate_worktree {
        let (wt_dir, branch) = ensure_isolated_worktree(&project_root, &run.id, &step_id)?;
        run.steps
            .get_mut(&step_id)
            .expect("step exists")
            .worktree_branch = Some(branch);
        wt_dir
    } else {
        project_root.clone()
    };

    {
        let step = run.steps.get_mut(&step_id).expect("step exists");
        step.state = State::Doing;
        step.adapter_used = Some(adapter_kind.as_wire().to_string());
    }

    let budget_usd_micros = step_def
        .budget_usd_micros
        .or(def.budget_usd_micros)
        .unwrap_or(options.budget_usd_micros);

    let spec = AttemptSpec {
        project_root: exec_root,
        record: record_id.clone(),
        instruction,
        timeout_secs: options.timeout_secs,
        budget_usd_micros,
    };

    let binary_override = options.binary_override.clone();
    let rate_card = options.rate_card;
    let outcome = std::thread::scope(|s| {
        s.spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio current_thread runtime builds");
            let adapter = AnyAdapter::new(adapter_kind, binary_override, rate_card);
            rt.block_on(adapter.attempt(spec))
        })
        .join()
        .expect("adapter thread did not panic")
    })?;

    let attempt_id = stamp::entry_id();
    let now = stamp::now();
    let attempt = Attempt {
        id: Some(attempt_id.clone()),
        at: Some(now.clone()),
        project: run.project.clone(),
        record: record_id.clone(),
        adapter: adapter_kind.as_wire().to_string(),
        status: outcome.status,
        duration_ms: outcome.duration_ms,
        tokens_in: outcome.tokens_in,
        tokens_out: outcome.tokens_out,
        cost_usd_micros: outcome.cost_usd_micros,
        paths_touched: PathsTouched::Listed(outcome.paths_touched.clone()),
        output_excerpt: Some(outcome.output_excerpt.clone()),
    };
    env.roles.ledger.append_attempt(attempt.clone())?;
    let _ = env
        .roles
        .ledger
        .flush(decision::for_attempt(&attempt_id, &attempt));

    // 4. Evaluate completion mode and exit gates.
    let mut gate_reports = Vec::new();
    let mut shipped = outcome.paths_touched.clone();
    let mut blockers = Vec::new();
    let mut approval_message_id = None;
    let (step_outcome, reason) = if outcome.status != AttemptStatus::Completed {
        let msg = format!("attempt ended with status `{}`", outcome.status.as_wire());
        blockers.push(sanitize_handoff_summary(&outcome.output_excerpt, &msg));
        env.roles
            .tracker
            .set_record_state(&record_id, State::Review)?;
        (StepOutcome::Fail, Some(msg))
    } else {
        match &step_def.completion {
            StepCompletion::Ungated => {
                env.roles
                    .tracker
                    .set_record_state(&record_id, State::Done)?;
                (StepOutcome::Pass, None)
            }
            StepCompletion::HumanSignOff { approval_tier } => {
                let area_slug = infer_project_area_slug(env.registry, &project.root, &def);
                let to = match approval_tier {
                    ApprovalTier::Lead => WireAddress::AreaRole {
                        area: area_slug,
                        role: AgentRole::Lead,
                    },
                    ApprovalTier::LabManager => WireAddress::Role(AgentRole::LabManager),
                    ApprovalTier::Owner => WireAddress::AgentName("owner".to_string()),
                };
                let sender = assigned_agent.clone().unwrap_or_else(|| options.by.clone());
                let body = if outcome.output_excerpt.trim().is_empty() {
                    format!(
                        "Workflow `{}` run `{}` step `{}` awaits `{}` sign-off.",
                        def.slug,
                        run.id,
                        step_id,
                        approval_tier.as_wire()
                    )
                } else {
                    format!(
                        "Workflow `{}` run `{}` step `{}` awaits `{}` sign-off:\n\n{}",
                        def.slug,
                        run.id,
                        step_id,
                        approval_tier.as_wire(),
                        outcome.output_excerpt.trim()
                    )
                };
                let (msg, _) = env.wire.send_wire(SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name: sender,
                    sender_host: None,
                    to,
                    kind: MessageKind::ApprovalRequest,
                    urgency: MessageUrgency::Urgent,
                    durable: true,
                    subject: format!(
                        "Approval requested: workflow `{}` step `{}`",
                        def.slug, step_id
                    ),
                    body,
                    reply_to: None,
                    thread_id: None,
                    deadline_at: None,
                    at: now.clone(),
                })?;
                approval_message_id = Some(msg.id);
                env.roles
                    .tracker
                    .set_record_state(&record_id, State::NeedsHuman)?;
                let nh_reason = format!("awaiting {} sign-off", approval_tier.as_wire());
                (StepOutcome::NeedsHuman, Some(nh_reason))
            }
            StepCompletion::Gated { gates, regret } => {
                for gate_id in gates {
                    let rep = run_single_gate_with_regret(
                        env.roles.catalog,
                        env.roles.ledger,
                        &run.project,
                        gate_id,
                        *regret,
                        Some(&record_id),
                    )?;
                    gate_reports.push(rep);
                }
                let t_report = TransitionReport {
                    transition: format!("workflow:{}:{step_id}", def.slug),
                    regret: *regret,
                    gates: gate_reports.clone(),
                };
                let all_passed = t_report.passed();
                let record = env
                    .roles
                    .tracker
                    .get_record(&record_id)?
                    .ok_or_else(|| StoreError::NoSuchRecord(record_id.clone()))?;
                let target_state = if all_passed {
                    State::Done
                } else {
                    State::Review
                };
                env.roles
                    .ledger
                    .flush(decision::for_move(
                        &record,
                        target_state,
                        &[t_report],
                        all_passed,
                    ))
                    .map_err(refused_publish)?;
                env.roles
                    .tracker
                    .set_record_state(&record_id, target_state)?;

                if all_passed {
                    for gr in &gate_reports {
                        shipped.push(format!("gate:{}:pass", gr.name));
                    }
                    (StepOutcome::Pass, None)
                } else {
                    let mut failed_names = Vec::new();
                    for gr in &gate_reports {
                        if !gr.verdict.is_pass() {
                            failed_names.push(gr.name.clone());
                            let excerpt = gr.output_excerpt.trim();
                            if excerpt.is_empty() {
                                blockers
                                    .push(format!("gate `{}` failed ({:?})", gr.name, gr.verdict));
                            } else {
                                blockers.push(format!(
                                    "gate `{}` failed: {}",
                                    gr.name,
                                    sanitize_handoff_summary(excerpt, "gate failed")
                                ));
                            }
                        }
                    }
                    let fail_reason = format!("failed gates: {}", failed_names.join(", "));
                    (StepOutcome::Fail, Some(fail_reason))
                }
            }
        }
    };

    // 5. Record curated HandoffEntry and advance workflow.
    let visit_count = run
        .steps
        .get(&step_id)
        .map(|s| s.visit_count.max(1))
        .unwrap_or(1);
    let in_flight: Vec<String> = run
        .steps
        .get(&step_id)
        .and_then(|s| s.worktree_branch.clone())
        .into_iter()
        .collect();
    let gate_run_iris: Vec<String> = gate_reports.iter().map(|g| g.run.to_string()).collect();
    let notes_json = serde_json::json!({
        "attempt": attempt_id.to_string(),
        "record": record_id.to_string(),
        "gate_runs": gate_run_iris,
    });

    let handoff_author = assigned_agent.as_deref().unwrap_or(options.by.as_str());
    let handoff_entry = record_step_handoff(
        env.state,
        &run.id,
        &step_id,
        visit_count,
        HandoffEntry {
            summary: sanitize_handoff_summary(
                &outcome.output_excerpt,
                &format!("Step `{step_id}` {}", step_outcome.as_wire()),
            ),
            shipped,
            in_flight,
            blockers,
            next_steps: Vec::new(),
            notes: Some(notes_json.to_string()),
        },
        now.clone(),
        handoff_author,
    )?;

    // Commit onto the run as it is now, not as it was when this step started: sibling branches
    // may have committed during the attempt.
    {
        let _commit = lock_run_commits();
        let mut fresh = env
            .workflows
            .get_workflow_run(&run.id)?
            .ok_or_else(|| StoreError::NoSuchWorkflowRun(run.id.to_string()))?;
        if let Some(local) = run.steps.get(&step_id) {
            fresh.steps.insert(step_id.clone(), local.clone());
        }
        advance_workflow(
            &def,
            &mut fresh,
            StepAttemptReport {
                step_id: step_id.clone(),
                outcome: step_outcome,
                attempt_iri: Some(attempt_id.clone()),
                tokens_used: outcome.tokens_in.saturating_add(outcome.tokens_out),
                cost_usd_micros: outcome.cost_usd_micros,
                reason,
            },
            now,
        )?;
        run = env.workflows.update_workflow_run(fresh)?;
    }

    Ok(StepExecutionReport {
        step_id,
        action: StepExecutionAction::Executed {
            agent: assigned_agent,
            adapter: adapter_kind.as_wire().to_string(),
            attempt_id,
            outcome: step_outcome,
            approval_message_id,
        },
        gate_reports,
        handoff: Some(handoff_entry),
        run,
    })
}

/// Approve a `NeedsHuman` workflow step, verifying `authorize_step_sign_off`,
/// recording a ledger `Decision`, writing the curated `HandoffEntry`, and advancing
/// the workflow with `StepOutcome::Pass` (spec §1.6, §3.3, §4.1).
pub fn approve_workflow_step(
    env: WorkflowEnv<'_>,
    run_id: &WorkflowRunId,
    step_id: &str,
    by: &str,
    note: Option<&str>,
    owner_login: &str,
) -> Result<WorkflowRun, WorkflowExecError> {
    let _commit = lock_run_commits();
    let mut run = env
        .workflows
        .get_workflow_run(run_id)?
        .ok_or_else(|| StoreError::NoSuchWorkflowRun(run_id.to_string()))?;

    let def = env
        .workflows
        .get_workflow_def(&run.workflow_id)?
        .ok_or_else(|| StoreError::NoSuchWorkflow(run.workflow_id.to_string()))?;

    let step_def = def
        .steps
        .iter()
        .find(|s| s.step_id == step_id)
        .ok_or_else(|| WorkflowError::UnknownStep(step_id.to_string()))?
        .clone();

    if !run.active_steps.iter().any(|s| s == step_id) {
        return Err(WorkflowError::StepNotActive(step_id.to_string()).into());
    }

    let step_instance = run
        .steps
        .get(step_id)
        .ok_or_else(|| WorkflowError::UnknownStep(step_id.to_string()))?
        .clone();

    let project_root = env
        .roles
        .catalog
        .get_project(&run.project)?
        .map(|p| p.root)
        .unwrap_or_default();
    let project_slug = infer_project_area_slug(env.registry, &project_root, &def);
    let agents = env.registry.list_agents(None, false)?;

    authorize_step_sign_off(
        &project_slug,
        &step_def,
        &step_instance,
        by,
        &agents,
        owner_login,
    )?;

    let record_id = match step_instance.record_id.clone() {
        Some(id) => id,
        None => {
            let rec_title = format!("{} / {}", run.title, step_def.title);
            let id = env.roles.tracker.add_record(&run.project, &rec_title)?;
            run.steps.get_mut(step_id).expect("step exists").record_id = Some(id.clone());
            id
        }
    };

    let record = env
        .roles
        .tracker
        .get_record(&record_id)?
        .ok_or_else(|| StoreError::NoSuchRecord(record_id.clone()))?;

    env.roles
        .ledger
        .flush(decision::for_move(&record, State::Done, &[], true))
        .map_err(refused_publish)?;
    env.roles
        .tracker
        .set_record_state(&record_id, State::Done)?;

    let now = stamp::now();
    let visit_count = step_instance.visit_count.max(1);
    let summary = sanitize_handoff_summary(
        note.unwrap_or(""),
        &format!("Step `{step_id}` approved by {by}"),
    );
    record_step_handoff(
        env.state,
        &run.id,
        step_id,
        visit_count,
        HandoffEntry {
            summary,
            shipped: vec![format!("approved by {by}")],
            in_flight: Vec::new(),
            blockers: Vec::new(),
            next_steps: Vec::new(),
            notes: note.map(ToOwned::to_owned),
        },
        now.clone(),
        by,
    )?;

    {
        let step = run.steps.get_mut(step_id).expect("step exists");
        step.state = State::NeedsHuman;
        step.attempt_count = step.attempt_count.max(1);
    }

    advance_workflow(
        &def,
        &mut run,
        StepAttemptReport {
            step_id: step_id.to_string(),
            outcome: StepOutcome::Pass,
            attempt_iri: None,
            tokens_used: 0,
            cost_usd_micros: 0,
            reason: note.map(ToOwned::to_owned),
        },
        now,
    )?;
    run.by = by.to_string();

    Ok(env.workflows.update_workflow_run(run)?)
}

/// Reject a `NeedsHuman` workflow step, verifying `authorize_step_sign_off`,
/// recording a ledger `Decision`, writing the curated `HandoffEntry` with `reason` in
/// `blockers`, and advancing the workflow with `StepOutcome::Fail` (immediately
/// traversing any outgoing `Fail` edge rather than in-step retrying) (spec §1.6, §3.3, §4.1).
pub fn reject_workflow_step(
    env: WorkflowEnv<'_>,
    run_id: &WorkflowRunId,
    step_id: &str,
    by: &str,
    reason: &str,
    owner_login: &str,
) -> Result<WorkflowRun, WorkflowExecError> {
    let _commit = lock_run_commits();
    let mut run = env
        .workflows
        .get_workflow_run(run_id)?
        .ok_or_else(|| StoreError::NoSuchWorkflowRun(run_id.to_string()))?;

    let def = env
        .workflows
        .get_workflow_def(&run.workflow_id)?
        .ok_or_else(|| StoreError::NoSuchWorkflow(run.workflow_id.to_string()))?;

    let step_def = def
        .steps
        .iter()
        .find(|s| s.step_id == step_id)
        .ok_or_else(|| WorkflowError::UnknownStep(step_id.to_string()))?
        .clone();

    if !run.active_steps.iter().any(|s| s == step_id) {
        return Err(WorkflowError::StepNotActive(step_id.to_string()).into());
    }

    let step_instance = run
        .steps
        .get(step_id)
        .ok_or_else(|| WorkflowError::UnknownStep(step_id.to_string()))?
        .clone();

    let project_root = env
        .roles
        .catalog
        .get_project(&run.project)?
        .map(|p| p.root)
        .unwrap_or_default();
    let project_slug = infer_project_area_slug(env.registry, &project_root, &def);
    let agents = env.registry.list_agents(None, false)?;

    authorize_step_sign_off(
        &project_slug,
        &step_def,
        &step_instance,
        by,
        &agents,
        owner_login,
    )?;

    let record_id = match step_instance.record_id.clone() {
        Some(id) => id,
        None => {
            let rec_title = format!("{} / {}", run.title, step_def.title);
            let id = env.roles.tracker.add_record(&run.project, &rec_title)?;
            run.steps.get_mut(step_id).expect("step exists").record_id = Some(id.clone());
            id
        }
    };

    let record = env
        .roles
        .tracker
        .get_record(&record_id)?
        .ok_or_else(|| StoreError::NoSuchRecord(record_id.clone()))?;

    env.roles
        .ledger
        .flush(decision::for_move(&record, State::Review, &[], false))
        .map_err(refused_publish)?;
    env.roles
        .tracker
        .set_record_state(&record_id, State::Review)?;

    let now = stamp::now();
    let visit_count = step_instance.visit_count.max(1);
    let summary = sanitize_handoff_summary(reason, &format!("Step `{step_id}` rejected by {by}"));
    record_step_handoff(
        env.state,
        &run.id,
        step_id,
        visit_count,
        HandoffEntry {
            summary,
            shipped: Vec::new(),
            in_flight: Vec::new(),
            blockers: vec![reason.to_string()],
            next_steps: vec![format!("Address rejection from {by}: {reason}")],
            notes: Some(format!("Rejected by {by}: {reason}")),
        },
        now.clone(),
        by,
    )?;

    {
        let step = run.steps.get_mut(step_id).expect("step exists");
        step.state = State::NeedsHuman;
        step.attempt_count = step_def.max_attempts.max(1);
    }

    advance_workflow(
        &def,
        &mut run,
        StepAttemptReport {
            step_id: step_id.to_string(),
            outcome: StepOutcome::Fail,
            attempt_iri: None,
            tokens_used: 0,
            cost_usd_micros: 0,
            reason: Some(reason.to_string()),
        },
        now,
    )?;
    run.by = by.to_string();

    Ok(env.workflows.update_workflow_run(run)?)
}

/// Sign off (`approved = true` or `false`) on a `NeedsHuman` workflow step.
#[allow(clippy::too_many_arguments)]
pub fn sign_off_workflow_step(
    env: WorkflowEnv<'_>,
    run_id: &WorkflowRunId,
    step_id: &str,
    approved: bool,
    by: &str,
    note_or_reason: Option<&str>,
    owner_login: &str,
) -> Result<WorkflowRun, WorkflowExecError> {
    if approved {
        approve_workflow_step(env, run_id, step_id, by, note_or_reason, owner_login)
    } else {
        reject_workflow_step(
            env,
            run_id,
            step_id,
            by,
            note_or_reason.unwrap_or("rejected"),
            owner_login,
        )
    }
}

/// Cancel an active or `NeedsHuman` workflow run.
pub fn abort_workflow_run(
    workflows: &dyn WorkflowStore,
    run_id: &WorkflowRunId,
    at: At,
    by: &str,
) -> Result<WorkflowRun, WorkflowExecError> {
    let _commit = lock_run_commits();
    let mut run = workflows
        .get_workflow_run(run_id)?
        .ok_or_else(|| StoreError::NoSuchWorkflowRun(run_id.to_string()))?;
    match run.status {
        // Aborting twice is harmless.
        WorkflowRunStatus::Canceled => return Ok(run),
        // A finished run keeps the status it earned.
        WorkflowRunStatus::Completed | WorkflowRunStatus::Failed => {
            return Err(WorkflowError::RunNotRunning(run.status).into());
        }
        WorkflowRunStatus::Running | WorkflowRunStatus::NeedsHuman => {}
    }
    run.status = WorkflowRunStatus::Canceled;
    run.active_steps.clear();
    run.updated_at = at;
    run.by = by.to_string();
    Ok(workflows.update_workflow_run(run)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;
    use fl_core::agent::RegisterAgentArgs;
    use fl_core::ferrowire::WireFilter;
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
    use fl_core::workflow::{END_STEP, EdgeCondition};
    use std::fs;

    fn git_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(d.path())
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        fs::create_dir_all(d.path().join("src")).unwrap();
        fs::write(d.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "init"]);
        d
    }

    fn write_stub_adapter(dir: &Path, name: &str, text: &str, exit_code: i32) -> PathBuf {
        let path = dir.join(name);
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"{text}\",\"usage\":{{\"input_tokens\":120,\"output_tokens\":30}},\"total_cost_usd\":0.0015}}'\nexit {exit_code}\n"
        );
        fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    #[test]
    fn toml_and_json_manifest_export_and_import_resolve_gate_names_and_round_trip() {
        let repo = git_repo();
        let head = crate::git::Git::head(repo.path()).unwrap();
        let store = MemStore::default();
        let project = store
            .add_project(&repo.path().display().to_string())
            .unwrap();
        let gate_id = store
            .add_gate(
                &project,
                "cargo-test",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "src/**/*.rs".into(),
                },
                1,
                &head,
                "ferris",
            )
            .unwrap();

        let toml_text = r#"
slug = "review-loop"
title = "Implement and Independent Review"
start_step = "implement"
max_total_steps = 16
budget_usd_micros = 5000000
max_tokens = 200000

[declared_keys]
branch = "last_writer"
changed_files = "append"

[[steps]]
step_id = "implement"
title = "Implement feature"
prompt = "Implement the requested change and run unit tests."
assignee = { kind = "agent", name = "felicia" }
completion = { mode = "gated", gates = ["cargo-test"], regret = "low" }
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
"#;

        let imported = import_workflow_toml(&store, &project, toml_text, &head, "ferris").unwrap();
        assert_eq!(imported.slug, "review-loop");
        assert_eq!(imported.authored_at_commit, head);
        assert_eq!(imported.authored_by, "ferris");
        assert_eq!(
            imported.steps[0].completion,
            StepCompletion::Gated {
                gates: vec![gate_id.clone()],
                regret: Regret::Low,
            }
        );

        let exported_toml = export_workflow_toml(&store, &imported).unwrap();
        assert!(
            exported_toml.contains("\"cargo-test\""),
            "exported TOML should use unambiguous gate name: {exported_toml}"
        );

        let reimported =
            import_workflow_toml(&store, &project, &exported_toml, &head, "ferris").unwrap();
        assert_eq!(reimported.slug, imported.slug);
        assert_eq!(reimported.steps, imported.steps);
        assert_eq!(reimported.edges, imported.edges);
        assert_eq!(reimported.declared_keys, imported.declared_keys);

        // JSON format also parses cleanly via import_workflow_toml.
        let json_text =
            serde_json::to_string_pretty(&export_workflow_manifest(&store, &imported).unwrap())
                .unwrap();
        let from_json =
            import_workflow_toml(&store, &project, &json_text, &head, "ferris").unwrap();
        assert_eq!(from_json.steps, imported.steps);

        // Referencing an unknown gate name is refused.
        let bad_toml = toml_text.replace("cargo-test", "nonexistent-gate");
        let err = import_workflow_toml(&store, &project, &bad_toml, &head, "ferris").unwrap_err();
        assert!(err.to_string().contains("nonexistent-gate"), "{err}");
    }

    #[test]
    fn curated_handoffs_and_declared_keys_assemble_prompt_without_raw_transcripts() {
        let repo = git_repo();
        let head = crate::git::Git::head(repo.path()).unwrap();
        let store = MemStore::default();
        let project = store
            .add_project(&repo.path().display().to_string())
            .unwrap();

        let mut declared_keys = BTreeMap::new();
        declared_keys.insert("branch".to_string(), MergeMode::LastWriter);
        declared_keys.insert("touched_modules".to_string(), MergeMode::Append);

        let def = store
            .upsert_workflow_def(WorkflowDef {
                id: WorkflowId(stamp::entry_id()),
                project: project.clone(),
                slug: "handoff-flow".into(),
                title: "Handoff Flow".into(),
                start_step: "implement".into(),
                steps: vec![
                    StepDef {
                        step_id: "implement".into(),
                        title: "Implement".into(),
                        prompt: "Write the feature code.".into(),
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
                    },
                    StepDef {
                        step_id: "review".into(),
                        title: "Review".into(),
                        prompt: "Verify the implementation.".into(),
                        assignee: AssigneeRule::Manual,
                        reviews_step: Some("implement".into()),
                        require_different_vendor: false,
                        completion: StepCompletion::Ungated,
                        join_policy: JoinPolicy::All,
                        max_attempts: 1,
                        max_visits: 3,
                        budget_usd_micros: None,
                        max_tokens: None,
                        isolate_worktree: false,
                    },
                ],
                edges: vec![
                    EdgeDef {
                        from_step: "implement".into(),
                        to_step: "review".into(),
                        on: EdgeCondition::Pass,
                    },
                    EdgeDef {
                        from_step: "review".into(),
                        to_step: END_STEP.into(),
                        on: EdgeCondition::Pass,
                    },
                ],
                max_total_steps: 10,
                budget_usd_micros: None,
                max_tokens: None,
                declared_keys,
                authored_at_commit: head,
                authored_by: "ferris".into(),
            })
            .unwrap();

        let run = start_workflow_run(
            &store,
            &def,
            "Build handoff feature",
            At::from_unix_millis(100),
            "ferris",
        )
        .unwrap();

        // Undeclared runtime key is rejected with UndeclaredWorkflowKey (policy refusal).
        let err = write_workflow_fact(
            &store,
            &def,
            &run,
            "undeclared_secret",
            serde_json::json!("nope"),
            At::from_unix_millis(101),
            "felicia",
        )
        .unwrap_err();
        assert!(matches!(err, WorkflowError::UndeclaredWorkflowKey { .. }));
        assert!(err.is_policy_refusal());

        // Declared keys succeed and use their declared MergeMode.
        write_workflow_fact(
            &store,
            &def,
            &run,
            "branch",
            serde_json::json!("feat/workflow"),
            At::from_unix_millis(102),
            "felicia",
        )
        .unwrap();
        write_workflow_fact(
            &store,
            &def,
            &run,
            "touched_modules",
            serde_json::json!(["core", "exec"]),
            At::from_unix_millis(103),
            "felicia",
        )
        .unwrap();

        // Record v1 and v2 handoffs for `implement`; downstream prompt must use latest (v2).
        record_step_handoff(
            &store,
            &run.id,
            "implement",
            1,
            HandoffEntry {
                summary: "Initial draft".into(),
                shipped: vec!["src/old.rs".into()],
                in_flight: vec![],
                blockers: vec!["missing test".into()],
                next_steps: vec![],
                notes: Some("RAW_TRANSCRIPT_DO_NOT_INCLUDE_IN_PROMPT".into()),
            },
            At::from_unix_millis(104),
            "felicia",
        )
        .unwrap();
        record_step_handoff(
            &store,
            &run.id,
            "implement",
            2,
            HandoffEntry {
                summary: "Implemented workflow runner and tests".into(),
                shipped: vec!["crates/exec/src/workflow.rs".into()],
                in_flight: vec!["fl/flow/123/implement".into()],
                blockers: vec![],
                next_steps: vec!["Run independent review".into()],
                notes: Some("PRIVATE_VENDOR_TRANSCRIPT_NOTES".into()),
            },
            At::from_unix_millis(105),
            "felicia",
        )
        .unwrap();

        let prompt = build_step_instruction(&store, &def, &run, "review").unwrap();
        assert!(prompt.starts_with("Verify the implementation."));
        assert!(prompt.contains("## Predecessor Handoffs"));
        assert!(
            prompt.contains(
                "`implement` (`step-implement-v2`): Implemented workflow runner and tests"
            )
        );
        assert!(prompt.contains("- Shipped: crates/exec/src/workflow.rs"));
        assert!(prompt.contains("- In flight: fl/flow/123/implement"));
        assert!(prompt.contains("- Next steps: Run independent review"));
        assert!(!prompt.contains("Initial draft"));
        assert!(!prompt.contains("PRIVATE_VENDOR_TRANSCRIPT_NOTES"));
        assert!(!prompt.contains("RAW_TRANSCRIPT_DO_NOT_INCLUDE_IN_PROMPT"));
        assert!(prompt.contains("## Workflow Runtime Facts"));
        assert!(prompt.contains("- `branch` (last_writer): \"feat/workflow\""));
        assert!(prompt.contains("- `touched_modules` (append): [\"core\",\"exec\"]"));
    }

    #[test]
    fn execute_approve_and_reject_workflow_steps_with_gates_wire_and_worktree_isolation() {
        let repo = git_repo();
        let head = crate::git::Git::head(repo.path()).unwrap();
        let stub_bin =
            write_stub_adapter(repo.path(), "fake-claude.sh", "Implemented step cleanly", 0);

        let store = MemStore::default();
        let project = store
            .add_project(&repo.path().display().to_string())
            .unwrap();
        let gate_id = store
            .add_gate(
                &project,
                "check-rs",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob {
                    pattern: "src/**/*.rs".into(),
                },
                1,
                &head,
                "ferris",
            )
            .unwrap();

        // Register felicia (assistant) and ferris (lead) in ferroloop.
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Ferris".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Lead,
                extra_areas: vec![],
                create_area: true,
                replace: false,
                at: At::from_unix_millis(10),
                by: "owner".into(),
            })
            .unwrap();
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Felicia".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Assistant,
                extra_areas: vec![],
                create_area: false,
                replace: false,
                at: At::from_unix_millis(11),
                by: "ferris".into(),
            })
            .unwrap();

        let def = store
            .upsert_workflow_def(WorkflowDef {
                id: WorkflowId(stamp::entry_id()),
                project: project.clone(),
                slug: "gated-review-loop".into(),
                title: "Gated Review Loop".into(),
                start_step: "implement".into(),
                steps: vec![
                    StepDef {
                        step_id: "implement".into(),
                        title: "Implement".into(),
                        prompt: "Implement feature.".into(),
                        assignee: AssigneeRule::Agent {
                            name: "felicia".into(),
                        },
                        reviews_step: None,
                        require_different_vendor: false,
                        completion: StepCompletion::Gated {
                            gates: vec![gate_id.clone()],
                            regret: Regret::Low,
                        },
                        join_policy: JoinPolicy::All,
                        max_attempts: 2,
                        max_visits: 3,
                        budget_usd_micros: None,
                        max_tokens: None,
                        isolate_worktree: true,
                    },
                    StepDef {
                        step_id: "review".into(),
                        title: "Review".into(),
                        prompt: "Review feature.".into(),
                        assignee: AssigneeRule::Selector {
                            role: Some(AgentRole::Lead),
                            area: Some("ferroloop".into()),
                            adapter: Some("codex".into()),
                        },
                        reviews_step: Some("implement".into()),
                        require_different_vendor: true,
                        completion: StepCompletion::HumanSignOff {
                            approval_tier: ApprovalTier::Lead,
                        },
                        join_policy: JoinPolicy::All,
                        max_attempts: 1,
                        max_visits: 3,
                        budget_usd_micros: None,
                        max_tokens: None,
                        isolate_worktree: false,
                    },
                ],
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
                budget_usd_micros: None,
                max_tokens: None,
                declared_keys: BTreeMap::new(),
                authored_at_commit: head,
                authored_by: "ferris".into(),
            })
            .unwrap();

        let run = start_workflow_run(
            &store,
            &def,
            "Ship workflow engine",
            At::from_unix_millis(20),
            "ferris",
        )
        .unwrap();
        let env = WorkflowEnv::single(&store);

        // Offer-only mode sends a TaskOffer wire message to felicia and leaves step in Doing.
        let offer_rep = execute_workflow_step(
            env,
            &run.id,
            &StepExecutionOptions {
                offer_only: true,
                by: "ferris".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            offer_rep.action,
            StepExecutionAction::Offered { ref agent, .. } if agent == "felicia"
        ));
        let felicia_inbox = store
            .list_wire_inbox(&WireFilter {
                recipient: Some("felicia".into()),
                unread_only: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(felicia_inbox.len(), 1);
        assert_eq!(felicia_inbox[0].message.kind, MessageKind::TaskOffer);

        // Execute `implement` with the stub adapter: creates isolated worktree, runs gate, advances to `review`.
        let step1_rep = execute_workflow_step(
            env,
            &run.id,
            &StepExecutionOptions {
                adapter_override: Some("claude".into()),
                binary_override: Some(stub_bin.to_string_lossy().into_owned()),
                by: "felicia".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(step1_rep.step_id, "implement");
        assert_eq!(step1_rep.gate_reports.len(), 1);
        assert!(step1_rep.gate_reports[0].verdict.is_pass());
        assert_eq!(step1_rep.run.active_steps, vec!["review".to_string()]);
        assert!(
            step1_rep.run.steps["implement"]
                .worktree_branch
                .as_deref()
                .unwrap()
                .starts_with("fl/flow/")
        );

        // Assigning `felicia` to `review` is refused by review independence (same agent).
        let assign_err = assign_workflow_step(
            &store,
            &store,
            &def,
            &run.id,
            "review",
            "felicia",
            None,
            At::from_unix_millis(30),
            "ferris",
        )
        .unwrap_err();
        assert!(assign_err.is_policy_refusal());

        // Execute `review`: selects `ferris` with `codex`, sends urgent ApprovalRequest, enters NeedsHuman.
        let step2_rep = execute_workflow_step(
            env,
            &run.id,
            &StepExecutionOptions {
                binary_override: Some(stub_bin.to_string_lossy().into_owned()),
                by: "ferris".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(step2_rep.step_id, "review");
        assert_eq!(step2_rep.run.status, WorkflowRunStatus::NeedsHuman);
        assert_eq!(step2_rep.run.steps["review"].state, State::NeedsHuman);
        assert_eq!(
            step2_rep.run.steps["review"].assigned_agent.as_deref(),
            Some("ferris")
        );

        // Self-approval by `ferris` (who executed `review`) is forbidden!
        let self_approve_err = approve_workflow_step(
            env,
            &run.id,
            "review",
            "ferris",
            Some("self LGTM"),
            DEFAULT_OWNER_LOGIN,
        )
        .unwrap_err();
        assert!(self_approve_err.is_policy_refusal());

        // Approval by `felicia` (assistant) is refused for InsufficientApprovalAuthority!
        let low_tier_err = approve_workflow_step(
            env,
            &run.id,
            "review",
            "felicia",
            Some("assistant LGTM"),
            DEFAULT_OWNER_LOGIN,
        )
        .unwrap_err();
        assert!(low_tier_err.is_policy_refusal());

        // Reject `review` as owner (`lmcfarlin`): immediately follows `Fail` edge back to `implement` (visit 2).
        let rejected_run = reject_workflow_step(
            env,
            &run.id,
            "review",
            DEFAULT_OWNER_LOGIN,
            "Handle missing edge case",
            DEFAULT_OWNER_LOGIN,
        )
        .unwrap();
        assert_eq!(rejected_run.status, WorkflowRunStatus::Running);
        assert_eq!(rejected_run.active_steps, vec!["implement".to_string()]);
        assert_eq!(rejected_run.steps["implement"].visit_count, 2);

        // Prompt for `implement` visit 2 now includes the rejection blocker from `review`.
        let retry_prompt =
            build_step_instruction(&store, &def, &rejected_run, "implement").unwrap();
        assert!(retry_prompt.contains("Handle missing edge case"));

        // Re-run `implement` (visit 2) and `review` (visit 2), then approve as owner (`lmcfarlin`).
        execute_workflow_step(
            env,
            &run.id,
            &StepExecutionOptions {
                adapter_override: Some("claude".into()),
                binary_override: Some(stub_bin.to_string_lossy().into_owned()),
                by: "felicia".into(),
                ..Default::default()
            },
        )
        .unwrap();
        execute_workflow_step(
            env,
            &run.id,
            &StepExecutionOptions {
                binary_override: Some(stub_bin.to_string_lossy().into_owned()),
                by: "ferris".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let completed_run = approve_workflow_step(
            env,
            &run.id,
            "review",
            DEFAULT_OWNER_LOGIN,
            Some("Ship it"),
            DEFAULT_OWNER_LOGIN,
        )
        .unwrap();
        assert_eq!(completed_run.status, WorkflowRunStatus::Completed);
        assert!(completed_run.active_steps.is_empty());
        assert_eq!(completed_run.steps["review"].state, State::Done);
    }

    fn one_step_def(store: &MemStore) -> WorkflowDef {
        let repo = git_repo();
        let head = crate::git::Git::head(repo.path()).unwrap();
        let project = store
            .add_project(&repo.path().display().to_string())
            .unwrap();
        // Keep the repo alive for the test's lifetime by leaking the temp dir handle.
        std::mem::forget(repo);
        store
            .upsert_workflow_def(WorkflowDef {
                id: WorkflowId(stamp::entry_id()),
                project,
                slug: "one-step".into(),
                title: "One step".into(),
                start_step: "only".into(),
                steps: vec![StepDef {
                    step_id: "only".into(),
                    title: "Only".into(),
                    prompt: "Do it.".into(),
                    assignee: AssigneeRule::Agent {
                        name: "felicia".into(),
                    },
                    reviews_step: None,
                    require_different_vendor: false,
                    completion: StepCompletion::Ungated,
                    join_policy: JoinPolicy::All,
                    max_attempts: 1,
                    max_visits: 1,
                    budget_usd_micros: None,
                    max_tokens: None,
                    isolate_worktree: false,
                }],
                edges: vec![EdgeDef {
                    from_step: "only".into(),
                    to_step: END_STEP.into(),
                    on: EdgeCondition::Pass,
                }],
                max_total_steps: 4,
                budget_usd_micros: None,
                max_tokens: None,
                declared_keys: BTreeMap::new(),
                authored_at_commit: head,
                authored_by: "ferris".into(),
            })
            .unwrap()
    }

    /// Audit X-2: `abort_workflow_run` set `Canceled` on any run, including one that had already
    /// completed or failed, rewriting history.
    #[test]
    fn abort_refuses_a_finished_run_and_is_idempotent_for_a_canceled_one() {
        let store = MemStore::default();
        let def = one_step_def(&store);
        let run = start_workflow_run(&store, &def, "abort me", At::from_unix_millis(5), "ferris")
            .unwrap();
        for terminal in [WorkflowRunStatus::Completed, WorkflowRunStatus::Failed] {
            let mut r = store.get_workflow_run(&run.id).unwrap().unwrap();
            r.status = terminal;
            store.update_workflow_run(r).unwrap();
            let err =
                abort_workflow_run(&store, &run.id, At::from_unix_millis(6), "ferris").unwrap_err();
            assert!(err.is_policy_refusal() || matches!(err, WorkflowExecError::Workflow(_)));
            assert_eq!(
                store.get_workflow_run(&run.id).unwrap().unwrap().status,
                terminal,
                "abort rewrote a finished run"
            );
        }
        let mut r = store.get_workflow_run(&run.id).unwrap().unwrap();
        r.status = WorkflowRunStatus::NeedsHuman;
        store.update_workflow_run(r).unwrap();
        let canceled =
            abort_workflow_run(&store, &run.id, At::from_unix_millis(7), "ferris").unwrap();
        assert_eq!(canceled.status, WorkflowRunStatus::Canceled);
        assert!(canceled.active_steps.is_empty());
        let again = abort_workflow_run(&store, &run.id, At::from_unix_millis(8), "owner").unwrap();
        assert_eq!(again.status, WorkflowRunStatus::Canceled);
        assert_eq!(
            again.by, "ferris",
            "a repeated abort must not rewrite the record"
        );
        assert!(
            abort_workflow_run(
                &store,
                &WorkflowRunId(stamp::entry_id()),
                At::from_unix_millis(9),
                "ferris"
            )
            .is_err()
        );
    }

    #[test]
    fn handoff_summary_is_one_bounded_non_empty_line() {
        assert_eq!(
            sanitize_handoff_summary("\n\n  first line \nsecond", "fb"),
            "first line"
        );
        assert_eq!(sanitize_handoff_summary("   \n\t", "fallback"), "fallback");
        assert_eq!(sanitize_handoff_summary("", "  "), "Step completed");
        let long = "é".repeat(MAX_SUMMARY_CHARS + 50);
        let cut = sanitize_handoff_summary(&long, "fb");
        assert_eq!(
            cut.chars().count(),
            MAX_SUMMARY_CHARS,
            "truncation must be on char boundaries"
        );
        let exact = "x".repeat(MAX_SUMMARY_CHARS);
        assert_eq!(sanitize_handoff_summary(&exact, "fb"), exact);
    }

    #[test]
    fn run_and_handoff_slugs_are_deterministic_and_valid() {
        let a = WorkflowRunId(stamp::entry_id());
        let b = WorkflowRunId(stamp::entry_id());
        assert_eq!(run_scope_slug(&a), run_scope_slug(&a));
        assert_ne!(run_scope_slug(&a), run_scope_slug(&b));
        assert!(validate_slug(&run_scope_slug(&a)).is_ok());
        assert_eq!(step_handoff_slug("build", 3), "step-build-v3");
        assert!(validate_slug(&step_handoff_slug("build", u32::MAX)).is_ok());
    }

    fn handoff_entry(slug: &str, summary: &str) -> StateEntry {
        use fl_core::ids::{RevisionId, StateId};
        StateEntry {
            id: StateId(stamp::entry_id()),
            scope: StateScope::Workflow("run-x".into()),
            concern: StateConcern::Handoff,
            slug: slug.into(),
            sensitive: false,
            ever_sensitive: false,
            payload: StatePayload::Handoff(HandoffEntry {
                summary: summary.into(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            head_revision: RevisionId(stamp::entry_id()),
            updated_at: At::from_unix_millis(1),
            by: "t".into(),
        }
    }

    #[test]
    fn latest_handoff_picks_the_highest_visit_and_ignores_lookalike_steps() {
        let entries = vec![
            handoff_entry("step-build-v2", "second"),
            handoff_entry("step-build-v10", "tenth"),
            handoff_entry("step-build-v1", "first"),
            // A different step whose id merely starts with `build`.
            handoff_entry("step-build-v2-v99", "lookalike"),
            handoff_entry("step-builder-v50", "other step"),
            handoff_entry("step-build-vx", "not a number"),
        ];
        let (slug, h) = latest_handoff_for_step(&entries, "build").unwrap();
        assert_eq!((slug, h.summary.as_str()), ("step-build-v10", "tenth"));
        assert!(latest_handoff_for_step(&entries, "missing").is_none());
        assert!(latest_handoff_for_step(&[], "build").is_none());
    }
}
