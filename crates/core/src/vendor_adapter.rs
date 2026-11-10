//! Pure dependency-inversion (DI/IoC) contract for vendor host adapters and runtime ports.
//!
//! `fl-core` defines the traits ([`VendorHostAdapter`], [`VendorHostAdapterRegistry`],
//! [`RuntimePort`]) and domain types for the five vendor capability groups:
//!
//! 1. **Identity & Lifecycle** — [`AdapterIdentityBinding`], [`AdapterReadiness`],
//!    [`VendorLaunchRequest`], [`VendorLaunchPlan`], [`AttachOrResumeRequest`],
//!    [`AttachOrResumeOutcome`].
//! 2. **Delivery** — [`TurnDeliveryRequest`] and [`TurnDeliveryOutcome`] (`Accepted`,
//!    `Rejected`, `OutcomeUnknown`), distinguishing pre-write rejection from post-write
//!    transport ambiguity so callers never blindly retry and cause duplicate turns.
//! 3. **Configuration** — [`ConfigEffectiveScope`], [`VendorConfigSpec`], and model/effort
//!    enumeration and validation.
//! 4. **Events & Transcripts** — [`NormalizedSessionTurn`] and [`TranscriptNormalizeReport`]
//!    for versioned vendor transcript normalization with source cursors.
//! 5. **Teardown** — [`TeardownTarget`] and [`TeardownOutcome`] for distinct, idempotent
//!    `stop`, `detach`, `archive`, and `delete` operations scoped strictly to owned session
//!    identities (never cwd-wide).
//!
//! Concrete vendor implementations live in `fl-vendor-adapters` (`crates/vendor-adapters`)
//! and are composed at `fl-cli` (`crates/cli`). `fl-hub`, `fl-wire`, and `fl-host` depend only
//! on these `fl-core` traits and domain types and never on `fl-vendor-adapters`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Exact session-identity binding pinning an adapter operation to a specific agent generation,
/// host session, optional OS `(pid, start_time_ticks)` birth tuple, and optional explicit vendor
/// session ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterIdentityBinding {
    pub agent_id: String,
    pub generation: u64,
    pub host_session: String,
    pub pid_birth: Option<(u32, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_session_id: Option<String>,
}

impl AdapterIdentityBinding {
    pub fn new(
        agent_id: impl Into<String>,
        generation: u64,
        host_session: impl Into<String>,
        pid_birth: Option<(u32, u64)>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            generation,
            host_session: host_session.into(),
            pid_birth,
            vendor_session_id: None,
        }
    }

    pub fn with_vendor_session_id(mut self, vendor_session_id: impl Into<String>) -> Self {
        self.vendor_session_id = Some(vendor_session_id.into());
        self
    }

    pub fn host_session_name(&self) -> &str {
        &self.host_session
    }
}

/// Readiness and turn-state classification of a hosted vendor session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AdapterReadiness {
    #[default]
    Unknown,
    Starting,
    ReadyIdle,
    Busy,
    Exiting,
    Exited,
}

/// Startup prompt rule describing how a vendor CLI behaves when given a startup prompt or
/// interactive first-launch modal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorStartupPromptRule {
    pub pattern: String,
    #[serde(default)]
    pub response_keys: String,
    #[serde(default)]
    pub warn_on_miss: bool,
    pub description: String,
}

impl VendorStartupPromptRule {
    pub fn new(
        pattern: impl Into<String>,
        response_keys: impl Into<String>,
        warn_on_miss: bool,
        description: impl Into<String>,
    ) -> Self {
        Self {
            pattern: pattern.into(),
            response_keys: response_keys.into(),
            warn_on_miss,
            description: description.into(),
        }
    }
}

fn default_remote_control() -> bool {
    true
}

/// Input parameters for preparing a vendor CLI launch plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorLaunchRequest {
    pub binding: AdapterIdentityBinding,
    pub persona: String,
    pub model: Option<String>,
    pub thinking_effort: Option<String>,
    pub initial_prompt: String,
    pub cwd: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
    pub remote_control: bool,
    #[serde(default)]
    pub posture: crate::permissions::Posture,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_approval: Option<crate::permissions::AutoApproval>,
}

/// Prepared command-line and environment plan for launching a vendor CLI in a runtime session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorLaunchPlan {
    pub vendor_id: String,
    pub binary: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub remote_control: bool,
    pub startup_rules: Vec<VendorStartupPromptRule>,
}

/// Request to attach to or resume an existing vendor session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachOrResumeRequest {
    pub binding: AdapterIdentityBinding,
    pub vendor_session_id: String,
    pub control_endpoint: Option<String>,
    pub cwd: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    #[serde(default = "default_remote_control")]
    pub remote_control: bool,
    #[serde(default)]
    pub posture: crate::permissions::Posture,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_approval: Option<crate::permissions::AutoApproval>,
}

impl AttachOrResumeRequest {
    pub fn new(binding: AdapterIdentityBinding, vendor_session_id: impl Into<String>) -> Self {
        Self {
            binding,
            vendor_session_id: vendor_session_id.into(),
            control_endpoint: None,
            cwd: None,
            home_dir: None,
            persona: None,
            model: None,
            thinking_effort: None,
            remote_control: true,
            posture: crate::permissions::Posture::default(),
            auto_approval: None,
        }
    }
}

/// Outcome of attaching to or resuming an existing vendor session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AttachOrResumeOutcome {
    Attached {
        vendor_session_id: String,
        readiness: AdapterReadiness,
    },
    Resumed {
        vendor_session_id: String,
        readiness: AdapterReadiness,
    },
    Rejected {
        reason: String,
    },
}

/// Request to deliver, steer, or cancel a turn on a vendor session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnDeliveryRequest {
    pub binding: AdapterIdentityBinding,
    pub message_id: String,
    pub vendor_session_id: String,
    pub vendor_turn_id: Option<String>,
    pub expected_generation: u64,
    pub text: String,
    pub control_endpoint: Option<String>,
    pub home_dir: Option<PathBuf>,
}

/// Three-valued delivery outcome distinguishing definite acceptance, definite pre-write
/// refusal, and post-write ambiguity (`OutcomeUnknown`).
///
/// When `OutcomeUnknown` is returned, bytes may already have reached the vendor server or TUI;
/// callers MUST NOT automatically retry or re-enqueue the same turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TurnDeliveryOutcome {
    Accepted { turn_id: Option<String> },
    Rejected { reason: String },
    OutcomeUnknown { stage: String, message: String },
}

impl TurnDeliveryOutcome {
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }

    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected { .. })
    }

    pub fn is_outcome_unknown(&self) -> bool {
        matches!(self, Self::OutcomeUnknown { .. })
    }
}

/// When a configuration change (such as model or thinking-effort level) takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigEffectiveScope {
    Immediate,
    NextTurn,
    NextLaunch,
    RestartRequired,
    Unsupported,
}

/// Validated configuration specification for a vendor adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorConfigSpec {
    pub normalized_model: Option<String>,
    pub normalized_thinking_effort: Option<String>,
    pub model_scope: ConfigEffectiveScope,
    pub thinking_effort_scope: ConfigEffectiveScope,
}

/// One normalized session turn extracted from a vendor transcript or event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedSessionTurn {
    pub turn_id: String,
    pub role: String,
    pub content: String,
    pub source_cursor: String,
}

/// Result of normalizing a batch of raw vendor transcript records or events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TranscriptNormalizeReport {
    pub schema_version: u32,
    pub turns: Vec<NormalizedSessionTurn>,
    pub next_cursor: Option<String>,
}

/// Post-compaction transcript inspection summary for `--resume` eligibility and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PostCompactionContext {
    pub turn_count: usize,
    pub latest_model: Option<String>,
    pub active_context_tokens: Option<u64>,
    pub post_compact_conversational_bytes: u64,
    pub bridge_session_id: Option<String>,
}

impl PostCompactionContext {
    /// Returns `true` when the transcript contains a persisted Remote Control `bridgeSessionId`.
    pub fn has_persisted_bridge_session(&self) -> bool {
        self.bridge_session_id.is_some()
    }
}

/// Target descriptor for teardown operations (`stop`, `detach`, `archive`, `delete`).
///
/// Every teardown operation is scoped strictly to the owned `binding` and `vendor_session_id`
/// (never cwd-wide).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeardownTarget {
    pub binding: AdapterIdentityBinding,
    pub vendor_session_id: Option<String>,
    pub control_endpoint: Option<String>,
    pub home_dir: Option<PathBuf>,
}

/// Idempotent outcome of a teardown operation (`stop`, `detach`, `archive`, `delete`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TeardownOutcome {
    Completed,
    AlreadyAbsent,
    Unsupported { reason: String },
}

/// Pure errors returned by [`VendorHostAdapter`] and [`RuntimePort`] operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VendorAdapterError {
    #[error("permission policy refused for vendor `{vendor}`: {reason}")]
    PermissionRefused { vendor: String, reason: String },
    #[error("invalid configuration for vendor `{vendor}`: {reason}")]
    InvalidConfig { vendor: String, reason: String },
    #[error("generation mismatch: expected {expected}, actual {actual}")]
    GenerationMismatch { expected: u64, actual: u64 },
    #[error("missing session identity for vendor `{vendor}`: {reason}")]
    MissingSessionIdentity { vendor: String, reason: String },
    #[error("transport error on vendor `{vendor}`: {reason}")]
    Transport { vendor: String, reason: String },
    #[error("protocol error on vendor `{vendor}`: {reason}")]
    Protocol { vendor: String, reason: String },
    #[error("operation `{operation}` is unsupported by vendor `{vendor}`: {reason}")]
    Unsupported {
        vendor: String,
        operation: String,
        reason: String,
    },
}

/// Dependency-inverted port over `fl-host` session lifecycle and terminal I/O.
///
/// Allows `fl-hub`, `fl-wire`, and vendor adapters to query session liveness, capture output,
/// send keys, or stop a host session without coupling domain logic to concrete transport sockets.
pub trait RuntimePort: fmt::Debug + Send + Sync {
    /// Check the current readiness of a bound host session.
    fn session_readiness(
        &self,
        binding: &AdapterIdentityBinding,
    ) -> Result<AdapterReadiness, VendorAdapterError>;

    /// Send raw bytes to a bound host session.
    fn write_bytes(
        &self,
        binding: &AdapterIdentityBinding,
        bytes: &[u8],
    ) -> Result<(), VendorAdapterError>;

    /// Capture recent UTF-8 terminal output from a bound host session.
    fn capture_output(
        &self,
        binding: &AdapterIdentityBinding,
        max_bytes: usize,
    ) -> Result<String, VendorAdapterError>;

    /// Stop the bound host session idempotently.
    fn stop_session(
        &self,
        binding: &AdapterIdentityBinding,
    ) -> Result<TeardownOutcome, VendorAdapterError>;
}

use std::fmt;

/// Dependency-inverted contract implemented by each concrete vendor host adapter
/// (`CodexHostAdapter`, `AntigravityHostAdapter`, `ClaudeHostAdapter`, `MuseGrokPtyAdapter`, etc.).
pub trait VendorHostAdapter: fmt::Debug + Send + Sync {
    /// Canonical lowercase vendor identifier (for example `"codex"`, `"antigravity"`, `"claude"`).
    fn vendor_id(&self) -> &str;

    /// Default executable name for this vendor CLI.
    fn default_binary(&self) -> &str;

    /// Optional environment variable overriding the binary path (for example `"FL_WIRE_CODEX_BIN"`).
    fn binary_env_var(&self) -> Option<&str> {
        None
    }

    // --- 1. Identity & Lifecycle -------------------------------------------

    /// Build a deterministic launch plan (`argv`, `env`, `cwd`, `remote_control`) for starting a
    /// new hosted session of this vendor.
    fn prepare_launch(
        &self,
        request: &VendorLaunchRequest,
    ) -> Result<VendorLaunchPlan, VendorAdapterError>;

    /// Build a deterministic resume launch plan (`argv`, `env`, `cwd`, `remote_control`) for
    /// resuming an existing session identified by `request.vendor_session_id`.
    fn prepare_resume(
        &self,
        _request: &AttachOrResumeRequest,
    ) -> Result<VendorLaunchPlan, VendorAdapterError> {
        Err(VendorAdapterError::Unsupported {
            vendor: self.vendor_id().to_string(),
            operation: "prepare_resume".to_string(),
            reason: format!(
                "vendor `{}` does not support CLI resume launch plans",
                self.vendor_id()
            ),
        })
    }

    /// Attach to or resume an existing vendor session scoped strictly to `request.binding` and
    /// `request.vendor_session_id`.
    fn attach_or_resume(
        &self,
        request: &AttachOrResumeRequest,
    ) -> Result<AttachOrResumeOutcome, VendorAdapterError>;

    /// Probe readiness and turn state (`Unknown`, `Starting`, `ReadyIdle`, `Busy`, `Exiting`, `Exited`)
    /// for a bound session.
    fn probe_readiness(
        &self,
        binding: &AdapterIdentityBinding,
        vendor_session_id: Option<&str>,
        control_endpoint: Option<&str>,
    ) -> Result<AdapterReadiness, VendorAdapterError>;

    // --- 2. Delivery -------------------------------------------------------

    /// Submit a new turn (or vendor-native slash command such as `/compact`) to an idle session.
    fn submit_turn(&self, request: &TurnDeliveryRequest) -> TurnDeliveryOutcome;

    /// Steer an in-progress turn on the vendor session using the vendor's concurrency precondition
    /// (for example `expectedTurnId` on Codex).
    fn steer_turn(&self, request: &TurnDeliveryRequest) -> TurnDeliveryOutcome;

    /// Cancel/interrupt an in-progress turn on the vendor session.
    fn cancel_turn(&self, request: &TurnDeliveryRequest) -> TurnDeliveryOutcome;

    /// Alias for [`Self::cancel_turn`] matching vendor interrupt terminology.
    fn interrupt_turn(&self, request: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
        self.cancel_turn(request)
    }

    // --- 3. Configuration --------------------------------------------------

    /// Supported thinking/reasoning effort tokens for this vendor (empty if unsupported).
    fn supported_thinking_efforts(&self) -> &[&str] {
        &[]
    }

    /// Validate and normalize requested model and thinking-effort settings, returning their
    /// effective scopes (`Immediate`, `NextTurn`, `RestartRequired`, `Unsupported`).
    fn validate_config(
        &self,
        model: Option<&str>,
        thinking_effort: Option<&str>,
    ) -> Result<VendorConfigSpec, VendorAdapterError>;

    // --- 4. Events & Transcripts -------------------------------------------

    /// Normalize raw vendor transcript JSONL lines or event payloads into versioned
    /// [`NormalizedSessionTurn`] records with source cursors.
    fn normalize_transcript(
        &self,
        raw_payload: &str,
        after_cursor: Option<&str>,
    ) -> Result<TranscriptNormalizeReport, VendorAdapterError>;

    /// Locate the active session transcript file for `vendor_session_id` under `home_dir`
    /// (excluding archived transcripts; never scanning by newest `mtime`).
    fn find_active_session_transcript_path(
        &self,
        _home_dir: &Path,
        _vendor_session_id: &str,
    ) -> Result<Option<PathBuf>, VendorAdapterError> {
        Err(VendorAdapterError::Unsupported {
            vendor: self.vendor_id().to_string(),
            operation: "find_active_session_transcript_path".to_string(),
            reason: format!(
                "vendor `{}` does not support active session transcript lookup",
                self.vendor_id()
            ),
        })
    }

    /// Inspect the final non-empty line of a session transcript file and truncate any torn
    /// trailing write back to the last complete valid record (`0` when already valid or empty).
    fn repair_session_transcript_tail(
        &self,
        _transcript_path: &Path,
    ) -> Result<usize, VendorAdapterError> {
        Err(VendorAdapterError::Unsupported {
            vendor: self.vendor_id().to_string(),
            operation: "repair_session_transcript_tail".to_string(),
            reason: format!(
                "vendor `{}` does not support session transcript tail repair",
                self.vendor_id()
            ),
        })
    }

    /// Estimate post-compaction active context tokens, conversational byte volume, latest
    /// model ID, normalized turn count, and persisted bridge session identity from a transcript.
    fn estimate_post_compaction_context(
        &self,
        _transcript_path: &Path,
    ) -> Result<PostCompactionContext, VendorAdapterError> {
        Err(VendorAdapterError::Unsupported {
            vendor: self.vendor_id().to_string(),
            operation: "estimate_post_compaction_context".to_string(),
            reason: format!(
                "vendor `{}` does not support post-compaction context estimation",
                self.vendor_id()
            ),
        })
    }

    // --- 5. Teardown -------------------------------------------------------

    /// Stop the active runtime process for `target` without deleting vendor state.
    fn stop(&self, target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError>;

    /// Detach FerroLoop from `target` while leaving the underlying session intact.
    fn detach(&self, target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError>;

    /// Archive the vendor session identified by `target.vendor_session_id` using the vendor's
    /// native control protocol (for example `thread/archive` on Codex).
    fn archive(&self, target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError>;

    /// Permanently delete the vendor session trajectory identified by `target.vendor_session_id`
    /// (for example `DeleteCascadeTrajectory` on Antigravity).
    fn delete(&self, target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError>;
}

/// Thread-safe dependency-injection registry mapping canonical vendor IDs to
/// [`VendorHostAdapter`] implementations composed at the application boundary (`fl-cli`).
#[derive(Debug, Default)]
pub struct VendorHostAdapterRegistry {
    adapters: RwLock<BTreeMap<String, Arc<dyn VendorHostAdapter>>>,
}

impl VendorHostAdapterRegistry {
    pub fn new() -> Self {
        Self {
            adapters: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn register(&self, adapter: Arc<dyn VendorHostAdapter>) {
        let key = adapter.vendor_id().trim().to_ascii_lowercase();
        self.adapters
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, adapter);
    }

    pub fn lookup(&self, vendor_id: &str) -> Option<Arc<dyn VendorHostAdapter>> {
        let key = vendor_id.trim().to_ascii_lowercase();
        self.adapters
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    }

    pub fn list(&self) -> Vec<Arc<dyn VendorHostAdapter>> {
        self.adapters
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub fn vendor_ids(&self) -> Vec<String> {
        self.adapters
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Shared Claude JSONL transcript & session-ID helpers
// ---------------------------------------------------------------------------

/// Validate that a Claude session ID is non-empty and safe to use as a CLI argument and filename
/// stem (no path separators, traversal, whitespace, control characters, or leading dash).
pub fn is_valid_claude_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Validate a required `vendor_session_id` for a Claude adapter operation.
pub fn validate_claude_session_id<'a>(
    raw: &'a str,
    operation: &str,
) -> Result<&'a str, VendorAdapterError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(VendorAdapterError::MissingSessionIdentity {
            vendor: "claude".into(),
            reason: format!(
                "Claude `{operation}` requires an explicit vendor_session_id; \
                 --continue and newest-by-mtime fallback are forbidden"
            ),
        });
    }
    if !is_valid_claude_session_id(trimmed) {
        return Err(VendorAdapterError::InvalidConfig {
            vendor: "claude".into(),
            reason: format!("invalid Claude vendor_session_id `{trimmed}` for `{operation}`"),
        });
    }
    Ok(trimmed)
}

/// Locate `<vendor_session_id>.jsonl` under `<home_dir>/.claude/projects/*/` (and optionally
/// `<home_dir>/.claude/archived/*/`), sorted deterministically by project directory name.
pub fn find_claude_session_transcript_path(
    home_dir: &Path,
    vendor_session_id: &str,
    include_archived: bool,
) -> Result<Option<PathBuf>, VendorAdapterError> {
    let op = if include_archived {
        "find_transcript"
    } else {
        "find_active_transcript"
    };
    let session_id = validate_claude_session_id(vendor_session_id, op)?;
    let target_file = format!("{session_id}.jsonl");
    let claude_dir = home_dir.join(".claude");
    let buckets: &[&str] = if include_archived {
        &["projects", "archived"]
    } else {
        &["projects"]
    };
    for bucket in buckets {
        let root = claude_dir.join(bucket);
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        let mut subdirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        subdirs.sort();
        for proj_dir in subdirs {
            let candidate = proj_dir.join(&target_file);
            if candidate.is_file() {
                return Ok(Some(candidate));
            }
        }
    }
    Ok(None)
}

/// Locate the active `<vendor_session_id>.jsonl` strictly under `<home_dir>/.claude/projects/*/`.
pub fn find_claude_active_session_transcript_path(
    home_dir: &Path,
    vendor_session_id: &str,
) -> Result<Option<PathBuf>, VendorAdapterError> {
    find_claude_session_transcript_path(home_dir, vendor_session_id, false)
}

/// Inspect only the final non-empty line of a Claude `.jsonl` transcript and truncate any torn
/// trailing non-JSON write back to the last complete valid JSON line.
pub fn repair_claude_session_transcript_tail(
    transcript_path: &Path,
) -> Result<usize, VendorAdapterError> {
    let raw = fs::read(transcript_path).map_err(|e| VendorAdapterError::Transport {
        vendor: "claude".into(),
        reason: format!(
            "failed to read Claude transcript {}: {e}",
            transcript_path.display()
        ),
    })?;
    let original_len = raw.len();
    if original_len == 0 {
        return Ok(0);
    }

    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0;
    while pos < original_len {
        let next_end = match raw[pos..].iter().position(|&b| b == b'\n') {
            Some(rel) => pos + rel + 1,
            None => original_len,
        };
        spans.push((pos, next_end));
        pos = next_end;
    }

    let is_valid_json_span = |start: usize, end: usize| -> Option<bool> {
        let trimmed = raw[start..end].trim_ascii();
        if trimmed.is_empty() {
            return None;
        }
        let valid = std::str::from_utf8(trimmed)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .is_some();
        Some(valid)
    };

    let Some((last_non_empty_idx, last_is_valid)) = spans
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, &(s, e))| is_valid_json_span(s, e).map(|v| (idx, v)))
    else {
        return Ok(0);
    };

    if last_is_valid {
        return Ok(0);
    }

    let truncate_to = spans[..last_non_empty_idx]
        .iter()
        .rev()
        .find_map(|&(s, e)| (is_valid_json_span(s, e) == Some(true)).then_some(e))
        .unwrap_or(0);

    let file = fs::OpenOptions::new()
        .write(true)
        .open(transcript_path)
        .map_err(|e| VendorAdapterError::Transport {
            vendor: "claude".into(),
            reason: format!(
                "failed to open Claude transcript {} for tail repair: {e}",
                transcript_path.display()
            ),
        })?;
    file.set_len(truncate_to as u64)
        .map_err(|e| VendorAdapterError::Transport {
            vendor: "claude".into(),
            reason: format!(
                "failed to truncate torn Claude transcript {}: {e}",
                transcript_path.display()
            ),
        })?;

    Ok(original_len - truncate_to)
}

/// Extract displayable text / tool summary content from a Claude JSONL record.
pub fn extract_claude_message_content(val: &Value) -> String {
    let Some(content) = val
        .get("content")
        .or_else(|| val.pointer("/message/content"))
    else {
        if let Some(s) = val
            .get("result")
            .or_else(|| val.get("text"))
            .and_then(Value::as_str)
        {
            return s.trim().to_string();
        }
        return String::new();
    };
    match content {
        Value::String(s) => s.trim().to_string(),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
                match btype {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(Value::as_str)
                            && !t.trim().is_empty()
                        {
                            parts.push(t.trim().to_string());
                        }
                    }
                    "tool_result" => {
                        if let Some(c) = block.get("content") {
                            match c {
                                Value::String(s) if !s.trim().is_empty() => {
                                    parts.push(s.trim().to_string());
                                }
                                Value::Array(arr) => {
                                    for item in arr {
                                        if let Some(t) = item.get("text").and_then(Value::as_str)
                                            && !t.trim().is_empty()
                                        {
                                            parts.push(t.trim().to_string());
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "tool_use" => {
                        if let Some(name) = block.get("name").and_then(Value::as_str)
                            && !name.trim().is_empty()
                        {
                            parts.push(format!("[tool_use:{name}]"));
                        }
                    }
                    _ => {}
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// Returns `true` for non-conversational Claude JSONL telemetry/snapshot record types.
pub fn is_claude_skipped_record_type(record_type: &str) -> bool {
    record_type.starts_with("api-request")
        || record_type.starts_with("file-history-")
        || matches!(
            record_type,
            "attachment"
                | "queue-operation"
                | "last-prompt"
                | "file-history-snapshot"
                | "mode"
                | "permission-mode"
                | "atis-latch"
                | "bridge-session"
                | "cost-state"
                | "ai-title"
                | "tools-used"
                | "summary"
        )
}

/// Classify a Claude JSONL record into a normalized conversational role (`"user"`, `"assistant"`,
/// `"tool"`, `"system"`), excluding sidechains and skipped telemetry record types.
pub fn classify_claude_turn_role(val: &Value) -> Option<String> {
    let obj = val.as_object()?;
    if obj.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if let Some(t) = obj.get("type").and_then(Value::as_str)
        && is_claude_skipped_record_type(t)
    {
        return None;
    }
    let raw_role = obj
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| val.pointer("/message/role").and_then(Value::as_str))
        .or_else(|| obj.get("type").and_then(Value::as_str))?;

    let mut role = match raw_role.trim().to_ascii_lowercase().as_str() {
        "user" | "human" => "user".to_string(),
        "assistant" | "model" | "result" | "reasoning" => "assistant".to_string(),
        "tool" | "tool_result" => "tool".to_string(),
        "system" => "system".to_string(),
        _ => return None,
    };

    if role == "user" {
        if obj.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
            || obj.get("synthetic_reason").is_some()
        {
            role = "system".to_string();
        } else if let Some(arr) = obj
            .get("content")
            .or_else(|| val.pointer("/message/content"))
            .and_then(Value::as_array)
            && !arr.is_empty()
            && arr
                .iter()
                .all(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            role = "tool".to_string();
        }
    }

    Some(role)
}

/// Estimate post-compaction active context tokens, conversational byte volume, latest real model
/// ID, normalized turn count, and persisted Remote Control `bridgeSessionId` from a Claude
/// `.jsonl` transcript file.
pub fn estimate_claude_post_compaction_context(
    transcript_path: &Path,
) -> Result<PostCompactionContext, VendorAdapterError> {
    let raw = fs::read_to_string(transcript_path).map_err(|e| VendorAdapterError::Transport {
        vendor: "claude".into(),
        reason: format!(
            "failed to read Claude transcript {}: {e}",
            transcript_path.display()
        ),
    })?;

    let mut turn_count: usize = 0;
    let mut latest_model: Option<String> = None;
    let mut bridge_session_id: Option<String> = None;
    let mut last_compact_post_tokens: Option<u64> = None;
    let mut post_compact_assistant_tokens: Option<u64> = None;
    let mut post_compact_conversational_bytes: u64 = 0;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        if classify_claude_turn_role(&val).is_some()
            && !extract_claude_message_content(&val).is_empty()
        {
            turn_count += 1;
        }

        if val.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }

        let record_type = val.get("type").and_then(Value::as_str).unwrap_or("");

        if record_type == "bridge-session" {
            if let Some(bsid) = val
                .get("bridgeSessionId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                bridge_session_id = Some(bsid.to_string());
            }
            continue;
        }

        if record_type == "system"
            && val.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            last_compact_post_tokens = val
                .pointer("/compactMetadata/postTokens")
                .and_then(Value::as_u64)
                .filter(|&n| n > 0);
            post_compact_assistant_tokens = None;
            post_compact_conversational_bytes = 0;
            continue;
        }

        if matches!(record_type, "user" | "assistant")
            && !is_claude_skipped_record_type(record_type)
        {
            post_compact_conversational_bytes =
                post_compact_conversational_bytes.saturating_add(line.len() as u64);
        }

        if record_type == "assistant" {
            let model_opt = val
                .pointer("/message/model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if model_opt != Some("<synthetic>") {
                if let Some(m) = model_opt {
                    latest_model = Some(m.to_string());
                }
                if let Some(usage) = val.pointer("/message/usage") {
                    let input = usage
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cache_creation = usage
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cache_read = usage
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let total = input
                        .saturating_add(cache_creation)
                        .saturating_add(cache_read);
                    if total > 0 {
                        post_compact_assistant_tokens = Some(total);
                    }
                }
            }
        }
    }

    Ok(PostCompactionContext {
        turn_count,
        latest_model,
        active_context_tokens: post_compact_assistant_tokens.or(last_compact_post_tokens),
        post_compact_conversational_bytes,
        bridge_session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct DummyAdapter;

    impl VendorHostAdapter for DummyAdapter {
        fn vendor_id(&self) -> &str {
            "dummy"
        }

        fn default_binary(&self) -> &str {
            "dummy-cli"
        }

        fn prepare_launch(
            &self,
            request: &VendorLaunchRequest,
        ) -> Result<VendorLaunchPlan, VendorAdapterError> {
            Ok(VendorLaunchPlan {
                vendor_id: "dummy".into(),
                binary: "dummy-cli".into(),
                argv: vec!["dummy-cli".into(), request.initial_prompt.clone()],
                env: Vec::new(),
                cwd: request.cwd.clone(),
                remote_control: request.remote_control,
                startup_rules: Vec::new(),
            })
        }

        fn attach_or_resume(
            &self,
            request: &AttachOrResumeRequest,
        ) -> Result<AttachOrResumeOutcome, VendorAdapterError> {
            Ok(AttachOrResumeOutcome::Attached {
                vendor_session_id: request.vendor_session_id.clone(),
                readiness: AdapterReadiness::ReadyIdle,
            })
        }

        fn probe_readiness(
            &self,
            _binding: &AdapterIdentityBinding,
            _vendor_session_id: Option<&str>,
            _control_endpoint: Option<&str>,
        ) -> Result<AdapterReadiness, VendorAdapterError> {
            Ok(AdapterReadiness::ReadyIdle)
        }

        fn submit_turn(&self, request: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
            if request.binding.generation != request.expected_generation {
                return TurnDeliveryOutcome::Rejected {
                    reason: "stale generation".into(),
                };
            }
            TurnDeliveryOutcome::Accepted {
                turn_id: Some("turn-1".into()),
            }
        }

        fn steer_turn(&self, _request: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
            TurnDeliveryOutcome::Rejected {
                reason: "steering unsupported".into(),
            }
        }

        fn cancel_turn(&self, _request: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
            TurnDeliveryOutcome::Accepted { turn_id: None }
        }

        fn validate_config(
            &self,
            model: Option<&str>,
            thinking_effort: Option<&str>,
        ) -> Result<VendorConfigSpec, VendorAdapterError> {
            Ok(VendorConfigSpec {
                normalized_model: model.map(str::to_string),
                normalized_thinking_effort: thinking_effort.map(str::to_string),
                model_scope: ConfigEffectiveScope::RestartRequired,
                thinking_effort_scope: ConfigEffectiveScope::Unsupported,
            })
        }

        fn normalize_transcript(
            &self,
            _raw_payload: &str,
            _after_cursor: Option<&str>,
        ) -> Result<TranscriptNormalizeReport, VendorAdapterError> {
            Ok(TranscriptNormalizeReport {
                schema_version: 1,
                turns: Vec::new(),
                next_cursor: None,
            })
        }

        fn stop(&self, _target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
            Ok(TeardownOutcome::Completed)
        }

        fn detach(&self, _target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
            Ok(TeardownOutcome::Completed)
        }

        fn archive(&self, _target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
            Ok(TeardownOutcome::Unsupported {
                reason: "archive not supported".into(),
            })
        }

        fn delete(&self, _target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
            Ok(TeardownOutcome::AlreadyAbsent)
        }
    }

    #[test]
    fn registry_registers_and_resolves_adapter_case_insensitively() {
        let registry = VendorHostAdapterRegistry::new();
        registry.register(Arc::new(DummyAdapter));
        assert_eq!(registry.vendor_ids(), vec!["dummy".to_string()]);
        let adapter = registry.lookup("DUMMY").expect("lookup dummy adapter");
        assert_eq!(adapter.vendor_id(), "dummy");
        assert_eq!(adapter.default_binary(), "dummy-cli");
    }

    #[test]
    fn turn_delivery_outcome_classifiers_distinguish_three_states() {
        let accepted = TurnDeliveryOutcome::Accepted {
            turn_id: Some("t-1".into()),
        };
        assert!(accepted.is_accepted());
        assert!(!accepted.is_rejected());
        assert!(!accepted.is_outcome_unknown());

        let rejected = TurnDeliveryOutcome::Rejected {
            reason: "idle".into(),
        };
        assert!(rejected.is_rejected());

        let unknown = TurnDeliveryOutcome::OutcomeUnknown {
            stage: "turn/start".into(),
            message: "socket EOF after write".into(),
        };
        assert!(unknown.is_outcome_unknown());
    }

    #[test]
    fn default_transcript_preflight_methods_return_unsupported() {
        let adapter: Arc<dyn VendorHostAdapter> = Arc::new(DummyAdapter);
        let p = Path::new("/tmp/nonexistent");
        assert!(matches!(
            adapter.find_active_session_transcript_path(p, "sid"),
            Err(VendorAdapterError::Unsupported { .. })
        ));
        assert!(matches!(
            adapter.repair_session_transcript_tail(p),
            Err(VendorAdapterError::Unsupported { .. })
        ));
        assert!(matches!(
            adapter.estimate_post_compaction_context(p),
            Err(VendorAdapterError::Unsupported { .. })
        ));
    }
}
