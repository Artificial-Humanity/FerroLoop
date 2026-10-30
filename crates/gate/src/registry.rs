//! Local MCP Registry `v0.1` formatter and router for `FerroGate` connectors (spec §4.3).
//! Compatible with `crates/mcp/src/registry.rs` (`fl mcp search`, `fl mcp add --from`, `fl mcp upgrade`).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::connector::ConnectorManifest;

const SCHEMA: &str = "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json";
const PUBLISHED_AT: &str = "2026-10-27T00:00:00Z";

/// Format a [`ConnectorManifest`] as an MCP Registry `v0.1` `ServerResponse` JSON object.
pub fn connector_to_registry_entry(manifest: &ConnectorManifest, hub_base_url: &str) -> Value {
    let reg_name = format!("io.ferroloop.gate/{}", manifest.id);
    let status = if manifest.enabled {
        "active"
    } else {
        "deprecated"
    };
    let remote_url = format!(
        "{}/api/v1/gate/mcp/{}",
        hub_base_url.trim_end_matches('/'),
        manifest.id
    );
    let desc = if manifest.description.trim().is_empty() {
        format!("FerroGate declarative MCP connector for {}", manifest.title)
    } else {
        manifest.description.clone()
    };

    json!({
        "server": {
            "$schema": SCHEMA,
            "name": reg_name,
            "title": manifest.title,
            "description": desc,
            "version": manifest.version,
            "packages": [],
            "remotes": [
                {
                    "type": "streamable-http",
                    "url": remote_url
                }
            ]
        },
        "_meta": {
            "io.modelcontextprotocol.registry/official": {
                "status": status,
                "statusChangedAt": PUBLISHED_AT,
                "publishedAt": PUBLISHED_AT,
                "updatedAt": PUBLISHED_AT,
                "isLatest": true
            }
        }
    })
}

/// Handle a `GET` request under `/v0.1/servers...` against the discovered `connectors` list,
/// returning `(http_status, json_body)`.
pub fn handle_registry_v01_get(
    subpath: &str,
    query: &BTreeMap<String, String>,
    connectors: &[ConnectorManifest],
    hub_base_url: &str,
) -> (u16, Value) {
    let entries: Vec<Value> = connectors
        .iter()
        .map(|c| connector_to_registry_entry(c, hub_base_url))
        .collect();
    respond_registry_v01(subpath, query, entries)
}

/// List, search, and detail over an already built v0.1 entry list.
pub(crate) fn respond_registry_v01(
    subpath: &str,
    query: &BTreeMap<String, String>,
    entries: Vec<Value>,
) -> (u16, Value) {
    let rest = subpath
        .trim_start_matches('/')
        .strip_prefix("v0.1/servers")
        .unwrap_or("");

    if rest.is_empty() || rest == "/" {
        let search = query.get("search").map(|s| s.to_ascii_lowercase());
        let version_filter = query.get("version").map(String::as_str);

        let found: Vec<Value> = entries
            .into_iter()
            .filter(|e| {
                let name = e["server"]["name"].as_str().unwrap_or("");
                let desc = e["server"]["description"].as_str().unwrap_or("");
                match &search {
                    Some(q) => {
                        name.to_ascii_lowercase().contains(q)
                            || desc.to_ascii_lowercase().contains(q)
                    }
                    None => true,
                }
            })
            .filter(|e| match version_filter {
                Some("latest") | None => true,
                Some(v) => e["server"]["version"].as_str() == Some(v),
            })
            .collect();

        let count = found.len();
        return (
            200,
            json!({
                "servers": found,
                "metadata": { "count": count }
            }),
        );
    }

    let segments: Vec<&str> = rest
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    let (raw_name, version_opt) = match segments.as_slice() {
        [name, "versions"] => (percent_decode(name), None),
        [name, "versions", ver] => (percent_decode(name), Some(percent_decode(ver))),
        _ => {
            return (
                404,
                json!({
                    "title": "Not Found",
                    "status": 404,
                    "detail": "Server not found"
                }),
            );
        }
    };

    let matching: Vec<Value> = entries
        .into_iter()
        .filter(|e| e["server"]["name"].as_str() == Some(raw_name.as_str()))
        .collect();

    if matching.is_empty() {
        return (
            404,
            json!({
                "title": "Not Found",
                "status": 404,
                "detail": "Server not found"
            }),
        );
    }

    match version_opt.as_deref() {
        None => {
            let count = matching.len();
            (
                200,
                json!({
                    "servers": matching,
                    "metadata": { "count": count }
                }),
            )
        }
        Some(v) => {
            let hit = matching
                .into_iter()
                .find(|e| v == "latest" || e["server"]["version"].as_str() == Some(v));
            match hit {
                Some(entry) => (200, entry),
                None => (
                    404,
                    json!({
                        "title": "Not Found",
                        "status": 404,
                        "detail": "Server version not found"
                    }),
                ),
            }
        }
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
