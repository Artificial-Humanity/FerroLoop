//! Everything that touches the outside world: filesystem, git, processes, vendors.

pub mod population;
pub use population::{ChangedPaths, ExecError, resolve};
