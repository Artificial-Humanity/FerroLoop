pub mod antigravity;
pub mod api;
pub mod claude;
pub mod codex;
pub mod grok;
pub mod harness;
pub mod muse;

pub use antigravity::AntigravityAdapter;
pub use api::{ApiKeyResolver, HttpApiAdapter, env_key_resolver};
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use grok::GrokAdapter;
pub use harness::{CliDialect, GitSnapshot, LivenessSink, ParsedOutput, ProcessHarness};
pub use muse::MuseAdapter;

use crate::runner::{AttemptError, AttemptOutcome, AttemptSpec, DynamicRunner, Runner};
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::telemetry::{AdapterKind, RateCard};
use fl_core::vendor::{VendorRegistry, VendorTransport};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

pub enum AnyAdapter {
    Claude(ClaudeAdapter),
    Codex(CodexAdapter),
    Antigravity(AntigravityAdapter),
    Muse(MuseAdapter),
    Grok(GrokAdapter),
    /// A direct-API or local-engine vendor.
    HttpApi(Box<HttpApiAdapter>),
    /// An adapter a plugin supplied through [`register_exec_adapter`]. It is configured when it
    /// is built (see [`AdapterBuild::liveness`]); [`AnyAdapter::with_liveness`] leaves it as it is.
    Boxed(Box<dyn DynamicRunner>),
}

/// Everything [`AnyAdapter::from_vendor_id`] can set on an adapter it builds.
///
/// Not every field applies to every vendor. `model`, `base_url` and `api_key` apply to the
/// direct-API vendors (the built-in `HttpApi` adapter) and are passed to a plugin's
/// [`ExecVendorAdapter::build`]; a built-in subscription CLI (`claude`, `codex`, ...) ignores
/// them without an error. `binary_override` applies to the built-in CLIs only.
#[derive(Default, Clone)]
pub struct AdapterBuild {
    /// A subscription CLI's binary, instead of the vendor's default.
    pub binary_override: Option<String>,
    pub rate_card: Option<RateCard>,
    /// Names of parent variables a built-in CLI's child receives besides the allow-list
    /// (`[adapters.<name>] env_pass`; see [`crate::child_env`]). Other vendors ignore it.
    pub env_pass: Vec<String>,
    /// An API vendor's model, instead of the vendor's default.
    pub model: Option<String>,
    /// An API vendor's base URL (a proxy), instead of the vendor's default. It must be a place a
    /// key may be sent. The built-in `HttpApi` adapter checks it; a plugin receives it
    /// **unvalidated**.
    pub base_url: Option<String>,
    /// Where an API vendor's key comes from. `None`: the vendor's environment variable.
    pub api_key: Option<ApiKeyResolver>,
    /// `(attempt, project, agent, sink)` for liveness heartbeats.
    pub liveness: Option<(Iri, ProjectId, Option<String>, LivenessSink)>,
}

/// A plugin's way to build a runner for one vendor: a subscription CLI of its own, or an API
/// that is none of the three dialects. The vendor itself is registered in the vendor registry
/// ([`fl_core::register_vendor`]) first; this adds what runs it.
pub trait ExecVendorAdapter: Send + Sync {
    /// The vendor id this adapter runs.
    fn vendor_id(&self) -> &str;
    /// Builds the runner. `build.base_url` reaches the plugin **unvalidated**: a plugin that
    /// sends an API key to it must call [`fl_core::validate_api_base_url`] on it first, or the
    /// key can go to a plain-`http` remote host.
    fn build(&self, build: &AdapterBuild) -> Result<Box<dyn DynamicRunner>, AttemptError>;
}

/// Why an adapter was not registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegisterAdapterError {
    #[error("vendor `{0}` is built in; its adapter cannot be replaced")]
    BuiltIn(String),
    #[error("vendor `{0}` is not in the vendor registry; register it with `register_vendor` first")]
    UnknownVendor(String),
    #[error("an adapter for vendor `{0}` is already registered")]
    Duplicate(String),
}

type Plugins = RwLock<BTreeMap<String, Arc<dyn ExecVendorAdapter>>>;

fn plugins() -> &'static Plugins {
    static PLUGINS: OnceLock<Plugins> = OnceLock::new();
    PLUGINS.get_or_init(Default::default)
}

/// Registers the adapter that runs a plugin vendor. Refused for a built-in vendor, for a vendor
/// not in the vendor registry, and for one that already has an adapter.
pub fn register_exec_adapter(
    adapter: Arc<dyn ExecVendorAdapter>,
) -> Result<(), RegisterAdapterError> {
    let id = adapter.vendor_id().trim().to_ascii_lowercase();
    if VendorRegistry::is_builtin(&id) {
        return Err(RegisterAdapterError::BuiltIn(id));
    }
    if VendorRegistry::global().get(&id).is_none() {
        return Err(RegisterAdapterError::UnknownVendor(id));
    }
    let mut map = plugins().write().unwrap_or_else(|e| e.into_inner());
    if map.contains_key(&id) {
        return Err(RegisterAdapterError::Duplicate(id));
    }
    map.insert(id, adapter);
    Ok(())
}

impl AnyAdapter {
    pub fn new(
        kind: AdapterKind,
        binary_override: Option<String>,
        rate_card: Option<RateCard>,
    ) -> Self {
        let bin = binary_override.unwrap_or_else(|| kind.default_binary().to_owned());
        match kind {
            AdapterKind::Claude => Self::Claude(ClaudeAdapter::new(bin).with_rate_card(rate_card)),
            AdapterKind::Codex => Self::Codex(CodexAdapter::new(bin).with_rate_card(rate_card)),
            AdapterKind::Antigravity => {
                Self::Antigravity(AntigravityAdapter::new(bin).with_rate_card(rate_card))
            }
            AdapterKind::Muse => Self::Muse(MuseAdapter::new(bin).with_rate_card(rate_card)),
            AdapterKind::Grok => Self::Grok(GrokAdapter::new(bin).with_rate_card(rate_card)),
        }
    }

    /// The adapter for any vendor in the registry: a built-in subscription CLI, a direct-API or
    /// local-engine vendor (all registered `HttpApi` vendors), or a plugin vendor with an adapter
    /// from [`register_exec_adapter`]. A subscription-CLI vendor with no adapter is
    /// `UnknownAdapter`: fl cannot guess how to drive a CLI it has not been told about.
    ///
    /// Of `build`, a built-in CLI vendor uses only `binary_override`, `rate_card` and `liveness`:
    /// its `model`, `base_url` and `api_key` are ignored without an error. A plugin's
    /// [`ExecVendorAdapter::build`] gets all of it, with `base_url` unvalidated (see there).
    pub fn from_vendor_id(vendor_id: &str, build: AdapterBuild) -> Result<Self, AttemptError> {
        let id = vendor_id.trim().to_ascii_lowercase();
        let mut adapter = if let Some(kind) = AdapterKind::from_wire(&id) {
            Self::new(kind, build.binary_override.clone(), build.rate_card)
        } else if let Some(plugin) = plugins()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
        {
            return plugin.build(&build).map(Self::Boxed);
        } else {
            let descriptor = VendorRegistry::global()
                .get(&id)
                .ok_or_else(|| AttemptError::UnknownAdapter(vendor_id.to_string()))?;
            if matches!(
                descriptor.transport,
                VendorTransport::SubscriptionCli { .. }
            ) {
                return Err(AttemptError::UnknownAdapter(format!(
                    "{id} (a subscription CLI with no adapter; register one with \
                     `register_exec_adapter`)"
                )));
            }
            let mut api = HttpApiAdapter::new(descriptor)?.with_rate_card(build.rate_card);
            if let Some(model) = &build.model {
                api = api.with_model(model.clone());
            }
            if let Some(url) = &build.base_url {
                api = api.with_base_url(url)?;
            }
            if let Some(resolver) = &build.api_key {
                api = api.with_key_resolver(Arc::clone(resolver));
            }
            Self::HttpApi(Box::new(api))
        };
        adapter = adapter.with_env_pass(build.env_pass.clone());
        if let Some((attempt_id, project, agent, sink)) = build.liveness {
            adapter = adapter.with_liveness(attempt_id, project, agent, sink);
        }
        Ok(adapter)
    }

    /// Names parent variables to copy to a built-in CLI's child (see [`crate::child_env`]); any
    /// other adapter is left as it is.
    pub fn with_env_pass(self, names: Vec<String>) -> Self {
        match self {
            Self::Claude(a) => Self::Claude(a.with_env_pass(names)),
            Self::Codex(a) => Self::Codex(a.with_env_pass(names)),
            Self::Antigravity(a) => Self::Antigravity(a.with_env_pass(names)),
            Self::Muse(a) => Self::Muse(a.with_env_pass(names)),
            Self::Grok(a) => Self::Grok(a.with_env_pass(names)),
            other @ (Self::HttpApi(_) | Self::Boxed(_)) => other,
        }
    }

    pub fn with_liveness(
        self,
        attempt_id: Iri,
        project: ProjectId,
        agent: Option<String>,
        sink: LivenessSink,
    ) -> Self {
        match self {
            Self::Claude(a) => Self::Claude(a.with_liveness(attempt_id, project, agent, sink)),
            Self::Codex(a) => Self::Codex(a.with_liveness(attempt_id, project, agent, sink)),
            Self::Antigravity(a) => {
                Self::Antigravity(a.with_liveness(attempt_id, project, agent, sink))
            }
            Self::Muse(a) => Self::Muse(a.with_liveness(attempt_id, project, agent, sink)),
            Self::Grok(a) => Self::Grok(a.with_liveness(attempt_id, project, agent, sink)),
            Self::HttpApi(a) => {
                Self::HttpApi(Box::new(a.with_liveness(attempt_id, project, agent, sink)))
            }
            boxed @ Self::Boxed(_) => boxed,
        }
    }
}

impl Runner for AnyAdapter {
    fn id(&self) -> &str {
        match self {
            Self::Claude(a) => a.id(),
            Self::Codex(a) => a.id(),
            Self::Antigravity(a) => a.id(),
            Self::Muse(a) => a.id(),
            Self::Grok(a) => a.id(),
            Self::HttpApi(a) => a.id(),
            Self::Boxed(a) => a.runner_id(),
        }
    }

    async fn attempt(&self, spec: AttemptSpec) -> Result<AttemptOutcome, AttemptError> {
        match self {
            Self::Claude(a) => a.attempt(spec).await,
            Self::Codex(a) => a.attempt(spec).await,
            Self::Antigravity(a) => a.attempt(spec).await,
            Self::Muse(a) => a.attempt(spec).await,
            Self::Grok(a) => a.attempt(spec).await,
            Self::HttpApi(a) => a.attempt(spec).await,
            Self::Boxed(a) => a.attempt_boxed(spec).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ids::{RecordId, seq_iri};
    use fl_core::log::AttemptStatus;
    use fl_core::telemetry::LivenessStatus;
    use std::sync::{Arc, Mutex};

    #[test]
    fn any_adapter_dispatches_id_for_all_five_kinds() {
        let cases = [
            (AdapterKind::Claude, "claude"),
            (AdapterKind::Codex, "codex"),
            (AdapterKind::Antigravity, "antigravity"),
            (AdapterKind::Muse, "muse"),
            (AdapterKind::Grok, "grok"),
        ];
        for (kind, expected_id) in cases {
            let adapter = AnyAdapter::new(kind, None, None);
            assert_eq!(adapter.id(), expected_id);
        }
    }

    #[test]
    fn from_vendor_id_builds_every_registered_cli_and_api_vendor() {
        for id in ["claude", "codex", "antigravity", "muse", "grok"] {
            assert_eq!(
                AnyAdapter::from_vendor_id(id, AdapterBuild::default())
                    .unwrap()
                    .id(),
                id
            );
        }
        for id in [
            "anthropic-api",
            "openai-api",
            "gemini-api",
            "xai-api",
            "openrouter-api",
            "kimi-api",
            "ollama",
            "lemonade",
        ] {
            let a = AnyAdapter::from_vendor_id(id, AdapterBuild::default()).unwrap();
            assert!(matches!(a, AnyAdapter::HttpApi(_)), "{id}");
            assert_eq!(a.id(), id);
        }
        // Case and padding do not matter.
        assert_eq!(
            AnyAdapter::from_vendor_id(" Kimi-API ", AdapterBuild::default())
                .unwrap()
                .id(),
            "kimi-api"
        );
        assert!(matches!(
            AnyAdapter::from_vendor_id("no-such-vendor", AdapterBuild::default()),
            Err(AttemptError::UnknownAdapter(_))
        ));
        // A base URL a key must not go to is refused when the adapter is built.
        let build = AdapterBuild {
            base_url: Some("http://example.com/v1".into()),
            ..AdapterBuild::default()
        };
        assert!(AnyAdapter::from_vendor_id("kimi-api", build).is_err());
    }

    struct EchoRunner(String);

    impl Runner for EchoRunner {
        fn id(&self) -> &str {
            &self.0
        }

        async fn attempt(&self, spec: AttemptSpec) -> Result<AttemptOutcome, AttemptError> {
            Ok(AttemptOutcome {
                status: fl_core::log::AttemptStatus::Completed,
                output_excerpt: format!("echo: {}", spec.instruction),
                duration_ms: 1,
                tokens_in: 1,
                tokens_out: 1,
                cost_usd_micros: 0,
                paths_touched: vec![],
            })
        }
    }

    struct EchoPlugin(&'static str);

    impl ExecVendorAdapter for EchoPlugin {
        fn vendor_id(&self) -> &str {
            self.0
        }

        fn build(&self, _build: &AdapterBuild) -> Result<Box<dyn DynamicRunner>, AttemptError> {
            Ok(Box::new(EchoRunner(self.0.to_string())))
        }
    }

    fn cli_vendor(id: &str) -> fl_core::vendor::VendorDescriptor {
        let mut d = fl_core::vendor::lookup_vendor("claude").unwrap();
        d.id = id.into();
        d.display_name = "Plugin CLI".into();
        d.transport = VendorTransport::SubscriptionCli {
            default_binary: "plugin-cli".into(),
        };
        d.mcp_config_rel_path = None;
        d
    }

    #[tokio::test]
    async fn a_plugin_vendor_is_one_registration_and_runs_through_any_adapter() {
        let id = "exec-plugin-cli";
        // A CLI vendor with no adapter is not guessed at.
        fl_core::register_vendor(cli_vendor(id)).unwrap();
        assert!(matches!(
            AnyAdapter::from_vendor_id(id, AdapterBuild::default()),
            Err(AttemptError::UnknownAdapter(_))
        ));
        register_exec_adapter(Arc::new(EchoPlugin("exec-plugin-cli"))).unwrap();
        let a = AnyAdapter::from_vendor_id(id, AdapterBuild::default()).unwrap();
        assert!(matches!(a, AnyAdapter::Boxed(_)));
        assert_eq!(a.id(), id);
        let d = tempfile::tempdir().unwrap();
        let out = a
            .attempt(AttemptSpec {
                project_root: d.path().to_path_buf(),
                record: fl_core::ids::RecordId(fl_core::ids::seq_iri(1)),
                instruction: "ping".into(),
                timeout_secs: 5,
                budget_usd_micros: 1,
            })
            .await
            .unwrap();
        assert_eq!(out.output_excerpt, "echo: ping");
        // A second adapter for the same vendor is refused.
        assert_eq!(
            register_exec_adapter(Arc::new(EchoPlugin("exec-plugin-cli"))),
            Err(RegisterAdapterError::Duplicate(id.into()))
        );
    }

    #[test]
    fn a_plugin_cannot_replace_a_built_in_adapter_or_run_an_unregistered_vendor() {
        for id in ["claude", "kimi-api", "ollama"] {
            let plugin: Arc<dyn ExecVendorAdapter> = Arc::new(EchoPlugin(match id {
                "claude" => "claude",
                "kimi-api" => "kimi-api",
                _ => "ollama",
            }));
            assert_eq!(
                register_exec_adapter(plugin),
                Err(RegisterAdapterError::BuiltIn(id.into()))
            );
        }
        assert_eq!(
            register_exec_adapter(Arc::new(EchoPlugin("never-registered"))),
            Err(RegisterAdapterError::UnknownVendor(
                "never-registered".into()
            ))
        );
        // The claude adapter is still the real one.
        assert!(matches!(
            AnyAdapter::from_vendor_id("claude", AdapterBuild::default()).unwrap(),
            AnyAdapter::Claude(_)
        ));
    }

    /// `env_pass` through the whole path a config takes: `AdapterBuild` to `AnyAdapter` to the
    /// harness to a real spawn, against this process's real environment. `CARGO_PKG_NAME` and
    /// `CARGO_PKG_VERSION` are set by cargo for the test run and are in no allow-list.
    #[tokio::test]
    async fn env_pass_in_the_build_reaches_the_spawned_cli_and_unnamed_variables_do_not() {
        let d = tempfile::tempdir().unwrap();
        let dump = d.path().join("env.txt");
        let stub = d.path().join("env-claude.sh");
        std::fs::write(
            &stub,
            format!("#!/bin/sh\n/usr/bin/env > '{}'\n", dump.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();
        let name = std::env::var("CARGO_PKG_NAME").expect("cargo sets CARGO_PKG_NAME");
        assert!(std::env::var_os("CARGO_PKG_VERSION").is_some());

        let adapter = AnyAdapter::from_vendor_id(
            "claude",
            AdapterBuild {
                binary_override: Some(stub.to_string_lossy().into_owned()),
                env_pass: vec!["CARGO_PKG_NAME".into(), "FL_VAULT_MASTER_KEY".into()],
                ..AdapterBuild::default()
            },
        )
        .unwrap();
        let out = adapter
            .attempt(AttemptSpec {
                project_root: d.path().to_path_buf(),
                record: RecordId(seq_iri(1)),
                instruction: "x".into(),
                timeout_secs: 10,
                budget_usd_micros: 1_000_000,
            })
            .await
            .unwrap();
        assert_eq!(out.status, AttemptStatus::Completed);
        let text = std::fs::read_to_string(&dump).unwrap();
        assert!(text.contains(&format!("CARGO_PKG_NAME={name}\n")), "{text}");
        assert!(!text.contains("CARGO_PKG_VERSION="), "{text}");
        assert!(!text.contains("FL_VAULT_MASTER_KEY"), "{text}");
    }

    #[tokio::test]
    async fn any_adapter_runs_attempt_with_liveness_and_rate_card() {
        let d = tempfile::tempdir().unwrap();
        let stub = d.path().join("any-codex.sh");
        let script = r#"#!/bin/sh
printf '%s\n' '{"type":"item.completed","item":{"text":"from any adapter"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":500,"output_tokens":100}}'
"#;
        std::fs::write(&stub, script).unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let beats = Arc::new(Mutex::new(Vec::new()));
        let beats_clone = Arc::clone(&beats);
        let sink: LivenessSink = Arc::new(move |hb| {
            beats_clone.lock().unwrap().push(hb.clone());
        });

        let adapter = AnyAdapter::new(
            AdapterKind::Codex,
            Some(stub.to_string_lossy().into_owned()),
            Some(RateCard {
                input_usd_micros_per_mtok: 2_000_000,
                output_usd_micros_per_mtok: 6_000_000,
            }),
        )
        .with_liveness(
            seq_iri(10),
            ProjectId(seq_iri(20)),
            Some("ferris".into()),
            sink,
        );

        let spec = AttemptSpec {
            project_root: d.path().to_path_buf(),
            record: RecordId(seq_iri(1)),
            instruction: "run test".into(),
            timeout_secs: 5,
            budget_usd_micros: 1_000_000,
        };
        let out = adapter.attempt(spec).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Completed);
        assert_eq!(out.output_excerpt, "from any adapter");
        assert_eq!(out.tokens_in, 500);
        assert_eq!(out.tokens_out, 100);
        assert_eq!(out.cost_usd_micros, 1600);

        let recorded = beats.lock().unwrap();
        assert!(recorded.len() >= 2);
        let last = recorded.last().unwrap();
        assert_eq!(last.adapter, "codex");
        assert_eq!(last.agent.as_deref(), Some("ferris"));
        assert_eq!(last.status, LivenessStatus::Exited);
        assert_eq!(last.outcome, Some(AttemptStatus::Completed));
    }
}
