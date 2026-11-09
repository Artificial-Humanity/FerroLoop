//! Unified 7-vendor CLI adapter/wrapper interface and `fl-host` session mapping.
//!
//! Covers all interactive CLI vendors hosted under `fl-host`:
//! `claude`, `codex`, `antigravity`, `grok`, `muse`, `lemonade`, and `ollama`.

pub use crate::host_runtime::HostRuntimePort;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use fl_core::permissions::{
    AutoApproval, Effect, EvidenceClass, Origin, Posture, ReceiptClass, ReceiptLine,
    ReceiptSubject, Transport, Vendor, VendorFacts, compile_posture,
};
use fl_host_proto::{ChildState, SessionId, SessionSummary};
use serde::{Deserialize, Serialize};

/// The command a runtime session starts in place of a blank shell: run as an argument vector,
/// never through a shell, so no argument needs quoting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Environment variables set for the command.
    pub env: Vec<(String, String)>,
    /// The program, then its arguments.
    pub argv: Vec<String>,
}

/// Per-session launch options interpreted by the vendor's [`VendorCliAdapter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorLaunchOptions {
    /// Whether the session should start with the vendor's remote-control / remote-interface
    /// bridge enabled when supported by the vendor CLI (`true` by default).
    pub remote_control: bool,
    /// Permission posture compiled into the vendor's launch flags (`Posture::Auto` by default).
    pub posture: Posture,
    /// Explicit [`AutoApproval`] choice used when `posture == Posture::Auto`. When `None`, the
    /// vendor's compiler default ([`Vendor::default_auto_approval`]) is used.
    pub auto_approval: Option<AutoApproval>,
}

impl Default for VendorLaunchOptions {
    fn default() -> Self {
        Self {
            remote_control: true,
            posture: Posture::Auto,
            auto_approval: None,
        }
    }
}

/// One permission receipt line returned by spin-up and remodel (`permission_receipts`).
///
/// Includes `summary` alongside `class` and `evidence` to satisfy the FerroDeck UI contract
/// (`{ class, summary, evidence? }`) as well as the underlying [`ReceiptLine`] fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionReceipt {
    pub subject: ReceiptSubject,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<Effect>,
    pub requested: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native: Vec<String>,
    pub class: ReceiptClass,
    pub evidence: EvidenceClass,
    pub note: String,
    pub summary: String,
}

impl From<ReceiptLine> for PermissionReceipt {
    fn from(line: ReceiptLine) -> Self {
        let summary = if line.native.is_empty() {
            format!("{}: {}", line.requested, line.note)
        } else {
            format!(
                "{}: {} ({})",
                line.requested,
                line.native.join(" "),
                line.note
            )
        };
        Self {
            subject: line.subject,
            origin: line.origin,
            effect: line.effect,
            requested: line.requested,
            native: line.native,
            class: line.class,
            evidence: line.evidence,
            note: line.note,
            summary,
        }
    }
}

/// Permission launch arguments and receipts compiled for a vendor and posture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchPermissions {
    pub args: Vec<String>,
    pub receipts: Vec<PermissionReceipt>,
}

/// Compile the launch permission arguments and receipts for `vendor` and `posture` using the
/// vendor's default [`AutoApproval`] choice.
///
/// The five permission-compiler vendors (`claude`, `codex`, `muse`, `grok`, `antigravity`) run
/// [`compile_posture`]. Vendors without a permission compiler (`lemonade`, `ollama`, and custom
/// prompt-delivery or API vendors) accept only [`Posture::Auto`] with an `unsupported` receipt and
/// refuse any other posture with an actionable error.
pub fn launch_permissions(vendor: &str, posture: Posture) -> Result<LaunchPermissions, String> {
    launch_permissions_with_auto_approval(vendor, posture, None)
}

/// Compile the launch permission arguments and receipts for `vendor`, `posture`, and an optional
/// explicit [`AutoApproval`] choice (defaulting to [`Vendor::default_auto_approval`] when `None`).
pub fn launch_permissions_with_auto_approval(
    vendor: &str,
    posture: Posture,
    auto_approval: Option<AutoApproval>,
) -> Result<LaunchPermissions, String> {
    let v = vendor.trim().to_ascii_lowercase();
    if let Ok(cv) = v.parse::<Vendor>() {
        let choice = auto_approval.unwrap_or_else(|| cv.default_auto_approval());
        let plan = compile_posture(cv, &VendorFacts::new(Transport::Pty), posture, choice)
            .map_err(|e| e.to_string())?;
        return Ok(LaunchPermissions {
            args: plan.launch_args,
            receipts: plan
                .receipts
                .into_iter()
                .map(PermissionReceipt::from)
                .collect(),
        });
    }
    if posture != Posture::Auto {
        return Err(format!(
            "vendor `{v}` cannot express posture `{posture}`: no permission flags exist for it"
        ));
    }
    Ok(LaunchPermissions {
        args: Vec::new(),
        receipts: vec![PermissionReceipt::from(ReceiptLine {
            subject: ReceiptSubject::Posture,
            origin: Origin::Axis,
            effect: None,
            requested: "posture Auto".to_string(),
            native: Vec::new(),
            class: ReceiptClass::Unsupported,
            evidence: EvidenceClass::Unsupported,
            note: format!(
                "vendor `{v}` has no permission compiler; auto preserves its native launch without permission flags"
            ),
        })],
    })
}

/// Interactive startup modal or trust prompt that the host supervisor dismisses or accepts on
/// first launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupPromptRule {
    pub match_text: &'static str,
    pub response_keys: &'static str,
    pub warn_on_miss: bool,
}

/// Unified adapter interface for an interactive vendor CLI hosted in `fl-host`.
pub trait VendorCliAdapter: Send + Sync + std::fmt::Debug {
    /// Canonical lowercase vendor identifier (`"claude"`, `"codex"`, `"antigravity"`, `"grok"`,
    /// `"muse"`, `"lemonade"`, `"ollama"`).
    fn vendor_id(&self) -> &str;

    /// Optional environment variable overriding the binary path (`FL_WIRE_*_BIN`).
    fn binary_env_var(&self) -> Option<&str>;

    /// Default executable name in `$HOME/.local/bin` or `$PATH`.
    fn default_binary(&self) -> &str;

    /// Resolve the executable path using `self.binary_env_var()`, `<home>/.local/bin`, or `$PATH`.
    fn resolve_binary(&self, home: Option<&Path>) -> String {
        let configured = self.binary_env_var().and_then(std::env::var_os);
        resolve_vendor_bin(configured, home, self.default_binary())
    }

    /// Normalize a requested model id for this vendor's CLI flags, or `None` when no `--model`
    /// argument should be passed.
    fn normalize_model(&self, requested: Option<&str>) -> Option<String>;

    /// Whether this vendor CLI supports starting sessions under remote control.
    fn supports_remote_control(&self) -> bool {
        false
    }

    /// CLI arguments that enable remote control for this session when [`VendorLaunchOptions::remote_control`]
    /// is `true`. Handled strictly at the adapter layer.
    fn remote_control_args(&self, _persona: &str) -> Vec<String> {
        Vec::new()
    }

    /// Build the full `LaunchSpec` (`env` + `argv`) for spawning a hosted session with default
    /// [`VendorLaunchOptions`] (`remote_control: true`).
    /// `thinking_level` is the vendor-native effort the operator chose; a vendor with no such
    /// launch option ignores it, and a value that is not a plain token never reaches argv.
    fn build_launch_spec(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
    ) -> LaunchSpec {
        self.build_launch_spec_with_options(
            persona,
            model,
            thinking_level,
            prompt,
            home,
            VendorLaunchOptions::default(),
        )
        .expect("default Auto posture is supported by all built-in adapters")
    }

    /// Build the full `LaunchSpec` (`env` + `argv`) with explicit [`VendorLaunchOptions`].
    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String>;

    /// Whether a launch with `thinking_level` (already a plain token) can honour it. The default
    /// refuses: a vendor with no launch option for it must not report a level it never applied.
    /// An `Err` is an actionable message for the operator.
    fn check_thinking_level(&self, _model: Option<&str>, _level: &str) -> Result<(), String> {
        Err(format!(
            "vendor `{}` does not support thinking_level; omit it",
            self.vendor_id()
        ))
    }

    /// Whether a launch on this vendor can express `posture`, returning the compiled CLI flags
    /// and receipts when supported.
    fn check_posture(&self, posture: Posture) -> Result<LaunchPermissions, String> {
        launch_permissions(self.vendor_id(), posture)
    }

    /// Whether a launch on this vendor can express `posture` with `auto_approval`, returning the
    /// compiled CLI flags and receipts when supported.
    fn check_posture_with_auto_approval(
        &self,
        posture: Posture,
        auto_approval: Option<AutoApproval>,
    ) -> Result<LaunchPermissions, String> {
        launch_permissions_with_auto_approval(self.vendor_id(), posture, auto_approval)
    }

    /// Interactive startup prompts to poll and answer right after spawning.
    fn startup_prompt_rules(&self) -> &[StartupPromptRule] {
        &[]
    }

    /// Key sequence used to interrupt an in-flight turn.
    fn interrupt_key(&self) -> &str {
        "C-c"
    }

    /// In-session slash command used to clear/reset context on reforge.
    fn reset_command(&self) -> &str {
        "/clear"
    }

    /// Whether the vendor's TUI accepts bracketed paste (`\x1b[200~...\x1b[201~`).
    fn supports_bracketed_paste(&self) -> bool {
        true
    }
}

/// The Muse model used when none is requested (or the request is unusable).
pub const MUSE_DEFAULT_MODEL: &str = "muse-wonder-full-1.0";

/// The Muse catalog model ids the CLI accepts for `--model`, default first.
pub const MUSE_CATALOG_MODELS: [&str; 3] =
    [MUSE_DEFAULT_MODEL, "muse-wonder-1.0", "muse-spark-1.3"];

/// The Muse reasoning effort tiers the CLI accepts for `--reasoning-effort`.
pub const MUSE_REASONING_EFFORTS: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// Canonical lowercase Muse `--reasoning-effort` tier for `level`, or `None` when unsupported.
pub fn normalize_muse_reasoning_effort(level: Option<&str>) -> Option<&'static str> {
    let lower = level?.trim().to_ascii_lowercase();
    MUSE_REASONING_EFFORTS
        .iter()
        .copied()
        .find(|tier| *tier == lower)
}

/// Unversioned names the CLI rejects ("model ... does not exist"), with the catalog id each means.
const MUSE_MODEL_ALIASES: [(&str, &str); 3] = [
    ("muse-wonder-full", MUSE_DEFAULT_MODEL),
    ("muse-wonder", "muse-wonder-1.0"),
    ("muse-spark", "muse-spark-1.3"),
];

/// Default model used when launching `lemonade chat` without an explicit model.
pub const LEMONADE_DEFAULT_CLI_MODEL: &str = "Gemma-4-26B-A4B-it-GGUF";

/// Default model used when launching `ollama run` without an explicit model.
pub const OLLAMA_DEFAULT_CLI_MODEL: &str = "llama3.2";

/// The text Grok Build 1.0.70 prints when `--yolo` is not enough for a new directory.
pub const GROK_DIRECTORY_TRUST_PROMPT: &str = "Do you trust the contents of this directory";

/// The text Antigravity CLI prints when launching in an untrusted workspace directory.
pub const AGY_PROJECT_TRUST_PROMPT: &str = "Do you trust the contents of this project?";

/// The modal Claude Code prints on first launch when auto mode asks for environment setup.
pub const CLAUDE_AUTO_MODE_PROMPT: &str = "Teach auto mode about your environment";

static CLAUDE_STARTUP_RULES: [StartupPromptRule; 1] = [StartupPromptRule {
    match_text: CLAUDE_AUTO_MODE_PROMPT,
    response_keys: "Escape",
    warn_on_miss: false,
}];

static GROK_STARTUP_RULES: [StartupPromptRule; 1] = [StartupPromptRule {
    match_text: GROK_DIRECTORY_TRUST_PROMPT,
    response_keys: "y",
    warn_on_miss: true,
}];

static AGY_STARTUP_RULES: [StartupPromptRule; 1] = [StartupPromptRule {
    match_text: AGY_PROJECT_TRUST_PROMPT,
    response_keys: "Enter",
    warn_on_miss: false,
}];

/// The model id to hand to `muse --model` for a requested `model`.
pub fn normalize_muse_model(requested: Option<&str>) -> String {
    let trimmed = requested.map_or("", str::trim);
    let lower = trimmed.to_ascii_lowercase();
    if let Some(id) = MUSE_CATALOG_MODELS.iter().find(|id| **id == lower) {
        return (*id).to_string();
    }
    if let Some((_, id)) = MUSE_MODEL_ALIASES.iter().find(|(alias, _)| *alias == lower) {
        return (*id).to_string();
    }
    let plain = trimmed.len() <= 64
        && trimmed
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if plain {
        trimmed.to_string()
    } else {
        MUSE_DEFAULT_MODEL.to_string()
    }
}

/// Split a trailing context-window tag (`[1m]`, `[200k]`: one to four digits, then lower-case
/// `k` or `m`) off a model id. `None` for an id with no such tag.
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
    id.len() <= 64
        && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A plain model id cannot be interpreted as a command-line option. It may end in ONE
/// context-window tag (`opus[1m]`, `sonnet[1m]`: [`split_window_tag`]); no other bracket is
/// accepted.
pub fn normalize_plain_model(requested: Option<&str>) -> Option<String> {
    let id = requested?.trim();
    let base = split_window_tag(id).map_or(id, |(base, _)| base);
    is_plain_model_id(base).then(|| id.to_string())
}

/// Split a trailing `-low`, `-medium` or `-high` off a model id. An id with no such suffix
/// is returned whole with no thinking level.
pub fn split_agy_thinking_suffix(model_id: &str) -> (String, Option<String>) {
    for level in ["low", "medium", "high"] {
        if let Some(base) = model_id.strip_suffix(&format!("-{level}"))
            && !base.is_empty()
        {
            return (base.to_string(), Some(level.to_string()));
        }
    }
    (model_id.to_string(), None)
}

/// The `agy --model` id for `model`.
pub fn agy_cli_model_id(model: &str, thinking_level: Option<&str>) -> String {
    let model = model.trim().to_ascii_lowercase();
    if model.is_empty() {
        return String::new();
    }
    let (base, existing) = split_agy_thinking_suffix(&model);
    let base = match base.as_str() {
        "argon" | "argon-4" | "argon-4.0" | "gemini-4.0-argon" => "gemini-4-argon",
        other => other,
    };
    if base == "gpt-oss-120b" {
        return format!("{base}-medium");
    }
    let explicit = thinking_level
        .map(|level| level.trim().to_ascii_lowercase())
        .filter(|level| matches!(level.as_str(), "low" | "medium" | "high"));
    let effort = explicit
        .or(existing)
        .unwrap_or_else(|| "medium".to_string());
    format!("{base}-{effort}")
}

/// `level` trimmed, when it is a plain token ([`is_plain_model_id`]) that is safe as one argv
/// element or one config value. Empty and non-plain values (quotes, spaces, `;`, newlines) are
/// dropped, never escaped.
pub fn plain_thinking_level(level: Option<&str>) -> Option<&str> {
    level.map(str::trim).filter(|l| is_plain_model_id(l))
}

/// The prompt as a positional CLI argument. A prompt starting with `-` is given a neutral
/// leading word so it cannot be read as a flag.
pub fn muse_prompt_argument(prompt: &str) -> String {
    if prompt.starts_with('-') {
        format!("Prompt: {prompt}")
    } else {
        prompt.to_string()
    }
}

/// `AGENT_PERSONA` (omitted for `codex`, which shares one `codex app-server` daemon across
/// sessions), `MUSE_NO_AUTO_UPDATE=1` for `muse` (so the `~/.local/bin/muse` launcher never forks
/// a background auto-update subshell that pins the session supervisor), plus a `PATH` that starts
/// with `<home>/.local/bin` when that directory is not already a `PATH` entry.
pub fn launch_env_for_vendor(
    vendor: &str,
    persona: &str,
    home: Option<&Path>,
) -> Vec<(String, String)> {
    launch_env_for_vendor_with_generation(vendor, persona, home, None)
}

/// Same as [`launch_env_for_vendor`], plus `AGENT_HYDRATION_GENERATION=<G>` for per-session CLI
/// vendors (omitted for `codex`, which shares one `codex app-server` daemon across sessions).
pub fn launch_env_for_vendor_with_generation(
    vendor: &str,
    persona: &str,
    home: Option<&Path>,
    hydration_generation: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if vendor != "codex" {
        env.push(("AGENT_PERSONA".into(), persona.into()));
        if let Some(generation) = hydration_generation
            .map(str::trim)
            .filter(|g| !g.is_empty())
        {
            env.push(("AGENT_HYDRATION_GENERATION".into(), generation.into()));
        }
    }
    if vendor == "muse" {
        env.push(("MUSE_NO_AUTO_UPDATE".into(), "1".into()));
    }
    if let Some(path) = path_with_local_bin(home) {
        env.push(("PATH".into(), path));
    }
    env
}

fn path_with_local_bin(home: Option<&Path>) -> Option<String> {
    let local = home?.join(".local").join("bin");
    let current = std::env::var("PATH").unwrap_or_default();
    Some(with_local_bin(&local, &current))
}

/// `path` unchanged when `local` is already a component; otherwise `local` first.
pub fn with_local_bin(local: &Path, path: &str) -> String {
    if std::env::split_paths(path).any(|entry| entry == local) {
        return path.to_string();
    }
    let local = local.to_string_lossy();
    if path.is_empty() {
        local.into_owned()
    } else {
        format!("{local}:{path}")
    }
}

/// Resolve a configured binary, then a user-local binary, then a PATH command.
pub fn resolve_vendor_bin(
    configured: Option<OsString>,
    home: Option<&Path>,
    bin_name: &str,
) -> String {
    if let Some(bin) = configured.filter(|b| !b.is_empty()) {
        return bin.to_string_lossy().into_owned();
    }
    home.map(|h| h.join(".local").join("bin").join(bin_name))
        .filter(|p| p.is_file())
        .map_or_else(
            || bin_name.to_string(),
            |p| p.to_string_lossy().into_owned(),
        )
}

// ---------------------------------------------------------------------------
// 7 Built-in Vendor CLI Adapters
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeCliAdapter;

impl VendorCliAdapter for ClaudeCliAdapter {
    fn vendor_id(&self) -> &str {
        "claude"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_CLAUDE_BIN")
    }

    fn default_binary(&self) -> &str {
        "claude"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        normalize_plain_model(requested)
    }

    fn supports_remote_control(&self) -> bool {
        true
    }

    fn remote_control_args(&self, _persona: &str) -> Vec<String> {
        vec!["--remote-control".to_string()]
    }

    /// Any plain token is passed through (`--effort` / reasoning effort); it applies with or
    /// without `--model`.
    fn check_thinking_level(&self, _model: Option<&str>, _level: &str) -> Result<(), String> {
        Ok(())
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        // Place `--remote-control` before `--permission-mode <mode>` so `--remote-control [name]`
        // (which accepts an optional name in Commander.js) is always followed by a `--` flag
        // and never consumes the trailing positional `<prompt>` when `model` and
        // `thinking_level` are both `None`.
        let perms =
            self.check_posture_with_auto_approval(options.posture, options.auto_approval)?;
        let mut argv = vec![self.resolve_binary(home)];
        if options.remote_control {
            argv.extend(self.remote_control_args(persona));
        }
        argv.extend(perms.args);
        if let Some(m) = self.normalize_model(model) {
            argv.extend(["--model".to_string(), m]);
        }
        if let Some(level) = plain_thinking_level(thinking_level) {
            argv.extend(["--effort".to_string(), level.to_string()]);
        }
        argv.push(muse_prompt_argument(prompt));
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }

    fn startup_prompt_rules(&self) -> &[StartupPromptRule] {
        &CLAUDE_STARTUP_RULES
    }

    fn interrupt_key(&self) -> &str {
        "Escape"
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CodexCliAdapter;

impl VendorCliAdapter for CodexCliAdapter {
    fn vendor_id(&self) -> &str {
        "codex"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_CODEX_BIN")
    }

    fn default_binary(&self) -> &str {
        "codex"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        normalize_plain_model(requested).filter(|m| split_window_tag(m).is_none())
    }

    fn supports_remote_control(&self) -> bool {
        true
    }

    fn remote_control_args(&self, _persona: &str) -> Vec<String> {
        vec!["--remote".to_string(), "unix://".to_string()]
    }

    /// Any plain token is passed through (`--effort` / reasoning effort); it applies with or
    /// without `--model`.
    fn check_thinking_level(&self, _model: Option<&str>, _level: &str) -> Result<(), String> {
        Ok(())
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms =
            self.check_posture_with_auto_approval(options.posture, options.auto_approval)?;
        let mut argv = vec![self.resolve_binary(home)];
        argv.extend(perms.args);
        if options.remote_control {
            argv.extend(self.remote_control_args(persona));
        }
        if let Some(m) = self.normalize_model(model) {
            argv.extend(["--model".to_string(), m]);
        }
        if let Some(level) = plain_thinking_level(thinking_level) {
            argv.extend([
                "-c".to_string(),
                format!("model_reasoning_effort=\"{level}\""),
            ]);
        }
        argv.push(muse_prompt_argument(prompt));
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }

    fn interrupt_key(&self) -> &str {
        "Escape"
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AntigravityCliAdapter;

impl VendorCliAdapter for AntigravityCliAdapter {
    fn vendor_id(&self) -> &str {
        "antigravity"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_AGY_BIN")
    }

    fn default_binary(&self) -> &str {
        "agy"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        requested
            .map(|m| agy_cli_model_id(m, None))
            .and_then(|m| normalize_plain_model(Some(&m)))
            .filter(|m| split_window_tag(m).is_none())
    }

    fn supports_remote_control(&self) -> bool {
        true
    }

    fn remote_control_args(&self, _persona: &str) -> Vec<String> {
        vec!["--remote-control".to_string()]
    }

    /// Antigravity takes its level only as the `-low|-medium|-high` suffix of the model id, so
    /// the level needs a model and must be one of those three; `gpt-oss-120b` only runs at medium.
    fn check_thinking_level(&self, model: Option<&str>, level: &str) -> Result<(), String> {
        let level = level.trim().to_ascii_lowercase();
        if !matches!(level.as_str(), "low" | "medium" | "high") {
            return Err(format!(
                "antigravity supports thinking_level low, medium or high, not `{level}`"
            ));
        }
        let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else {
            return Err(
                "antigravity applies thinking_level only through the model id: pass a model too"
                    .to_string(),
            );
        };
        let (base, _) = split_agy_thinking_suffix(&model.to_ascii_lowercase());
        if base == "gpt-oss-120b" && level != "medium" {
            return Err(format!(
                "model gpt-oss-120b only runs at thinking_level medium, not `{level}`"
            ));
        }
        Ok(())
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms =
            self.check_posture_with_auto_approval(options.posture, options.auto_approval)?;
        let mut argv = vec![self.resolve_binary(home)];
        argv.extend(perms.args);
        if options.remote_control {
            argv.extend(self.remote_control_args(persona));
        }
        let cli_model = model
            .map(|m| agy_cli_model_id(m, thinking_level))
            .and_then(|m| normalize_plain_model(Some(&m)))
            .filter(|m| split_window_tag(m).is_none());
        if let Some(m) = cli_model {
            argv.extend(["--model".to_string(), m]);
        }
        argv.push("--prompt-interactive".to_string());
        argv.push(muse_prompt_argument(prompt));
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }

    fn startup_prompt_rules(&self) -> &[StartupPromptRule] {
        &AGY_STARTUP_RULES
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GrokCliAdapter;

impl VendorCliAdapter for GrokCliAdapter {
    fn vendor_id(&self) -> &str {
        "grok"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_GROK_BIN")
    }

    fn default_binary(&self) -> &str {
        "grok"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        normalize_plain_model(requested).filter(|m| split_window_tag(m).is_none())
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        _thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms =
            self.check_posture_with_auto_approval(options.posture, options.auto_approval)?;
        let mut argv = vec![self.resolve_binary(home)];
        argv.extend(perms.args);
        if let Some(m) = self.normalize_model(model) {
            argv.extend(["--model".to_string(), m]);
        }
        argv.push(muse_prompt_argument(prompt));
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }

    fn startup_prompt_rules(&self) -> &[StartupPromptRule] {
        &GROK_STARTUP_RULES
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MuseCliAdapter;

impl VendorCliAdapter for MuseCliAdapter {
    fn vendor_id(&self) -> &str {
        "muse"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_MUSE_BIN")
    }

    fn default_binary(&self) -> &str {
        "muse"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        Some(normalize_muse_model(requested))
    }

    /// Muse supports `--reasoning-effort <none|minimal|low|medium|high|xhigh|max|ultra>` with or
    /// without `--model`.
    fn check_thinking_level(&self, _model: Option<&str>, level: &str) -> Result<(), String> {
        if normalize_muse_reasoning_effort(Some(level)).is_some() {
            Ok(())
        } else {
            Err(format!(
                "muse supports thinking_level {}, not `{}`",
                MUSE_REASONING_EFFORTS.join(", "),
                level.trim().to_ascii_lowercase()
            ))
        }
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms =
            self.check_posture_with_auto_approval(options.posture, options.auto_approval)?;
        let mut argv = vec![
            self.resolve_binary(home),
            "--model".to_string(),
            normalize_muse_model(model),
        ];
        if let Some(effort) = normalize_muse_reasoning_effort(thinking_level) {
            argv.extend(["--reasoning-effort".to_string(), effort.to_string()]);
        }
        argv.extend(perms.args);
        if options.posture == Posture::Auto {
            argv.push("--disable-sandbox".to_string());
        }
        argv.push(muse_prompt_argument(prompt));
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }

    fn reset_command(&self) -> &str {
        "/new"
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LemonadeCliAdapter;

impl VendorCliAdapter for LemonadeCliAdapter {
    fn vendor_id(&self) -> &str {
        "lemonade"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_LEMONADE_BIN")
    }

    fn default_binary(&self) -> &str {
        "lemonade"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        normalize_plain_model(requested)
            .filter(|m| split_window_tag(m).is_none())
            .or_else(|| Some(LEMONADE_DEFAULT_CLI_MODEL.to_string()))
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        _thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms =
            self.check_posture_with_auto_approval(options.posture, options.auto_approval)?;
        let host = std::env::var("FL_LEMONADE_HOST")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("LEMONADE_HOST")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = std::env::var("FL_LEMONADE_PORT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("LEMONADE_PORT")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| "13305".to_string());
        let mut argv = vec![
            self.resolve_binary(home),
            "--no-discovery".to_string(),
            "--host".to_string(),
            host,
            "--port".to_string(),
            port,
            "chat".to_string(),
        ];
        argv.extend(perms.args);
        if !prompt.trim().is_empty() {
            argv.extend(["--system".to_string(), prompt.to_string()]);
        }
        argv.push(
            self.normalize_model(model)
                .unwrap_or_else(|| LEMONADE_DEFAULT_CLI_MODEL.to_string()),
        );
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OllamaCliAdapter;

impl VendorCliAdapter for OllamaCliAdapter {
    fn vendor_id(&self) -> &str {
        "ollama"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_OLLAMA_BIN")
    }

    fn default_binary(&self) -> &str {
        "ollama"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        requested
            .map(str::trim)
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 64
                    && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
                    && id.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/')
                    })
            })
            .map(str::to_string)
            .or_else(|| Some(OLLAMA_DEFAULT_CLI_MODEL.to_string()))
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        _thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms =
            self.check_posture_with_auto_approval(options.posture, options.auto_approval)?;
        let mut argv = vec![
            self.resolve_binary(home),
            "run".to_string(),
            self.normalize_model(model)
                .unwrap_or_else(|| OLLAMA_DEFAULT_CLI_MODEL.to_string()),
        ];
        argv.extend(perms.args);
        if !prompt.trim().is_empty() {
            argv.push(muse_prompt_argument(prompt));
        }
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }
}

// ---------------------------------------------------------------------------
// HostVendorAdapterRegistry
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct HostVendorAdapterRegistry {
    adapters: RwLock<BTreeMap<String, Arc<dyn VendorCliAdapter>>>,
}

impl Default for HostVendorAdapterRegistry {
    fn default() -> Self {
        let mut map: BTreeMap<String, Arc<dyn VendorCliAdapter>> = BTreeMap::new();
        let builtins: [Arc<dyn VendorCliAdapter>; 7] = [
            Arc::new(ClaudeCliAdapter),
            Arc::new(CodexCliAdapter),
            Arc::new(AntigravityCliAdapter),
            Arc::new(GrokCliAdapter),
            Arc::new(MuseCliAdapter),
            Arc::new(LemonadeCliAdapter),
            Arc::new(OllamaCliAdapter),
        ];
        for adapter in builtins {
            map.insert(adapter.vendor_id().to_string(), adapter);
        }
        Self {
            adapters: RwLock::new(map),
        }
    }
}

impl HostVendorAdapterRegistry {
    pub fn global() -> &'static Self {
        static REGISTRY: OnceLock<HostVendorAdapterRegistry> = OnceLock::new();
        REGISTRY.get_or_init(Self::default)
    }

    pub fn lookup(&self, vendor: &str) -> Option<Arc<dyn VendorCliAdapter>> {
        let key = vendor.trim().to_ascii_lowercase();
        self.adapters
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    }

    pub fn register(&self, adapter: Arc<dyn VendorCliAdapter>) {
        let key = adapter.vendor_id().trim().to_ascii_lowercase();
        self.adapters
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, adapter);
    }

    pub fn list(&self) -> Vec<Arc<dyn VendorCliAdapter>> {
        self.adapters
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }
}

pub fn lookup_vendor_cli_adapter(vendor: &str) -> Option<Arc<dyn VendorCliAdapter>> {
    HostVendorAdapterRegistry::global().lookup(vendor)
}

pub fn register_vendor_cli_adapter(adapter: Arc<dyn VendorCliAdapter>) {
    HostVendorAdapterRegistry::global().register(adapter);
}

pub fn list_vendor_cli_adapters() -> Vec<Arc<dyn VendorCliAdapter>> {
    HostVendorAdapterRegistry::global().list()
}

/// Default binary names (lowercased file names) of all registered CLI adapters.
pub fn vendor_cli_binaries() -> Vec<String> {
    let mut bins: Vec<String> = list_vendor_cli_adapters()
        .into_iter()
        .filter_map(|a| {
            Path::new(a.default_binary())
                .file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
        })
        .collect();
    bins.sort();
    bins.dedup();
    bins
}

// ---------------------------------------------------------------------------
// Deterministic unit <-> SessionId mapping helpers
// ---------------------------------------------------------------------------

/// Map a runtime unit or target (`"Ada:claude"`, `"muse:ada"`, `"Ada"`) onto a valid
/// `fl_host_proto::SessionId` (`"Ada.claude"`, `"muse.ada"`, `"Ada"`).
pub fn unit_to_host_session_id(unit: &str) -> Option<SessionId> {
    let trimmed = unit.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.replace(':', ".");
    SessionId::new(&normalized).ok()
}

/// Map a `fl_host_proto::SessionId` (`"Ada.claude"`, `"muse.ada"`, `"Ada"`) back to its
/// `session:window` or bare session string (`"Ada:claude"`, `"muse:ada"`, `"Ada"`).
pub fn host_session_id_to_unit(id: &SessionId) -> String {
    match id.as_str().split_once('.') {
        Some((session, window)) => format!("{session}:{window}"),
        None => id.as_str().to_string(),
    }
}

/// Split `target` into `(vendor, persona)` when it has the `<vendor>:<persona>` lifecycle
/// session-id shape (`"codex:kage"`, `"muse:eve"`): a registered vendor id (matched
/// case-insensitively) and a non-empty persona slug separated by a single `:`.
pub fn split_lifecycle_session_id(target: &str) -> Option<(String, &str)> {
    let (vendor, persona) = target.trim().split_once(':')?;
    if vendor.is_empty() || persona.is_empty() || persona.contains(':') {
        return None;
    }
    let spec = fl_core::lookup_vendor(&vendor.to_ascii_lowercase())?;
    Some((spec.id, persona))
}

/// Map a `<vendor>:<persona>` lifecycle session id (`"codex:kage"`, `"muse:eve"`) onto its
/// `fl_host_proto::SessionId` (`"Kage.codex"`, `"muse.eve"`) using the registry's display
/// casing for `<persona>` (`agents`).
///
/// - For `"muse:<persona>"`, the host session id is `"muse.<persona>"` (with `<persona>`
///   lowercased).
/// - For any other registered vendor, `<persona>` is looked up in `agents` (active records
///   take precedence over retired ones). When matching records disagree on `display_name`,
///   an `Err` is returned describing the ambiguity.
/// - Returns `Ok(None)` when `target` is not a `<vendor>:<persona>` lifecycle id or `<persona>`
///   is not present in `agents`.
pub fn lifecycle_id_to_host_session_id(
    target: &str,
    agents: &[fl_core::AgentRecord],
) -> Result<Option<SessionId>, String> {
    let Some((vendor, persona)) = split_lifecycle_session_id(target) else {
        return Ok(None);
    };
    if vendor == "muse" {
        return Ok(SessionId::new(&format!("muse.{}", persona.to_ascii_lowercase())).ok());
    }
    let matches_persona = |a: &&fl_core::AgentRecord| {
        a.name_key.eq_ignore_ascii_case(persona) || a.display_name.eq_ignore_ascii_case(persona)
    };
    let active: Vec<&fl_core::AgentRecord> = agents
        .iter()
        .filter(|a| a.status == fl_core::AgentStatus::Active)
        .filter(matches_persona)
        .collect();
    let candidates = if !active.is_empty() {
        active
    } else {
        agents.iter().filter(matches_persona).collect()
    };
    let mut display_names: Vec<&str> = candidates
        .into_iter()
        .map(|a| a.display_name.as_str())
        .collect();
    display_names.sort_unstable();
    display_names.dedup();
    match display_names.as_slice() {
        [] => Ok(None),
        [display] => Ok(SessionId::new(&format!("{display}.{vendor}")).ok()),
        many => Err(format!(
            "ambiguous fl-host session target `{}`: registry has multiple display casings for persona `{persona}` ({})",
            target.trim(),
            many.join(", ")
        )),
    }
}

/// Live `SessionId`s in `sessions` that belong to `unit`.
///
/// - `unit == ""` or `unit == "muse"` matches nothing (the shared Muse container is never a single
///   session unit).
/// - `unit` containing `:` (`"muse:ada"`, `"Ada:claude"`) matches the exact `SessionId`
///   (`"muse.ada"`, `"Ada.claude"`), and when `unit` has the `<vendor>:<persona>` lifecycle shape
///   (`"codex:kage"`, `"muse:eve"`), also matches live sessions whose `s.stream.session` is
///   `muse.<persona_lower>` (for `vendor == "muse"`) or `<SessionPersona>.<vendor>` where
///   `SessionPersona.eq_ignore_ascii_case(persona)` (for non-Muse vendors).
/// - Bare `unit` (`"Ada"`) matches `"Ada"` and any `"Ada.<window>"` (`"Ada.claude"`, etc.).
pub fn matching_live_host_sessions(sessions: &[SessionSummary], unit: &str) -> Vec<SessionId> {
    let unit = unit.trim();
    if unit.is_empty() || unit == "muse" {
        return Vec::new();
    }
    let live = sessions
        .iter()
        .filter(|s| matches!(s.state, ChildState::Running(_)));
    if unit.contains(':') {
        let wanted = unit_to_host_session_id(unit);
        let lifecycle = split_lifecycle_session_id(unit);
        return live
            .filter(|s| {
                if wanted.as_ref() == Some(&s.stream.session) {
                    return true;
                }
                if let Some((ref vendor, persona)) = lifecycle {
                    if vendor == "muse" {
                        let expected = format!("muse.{}", persona.to_ascii_lowercase());
                        return s.stream.session.as_str() == expected;
                    }
                    if let Some((sess_persona, sess_vendor)) =
                        s.stream.session.as_str().split_once('.')
                    {
                        return sess_vendor == vendor.as_str()
                            && sess_persona.eq_ignore_ascii_case(persona);
                    }
                }
                false
            })
            .map(|s| s.stream.session.clone())
            .collect();
    }
    live.filter(|s| {
        let id = s.stream.session.as_str();
        id == unit
            || id
                .strip_prefix(unit)
                .is_some_and(|rest| rest.starts_with('.'))
    })
    .map(|s| s.stream.session.clone())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_host_proto::{
        ChildRecord, HostInstance, InputState, SessionEpoch, SessionMode, SpawnKind, StreamId,
    };

    fn summary(id: &str, running: bool) -> SessionSummary {
        let session = SessionId::new(id).unwrap();
        let record = ChildRecord {
            session: session.clone(),
            session_epoch: SessionEpoch(1),
            kind: SpawnKind::Pipe,
            pid: 42,
            input: InputState::Writable,
            started_at_ms: 1,
            exit: None,
            consumer_stalled: false,
        };
        SessionSummary {
            stream: StreamId {
                host: HostInstance::new([1u8; 16]),
                session,
                epoch: SessionEpoch(1),
            },
            mode: SessionMode::ReliablePipe,
            state: if running {
                ChildState::Running(record)
            } else {
                ChildState::CleanupComplete(record)
            },
            acked: 0,
            latest: 0,
            input: InputState::Writable,
        }
    }

    #[test]
    fn registry_contains_all_seven_builtin_vendor_cli_adapters() {
        let ids: Vec<String> = list_vendor_cli_adapters()
            .into_iter()
            .map(|a| a.vendor_id().to_string())
            .collect();
        for expected in [
            "antigravity",
            "claude",
            "codex",
            "grok",
            "lemonade",
            "muse",
            "ollama",
        ] {
            assert!(ids.contains(&expected.to_string()), "missing {expected}");
            assert!(lookup_vendor_cli_adapter(expected).is_some());
        }
        let bins = vendor_cli_binaries();
        for bin in [
            "agy", "claude", "codex", "grok", "lemonade", "muse", "ollama",
        ] {
            assert!(bins.contains(&bin.to_string()), "missing bin {bin}");
        }
    }

    #[test]
    fn thinking_level_reaches_claude_codex_and_agy_launch_argv() {
        let claude = lookup_vendor_cli_adapter("claude").unwrap();
        let spec = claude.build_launch_spec("ada", Some("opus"), Some("high"), "Hi", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--remote-control",
                "--permission-mode",
                "auto",
                "--model",
                "opus",
                "--effort",
                "high",
                "Hi"
            ]
        );
        let spec = claude.build_launch_spec("ada", None, Some(" max "), "Hi", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--remote-control",
                "--permission-mode",
                "auto",
                "--effort",
                "max",
                "Hi"
            ]
        );
        // When both model and thinking_level are None, `--permission-mode` must still sit
        // between `--remote-control` and the positional prompt so Commander.js (`--remote-control [name]`)
        // never swallows the prompt as the session name.
        let spec = claude.build_launch_spec("ada", None, None, "Hi", None);
        assert_eq!(
            &spec.argv[1..],
            &["--remote-control", "--permission-mode", "auto", "Hi"]
        );

        let codex = lookup_vendor_cli_adapter("codex").unwrap();
        let spec = codex.build_launch_spec("ada", Some("gpt-5"), Some("xhigh"), "Hi", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--approve-for-me",
                "--remote",
                "unix://",
                "--model",
                "gpt-5",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "Hi"
            ]
        );

        let agy = lookup_vendor_cli_adapter("antigravity").unwrap();
        let spec = agy.build_launch_spec(
            "ada",
            Some("claude-opus-4-6-thinking"),
            Some("high"),
            "Hi",
            None,
        );
        assert!(
            spec.argv
                .windows(2)
                .any(|w| w == ["--model", "claude-opus-4-6-thinking-high"]),
            "{:?}",
            spec.argv
        );

        let muse = lookup_vendor_cli_adapter("muse").unwrap();
        for effort in MUSE_REASONING_EFFORTS {
            assert!(muse.check_thinking_level(None, effort).is_ok(), "{effort}");
            assert!(
                muse.check_thinking_level(None, &effort.to_ascii_uppercase())
                    .is_ok(),
                "{effort}"
            );
        }
        assert!(muse.check_thinking_level(None, "extreme").is_err());
        let spec = muse.build_launch_spec("ada", None, Some(" HIGH "), "Hi", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--model",
                MUSE_DEFAULT_MODEL,
                "--reasoning-effort",
                "high",
                "--approval-mode",
                "on-request",
                "--approval-judge",
                "on",
                "--disable-sandbox",
                "Hi"
            ]
        );
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "MUSE_NO_AUTO_UPDATE" && v == "1"),
            "{:?}",
            spec.env
        );

        // Vendors with no launch option for it ignore the level.
        for vendor in ["grok", "lemonade", "ollama"] {
            let a = lookup_vendor_cli_adapter(vendor).unwrap();
            let with = a.build_launch_spec("ada", None, Some("high"), "Hi", None);
            let without = a.build_launch_spec("ada", None, None, "Hi", None);
            assert_eq!(with.argv, without.argv, "{vendor}");
        }
    }

    #[test]
    fn remote_control_is_configurable_at_the_adapter_layer() {
        let off = VendorLaunchOptions {
            remote_control: false,
            posture: Posture::Auto,
            auto_approval: None,
        };
        let claude = lookup_vendor_cli_adapter("claude").unwrap();
        assert!(claude.supports_remote_control());
        let spec_off = claude
            .build_launch_spec_with_options("ada", Some("opus"), None, "Hi", None, off)
            .unwrap();
        assert_eq!(
            &spec_off.argv[1..],
            &["--permission-mode", "auto", "--model", "opus", "Hi"]
        );

        let codex = lookup_vendor_cli_adapter("codex").unwrap();
        assert!(codex.supports_remote_control());
        let spec_off = codex
            .build_launch_spec_with_options("ada", Some("gpt-5"), None, "Hi", None, off)
            .unwrap();
        assert_eq!(
            &spec_off.argv[1..],
            &["--approve-for-me", "--model", "gpt-5", "Hi"]
        );

        let agy = lookup_vendor_cli_adapter("antigravity").unwrap();
        assert!(agy.supports_remote_control());
        let spec_off = agy
            .build_launch_spec_with_options("ada", Some("claude-sonnet-5-5"), None, "Hi", None, off)
            .unwrap();
        assert_eq!(
            &spec_off.argv[1..],
            &[
                "--mode",
                "auto",
                "--model",
                "claude-sonnet-5-5-medium",
                "--prompt-interactive",
                "Hi"
            ]
        );
    }

    #[test]
    fn hostile_or_empty_thinking_levels_never_reach_argv() {
        for bad in [
            "",
            "   ",
            "high; rm -rf /",
            "hi\"gh",
            "hi'gh",
            "high\nlow",
            "--model",
            "high low",
            "$(id)",
        ] {
            for vendor in ["claude", "codex", "antigravity", "muse"] {
                let a = lookup_vendor_cli_adapter(vendor).unwrap();
                let with = a.build_launch_spec("ada", Some("opus"), Some(bad), "Hi", None);
                let without = a.build_launch_spec("ada", Some("opus"), None, "Hi", None);
                assert_eq!(with.argv, without.argv, "{vendor} {bad:?}");
            }
        }
        assert_eq!(plain_thinking_level(Some("high")), Some("high"));
        assert_eq!(plain_thinking_level(Some("-x")), None);
    }

    #[test]
    fn seven_vendor_launch_specs_and_controls_match_contracts() {
        let claude = lookup_vendor_cli_adapter("claude").unwrap();
        let spec = claude.build_launch_spec("ada", Some("opus[1m]"), None, "Hello", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--remote-control",
                "--permission-mode",
                "auto",
                "--model",
                "opus[1m]",
                "Hello"
            ]
        );
        assert_eq!(claude.interrupt_key(), "Escape");
        assert_eq!(claude.reset_command(), "/clear");
        assert_eq!(claude.startup_prompt_rules().len(), 1);

        let codex = lookup_vendor_cli_adapter("codex").unwrap();
        let spec = codex.build_launch_spec("ada", Some("gpt-5"), None, "Hello", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--approve-for-me",
                "--remote",
                "unix://",
                "--model",
                "gpt-5",
                "Hello"
            ]
        );
        let spec_ask = codex
            .build_launch_spec_with_options(
                "ada",
                Some("gpt-5"),
                None,
                "Hello",
                None,
                VendorLaunchOptions {
                    remote_control: true,
                    posture: Posture::Auto,
                    auto_approval: Some(AutoApproval::AskHuman),
                },
            )
            .unwrap();
        assert_eq!(
            &spec_ask.argv[1..],
            &[
                "-a",
                "on-request",
                "-s",
                "workspace-write",
                "--remote",
                "unix://",
                "--model",
                "gpt-5",
                "Hello"
            ]
        );
        assert!(spec.env.iter().all(|(k, _)| k != "AGENT_PERSONA"));
        assert_eq!(codex.interrupt_key(), "Escape");

        let agy = lookup_vendor_cli_adapter("antigravity").unwrap();
        let spec = agy.build_launch_spec("ada", Some("claude-sonnet-5-5"), None, "Hello", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--mode",
                "auto",
                "--remote-control",
                "--model",
                "claude-sonnet-5-5-medium",
                "--prompt-interactive",
                "Hello"
            ]
        );
        assert_eq!(agy.interrupt_key(), "C-c");
        assert_eq!(agy.startup_prompt_rules().len(), 1);
        assert_eq!(
            agy.startup_prompt_rules()[0].match_text,
            AGY_PROJECT_TRUST_PROMPT
        );
        assert_eq!(agy.startup_prompt_rules()[0].response_keys, "Enter");
        assert!(!agy.startup_prompt_rules()[0].warn_on_miss);
        for alias in [
            "argon-4.0",
            "argon-4.0-medium",
            "argon-4",
            "argon",
            "gemini-4.0-argon",
            "gemini-4-argon",
        ] {
            assert_eq!(agy_cli_model_id(alias, None), "gemini-4-argon-medium");
            assert_eq!(agy_cli_model_id(alias, Some("high")), "gemini-4-argon-high");
        }

        let grok = lookup_vendor_cli_adapter("grok").unwrap();
        let spec = grok.build_launch_spec("ada", Some("grok-4"), None, "-start", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--permission-mode",
                "auto",
                "--model",
                "grok-4",
                "Prompt: -start"
            ]
        );
        assert_eq!(grok.startup_prompt_rules().len(), 1);
        assert!(grok.startup_prompt_rules()[0].warn_on_miss);

        let muse = lookup_vendor_cli_adapter("muse").unwrap();
        let spec = muse.build_launch_spec("ada", None, None, "Hello", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--model",
                MUSE_DEFAULT_MODEL,
                "--approval-mode",
                "on-request",
                "--approval-judge",
                "on",
                "--disable-sandbox",
                "Hello"
            ]
        );
        let spec_muse_ask = muse
            .build_launch_spec_with_options(
                "ada",
                None,
                None,
                "Hello",
                None,
                VendorLaunchOptions {
                    remote_control: true,
                    posture: Posture::Auto,
                    auto_approval: Some(AutoApproval::AskHuman),
                },
            )
            .unwrap();
        assert_eq!(
            &spec_muse_ask.argv[1..],
            &[
                "--model",
                MUSE_DEFAULT_MODEL,
                "--approval-mode",
                "on-request",
                "--approval-judge",
                "off",
                "--disable-sandbox",
                "Hello"
            ]
        );
        for non_auto in [Posture::Ask, Posture::AcceptEdits] {
            let spec_non_auto = muse
                .build_launch_spec_with_options(
                    "ada",
                    None,
                    None,
                    "Hello",
                    None,
                    VendorLaunchOptions {
                        remote_control: true,
                        posture: non_auto,
                        auto_approval: None,
                    },
                )
                .unwrap();
            assert!(
                !spec_non_auto.argv.iter().any(|a| a == "--disable-sandbox"),
                "muse {non_auto} must keep the sandbox: {:?}",
                spec_non_auto.argv
            );
        }
        let spec_muse_plan = muse.build_launch_spec_with_options(
            "ada",
            None,
            None,
            "Hello",
            None,
            VendorLaunchOptions {
                remote_control: true,
                posture: Posture::Plan,
                auto_approval: None,
            },
        );
        assert!(spec_muse_plan.is_err());
        assert_eq!(muse.reset_command(), "/new");

        let lemonade = lookup_vendor_cli_adapter("lemonade").unwrap();
        let spec = lemonade.build_launch_spec("ada", None, None, "System prompt", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--no-discovery",
                "--host",
                "127.0.0.1",
                "--port",
                "13305",
                "chat",
                "--system",
                "System prompt",
                LEMONADE_DEFAULT_CLI_MODEL
            ]
        );

        let ollama = lookup_vendor_cli_adapter("ollama").unwrap();
        let spec = ollama.build_launch_spec("ada", None, None, "Hello ollama", None);
        assert_eq!(
            &spec.argv[1..],
            &["run", OLLAMA_DEFAULT_CLI_MODEL, "Hello ollama"]
        );
    }

    #[test]
    fn unit_and_session_id_round_trip_and_match_live_sessions() {
        let id = unit_to_host_session_id("Ada:claude").unwrap();
        assert_eq!(id.as_str(), "Ada.claude");
        assert_eq!(host_session_id_to_unit(&id), "Ada:claude");

        let sessions = vec![
            summary("Ada.claude", true),
            summary("muse.ada", true),
            summary("muse.bob", false),
            summary("Ada2.claude", true),
        ];
        assert_eq!(
            matching_live_host_sessions(&sessions, "Ada"),
            vec![SessionId::new("Ada.claude").unwrap()]
        );
        assert_eq!(
            matching_live_host_sessions(&sessions, "Ada:claude"),
            vec![SessionId::new("Ada.claude").unwrap()]
        );
        assert_eq!(
            matching_live_host_sessions(&sessions, "claude:ada"),
            vec![SessionId::new("Ada.claude").unwrap()]
        );
        assert_eq!(
            matching_live_host_sessions(&sessions, "CLAUDE:Ada"),
            vec![SessionId::new("Ada.claude").unwrap()]
        );
        assert_eq!(
            matching_live_host_sessions(&sessions, "muse:ada"),
            vec![SessionId::new("muse.ada").unwrap()]
        );
        assert_eq!(
            matching_live_host_sessions(&sessions, "Muse:Ada"),
            vec![SessionId::new("muse.ada").unwrap()]
        );
        assert!(matching_live_host_sessions(&sessions, "muse:bob").is_empty());
        assert!(matching_live_host_sessions(&sessions, "muse").is_empty());
    }

    #[test]
    fn lifecycle_id_to_host_session_id_uses_registry_display_casing_and_refuses_ambiguity() {
        use fl_core::{AgentId, AgentRecord, AgentRole, AgentStatus, At, Iri};

        let make_agent =
            |id: u64, name_key: &str, display_name: &str, status: AgentStatus| AgentRecord {
                id: AgentId(Iri::parse(&format!("urn:test:agent:{id}")).unwrap()),
                name_key: name_key.to_string(),
                display_name: display_name.to_string(),
                email: format!("{name_key}@artificialhumanity.io"),
                project: "ferroloop".to_string(),
                role: AgentRole::Assistant,
                extra_areas: Vec::new(),
                status,
                updated_at: At::from_unix_millis(1),
                by: "owner".to_string(),
            };

        let agents = vec![
            make_agent(1, "kage", "Kage", AgentStatus::Active),
            make_agent(2, "jarvis", "JARVIS", AgentStatus::Active),
            make_agent(3, "jarvis", "Jarvis", AgentStatus::Retired),
        ];

        assert_eq!(
            split_lifecycle_session_id("codex:kage"),
            Some(("codex".to_string(), "kage"))
        );
        assert_eq!(
            split_lifecycle_session_id("codex:Kage"),
            Some(("codex".to_string(), "Kage"))
        );
        assert_eq!(
            split_lifecycle_session_id("CODEX:kage"),
            Some(("codex".to_string(), "kage"))
        );
        assert_eq!(
            split_lifecycle_session_id("Muse:Eve"),
            Some(("muse".to_string(), "Eve"))
        );
        assert_eq!(split_lifecycle_session_id("Kage:codex"), None);
        assert_eq!(
            lifecycle_id_to_host_session_id("codex:kage", &agents).unwrap(),
            Some(SessionId::new("Kage.codex").unwrap())
        );
        assert_eq!(
            lifecycle_id_to_host_session_id("CODEX:Kage", &agents).unwrap(),
            Some(SessionId::new("Kage.codex").unwrap())
        );
        // Active record wins over retired record with older casing.
        assert_eq!(
            lifecycle_id_to_host_session_id("claude:jarvis", &agents).unwrap(),
            Some(SessionId::new("JARVIS.claude").unwrap())
        );
        // Muse keeps `muse.<persona>` lowercased.
        assert_eq!(
            lifecycle_id_to_host_session_id("muse:eve", &[]).unwrap(),
            Some(SessionId::new("muse.eve").unwrap())
        );
        assert_eq!(
            lifecycle_id_to_host_session_id("Muse:Eve", &[]).unwrap(),
            Some(SessionId::new("muse.eve").unwrap())
        );
        // Unregistered non-Muse persona returns None.
        assert_eq!(
            lifecycle_id_to_host_session_id("codex:unknown", &agents).unwrap(),
            None
        );
        // Conflicting active display casings refuse on ambiguity.
        let conflicting = vec![
            make_agent(10, "kage", "Kage", AgentStatus::Active),
            make_agent(11, "kage", "KAGE", AgentStatus::Active),
        ];
        let err = lifecycle_id_to_host_session_id("codex:kage", &conflicting).unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
    }

    #[test]
    fn vendor_posture_matrix_emits_bypass_only_on_bypass_and_refuses_unsupported() {
        const BYPASS_FLAGS: [&str; 7] = [
            "--yolo",
            "--disable-approval",
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "--dangerously-bypass-approvals-and-sandbox",
            "--always-approve",
            "bypassPermissions",
        ];
        let has_bypass = |argv: &[String]| {
            argv.iter().any(|a| BYPASS_FLAGS.contains(&a.as_str()))
                || argv.windows(2).any(|w| w == ["-a", "never"])
        };

        let all_postures = [
            Posture::Ask,
            Posture::AcceptEdits,
            Posture::Auto,
            Posture::Plan,
            Posture::Bypass,
        ];
        for vendor in [
            "claude",
            "codex",
            "antigravity",
            "grok",
            "muse",
            "lemonade",
            "ollama",
        ] {
            let adapter = lookup_vendor_cli_adapter(vendor).unwrap();
            for posture in all_postures {
                let res = adapter.check_posture(posture);
                let unsupported = matches!(
                    (vendor, posture),
                    ("codex", Posture::Ask)
                        | ("antigravity", Posture::Ask)
                        | ("muse", Posture::Plan)
                ) || (matches!(vendor, "lemonade" | "ollama")
                    && posture != Posture::Auto);
                let spec_res = adapter.build_launch_spec_with_options(
                    "ada",
                    None,
                    None,
                    "Hydrate",
                    None,
                    VendorLaunchOptions {
                        remote_control: true,
                        posture,
                        auto_approval: None,
                    },
                );
                if unsupported {
                    let err = res
                        .err()
                        .unwrap_or_else(|| panic!("{vendor} {posture} must be refused"));
                    assert!(!err.is_empty(), "{vendor} {posture}");
                    let spec_err = spec_res.err().unwrap_or_else(|| {
                        panic!("{vendor} {posture} launch spec must fail closed")
                    });
                    assert_eq!(spec_err, err, "{vendor} {posture}");
                    continue;
                }
                let perms = res
                    .unwrap_or_else(|e| panic!("{vendor} {posture} must be supported, got: {e}"));
                assert!(!perms.receipts.is_empty(), "{vendor} {posture}");
                for receipt in &perms.receipts {
                    assert!(!receipt.summary.is_empty(), "{vendor} {posture}");
                }
                let spec = spec_res.unwrap_or_else(|e| {
                    panic!("{vendor} {posture} launch spec must succeed, got: {e}")
                });
                assert_eq!(
                    has_bypass(&spec.argv),
                    posture == Posture::Bypass && !matches!(vendor, "lemonade" | "ollama"),
                    "{vendor} {posture}: {:?}",
                    spec.argv
                );
                assert!(
                    !spec
                        .argv
                        .iter()
                        .any(|a| a == "--yolo" || a == "--always-approve"),
                    "{vendor} {posture} must never emit --yolo or --always-approve: {:?}",
                    spec.argv
                );
            }
        }
    }
}
