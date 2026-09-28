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
pub use ids::{FindingId, GateId, Kind, ProjectId, RecordId};
pub use iri::{Iri, IriError};
pub use log::{Attempt, AttemptStatus, GateRun};
pub use mem::MemStore;
pub use model::{
    AgentSpec, CommandSpec, GateDef, GateKind, PopulationDelivery, Project, Record, Regret,
    Selector, State, Transition,
};
pub use stale::{Staleness, apply_staleness, is_stale};
pub use store::{
    Bindings, Catalog, CatalogChecked, Handles, KindRouted, Ledger, Roles, StoreError, Tracker,
    follow,
};
pub use verdict::{FailReason, Population, Verdict};

/// Where a project's committed manifest lives, relative to the project root
/// (GitHub tracker spec §4.1). Here rather than in `fl-store` because the
/// engine must recognise the file too: it never makes a gate stale.
pub const MANIFEST_PATH: &str = ".fl/manifest.json";
