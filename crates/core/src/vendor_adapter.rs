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
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// Exact session-identity binding pinning an adapter operation to a specific agent generation,
/// host session, and optional OS `(pid, start_time_ticks)` birth tuple.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterIdentityBinding {
    pub agent_id: String,
    pub generation: u64,
    pub host_session: String,
    pub pid_birth: Option<(u32, u64)>,
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
        }
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

/// Startup prompt rule describing how a vendor CLI behaves when given a startup prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorStartupPromptRule {
    pub pattern: String,
    pub description: String,
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
}
