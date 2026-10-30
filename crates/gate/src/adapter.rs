//! Tiered MCP registry and cloud relay sockets. No cloud SDK is linked here.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::GateError;
use crate::connector::ConnectorManifest;
use crate::registry::{self, connector_to_registry_entry};

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
    for adapter in adapters {
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
    use crate::connector::ConnectorManifest;
    use std::sync::Mutex;

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
}
