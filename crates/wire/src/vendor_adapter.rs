//! Unified 7-vendor CLI adapter/wrapper interface and `fl-host` session mapping.
//!
//! Covers all interactive CLI vendors hosted under `fl-host`:
//! `claude`, `codex`, `antigravity`, `grok`, `muse`, `lemonade`, and `ollama`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use fl_host_proto::{ChildState, SessionId, SessionSummary};

/// The command a runtime session starts in place of a blank shell: run as an argument vector,
/// never through a shell, so no argument needs quoting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Environment variables set for the command.
    pub env: Vec<(String, String)>,
    /// The program, then its arguments.
    pub argv: Vec<String>,
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

    /// Build the full `LaunchSpec` (`env` + `argv`) for spawning a hosted session.
    /// `thinking_level` is the vendor-native effort the operator chose; a vendor with no such
    /// launch option ignores it, and a value that is not a plain token never reaches argv.
    fn build_launch_spec(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
    ) -> LaunchSpec;

    /// Whether a launch with `thinking_level` (already a plain token) can honour it. The default
    /// refuses: a vendor with no launch option for it must not report a level it never applied.
    /// An `Err` is an actionable message for the operator.
    fn check_thinking_level(&self, _model: Option<&str>, _level: &str) -> Result<(), String> {
        Err(format!(
            "vendor `{}` does not support thinking_level; omit it",
            self.vendor_id()
        ))
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
/// sessions) plus a `PATH` that starts with `<home>/.local/bin` when that directory is not
/// already a `PATH` entry.
pub fn launch_env_for_vendor(
    vendor: &str,
    persona: &str,
    home: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if vendor != "codex" {
        env.push(("AGENT_PERSONA".into(), persona.into()));
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

    /// Any plain token is passed through (`--effort` / reasoning effort); it applies with or
    /// without `--model`.
    fn check_thinking_level(&self, _model: Option<&str>, _level: &str) -> Result<(), String> {
        Ok(())
    }

    fn build_launch_spec(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
    ) -> LaunchSpec {
        let mut argv = vec![self.resolve_binary(home), "--enable-auto-mode".to_string()];
        if let Some(m) = self.normalize_model(model) {
            argv.extend(["--model".to_string(), m]);
        }
        if let Some(level) = plain_thinking_level(thinking_level) {
            argv.extend(["--effort".to_string(), level.to_string()]);
        }
        argv.push(muse_prompt_argument(prompt));
        LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        }
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

    /// Any plain token is passed through (`--effort` / reasoning effort); it applies with or
    /// without `--model`.
    fn check_thinking_level(&self, _model: Option<&str>, _level: &str) -> Result<(), String> {
        Ok(())
    }

    fn build_launch_spec(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
    ) -> LaunchSpec {
        let mut argv = vec![self.resolve_binary(home), "--yolo".to_string()];
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
        LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        }
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

    fn build_launch_spec(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
    ) -> LaunchSpec {
        let mut argv = vec![
            self.resolve_binary(home),
            "--mode".to_string(),
            "auto".to_string(),
        ];
        let cli_model = model
            .map(|m| agy_cli_model_id(m, thinking_level))
            .and_then(|m| normalize_plain_model(Some(&m)))
            .filter(|m| split_window_tag(m).is_none());
        if let Some(m) = cli_model {
            argv.extend(["--model".to_string(), m]);
        }
        argv.push("--prompt-interactive".to_string());
        argv.push(muse_prompt_argument(prompt));
        LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        }
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

    fn build_launch_spec(
        &self,
        persona: &str,
        model: Option<&str>,
        _thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
    ) -> LaunchSpec {
        let mut argv = vec![
            self.resolve_binary(home),
            "--yolo".to_string(),
            "--always-approve".to_string(),
        ];
        if let Some(m) = self.normalize_model(model) {
            argv.extend(["--model".to_string(), m]);
        }
        argv.push(muse_prompt_argument(prompt));
        LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        }
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

    fn build_launch_spec(
        &self,
        persona: &str,
        model: Option<&str>,
        _thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
    ) -> LaunchSpec {
        let argv = vec![
            self.resolve_binary(home),
            "--model".to_string(),
            normalize_muse_model(model),
            muse_prompt_argument(prompt),
        ];
        LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        }
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

    fn build_launch_spec(
        &self,
        persona: &str,
        model: Option<&str>,
        _thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
    ) -> LaunchSpec {
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
        if !prompt.trim().is_empty() {
            argv.extend(["--system".to_string(), prompt.to_string()]);
        }
        argv.push(
            self.normalize_model(model)
                .unwrap_or_else(|| LEMONADE_DEFAULT_CLI_MODEL.to_string()),
        );
        LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        }
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

    fn build_launch_spec(
        &self,
        persona: &str,
        model: Option<&str>,
        _thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
    ) -> LaunchSpec {
        let mut argv = vec![
            self.resolve_binary(home),
            "run".to_string(),
            self.normalize_model(model)
                .unwrap_or_else(|| OLLAMA_DEFAULT_CLI_MODEL.to_string()),
        ];
        if !prompt.trim().is_empty() {
            argv.push(muse_prompt_argument(prompt));
        }
        LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        }
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

/// Live `SessionId`s in `sessions` that belong to `unit`.
///
/// - `unit == ""` or `unit == "muse"` matches nothing (the shared Muse container is never a single
///   session unit).
/// - `unit` containing `:` (`"muse:ada"`, `"Ada:claude"`) matches the exact `SessionId`
///   (`"muse.ada"`, `"Ada.claude"`).
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
        let Some(wanted) = unit_to_host_session_id(unit) else {
            return Vec::new();
        };
        return live
            .filter(|s| s.stream.session == wanted)
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
                "--enable-auto-mode",
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
            &["--enable-auto-mode", "--effort", "max", "Hi"]
        );

        let codex = lookup_vendor_cli_adapter("codex").unwrap();
        let spec = codex.build_launch_spec("ada", Some("gpt-5"), Some("xhigh"), "Hi", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--yolo",
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

        // Vendors with no launch option for it ignore the level.
        for vendor in ["grok", "muse", "lemonade", "ollama"] {
            let a = lookup_vendor_cli_adapter(vendor).unwrap();
            let with = a.build_launch_spec("ada", None, Some("high"), "Hi", None);
            let without = a.build_launch_spec("ada", None, None, "Hi", None);
            assert_eq!(with.argv, without.argv, "{vendor}");
        }
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
            for vendor in ["claude", "codex", "antigravity"] {
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
            &["--enable-auto-mode", "--model", "opus[1m]", "Hello"]
        );
        assert_eq!(claude.interrupt_key(), "Escape");
        assert_eq!(claude.reset_command(), "/clear");
        assert_eq!(claude.startup_prompt_rules().len(), 1);

        let codex = lookup_vendor_cli_adapter("codex").unwrap();
        let spec = codex.build_launch_spec("ada", Some("gpt-5"), None, "Hello", None);
        assert_eq!(&spec.argv[1..], &["--yolo", "--model", "gpt-5", "Hello"]);
        assert!(spec.env.iter().all(|(k, _)| k != "AGENT_PERSONA"));
        assert_eq!(codex.interrupt_key(), "Escape");

        let agy = lookup_vendor_cli_adapter("antigravity").unwrap();
        let spec = agy.build_launch_spec("ada", Some("claude-sonnet-5-5"), None, "Hello", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--mode",
                "auto",
                "--model",
                "claude-sonnet-5-5-medium",
                "--prompt-interactive",
                "Hello"
            ]
        );
        assert_eq!(agy.interrupt_key(), "C-c");
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
                "--yolo",
                "--always-approve",
                "--model",
                "grok-4",
                "Prompt: -start"
            ]
        );
        assert_eq!(grok.startup_prompt_rules().len(), 1);
        assert!(grok.startup_prompt_rules()[0].warn_on_miss);

        let muse = lookup_vendor_cli_adapter("muse").unwrap();
        let spec = muse.build_launch_spec("ada", None, None, "Hello", None);
        assert_eq!(&spec.argv[1..], &["--model", MUSE_DEFAULT_MODEL, "Hello"]);
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
            matching_live_host_sessions(&sessions, "muse:ada"),
            vec![SessionId::new("muse.ada").unwrap()]
        );
        assert!(matching_live_host_sessions(&sessions, "muse:bob").is_empty());
        assert!(matching_live_host_sessions(&sessions, "muse").is_empty());
    }
}
