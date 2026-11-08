//! Slice 4b: Concrete [`VendorHostAdapter`] implementation for Anthropic Claude Code (`claude`).
//!
//! Enforces:
//! - `--remote-control` placed **before** `--enable-auto-mode` so Commander.js
//!   `--remote-control [name]` never consumes a trailing positional `<prompt>` when `model` and
//!   `thinking_effort` are both `None`.
//! - Explicit session-ID binding on launch (`--session-id <uuid>` and `--name <DisplayName>` when
//!   bound) and resume (`--resume <vendor_session_id>`), preserving genuine project/worktree `cwd`
//!   without mutating global `~/.claude/settings.json` and forbidding `--continue` or
//!   newest-by-`mtime` scanning in shared directories.
//! - Startup modal dismissal (`"Teach auto mode about your environment"` -> `"Escape"`,
//!   `warn_on_miss: false`).
//! - Turn delivery and `"Escape"` interrupt routing through the injected [`RuntimePort`] with
//!   generation precondition checks and three-valued [`TurnDeliveryOutcome`].
//! - Exact-session `.jsonl` transcript normalization with incremental cursor resumption and
//!   session-scoped teardown (`archive` / `delete` strictly by `vendor_session_id`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fl_core::vendor_adapter::{
    AdapterIdentityBinding, AdapterReadiness, AttachOrResumeOutcome, AttachOrResumeRequest,
    ConfigEffectiveScope, NormalizedSessionTurn, RuntimePort, TeardownOutcome, TeardownTarget,
    TranscriptNormalizeReport, TurnDeliveryOutcome, TurnDeliveryRequest, VendorAdapterError,
    VendorConfigSpec, VendorHostAdapter, VendorLaunchPlan, VendorLaunchRequest,
    VendorStartupPromptRule,
};
use serde_json::Value;

/// First-launch modal printed by Claude Code when auto mode asks for environment setup.
pub const CLAUDE_AUTO_MODE_PROMPT: &str = "Teach auto mode about your environment";

/// Key sequence used to interrupt an in-flight Claude Code turn (`Escape`, never `C-c`).
pub const CLAUDE_INTERRUPT_KEY: &str = "Escape";

/// Raw terminal bytes sent for [`CLAUDE_INTERRUPT_KEY`].
pub const CLAUDE_INTERRUPT_BYTES: &[u8] = b"\x1b";

/// Supported effort levels accepted by `claude --effort <level>`.
pub const CLAUDE_SUPPORTED_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Maximum byte length of a single JSONL line normalized from a Claude transcript.
const TRANSCRIPT_LINE_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Concrete [`VendorHostAdapter`] for Anthropic Claude Code (`claude`).
#[derive(Debug, Clone, Default)]
pub struct ClaudeHostAdapter {
    runtime_port: Option<Arc<dyn RuntimePort>>,
}

impl ClaudeHostAdapter {
    pub fn new() -> Self {
        Self { runtime_port: None }
    }

    pub fn with_runtime_port(runtime_port: Arc<dyn RuntimePort>) -> Self {
        Self {
            runtime_port: Some(runtime_port),
        }
    }

    /// Key sequence used to interrupt an in-flight turn (`"Escape"`).
    pub fn interrupt_key(&self) -> &'static str {
        CLAUDE_INTERRUPT_KEY
    }

    /// Resolve the `claude` binary path using `FL_WIRE_CLAUDE_BIN`, `<home>/.local/bin/claude`,
    /// or `"claude"`.
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
            .filter(|p| !p.as_os_str().is_empty())
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

    /// Locate the exact `<session-uuid>.jsonl` transcript file under `<home_dir>/.claude/projects/`
    /// (or `<home_dir>/.claude/archived/`).
    ///
    /// Never scans by newest `mtime`; only matches the exact `<vendor_session_id>.jsonl` filename.
    pub fn find_session_transcript_path(
        &self,
        home_dir: &Path,
        vendor_session_id: &str,
    ) -> Result<Option<PathBuf>, VendorAdapterError> {
        let session_id = validate_required_session_id(vendor_session_id, "find_transcript")?;
        let target_file = format!("{session_id}.jsonl");
        let claude_dir = home_dir.join(".claude");
        for bucket in ["projects", "archived"] {
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

    /// Read and normalize an exact-session `.jsonl` transcript file from disk, advancing
    /// incrementally from `after_cursor`.
    pub fn normalize_session_transcript_file(
        &self,
        transcript_path: &Path,
        after_cursor: Option<&str>,
    ) -> Result<TranscriptNormalizeReport, VendorAdapterError> {
        let raw =
            fs::read_to_string(transcript_path).map_err(|e| VendorAdapterError::Transport {
                vendor: "claude".into(),
                reason: format!(
                    "failed to read Claude transcript {}: {e}",
                    transcript_path.display()
                ),
            })?;
        self.normalize_transcript(&raw, after_cursor)
    }
}

/// Split a trailing context-window tag (`[1m]`, `[200k]`: one to four ASCII digits followed by
/// lowercase `k` or `m`) off a model ID.
pub fn split_window_tag(id: &str) -> Option<(&str, &str)> {
    let open = id.rfind('[')?;
    let (base, tag) = id.split_at(open);
    let inner = tag.strip_prefix('[')?.strip_suffix(']')?;
    let bytes = inner.as_bytes();
    let (&unit, digits) = bytes.split_last()?;
    let valid = matches!(unit, b'k' | b'm')
        && (1..=4).contains(&digits.len())
        && digits.iter().all(u8::is_ascii_digit);
    valid.then_some((base, tag))
}

/// `true` for `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`.
pub fn is_plain_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Normalize a requested Claude model ID, allowing plain model IDs with an optional trailing
/// `[1m]` or `[200k]` context-window tag (for example `opus[1m]`, `claude-sonnet-4-5[200k]`).
pub fn normalize_plain_model(requested: Option<&str>) -> Option<String> {
    let id = requested?.trim();
    if id.is_empty() {
        return None;
    }
    let base = split_window_tag(id).map_or(id, |(base, _)| base);
    is_plain_model_id(base).then(|| id.to_string())
}

/// Normalize a requested thinking-effort token when it is a safe plain token (`<= 32` chars).
pub fn plain_thinking_level(level: Option<&str>) -> Option<String> {
    let trimmed = level?.trim();
    if trimmed.is_empty() || trimmed.len() > 32 || !is_plain_model_id(trimmed) {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if CLAUDE_SUPPORTED_EFFORTS.contains(&lower.as_str()) {
        Some(lower)
    } else {
        Some(trimmed.to_string())
    }
}

/// Format a positional prompt argument so leading-dash prompts cannot be parsed as CLI flags.
pub fn safe_positional_prompt(prompt: &str) -> String {
    if prompt.starts_with('-') {
        format!("Prompt: {prompt}")
    } else {
        prompt.to_string()
    }
}

/// Validate that a session ID is non-empty and safe to use as a CLI argument and filename stem
/// (no path separators, traversal, whitespace, control characters, or leading dash).
fn is_valid_claude_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn validate_required_session_id<'a>(
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

/// Convert a persona identifier (`"ferris"`, `"kayla"`) into a capitalized display name
/// (`"Ferris"`, `"Kayla"`) for `claude --name <DisplayName>`.
pub fn display_name_for_persona(persona: &str) -> Option<String> {
    let trimmed = persona.trim();
    if trimmed.is_empty() || !is_plain_model_id(trimmed) {
        return None;
    }
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    let mut out = String::with_capacity(trimmed.len());
    out.push(first.to_ascii_uppercase());
    out.push_str(chars.as_str());
    Some(out)
}

fn build_claude_launch_env(persona: &str, home_dir: Option<&Path>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let trimmed_persona = persona.trim();
    if !trimmed_persona.is_empty() {
        env.push(("AGENT_PERSONA".to_string(), trimmed_persona.to_string()));
    }
    let resolved_home = home_dir
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    if let Some(h) = resolved_home {
        let local_bin = h.join(".local").join("bin");
        let current = std::env::var("PATH").unwrap_or_default();
        let path_val = if std::env::split_paths(&current).any(|entry| entry == local_bin) {
            current
        } else if current.is_empty() {
            format!("{}:/usr/local/bin:/usr/bin:/bin", local_bin.display())
        } else {
            format!("{}:{current}", local_bin.display())
        };
        env.push(("PATH".to_string(), path_val));
    }
    env
}

fn claude_startup_rules() -> Vec<VendorStartupPromptRule> {
    vec![VendorStartupPromptRule::new(
        CLAUDE_AUTO_MODE_PROMPT,
        "Escape",
        false,
        "Dismiss Claude Code first-launch auto-mode environment modal with Escape",
    )]
}

/// Strip control characters other than `\n` and `\t` before bracketed-paste terminal delivery.
fn sanitize_for_paste(text: &str) -> String {
    text.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

/// Format sanitized turn text into a bracketed paste sequence followed by carriage return.
fn format_bracketed_paste_turn(text: &str) -> Vec<u8> {
    let clean = sanitize_for_paste(text);
    format!("\x1b[200~{clean}\x1b[201~\r").into_bytes()
}

fn parse_cursor_line(after_cursor: Option<&str>) -> Result<usize, VendorAdapterError> {
    let Some(raw) = after_cursor.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(0);
    };
    let digits = raw.strip_prefix("line:").unwrap_or(raw);
    digits
        .parse::<usize>()
        .map_err(|_| VendorAdapterError::Protocol {
            vendor: "claude".into(),
            reason: format!("invalid transcript cursor `{raw}`"),
        })
}

fn extract_claude_message_content(val: &Value) -> String {
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

fn is_claude_skipped_record_type(record_type: &str) -> bool {
    record_type.starts_with("api-request")
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
                | "ai-title"
                | "tools-used"
                | "summary"
        )
}

fn classify_claude_turn_role(val: &Value) -> Option<String> {
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

impl VendorHostAdapter for ClaudeHostAdapter {
    fn vendor_id(&self) -> &str {
        "claude"
    }

    fn default_binary(&self) -> &str {
        "claude"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_CLAUDE_BIN")
    }

    fn prepare_launch(
        &self,
        request: &VendorLaunchRequest,
    ) -> Result<VendorLaunchPlan, VendorAdapterError> {
        let cfg =
            self.validate_config(request.model.as_deref(), request.thinking_effort.as_deref())?;
        let binary = self.resolve_binary(request.home_dir.as_deref());

        // Place `--remote-control` before `--enable-auto-mode` so `--remote-control [name]`
        // in Commander.js is always followed by a `--` flag and never consumes a trailing
        // positional `<prompt>` when `model` and `thinking_effort` are both `None`.
        let mut argv = vec![binary.clone()];
        if request.remote_control {
            argv.push("--remote-control".to_string());
        }
        argv.push("--enable-auto-mode".to_string());

        if let Some(raw_sid) = request
            .binding
            .vendor_session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let sid = validate_required_session_id(raw_sid, "prepare_launch")?;
            argv.extend(["--session-id".to_string(), sid.to_string()]);
            if let Some(display_name) = display_name_for_persona(&request.persona) {
                argv.extend(["--name".to_string(), display_name]);
            }
        }

        if let Some(model) = cfg.normalized_model {
            argv.extend(["--model".to_string(), model]);
        }
        if let Some(effort) = cfg.normalized_thinking_effort {
            argv.extend(["--effort".to_string(), effort]);
        }
        if !request.initial_prompt.trim().is_empty() {
            argv.push(safe_positional_prompt(&request.initial_prompt));
        }

        Ok(VendorLaunchPlan {
            vendor_id: self.vendor_id().to_string(),
            binary,
            argv,
            env: build_claude_launch_env(&request.persona, request.home_dir.as_deref()),
            cwd: request.cwd.clone(),
            remote_control: request.remote_control,
            startup_rules: claude_startup_rules(),
        })
    }

    fn prepare_resume(
        &self,
        request: &AttachOrResumeRequest,
    ) -> Result<VendorLaunchPlan, VendorAdapterError> {
        let session_id =
            validate_required_session_id(&request.vendor_session_id, "prepare_resume")?;
        let cfg =
            self.validate_config(request.model.as_deref(), request.thinking_effort.as_deref())?;
        let binary = self.resolve_binary(request.home_dir.as_deref());

        let mut argv = vec![binary.clone()];
        if request.remote_control {
            argv.push("--remote-control".to_string());
        }
        argv.push("--enable-auto-mode".to_string());
        argv.extend(["--resume".to_string(), session_id.to_string()]);

        if let Some(persona) = request.persona.as_deref()
            && let Some(display_name) = display_name_for_persona(persona)
        {
            argv.extend(["--name".to_string(), display_name]);
        }
        if let Some(model) = cfg.normalized_model {
            argv.extend(["--model".to_string(), model]);
        }
        if let Some(effort) = cfg.normalized_thinking_effort {
            argv.extend(["--effort".to_string(), effort]);
        }

        let persona = request.persona.as_deref().unwrap_or("");
        Ok(VendorLaunchPlan {
            vendor_id: self.vendor_id().to_string(),
            binary,
            argv,
            env: build_claude_launch_env(persona, request.home_dir.as_deref()),
            cwd: request.cwd.clone(),
            remote_control: request.remote_control,
            startup_rules: claude_startup_rules(),
        })
    }

    fn attach_or_resume(
        &self,
        request: &AttachOrResumeRequest,
    ) -> Result<AttachOrResumeOutcome, VendorAdapterError> {
        let _plan = self.prepare_resume(request)?;
        let session_id = request.vendor_session_id.trim().to_string();
        let readiness = self.probe_readiness(
            &request.binding,
            Some(&session_id),
            request.control_endpoint.as_deref(),
        )?;
        Ok(AttachOrResumeOutcome::Resumed {
            vendor_session_id: session_id,
            readiness,
        })
    }

    fn probe_readiness(
        &self,
        binding: &AdapterIdentityBinding,
        vendor_session_id: Option<&str>,
        control_endpoint: Option<&str>,
    ) -> Result<AdapterReadiness, VendorAdapterError> {
        if let Some(port) = &self.runtime_port {
            let port_state = port.session_readiness(binding)?;
            if matches!(
                port_state,
                AdapterReadiness::Exited | AdapterReadiness::Exiting
            ) {
                return Ok(port_state);
            }
        }

        // Check explicit session record or transcript path if provided in `control_endpoint`.
        if let Some(ep) = control_endpoint.map(str::trim).filter(|s| !s.is_empty()) {
            let path = Path::new(ep);
            if path.extension().and_then(|s| s.to_str()) == Some("json")
                && let Ok(raw) = fs::read_to_string(path)
                && let Ok(val) = serde_json::from_str::<Value>(&raw)
                && let Some(status) = val.get("status").and_then(Value::as_str)
            {
                return match status.trim().to_ascii_lowercase().as_str() {
                    "idle" => Ok(AdapterReadiness::ReadyIdle),
                    "busy" | "running" | "thinking" => Ok(AdapterReadiness::Busy),
                    _ => Ok(AdapterReadiness::Unknown),
                };
            }
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl")
                && path.is_file()
                && let Ok(report) = self.normalize_session_transcript_file(path, None)
            {
                return Ok(match report.turns.last().map(|t| t.role.as_str()) {
                    Some("user" | "tool") => AdapterReadiness::Busy,
                    Some("assistant") => AdapterReadiness::ReadyIdle,
                    _ => AdapterReadiness::Starting,
                });
            }
        }

        if let Some(port) = &self.runtime_port {
            return port.session_readiness(binding);
        }

        let _ = vendor_session_id;
        Ok(AdapterReadiness::Unknown)
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
        if request.binding.host_session.trim().is_empty() {
            return TurnDeliveryOutcome::Rejected {
                reason: "Claude submit_turn requires a non-empty host_session binding".into(),
            };
        }
        if request.text.trim().is_empty() {
            return TurnDeliveryOutcome::Rejected {
                reason: "Claude submit_turn requires non-empty turn text".into(),
            };
        }
        let Some(port) = &self.runtime_port else {
            return TurnDeliveryOutcome::Rejected {
                reason: "Claude submit_turn requires an injected RuntimePort".into(),
            };
        };
        match port.session_readiness(&request.binding) {
            Ok(AdapterReadiness::Exited | AdapterReadiness::Exiting) => {
                return TurnDeliveryOutcome::Rejected {
                    reason: format!(
                        "host session `{}` is not running",
                        request.binding.host_session
                    ),
                };
            }
            Ok(AdapterReadiness::Busy) => {
                return TurnDeliveryOutcome::Rejected {
                    reason: format!(
                        "host session `{}` is busy; leaving turn pending",
                        request.binding.host_session
                    ),
                };
            }
            Ok(_) => {}
            Err(err) => {
                return TurnDeliveryOutcome::Rejected {
                    reason: err.to_string(),
                };
            }
        }

        let payload = format_bracketed_paste_turn(&request.text);
        match port.write_bytes(&request.binding, &payload) {
            Ok(()) => TurnDeliveryOutcome::Accepted { turn_id: None },
            Err(
                err @ (VendorAdapterError::GenerationMismatch { .. }
                | VendorAdapterError::MissingSessionIdentity { .. }
                | VendorAdapterError::InvalidConfig { .. }),
            ) => TurnDeliveryOutcome::Rejected {
                reason: err.to_string(),
            },
            Err(err) => TurnDeliveryOutcome::OutcomeUnknown {
                stage: "runtime_port_write".into(),
                message: err.to_string(),
            },
        }
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
        if request.binding.host_session.trim().is_empty() {
            return TurnDeliveryOutcome::Rejected {
                reason: "Claude steer_turn requires a non-empty host_session binding".into(),
            };
        }
        if request.text.trim().is_empty() {
            return TurnDeliveryOutcome::Rejected {
                reason: "Claude steer_turn requires non-empty turn text".into(),
            };
        }
        let Some(port) = &self.runtime_port else {
            return TurnDeliveryOutcome::Rejected {
                reason: "Claude steer_turn requires an injected RuntimePort".into(),
            };
        };
        let payload = format_bracketed_paste_turn(&request.text);
        match port.write_bytes(&request.binding, &payload) {
            Ok(()) => TurnDeliveryOutcome::Accepted {
                turn_id: request.vendor_turn_id.clone(),
            },
            Err(
                err @ (VendorAdapterError::GenerationMismatch { .. }
                | VendorAdapterError::MissingSessionIdentity { .. }
                | VendorAdapterError::InvalidConfig { .. }),
            ) => TurnDeliveryOutcome::Rejected {
                reason: err.to_string(),
            },
            Err(err) => TurnDeliveryOutcome::OutcomeUnknown {
                stage: "runtime_port_write".into(),
                message: err.to_string(),
            },
        }
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
        if request.binding.host_session.trim().is_empty() {
            return TurnDeliveryOutcome::Rejected {
                reason: "Claude cancel_turn requires a non-empty host_session binding".into(),
            };
        }
        let Some(port) = &self.runtime_port else {
            return TurnDeliveryOutcome::Rejected {
                reason: "Claude cancel_turn requires an injected RuntimePort".into(),
            };
        };
        // Send Claude Code's native interrupt key (`Escape`, `\x1b`), never `C-c`.
        match port.write_bytes(&request.binding, CLAUDE_INTERRUPT_BYTES) {
            Ok(()) => TurnDeliveryOutcome::Accepted {
                turn_id: request.vendor_turn_id.clone(),
            },
            Err(
                err @ (VendorAdapterError::GenerationMismatch { .. }
                | VendorAdapterError::MissingSessionIdentity { .. }
                | VendorAdapterError::InvalidConfig { .. }),
            ) => TurnDeliveryOutcome::Rejected {
                reason: err.to_string(),
            },
            Err(err) => TurnDeliveryOutcome::OutcomeUnknown {
                stage: "runtime_port_interrupt".into(),
                message: err.to_string(),
            },
        }
    }

    fn supported_thinking_efforts(&self) -> &[&str] {
        CLAUDE_SUPPORTED_EFFORTS
    }

    fn validate_config(
        &self,
        model: Option<&str>,
        thinking_effort: Option<&str>,
    ) -> Result<VendorConfigSpec, VendorAdapterError> {
        let normalized_model = match model.map(str::trim).filter(|s| !s.is_empty()) {
            None => None,
            Some(raw) => Some(normalize_plain_model(Some(raw)).ok_or_else(|| {
                VendorAdapterError::InvalidConfig {
                    vendor: "claude".into(),
                    reason: format!(
                        "model `{raw}` is not a valid plain Claude model ID \
                         (expected plain model ID with optional [1m] or [200k] window tag)"
                    ),
                }
            })?),
        };

        let normalized_thinking_effort =
            match thinking_effort.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(raw) => Some(plain_thinking_level(Some(raw)).ok_or_else(|| {
                    VendorAdapterError::InvalidConfig {
                        vendor: "claude".into(),
                        reason: format!("thinking_effort `{raw}` is not a valid plain token"),
                    }
                })?),
            };

        Ok(VendorConfigSpec {
            normalized_model,
            normalized_thinking_effort,
            model_scope: ConfigEffectiveScope::NextLaunch,
            thinking_effort_scope: ConfigEffectiveScope::NextLaunch,
        })
    }

    fn normalize_transcript(
        &self,
        raw_payload: &str,
        after_cursor: Option<&str>,
    ) -> Result<TranscriptNormalizeReport, VendorAdapterError> {
        let min_line = parse_cursor_line(after_cursor)?;
        let mut turns = Vec::new();
        let mut last_cursor = after_cursor.map(str::to_string);

        for (idx, line) in raw_payload.lines().enumerate() {
            let line_num = idx + 1;
            if line_num <= min_line {
                continue;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.len() > TRANSCRIPT_LINE_MAX_BYTES {
                continue;
            }
            let cursor = format!("line:{line_num}");
            let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            last_cursor = Some(cursor.clone());

            let Some(role) = classify_claude_turn_role(&val) else {
                continue;
            };
            let content = extract_claude_message_content(&val);
            if content.is_empty() {
                continue;
            }
            let turn_id = val
                .get("uuid")
                .or_else(|| val.pointer("/message/id"))
                .or_else(|| val.get("id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("claude-turn-{line_num}"));

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

    fn stop(&self, target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
        if let Some(port) = &self.runtime_port {
            return port.stop_session(&target.binding);
        }
        Ok(TeardownOutcome::Completed)
    }

    fn detach(&self, _target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
        Ok(TeardownOutcome::Completed)
    }

    fn archive(&self, target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
        let raw_sid = target.vendor_session_id.as_deref().unwrap_or("");
        let session_id = validate_required_session_id(raw_sid, "archive")?;

        let Some(home) = target
            .home_dir
            .as_deref()
            .filter(|p| !p.as_os_str().is_empty())
        else {
            return Ok(TeardownOutcome::AlreadyAbsent);
        };

        let projects_dir = home.join(".claude").join("projects");
        let archived_root = home.join(".claude").join("archived");
        let target_file = format!("{session_id}.jsonl");
        let mut archived_any = false;

        if let Ok(entries) = fs::read_dir(&projects_dir) {
            for entry in entries.flatten() {
                let proj_path = entry.path();
                if !proj_path.is_dir() {
                    continue;
                }
                let src = proj_path.join(&target_file);
                if !src.is_file() {
                    continue;
                }
                let slug = entry.file_name();
                let dst_dir = archived_root.join(slug);
                fs::create_dir_all(&dst_dir).map_err(|e| VendorAdapterError::Transport {
                    vendor: "claude".into(),
                    reason: format!(
                        "failed to create archive directory {}: {e}",
                        dst_dir.display()
                    ),
                })?;
                let dst = dst_dir.join(&target_file);
                fs::rename(&src, &dst).map_err(|e| VendorAdapterError::Transport {
                    vendor: "claude".into(),
                    reason: format!(
                        "failed to archive Claude transcript {} to {}: {e}",
                        src.display(),
                        dst.display()
                    ),
                })?;
                archived_any = true;
            }
        }

        if archived_any {
            Ok(TeardownOutcome::Completed)
        } else {
            Ok(TeardownOutcome::AlreadyAbsent)
        }
    }

    fn delete(&self, target: &TeardownTarget) -> Result<TeardownOutcome, VendorAdapterError> {
        let raw_sid = target.vendor_session_id.as_deref().unwrap_or("");
        let session_id = validate_required_session_id(raw_sid, "delete")?;

        let Some(home) = target
            .home_dir
            .as_deref()
            .filter(|p| !p.as_os_str().is_empty())
        else {
            return Ok(TeardownOutcome::AlreadyAbsent);
        };

        let claude_dir = home.join(".claude");
        let target_file = format!("{session_id}.jsonl");
        let mut deleted_any = false;

        for bucket in ["projects", "archived"] {
            let root = claude_dir.join(bucket);
            let Ok(entries) = fs::read_dir(&root) else {
                continue;
            };
            for entry in entries.flatten() {
                let proj_path = entry.path();
                if !proj_path.is_dir() {
                    continue;
                }
                let candidate = proj_path.join(&target_file);
                if candidate.is_file() {
                    fs::remove_file(&candidate).map_err(|e| VendorAdapterError::Transport {
                        vendor: "claude".into(),
                        reason: format!(
                            "failed to remove Claude transcript {}: {e}",
                            candidate.display()
                        ),
                    })?;
                    deleted_any = true;
                }
            }
        }

        if deleted_any {
            Ok(TeardownOutcome::Completed)
        } else {
            Ok(TeardownOutcome::AlreadyAbsent)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use tempfile::TempDir;

    #[derive(Debug, Default)]
    struct FakeRuntimePort {
        readiness: Mutex<AdapterReadiness>,
        writes: Mutex<Vec<(AdapterIdentityBinding, Vec<u8>)>>,
        fail_writes_with: Mutex<Option<VendorAdapterError>>,
        stopped: Mutex<Vec<AdapterIdentityBinding>>,
    }

    impl FakeRuntimePort {
        fn with_readiness(state: AdapterReadiness) -> Arc<Self> {
            Arc::new(Self {
                readiness: Mutex::new(state),
                writes: Mutex::new(Vec::new()),
                fail_writes_with: Mutex::new(None),
                stopped: Mutex::new(Vec::new()),
            })
        }

        fn recorded_writes(&self) -> Vec<(AdapterIdentityBinding, Vec<u8>)> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl RuntimePort for FakeRuntimePort {
        fn session_readiness(
            &self,
            _binding: &AdapterIdentityBinding,
        ) -> Result<AdapterReadiness, VendorAdapterError> {
            Ok(*self.readiness.lock().unwrap())
        }

        fn write_bytes(
            &self,
            binding: &AdapterIdentityBinding,
            bytes: &[u8],
        ) -> Result<(), VendorAdapterError> {
            if let Some(err) = self.fail_writes_with.lock().unwrap().clone() {
                return Err(err);
            }
            self.writes
                .lock()
                .unwrap()
                .push((binding.clone(), bytes.to_vec()));
            Ok(())
        }

        fn capture_output(
            &self,
            _binding: &AdapterIdentityBinding,
            _max_bytes: usize,
        ) -> Result<String, VendorAdapterError> {
            Ok(String::new())
        }

        fn stop_session(
            &self,
            binding: &AdapterIdentityBinding,
        ) -> Result<TeardownOutcome, VendorAdapterError> {
            self.stopped.lock().unwrap().push(binding.clone());
            Ok(TeardownOutcome::Completed)
        }
    }

    fn sample_binding() -> AdapterIdentityBinding {
        AdapterIdentityBinding::new("claude:ferris", 7, "Ferris.claude", Some((5150, 12345)))
    }

    #[test]
    fn prepare_launch_places_remote_control_before_enable_auto_mode_and_binds_session_id() {
        let tmp = TempDir::new().unwrap();
        let adapter = ClaudeHostAdapter::new();

        // Case 1: model and thinking_effort are both None — `--enable-auto-mode` must still
        // sit immediately after `--remote-control` so Commander.js `--remote-control [name]`
        // never consumes the positional prompt.
        let req_minimal = VendorLaunchRequest {
            binding: sample_binding(),
            persona: "ferris".into(),
            model: None,
            thinking_effort: None,
            initial_prompt: "Hydrate and check roster".into(),
            cwd: Some(tmp.path().to_path_buf()),
            home_dir: Some(tmp.path().to_path_buf()),
            remote_control: true,
        };
        let plan_minimal = adapter.prepare_launch(&req_minimal).unwrap();
        assert_eq!(
            &plan_minimal.argv[1..],
            &[
                "--remote-control",
                "--enable-auto-mode",
                "Hydrate and check roster"
            ]
        );
        assert!(
            plan_minimal
                .env
                .iter()
                .any(|(k, v)| k == "AGENT_PERSONA" && v == "ferris"),
            "Claude launch env must carry AGENT_PERSONA"
        );
        assert_eq!(plan_minimal.startup_rules.len(), 1);
        assert_eq!(
            plan_minimal.startup_rules[0].pattern,
            CLAUDE_AUTO_MODE_PROMPT
        );
        assert_eq!(plan_minimal.startup_rules[0].response_keys, "Escape");
        assert!(!plan_minimal.startup_rules[0].warn_on_miss);

        // Case 2: explicit vendor_session_id binding, [1m] window tag on model, effort, and
        // leading-dash prompt.
        let session_uuid = "550e8400-e29b-41d4-a716-446655440000";
        let req_bound = VendorLaunchRequest {
            binding: sample_binding().with_vendor_session_id(session_uuid),
            persona: "ferris".into(),
            model: Some("opus[1m]".into()),
            thinking_effort: Some("high".into()),
            initial_prompt: "-v check status".into(),
            cwd: Some(tmp.path().to_path_buf()),
            home_dir: Some(tmp.path().to_path_buf()),
            remote_control: true,
        };
        let plan_bound = adapter.prepare_launch(&req_bound).unwrap();
        assert_eq!(
            &plan_bound.argv[1..],
            &[
                "--remote-control",
                "--enable-auto-mode",
                "--session-id",
                session_uuid,
                "--name",
                "Ferris",
                "--model",
                "opus[1m]",
                "--effort",
                "high",
                "Prompt: -v check status",
            ]
        );
        assert_eq!(plan_bound.cwd.as_deref(), Some(tmp.path()));
    }

    #[test]
    fn prepare_resume_requires_explicit_session_id_and_never_uses_continue() {
        let tmp = TempDir::new().unwrap();
        let adapter = ClaudeHostAdapter::new();

        let empty_req = AttachOrResumeRequest::new(sample_binding(), "   ");
        let err = adapter.prepare_resume(&empty_req).unwrap_err();
        assert!(
            matches!(err, VendorAdapterError::MissingSessionIdentity { .. }),
            "empty vendor_session_id must fail with MissingSessionIdentity, got: {err:?}"
        );

        let flag_injection_req = AttachOrResumeRequest::new(sample_binding(), "--continue");
        let err = adapter.prepare_resume(&flag_injection_req).unwrap_err();
        assert!(
            matches!(err, VendorAdapterError::InvalidConfig { .. }),
            "flag-like vendor_session_id must fail with InvalidConfig, got: {err:?}"
        );

        let uuid = "019d8f34-89cd-7e43-8a66-31f3b49788b7";
        let mut valid_req = AttachOrResumeRequest::new(sample_binding(), uuid);
        valid_req.persona = Some("kayla".into());
        valid_req.model = Some("claude-sonnet-4-5[200k]".into());
        valid_req.thinking_effort = Some("max".into());
        valid_req.cwd = Some(tmp.path().to_path_buf());
        valid_req.home_dir = Some(tmp.path().to_path_buf());

        let plan = adapter.prepare_resume(&valid_req).unwrap();
        assert_eq!(
            &plan.argv[1..],
            &[
                "--remote-control",
                "--enable-auto-mode",
                "--resume",
                uuid,
                "--name",
                "Kayla",
                "--model",
                "claude-sonnet-4-5[200k]",
                "--effort",
                "max",
            ]
        );
        assert!(
            !plan
                .argv
                .iter()
                .any(|arg| arg == "--continue" || arg == "-c")
        );
        assert_eq!(plan.cwd.as_deref(), Some(tmp.path()));
    }

    #[test]
    fn turn_delivery_and_escape_interrupt_route_through_runtime_port_with_generation_check() {
        let port = FakeRuntimePort::with_readiness(AdapterReadiness::ReadyIdle);
        let adapter = ClaudeHostAdapter::with_runtime_port(port.clone());
        let binding = sample_binding();

        // Stale generation is rejected pre-write.
        let stale_req = TurnDeliveryRequest {
            binding: binding.clone(),
            message_id: "msg-1".into(),
            vendor_session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            vendor_turn_id: None,
            expected_generation: 99,
            text: "Hello Ferris".into(),
            control_endpoint: None,
            home_dir: None,
        };
        assert!(adapter.submit_turn(&stale_req).is_rejected());
        assert!(port.recorded_writes().is_empty());

        // Matching generation writes bracketed paste payload.
        let ok_req = TurnDeliveryRequest {
            expected_generation: binding.generation,
            ..stale_req.clone()
        };
        assert!(adapter.submit_turn(&ok_req).is_accepted());
        let writes = port.recorded_writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].1, b"\x1b[200~Hello Ferris\x1b[201~\r");

        // Interrupt sends Escape (`\x1b`), never Ctrl-C (`\x03`).
        assert_eq!(adapter.interrupt_key(), "Escape");
        assert!(adapter.interrupt_turn(&ok_req).is_accepted());
        let writes = port.recorded_writes();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1].1, b"\x1b");

        // Transport failure during write returns OutcomeUnknown so callers never auto-retry.
        *port.fail_writes_with.lock().unwrap() = Some(VendorAdapterError::Transport {
            vendor: "claude".into(),
            reason: "broken pipe after partial write".into(),
        });
        assert!(adapter.submit_turn(&ok_req).is_outcome_unknown());
    }

    #[test]
    fn normalize_transcript_parses_claude_jsonl_and_resumes_from_cursor() {
        let adapter = ClaudeHostAdapter::new();
        let raw = [
            r#"{"type":"queue-operation","timestamp":"2026-11-08T21:00:00Z"}"#,
            r#"{"uuid":"u-1","type":"user","message":{"role":"user","content":"Check status"}}"#,
            r#"{"uuid":"a-1","type":"assistant","message":{"id":"msg_01","role":"assistant","content":[{"type":"thinking","thinking":"Checking..."},{"type":"text","text":"All systems nominal."}]}}"#,
            r#"{"uuid":"u-2","type":"user","isSidechain":true,"message":{"role":"user","content":"subagent prompt"}}"#,
            r#"{"uuid":"t-1","type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"exit 0"}]}}"#,
        ]
        .join("\n");

        let full = adapter.normalize_transcript(&raw, None).unwrap();
        assert_eq!(full.turns.len(), 3);
        assert_eq!(full.turns[0].turn_id, "u-1");
        assert_eq!(full.turns[0].role, "user");
        assert_eq!(full.turns[0].content, "Check status");
        assert_eq!(full.turns[0].source_cursor, "line:2");

        assert_eq!(full.turns[1].turn_id, "a-1");
        assert_eq!(full.turns[1].role, "assistant");
        assert_eq!(full.turns[1].content, "All systems nominal.");
        assert_eq!(full.turns[1].source_cursor, "line:3");

        assert_eq!(full.turns[2].turn_id, "t-1");
        assert_eq!(full.turns[2].role, "tool");
        assert_eq!(full.turns[2].content, "exit 0");
        assert_eq!(full.turns[2].source_cursor, "line:5");
        assert_eq!(full.next_cursor.as_deref(), Some("line:5"));

        // Incremental cursor resumption after line:2 returns only turns on lines > 2.
        let delta = adapter.normalize_transcript(&raw, Some("line:2")).unwrap();
        assert_eq!(delta.turns.len(), 2);
        assert_eq!(delta.turns[0].turn_id, "a-1");
        assert_eq!(delta.turns[1].turn_id, "t-1");
    }

    #[test]
    fn archive_and_delete_require_explicit_session_id_and_preserve_sibling_transcripts() {
        let tmp = TempDir::new().unwrap();
        let proj_dir = tmp
            .path()
            .join(".claude")
            .join("projects")
            .join("ferroloop");
        fs::create_dir_all(&proj_dir).unwrap();

        let owned_uuid = "11111111-2222-3333-4444-555555555555";
        let sibling_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let owned_file = proj_dir.join(format!("{owned_uuid}.jsonl"));
        let sibling_file = proj_dir.join(format!("{sibling_uuid}.jsonl"));
        fs::write(&owned_file, r#"{"uuid":"1","type":"user","content":"hi"}"#).unwrap();
        fs::write(
            &sibling_file,
            r#"{"uuid":"2","type":"user","content":"sibling"}"#,
        )
        .unwrap();

        let adapter = ClaudeHostAdapter::new();

        // Missing session ID is rejected for both archive and delete.
        let missing_target = TeardownTarget {
            binding: sample_binding(),
            vendor_session_id: None,
            control_endpoint: None,
            home_dir: Some(tmp.path().to_path_buf()),
        };
        assert!(matches!(
            adapter.archive(&missing_target),
            Err(VendorAdapterError::MissingSessionIdentity { .. })
        ));
        assert!(matches!(
            adapter.delete(&missing_target),
            Err(VendorAdapterError::MissingSessionIdentity { .. })
        ));

        // Archive moves only the owned session transcript and leaves the sibling untouched.
        let owned_target = TeardownTarget {
            binding: sample_binding(),
            vendor_session_id: Some(owned_uuid.into()),
            control_endpoint: None,
            home_dir: Some(tmp.path().to_path_buf()),
        };
        assert_eq!(
            adapter.archive(&owned_target).unwrap(),
            TeardownOutcome::Completed
        );
        assert!(
            !owned_file.exists(),
            "owned transcript must be moved out of projects/"
        );
        assert!(
            sibling_file.exists(),
            "sibling transcript in shared project dir must stay intact"
        );

        // Find still locates the archived transcript by exact UUID.
        let located = adapter
            .find_session_transcript_path(tmp.path(), owned_uuid)
            .unwrap()
            .expect("archived transcript should be locatable by exact UUID");
        assert!(located.exists());

        // Delete removes the archived owned transcript and still leaves sibling untouched.
        assert_eq!(
            adapter.delete(&owned_target).unwrap(),
            TeardownOutcome::Completed
        );
        assert!(!located.exists());
        assert!(sibling_file.exists());
        assert_eq!(
            adapter.delete(&owned_target).unwrap(),
            TeardownOutcome::AlreadyAbsent
        );
    }
}
