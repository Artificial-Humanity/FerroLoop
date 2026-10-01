//! Pure domain model. No IO, no async, no clock, no network.

mod wire;

pub mod at;
#[cfg(any(test, feature = "conformance"))]
#[doc(hidden)]
pub mod conformance;
pub mod decision;
pub mod fault;
pub mod finding;
pub mod ids;
pub mod iri;
pub mod log;
pub mod mem;
pub mod model;
pub mod split;
pub mod stale;
pub mod store;
pub mod verdict;

pub use at::{At, AtError};
pub use decision::{Decision, DecisionKind, Flushed, LeftLocal, Outcome, TransitionOutcome};
pub use fault::LedgerFault;
pub use finding::{Finding, FindingError, FindingState};
pub use ids::{FindingId, GateId, Kind, ProjectId, RecordId};
pub use iri::{Iri, IriError};
pub use log::{Attempt, AttemptStatus, GateRun, PathsTouched};
pub use mem::MemStore;
pub use model::{
    AgentSpec, CommandSpec, GateDef, GateKind, PopulationDelivery, Project, Record, Regret,
    Selector, State, Transition,
};
pub use split::{Batch, Coverage, LocalLedger, Outbox, Pending, RemoteLedger, SplitLedger};
pub use stale::{Staleness, apply_staleness, is_stale};
pub use store::{
    Bindings, Catalog, CatalogChecked, Handles, KindRouted, Ledger, Roles, StoreError, Tracker,
    follow, ledger_root_shape, node_id_shape,
};
pub use verdict::{FailReason, Population, Verdict};

/// Where a project's committed manifest lives, relative to the project root
/// (GitHub tracker spec §4.1). Here rather than in `fl-store` because the
/// engine must recognise the file too: it never makes a gate stale.
pub const MANIFEST_PATH: &str = ".fl/manifest.json";
