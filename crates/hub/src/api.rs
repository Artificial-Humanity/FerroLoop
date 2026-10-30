//! Versioned `/api/v1/*` JSON REST API and static `FerroDeck` asset / SPA serving (spec §2.3, §3).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;

use fl_core::agent::{AgentRecord, AgentRole, AgentStatus};
use fl_core::arch::{
    ArchitectureModel, DEFAULT_ARCH_MANIFEST_PATH, export_likec4_dsl, validate_architecture_model,
};
use fl_core::code_index::{
    CodeSearchKind, load_token_gain, record_token_gain_query, scan_code_index,
};
use fl_core::ferrostate::{
    apply_workspace_import, create_state_checkpoint_from_probe, derive_import_slug, probe_git_repo,
    resolve_checkpoint_slug, resolve_workspace_root, scan_workspace_import,
};
use fl_core::ferrowire::{
    DeliveryStatus, MessageKind, MessageUrgency, SendWireArgs, ThreadId, WireAddress, WireFilter,
    validate_sender_name,
};
use fl_core::ids::{GateId, Kind, MessageId, WorkflowId, WorkflowRunId};
use fl_core::iri::Iri;
use fl_core::log::{AttemptStatus, GateRun};
use fl_core::pr_policy::{
    DEFAULT_OWNER_LOGIN, FlReviewMarker, PrCommitSummary, PrReviewSummary, ReviewVerdict,
    authorize_agent_action, authorize_agent_merge, evaluate_pr_policy, format_review_body,
    verify_merge_approvals,
};
use fl_core::state::{
    HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
    RuntimeEntry, SensitivityPatch, StateConcern, StateEntry, StatePayload, StateScope,
    StateSearchHit, UpsertStateArgs, search_state_entries,
};
use fl_core::store::{
    AgentRegistry, Catalog, Handles, Ledger, MessageStore, StateStore, WorkflowStore,
};
use fl_core::telemetry::{
    AttemptHeartbeat, DEFAULT_STALE_AFTER_MS, audit_transition_regret, evaluate_liveness,
    liveness_agent_slug,
};
use fl_core::workflow::{ApprovalTier, StepCompletion};
use fl_exec::stamp;
use fl_exec::workflow::{
    StepExecutionOptions, WorkflowEnv, abort_workflow_run, approve_workflow_step,
    execute_workflow_step, list_workflow_facts, list_workflow_handoffs, record_step_handoff,
    reject_workflow_step, start_workflow_run,
};
use fl_gate::{
    BridgeTarget, InboundBridgeEvent, ProxyRequest, SetSecretArgs, Vault, discover_connectors,
    evaluate_inbound_bridge_event, execute_proxied_request, handle_mcp_jsonrpc_request,
    handle_registry_v01_get, verify_inbound_bridge_webhook,
};
use fl_store::RedbStore;
use fl_wire::dispatcher::{
    MAX_DELIVER, SessionResetMode, clear_or_reforge_session, dispatch_operator_prompt_to_route,
    dispatch_to_route,
};
use fl_wire::identity::RouteTable;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::HubStoreGuard;
use crate::artifacts::{self, ArtifactEntry};
use crate::auth::{
    HubAuthority, constant_time_eq, extract_bearer_token, extract_session_cookie,
    format_logout_set_cookie, format_session_set_cookie, sha256_hex,
};
use crate::discovery::{
    DiscoveryStoreSnapshot, discover_route_sessions_without_model_info,
    discover_sessions_from_snapshot, list_git_worktrees, read_session_context_usage_from_snapshot,
    read_session_quota_usage_with_home, read_session_transcript_from_snapshot, read_worktree_diff,
    scan_cargo_architecture_conformance, snapshot_discovery_store,
};
use crate::lifecycle::{
    self, DecommissionMode, LifecycleEnv, LifecycleError, SessionTarget, SpinUpRequest,
};
use crate::nodes::{
    EnrollFail, EnrollWorker, EnsureHubArgs, HeartbeatFail, HeartbeatUpdate, NodeRecord, NodeView,
    RevokeFail, detect_capabilities, enroll_worker, ensure_hub_node, find_hub_node,
    list_visible_nodes, local_hostname, mint_enroll_token, node_id_from_hostname, node_state_slug,
    node_token_matches, revoke_node, touch_node,
};
use crate::session_actions::{
    self, METHOD_OPERATOR_PROMPT, METHOD_OVERRIDE_ONLY, ModelOverride, route_vendor,
};
use crate::settings;
pub use crate::{HubState, QueuedOperatorPrompt, SavedPromptAttachment};

/// Parsed HTTP/1.1 request passed to [`handle_http_request`].
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub raw_target: String,
    pub query: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn new(method: impl Into<String>, target: impl Into<String>) -> Self {
        let raw_target = target.into();
        let (raw_path, query) = split_target_and_query(&raw_target);
        Self {
            method: method.into().to_ascii_uppercase(),
            path: raw_path,
            raw_target,
            query,
            headers: BTreeMap::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers
            .insert(name.into().trim().to_ascii_lowercase(), value.into());
        self
    }

    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    pub fn with_json_body<T: Serialize>(mut self, value: &T) -> Self {
        self.body = serde_json::to_vec(value).unwrap_or_default();
        self.headers
            .insert("content-type".to_string(), "application/json".to_string());
        self
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        if self.body.is_empty() {
            return serde_json::from_str("{}").map_err(|e| format!("invalid empty JSON body: {e}"));
        }
        serde_json::from_slice(&self.body).map_err(|e| format!("invalid JSON body: {e}"))
    }
}

/// HTTP/1.1 response returned by [`handle_http_request`].
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub status_text: &'static str,
    pub content_type: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn json(status: u16, body: Value) -> Self {
        let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
        Self {
            status,
            status_text: status_reason(status),
            content_type: "application/json".to_string(),
            headers: Vec::new(),
            body: bytes,
        }
    }

    pub fn html(status: u16, html: impl Into<String>) -> Self {
        Self {
            status,
            status_text: status_reason(status),
            content_type: "text/html; charset=utf-8".to_string(),
            headers: Vec::new(),
            body: html.into().into_bytes(),
        }
    }

    pub fn bytes(status: u16, content_type: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            status,
            status_text: status_reason(status),
            content_type: content_type.into(),
            headers: Vec::new(),
            body,
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn json_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    pub fn to_http_bytes(&self, is_head: bool) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            self.status_text,
            self.content_type,
            self.body.len()
        );
        for (k, v) in &self.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        if !is_head {
            out.extend_from_slice(&self.body);
        }
        out
    }
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) =
                (from_hex_digit(bytes[i + 1]), from_hex_digit(bytes[i + 2]))
            {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn from_hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn split_target_and_query(raw_target: &str) -> (String, BTreeMap<String, String>) {
    let (path_part, query_part) = match raw_target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (raw_target, None),
    };
    let mut query = BTreeMap::new();
    if let Some(q) = query_part {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (percent_decode(k), percent_decode(v)),
                None => (percent_decode(pair), String::new()),
            };
            query.insert(k, v);
        }
    }
    (path_part.to_string(), query)
}

/// Parse a complete HTTP/1.1 request from `buf`, returning `Ok(None)` if more bytes are needed.
pub fn parse_http_request(buf: &[u8]) -> Result<Option<(HttpRequest, usize)>, String> {
    let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Ok(None);
    };
    let body_start = header_end + 4;
    let header_str = std::str::from_utf8(&buf[..header_end])
        .map_err(|e| format!("invalid UTF-8 in HTTP headers: {e}"))?;
    let mut lines = header_str.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| "empty HTTP request".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "missing HTTP method".to_string())?;
    let target = parts
        .next()
        .ok_or_else(|| "missing HTTP request target".to_string())?;

    let mut req = HttpRequest::new(method, target);
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            req.headers
                .insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let content_length = req
        .headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if buf.len() < body_start + content_length {
        return Ok(None);
    }
    req.body = buf[body_start..body_start + content_length].to_vec();
    Ok(Some((req, body_start + content_length)))
}

/// Handle an incoming HTTP/1.1 request against [`HubState`] (spec §2.3, §3).
pub fn handle_http_request(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let method = req.method.as_str();
    let path = req.path.as_str();

    // 1. Unauthenticated health check (`GET /api/v1/health`).
    if (method == "GET" || method == "HEAD") && path == "/api/v1/health" {
        return HttpResponse::json(
            200,
            json!({
                "status": "ok",
                "service": "fl-hub",
            }),
        );
    }

    // 2. Unauthenticated pairing exchange (`POST /api/v1/auth/pair`).
    if method == "POST" && path == "/api/v1/auth/pair" {
        return handle_auth_pair(state, req);
    }

    // Public worker enrollment. A one-time code is the credential.
    if method == "POST" && path == "/api/v1/nodes/enroll" {
        return handle_post_node_enroll(state, req);
    }

    // Worker heartbeat stays outside the general gate. A node bearer is not an
    // owner token and must not authenticate any other route.
    if method == "POST"
        && let Some(raw_id) = node_heartbeat_segment(path)
    {
        return handle_post_node_heartbeat(state, req, raw_id);
    }

    // 2b. Unauthenticated local MCP Registry v0.1 (`GET /api/v1/gate/registry/v0.1/...`).
    if (method == "GET" || method == "HEAD") && path.starts_with("/api/v1/gate/registry/") {
        let subpath = path.trim_start_matches("/api/v1/gate/registry");
        let connectors = discover_connectors(
            Some(&state.config.repo_root),
            state.config.home_override.as_deref(),
        )
        .unwrap_or_default();
        let hub_base = format!("http://{}", state.config.listen);
        let (status, body) = handle_registry_v01_get(subpath, &req.query, &connectors, &hub_base);
        return HttpResponse::json(status, body);
    }

    // 3. Static `FerroDeck` asset & SPA serving outside `/api/*` and `/ws/*`.
    if !path.starts_with("/api/") && path != "/api" && !path.starts_with("/ws/") && path != "/ws" {
        return handle_static_request(state, req);
    }

    // 4. Authenticate `/api/v1/*`. Owner and peer bearer tokens compare in memory.
    // Session tokens open the store only for that lookup, then the guard drops.
    let authority = match authenticate_api_request(state, req) {
        Ok(authority) => authority,
        Err(resp) => return resp,
    };

    match (method, path) {
        ("GET", "/api/v1/auth/session") => HttpResponse::json(
            200,
            json!({
                "authenticated": true,
                "authority": authority,
            }),
        ),
        ("POST", "/api/v1/auth/logout") => handle_auth_logout(state, req),
        ("GET", "/api/v1/agents") => handle_get_agents(state, req),
        ("GET", "/api/v1/agents/liveness") => handle_get_agents_liveness(state, req),
        ("GET", "/api/v1/sessions") => handle_get_sessions(state),
        ("GET", "/api/v1/worktrees") => match list_git_worktrees(&state.config.repo_root) {
            Ok(worktrees) => HttpResponse::json(200, json!({ "worktrees": worktrees })),
            Err(e) => HttpResponse::json(500, json!({ "error": e })),
        },
        ("GET", "/api/v1/prs") => {
            let Ok(store) = open_store_response(state) else {
                return store_open_failure();
            };
            handle_get_prs(&store)
        }
        ("GET", "/api/v1/wire/messages") => {
            let Ok(store) = open_store_response(state) else {
                return store_open_failure();
            };
            handle_get_wire_messages(&store, req)
        }
        ("POST", "/api/v1/wire/send") => handle_post_wire_send(state, &authority, req),
        ("POST", "/api/v1/wire/ack") => {
            let Ok(store) = open_store_response(state) else {
                return store_open_failure();
            };
            handle_post_wire_ack(state, &store, &authority, req)
        }
        ("GET", "/api/v1/workflows") => {
            let Ok(store) = open_store_response(state) else {
                return store_open_failure();
            };
            match store.list_workflow_defs(None) {
                Ok(workflows) => HttpResponse::json(200, json!({ "workflows": workflows })),
                Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
            }
        }
        ("GET", "/api/v1/workflow-runs") => {
            let Ok(store) = open_store_response(state) else {
                return store_open_failure();
            };
            match store.list_workflow_runs(None, None) {
                Ok(runs) => HttpResponse::json(200, json!({ "runs": runs })),
                Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
            }
        }
        ("POST", "/api/v1/workflow-runs") => {
            let Ok(store) = open_store_response(state) else {
                return store_open_failure();
            };
            handle_post_workflow_run_start(state, &store, &authority, req)
        }
        ("GET", "/api/v1/state") => {
            let Ok(store) = open_store_response(state) else {
                return store_open_failure();
            };
            handle_get_state(&store, &authority, req)
        }
        ("GET", "/api/v1/state/search") => {
            let Ok(store) = open_store_response(state) else {
                return store_open_failure();
            };
            handle_get_state_search(&store, &authority, req)
        }
        ("GET", "/api/v1/state/code-search") => handle_get_code_search(state, req),
        ("POST", "/api/v1/state") => {
            let Ok(store) = open_store_response(state) else {
                return store_open_failure();
            };
            handle_post_state(&store, &authority, req)
        }
        ("POST", "/api/v1/sessions/spin-up") => handle_post_session_spin_up(state, &authority, req),
        // Exact match, so `bulk-action` is never read as a session id by the dynamic routes.
        ("POST", "/api/v1/sessions/bulk-action") => {
            handle_post_session_bulk_action(state, &authority, req)
        }
        ("GET", "/api/v1/commands") => handle_get_commands(state),
        ("GET", "/api/v1/hosted-sessions") => handle_get_hosted_sessions(state),
        ("POST", "/api/v1/state/checkpoint") => {
            handle_post_state_checkpoint(state, &authority, req)
        }
        ("POST", "/api/v1/state/import-workspace") => {
            handle_post_state_import_workspace(state, &authority, req)
        }
        ("GET", "/api/v1/stats") => {
            let Ok(store) = open_store_response(state) else {
                return store_open_failure();
            };
            handle_get_stats(&store, req)
        }
        ("GET", "/api/v1/architecture") => handle_get_architecture(state, req),
        ("GET" | "HEAD", "/api/v1/artifacts/preview") => handle_get_artifact_preview(state, req),
        ("GET", "/api/v1/artifacts/list") => handle_get_artifacts_list(state, req),
        ("POST", "/api/v1/artifacts/prune") => handle_post_artifacts_prune(state, &authority, req),
        ("GET", "/api/v1/settings") => handle_get_settings(state),
        ("PUT", "/api/v1/settings") => handle_put_settings(state, &authority, req),
        ("GET", "/api/v1/gate/secrets") => handle_get_gate_secrets(state),
        ("POST", "/api/v1/gate/secrets") => handle_post_gate_secret(state, &authority, req),
        ("GET", "/api/v1/gate/connectors") => handle_get_gate_connectors(state),
        ("POST", "/api/v1/gate/call") => handle_post_gate_call(state, &authority, req),
        ("POST", "/api/v1/gate/mcp") => handle_post_gate_mcp(state, &authority, None, req),
        ("GET", "/api/v1/nodes") => handle_get_nodes(state),
        ("POST", "/api/v1/nodes/enroll-token") => handle_post_enroll_token(state, &authority, req),
        _ => handle_dynamic_api_routes(state, &authority, req),
    }
}

fn store_open_failure() -> HttpResponse {
    HttpResponse::json(
        500,
        json!({ "error": "failed to open store: store unavailable" }),
    )
}

fn open_store_response(state: &HubState) -> Result<HubStoreGuard<'_>, ()> {
    state.open_store().map_err(|_| ())
}

fn hub_url_for_request(state: &HubState, req: &HttpRequest) -> String {
    if let Some(host) = req.headers.get("host").map(|host| host.trim())
        && !host.is_empty()
        && !host.contains(' ')
        && !host.contains('/')
    {
        if host.starts_with("http://") || host.starts_with("https://") {
            return host.to_string();
        }
        return format!("http://{host}");
    }
    let listen = state.config.listen.trim_end_matches('/');
    format!("http://{listen}")
}

fn json_object_body(req: &HttpRequest) -> Result<Value, HttpResponse> {
    if req.body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_slice(&req.body).map_err(|err| {
        HttpResponse::json(400, json!({ "error": format!("invalid JSON body: {err}") }))
    })?;
    if !value.is_object() {
        return Err(HttpResponse::json(
            400,
            json!({ "error": "JSON body must be an object" }),
        ));
    }
    Ok(value)
}

fn json_string(body: &Value, key: &str) -> Result<String, String> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn json_string_items(key: &str, items: &[Value]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(value) = item.as_str() else {
            return Err(format!("`{key}` must be an array of strings"));
        };
        out.push(value.to_string());
    }
    Ok(out)
}

fn json_string_array(body: &Value, key: &str) -> Result<Vec<String>, String> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => json_string_items(key, items),
        Some(_) => Err(format!("`{key}` must be an array of strings")),
    }
}

fn json_optional_string_array(body: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => Ok(Some(json_string_items(key, items)?)),
        Some(_) => Err(format!("`{key}` must be an array of strings")),
    }
}

fn json_listen_addr(body: &Value) -> Result<Option<String>, String> {
    match body.get("listen_addr") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err("`listen_addr` must be a string or null".to_string()),
    }
}

fn json_optional_string(body: &Value, key: &str) -> Result<Option<String>, String> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn json_u64_or(body: &Value, key: &str, default: u64) -> Result<u64, String> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("`{key}` must be a number")),
    }
}

fn node_view_json(view: &NodeView) -> Value {
    json!({
        "node_id": view.node_id,
        "display_name": view.display_name,
        "role": view.role,
        "hostname": view.hostname,
        "os": view.os,
        "arch": view.arch,
        "listen_addr": view.listen_addr,
        "capabilities": view.capabilities,
        "labels": view.labels,
        "status": view.status,
        "enrolled_at_ms": view.enrolled_at_ms,
        "last_seen_at_ms": view.last_seen_at_ms,
    })
}

fn decode_node_id(raw_id: &str) -> Result<String, HttpResponse> {
    let decoded = percent_decode(raw_id);
    if raw_id.is_empty()
        || raw_id == "."
        || raw_id == ".."
        || raw_id.contains('/')
        || raw_id.contains('\\')
        || decoded.is_empty()
        || decoded == "."
        || decoded == ".."
        || decoded.contains('/')
        || decoded.contains('\\')
    {
        return Err(HttpResponse::json(
            400,
            json!({ "error": "invalid node id" }),
        ));
    }
    Ok(decoded)
}

fn node_heartbeat_segment(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/api/v1/nodes/")?;
    let (raw_id, tail) = rest.split_once('/')?;
    (tail == "heartbeat").then_some(raw_id)
}

/// Same slug as `nodes::node_state_slug`. Heartbeat reads the row after
/// `node_token_matches` returns false for a revoked worker.
fn stored_node_slug(node_id: &str) -> Option<String> {
    node_state_slug(node_id).ok()
}

fn revoked_node_bearer_matches(
    store: &RedbStore,
    node_id: &str,
    bearer: &str,
) -> Result<bool, String> {
    let Some(slug) = stored_node_slug(node_id) else {
        return Ok(false);
    };
    let Some(entry) = store
        .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)
        .map_err(|err| err.to_string())?
    else {
        return Ok(false);
    };
    if !entry.sensitive {
        return Ok(false);
    }
    let StatePayload::Runtime(runtime) = entry.payload else {
        return Ok(false);
    };
    let Ok(record) = serde_json::from_value::<NodeRecord>(runtime.value) else {
        return Ok(false);
    };
    if !record.revoked {
        return Ok(false);
    }
    let Some(token_hash) = record.token_hash.as_deref() else {
        return Ok(false);
    };
    Ok(constant_time_eq(&sha256_hex(bearer), token_hash))
}

fn authorize_node_heartbeat<'a>(
    state: &'a HubState,
    req: &HttpRequest,
    node_id: &str,
) -> Result<HubStoreGuard<'a>, HttpResponse> {
    let authenticated = authenticate_api_request(state, req).is_ok();
    let store = match open_store_response(state) {
        Ok(store) => store,
        Err(()) => return Err(store_open_failure()),
    };
    if authenticated {
        return Ok(store);
    }
    let bearer = req
        .headers
        .get("authorization")
        .and_then(|header| extract_bearer_token(header))
        .unwrap_or_default();
    match node_token_matches(&store, node_id, &bearer) {
        Ok(true) => Ok(store),
        Ok(false) => match revoked_node_bearer_matches(&store, node_id, &bearer) {
            Ok(true) => Ok(store),
            Ok(false) => Err(HttpResponse::json(401, json!({ "error": "unauthorized" }))),
            Err(err) => Err(HttpResponse::json(500, json!({ "error": err }))),
        },
        Err(err) => Err(HttpResponse::json(500, json!({ "error": err }))),
    }
}

fn ensure_primary_hub(state: &HubState, store: &RedbStore, now_ms: u64) -> Result<(), String> {
    let hostname = local_hostname();
    let (node_id, display_name) = match find_hub_node(store)? {
        Some(hub) => (hub.node_id, hub.display_name),
        None => (node_id_from_hostname(&hostname), hostname.clone()),
    };
    let path_env = std::env::var("PATH").unwrap_or_default();
    let listen_addr = (!state.config.listen.is_empty()).then(|| state.config.listen.clone());
    ensure_hub_node(
        store,
        EnsureHubArgs {
            node_id,
            display_name,
            hostname,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            listen_addr,
            capabilities: detect_capabilities(&path_env),
            now_ms,
        },
    )?;
    Ok(())
}

fn handle_post_node_enroll(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let worker = match enroll_request_from_body(&body) {
        Ok(worker) => worker,
        Err(error) => return HttpResponse::json(400, json!({ "error": error })),
    };
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    if let Err(err) = ensure_primary_hub(state, &store, stamp::now().unix_millis()) {
        return HttpResponse::json(500, json!({ "error": err }));
    }
    match enroll_worker(&store, worker) {
        Ok(enrolled) => HttpResponse::json(
            200,
            json!({
                "node_id": enrolled.node_id,
                "node_token": enrolled.node_token,
                "hub_url": hub_url_for_request(state, req),
            }),
        ),
        Err(EnrollFail::Invalid) => {
            HttpResponse::json(401, json!({ "error": "invalid enrollment code" }))
        }
        Err(EnrollFail::Expired) => {
            HttpResponse::json(401, json!({ "error": "enrollment code expired" }))
        }
        Err(EnrollFail::Used) => {
            HttpResponse::json(409, json!({ "error": "enrollment code already used" }))
        }
        Err(EnrollFail::Rejected(msg)) => HttpResponse::json(400, json!({ "error": msg })),
        Err(EnrollFail::Store(err)) => HttpResponse::json(500, json!({ "error": err })),
    }
}

fn enroll_request_from_body(body: &Value) -> Result<EnrollWorker, String> {
    Ok(EnrollWorker {
        code: json_string(body, "code")?,
        node_id: json_string(body, "node_id")?,
        display_name: json_string(body, "display_name")?,
        hostname: json_string(body, "hostname")?,
        os: json_string(body, "os")?,
        arch: json_string(body, "arch")?,
        listen_addr: json_listen_addr(body)?,
        capabilities: json_string_array(body, "capabilities")?,
        labels: json_string_array(body, "labels")?,
        now_ms: stamp::now().unix_millis(),
    })
}

fn handle_post_node_heartbeat(state: &HubState, req: &HttpRequest, raw_id: &str) -> HttpResponse {
    let node_id = match decode_node_id(raw_id) {
        Ok(node_id) => node_id,
        Err(resp) => return resp,
    };
    let store = match authorize_node_heartbeat(state, req, &node_id) {
        Ok(store) => store,
        Err(resp) => return resp,
    };
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let capabilities = match json_optional_string_array(&body, "capabilities") {
        Ok(capabilities) => capabilities,
        Err(error) => return HttpResponse::json(400, json!({ "error": error })),
    };
    let labels = match json_optional_string_array(&body, "labels") {
        Ok(labels) => labels,
        Err(error) => return HttpResponse::json(400, json!({ "error": error })),
    };
    match touch_node(
        &store,
        HeartbeatUpdate {
            node_id,
            capabilities,
            labels,
            now_ms: stamp::now().unix_millis(),
        },
    ) {
        Ok(view) => HttpResponse::json(
            200,
            json!({
                "node_id": view.node_id,
                "status": view.status,
                "last_seen_at_ms": view.last_seen_at_ms,
            }),
        ),
        Err(HeartbeatFail::NotFound) => {
            HttpResponse::json(404, json!({ "error": "node not found" }))
        }
        Err(HeartbeatFail::Revoked) => HttpResponse::json(403, json!({ "error": "node revoked" })),
        Err(HeartbeatFail::Store(err)) => HttpResponse::json(500, json!({ "error": err })),
    }
}

fn handle_get_nodes(state: &HubState) -> HttpResponse {
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let now_ms = stamp::now().unix_millis();
    if let Err(err) = ensure_primary_hub(state, &store, now_ms) {
        return HttpResponse::json(500, json!({ "error": err }));
    }
    match list_visible_nodes(&store, now_ms) {
        Ok(nodes) => {
            let nodes: Vec<Value> = nodes.iter().map(node_view_json).collect();
            HttpResponse::json(200, json!({ "nodes": nodes }))
        }
        Err(err) => HttpResponse::json(500, json!({ "error": err })),
    }
}

fn handle_post_enroll_token(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(403, json!({ "error": "owner authority required" }));
    }
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let label = match json_optional_string(&body, "label") {
        Ok(label) => label,
        Err(error) => return HttpResponse::json(400, json!({ "error": error })),
    };
    let ttl_secs = match json_u64_or(&body, "ttl_secs", 600) {
        Ok(ttl_secs) => ttl_secs,
        Err(error) => return HttpResponse::json(400, json!({ "error": error })),
    };
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match mint_enroll_token(
        &store,
        label,
        ttl_secs,
        &hub_url_for_request(state, req),
        stamp::now().unix_millis(),
    ) {
        Ok(info) => HttpResponse::json(
            200,
            json!({
                "code": info.code,
                "expires_at_ms": info.expires_at_ms,
                "ttl_secs": info.ttl_secs,
                "join_command": info.join_command,
            }),
        ),
        Err(err) => HttpResponse::json(500, json!({ "error": err })),
    }
}

fn handle_delete_node(state: &HubState, authority: &HubAuthority, raw_id: &str) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(403, json!({ "error": "owner authority required" }));
    }
    let node_id = match decode_node_id(raw_id) {
        Ok(node_id) => node_id,
        Err(resp) => return resp,
    };
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match revoke_node(&store, &node_id) {
        Ok(record) => HttpResponse::json(
            200,
            json!({
                "revoked": true,
                "node_id": record.node_id,
            }),
        ),
        Err(RevokeFail::NotFound) => HttpResponse::json(404, json!({ "error": "node not found" })),
        Err(RevokeFail::IsHub) => HttpResponse::json(
            400,
            json!({ "error": "cannot revoke the primary hub node" }),
        ),
        Err(RevokeFail::Store(err)) => HttpResponse::json(500, json!({ "error": err })),
    }
}

fn authenticate_api_request(
    state: &HubState,
    req: &HttpRequest,
) -> Result<HubAuthority, HttpResponse> {
    if let Some(authority) = state
        .auth
        .authenticate_request(None, &req.headers, &req.query)
    {
        return Ok(authority);
    }
    let Ok(store) = open_store_response(state) else {
        return Err(HttpResponse::json(
            500,
            json!({ "error": "failed to open store: store unavailable" }),
        ));
    };
    state
        .auth
        .authenticate_request(Some(&*store), &req.headers, &req.query)
        .ok_or_else(|| HttpResponse::json(401, json!({ "error": "unauthorized" })))
}

fn handle_auth_logout(state: &HubState, req: &HttpRequest) -> HttpResponse {
    if let Some(tok) = req
        .headers
        .get("cookie")
        .and_then(|c| extract_session_cookie(c))
        .or_else(|| {
            req.headers
                .get("authorization")
                .and_then(|a| extract_bearer_token(a))
        })
        && let Ok(store) = state.open_store()
    {
        state.auth.revoke_session_token(Some(&*store), &tok);
    }
    HttpResponse::json(200, json!({ "authenticated": false }))
        .with_header("Set-Cookie", format_logout_set_cookie())
}

fn load_discovery_snapshot(state: &HubState) -> DiscoveryStoreSnapshot {
    match state.open_store() {
        Ok(store) => snapshot_discovery_store(&store),
        Err(_) => DiscoveryStoreSnapshot::default(),
    }
}

fn handle_get_sessions(state: &HubState) -> HttpResponse {
    drain_ready_session_queues(state);
    let snap = match state.open_store() {
        Ok(store) => snapshot_discovery_store(&store),
        Err(_) => return store_open_failure(),
    };
    let sessions = state.with_model_overrides(discover_sessions_from_snapshot(
        &snap,
        state.config.home_override.as_deref(),
    ));
    HttpResponse::json(200, json!({ "sessions": sessions }))
}

fn handle_auth_pair(state: &HubState, req: &HttpRequest) -> HttpResponse {
    #[derive(Deserialize, Default)]
    struct PairReq {
        code: Option<String>,
        token: Option<String>,
    }
    let parsed: PairReq = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let store = state.open_store().ok();
    match state.auth.exchange_pairing_or_token(
        store.as_deref(),
        parsed.code.as_deref(),
        parsed.token.as_deref(),
    ) {
        Ok((session_token, authority, ttl_secs)) => HttpResponse::json(
            200,
            json!({
                "authenticated": true,
                "authority": authority,
                "session_token": session_token,
            }),
        )
        .with_header(
            "Set-Cookie",
            format_session_set_cookie(&session_token, ttl_secs),
        ),
        Err(reason) => HttpResponse::json(
            401,
            json!({
                "error": "unauthorized",
                "detail": reason,
            }),
        ),
    }
}

fn has_path_traversal(raw_path: &str, decoded_path: &str) -> bool {
    for candidate in [raw_path, decoded_path] {
        if candidate.contains('\0') || candidate.contains('\\') || candidate.contains("..") {
            return true;
        }
        for seg in candidate.split('/') {
            if seg == ".." || seg == "." {
                return true;
            }
        }
    }
    false
}

fn mime_type_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("pdf") => "application/pdf",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("wasm") => "application/wasm",
        Some("txt" | "c4" | "md" | "log") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn handle_static_request(state: &HubState, req: &HttpRequest) -> HttpResponse {
    if req.method != "GET" && req.method != "HEAD" {
        return HttpResponse::json(405, json!({ "error": "method not allowed" }));
    }

    let decoded = percent_decode(&req.path);
    if has_path_traversal(&req.path, &decoded) {
        return HttpResponse::json(400, json!({ "error": "invalid path" }));
    }

    if let Some(ref ui_dir) = state.config.ui_dir
        && ui_dir.is_dir()
    {
        let rel = decoded.trim_start_matches('/');
        let target_rel = if rel.is_empty() { "index.html" } else { rel };
        let candidate = ui_dir.join(target_rel);
        if candidate.is_file()
            && let Ok(bytes) = fs::read(&candidate)
        {
            return HttpResponse::bytes(200, mime_type_for_path(&candidate), bytes);
        }

        let index_path = ui_dir.join("index.html");
        if index_path.is_file()
            && let Ok(bytes) = fs::read(&index_path)
        {
            return HttpResponse::bytes(200, "text/html; charset=utf-8", bytes);
        }

        return HttpResponse::json(404, json!({ "error": "not found" }));
    }

    HttpResponse::html(
        200,
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>FerroDeck Hub</title></head>\
         <body><h1>FerroDeck Hub Online</h1><p>Static UI bundle not mounted; API is active at <code>/api/v1/health</code>.</p></body></html>",
    )
}

fn handle_get_agents(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let project_filter = req.query.get("project").map(String::as_str);
    let include_retired = req
        .query
        .get("include_retired")
        .is_some_and(|v| v == "true" || v == "1");
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let agents = match store.list_agents(project_filter, include_retired) {
        Ok(a) => a,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };
    let areas = match store.list_areas() {
        Ok(a) => a,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };
    let snap = snapshot_discovery_store(&store);
    drop(store);
    let routes = state.with_model_overrides(discover_sessions_from_snapshot(
        &snap,
        state.config.home_override.as_deref(),
    ));
    HttpResponse::json(
        200,
        json!({
            "agents": agents,
            "areas": areas,
            "routes": routes,
        }),
    )
}

fn handle_get_agents_liveness(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let stale_after_ms = req
        .query
        .get("stale_after_ms")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_STALE_AFTER_MS);
    let now = stamp::now();
    let local_host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string());

    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let agents = store.list_agents(None, false).unwrap_or_default();
    let mut heartbeats = Vec::with_capacity(agents.len());
    for ag in &agents {
        let slug = liveness_agent_slug(&ag.name_key);
        let hb: Option<AttemptHeartbeat> = store
            .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)
            .ok()
            .flatten()
            .and_then(|entry| match entry.payload {
                StatePayload::Runtime(rt) => serde_json::from_value(rt.value).ok(),
                _ => None,
            });
        heartbeats.push(hb);
    }
    let snap = snapshot_discovery_store(&store);
    drop(store);
    // Liveness reads only `source`, `persona` and `adapter`: no model discovery.
    let sessions =
        discover_route_sessions_without_model_info(&snap, state.config.home_override.as_deref());

    let mut items = Vec::with_capacity(agents.len());
    for (ag, hb) in agents.iter().zip(heartbeats.iter()) {
        let verdict = evaluate_liveness(hb.as_ref(), &now, stale_after_ms, &local_host, |pid| {
            Path::new(&format!("/proc/{pid}")).exists()
        });
        let active_route = sessions
            .iter()
            .find(|s| s.source == "route" && s.persona.as_deref() == Some(ag.name_key.as_str()));
        items.push(json!({
            "agent": ag.name_key,
            "project": ag.project,
            "role": ag.role,
            "has_route": active_route.is_some(),
            "adapter": active_route.map(|r| r.adapter.as_str()).or_else(|| hb.as_ref().map(|h| h.adapter.as_str())),
            "verdict": verdict,
        }));
    }

    HttpResponse::json(200, json!({ "liveness": items }))
}

fn handle_get_prs(store: &RedbStore) -> HttpResponse {
    let mut prs = Vec::new();
    if let Ok(entries) = store.list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))
    {
        for entry in entries {
            if entry.slug.starts_with("pr-")
                && let StatePayload::Runtime(rt) = entry.payload
            {
                prs.push(rt.value);
            }
        }
    }
    HttpResponse::json(200, json!({ "prs": prs }))
}

fn handle_get_wire_messages(store: &RedbStore, req: &HttpRequest) -> HttpResponse {
    let unread_only = req
        .query
        .get("unread")
        .is_some_and(|v| v == "true" || v == "1");
    let thread_id = match req.query.get("thread") {
        Some(raw) if !raw.trim().is_empty() => match Iri::parse(raw.trim()) {
            Ok(iri) => Some(MessageId(iri)),
            Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
        },
        _ => None,
    };

    if let Some(recipient) = req
        .query
        .get("to")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let filter = WireFilter {
            recipient: Some(recipient.to_string()),
            unread_only,
            thread_id,
            correlation_id: None,
            limit: None,
        };
        return match store.list_wire_inbox(&filter) {
            Ok(items) => HttpResponse::json(200, json!({ "messages": items })),
            Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
        };
    }

    if let Some(ref tid) = thread_id {
        return match store.list_wire_thread(tid) {
            Ok(msgs) => HttpResponse::json(200, json!({ "messages": msgs })),
            Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
        };
    }

    match store.wire_messages_after(0) {
        Ok(seq_msgs) => {
            let mut out = Vec::with_capacity(seq_msgs.len());
            for (_seq, msg) in seq_msgs {
                let receipts = store.list_wire_receipts(&msg.id).unwrap_or_default();
                if unread_only && receipts.iter().all(|r| r.status == DeliveryStatus::Acked) {
                    continue;
                }
                out.push(json!({
                    "message": msg,
                    "receipts": receipts,
                }));
            }
            HttpResponse::json(200, json!({ "messages": out }))
        }
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

fn handle_post_wire_send(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize)]
    struct SendPayload {
        to: Value,
        subject: String,
        body: String,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        sender_name: Option<String>,
        #[serde(default)]
        kind: Option<MessageKind>,
        #[serde(default)]
        urgency: Option<MessageUrgency>,
        #[serde(default)]
        durable: Option<bool>,
        #[serde(default)]
        reply_to: Option<MessageId>,
        #[serde(default)]
        thread_id: Option<ThreadId>,
    }

    let payload: SendPayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let to: WireAddress = match &payload.to {
        Value::String(s) => match WireAddress::parse(s) {
            Ok(addr) => addr,
            Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
        },
        Value::Array(arr) if arr.len() == 1 && arr[0].is_string() => {
            match WireAddress::parse(arr[0].as_str().unwrap_or_default()) {
                Ok(addr) => addr,
                Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
            }
        }
        other => match serde_json::from_value(other.clone()) {
            Ok(addr) => addr,
            Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
        },
    };

    // A Peer can never send as the owner: `resolve_prompt_sender` rewrites an owner or unusable
    // claim to the Peer's own authenticated name (or `peer`). An Owner keeps its claimed label.
    let sender_name = resolve_prompt_sender(
        authority,
        payload.sender_name.as_deref().or(payload.from.as_deref()),
    );
    let now = stamp::now();
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let sender_id = store
        .agent_by_name(&sender_name)
        .ok()
        .flatten()
        .map(|a| a.id);

    let args = SendWireArgs {
        id: None,
        sender_id,
        sender_name,
        sender_host: None,
        to,
        kind: payload.kind.unwrap_or(MessageKind::Mail),
        urgency: payload.urgency.unwrap_or(MessageUrgency::Normal),
        durable: payload.durable.unwrap_or(true),
        subject: payload.subject,
        body: payload.body,
        reply_to: payload.reply_to,
        thread_id: payload.thread_id,
        deadline_at: None,
        at: now.clone(),
    };

    let (msg, mut receipts) = match store.send_wire(args) {
        Ok(res) => res,
        Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
    };
    drop(store);

    let route_override = state
        .config
        .home_override
        .as_ref()
        .map(|h| h.join(".cache").join("agent-bus").join("routes.json"));
    let mut routes = RouteTable::load(route_override.as_deref());
    routes.discover_routes(state.config.home_override.as_deref(), now.clone());
    let mut any_updated = false;
    for rcpt in &receipts {
        if matches!(rcpt.status, DeliveryStatus::Acked | DeliveryStatus::Expired)
            || rcpt.attempts >= MAX_DELIVER
        {
            continue;
        }
        let Some(entry) = routes.get(&rcpt.recipient_name).cloned() else {
            continue;
        };
        let target_status = match dispatch_to_route(&entry.route, &msg) {
            Ok(()) => DeliveryStatus::Acked,
            Err(_) => DeliveryStatus::Pending,
        };
        if let Ok(store) = state.open_store()
            && store
                .update_wire_receipt(
                    &msg.id,
                    &rcpt.recipient_name,
                    target_status,
                    true,
                    now.clone(),
                )
                .ok()
                .flatten()
                .is_some()
        {
            any_updated = true;
        }
    }
    if any_updated
        && let Ok(store) = state.open_store()
        && let Ok(fresh) = store.list_wire_receipts(&msg.id)
    {
        receipts = fresh;
    }

    let event_data = json!({
        "message": msg,
        "receipts": receipts,
    });
    state.broadcast("wire", "message_sent", event_data.clone());
    HttpResponse::json(200, event_data)
}

fn resolve_message_id(store: &RedbStore, raw: &str) -> Result<MessageId, String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("urn:uuid:") {
        let iri = Iri::parse(trimmed).map_err(|e| e.to_string())?;
        return Ok(MessageId(iri));
    }
    if let Ok(handle) = trimmed.parse::<u64>() {
        let iri = store
            .resolve_handle(Kind::Message, handle)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no wire message with handle {handle}"))?;
        return Ok(MessageId(iri));
    }
    Err(format!("invalid message identifier `{raw}`"))
}

fn handle_post_wire_ack(
    state: &HubState,
    store: &RedbStore,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize)]
    struct AckPayload {
        #[serde(alias = "message_id")]
        id: String,
        #[serde(default, alias = "recipient")]
        by: Option<String>,
    }

    let payload: AckPayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let msg_id = match resolve_message_id(store, &payload.id) {
        Ok(id) => id,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let by = payload
        .by
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| authority.actor_name())
        .to_ascii_lowercase();

    match store.update_wire_receipt(&msg_id, &by, DeliveryStatus::Acked, false, stamp::now()) {
        Ok(receipt) => {
            let data = json!({
                "message_id": msg_id,
                "by": by,
                "receipt": receipt,
            });
            state.broadcast("wire", "message_acked", data.clone());
            HttpResponse::json(200, data)
        }
        Err(e) => HttpResponse::json(400, json!({ "error": e.to_string() })),
    }
}

fn resolve_workflow_def_in_store(
    store: &RedbStore,
    raw: &str,
) -> Result<fl_core::workflow::WorkflowDef, String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("urn:uuid:") {
        let iri = Iri::parse(trimmed).map_err(|e| e.to_string())?;
        return store
            .get_workflow_def(&WorkflowId(iri))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no workflow definition `{trimmed}`"));
    }
    if let Ok(handle) = trimmed.parse::<u64>() {
        let iri = store
            .resolve_handle(Kind::Workflow, handle)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no workflow definition with handle {handle}"))?;
        return store
            .get_workflow_def(&WorkflowId(iri))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no workflow definition `{trimmed}`"));
    }
    let defs = store.list_workflow_defs(None).map_err(|e| e.to_string())?;
    defs.into_iter()
        .find(|d| d.slug == trimmed)
        .ok_or_else(|| format!("no workflow definition matching `{trimmed}`"))
}

fn resolve_workflow_run_id(store: &RedbStore, raw: &str) -> Result<WorkflowRunId, String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("urn:uuid:") {
        let iri = Iri::parse(trimmed).map_err(|e| e.to_string())?;
        return Ok(WorkflowRunId(iri));
    }
    if let Ok(handle) = trimmed.parse::<u64>() {
        let iri = store
            .resolve_handle(Kind::WorkflowRun, handle)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no workflow run with handle {handle}"))?;
        return Ok(WorkflowRunId(iri));
    }
    Err(format!("invalid workflow run identifier `{raw}`"))
}

fn handle_post_workflow_run_start(
    state: &HubState,
    store: &RedbStore,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize)]
    struct StartRunPayload {
        #[serde(alias = "workflow_id")]
        workflow: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        by: Option<String>,
    }

    let payload: StartRunPayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let def = match resolve_workflow_def_in_store(store, &payload.workflow) {
        Ok(d) => d,
        Err(e) => return HttpResponse::json(404, json!({ "error": e })),
    };

    let title = payload
        .title
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| def.title.clone());
    let by = payload
        .by
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| authority.actor_name().to_string());

    match start_workflow_run(store, &def, title, stamp::now(), by) {
        Ok(run) => {
            let data = json!({ "run": run });
            state.broadcast("workflow", "run_updated", data.clone());
            HttpResponse::json(200, data)
        }
        Err(e) => HttpResponse::json(400, json!({ "error": e.to_string() })),
    }
}

fn redact_sensitive_entry(mut entry: StateEntry) -> StateEntry {
    if !entry.sensitive {
        return entry;
    }
    entry.payload = match entry.payload {
        StatePayload::Memory(mut m) => {
            m.summary = "[REDACTED]".to_string();
            m.body = "[REDACTED]".to_string();
            StatePayload::Memory(m)
        }
        StatePayload::Handoff(mut h) => {
            h.summary = "[REDACTED]".to_string();
            h.notes = Some("[REDACTED]".to_string());
            h.shipped.clear();
            h.in_flight.clear();
            h.blockers.clear();
            h.next_steps.clear();
            StatePayload::Handoff(h)
        }
        StatePayload::Runtime(mut r) => {
            r.value = json!("[REDACTED]");
            StatePayload::Runtime(r)
        }
    };
    entry
}

fn handle_get_state(
    store: &RedbStore,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    let scope = match req.query.get("scope") {
        Some(s) if !s.trim().is_empty() => match StateScope::from_wire(s.trim()) {
            Ok(sc) => Some(sc),
            Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
        },
        _ => None,
    };
    let concern = match req.query.get("concern") {
        Some(c) if !c.trim().is_empty() => match StateConcern::from_wire(c.trim()) {
            Some(cn) => Some(cn),
            None => {
                return HttpResponse::json(
                    400,
                    json!({ "error": format!("unknown state concern `{c}`") }),
                );
            }
        },
        _ => None,
    };

    let entries = match store.list_state(scope.as_ref(), concern) {
        Ok(list) => list,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };

    let filtered: Vec<StateEntry> = entries
        .into_iter()
        .filter(|e| !(e.scope == StateScope::Workspace && is_reserved_hub_slug(&e.slug)))
        .map(|e| {
            if authority.is_owner() {
                e
            } else {
                redact_sensitive_entry(e)
            }
        })
        .collect();

    HttpResponse::json(200, json!({ "entries": filtered }))
}

fn is_reserved_hub_slug(slug: &str) -> bool {
    slug.starts_with("hub-pairing-")
        || slug.starts_with("hub-session-")
        || slug.starts_with("hub-node-")
}

fn is_internal_hub_entry(entry: &StateEntry) -> bool {
    entry.scope == StateScope::Workspace && is_reserved_hub_slug(&entry.slug)
}

fn redact_search_hit(mut hit: StateSearchHit, is_owner: bool) -> StateSearchHit {
    if !is_owner && hit.entry.sensitive {
        hit.entry = redact_sensitive_entry(hit.entry);
        hit.snippet = "[REDACTED]".to_string();
    }
    hit
}

/// Rank hub search candidates: internal hub entries excluded, full ranked
/// count returned before `limit` truncation, sensitive hits redacted for
/// non-owners (entry and snippet).
fn search_state_for_hub(
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
    is_owner: bool,
) -> (Vec<StateSearchHit>, usize) {
    let candidates: Vec<StateEntry> = entries
        .iter()
        .filter(|e| !is_internal_hub_entry(e))
        .cloned()
        .collect();
    let all = search_state_entries(&candidates, query, scope, concern, usize::MAX);
    let total_hits = all.len();
    let hits: Vec<StateSearchHit> = all
        .into_iter()
        .take(limit)
        .map(|hit| redact_search_hit(hit, is_owner))
        .collect();
    (hits, total_hits)
}

fn handle_get_state_search(
    store: &RedbStore,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    let scope = match req.query.get("scope") {
        Some(s) if !s.trim().is_empty() => match StateScope::from_wire(s.trim()) {
            Ok(sc) => Some(sc),
            Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
        },
        _ => None,
    };
    let concern = match req.query.get("concern") {
        Some(c) if !c.trim().is_empty() => match StateConcern::from_wire(c.trim()) {
            Some(cn) => Some(cn),
            None => {
                return HttpResponse::json(
                    400,
                    json!({ "error": format!("unknown state concern `{c}`") }),
                );
            }
        },
        _ => None,
    };
    let limit = match req.query.get("limit") {
        Some(s) if !s.trim().is_empty() => match s.trim().parse::<usize>() {
            Ok(n) => n.min(100),
            Err(_) => {
                return HttpResponse::json(
                    400,
                    json!({ "error": format!("invalid `limit` `{s}`; expected a non-negative integer") }),
                );
            }
        },
        _ => 10,
    };

    let query = req.query.get("q").cloned().unwrap_or_default();
    if query.trim().is_empty() {
        return HttpResponse::json(200, json!({ "query": query, "hits": [], "total_hits": 0 }));
    }

    let entries = match store.list_state(scope.as_ref(), concern) {
        Ok(list) => list,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };

    let (hits, total_hits) = search_state_for_hub(
        &entries,
        &query,
        scope.as_ref(),
        concern,
        limit,
        authority.is_owner(),
    );

    HttpResponse::json(
        200,
        json!({ "query": query, "hits": hits, "total_hits": total_hits }),
    )
}

fn handle_get_code_search(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let kind = match req.query.get("kind") {
        Some(raw) if !raw.trim().is_empty() => match CodeSearchKind::from_wire(raw.trim()) {
            Some(k) => k,
            None => {
                return HttpResponse::json(
                    400,
                    json!({
                        "error": format!(
                            "unknown code search kind `{raw}`; expected one of: {}",
                            CodeSearchKind::wire_values()
                        )
                    }),
                );
            }
        },
        _ => CodeSearchKind::Search,
    };
    // Per-kind defaults mirror the CLI/MCP surfaces: 50 def/search,
    // 500 outline, capped at 1000; an explicit 0 stays 0.
    let default_limit = match kind {
        CodeSearchKind::Outline => 500,
        CodeSearchKind::Def | CodeSearchKind::Search => 50,
    };
    let limit = match req.query.get("limit") {
        Some(raw) if !raw.trim().is_empty() => match raw.trim().parse::<usize>() {
            Ok(n) => n.min(1000),
            Err(_) => {
                return HttpResponse::json(
                    400,
                    json!({ "error": format!("invalid `limit` `{raw}`; expected a non-negative integer") }),
                );
            }
        },
        _ => default_limit,
    };

    let query = req.query.get("q").cloned().unwrap_or_default();
    // Missing/blank `q`, and an explicit `limit` of 0, return no symbols
    // and record nothing; `token_gain` is the stored ledger unchanged.
    if query.trim().is_empty() || limit == 0 {
        let Ok(store) = open_store_response(state) else {
            return store_open_failure();
        };
        return HttpResponse::json(
            200,
            json!({
                "query": query,
                "kind": kind.as_wire(),
                "symbols": [],
                "tokens_saved_this_query": 0,
                "token_gain": load_token_gain(&*store),
            }),
        );
    }

    let outcome = scan_code_index(&state.config.repo_root).search(kind, &query, limit);
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let token_gain = match record_token_gain_query(
        &*store,
        outcome.tokens_saved_this_query,
        outcome.files_indexed,
        outcome.symbols_indexed,
        stamp::now(),
        "fl",
    ) {
        Ok(gain) => gain,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };
    HttpResponse::json(
        200,
        json!({
            "query": query,
            "kind": kind.as_wire(),
            "symbols": outcome.symbols,
            "tokens_saved_this_query": outcome.tokens_saved_this_query,
            "token_gain": token_gain,
        }),
    )
}

fn handle_post_state(
    store: &RedbStore,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    let raw: Value = match req.json() {
        Ok(v) => v,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let Some(obj) = raw.as_object() else {
        return HttpResponse::json(400, json!({ "error": "expected JSON object" }));
    };

    let scope_str = obj
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("workspace");
    let scope = match StateScope::from_wire(scope_str) {
        Ok(s) => s,
        Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
    };

    let Some(slug) = obj
        .get("slug")
        .or_else(|| obj.get("key"))
        .and_then(Value::as_str)
    else {
        return HttpResponse::json(400, json!({ "error": "missing `slug` or `key`" }));
    };

    let normalized_slug = slug.trim().to_ascii_lowercase();
    if is_reserved_hub_slug(&normalized_slug) {
        return HttpResponse::json(
            403,
            json!({ "error": "reserved internal hub authentication slug prefix" }),
        );
    }

    let sensitive_flag = obj.get("sensitive").and_then(Value::as_bool);
    if sensitive_flag == Some(true) && !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "owner authority required to write sensitive state entries" }),
        );
    }

    let payload: StatePayload = if let Some(p_val) = obj.get("payload") {
        match serde_json::from_value(p_val.clone()) {
            Ok(p) => p,
            Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
        }
    } else {
        let concern_str = obj
            .get("concern")
            .and_then(Value::as_str)
            .unwrap_or("runtime");
        match concern_str {
            "memory" => {
                let category = obj
                    .get("category")
                    .and_then(Value::as_str)
                    .and_then(MemoryCategory::from_wire)
                    .unwrap_or(MemoryCategory::Note);
                let basis = obj
                    .get("basis")
                    .and_then(Value::as_str)
                    .and_then(MemoryBasis::from_wire)
                    .unwrap_or(MemoryBasis::Inference);
                let summary = obj
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let body = obj
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let pinned = obj.get("pinned").and_then(Value::as_bool).unwrap_or(false);
                StatePayload::Memory(MemoryEntry {
                    category,
                    basis,
                    lifecycle: MemoryLifecycle::Active,
                    superseded_by: None,
                    pinned,
                    summary,
                    body,
                })
            }
            "handoff" => {
                let summary = obj
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("Handoff")
                    .to_string();
                StatePayload::Handoff(HandoffEntry {
                    summary,
                    shipped: Vec::new(),
                    in_flight: Vec::new(),
                    blockers: Vec::new(),
                    next_steps: Vec::new(),
                    notes: obj
                        .get("notes")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                })
            }
            _ => {
                let value = obj.get("value").cloned().unwrap_or(Value::Null);
                let merge_mode = obj
                    .get("merge_mode")
                    .and_then(Value::as_str)
                    .and_then(MergeMode::from_wire)
                    .unwrap_or(MergeMode::LastWriter);
                StatePayload::Runtime(RuntimeEntry { value, merge_mode })
            }
        }
    };

    // Also block Peer from overwriting an already-sensitive entry.
    if !authority.is_owner()
        && let Ok(Some(existing)) = store.get_state_by_key(&scope, payload.concern(), slug)
        && (existing.sensitive || existing.ever_sensitive)
    {
        return HttpResponse::json(
            403,
            json!({ "error": "owner authority required to modify sensitive state entries" }),
        );
    }

    let sensitivity = match sensitive_flag {
        Some(true) => SensitivityPatch::SetSensitive,
        Some(false) => SensitivityPatch::ClearSensitive,
        None => SensitivityPatch::Inherit,
    };
    let by = obj
        .get("by")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| authority.actor_name())
        .to_string();

    match store.upsert_state(UpsertStateArgs {
        scope,
        slug: slug.to_string(),
        sensitivity,
        payload,
        at: stamp::now(),
        by,
    }) {
        Ok(entry) => HttpResponse::json(200, json!({ "entry": entry })),
        Err(e) => HttpResponse::json(400, json!({ "error": e.to_string() })),
    }
}

fn handle_post_state_checkpoint(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    let raw: Value = if req.body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        match req.json() {
            Ok(v) => v,
            Err(e) => return HttpResponse::json(400, json!({ "error": e })),
        }
    };
    let Some(obj) = raw.as_object() else {
        return HttpResponse::json(400, json!({ "error": "expected JSON object" }));
    };
    let optional_str = |key: &str| -> Result<Option<String>, HttpResponse> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(HttpResponse::json(
                400,
                json!({ "error": format!("`{key}` must be a string") }),
            )),
        }
    };
    let scope_field = match optional_str("scope") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let slug_field = match optional_str("slug") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let summary_field = match optional_str("summary") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let by_field = match optional_str("by") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let by = by_field
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| authority.actor_name().to_string());

    // Reserved internal hub slugs, mirroring `handle_post_state` (checked
    // on the trimmed, lowercased input before slug validation).
    if let Some(raw_slug) = slug_field.as_deref() {
        let normalized = raw_slug.trim().to_ascii_lowercase();
        if is_reserved_hub_slug(&normalized) {
            return HttpResponse::json(
                403,
                json!({ "error": "reserved internal hub authentication slug prefix" }),
            );
        }
    }

    // Validate explicit scope and slug before probing git so invalid
    // input is a 400 even when `repo_root` is not a repository. Only the
    // default scope derivation waits for the probe's top level.
    let explicit_scope = match scope_field.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(raw) => match StateScope::from_wire(raw.trim()) {
            Ok(s) => Some(s),
            Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
        },
        None => None,
    };

    let slug = match resolve_checkpoint_slug(slug_field.as_deref(), &by) {
        Ok(s) => s,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let probe = match probe_git_repo(&state.config.repo_root) {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(500, json!({ "error": e })),
    };

    let scope = explicit_scope.unwrap_or_else(|| {
        probe
            .top_level
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(derive_import_slug)
            .map(StateScope::Project)
            .unwrap_or(StateScope::Workspace)
    });

    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };

    // Non-owners cannot overwrite an already-sensitive checkpoint entry.
    if !authority.is_owner()
        && let Ok(Some(existing)) = store.get_state_by_key(&scope, StateConcern::Handoff, &slug)
        && (existing.sensitive || existing.ever_sensitive)
    {
        return HttpResponse::json(
            403,
            json!({ "error": "owner authority required to modify sensitive state entries" }),
        );
    }

    match create_state_checkpoint_from_probe(
        &*store,
        &probe,
        scope,
        Some(slug.as_str()),
        summary_field.as_deref(),
        &by,
        stamp::now(),
    ) {
        Ok(report) => HttpResponse::json(
            200,
            serde_json::to_value(&report).unwrap_or_else(|_| json!({})),
        ),
        Err(e) => HttpResponse::json(500, json!({ "error": e })),
    }
}

fn handle_post_state_import_workspace(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "owner authority required to import workspace state" }),
        );
    }
    let raw: Value = if req.body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        match req.json() {
            Ok(v) => v,
            Err(e) => return HttpResponse::json(400, json!({ "error": e })),
        }
    };
    let Some(obj) = raw.as_object() else {
        return HttpResponse::json(400, json!({ "error": "expected JSON object" }));
    };
    let optional_str = |key: &str| -> Result<Option<String>, HttpResponse> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(HttpResponse::json(
                400,
                json!({ "error": format!("`{key}` must be a string") }),
            )),
        }
    };
    let workspace_root = match optional_str("workspace_root") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let by_field = match optional_str("by") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let root = workspace_root
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| resolve_workspace_root(&state.config.repo_root));
    let by = by_field
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| authority.actor_name().to_string());
    let scan = scan_workspace_import(&root, &by, stamp::now());
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let report = apply_workspace_import(&*store, scan.upserts, scan.skipped);
    HttpResponse::json(
        200,
        json!({
            "imported_memories": report.imported_memories,
            "imported_handoffs": report.imported_handoffs,
            "scopes": report.scopes,
        }),
    )
}

fn handle_get_stats(store: &RedbStore, _req: &HttpRequest) -> HttpResponse {
    let projects = match store.list_projects() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };

    let mut all_attempts = Vec::new();
    let mut all_transitions = Vec::new();
    let mut all_gate_runs: Vec<GateRun> = Vec::new();

    for proj in &projects {
        if let Ok(att) = store.attempts(&proj.id) {
            all_attempts.extend(att);
        }
        if let Ok(tr) = store.list_transitions(&proj.id) {
            let mut gate_ids: BTreeSet<GateId> = BTreeSet::new();
            for t in &tr {
                for g in &t.gates {
                    gate_ids.insert(g.clone());
                }
            }
            for gid in gate_ids {
                if let Ok(runs) = store.gate_runs(&gid) {
                    all_gate_runs.extend(runs);
                }
            }
            all_transitions.extend(tr);
        }
    }

    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_adapter: BTreeMap<String, Value> = BTreeMap::new();
    let mut wall_clock_ms = 0u64;
    let mut tokens_in = 0u64;
    let mut tokens_out = 0u64;
    let mut cost_usd_micros = 0u64;
    let mut zero_cost_attempts = 0u64;

    #[derive(Default)]
    struct AdapterAcc {
        attempts: u64,
        completed: u64,
        duration_ms: u64,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd_micros: u64,
    }
    let mut adapter_map: BTreeMap<String, AdapterAcc> = BTreeMap::new();

    for a in &all_attempts {
        *by_status.entry(a.status.as_wire().to_string()).or_default() += 1;
        wall_clock_ms = wall_clock_ms.saturating_add(a.duration_ms);
        tokens_in = tokens_in.saturating_add(a.tokens_in);
        tokens_out = tokens_out.saturating_add(a.tokens_out);
        cost_usd_micros = cost_usd_micros.saturating_add(a.cost_usd_micros);
        if a.status != AttemptStatus::Refused && a.cost_usd_micros == 0 {
            zero_cost_attempts += 1;
        }
        let acc = adapter_map.entry(a.adapter.clone()).or_default();
        acc.attempts += 1;
        if a.status == AttemptStatus::Completed {
            acc.completed += 1;
        }
        acc.duration_ms = acc.duration_ms.saturating_add(a.duration_ms);
        acc.tokens_in = acc.tokens_in.saturating_add(a.tokens_in);
        acc.tokens_out = acc.tokens_out.saturating_add(a.tokens_out);
        acc.cost_usd_micros = acc.cost_usd_micros.saturating_add(a.cost_usd_micros);
    }

    for (k, v) in adapter_map {
        by_adapter.insert(
            k,
            json!({
                "attempts": v.attempts,
                "completed": v.completed,
                "duration_ms": v.duration_ms,
                "tokens_in": v.tokens_in,
                "tokens_out": v.tokens_out,
                "cost_usd_micros": v.cost_usd_micros,
            }),
        );
    }

    let regret_audit = audit_transition_regret(&all_transitions, &all_gate_runs);

    HttpResponse::json(
        200,
        json!({
            "attempts": all_attempts.len(),
            "by_status": by_status,
            "wall_clock_ms": wall_clock_ms,
            "tokens_in": tokens_in,
            "tokens_out": tokens_out,
            "cost_usd_micros": cost_usd_micros,
            "zero_cost_attempts": zero_cost_attempts,
            "by_adapter": by_adapter,
            "regret_audit": regret_audit,
            "token_gain": load_token_gain(store),
        }),
    )
}

fn handle_get_architecture(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let rel_path = req
        .query
        .get("path")
        .map(String::as_str)
        .unwrap_or(DEFAULT_ARCH_MANIFEST_PATH);
    if Path::new(rel_path).is_absolute() || has_path_traversal(rel_path, rel_path) {
        return HttpResponse::json(
            400,
            json!({ "error": "invalid architecture manifest path" }),
        );
    }
    let manifest_path = state.config.repo_root.join(rel_path);
    if !manifest_path.is_file() {
        return HttpResponse::json(
            200,
            json!({
                "model": null,
                "likec4_dsl": null,
                "conformance": null,
            }),
        );
    }

    let raw = match fs::read_to_string(&manifest_path) {
        Ok(r) => r,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };

    let model: ArchitectureModel = if raw.trim_start().starts_with('{') {
        match serde_json::from_str(&raw) {
            Ok(m) => m,
            Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
        }
    } else {
        match toml::from_str(&raw) {
            Ok(m) => m,
            Err(e) => return HttpResponse::json(400, json!({ "error": e.to_string() })),
        }
    };

    if let Err(e) = validate_architecture_model(&model) {
        return HttpResponse::json(400, json!({ "error": e.to_string() }));
    }

    let likec4_dsl = export_likec4_dsl(&model).ok();
    let conformance = scan_cargo_architecture_conformance(&state.config.repo_root, &model).ok();

    HttpResponse::json(
        200,
        json!({
            "model": model,
            "likec4_dsl": likec4_dsl,
            "conformance": conformance,
        }),
    )
}

fn attachments_dir(state: &HubState) -> PathBuf {
    state
        .config
        .store_path
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("attachments")
}

const MAX_ARTIFACT_PREVIEW_BYTES: u64 = 16 * 1024 * 1024;

fn percent_encode_query_value(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn classify_artifact_kind(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("md" | "markdown" | "mdx") => "markdown",
        Some("mmd" | "mermaid") => "mermaid",
        Some("svg") => "svg",
        Some("png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "ico") => "image",
        Some("pdf") => "pdf",
        Some(
            "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" | "toml" | "json" | "jsonl"
            | "yaml" | "yml" | "css" | "html" | "htm" | "sh" | "bash" | "zsh" | "sql" | "c" | "cc"
            | "cpp" | "h" | "hpp" | "go" | "java" | "kt" | "swift" | "txt" | "log" | "c4" | "xml"
            | "csv" | "lock",
        ) => "code",
        _ => "binary",
    }
}

fn is_forbidden_artifact_path(path: &Path) -> bool {
    let comps: Vec<std::path::Component<'_>> = path.components().collect();
    if comps
        .iter()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return true;
    }
    let mut in_fl_attachments = false;
    for (idx, comp) in comps.iter().enumerate() {
        if let std::path::Component::Normal(os) = comp {
            let seg = os.to_string_lossy().to_ascii_lowercase();
            let next_is_attachments = comps
                .get(idx + 1)
                .and_then(|next| match next {
                    std::path::Component::Normal(n) => {
                        Some(n.to_string_lossy().to_ascii_lowercase())
                    }
                    _ => None,
                })
                .is_some_and(|n| n == "attachments");
            if seg == ".fl" && next_is_attachments {
                in_fl_attachments = true;
                continue;
            }
            if seg == ".git"
                || seg == ".fl"
                || seg == "secrets"
                || seg == ".env"
                || seg.starts_with(".env.")
                || seg == "vault.age"
                || seg == "vault.enc"
                || seg == "vault.json"
                || seg == "vault.key"
                || seg == "gate-audit.jsonl"
                || seg.ends_with(".redb")
            {
                return true;
            }
        }
    }
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    normalized.contains("/.fl/gate/vault")
        || normalized.ends_with("/.fl/gate")
        || (normalized.contains("/.fl/") && !in_fl_attachments)
        || normalized.contains("/.git/")
        || normalized.contains("/secrets/")
}

fn allowed_artifact_roots(state: &HubState) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(state.config.repo_root.clone());
    candidates.push(resolve_workspace_root(&state.config.repo_root));
    candidates.push(attachments_dir(state));

    if let Ok(worktrees) = list_git_worktrees(&state.config.repo_root) {
        for wt in worktrees {
            candidates.push(PathBuf::from(wt.path));
        }
    }

    if let Some(home) = hub_home_dir(state) {
        candidates.push(home.join(".gemini/antigravity-cli/brain"));
        candidates.push(home.join(".local/share/fl/attachments"));
        // Archived artifact copies, so previews of files whose worktree is gone still work.
        candidates.push(artifacts::managed_dir(&artifacts::data_root(&home)));
        if state.config.home_override.is_none() {
            candidates.push(home.join("Projects/Artificial-Humanity"));
        }
    }

    let mut canonical_roots = Vec::new();
    for root in candidates {
        if let Ok(canon) = fs::canonicalize(&root)
            && !canonical_roots.contains(&canon)
        {
            canonical_roots.push(canon);
        }
    }
    canonical_roots
}

fn resolve_artifact_candidate_path(
    state: &HubState,
    raw_query_path: &str,
) -> Result<PathBuf, (u16, &'static str)> {
    let trimmed = raw_query_path.trim();
    if trimmed.is_empty() {
        return Err((400, "missing required query parameter `path`"));
    }
    if trimmed.contains('\0') {
        return Err((400, "invalid artifact path"));
    }
    let without_scheme = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    let without_fragment = without_scheme
        .split('#')
        .next()
        .unwrap_or(without_scheme)
        .trim();
    if without_fragment.is_empty() {
        return Err((400, "empty artifact path"));
    }

    let candidate = if let Some(att_rel) = without_fragment.strip_prefix("/api/v1/attachments/") {
        let filename = percent_decode(att_rel.split('?').next().unwrap_or(att_rel));
        if filename.is_empty()
            || has_path_traversal(att_rel, &filename)
            || filename.contains('/')
            || filename.contains('\\')
        {
            return Err((400, "invalid attachment filename"));
        }
        attachments_dir(state).join(filename)
    } else {
        let p = Path::new(without_fragment);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            state.config.repo_root.join(p)
        }
    };

    if is_forbidden_artifact_path(&candidate) {
        return Err((403, "access to sensitive or internal path is forbidden"));
    }

    let allowed_roots = allowed_artifact_roots(state);
    let canonical = match fs::canonicalize(&candidate) {
        Ok(c) => c,
        Err(_) => {
            // Check whether the non-existent path was attempting to escape allowed roots.
            let within_raw_root = allowed_roots.iter().any(|r| candidate.starts_with(r))
                || candidate.starts_with(&state.config.repo_root)
                || candidate.starts_with(attachments_dir(state));
            if !within_raw_root || has_path_traversal(without_fragment, without_fragment) {
                return Err((403, "artifact path outside allowed workspace roots"));
            }
            return Err((404, "artifact file not found"));
        }
    };

    if is_forbidden_artifact_path(&canonical) {
        return Err((403, "access to sensitive or internal path is forbidden"));
    }

    if !allowed_roots.iter().any(|root| canonical.starts_with(root)) {
        return Err((403, "artifact path outside allowed workspace roots"));
    }

    Ok(canonical)
}

fn handle_get_artifact_preview(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let raw_path = req.query.get("path").map(String::as_str).unwrap_or("");
    let canonical = match resolve_artifact_candidate_path(state, raw_path) {
        Ok(p) => p,
        Err((status, msg)) => return HttpResponse::json(status, json!({ "error": msg })),
    };

    let meta = match fs::metadata(&canonical) {
        Ok(m) => m,
        Err(_) => return HttpResponse::json(404, json!({ "error": "artifact file not found" })),
    };
    if !meta.is_file() {
        return HttpResponse::json(404, json!({ "error": "artifact is not a regular file" }));
    }
    let size_bytes = meta.len();
    if size_bytes > MAX_ARTIFACT_PREVIEW_BYTES {
        return HttpResponse::json(
            413,
            json!({
                "error": "artifact exceeds 16 MiB preview limit",
                "size_bytes": size_bytes,
                "max_bytes": MAX_ARTIFACT_PREVIEW_BYTES,
            }),
        );
    }

    let download = req
        .query
        .get("download")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let mode = req
        .query
        .get("mode")
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "json".to_string());

    let filename = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact")
        .to_string();
    let safe_header_name: String = filename
        .chars()
        .filter(|c| *c != '"' && *c != '\r' && *c != '\n' && *c != '\\')
        .collect();
    let kind = classify_artifact_kind(&canonical);
    let mime_type = if matches!(kind, "markdown" | "mermaid" | "code") {
        "text/plain; charset=utf-8"
    } else {
        mime_type_for_path(&canonical)
    };

    if download || mode == "raw" {
        let bytes = match fs::read(&canonical) {
            Ok(b) => b,
            Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
        };
        let disposition = if download {
            format!("attachment; filename=\"{safe_header_name}\"")
        } else {
            format!("inline; filename=\"{safe_header_name}\"")
        };
        return HttpResponse::bytes(200, mime_type, bytes)
            .with_header("Content-Disposition", disposition)
            .with_header("X-Content-Type-Options", "nosniff")
            .with_header(
                "Content-Security-Policy",
                "script-src 'none'; object-src 'none'",
            );
    }

    let ext = canonical
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let content = if matches!(kind, "markdown" | "mermaid" | "svg" | "code") {
        fs::read(&canonical)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).to_string())
    } else {
        None
    };

    let canonical_str = canonical.display().to_string();
    let encoded_path = percent_encode_query_value(&canonical_str);
    let raw_url = format!("/api/v1/artifacts/preview?path={encoded_path}&mode=raw");
    let download_url = format!("/api/v1/artifacts/preview?path={encoded_path}&download=1");

    HttpResponse::json(
        200,
        json!({
            "path": canonical_str,
            "name": filename,
            "ext": ext,
            "mime_type": mime_type,
            "size_bytes": size_bytes,
            "kind": kind,
            "content": content,
            "raw_url": raw_url,
            "download_url": download_url,
        }),
    )
}

/// Most artifacts one list call returns.
const MAX_ARTIFACT_LIST_ITEMS: usize = 50;

/// The hub's home directory: the configured override, else `$HOME`.
fn hub_home_dir(state: &HubState) -> Option<PathBuf> {
    state
        .config
        .home_override
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

/// Data root of the artifact registry and its managed copies.
fn artifacts_root(state: &HubState) -> Option<PathBuf> {
    hub_home_dir(state).map(|home| artifacts::data_root(&home))
}

/// Files a session's brain dir and transcript point at. Not yet vetted against the allowed roots.
fn discover_session_artifact_candidates(
    state: &HubState,
    session_id: &str,
    canonical_session_id: &str,
) -> Vec<PathBuf> {
    let mut candidate_paths: Vec<PathBuf> = Vec::new();
    if let Some(home) = hub_home_dir(state) {
        for candidate_id in [session_id, canonical_session_id] {
            if candidate_id.is_empty()
                || candidate_id.contains('/')
                || candidate_id.contains('\\')
                || candidate_id.contains("..")
                || candidate_id.contains('\0')
            {
                continue;
            }
            let brain_dir = home
                .join(".gemini/antigravity-cli/brain")
                .join(candidate_id);
            if brain_dir.is_dir()
                && let Ok(entries) = fs::read_dir(&brain_dir)
            {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() {
                        candidate_paths.push(p);
                    }
                }
            }
        }
    }

    let snap = load_discovery_snapshot(state);
    let turns = read_session_transcript_from_snapshot(
        session_id,
        &snap,
        state.config.home_override.as_deref(),
    );
    for turn in turns.iter().rev().take(60) {
        let mut remainder = turn.content.as_str();
        while let Some(pos) = remainder.find("file:///") {
            let valid_prefix = pos == 0
                || remainder[..pos].chars().next_back().is_some_and(|c| {
                    c.is_whitespace() || matches!(c, '(' | '<' | '"' | '\'' | '`')
                });
            let after = &remainder[pos + "file:///".len()..];
            let end = after
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\'' | '`')
                })
                .unwrap_or(after.len());
            if valid_prefix {
                let raw_target = &after[..end];
                let clean = raw_target
                    .trim_end_matches(['.', ',', ';'])
                    .split('#')
                    .next()
                    .unwrap_or(raw_target);
                if !clean.is_empty() {
                    candidate_paths.push(PathBuf::from(format!("/{clean}")));
                }
            }
            remainder = &after[end..];
        }
    }
    candidate_paths
}

fn discover_attachment_candidates(state: &HubState) -> Vec<PathBuf> {
    let mut candidate_paths = Vec::new();
    let att_dir = attachments_dir(state);
    if att_dir.is_dir()
        && let Ok(entries) = fs::read_dir(&att_dir)
    {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                candidate_paths.push(p);
            }
        }
    }
    candidate_paths
}

/// The canonical path and metadata of `cand`, when it is a regular file the hub may serve as an
/// artifact: not forbidden, inside an allowed root, within the preview size cap.
fn vet_artifact_file(allowed_roots: &[PathBuf], cand: &Path) -> Option<(PathBuf, fs::Metadata)> {
    if is_forbidden_artifact_path(cand) {
        return None;
    }
    let canon = fs::canonicalize(cand).ok()?;
    if is_forbidden_artifact_path(&canon) || !allowed_roots.iter().any(|r| canon.starts_with(r)) {
        return None;
    }
    let meta = fs::metadata(&canon).ok()?;
    (meta.is_file() && meta.len() <= MAX_ARTIFACT_PREVIEW_BYTES).then_some((canon, meta))
}

fn artifact_name_and_ext(canon: &Path) -> (String, String) {
    let name = canon
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact")
        .to_string();
    let ext = canon
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    (name, ext)
}

fn artifact_entry(canon: &Path, size_bytes: u64, session_id: Option<&str>) -> ArtifactEntry {
    let (name, ext) = artifact_name_and_ext(canon);
    let path = canon.display().to_string();
    ArtifactEntry {
        path: path.clone(),
        canonical_path: path,
        managed_path: None,
        name,
        ext,
        kind: classify_artifact_kind(canon).to_string(),
        size_bytes,
        session_id: session_id.map(str::to_string),
        status: artifacts::STATUS_LIVE.to_string(),
        updated_at_ms: artifacts::now_ms(),
    }
}

fn live_artifact_item(canon: &Path, size_bytes: u64) -> Value {
    let (name, ext) = artifact_name_and_ext(canon);
    json!({
        "path": canon.display().to_string(),
        "name": name,
        "ext": ext,
        "kind": classify_artifact_kind(canon),
        "size_bytes": size_bytes,
        "status": artifacts::STATUS_LIVE,
    })
}

/// An entry whose original is gone, served from its managed copy.
fn archived_artifact_item(entry: &ArtifactEntry) -> Option<Value> {
    let managed = entry.managed_path.as_deref()?;
    let size_bytes = fs::metadata(managed)
        .map(|m| m.len())
        .unwrap_or(entry.size_bytes);
    Some(json!({
        "path": managed,
        "original_path": entry.canonical_path,
        "name": entry.name,
        "ext": entry.ext,
        "kind": entry.kind,
        "size_bytes": size_bytes,
        "status": artifacts::STATUS_ARCHIVED,
    }))
}

fn handle_get_artifacts_list(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let session_id = req
        .query
        .get("session_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let canonical_session_id = session_id.map(|raw| resolve_session_target(state, raw).0);

    let mut candidate_paths: Vec<PathBuf> = Vec::new();
    if let (Some(raw), Some(canonical)) = (session_id, canonical_session_id.as_deref()) {
        candidate_paths.extend(discover_session_artifact_candidates(state, raw, canonical));
    }
    candidate_paths.extend(discover_attachment_candidates(state));

    let allowed_roots = allowed_artifact_roots(state);
    let mut live: Vec<(PathBuf, u64)> = Vec::new();
    let mut seen = BTreeSet::new();
    for cand in candidate_paths {
        let Some((canon, meta)) = vet_artifact_file(&allowed_roots, &cand) else {
            continue;
        };
        if !seen.insert(canon.clone()) {
            continue;
        }
        live.push((canon, meta.len()));
        if live.len() >= MAX_ARTIFACT_LIST_ITEMS {
            break;
        }
    }
    let mut items: Vec<Value> = live
        .iter()
        .map(|(canon, size)| live_artifact_item(canon, *size))
        .collect();

    let mut pruned_count = 0;
    if let Some(root) = artifacts_root(state) {
        let session_ids: Option<Vec<&str>> = session_id.map(|raw| {
            let mut ids = vec![raw];
            if let Some(canonical) = canonical_session_id.as_deref()
                && canonical != raw
            {
                ids.push(canonical);
            }
            ids
        });
        // Files in the shared attachments dir belong to no session.
        let attachments = fs::canonicalize(attachments_dir(state)).ok();
        let outcome = artifacts::with_registry(&root, |registry| {
            for (canon, size) in &live {
                let shared = attachments.as_ref().is_some_and(|a| canon.starts_with(a));
                let tag = if shared {
                    None
                } else {
                    canonical_session_id.as_deref()
                };
                registry.upsert(artifact_entry(canon, *size, tag));
            }
            registry.reconcile(&artifacts::managed_dir(&root), session_ids.as_deref())
        });
        pruned_count = outcome.pruned;
        for entry in &outcome.archived {
            if items.len() >= MAX_ARTIFACT_LIST_ITEMS {
                break;
            }
            items.extend(archived_artifact_item(entry));
        }
    }

    HttpResponse::json(
        200,
        json!({ "artifacts": items, "pruned_count": pruned_count }),
    )
}

/// What `preserve_artifacts` archives.
struct ArtifactPreservation {
    /// Files to register and archive (each still vetted).
    candidates: Vec<PathBuf>,
    /// Canonical dirs the files must sit in. Empty means no location limit.
    scope: Vec<PathBuf>,
    /// Session recorded on newly registered files.
    session_tag: Option<String>,
    /// Session ids whose tracked registry entries are archived too (all when empty). Tracked
    /// entries are archived only when `scope` is set.
    session_ids: Vec<String>,
}

/// Most files one `preserve_artifacts` call copies.
const MAX_PRESERVE_TARGETS: usize = 200;

/// Copy the plan's user-facing files into the managed dir and record the copies in the registry.
/// Every file, tracked or named, is vetted again here (the registry file is only a hint), so
/// nothing forbidden, symlinked out of its scope, or outside the allowed roots is copied; the
/// copy itself refuses a symlink and vets the path it opened (see `archive_file`). The copies are
/// made outside the registry lock, which is taken once at the end. A copy that is already
/// current is not made again, and one call handles at most `MAX_PRESERVE_TARGETS` files.
/// Returns how many files now have a managed copy; failures are logged and skipped.
fn preserve_artifacts(state: &HubState, root: &Path, plan: &ArtifactPreservation) -> usize {
    let allowed_roots = allowed_artifact_roots(state);
    let wanted = |canon: &Path, size: u64| {
        (plan.scope.is_empty() || plan.scope.iter().any(|r| canon.starts_with(r)))
            && artifacts::is_archivable(canon, size)
    };

    let mut fresh: Vec<(PathBuf, u64)> = Vec::new();
    for cand in &plan.candidates {
        if fresh.len() >= MAX_PRESERVE_TARGETS {
            break;
        }
        if let Some((canon, meta)) = vet_artifact_file(&allowed_roots, cand)
            && wanted(&canon, meta.len())
            && !fresh.iter().any(|(p, _)| *p == canon)
        {
            fresh.push((canon, meta.len()));
        }
    }

    let mut targets: Vec<PathBuf> = fresh.iter().map(|(p, _)| p.clone()).collect();
    if !plan.scope.is_empty() {
        let tracked: Vec<String> = artifacts::with_registry(root, |registry| {
            registry
                .entries
                .iter()
                .filter(|e| {
                    plan.session_ids.is_empty()
                        || e.session_id
                            .as_deref()
                            .is_some_and(|sid| plan.session_ids.iter().any(|id| id == sid))
                })
                .map(|e| e.canonical_path.clone())
                .collect()
        });
        for path in tracked {
            if targets.len() >= MAX_PRESERVE_TARGETS {
                break;
            }
            if let Some((canon, meta)) = vet_artifact_file(&allowed_roots, Path::new(&path))
                && canon == Path::new(&path)
                && wanted(&canon, meta.len())
                && !targets.contains(&canon)
            {
                targets.push(canon);
            }
        }
    }

    // The vetting again, on the path the opened file resolves to.
    let still_vetted = |resolved: &Path| {
        vet_artifact_file(&allowed_roots, resolved)
            .is_some_and(|(canon, meta)| canon == resolved && wanted(&canon, meta.len()))
    };
    let mut copies: Vec<(PathBuf, PathBuf)> = Vec::new();
    for canon in targets {
        if let Some(dest) = artifacts::current_managed_copy(root, &canon) {
            copies.push((canon, dest));
            continue;
        }
        match artifacts::archive_file(root, &canon, &still_vetted) {
            Ok(dest) => copies.push((canon, dest)),
            Err(err) => eprintln!(
                "fl-hub: could not archive artifact {}: {err}",
                canon.display()
            ),
        }
    }

    artifacts::with_registry(root, |registry| {
        for (canon, size) in &fresh {
            registry.upsert(artifact_entry(canon, *size, plan.session_tag.as_deref()));
        }
        for (canon, dest) in &copies {
            let canon_str = canon.display().to_string();
            if let Some(entry) = registry
                .entries
                .iter_mut()
                .find(|e| e.canonical_path == canon_str)
            {
                let dest_str = dest.display().to_string();
                if entry.managed_path.as_deref() != Some(dest_str.as_str()) {
                    entry.managed_path = Some(dest_str);
                    entry.updated_at_ms = artifacts::now_ms();
                }
            }
        }
    });
    copies.len()
}

/// Canonical dirs a session's artifacts live in: its recorded cwd (none when that is gone), else
/// every git worktree other than the primary checkout.
fn session_worktree_scope(state: &HubState, cwd: Option<&str>) -> Vec<PathBuf> {
    if let Some(cwd) = cwd.map(str::trim).filter(|c| !c.is_empty()) {
        return fs::canonicalize(cwd)
            .ok()
            .filter(|c| c.is_dir())
            .into_iter()
            .collect();
    }
    let primary = fs::canonicalize(&state.config.repo_root).ok();
    list_git_worktrees(&state.config.repo_root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|wt| fs::canonicalize(wt.path).ok())
        .filter(|p| p.is_dir() && Some(p) != primary.as_ref())
        .collect()
}

/// Best-effort archive of a session's artifacts before its teardown removes the worktree. Never
/// fails the teardown: problems are logged and skipped.
fn archive_session_artifacts(
    state: &HubState,
    target: &SessionTarget,
    discovered_id: &str,
    candidates: Vec<PathBuf>,
) {
    let Some(root) = artifacts_root(state) else {
        return;
    };
    let scope =
        session_worktree_scope(state, target.hosted.as_ref().and_then(|h| h.cwd.as_deref()));
    if scope.is_empty() {
        return;
    }
    let mut session_ids: Vec<String> = Vec::new();
    for id in [
        target.raw_id.as_str(),
        discovered_id,
        target.session_id.as_str(),
    ] {
        if !session_ids.iter().any(|s| s == id) {
            session_ids.push(id.to_string());
        }
    }
    preserve_artifacts(
        state,
        &root,
        &ArtifactPreservation {
            candidates,
            scope,
            session_tag: Some(discovered_id.to_string()),
            session_ids,
        },
    );
}

/// The worktree a prune request names, canonical. `Ok(None)` when it does not exist (nothing to
/// copy); an error response when it is forbidden, outside the allowed roots, or not a directory.
fn resolve_prune_worktree(
    state: &HubState,
    allowed_roots: &[PathBuf],
    raw: &str,
) -> Result<Option<PathBuf>, HttpResponse> {
    let deny = |status, msg: &str| Err(HttpResponse::json(status, json!({ "error": msg })));
    if raw.contains('\0') {
        return deny(400, "invalid worktree_path");
    }
    let given = Path::new(raw);
    let candidate = if given.is_absolute() {
        given.to_path_buf()
    } else {
        state.config.repo_root.join(given)
    };
    if is_forbidden_artifact_path(&candidate) {
        return deny(403, "access to sensitive or internal path is forbidden");
    }
    let Ok(canonical) = fs::canonicalize(&candidate) else {
        // Same containment check as an existing path, so a missing path leaks nothing.
        if allowed_roots.iter().any(|r| candidate.starts_with(r))
            || candidate.starts_with(&state.config.repo_root)
        {
            return Ok(None);
        }
        return deny(403, "worktree_path outside allowed workspace roots");
    };
    if is_forbidden_artifact_path(&canonical) {
        return deny(403, "access to sensitive or internal path is forbidden");
    }
    if !allowed_roots.iter().any(|r| canonical.starts_with(r)) {
        return deny(403, "worktree_path outside allowed workspace roots");
    }
    if !canonical.is_dir() {
        return deny(400, "worktree_path is not a directory");
    }
    Ok(Some(canonical))
}

/// `POST /api/v1/artifacts/prune` (Record 29): archive the user-facing artifacts of a worktree
/// before it goes, then drop registry entries that have neither an original nor a copy. Owner
/// only. Body (all optional): `paths`, `worktree_path`, `session_id`.
fn handle_post_artifacts_prune(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may prune artifacts" }),
        );
    }
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let paths: Vec<String> = match body.get("paths") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => match json_string_items("paths", items) {
            Ok(paths) => paths,
            Err(e) => return HttpResponse::json(400, json!({ "error": e })),
        },
        Some(_) => {
            return HttpResponse::json(
                400,
                json!({ "error": "`paths` must be an array of strings" }),
            );
        }
    };
    let worktree_raw = match json_string(&body, "worktree_path") {
        Ok(v) => v.trim().to_string(),
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let session_raw = match json_string(&body, "session_id") {
        Ok(v) => v.trim().to_string(),
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let Some(root) = artifacts_root(state) else {
        return HttpResponse::json(500, json!({ "error": "home directory unavailable" }));
    };

    let allowed_roots = allowed_artifact_roots(state);
    let mut scope: Vec<PathBuf> = Vec::new();
    // A worktree that was named but is gone gives no scope, and an empty scope would let `paths`
    // be archived from anywhere, so nothing is archived then (the prune below still runs).
    let mut worktree_missing = false;
    if !worktree_raw.is_empty() {
        match resolve_prune_worktree(state, &allowed_roots, &worktree_raw) {
            Ok(Some(found)) => scope.push(found),
            Ok(None) => worktree_missing = true,
            Err(resp) => return resp,
        }
    }

    // Named paths take the same checks as a preview. A missing file is skipped: it is what the
    // prune below is for.
    let mut candidates: Vec<PathBuf> = Vec::new();
    for raw in &paths {
        match resolve_artifact_candidate_path(state, raw) {
            Ok(canon) => candidates.push(canon),
            Err((404, _)) => {}
            Err((status, msg)) => return HttpResponse::json(status, json!({ "error": msg })),
        }
    }

    // Resolve before taking the store guard: discovery opens the store itself.
    let mut session_ids: Vec<String> = Vec::new();
    let mut session_tag = None;
    if !session_raw.is_empty() {
        let (canonical, _) = resolve_session_target(state, &session_raw);
        let mut cwd: Option<String> = None;
        if scope.is_empty()
            && let Ok(store) = open_store_response(state)
        {
            cwd = lifecycle::find_hosted_session(&*store, &[&session_raw, &canonical])
                .ok()
                .flatten()
                .and_then(|rec| rec.cwd);
        }
        // A cwd the Owner did not name is used only when it passes the same checks.
        if let Some(cwd) = cwd
            && let Ok(Some(found)) = resolve_prune_worktree(state, &allowed_roots, cwd.trim())
        {
            scope.push(found);
        }
        if !scope.is_empty() {
            candidates.extend(discover_session_artifact_candidates(
                state,
                &session_raw,
                &canonical,
            ));
        }
        session_ids.push(session_raw.clone());
        if canonical != session_raw {
            session_ids.push(canonical.clone());
        }
        session_tag = Some(canonical);
    }

    if !worktree_missing {
        preserve_artifacts(
            state,
            &root,
            &ArtifactPreservation {
                candidates,
                scope,
                session_tag,
                session_ids,
            },
        );
    }
    let outcome = artifacts::with_registry(&root, |registry| {
        registry.reconcile(&artifacts::managed_dir(&root), None)
    });
    HttpResponse::json(
        200,
        json!({
            "ok": true,
            "pruned_count": outcome.pruned,
            "archived_count": outcome.archived.len(),
            "active_count": outcome.live,
        }),
    )
}

/// `GET /api/v1/settings` (Record 4): the saved settings, or the defaults.
fn handle_get_settings(state: &HubState) -> HttpResponse {
    let Some(home) = hub_home_dir(state) else {
        return HttpResponse::json(200, json!(settings::Settings::default()));
    };
    HttpResponse::json(200, json!(settings::load(&home)))
}

/// `PUT /api/v1/settings` (Record 4): merge a full or partial body into the saved settings.
/// Owner only. Broadcasts `settings.updated` on the `settings` channel.
fn handle_put_settings(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may change settings" }),
        );
    }
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let patch: settings::SettingsPatch = match serde_json::from_value(body) {
        Ok(patch) => patch,
        Err(e) => {
            return HttpResponse::json(400, json!({ "error": format!("invalid settings: {e}") }));
        }
    };
    let Some(home) = hub_home_dir(state) else {
        return HttpResponse::json(500, json!({ "error": "home directory unavailable" }));
    };
    match settings::update(&home, patch) {
        Ok(updated) => {
            let data = json!(updated);
            state.broadcast("settings", "settings.updated", data.clone());
            HttpResponse::json(200, data)
        }
        Err(settings::UpdateError::Invalid(msg)) => {
            HttpResponse::json(400, json!({ "error": msg }))
        }
        Err(settings::UpdateError::Io(e)) => HttpResponse::json(
            500,
            json!({ "error": format!("could not save settings: {e}") }),
        ),
    }
}

fn handle_dynamic_api_routes(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    let method = req.method.as_str();
    let segments: Vec<&str> = req
        .path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    // /api/v1/attachments/:filename
    if segments.len() == 4
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "attachments"
        && (method == "GET" || method == "HEAD")
    {
        let filename = percent_decode(segments[3]);
        if filename.is_empty()
            || has_path_traversal(segments[3], &filename)
            || filename.contains('/')
            || filename.contains('\\')
        {
            return HttpResponse::json(400, json!({ "error": "invalid attachment filename" }));
        }
        let path = attachments_dir(state).join(&filename);
        if path.is_file()
            && let Ok(bytes) = fs::read(&path)
        {
            return HttpResponse::bytes(200, mime_type_for_path(&path), bytes);
        }
        return HttpResponse::json(404, json!({ "error": "attachment not found" }));
    }

    // /api/v1/sessions/:id/transcript, /prompt, /clear, /decommission, or /queue
    if segments.len() == 5
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "sessions"
    {
        let session_id = percent_decode(segments[3]);
        match (method, segments[4]) {
            ("GET", "transcript") => {
                drain_ready_session_queues(state);
                let snap = load_discovery_snapshot(state);
                let home = state.config.home_override.as_deref();
                let (canonical_id, _persona) =
                    resolve_session_target_from_snapshot(state, &snap, &session_id);
                let all_turns = read_session_transcript_from_snapshot(&session_id, &snap, home);
                let total_turns = all_turns.len();
                let limit_opt = match req.query.get("limit").map(|s| s.trim()) {
                    Some("all" | "0") => None,
                    Some(raw) => raw.parse::<usize>().ok().or(Some(250)),
                    None => Some(250),
                };
                let turns = match limit_opt {
                    Some(limit) if total_turns > limit => all_turns[total_turns - limit..].to_vec(),
                    _ => all_turns,
                };
                let busy_turns = read_session_transcript_from_snapshot(&canonical_id, &snap, home);
                let is_busy = state.is_session_busy_from_turns(&canonical_id, &busy_turns);
                let queue = state.session_queue(&canonical_id);
                let context_usage =
                    read_session_context_usage_from_snapshot(&session_id, &snap, home);
                let quota_usage =
                    read_session_quota_usage_with_home(&canonical_id, home).or_else(|| {
                        if canonical_id == session_id {
                            None
                        } else {
                            read_session_quota_usage_with_home(&session_id, home)
                        }
                    });
                return HttpResponse::json(
                    200,
                    json!({
                        "session_id": session_id,
                        "turns": turns,
                        "total_turns": total_turns,
                        "is_busy": is_busy,
                        "queue": queue,
                        "context_usage": context_usage,
                        "quota_usage": quota_usage,
                    }),
                );
            }
            ("GET", "queue") => {
                drain_ready_session_queues(state);
                let (canonical_id, _persona) = resolve_session_target(state, &session_id);
                let is_busy = state.is_session_busy_released(&canonical_id);
                let queue = state.session_queue(&canonical_id);
                return HttpResponse::json(
                    200,
                    json!({
                        "session_id": canonical_id,
                        "is_busy": is_busy,
                        "queue": queue,
                    }),
                );
            }
            ("POST", "prompt") => {
                return handle_post_session_prompt(state, authority, &session_id, req);
            }
            ("POST", "clear") => {
                return handle_post_session_clear(state, authority, &session_id, req);
            }
            ("POST", "interrupt") => {
                return handle_post_session_interrupt(state, authority, &session_id, req);
            }
            ("POST", "model") => {
                return handle_post_session_model(state, authority, &session_id, req);
            }
            ("POST", "decommission") => {
                return handle_post_session_decommission(state, authority, &session_id, req);
            }
            _ => {}
        }
    }

    // /api/v1/hosted-sessions/:id/restart
    if segments.len() == 5
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "hosted-sessions"
        && segments[4] == "restart"
        && method == "POST"
    {
        return handle_post_hosted_session_restart(state, authority, &percent_decode(segments[3]));
    }

    // /api/v1/sessions/:id/queue/:prompt_id or /api/v1/sessions/:id/queue/:prompt_id/send-now
    if (segments.len() == 6 || segments.len() == 7)
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "sessions"
        && segments[4] == "queue"
    {
        let session_id = percent_decode(segments[3]);
        let prompt_id = percent_decode(segments[5]);
        if segments.len() == 7 && segments[6] == "send-now" && method == "POST" {
            return handle_post_queued_prompt_send_now(state, &session_id, &prompt_id);
        }
        if segments.len() == 6 && (method == "PATCH" || method == "PUT") {
            return handle_patch_queued_prompt(state, &session_id, &prompt_id, req);
        }
        if segments.len() == 6 && method == "DELETE" {
            return handle_delete_queued_prompt(state, &session_id, &prompt_id);
        }
    }

    // /api/v1/worktrees/:id/diff
    if segments.len() == 5
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "worktrees"
        && segments[4] == "diff"
        && method == "GET"
    {
        let wt_id = percent_decode(segments[3]);
        return match read_worktree_diff(&state.config.repo_root, &wt_id) {
            Ok(Some(diff)) => {
                HttpResponse::json(200, serde_json::to_value(diff).unwrap_or_default())
            }
            Ok(None) => HttpResponse::json(404, json!({ "error": "worktree not found" })),
            Err(e) => HttpResponse::json(500, json!({ "error": e })),
        };
    }

    // /api/v1/prs/:number/review or /api/v1/prs/:number/merge
    if segments.len() == 5 && segments[0] == "api" && segments[1] == "v1" && segments[2] == "prs" {
        let Ok(number) = segments[3].parse::<u64>() else {
            return HttpResponse::json(400, json!({ "error": "invalid PR number" }));
        };
        let Ok(store) = open_store_response(state) else {
            return store_open_failure();
        };
        match (method, segments[4]) {
            ("POST", "review") => {
                return handle_post_pr_review(&store, authority, number, req);
            }
            ("POST", "merge") => {
                return handle_post_pr_merge(&store, authority, number, req);
            }
            _ => {}
        }
    }

    // /api/v1/workflow-runs/:id and /api/v1/workflow-runs/:id/<action>
    if segments.len() >= 4
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "workflow-runs"
    {
        let Ok(store) = open_store_response(state) else {
            return store_open_failure();
        };
        let raw_run_id = percent_decode(segments[3]);
        let run_id = match resolve_workflow_run_id(&store, &raw_run_id) {
            Ok(id) => id,
            Err(e) => return HttpResponse::json(404, json!({ "error": e })),
        };

        if segments.len() == 4 && method == "GET" {
            return match store.get_workflow_run(&run_id) {
                Ok(Some(run)) => {
                    let handoffs = list_workflow_handoffs(&*store, &run_id).unwrap_or_default();
                    let facts = list_workflow_facts(&*store, &run_id).unwrap_or_default();
                    HttpResponse::json(
                        200,
                        json!({
                            "run": run,
                            "handoffs": handoffs,
                            "facts": facts,
                        }),
                    )
                }
                Ok(None) => HttpResponse::json(404, json!({ "error": "workflow run not found" })),
                Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
            };
        }

        if segments.len() == 5 && method == "POST" {
            match segments[4] {
                "step" => {
                    return handle_post_workflow_step(state, &store, authority, &run_id, req);
                }
                "handoff" => {
                    return handle_post_workflow_handoff(state, &store, authority, &run_id, req);
                }
                "approve" => {
                    return handle_post_workflow_sign_off(
                        state, &store, authority, &run_id, true, req,
                    );
                }
                "reject" => {
                    return handle_post_workflow_sign_off(
                        state, &store, authority, &run_id, false, req,
                    );
                }
                "abort" => {
                    return handle_post_workflow_abort(state, &store, authority, &run_id, req);
                }
                _ => {}
            }
        }
    }

    // /api/v1/gate/secrets/:name, /api/v1/gate/mcp/:connector, or /api/v1/gate/bridges/:bridge_id/webhook
    if segments.len() >= 4 && segments[0] == "api" && segments[1] == "v1" && segments[2] == "gate" {
        if segments.len() == 5 && segments[3] == "secrets" && method == "DELETE" {
            let secret_name = percent_decode(segments[4]);
            return handle_delete_gate_secret(state, authority, &secret_name);
        }
        if segments.len() == 5 && segments[3] == "mcp" && method == "POST" {
            let connector_id = percent_decode(segments[4]);
            return handle_post_gate_mcp(state, authority, Some(&connector_id), req);
        }
        if segments.len() == 6
            && segments[3] == "bridges"
            && segments[5] == "webhook"
            && method == "POST"
        {
            let bridge_id = percent_decode(segments[4]);
            return handle_post_gate_bridge_webhook(state, authority, &bridge_id, req);
        }
    }

    // /api/v1/nodes/:id — after sessions and attachments so those prefixes win.
    if segments.len() == 4
        && segments[0] == "api"
        && segments[1] == "v1"
        && segments[2] == "nodes"
        && method == "DELETE"
    {
        return handle_delete_node(state, authority, segments[3]);
    }

    HttpResponse::json(404, json!({ "error": "not found" }))
}

fn resolve_session_target(state: &HubState, raw_session_id: &str) -> (String, Option<String>) {
    let snap = load_discovery_snapshot(state);
    resolve_session_target_from_snapshot(state, &snap, raw_session_id)
}

fn resolve_session_target_from_snapshot(
    state: &HubState,
    snap: &DiscoveryStoreSnapshot,
    raw_session_id: &str,
) -> (String, Option<String>) {
    let trimmed = raw_session_id.trim();
    // Only `id` and `persona` are read, so skip model discovery.
    let sessions =
        discover_route_sessions_without_model_info(snap, state.config.home_override.as_deref());
    if let Some(matched) = sessions
        .iter()
        .find(|s| s.id == trimmed || s.persona.as_deref() == Some(trimmed))
    {
        return (matched.id.clone(), matched.persona.clone());
    }
    (trimmed.to_string(), Some(trimmed.to_ascii_lowercase()))
}

pub(crate) fn route_matches_session_id(
    route: &fl_wire::identity::VendorRoute,
    raw_id: &str,
) -> bool {
    let trimmed = raw_id.trim();
    match route {
        fl_wire::identity::VendorRoute::Claude { session_id, .. } => {
            session_id == trimmed || trimmed.strip_prefix("claude-") == Some(session_id.as_str())
        }
        fl_wire::identity::VendorRoute::Agy {
            conversation_id, ..
        } => {
            conversation_id == trimmed
                || trimmed.strip_prefix("agy-") == Some(conversation_id.as_str())
        }
        fl_wire::identity::VendorRoute::Codex { thread_id, .. } => {
            thread_id == trimmed || trimmed.strip_prefix("codex-") == Some(thread_id.as_str())
        }
        fl_wire::identity::VendorRoute::Muse { session_id, .. } => {
            session_id == trimmed || trimmed.strip_prefix("muse-") == Some(session_id.as_str())
        }
        fl_wire::identity::VendorRoute::Grok { session_id, .. } => {
            session_id == trimmed || trimmed.strip_prefix("grok-") == Some(session_id.as_str())
        }
        fl_wire::identity::VendorRoute::Webhook { .. } => false,
    }
}

fn vendor_route_for_session(
    state: &HubState,
    persona: Option<&str>,
    fallback_session_id: &str,
) -> Result<fl_wire::identity::VendorRoute, String> {
    crate::lifecycle::find_route(
        state.config.home_override.as_deref(),
        persona,
        fallback_session_id,
    )
    .ok_or_else(|| {
        format!(
            "no active vendor route for `{}`",
            persona.unwrap_or(fallback_session_id)
        )
    })
}

fn dispatch_operator_prompt_for_session(
    state: &HubState,
    persona: Option<&str>,
    fallback_session_id: &str,
    sender_name: &str,
    effective_body: &str,
) -> Result<(), String> {
    let route = vendor_route_for_session(state, persona, fallback_session_id)?;
    dispatch_operator_prompt_to_route(&route, sender_name, effective_body)
}

/// Automatically drain and dispatch the next queued operator prompt for any session whose
/// transcript/in-flight state has transitioned from busy to idle.
pub fn drain_ready_session_queues(state: &HubState) {
    let keys = state.queued_session_keys();
    if keys.is_empty() {
        return;
    }
    let snap = {
        let Ok(store) = state.open_store() else {
            return;
        };
        snapshot_discovery_store(&store)
    };
    let home = state.config.home_override.as_deref();
    for session_id in keys {
        let turns = read_session_transcript_from_snapshot(&session_id, &snap, home);
        if !state.is_session_busy_from_turns(&session_id, &turns)
            && let Some((item, queue)) = state.pop_next_queued_prompt(&session_id)
        {
            let delivery_res = dispatch_operator_prompt_for_session(
                state,
                item.persona.as_deref(),
                &item.session_id,
                &item.by,
                &item.effective_body,
            );
            let after = read_session_transcript_from_snapshot(&session_id, &snap, home);
            state.mark_session_in_flight_from_turns(&session_id, &after);
            let sent_data = json!({
                "session_id": item.session_id,
                "persona": item.persona,
                "status": "sent",
                "queued": false,
                "is_busy": true,
                "delivered": delivery_res.is_ok(),
                "delivery_error": delivery_res.as_ref().err(),
                "prompt_id": item.id,
                "prompt": item.prompt,
                "effective_body": item.effective_body,
                "by": item.by,
                "queue": queue,
                "attachments": item.attachments,
            });
            state.broadcast("session", "prompt_sent", sent_data);
            state.broadcast(
                "session",
                "queue_updated",
                json!({
                    "session_id": item.session_id,
                    "persona": item.persona,
                    "is_busy": true,
                    "queue": queue,
                }),
            );
        }
    }
}

fn handle_post_queued_prompt_send_now(
    state: &HubState,
    raw_session_id: &str,
    prompt_id: &str,
) -> HttpResponse {
    let (canonical_id, target_persona) = resolve_session_target(state, raw_session_id);
    let removed = state
        .remove_queued_prompt(&canonical_id, prompt_id)
        .or_else(|| state.remove_queued_prompt(raw_session_id, prompt_id));
    let Some((item, queue)) = removed else {
        return HttpResponse::json(404, json!({ "error": "queued prompt not found" }));
    };

    let delivery_res = dispatch_operator_prompt_for_session(
        state,
        item.persona.as_deref().or(target_persona.as_deref()),
        &canonical_id,
        &item.by,
        &item.effective_body,
    );
    state.mark_session_in_flight_released(&canonical_id);

    let data = json!({
        "session_id": canonical_id,
        "persona": item.persona.clone().or(target_persona.clone()),
        "status": "sent",
        "queued": false,
        "is_busy": true,
        "delivered": delivery_res.is_ok(),
        "delivery_error": delivery_res.as_ref().err(),
        "prompt_id": prompt_id,
        "prompt": item.prompt,
        "effective_body": item.effective_body,
        "by": item.by,
        "queue": queue,
        "attachments": item.attachments,
    });
    state.broadcast("session", "prompt_sent", data.clone());
    state.broadcast(
        "session",
        "queue_updated",
        json!({
            "session_id": canonical_id,
            "persona": item.persona.or(target_persona),
            "is_busy": true,
            "queue": queue,
        }),
    );
    HttpResponse::json(200, data)
}

fn handle_patch_queued_prompt(
    state: &HubState,
    raw_session_id: &str,
    prompt_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize, Default)]
    struct PatchQueuedPromptPayload {
        #[serde(default)]
        prompt: String,
    }

    let payload: PatchQueuedPromptPayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let trimmed = payload.prompt.trim().to_string();
    if trimmed.is_empty() {
        return HttpResponse::json(400, json!({ "error": "prompt must not be empty" }));
    }

    let (canonical_id, target_persona) = resolve_session_target(state, raw_session_id);
    let updated = state
        .update_queued_prompt(&canonical_id, prompt_id, trimmed.clone())
        .or_else(|| state.update_queued_prompt(raw_session_id, prompt_id, trimmed));
    let Some((item, queue)) = updated else {
        return HttpResponse::json(404, json!({ "error": "queued prompt not found" }));
    };

    let is_busy = state.is_session_busy_released(&canonical_id);
    state.broadcast(
        "session",
        "queue_updated",
        json!({
            "session_id": canonical_id,
            "persona": item.persona.clone().or(target_persona),
            "is_busy": is_busy,
            "queue": queue,
        }),
    );
    HttpResponse::json(
        200,
        json!({
            "session_id": canonical_id,
            "status": "updated",
            "prompt_id": prompt_id,
            "queued_prompt": item,
            "is_busy": is_busy,
            "queue": queue,
        }),
    )
}

fn handle_delete_queued_prompt(
    state: &HubState,
    raw_session_id: &str,
    prompt_id: &str,
) -> HttpResponse {
    let (canonical_id, target_persona) = resolve_session_target(state, raw_session_id);
    let removed = state
        .remove_queued_prompt(&canonical_id, prompt_id)
        .or_else(|| state.remove_queued_prompt(raw_session_id, prompt_id));
    let Some((item, queue)) = removed else {
        return HttpResponse::json(404, json!({ "error": "queued prompt not found" }));
    };

    let is_busy = state.is_session_busy_released(&canonical_id);
    state.broadcast(
        "session",
        "queue_updated",
        json!({
            "session_id": canonical_id,
            "persona": item.persona.or(target_persona),
            "is_busy": is_busy,
            "queue": queue,
        }),
    );
    HttpResponse::json(
        200,
        json!({
            "session_id": canonical_id,
            "status": "cancelled",
            "prompt_id": prompt_id,
            "is_busy": is_busy,
            "queue": queue,
        }),
    )
}

fn handle_post_session_prompt(
    state: &HubState,
    authority: &HubAuthority,
    session_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize)]
    struct PromptAttachmentInput {
        name: String,
        #[serde(default, alias = "mime_type")]
        media_type: String,
        data_base64: String,
    }

    #[derive(Deserialize)]
    struct PromptPayload {
        #[serde(default)]
        prompt: String,
        #[serde(default)]
        by: Option<String>,
        #[serde(default)]
        send_now: bool,
        #[serde(default)]
        attachments: Vec<PromptAttachmentInput>,
    }

    let payload: PromptPayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    if payload.prompt.trim().is_empty() && payload.attachments.is_empty() {
        return HttpResponse::json(
            400,
            json!({ "error": "prompt or attachments must not be empty" }),
        );
    }

    let mut saved_attachments = Vec::with_capacity(payload.attachments.len());
    if !payload.attachments.is_empty() {
        let dir = attachments_dir(state);
        if let Err(e) = fs::create_dir_all(&dir) {
            return HttpResponse::json(
                500,
                json!({ "error": format!("failed to create attachments dir: {e}") }),
            );
        }
        let now_ms = stamp::now().unix_millis();
        for (idx, att) in payload.attachments.iter().enumerate() {
            let raw_b64 = match att.data_base64.find(";base64,") {
                Some(pos) if att.data_base64.trim_start().starts_with("data:") => {
                    &att.data_base64[pos + ";base64,".len()..]
                }
                _ => att.data_base64.as_str(),
            };
            let decoded = match base64::engine::general_purpose::STANDARD.decode(raw_b64.trim()) {
                Ok(b) => b,
                Err(e) => {
                    return HttpResponse::json(
                        400,
                        json!({ "error": format!("invalid base64 in attachment `{}`: {e}", att.name) }),
                    );
                }
            };
            let mut sanitized: String = att
                .name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            sanitized = sanitized.trim_start_matches('.').to_string();
            if sanitized.is_empty() {
                sanitized = "attachment.bin".to_string();
            }
            let saved_name = format!("{now_ms}-{idx}-{sanitized}");
            let saved_path = dir.join(&saved_name);
            if let Err(e) = fs::write(&saved_path, &decoded) {
                return HttpResponse::json(
                    500,
                    json!({ "error": format!("failed to write attachment: {e}") }),
                );
            }
            let media_type = if att.media_type.trim().is_empty() {
                mime_type_for_path(&saved_path).to_string()
            } else {
                att.media_type.trim().to_string()
            };
            saved_attachments.push(SavedPromptAttachment {
                name: sanitized,
                media_type,
                path: saved_path.display().to_string(),
                url: format!("/api/v1/attachments/{saved_name}"),
            });
        }
    }

    let prompt_text = if payload.prompt.trim().is_empty() {
        "Attached file(s).".to_string()
    } else {
        payload.prompt.trim().to_string()
    };
    let mut effective_body = prompt_text.clone();
    for saved in &saved_attachments {
        effective_body.push_str(&format!(
            "\n\n[Attached file: {} ({})]",
            saved.path, saved.url
        ));
    }

    let sender_name = resolve_prompt_sender(authority, payload.by.as_deref());

    drain_ready_session_queues(state);
    let (canonical_session_id, target_persona) = resolve_session_target(state, session_id);
    let busy = state.is_session_busy_released(&canonical_session_id);
    let has_queued = !state.session_queue(&canonical_session_id).is_empty();

    if !payload.send_now && (busy || has_queued) {
        let (item, queue) = state.enqueue_session_prompt(
            &canonical_session_id,
            target_persona.clone(),
            prompt_text,
            effective_body,
            sender_name,
            saved_attachments.clone(),
        );
        state.broadcast(
            "session",
            "queue_updated",
            json!({
                "session_id": canonical_session_id,
                "persona": target_persona,
                "is_busy": true,
                "queue": queue,
            }),
        );
        return HttpResponse::json(
            200,
            json!({
                "session_id": canonical_session_id,
                "persona": target_persona,
                "status": "queued",
                "queued": true,
                "is_busy": true,
                "queued_prompt": item,
                "queue": queue,
                "attachments": saved_attachments,
            }),
        );
    }

    let delivery_res = dispatch_operator_prompt_for_session(
        state,
        target_persona.as_deref(),
        &canonical_session_id,
        &sender_name,
        &effective_body,
    );
    state.mark_session_in_flight_released(&canonical_session_id);
    let queue = state.session_queue(&canonical_session_id);

    let data = json!({
        "session_id": canonical_session_id,
        "persona": target_persona,
        "status": "sent",
        "queued": false,
        "is_busy": true,
        "delivered": delivery_res.is_ok(),
        "delivery_error": delivery_res.as_ref().err(),
        "prompt": prompt_text,
        "effective_body": effective_body,
        "by": sender_name,
        "queue": queue,
        "attachments": saved_attachments,
    });
    state.broadcast("session", "prompt_sent", data.clone());
    HttpResponse::json(200, data)
}

/// The sender name of an operator prompt. `claimed_by` is client-supplied text: it labels the
/// prompt, and it must never decide who the sender is.
///
/// An Owner keeps the label it asked for (an unusable one becomes `owner`). Anyone else is a
/// peer: a claim of the owner's name, or no usable claim, falls back to the peer's own
/// authenticated name, which [`HubAuthority::actor_name`] never makes the owner's. The result
/// is the only thing that decides whether the prompt may arrive as direct terminal input (see
/// `fl_wire::dispatcher::is_owner_sender`), so a peer cannot produce an owner name.
fn resolve_prompt_sender(authority: &HubAuthority, claimed_by: Option<&str>) -> String {
    let own = authority.actor_name();
    let claimed = claimed_by.unwrap_or(own);
    if authority.is_owner() {
        return validate_sender_name(claimed).unwrap_or_else(|_| "owner".to_string());
    }
    match validate_sender_name(claimed) {
        Ok(name) if !fl_wire::dispatcher::is_owner_sender(&name) => name,
        _ => match validate_sender_name(own) {
            Ok(name) if !fl_wire::dispatcher::is_owner_sender(&name) => name,
            _ => "peer".to_string(),
        },
    }
}

/// The sender label of a prompt that arrives through an inbound bridge webhook. The webhook
/// body is third-party text: the secret (or Owner auth) that admits the call says nothing about
/// who wrote the message, so the label is never an owner name. An unusable label, or one that
/// names the owner, becomes `bridge`.
fn bridge_prompt_sender(claimed: &str) -> String {
    match validate_sender_name(claimed) {
        Ok(name) if !fl_wire::dispatcher::is_owner_sender(&name) => name,
        _ => "bridge".to_string(),
    }
}

thread_local! {
    static HYDRATE_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

static HYDRATE_LOCK: Mutex<()> = Mutex::new(());

/// Serialize `FL_HUB_AGENT_HYDRATE_BIN` reads with tests that set it.
/// Re-entrant on the same thread so a test can hold the lock across the handler.
pub(crate) fn with_hydrate_lock<T>(body: impl FnOnce() -> T) -> T {
    struct DepthGuard;
    impl Drop for DepthGuard {
        fn drop(&mut self) {
            HYDRATE_LOCK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        }
    }

    let outer = HYDRATE_LOCK_DEPTH.with(|depth| depth.get()) == 0;
    let _guard = if outer {
        Some(HYDRATE_LOCK.lock().unwrap_or_else(|err| err.into_inner()))
    } else {
        None
    };
    HYDRATE_LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let _depth = DepthGuard;
    body()
}

fn handle_post_session_clear(
    state: &HubState,
    authority: &HubAuthority,
    session_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize)]
    struct ClearPayload {
        mode: String,
    }

    let payload: ClearPayload = match req.json() {
        Ok(payload) => payload,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let mode = match payload.mode.trim().to_ascii_lowercase().as_str() {
        "clear" | "clear_only" => "clear",
        "reforge" | "rehydrate" => "reforge",
        _ => {
            return HttpResponse::json(
                400,
                json!({ "error": "mode must be \"clear\" or \"reforge\"" }),
            );
        }
    };

    let (canonical_id, persona) = resolve_session_target(state, session_id);
    match reset_session(
        state,
        authority.actor_name(),
        session_id,
        &canonical_id,
        persona.as_deref(),
        mode,
    ) {
        Ok(data) => HttpResponse::json(200, data),
        Err((status, error)) => HttpResponse::json(status, json!({ "error": error })),
    }
}

/// Clear or reforge one session: drop its queued prompts, build the reforge prompt, and reset
/// the live vendor route. `mode` is `"clear"` or `"reforge"`. `raw_session_id` is the id the
/// caller used (its queue is dropped too); `canonical_id` and `persona` come from
/// [`resolve_session_target`]. Returns the response body, or `(status, error)`: 404 when there
/// is no agent record (reforge) or no live route, 502 when the dispatch itself fails.
/// Shared by the single-session clear handler and the bulk action.
fn reset_session(
    state: &HubState,
    actor: &str,
    raw_session_id: &str,
    canonical_id: &str,
    persona: Option<&str>,
    mode: &str,
) -> Result<Value, (u16, String)> {
    state.clear_session_prompt_queue(canonical_id);
    if raw_session_id != canonical_id {
        state.clear_session_prompt_queue(raw_session_id);
    }
    if let Some(name) = persona
        && name != canonical_id
        && name != raw_session_id
    {
        state.clear_session_prompt_queue(name);
    }
    // The session starts over, so a model the operator chose for the old run no longer applies.
    for key in [Some(canonical_id), Some(raw_session_id), persona]
        .into_iter()
        .flatten()
    {
        state.clear_session_model_override(key);
    }

    let prompt = if mode == "reforge" {
        let lookup = persona.unwrap_or(canonical_id);
        match build_reforge_prompt(state, lookup) {
            Some(prompt) => Some(prompt),
            None => {
                return Err((404, format!("no agent record for reforge of `{lookup}`")));
            }
        }
    } else {
        None
    };

    let route = vendor_route_for_session(state, persona, canonical_id).map_err(|e| (404, e))?;
    let reset_mode = if mode == "reforge" {
        SessionResetMode::Reforge
    } else {
        SessionResetMode::Clear
    };
    clear_or_reforge_session(&route, reset_mode, actor, prompt.as_deref()).map_err(|e| (502, e))?;

    let data = json!({
        "ok": true,
        "session_id": canonical_id,
        "persona": persona,
        "mode": mode,
        "prompt": prompt,
    });
    state.broadcast("sessions", "session.cleared", data.clone());
    Ok(data)
}

/// `{ "reason"?: string }`: an optional body. An empty body or `{}` is fine.
fn optional_reason(req: &HttpRequest) -> Result<Option<String>, HttpResponse> {
    let body = json_object_body(req)?;
    match body.get("reason") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(reason)) => Ok(Some(reason.chars().take(500).collect())),
        Some(_) => Err(HttpResponse::json(
            400,
            json!({ "error": "`reason` must be a string" }),
        )),
    }
}

/// `POST /api/v1/sessions/:id/interrupt` (Record 47): stop the turn the session is running.
/// Any authenticated token may call it, as for clear. 404 when the session has no live route;
/// 502 when the interrupt cannot be delivered. See [`session_actions::interrupt_route`] for
/// the per-vendor mechanism and the test-mode no-op.
fn handle_post_session_interrupt(
    state: &HubState,
    authority: &HubAuthority,
    session_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    let reason = match optional_reason(req) {
        Ok(reason) => reason,
        Err(resp) => return resp,
    };
    let (canonical_id, persona) = resolve_session_target(state, session_id);
    let route = match vendor_route_for_session(state, persona.as_deref(), &canonical_id) {
        Ok(route) => route,
        Err(e) => return HttpResponse::json(404, json!({ "error": e })),
    };
    // Claude and Codex routes carry no tmux target; a hosted session's record names its pane.
    // A Claude session the hub does not host falls back to the live pane its key file names.
    let hosted_tmux = hosted_record_for(state, &[session_id, &canonical_id], persona.as_deref())
        .and_then(|record| record.tmux_target)
        .or_else(|| fl_wire::dispatcher::claude_route_live_pane(&route));
    let method = match session_actions::interrupt_route(
        &lifecycle_env(state),
        &route,
        hosted_tmux.as_deref(),
    ) {
        Ok(method) => method,
        Err(e) => return HttpResponse::json(502, json!({ "error": e })),
    };
    // Whatever prompt was in flight is no longer running, so the session stops reading as busy.
    // Queued prompts stay queued and drain as usual once the session is idle.
    state.clear_session_in_flight(&canonical_id);
    if session_id != canonical_id {
        state.clear_session_in_flight(session_id);
    }
    let vendor = route_vendor(&route);
    let data = json!({
        "session_id": canonical_id,
        "interrupted": true,
        "vendor": vendor,
        "method": method,
    });
    let mut event = data.clone();
    event["persona"] = json!(persona);
    event["reason"] = json!(reason);
    event["by"] = json!(authority.actor_name());
    state.broadcast("sessions", "session.interrupted", event);
    HttpResponse::json(200, data)
}

/// The hosted-session record for one of `keys` (or the persona), if the store has one.
fn hosted_record_for(
    state: &HubState,
    keys: &[&str],
    persona: Option<&str>,
) -> Option<lifecycle::HostedSessionRecord> {
    let mut all: Vec<&str> = keys.to_vec();
    if let Some(persona) = persona {
        all.push(persona);
    }
    let store = state.open_store().ok()?;
    lifecycle::find_hosted_session(&*store, &all).ok().flatten()
}

/// `POST /api/v1/sessions/:id/model` (Records 48 and 57): switch the model of a session.
/// Any authenticated token may call it, as for clear. The choice is kept in `HubState` and
/// overlaid on `GET /api/v1/sessions` (in memory: gone after a hub restart), written to the
/// hosted-session record when one exists, and sent to a live CLI as `/model <id>`.
/// Antigravity is not sent anything: its method is `override-only`.
fn handle_post_session_model(
    state: &HubState,
    authority: &HubAuthority,
    session_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let model_raw = match json_string(&body, "model") {
        Ok(model) => model,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let model = match session_actions::validate_model(&model_raw) {
        Ok(model) => model,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let thinking_level = match body.get("thinking_level") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => match session_actions::validate_thinking_level(raw) {
            Ok(level) => Some(level),
            Err(e) => return HttpResponse::json(400, json!({ "error": e })),
        },
        Some(_) => {
            return HttpResponse::json(
                400,
                json!({ "error": "`thinking_level` must be a string" }),
            );
        }
    };

    let snap = load_discovery_snapshot(state);
    // Only `id`, `persona` and `adapter` are read, so skip model discovery.
    let sessions =
        discover_route_sessions_without_model_info(&snap, state.config.home_override.as_deref());
    let trimmed = session_id.trim();
    let matched = sessions
        .iter()
        .find(|s| s.id == trimmed || s.persona.as_deref() == Some(trimmed));
    let (canonical_id, persona) = match matched {
        Some(s) => (s.id.clone(), s.persona.clone()),
        None => (trimmed.to_string(), Some(trimmed.to_ascii_lowercase())),
    };
    let route = vendor_route_for_session(state, persona.as_deref(), &canonical_id).ok();
    let hosted = hosted_record_for(state, &[trimmed, &canonical_id], persona.as_deref());
    if matched.is_none() && route.is_none() && hosted.is_none() {
        return HttpResponse::json(
            404,
            json!({ "error": format!("unknown session `{trimmed}`") }),
        );
    }
    let vendor = match (&route, matched, &hosted) {
        (Some(route), _, _) => route_vendor(route).to_string(),
        (None, Some(s), _) => s.adapter.clone(),
        (None, None, Some(record)) => record.vendor.clone(),
        (None, None, None) => "unknown".to_string(),
    };

    // Key the override by the discovered session id. A session discovery does not list (a
    // route or hosted record only) has no id a summary could match, so it is keyed by its
    // lowercased persona, which `apply_model_overrides` also matches.
    let override_key = match matched {
        Some(s) => s.id.clone(),
        None => hosted
            .as_ref()
            .map(|record| record.persona.as_str())
            .or(persona.as_deref())
            .unwrap_or(&canonical_id)
            .to_ascii_lowercase(),
    };
    state.set_session_model_override(
        &override_key,
        ModelOverride {
            model: model.clone(),
            thinking_level: thinking_level.clone(),
        },
    );
    if hosted.is_some() {
        match open_store_response(state) {
            Ok(store) => {
                // Best effort: the override above already answers `GET /api/v1/sessions`.
                let _ = lifecycle::set_hosted_session_model(
                    &*store,
                    &[trimmed, &canonical_id],
                    &model,
                    authority.actor_name(),
                );
            }
            Err(()) => return store_open_failure(),
        }
    }

    let method = match &route {
        Some(route) if !matches!(route, fl_wire::identity::VendorRoute::Agy { .. }) => {
            match dispatch_operator_prompt_to_route(
                route,
                authority.actor_name(),
                &format!("/model {model}"),
            ) {
                Ok(()) => METHOD_OPERATOR_PROMPT,
                Err(_) => METHOD_OVERRIDE_ONLY,
            }
        }
        _ => METHOD_OVERRIDE_ONLY,
    };

    let data = json!({
        "ok": true,
        "session_id": canonical_id,
        "vendor": vendor,
        "model": model,
        "thinking_level": thinking_level,
        "switched": true,
        "method": method,
    });
    state.broadcast("sessions", "session.model_changed", data.clone());
    HttpResponse::json(200, data)
}

/// `POST /api/v1/sessions/bulk-action` (Record 44): run `compact`, `clear` or `reforge` on
/// every discovered session that has a live route. Owner only. One failure never stops the
/// batch; each session reports its own result.
fn handle_post_session_bulk_action(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may run a bulk session action" }),
        );
    }
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let action = match json_string(&body, "action") {
        Ok(action) => action.trim().to_ascii_lowercase(),
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    if !matches!(action.as_str(), "compact" | "clear" | "reforge") {
        return HttpResponse::json(
            400,
            json!({ "error": "action must be \"compact\", \"clear\" or \"reforge\"" }),
        );
    }

    let snap = load_discovery_snapshot(state);
    let targets: Vec<(String, Option<String>)> =
        discover_route_sessions_without_model_info(&snap, state.config.home_override.as_deref())
            .into_iter()
            .filter(|s| s.source == "route")
            .map(|s| (s.id, s.persona))
            .collect();
    let actor = authority.actor_name();
    let mut results = Vec::with_capacity(targets.len());
    for (id, persona) in &targets {
        let outcome: Result<(), String> = match action.as_str() {
            "compact" => compact_session(state, actor, id, persona.as_deref()),
            mode => reset_session(state, actor, id, id, persona.as_deref(), mode)
                .map(|_| ())
                .map_err(|(_, error)| error),
        };
        results.push(match outcome {
            Ok(()) => json!({ "session_id": id, "ok": true }),
            Err(error) => json!({ "session_id": id, "ok": false, "error": error }),
        });
    }
    let succeeded = results.iter().filter(|r| r["ok"] == true).count();
    let data = json!({
        "action": action,
        "total": results.len(),
        "succeeded": succeeded,
        "results": results,
    });
    state.broadcast("sessions", "session.bulk_action", data.clone());
    HttpResponse::json(200, data)
}

/// Send `/compact` to one session through the operator-prompt path.
fn compact_session(
    state: &HubState,
    actor: &str,
    session_id: &str,
    persona: Option<&str>,
) -> Result<(), String> {
    dispatch_operator_prompt_for_session(state, persona, session_id, actor, "/compact")?;
    state.mark_session_in_flight_released(session_id);
    Ok(())
}

/// `GET /api/v1/commands` (Record 30): built-in slash commands plus the workspace's skills
/// and workflows. The workspace is the hub's `repo_root`.
fn handle_get_commands(state: &HubState) -> HttpResponse {
    let commands = crate::commands::list_commands(&state.config.repo_root);
    HttpResponse::json(200, json!({ "commands": commands }))
}

fn lifecycle_error_response(err: &LifecycleError) -> HttpResponse {
    HttpResponse::json(err.status(), json!({ "error": err.to_string() }))
}

fn lifecycle_env(state: &HubState) -> LifecycleEnv<'_> {
    LifecycleEnv::new(state.config.home_override.as_deref())
}

fn caller_of(authority: &HubAuthority) -> lifecycle::Caller {
    if authority.is_owner() {
        lifecycle::Caller::Owner
    } else {
        lifecycle::Caller::Peer
    }
}

fn handle_post_session_spin_up(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    let payload: SpinUpRequest = match req.json() {
        Ok(payload) => payload,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match lifecycle::spin_up(
        &*store,
        &lifecycle_env(state),
        &payload,
        authority.actor_name(),
        caller_of(authority),
    ) {
        Ok(out) => {
            // A new run has its own model: drop what the operator chose for an earlier one.
            state.clear_session_model_override(&out.session_id);
            state.clear_session_model_override(&out.persona);
            let data = serde_json::to_value(&out).unwrap_or_default();
            state.broadcast("sessions", "session.spun_up", data.clone());
            HttpResponse::json(200, data)
        }
        Err(e) => lifecycle_error_response(&e),
    }
}

fn handle_post_session_decommission(
    state: &HubState,
    authority: &HubAuthority,
    session_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize)]
    struct DecommissionPayload {
        #[serde(default)]
        mode: String,
    }

    let payload: DecommissionPayload = match req.json() {
        Ok(payload) => payload,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let mode = match DecommissionMode::parse(&payload.mode) {
        Ok(mode) => mode,
        Err(e) => return lifecycle_error_response(&e),
    };

    // Resolve before taking the store guard: discovery opens the store itself.
    let discovered = resolve_session_target(state, session_id);
    let discovered_id = discovered.0.clone();
    // Read-only discovery of what the session's transcript and brain reference; it opens the
    // store itself, so it runs before the guard below. Nothing is archived until the teardown
    // gate has passed.
    let artifact_candidates = if mode == DecommissionMode::Teardown {
        discover_session_artifact_candidates(state, session_id, &discovered_id)
    } else {
        Vec::new()
    };
    let by = authority.actor_name();
    let env = lifecycle_env(state);
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    let target = match SessionTarget::resolve(&*store, session_id, Some(discovered)) {
        Ok(target) => target,
        Err(e) => return lifecycle_error_response(&e),
    };
    let caller = caller_of(authority);
    let result = match mode {
        DecommissionMode::Prepare => {
            lifecycle::prepare_decommission(&*store, &env, &target, by, caller)
        }
        DecommissionMode::Teardown => {
            // Keep the session's artifacts before the teardown lets its worktree go. Asking the
            // teardown's own gate first means a refused caller archives nothing. Best effort:
            // archive_session_artifacts logs and swallows its failures, so the teardown and its
            // status never depend on it.
            if lifecycle::authorize_teardown(&*store, &target, caller).is_ok() {
                archive_session_artifacts(state, &target, &discovered_id, artifact_candidates);
            }
            lifecycle::teardown_session(&*store, &env, &target, by, caller, |key| {
                state.clear_session_prompt_queue(key);
                state.clear_session_model_override(key);
            })
        }
    };
    let out = match result {
        Ok(out) => out,
        Err(e) => return lifecycle_error_response(&e),
    };
    if let (DecommissionMode::Prepare, false, Some(prompt)) =
        (mode, out.handshake_dispatched, out.prompt.as_ref())
    {
        // No live route took the handshake: leave it on the session queue for the next idle turn.
        let (_, queue) = state.enqueue_session_prompt(
            &target.session_id,
            Some(target.persona.clone()),
            prompt.clone(),
            prompt.clone(),
            by.to_string(),
            Vec::new(),
        );
        state.broadcast(
            "session",
            "queue_updated",
            json!({
                "session_id": target.session_id,
                "persona": target.persona,
                "is_busy": true,
                "queue": queue,
            }),
        );
    }
    let data = serde_json::to_value(&out).unwrap_or_default();
    state.broadcast("sessions", "session.decommission", data.clone());
    HttpResponse::json(200, data)
}

fn handle_get_hosted_sessions(state: &HubState) -> HttpResponse {
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match lifecycle::list_hosted_sessions(&*store) {
        Ok(sessions) => HttpResponse::json(200, json!({ "sessions": sessions })),
        Err(e) => lifecycle_error_response(&e),
    }
}

fn handle_post_hosted_session_restart(
    state: &HubState,
    authority: &HubAuthority,
    id: &str,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may restart a hosted session" }),
        );
    }
    let Ok(store) = open_store_response(state) else {
        return store_open_failure();
    };
    match lifecycle::restart_hosted_session(
        &*store,
        &lifecycle_env(state),
        id,
        authority.actor_name(),
    ) {
        Ok(out) => {
            let data = serde_json::to_value(&out).unwrap_or_default();
            state.broadcast("sessions", "session.restarted", data.clone());
            HttpResponse::json(200, data)
        }
        Err(e) => lifecycle_error_response(&e),
    }
}

fn build_reforge_prompt(state: &HubState, persona: &str) -> Option<String> {
    if let Some(prompt) = reforge_prompt_from_hydrate(state, persona)
        && !prompt.trim().is_empty()
    {
        return Some(prompt);
    }
    let agent = agent_record_for_persona(state, persona)?;
    Some(canonical_reforge_prompt(
        &agent.display_name,
        &agent.project,
        &reforge_role_label(&agent.role),
    ))
}

fn agent_record_for_persona(state: &HubState, persona: &str) -> Option<AgentRecord> {
    let store = state.open_store().ok()?;
    let agents = store.list_agents(None, true).ok()?;
    drop(store);
    let key = persona.trim();
    let mut matches: Vec<AgentRecord> = agents
        .into_iter()
        .filter(|agent| {
            agent.name_key.eq_ignore_ascii_case(key) || agent.display_name.eq_ignore_ascii_case(key)
        })
        .collect();
    matches.sort_by_key(|agent| agent.status != AgentStatus::Active);
    matches.into_iter().next()
}

pub(crate) fn reforge_role_label(role: &AgentRole) -> String {
    match role {
        AgentRole::Lead => "lead".to_string(),
        AgentRole::Assistant => "assistant".to_string(),
        AgentRole::LabManager => "lab_manager".to_string(),
        AgentRole::Custom(slug) => slug.clone(),
    }
}

pub(crate) fn canonical_reforge_prompt(name: &str, project: &str, role: &str) -> String {
    let name_lower = name.to_ascii_lowercase();
    format!(
        "Take the name of {name} and take up residence in {project}/ as the {role}.\n\
         \n\
         This session has just been reset (`/clear`) for clean-slate context hygiene. Perform your root startup hydration now:\n\
         1. Read `.agents/AGENTS.md` and `.agents/REGISTRY.md`.\n\
         2. Read your role file(s): `.agents/roles/{role}.md`.\n\
         3. Read your project instruction files: `{project}/AGENTS.md`, `{project}/WORKFLOW.md`, `{project}/PERSONA.md`.\n\
         4. Read your project's current working state note: `Notes/{project}/STATE.md`.\n\
         5. Re-register your active session route so NATS and FerroDeck track this fresh session:\n\
            ```bash\n\
            AGENT_PERSONA={name_lower} agent-msg register {name} --project {project} --role {role} --replace\n\
            ```\n\
         6. Check and acknowledge any pending NATS messages:\n\
            ```bash\n\
            AGENT_PERSONA={name_lower} agent-msg read --ack\n\
            ```\n\
         \n\
         Identity summary: `{name}` | Co-author trailer: `Co-authored-by: {name} <{name_lower}@artificialhumanity.io>`"
    )
}

/// Prompt that tells an agent it is being prepared for decommission and must save and hand off
/// its work. `role` is a [`reforge_role_label`] value; a Lead hands off to `jarvis`.
pub(crate) fn synthesize_decommission_prompt(name: &str, project: &str, role: &str) -> String {
    let name_lower = name.to_ascii_lowercase();
    let recipient = match role {
        "lead" => "`jarvis`".to_string(),
        "lab_manager" => "the owner".to_string(),
        _ => format!(
            "the {project} Lead (`fl agent roster --project {project}` names it; use that lowercase name below)"
        ),
    };
    let to = if role == "lead" { "jarvis" } else { "<lead>" };
    format!(
        "{name}, you are being prepared for decommission. This session will be shut down after you finish; do not start new work. Complete these steps now:\n\
         1. Save unsaved progress, findings and your handoff in `Notes/{project}/STATE.md` and in FerroState (`fl state handoff set` for the handoff, `fl state checkpoint` for the git checkpoint).\n\
         2. Commit and push your work, or clean up your temp worktrees and branches (`git worktree remove`, `git branch -d`). Leave nothing only on this session's disk.\n\
         3. Send a final handoff to {recipient} over both channels:\n\
            ```bash\n\
            fl wire send {to} \"{name} decommission handoff\" \"<what is done, open, and where it lives>\" --from {name_lower}\n\
            AGENT_PERSONA={name_lower} agent-msg send {to} \"{name} decommission handoff\" \"<same summary>\"\n\
            ```\n\
         4. Acknowledge the remaining inbox messages:\n\
            ```bash\n\
            fl wire read --ack\n\
            AGENT_PERSONA={name_lower} agent-msg read --ack\n\
            ```\n\
         \n\
         Reply `decommission-ready` when all four steps are done. Identity summary: `{name}` | Project: `{project}` | Role: `{role}`"
    )
}

fn reforge_prompt_from_hydrate(state: &HubState, persona: &str) -> Option<String> {
    with_hydrate_lock(|| {
        let bin = hydrate_bin(state.config.home_override.as_deref())?;
        let stdout = read_command_stdout(
            &bin,
            &[persona, "--print", "--json"],
            Duration::from_secs(3),
        )?;
        let value = serde_json::from_str::<Value>(stdout.trim()).ok()?;
        let obj = value.as_object()?;
        // Only a case-insensitive display-name or persona key. Any other
        // entry must not replace the canonical template.
        for (key, item) in obj {
            if key.eq_ignore_ascii_case(persona) {
                return item.as_str().map(ToOwned::to_owned);
            }
        }
        None
    })
}

fn hydrate_bin(home_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("FL_HUB_AGENT_HYDRATE_BIN") {
        let path = PathBuf::from(raw);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    if home_override.is_some() {
        return None;
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("agent-hydrate");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("HOME")?;
    let local = PathBuf::from(home)
        .join(".local")
        .join("bin")
        .join("agent-hydrate");
    local.is_file().then_some(local)
}

fn read_command_stdout(bin: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                drop(reader);
                return None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(reader);
                return None;
            }
        }
    };
    // A descendant can keep stdout open after the child exits. Wait only
    // until the original deadline, then drop the reader. Do not kill the group.
    let remaining = timeout.saturating_sub(started.elapsed());
    let output = match rx.recv_timeout(remaining) {
        Ok(output) => output,
        Err(_) => {
            drop(reader);
            return None;
        }
    };
    reader.join().ok()?;
    status.success().then_some(output)
}

fn handle_post_pr_review(
    store: &RedbStore,
    authority: &HubAuthority,
    number: u64,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize, Default)]
    struct PrReviewPayload {
        verdict: Option<String>,
        #[serde(default)]
        approve: bool,
        #[serde(default)]
        request_changes: bool,
        #[serde(default)]
        body: String,
        by: Option<String>,
        project: Option<String>,
        head_sha: Option<String>,
        #[serde(default)]
        commits: Vec<PrCommitSummary>,
        #[serde(default)]
        files: Vec<String>,
    }

    let payload: PrReviewPayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let verdict = if payload.approve
        || payload.verdict.as_deref().is_some_and(|v| {
            v.eq_ignore_ascii_case("approved") || v.eq_ignore_ascii_case("approve")
        }) {
        ReviewVerdict::Approved
    } else if payload.request_changes
        || payload.verdict.as_deref().is_some_and(|v| {
            v.eq_ignore_ascii_case("changes_requested") || v.eq_ignore_ascii_case("request_changes")
        })
    {
        ReviewVerdict::ChangesRequested
    } else {
        ReviewVerdict::Commented
    };

    let project_slug = payload
        .project
        .unwrap_or_else(|| "ferroloop".to_string())
        .to_ascii_lowercase();
    let head_sha = payload.head_sha.unwrap_or_else(|| "HEAD".to_string());
    let agents = store.list_agents(None, true).unwrap_or_default();
    let eval = evaluate_pr_policy(
        &project_slug,
        &payload.commits,
        &payload.files,
        &[],
        &agents,
        DEFAULT_OWNER_LOGIN,
    );

    let caller_name = payload
        .by
        .as_deref()
        .unwrap_or_else(|| authority.actor_name())
        .trim()
        .to_ascii_lowercase();

    if !authority.is_owner() {
        let Some(caller) = agents
            .iter()
            .find(|a| a.status == AgentStatus::Active && a.name_key == caller_name)
        else {
            return HttpResponse::json(
                403,
                json!({ "error": format!("unknown or inactive agent `{caller_name}`") }),
            );
        };
        if matches!(
            verdict,
            ReviewVerdict::Approved | ReviewVerdict::ChangesRequested
        ) && let Err(refusal) = authorize_agent_action(&eval, caller)
        {
            return HttpResponse::json(
                403,
                json!({ "error": refusal.to_string(), "refusal": refusal }),
            );
        }
        let marker = FlReviewMarker {
            reviewer: caller.name_key.clone(),
            role: caller.role.clone(),
            project: eval.project.clone(),
            head_sha: head_sha.clone(),
            verdict,
            at: stamp::now(),
        };
        let formatted_body = format_review_body(&payload.body, &marker);
        return HttpResponse::json(
            200,
            json!({
                "number": number,
                "verdict": verdict,
                "reviewer": caller.name_key,
                "role": caller.role,
                "head_sha": head_sha,
                "body": formatted_body,
                "policy": eval,
            }),
        );
    }

    HttpResponse::json(
        200,
        json!({
            "number": number,
            "verdict": verdict,
            "reviewer": caller_name,
            "head_sha": head_sha,
            "body": payload.body,
            "policy": eval,
        }),
    )
}

fn handle_post_pr_merge(
    store: &RedbStore,
    authority: &HubAuthority,
    number: u64,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize, Default)]
    struct PrMergePayload {
        method: Option<String>,
        by: Option<String>,
        project: Option<String>,
        head_sha: Option<String>,
        #[serde(default)]
        commits: Vec<PrCommitSummary>,
        #[serde(default)]
        files: Vec<String>,
        #[serde(default)]
        reviews: Vec<PrReviewSummary>,
    }

    let payload: PrMergePayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let project_slug = payload
        .project
        .unwrap_or_else(|| "ferroloop".to_string())
        .to_ascii_lowercase();
    let head_sha = payload.head_sha.unwrap_or_else(|| "HEAD".to_string());
    let agents = store.list_agents(None, true).unwrap_or_default();
    let eval = evaluate_pr_policy(
        &project_slug,
        &payload.commits,
        &payload.files,
        &[],
        &agents,
        DEFAULT_OWNER_LOGIN,
    );

    let caller_name = payload
        .by
        .as_deref()
        .unwrap_or_else(|| authority.actor_name())
        .trim()
        .to_ascii_lowercase();

    if !authority.is_owner() {
        let Some(caller) = agents
            .iter()
            .find(|a| a.status == AgentStatus::Active && a.name_key == caller_name)
        else {
            return HttpResponse::json(
                403,
                json!({ "error": format!("unknown or inactive agent `{caller_name}`") }),
            );
        };
        if let Err(refusal) = authorize_agent_merge(&eval, caller) {
            return HttpResponse::json(
                403,
                json!({ "error": refusal.to_string(), "refusal": refusal }),
            );
        }
        if let Err(refusal) = verify_merge_approvals(
            &head_sha,
            &eval,
            &payload.reviews,
            &agents,
            DEFAULT_OWNER_LOGIN,
        ) {
            return HttpResponse::json(
                403,
                json!({ "error": refusal.to_string(), "refusal": refusal }),
            );
        }
    }

    HttpResponse::json(
        200,
        json!({
            "number": number,
            "merged": true,
            "method": payload.method.unwrap_or_else(|| "merge".to_string()),
            "merged_by": caller_name,
            "head_sha": head_sha,
            "policy": eval,
        }),
    )
}

fn handle_post_workflow_step(
    state: &HubState,
    store: &RedbStore,
    authority: &HubAuthority,
    run_id: &WorkflowRunId,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize, Default)]
    struct StepPayload {
        step_id: Option<String>,
        adapter: Option<String>,
        binary: Option<String>,
        #[serde(default)]
        offer_only: bool,
        timeout_secs: Option<u64>,
        by: Option<String>,
    }

    let payload: StepPayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let opts = StepExecutionOptions {
        step_id: payload.step_id,
        adapter_override: payload.adapter,
        binary_override: payload.binary,
        rate_card: None,
        timeout_secs: payload.timeout_secs.unwrap_or(900),
        budget_usd_micros: 1_000_000,
        offer_only: payload.offer_only,
        owner_login: state.config.owner_login.clone(),
        by: payload
            .by
            .unwrap_or_else(|| authority.actor_name().to_string()),
    };

    match execute_workflow_step(WorkflowEnv::single(store), run_id, &opts) {
        Ok(report) => {
            let data = json!({
                "step_id": report.step_id,
                "action": report.action,
                "run": report.run,
            });
            state.broadcast("workflow", "run_updated", json!({ "run": report.run }));
            HttpResponse::json(200, data)
        }
        Err(e) => {
            let status = if e.is_policy_refusal() { 403 } else { 400 };
            HttpResponse::json(status, json!({ "error": e.to_string() }))
        }
    }
}

fn handle_post_workflow_handoff(
    state: &HubState,
    store: &RedbStore,
    authority: &HubAuthority,
    run_id: &WorkflowRunId,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize)]
    struct HandoffPayload {
        step_id: String,
        #[serde(default)]
        visit_count: Option<u32>,
        summary: String,
        #[serde(default)]
        shipped: Vec<String>,
        #[serde(default)]
        in_flight: Vec<String>,
        #[serde(default)]
        blockers: Vec<String>,
        #[serde(default)]
        next_steps: Vec<String>,
        #[serde(default)]
        notes: Option<String>,
        #[serde(default)]
        by: Option<String>,
    }

    let payload: HandoffPayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let visit_count = payload.visit_count.unwrap_or(1).max(1);
    let by = payload
        .by
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| authority.actor_name().to_string());

    let handoff = HandoffEntry {
        summary: payload.summary,
        shipped: payload.shipped,
        in_flight: payload.in_flight,
        blockers: payload.blockers,
        next_steps: payload.next_steps,
        notes: payload.notes,
    };

    match record_step_handoff(
        store,
        run_id,
        &payload.step_id,
        visit_count,
        handoff,
        stamp::now(),
        &by,
    ) {
        Ok(entry) => {
            let data = json!({
                "run_id": run_id,
                "handoff": entry,
            });
            state.broadcast("workflow", "run_updated", data.clone());
            HttpResponse::json(200, data)
        }
        Err(e) => HttpResponse::json(400, json!({ "error": e.to_string() })),
    }
}

fn handle_post_workflow_sign_off(
    state: &HubState,
    store: &RedbStore,
    authority: &HubAuthority,
    run_id: &WorkflowRunId,
    approve: bool,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize, Default)]
    struct SignOffPayload {
        step_id: Option<String>,
        by: Option<String>,
        note: Option<String>,
        reason: Option<String>,
    }

    let payload: SignOffPayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let run = match store.get_workflow_run(run_id) {
        Ok(Some(r)) => r,
        Ok(None) => return HttpResponse::json(404, json!({ "error": "workflow run not found" })),
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };
    let def = match store.get_workflow_def(&run.workflow_id) {
        Ok(Some(d)) => d,
        Ok(None) => return HttpResponse::json(404, json!({ "error": "workflow def not found" })),
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };

    let step_id = match payload.step_id {
        Some(s) if !s.trim().is_empty() => s,
        _ => match run.active_steps.first() {
            Some(s) => s.clone(),
            None => {
                return HttpResponse::json(
                    400,
                    json!({ "error": "workflow run has no active steps" }),
                );
            }
        },
    };

    let Some(step_def) = def.steps.iter().find(|s| s.step_id == step_id) else {
        return HttpResponse::json(400, json!({ "error": format!("unknown step `{step_id}`") }));
    };

    // Enforce two-tier HubAuthority boundary: a Peer token cannot approve/reject an Owner-tier step
    // or impersonate the Owner login (spec §2.2, §3.5).
    if let StepCompletion::HumanSignOff {
        approval_tier: ApprovalTier::Owner,
    } = step_def.completion
        && !authority.is_owner()
    {
        return HttpResponse::json(
            403,
            json!({ "error": "owner authority required for Owner-tier workflow sign-off" }),
        );
    }

    let by = match authority {
        HubAuthority::Owner { login } => payload
            .by
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| login.clone()),
        HubAuthority::Peer { agent } => {
            let candidate = payload
                .by
                .as_deref()
                .or(agent.as_deref())
                .unwrap_or("peer")
                .trim();
            if candidate.eq_ignore_ascii_case("owner")
                || candidate.eq_ignore_ascii_case(&state.config.owner_login)
            {
                return HttpResponse::json(
                    403,
                    json!({ "error": "peer authority cannot sign off as owner" }),
                );
            }
            candidate.to_string()
        }
    };

    // Ensure a backing record exists on the step so `approve_workflow_step` / `reject_workflow_step`
    // works even when a step was transitioned into `NeedsHuman` directly in unit tests.
    if store.get_project(&run.project).ok().flatten().is_none() {
        // If the run's project was created in-memory or without a catalog entry, ensure we still
        // check `authorize_step_sign_off` and advance the workflow run cleanly.
        let step_inst = match run.steps.get(&step_id) {
            Some(s) => s.clone(),
            None => {
                return HttpResponse::json(
                    400,
                    json!({ "error": format!("unknown step `{step_id}`") }),
                );
            }
        };
        let agents = store.list_agents(None, false).unwrap_or_default();
        if let Err(e) = fl_core::workflow::authorize_step_sign_off(
            &def.slug,
            step_def,
            &step_inst,
            &by,
            &agents,
            &state.config.owner_login,
        ) {
            return HttpResponse::json(403, json!({ "error": e.to_string() }));
        }
        let mut updated_run = run;
        if let Some(st) = updated_run.steps.get_mut(&step_id) {
            st.state = fl_core::model::State::NeedsHuman;
            st.attempt_count = st.attempt_count.max(1);
        }
        let outcome = if approve {
            fl_core::workflow::StepOutcome::Pass
        } else {
            fl_core::workflow::StepOutcome::Fail
        };
        if let Err(e) = fl_core::workflow::advance_workflow(
            &def,
            &mut updated_run,
            fl_core::workflow::StepAttemptReport {
                step_id,
                outcome,
                attempt_iri: None,
                tokens_used: 0,
                cost_usd_micros: 0,
                reason: payload.reason.or(payload.note),
            },
            stamp::now(),
        ) {
            return HttpResponse::json(400, json!({ "error": e.to_string() }));
        }
        updated_run.by = by;
        return match store.update_workflow_run(updated_run) {
            Ok(saved) => {
                let data = json!({ "run": saved });
                state.broadcast("workflow", "run_updated", data.clone());
                HttpResponse::json(200, data)
            }
            Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
        };
    }

    let env = WorkflowEnv::single(store);
    let res = if approve {
        approve_workflow_step(
            env,
            run_id,
            &step_id,
            &by,
            payload.note.as_deref(),
            &state.config.owner_login,
        )
    } else {
        let reason = payload
            .reason
            .as_deref()
            .or(payload.note.as_deref())
            .unwrap_or("rejected");
        reject_workflow_step(
            env,
            run_id,
            &step_id,
            &by,
            reason,
            &state.config.owner_login,
        )
    };

    match res {
        Ok(updated) => {
            let data = json!({ "run": updated });
            state.broadcast("workflow", "run_updated", data.clone());
            HttpResponse::json(200, data)
        }
        Err(e) => {
            let status = if e.is_policy_refusal() { 403 } else { 400 };
            HttpResponse::json(status, json!({ "error": e.to_string() }))
        }
    }
}

fn handle_post_workflow_abort(
    state: &HubState,
    store: &RedbStore,
    authority: &HubAuthority,
    run_id: &WorkflowRunId,
    req: &HttpRequest,
) -> HttpResponse {
    #[derive(Deserialize, Default)]
    struct AbortPayload {
        by: Option<String>,
    }

    let payload: AbortPayload = req.json().unwrap_or_default();
    let by = payload
        .by
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| authority.actor_name().to_string());

    match abort_workflow_run(store, run_id, stamp::now(), &by) {
        Ok(updated) => {
            let data = json!({ "run": updated });
            state.broadcast("workflow", "run_updated", data.clone());
            HttpResponse::json(200, data)
        }
        Err(e) => HttpResponse::json(400, json!({ "error": e.to_string() })),
    }
}

fn handle_get_gate_secrets(state: &HubState) -> HttpResponse {
    match Vault::open_default(state.config.home_override.as_deref()) {
        Ok(vault) => HttpResponse::json(200, json!({ "secrets": vault.list() })),
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

fn handle_post_gate_secret(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may write secrets to the FerroGate vault" }),
        );
    }

    #[derive(Deserialize)]
    struct SetSecretPayload {
        name: String,
        value: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        allowed_hosts: Vec<String>,
        #[serde(default)]
        allowed_methods: Vec<String>,
        #[serde(default)]
        allowed_agents: Vec<String>,
        #[serde(default)]
        allow_private_network: bool,
        #[serde(default)]
        lease_ttl_secs: Option<u64>,
    }

    let payload: SetSecretPayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let mut vault = match Vault::open_default(state.config.home_override.as_deref()) {
        Ok(v) => v,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };

    match vault.set(SetSecretArgs {
        name: payload.name,
        value: payload.value,
        description: payload.description,
        allowed_hosts: payload.allowed_hosts,
        allowed_methods: payload.allowed_methods,
        allowed_agents: payload.allowed_agents,
        allow_private_network: payload.allow_private_network,
        lease_ttl_secs: payload.lease_ttl_secs,
    }) {
        Ok(meta) => HttpResponse::json(200, json!({ "secret": meta })),
        Err(e) => HttpResponse::json(400, json!({ "error": e.to_string() })),
    }
}

fn handle_delete_gate_secret(
    state: &HubState,
    authority: &HubAuthority,
    secret_name: &str,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(
            403,
            json!({ "error": "only Owner authority may delete secrets from the FerroGate vault" }),
        );
    }

    let mut vault = match Vault::open_default(state.config.home_override.as_deref()) {
        Ok(v) => v,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };

    match vault.remove(secret_name) {
        Ok(true) => HttpResponse::json(200, json!({ "removed": true, "name": secret_name })),
        Ok(false) => HttpResponse::json(404, json!({ "error": "secret not found" })),
        Err(e) => HttpResponse::json(400, json!({ "error": e.to_string() })),
    }
}

fn handle_get_gate_connectors(state: &HubState) -> HttpResponse {
    match discover_connectors(
        Some(&state.config.repo_root),
        state.config.home_override.as_deref(),
    ) {
        Ok(connectors) => HttpResponse::json(200, json!({ "connectors": connectors })),
        Err(e) => HttpResponse::json(500, json!({ "error": e.to_string() })),
    }
}

fn handle_post_gate_call(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    let mut proxy_req: ProxyRequest = match req.json() {
        Ok(r) => r,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    if !authority.is_owner() || proxy_req.caller_agent.is_none() {
        proxy_req.caller_agent = Some(authority.actor_name().to_string());
    }

    let vault = match Vault::open_default(state.config.home_override.as_deref()) {
        Ok(v) => v,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };

    match execute_proxied_request(&vault, &proxy_req) {
        Ok(resp) => HttpResponse::json(200, json!({ "response": resp })),
        Err(e) => HttpResponse::json(403, json!({ "error": e.to_string() })),
    }
}

fn handle_post_gate_mcp(
    state: &HubState,
    authority: &HubAuthority,
    connector_filter: Option<&str>,
    req: &HttpRequest,
) -> HttpResponse {
    let rpc_req: Value = match req.json() {
        Ok(v) => v,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };

    let vault = match Vault::open_default(state.config.home_override.as_deref()) {
        Ok(v) => v,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };

    let connectors = discover_connectors(
        Some(&state.config.repo_root),
        state.config.home_override.as_deref(),
    )
    .unwrap_or_default();

    match handle_mcp_jsonrpc_request(
        &rpc_req,
        &vault,
        &connectors,
        connector_filter,
        Some(authority.actor_name().to_string()),
    ) {
        Some(reply) => HttpResponse::json(200, reply),
        None => HttpResponse::json(202, json!({ "accepted": true })),
    }
}

fn handle_post_gate_bridge_webhook(
    state: &HubState,
    authority: &HubAuthority,
    bridge_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    let mut event: InboundBridgeEvent = match req.json() {
        Ok(ev) => ev,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    event.bridge_id = bridge_id.to_string();

    let connectors = match discover_connectors(
        Some(&state.config.repo_root),
        state.config.home_override.as_deref(),
    ) {
        Ok(c) => c,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };

    let Some(bridge_cfg) = connectors
        .iter()
        .flat_map(|c| c.bridges.iter())
        .find(|b| b.id == bridge_id)
    else {
        return HttpResponse::json(
            404,
            json!({ "error": format!("bridge `{bridge_id}` not found") }),
        );
    };

    let vault = match Vault::open_default(state.config.home_override.as_deref()) {
        Ok(v) => v,
        Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
    };

    let provided_secret = req
        .headers
        .get("x-ferrogate-secret")
        .or_else(|| req.headers.get("x-webhook-secret"))
        .map(String::as_str);
    if let Err(e) =
        verify_inbound_bridge_webhook(bridge_cfg, &vault, authority.is_owner(), provided_secret)
    {
        return HttpResponse::json(403, json!({ "error": e.to_string() }));
    }

    let plan = match evaluate_inbound_bridge_event(bridge_cfg, &event) {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(403, json!({ "error": e.to_string() })),
    };

    match &plan.target {
        BridgeTarget::WireAgent { persona } => {
            let (canonical_id, target_persona) = resolve_session_target(state, persona);
            let sender_label =
                bridge_prompt_sender(event.sender_name.as_deref().unwrap_or(&event.sender_id));
            let delivery_res = dispatch_operator_prompt_for_session(
                state,
                target_persona.as_deref(),
                &canonical_id,
                &sender_label,
                &plan.prompt_body,
            );
            state.mark_session_in_flight_released(&canonical_id);
            HttpResponse::json(
                200,
                json!({
                    "status": "dispatched",
                    "delivered": delivery_res.is_ok(),
                    "session_id": canonical_id,
                    "plan": plan,
                }),
            )
        }
        BridgeTarget::Connector {
            connector_id,
            tool_name,
        } => {
            let Some(conn) = connectors.iter().find(|c| c.id == *connector_id) else {
                return HttpResponse::json(
                    404,
                    json!({ "error": format!("target connector `{connector_id}` not found") }),
                );
            };
            let resolved_tool = tool_name
                .clone()
                .or_else(|| conn.tools.first().map(|t| t.name.clone()))
                .unwrap_or_default();
            let vault = match Vault::open_default(state.config.home_override.as_deref()) {
                Ok(v) => v,
                Err(e) => return HttpResponse::json(500, json!({ "error": e.to_string() })),
            };
            let args = json!({
                "prompt": plan.prompt_body,
                "content": plan.prompt_body,
                "channel_id": plan.channel_id,
            });
            match conn.execute_tool(&vault, &resolved_tool, &args, Some("bridge".to_string())) {
                Ok((resp, projected)) => HttpResponse::json(
                    200,
                    json!({
                        "status": "executed",
                        "plan": plan,
                        "upstream_status": resp.status,
                        "result": projected,
                    }),
                ),
                Err(e) => HttpResponse::json(502, json!({ "error": e.to_string(), "plan": plan })),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::at::At;
    use fl_core::ids::{RevisionId, StateId, seq_iri};

    fn memory_entry(n: u64, slug: &str, body: &str, sensitive: bool) -> StateEntry {
        StateEntry {
            id: StateId(seq_iri(n)),
            scope: StateScope::Workspace,
            concern: StateConcern::Memory,
            slug: slug.to_string(),
            sensitive,
            ever_sensitive: sensitive,
            payload: StatePayload::Memory(MemoryEntry {
                category: MemoryCategory::Note,
                basis: MemoryBasis::Inference,
                lifecycle: MemoryLifecycle::Active,
                superseded_by: None,
                pinned: false,
                summary: format!("{slug} summary"),
                body: body.to_string(),
            }),
            head_revision: RevisionId(seq_iri(1000 + n)),
            updated_at: At::from_unix_millis(1_000 + n),
            by: "test".to_string(),
        }
    }

    fn sensitive_handoff_entry(n: u64) -> StateEntry {
        StateEntry {
            id: StateId(seq_iri(n)),
            scope: StateScope::Workspace,
            concern: StateConcern::Handoff,
            slug: "secret-handoff".to_string(),
            sensitive: true,
            ever_sensitive: true,
            payload: StatePayload::Handoff(HandoffEntry {
                summary: "zephyr handoff summary".to_string(),
                shipped: vec!["zephyr shippedsecrettoken".to_string()],
                in_flight: vec!["zephyr inflightsecrettoken".to_string()],
                blockers: vec!["zephyr blockersecrettoken".to_string()],
                next_steps: vec!["zephyr nextstepsecrettoken".to_string()],
                notes: Some("zephyr notes".to_string()),
            }),
            head_revision: RevisionId(seq_iri(1000 + n)),
            updated_at: At::from_unix_millis(1_000 + n),
            by: "test".to_string(),
        }
    }

    #[test]
    fn hub_search_redacts_sensitive_snippet_for_non_owner_only() {
        let secret = memory_entry(
            1,
            "secret-note",
            "zephyr secretbodytoken zephyr zephyr",
            true,
        );
        let public = memory_entry(2, "public-note", "zephyr public note", false);
        let internal = memory_entry(3, "hub-session-abc123", "zephyr internal", false);
        let entries = vec![public, internal, secret];

        // Non-owner: sensitive entry and snippet redacted, internal excluded.
        let (hits, total) = search_state_for_hub(&entries, "zephyr", None, None, 10, false);
        assert_eq!(total, 2, "internal hub entry must be excluded: {hits:?}");
        let secret_hit = hits
            .iter()
            .find(|h| h.entry.slug == "secret-note")
            .expect("sensitive hit present but redacted");
        assert_eq!(secret_hit.snippet, "[REDACTED]");
        let serialized = serde_json::to_string(&secret_hit).unwrap();
        assert!(!serialized.contains("secretbodytoken"), "{serialized}");
        assert!(serialized.contains("[REDACTED]"), "{serialized}");
        // Public hit is untouched for non-owners.
        let public_hit = hits.iter().find(|h| h.entry.slug == "public-note").unwrap();
        assert_ne!(public_hit.snippet, "[REDACTED]");

        // Owner sees full sensitive hit.
        let (owner_hits, owner_total) =
            search_state_for_hub(&entries, "zephyr", None, None, 10, true);
        assert_eq!(owner_total, 2);
        let owner_secret = owner_hits
            .iter()
            .find(|h| h.entry.slug == "secret-note")
            .unwrap();
        assert_ne!(owner_secret.snippet, "[REDACTED]");
        assert!(
            serde_json::to_string(owner_secret)
                .unwrap()
                .contains("secretbodytoken")
        );

        // Limit 0 returns no hits but keeps the full ranked count.
        let (none, total_zero) = search_state_for_hub(&entries, "zephyr", None, None, 0, true);
        assert!(none.is_empty());
        assert_eq!(total_zero, 2);

        // Truncation keeps total before the cap.
        let (one, total_one) = search_state_for_hub(&entries, "zephyr", None, None, 1, true);
        assert_eq!(one.len(), 1);
        assert_eq!(total_one, 2);
    }

    #[test]
    fn hub_search_redacts_sensitive_handoff_lists_for_non_owner_only() {
        const SECRET_TOKENS: [&str; 4] = [
            "shippedsecrettoken",
            "inflightsecrettoken",
            "blockersecrettoken",
            "nextstepsecrettoken",
        ];
        let entry = sensitive_handoff_entry(10);
        let entries = vec![entry.clone()];

        // List path: redact_sensitive_entry clears all four handoff lists.
        let redacted = redact_sensitive_entry(entry.clone());
        let StatePayload::Handoff(h) = &redacted.payload else {
            panic!("expected handoff payload");
        };
        assert_eq!(h.summary, "[REDACTED]");
        assert_eq!(h.notes.as_deref(), Some("[REDACTED]"));
        assert!(h.shipped.is_empty() && h.in_flight.is_empty());
        assert!(h.blockers.is_empty() && h.next_steps.is_empty());
        let serialized_entry = serde_json::to_string(&redacted).unwrap();
        for token in SECRET_TOKENS {
            assert!(!serialized_entry.contains(token), "{serialized_entry}");
        }

        // Search path: non-owner hit leaks no list-only token.
        let (hits, total) = search_state_for_hub(&entries, "zephyr", None, None, 10, false);
        assert_eq!(total, 1);
        let hit = &hits[0];
        assert_eq!(hit.snippet, "[REDACTED]");
        let StatePayload::Handoff(hit_h) = &hit.entry.payload else {
            panic!("expected handoff payload");
        };
        assert!(hit_h.shipped.is_empty() && hit_h.in_flight.is_empty());
        assert!(hit_h.blockers.is_empty() && hit_h.next_steps.is_empty());
        let serialized_hit = serde_json::to_string(hit).unwrap();
        for token in SECRET_TOKENS {
            assert!(!serialized_hit.contains(token), "{serialized_hit}");
        }

        // Owner still sees the list contents.
        let (owner_hits, _) = search_state_for_hub(&entries, "zephyr", None, None, 10, true);
        let serialized_owner = serde_json::to_string(&owner_hits[0]).unwrap();
        for token in SECRET_TOKENS {
            assert!(serialized_owner.contains(token), "{serialized_owner}");
        }
    }

    #[test]
    fn artifact_preview_serves_allowed_files_and_rejects_sensitive_or_outside_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        let home_dir = tmp.path().join("home");
        let brain_dir = home_dir.join(".gemini/antigravity-cli/brain/conv-1");
        fs::create_dir_all(&repo_root).unwrap();
        fs::create_dir_all(&brain_dir).unwrap();

        let md_path = repo_root.join("spec.md");
        fs::write(
            &md_path,
            "# Tactical Spec\n\n```mermaid\ngraph TD\n  A-->B\n```\n",
        )
        .unwrap();

        let brain_svg = brain_dir.join("diagram.svg");
        fs::write(&brain_svg, "<svg viewBox=\"0 0 10 10\"></svg>").unwrap();

        let html_code = repo_root.join("page.html");
        fs::write(&html_code, "<html><script>alert(1)</script></html>").unwrap();

        // Sensitive paths inside repo_root that must be rejected with 403.
        let git_config = repo_root.join(".git/config");
        fs::create_dir_all(git_config.parent().unwrap()).unwrap();
        fs::write(&git_config, "[core]\n").unwrap();

        let secrets_file = repo_root.join("secrets/api_key.txt");
        fs::create_dir_all(secrets_file.parent().unwrap()).unwrap();
        fs::write(&secrets_file, "TOP_SECRET").unwrap();

        let env_file = repo_root.join(".env");
        fs::write(&env_file, "TOKEN=123").unwrap();

        let vault_file = repo_root.join(".fl/gate/vault.enc");
        fs::create_dir_all(vault_file.parent().unwrap()).unwrap();
        fs::write(&vault_file, "ENC").unwrap();

        let gate_audit = repo_root.join("gate-audit.jsonl");
        fs::write(&gate_audit, "{}").unwrap();

        let redb_file = repo_root.join("store.redb");
        fs::write(&redb_file, "REDB").unwrap();

        let outside_file = tmp.path().join("outside.txt");
        fs::write(&outside_file, "outside root").unwrap();

        let cfg = crate::HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: repo_root.clone(),
            owner_token: Some("owner-test-tok".to_string()),
            home_override: Some(home_dir),
            ..crate::HubServerConfig::default()
        };
        let state = HubState::new(cfg);

        let authed_get = |target: &str| -> HttpResponse {
            let mut req = HttpRequest::new("GET", target);
            req.headers.insert(
                "authorization".to_string(),
                "Bearer owner-test-tok".to_string(),
            );
            handle_http_request(&state, &req)
        };

        // 1. JSON preview of Markdown file inside repo_root.
        let enc_md = percent_encode_query_value(&md_path.display().to_string());
        let resp = authed_get(&format!("/api/v1/artifacts/preview?path={enc_md}"));
        assert_eq!(resp.status, 200);
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["name"], "spec.md");
        assert_eq!(body["kind"], "markdown");
        assert!(
            body["content"]
                .as_str()
                .unwrap_or_default()
                .contains("# Tactical Spec")
        );
        assert!(
            body["download_url"]
                .as_str()
                .unwrap_or_default()
                .contains("download=1")
        );

        // 2. Download mode sets Content-Disposition: attachment and CSP header.
        let dl_resp = authed_get(&format!(
            "/api/v1/artifacts/preview?path={enc_md}&download=1"
        ));
        assert_eq!(dl_resp.status, 200);
        let disp = dl_resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Content-Disposition"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert!(
            disp.starts_with("attachment; filename=\"spec.md\""),
            "{disp}"
        );
        let csp = dl_resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Content-Security-Policy"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert_eq!(csp, "script-src 'none'; object-src 'none'");

        // 2b. Code files (.html) serve with text/plain; charset=utf-8 in raw mode.
        let enc_html = percent_encode_query_value(&html_code.display().to_string());
        let html_raw = authed_get(&format!(
            "/api/v1/artifacts/preview?path={enc_html}&mode=raw"
        ));
        assert_eq!(html_raw.status, 200);
        assert_eq!(html_raw.content_type, "text/plain; charset=utf-8");

        // 3. Brain artifact directory file is allowed.
        let enc_svg = percent_encode_query_value(&brain_svg.display().to_string());
        let svg_resp = authed_get(&format!("/api/v1/artifacts/preview?path={enc_svg}"));
        assert_eq!(svg_resp.status, 200);
        let svg_body: Value = serde_json::from_slice(&svg_resp.body).unwrap();
        assert_eq!(svg_body["kind"], "svg");

        // 4. Reject sensitive paths (.git, secrets, .env, .fl/gate/vault.enc, gate-audit.jsonl, .redb) and outside files with 403.
        for forbidden in [
            &git_config,
            &secrets_file,
            &env_file,
            &vault_file,
            &gate_audit,
            &redb_file,
            &outside_file,
            Path::new("/etc/passwd"),
        ] {
            let enc = percent_encode_query_value(&forbidden.display().to_string());
            let r = authed_get(&format!("/api/v1/artifacts/preview?path={enc}"));
            assert_eq!(
                r.status,
                403,
                "expected 403 for forbidden path {}, got {}",
                forbidden.display(),
                r.status
            );
        }
    }
}
