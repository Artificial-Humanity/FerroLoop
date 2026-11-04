//! Unified JSON-RPC 2.0 MCP server (`fl vault mcp`) exposing all enabled `FerroGate` connectors
//! and zero-knowledge vault metadata tools over `stdio` or HTTP (spec §4.3).

use serde_json::{Value, json};

use fl_core::governor::Principal;

use crate::adapter::{SecretProviderAdapter, authorize_mcp_tool};
use crate::connector::{ConnectorManifest, ToolOutcome};
use crate::governor::{
    ADMISSION_UNAVAILABLE, GateGovernor, GovernedOutcome, admission_refusal_text,
};
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
            let mut entry = json!({
                "name": tool.name,
                "description": format!("[{}] {}", conn.title, tool.description),
                "inputSchema": tool.input_schema
            });
            if let Some(contract) = &tool.response_contract {
                entry["outputSchema"] = contract.schema.as_value().clone();
            }
            tools.push(entry);
        }
    }
    tools
}

/// Handle a single MCP JSON-RPC 2.0 request object (`initialize`, `ping`, `tools/list`, `tools/call`).
/// Returns `None` for JSON-RPC notifications (`notifications/initialized`).
///
/// Legacy entry point: built-in (non-connector) tools keep working, but
/// CONNECTOR tool dispatch is refused with `admission_unavailable` —
/// governed dispatch must go through
/// [`handle_mcp_jsonrpc_request_with_governor`].
pub fn handle_mcp_jsonrpc_request(
    req: &Value,
    vault: &Vault,
    connectors: &[ConnectorManifest],
    connector_filter: Option<&str>,
    caller_agent: Option<String>,
) -> Option<Value> {
    handle_mcp_jsonrpc_request_with_secret_provider(
        req,
        vault,
        connectors,
        connector_filter,
        caller_agent,
        None,
    )
}

/// [`handle_mcp_jsonrpc_request`] with an optional IAM and secret provider.
///
/// Legacy entry point: like [`handle_mcp_jsonrpc_request`], CONNECTOR
/// tool dispatch is refused with `admission_unavailable` (built-in tools
/// keep working); use [`handle_mcp_jsonrpc_request_with_governor`] for
/// governed connector dispatch.
pub fn handle_mcp_jsonrpc_request_with_secret_provider(
    req: &Value,
    vault: &Vault,
    connectors: &[ConnectorManifest],
    connector_filter: Option<&str>,
    caller_agent: Option<String>,
    secret_provider: Option<&dyn SecretProviderAdapter>,
) -> Option<Value> {
    handle_mcp_inner(
        req,
        vault,
        connectors,
        connector_filter,
        caller_agent,
        secret_provider,
        None,
        &Principal::Owner,
        "",
    )
}

/// MCP handler with a [`GateGovernor`]: connector tools are governed
/// (admit/reserve/settle); built-in list tools stay ungoverned.
/// Project binding: `project` when non-empty, otherwise the connector id
/// of the matched tool. Principal and project are supplied by the caller
/// surface (hub: Owner authority maps to `Principal::Owner`).
#[allow(clippy::too_many_arguments)]
pub fn handle_mcp_jsonrpc_request_with_governor(
    req: &Value,
    vault: &Vault,
    connectors: &[ConnectorManifest],
    connector_filter: Option<&str>,
    caller_agent: Option<String>,
    governor: Option<&GateGovernor>,
    principal: &Principal,
    project: &str,
) -> Option<Value> {
    handle_mcp_jsonrpc_request_with_governor_and_secret_provider(
        req,
        vault,
        connectors,
        connector_filter,
        caller_agent,
        governor,
        principal,
        project,
        None,
    )
}

/// [`handle_mcp_jsonrpc_request_with_governor`] with an optional IAM and
/// secret provider: the provider authorizes via `authorize_mcp_tool`
/// before governed dispatch and resolves provider-backed secrets during
/// request preparation.
#[allow(clippy::too_many_arguments)]
pub fn handle_mcp_jsonrpc_request_with_governor_and_secret_provider(
    req: &Value,
    vault: &Vault,
    connectors: &[ConnectorManifest],
    connector_filter: Option<&str>,
    caller_agent: Option<String>,
    governor: Option<&GateGovernor>,
    principal: &Principal,
    project: &str,
    secret_provider: Option<&dyn SecretProviderAdapter>,
) -> Option<Value> {
    handle_mcp_inner(
        req,
        vault,
        connectors,
        connector_filter,
        caller_agent,
        secret_provider,
        governor,
        principal,
        project,
    )
}

#[allow(clippy::too_many_arguments)]
fn handle_mcp_inner(
    req: &Value,
    vault: &Vault,
    connectors: &[ConnectorManifest],
    connector_filter: Option<&str>,
    caller_agent: Option<String>,
    secret_provider: Option<&dyn SecretProviderAdapter>,
    governor: Option<&GateGovernor>,
    principal: &Principal,
    project: &str,
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
                    // Legacy entries (no governor) never dispatch connector
                    // tools: refuse with admission_unavailable. Built-in
                    // tools above are unaffected.
                    let Some(gov) = governor else {
                        return Some(mcp_tool_text_response(id_val, ADMISSION_UNAVAILABLE, true));
                    };
                    // A secret provider, when supplied, still authorizes
                    // before governed dispatch.
                    if let Some(provider) = secret_provider
                        && let Err(err) = authorize_mcp_tool(
                            provider,
                            caller_agent.as_deref(),
                            &conn.id,
                            tool_name,
                        )
                    {
                        return Some(mcp_tool_text_response(id_val, &err.to_string(), true));
                    }
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    // Project binding: explicit project, else connector id.
                    let effective_project: &str = if project.is_empty() {
                        conn.id.as_str()
                    } else {
                        project
                    };
                    return match gov.governed_invoke_with_provider(
                        conn,
                        vault,
                        tool_name,
                        &arguments,
                        caller_agent,
                        principal.clone(),
                        effective_project,
                        now_ms,
                        // No real quota observation exists on this path.
                        None,
                        secret_provider,
                    ) {
                        Ok(GovernedOutcome::Completed {
                            response, outcome, ..
                        }) => Some(mcp_tool_outcome_response(id_val, &response, outcome)),
                        Ok(GovernedOutcome::Refused(outcome)) => Some(mcp_tool_text_response(
                            id_val,
                            &admission_refusal_text(&outcome),
                            true,
                        )),
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

/// Render a connector [`ToolOutcome`] as an MCP `tools/call` result.
///
/// Shape (documented choice): tool errors use the existing `isError: true`
/// text-result shape, not a JSON-RPC protocol error. `ContractOk` carries
/// `structuredContent` = validated value plus the legacy text envelope
/// (`{status,data,scrubbed_secrets}`) for compat. `Uncontracted` keeps the
/// legacy envelope only, marked `_meta: {uncontracted: true}`.
/// `ContractFailedAfterExecution` is an `isError` text result whose text is
/// only the typed refusal (`executed_result_withheld`, sha256, bytes,
/// violation rule) — never the executed body, in text or structured form.
fn mcp_tool_outcome_response(
    id: Value,
    resp: &crate::proxy::ProxyResponse,
    outcome: ToolOutcome,
) -> Value {
    let is_error = resp.status >= 400;
    match outcome {
        ToolOutcome::ContractOk {
            value,
            token_count,
            count_unit,
        } => {
            let mut payload = json!({"status": resp.status, "outcome": "contract_ok", "data": value, "scrubbed_secrets": resp.scrubbed_secrets});
            if let Some(tc) = token_count {
                payload["token_count"] = json!(tc);
                payload["count_unit"] = json!(count_unit);
            }
            let text = serde_json::to_string_pretty(&payload).unwrap_or_default();
            let mut r = mcp_tool_text_response(id, &text, is_error);
            r["result"]["structuredContent"] = payload["data"].clone();
            r["result"]["_meta"] = json!({"outcome": "contract_ok"});
            r
        }
        ToolOutcome::Uncontracted(value) => {
            let payload = json!({"status": resp.status, "outcome": "uncontracted", "data": value, "scrubbed_secrets": resp.scrubbed_secrets});
            let text = serde_json::to_string_pretty(&payload).unwrap_or_default();
            let mut r = mcp_tool_text_response(id, &text, is_error);
            r["result"]["_meta"] = json!({"uncontracted": true, "outcome": "uncontracted"});
            r
        }
        ToolOutcome::ContractFailedAfterExecution {
            body_sha256,
            body_bytes,
            violation,
        } => {
            let refusal = json!({"error": "executed_result_withheld", "outcome": "executed_result_withheld", "body_sha256": body_sha256, "body_bytes": body_bytes, "violation": violation});
            mcp_tool_text_response(id, &refusal.to_string(), true)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{ConnectorToolDef, evaluate_tool_response};
    use crate::contract::{ResponseContract, ResponseSchema};
    use std::collections::BTreeMap;

    fn contract() -> ResponseContract {
        ResponseContract {
            schema: ResponseSchema::from_value(json!({"type":"object","additionalProperties":false,"required":["id"],"properties":{"id":{"type":"string"}}})).unwrap(),
            max_response_bytes: 100000,
            max_response_tokens: None,
            max_depth: 32,
            max_array_len: 1000,
            max_string_len: 10000,
        }
    }
    fn manifest(contracted: bool) -> ConnectorManifest {
        ConnectorManifest {
            id: "demo".into(),
            title: "Demo".into(),
            version: "0.1.0".into(),
            description: String::new(),
            base_url: "http://127.0.0.1/".into(),
            allowed_hosts: vec![],
            default_headers: BTreeMap::new(),
            enabled: true,
            tools: vec![ConnectorToolDef {
                name: "get_thing".into(),
                description: "Get".into(),
                method: "GET".into(),
                path: "/thing".into(),
                path_params: vec![],
                query_params: vec![],
                body_params: vec![],
                headers: BTreeMap::new(),
                response_fields: vec![],
                input_schema: json!({"type":"object","properties":{}}),
                response_contract: if contracted { Some(contract()) } else { None },
            }],
            bridges: vec![],
        }
    }
    fn fake_resp(body: &str) -> crate::proxy::ProxyResponse {
        crate::proxy::ProxyResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: body.to_string(),
            scrubbed_secrets: vec![],
        }
    }

    #[test]
    fn mcp_output_schema_matches_result() {
        let m = manifest(true);
        let tools = build_mcp_tools_list(std::slice::from_ref(&m), None);
        let tool = tools.iter().find(|t| t["name"] == "get_thing").unwrap();
        assert_eq!(tool["outputSchema"], contract().schema.as_value().clone());
        // Uncontracted tool has no outputSchema.
        let tools2 = build_mcp_tools_list(&[manifest(false)], None);
        assert!(
            tools2
                .iter()
                .find(|t| t["name"] == "get_thing")
                .unwrap()
                .get("outputSchema")
                .is_none()
        );
        // ContractOk structuredContent validates against that same schema.
        let outcome = evaluate_tool_response(Some(&contract()), r#"{"id":"x1"}"#, &[]);
        let resp = mcp_tool_outcome_response(json!(1), &fake_resp(r#"{"id":"x1"}"#), outcome);
        assert_eq!(resp["result"]["structuredContent"], json!({"id":"x1"}));
        assert!(
            contract()
                .validate_value(&resp["result"]["structuredContent"])
                .is_ok()
        );
        // Uncontracted marker.
        let outcome = evaluate_tool_response(None, r#"{"id":"x1"}"#, &[]);
        let resp = mcp_tool_outcome_response(json!(1), &fake_resp(r#"{"id":"x1"}"#), outcome);
        assert_eq!(resp["result"]["_meta"]["uncontracted"], json!(true));
        assert!(resp["result"].get("structuredContent").is_none());
    }

    #[test]
    fn legacy_entry_refuses_connector_but_serves_builtins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let key = crate::load_or_init_master_key(&tmp.path().join("k")).unwrap();
        let vault = Vault::open(tmp.path().join("v.enc"), key).unwrap();
        let m = manifest(false);
        // Connector tool via the legacy entry: refused, never dispatched.
        let resp = handle_mcp_jsonrpc_request(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_thing","arguments":{}}}),
            &vault,
            std::slice::from_ref(&m),
            None,
            None,
        )
        .unwrap();
        assert_eq!(resp["result"]["isError"], json!(true));
        assert_eq!(
            resp["result"]["content"][0]["text"],
            json!("admission_unavailable")
        );
        // Built-in (non-connector) tool still works on the legacy entry.
        let resp = handle_mcp_jsonrpc_request(
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ferrogate_list_connectors","arguments":{}}}),
            &vault,
            std::slice::from_ref(&m),
            None,
            None,
        )
        .unwrap();
        assert_eq!(resp["result"]["isError"], json!(false));
        // tools/list also keeps working.
        let resp = handle_mcp_jsonrpc_request(
            &json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
            &vault,
            std::slice::from_ref(&m),
            None,
            None,
        )
        .unwrap();
        assert!(resp["result"]["tools"].is_array());
    }

    #[test]
    fn mcp_connector_public_result_parity() {
        // Same fixture body via connector outcome (CLI cmd_call is raw proxy = Uncontracted,
        // Hub bridge/CLI MCP share this path) and via MCP rendering must agree.
        for (body, ok) in [
            (r#"{"id":"x1"}"#, true),
            (r#"{"id":42,"secret_body":"BODY_SENTINEL"}"#, false),
        ] {
            let outcome = evaluate_tool_response(Some(&contract()), body, &[]);
            let mcp = mcp_tool_outcome_response(json!(1), &fake_resp(body), outcome.clone());
            match (&outcome, ok) {
                (ToolOutcome::ContractOk { value: v, .. }, true) => {
                    assert_eq!(&mcp["result"]["structuredContent"], v)
                }
                (
                    ToolOutcome::ContractFailedAfterExecution {
                        body_sha256,
                        body_bytes,
                        violation,
                    },
                    false,
                ) => {
                    assert_eq!(mcp["result"]["isError"], json!(true));
                    assert!(mcp["result"].get("structuredContent").is_none());
                    let text = mcp["result"]["content"][0]["text"].as_str().unwrap();
                    assert!(
                        text.contains("executed_result_withheld")
                            && text.contains(body_sha256)
                            && text.contains(&body_bytes.to_string())
                            && text.contains(violation.as_str())
                    );
                    assert!(
                        !text.contains("BODY_SENTINEL")
                            && !mcp.to_string().contains("BODY_SENTINEL"),
                        "body must be withheld: {mcp}"
                    );
                }
                other => panic!("unexpected {other:?} ok={ok}"),
            }
        }
    }

    #[test]
    fn governed_mcp_provider_iam_denial_reaches_authorize() {
        use crate::adapter::{ExternalSecret, IamDecision};
        use crate::governor::GateGovernor;
        use std::sync::Arc;
        struct DenyProvider;
        impl SecretProviderAdapter for DenyProvider {
            fn provider_name(&self) -> &str {
                "deny-test"
            }
            fn resolve_secret(
                &self,
                _name: &str,
                _caller: Option<&str>,
            ) -> Result<Option<ExternalSecret>, crate::GateError> {
                Ok(None)
            }
            fn check_tool_authorization(
                &self,
                _caller: Option<&str>,
                _connector: &str,
                _tool: &str,
            ) -> Result<IamDecision, crate::GateError> {
                Ok(IamDecision::Deny {
                    policy: "test-policy".into(),
                    reason: "not allowed".into(),
                })
            }
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let key = crate::load_or_init_master_key(&tmp.path().join("k")).unwrap();
        let vault = Vault::open(tmp.path().join("v.enc"), key).unwrap();
        let m = manifest(false);
        let gov = GateGovernor::new(Arc::new(fl_core::governor::MemGovernorStore::new(
            fl_core::governor::GovernorPolicy::default(),
        )));
        let resp = handle_mcp_jsonrpc_request_with_governor_and_secret_provider(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_thing","arguments":{}}}),
            &vault,
            std::slice::from_ref(&m),
            None,
            Some("felicia".into()),
            Some(&gov),
            &Principal::Owner,
            "",
            Some(&DenyProvider),
        )
        .unwrap();
        assert_eq!(resp["result"]["isError"], json!(true));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("test-policy"), "{text}");
    }
}
