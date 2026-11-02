//! The vendor registry: one descriptor per vendor, so adding a vendor is one registration and
//! not an edit to every `match` over a closed list.
//!
//! A [`VendorDescriptor`] says what a vendor *is* (how fl reaches it, what it is called, what
//! model and context window it defaults to, what its tokens cost and where its MCP file lives).
//! What fl *does* with a vendor (run an attempt, write an MCP file) is each sub-product's own
//! adapter, looked up by the descriptor's `id`: `fl-exec` for attempts, `fl-mcp` for MCP files.
//!
//! The registry is pre-populated with the five subscription CLIs ([`AdapterKind`]'s kinds), six
//! direct-API vendors and two local inference engines. [`register_vendor`] adds a vendor of
//! the caller's own (a Kimi proxy, a vLLM endpoint) and never replaces or shadows a built-in.
//!
//! ⚠ `default_rate_card` is zero for every built-in hosted vendor on purpose: fl does not ship
//! prices that go stale. A zero card prices an attempt at nothing, so `fl-exec`'s API adapter
//! refuses a hosted attempt that has no rate card (from `config.toml`) rather than run it with
//! a spend ceiling it cannot keep.
//!
//! [`AdapterKind`]: crate::telemetry::AdapterKind

use crate::telemetry::{AdapterKind, RateCard};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

/// The wire protocol an HTTP API vendor speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiDialect {
    /// `POST {base}/chat/completions`: OpenAI, xAI, OpenRouter, Kimi, Ollama, Lemonade.
    OpenAiChatCompletions,
    /// `POST {base}/v1/messages`: Anthropic.
    AnthropicMessages,
    /// `POST {base}/v1beta/models/{model}:generateContent`: Gemini.
    GeminiGenerateContent,
}

/// How fl reaches a vendor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VendorTransport {
    /// A subscription CLI fl starts as a process.
    SubscriptionCli { default_binary: String },
    /// An HTTP API (hosted with an API key, or a local inference engine without one).
    HttpApi {
        default_base_url: String,
        api_dialect: ApiDialect,
        /// The Vault secret holding the API key. `None`: the vendor needs no key.
        vault_secret_name: Option<String>,
        /// The environment variable holding the API key. `None`: the vendor needs no key.
        env_key_name: Option<String>,
    },
}

impl VendorTransport {
    /// Whether an attempt through this transport needs an API key.
    pub fn needs_api_key(&self) -> bool {
        matches!(
            self,
            Self::HttpApi {
                vault_secret_name,
                env_key_name,
                ..
            } if vault_secret_name.is_some() || env_key_name.is_some()
        )
    }
}

/// One vendor, as every sub-product sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorDescriptor {
    /// A lowercase slug: `claude`, `kimi-api`, `my-vllm`. Unique in a registry.
    pub id: String,
    pub display_name: String,
    pub transport: VendorTransport,
    /// The model an attempt asks for when none is named. Empty for a subscription CLI: the CLI
    /// chooses its own.
    pub default_model: String,
    /// Tokens. `0`: unknown, or the CLI decides.
    pub default_context_window: u64,
    /// Zero for every built-in hosted vendor; see the module note.
    pub default_rate_card: RateCard,
    /// The vendor's project MCP file, relative to the project root. `None`: it has none.
    pub mcp_config_rel_path: Option<String>,
}

/// Why a descriptor was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VendorError {
    #[error(
        "vendor id `{0}` is not a slug: use 1 to 64 lowercase letters, digits, `-`, `_` or `.`, \
         starting with a letter or digit"
    )]
    InvalidId(String),
    #[error("vendor `{0}` is already registered; a registered vendor is never replaced")]
    Duplicate(String),
    #[error("vendor `{id}`: {problem}")]
    Invalid { id: String, problem: String },
}

/// The vendors known to this process. Cheap to read; registering takes a write lock.
#[derive(Debug)]
pub struct VendorRegistry {
    vendors: RwLock<BTreeMap<String, VendorDescriptor>>,
}

impl Default for VendorRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl VendorRegistry {
    /// A registry holding the built-in vendors.
    pub fn with_builtins() -> Self {
        let vendors = builtin_vendors()
            .into_iter()
            .map(|v| (v.id.clone(), v))
            .collect();
        Self {
            vendors: RwLock::new(vendors),
        }
    }

    /// The process-wide registry that [`register_vendor`] and the sub-products use.
    pub fn global() -> &'static VendorRegistry {
        static GLOBAL: OnceLock<VendorRegistry> = OnceLock::new();
        GLOBAL.get_or_init(VendorRegistry::with_builtins)
    }

    /// Adds a vendor. Refuses a duplicate id (built-ins included), an id that is not a slug,
    /// and a descriptor that could send an API key somewhere unsafe (see
    /// [`validate_api_base_url`]).
    pub fn register(&self, descriptor: VendorDescriptor) -> Result<(), VendorError> {
        validate(&descriptor)?;
        let mut vendors = self.vendors.write().unwrap_or_else(|e| e.into_inner());
        if vendors.contains_key(&descriptor.id) {
            return Err(VendorError::Duplicate(descriptor.id));
        }
        vendors.insert(descriptor.id.clone(), descriptor);
        Ok(())
    }

    /// Like [`Self::register`], but a vendor already registered with an identical descriptor is
    /// `Ok`: loading the same configuration twice in one process must not fail. A different
    /// descriptor under a registered id is still [`VendorError::Duplicate`].
    pub fn ensure_registered(&self, descriptor: VendorDescriptor) -> Result<(), VendorError> {
        if self.get(&descriptor.id).as_ref() == Some(&descriptor) {
            return Ok(());
        }
        self.register(descriptor)
    }

    /// The vendor with this id (compared without case).
    pub fn get(&self, id: &str) -> Option<VendorDescriptor> {
        let vendors = self.vendors.read().unwrap_or_else(|e| e.into_inner());
        vendors.get(&id.trim().to_ascii_lowercase()).cloned()
    }

    /// Every vendor, by id.
    pub fn list(&self) -> Vec<VendorDescriptor> {
        let vendors = self.vendors.read().unwrap_or_else(|e| e.into_inner());
        vendors.values().cloned().collect()
    }

    /// Whether `id` names one of the built-in vendors.
    pub fn is_builtin(id: &str) -> bool {
        let id = id.trim().to_ascii_lowercase();
        builtin_vendors().iter().any(|v| v.id == id)
    }
}

/// Adds a vendor to the process-wide registry: one call, and `fl-exec` can run attempts through
/// it (HTTP API vendors) and `fl-mcp` can be told where its MCP file is.
pub fn register_vendor(descriptor: VendorDescriptor) -> Result<(), VendorError> {
    VendorRegistry::global().register(descriptor)
}

/// [`VendorRegistry::ensure_registered`] on the process-wide registry.
pub fn ensure_vendor_registered(descriptor: VendorDescriptor) -> Result<(), VendorError> {
    VendorRegistry::global().ensure_registered(descriptor)
}

/// The vendor with this id in the process-wide registry.
pub fn lookup_vendor(id: &str) -> Option<VendorDescriptor> {
    VendorRegistry::global().get(id)
}

impl AdapterKind {
    /// This kind's id in the vendor registry.
    pub const fn vendor_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::Muse => "muse",
            Self::Grok => "grok",
        }
    }
}

/// Whether `url` is a place an API key may be sent: `https://`, or `http://` to this machine
/// (`127.0.0.1`, `localhost` or `[::1]`). An authority carrying `@` (credentials in the URL), a
/// query or a fragment is refused, and so is anything that is not a URL with a host. fl sends
/// the key to this address, so a plain-`http` remote would put it on the wire in the clear.
pub fn validate_api_base_url(url: &str) -> Result<(), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("`{url}` is not a URL with a scheme"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err(format!("`{url}` names no host"));
    }
    if authority.contains('@') || authority.contains('\\') {
        return Err(format!(
            "`{url}` carries a user name or password before the host"
        ));
    }
    if url.contains('?') || url.contains('#') {
        return Err(format!("`{url}` must not carry a query or fragment"));
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed
            .find(']')
            .ok_or_else(|| format!("`{url}` has an unterminated IPv6 host"))?;
        format!("[{}]", &bracketed[..end])
    } else {
        authority.split(':').next().unwrap_or_default().to_string()
    };
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(()),
        "http" if matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") => Ok(()),
        "http" => Err(format!(
            "`{url}` is plain http to another machine; use https://, or http:// to this machine \
             (127.0.0.1, localhost or [::1])"
        )),
        other => Err(format!(
            "`{url}` uses the scheme `{other}`, not https or http"
        )),
    }
}

fn is_slug(id: &str) -> bool {
    let mut chars = id.chars();
    id.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

fn validate(d: &VendorDescriptor) -> Result<(), VendorError> {
    if !is_slug(&d.id) {
        return Err(VendorError::InvalidId(d.id.clone()));
    }
    let bad = |problem: &str| VendorError::Invalid {
        id: d.id.clone(),
        problem: problem.to_string(),
    };
    if d.display_name.trim().is_empty() {
        return Err(bad("`display_name` is empty"));
    }
    match &d.transport {
        VendorTransport::SubscriptionCli { default_binary } => {
            if default_binary.trim().is_empty() {
                return Err(bad("`default_binary` is empty"));
            }
        }
        VendorTransport::HttpApi {
            default_base_url,
            vault_secret_name,
            env_key_name,
            ..
        } => {
            validate_api_base_url(default_base_url).map_err(|e| bad(&e))?;
            if d.default_model.trim().is_empty() {
                return Err(bad("an HTTP API vendor needs a `default_model`"));
            }
            for name in [vault_secret_name, env_key_name].into_iter().flatten() {
                if name.trim().is_empty() {
                    return Err(bad("a key name is empty; use `None` for no key"));
                }
            }
        }
    }
    if let Some(path) = &d.mcp_config_rel_path {
        let escapes = path.starts_with('/')
            || path.starts_with('\\')
            || path.split(['/', '\\']).any(|part| part == "..")
            || path.trim().is_empty();
        if escapes {
            return Err(bad(
                "`mcp_config_rel_path` must be a non-empty path inside the project",
            ));
        }
    }
    Ok(())
}

fn cli(id: &str, display: &str, binary: &str, mcp: Option<&str>) -> VendorDescriptor {
    VendorDescriptor {
        id: id.to_string(),
        display_name: display.to_string(),
        transport: VendorTransport::SubscriptionCli {
            default_binary: binary.to_string(),
        },
        default_model: String::new(),
        default_context_window: 0,
        default_rate_card: RateCard::default(),
        mcp_config_rel_path: mcp.map(str::to_string),
    }
}

fn api(
    id: &str,
    display: &str,
    base: &str,
    dialect: ApiDialect,
    key: Option<&str>,
    model: &str,
    context: u64,
) -> VendorDescriptor {
    VendorDescriptor {
        id: id.to_string(),
        display_name: display.to_string(),
        transport: VendorTransport::HttpApi {
            default_base_url: base.to_string(),
            api_dialect: dialect,
            vault_secret_name: key.map(str::to_string),
            env_key_name: key.map(str::to_string),
        },
        default_model: model.to_string(),
        default_context_window: context,
        default_rate_card: RateCard::default(),
        mcp_config_rel_path: None,
    }
}

/// The vendors every registry starts with. The defaults are starting points a caller overrides
/// (a model name, a context window), not facts fl keeps current.
fn builtin_vendors() -> Vec<VendorDescriptor> {
    use ApiDialect::{AnthropicMessages, GeminiGenerateContent, OpenAiChatCompletions};
    vec![
        cli("claude", "Claude Code", "claude", Some(".mcp.json")),
        cli("codex", "Codex", "codex", Some(".codex/config.toml")),
        cli(
            "antigravity",
            "Antigravity",
            "agy",
            Some(".agents/mcp_config.json"),
        ),
        cli("muse", "Muse", "muse", Some(".muse/settings.json")),
        cli("grok", "Grok", "grok", Some(".grok/config.toml")),
        api(
            "anthropic-api",
            "Anthropic API",
            "https://api.anthropic.com",
            AnthropicMessages,
            Some("ANTHROPIC_API_KEY"),
            "claude-sonnet-5-5",
            200_000,
        ),
        api(
            "openai-api",
            "OpenAI API",
            "https://api.openai.com/v1",
            OpenAiChatCompletions,
            Some("OPENAI_API_KEY"),
            "gpt-5",
            400_000,
        ),
        api(
            "gemini-api",
            "Gemini API",
            "https://generativelanguage.googleapis.com",
            GeminiGenerateContent,
            Some("GEMINI_API_KEY"),
            "gemini-2.5-pro",
            1_000_000,
        ),
        api(
            "xai-api",
            "xAI API",
            "https://api.x.ai/v1",
            OpenAiChatCompletions,
            Some("XAI_API_KEY"),
            "grok-4",
            256_000,
        ),
        api(
            "openrouter-api",
            "OpenRouter API",
            "https://openrouter.ai/api/v1",
            OpenAiChatCompletions,
            Some("OPENROUTER_API_KEY"),
            "openrouter/auto",
            128_000,
        ),
        api(
            "kimi-api",
            "Kimi (Moonshot) API",
            "https://api.moonshot.ai/v1",
            OpenAiChatCompletions,
            Some("MOONSHOT_API_KEY"),
            "kimi-k2.5",
            256_000,
        ),
        api(
            "ollama",
            "Ollama (local)",
            "http://127.0.0.1:11434/v1",
            OpenAiChatCompletions,
            None,
            "llama3.2",
            128_000,
        ),
        api(
            "lemonade",
            "Lemonade (local)",
            "http://127.0.0.1:8000/v1",
            OpenAiChatCompletions,
            None,
            "Llama-3.2-3B-Instruct-Hybrid",
            128_000,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(id: &str, base: &str) -> VendorDescriptor {
        api(
            id,
            "Custom",
            base,
            ApiDialect::OpenAiChatCompletions,
            Some("CUSTOM_API_KEY"),
            "m",
            8_000,
        )
    }

    #[test]
    fn the_registry_starts_with_five_clis_six_apis_and_two_local_engines() {
        let reg = VendorRegistry::with_builtins();
        let ids: Vec<String> = reg.list().into_iter().map(|v| v.id).collect();
        assert_eq!(ids.len(), 13, "{ids:?}");
        for kind in [
            AdapterKind::Claude,
            AdapterKind::Codex,
            AdapterKind::Antigravity,
            AdapterKind::Muse,
            AdapterKind::Grok,
        ] {
            let d = reg.get(kind.vendor_id()).expect("a CLI vendor");
            assert_eq!(kind.vendor_id(), kind.as_wire());
            assert_eq!(
                d.transport,
                VendorTransport::SubscriptionCli {
                    default_binary: kind.default_binary().to_string()
                }
            );
            assert!(d.mcp_config_rel_path.is_some());
        }
        let kimi = reg.get("kimi-api").unwrap();
        assert_eq!(kimi.default_model, "kimi-k2.5");
        assert_eq!(
            kimi.transport,
            VendorTransport::HttpApi {
                default_base_url: "https://api.moonshot.ai/v1".into(),
                api_dialect: ApiDialect::OpenAiChatCompletions,
                vault_secret_name: Some("MOONSHOT_API_KEY".into()),
                env_key_name: Some("MOONSHOT_API_KEY".into()),
            }
        );
        for id in ["ollama", "lemonade"] {
            let d = reg.get(id).unwrap();
            assert!(!d.transport.needs_api_key(), "{id}");
            assert_eq!(d.default_rate_card, RateCard::default());
        }
        assert_eq!(
            reg.get("anthropic-api").unwrap().transport,
            VendorTransport::HttpApi {
                default_base_url: "https://api.anthropic.com".into(),
                api_dialect: ApiDialect::AnthropicMessages,
                vault_secret_name: Some("ANTHROPIC_API_KEY".into()),
                env_key_name: Some("ANTHROPIC_API_KEY".into()),
            }
        );
        assert!(
            reg.get("KIMI-API ").is_some(),
            "lookup ignores case and padding"
        );
    }

    #[test]
    fn register_adds_a_vendor_and_never_replaces_one() {
        let reg = VendorRegistry::with_builtins();
        reg.register(custom("my-vllm", "http://127.0.0.1:9000/v1"))
            .unwrap();
        assert_eq!(reg.get("my-vllm").unwrap().display_name, "Custom");
        assert_eq!(
            reg.register(custom("my-vllm", "http://127.0.0.1:9000/v1")),
            Err(VendorError::Duplicate("my-vllm".into()))
        );
        // A plugin cannot take over a built-in id.
        assert_eq!(
            reg.register(custom("claude", "https://example.com")),
            Err(VendorError::Duplicate("claude".into()))
        );
        assert_eq!(reg.get("claude").unwrap().display_name, "Claude Code");
        assert!(VendorRegistry::is_builtin("Kimi-API"));
        assert!(!VendorRegistry::is_builtin("my-vllm"));
    }

    #[test]
    fn ensure_registered_accepts_the_same_descriptor_again_but_not_a_different_one() {
        let reg = VendorRegistry::with_builtins();
        let d = custom("again", "https://e.example/v1");
        reg.ensure_registered(d.clone()).unwrap();
        reg.ensure_registered(d.clone()).unwrap();
        let mut other = d;
        other.default_model = "different".into();
        assert_eq!(
            reg.ensure_registered(other),
            Err(VendorError::Duplicate("again".into()))
        );
        // A built-in id is never "the same" as a plugin's descriptor.
        assert!(
            reg.ensure_registered(custom("claude", "https://e.example"))
                .is_err()
        );
        assert_eq!(reg.list().len(), 14);
    }

    #[test]
    fn the_global_registry_takes_a_vendor_in_one_call() {
        let id = "global-test-vendor";
        assert!(lookup_vendor(id).is_none());
        register_vendor(custom(id, "https://llm.example.com/v1")).unwrap();
        assert_eq!(lookup_vendor(id).unwrap().default_model, "m");
        assert!(register_vendor(custom(id, "https://llm.example.com/v1")).is_err());
    }

    #[test]
    fn a_bad_id_or_descriptor_is_refused() {
        let reg = VendorRegistry::with_builtins();
        for id in ["", "Upper", "has space", "-lead", "a/b", &"x".repeat(65)] {
            assert_eq!(
                reg.register(custom(id, "https://e.com")),
                Err(VendorError::InvalidId(id.to_string())),
                "{id:?}"
            );
        }
        let mut d = custom("ok", "https://e.com");
        d.default_model = "  ".into();
        assert!(matches!(reg.register(d), Err(VendorError::Invalid { .. })));
        let mut d = custom("ok", "https://e.com");
        d.mcp_config_rel_path = Some("../../etc/x".into());
        assert!(matches!(reg.register(d), Err(VendorError::Invalid { .. })));
        let mut d = custom("ok", "https://e.com");
        d.mcp_config_rel_path = Some("/etc/x".into());
        assert!(matches!(reg.register(d), Err(VendorError::Invalid { .. })));
        assert!(reg.get("ok").is_none());
    }

    #[test]
    fn a_base_url_that_could_leak_a_key_is_refused() {
        for url in [
            "http://llm.example.com/v1",
            "http://127.0.0.1:80@evil.example/v1",
            "https://user:pw@example.com/v1",
            "ftp://example.com",
            "example.com/v1",
            "https://",
            "https://example.com/v1?key=x",
            "http://localhost.evil.example/v1",
        ] {
            assert!(validate_api_base_url(url).is_err(), "{url}");
            assert!(
                matches!(
                    VendorRegistry::with_builtins().register(custom("x", url)),
                    Err(VendorError::Invalid { .. })
                ),
                "{url}"
            );
        }
        for url in [
            "https://api.example.com/v1",
            "http://127.0.0.1:11434/v1",
            "http://localhost:8000",
            "http://[::1]:8000/v1",
        ] {
            assert_eq!(validate_api_base_url(url), Ok(()), "{url}");
        }
    }

    #[test]
    fn a_descriptor_round_trips_through_serde() {
        let d = VendorRegistry::with_builtins().get("kimi-api").unwrap();
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"kind\":\"http_api\""), "{json}");
        assert!(
            json.contains("\"api_dialect\":\"open_ai_chat_completions\""),
            "{json}"
        );
        let back: VendorDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }
}
