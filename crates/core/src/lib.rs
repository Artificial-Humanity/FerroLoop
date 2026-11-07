//! Pure domain model. No IO, no async, no clock, no network.

mod wire;

pub mod agent;
pub mod arch;
pub mod artifact_layout;
pub mod at;
pub mod build_info;
pub mod child_env;
pub mod cloud_sessions;
pub mod code_index;
#[cfg(any(test, feature = "conformance"))]
#[doc(hidden)]
pub mod conformance;
pub mod decision;
pub mod doctor;
pub mod embed;
pub mod escalate;
pub mod escalation;
pub mod failover;
pub mod fault;
pub mod feedback;
pub mod ferrostate;
pub mod ferrowire;
pub mod finding;
pub mod governor;
pub mod ids;
pub mod install;
pub mod intent;
pub mod iri;
pub mod local_providers;
pub mod log;
pub mod mem;
#[cfg(any(test, feature = "conformance"))]
#[doc(hidden)]
pub mod mem_issues;
pub mod model;
pub mod notifications;
pub mod permissions;
pub mod pr_policy;
pub mod ref_index;
pub mod routine;
pub mod routine_authority;
pub mod routing;
pub mod session_protocol;
pub mod slice_proof;
pub mod split;
pub mod stale;
pub mod state;
pub mod store;
pub mod telemetry;
pub mod tiered;
pub mod token_count;
pub mod ui_bundle;
pub mod vendor;
pub mod verdict;
pub mod wire_delivery;
pub use wire_delivery::WireDeliveryStore;
pub mod workflow;
pub mod workspace;

pub use agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, RegistryEvent,
    RegistryEventKind, RoleError, RoleRecord, RoleReviewPolicy, builtin_role_records, delete_role,
    get_role, is_builtin_role, list_roles, normalize_role_slug, reports_to, upsert_role,
    validate_agent_identity,
};
pub use arch::{
    ArchComponent, ArchComponentKind, ArchConformanceReport, ArchEdge, ArchError, ArchViolation,
    ArchitectureModel, DEFAULT_ARCH_MANIFEST_PATH, PackageDep, SourceReference,
    check_architecture_conformance, export_likec4_dsl, validate_architecture_model,
};
pub use artifact_layout::{
    ARTIFACT_KINDS, ArtifactLayoutError, ArtifactRetentionPolicyConfig, ArtifactScatterVendor,
    DEDICATED_ARTIFACT_LAYOUT_SLUG, DedicatedArtifactLayoutState, ScatterVendor,
    VendorArtifactScatterOrigin, consolidate_artifact_layout, get_artifact_layout,
    get_artifact_layout_strict, scattered_totals, set_artifact_layout,
};
pub use at::{At, AtError};
pub use cloud_sessions::{
    CloudSessionError, VENDOR_CLOUD_SESSIONS_SLUG, VendorCloudSessionEntry,
    VendorCloudSessionStatus, VendorCloudSessionVendor, attach_cloud_session,
    default_vendor_cloud_sessions, list_cloud_sessions, record_cloud_session_handoff,
    set_cloud_session_status, upsert_cloud_session,
};
pub use code_index::{
    CodeFileRef, CodeIndex, CodeSearchKind, CodeSearchOutcome, CodeSymbolHit, CodeSymbolRef,
    MAX_CODE_FILE_BYTES, TOKEN_GAIN_SLUG, TokenGainSummary, load_token_gain, outcome_from_hits,
    record_token_gain_index, record_token_gain_query, scan_code_index, search_code_index,
};
pub use decision::{Decision, DecisionKind, Flushed, LeftLocal, Outcome, TransitionOutcome};
pub use embed::{
    EmbedConfig, EmbedError, Embedder, HashingEmbedder, MAX_EMBED_BYTES, cosine, decode_weights,
    encode_weights,
};
pub use escalate::Prepared;
pub use escalation::{EscalationFault, Escalations, Mark, Outgoing, Provenance, Tombstone};
pub use failover::{
    AttemptJournal, AttemptJournalEntry, AttemptOutcome, AttemptState, EndpointSet, FailoverAction,
    FailoverDecision, FailoverError, JournalStore, MemJournal, ParentEnvelope, PolicyFingerprint,
    StopReason, TransportWitnessInput, ZeroBytesWitness, canonical_json,
    classify_from_connection_facts, classify_transport_witness, drift_invalidates,
    effective_endpoint_set, is_absolute_http_url, next_action,
};
pub use fault::LedgerFault;
pub use feedback::{
    BindingAuthorization, BindingStatus, DiffSide, EventSource, ExistingMessage,
    FeedbackDeliveryState, FeedbackError, FeedbackFreshness, FeedbackInput, FeedbackKind,
    FeedbackOutboxRecord, FeedbackRouteDecision, FeedbackTrust, FeedbackWireEnvelope,
    GithubFeedbackV1, MAX_FEEDBACK_ENVELOPE_BYTES, MAX_FEEDBACK_EXCERPT_BYTES, PrAuthorBinding,
    ReviewState, RoutingContext, RunClassification, RunConclusion, classify_run,
    compare_existing_message, event_key, normalize_feedback, route_feedback_event,
    sanitize_excerpt,
};
pub use ferrostate::{
    GitRepoProbe, MAX_CHECKPOINT_DIRTY_ENTRIES, StateCheckpointReport, WorkspaceImportReport,
    WorkspaceImportScan, apply_workspace_import, build_checkpoint_upsert, create_state_checkpoint,
    create_state_checkpoint_from_probe, derive_import_slug, find_workspace_root, is_workspace_root,
    probe_git_repo, resolve_checkpoint_slug, resolve_workspace_root, scan_workspace_import,
};
pub use ferrowire::{
    DURABLE_WIRE_RETENTION_MS, DeliveryReceipt, DeliveryStatus, MAX_SUBJECT_BYTES,
    MAX_WIRE_BODY_BYTES, MessageKind, MessageUrgency, SendWireArgs, ThreadId, WireAddress,
    WireFilter, WireImportRejectReason, WireImportRejection, WireImportReport, WireInboxItem,
    WireMessage, fallback_agent_iri, parse_role_selector, resolve_recipients, validate_sender_name,
    validate_subject, validate_wire_body, wire_message_prune_eligible,
};
pub use finding::{Finding, FindingError, FindingState};
pub use governor::{
    AdmissionOutcome, AdmissionRequest, DEFAULT_BUDGET_UNITS, EndpointAlias, GOVERNANCE_CALLS,
    GovernorError, GovernorPolicy, GovernorSnapshot, GovernorStore, LogicalOperationId,
    MemGovernorStore, Permit, PermitRecord, Principal, RecordedOutcome, ReplayAttempt,
    ReplayDecision, ReplayRecord, ReplayRecordAction, ReplayRequest, ScopeKey, ScopePolicy,
    ScopeState, SettleRequest, SettledOutcome, SideEffectClass, WindowState, canonical_endpoint,
    replay_decision, resolve_canonical_endpoint, scopes_for, window_admit,
};
pub use ids::{
    AgentId, FindingId, GateId, Kind, MessageId, ProjectId, RecordId, RevisionId, StateId,
    WorkflowId, WorkflowRunId,
};
pub use intent::{
    AllocationAmount, AllocationCarve, AllocationPool, Binding, CarveRequest, CommitteeCounts,
    CommitteeIntent, CommitteeOutcome, CommitteeTally, CommitteeVerdict, DelegateIntent,
    DelegateTarget, IntentError, IntentResult, QuorumRule, ResultSchema, TypedResult,
    validate_ancestry,
};
pub use iri::{Iri, IriError};
pub use local_providers::{
    KEYLESS_LOOPBACK_REF, LOCAL_MODEL_PROVIDERS_SLUG, LocalLifecycleOp, LocalModelProviderKind,
    LocalModelProviderPreset, LocalProviderError, LocalProviderUpdate, VAULT_REF_PREFIX,
    default_local_model_providers, list_local_model_providers, update_local_model_provider_by_kind,
    upsert_local_model_provider,
};
pub use log::{Attempt, AttemptStatus, GateRun, PathsTouched};
pub use mem::MemStore;
pub use model::{
    AgentSpec, CommandSpec, GateDef, GateKind, PopulationDelivery, Project, Record, Regret,
    Selector, State, Transition,
};
pub use notifications::{
    MAX_PUSH_RECEIPTS, MobilePushChannelStatus, NotificationError, OPERATOR_PUSH_RECEIPTS_SLUG,
    OPERATOR_PUSH_STATE_SLUG, OperatorPushQuietHours, OperatorPushSubscriptionState,
    OperatorPushTriggers, PushDispatchReceipt, PushTriggerKind, WebPushPermissionStatus,
    active_channels, append_push_receipt, get_operator_push_state, list_push_receipts,
    set_operator_push_state, should_deliver,
};
pub use pr_policy::{
    CommitTrailerAuthor, DEFAULT_OWNER_LOGIN, DEFAULT_PROTECTED_PATTERNS, DEFAULT_REVIEWER_APP_IDS,
    DEFAULT_REVIEWER_APP_LOGINS, FlReviewMarker, PolicyRefusal, PrCommitSummary,
    PrPolicyEvaluation, PrReviewSummary, RequiredTier, ReviewVerdict, ReviewerApps,
    authorize_agent_action, authorize_agent_merge, evaluate_pr_policy, extract_review_marker,
    format_review_body, is_protected_path, parse_co_authors, verify_merge_approvals,
    verify_review_integrity,
};
pub use ref_index::{
    MAX_REF_SOURCES, RefIndexError, RefIndexPlan, RefIndexReport, RefIndexStatus, RefKind,
    RefLocatorClass, RefSource, classify_locator, index_local_source,
};
pub use routine::{
    Allocation, Approval, CatchUp, CatchUpEvaluation, CatchUpPage, CatchUpScan, Concurrency,
    FiringRecord, FiringState, RoutineError, RoutineSpec, RoutineState, RoutineTarget, RoutineWork,
    Schedule, ScheduledInstant, Window, firing_key,
};
pub use routine_authority::{
    AuthorityError, LifecycleAuthority, LifecycleTarget, LifecycleVerificationRequest,
    LogicalWorkIdentity, ProofRegistryVerifier, ProofVerificationRequest, TerminalReceiptVerifier,
    TerminalVerificationRequest, UnavailableAuthority, VerifiedLifecycle, VerifiedProof,
    VerifiedTerminal, VerifiedTerminalOutcome,
};
pub use routing::{AreaRoute, ForeignRecord, GithubTier, Routes, RoutingFault, RoutingMap, Tier};
pub use session_protocol::{
    AcpOperation, AcpProfileFingerprint, AcpProfileProofRecord, ConformanceState,
    OperationEvidence, SessionProtocolKind,
};
pub use slice_proof::{
    AccountingUnit, AdmittedSliceBound, EpsilonStepBound, SliceAdmissionRefusal, SliceBoundKind,
    SliceBoundProof, SliceProofInput, Usd, evaluate_slice_bounds,
};
pub use split::{
    Batch, CachedSegment, Coverage, LedgerCache, LedgerMemory, LocalLedger, Outbox, Pending,
    RemoteLedger, SplitLedger,
};
pub use stale::{Staleness, apply_staleness, is_stale};
pub use state::{
    HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
    RuntimeEntry, SensitivityPatch, StateConcern, StateConflict, StateEntry, StateError,
    StatePayload, StateRevision, StateScope, StateSearchHit, StateSyncCursor, UpsertStateArgs,
    estimate_tokens, rank_weight, search_state_entries, validate_slug, validate_summary,
};
pub use store::{
    AgentRegistry, Bindings, Catalog, CatalogChecked, Handles, KindRouted, Ledger, MessageStore,
    Roles, StateStore, StoreError, Tracker, WorkflowStore, as_clause, follow, ledger_root_shape,
    node_id_shape,
};
pub use tiered::{FindingPlacement, Placement, RecordSeen, TieredTracker};
pub use token_count::{CountUnit, count_unit, counted_tokens};
pub use vendor::{
    ApiDialect, VendorDescriptor, VendorError, VendorRegistry, VendorTransport,
    ensure_vendor_registered, lookup_vendor, register_vendor, validate_api_base_url,
};
pub use verdict::{FailReason, Population, Verdict};
pub use workflow::{
    ApprovalTier, AssigneeRule, DEFAULT_MAX_TOTAL_STEPS, DEFAULT_STEP_MAX_ATTEMPTS,
    DEFAULT_STEP_MAX_VISITS, END_STEP, EdgeCondition, EdgeDef, JoinPolicy,
    ReviewIndependenceViolation, SelectedStepAgent, StepAttemptReport, StepCompletion, StepDef,
    StepInstance, StepOutcome, WorkflowDef, WorkflowError, WorkflowRun, WorkflowRunStatus,
    advance_workflow, authorize_step_sign_off, select_step_agent, validate_reviewer_independence,
    validate_workflow_def,
};
pub use workspace::{
    ContentEvidence, ExclusionPlan, ExclusionReceipt, HookExecution, HookPlan, HookRun,
    HookRunState, HookSpec, InRepoIsolation, PlanInputs, PlanRefusal, PortLease, PortRange,
    ResolvedWorkspacePaths, ServiceSpec, TeardownPolicy, WorkspaceApproval, WorkspaceError,
    WorkspaceOperation, WorkspacePlan, WorkspacePolicy, WorktreeConfig, WorktreeMode,
    WorktreeRecord, executable_digest, plan_workspace, resolve_worktree_paths,
    workspace_policy_digest,
};

/// Where a project's committed manifest lives, relative to the project root
/// (GitHub tracker spec §4.1). Here rather than in `fl-store` because the
/// engine must recognise the file too: it never makes a gate stale.
pub const MANIFEST_PATH: &str = ".fl/manifest.json";

pub mod f1_memory;
pub mod f1_records;
pub use f1_records::{
    AllocationEvidenceRecord, ControlRecordKey, F1PreparationStore, F1RecordError,
    IntentDefinition, IntentEvidenceRecord, RoutineProposalRecord, SafetyBatch, SafetyChange,
    SafetyState,
};

// A process-local, fail-closed preparation backend for tests and embedded callers.
pub use f1_memory::MemoryF1PreparationStore;
