//! Pure GitHub feedback evidence. No API access, authentication, delivery or model dispatch.
//! Callers must authenticate GitHub data, binding receipts, session state and admission separately.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_FEEDBACK_EXCERPT_BYTES: usize = 8 * 1024;
pub const MAX_FEEDBACK_ENVELOPE_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeedbackError {
    #[error("invalid feedback field: {0}")]
    Invalid(&'static str),
    #[error("feedback envelope exceeds size limit")]
    EnvelopeTooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackKind {
    CiFailed,
    CiRecovered,
    ReviewSubmitted,
    ReviewComment,
    PrComment,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackFreshness {
    Current,
    Stale,
    Unmapped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackTrust {
    ExternalEvidence,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunConclusion {
    Success,
    Failure,
    TimedOut,
    ActionRequired,
    Cancelled,
    Skipped,
    Neutral,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunClassification {
    Feedback(FeedbackKind),
    StatusOnly,
}
/// Call only for completed Actions workflow runs. Recovery requires a previously recorded failure.
pub fn classify_run(conclusion: RunConclusion, prior_failure: bool) -> RunClassification {
    match conclusion {
        RunConclusion::Failure | RunConclusion::TimedOut | RunConclusion::ActionRequired => {
            RunClassification::Feedback(FeedbackKind::CiFailed)
        }
        RunConclusion::Success if prior_failure => {
            RunClassification::Feedback(FeedbackKind::CiRecovered)
        }
        _ => RunClassification::StatusOnly,
    }
}

/// Event identity includes the source family to keep review summaries and comments distinct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventSource {
    WorkflowRun {
        object_id: u64,
        attempt: u32,
        conclusion: RunConclusion,
    },
    Review {
        object_id: u64,
        state: ReviewState,
        body_sha256: String,
    },
    ReviewComment {
        object_id: u64,
        updated_at: String,
        body_sha256: String,
    },
    PrComment {
        object_id: u64,
        updated_at: String,
        body_sha256: String,
    },
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn nonempty(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}
fn valid_origin(s: &str) -> bool {
    let Some(host) = s.strip_prefix("https://") else {
        return false;
    };
    nonempty(host, 512)
        && !host.contains(['/', '?', '#', '@', ' ', '\\'])
        && !host.starts_with(':')
        && !host.ends_with(':')
}
fn valid_oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}
impl EventSource {
    pub fn workflow_run(
        object_id: u64,
        attempt: u32,
        conclusion: RunConclusion,
    ) -> Result<Self, FeedbackError> {
        if object_id == 0 || attempt == 0 {
            return Err(FeedbackError::Invalid("workflow identity"));
        }
        Ok(Self::WorkflowRun {
            object_id,
            attempt,
            conclusion,
        })
    }
    pub fn review(object_id: u64, state: ReviewState, body: &str) -> Result<Self, FeedbackError> {
        if object_id == 0 {
            return Err(FeedbackError::Invalid("review identity"));
        }
        Ok(Self::Review {
            object_id,
            state,
            body_sha256: sha(body.as_bytes()),
        })
    }
    pub fn review_comment(
        object_id: u64,
        updated_at: &str,
        body: &str,
    ) -> Result<Self, FeedbackError> {
        if object_id == 0 || !nonempty(updated_at, 128) {
            return Err(FeedbackError::Invalid("comment identity"));
        }
        Ok(Self::ReviewComment {
            object_id,
            updated_at: updated_at.into(),
            body_sha256: sha(body.as_bytes()),
        })
    }
    pub fn pr_comment(object_id: u64, updated_at: &str, body: &str) -> Result<Self, FeedbackError> {
        if object_id == 0 || !nonempty(updated_at, 128) {
            return Err(FeedbackError::Invalid("comment identity"));
        }
        Ok(Self::PrComment {
            object_id,
            updated_at: updated_at.into(),
            body_sha256: sha(body.as_bytes()),
        })
    }
    fn validate(&self) -> Result<(), FeedbackError> {
        let hash = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        let valid = match self {
            Self::WorkflowRun {
                object_id, attempt, ..
            } => *object_id > 0 && *attempt > 0,
            Self::Review {
                object_id,
                body_sha256,
                ..
            } => *object_id > 0 && hash(body_sha256),
            Self::ReviewComment {
                object_id,
                updated_at,
                body_sha256,
            }
            | Self::PrComment {
                object_id,
                updated_at,
                body_sha256,
            } => *object_id > 0 && nonempty(updated_at, 128) && hash(body_sha256),
        };
        if valid {
            Ok(())
        } else {
            Err(FeedbackError::Invalid("source"))
        }
    }
    fn object_id(&self) -> u64 {
        match self {
            Self::WorkflowRun { object_id, .. }
            | Self::Review { object_id, .. }
            | Self::ReviewComment { object_id, .. }
            | Self::PrComment { object_id, .. } => *object_id,
        }
    }
    fn version(&self) -> String {
        match self {
            Self::WorkflowRun {
                attempt,
                conclusion,
                ..
            } => format!(
                "{}:{}",
                attempt,
                serde_json::to_string(conclusion).unwrap_or_default()
            ),
            Self::Review {
                state, body_sha256, ..
            } => format!(
                "{}:{body_sha256}",
                serde_json::to_string(state).unwrap_or_default()
            ),
            Self::ReviewComment {
                updated_at,
                body_sha256,
                ..
            }
            | Self::PrComment {
                updated_at,
                body_sha256,
                ..
            } => format!("{updated_at}:{body_sha256}"),
        }
    }
}
/// SHA-256 of individually length-prefixed origin, numeric repository identity and typed source.
pub fn event_key(
    origin: &str,
    repository_id: u64,
    source: &EventSource,
) -> Result<String, FeedbackError> {
    if !valid_origin(origin) || repository_id == 0 {
        return Err(FeedbackError::Invalid("origin or repository"));
    }
    source.validate()?;
    let source_json = serde_json::to_vec(source).map_err(|_| FeedbackError::Invalid("source"))?;
    let repository = repository_id.to_be_bytes();
    let mut h = Sha256::new();
    for value in [
        origin.as_bytes(),
        repository.as_slice(),
        source_json.as_slice(),
    ] {
        h.update((value.len() as u64).to_be_bytes());
        h.update(value);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// Strip control sequences, including full ANSI CSI/OSC sequences, and cap at a UTF-8 boundary.
pub fn sanitize_excerpt(input: &str) -> (String, bool) {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    let mut truncated = false;
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        if c.is_control() && !matches!(c, '\n' | '\t') {
            continue;
        }
        if out.len() + c.len_utf8() > MAX_FEEDBACK_EXCERPT_BYTES {
            truncated = true;
            break;
        }
        out.push(c);
    }
    (out, truncated)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    Left,
    Right,
}
/// Normalized, bounded GitHub evidence. The body has no authority and is never a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubFeedbackV1 {
    pub schema_version: u32,
    pub event_key: String,
    pub api_origin: String,
    pub repository_id: u64,
    pub pr_number: u64,
    pub binding_id: String,
    pub binding_revision: u64,
    pub kind: FeedbackKind,
    pub source_object_id: u64,
    pub source_version: String,
    pub run_attempt: Option<u32>,
    pub source_head_sha: String,
    pub current_head_sha: String,
    pub actor_id: u64,
    pub occurred_at: String,
    pub received_at: String,
    pub url: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub side: Option<DiffSide>,
    pub outdated: bool,
    pub body_excerpt: String,
    pub truncated: bool,
    pub freshness: FeedbackFreshness,
    pub trust: FeedbackTrust,
}
/// Values here must be obtained through an authenticated GitHub API; URLs in body text are not followed.
#[derive(Debug, Clone)]
pub struct FeedbackInput<'a> {
    pub api_origin: &'a str,
    pub repository_id: u64,
    pub pr_number: u64,
    pub binding_id: &'a str,
    pub binding_revision: u64,
    pub source: &'a EventSource,
    pub previous_run_failed: bool,
    pub source_head_sha: &'a str,
    pub current_head_sha: &'a str,
    /// Whether the source was mapped to this PR by verified metadata (not branch name).
    pub association_verified: bool,
    pub actor_id: u64,
    pub occurred_at: &'a str,
    pub received_at: &'a str,
    pub authenticated_url: &'a str,
    pub path: Option<&'a str>,
    pub line: Option<u64>,
    pub side: Option<DiffSide>,
    pub outdated: bool,
    pub body: &'a str,
    /// Set only after the caller authenticates the broker actor and marker, never from text alone.
    pub authenticated_broker_output: bool,
}
/// `None` means status-only Actions evidence or authenticated broker output; neither is routed as a feedback event.
pub fn normalize_feedback(
    input: &FeedbackInput<'_>,
) -> Result<Option<GithubFeedbackV1>, FeedbackError> {
    if input.authenticated_broker_output {
        return Ok(None);
    }
    input.source.validate()?;
    if !valid_origin(input.api_origin)
        || input.repository_id == 0
        || input.pr_number == 0
        || input.actor_id == 0
        || !nonempty(input.binding_id, 256)
        || !valid_oid(input.source_head_sha)
        || !valid_oid(input.current_head_sha)
        || !nonempty(input.occurred_at, 128)
        || !nonempty(input.received_at, 128)
        || !input.authenticated_url.starts_with("https://")
        || !nonempty(input.authenticated_url, 4096)
        || input.path.is_some_and(|p| !nonempty(p, 4096))
        || input.line == Some(0)
    {
        return Err(FeedbackError::Invalid("feedback metadata"));
    }
    let expected_body_hash = match input.source {
        EventSource::Review { body_sha256, .. }
        | EventSource::ReviewComment { body_sha256, .. }
        | EventSource::PrComment { body_sha256, .. } => Some(body_sha256),
        EventSource::WorkflowRun { .. } => None,
    };
    if expected_body_hash.is_some_and(|hash| *hash != sha(input.body.as_bytes())) {
        return Err(FeedbackError::Invalid("source content hash"));
    }
    let (kind, attempt) = match input.source {
        EventSource::WorkflowRun {
            attempt,
            conclusion,
            ..
        } => match classify_run(*conclusion, input.previous_run_failed) {
            RunClassification::Feedback(kind) => (kind, Some(*attempt)),
            RunClassification::StatusOnly => return Ok(None),
        },
        EventSource::Review { .. } => (FeedbackKind::ReviewSubmitted, None),
        EventSource::ReviewComment { .. } => (FeedbackKind::ReviewComment, None),
        EventSource::PrComment { .. } => (FeedbackKind::PrComment, None),
    };
    let (body_excerpt, truncated) = sanitize_excerpt(input.body);
    let event = GithubFeedbackV1 {
        schema_version: 1,
        event_key: event_key(input.api_origin, input.repository_id, input.source)?,
        api_origin: input.api_origin.into(),
        repository_id: input.repository_id,
        pr_number: input.pr_number,
        binding_id: input.binding_id.into(),
        binding_revision: input.binding_revision,
        kind,
        source_object_id: input.source.object_id(),
        source_version: input.source.version(),
        run_attempt: attempt,
        source_head_sha: input.source_head_sha.to_ascii_lowercase(),
        current_head_sha: input.current_head_sha.to_ascii_lowercase(),
        actor_id: input.actor_id,
        occurred_at: input.occurred_at.into(),
        received_at: input.received_at.into(),
        url: input.authenticated_url.into(),
        path: input.path.map(str::to_owned),
        line: input.line,
        side: input.side,
        outdated: input.outdated,
        body_excerpt,
        truncated,
        freshness: if !input.association_verified {
            FeedbackFreshness::Unmapped
        } else if input.outdated
            || !input
                .source_head_sha
                .eq_ignore_ascii_case(input.current_head_sha)
        {
            FeedbackFreshness::Stale
        } else {
            FeedbackFreshness::Current
        },
        trust: FeedbackTrust::ExternalEvidence,
    };
    if serde_json::to_vec(&event)
        .map_err(|_| FeedbackError::Invalid("event"))?
        .len()
        > MAX_FEEDBACK_ENVELOPE_BYTES
    {
        return Err(FeedbackError::EnvelopeTooLarge);
    }
    Ok(Some(event))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingStatus {
    Unverified,
    Active,
    Retired,
}
/// Evidence is an opaque, nonsecret identifier. The caller must authenticate the receipt before use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BindingAuthorization {
    Owner {
        approval_receipt: String,
    },
    ManagedHost {
        capability_issuer: String,
        authorization_receipt: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrAuthorBinding {
    pub binding_id: String,
    pub project_id: String,
    pub api_origin: String,
    pub repository_id: u64,
    pub repository_full_name: String,
    pub pr_number: u64,
    pub head_repository_id: u64,
    pub head_ref: String,
    pub head_sha: String,
    pub author_agent_id: String,
    pub author_session_incarnation: String,
    pub worktree_id: String,
    pub binding_revision: u64,
    pub approved_by: Option<String>,
    pub approved_at: Option<String>,
    pub status: BindingStatus,
    pub authorization: Option<BindingAuthorization>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingContext {
    pub api_origin: String,
    pub repository_id: u64,
    pub pr_number: u64,
    pub head_repository_id: u64,
    pub head_sha: String,
    pub binding_revision: u64,
    pub session_incarnation: String,
    pub session_live: bool,
    pub dispatch_ambiguous: bool,
    /// True only after the caller authenticates the receipt and its scoped binding.
    pub binding_verified: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum FeedbackRouteDecision {
    OperatorPending,
    Stale,
    AmbiguousDispatch,
    ManagedSession {
        agent_id: String,
        incarnation_id: String,
        binding_revision: u64,
    },
}
/// Plans evidence routing only. Inputs and receipts must be verified independently by the caller.
/// This does not authorize a model turn, choose a persona route, or permit ambiguous redelivery.
fn route_feedback(binding: &PrAuthorBinding, current: &RoutingContext) -> FeedbackRouteDecision {
    if current.dispatch_ambiguous {
        return FeedbackRouteDecision::AmbiguousDispatch;
    }
    let authorized = match &binding.authorization {
        Some(BindingAuthorization::Owner { approval_receipt }) => nonempty(approval_receipt, 1024),
        Some(BindingAuthorization::ManagedHost {
            capability_issuer,
            authorization_receipt,
        }) => nonempty(capability_issuer, 1024) && nonempty(authorization_receipt, 1024),
        None => false,
    };
    if !current.binding_verified
        || binding.status != BindingStatus::Active
        || !authorized
        || !valid_origin(&binding.api_origin)
        || binding.repository_id == 0
        || binding.pr_number == 0
        || binding.head_repository_id == 0
        || !nonempty(&binding.binding_id, 256)
        || !nonempty(&binding.project_id, 256)
        || !nonempty(&binding.author_agent_id, 256)
        || !nonempty(&binding.author_session_incarnation, 256)
        || !nonempty(&binding.worktree_id, 256)
        || !valid_oid(&binding.head_sha)
        || !current.session_live
        || binding.author_session_incarnation != current.session_incarnation
        || binding.binding_revision != current.binding_revision
        || binding.api_origin != current.api_origin
        || binding.repository_id != current.repository_id
        || binding.pr_number != current.pr_number
        || binding.head_repository_id != current.head_repository_id
    {
        return FeedbackRouteDecision::OperatorPending;
    }
    if !valid_oid(&current.head_sha) || !binding.head_sha.eq_ignore_ascii_case(&current.head_sha) {
        return FeedbackRouteDecision::Stale;
    }
    FeedbackRouteDecision::ManagedSession {
        agent_id: binding.author_agent_id.clone(),
        incarnation_id: binding.author_session_incarnation.clone(),
        binding_revision: binding.binding_revision,
    }
}

/// Also fences the exact observed event against the current binding and PR head.
pub fn route_feedback_event(
    binding: &PrAuthorBinding,
    current: &RoutingContext,
    event: &GithubFeedbackV1,
) -> FeedbackRouteDecision {
    if current.dispatch_ambiguous {
        return FeedbackRouteDecision::AmbiguousDispatch;
    }
    if event.schema_version != 1
        || event.trust != FeedbackTrust::ExternalEvidence
        || event.api_origin != binding.api_origin
        || event.repository_id != binding.repository_id
        || event.pr_number != binding.pr_number
        || event.binding_id != binding.binding_id
        || event.binding_revision != binding.binding_revision
        || event.freshness == FeedbackFreshness::Unmapped
    {
        return FeedbackRouteDecision::OperatorPending;
    }
    if event.freshness == FeedbackFreshness::Stale
        || !event
            .source_head_sha
            .eq_ignore_ascii_case(&current.head_sha)
        || !event
            .current_head_sha
            .eq_ignore_ascii_case(&current.head_sha)
    {
        return FeedbackRouteDecision::Stale;
    }
    route_feedback(binding, current)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackDeliveryState {
    Observed,
    Staged,
    WireRecorded,
    SessionPresented,
    RemediationQueued,
    ProtocolAccepted,
    Completed,
    Failed,
    OperatorPending,
    Stale,
    RefusedBudget,
    AmbiguousDispatch,
}
/// Immutable message arguments selected and persisted by the caller before any insertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackWireEnvelope {
    pub message_id: String,
    pub event_key: String,
    pub routing_revision: u64,
    pub target_agent_id: String,
    pub target_incarnation: String,
    pub body_json: String,
}
impl FeedbackWireEnvelope {
    /// Check the complete encoded envelope before persisting or publishing it.
    pub fn validate(&self) -> Result<(), FeedbackError> {
        if !nonempty(&self.message_id, 256)
            || !nonempty(&self.event_key, 256)
            || !nonempty(&self.target_agent_id, 256)
            || !nonempty(&self.target_incarnation, 256)
            || serde_json::from_str::<serde_json::Value>(&self.body_json).is_err()
        {
            return Err(FeedbackError::Invalid("wire envelope"));
        }
        if serde_json::to_vec(self)
            .map_err(|_| FeedbackError::Invalid("wire envelope"))?
            .len()
            > MAX_FEEDBACK_ENVELOPE_BYTES
        {
            return Err(FeedbackError::EnvelopeTooLarge);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackOutboxRecord {
    pub envelope: FeedbackWireEnvelope,
    pub state: FeedbackDeliveryState,
    pub revision: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingMessage {
    ExactMatch,
    Corruption,
}
/// A duplicate message ID is success only when every immutable argument is equal.
pub fn compare_existing_message(
    expected: &FeedbackWireEnvelope,
    existing: &FeedbackWireEnvelope,
) -> ExistingMessage {
    if expected.validate().is_ok() && existing.validate().is_ok() && expected == existing {
        ExistingMessage::ExactMatch
    } else {
        ExistingMessage::Corruption
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> PrAuthorBinding {
        PrAuthorBinding {
            binding_id: "binding".into(),
            project_id: "project".into(),
            api_origin: "https://api.github.com".into(),
            repository_id: 42,
            repository_full_name: "org/repo".into(),
            pr_number: 5,
            head_repository_id: 42,
            head_ref: "work".into(),
            head_sha: "a".repeat(40),
            author_agent_id: "agent".into(),
            author_session_incarnation: "incarnation".into(),
            worktree_id: "tree".into(),
            binding_revision: 3,
            approved_by: None,
            approved_at: None,
            status: BindingStatus::Unverified,
            authorization: None,
        }
    }

    #[test]
    fn event_keys_separate_sources_attempts_versions_and_repositories() {
        let a = EventSource::workflow_run(7, 1, RunConclusion::Failure).unwrap();
        let b = EventSource::workflow_run(7, 2, RunConclusion::Failure).unwrap();
        assert_ne!(
            event_key("https://api.github.com", 42, &a).unwrap(),
            event_key("https://api.github.com", 42, &b).unwrap()
        );
        let review = EventSource::review(7, ReviewState::Approved, "ok").unwrap();
        let comment = EventSource::review_comment(7, "2026-01-01T00:00:00Z", "ok").unwrap();
        assert_ne!(
            event_key("https://api.github.com", 42, &review).unwrap(),
            event_key("https://api.github.com", 42, &comment).unwrap()
        );
        assert_ne!(
            event_key("https://api.github.com", 42, &a).unwrap(),
            event_key("https://api.github.com", 43, &a).unwrap()
        );
        assert_eq!(
            event_key("https://api.github.com", 42, &a).unwrap(),
            event_key("https://api.github.com", 42, &a).unwrap()
        );
    }

    #[test]
    fn workflow_classification_never_treats_cancel_as_code_failure() {
        assert_eq!(
            classify_run(RunConclusion::Failure, false),
            RunClassification::Feedback(FeedbackKind::CiFailed)
        );
        assert_eq!(
            classify_run(RunConclusion::TimedOut, false),
            RunClassification::Feedback(FeedbackKind::CiFailed)
        );
        assert_eq!(
            classify_run(RunConclusion::ActionRequired, false),
            RunClassification::Feedback(FeedbackKind::CiFailed)
        );
        assert_eq!(
            classify_run(RunConclusion::Success, true),
            RunClassification::Feedback(FeedbackKind::CiRecovered)
        );
        assert_eq!(
            classify_run(RunConclusion::Success, false),
            RunClassification::StatusOnly
        );
        assert_eq!(
            classify_run(RunConclusion::Cancelled, true),
            RunClassification::StatusOnly
        );
        assert_eq!(
            classify_run(RunConclusion::Neutral, false),
            RunClassification::StatusOnly
        );
    }

    #[test]
    fn text_is_bounded_and_terminal_sequences_removed() {
        let (v, truncated) =
            sanitize_excerpt(&format!("ok\u{1b}[31mred\u{1b}[0m{}", "é".repeat(5000)));
        assert!(truncated && v.len() <= 8192 && !v.contains('\u{1b}') && v.contains("okred"));
    }

    #[test]
    fn unverified_claims_cannot_route_and_binding_must_match_current_head() {
        let mut b = binding();
        let ctx = RoutingContext {
            api_origin: "https://api.github.com".into(),
            repository_id: 42,
            pr_number: 5,
            head_repository_id: 42,
            head_sha: "a".repeat(40),
            binding_revision: 3,
            session_incarnation: "incarnation".into(),
            session_live: true,
            dispatch_ambiguous: false,
            binding_verified: true,
        };
        assert_eq!(
            route_feedback(&b, &ctx),
            FeedbackRouteDecision::OperatorPending
        );
        b.status = BindingStatus::Active;
        assert_eq!(
            route_feedback(&b, &ctx),
            FeedbackRouteDecision::OperatorPending
        );
        b.authorization = Some(BindingAuthorization::Owner {
            approval_receipt: "receipt".into(),
        });
        assert_eq!(
            route_feedback(&b, &ctx),
            FeedbackRouteDecision::ManagedSession {
                agent_id: "agent".into(),
                incarnation_id: "incarnation".into(),
                binding_revision: 3
            }
        );
        let mut stale = ctx.clone();
        stale.head_sha = "b".repeat(40);
        assert_eq!(route_feedback(&b, &stale), FeedbackRouteDecision::Stale);
        let mut retired = ctx.clone();
        retired.session_live = false;
        assert_eq!(
            route_feedback(&b, &retired),
            FeedbackRouteDecision::OperatorPending
        );
        let mut ambiguous = ctx;
        ambiguous.dispatch_ambiguous = true;
        assert_eq!(
            route_feedback(&b, &ambiguous),
            FeedbackRouteDecision::AmbiguousDispatch
        );
    }

    fn feedback_input<'a>(source: &'a EventSource, body: &'a str) -> FeedbackInput<'a> {
        FeedbackInput {
            api_origin: "https://api.github.com",
            repository_id: 42,
            pr_number: 5,
            binding_id: "binding",
            binding_revision: 3,
            source,
            previous_run_failed: false,
            source_head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            current_head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            association_verified: true,
            actor_id: 77,
            occurred_at: "2026-01-01T00:00:00Z",
            received_at: "2026-01-01T00:00:01Z",
            authenticated_url: "https://github.com/org/repo/pull/5",
            path: None,
            line: None,
            side: None,
            outdated: false,
            body,
            authenticated_broker_output: false,
        }
    }
    #[test]
    fn normalization_preserves_kind_and_never_follows_external_text() {
        for (source, kind) in [
            (
                EventSource::review(9, ReviewState::ChangesRequested, "bad").unwrap(),
                FeedbackKind::ReviewSubmitted,
            ),
            (
                EventSource::review_comment(9, "2026-01-01T00:00:00Z", "bad").unwrap(),
                FeedbackKind::ReviewComment,
            ),
            (
                EventSource::pr_comment(9, "2026-01-01T00:00:00Z", "bad").unwrap(),
                FeedbackKind::PrComment,
            ),
        ] {
            let event = normalize_feedback(&feedback_input(&source, "bad"))
                .unwrap()
                .unwrap();
            assert_eq!(event.kind, kind);
            assert_eq!(event.trust, FeedbackTrust::ExternalEvidence);
            assert_eq!(event.freshness, FeedbackFreshness::Current);
            assert!(serde_json::to_vec(&event).unwrap().len() <= MAX_FEEDBACK_ENVELOPE_BYTES);
            assert!(normalize_feedback(&feedback_input(&source, "tampered")).is_err());
        }
    }
    #[test]
    fn stale_and_unmapped_events_cannot_be_managed() {
        let mut b = binding();
        b.status = BindingStatus::Active;
        b.authorization = Some(BindingAuthorization::Owner {
            approval_receipt: "owner-receipt".into(),
        });
        let mut ctx = RoutingContext {
            api_origin: "https://api.github.com".into(),
            repository_id: 42,
            pr_number: 5,
            head_repository_id: 42,
            head_sha: "a".repeat(40),
            binding_revision: 3,
            session_incarnation: "incarnation".into(),
            session_live: true,
            dispatch_ambiguous: false,
            binding_verified: true,
        };
        let source = EventSource::pr_comment(11, "2026-01-01T00:00:00Z", "bad").unwrap();
        let mut input = feedback_input(&source, "bad");
        input.association_verified = false;
        let event = normalize_feedback(&input).unwrap().unwrap();
        assert_eq!(
            route_feedback_event(&b, &ctx, &event),
            FeedbackRouteDecision::OperatorPending
        );
        input.association_verified = true;
        input.source_head_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let event = normalize_feedback(&input).unwrap().unwrap();
        assert_eq!(
            route_feedback_event(&b, &ctx, &event),
            FeedbackRouteDecision::Stale
        );
        input.source_head_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let event = normalize_feedback(&input).unwrap().unwrap();
        ctx.binding_verified = false;
        assert_eq!(
            route_feedback_event(&b, &ctx, &event),
            FeedbackRouteDecision::OperatorPending
        );
    }
    #[test]
    fn outdated_comment_and_oversized_wire_envelope_fail_closed() {
        let source = EventSource::review_comment(9, "2026-01-01T00:00:00Z", "bad").unwrap();
        let mut input = feedback_input(&source, "bad");
        input.outdated = true;
        assert_eq!(
            normalize_feedback(&input).unwrap().unwrap().freshness,
            FeedbackFreshness::Stale
        );
        let envelope = FeedbackWireEnvelope {
            message_id: "id".into(),
            event_key: "key".into(),
            routing_revision: 1,
            target_agent_id: "agent".into(),
            target_incarnation: "inc".into(),
            body_json: format!("\"{}\"", "a".repeat(MAX_FEEDBACK_ENVELOPE_BYTES)),
        };
        assert_eq!(envelope.validate(), Err(FeedbackError::EnvelopeTooLarge));
        assert_eq!(
            compare_existing_message(&envelope, &envelope),
            ExistingMessage::Corruption
        );
    }

    #[test]
    fn outbox_idempotency_requires_every_immutable_field() {
        let a = FeedbackWireEnvelope {
            message_id: "id".into(),
            event_key: "event".into(),
            routing_revision: 1,
            target_agent_id: "agent".into(),
            target_incarnation: "inc".into(),
            body_json: "{}".into(),
        };
        let mut b = a.clone();
        assert_eq!(
            compare_existing_message(&a, &b),
            ExistingMessage::ExactMatch
        );
        b.target_incarnation = "other".into();
        assert_eq!(
            compare_existing_message(&a, &b),
            ExistingMessage::Corruption
        );
    }
}
