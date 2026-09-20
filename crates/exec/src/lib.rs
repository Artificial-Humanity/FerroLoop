//! Everything that touches the outside world: filesystem, git, processes, vendors.

pub mod command;
pub mod evaluate;
pub mod git;
pub mod population;
pub use command::{GateOutcome, run_command_gate};
pub use evaluate::{GateReport, TransitionReport, evaluate_transition};
pub use git::Git;
pub use population::{ChangedPaths, ExecError, resolve};
