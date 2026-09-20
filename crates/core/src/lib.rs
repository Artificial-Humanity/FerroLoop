//! Pure domain model. No IO, no async, no clock, no network.

pub mod ids;
pub mod log;
pub mod model;
pub mod stale;
pub mod store;
pub mod verdict;

pub use ids::{GateId, ProjectId, RecordId};
pub use log::{Attempt, AttemptStatus, GateRun};
pub use model::{
    AgentSpec, CommandSpec, GateDef, GateKind, PopulationDelivery, Project, Record, Regret,
    Selector, State, Transition,
};
pub use stale::{apply_staleness, is_stale, Staleness};
pub use store::{MemStore, Store, StoreError};
pub use verdict::{FailReason, Population, Verdict};
