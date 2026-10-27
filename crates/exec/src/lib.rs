//! Everything that touches the outside world: filesystem, git, processes, vendors.

pub mod adapters;
pub mod command;
pub mod decision;
pub mod evaluate;
pub mod finding;
pub mod git;
#[cfg(test)]
mod journal;
pub mod population;
pub mod record;
pub mod runner;
pub mod stamp;
pub mod state_sync;
pub mod workflow;
pub use adapters::{
    AntigravityAdapter, AnyAdapter, ClaudeAdapter, CodexAdapter, GrokAdapter, LivenessSink,
    MuseAdapter,
};
pub use command::{GateOutcome, run_command_gate};
pub use evaluate::{
    GateReport, TransitionReport, evaluate_transition, run_single_gate, run_single_gate_with_regret,
};
pub use finding::{FindingExecError, FixReport, attach_reproduction, verify_finding};
pub use git::Git;
pub use population::{ChangedPaths, ExecError, refused_publish, resolve};
pub use runner::{AttemptError, AttemptOutcome, AttemptSpec, Runner};
pub use workflow::{
    FLOWS_DIR, SelectedStepAgent, StepCompletionManifest, StepExecutionAction,
    StepExecutionOptions, StepExecutionReport, StepManifest, WorkflowEnv, WorkflowExecError,
    WorkflowManifest, abort_workflow_run, approve_workflow_step, assign_workflow_step,
    build_step_instruction, build_step_prompt, execute_workflow_step, export_workflow_manifest,
    export_workflow_toml, import_workflow_manifest, import_workflow_toml, list_workflow_facts,
    list_workflow_handoffs, parse_workflow_file, record_step_handoff, reject_workflow_step,
    run_scope_slug, select_step_agent, sign_off_workflow_step, start_workflow_run,
    step_handoff_slug, validate_reviewer_independence, write_workflow_fact,
};
