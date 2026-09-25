//! Pure domain model. No IO, no async, no clock, no network.

mod wire;

#[cfg(any(test, feature = "conformance"))]
#[doc(hidden)]
pub mod conformance;
pub mod finding;
pub mod ids;
pub mod iri;
pub mod log;
pub mod mem;
pub mod model;
pub mod stale;
pub mod store;
pub mod verdict;

pub use finding::{Finding, FindingError, FindingState};
pub use ids::{FindingId, GateId, ProjectId, RecordId};
pub use iri::{Iri, IriError};
pub use log::{Attempt, AttemptStatus, GateRun};
pub use mem::MemStore;
pub use model::{
    AgentSpec, CommandSpec, GateDef, GateKind, PopulationDelivery, Project, Record, Regret,
    Selector, State, Transition,
};
pub use stale::{Staleness, apply_staleness, is_stale};
pub use store::{Catalog, Ledger, Roles, StoreError, Tracker};
pub use verdict::{FailReason, Population, Verdict};
