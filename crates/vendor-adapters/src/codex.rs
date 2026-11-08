//! Slice 4a: Concrete [`VendorHostAdapter`] implementation for OpenAI Codex (`codex`).
//!
//! Consolidates Codex host launch preparation (`--yolo --remote unix://`, plain model IDs,
//! `model_reasoning_effort` config overrides, omitting `AGENT_PERSONA` for the shared
//! `codex app-server`), single `codex app-server` Unix WebSocket (RFC 6455) JSON-RPC control
//! socket writer (`submit_turn`, `steer_turn`, `cancel_turn`), and native `thread/archive`
//! teardown scoped strictly to the owned `thread_id`.
//!
//! Zero `codex exec resume` fallback, zero raw `state_5.sqlite` `UPDATE` statements, and zero
//! lock-file deletions occur in this adapter.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fl_core::vendor_adapter::{
    AdapterIdentityBinding, AdapterReadiness, AttachOrResumeOutcome, AttachOrResumeRequest,
    ConfigEffectiveScope, NormalizedSessionTurn, TeardownOutcome, TeardownTarget,
    TranscriptNormalizeReport, TurnDeliveryOutcome, TurnDeliveryRequest, VendorAdapterError,
    VendorConfigSpec, VendorHostAdapter, VendorLaunchPlan, VendorLaunchRequest,
    VendorStartupPromptRule,
};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;
use serde_json::{Value, json};

/// Default overall timeout for a Codex app-server control socket RPC sequence.
pub const CODEX_RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a single socket read may block.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest WebSocket frame or reassembled text message accepted from the server.
const MAX_MESSAGE: usize = 1024 * 1024;
/// Raw turn input text and thread ID must each fit within this bound before any copying.
const MAX_TURN_INPUT: usize = 1024 * 1024;
/// Largest encoded JSON-RPC request payload (WebSocket framing adds at most 14 bytes).
const MAX_RPC_REQUEST: usize = 1024 * 1024;
/// Longest RPC error text carried into a rejection or error string.
const MAX_ERROR_TEXT: usize = 300;
/// Control socket relative to `CODEX_HOME` or `~/.codex`.
pub const CODEX_CONTROL_SOCKET_REL: &str = "app-server-control/app-server-control.sock";

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Supported reasoning effort tokens for Codex (`-c model_reasoning_effort="<level>"`).
pub const CODEX_SUPPORTED_EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];

/// Concrete [`VendorHostAdapter`] for OpenAI Codex (`codex`).
#[derive(Debug, Clone)]
pub struct CodexHostAdapter {
    rpc_timeout: Duration,
}

impl Default for CodexHostAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexHostAdapter {
    pub fn new() -> Self {
        Self {
            rpc_timeout: CODEX_RPC_TIMEOUT,
        }
    }

    pub fn with_rpc_timeout(rpc_timeout: Duration) -> Self {
        Self { rpc_timeout }
    }

    /// Resolve the `codex` binary path using `FL_WIRE_CODEX_BIN`, `<home>/.local/bin/codex`,
    /// or `"codex"`.
    pub fn resolve_binary(&self, home_dir: Option<&Path>) -> String {
        if let Some(var) = self.binary_env_var()
            && let Ok(val) = std::env::var(var)
        {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        let default_bin = self.default_binary();
        let resolved_home = home_dir
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        if let Some(h) = resolved_home {
            let candidate = h.join(".local").join("bin").join(default_bin);
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
        default_bin.to_string()
    }

    /// Resolve the Codex `app-server` Unix control socket path from an explicit endpoint,
    /// `FL_WIRE_CODEX_CONTROL_SOCK`, `CODEX_HOME`, or `home_dir` / `HOME`.
    pub fn resolve_control_socket_path(
        &self,
        explicit_endpoint: Option<&str>,
        home_dir: Option<&Path>,
    ) -> Result<PathBuf, String> {
        if let Some(ep) = explicit_endpoint.map(str::trim).filter(|s| !s.is_empty()) {
            let stripped = ep.strip_prefix("unix://").unwrap_or(ep);
            return Ok(PathBuf::from(stripped));
        }
        let env_sock = std::env::var_os("FL_WIRE_CODEX_CONTROL_SOCK");
        let env_codex_home = std::env::var_os("CODEX_HOME");
        let env_home = std::env::var_os("HOME");
        resolve_codex_control_socket(
            env_sock.as_deref(),
            env_codex_home.as_deref(),
            home_dir.map(Path::as_os_str).or(env_home.as_deref()),
        )
    }
}

/// Resolve the Codex app-server control socket path from environment/home overrides.
pub fn resolve_codex_control_socket(
    sock_override: Option<&OsStr>,
    codex_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, String> {
    fn non_empty(v: Option<&OsStr>) -> Option<&OsStr> {
        v.filter(|s| !s.is_empty())
    }
    if let Some(sock) = non_empty(sock_override) {
        return Ok(PathBuf::from(sock));
    }
    if let Some(dir) = non_empty(codex_home) {
        let dir = Path::new(dir);
        if !dir.is_absolute() {
            return Err(format!(
                "CODEX_HOME must be an absolute path, got `{}`",
                dir.display()
            ));
        }
        return Ok(dir.join(CODEX_CONTROL_SOCKET_REL));
    }
    if let Some(dir) = non_empty(home) {
        return Ok(PathBuf::from(dir)
            .join(".codex")
            .join(CODEX_CONTROL_SOCKET_REL));
    }
    Err("cannot locate the Codex app-server control socket: \
         set FL_WIRE_CODEX_CONTROL_SOCK, CODEX_HOME or HOME"
        .to_string())
}

fn is_plain_model_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn is_plain_thinking_effort(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn build_codex_launch_env(home_dir: Option<&Path>) -> Vec<(String, String)> {
    // Codex shares a single `codex app-server` across all personas, so `AGENT_PERSONA`
    // MUST NOT be injected into its environment (or the first persona to start the daemon
    // would poison `AGENT_PERSONA` for every subsequent Codex session).
    let mut env = Vec::new();
    let resolved_home = home_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    if let Some(h) = resolved_home {
        let local_bin = h.join(".local").join("bin");
        let path_val = match std::env::var("PATH") {
            Ok(curr) if !curr.is_empty() => format!("{}:{curr}", local_bin.display()),
            _ => format!("{}:/usr/local/bin:/usr/bin:/bin", local_bin.display()),
        };
        env.push(("PATH".to_string(), path_val));
    }
    env
}

impl VendorHostAdapter for CodexHostAdapter {
    fn vendor_id(&self) -> &str {
        "codex"
    }

    fn default_binary(&self) -> &str {
        "codex"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_CODEX_BIN")
    }

    fn prepare_launch(
        &self,
        request: &VendorLaunchRequest,
    ) -> Result<VendorLaunchPlan, VendorAdapterError> {
        let cfg =
            self.validate_config(request.model.as_deref(), request.thinking_effort.as_deref())?;
        let binary = self.resolve_binary(request.home_dir.as_deref());
        let mut argv = vec![binary.clone(), "--yolo".to_string()];
        if request.remote_control {
            argv.extend(["--remote".to_string(), "unix://".to_string()]);
        }
        if let Some(model) = cfg.normalized_model {
            argv.extend(["--model".to_string(), model]);
        }
        if let Some(level) = cfg.normalized_thinking_effort {
            argv.extend([
                "-c".to_string(),
                format!("model_reasoning_effort=\"{level}\""),
            ]);
        }
        argv.push(request.initial_prompt.clone());

        Ok(VendorLaunchPlan {
            vendor_id: self.vendor_id().to_string(),
            binary,
            argv,
            env: build_codex_launch_env(request.home_dir.as_deref()),
            cwd: request.cwd.clone(),
            remote_control: request.remote_control,
            startup_rules: vec![VendorStartupPromptRule {
                pattern: "positional_prompt".to_string(),
                description: "Codex accepts the initial prompt as its trailing positional argument"
                    .to_string(),
            }],
        })
    }

    fn attach_or_resume(
        &self,
        request: &AttachOrResumeRequest,
    ) -> Result<AttachOrResumeOutcome, VendorAdapterError> {
        let thread_id = request.vendor_session_id.trim();
        if thread_id.is_empty() {
            return Err(VendorAdapterError::MissingSessionIdentity {
                vendor: "codex".into(),
                reason: "thread_id must not be empty".into(),
            });
        }
        let sock = self
            .resolve_control_socket_path(
                request.control_endpoint.as_deref(),
                request.home_dir.as_deref(),
            )
            .map_err(|reason| VendorAdapterError::Transport {
                vendor: "codex".into(),
                reason,
            })?;
        match probe_codex_thread_readiness(&sock, thread_id, self.rpc_timeout) {
            Ok(readiness) => Ok(AttachOrResumeOutcome::Attached {
                vendor_session_id: thread_id.to_string(),
                readiness,
            }),
            Err(reason) => Ok(AttachOrResumeOutcome::Rejected { reason }),
        }
    }

    fn probe_readiness(
        &self,
        _binding: &AdapterIdentityBinding,
        vendor_session_id: Option<&str>,
        control_endpoint: Option<&str>,
    ) -> Result<AdapterReadiness, VendorAdapterError> {
        let Some(thread_id) = vendor_session_id.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(AdapterReadiness::Unknown);
        };
        let sock = self
            .resolve_control_socket_path(control_endpoint, None)
            .map_err(|reason| VendorAdapterError::Transport {
                vendor: "codex".into(),
                reason,
            })?;
        probe_codex_thread_readiness(&sock, thread_id, self.rpc_timeout).map_err(|reason| {
            VendorAdapterError::Transport {
                vendor: "codex".into(),
                reason,
            }
        })
    }

    fn submit_turn(&self, request: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
        if request.binding.generation != request.expected_generation {
            return TurnDeliveryOutcome::Rejected {
                reason: format!(
                    "generation mismatch: binding={} expected={}",
                    request.binding.generation, request.expected_generation
                ),
            };
        }
        let thread_id = request.vendor_session_id.trim();
        if thread_id.is_empty() {
            return TurnDeliveryOutcome::Rejected {
                reason: "Codex submit_turn requires a non-empty thread_id".into(),
            };
        }
        let sock = match self.resolve_control_socket_path(
            request.control_endpoint.as_deref(),
            request.home_dir.as_deref(),
        ) {
            Ok(sock) => sock,
            Err(reason) => return TurnDeliveryOutcome::Rejected { reason },
        };
        submit_codex_turn(&sock, thread_id, &request.text, self.rpc_timeout)
    }

    fn steer_turn(&self, request: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
        if request.binding.generation != request.expected_generation {
            return TurnDeliveryOutcome::Rejected {
                reason: format!(
                    "generation mismatch: binding={} expected={}",
                    request.binding.generation, request.expected_generation
                ),
            };
        }
        let thread_id = request.vendor_session_id.trim();
        if thread_id.is_empty() {
            return TurnDeliveryOutcome::Rejected {
                reason: "Codex steer_turn requires a non-empty thread_id".into(),
            };
        }
        let sock = match self.resolve_control_socket_path(
            request.control_endpoint.as_deref(),
            request.home_dir.as_deref(),
        ) {
            Ok(sock) => sock,
            Err(reason) => return TurnDeliveryOutcome::Rejected { reason },
        };
        steer_codex_turn(
            &sock,
            thread_id,
            request.vendor_turn_id.as_deref(),
            &request.text,
            self.rpc_timeout,
        )
    }

    fn cancel_turn(&self, request: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
        if request.binding.generation != request.expected_generation {
            return TurnDeliveryOutcome::Rejected {
                reason: format!(
                    "generation mismatch: binding={} expected={}",
                    request.binding.generation, request.expected_generation
                ),
            };
        }
        let thread_id = request.vendor_session_id.trim();
        if thread_id.is_empty() {
            return TurnDeliveryOutcome::Rejected {
                reason: "Codex cancel_turn requires a non-empty thread_id".into(),
            };
        }
        let sock = match self.resolve_control_socket_path(
            request.control_endpoint.as_deref(),
            request.home_dir.as_deref(),
        ) {
            Ok(sock) => sock,
            Err(reason) => return TurnDeliveryOutcome::Rejected { reason },
        };
        interrupt_codex_turn(
            &sock,
            thread_id,
            request.vendor_turn_id.as_deref(),
            self.rpc_timeout,
        )
    }

    fn supported_thinking_efforts(&self) -> &[&str] {
        CODEX_SUPPORTED_EFFORTS
    }

    fn validate_config(
        &self,
        model: Option<&str>,
        thinking_effort: Option<&str>,
    ) -> Result<VendorConfigSpec, VendorAdapterError> {
        let normalized_model = match model.map(str::trim).filter(|s| !s.is_empty()) {
            Some(m) if is_plain_model_id(m) => Some(m.to_string()),
            Some(bad) => {
                return Err(VendorAdapterError::InvalidConfig {
                    vendor: "codex".into(),
                    reason: format!(
                        "invalid Codex model id `{bad}`; must be a plain model identifier without context-window tags"
                    ),
                });
            }
            None => None,
        };
        let normalized_thinking_effort =
            match thinking_effort.map(str::trim).filter(|s| !s.is_empty()) {
                Some(level) if is_plain_thinking_effort(level) => Some(level.to_string()),
                Some(bad) => {
                    return Err(VendorAdapterError::InvalidConfig {
                        vendor: "codex".into(),
                        reason: format!("invalid Codex reasoning effort `{bad}`"),
                    });
                }
                None => None,
            };
        Ok(VendorConfigSpec {
            normalized_model,
            normalized_thinking_effort,
            model_scope: ConfigEffectiveScope::NextTurn,
            thinking_effort_scope: ConfigEffectiveScope::NextTurn,
        })
    }

    fn normalize_transcript(
        &self,
        raw_payload: &str,
        after_cursor: Option<&str>,
    ) -> Result<TranscriptNormalizeReport, VendorAdapterError> {
        let mut turns = Vec::new();
        let mut past_cursor = after_cursor.is_none();
        let mut last_cursor = None;

        for (idx, line) in raw_payload.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let cursor = format!("line:{}", idx + 1);
            if !past_cursor {
                if Some(cursor.as_str()) == after_cursor {
                    past_cursor = true;
                }
                continue;
            }
            let val: Value =
                serde_json::from_str(trimmed).map_err(|e| VendorAdapterError::Protocol {
                    vendor: "codex".into(),
                    reason: format!("malformed Codex transcript JSONL at {cursor}: {e}"),
                })?;
            let role = val
                .get("role")
                .and_then(Value::as_str)
                .or_else(|| val.pointer("/payload/role").and_then(Value::as_str))
                .or_else(|| val.pointer("/message/role").and_then(Value::as_str))
                .unwrap_or("assistant")
                .to_string();
            let content = val
                .get("content")
                .and_then(Value::as_str)
                .or_else(|| val.pointer("/payload/text").and_then(Value::as_str))
                .or_else(|| val.pointer("/message/content").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            let turn_id = val
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| val.get("turnId").and_then(Value::as_str))
                .map(str::to_string)
                .unwrap_or_else(|| format!("codex-turn-{}", idx + 1));
            last_cursor = Some(cursor.clone());
            turns.push(NormalizedSessionTurn {
                turn_id,
                role,
                content,
                source_cursor: cursor,
            });
        }

        Ok(TranscriptNormalizeReport {
            schema_version: 1,
            turns,
            next_cursor: last_cursor,
        })
    }

    fn stop(&self, _target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
        Ok(TeardownOutcome::Completed)
    }

    fn detach(&self, _target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
        Ok(TeardownOutcome::Completed)
    }

    fn archive(&self, target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
        let thread_id = target
            .vendor_session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| VendorAdapterError::MissingSessionIdentity {
                vendor: "codex".into(),
                reason: "Codex archive requires an explicit owned thread_id".into(),
            })?;
        let sock = self
            .resolve_control_socket_path(
                target.control_endpoint.as_deref(),
                target.home_dir.as_deref(),
            )
            .map_err(|reason| VendorAdapterError::Transport {
                vendor: "codex".into(),
                reason,
            })?;
        archive_codex_thread(&sock, thread_id, self.rpc_timeout)
    }

    fn delete(&self, _target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
        Ok(TeardownOutcome::Unsupported {
            reason: "Codex sessions use native `thread/archive` teardown rather than hard deletion"
                .into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Single Codex app-server Unix WebSocket (RFC 6455) JSON-RPC writer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcStage {
    Connect,
    Handshake,
    Initialize,
    ThreadRead,
    TurnsList,
    TurnStart,
    CompactStart,
    TurnSteer,
    TurnInterrupt,
    ThreadArchive,
}

impl RpcStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Handshake => "handshake",
            Self::Initialize => "initialize",
            Self::ThreadRead => "thread/read",
            Self::TurnsList => "thread/turns/list",
            Self::TurnStart => "turn/start",
            Self::CompactStart => "thread/compact/start",
            Self::TurnSteer => "turn/steer",
            Self::TurnInterrupt => "turn/interrupt",
            Self::ThreadArchive => "thread/archive",
        }
    }

    const fn is_mutating_delivery(self) -> bool {
        matches!(
            self,
            Self::TurnStart | Self::CompactStart | Self::TurnSteer | Self::TurnInterrupt
        )
    }
}

/// Probe `thread/read` over the Codex app-server control socket and map `thread.status.type`
/// into [`AdapterReadiness`].
pub fn probe_codex_thread_readiness(
    sock: &Path,
    thread_id: &str,
    timeout: Duration,
) -> Result<AdapterReadiness, String> {
    let deadline = Instant::now() + timeout;
    let mut conn = Conn::connect_and_initialize(sock, deadline).map_err(|(_, msg)| msg)?;
    conn.send_json(&json!({
        "id": 2,
        "method": "thread/read",
        "params": { "threadId": thread_id, "includeTurns": false },
    }))?;
    let result = conn
        .await_rpc_result(2, "thread/read", RpcStage::ThreadRead)
        .map_err(|(_, msg)| msg)?;
    let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
    let thread = &result["thread"];
    if thread["id"].as_str() != Some(thread_id) {
        return Err("thread/read response did not match the requested threadId".into());
    }
    let readiness = match thread["status"]["type"].as_str() {
        Some("idle") => AdapterReadiness::ReadyIdle,
        Some("active") => AdapterReadiness::Busy,
        Some("notLoaded") => AdapterReadiness::Starting,
        Some("systemError") => AdapterReadiness::Exited,
        _ => AdapterReadiness::Unknown,
    };
    Ok(readiness)
}

/// Submit a turn (or `/compact` via `thread/compact/start`) over the Codex app-server control
/// socket.
///
/// Preflights the thread via `thread/read` to ensure `status.type == "idle"` and
/// `canAcceptDirectInput == true` before writing `turn/start`. Any failure before the mutating
/// write returns [`TurnDeliveryOutcome::Rejected`]; any transport or unverified error after the
/// mutating write begins returns [`TurnDeliveryOutcome::OutcomeUnknown`].
pub fn submit_codex_turn(
    sock: &Path,
    thread_id: &str,
    text: &str,
    timeout: Duration,
) -> TurnDeliveryOutcome {
    if thread_id.trim().is_empty() {
        return TurnDeliveryOutcome::Rejected {
            reason: "thread_id must not be empty".into(),
        };
    }
    if thread_id.len() > MAX_TURN_INPUT || text.len() > MAX_TURN_INPUT {
        return TurnDeliveryOutcome::Rejected {
            reason: format!("Codex turn request exceeds {MAX_RPC_REQUEST} bytes"),
        };
    }

    let is_compact = text.trim().eq_ignore_ascii_case("/compact");
    let mut encoded_mutation = Vec::new();
    let encode_res = if is_compact {
        encode_compact_request(&mut encoded_mutation, 3, thread_id)
    } else {
        encode_turn_start_request(&mut encoded_mutation, 3, thread_id, text)
    };
    if let Err(reason) = encode_res {
        return TurnDeliveryOutcome::Rejected { reason };
    }

    let deadline = Instant::now() + timeout;
    let mut conn = match Conn::connect_and_initialize(sock, deadline) {
        Ok(c) => c,
        Err((_stage, reason)) => return TurnDeliveryOutcome::Rejected { reason },
    };

    if is_compact {
        if let Err(message) = conn.send(OP_TEXT, &encoded_mutation) {
            return TurnDeliveryOutcome::OutcomeUnknown {
                stage: RpcStage::CompactStart.as_str().to_string(),
                message,
            };
        }
        let outcome = match conn.await_rpc_result(3, "thread/compact/start", RpcStage::CompactStart)
        {
            Ok(_) => TurnDeliveryOutcome::Accepted { turn_id: None },
            Err((OutcomeClass::Rejected, reason)) => TurnDeliveryOutcome::Rejected { reason },
            Err((OutcomeClass::Unknown, message)) => TurnDeliveryOutcome::OutcomeUnknown {
                stage: RpcStage::CompactStart.as_str().to_string(),
                message,
            },
        };
        let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
        return outcome;
    }

    // Preflight `thread/read` to verify the thread is idle and accepts direct input.
    if let Err(reason) = conn.send_json(&json!({
        "id": 2,
        "method": "thread/read",
        "params": { "threadId": thread_id, "includeTurns": false },
    })) {
        return TurnDeliveryOutcome::Rejected { reason };
    }
    let metadata = match conn.await_rpc_result(2, "thread/read", RpcStage::ThreadRead) {
        Ok(val) => val,
        Err((_, reason)) => {
            let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
            return TurnDeliveryOutcome::Rejected { reason };
        }
    };
    let thread = &metadata["thread"];
    if thread["id"].as_str() != Some(thread_id) {
        let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
        return TurnDeliveryOutcome::Rejected {
            reason: "thread/read did not prove the requested thread identity".into(),
        };
    }
    let status_type = thread["status"]["type"].as_str().unwrap_or("unknown");
    if status_type != "idle" {
        let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
        return TurnDeliveryOutcome::Rejected {
            reason: format!("thread `{thread_id}` is `{status_type}`, not `idle`"),
        };
    }
    if thread["canAcceptDirectInput"].as_bool() != Some(true) {
        let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
        return TurnDeliveryOutcome::Rejected {
            reason: format!("thread `{thread_id}` does not currently accept direct input"),
        };
    }

    // Mutating write: from this point on, any transport or non-validation failure is OutcomeUnknown.
    if let Err(message) = conn.send(OP_TEXT, &encoded_mutation) {
        return TurnDeliveryOutcome::OutcomeUnknown {
            stage: RpcStage::TurnStart.as_str().to_string(),
            message,
        };
    }
    let outcome = match conn.await_rpc_result(3, "turn/start", RpcStage::TurnStart) {
        Ok(ack) => {
            let turn_id = ack["turn"]["id"]
                .as_str()
                .or_else(|| ack["turnId"].as_str())
                .map(str::to_string);
            if ack
                .get("threadId")
                .and_then(Value::as_str)
                .is_some_and(|id| id != thread_id)
            {
                TurnDeliveryOutcome::OutcomeUnknown {
                    stage: RpcStage::TurnStart.as_str().to_string(),
                    message: "turn/start acknowledgment threadId did not match requested thread"
                        .into(),
                }
            } else {
                TurnDeliveryOutcome::Accepted { turn_id }
            }
        }
        Err((OutcomeClass::Rejected, reason)) => TurnDeliveryOutcome::Rejected { reason },
        Err((OutcomeClass::Unknown, message)) => TurnDeliveryOutcome::OutcomeUnknown {
            stage: RpcStage::TurnStart.as_str().to_string(),
            message,
        },
    };
    let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
    outcome
}

/// Steer an active turn over the Codex app-server control socket using `expectedTurnId`.
///
/// Preflights via `thread/read` (`status.type == "active"` and `canAcceptDirectInput == true`)
/// and `thread/turns/list` (`status == "inProgress"`). When `expected_turn_id` is supplied by
/// the caller, verifies that the server's active turn matches it before sending `turn/steer`.
pub fn steer_codex_turn(
    sock: &Path,
    thread_id: &str,
    expected_turn_id: Option<&str>,
    text: &str,
    timeout: Duration,
) -> TurnDeliveryOutcome {
    if thread_id.trim().is_empty() {
        return TurnDeliveryOutcome::Rejected {
            reason: "thread_id must not be empty".into(),
        };
    }
    if thread_id.len() > MAX_TURN_INPUT || text.len() > MAX_TURN_INPUT {
        return TurnDeliveryOutcome::Rejected {
            reason: format!("Codex steering request exceeds {MAX_RPC_REQUEST} bytes"),
        };
    }
    if let Err(reason) = encode_turn_steer_request(io::sink(), 4, thread_id, "", text) {
        return TurnDeliveryOutcome::Rejected { reason };
    }

    let deadline = Instant::now() + timeout;
    let mut conn = match Conn::connect_and_initialize(sock, deadline) {
        Ok(c) => c,
        Err((_stage, reason)) => return TurnDeliveryOutcome::Rejected { reason },
    };

    if let Err(reason) = conn.send_json(&json!({
        "id": 2,
        "method": "thread/read",
        "params": { "threadId": thread_id, "includeTurns": false },
    })) {
        return TurnDeliveryOutcome::Rejected { reason };
    }
    let metadata = match conn.await_rpc_result(2, "thread/read", RpcStage::ThreadRead) {
        Ok(val) => val,
        Err((_, reason)) => {
            let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
            return TurnDeliveryOutcome::Rejected { reason };
        }
    };
    let thread = &metadata["thread"];
    if thread["id"].as_str() != Some(thread_id) {
        let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
        return TurnDeliveryOutcome::Rejected {
            reason: "thread/read did not prove the requested thread identity".into(),
        };
    }
    match thread["status"]["type"].as_str() {
        Some("active") if thread["canAcceptDirectInput"].as_bool() == Some(true) => {}
        Some(other) => {
            let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
            return TurnDeliveryOutcome::Rejected {
                reason: format!(
                    "thread/read did not prove an active thread accepting direct input (status `{other}`)"
                ),
            };
        }
        None => {
            let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
            return TurnDeliveryOutcome::Rejected {
                reason: "thread/read returned no status.type".into(),
            };
        }
    }

    if let Err(reason) = conn.send_json(&json!({
        "id": 3,
        "method": "thread/turns/list",
        "params": {
            "threadId": thread_id,
            "limit": 1,
            "sortDirection": "desc",
            "itemsView": "notLoaded"
        },
    })) {
        return TurnDeliveryOutcome::Rejected { reason };
    }
    let turns = match conn.await_rpc_result(3, "thread/turns/list", RpcStage::TurnsList) {
        Ok(val) => val,
        Err((_, reason)) => {
            let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
            return TurnDeliveryOutcome::Rejected { reason };
        }
    };
    let latest = turns["data"]
        .as_array()
        .filter(|arr| arr.len() == 1)
        .and_then(|arr| arr.first())
        .filter(|turn| turn["status"].as_str() == Some("inProgress"));
    let Some(proven_turn_id) = latest
        .and_then(|turn| turn["id"].as_str())
        .filter(|id| !id.trim().is_empty())
    else {
        let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
        return TurnDeliveryOutcome::Rejected {
            reason: "thread/turns/list did not prove one latest active turn".into(),
        };
    };

    if let Some(caller_expected) = expected_turn_id.map(str::trim).filter(|s| !s.is_empty())
        && caller_expected != proven_turn_id
    {
        let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
        return TurnDeliveryOutcome::Rejected {
            reason: format!(
                "expectedTurnId mismatch: caller expected `{caller_expected}`, active turn is `{proven_turn_id}`"
            ),
        };
    }

    let mut request_bytes = Vec::new();
    if let Err(reason) =
        encode_turn_steer_request(&mut request_bytes, 4, thread_id, proven_turn_id, text)
    {
        let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
        return TurnDeliveryOutcome::Rejected { reason };
    }

    if let Err(message) = conn.send(OP_TEXT, &request_bytes) {
        return TurnDeliveryOutcome::OutcomeUnknown {
            stage: RpcStage::TurnSteer.as_str().to_string(),
            message,
        };
    }
    let outcome = match conn.await_rpc_result(4, "turn/steer", RpcStage::TurnSteer) {
        Ok(ack) => {
            if ack["turnId"].as_str() != Some(proven_turn_id)
                || ack
                    .get("threadId")
                    .is_some_and(|id| id.as_str() != Some(thread_id))
            {
                TurnDeliveryOutcome::OutcomeUnknown {
                    stage: RpcStage::TurnSteer.as_str().to_string(),
                    message:
                        "turn/steer acknowledgment did not match the requested thread and active turn"
                            .into(),
                }
            } else {
                TurnDeliveryOutcome::Accepted {
                    turn_id: Some(proven_turn_id.to_string()),
                }
            }
        }
        Err((OutcomeClass::Rejected, reason)) => TurnDeliveryOutcome::Rejected { reason },
        Err((OutcomeClass::Unknown, message)) => TurnDeliveryOutcome::OutcomeUnknown {
            stage: RpcStage::TurnSteer.as_str().to_string(),
            message,
        },
    };
    let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
    outcome
}

/// Interrupt an active turn on `thread_id` via `turn/interrupt` over the Codex app-server control socket.
pub fn interrupt_codex_turn(
    sock: &Path,
    thread_id: &str,
    turn_id: Option<&str>,
    timeout: Duration,
) -> TurnDeliveryOutcome {
    if thread_id.trim().is_empty() {
        return TurnDeliveryOutcome::Rejected {
            reason: "thread_id must not be empty".into(),
        };
    }
    let deadline = Instant::now() + timeout;
    let mut conn = match Conn::connect_and_initialize(sock, deadline) {
        Ok(c) => c,
        Err((_stage, reason)) => return TurnDeliveryOutcome::Rejected { reason },
    };
    let mut params = json!({ "threadId": thread_id });
    if let Some(tid) = turn_id.map(str::trim).filter(|s| !s.is_empty()) {
        params["turnId"] = json!(tid);
    }
    if let Err(message) = conn.send_json(&json!({
        "id": 2,
        "method": "turn/interrupt",
        "params": params,
    })) {
        return TurnDeliveryOutcome::OutcomeUnknown {
            stage: RpcStage::TurnInterrupt.as_str().to_string(),
            message,
        };
    }
    let outcome = match conn.await_rpc_result(2, "turn/interrupt", RpcStage::TurnInterrupt) {
        Ok(_) => TurnDeliveryOutcome::Accepted {
            turn_id: turn_id.map(str::to_string),
        },
        Err((OutcomeClass::Rejected, reason)) => TurnDeliveryOutcome::Rejected { reason },
        Err((OutcomeClass::Unknown, message)) => TurnDeliveryOutcome::OutcomeUnknown {
            stage: RpcStage::TurnInterrupt.as_str().to_string(),
            message,
        },
    };
    let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
    outcome
}

/// Archive `thread_id` using the native `thread/archive` JSON-RPC method over the Codex
/// app-server control socket, scoped strictly to `thread_id`.
pub fn archive_codex_thread(
    sock: &Path,
    thread_id: &str,
    timeout: Duration,
) -> Result<TeardownOutcome, VendorAdapterError> {
    let thread_id = thread_id.trim();
    if thread_id.is_empty() {
        return Err(VendorAdapterError::MissingSessionIdentity {
            vendor: "codex".into(),
            reason: "thread_id must not be empty".into(),
        });
    }
    let deadline = Instant::now() + timeout;
    let mut conn = Conn::connect_and_initialize(sock, deadline).map_err(|(_, reason)| {
        VendorAdapterError::Transport {
            vendor: "codex".into(),
            reason,
        }
    })?;
    conn.send_json(&json!({
        "id": 2,
        "method": "thread/archive",
        "params": { "threadId": thread_id },
    }))
    .map_err(|reason| VendorAdapterError::Transport {
        vendor: "codex".into(),
        reason,
    })?;
    let res = conn.await_rpc_result(2, "thread/archive", RpcStage::ThreadArchive);
    let _ = conn.send(OP_CLOSE, &1000u16.to_be_bytes());
    match res {
        Ok(_) => Ok(TeardownOutcome::Completed),
        Err((_class, reason)) => {
            let lower = reason.to_ascii_lowercase();
            if lower.contains("not found") || lower.contains("already archived") {
                Ok(TeardownOutcome::AlreadyAbsent)
            } else {
                Err(VendorAdapterError::Protocol {
                    vendor: "codex".into(),
                    reason,
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeClass {
    Rejected,
    Unknown,
}

struct BoundedWriter<W> {
    inner: W,
    left: usize,
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > self.left {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let written = self.inner.write(buf)?;
        self.left -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn encode_turn_start_request(
    writer: impl Write,
    id: u8,
    thread_id: &str,
    text: &str,
) -> Result<(), String> {
    #[derive(Serialize)]
    struct Request<'a> {
        id: u8,
        method: &'static str,
        params: Params<'a>,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Params<'a> {
        thread_id: &'a str,
        input: [Input<'a>; 1],
    }
    #[derive(Serialize)]
    struct Input<'a> {
        r#type: &'static str,
        text: &'a str,
    }
    serde_json::to_writer(
        BoundedWriter {
            inner: writer,
            left: MAX_RPC_REQUEST,
        },
        &Request {
            id,
            method: "turn/start",
            params: Params {
                thread_id,
                input: [Input {
                    r#type: "text",
                    text,
                }],
            },
        },
    )
    .map_err(|_| format!("Codex turn/start request exceeds {MAX_RPC_REQUEST} bytes"))
}

fn encode_compact_request(writer: impl Write, id: u8, thread_id: &str) -> Result<(), String> {
    #[derive(Serialize)]
    struct Request<'a> {
        id: u8,
        method: &'static str,
        params: Params<'a>,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Params<'a> {
        thread_id: &'a str,
    }
    serde_json::to_writer(
        BoundedWriter {
            inner: writer,
            left: MAX_RPC_REQUEST,
        },
        &Request {
            id,
            method: "thread/compact/start",
            params: Params { thread_id },
        },
    )
    .map_err(|_| format!("Codex thread/compact/start request exceeds {MAX_RPC_REQUEST} bytes"))
}

fn encode_turn_steer_request(
    writer: impl Write,
    id: u8,
    thread_id: &str,
    expected_turn_id: &str,
    text: &str,
) -> Result<(), String> {
    #[derive(Serialize)]
    struct Request<'a> {
        id: u8,
        method: &'static str,
        params: Params<'a>,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Params<'a> {
        thread_id: &'a str,
        expected_turn_id: &'a str,
        input: [Input<'a>; 1],
    }
    #[derive(Serialize)]
    struct Input<'a> {
        r#type: &'static str,
        text: &'a str,
    }
    serde_json::to_writer(
        BoundedWriter {
            inner: writer,
            left: MAX_RPC_REQUEST,
        },
        &Request {
            id,
            method: "turn/steer",
            params: Params {
                thread_id,
                expected_turn_id,
                input: [Input {
                    r#type: "text",
                    text,
                }],
            },
        },
    )
    .map_err(|_| format!("Codex steering request exceeds {MAX_RPC_REQUEST} bytes"))
}

#[cfg(unix)]
fn connect_unix_with_deadline(sock: &Path, deadline: Instant) -> io::Result<UnixStream> {
    use socket2::{Domain, SockAddr, Socket, Type};
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
    };
    remaining()?;
    let address = SockAddr::unix(sock)?;
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.connect_timeout(&address, remaining()?)?;
    if let Some(error) = socket.take_error()? {
        return Err(error);
    }
    socket.peer_addr()?;
    remaining()?;
    let descriptor: std::os::fd::OwnedFd = socket.into();
    Ok(UnixStream::from(descriptor))
}

#[cfg(unix)]
fn write_with_deadline(stream: &UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let socket = socket2::SockRef::from(stream);
    socket.set_nonblocking(true)?;
    let result = (|| {
        let mut writer = stream;
        while !bytes.is_empty() {
            if Instant::now() >= deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match writer.write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    let mut fd = libc::pollfd {
                        fd: stream.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    let wait_ms = left.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
                    // SAFETY: fd is one initialized pollfd borrowed from the live stream.
                    let ready = unsafe { libc::poll(&mut fd, 1, wait_ms) };
                    if ready < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::Interrupted {
                            return Err(error);
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })();
    let restored = socket.set_nonblocking(false);
    result.and(restored)
}

#[cfg(unix)]
struct DeadlineReader<'a> {
    stream: &'a UnixStream,
    deadline: Instant,
}

#[cfg(unix)]
impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(left.min(IO_TIMEOUT)))?;
        self.stream.read(buf)
    }
}

#[cfg(unix)]
struct Conn {
    stream: UnixStream,
    rbuf: Vec<u8>,
    rng: SystemRandom,
    deadline: Instant,
}

#[cfg(unix)]
impl Conn {
    fn connect_and_initialize(sock: &Path, deadline: Instant) -> Result<Self, (RpcStage, String)> {
        let stream = connect_unix_with_deadline(sock, deadline).map_err(|e| {
            (
                RpcStage::Connect,
                format!(
                    "cannot connect to the Codex app-server control socket {}: {e}",
                    sock.display()
                ),
            )
        })?;
        let mut conn = Self {
            stream,
            rbuf: Vec::new(),
            rng: SystemRandom::new(),
            deadline,
        };
        conn.handshake().map_err(|msg| (RpcStage::Handshake, msg))?;
        conn.send_json(&json!({
            "id": 1,
            "method": "initialize",
            "params": { "clientInfo": { "name": "ferroloop", "version": "0.1.0" } },
        }))
        .map_err(|msg| (RpcStage::Initialize, msg))?;
        conn.await_rpc_result(1, "initialize", RpcStage::Initialize)
            .map_err(|(_, msg)| (RpcStage::Initialize, msg))?;
        conn.send_json(&json!({ "method": "initialized", "params": {} }))
            .map_err(|msg| (RpcStage::Initialize, msg))?;
        Ok(conn)
    }

    fn io_budget(&self) -> Result<Duration, String> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(timed_out());
        }
        Ok(left.min(IO_TIMEOUT))
    }

    fn handshake(&mut self) -> Result<(), String> {
        let mut nonce = [0u8; 16];
        self.rng
            .fill(&mut nonce)
            .map_err(|_| "random number generator failure".to_string())?;
        let key = BASE64.encode(nonce);
        let request = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        write_with_deadline(&self.stream, request.as_bytes(), self.deadline)
            .map_err(|e| io_failure("sending the WebSocket handshake", &e))?;
        let mut reader = DeadlineReader {
            stream: &self.stream,
            deadline: self.deadline,
        };
        let headers = read_http_headers(&mut reader)?;
        let mut lines = headers.split("\r\n");
        let status_line = lines.next().unwrap_or_default();
        if status_line.split_whitespace().nth(1) != Some("101") {
            return Err(format!(
                "Codex app-server refused the WebSocket upgrade: {}",
                truncate(status_line, MAX_ERROR_TEXT)
            ));
        }
        let accept = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("sec-websocket-accept")
                .then(|| value.trim().to_string())
        });
        match accept {
            Some(accept) if accept == compute_accept_key(&key) => Ok(()),
            Some(_) => Err("Codex app-server sent a wrong Sec-WebSocket-Accept".to_string()),
            None => Err("Codex app-server upgrade reply has no Sec-WebSocket-Accept".to_string()),
        }
    }

    fn send_json(&mut self, value: &Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|e| format!("cannot encode request: {e}"))?;
        self.send(OP_TEXT, &bytes)
    }

    fn send(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut mask = [0u8; 4];
        self.rng
            .fill(&mut mask)
            .map_err(|_| "random number generator failure".to_string())?;
        let frame = encode_client_frame(opcode, payload, mask);
        write_with_deadline(&self.stream, &frame, self.deadline)
            .map_err(|e| io_failure("writing to the Codex app-server", &e))
    }

    fn read_frame(&mut self) -> Result<Frame, String> {
        loop {
            if let Some((frame, used)) = decode_server_frame(&self.rbuf)? {
                self.rbuf.drain(..used);
                return Ok(frame);
            }
            self.stream
                .set_read_timeout(Some(self.io_budget()?))
                .map_err(|e| format!("Codex app-server socket error: {e}"))?;
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("Codex app-server closed the connection".to_string()),
                Ok(n) => self.rbuf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io_failure("reading from the Codex app-server", &e)),
            }
        }
    }

    fn recv_text(&mut self) -> Result<String, String> {
        let mut partial: Option<(bool, Vec<u8>)> = None;
        loop {
            self.io_budget()?;
            let frame = self.read_frame()?;
            let (is_text, bytes) = match frame.opcode {
                OP_PING => {
                    self.send(OP_PONG, &frame.payload)?;
                    continue;
                }
                OP_PONG => continue,
                OP_CLOSE => return Err(close_error(&frame.payload)),
                OP_TEXT | OP_BINARY => {
                    if partial.is_some() {
                        return Err("Codex app-server started a new message inside a \
                                    fragmented one"
                            .to_string());
                    }
                    if !frame.fin {
                        partial = Some((frame.opcode == OP_TEXT, frame.payload));
                        continue;
                    }
                    (frame.opcode == OP_TEXT, frame.payload)
                }
                _ => {
                    let Some((is_text, mut bytes)) = partial.take() else {
                        return Err("Codex app-server sent a stray continuation frame".to_string());
                    };
                    if bytes.len() + frame.payload.len() > MAX_MESSAGE {
                        return Err(format!(
                            "Codex app-server message exceeds {MAX_MESSAGE} bytes"
                        ));
                    }
                    bytes.extend_from_slice(&frame.payload);
                    if !frame.fin {
                        partial = Some((is_text, bytes));
                        continue;
                    }
                    (is_text, bytes)
                }
            };
            if !is_text {
                continue;
            }
            return String::from_utf8(bytes)
                .map_err(|_| "Codex app-server sent a text message that is not UTF-8".to_string());
        }
    }

    fn await_rpc_result(
        &mut self,
        id: i64,
        method: &str,
        stage: RpcStage,
    ) -> Result<Value, (OutcomeClass, String)> {
        let default_class = if stage.is_mutating_delivery() {
            OutcomeClass::Unknown
        } else {
            OutcomeClass::Rejected
        };
        loop {
            let text = self.recv_text().map_err(|msg| (default_class, msg))?;
            let malformed = || {
                (
                    default_class,
                    format!("Codex app-server sent a malformed response to `{method}`"),
                )
            };
            let message: Value = serde_json::from_str(&text).map_err(|_| malformed())?;
            if !message.is_object() {
                return Err(malformed());
            }
            if let Some(m) = message.get("method") {
                if m.is_string()
                    && message.get("result").is_none()
                    && message.get("error").is_none()
                {
                    continue;
                }
                return Err(malformed());
            }
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                return Err((
                    default_class,
                    format!("Codex app-server response ID did not match `{method}`"),
                ));
            }
            return match (
                message.get("error").filter(|error| !error.is_null()),
                message.get("result"),
            ) {
                (Some(error), None) => {
                    let description = format!(
                        "Codex app-server rejected `{method}`: {}",
                        describe_rpc_error(error)
                    );
                    let code = error.get("code").and_then(Value::as_i64);
                    // JSON-RPC protocol/validation errors (-32602..-32600) establish definite
                    // rejection even on mutating requests (for example stale expectedTurnId).
                    if error.get("message").is_some_and(Value::is_string)
                        && matches!(code, Some(-32602..=-32600))
                    {
                        Err((OutcomeClass::Rejected, description))
                    } else {
                        Err((default_class, description))
                    }
                }
                (None, Some(result)) if result.is_object() => Ok(result.clone()),
                _ => Err(malformed()),
            };
        }
    }
}

fn read_http_headers<R: Read>(reader: &mut R) -> Result<String, String> {
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while buf.len() < 16_384 {
        let n = match reader.read(&mut byte) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_failure("reading the WebSocket handshake reply", &e)),
        };
        if n == 0 {
            return Err(
                "Codex app-server WebSocket handshake failed: unexpected EOF reading headers"
                    .to_string(),
            );
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            return String::from_utf8(buf).map_err(|e| {
                format!("Codex app-server WebSocket handshake failed: non-UTF8 HTTP headers: {e}")
            });
        }
    }
    Err("Codex app-server WebSocket handshake failed: HTTP headers exceeded 16 KB".to_string())
}

pub(crate) fn compute_accept_key(sec_key: &str) -> String {
    let mut input = Vec::with_capacity(sec_key.len() + WS_GUID.len());
    input.extend_from_slice(sec_key.trim().as_bytes());
    input.extend_from_slice(WS_GUID.as_bytes());
    let digest = sha1_digest(&input);
    BASE64.encode(digest)
}

/// Compact RFC 3174 SHA-1 implementation used solely for RFC 6455 `Sec-WebSocket-Accept`.
fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

fn describe_rpc_error(error: &Value) -> String {
    let message: Option<String> = error
        .get("message")
        .and_then(Value::as_str)
        .map(|m| m.chars().filter(|c| !c.is_control()).collect());
    let code = error.get("code").and_then(Value::as_i64);
    let text = match (message.filter(|m| !m.is_empty()), code) {
        (Some(message), Some(code)) => format!("{message} (code {code})"),
        (Some(message), None) => message,
        (None, Some(code)) => format!("(code {code})"),
        (None, None) => "no message".to_string(),
    };
    truncate(&text, MAX_ERROR_TEXT)
}

fn close_error(payload: &[u8]) -> String {
    match payload {
        [hi, lo, ..] => format!(
            "Codex app-server closed the connection (code {})",
            u16::from_be_bytes([*hi, *lo])
        ),
        _ => "Codex app-server closed the connection".to_string(),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn timed_out() -> String {
    "timed out waiting for the Codex app-server".to_string()
}

fn io_failure(doing: &str, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => timed_out(),
        _ => format!("error {doing}: {e}"),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn encode_client_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + payload.len());
    out.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if let Ok(len) = u16::try_from(len) {
        out.push(0x80 | 126);
        out.extend_from_slice(&len.to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
    out
}

fn decode_server_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>, String> {
    if buf.len() < 2 {
        return Ok(None);
    }
    if buf[0] & 0x70 != 0 {
        return Err("Codex app-server frame has reserved bits set".to_string());
    }
    let fin = buf[0] & 0x80 != 0;
    let opcode = buf[0] & 0x0F;
    if !matches!(
        opcode,
        OP_CONTINUATION | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG
    ) {
        return Err(format!("Codex app-server sent unknown opcode 0x{opcode:x}"));
    }
    if buf[1] & 0x80 != 0 {
        return Err("Codex app-server sent a masked frame".to_string());
    }
    let mut pos = 2usize;
    let mut len = u64::from(buf[1] & 0x7F);
    if len == 126 {
        if buf.len() < pos + 2 {
            return Ok(None);
        }
        len = u64::from(u16::from_be_bytes([buf[pos], buf[pos + 1]]));
        pos += 2;
    } else if len == 127 {
        if buf.len() < pos + 8 {
            return Ok(None);
        }
        let mut ext = [0u8; 8];
        ext.copy_from_slice(&buf[pos..pos + 8]);
        len = u64::from_be_bytes(ext);
        pos += 8;
    }
    if opcode & 0x8 != 0 && (!fin || len > 125) {
        return Err("Codex app-server sent a fragmented or oversize control frame".to_string());
    }
    if len > MAX_MESSAGE as u64 {
        return Err(format!(
            "Codex app-server frame of {len} bytes exceeds {MAX_MESSAGE}"
        ));
    }
    let len = len as usize;
    if buf.len() < pos + len {
        return Ok(None);
    }
    let frame = Frame {
        fin,
        opcode,
        payload: buf[pos..pos + len].to_vec(),
    };
    Ok(Some((frame, pos + len)))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread::JoinHandle;
    use tempfile::TempDir;

    #[derive(Debug, Clone)]
    struct Received {
        masked: bool,
        opcode: u8,
        payload: Vec<u8>,
    }

    impl Received {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.payload).expect("client text frame is JSON")
        }
    }

    struct ServerConn {
        stream: UnixStream,
        rbuf: Vec<u8>,
        log: Vec<Received>,
    }

    impl ServerConn {
        fn handshake(&mut self) {
            let request = read_http_headers(&mut self.stream).expect("upgrade request");
            let key = request
                .split("\r\n")
                .find_map(|l| {
                    let (n, v) = l.split_once(':')?;
                    n.trim()
                        .eq_ignore_ascii_case("Sec-WebSocket-Key")
                        .then(|| v.trim().to_string())
                })
                .expect("Sec-WebSocket-Key");
            let accept = compute_accept_key(&key);
            let _ = self.stream.write_all(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                )
                .as_bytes(),
            );
            let _ = self.stream.flush();
        }

        fn send_text(&mut self, text: &str) {
            let payload = text.as_bytes();
            let mut out = vec![0x80 | OP_TEXT];
            if payload.len() < 126 {
                out.push(payload.len() as u8);
            } else {
                out.push(126);
                out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            out.extend_from_slice(payload);
            let _ = self.stream.write_all(&out);
            let _ = self.stream.flush();
        }

        fn read_client_frame(&mut self) -> Option<Received> {
            loop {
                if let Some((received, used)) = parse_client_frame(&self.rbuf) {
                    self.rbuf.drain(..used);
                    self.log.push(received.clone());
                    return Some(received);
                }
                let mut chunk = [0u8; 4096];
                match self.stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => self.rbuf.extend_from_slice(&chunk[..n]),
                }
            }
        }

        fn expect_json(&mut self) -> Value {
            let frame = self.read_client_frame().expect("client frame");
            assert_eq!(frame.opcode, OP_TEXT);
            frame.json()
        }

        fn drain_until_eof(&mut self) {
            while self.read_client_frame().is_some() {}
        }
    }

    fn parse_client_frame(buf: &[u8]) -> Option<(Received, usize)> {
        if buf.len() < 2 {
            return None;
        }
        let opcode = buf[0] & 0x0F;
        let masked = buf[1] & 0x80 != 0;
        let mut pos = 2;
        let mut len = (buf[1] & 0x7F) as usize;
        if len == 126 {
            if buf.len() < 4 {
                return None;
            }
            len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            pos = 4;
        } else if len == 127 {
            if buf.len() < 10 {
                return None;
            }
            len = u64::from_be_bytes(buf[2..10].try_into().unwrap()) as usize;
            pos = 10;
        }
        let mut mask = [0u8; 4];
        if masked {
            if buf.len() < pos + 4 {
                return None;
            }
            mask.copy_from_slice(&buf[pos..pos + 4]);
            pos += 4;
        }
        if buf.len() < pos + len {
            return None;
        }
        let mut payload = buf[pos..pos + len].to_vec();
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }
        }
        Some((
            Received {
                masked,
                opcode,
                payload,
            },
            pos + len,
        ))
    }

    fn spawn_fake_server(
        dir: &Path,
        script: impl FnOnce(&mut ServerConn) + Send + 'static,
    ) -> (PathBuf, JoinHandle<Vec<Received>>) {
        let sock = dir.join("codex.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind fake control socket");
        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let give_up = Instant::now() + Duration::from_secs(3);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < give_up, "no client connected");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut conn = ServerConn {
                stream,
                rbuf: Vec::new(),
                log: Vec::new(),
            };
            script(&mut conn);
            conn.log
        });
        (sock, handle)
    }

    fn sample_binding() -> AdapterIdentityBinding {
        AdapterIdentityBinding::new("codex:feira", 3, "Feira.codex", Some((4242, 999)))
    }

    #[test]
    fn prepare_launch_builds_expected_argv_and_omits_agent_persona() {
        let adapter = CodexHostAdapter::new();
        let req = VendorLaunchRequest {
            binding: sample_binding(),
            persona: "feira".into(),
            model: Some("gpt-5.4".into()),
            thinking_effort: Some("xhigh".into()),
            initial_prompt: "Bootstrap prompt".into(),
            cwd: Some(PathBuf::from("/tmp/work")),
            home_dir: Some(PathBuf::from("/tmp/home")),
            remote_control: true,
        };
        let plan = adapter.prepare_launch(&req).unwrap();
        assert_eq!(plan.vendor_id, "codex");
        assert_eq!(
            &plan.argv[1..],
            &[
                "--yolo",
                "--remote",
                "unix://",
                "--model",
                "gpt-5.4",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "Bootstrap prompt"
            ]
        );
        assert!(
            plan.env.iter().all(|(k, _)| k != "AGENT_PERSONA"),
            "shared codex app-server must never receive AGENT_PERSONA"
        );
    }

    #[test]
    fn validate_config_rejects_window_tags_and_hostile_thinking_effort() {
        let adapter = CodexHostAdapter::new();
        assert!(adapter.validate_config(Some("gpt-5[1m]"), None).is_err());
        assert!(
            adapter
                .validate_config(Some("gpt-5"), Some("high; rm -rf /"))
                .is_err()
        );
        let ok = adapter
            .validate_config(Some("gpt-5"), Some("high"))
            .unwrap();
        assert_eq!(ok.normalized_model.as_deref(), Some("gpt-5"));
        assert_eq!(ok.normalized_thinking_effort.as_deref(), Some("high"));
        assert_eq!(ok.model_scope, ConfigEffectiveScope::NextTurn);
    }

    #[test]
    fn submit_turn_preflights_idle_thread_and_starts_turn() {
        let tmp = TempDir::new().unwrap();
        let (sock, server) = spawn_fake_server(tmp.path(), |c| {
            c.handshake();
            assert_eq!(c.expect_json()["method"], "initialize");
            c.send_text(r#"{"id":1,"result":{}}"#);
            assert_eq!(c.expect_json()["method"], "initialized");
            let read_req = c.expect_json();
            assert_eq!(read_req["method"], "thread/read");
            assert_eq!(read_req["params"]["threadId"], "thread-42");
            c.send_text(
                r#"{"id":2,"result":{"thread":{"id":"thread-42","status":{"type":"idle"},"canAcceptDirectInput":true}}}"#,
            );
            let start_req = c.expect_json();
            assert_eq!(start_req["method"], "turn/start");
            assert_eq!(start_req["params"]["threadId"], "thread-42");
            assert_eq!(start_req["params"]["input"][0]["text"], "run tests");
            c.send_text(r#"{"id":3,"result":{"turn":{"id":"turn-101"}}}"#);
            c.drain_until_eof();
        });

        let adapter = CodexHostAdapter::new();
        let outcome = adapter.submit_turn(&TurnDeliveryRequest {
            binding: sample_binding(),
            message_id: "msg-1".into(),
            vendor_session_id: "thread-42".into(),
            vendor_turn_id: None,
            expected_generation: 3,
            text: "run tests".into(),
            control_endpoint: Some(sock.to_string_lossy().into_owned()),
            home_dir: None,
        });
        assert_eq!(
            outcome,
            TurnDeliveryOutcome::Accepted {
                turn_id: Some("turn-101".into())
            }
        );
        let frames = server.join().unwrap();
        assert!(frames.iter().all(|f| f.masked));
    }

    #[test]
    fn submit_turn_returns_outcome_unknown_when_socket_closes_after_write() {
        let tmp = TempDir::new().unwrap();
        let (sock, server) = spawn_fake_server(tmp.path(), |c| {
            c.handshake();
            c.expect_json();
            c.send_text(r#"{"id":1,"result":{}}"#);
            c.expect_json();
            c.expect_json();
            c.send_text(
                r#"{"id":2,"result":{"thread":{"id":"thread-42","status":{"type":"idle"},"canAcceptDirectInput":true}}}"#,
            );
            // Receive `turn/start` and drop connection without replying.
            let start_req = c.expect_json();
            assert_eq!(start_req["method"], "turn/start");
        });

        let adapter = CodexHostAdapter::new();
        let outcome = adapter.submit_turn(&TurnDeliveryRequest {
            binding: sample_binding(),
            message_id: "msg-2".into(),
            vendor_session_id: "thread-42".into(),
            vendor_turn_id: None,
            expected_generation: 3,
            text: "ambiguous turn".into(),
            control_endpoint: Some(sock.to_string_lossy().into_owned()),
            home_dir: None,
        });
        assert!(
            matches!(
                outcome,
                TurnDeliveryOutcome::OutcomeUnknown { ref stage, .. } if stage == "turn/start"
            ),
            "expected OutcomeUnknown after turn/start write, got {outcome:?}"
        );
        server.join().unwrap();
    }

    #[test]
    fn steer_turn_enforces_expected_turn_id_and_handles_protocol_rejection() {
        let tmp = TempDir::new().unwrap();
        let (sock, server) = spawn_fake_server(tmp.path(), |c| {
            c.handshake();
            c.expect_json();
            c.send_text(r#"{"id":1,"result":{}}"#);
            c.expect_json();
            c.expect_json();
            c.send_text(
                r#"{"id":2,"result":{"thread":{"id":"thread-9","status":{"type":"active"},"canAcceptDirectInput":true}}}"#,
            );
            c.expect_json();
            c.send_text(r#"{"id":3,"result":{"data":[{"id":"turn-7","status":"inProgress"}]}}"#);
            let steer_req = c.expect_json();
            assert_eq!(steer_req["method"], "turn/steer");
            assert_eq!(steer_req["params"]["expectedTurnId"], "turn-7");
            c.send_text(r#"{"id":4,"result":{"turnId":"turn-7"}}"#);
            c.drain_until_eof();
        });

        let adapter = CodexHostAdapter::new();
        let outcome = adapter.steer_turn(&TurnDeliveryRequest {
            binding: sample_binding(),
            message_id: "msg-3".into(),
            vendor_session_id: "thread-9".into(),
            vendor_turn_id: Some("turn-7".into()),
            expected_generation: 3,
            text: "steer input".into(),
            control_endpoint: Some(sock.to_string_lossy().into_owned()),
            home_dir: None,
        });
        assert_eq!(
            outcome,
            TurnDeliveryOutcome::Accepted {
                turn_id: Some("turn-7".into())
            }
        );
        server.join().unwrap();

        // Caller expectedTurnId mismatch is rejected during preflight before writing turn/steer.
        let (sock_mismatch, server_mismatch) = spawn_fake_server(tmp.path(), |c| {
            c.handshake();
            c.expect_json();
            c.send_text(r#"{"id":1,"result":{}}"#);
            c.expect_json();
            c.expect_json();
            c.send_text(
                r#"{"id":2,"result":{"thread":{"id":"thread-9","status":{"type":"active"},"canAcceptDirectInput":true}}}"#,
            );
            c.expect_json();
            c.send_text(r#"{"id":3,"result":{"data":[{"id":"turn-8","status":"inProgress"}]}}"#);
            c.drain_until_eof();
        });
        let mismatch = adapter.steer_turn(&TurnDeliveryRequest {
            binding: sample_binding(),
            message_id: "msg-4".into(),
            vendor_session_id: "thread-9".into(),
            vendor_turn_id: Some("turn-7".into()),
            expected_generation: 3,
            text: "steer input".into(),
            control_endpoint: Some(sock_mismatch.to_string_lossy().into_owned()),
            home_dir: None,
        });
        assert!(
            mismatch.is_rejected(),
            "expected Rejected, got {mismatch:?}"
        );
        let log = server_mismatch.join().unwrap();
        assert!(
            !log.iter()
                .filter(|f| f.opcode == OP_TEXT)
                .any(|f| f.json()["method"] == "turn/steer"),
            "turn/steer must not be sent when caller expectedTurnId mismatches active turn"
        );
    }

    #[test]
    fn archive_sends_native_thread_archive_rpc_for_owned_thread_id() {
        let tmp = TempDir::new().unwrap();
        let (sock, server) = spawn_fake_server(tmp.path(), |c| {
            c.handshake();
            assert_eq!(c.expect_json()["method"], "initialize");
            c.send_text(r#"{"id":1,"result":{}}"#);
            assert_eq!(c.expect_json()["method"], "initialized");
            let archive_req = c.expect_json();
            assert_eq!(archive_req["method"], "thread/archive");
            assert_eq!(archive_req["params"]["threadId"], "thread-archive-99");
            c.send_text(r#"{"id":2,"result":{}}"#);
            c.drain_until_eof();
        });

        let adapter = CodexHostAdapter::new();
        let outcome = adapter
            .archive(&TeardownTarget {
                binding: sample_binding(),
                vendor_session_id: Some("thread-archive-99".into()),
                control_endpoint: Some(sock.to_string_lossy().into_owned()),
                home_dir: None,
            })
            .expect("archive succeeds");
        assert_eq!(outcome, TeardownOutcome::Completed);

        let frames = server.join().unwrap();
        let methods: Vec<String> = frames
            .iter()
            .filter(|f| f.opcode == OP_TEXT)
            .map(|f| f.json()["method"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(methods, ["initialize", "initialized", "thread/archive"]);
    }
}
