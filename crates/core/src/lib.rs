//! Pure domain model. No IO, no async, no clock, no network.

mod wire;

pub mod agent;
pub mod arch;
pub mod at;
pub mod code_index;
#[cfg(any(test, feature = "conformance"))]
#[doc(hidden)]
pub mod conformance;
pub mod decision;
pub mod escalate;
pub mod escalation;
pub mod fault;
pub mod ferrostate;
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
pub mod pr_policy;
pub mod routing;
pub mod split;
pub mod stale;
pub mod state;
pub mod store;
pub mod telemetry;
pub mod tiered;
pub mod verdict;
pub mod workflow;

pub use agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, RegistryEvent,
    RegistryEventKind, reports_to, validate_agent_identity,
};
pub use arch::{
    ArchComponent, ArchComponentKind, ArchConformanceReport, ArchEdge, ArchError, ArchViolation,
    ArchitectureModel, DEFAULT_ARCH_MANIFEST_PATH, PackageDep, SourceReference,
    check_architecture_conformance, export_likec4_dsl, validate_architecture_model,
};
pub use at::{At, AtError};
pub use code_index::{
    CodeIndex, CodeSearchKind, CodeSearchOutcome, CodeSymbolHit, MAX_CODE_FILE_BYTES,
    TOKEN_GAIN_SLUG, TokenGainSummary, load_token_gain, record_token_gain_index,
    record_token_gain_query, scan_code_index, search_code_index,
};
pub use decision::{Decision, DecisionKind, Flushed, LeftLocal, Outcome, TransitionOutcome};
pub use escalate::Prepared;
pub use escalation::{EscalationFault, Escalations, Mark, Outgoing, Provenance, Tombstone};
pub use fault::LedgerFault;
pub use ferrostate::{
    GitRepoProbe, MAX_CHECKPOINT_DIRTY_ENTRIES, StateCheckpointReport, WorkspaceImportReport,
    WorkspaceImportScan, apply_workspace_import, build_checkpoint_upsert, create_state_checkpoint,
    create_state_checkpoint_from_probe, derive_import_slug, probe_git_repo,
    resolve_checkpoint_slug, resolve_workspace_root, scan_workspace_import,
};
pub use ferrowire::{
    DeliveryReceipt, DeliveryStatus, MAX_SUBJECT_BYTES, MAX_WIRE_BODY_BYTES, MessageKind,
    MessageUrgency, SendWireArgs, ThreadId, WireAddress, WireFilter, WireImportRejectReason,
    WireImportRejection, WireImportReport, WireInboxItem, WireMessage, fallback_agent_iri,
    parse_role_selector, resolve_recipients, validate_sender_name, validate_subject,
    validate_wire_body,
};
pub use finding::{Finding, FindingError, FindingState};
pub use ids::{
    AgentId, FindingId, GateId, Kind, MessageId, ProjectId, RecordId, RevisionId, StateId,
    WorkflowId, WorkflowRunId,
};
pub use iri::{Iri, IriError};
pub use log::{Attempt, AttemptStatus, GateRun, PathsTouched};
pub use mem::MemStore;
pub use model::{
    AgentSpec, CommandSpec, GateDef, GateKind, PopulationDelivery, Project, Record, Regret,
    Selector, State, Transition,
};
pub use pr_policy::{
    CommitTrailerAuthor, DEFAULT_OWNER_LOGIN, DEFAULT_PROTECTED_PATTERNS, DEFAULT_REVIEWER_APP_IDS,
    DEFAULT_REVIEWER_APP_LOGINS, FlReviewMarker, PolicyRefusal, PrCommitSummary,
    PrPolicyEvaluation, PrReviewSummary, RequiredTier, ReviewVerdict, ReviewerApps,
    authorize_agent_action, authorize_agent_merge, evaluate_pr_policy, extract_review_marker,
    format_review_body, is_protected_path, parse_co_authors, verify_merge_approvals,
    verify_review_integrity,
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
    StatePayload, StateRevision, StateScope, StateSearchHit, StateSyncCursor, UpsertStateArgs,
    estimate_tokens, search_state_entries, validate_slug, validate_summary,
};
pub use store::{
    AgentRegistry, Bindings, Catalog, CatalogChecked, Handles, KindRouted, Ledger, MessageStore,
    Roles, StateStore, StoreError, Tracker, WorkflowStore, as_clause, follow, ledger_root_shape,
    node_id_shape,
};
pub use tiered::{FindingPlacement, Placement, RecordSeen, TieredTracker};
pub use verdict::{FailReason, Population, Verdict};
pub use workflow::{
    ApprovalTier, AssigneeRule, DEFAULT_MAX_TOTAL_STEPS, DEFAULT_STEP_MAX_ATTEMPTS,
    DEFAULT_STEP_MAX_VISITS, END_STEP, EdgeCondition, EdgeDef, JoinPolicy,
    ReviewIndependenceViolation, SelectedStepAgent, StepAttemptReport, StepCompletion, StepDef,
    StepInstance, StepOutcome, WorkflowDef, WorkflowError, WorkflowRun, WorkflowRunStatus,
    advance_workflow, authorize_step_sign_off, select_step_agent, validate_reviewer_independence,
    validate_workflow_def,
};

/// Where a project's committed manifest lives, relative to the project root
/// (GitHub tracker spec §4.1). Here rather than in `fl-store` because the
/// engine must recognise the file too: it never makes a gate stale.
pub const MANIFEST_PATH: &str = ".fl/manifest.json";
