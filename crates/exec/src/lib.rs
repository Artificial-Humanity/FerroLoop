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
pub use adapters::ClaudeAdapter;
pub use command::{GateOutcome, run_command_gate};
pub use evaluate::{GateReport, TransitionReport, evaluate_transition};
pub use finding::{FindingExecError, FixReport, attach_reproduction, verify_finding};
pub use git::Git;
pub use population::{ChangedPaths, ExecError, resolve};
pub use runner::{AttemptError, AttemptOutcome, AttemptSpec, Runner};
