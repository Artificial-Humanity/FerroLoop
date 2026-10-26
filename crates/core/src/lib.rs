//! Pure domain model. No IO, no async, no clock, no network.

mod wire;

pub mod agent;
pub mod at;
#[cfg(any(test, feature = "conformance"))]
#[doc(hidden)]
pub mod conformance;
pub mod decision;
pub mod escalate;
pub mod escalation;
pub mod fault;
pub mod ferrowire;
pub mod finding;
pub mod ids;
pub mod iri;
pub mod log;
pub mod mem;
#[cfg(any(test, feature = "conformance"))]
#[doc(hidden)]
pub mod mem_issues;
pub mod model;
pub mod routing;
pub mod split;
pub mod stale;
pub mod state;
pub mod store;
pub mod telemetry;
pub mod tiered;
pub mod verdict;

pub use agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, RegistryEvent,
    RegistryEventKind, reports_to, validate_agent_identity,
};
pub use at::{At, AtError};
pub use decision::{Decision, DecisionKind, Flushed, LeftLocal, Outcome, TransitionOutcome};
pub use escalate::Prepared;
pub use escalation::{EscalationFault, Escalations, Mark, Outgoing, Provenance, Tombstone};
pub use fault::LedgerFault;
pub use ferrowire::{
    DeliveryReceipt, DeliveryStatus, MAX_SUBJECT_BYTES, MAX_WIRE_BODY_BYTES, MessageKind,
    MessageUrgency, SendWireArgs, ThreadId, WireAddress, WireFilter, WireInboxItem, WireMessage,
    fallback_agent_iri, parse_role_selector, resolve_recipients, validate_sender_name,
    validate_subject, validate_wire_body,
};
pub use finding::{Finding, FindingError, FindingState};
pub use ids::{
    AgentId, FindingId, GateId, Kind, MessageId, ProjectId, RecordId, RevisionId, StateId,
};
pub use iri::{Iri, IriError};
pub use log::{Attempt, AttemptStatus, GateRun, PathsTouched};
pub use mem::MemStore;
pub use model::{
    AgentSpec, CommandSpec, GateDef, GateKind, PopulationDelivery, Project, Record, Regret,
    Selector, State, Transition,
};
pub use routing::{AreaRoute, ForeignRecord, GithubTier, Routes, RoutingFault, RoutingMap, Tier};
pub use split::{
    Batch, CachedSegment, Coverage, LedgerCache, LedgerMemory, LocalLedger, Outbox, Pending,
    RemoteLedger, SplitLedger,
};
pub use stale::{Staleness, apply_staleness, is_stale};
pub use state::{
    HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
    RuntimeEntry, SensitivityPatch, StateConcern, StateConflict, StateEntry, StateError,
    StatePayload, StateRevision, StateScope, StateSyncCursor, UpsertStateArgs, validate_slug,
    validate_summary,
};
pub use store::{
    AgentRegistry, Bindings, Catalog, CatalogChecked, Handles, KindRouted, Ledger, MessageStore,
    Roles, StateStore, StoreError, Tracker, as_clause, follow, ledger_root_shape, node_id_shape,
};
pub use tiered::{FindingPlacement, Placement, RecordSeen, TieredTracker};
pub use verdict::{FailReason, Population, Verdict};

/// Where a project's committed manifest lives, relative to the project root
/// (GitHub tracker spec §4.1). Here rather than in `fl-store` because the
/// engine must recognise the file too: it never makes a gate stale.
pub const MANIFEST_PATH: &str = ".fl/manifest.json";
