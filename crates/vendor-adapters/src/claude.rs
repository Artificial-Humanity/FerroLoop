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
    VendorStartupPromptRule, classify_claude_turn_role, estimate_claude_post_compaction_context,
    extract_claude_message_content, find_claude_active_session_transcript_path,
    find_claude_session_transcript_path, repair_claude_session_transcript_tail,
    validate_claude_session_id,
};
use serde_json::Value;

pub use fl_core::PostCompactionContext as ClaudePostCompactionContext;

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
        find_claude_session_transcript_path(home_dir, vendor_session_id, true)
    }

    /// Locate the active `<session-uuid>.jsonl` transcript file strictly under
    /// `<home_dir>/.claude/projects/*/` (deterministic sorted subdirectory order; never searches
    /// `.claude/archived/` and never scans by newest `mtime`).
    pub fn find_active_session_transcript_path(
        &self,
        home_dir: &Path,
        vendor_session_id: &str,
    ) -> Result<Option<PathBuf>, VendorAdapterError> {
        find_claude_active_session_transcript_path(home_dir, vendor_session_id)
    }

    /// Inspect only the final non-empty line of `<session-uuid>.jsonl` and truncate any torn
    /// trailing non-JSON write back to the last complete valid JSON line.
    ///
    /// Complete JSONL transcripts—including dangling `tool_use` turns (`stop_reason: "tool_use"`)
    /// and unarchived `SIGKILL` transcripts—are left 100% untouched (`Ok(0)`).
    pub fn repair_session_transcript_tail(
        &self,
        transcript_path: &Path,
    ) -> Result<usize, VendorAdapterError> {
        repair_claude_session_transcript_tail(transcript_path)
    }

    /// Estimate post-compaction active context tokens, conversational byte volume, latest real
    /// model ID, normalized turn count, and persisted Remote Control `bridgeSessionId`.
    pub fn estimate_post_compaction_context(
        &self,
        transcript_path: &Path,
    ) -> Result<ClaudePostCompactionContext, VendorAdapterError> {
        estimate_claude_post_compaction_context(transcript_path)
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

fn validate_required_session_id<'a>(
    raw: &'a str,
    operation: &str,
) -> Result<&'a str, VendorAdapterError> {
    validate_claude_session_id(raw, operation)
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
        // Place `--remote-control` before `--permission-mode <mode>` so `--remote-control [name]`
        // in Commander.js is always followed by a `--` flag and never consumes a trailing
        // positional `<prompt>` when `model` and `thinking_effort` are both `None`.
        let auto_approval = request
            .auto_approval
            .unwrap_or_else(|| fl_core::permissions::Vendor::Claude.default_auto_approval());
        let perms = fl_core::permissions::compile_posture(
            fl_core::permissions::Vendor::Claude,
            &fl_core::permissions::VendorFacts::new(fl_core::permissions::Transport::Pty),
            request.posture,
            auto_approval,
        )
        .map_err(|e| VendorAdapterError::PermissionRefused {
            vendor: self.vendor_id().to_string(),
            reason: format!("posture: {e}"),
        })?;
        let cfg =
            self.validate_config(request.model.as_deref(), request.thinking_effort.as_deref())?;
        let binary = self.resolve_binary(request.home_dir.as_deref());
        let mut argv = vec![binary.clone()];
        if request.remote_control {
            argv.push("--remote-control".to_string());
        }
        argv.extend(perms.launch_args);

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
        let auto_approval = request
            .auto_approval
            .unwrap_or_else(|| fl_core::permissions::Vendor::Claude.default_auto_approval());
        let perms = fl_core::permissions::compile_posture(
            fl_core::permissions::Vendor::Claude,
            &fl_core::permissions::VendorFacts::new(fl_core::permissions::Transport::Pty),
            request.posture,
            auto_approval,
        )
        .map_err(|e| VendorAdapterError::PermissionRefused {
            vendor: self.vendor_id().to_string(),
            reason: format!("posture: {e}"),
        })?;
        let session_id =
            validate_required_session_id(&request.vendor_session_id, "prepare_resume")?;
        let cfg =
            self.validate_config(request.model.as_deref(), request.thinking_effort.as_deref())?;
        let binary = self.resolve_binary(request.home_dir.as_deref());
        let mut argv = vec![binary.clone()];
        if request.remote_control {
            argv.push("--remote-control".to_string());
        }
        argv.extend(perms.launch_args);
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

    fn find_active_session_transcript_path(
        &self,
        home_dir: &Path,
        vendor_session_id: &str,
    ) -> Result<Option<PathBuf>, VendorAdapterError> {
        Self::find_active_session_transcript_path(self, home_dir, vendor_session_id)
    }

    fn repair_session_transcript_tail(
        &self,
        transcript_path: &Path,
    ) -> Result<usize, VendorAdapterError> {
        Self::repair_session_transcript_tail(self, transcript_path)
    }

    fn estimate_post_compaction_context(
        &self,
        transcript_path: &Path,
    ) -> Result<ClaudePostCompactionContext, VendorAdapterError> {
        Self::estimate_post_compaction_context(self, transcript_path)
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
    fn prepare_launch_places_remote_control_before_permission_mode_and_binds_session_id() {
        let tmp = TempDir::new().unwrap();
        let adapter = ClaudeHostAdapter::new();

        // Case 1: model and thinking_effort are both None — `--permission-mode` must still
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
            posture: fl_core::permissions::Posture::Auto,
            auto_approval: None,
        };
        let plan_minimal = adapter.prepare_launch(&req_minimal).unwrap();
        assert_eq!(
            &plan_minimal.argv[1..],
            &[
                "--remote-control",
                "--permission-mode",
                "auto",
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
            posture: fl_core::permissions::Posture::Auto,
            auto_approval: None,
        };
        let plan_bound = adapter.prepare_launch(&req_bound).unwrap();
        assert_eq!(
            &plan_bound.argv[1..],
            &[
                "--remote-control",
                "--permission-mode",
                "auto",
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
                "--permission-mode",
                "auto",
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

    #[test]
    fn find_active_session_transcript_path_ignores_archived_and_validates_session_id() {
        let tmp = TempDir::new().unwrap();
        let adapter = ClaudeHostAdapter::new();

        assert!(matches!(
            adapter.find_active_session_transcript_path(tmp.path(), "   "),
            Err(VendorAdapterError::MissingSessionIdentity { .. })
        ));
        for bad in ["../escape", "nested/id", "--continue", "-bad"] {
            assert!(
                matches!(
                    adapter.find_active_session_transcript_path(tmp.path(), bad),
                    Err(VendorAdapterError::InvalidConfig { .. })
                ),
                "expected InvalidConfig for {bad:?}"
            );
        }

        let uuid = "4d9bd978-abdb-432c-9f25-e002b9b3ae61";
        assert_eq!(
            adapter
                .find_active_session_transcript_path(tmp.path(), uuid)
                .unwrap(),
            None
        );

        let archived_dir = tmp
            .path()
            .join(".claude")
            .join("archived")
            .join("ferroloop");
        fs::create_dir_all(&archived_dir).unwrap();
        let archived_file = archived_dir.join(format!("{uuid}.jsonl"));
        fs::write(
            &archived_file,
            r#"{"uuid":"1","type":"user","content":"archived"}"#,
        )
        .unwrap();

        // Present only in archived/: active lookup MUST return None while general lookup finds it.
        assert_eq!(
            adapter
                .find_active_session_transcript_path(tmp.path(), uuid)
                .unwrap(),
            None
        );
        assert_eq!(
            adapter
                .find_session_transcript_path(tmp.path(), uuid)
                .unwrap(),
            Some(archived_file)
        );

        // Present in projects/: deterministic sorted project directory selection.
        let proj_b = tmp.path().join(".claude").join("projects").join("z-proj");
        let proj_a = tmp.path().join(".claude").join("projects").join("a-proj");
        fs::create_dir_all(&proj_b).unwrap();
        fs::create_dir_all(&proj_a).unwrap();
        let file_b = proj_b.join(format!("{uuid}.jsonl"));
        let file_a = proj_a.join(format!("{uuid}.jsonl"));
        fs::write(&file_b, r#"{"uuid":"b","type":"user","content":"b"}"#).unwrap();
        fs::write(&file_a, r#"{"uuid":"a","type":"user","content":"a"}"#).unwrap();

        assert_eq!(
            adapter
                .find_active_session_transcript_path(tmp.path(), uuid)
                .unwrap(),
            Some(file_a)
        );
    }

    #[test]
    fn repair_session_transcript_tail_preserves_complete_dangling_tool_use_and_sigkill_transcripts()
    {
        let tmp = TempDir::new().unwrap();
        let adapter = ClaudeHostAdapter::new();

        // 1. Complete JSONL ending with a dangling tool_use + SIGKILL metadata (no Archive record).
        let dangling_path = tmp.path().join("dangling.jsonl");
        let complete_lines = concat!(
            r#"{"type":"bridge-session","sessionId":"4d9bd978-abdb-432c-9f25-e002b9b3ae61","bridgeSessionId":"cse_01SP4ortBQU3hQGNi4NtM8pN"}"#,
            "\n",
            r#"{"uuid":"u-1","type":"user","message":{"role":"user","content":"Run sleep"}}"#,
            "\n",
            r#"{"uuid":"a-1","type":"assistant","message":{"model":"claude-opus-5-5","role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"sleep 25"}}]}}"#,
            "\n"
        );
        fs::write(&dangling_path, complete_lines).unwrap();
        assert_eq!(
            adapter
                .repair_session_transcript_tail(&dangling_path)
                .unwrap(),
            0
        );
        assert_eq!(fs::read(&dangling_path).unwrap(), complete_lines.as_bytes());

        // 2. Complete JSONL without a trailing newline must also remain byte-for-byte untouched.
        let no_nl_path = tmp.path().join("no_nl.jsonl");
        let no_nl = complete_lines.trim_end_matches('\n');
        fs::write(&no_nl_path, no_nl).unwrap();
        assert_eq!(
            adapter.repair_session_transcript_tail(&no_nl_path).unwrap(),
            0
        );
        assert_eq!(fs::read(&no_nl_path).unwrap(), no_nl.as_bytes());

        // 3. Torn trailing non-JSON fragment (including torn UTF-8 bytes) is truncated back to the
        //    byte offset immediately after the last valid newline-terminated JSON record.
        let torn_path = tmp.path().join("torn.jsonl");
        let mut torn_bytes = complete_lines.as_bytes().to_vec();
        let fragment = b"{\"type\":\"assistant\",\"message\":{\"content\":\"\xe2\x97";
        torn_bytes.extend_from_slice(fragment);
        fs::write(&torn_path, &torn_bytes).unwrap();

        let removed = adapter.repair_session_transcript_tail(&torn_path).unwrap();
        assert_eq!(removed, fragment.len());
        assert_eq!(fs::read(&torn_path).unwrap(), complete_lines.as_bytes());

        // 4. Transcript with zero valid JSON lines truncates to 0 bytes and returns original_len.
        let all_bad_path = tmp.path().join("all_bad.jsonl");
        let bad_payload = b"{\"type\":\"user\",\"message\":";
        fs::write(&all_bad_path, bad_payload).unwrap();
        assert_eq!(
            adapter
                .repair_session_transcript_tail(&all_bad_path)
                .unwrap(),
            bad_payload.len()
        );
        assert!(fs::read(&all_bad_path).unwrap().is_empty());
    }

    #[test]
    fn estimate_post_compaction_context_computes_tokens_bytes_model_and_bridge_session() {
        let tmp = TempDir::new().unwrap();
        let adapter = ClaudeHostAdapter::new();

        // Case 1: Uncompacted SIGKILL transcript with bridge-session, telemetry bloat, sidechains,
        // and a trailing <synthetic> 0-token interrupted-turn recovery record.
        let uncompacted_path = tmp.path().join("uncompacted.jsonl");
        let user_line = r#"{"uuid":"u-1","type":"user","isSidechain":false,"message":{"role":"user","content":"Inspect workspace"}}"#;
        let asst_line = r#"{"uuid":"a-1","type":"assistant","isSidechain":false,"message":{"model":"claude-opus-5-5","role":"assistant","usage":{"input_tokens":2,"cache_creation_input_tokens":974,"cache_read_input_tokens":387420,"output_tokens":612},"content":[{"type":"text","text":"Workspace clean."}]}}"#;
        let synth_tool = r#"{"uuid":"u-2","type":"user","isSidechain":false,"toolDenialKind":"interrupted","message":{"role":"user","content":[{"type":"tool_result","is_error":true,"tool_use_id":"toolu_01","content":"[Tool call interrupted]"}]}}"#;
        let synth_asst = r#"{"uuid":"a-2","type":"assistant","isSidechain":false,"message":{"model":"<synthetic>","role":"assistant","usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[{"type":"text","text":"No response requested."}]}}"#;
        let uncompacted = [
            r#"{"type":"bridge-session","sessionId":"4d9bd978-abdb-432c-9f25-e002b9b3ae61","bridgeSessionId":"cse_01SP4ortBQU3hQGNi4NtM8pN"}"#,
            r#"{"type":"attachment","blob":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}"#,
            r#"{"type":"api-request-shape","payload":"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"}"#,
            r#"{"type":"api-request","payload":"CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC"}"#,
            r#"{"type":"queue-operation","timestamp":"2026-11-09T20:00:00Z"}"#,
            r#"{"type":"cost-state","costUSD":1.25}"#,
            r#"{"type":"file-history-snapshot","files":[]}"#,
            user_line,
            asst_line,
            r#"{"uuid":"sc-1","type":"assistant","isSidechain":true,"message":{"model":"claude-haiku-4-5","role":"assistant","usage":{"input_tokens":999999,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[{"type":"text","text":"sidechain"}]}}"#,
            synth_tool,
            synth_asst,
        ]
        .join("\n");
        fs::write(&uncompacted_path, &uncompacted).unwrap();

        let ctx = adapter
            .estimate_post_compaction_context(&uncompacted_path)
            .unwrap();
        assert_eq!(ctx.turn_count, 4);
        assert_eq!(ctx.latest_model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(ctx.active_context_tokens, Some(388_396));
        assert_eq!(
            ctx.post_compact_conversational_bytes,
            (user_line.len() + asst_line.len() + synth_tool.len() + synth_asst.len()) as u64
        );
        assert_eq!(
            ctx.bridge_session_id.as_deref(),
            Some("cse_01SP4ortBQU3hQGNi4NtM8pN")
        );
        assert!(ctx.has_persisted_bridge_session());

        // Case 2: Compacted transcript right after compact_boundary falls back to postTokens and
        // drops pre-compaction bytes/tokens while keeping bridge_session_id and latest_model.
        let compacted_path = tmp.path().join("compacted.jsonl");
        let compact_summary = r#"{"uuid":"cs-1","type":"user","isCompactSummary":true,"message":{"role":"user","content":"Compacted summary"}}"#;
        let compacted = [
            r#"{"type":"bridge-session","sessionId":"032bc4ff-1451-486f-95ec-69215c9773bc","bridgeSessionId":"cse_compacted_01"}"#,
            r#"{"uuid":"pre-u","type":"user","message":{"role":"user","content":"Huge pre-compact prompt"}}"#,
            r#"{"uuid":"pre-a","type":"assistant","message":{"model":"claude-sonnet-5-5","role":"assistant","usage":{"input_tokens":10,"cache_creation_input_tokens":1000,"cache_read_input_tokens":850000},"content":[{"type":"text","text":"Pre-compact reply"}]}}"#,
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto","preTokens":851010,"postTokens":21769,"cumulativeDroppedTokens":2818270}}"#,
            compact_summary,
        ]
        .join("\n");
        fs::write(&compacted_path, &compacted).unwrap();

        let ctx_post_boundary = adapter
            .estimate_post_compaction_context(&compacted_path)
            .unwrap();
        assert_eq!(ctx_post_boundary.turn_count, 3);
        assert_eq!(
            ctx_post_boundary.latest_model.as_deref(),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(ctx_post_boundary.active_context_tokens, Some(21_769));
        assert_eq!(
            ctx_post_boundary.post_compact_conversational_bytes,
            compact_summary.len() as u64
        );
        assert_eq!(
            ctx_post_boundary.bridge_session_id.as_deref(),
            Some("cse_compacted_01")
        );

        // Case 3: Real assistant turn after compact_boundary supersedes postTokens.
        let post_asst = r#"{"uuid":"post-a","type":"assistant","message":{"model":"claude-sonnet-5-5","role":"assistant","usage":{"input_tokens":2,"cache_creation_input_tokens":2085,"cache_read_input_tokens":893089},"content":[{"type":"text","text":"Post-compact reply"}]}}"#;
        let compacted_grown = format!("{compacted}\n{post_asst}\n");
        fs::write(&compacted_path, &compacted_grown).unwrap();

        let ctx_grown = adapter
            .estimate_post_compaction_context(&compacted_path)
            .unwrap();
        assert_eq!(ctx_grown.turn_count, 4);
        assert_eq!(ctx_grown.active_context_tokens, Some(895_176));
        assert_eq!(
            ctx_grown.post_compact_conversational_bytes,
            (compact_summary.len() + post_asst.len()) as u64
        );
    }

    #[test]
    fn vendor_host_adapter_trait_object_exposes_transcript_preflight_repair_and_estimation() {
        let tmp = TempDir::new().unwrap();
        let registry = crate::default_vendor_host_adapter_registry();
        let adapter: Arc<dyn VendorHostAdapter> =
            registry.lookup("claude").expect("claude host adapter");

        let uuid = "77777777-8888-9999-aaaa-bbbbbbbbbbbb";
        let proj_dir = tmp
            .path()
            .join(".claude")
            .join("projects")
            .join("ferroloop");
        fs::create_dir_all(&proj_dir).unwrap();
        let tx_path = proj_dir.join(format!("{uuid}.jsonl"));
        let valid_line = r#"{"uuid":"a-1","type":"assistant","message":{"model":"claude-sonnet-4@20250514","role":"assistant","usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":1000},"content":[{"type":"text","text":"Ready"}]}}"#;
        fs::write(&tx_path, format!("{valid_line}\n{{\"torn\":")).unwrap();

        let located = adapter
            .find_active_session_transcript_path(tmp.path(), uuid)
            .unwrap()
            .expect("active transcript path via VendorHostAdapter");
        assert_eq!(located, tx_path);

        let removed = adapter.repair_session_transcript_tail(&located).unwrap();
        assert_eq!(removed, b"{\"torn\":".len());

        let ctx = adapter.estimate_post_compaction_context(&located).unwrap();
        assert_eq!(ctx.turn_count, 1);
        assert_eq!(
            ctx.latest_model.as_deref(),
            Some("claude-sonnet-4@20250514")
        );
        assert_eq!(ctx.active_context_tokens, Some(1010));
    }
}
