//! Unified JSON-RPC 2.0 MCP server (`fl vault mcp`) exposing all enabled `FerroGate` connectors
//! and zero-knowledge vault metadata tools over `stdio` or HTTP (spec §4.3).

use serde_json::{Value, json};

use crate::connector::ConnectorManifest;
use crate::vault::Vault;

/// Build the MCP `tools/list` array across all enabled `connectors` (plus built-in discovery tools).
pub fn build_mcp_tools_list(
    connectors: &[ConnectorManifest],
    connector_filter: Option<&str>,
) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "ferrogate_list_connectors",
            "description": "List all declarative API-to-MCP connectors registered in FerroGate",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "ferrogate_list_secrets",
            "description": "List redacted secret names and allowed-host policies stored in FerroGate (never returns secret values)",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
    ];

    for conn in connectors {
        if !conn.enabled {
            continue;
        }
        if let Some(filter) = connector_filter
            && conn.id != filter
        {
            continue;
        }
        for tool in &conn.tools {
            tools.push(json!({
                "name": tool.name,
                "description": format!("[{}] {}", conn.title, tool.description),
                "inputSchema": tool.input_schema
            }));
        }
    }
    tools
}

/// Handle a single MCP JSON-RPC 2.0 request object (`initialize`, `ping`, `tools/list`, `tools/call`).
/// Returns `None` for JSON-RPC notifications (`notifications/initialized`).
pub fn handle_mcp_jsonrpc_request(
    req: &Value,
    vault: &Vault,
    connectors: &[ConnectorManifest],
    connector_filter: Option<&str>,
    caller_agent: Option<String>,
) -> Option<Value> {
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();

    // JSON-RPC notifications have no `id`
    if id.is_none() || method.starts_with("notifications/") {
        return None;
    }
    let id_val = id.unwrap_or(Value::Null);

    match method {
        "initialize" => Some(json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "ferrogate",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        })),
        "ping" => Some(json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "result": {}
        })),
        "tools/list" => {
            let tools = build_mcp_tools_list(connectors, connector_filter);
            Some(json!({
                "jsonrpc": "2.0",
                "id": id_val,
                "result": {
                    "tools": tools
                }
            }))
        }
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
            let tool_name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            if tool_name == "ferrogate_list_connectors" {
                let summary: Vec<Value> = connectors
                    .iter()
                    .filter(|c| connector_filter.is_none_or(|f| c.id == f))
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "title": c.title,
                            "version": c.version,
                            "enabled": c.enabled,
                            "base_url": c.base_url,
                            "tools": c.tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
                            "bridges": c.bridges.iter().map(|b| &b.id).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                return Some(mcp_tool_text_response(
                    id_val,
                    &json!(summary).to_string(),
                    false,
                ));
            }

            if tool_name == "ferrogate_list_secrets" {
                let secrets = vault.list();
                return Some(mcp_tool_text_response(
                    id_val,
                    &serde_json::to_string_pretty(&secrets).unwrap_or_default(),
                    false,
                ));
            }

            for conn in connectors {
                if !conn.enabled {
                    continue;
                }
                if let Some(filter) = connector_filter
                    && conn.id != filter
                {
                    continue;
                }
                if conn.tools.iter().any(|t| t.name == tool_name) {
                    return match conn.execute_tool(vault, tool_name, &arguments, caller_agent) {
                        Ok((resp, projected)) => {
                            let is_error = resp.status >= 400;
                            let payload = json!({
                                "status": resp.status,
                                "data": projected,
                                "scrubbed_secrets": resp.scrubbed_secrets,
                            });
                            Some(mcp_tool_text_response(
                                id_val,
                                &serde_json::to_string_pretty(&payload).unwrap_or_default(),
                                is_error,
                            ))
                        }
                        Err(err) => Some(mcp_tool_text_response(id_val, &err.to_string(), true)),
                    };
                }
            }

            Some(mcp_tool_text_response(
                id_val,
                &format!("unknown FerroGate tool `{tool_name}`"),
                true,
            ))
        }
        other => Some(json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "error": {
                "code": -32601,
                "message": format!("method not found: {other}")
            }
        })),
    }
}

fn mcp_tool_text_response(id: Value, text: &str, is_error: bool) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [
                {
                    "type": "text",
                    "text": text
                }
            ],
            "isError": is_error
        }
    })
}
