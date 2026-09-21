//! Pure domain model. No IO, no async, no clock, no network.

mod wire;

pub mod finding;
pub mod ids;
pub mod log;
pub mod model;
pub mod stale;
pub mod store;
pub mod verdict;

pub use finding::{Finding, FindingError, FindingState};
pub use ids::{FindingId, GateId, ProjectId, RecordId};
pub use log::{Attempt, AttemptStatus, GateRun};
pub use model::{
    AgentSpec, CommandSpec, GateDef, GateKind, PopulationDelivery, Project, Record, Regret,
    Selector, State, Transition,
};
pub use stale::{Staleness, apply_staleness, is_stale};
pub use store::{MemStore, Store, StoreError};
pub use verdict::{FailReason, Population, Verdict};
