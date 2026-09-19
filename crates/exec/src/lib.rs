//! Everything that touches the outside world: filesystem, git, processes, vendors.

pub mod command;
pub mod population;
pub use command::{GateOutcome, run_command_gate};
pub use population::{ChangedPaths, ExecError, resolve};
